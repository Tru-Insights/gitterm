use super::*;
use crate::ReviewHost;
use gitterm_core::review::viewed::ViewedMarks;
use gitterm_core::{review::state::CollectStamp, turn_snapshots::SnapshotRoots};
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext, size};
use gpui_component::Root;

fn collection(effects: Vec<Effect>) -> CollectStamp {
    effects
        .into_iter()
        .find_map(|effect| {
            if let Effect::Collect(stamp) = effect {
                Some(stamp)
            } else {
                None
            }
        })
        .unwrap()
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn aggregate_result_draws_bounded_lines_limits_and_authoritative_marks(cx: &mut TestAppContext) {
    let dir = crate::fixture::aggregate_repo();
    std::fs::write(dir.path().join("large.txt"), "line\n".repeat(2200)).unwrap();
    let executor = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let (target, bases) = executor
        .block_on(crate::repo::aggregate_target(dir.path().into()))
        .unwrap();
    let mut model = AggregateModel::default();
    model.dispatch(Intent::MarksLoaded(Ok(ViewedMarks::default())));
    let stamp = collection(model.dispatch(Intent::Open {
        target,
        source: ReviewSource::Uncommitted,
        hide_noise: false,
        focus: None,
        page: AggregateModel::page(),
    }));
    let result = executor.block_on(crate::repo::collect_aggregate(stamp.clone()));
    model.dispatch(result);
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| AggregateView {
            model,
            selected: Some("large.txt".into()),
            ui: AggregateUi::default(),
            bases,
            error: None,
            executor,
            host: Default::default(),
        });
        Root::new(view, window, cx)
    });
    let view: Entity<AggregateView> =
        root.read_with(cx, |root, _| root.view().clone().downcast().unwrap());
    cx.simulate_resize(size(px(1200.), px(850.)));
    draw(cx);
    view.update_in(cx, |view, _, cx| {
        assert!(
            view.model
                .projection()
                .notices
                .iter()
                .any(|notice| notice.contains("truncated"))
        );
        let lines: Vec<_> = view
            .ui
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| matches!(row, AggregateRow::Line(_, _, _)).then_some(index))
            .take(10)
            .collect();
        assert_eq!(lines.len(), 10);
        for index in lines.into_iter().take(3) {
            let mut row = view.render_row(index, cx);
            let style = row.text_style().clone();
            assert_eq!(style.font_family.as_deref(), Some("Menlo"));
            assert_eq!(style.font_size, Some(px(12.).into()));
            assert_eq!(style.line_height, Some(px(18.).into()));
        }
        assert!(
            view.model
                .projection()
                .files
                .iter()
                .find(|row| row.file.path == "large.txt")
                .unwrap()
                .file
                .shown_lines()
                <= gitterm_core::review::budget::FILE_LINE_CAP
        );
        view.toggle_viewed(cx);
        assert!(
            view.model
                .projection()
                .files
                .iter()
                .any(|row| row.file.path == "large.txt" && row.viewed == ViewedState::Viewed)
        );
        view.dispatch(Intent::MarksLoaded(Ok(ViewedMarks::default())), cx);
        assert!(
            view.model
                .projection()
                .files
                .iter()
                .all(|row| row.viewed == ViewedState::Unviewed)
        );
        let mut stale = stamp;
        stale.generation += 1;
        view.dispatch(
            Intent::CollectFinished {
                stamp: stale,
                result: Box::new(Err("stale".into())),
            },
            cx,
        );
        assert!(view.model.state().pane().unwrap().error.is_none());
        assert_eq!(view.selected.as_deref(), Some("large.txt"));
        view.navigate_to(false, cx);
        assert_eq!(view.selected.as_deref(), Some("hello.rs"));
        view.navigate(true, cx);
        assert_eq!(view.selected.as_deref(), Some("hello.rs"));
        view.navigate_to(true, cx);
        assert_eq!(view.selected.as_deref(), Some("large.txt"));
        view.navigate(false, cx);
        assert_eq!(view.selected.as_deref(), Some("large.txt"));
    });
    draw(cx);
}

#[gpui::test]
fn base_selection_then_collection_replaces_the_native_file_projection(cx: &mut TestAppContext) {
    let dir = crate::fixture::aggregate_repo();
    let executor = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let (target, bases) = executor
        .block_on(crate::repo::aggregate_target(dir.path().into()))
        .unwrap();
    let mut model = AggregateModel::default();
    let stamp = collection(model.dispatch(Intent::Open {
        target,
        source: ReviewSource::Uncommitted,
        hide_noise: false,
        focus: None,
        page: AggregateModel::page(),
    }));
    model.dispatch(executor.block_on(crate::repo::collect_aggregate(stamp)));
    let intent = model
        .base_intent(
            bases
                .iter()
                .find(|base| base.reference == "release")
                .unwrap()
                .clone(),
        )
        .unwrap();
    collection(model.dispatch(intent));
    let stamp = collection(model.dispatch(Intent::Collect {
        page: AggregateModel::page(),
    }));
    let result = executor.block_on(crate::repo::collect_aggregate(stamp));
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| AggregateView {
            model,
            selected: None,
            ui: AggregateUi::default(),
            bases,
            error: None,
            executor,
            host: Default::default(),
        });
        Root::new(view, window, cx)
    });
    let view: Entity<AggregateView> =
        root.read_with(cx, |root, _| root.view().clone().downcast().unwrap());
    view.update_in(cx, |view, _, cx| view.dispatch(result, cx));
    draw(cx);
    view.read_with(cx, |view, _| {
        assert_eq!(view.selected.as_deref(), Some("hello.rs"));
        assert_eq!(
            view.model
                .state()
                .pane()
                .unwrap()
                .target
                .base
                .as_ref()
                .unwrap()
                .reference,
            "release"
        );
        assert!(
            view.ui
                .rows
                .iter()
                .any(|row| matches!(row, AggregateRow::Line(_, _, _)))
        );
    });
}

#[gpui::test]
fn aggregate_file_names_follow_iced_viewed_and_stale_styles(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let style = |state| {
            let mut name = super::file_name("a.rs".into(), state, cx);
            let text = &name.style().text;
            (text.color, text.strikethrough.is_some())
        };
        assert_eq!(style(ViewedState::Unviewed), (None, false));
        assert_eq!(
            style(ViewedState::Viewed),
            (Some(cx.theme().muted_foreground), true)
        );
        assert_eq!(
            style(ViewedState::ChangedSinceViewed),
            (Some(cx.theme().warning), false)
        );
    });
}

fn runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    )
}

fn open_view<'a>(
    cx: &'a mut TestAppContext,
    repo: &std::path::Path,
    host: ReviewHost,
) -> (Entity<AggregateView>, &'a mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let repo = repo.to_path_buf();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let view =
            cx.new(|cx| AggregateView::new(repo.clone(), ReviewSource::Uncommitted, host, cx));
        Root::new(view, window, cx)
    });
    let view: Entity<AggregateView> =
        root.read_with(cx, |root, _| root.view().clone().downcast().unwrap());
    wait_for(cx, &view, |view| {
        view.model
            .state()
            .pane()
            .is_some_and(|pane| pane.loaded.is_some())
    });
    (view, cx)
}

fn wait_for(
    cx: &mut VisualTestContext,
    view: &Entity<AggregateView>,
    ready: impl Fn(&AggregateView) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, _| ready(view)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "review did not settle"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[gpui::test]
fn edit_review_preserves_resolved_target_and_selects_and_scrolls_to_focus(cx: &mut TestAppContext) {
    let repo = crate::fixture::aggregate_repo();
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/first.rs"),
        "fn first() {}\n".repeat(120),
    )
    .unwrap();
    std::fs::create_dir(repo.path().join("tests")).unwrap();
    std::fs::write(repo.path().join("tests/focus.rs"), "fn focused() {}\n").unwrap();
    let (mut target, _) = runtime()
        .block_on(crate::repo::aggregate_target(repo.path().into()))
        .unwrap();
    target.owner = "fixture Work row".into();
    target.repo = "fixture owning repo".into();
    let expected = target.clone();
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let view = cx.new(|cx| {
            AggregateView::open_edit(
                EditReview {
                    target,
                    source: DiffSource::Uncommitted,
                    focus: "tests/focus.rs".into(),
                },
                ReviewHost::default(),
                cx,
            )
        });
        Root::new(view, window, cx)
    });
    let view: Entity<AggregateView> =
        root.read_with(cx, |root, _| root.view().clone().downcast().unwrap());
    cx.simulate_resize(size(px(1200.), px(500.)));
    wait_for(cx, &view, |view| {
        view.model
            .state()
            .pane()
            .is_some_and(|pane| pane.loaded.is_some())
    });
    draw(cx);
    view.read_with(cx, |view, _| {
        let pane = view.model.state().pane().unwrap();
        assert_eq!(pane.target, expected);
        assert_eq!(pane.source, ReviewSource::Uncommitted);
        assert_eq!(pane.focus.as_deref(), Some("tests/focus.rs"));
        assert!(!pane.hide_noise);
        assert_eq!(view.selected.as_deref(), Some("tests/focus.rs"));
        assert!(
            view.ui.list.logical_scroll_top().item_ix > 0,
            "focus must scroll past the first file: offset={:?}, headers={:?}",
            view.ui.list.logical_scroll_top(),
            view.ui
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| matches!(row, AggregateRow::Header(_)))
                .collect::<Vec<_>>()
        );
    });
}

fn saver(dir: &std::path::Path) -> crate::SaveMarks {
    let path = dir.join(gitterm_core::review::viewed::VIEWED_FILE);
    Arc::new(move |bytes| std::fs::write(&path, bytes).map_err(|error| error.to_string()))
}

fn viewed_of(view: &AggregateView, path: &str) -> ViewedState {
    view.model
        .projection()
        .files
        .iter()
        .find(|row| row.file.path == path)
        .unwrap()
        .viewed
}

/// The canonical checkout and content id Iced would have keyed a mark of `hello.rs` on.
fn mark_key(cx: &mut TestAppContext, repo: &std::path::Path) -> (String, String) {
    let (view, vcx) = open_view(cx, repo, ReviewHost::default());
    view.read_with(vcx, |view, _| {
        let (diff, canonical) = view.model.state().pane().unwrap().loaded.as_ref().unwrap();
        (
            canonical.to_string_lossy().into_owned(),
            diff.file("hello.rs").unwrap().content_id.clone().unwrap(),
        )
    })
}

#[gpui::test]
fn iced_marks_show_viewed_and_toggling_writes_only_the_preview_root(cx: &mut TestAppContext) {
    let repo = crate::fixture::aggregate_repo();
    let (checkout, id) = mark_key(cx, repo.path());
    let iced = tempfile::tempdir().unwrap();
    let preview = tempfile::tempdir().unwrap();
    let iced_file = iced.path().join(gitterm_core::review::viewed::VIEWED_FILE);
    std::fs::write(
        &iced_file,
        format!(r#"{{"checkouts":{{{checkout:?}:{{"hello.rs":{id:?}}}}}}}"#),
    )
    .unwrap();
    let before = std::fs::read(&iced_file).unwrap();
    let host = || ReviewHost {
        iced_marks_dir: Some(iced.path().into()),
        preview_marks_dir: Some(preview.path().into()),
        save_marks: Some(saver(preview.path())),
        ..Default::default()
    };
    {
        let (view, vcx) = open_view(cx, repo.path(), host());
        view.update_in(vcx, |view, _, cx| {
            assert_eq!(viewed_of(view, "hello.rs"), ViewedState::Viewed);
            view.selected = Some("hello.rs".into());
            view.toggle_viewed(cx);
            assert_eq!(viewed_of(view, "hello.rs"), ViewedState::Unviewed);
        });
    }
    assert_eq!(std::fs::read(&iced_file).unwrap(), before);
    assert!(preview.path().join("review-viewed.json").is_file());
    // Reopening keeps the cleared mark although Iced's file still has it.
    let (view, vcx) = open_view(cx, repo.path(), host());
    view.read_with(vcx, |view, _| {
        assert_eq!(viewed_of(view, "hello.rs"), ViewedState::Unviewed)
    });
    assert_eq!(std::fs::read(&iced_file).unwrap(), before);
}

#[gpui::test]
fn a_file_changed_since_it_was_marked_shows_changed_since_viewed(cx: &mut TestAppContext) {
    let repo = crate::fixture::aggregate_repo();
    let (checkout, _) = mark_key(cx, repo.path());
    let iced = tempfile::tempdir().unwrap();
    std::fs::write(
        iced.path().join(gitterm_core::review::viewed::VIEWED_FILE),
        format!(r#"{{"checkouts":{{{checkout:?}:{{"hello.rs":"older-content"}}}}}}"#),
    )
    .unwrap();
    let host = ReviewHost {
        iced_marks_dir: Some(iced.path().into()),
        ..Default::default()
    };
    let (view, cx) = open_view(cx, repo.path(), host);
    view.read_with(cx, |view, _| {
        assert_eq!(viewed_of(view, "hello.rs"), ViewedState::ChangedSinceViewed)
    });
}

#[gpui::test]
fn turns_list_from_a_snapshot_store_and_select_a_complete_one(cx: &mut TestAppContext) {
    let repo = crate::fixture::aggregate_repo();
    let store = tempfile::tempdir().unwrap();
    let roots =
        SnapshotRoots::new(store.path().join("turn-snapshots"), store.path().into()).unwrap();
    let runtime = runtime();
    for (turn, phase) in [(1, "start"), (1, "end"), (2, "start")] {
        runtime
            .block_on(gitterm_core::turn_snapshots::save(
                &roots,
                repo.path(),
                "chat-1",
                turn,
                phase,
                "HEAD",
            ))
            .unwrap();
    }
    let host = ReviewHost {
        snapshot_roots: Some(roots),
        active_chat: Some("chat-1".into()),
        ..Default::default()
    };
    let (view, cx) = open_view(cx, repo.path(), host);
    view.update_in(cx, |view, _, cx| {
        let intent = view
            .model
            .turns_intent(view.host.active_chat.clone())
            .unwrap();
        view.dispatch(intent, cx);
    });
    wait_for(cx, &view, |view| {
        view.model.state().pane().is_some_and(|pane| {
            matches!(pane.source, ReviewSource::Turns { .. }) && pane.loaded.is_some()
        })
    });
    view.update_in(cx, |view, _, cx| {
        let pane = view.model.state().pane().unwrap();
        let listed: Vec<_> = pane
            .turns
            .iter()
            .map(|turn| {
                (
                    turn.number,
                    turn_detail(turn).split(" · ").skip(1).collect::<String>(),
                )
            })
            .collect();
        // Newest first, as Iced lists them (the shared snapshot reader's order).
        assert_eq!(
            listed,
            [(2, "running or incomplete".to_string()), (1, String::new())]
        );
        view.select_turn(2, cx);
    });
    // An incomplete turn is refused by the reducer; the complete one opens.
    view.read_with(cx, |view, _| {
        assert!(matches!(
            view.model.state().pane().unwrap().source,
            ReviewSource::Turns { selected: None, .. }
        ))
    });
    view.update_in(cx, |view, _, cx| view.select_turn(1, cx));
    wait_for(cx, &view, |view| {
        matches!(
            view.model.state().pane().map(|pane| &pane.source),
            Some(ReviewSource::Turns {
                selected: Some(1),
                ..
            })
        ) && view.model.state().pane().unwrap().loaded.is_some()
    });
}

#[gpui::test]
fn opening_a_review_file_asks_the_host_to_open_its_worktree_path(cx: &mut TestAppContext) {
    let repo = crate::fixture::aggregate_repo();
    let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = opened.clone();
    let host = ReviewHost {
        open_file: Some(std::rc::Rc::new(move |path, _| {
            sink.borrow_mut().push(path)
        })),
        ..Default::default()
    };
    let (view, cx) = open_view(cx, repo.path(), host);
    view.update_in(cx, |view, _, cx| {
        view.selected = Some("hello.rs".into());
        cx.notify();
    });
    cx.run_until_parked();
    // Iced's per-file Open button (not the title) opens the worktree file.
    let open = cx
        .debug_bounds("aggregate-open")
        .expect("Open button is shown for a worktree file");
    cx.simulate_click(open.center(), gpui::Modifiers::none());
    view.read_with(cx, |view, _| assert!(view.error.is_none()));
    assert_eq!(*opened.borrow(), [repo.path().join("hello.rs")]);
}

#[gpui::test]
fn continuous_sections_render_each_file_and_viewed_collapses_only_its_body(
    cx: &mut TestAppContext,
) {
    let repo = crate::fixture::aggregate_repo();
    std::fs::write(repo.path().join("other.rs"), "fn other() {}\n").unwrap();
    let (view, cx) = open_view(cx, repo.path(), ReviewHost::default());
    cx.simulate_resize(size(px(1200.), px(850.)));
    draw(cx);
    let indices = view.read_with(cx, |view, _| {
        view.ui
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| matches!(row, AggregateRow::Header(_)).then_some(index))
            .collect::<Vec<_>>()
    });
    for index in indices {
        assert!(
            cx.debug_bounds(Box::leak(
                format!("aggregate-section-{index}").into_boxed_str()
            ))
            .is_some()
        );
    }
    view.update_in(cx, |view, _, cx| {
        let sections: Vec<_> = view
            .ui
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if let AggregateRow::Header(path) = row {
                    Some((index, path.clone()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            sections
                .iter()
                .map(|(_, path)| path.as_str())
                .collect::<Vec<_>>(),
            ["hello.rs", "other.rs"]
        );
        view.selected = Some("hello.rs".into());
        view.toggle_viewed(cx);
        assert!(
            view.ui
                .rows
                .contains(&AggregateRow::Header("hello.rs".into()))
        );
        assert!(
            !view
                .ui
                .rows
                .iter()
                .any(|row| matches!(row, AggregateRow::Line(path, _, _) if path == "hello.rs"))
        );
        assert!(
            view.ui
                .rows
                .iter()
                .any(|row| matches!(row, AggregateRow::Line(path, _, _) if path == "other.rs"))
        );
        view.toggle_viewed(cx);
        assert!(
            view.ui
                .rows
                .iter()
                .any(|row| matches!(row, AggregateRow::Line(path, _, _) if path == "hello.rs"))
        );
    });
}

#[test]
fn folder_tree_matches_iced_directories_first_and_nested_basename_grouping() {
    let tree = FileTree::from_paths(
        [
            "README.md",
            "src/main.rs",
            "src/view/one.rs",
            "tests/a.rs",
            "src/view/two.rs",
        ]
        .into_iter(),
    );
    assert_eq!(tree.files, ["README.md"]);
    assert_eq!(
        tree.dirs
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["src", "tests"]
    );
    let src = &tree.dirs[0].1;
    assert_eq!(src.files, ["src/main.rs"]);
    assert_eq!(src.dirs[0].0, "view");
    assert_eq!(src.dirs[0].1.files, ["src/view/one.rs", "src/view/two.rs"]);
}

#[gpui::test]
fn folder_controls_expand_and_collapse_their_children(cx: &mut TestAppContext) {
    let repo = crate::fixture::aggregate_repo();
    std::fs::create_dir_all(repo.path().join("src/view")).unwrap();
    std::fs::write(repo.path().join("src/view/one.rs"), "fn one() {}\n").unwrap();
    let (view, cx) = open_view(cx, repo.path(), ReviewHost::default());
    draw(cx);
    assert!(cx.debug_bounds("aggregate-folder-src/view/").is_some());
    let folder = cx.debug_bounds("aggregate-folder-src/").unwrap();
    cx.simulate_click(folder.center(), gpui::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("aggregate-folder-src/view/").is_none());
    view.read_with(cx, |view, _| {
        assert!(view.ui.closed_folders.contains("src/"))
    });
    let folder = cx.debug_bounds("aggregate-folder-src/").unwrap();
    cx.simulate_click(folder.center(), gpui::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("aggregate-folder-src/view/").is_some());
}

#[gpui::test]
fn noise_is_initially_hidden_and_remembered_per_checkout_without_recollection(
    cx: &mut TestAppContext,
) {
    let repo = crate::fixture::aggregate_repo();
    std::fs::create_dir(repo.path().join("tests")).unwrap();
    std::fs::write(repo.path().join("tests/example.rs"), "fn test() {}\n").unwrap();
    std::fs::write(repo.path().join("Cargo.lock"), "generated\n").unwrap();
    let (view, vcx) = open_view(cx, repo.path(), ReviewHost::default());
    view.update_in(vcx, |view, _, cx| {
        let projection = view.model.projection();
        assert!(projection.pane.unwrap().hide_noise);
        assert_eq!(projection.files.len(), 1);
        let generation = projection.pane.unwrap().generation;
        view.toggle_noise(cx);
        let projection = view.model.projection();
        assert_eq!(projection.pane.unwrap().generation, generation);
        assert_eq!(projection.files.len(), 3);
        for row in &projection.files {
            assert_eq!(
                row.file.kind,
                gitterm_core::review::model::classify_path(&row.file.path)
            );
        }
        view.dispatch(
            Intent::Collect {
                page: AggregateModel::page(),
            },
            cx,
        );
        assert!(!view.model.state().pane().unwrap().hide_noise);
    });
    let (reopened, vcx) = open_view(cx, repo.path(), ReviewHost::default());
    reopened.read_with(vcx, |view, _| {
        assert!(!view.model.state().pane().unwrap().hide_noise)
    });
    let other = crate::fixture::aggregate_repo();
    let (other, vcx) = open_view(cx, other.path(), ReviewHost::default());
    other.read_with(vcx, |view, _| {
        assert!(view.model.state().pane().unwrap().hide_noise)
    });
}

#[gpui::test]
fn aggregate_syntax_tokens_and_surfaces_follow_light_and_dark_appearance(cx: &mut TestAppContext) {
    // The default test text system emits no glyphs. Give its glyphs a
    // one-pixel raster so the headless scene records their paint colours.
    let mut font_cx = TestAppContext::build_with_text_system(
        cx.dispatcher.clone(),
        None,
        Arc::new(PaintTextSystem),
    );
    let cx = &mut font_cx;
    let repo = crate::fixture::aggregate_repo();
    let (view, cx) = open_view(cx, repo.path(), ReviewHost::default());
    cx.simulate_resize(size(px(1200.), px(850.)));
    let mut palettes = Vec::new();
    for mode in [
        gpui_component::ThemeMode::Light,
        gpui_component::ThemeMode::Dark,
    ] {
        cx.update(|_, cx| gpui_component::Theme::change(mode, None, cx));
        draw(cx);
        let (keyword, background, foreground) = view.read_with(cx, |_, cx| {
            (
                cx.theme()
                    .highlight_theme
                    .style("keyword")
                    .unwrap()
                    .color
                    .unwrap(),
                cx.theme().background,
                cx.theme().foreground,
            )
        });
        cx.update(|window, _| {
            let scene = window.rendered_scene();
            assert!(
                scene
                    .monochrome_sprites
                    .iter()
                    .any(|sprite| sprite.color == keyword)
                    || scene
                        .subpixel_sprites
                        .iter()
                        .any(|sprite| sprite.color == keyword),
                "language keyword colour must be painted"
            );
            assert!(
                scene
                    .quads
                    .iter()
                    .any(|quad| quad.background == background.into()),
                "appearance background must be painted"
            );
        });
        view.update_in(cx, |view, _, cx| {
            let line = view
                .ui
                .rows
                .iter()
                .position(|row| matches!(row, AggregateRow::Line(_, _, _)))
                .unwrap();
            let mut row = view.render_row(line, cx);
            assert_eq!(row.style().text.color, Some(foreground));
            for state in [ViewedState::Viewed, ViewedState::ChangedSinceViewed] {
                let mut name = file_name("hello.rs".into(), state, cx);
                assert_eq!(
                    name.style().text.color,
                    Some(if state == ViewedState::Viewed {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().warning
                    })
                );
            }
        });
        palettes.push((keyword, background, foreground));
    }
    assert_ne!(palettes[0], palettes[1]);
}

/// Keep GPUI's deterministic metrics, but emit a raster instead of suppressing
/// all text paint as NoopTextSystem does. No platform window or fonts needed.
struct PaintTextSystem;
impl gpui::PlatformTextSystem for PaintTextSystem {
    fn add_fonts(&self, fonts: Vec<std::borrow::Cow<'static, [u8]>>) -> gpui::Result<()> {
        gpui::NoopTextSystem.add_fonts(fonts)
    }
    fn all_font_names(&self) -> Vec<String> {
        gpui::NoopTextSystem.all_font_names()
    }
    fn font_id(&self, font: &gpui::Font) -> gpui::Result<gpui::FontId> {
        gpui::NoopTextSystem.font_id(font)
    }
    fn font_metrics(&self, id: gpui::FontId) -> gpui::FontMetrics {
        gpui::NoopTextSystem.font_metrics(id)
    }
    fn typographic_bounds(
        &self,
        font: gpui::FontId,
        glyph: gpui::GlyphId,
    ) -> gpui::Result<gpui::Bounds<f32>> {
        gpui::NoopTextSystem.typographic_bounds(font, glyph)
    }
    fn advance(&self, font: gpui::FontId, glyph: gpui::GlyphId) -> gpui::Result<gpui::Size<f32>> {
        gpui::NoopTextSystem.advance(font, glyph)
    }
    fn glyph_for_char(&self, font: gpui::FontId, ch: char) -> Option<gpui::GlyphId> {
        gpui::NoopTextSystem.glyph_for_char(font, ch)
    }
    fn glyph_raster_bounds(
        &self,
        _: &gpui::RenderGlyphParams,
    ) -> gpui::Result<gpui::Bounds<gpui::DevicePixels>> {
        Ok(gpui::Bounds::new(
            gpui::point(0.into(), 0.into()),
            size(1.into(), 1.into()),
        ))
    }
    fn rasterize_glyph(
        &self,
        _: &gpui::RenderGlyphParams,
        bounds: gpui::Bounds<gpui::DevicePixels>,
    ) -> gpui::Result<(gpui::Size<gpui::DevicePixels>, Vec<u8>)> {
        Ok((bounds.size, vec![255]))
    }
    fn layout_line(
        &self,
        text: &str,
        font_size: gpui::Pixels,
        runs: &[gpui::FontRun],
    ) -> gpui::LineLayout {
        gpui::NoopTextSystem.layout_line(text, font_size, runs)
    }
    fn recommended_rendering_mode(
        &self,
        font: gpui::FontId,
        size: gpui::Pixels,
    ) -> gpui::TextRenderingMode {
        gpui::NoopTextSystem.recommended_rendering_mode(font, size)
    }
}
