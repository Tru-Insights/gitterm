use super::*;
use crate::{WorkspaceOwner, data::LocalData, workspace_status::WorkspaceStatus};
use ::core::prelude::v1::test;
use gpui::{TestAppContext, VisualTestContext};
use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    linked: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = dir.path().join("repo with spaces");
        let linked = dir.path().join("linked checkout");
        std::fs::create_dir(&root).unwrap();
        let this = Self { dir, root, linked };
        this.git(&["init", "-q", "-b", "main"]);
        for name in ["both.txt", "deleted.txt"] {
            std::fs::write(this.root.join(name), "before\n").unwrap();
        }
        this.git(&["add", "."]);
        this.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "fixture",
        ]);
        this.git(&["branch", "unused"]);
        this.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            this.linked.to_str().unwrap(),
        ]);
        std::fs::write(this.root.join("both.txt"), "staged\n").unwrap();
        std::fs::write(this.root.join("new.txt"), "added\n").unwrap();
        this.git(&["add", "."]);
        std::fs::write(this.root.join("both.txt"), "unstaged\n").unwrap();
        std::fs::remove_file(this.root.join("deleted.txt")).unwrap();
        std::fs::write(this.root.join("untracked.txt"), "new\n").unwrap();
        this
    }
    fn git(&self, args: &[&str]) {
        git(&self.root, args).unwrap();
    }
    fn model(&self) -> (WorkspaceStatus, WorkspaceId) {
        let mut model = WorkspaceStatus::default();
        let id = model.add(self.root.clone(), Instant::now());
        let status = gitterm_core::git_status::collect_repo_status(&self.root);
        let workspace = &mut model.workspaces[0];
        workspace.checked = true;
        workspace.status.is_git_repo = status.is_git_repo;
        workspace.status.branch_name = status.branch_name;
        workspace.status.staged = status.staged;
        workspace.status.unstaged = status.unstaged;
        workspace.status.untracked = status.untracked;
        (model, id)
    }
}

#[cfg(unix)]
#[test]
fn git_view_status_read_never_runs_fsmonitor_hook() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let hooks = tempfile::tempdir().unwrap();
    let marker = hooks.path().join("marker");
    let hook = hooks.path().join("hook.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\necho ran >> '{}'\nprintf '\\0'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.git(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
    let snapshot = collect(fixture.root.clone());
    assert!(snapshot.worktrees.iter().any(|row| row.available));
    assert!(!marker.exists(), "git view ran the fsmonitor hook");
    assert!(!fixture.root.join(".git/fsmonitor--daemon").exists());
    // Control: a plain status in the same repo does run the hook.
    gitterm_core::git_status::collect_repo_status(&fixture.root);
    assert!(marker.exists());
}

#[test]
fn fixture_changes_have_iced_order_labels_counts_and_badges() {
    let fixture = Fixture::new();
    let (model, id) = fixture.model();
    let status = model.status_view(id);
    let groups = sections(&status);
    assert_eq!(
        groups
            .iter()
            .map(|(group, files)| (group.label(), files.len()))
            .collect::<Vec<_>>(),
        [
            ("S T A G E D", 2),
            ("U N S T A G E D", 2),
            ("U N T R A C K E D", 1)
        ]
    );
    assert_eq!(
        groups
            .iter()
            .map(|(_, files)| files.iter().map(file_badge).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [vec!["M", "A"], vec!["M", "D"], vec!["?"]]
    );
    assert_eq!(groups[0].1[0].path, groups[1].1[0].path);
    assert!(groups[0].1[0].staged);
    assert!(!groups[1].1[0].staged);
    let mut empty = status;
    empty.files.clear();
    assert!(sections(&empty).is_empty());
}

#[test]
fn fixture_worktrees_and_branches_match_iced_sort_and_badges() {
    let fixture = Fixture::new();
    let snapshot = collect(fixture.root.clone());
    assert_eq!(snapshot.error, None);
    assert_eq!(
        snapshot.section_counts(),
        [("W O R K T R E E S", 2), ("B R A N C H E S", 3)]
    );
    assert_eq!(snapshot.worktrees.len(), 2);
    assert_eq!(
        snapshot
            .worktrees
            .iter()
            .map(|w| w.label())
            .collect::<Vec<_>>(),
        ["main", "linked"]
    );
    assert_eq!(snapshot.worktrees[0].badge(), "5 changed");
    assert_eq!(snapshot.worktrees[0].counts, [2, 2, 1]);
    assert!(snapshot.worktrees[0].current);
    assert_eq!(snapshot.worktrees[1].badge(), "clean");
    assert_eq!(snapshot.worktrees[1].path, fixture.linked);
    assert_eq!(
        snapshot
            .branches
            .iter()
            .map(|b| (b.name.as_str(), b.marker()))
            .collect::<Vec<_>>(),
        [("main", "◆"), ("linked", "◇"), ("unused", "∙")]
    );
    assert_eq!(snapshot.branches[1].worktree, Some(fixture.linked));
    let mut row = Worktree {
        detached: true,
        head: "1234567890".into(),
        ..Default::default()
    };
    assert_eq!(row.label(), "detached 1234567");
    assert_eq!(row.badge(), "unavailable");
    row.prunable = true;
    assert_eq!(row.badge(), "prunable");
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}
fn click(selector: &'static str, cx: &mut VisualTestContext) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    draw(cx);
}

// GPUI's mock test platform only; no native GUI or production config access.
#[gpui::test]
fn toggle_clicks_switch_and_row_click_emits_existing_file_open(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (model, workspace) = fixture.model();
    let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    cx.update(|cx| {
        cx.subscribe(&owner, move |_, event: &crate::FileOpen, _| {
            received.borrow_mut().push(event.clone())
        })
        .detach();
    });
    let (view, cx) =
        cx.add_window_view(|_, cx| GitView::new(workspace, Rc::new(LocalData(owner.clone())), cx));
    cx.simulate_resize(size(px(360.), px(900.)));
    draw(cx);
    assert_eq!(view.read_with(cx, |view, _| view.mode), Mode::Changes);
    click("git-file-Staged-both.txt", cx);
    click("git-file-Unstaged-both.txt", cx);
    assert_eq!(events.borrow().len(), 2);
    for (event, staged) in events.borrow().iter().zip([true, false]) {
        assert_eq!(event.workspace, workspace);
        assert_eq!(event.path, fixture.root.join("both.txt"));
        assert!(!event.from_files);
        assert_eq!(event.changed.as_ref().unwrap().staged, staged);
    }
    click("git-worktrees", cx);
    assert_eq!(view.read_with(cx, |view, _| view.mode), Mode::Worktrees);
    let deadline = Instant::now() + Duration::from_secs(15);
    while view.read_with(cx, |view, _| view.loading) {
        cx.run_until_parked();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    view.read_with(cx, |view, _| {
        let snapshot = view.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.worktrees.len(), 2);
        assert_eq!(snapshot.branches.len(), 3);
    });
    click("git-changes", cx);
    assert_eq!(view.read_with(cx, |view, _| view.mode), Mode::Changes);
    assert!(cx.debug_bounds("git-file-Staged-both.txt").is_some());
    assert_eq!(events.borrow().len(), 2);
    // Keep the fixture root alive until the background work completes.
    assert!(fixture.dir.path().is_dir());
}
