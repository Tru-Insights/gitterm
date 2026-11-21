//! TRU-274: the Work sidebar reads gh's active login as Iced does.
//! A fake `gh` on PATH answers; the real gh is never run.
use super::pr_confirm_tests::{model, open_panel};
use super::tests::click_control;
use super::*;
use std::sync::Mutex;

/// PATH is process state: one test at a time owns it.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// Puts a fake `gh` first on PATH. `api` answers `answer` (a full `gh api -i`
/// response) and appends a line to `calls`; anything else exits 0.
struct FakeGh {
    _dir: tempfile::TempDir,
    calls: PathBuf,
    old_path: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl FakeGh {
    fn new(answer: &str) -> Self {
        let guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = api ]; then echo \"$*\" >> '{}'; printf '{}'; fi\n",
            calls.display(),
            answer
        );
        let gh = dir.path().join("gh");
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(&old_path.clone().unwrap_or_default()));
        // SAFETY: PATH_LOCK serialises every test that touches PATH.
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
        Self {
            _dir: dir,
            calls,
            old_path,
            _guard: guard,
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        // SAFETY: still holding PATH_LOCK.
        unsafe {
            match &self.old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

const LOGIN_OK: &str =
    r#"HTTP/2.0 200 OK\r\ncontent-type: application/json\r\n\r\n{"login":"Example-Dev"}"#;
const LOGIN_DENIED: &str = r#"HTTP/2.0 401 Unauthorized\r\ncontent-type: application/json\r\n\r\n{"message":"Bad credentials"}"#;

fn has_approve(s: &WorkSidebar) -> bool {
    s.rows()
        .into_iter()
        .find(|r| r.label == "Second change")
        .unwrap()
        .detail_controls
        .iter()
        .any(|c| c.label.starts_with("Approve"))
}

fn unmapped_author(s: &mut WorkSidebar) {
    s.pr_accounts.clear();
    s.pull_requests[0].prs[1]
        .snapshot
        .core
        .as_mut()
        .unwrap()
        .author = "example-dev".into();
}

/// Lets the background read finish; the executor does not wait for a child process.
fn settle(sidebar: &Entity<WorkSidebar>, cx: &mut gpui::VisualTestContext) {
    for _ in 0..200 {
        cx.run_until_parked();
        if !sidebar.read_with(cx, |s, _| s.login_reading) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the gh read never finished");
}

#[gpui::test]
fn active_login_read_off_thread_and_used_by_own_pr_rule(cx: &mut gpui::TestAppContext) {
    let gh = FakeGh::new(LOGIN_OK);
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        unmapped_author(s);
        assert!(
            s.needs_active_login(),
            "an unmapped PR runs as gh's account"
        );
        s.read_active_login(cx);
        assert!(s.login_reading, "the read is under way, not awaited here");
        assert_eq!(s.active_login, None, "update() did not block on gh");
    });
    sidebar.read_with(cx, |s, _| assert!(has_approve(s), "no login yet: not own"));
    settle(&sidebar, cx);
    sidebar.read_with(cx, |s, _| {
        assert_eq!(s.active_login.as_deref(), Some("Example-Dev"));
        assert!(!has_approve(s), "core's own-PR rule sees the login");
    });
    let calls = gh.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].ends_with("user"), "{calls:?}");
    // A mapped account needs no read.
    sidebar.update(cx, |s, _| {
        s.pr_accounts = [("example".to_string(), "someone".to_string())].into();
        assert!(!s.needs_active_login());
    });
}

#[gpui::test]
fn confirm_panel_shows_login_like_iced(cx: &mut gpui::TestAppContext) {
    let _gh = FakeGh::new(LOGIN_OK);
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        unmapped_author(s);
        s.confirm_close(cx);
        s.read_active_login(cx);
    });
    settle(&sidebar, cx);
    click_control(&sidebar, cx, "Second change", "Merge");
    assert_eq!(
        model(&sidebar, cx).account_line,
        "Runs as gh's active account (Example-Dev): github_accounts has no entry for example"
    );
}

#[gpui::test]
fn gh_failure_leaves_none_and_logs_once(cx: &mut gpui::TestAppContext) {
    let gh = FakeGh::new(LOGIN_DENIED);
    let (sidebar, cx, _) = open_panel(cx);
    for _ in 0..2 {
        sidebar.update(cx, |s, cx| s.read_active_login(cx));
        settle(&sidebar, cx);
    }
    assert_eq!(gh.calls().len(), 2, "each read asks gh");
    sidebar.update(cx, |s, _| {
        assert_eq!(s.active_login, None);
        let error = s.login_error.clone().expect("the failure is remembered");
        assert!(error.contains("HTTP 401"), "{error}");
        // The same failure again does not log again; a new one does.
        assert!(!s.apply_login_read(Err(error.clone())));
        assert!(s.apply_login_read(Err("gh api user: timed out".into())));
        // A good read keeps the login and re-arms the log.
        assert!(!s.apply_login_read(Ok("Example-Dev".into())));
        assert!(s.apply_login_read(Err(error.clone())));
        assert_eq!(s.active_login.as_deref(), Some("Example-Dev"));
    });
}
