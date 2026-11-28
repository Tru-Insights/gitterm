use super::*;
use crate::desktop_store::{DesktopStore, LoadState};
use serde_json::json;

fn file() -> WorkspacesFile {
    serde_json::from_value(json!({"future":42,"active_workspace":0,"workspaces":[
        {"name":"First","abbrev":"F","dir":"/fixture/project","color":"blue","active_tab":0,
         "future_workspace":7,"tabs":[{"dir":"/fixture/project","session_uid":"first","future_tab":true}],
         "bottom_terminals":[{"dir":"/fixture/project"}]},
        {"name":"Closed","abbrev":"C","dir":"/fixture/closed","color":"blue","active":false,"tabs":[]},
        {"name":"Second","abbrev":"S","dir":"/fixture/second","color":"blue","tabs":[{"dir":"/fixture/second","session_uid":"second"}]}]
    })).unwrap()
}

#[test]
fn tru274_live_desktop_inventory_and_selection_have_iced_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let mut expected = file();
    let path = temp.path().join("workspaces.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&expected).unwrap()).unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    let target = WorkspaceTarget::of(&expected.workspaces[0]);
    let tab: WorkspaceTabConfig = serde_json::from_value(json!({"dir":"/fixture/project/sub",
        "repo_dir":"/fixture/project/sub","startup_command":"placeholder-cli --example",
        "chat_session_id":"placeholder-chat","session_uid":"new-shell"}))
    .unwrap();
    let projection = WorkspaceTabConfig::from_terminal(
        Path::new(&tab.dir),
        Path::new(&tab.dir),
        tab.startup_command.as_deref(),
        &crate::session::Target {
            session_uid: "new-shell".into(),
            tab_id: 1,
            task_id: None,
            task_session_id: None,
        },
        tab.chat_session_id.clone(),
        false,
    );
    assert_eq!(
        serde_json::to_vec(&projection).unwrap(),
        serde_json::to_vec(&tab).unwrap()
    );
    let edits = [
        DesktopSessionEdit::OpenTabs(vec![projection]),
        DesktopSessionEdit::OpenBottom {
            terminal: BottomTerminalConfig {
                extra: Default::default(),
                dir: "/fixture/project".into(),
            },
            initial_tabs: vec![],
        },
    ];
    for edit in edits {
        let target = target.clone();
        store
            .try_edit_workspaces(move |file| file.edit_desktop_sessions(&target, edit))
            .wait()
            .unwrap();
    }
    expected.workspaces[0].tabs.push(tab);
    expected.workspaces[0].active_tab = Some(1);
    expected.workspaces[0]
        .bottom_terminals
        .push(BottomTerminalConfig {
            extra: Default::default(),
            dir: "/fixture/project".into(),
        });
    assert_eq!(
        std::fs::read(&path).unwrap(),
        serde_json::to_vec_pretty(&expected).unwrap()
    );
    // Re-adoption is idempotent and does not erase future metadata.
    let target = target.clone();
    let adopted = expected.workspaces[0].tabs[1].clone();
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(&target, DesktopSessionEdit::OpenTabs(vec![adopted]))
        })
        .wait()
        .unwrap();
    let target = WorkspaceTarget::of(&expected.workspaces[2]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(&target, DesktopSessionEdit::Select(Some(0)))
        })
        .wait()
        .unwrap();
    expected.active_workspace = 1;
    expected.workspaces[2].active_tab = Some(0);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        serde_json::to_vec_pretty(&expected).unwrap()
    );
    drop(store);
    let reopened = DesktopStore::open(temp.path()).unwrap();
    let LoadState::Valid(saved) = reopened.snapshot().wait().unwrap().workspaces.loaded else {
        panic!("valid")
    };
    assert_eq!(saved.active_workspace, 1);
    assert_eq!(saved.workspaces[0].active_tab, Some(1));
}

#[test]
fn tru274_session_edits_refuse_ambiguous_or_stale_targets_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let mut file = file();
    let target = WorkspaceTarget::of(&file.workspaces[0]);
    file.workspaces.push(file.workspaces[0].clone());
    let path = temp.path().join("workspaces.json");
    let bytes = serde_json::to_vec_pretty(&file).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    assert!(store
        .try_edit_workspaces(
            move |file| file.edit_desktop_sessions(&target, DesktopSessionEdit::Select(Some(0)))
        )
        .wait()
        .is_err());
    let target = WorkspaceTarget::of(&file.workspaces[2]);
    assert!(store
        .try_edit_workspaces(
            move |file| file.edit_desktop_sessions(&target, DesktopSessionEdit::Select(Some(9)))
        )
        .wait()
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn tru274_session_save_preserves_unknown_nested_records() {
    let temp = tempfile::tempdir().unwrap();
    let mut raw = serde_json::to_value(file()).unwrap();
    raw["workspaces"][0]["bottom_terminals"][0]["future_bottom"] = json!(true);
    std::fs::write(
        temp.path().join("workspaces.json"),
        serde_json::to_vec(&raw).unwrap(),
    )
    .unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    let target = WorkspaceTarget::of(&file().workspaces[0]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(&target, DesktopSessionEdit::Select(None))
        })
        .wait()
        .unwrap();
    // Force a changed document through the serializer.
    let target = WorkspaceTarget::of(&file().workspaces[2]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(&target, DesktopSessionEdit::Select(Some(0)))
        })
        .wait()
        .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved["workspaces"][0]["bottom_terminals"][0]["future_bottom"],
        json!(true)
    );
}

#[test]
fn tru274_f4_migrate_repo_bottom_terminals_before_append_matches_iced() {
    let temp = tempfile::tempdir().unwrap();
    let mut initial = file();
    initial.repos = vec![serde_json::from_value(json!({"dir":"/fixture/project",
        "future_repo":9,"bottom_terminals":[{"dir":"/fixture/legacy","future_bottom":"legacy"}]}))
    .unwrap()];
    let mut iced = initial.clone();
    iced.normalize_repos();
    let terminal: BottomTerminalConfig =
        serde_json::from_value(json!({"dir":"/fixture/new"})).unwrap();
    iced.workspaces[0].bottom_terminals.push(terminal.clone());
    let path = temp.path().join("workspaces.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&initial).unwrap()).unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    let target = WorkspaceTarget::of(&initial.workspaces[0]);
    let mut saved = store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(
                &target,
                DesktopSessionEdit::OpenBottom {
                    terminal,
                    initial_tabs: vec![],
                },
            )
        })
        .wait()
        .unwrap();
    saved.normalize_repos(); // Same startup normalization as IcedCatalog.
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        serde_json::to_value(&iced).unwrap()
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        serde_json::to_vec_pretty(&iced).unwrap()
    );
}

#[test]
fn tru274_f5_equal_cwds_keep_each_bottom_descriptors_own_unknown_fields() {
    let temp = tempfile::tempdir().unwrap();
    let mut raw = serde_json::to_value(file()).unwrap();
    raw["workspaces"][0]["bottom_terminals"] = json!([
        {"dir":"/fixture/project","future_bottom":"first"},
        {"dir":"/fixture/project","future_bottom":"second"}]);
    let path = temp.path().join("workspaces.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    let target = WorkspaceTarget::of(&file().workspaces[0]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(
                &target,
                DesktopSessionEdit::OpenBottom {
                    terminal: serde_json::from_value(json!({"dir":"/fixture/project"})).unwrap(),
                    initial_tabs: vec![],
                },
            )
        })
        .wait()
        .unwrap();
    raw["workspaces"][0]["bottom_terminals"]
        .as_array_mut()
        .unwrap()
        .push(json!({"dir":"/fixture/project"}));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved, raw);
    // Removing the first of identical-cwd terminals keeps the second's metadata.
    let target = WorkspaceTarget::of(&file().workspaces[0]);
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(&target, DesktopSessionEdit::CloseBottom(0))
        })
        .wait()
        .unwrap();
    raw["workspaces"][0]["bottom_terminals"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved, raw);
}

#[test]
fn tru274_f6_explicit_local_location_accepts_terminal_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let mut iced = file();
    iced.workspaces[0].location = Some(WorkspaceLocationConfig::Local {
        root: "/fixture/project".into(),
    });
    let path = temp.path().join("workspaces.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&iced).unwrap()).unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    let target = WorkspaceTarget::of(&iced.workspaces[0]);
    let terminal: BottomTerminalConfig =
        serde_json::from_value(json!({"dir":"/fixture/new"})).unwrap();
    iced.workspaces[0].bottom_terminals.push(terminal.clone());
    store
        .try_edit_workspaces(move |file| {
            file.edit_desktop_sessions(
                &target,
                DesktopSessionEdit::OpenBottom {
                    terminal,
                    initial_tabs: vec![],
                },
            )
        })
        .wait()
        .unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        serde_json::to_vec_pretty(&iced).unwrap()
    );
}

#[test]
fn tru274_f7_failed_runtime_inventory_stays_dirty_and_retries_in_order() {
    for next_save in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workspaces.json");
        let mut iced = file();
        iced.workspaces[0].location = Some(WorkspaceLocationConfig::Local {
            root: "/fixture/project".into(),
        });
        let mut raw = serde_json::to_value(&iced).unwrap();
        raw["workspaces"][0]["location"]["future_location"] = json!({"keep":true});
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let store = DesktopStore::open(temp.path()).unwrap();
        let target = WorkspaceTarget::of(&iced.workspaces[0]);
        let tab: WorkspaceTabConfig =
            serde_json::from_value(json!({"dir":"/fixture/new","session_uid":"first-new"}))
                .unwrap();
        iced.workspaces[0].tabs.push(tab.clone());
        iced.workspaces[0].active_tab = Some(1);
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let at = target.clone();
        assert!(store
            .edit_runtime_workspaces(move |file| file
                .edit_desktop_sessions(&at, DesktopSessionEdit::OpenTabs(vec![tab])))
            .wait()
            .is_err());
        let snapshot = store.snapshot().wait().unwrap();
        assert!(snapshot.workspaces.dirty);
        assert!(snapshot.workspaces.error.is_some());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, original).unwrap();
        if next_save {
            let tab: WorkspaceTabConfig =
                serde_json::from_value(json!({"dir":"/fixture/next","session_uid":"second-new"}))
                    .unwrap();
            iced.workspaces[0].tabs.push(tab.clone());
            iced.workspaces[0].active_tab = Some(2);
            store
                .edit_runtime_workspaces(move |file| {
                    file.edit_desktop_sessions(&target, DesktopSessionEdit::OpenTabs(vec![tab]))
                })
                .wait()
                .unwrap();
        } else {
            store.drain().wait().unwrap();
        }
        assert!(!store.snapshot().wait().unwrap().workspaces.dirty);
        let mut raw = serde_json::to_value(&iced).unwrap();
        raw["workspaces"][0]["location"]["future_location"] = json!({"keep":true});
        assert_eq!(
            std::fs::read(path).unwrap(),
            serde_json::to_vec_pretty(&raw).unwrap()
        );
    }
}

#[test]
fn tru274_workspace_save_keeps_typed_environment_order() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workspaces.json");
    let document = file();
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let store = DesktopStore::open(temp.path()).unwrap();
    store
        .edit_workspaces(|file| {
            file.workspaces[0].env = (0..32)
                .map(|index| (format!("PLACEHOLDER_{index}"), "fixture".into()))
                .collect();
        })
        .wait()
        .unwrap();
    let LoadState::Valid(saved) = store.snapshot().wait().unwrap().workspaces.loaded else {
        panic!("valid workspace document")
    };
    // A JSON round-trip rebuilds randomized HashMaps, changing Iced's bytes.
    assert_eq!(
        std::fs::read(path).unwrap(),
        serde_json::to_vec_pretty(&saved).unwrap()
    );
}
