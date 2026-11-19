use super::*;
use crate::{data::WorkspaceId, workspace_status::WorkspaceStatus};
use gitterm_core::status::GitFileStatus;
use std::time::Instant;

fn workspace() -> LocalWorkspace {
    let mut model = WorkspaceStatus::default();
    model.add("/fixture".into(), Instant::now());
    assert_eq!(model.workspaces[0].id, WorkspaceId(1));
    let mut workspace = model.workspaces.pop().unwrap();
    workspace.status.unstaged = vec![GitFileStatus {
        path: "file.txt".into(),
        status: "M".into(),
        is_staged: false,
    }];
    workspace
}
fn loaded(workspace: &LocalWorkspace) -> ReviewReload {
    let mut reload = ReviewReload {
        visible: true,
        ..Default::default()
    };
    assert!(reload.request(SnapshotKey::new(workspace), true));
    reload.finish();
    reload
}

#[test]
fn unchanged_status_ticks_do_not_reload() {
    let mut workspace = workspace();
    let mut reload = loaded(&workspace);
    workspace.status.branch_name = "different branch".into();
    for _ in 0..3 {
        assert!(!reload.request(SnapshotKey::new(&workspace), false));
    }
}

#[test]
fn changed_path_group_or_status_reloads_once() {
    let mut workspace = workspace();
    let mut reload = loaded(&workspace);
    for change in 0..3 {
        match change {
            0 => workspace.status.unstaged[0].path = "renamed.txt".into(),
            1 => workspace.status.unstaged[0].status = "D".into(),
            _ => workspace.status.staged = std::mem::take(&mut workspace.status.unstaged),
        }
        let key = SnapshotKey::new(&workspace);
        assert!(reload.request(key.clone(), false));
        assert!(!reload.request(key.clone(), false));
        reload.finish();
        assert!(!reload.request(key, false));
    }
}

#[test]
fn hidden_change_reloads_once_when_shown() {
    let mut workspace = workspace();
    let mut reload = loaded(&workspace);
    reload.visible = false;
    workspace.status.unstaged.clear();
    let key = SnapshotKey::new(&workspace);
    for _ in 0..3 {
        assert!(!reload.request(key.clone(), false));
    }
    reload.visible = true;
    assert!(reload.request(key.clone(), false));
    reload.finish();
    assert!(!reload.request(key, false));
}

#[test]
fn showing_unchanged_pane_does_not_reload() {
    let workspace = workspace();
    let mut reload = loaded(&workspace);
    reload.visible = false;
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
    reload.visible = true;
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
}

#[test]
fn overlapping_ticks_produce_exactly_one_follow_up() {
    let mut workspace = workspace();
    let mut reload = ReviewReload {
        visible: true,
        ..Default::default()
    };
    assert!(reload.request(SnapshotKey::new(&workspace), true));
    for path in ["second.txt", "third.txt", "third.txt"] {
        workspace.status.unstaged[0].path = path.into();
        assert!(!reload.request(SnapshotKey::new(&workspace), false));
    }
    reload.finish();
    assert!(reload.request(SnapshotKey::new(&workspace), false));
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
    reload.finish();
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
}

#[test]
fn pending_load_waits_if_hidden_before_completion() {
    let mut workspace = workspace();
    let mut reload = loaded(&workspace);
    assert!(reload.request(SnapshotKey::new(&workspace), true));
    workspace.status.unstaged.clear();
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
    reload.visible = false;
    reload.finish();
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
    reload.visible = true;
    assert!(reload.request(SnapshotKey::new(&workspace), false));
    reload.finish();
    assert!(!reload.request(SnapshotKey::new(&workspace), false));
}

#[test]
fn initial_click_and_error_retry_coalesce_and_survive_showing() {
    let workspace = workspace();
    let key = SnapshotKey::new(&workspace);
    let mut reload = ReviewReload::default();
    assert!(!reload.request(key.clone(), true));
    reload.visible = true;
    assert!(reload.request(key.clone(), false));
    assert!(!reload.request(key.clone(), true));
    reload.finish();
    assert!(reload.request(key.clone(), false));
    reload.finish();
    assert!(!reload.request(key.clone(), false));
    assert!(reload.request(key, true));
}

#[test]
fn file_set_order_is_irrelevant() {
    let mut workspace = workspace();
    workspace.status.unstaged.push(GitFileStatus {
        path: "other.txt".into(),
        status: "M".into(),
        is_staged: false,
    });
    let key = SnapshotKey::new(&workspace);
    workspace.status.unstaged.reverse();
    assert_eq!(key, SnapshotKey::new(&workspace));
}
