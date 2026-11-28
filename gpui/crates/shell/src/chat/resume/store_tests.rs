//! Chat and workspace lifecycle persistence must share the same authority.
use super::*;
use crate::workspace_config::{LiveWorkspaceAction, save_live_workspace};
use gitterm_core::{desktop_store::DesktopStore, park};
use std::{sync::mpsc, time::Duration};

fn fixture() -> (tempfile::TempDir, LiveTab, WorkspaceConfig) {
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let workspace: WorkspaceConfig = serde_json::from_value(serde_json::json!({
        "name":"Synthetic", "abbrev":"S", "color":"blue", "dir":root.path(),
        "future_workspace":42, "chats":["earlier-id"], "tabs":[
            {"dir":root.path(), "session_uid":"caller", "tab_kind":"agent", "future_tab":true},
            {"dir":"/other", "startup_command":"do not run", "future_other_tab":true}
        ]
    }))
    .unwrap();
    std::fs::write(
        root.path().join("workspaces.json"),
        serde_json::to_vec(&serde_json::json!({
            "future_document":{"keep":true}, "active_workspace":0, "workspaces":[workspace.clone()]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("settings.json"),
        br#"{"theme":"dark","future_setting":true}"#,
    )
    .unwrap();
    let services = ServiceContext::production(root.path(), "chat-store-test").unwrap();
    let mut confirmed = tab(
        &root.path().join("provider-cwd"),
        "caller".into(),
        Some("confirmed-id".into()),
    );
    confirmed.agent_config = Some(AgentBackendConfig::Claude {
        model: "opus".into(),
        permission_mode: Some("plan".into()),
        effort: Some("max".into()),
    });
    let live = LiveTab::new(services, &workspace, confirmed);
    (root, live, workspace)
}

fn park(store: &DesktopStore, workspace: WorkspaceConfig) {
    save_live_workspace(
        store,
        LiveWorkspaceAction::Park {
            key: park::Key {
                name: workspace.name,
                dir: workspace.dir,
            },
            info: park::Info {
                note: "come back".into(),
                parked_at: "2026-11-27T00:00:00Z".into(),
            },
            bottom_terminals: workspace.bottom_terminals,
            tabs: workspace.tabs,
            active_tab: Some(0),
            done: false,
        },
        None,
    )
    .unwrap();
}

#[test]
fn chat_confirmation_and_captured_park_both_land_through_the_live_owner() {
    for chat_first in [true, false] {
        let (root, live, captured) = fixture();
        let settings = std::fs::read(root.path().join("settings.json")).unwrap();
        // Both producers start with this same loaded document. A raw chat
        // replacement would be absent from the lifecycle owner's next save.
        let store = DesktopStore::open(root.path()).unwrap();
        if chat_first {
            live.persist(&store).unwrap();
            park(&store, captured);
        } else {
            park(&store, captured);
            live.persist(&store).unwrap();
        }
        store.drain().wait().unwrap();
        let file: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&std::fs::read(root.path().join("workspaces.json")).unwrap())
                .unwrap();
        let workspace = &file.workspaces[0];
        assert_eq!(
            std::fs::read(root.path().join("settings.json")).unwrap(),
            settings
        );
        assert!(!workspace.active);
        assert_eq!(workspace.parked.as_ref().unwrap().note, "come back");
        assert_eq!(file.extra["future_document"]["keep"], true);
        assert_eq!(workspace.extra["future_workspace"], 42);
        assert_eq!(workspace.chats, ["earlier-id", "confirmed-id"]);
        assert_eq!(workspace.tabs.len(), 2);
        let saved = &workspace.tabs[0];
        assert_eq!(saved.chat_session_id, live.tab.chat_session_id);
        assert_eq!(saved.repo_dir, live.tab.repo_dir);
        assert_eq!(
            serde_json::to_value(&saved.agent_config).unwrap(),
            serde_json::to_value(&live.tab.agent_config).unwrap()
        );
        assert_eq!(saved.extra["future_tab"], true);
        assert_eq!(workspace.tabs[1].dir, "/other");
        assert_eq!(
            workspace.tabs[1].startup_command.as_deref(),
            Some("do not run")
        );
        assert_eq!(workspace.tabs[1].extra["future_other_tab"], true);
    }
}

#[test]
fn chat_write_owner_failure_reaches_caller_and_drain_without_changing_file() {
    let (root, live, _) = fixture();
    let path = root.path().join("workspaces.json");
    let original = std::fs::read(&path).unwrap();
    // A refused owner cannot be bypassed by a chat-side read/modify/write,
    // even if an external repair makes the document readable again.
    std::fs::write(&path, b"{partial").unwrap();
    let store = DesktopStore::open(root.path()).unwrap();
    std::fs::write(&path, &original).unwrap();
    let shutdown = RuntimeShutdown::new();
    let (reply, saved) = tokio::sync::oneshot::channel();
    shutdown
        .sender
        .send(ShutdownCommand::Persist(
            Box::new(live),
            store.clone(),
            reply,
        ))
        .unwrap();
    let error = saved.blocking_recv().unwrap().unwrap_err();
    assert!(error.contains("Save confirmed Claude session"), "{error}");
    assert!(
        error.contains("parse") && error.contains("workspaces.json"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(
        shutdown
            .drain()
            .unwrap_err()
            .to_string()
            .contains("workspaces.json")
    );
}

#[test]
fn quit_drain_waits_for_pending_chat_write_on_the_live_owner() {
    let (root, live, _) = fixture();
    let store = DesktopStore::open(root.path()).unwrap();
    let (entered, started) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocked = store.transact_workspaces(move |_| {
        entered.send(()).unwrap();
        gate.recv().unwrap();
        Ok(())
    });
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    let shutdown = RuntimeShutdown::new();
    let (reply, saved) = tokio::sync::oneshot::channel();
    shutdown
        .sender
        .send(ShutdownCommand::Persist(
            Box::new(live),
            store.clone(),
            reply,
        ))
        .unwrap();
    // This is the same queue drain registered with the app's quit barrier.
    let (drained, completion) = mpsc::channel();
    let quitting = std::thread::spawn(move || {
        let result = shutdown.drain().map_err(|error| error.to_string());
        drained.send(result).unwrap();
    });
    let early = completion.recv_timeout(Duration::from_millis(150));
    let finished_early = early.is_ok();
    release.send(()).unwrap();
    blocked.wait().unwrap();
    let result = match early {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            completion.recv_timeout(Duration::from_secs(5)).unwrap()
        }
        Err(error) => panic!("quit drain disconnected: {error}"),
    };
    result.unwrap();
    saved.blocking_recv().unwrap().unwrap();
    quitting.join().unwrap();
    let gitterm_core::desktop_store::LoadState::Valid(document) =
        store.snapshot().wait().unwrap().workspaces.loaded
    else {
        panic!("valid owner document");
    };
    assert_eq!(
        document.workspaces[0].tabs[0].chat_session_id.as_deref(),
        Some("confirmed-id")
    );
    assert!(
        !finished_early,
        "quit finished before the pending chat write"
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved["workspaces"][0]["tabs"][0]["chat_session_id"],
        "confirmed-id"
    );
}

#[test]
fn chat_refuses_duplicate_legacy_workspace_identity_before_writing() {
    let (root, live, _) = fixture();
    let store = DesktopStore::open(root.path()).unwrap();
    store
        .transact_workspaces(|file| {
            file.workspaces.push(file.workspaces[0].clone());
            Ok(())
        })
        .wait()
        .unwrap();
    let path = root.path().join("workspaces.json");
    let before = std::fs::read(&path).unwrap();
    assert!(
        live.persist(&store)
            .unwrap_err()
            .to_string()
            .contains("Several saved workspaces")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn failed_chat_atomic_save_keeps_file_and_owner_snapshot_intact() {
    use std::os::unix::fs::PermissionsExt;
    let (root, live, _) = fixture();
    let path = root.path().join("workspaces.json");
    let original = std::fs::read(&path).unwrap();
    let store = DesktopStore::open(root.path()).unwrap();
    let permissions = std::fs::metadata(root.path()).unwrap().permissions();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let shutdown = RuntimeShutdown::new();
    let (reply, saved) = tokio::sync::oneshot::channel();
    shutdown
        .sender
        .send(ShutdownCommand::Persist(
            Box::new(live),
            store.clone(),
            reply,
        ))
        .unwrap();
    let result = saved.blocking_recv().unwrap();
    std::fs::set_permissions(root.path(), permissions).unwrap();
    let error = result.unwrap_err();
    assert!(
        error.contains("cannot write") && error.contains("workspaces.json"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let gitterm_core::desktop_store::LoadState::Valid(document) =
        store.snapshot().wait().unwrap().workspaces.loaded
    else {
        panic!("valid owner document");
    };
    assert!(document.workspaces[0].tabs[0].chat_session_id.is_none());
    assert!(shutdown.drain().is_err());
    store.drain().wait().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn acknowledged_settings_survive_a_park_captured_for_an_already_confirmed_chat() {
    let (root, live, mut captured) = fixture();
    captured.tabs[0].chat_session_id = live.tab.chat_session_id.clone();
    captured.tabs[0].agent_config = Some(default_config());
    captured.tabs[0].repo_dir = Some(captured.dir.clone());
    let store = DesktopStore::open(root.path()).unwrap();
    let prior = captured.clone();
    store
        .transact_workspaces(move |file| {
            file.workspaces[0] = prior;
            Ok(())
        })
        .wait()
        .unwrap();
    // The provider acknowledges new settings after Park captured the old ones,
    // and its save reaches the owner ahead of the background park operation.
    live.persist(&store).unwrap();
    park(&store, captured);
    let gitterm_core::desktop_store::LoadState::Valid(file) =
        store.snapshot().wait().unwrap().workspaces.loaded
    else {
        panic!("valid owner document");
    };
    let saved = &file.workspaces[0].tabs[0];
    assert_eq!(
        serde_json::to_value(&saved.agent_config).unwrap(),
        serde_json::to_value(&live.tab.agent_config).unwrap()
    );
    assert_eq!(saved.repo_dir, live.tab.repo_dir);
}

#[test]
fn review_regression_f1_chat_confirmation_then_repo_edit_keeps_tab() {
    use gitterm_core::workspace_config::{RepoEdit, WorkspaceTarget};
    let (root, live, _captured) = fixture();
    let store = DesktopStore::open(root.path()).unwrap();
    live.persist(&store).unwrap();
    let snapshot = store.snapshot().wait().unwrap();
    let gitterm_core::desktop_store::LoadState::Valid(file) = snapshot.workspaces.loaded else {
        panic!("valid")
    };
    let target = WorkspaceTarget::of(&file.workspaces[0]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_repo(
                &target,
                RepoEdit::Add {
                    dir: PathBuf::from("/fixture/extra"),
                    detected_command: None,
                },
            )
        })
        .wait()
        .unwrap();
    let file: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_slice(&std::fs::read(root.path().join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(
        file.workspaces[0].tabs[0].chat_session_id.as_deref(),
        Some("confirmed-id")
    );
    assert_eq!(file.workspaces[0].tabs.len(), 2);
    assert!(file.workspaces[0].repos.contains(&"/fixture/extra".into()));
}

#[test]
fn review_regression_f4_queued_chat_write_resolves_renamed_workspace_by_caller() {
    use gitterm_core::workspace_config::{RepoEdit, WorkspaceTarget};
    let (root, mut live, _captured) = fixture();
    let store = DesktopStore::open(root.path()).unwrap();
    live.persist(&store).unwrap();
    let snapshot = store.snapshot().wait().unwrap();
    let gitterm_core::desktop_store::LoadState::Valid(file) = snapshot.workspaces.loaded else {
        panic!("valid")
    };
    let target = WorkspaceTarget::of(&file.workspaces[0]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_repo(&target, RepoEdit::Rename("Renamed".into()))
        })
        .wait()
        .unwrap();
    // This queued clone still carries the old display name.
    live.tab.startup_command = Some("placeholder resume".into());
    live.persist(&store).unwrap();
    let file: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_slice(&std::fs::read(root.path().join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(file.workspaces[0].name, "Renamed");
    assert_eq!(
        file.workspaces[0].tabs[0].startup_command,
        live.tab.startup_command
    );
}
