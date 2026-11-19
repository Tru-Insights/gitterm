use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use gpui::{Image, ImageFormat};

use super::{
    Document, Origin,
    host::{SurfaceHost, allow_injected_document},
    overlay::*,
};

#[test]
fn native_navigation_accepts_only_one_host_injected_blank_document() {
    let pending = std::cell::Cell::new(true);
    assert!(allow_injected_document(&pending, Some("about:blank")));
    assert!(!allow_injected_document(&pending, Some("about:blank")));
    for url in [
        None,
        Some("file:///tmp/plan.md"),
        Some("https://example.test"),
        Some("custom:action"),
    ] {
        pending.set(true);
        assert!(!allow_injected_document(&pending, url));
        assert!(!allow_injected_document(&pending, Some("about:blank")));
    }
}

#[derive(Default)]
struct FixtureHost {
    events: Vec<&'static str>,
    requests: Vec<SnapshotTicket>,
    html: String,
    visible: bool,
    fail_capture: bool,
    fail_show: bool,
}

impl SurfaceHost for FixtureHost {
    fn load(&mut self, _: LoadTicket, document: &Document) -> Result<(), String> {
        self.events.push("load");
        self.html = document.html().into();
        Ok(())
    }
    fn show(&mut self, _: Geometry) -> Result<(), String> {
        self.events.push("show");
        if self.fail_show {
            return Err("fixture show failed".into());
        }
        self.visible = true;
        Ok(())
    }
    fn hide(&mut self) {
        self.events.push("hide");
        self.visible = false;
    }
    fn clear(&mut self) {
        self.events.push("clear");
        self.html.clear();
    }
    fn snapshot(&mut self, ticket: SnapshotTicket) -> Result<(), String> {
        assert!(self.visible, "capture must be initiated before hiding");
        self.events.push("snapshot");
        self.requests.push(ticket);
        if self.fail_capture {
            Err("fixture capture failed".into())
        } else {
            Ok(())
        }
    }
}

fn geometry(scale: f64) -> Geometry {
    Geometry {
        x: 10.,
        y: 40.,
        width: 100.,
        height: 80.,
        scale,
    }
}
fn origin(path: &str) -> Origin {
    Origin {
        workspace: 1,
        pane: 7,
        source: path.into(),
    }
}
fn document() -> Document {
    Document::local_file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/content/fixtures/viewer.html"),
        true,
    )
    .unwrap()
}
fn viewer() -> RetainedViewer<FixtureHost> {
    let mut viewer = RetainedViewer::new(FixtureHost::default());
    viewer.select(true, Some(geometry(2.)));
    let load = viewer.begin_content(origin("viewer.html"));
    assert!(viewer.complete_content(&load, Ok(document())));
    assert_eq!(viewer.state(), State::Loading);
    assert!(viewer.complete_navigation(&load, Ok(())));
    assert_eq!(viewer.state(), State::Visible);
    viewer
}
fn request(viewer: &mut RetainedViewer<FixtureHost>) -> SnapshotTicket {
    viewer.host_mut().requests.last().unwrap().clone()
}
fn snapshot(size: (u32, u32)) -> Result<Snapshot, String> {
    // Raster decoding belongs to GPUI; coordinator tests only need an owned image.
    Ok(Snapshot {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, vec![])),
        pixel_size: size,
    })
}

#[test]
fn retains_document_across_hide_snapshot_nested_dim_and_restore() {
    let mut viewer = viewer();
    viewer.select(false, Some(geometry(2.)));
    assert_eq!(viewer.state(), State::Hidden);
    viewer.select(true, Some(geometry(2.)));
    assert_eq!(viewer.state(), State::Visible);
    let now = Instant::now();
    let menu = viewer.open_cover(Cover::Menu, now);
    let ticket = request(&mut viewer);
    assert_eq!(viewer.state(), State::SnapshotPending);
    assert!(!viewer.host_mut().visible);
    let events = &viewer.host_mut().events;
    assert_eq!(&events[events.len() - 2..], ["snapshot", "hide"]);
    assert!(!viewer.dimmed());
    assert!(viewer.complete_snapshot(&ticket, snapshot((200, 160)), now));
    assert!(viewer.snapshot_image().is_some());
    let modal = viewer.open_cover(Cover::Modal, now);
    let picker = viewer.open_cover(Cover::Modal, now);
    assert!(viewer.dimmed());
    assert_eq!(viewer.host_mut().requests.len(), 1);
    assert_eq!(viewer.close_cover(picker, None), RestoreFocus::None);
    assert!(viewer.dimmed());
    assert_eq!(viewer.close_cover(modal, None), RestoreFocus::None);
    assert!(!viewer.dimmed());
    assert_eq!(viewer.state(), State::Covered);
    assert_eq!(
        viewer.close_cover(menu, Some(geometry(2.))),
        RestoreFocus::PriorOrActivePane
    );
    assert_eq!(viewer.state(), State::Visible);
    assert!(viewer.snapshot_image().is_none());
    assert_eq!(
        viewer
            .host_mut()
            .events
            .iter()
            .filter(|e| **e == "load")
            .count(),
        1
    );
    assert_eq!(viewer.close_cover(menu, None), RestoreFocus::None);
}

#[test]
fn capture_failure_timeout_and_wrong_scale_keep_controls_uncovered() {
    for failure in 0..4 {
        let mut viewer = viewer();
        viewer.host_mut().fail_capture = failure == 0;
        let now = Instant::now();
        let cover = viewer.open_cover(Cover::Modal, now);
        let ticket = request(&mut viewer);
        match failure {
            0 => {}
            1 => {
                viewer.complete_snapshot(&ticket, Err("fixture callback failed".into()), now);
            }
            2 => {
                viewer.tick(now + SNAPSHOT_TIMEOUT);
                assert!(!viewer.complete_snapshot(
                    &ticket,
                    snapshot((200, 160)),
                    now + SNAPSHOT_TIMEOUT
                ));
            }
            _ => {
                viewer.complete_snapshot(&ticket, snapshot((100, 80)), now);
            }
        }
        assert!(!viewer.host_mut().visible);
        assert_eq!(viewer.state(), State::Covered);
        assert!(viewer.error().is_some());
        assert!(viewer.snapshot_image().is_none());
        assert!(viewer.dimmed());
        viewer.close_cover(cover, Some(geometry(2.)));
        assert_eq!(viewer.state(), State::Visible);
    }
}

#[test]
fn stale_snapshot_cannot_cross_selection_resize_scale_reload_or_cover_cycles() {
    for change in 0..6 {
        let mut viewer = viewer();
        let now = Instant::now();
        let cover = viewer.open_cover(Cover::Menu, now);
        let stale = request(&mut viewer);
        match change {
            0 => viewer.select(false, Some(geometry(2.))),
            1 => viewer.select(
                true,
                Some(Geometry {
                    width: 150.,
                    ..geometry(2.)
                }),
            ),
            2 => viewer.select(true, Some(geometry(1.))),
            3 => {
                viewer.begin_content(origin("replacement.html"));
            }
            4 => {
                viewer.clear();
            }
            _ => {
                viewer.close_cover(cover, Some(geometry(2.)));
                viewer.open_cover(Cover::Menu, now);
            }
        }
        assert!(!viewer.complete_snapshot(&stale, snapshot((200, 160)), now));
        assert!(viewer.snapshot_image().is_none());
        assert!(!viewer.host_mut().visible);
    }
}

#[test]
fn tickets_cannot_cross_recreated_surface_with_identical_origin_and_counters() {
    let now = Instant::now();
    let mut old = viewer();
    old.open_cover(Cover::Menu, now);
    let stale = request(&mut old);
    let mut recreated = viewer();
    recreated.open_cover(Cover::Menu, now);
    assert!(!recreated.complete_snapshot(&stale, snapshot((200, 160)), now));
    let old_load = old.begin_content(origin("next.html"));
    recreated.begin_content(origin("next.html"));
    assert!(!recreated.complete_content(&old_load, Ok(document())));
}

#[test]
fn stale_render_and_duplicate_results_do_not_replace_selected_content() {
    let mut viewer = viewer();
    let old = viewer.begin_content(origin("old.html"));
    let new = viewer.begin_content(origin("new.html"));
    let cover = viewer.open_cover(Cover::Modal, Instant::now());
    assert!(!viewer.complete_content(&old, Ok(document())));
    assert!(viewer.complete_content(&new, Ok(document())));
    assert!(!viewer.complete_content(&new, Err("duplicate".into())));
    assert!(viewer.complete_navigation(&new, Ok(())));
    assert_eq!(viewer.state(), State::Covered);
    viewer.close_cover(cover, Some(geometry(2.)));
    assert_eq!(viewer.state(), State::Visible);
    let failure = viewer.begin_content(origin("missing.html"));
    assert!(viewer.complete_content(&failure, Err("fixture read failed".into())));
    assert_eq!(viewer.state(), State::Hidden);
    assert_eq!(viewer.error(), Some("fixture read failed"));
    assert!(!viewer.host_mut().visible);
}

#[test]
fn replacement_and_dock_motion_remove_snapshot_and_restore_only_on_final_close() {
    for kind in [Cover::Replacement, Cover::DockMotion] {
        let mut viewer = viewer();
        let now = Instant::now();
        let modal = viewer.open_cover(Cover::Modal, now);
        let ticket = request(&mut viewer);
        viewer.complete_snapshot(&ticket, snapshot((200, 160)), now);
        let replacement = viewer.open_cover(kind, now);
        assert!(viewer.snapshot_image().is_none());
        assert!(viewer.dimmed());
        viewer.close_cover(modal, Some(geometry(2.)));
        assert!(!viewer.host_mut().visible);
        viewer.close_cover(replacement, None);
        assert_eq!(viewer.state(), State::Hidden);
        viewer.select(true, Some(geometry(1.)));
        assert_eq!(viewer.state(), State::Visible);
    }
}

#[test]
fn native_navigation_is_required_and_stale_or_failed_navigations_cannot_show_or_capture() {
    let mut viewer = viewer();
    let now = Instant::now();
    let old = viewer.begin_content(origin("old.html"));
    assert!(viewer.complete_content(&old, Ok(document())));
    assert_eq!(viewer.state(), State::Loading);
    let requests = viewer.host_mut().requests.len();
    let cover = viewer.open_cover(Cover::Menu, now);
    assert_eq!(viewer.host_mut().requests.len(), requests);
    viewer.close_cover(cover, Some(geometry(2.)));
    assert!(!viewer.host_mut().visible);

    let current = viewer.begin_content(origin("new.html"));
    assert!(viewer.complete_content(&current, Ok(document())));
    assert!(!viewer.complete_navigation(&old, Ok(())));
    assert!(!viewer.complete_navigation(&old, Err("old cancellation".into())));
    assert_eq!(viewer.state(), State::Loading);
    let cover = viewer.open_cover(Cover::Modal, now);
    assert!(viewer.complete_navigation(&current, Ok(())));
    assert!(!viewer.complete_navigation(&current, Ok(())));
    assert_eq!(viewer.state(), State::Covered);
    assert!(!viewer.host_mut().visible);
    viewer.close_cover(cover, Some(geometry(2.)));
    assert_eq!(viewer.state(), State::Visible);

    let failed = viewer.begin_content(origin("failed.html"));
    assert!(viewer.complete_content(&failed, Ok(document())));
    assert!(viewer.complete_navigation(&failed, Err("fixture native navigation failed".into())));
    assert_eq!(viewer.state(), State::Hidden);
    assert_eq!(viewer.error(), Some("fixture native navigation failed"));
    assert!(!viewer.host_mut().visible);
    let closed = viewer.begin_content(origin("closed.html"));
    assert!(viewer.complete_content(&closed, Ok(document())));
    viewer.clear();
    assert!(!viewer.complete_navigation(&closed, Ok(())));
    assert_eq!(viewer.state(), State::Empty);
}

#[test]
fn modal_over_replacement_dims_once_without_restoring_retained_backdrop() {
    let mut viewer = viewer();
    let now = Instant::now();
    let replacement = viewer.open_cover(Cover::Replacement, now);
    assert!(!viewer.dimmed());
    let modal = viewer.open_cover(Cover::Modal, now);
    let picker = viewer.open_cover(Cover::Modal, now);
    assert!(viewer.dimmed());
    assert!(viewer.snapshot_image().is_none());
    assert!(!viewer.host_mut().visible);
    viewer.close_cover(picker, None);
    assert!(viewer.dimmed());
    viewer.close_cover(modal, None);
    assert!(!viewer.dimmed());
    assert!(!viewer.host_mut().visible);
    viewer.close_cover(replacement, Some(geometry(2.)));
    assert_eq!(viewer.state(), State::Visible);
}

#[test]
fn final_close_recomputes_bounds_and_respects_deselection_and_native_failure() {
    for hidden in 0..3 {
        let mut viewer = viewer();
        let cover = viewer.open_cover(Cover::Modal, Instant::now());
        if hidden == 0 {
            viewer.select(false, Some(geometry(2.)));
        }
        if hidden == 1 {
            viewer.host_mut().fail_show = true;
        }
        let bounds = (hidden != 2).then_some(geometry(2.));
        viewer.close_cover(cover, bounds);
        assert_eq!(viewer.state(), State::Hidden);
        assert!(!viewer.host_mut().visible);
    }
}

#[test]
fn scale_uses_window_points_and_rejects_invalid_or_empty_bounds() {
    assert_eq!(geometry(1.).pixel_size(), Some((100, 80)));
    assert_eq!(geometry(2.).pixel_size(), Some((200, 160)));
    assert_eq!(
        Geometry {
            width: 100.25,
            ..geometry(1.5)
        }
        .pixel_size(),
        Some((151, 120))
    );
    for invalid in [
        Geometry {
            width: 0.,
            ..geometry(2.)
        },
        Geometry {
            height: -1.,
            ..geometry(2.)
        },
        geometry(f64::NAN),
        geometry(0.),
        geometry(f64::INFINITY),
        geometry(f64::MAX),
        Geometry {
            x: f64::NAN,
            ..geometry(2.)
        },
    ] {
        assert!(invalid.pixel_size().is_none());
        let mut viewer = viewer();
        viewer.select(true, Some(invalid));
        assert_eq!(viewer.state(), State::Hidden);
        assert!(!viewer.host_mut().visible);
    }
}

#[test]
fn local_renderers_and_plan_root_switch_use_captured_roots() {
    assert!(document().html().contains("Local viewer fixture"));
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(
        first.path().join("plan.md"),
        include_str!("fixtures/plan.md"),
    )
    .unwrap();
    std::fs::write(second.path().join("plan.md"), "# Second workspace").unwrap();
    assert!(
        Document::plan(first.path(), "plan.md", true)
            .unwrap()
            .html()
            .contains("Local plan fixture")
    );
    assert!(
        Document::plan(second.path(), "plan.md", false)
            .unwrap()
            .html()
            .contains("Second workspace")
    );
    assert!(Document::plan(first.path(), "../plan.md", true).is_err());
    assert!(Document::plan(first.path(), "missing.md", true).is_err());
    std::os::unix::fs::symlink(
        second.path().join("plan.md"),
        first.path().join("escape.md"),
    )
    .unwrap();
    assert!(Document::plan(first.path(), "escape.md", true).is_err());
    assert!(Document::local_file(&PathBuf::from("/nonexistent/viewer.html"), true).is_err());
}

#[cfg(feature = "excalidraw")]
#[test]
fn optional_excalidraw_uses_shared_validation_and_renderer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drawing.excalidraw");
    std::fs::write(
        &path,
        r#"{"type":"excalidraw","elements":[],"appState":{}}"#,
    )
    .unwrap();
    assert!(
        Document::local_file(&path, true)
            .unwrap()
            .html()
            .contains("excalidraw")
    );
    std::fs::write(&path, "invalid").unwrap();
    assert!(Document::local_file(&path, true).is_err());
}
