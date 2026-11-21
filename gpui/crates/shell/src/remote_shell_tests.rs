//! Remote machines, read-only, on GPUI's test platform: a fake remote client
//! and a fixture Iced config. Nothing contacts a network or writes Iced.
use super::*;
use crate::{
    connect_form::CONNECT_UNAVAILABLE, folder_picker::FolderPicker, remote_client::RemoteClient,
};
use gitterm_core::{
    chats::{ChatBackend, ChatIndexEntry, ChatScope},
    remote::{
        client::RemoteAgentHandshake,
        config::RemoteAgentConfig,
        source::{SourceDirEntry, SourceDirListing, SourcePath, remote_parent_within_root},
        ssh_sessions::{RemoteHostConfig, RemoteSessionEntry, RemoteSessionsSnapshot},
    },
};
use gpui::{TestAppContext, VisualTestContext};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct FakeRemote {
    calls: Mutex<Vec<String>>,
}

impl FakeRemote {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl RemoteClient for FakeRemote {
    fn handshake(&self, remote: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String> {
        self.record(format!("handshake {}", remote.id));
        Ok(RemoteAgentHandshake {
            remote_id: remote.id.clone(),
            endpoint: remote.endpoint.clone(),
            agent_name: "fixture-agent".into(),
            agent_version: "0".into(),
            protocol_version: 1,
            capabilities: vec![],
            home_dir: "/home/fixture".into(),
        })
    }
    fn list_dir(&self, remote: &RemoteAgentConfig, dir: String) -> SourceDirListing {
        self.record(format!("list_dir {} {dir}", remote.id));
        let children: &[(&str, bool)] = match dir.as_str() {
            "/" => &[("home", false)],
            "/home" => &[("fixture", false)],
            "/home/fixture" => &[("code", true), ("notes", false)],
            "/home/fixture/code" => &[("app", true)],
            _ => &[],
        };
        let path = |name: &str| format!("{}/{name}", dir.trim_end_matches('/'));
        let mut entries: Vec<_> = children
            .iter()
            .map(|(name, git)| SourceDirEntry {
                name: (*name).into(),
                path: SourcePath::Remote(path(name)),
                is_dir: true,
                is_git_repo: *git,
            })
            .collect();
        entries.push(SourceDirEntry {
            name: "README.md".into(),
            path: SourcePath::Remote(path("README.md")),
            is_dir: false,
            is_git_repo: false,
        });
        SourceDirListing {
            tab_id: 0,
            parent: remote_parent_within_root(&dir, "/").map(SourcePath::Remote),
            display_dir: format!("{dir}/"),
            current_dir: SourcePath::Remote(dir),
            entries,
            error: None,
        }
    }
    fn list_chats(&self, remote: &RemoteAgentConfig) -> Result<Vec<ChatIndexEntry>, String> {
        self.record(format!("list_chats {}", remote.id));
        Ok(vec![ChatIndexEntry {
            id: "remote-chat".into(),
            backend: ChatBackend::Claude,
            path: "/home/fixture/.claude/remote-chat.jsonl".into(),
            cwd: "/home/fixture/code".into(),
            repo_root: None,
            is_worktree: false,
            branch: None,
            title: "Remote fixture chat".into(),
            latest: None,
            mtime: std::time::SystemTime::now(),
            size: 1,
            dead_cwd: false,
            facts: None,
            root_id: None,
        }])
    }
    fn list_sessions(&self, host: &RemoteHostConfig) -> RemoteSessionsSnapshot {
        self.record(format!("list_sessions {} {}", host.name, host.ssh_target));
        RemoteSessionsSnapshot {
            host_name: host.name.clone(),
            sessions: vec![RemoteSessionEntry {
                name: "box-app".into(),
                windows: 3,
                attached: 1,
            }],
            error: None,
        }
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

fn click(selector: &str, cx: &mut VisualTestContext) {
    let selector: &'static str = selector.to_owned().leak();
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

/// Every file under the Iced fixture dir, for byte-identical checks.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    files.sort();
    files
}

struct Fixture {
    root: tempfile::TempDir,
    shell: Entity<Shell>,
    fake: Arc<FakeRemote>,
}

impl Fixture {
    fn iced(&self) -> PathBuf {
        self.root.path().join("iced")
    }
    fn local(&self) -> PathBuf {
        self.root.path().join("work")
    }
}

fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(install_app_menus);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    let work = root.path().join("work");
    let boxdir = root.path().join("box");
    for dir in [&source, &work, &boxdir, &work.join("repo/.git")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(source.join("settings.json"), "{}").unwrap();
    std::fs::write(
        source.join("remote-agents.json"),
        serde_json::json!({"remote_agents":[{"id":"mini","name":"Mini","endpoint":"http://127.0.0.1:9",
            "auth":{"type":"token","token_ref":"env:GITTERM_FIXTURE_UNSET_TOKEN"}}]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(source.join("workspaces.json"), serde_json::json!({
        "active_workspace":0,
        "workspaces":[
            {"name":"Local","abbrev":"L","color":"blue","dir":work,"repos":[work],"tabs":[]},
            {"name":"box:app","abbrev":"B","color":"green","dir":boxdir,"tabs":[],
             "remote_session":{"label":"app","host_name":"box","ssh_target":"dev@box.invalid",
                "identity_file":"/fixture/id_ed25519","session_name":"box-app","remote_dir":"/srv/app",
                "codex_command":"codex"}}
        ]
    }).to_string()).unwrap();
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    let settings = workspace_config::load_iced_settings_from(&source);
    cx.update(|cx| theme::apply_settings(&settings, None, cx));
    let fake = Arc::new(FakeRemote::default());
    let client = fake.clone();
    let local_home = root.path().to_owned();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        let mut shell = Shell::with_owner_settings(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            &settings,
            window,
            cx,
        )
        .unwrap();
        shell.set_remote_client(client);
        shell.set_local_home(&local_home);
        shell
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    (Fixture { root, shell, fake }, cx)
}

fn picker(f: &Fixture, cx: &mut VisualTestContext) -> Entity<FolderPicker> {
    cx.read(|cx| f.shell.read(cx).folder_picker().cloned())
        .expect("folder picker open")
}

fn notice(f: &Fixture, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| f.shell.read(cx).owner.read(cx).error.clone())
}

fn bar(f: &Fixture, outcome: workspace_bar::Outcome, cx: &mut VisualTestContext) {
    f.shell.update_in(cx, |shell, window, cx| {
        shell.bar_outcome(outcome, window, cx)
    });
    draw(cx);
}

#[gpui::test]
fn picker_browses_a_remote_and_use_this_folder_is_a_notice(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    let before = snapshot(&f.iced());
    bar(&f, workspace_bar::Outcome::OpenFolder, cx);
    // Machine chips: this Mac, the configured remote, and Connect.
    for selector in [
        "folder-picker-machine-0",
        "folder-picker-machine-1",
        "folder-picker-connect",
        "folder-picker-native",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    let picker = picker(&f, cx);
    cx.read(|cx| {
        assert_eq!(*picker.read(cx).machine(), None);
        assert_eq!(
            picker.read(cx).files().dir,
            SourcePath::Local(f.root.path().to_owned())
        );
    });

    click("folder-picker-machine-1", cx);
    cx.read(|cx| {
        let files = picker.read(cx).files();
        assert_eq!(*picker.read(cx).machine(), Some("mini".into()));
        // Home arrived from the handshake: the picker moved there.
        assert_eq!(files.dir, SourcePath::Remote("/home/fixture".into()));
        let dirs: Vec<_> = files
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| (&*e.name, e.is_git_repo))
            .collect();
        assert_eq!(dirs, [("code", true), ("notes", false)]);
    });
    assert!(
        cx.debug_bounds("folder-picker-native").is_none(),
        "this Mac only"
    );
    let calls = f.fake.calls();
    assert!(calls.contains(&"handshake mini".to_string()), "{calls:?}");
    assert!(
        calls.contains(&"list_dir mini /home/fixture".to_string()),
        "{calls:?}"
    );

    click("folder-picker-dir-0", cx);
    cx.read(|cx| {
        assert_eq!(
            picker.read(cx).files().dir,
            SourcePath::Remote("/home/fixture/code".into())
        )
    });
    click("folder-picker-parent", cx);
    cx.read(|cx| {
        assert_eq!(
            picker.read(cx).files().dir,
            SourcePath::Remote("/home/fixture".into())
        )
    });

    click("folder-picker-confirm", cx);
    assert!(cx.read(|cx| f.shell.read(cx).folder_picker().is_none()));
    assert_eq!(
        notice(&f, cx).as_deref(),
        Some("Use this folder · not available in preview")
    );
    assert_eq!(snapshot(&f.iced()), before);
}

#[gpui::test]
fn typed_remote_path_lists_only_a_folder_and_home_tilde_expands(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    bar(
        &f,
        workspace_bar::Outcome::AddWorkspace("agent:mini".into()),
        cx,
    );
    let picker = picker(&f, cx);
    picker.update_in(cx, |picker, window, cx| {
        picker.path_input_for_test("~/code", window, cx);
        picker.submit_path(window, cx);
    });
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            picker.read(cx).files().dir,
            SourcePath::Remote("/home/fixture/code".into())
        );
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.path_input_for_test("relative", window, cx);
        picker.submit_path(window, cx);
    });
    cx.read(|cx| {
        assert_eq!(
            picker.read(cx).path_error(),
            Some("relative is not an absolute path")
        )
    });
    click("folder-picker-cancel", cx);
    assert!(cx.read(|cx| f.shell.read(cx).folder_picker().is_none()));
}

#[gpui::test]
fn use_macos_dialog_opens_the_os_picker(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    bar(&f, workspace_bar::Outcome::AddWorkspace("local".into()), cx);
    assert!(!cx.did_prompt_for_paths());
    click("folder-picker-native", cx);
    assert!(cx.did_prompt_for_paths());
    assert!(cx.read(|cx| f.shell.read(cx).folder_picker().is_none()));
    cx.simulate_path_prompt_response(|_| None);
    draw(cx);
    assert!(notice(&f, cx).is_none());
}

#[gpui::test]
fn new_work_add_repo_has_no_machine_chips_and_returns_the_folder(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    f.shell.update_in(cx, |shell, window, cx| {
        shell.open_new_work_sheet(window, cx)
    });
    draw(cx);
    click("workspace-sheet-add-repo", cx);
    assert!(cx.debug_bounds("folder-picker").is_some());
    assert!(cx.debug_bounds("folder-picker-machine-0").is_none());
    assert!(cx.debug_bounds("folder-picker-connect").is_none());
    let picker = picker(&f, cx);
    let repo = f.local().join("repo");
    picker.update_in(cx, |picker, window, cx| {
        picker.navigate(SourcePath::Local(repo.clone()), window, cx)
    });
    draw(cx);
    click("folder-picker-confirm", cx);
    draw(cx);
    let sheet = cx
        .read(|cx| f.shell.read(cx).new_work_sheet.clone())
        .unwrap();
    cx.read(|cx| assert_eq!(sheet.read(cx).draft().repos, [repo.canonicalize().unwrap()]));
}

#[gpui::test]
fn connect_form_drafts_locally_and_connect_saves_nothing(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    let before = snapshot(&f.iced());
    bar(&f, workspace_bar::Outcome::OpenFolder, cx);
    click("folder-picker-connect", cx);
    assert!(cx.debug_bounds("connect-form").is_some());
    cx.simulate_input("Studio");
    cx.simulate_keystrokes("enter");
    draw(cx);
    let form = cx
        .read(|cx| f.shell.read(cx).connect_form().cloned())
        .unwrap();
    cx.read(|cx| {
        assert_eq!(form.read(cx).draft(cx).0, "Studio");
        assert_eq!(form.read(cx).status(), Some(CONNECT_UNAVAILABLE));
    });
    click("connect-form-connect", cx);
    cx.read(|cx| assert_eq!(form.read(cx).status(), Some(CONNECT_UNAVAILABLE)));
    click("connect-form-close", cx);
    cx.read(|cx| {
        assert!(f.shell.read(cx).connect_form().is_none());
        assert!(f.shell.read(cx).folder_picker().is_some());
    });
    assert_eq!(snapshot(&f.iced()), before);
    assert!(!f.iced().join("tokens").exists());
}

#[gpui::test]
fn refresh_lists_remote_sessions_through_the_client(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    let id = cx.read(|cx| {
        f.shell
            .read(cx)
            .workspaces
            .iter()
            .find(|w| w.workspace.name == "box:app")
            .unwrap()
            .workspace
            .id
    });
    f.shell.update_in(cx, |shell, window, cx| {
        shell.select_workspace_id(id, window, cx)
    });
    draw(cx);
    assert!(cx.debug_bounds("remote-session-home").is_some());
    click("remote-session-refresh", cx);
    assert_eq!(
        f.fake.calls(),
        ["list_sessions box dev@box.invalid".to_string()]
    );
    cx.read(|cx| {
        let sessions = &f.shell.read(cx).remote.sessions;
        assert!(!sessions.loading && sessions.error.is_none());
        assert_eq!(sessions.sessions[0].name, "box-app");
    });
    click("remote-session-shell", cx);
    assert_eq!(
        notice(&f, cx).as_deref(),
        Some("Shell · not available in preview")
    );
}

#[gpui::test]
fn show_machine_chats_scopes_find_to_that_machine(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    bar(
        &f,
        workspace_bar::Outcome::ShowMachineChats("agent:mini".into()),
        cx,
    );
    assert!(f.fake.calls().contains(&"list_chats mini".to_string()));
    cx.read(|cx| {
        let shell = f.shell.read(cx);
        let regions = &shell.active(cx).store.regions;
        assert_eq!(regions.sidebar_mode, SidebarMode::Find);
        let state = shell.find_state.borrow();
        assert_eq!(state.filter().scope, ChatScope::Everywhere);
        let index = state.index(&Some("mini".into())).unwrap();
        assert_eq!(index.entries[0].id, "remote-chat");
        let views = shell.find_views.get(&shell.find_owner(cx)).unwrap();
        let open: Vec<_> = views
            .sidebar
            .read(cx)
            .open_machines()
            .iter()
            .cloned()
            .collect();
        assert_eq!(open, [Some("mini".to_string())]);
    });
}
