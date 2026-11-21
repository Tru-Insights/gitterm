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
        assert_eq!(view.render_lines(0..10).len(), 10);
        for mut row in view.render_lines(0..3) {
            let style = row.text_style().clone();
            assert_eq!(style.font_family.as_deref(), Some("Menlo"));
            assert_eq!(style.font_size, Some(px(12.).into()));
        }
        assert!(
            view.selected_file().unwrap().shown_lines()
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
        assert!(!view.render_lines(0..10).is_empty());
    });
}

#[test]
fn aggregate_file_names_follow_iced_viewed_and_stale_styles() {
    use gpui::Styled as _;
    let style = |state| {
        let mut name = super::file_name("a.rs".into(), state);
        let text = &name.style().text;
        (text.color, text.strikethrough.is_some())
    };
    assert_eq!(style(ViewedState::Unviewed), (None, false));
    assert_eq!(
        style(ViewedState::Viewed),
        (Some(gpui::rgb(0x9399b2).into()), true)
    );
    assert_eq!(
        style(ViewedState::ChangedSinceViewed),
        (Some(gpui::rgb(0xf9e2af).into()), false)
    );
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
