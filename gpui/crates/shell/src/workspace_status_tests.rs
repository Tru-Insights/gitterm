//! The local subset of src/baseline_tests.rs: workspace_selection_traces,
//! status_poll_visibility_and_reset_traces, input_wake_and_enter_git_reset_traces,
//! and status_backoff_completion_and_error_traces. Expected cases are pinned
//! there; the direct-core replay additionally compares every state field.
use super::*;
use gitterm_core::status::GitFileStatus;
use std::time::Duration;

fn model(now: Instant) -> WorkspaceStatus {
    let mut model = WorkspaceStatus::default();
    for name in ["alpha", "beta", "gamma"] {
        model.add(PathBuf::from(format!("/fixture/{name}")), now);
    }
    model.dispatch(Intent::Activate(WorkspaceId(1)), now);
    for workspace in &mut model.workspaces {
        workspace.status.last_poll = now;
    }
    model
}
fn snapshot(owner: usize, branch: &str, error: bool) -> GitStatusSnapshot {
    GitStatusSnapshot {
        tab_id: owner,
        repo_path: "/fixture/alpha".into(),
        repo_name: "synthetic".into(),
        branch_name: branch.into(),
        is_git_repo: true,
        staged: vec![GitFileStatus {
            path: "staged.txt".into(),
            status: "A".into(),
            is_staged: true,
        }],
        unstaged: vec![GitFileStatus {
            path: "modified.txt".into(),
            status: "M".into(),
            is_staged: false,
        }],
        untracked: vec![GitFileStatus {
            path: "new.txt".into(),
            status: "?".into(),
            is_staged: false,
        }],
        error: error.then(|| "remote status failed: synthetic disconnect".into()),
    }
}
fn commands(owner: usize, values: Vec<StatusCommand>) -> Vec<Command> {
    values
        .into_iter()
        .map(|command| Command::Status { owner, command })
        .collect()
}
fn same_state(actual: &GitStatusState, expected: &GitStatusState) {
    // Debug includes all core fields, including clocks/hash/loading/error/lists.
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
}
#[test]
fn bar_and_keyboard_selection_match_iced_baseline_and_core_replay() {
    let now = Instant::now();
    let roster: Vec<_> = (1..=3)
        .map(|id| WorkspaceFact { id, parked: false })
        .collect();
    for (index, usage, selected) in [
        (1, false, 2),
        (2, false, 3),
        (0, true, 1),
        (99, true, 1),
        (99, false, 1),
    ] {
        let mut model = model(now);
        model.usage_open = usage;
        let mut baseline = model.selection.clone();
        // Bar clicks and Ctrl+n both dispatch this same semantic index, as Iced does.
        let expected = baseline.reduce(
            SelectionEvent::Select(roster.get(index).map(|w| w.id)),
            &roster,
            usage,
            now,
        );
        assert_eq!(
            model.dispatch(Intent::SelectIndex(index), now),
            expected
                .into_iter()
                .map(Command::Selection)
                .collect::<Vec<_>>()
        );
        assert_eq!(model.selection, baseline);
        assert_eq!(model.active(), Some(WorkspaceId(selected)));
        assert!(!model.usage_open);
        assert!(
            model
                .workspaces
                .iter()
                .all(|w| !w.status.git_status_loading)
        );
    }
}
#[test]
fn maintenance_targets_only_active_owner_with_iced_visibility_and_loading_rules() {
    let now = Instant::now();
    for (focused, elapsed, loading, due) in [
        (true, 4999, false, false),
        (true, 5000, false, true),
        (false, 5000, false, false),
        (false, 15000, false, true),
        (true, 30000, true, false),
    ] {
        let mut model = model(now);
        let active = &mut model.workspaces[0];
        active.status.git_status_loading = loading;
        let mut baseline = active.status.clone();
        let at = now + Duration::from_millis(elapsed);
        let expected = baseline.reduce(
            StatusEvent::VisibilityChanged {
                focused,
                root: "/fixture/alpha".into(),
            },
            at,
        );
        let actual = model.dispatch(Intent::Maintenance { focused }, at);
        assert_eq!(actual, commands(1, expected));
        assert_eq!(actual.iter().filter(|c| matches!(c, Command::Status { command: StatusCommand::CollectStatus { root }, owner: 1 } if root == &PathBuf::from("/fixture/alpha"))).count(), usize::from(due));
        same_state(&model.workspaces[0].status, &baseline);
        assert!(!model.workspaces[1].status.git_status_loading);
    }
}
#[test]
fn git_entry_context_wake_and_completion_replay_keep_identical_core_state() {
    let now = Instant::now();
    let mut model = model(now);
    let mut baseline = model.workspaces[0].status.clone();
    let root = PathBuf::from("/fixture/alpha");
    let at = now + Duration::from_secs(1);
    let expected = baseline.reduce(
        StatusEvent::Request {
            root: root.clone(),
            prelude: RequestPrelude::Refresh,
        },
        at,
    );
    assert_eq!(model.dispatch(Intent::EnterGit, at), commands(1, expected));
    same_state(&model.workspaces[0].status, &baseline);
    // Forced requests overlap, including a context refresh. No generation guard.
    let root = PathBuf::from("/fixture/context");
    baseline.reduce(StatusEvent::Reset, at);
    let expected = baseline.reduce(
        StatusEvent::Request {
            root: root.clone(),
            prelude: RequestPrelude::Refresh,
        },
        at,
    );
    assert_eq!(
        model.dispatch(
            Intent::ContextChanged {
                workspace: WorkspaceId(1),
                root: root.clone()
            },
            at
        ),
        commands(1, expected)
    );
    same_state(&model.workspaces[0].status, &baseline);
    model.dispatch(Intent::Wake, at);
    baseline.reduce(StatusEvent::Wake, at);
    same_state(&model.workspaces[0].status, &baseline);
    assert_eq!(baseline.last_poll, at - Duration::from_secs(60));
    model.dispatch(Intent::SelectIndex(1), at); // Inactive completions still apply.
    for (branch, error) in [
        ("new", false),
        ("old", false),
        ("bad", true),
        ("good", false),
    ] {
        let mut value = snapshot(1, branch, error);
        value.repo_path = root.clone();
        baseline.reduce(StatusEvent::Completed(value.clone()), at);
        model.dispatch(Intent::Completed(value), at);
        same_state(&model.workspaces[0].status, &baseline);
    }
    assert_eq!(
        model
            .status_view(WorkspaceId(1))
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["staged.txt", "modified.txt", "new.txt"]
    );
    // A completion addressed to an absent owner has no effect.
    model.dispatch(Intent::Completed(snapshot(99, "late", false)), at);
    same_state(&model.workspaces[0].status, &baseline);
}
#[test]
fn unchanged_completions_preserve_iced_backoff_and_error_clock() {
    let now = Instant::now();
    let mut model = model(now);
    let mut baseline = model.workspaces[0].status.clone();
    for (streak, interval) in [
        5000, 5000, 5000, 10000, 10000, 10000, 10000, 10000, 10000, 15000,
    ]
    .into_iter()
    .enumerate()
    {
        let at = now + Duration::from_secs(streak as u64);
        let value = snapshot(1, "branch", false);
        baseline.reduce(StatusEvent::Completed(value.clone()), at);
        model.dispatch(Intent::Completed(value), at);
        same_state(&model.workspaces[0].status, &baseline);
        assert_eq!(model.workspaces[0].status.git_poll_interval_ms, interval);
    }
    let before = baseline.last_poll;
    model.dispatch(
        Intent::Completed(snapshot(1, "bad", true)),
        now + Duration::from_secs(20),
    );
    assert_eq!(model.workspaces[0].status.last_poll, before);
    assert_eq!(model.workspaces[0].status.branch_name, "branch");
}

#[test]
fn unpolled_workspace_is_not_checked_until_its_first_result() {
    let now = std::time::Instant::now();
    let mut model = WorkspaceStatus::default();
    let id = model.add("/fixture/repo".into(), now);
    assert!(!model.status_view(id).checked);
    model.dispatch(
        Intent::Completed(gitterm_core::status::GitStatusSnapshot {
            tab_id: id.0 as usize,
            repo_path: "/fixture/repo".into(),
            repo_name: "repo".into(),
            branch_name: "feature".into(),
            is_git_repo: true,
            staged: vec![],
            unstaged: vec![],
            untracked: vec![],
            error: None,
        }),
        now,
    );
    let view = model.status_view(id);
    assert!(view.checked);
    assert_eq!(view.branch, "feature");
}
