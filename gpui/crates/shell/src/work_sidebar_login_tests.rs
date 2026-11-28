//! TRU-274: the Work sidebar reads gh's active login as Iced does.
//! An injected reader answers; the real gh is never run.
use super::pr_confirm_tests::{model, open_panel};
use super::tests::click_control;
use super::*;
use std::sync::{Arc, Mutex};

/// A reader that answers `answer` and counts its calls; no process runs.
fn fake_reader(answer: Result<String, String>) -> (LoginReader, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    let reader: LoginReader = Arc::new(move || {
        *counter.lock().unwrap() += 1;
        answer.clone()
    });
    (reader, calls)
}

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

/// Lets the background read finish.
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
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (reader, calls) = fake_reader(Ok("Example-Dev".into()));
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        s.login_reader = reader;
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
    assert_eq!(*calls.lock().unwrap(), 1);
    // A mapped account needs no read.
    sidebar.update(cx, |s, _| {
        s.pr_accounts = [("example".to_string(), "someone".to_string())].into();
        assert!(!s.needs_active_login());
    });
}

#[gpui::test]
fn confirm_panel_shows_login_like_iced(cx: &mut gpui::TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (reader, _) = fake_reader(Ok("Example-Dev".into()));
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        s.login_reader = reader;
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
    let _test_app = crate::test_app::Cleanup::new(cx);
    let denied = "gh api user: HTTP 401: Bad credentials".to_string();
    let (reader, calls) = fake_reader(Err(denied));
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, _| s.login_reader = reader);
    for _ in 0..2 {
        sidebar.update(cx, |s, cx| s.read_active_login(cx));
        settle(&sidebar, cx);
    }
    assert_eq!(*calls.lock().unwrap(), 2, "each read asks the reader");
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

#[gpui::test]
fn default_test_sidebar_never_reads_gh(cx: &mut gpui::TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (sidebar, cx, _) = open_panel(cx);
    // Leave the default reader in place and count calls through a wrapper
    // that delegates to it.
    let inner = sidebar.read_with(cx, |s, _| s.login_reader.clone());
    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    sidebar.update(cx, |s, cx| {
        s.login_reader = Arc::new(move || {
            *counter.lock().unwrap() += 1;
            inner()
        });
        unmapped_author(s);
        assert!(s.needs_active_login());
        s.read_active_login(cx);
    });
    settle(&sidebar, cx);
    assert_eq!(*calls.lock().unwrap(), 1);
    sidebar.read_with(cx, |s, _| {
        assert_eq!(s.active_login, None);
        assert_eq!(
            s.login_error.as_deref(),
            Some("login reader not set in tests")
        );
    });
}
