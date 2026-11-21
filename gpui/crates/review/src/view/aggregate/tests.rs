use super::*;
use gitterm_core::review::state::CollectStamp;
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
