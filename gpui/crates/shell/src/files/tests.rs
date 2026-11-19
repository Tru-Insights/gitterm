use super::*;
use std::fs;

fn finish(tree: &mut FileTree, request: Request) {
    let result = list(&request);
    tree.complete(Outcome { request, result });
}
fn names(tree: &FileTree) -> Vec<String> {
    tree.rows()
        .into_iter()
        .filter_map(|row| match row {
            Row::Entry { entry, .. } => Some(entry.name),
            _ => None,
        })
        .collect()
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let output = gitterm_core::git_status::git_command()
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    dir
}
#[test]
fn iced_visibility_and_sorting_in_temp_repo() {
    let dir = fixture();
    for name in ["z-dir", "Alpha", "target", "node_modules"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    for name in ["beta.txt", "a.txt", ".hidden", "ignored.txt"] {
        fs::write(dir.path().join(name), "x").unwrap();
    }
    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    let mut tree = FileTree::default();
    let request = tree.refresh(WorkspaceId(1), dir.path().to_path_buf());
    finish(&mut tree, request);
    // Iced has no gitignore filtering and defaults show_hidden=true.
    assert_eq!(
        names(&tree),
        [
            ".git",
            "Alpha",
            "z-dir",
            ".gitignore",
            ".hidden",
            "a.txt",
            "beta.txt",
            "ignored.txt"
        ]
    );
}
#[test]
fn expand_is_lazy_and_reexpand_refreshes_children() {
    let dir = fixture();
    let child = dir.path().join("child");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("first"), "").unwrap();
    let mut tree = FileTree::default();
    let request = tree.refresh(WorkspaceId(1), dir.path().to_path_buf());
    finish(&mut tree, request);
    assert!(!names(&tree).contains(&"first".into()));
    assert_eq!(tree.directories.len(), 1);
    let request = tree.toggle(WorkspaceId(1), child.clone()).unwrap();
    assert!(
        tree.rows()
            .iter()
            .any(|row| matches!(row, Row::Loading { depth: 1 }))
    );
    finish(&mut tree, request);
    assert!(names(&tree).contains(&"first".into()));
    assert!(tree.toggle(WorkspaceId(1), child.clone()).is_none());
    fs::write(child.join("second"), "").unwrap();
    let request = tree.toggle(WorkspaceId(1), child).unwrap();
    finish(&mut tree, request);
    assert!(names(&tree).contains(&"second".into()));
}
#[test]
fn large_directory_pages_in_sorted_order() {
    let dir = fixture();
    let child = dir.path().join("large");
    fs::create_dir(&child).unwrap();
    for i in 0..PAGE_SIZE + 3 {
        fs::write(child.join(format!("{i:04}")), "").unwrap();
    }
    let mut tree = FileTree::default();
    let request = tree.refresh(WorkspaceId(1), child.clone());
    finish(&mut tree, request);
    assert_eq!(names(&tree).len(), PAGE_SIZE);
    assert!(
        tree.rows()
            .iter()
            .any(|row| matches!(row, Row::More { .. }))
    );
    let request = tree.more(WorkspaceId(1), child).unwrap();
    finish(&mut tree, request);
    assert_eq!(names(&tree).len(), PAGE_SIZE + 3);
    assert_eq!(names(&tree)[PAGE_SIZE + 2], "2002");
    assert!(
        !tree
            .rows()
            .iter()
            .any(|row| matches!(row, Row::More { .. }))
    );
}
#[test]
fn removed_directory_reports_error_and_stale_results_are_discarded() {
    let dir = fixture();
    let child = dir.path().join("removed");
    fs::create_dir(&child).unwrap();
    let mut tree = FileTree::default();
    let request = tree.refresh(WorkspaceId(1), dir.path().to_path_buf());
    finish(&mut tree, request);
    let stale = tree.toggle(WorkspaceId(1), child.clone()).unwrap();
    let old_result = list(&stale);
    tree.toggle(WorkspaceId(1), child.clone());
    fs::remove_dir(&child).unwrap();
    let request = tree.toggle(WorkspaceId(1), child).unwrap();
    finish(&mut tree, request);
    tree.complete(Outcome {
        request: stale,
        result: old_result,
    });
    assert!(
        tree.rows().iter().any(
            |row| matches!(row, Row::Error { message, depth: 1 } if message.contains("removed"))
        )
    );
    assert!(
        !tree
            .rows()
            .iter()
            .any(|row| matches!(row, Row::Loading { .. }))
    );
}
#[test]
fn workspace_refresh_discards_expansion_and_old_completions() {
    let dir = fixture();
    let mut tree = FileTree::default();
    let stale = tree.refresh(WorkspaceId(1), dir.path().to_path_buf());
    let result = list(&stale);
    let request = tree.refresh(WorkspaceId(1), dir.path().join("missing"));
    finish(&mut tree, request);
    tree.complete(Outcome {
        request: stale,
        result,
    });
    assert!(names(&tree).is_empty());
    assert!(matches!(&tree.rows()[0], Row::Error { .. }));
}
#[test]
fn jobs_use_owners_runtime_and_publish_errors_without_panicking() {
    let dir = fixture();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut jobs = FileJobs::default();
    let mut tree = FileTree::default();
    let request = tree.refresh(WorkspaceId(1), dir.path().join("missing"));
    jobs.execute(runtime.handle(), request, sender);
    let outcome = runtime.block_on(async { receiver.recv().await.unwrap() });
    tree.complete(outcome);
    assert!(matches!(&tree.rows()[0], Row::Error { .. }));
}

#[gpui::test]
fn owner_bridges_selection_expansion_refresh_and_file_open(cx: &mut gpui::TestAppContext) {
    use crate::{
        FileOpen, WorkspaceOwner,
        workspace_config::WorkspaceStore,
        workspace_status::{Intent, WorkspaceStatus},
    };
    use gpui::AppContext;
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };
    cx.background_executor.allow_parking();
    let repo = fixture();
    let config = tempfile::tempdir().unwrap();
    let child = repo.path().join("child");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("nested.txt"), "content").unwrap();
    let mut model = WorkspaceStatus::default();
    let workspace = model.add(repo.path().to_path_buf(), Instant::now());
    model.workspaces[0]
        .status
        .untracked
        .push(gitterm_core::status::GitFileStatus {
            path: "child/nested.txt".into(),
            status: "?".into(),
            is_staged: false,
        });
    let owner =
        cx.new(|cx| WorkspaceOwner::new(model, Some(WorkspaceStore::new(config.path())), cx));
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner.dispatch(Intent::Activate(workspace), Instant::now(), cx)
        })
    });
    let wait = |cx: &mut gpui::TestAppContext, expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if cx
                .read(|cx| names(&owner.read(cx).files[&workspace]).contains(&expected.to_string()))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "owner did not publish {expected}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(cx, "child");
    assert!(
        !cx.read(|cx| names(&owner.read(cx).files[&workspace]).contains(&"nested.txt".to_string()))
    );
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner.toggle_directory(workspace, child.clone(), cx)
        })
    });
    wait(cx, "nested.txt");
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&owner, move |_, event: &FileOpen, _| {
            received.borrow_mut().push(event.clone())
        })
    });
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner.open_file(workspace, child.join("nested.txt"), cx)
        })
    });
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 1);
    assert_eq!(events.borrow()[0].path, child.join("nested.txt"));
    assert_eq!(
        events.borrow()[0].changed.as_ref().unwrap().path,
        "child/nested.txt"
    );
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner.open_file(workspace, repo.path().join("clean.txt"), cx)
        })
    });
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 2);
    assert!(events.borrow()[1].changed.is_none());
    fs::write(repo.path().join("later.txt"), "").unwrap();
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner.dispatch(Intent::SelectIndex(0), Instant::now(), cx)
        })
    });
    wait(cx, "later.txt");
    assert!(
        !cx.read(|cx| names(&owner.read(cx).files[&workspace]).contains(&"nested.txt".to_string()))
    );
}

#[test]
fn permission_denied_directory_is_a_pane_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let child = dir.path().join("private");
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o000)).unwrap();
    let mut tree = FileTree::default();
    let request = tree.refresh(WorkspaceId(1), child.clone());
    let result = list(&request);
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
    tree.complete(Outcome { request, result });
    assert!(tree.rows().iter().any(|row| matches!(row, Row::Error { message, .. } if message.contains("private") && message.contains("Permission denied"))));
}
