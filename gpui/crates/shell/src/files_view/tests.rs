use super::{FilesView, sample_items};
use crate::{
    FileOpen,
    data::LocalData,
    workspace_config::WorkspaceStore,
    workspace_status::{Intent, WorkspaceStatus},
};
use crate::{WorkspaceOwner, data::FileNode};
use gpui::base::actions::Confirm;
use gpui::base::actions::{SelectLeft, SelectRight};
use gpui::{Action, AppContext, Entity, TestAppContext, VisualTestContext};
use std::rc::Rc;
use std::{
    cell::RefCell,
    fs,
    path::Path,
    time::{Duration, Instant},
};

fn setup<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
    config: &Path,
) -> (
    Entity<FilesView>,
    Entity<WorkspaceOwner>,
    &'a mut VisualTestContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let mut model = WorkspaceStatus::default();
    let workspace = model.add(root.to_path_buf(), Instant::now());
    let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(WorkspaceStore::new(config)), cx));
    owner.update(cx, |owner, cx| {
        owner.dispatch(Intent::Activate(workspace), Instant::now(), cx)
    });
    let (view, cx) = cx
        .add_window_view(|_, cx| FilesView::new(workspace, Rc::new(LocalData(owner.clone())), cx));
    (view, owner, cx)
}
fn wait_for(view: &Entity<FilesView>, label: &str, cx: &mut VisualTestContext) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let found = view.read_with(cx, |view, cx| {
            let state = view.state.read(cx);
            (0..10_000)
                .map_while(|ix| state.entry(ix))
                .any(|entry| entry.item().label == label)
        });
        if found {
            return;
        }
        assert!(Instant::now() < deadline, "Tree did not publish {label}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn select(view: &Entity<FilesView>, label: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, cx| {
                let ix = (0..10_000)
                    .map_while(|ix| state.entry(ix).map(|entry| (ix, entry)))
                    .find(|(_, entry)| entry.item().label == label)
                    .unwrap()
                    .0;
                state.set_selected_index(Some(ix), cx);
                state.focus(window, cx);
            });
        });
        window.draw(cx).clear(cx);
    });
}
fn dispatch(action: impl Action, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(action), cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

// GPUI test platform only: no native windows or synthetic OS input.
#[gpui::test]
fn tree_actions_load_refresh_and_confirm_file(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("child")).unwrap();
    fs::write(root.path().join("child/file.txt"), "").unwrap();
    let (view, owner, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "child", cx);
    select(&view, "child", cx);
    dispatch(SelectRight, cx);
    wait_for(&view, "file.txt", cx);
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        assert_eq!(state.entry(1).unwrap().depth(), 1);
        assert!(state.entry(0).unwrap().is_expanded());
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&owner, move |_, event: &FileOpen, _| {
            received.borrow_mut().push(event.clone())
        })
    });
    select(&view, "file.txt", cx);
    dispatch(Confirm { secondary: false }, cx);
    assert_eq!(events.borrow().len(), 1);
    assert_eq!(events.borrow()[0].path, root.path().join("child/file.txt"));
    select(&view, "child", cx);
    dispatch(SelectLeft, cx);
    fs::write(root.path().join("child/later.txt"), "").unwrap();
    dispatch(SelectRight, cx);
    wait_for(&view, "later.txt", cx);
    view.read_with(cx, |view, cx| {
        assert_eq!(view.state.read(cx).selected_item().unwrap().label, "child")
    });
}

#[gpui::test]
fn tree_large_directory_has_actionable_show_more(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    for ix in 0..crate::files::PAGE_SIZE + 1 {
        fs::write(root.path().join(format!("file-{ix:04}")), "").unwrap();
    }
    let (view, _, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "Show more…", cx);
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        assert_eq!(
            state.entry(crate::files::PAGE_SIZE).unwrap().item().label,
            "Show more…"
        );
        assert!(state.entry(crate::files::PAGE_SIZE + 1).is_none());
    });
    select(&view, "Show more…", cx);
    dispatch(Confirm { secondary: false }, cx);
    wait_for(&view, "file-2000", cx);
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        assert_eq!(
            state.entry(crate::files::PAGE_SIZE).unwrap().item().label,
            "file-2000"
        );
        assert!(state.entry(crate::files::PAGE_SIZE + 1).is_none());
    });
}

#[test]
fn sample_tree_has_nested_items_and_initial_src_expansion() {
    let items = sample_items(
        vec![FileNode {
            name: "src".into(),
            children: vec![FileNode {
                name: "main.rs".into(),
                children: vec![],
            }],
        }],
        "",
    );
    assert!(items[0].is_expanded());
    assert_eq!(items[0].children[0].label, "main.rs");
}

#[test]
fn work_folders_keep_iced_repo_then_docs_order_without_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("docs");
    fs::create_dir(&docs).unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let repos = vec![
        first.display().to_string(),
        second.display().to_string(),
        first.display().to_string(),
    ];
    assert_eq!(
        super::work_roots(&repos, docs.clone()),
        vec![first.clone(), second.clone(), docs.clone()]
    );
    let mut repos = repos;
    repos.push(docs.display().to_string());
    assert_eq!(
        super::work_roots(&repos, docs.clone()),
        vec![first.clone(), second.clone(), docs]
    );
    assert_eq!(
        super::work_roots(&repos[..2], root.path().join("missing")),
        vec![first, second]
    );
}

#[gpui::test]
fn selecting_work_root_loads_its_tree_and_retains_it_after_workspace_refresh(
    cx: &mut TestAppContext,
) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::write(home.path().join("home.txt"), "home").unwrap();
    fs::write(repo.path().join("repo.txt"), "repo").unwrap();
    let (view, owner, cx) = setup(cx, home.path(), config.path());
    wait_for(&view, "home.txt", cx);
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.roots = Some(vec![repo.path().to_owned()]);
            view.browse_root(repo.path().to_owned(), cx);
        })
    });
    wait_for(&view, "repo.txt", cx);
    owner.update(cx, |owner, cx| {
        owner.dispatch(
            Intent::Activate(crate::data::WorkspaceId(1)),
            Instant::now(),
            cx,
        );
    });
    wait_for(&view, "repo.txt", cx);
    cx.read(|cx| {
        assert_eq!(
            owner.read(cx).files[&crate::data::WorkspaceId(1)].root,
            repo.path()
        );
    });
}
