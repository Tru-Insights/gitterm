use super::*;
use gpui::{TestAppContext, VisualTestContext};
use gpui_component::diff::{DiffFileStatus, DiffGutterMarker, DiffLinePosition, DiffSide};
use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    time::{Duration, Instant},
};

fn model() -> ReviewModel {
    fixture::load_model(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/showcase"
    )))
    .unwrap()
}
fn setup(cx: &mut TestAppContext) -> (Entity<ReviewView>, &mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        init(cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| ReviewView::new(model(), window, cx));
        Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<ReviewView>().unwrap()
    });
    cx.simulate_resize(size(px(1200.), px(850.)));
    (view, cx)
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}
#[gpui::test]
fn removed_extra_bindings_leave_selection_and_viewed_marks_unchanged(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    draw(cx);
    let before = view.read_with(cx, |v, _| {
        (
            v.model.selected,
            v.model.files.iter().map(|f| f.viewed).collect::<Vec<_>>(),
        )
    });
    cx.simulate_keystrokes("alt-up alt-v alt-down");
    draw(cx);
    assert_eq!(
        view.read_with(cx, |v, _| (
            v.model.selected,
            v.model.files.iter().map(|f| f.viewed).collect::<Vec<_>>()
        )),
        before
    );
    let diff = view.read_with(cx, |v, _| v.diff.clone().unwrap());
    cx.update(|window, cx| diff.focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes("alt-down alt-v");
    draw(cx);
    assert_eq!(view.read_with(cx, |v, _| v.model.selected), before.0);
    // The existing native actions remain available to Iced-style controls.
    view.update_in(cx, |v, window, cx| v.next(&NextFile, window, cx));
    assert_eq!(view.read_with(cx, |v, _| v.model.selected), Some(1));
}

#[gpui::test]
fn edge_cases_use_real_diff_parser_and_large_button_handler(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    for (index, status, additions, deletions) in [
        (0, DiffFileStatus::Modified, 1, 1),
        (1, DiffFileStatus::Renamed, 0, 0),
        (4, DiffFileStatus::Deleted, 0, 1),
        (5, DiffFileStatus::Added, 1, 0),
    ] {
        view.update_in(cx, |v, window, cx| v.select(index, window, cx));
        draw(cx);
        view.read_with(cx, |v, cx| {
            assert!(v.diff_error.is_none(), "{:?}", v.diff_error);
            let file = &v.diff.as_ref().unwrap().read(cx).files()[0];
            assert_eq!(file.status(), status);
            assert_eq!((file.additions(), file.deletions()), (additions, deletions));
            if index == 0 {
                assert_eq!(file.language().as_str(), "rust");
            }
        });
    }
    view.update_in(cx, |v, window, cx| v.select(3, window, cx));
    draw(cx);
    assert!(view.read_with(cx, |v, _| v.diff.is_none() && v.model.files[3].is_binary()));
    view.update_in(cx, |v, window, cx| v.select(6, window, cx));
    draw(cx);
    assert!(view.read_with(cx, |v, _| v.diff.is_none() && v.model.files[6].collapsed()));
    view.update_in(cx, |v, window, cx| v.show_anyway(window, cx));
    draw(cx);
    view.read_with(cx, |v, cx| {
        assert!(v.diff_error.is_none(), "{:?}", v.diff_error);
        assert_eq!(
            v.diff.as_ref().unwrap().read(cx).files()[0].additions(),
            20_000
        );
    });
}

// Public marker slots let this probe count actual virtual row construction without
// relying on the component's private list internals or changing production code.
struct BudgetProbe {
    diff: Entity<DiffState>,
    painted: Rc<RefCell<HashSet<usize>>>,
}
impl Render for BudgetProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let painted = self.painted.clone();
        div().size_full().child(
            Diff::new(&self.diff)
                .gutter_markers((1..=20_000).map(|line| {
                    DiffGutterMarker::new(
                        ("budget", line),
                        DiffLinePosition::new("large.rs", DiffSide::Modified, line),
                    )
                }))
                .render_gutter_marker(move |marker, _, _| {
                    painted.borrow_mut().insert(marker.position().line());
                    div().child("·")
                }),
        )
    }
}
#[gpui::test]
fn twenty_thousand_line_parse_highlight_layout_and_virtual_row_budget(cx: &mut TestAppContext) {
    let start = Instant::now();
    cx.update(|cx| {
        gpui_component::init(cx);
        init(cx);
    });
    let painted = Rc::new(RefCell::new(HashSet::new()));
    let seen = painted.clone();
    // add_window_view drains every background task before returning. Use the
    // non-draining headless API to measure the UI path independently of syntect.
    let window = cx.open_window(size(px(1200.), px(850.)), move |window, cx| {
        let mut model = model();
        model.selected = Some(6);
        model.files[6].expanded = true;
        let view = cx.new(|cx| ReviewView::new(model, window, cx));
        let diff = view.read(cx).diff.clone().unwrap();
        BudgetProbe {
            diff,
            painted: seen,
        }
    });
    let parsed = start.elapsed();
    cx.update_window(*window, |_, window, cx| {
        _ = window.draw(cx);
    })
    .unwrap();
    let initial = start.elapsed();
    let visible = painted.borrow().len();
    eprintln!(
        "TRU-274 20k lines: parse {parsed:?}; first virtual layout {initial:?}; {visible} constructed rows"
    );
    assert!(
        initial < Duration::from_secs(5),
        "20k-line parse/first layout exceeded 5s: {initial:?}"
    );
    assert!(
        visible > 0 && visible < 100,
        "virtual renderer constructed {visible} rows"
    );
    // Jump while syntax is still pending: scrolling must not wait on coloring.
    let scroll_start = Instant::now();
    window
        .update(cx, |probe, _, cx| {
            probe.diff.update(cx, |diff, cx| {
                diff.scroll_to_line(
                    DiffLinePosition::new("large.rs", DiffSide::Modified, 20_000),
                    cx,
                );
            })
        })
        .unwrap();
    cx.update_window(*window, |_, window, cx| {
        _ = window.draw(cx);
    })
    .unwrap();
    assert!(
        painted.borrow().contains(&20_000),
        "tail line not painted after scroll"
    );
    assert!(painted.borrow().len() < 200);
    let scroll = scroll_start.elapsed();
    assert!(
        scroll < Duration::from_secs(1),
        "tail scroll blocked for {scroll:?}"
    );
    cx.run_until_parked();
    eprintln!(
        "TRU-274 20k lines: tail scroll {scroll:?}; background syntax complete at {:?}",
        start.elapsed()
    );
}
#[gpui::test]
fn malformed_patch_is_visible_error(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    view.update_in(cx, |v, _, cx| {
        v.model.files[0].patch = "not a patch".into();
        v.prepare(cx);
    });
    draw(cx);
    assert!(view.read_with(cx, |v, _| v.diff.is_none() && v.diff_error.is_some()));
}

#[gpui::test]
fn snapshot_replacement_clears_changed_viewed_mark_and_updates_rendered_diff(
    cx: &mut TestAppContext,
) {
    let (view, cx) = setup(cx);
    let (snapshot, mut patches) = fixture::load(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/showcase"
    )))
    .unwrap();
    view.update_in(cx, |v, window, cx| {
        v.toggle_viewed(&ToggleViewed, window, cx)
    });
    let key = (Group::Staged, "src/hello.rs".to_string());
    let updated = patches[&key].replace("hello review", "updated fixture");
    patches.insert(key, updated.into());
    view.update_in(cx, |v, _, cx| v.replace(&snapshot, &patches, cx).unwrap());
    draw(cx);
    view.read_with(cx, |v, cx| {
        assert!(!v.model.files[0].viewed);
        assert!(v.model.files[0].patch.contains("updated fixture"));
        assert!(v.diff_error.is_none());
        assert_eq!(v.diff.as_ref().unwrap().read(cx).files()[0].additions(), 1);
    });
}

#[gpui::test]
fn repo_backed_view_renders_real_files_and_hunks(cx: &mut TestAppContext) {
    let dir = crate::repo::tests::fixture();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (snapshot, patches) = executor
        .block_on(crate::repo::load(executor.handle(), 9, dir.path().into()))
        .unwrap();
    let model = ReviewModel::new(&snapshot, &patches).unwrap();
    cx.update(|cx| {
        gpui_component::init(cx);
        init(cx);
    });
    let (view, cx) = cx.add_window_view(|window, cx| ReviewView::new(model, window, cx));
    draw(cx);
    for (index, status, additions, deletions) in [
        (0, DiffFileStatus::Modified, 1, 1),
        (1, DiffFileStatus::Renamed, 0, 0),
        (2, DiffFileStatus::Modified, 1, 1),
        (3, DiffFileStatus::Added, 1, 0),
    ] {
        view.update_in(cx, |v, window, cx| v.select(index, window, cx));
        draw(cx);
        view.read_with(cx, |v, cx| {
            assert!(v.diff_error.is_none(), "{:?}", v.diff_error);
            let file = &v.diff.as_ref().unwrap().read(cx).files()[0];
            assert_eq!(file.path().as_str(), v.model.files[index].key.1);
            assert_eq!(file.status(), status);
            assert_eq!((file.additions(), file.deletions()), (additions, deletions));
        });
    }
}

#[gpui::test]
fn asynchronous_reload_keeps_marks_selection_and_rejects_stale_results(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    let (snapshot, patches) = fixture::load(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/showcase"
    )))
    .unwrap();
    view.update_in(cx, |v, window, cx| {
        v.toggle_viewed(&ToggleViewed, window, cx);
        let old = v.begin_load(cx);
        let current = v.begin_load(cx);
        v.select_file((Group::Unstaged, "src/hello.rs".into()), cx);
        v.finish_load(old, Err("stale error".into()), cx);
        assert!(v.is_loading());
        assert!(v.model.error.is_none());
        v.finish_load(current, Ok((snapshot, patches)), cx);
        assert!(!v.is_loading());
        assert!(v.model.files[0].viewed);
        assert_eq!(
            v.model.files[v.model.selected.unwrap()].key,
            (Group::Unstaged, "src/hello.rs".into())
        );
        let current = v.begin_load(cx);
        v.finish_load(current, Err("load failed for fixture".into()), cx);
        assert_eq!(v.model.error.as_deref(), Some("load failed for fixture"));
        assert!(!v.is_loading());
    });
    draw(cx);
}
