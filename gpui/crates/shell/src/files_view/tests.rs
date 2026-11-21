use super::{FilesView, Row};
use crate::{
    FileOpen, WorkspaceOwner,
    data::LocalData,
    workspace_config::WorkspaceStore,
    workspace_status::{Intent, WorkspaceStatus},
};
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext, px};
use std::{
    cell::RefCell,
    fs,
    path::Path,
    rc::Rc,
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
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::theme::apply(true, None, cx);
    });
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
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}
fn wait_for(view: &Entity<FilesView>, label: &str, cx: &mut VisualTestContext) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, _| {
            view.rows
                .iter()
                .any(|row| matches!(row, Row::Entry { entry, .. } if entry.name == label))
        }) {
            draw(cx);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Directory did not publish {label}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn bounds(cx: &mut VisualTestContext, selector: &str) -> gpui::Bounds<gpui::Pixels> {
    cx.debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .unwrap_or_else(|| panic!("missing {selector}"))
}
fn click(cx: &mut VisualTestContext, selector: &str) {
    let bounds = bounds(cx, selector);
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}
fn click_entry(view: &Entity<FilesView>, label: &str, cx: &mut VisualTestContext) {
    let ix = view.read_with(cx, |view, _| {
        view.rows
            .iter()
            .position(|row| matches!(row, Row::Entry { entry, .. } if entry.name == label))
            .unwrap()
    });
    click(cx, &format!("files-entry-{ix}"));
}
fn names(view: &Entity<FilesView>, cx: &mut VisualTestContext) -> Vec<String> {
    view.read_with(cx, |view, _| {
        view.rows
            .iter()
            .filter_map(|row| match row {
                Row::Entry {
                    entry,
                    expanded,
                    depth,
                } => {
                    assert!(!expanded, "no inline expansion");
                    assert_eq!(*depth, 0, "all entries are peers");
                    Some(entry.name.clone())
                }
                Row::More { .. } => panic!("Iced has no pager"),
                _ => None,
            })
            .collect()
    })
}

// All clicks use GPUI's headless platform, never native windows or OS input.
#[gpui::test]
fn folder_click_replaces_listing_and_parent_returns_without_inline_expansion(
    cx: &mut TestAppContext,
) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("child/nested")).unwrap();
    fs::write(root.path().join("child/file.txt"), "child").unwrap();
    fs::write(root.path().join("sibling.txt"), "sibling").unwrap();
    let (view, _, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "child", cx);
    assert_eq!(names(&view, cx), ["child", "sibling.txt"]);
    assert!(cx.debug_bounds("files-parent").is_none());
    click_entry(&view, "child", cx);
    wait_for(&view, "file.txt", cx);
    assert_eq!(names(&view, cx), ["nested", "file.txt"]);
    view.read_with(cx, |view, _| {
        assert_eq!(view.directory, root.path().join("child"))
    });
    click_entry(&view, "nested", cx);
    view.read_with(cx, |view, _| {
        assert_eq!(view.directory, root.path().join("child/nested"))
    });
    click(cx, "files-parent");
    wait_for(&view, "file.txt", cx);
    click(cx, "files-parent");
    wait_for(&view, "sibling.txt", cx);
    assert_eq!(names(&view, cx), ["child", "sibling.txt"]);
    assert!(cx.debug_bounds("files-parent").is_none());
}

#[gpui::test]
fn complete_directory_is_shown_without_paging(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    for ix in 0..crate::files::PAGE_SIZE + 1 {
        fs::write(root.path().join(format!("file-{ix:04}")), "").unwrap();
    }
    let (view, _, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "file-2000", cx);
    let names = names(&view, cx);
    assert_eq!(names.len(), crate::files::PAGE_SIZE + 1);
    assert_eq!(names.first().unwrap(), "file-0000");
    assert_eq!(names.last().unwrap(), "file-2000");
    assert!(!names.iter().any(|name| name.contains("Show more")));
}

#[gpui::test]
fn clicking_file_opens_it_and_selects_only_that_file(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::write(root.path().join("file.txt"), "file").unwrap();
    let (view, owner, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "file.txt", cx);
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&owner, move |_, event: &FileOpen, _| {
            received.borrow_mut().push(event.clone())
        })
    });
    click_entry(&view, "file.txt", cx);
    assert_eq!(events.borrow().len(), 1);
    assert_eq!(events.borrow()[0].path, root.path().join("file.txt"));
    assert!(events.borrow()[0].from_files);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.selected_file.as_deref(),
            Some(root.path().join("file.txt").as_path())
        )
    });
}

#[gpui::test]
fn edit_click_reports_preview_notice_without_open_or_launch(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("folder")).unwrap();
    fs::write(root.path().join("file.txt"), "unchanged").unwrap();
    let (view, owner, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "file.txt", cx);
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&owner, move |_, event: &FileOpen, _| {
            received.borrow_mut().push(event.clone())
        })
    });
    let before = owner.read_with(cx, |owner, _| {
        (owner.model.workspaces.len(), owner.chats.len())
    });
    assert!(
        cx.debug_bounds("files-edit-0").is_none(),
        "folders have no edit action"
    );
    click(cx, "files-edit-1");
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.error.as_deref(),
            Some("Edit file · not available in preview")
        );
        assert_eq!(
            owner.model.workspaces.len(),
            before.0,
            "no terminal/session was created"
        );
        assert_eq!(owner.chats.len(), before.1, "no agent process was created");
    });
    assert!(
        events.borrow().is_empty(),
        "edit does not invoke file-open subscribers"
    );
    view.read_with(cx, |view, _| assert!(view.selected_file.is_none()));
    assert_eq!(
        fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "unchanged"
    );
}

#[gpui::test]
fn roots_back_and_directory_rows_have_iced_insets_and_heights(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(repo.join("folder")).unwrap();
    fs::write(repo.join("file.txt"), "file").unwrap();
    let (view, _, cx) = setup(cx, root.path(), config.path());
    wait_for(&view, "repo", cx);
    view.update(cx, |view, cx| {
        view.roots = Some(vec![repo.clone(), root.path().to_path_buf()]);
        cx.notify();
    });
    draw(cx);
    let list = cx.debug_bounds("files-list").unwrap();
    for ix in 0..2 {
        let icon = bounds(cx, &format!("files-root-icon-{ix}"));
        let row = bounds(cx, &format!("files-root-{ix}"));
        assert_eq!(icon.origin.x - list.origin.x, px(16.));
        // Iced's 13px text × 1.3 line height + 8px padding, pixel-rounded.
        assert_eq!(row.size.height, px(25.));
    }
    click(cx, "files-root-0");
    wait_for(&view, "file.txt", cx);
    let back = cx.debug_bounds("files-all-folders-label").unwrap();
    assert_eq!(back.origin.x - list.origin.x, px(12.));
    for ix in 0..2 {
        let icon = bounds(cx, &format!("files-icon-{ix}"));
        let name = bounds(cx, &format!("files-name-{ix}"));
        let row = bounds(cx, &format!("files-entry-{ix}"));
        assert_eq!(icon.origin.x - list.origin.x, px(16.));
        assert_eq!(name.origin.x - list.origin.x, px(44.));
        // Iced's 13px text × 1.3 line height + 8px padding, pixel-rounded.
        assert_eq!(row.size.height, px(25.));
    }
    click_entry(&view, "folder", cx);
    draw(cx);
    let parent = cx.debug_bounds("files-parent-icon").unwrap();
    assert_eq!(parent.origin.x - list.origin.x, px(16.));
    click(cx, "files-all-folders");
    assert!(cx.debug_bounds("files-root-0").is_some());
    assert!(cx.debug_bounds("files-entry-0").is_none());
    assert!(cx.debug_bounds("files-all-folders").is_none());
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
fn selecting_work_root_retains_directory_after_workspace_refresh(cx: &mut TestAppContext) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::write(home.path().join("home.txt"), "home").unwrap();
    fs::create_dir(repo.path().join("child")).unwrap();
    fs::write(repo.path().join("child/repo.txt"), "repo").unwrap();
    let (view, owner, cx) = setup(cx, home.path(), config.path());
    wait_for(&view, "home.txt", cx);
    view.update(cx, |view, cx| {
        view.roots = Some(vec![repo.path().to_owned()]);
        view.browse_root(repo.path().to_owned(), cx);
    });
    wait_for(&view, "child", cx);
    click_entry(&view, "child", cx);
    wait_for(&view, "repo.txt", cx);
    owner.update(cx, |owner, cx| {
        owner.dispatch(
            Intent::Activate(crate::data::WorkspaceId(1)),
            Instant::now(),
            cx,
        )
    });
    wait_for(&view, "repo.txt", cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.files[&crate::data::WorkspaceId(1)].root,
            repo.path().join("child")
        )
    });
}

#[test]
fn edit_click_never_runs_configured_editor() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir_in("/private/tmp").unwrap();
    let editor = fixture.path().join("editor");
    let marker = fixture.path().join("editor-was-launched");
    fs::write(&editor, "#!/bin/sh\nprintf launched > \"$1\"\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
    // A separate headless test process isolates EDITOR from concurrent tests.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "files_view::tests::edit_click_reports_preview_notice_without_open_or_launch",
            "--test-threads=1",
        ])
        .env(
            "EDITOR",
            format!("{} {}", editor.display(), marker.display()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the click regression must run"
    );
    assert!(
        !marker.exists(),
        "the edit affordance must never launch EDITOR"
    );
}

#[gpui::test]
fn sample_folders_also_navigate_without_an_inline_tree(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|_, cx| {
        FilesView::new(
            crate::data::WorkspaceId(1),
            Rc::new(crate::data::SampleData),
            cx,
        )
    });
    draw(cx);
    assert!(names(&view, cx).contains(&"src".into()));
    assert!(!names(&view, cx).contains(&"client.rs".into()));
    click_entry(&view, "src", cx);
    assert_eq!(names(&view, cx), ["client.rs", "retry.rs"]);
    click(cx, "files-parent");
    assert!(names(&view, cx).contains(&"src".into()));
}

#[gpui::test]
fn mounted_files_pane_keeps_iced_insets_in_roots_and_directory(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::theme::apply(true, None, cx);
    });
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(repo.join("folder")).unwrap();
    fs::write(repo.join("file.txt"), "file").unwrap();
    let mut model = WorkspaceStatus::default();
    let workspace = model.add(root.path().to_owned(), Instant::now());
    let mut store = WorkspaceStore::new(config.path());
    store.iced = Some(crate::workspace_config::IcedCatalog {
        workspaces: vec![
            serde_json::from_value(serde_json::json!({
                "name": "Fixture", "abbrev": "F", "color": "blue",
                "dir": root.path(), "chats": ["owned"], "repos": [repo], "tabs": []
            }))
            .unwrap(),
        ],
        ..Default::default()
    });
    let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
    owner.update(cx, |owner, cx| {
        owner.dispatch(Intent::Activate(workspace), Instant::now(), cx)
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        crate::pane::Pane::new(
            "files-pane-fixture".into(),
            "Files".into(),
            crate::pane::PaneKind::Files,
            workspace,
            Rc::new(LocalData(owner.clone())),
            window,
            cx,
        )
    });
    draw(cx);
    assert_eq!(
        cx.debug_bounds("files-root-icon-0").unwrap().origin.x,
        px(16.)
    );
    click(cx, "files-root-0");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        draw(cx);
        if cx.debug_bounds("files-name-1").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "mounted directory did not load");
        std::thread::sleep(Duration::from_millis(10));
    }
    for ix in 0..2 {
        assert_eq!(bounds(cx, &format!("files-icon-{ix}")).origin.x, px(16.));
        assert_eq!(bounds(cx, &format!("files-name-{ix}")).origin.x, px(44.));
    }
    assert_eq!(
        cx.debug_bounds("files-all-folders-label").unwrap().origin.x,
        px(12.)
    );
    click(cx, "files-all-folders");
    assert_eq!(
        cx.debug_bounds("files-root-icon-0").unwrap().origin.x,
        px(16.)
    );
}
