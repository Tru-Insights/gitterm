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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
fn edit_click_requests_terminal_editor_without_launching_in_tests(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let requests = Rc::new(RefCell::new(Vec::new()));
    let received = requests.clone();
    let _editor_subscription = cx.update(|_, cx| {
        cx.subscribe(
            &view,
            move |_, request: &crate::content::actions::OpenEditor, _| {
                received.borrow_mut().push(request.clone())
            },
        )
    });
    click(cx, "files-edit-1");
    assert_eq!(
        *requests.borrow(),
        [crate::content::actions::OpenEditor::terminal(
            root.path().join("file.txt")
        )]
    );
    owner.read_with(cx, |owner, _| {
        assert!(owner.error.is_none());
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let back_row = cx.debug_bounds("files-all-folders").unwrap();
    assert_eq!(back.origin.x - list.origin.x, px(12.));
    assert_eq!(back.left(), back_row.left() + px(8.));
    assert_eq!(back.top(), back_row.top() + px(4.));
    assert_eq!(back_row.bottom(), back.bottom() + px(4.));
    let has_border = cx.update(|window, _| {
        let scaled = back_row.scale(window.scale_factor());
        window
            .painted_quads()
            .iter()
            .any(|quad| quad.bounds == scaled && quad.border_widths.any(|width| width.0 > 0.))
    });
    assert!(
        !has_border,
        "All folders must paint as a borderless ghost row"
    );
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
            "files_view::tests::edit_click_requests_terminal_editor_without_launching_in_tests",
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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

fn work_document(home: &Path, repos: &[&Path]) -> gitterm_core::workspace_config::WorkspacesFile {
    serde_json::from_value(serde_json::json!({
        "workspaces": [{ "name": "Fixture", "abbrev": "F", "color": "blue", "dir": home,
            "chats": ["owned"], "repos": repos, "tabs": [] }], "active_workspace": 0
    }))
    .unwrap()
}

#[gpui::test]
fn files_roots_follow_repositories_added_and_removed_during_session(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let first = home.path().join("first");
    let second = home.path().join("second");
    let docs = home.path().join("docs");
    for dir in [&first, &second, &docs] {
        fs::create_dir(dir).unwrap();
    }
    fs::write(first.join("file.txt"), "snapshot").unwrap();
    let (view, owner, cx) = setup(cx, home.path(), config.path());
    owner.update(cx, |owner, cx| {
        owner.apply_desktop_document(config.path(), work_document(home.path(), &[&first]), cx)
    });
    draw(cx);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.roots.as_ref().unwrap(),
            &[home.path().to_path_buf(), first.clone(), docs.clone()]
        )
    });
    view.update(cx, |view, cx| view.browse_root(first.clone(), cx));
    wait_for(&view, "file.txt", cx);
    owner.update(cx, |owner, cx| {
        owner.apply_desktop_document(
            config.path(),
            work_document(home.path(), &[&first, &second]),
            cx,
        )
    });
    draw(cx);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.roots.as_ref().unwrap(),
            &[
                home.path().to_path_buf(),
                first.clone(),
                second.clone(),
                docs.clone()
            ]
        );
        assert_eq!(view.browsing_root.as_ref(), Some(&first));
    });
    owner.update(cx, |owner, cx| {
        owner.apply_desktop_document(config.path(), work_document(home.path(), &[&second]), cx)
    });
    draw(cx);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.roots.as_ref().unwrap(),
            &[home.path().to_path_buf(), second, docs]
        );
        // Iced retains panel_repo_root until All folders is clicked.
        assert_eq!(view.browsing_root.as_ref(), Some(&first));
    });
    click(cx, "files-all-folders");
    assert!(cx.debug_bounds("files-root-0").is_some());
}

#[gpui::test]
fn files_hidden_entries_follow_saved_show_hidden_setting(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    for name in ["visible.txt", ".hidden"] {
        fs::write(root.path().join(name), "fixture").unwrap();
    }
    fs::create_dir(root.path().join(".hidden-dir")).unwrap();
    fs::create_dir(root.path().join("target")).unwrap();
    let (_, owner, cx) = setup(cx, root.path(), config.path());
    for show_hidden in [false, true] {
        let settings =
            serde_json::to_vec(&serde_json::json!({"show_hidden": show_hidden})).unwrap();
        fs::write(config.path().join("settings.json"), &settings).unwrap();
        owner.update(cx, |owner, cx| {
            owner.apply_desktop_document(config.path(), work_document(root.path(), &[]), cx)
        });
        let view = cx.new(|cx| {
            FilesView::new(
                crate::data::WorkspaceId(1),
                Rc::new(LocalData(owner.clone())),
                cx,
            )
        });
        wait_for(&view, "visible.txt", cx);
        let listed = names(&view, cx);
        assert_eq!(listed.contains(&".hidden".into()), show_hidden);
        assert_eq!(listed.contains(&".hidden-dir".into()), show_hidden);
        assert!(!listed.contains(&"target".into()));
        assert_eq!(
            fs::read(config.path().join("settings.json")).unwrap(),
            settings
        );
    }
}

#[gpui::test]
fn tab_context_gap3_files_directory_and_selection_return_with_the_tab(cx: &mut TestAppContext) {
    let _app = crate::test_app::Cleanup::new(cx);
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("child")).unwrap();
    fs::write(root.path().join("child/file.txt"), "fixture").unwrap();
    let (view, owner, cx) = setup(cx, root.path(), config.path());
    let first = crate::data::SessionId(41);
    let second = crate::data::SessionId(42);
    let workspace = crate::data::WorkspaceId(1);
    owner.update(cx, |owner, cx| {
        for id in [first, second] {
            owner.model.session_contexts.insert(
                id,
                crate::data::SessionContext {
                    cwd: root.path().into(),
                    terminal_dir: Some(root.path().into()),
                    repo: root.path().into(),
                    task_root: None,
                    task_context: false,
                    panel_repo: None,
                    follow: Default::default(),
                    observed_events: 0,
                    follow_listing: false,
                    follow_pending: Vec::new(),
                },
            );
        }
        owner.model.active_sessions.insert(workspace, first);
        cx.notify();
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.navigate(root.path().join("child"), cx);
        view.selected_file = Some(root.path().join("child/file.txt"));
    });
    owner.update(cx, |owner, cx| {
        assert_eq!(
            owner.model.session_contexts[&first].cwd,
            root.path().join("child")
        );
        owner.model.active_sessions.insert(workspace, second);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(view.directory, root.path());
        assert!(view.selected_file.is_none());
    });
    owner.update(cx, |owner, cx| {
        owner.model.active_sessions.insert(workspace, first);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(view.directory, root.path().join("child"));
        assert_eq!(view.selected_file, Some(root.path().join("child/file.txt")));
    });
}

#[gpui::test]
fn tab_context_gap3_directory_uses_v6_dir_in_live_and_never_writes_source_in_preview(
    cx: &mut TestAppContext,
) {
    let _app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let repo = root.path().join("repo");
    let child = repo.join("child");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&child).unwrap();
    let document = serde_json::json!({
        "future_document": true, "active_workspace": 0,
        "workspaces": [{ "name": "Fixture", "abbrev": "F", "color": "blue", "dir": repo,
            "future_workspace": 7, "tabs": [{"dir": repo, "repo_dir": repo, "session_uid": "fixture-caller", "future_tab": "kept"}] }]
    });
    let path = source.join("workspaces.json");
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();
    let catalog = crate::workspace_config::IcedCatalog::read(&source).unwrap();
    let model = WorkspaceStatus::from_iced(&catalog);
    let workspace = crate::data::WorkspaceId(1);
    let mut store = WorkspaceStore::new(&root.path().join("preview"));
    store.iced = Some(catalog);
    let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
    let (view, cx) = cx
        .add_window_view(|_, cx| FilesView::new(workspace, Rc::new(LocalData(owner.clone())), cx));
    cx.run_until_parked();
    // Browsing in Preview changes the runtime tab without touching Iced's bytes.
    view.update(cx, |view, cx| view.navigate(child.clone(), cx));
    cx.run_until_parked();
    assert_eq!(fs::read(&path).unwrap(), before);
    view.update(cx, |view, cx| view.navigate(repo.clone(), cx));
    let desktop = gitterm_core::desktop_store::DesktopStore::open(&source).unwrap();
    cx.update(|_, cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
    view.update(cx, |view, cx| view.navigate(child.clone(), cx));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if saved["workspaces"][0]["tabs"][0]["dir"] == child.to_str().unwrap() {
            assert_eq!(saved["future_document"], true);
            assert_eq!(saved["workspaces"][0]["future_workspace"], 7);
            assert_eq!(saved["workspaces"][0]["tabs"][0]["future_tab"], "kept");
            assert_eq!(
                saved["workspaces"][0]["tabs"][0]["repo_dir"],
                repo.to_str().unwrap()
            );
            break;
        }
        assert!(Instant::now() < deadline, "directory save did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    cx.update(|_, cx| {
        cx.remove_global::<crate::workspace_config::LiveWorkspaces>();
    });
}

#[gpui::test]
fn tab_context_gap5_chat_events_and_strip_toggle_wire_files_and_git(cx: &mut TestAppContext) {
    use crate::data::ShellData;
    use gitterm_core::chat::{runtime::RuntimeEvent, types::HarnessEvent};

    let _app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let output = gitterm_core::git_status::git_command()
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.com",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "fixture",
        "--allow-empty",
    ]);
    let linked = [temp.path().join("one"), temp.path().join("two")];
    for (path, branch) in linked.iter().zip(["one", "two"]) {
        git(&["worktree", "add", "-qb", branch, path.to_str().unwrap()]);
        fs::write(path.join(format!("{branch}.txt")), "fixture").unwrap();
    }
    let linked = linked.map(|path| path.canonicalize().unwrap());
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let config = temp.path().join("preview");
    let services = gitterm_core::preview::service::ServiceContext::production(
        &temp.path().join("live"),
        "worktree-follow-test",
    )
    .unwrap();
    cx.update(|cx| crate::chat::policy::install_live(services, cx).unwrap());
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let tabs = ["first", "second"].map(|uid| {
            serde_json::json!({
                "dir": repo, "repo_dir": repo, "tab_kind": "agent",
                "session_uid": uid,
                "agent_config": {"backend": "claude", "model": "default"},
            })
        });
        fs::write(
            source.join("workspaces.json"),
            serde_json::to_vec(&serde_json::json!({
                "workspaces": [{"name": "Fixture", "abbrev": "F", "color": "blue",
                    "dir": repo, "tabs": tabs}], "active_workspace": 0,
            }))
            .unwrap(),
        )
        .unwrap();
        let catalog = crate::workspace_config::IcedCatalog::read(&source).unwrap();
        let model = WorkspaceStatus::from_iced(&catalog);
        let mut store = crate::preview::tests::guarded_store(&config);
        store.iced = Some(catalog);
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        let mut shell = crate::Shell::with_owner(
            Rc::new(LocalData(owner.clone())),
            owner.clone(),
            &config,
            true,
            window,
            cx,
        )
        .unwrap();
        let sessions = shell.workspaces[0].sessions.clone();
        shell.restore_live_workspace_sessions(crate::WorkspaceId(1), window, cx);
        shell.select_session_id(sessions[0].id, window, cx);
        shell
    });
    let before = fs::read(source.join("workspaces.json")).unwrap();
    let (owner, sessions, chats) = cx.read(|cx| {
        let shell = shell.read(cx);
        let sessions = shell.workspaces[0].sessions.clone();
        let chats: Vec<_> = sessions
            .iter()
            .map(|session| {
                let pane = shell.workspaces[0].panes[&crate::session_pane_id(session.id)].read(cx);
                let crate::pane::PaneContent::PreviewChat(chat) = &pane.content else {
                    panic!("chat")
                };
                chat.clone()
            })
            .collect();
        for (session, chat) in sessions.iter().zip(&chats) {
            assert_eq!(
                chat.read(cx).tab_context(),
                Some((crate::WorkspaceId(1), session.id)),
            );
        }
        (shell.owner.clone(), sessions, chats)
    });
    let workspace = crate::data::WorkspaceId(1);
    let emit = |index: usize, cx: &mut VisualTestContext| {
        chats[index].update(cx, |chat, cx| {
            for event in [
                HarnessEvent::ItemStarted {
                    id: "read".into(),
                    kind: gitterm_core::chat::types::ItemKind::ToolCall {
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": linked[index].join("fixture.txt")}),
                    },
                },
                HarnessEvent::ItemCompleted {
                    id: "read".into(),
                    output: String::new(),
                    is_error: false,
                },
            ] {
                chat.receive_runtime_event(
                    &chat.runtime_source(),
                    RuntimeEvent::Harness(event),
                    cx,
                );
            }
        });
    };
    let owner_handle = owner.clone();
    let wait_context = |root: &Path, branch: &str, cx: &mut VisualTestContext| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            if owner.read_with(cx, |owner, cx| {
                let status = LocalData(owner_handle.clone()).status(workspace, cx);
                owner.model.workspaces[0].root == root
                    && status.checked
                    && status.branch == branch
                    && owner
                        .files
                        .get(&workspace)
                        .is_some_and(|tree| tree.root == root)
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Files/Git did not follow {}",
                root.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        draw(cx);
    };
    cx.run_until_parked();
    emit(0, cx);
    wait_context(&linked[0], "one", cx);
    emit(1, cx);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if owner.read_with(cx, |owner, _| {
            owner.model.session_contexts[&sessions[1].id]
                .follow
                .panel_root()
                == Some(linked[1].as_path())
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "background chat did not follow");
        std::thread::sleep(Duration::from_millis(10));
    }
    wait_context(&linked[0], "one", cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.select_session_id(sessions[1].id, window, cx)
        })
    });
    wait_context(&linked[1], "two", cx);
    click(cx, "pstrip-worktree");
    wait_context(&repo, "main", cx);
    click(cx, "pstrip-worktree");
    wait_context(&linked[1], "two", cx);
    owner.read_with(cx, |owner, _| {
        for session in &sessions {
            assert_eq!(owner.model.session_contexts[&session.id].cwd, repo);
        }
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.new_claude_chat(None, window, cx));
    });
    let (new_session, new_chat) = cx.read(|cx| {
        let shell = shell.read(cx);
        let id = shell.active(cx).active_session.unwrap();
        let pane = shell.active(cx).panes[&crate::session_pane_id(id)].read(cx);
        let crate::pane::PaneContent::PreviewChat(chat) = &pane.content else {
            panic!("new chat must be installed");
        };
        assert_eq!(chat.read(cx).tab_context(), Some((workspace, id)));
        assert!(!sessions.iter().any(|session| session.id == id));
        assert!(
            !shell
                .owner
                .read(cx)
                .iced_workspace(workspace)
                .unwrap()
                .tabs
                .iter()
                .any(|tab| tab.session_uid.as_deref() == Some(&chat.read(cx).target().session_uid))
        );
        (id, chat.clone())
    });
    new_chat.update(cx, |chat, cx| {
        chat.receive_runtime_event(
            &chat.runtime_source(),
            RuntimeEvent::Harness(HarnessEvent::ItemStarted {
                id: "unsaved-read".into(),
                kind: gitterm_core::chat::types::ItemKind::ToolCall {
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": linked[0].join("new.txt")}),
                },
            }),
            cx,
        );
    });
    wait_context(&linked[0], "one", cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.model.active_sessions[&workspace], new_session);
        assert_eq!(owner.model.session_contexts[&new_session].cwd, repo);
    });
    assert_eq!(fs::read(source.join("workspaces.json")).unwrap(), before);
}

#[gpui::test]
fn remote_parity_live_files_view_navigation_preserves_terminal_context(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (_root, owner, _fake) = crate::remote_client::source::tests::fixture(cx, true);
    let workspace = crate::data::WorkspaceId(1);
    let before = owner.read_with(cx, |owner, _| {
        owner
            .model
            .session_contexts
            .values()
            .next()
            .unwrap()
            .cwd
            .clone()
    });
    let view = cx.new(|cx| {
        FilesView::new(
            workspace,
            Rc::new(crate::data::LocalData(owner.clone())),
            cx,
        )
    });
    crate::remote_client::source::tests::wait(cx, |cx| {
        view.read(cx)
            .rows
            .iter()
            .any(|row| matches!(row, Row::Entry {entry, ..} if entry.name == "src"))
    });
    view.update(cx, |view, cx| view.navigate(before.join("src"), cx));
    crate::remote_client::source::tests::wait(cx, |cx| {
        view.read(cx)
            .rows
            .iter()
            .any(|row| matches!(row, Row::Entry {entry, ..} if entry.name == "example.rs"))
    });
    view.read_with(cx, |view, _| {
        assert_eq!(view.parent(), Some(before.clone()));
        assert_eq!(
            view.remote_navigation.as_ref().unwrap().display,
            "project/src/"
        );
    });
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.model.session_contexts.values().next().unwrap().cwd,
            before
        )
    });
}
