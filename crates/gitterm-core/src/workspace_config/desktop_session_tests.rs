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
