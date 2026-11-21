//! Window-owned local content surfaces. The dock owns identities, never native visibility.
use super::{
    Document, Origin,
    host::{LocalWebKitHost, NavigationResult, SnapshotResult, SurfaceHost},
    overlay::*,
};
use gitterm_core::page_store::PageStore;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, IntoElement as _, ParentElement as _,
    Styled as _, Subscription, Task, WeakFocusHandle, Window, canvas, div, img,
};
use gpui_component::{Root, WindowExt};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) fn supported(path: &Path) -> bool {
    path.file_name().is_some()
        && (cfg!(feature = "excalidraw") || path.extension().is_none_or(|e| e != "excalidraw"))
}

// Test-platform windows have no AppKit handle. Exercise the same coordinator
// with a host that completes navigation through the normal result queue.
struct WindowHost {
    native: Option<LocalWebKitHost>,
    navigation: Vec<NavigationResult>,
}
impl WindowHost {
    /// `page` selects a remote Page's login store; `None` is a local document.
    fn new(window: &Window, page: Option<&PageStore>) -> Result<Self, String> {
        #[cfg(test)]
        {
            let _ = (window, page);
            Ok(Self {
                native: None,
                navigation: vec![],
            })
        }
        #[cfg(not(test))]
        {
            Ok(Self {
                native: Some(LocalWebKitHost::new(window, page)?),
                navigation: vec![],
            })
        }
    }
    fn navigation(&mut self) -> Vec<NavigationResult> {
        if let Some(host) = &mut self.native {
            host.drain_navigation()
        } else {
            std::mem::take(&mut self.navigation)
        }
    }
    fn snapshots(&mut self) -> Vec<SnapshotResult> {
        self.native
            .as_mut()
            .map(|h| h.drain_snapshots())
            .unwrap_or_default()
    }
}
impl SurfaceHost for WindowHost {
    fn load(&mut self, ticket: LoadTicket, document: &Document) -> Result<(), String> {
        if let Some(host) = &mut self.native {
            host.load(ticket, document)
        } else {
            self.navigation.push((ticket, Ok(())));
            Ok(())
        }
    }
    fn show(&mut self, geometry: Geometry) -> Result<(), String> {
        self.native
            .as_mut()
            .map(|h| h.show(geometry))
            .unwrap_or(Ok(()))
    }
    fn hide(&mut self) {
        if let Some(h) = &mut self.native {
            h.hide();
        }
    }
    fn clear(&mut self) {
        if let Some(h) = &mut self.native {
            h.clear();
        }
        self.navigation.clear();
    }
    fn snapshot(&mut self, ticket: SnapshotTicket) -> Result<(), String> {
        self.native
            .as_mut()
            .map(|h| h.snapshot(ticket))
            .unwrap_or(Ok(()))
    }
}
enum Source {
    Local { root: PathBuf, relative: PathBuf },
    Page { url: String, store: PageStore },
}
struct Entry {
    source: Source,
    viewer: RetainedViewer<WindowHost>,
    covers: [Option<CoverId>; 3],
}
/// One inventory per window, including inactive workspaces. Covers are reconciled
/// before native callbacks and in pane prepaint, before any popup is painted.
pub(crate) struct ContentRequest {
    pub root: PathBuf,
    pub relative: PathBuf,
    pub workspace: u64,
    pub pane: u64,
    pub dark: bool,
}
pub struct ContentWindow {
    entries: HashMap<String, Entry>,
    pub(super) visits: super::session::ViewerVisits,
    pub selected: Option<String>,
    covered: [bool; 3],
    pub picker: bool,
    pub resizing: bool,
    prior_focus: Option<WeakFocusHandle>,
    fallback_focus: FocusHandle,
    focus_scope: Option<FocusHandle>,
    _clock: Task<()>,
    _popups: Subscription,
}
impl ContentWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let clock = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| {
                        if this.entries.is_empty() {
                            return;
                        }
                        this.sync_covers(window, cx);
                        let mut changed = false;
                        for entry in this.entries.values_mut() {
                            let before = entry.viewer.state();
                            if let Some(host) = &mut entry.viewer.host_mut().native {
                                for url in host.drain_external() {
                                    cx.open_url(&url);
                                }
                            }
                            for (ticket, result) in entry.viewer.host_mut().navigation() {
                                changed |= entry.viewer.complete_navigation(&ticket, result);
                            }
                            for (ticket, result) in entry.viewer.host_mut().snapshots() {
                                changed |=
                                    entry
                                        .viewer
                                        .complete_snapshot(&ticket, result, Instant::now());
                            }
                            entry.viewer.tick(Instant::now());
                            changed |= before != entry.viewer.state();
                        }
                        if changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        // Registration mutates GlobalState and delivers an effect before the
        // next paint, even if the shell/pane render is cached. Token destruction
        // does not notify, so the clock also reconciles final popup closure.
        let popups = cx.observe_global_in::<gpui::base::GlobalState>(window, |this, window, cx| {
            this.sync_covers(window, cx);
        });
        Self {
            entries: HashMap::new(),
            visits: HashMap::new(),
            selected: None,
            covered: [false; 3],
            picker: false,
            resizing: false,
            prior_focus: None,
            fallback_focus: cx.focus_handle(),
            focus_scope: None,
            _clock: clock,
            _popups: popups,
        }
    }
    pub(crate) fn open(
        &mut self,
        id: String,
        request: ContentRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ContentRequest {
            root,
            relative,
            workspace,
            pane,
            dark,
        } = request;
        if self.entries.contains_key(&id) {
            return;
        }
        let host = match WindowHost::new(window, None) {
            Ok(host) => host,
            Err(error) => {
                // Keep a normal failed entry so host failure remains inline.
                let mut viewer = RetainedViewer::new(WindowHost {
                    native: None,
                    navigation: vec![],
                });
                let ticket = viewer.begin_content(Origin {
                    workspace,
                    pane,
                    source: root.join(&relative),
                });
                viewer.complete_content(&ticket, Err(error));
                self.entries.insert(
                    id,
                    Entry {
                        source: Source::Local {
                            root: root.clone(),
                            relative: relative.clone(),
                        },
                        viewer,
                        covers: [None; 3],
                    },
                );
                cx.notify();
                return;
            }
        };
        let mut viewer = RetainedViewer::new(host);
        let ticket = viewer.begin_content(Origin {
            workspace,
            pane,
            source: root.join(&relative),
        });
        self.entries.insert(
            id.clone(),
            Entry {
                source: Source::Local {
                    root: root.clone(),
                    relative: relative.clone(),
                },
                viewer,
                covers: [None; 3],
            },
        );
        // The root/path/theme travel with the job; never consult active workspace at completion.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { Document::local_file(&root, &relative, dark) })
                .await;
            _ = this.update(cx, |this, cx| {
                if let Some(entry) = this.entries.get_mut(&id) {
                    entry.viewer.complete_content(&ticket, result);
                    cx.notify();
                }
            });
        })
        .detach();
        // Newly opened entries also inherit covers that predate their creation.
        self.apply_covers(window, cx);
    }
    /// Load `url` in the Page entry `id`. The same store reuses its surface;
    /// another store rebuilds it, since a WebKit view's store is fixed.
    pub(crate) fn open_page(
        &mut self,
        id: String,
        url: String,
        store: PageStore,
        origin: Origin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reuse = matches!(
            self.entries.get(&id),
            Some(Entry { source: Source::Page { store: current, .. }, .. }) if *current == store
        );
        let mut failed = None;
        if !reuse {
            if let Some(mut old) = self.entries.remove(&id) {
                old.viewer.clear();
            }
            let host = WindowHost::new(window, Some(&store)).unwrap_or_else(|error| {
                // Keep a normal failed entry so host failure remains inline.
                failed = Some(error);
                WindowHost {
                    native: None,
                    navigation: vec![],
                }
            });
            self.entries.insert(
                id.clone(),
                Entry {
                    source: Source::Page {
                        url: url.clone(),
                        store: store.clone(),
                    },
                    viewer: RetainedViewer::new(host),
                    covers: [None; 3],
                },
            );
        }
        let entry = self.entries.get_mut(&id).expect("Page entry inserted");
        let document = match failed {
            Some(error) => Err(error),
            None => Document::page(&url),
        };
        entry.source = Source::Page { url, store };
        let ticket = entry.viewer.begin_content(origin);
        entry.viewer.complete_content(&ticket, document);
        cx.notify();
        self.apply_covers(window, cx);
    }
    pub(super) fn source(&self, id: &str) -> Option<(PathBuf, PathBuf)> {
        match &self.entries.get(id)?.source {
            Source::Local { root, relative } => Some((root.clone(), relative.clone())),
            Source::Page { .. } => None,
        }
    }
    /// The URL and login store a Page entry was asked to show.
    pub(crate) fn page(&self, id: &str) -> Option<(&str, &PageStore)> {
        match &self.entries.get(id)?.source {
            Source::Page { url, store } => Some((url, store)),
            Source::Local { .. } => None,
        }
    }
    /// Whether the workspace's viewer cover currently shows this pane.
    pub(crate) fn is_visiting(&self, workspace: crate::WorkspaceId, pane: &str) -> bool {
        self.visits
            .get(&workspace)
            .is_some_and(|visit| visit.pane == pane)
    }
    pub fn close(&mut self, id: &str) {
        self.visits.retain(|_, visit| visit.pane != id);
        if let Some(mut entry) = self.entries.remove(id) {
            entry.viewer.clear();
        }
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
    }
    pub fn set_focus_target(&mut self, scope: FocusHandle, pane: FocusHandle) {
        self.focus_scope = Some(scope);
        self.fallback_focus = pane;
    }
    pub fn select(&mut self, id: Option<String>) {
        if self.selected != id {
            // Do not reuse geometry from another layout/workspace's previous frame.
            for entry in self.entries.values_mut() {
                entry.viewer.select(false, None);
            }
            self.selected = id;
        }
    }
    /// A menu, dialog, picker or drag is over native content.
    pub fn covered(&self) -> bool {
        self.covered.iter().any(|covered| *covered)
    }
    pub fn sync_covers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let modal = self.picker
            || (window.root::<Root>().flatten().is_some()
                && (window.has_active_dialog(cx) || window.has_active_sheet(cx)));
        let popup = gpui::base::GlobalState::is_in_deferred_context(cx);
        let motion = self.resizing || cx.has_active_drag();
        let next = [modal, popup, motion];
        if next == self.covered {
            return;
        }
        if !self.covered.iter().any(|x| *x) && next.iter().any(|x| *x) {
            self.prior_focus = window.focused(cx).map(|f| f.downgrade());
        }
        self.covered = next;
        self.apply_covers(window, cx);
        cx.notify();
    }
    fn apply_covers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut restore = false;
        for entry in self.entries.values_mut() {
            // Open new covers before removing old ones: menu -> dialog never flashes native content.
            for (index, kind) in [Cover::Modal, Cover::Menu, Cover::DockMotion]
                .into_iter()
                .enumerate()
            {
                if self.covered[index] && entry.covers[index].is_none() {
                    entry.covers[index] = Some(entry.viewer.open_cover(kind, Instant::now()));
                }
            }
            for index in 0..3 {
                if !self.covered[index]
                    && let Some(cover) = entry.covers[index].take()
                {
                    // Wait for fresh prepaint geometry before restoring a native surface.
                    entry.viewer.close_cover(cover, None);
                    restore = true;
                }
            }
        }
        if restore && !self.covered.iter().any(|x| *x) {
            if let Some(focus) = self
                .prior_focus
                .take()
                .and_then(|f| f.upgrade())
                .filter(|focus| {
                    self.focus_scope
                        .as_ref()
                        .is_none_or(|scope| scope.contains(focus, window))
                })
            {
                focus.focus(window, cx);
            } else {
                self.fallback_focus.focus(window, cx);
            }
        }
    }
    pub fn body(entity: &Entity<Self>, id: &str, cx: &App) -> AnyElement {
        let this = entity.read(cx);
        let Some(entry) = this.entries.get(id) else {
            return div().child("Content closed").into_any_element();
        };
        let image = entry.viewer.snapshot_image().cloned();
        let message = entry.viewer.error().map(str::to_owned).or_else(|| {
            (entry.viewer.state() == State::Loading).then(|| "Loading content…".into())
        });
        let id = id.to_owned();
        let entity = entity.clone();
        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .children(image.map(|image| img(image).size_full().absolute()))
            .children(message.map(|text| div().p_3().child(text)))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.sync_covers(window, cx);
                            if let Some(entry) = this.entries.get_mut(&id) {
                                let geometry = Geometry {
                                    x: f64::from(bounds.origin.x),
                                    y: f64::from(bounds.origin.y),
                                    width: f64::from(bounds.size.width),
                                    height: f64::from(bounds.size.height),
                                    scale: window.scale_factor() as f64,
                                };
                                entry
                                    .viewer
                                    .select(this.selected.as_ref() == Some(&id), Some(geometry));
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, IntoElement, Render, TestAppContext};
    #[::core::prelude::v1::test]
    fn retained_extensions_keep_feature_gate() {
        for path in ["a.md", "a.html", "a.htm", "A.MD", "A.HTML"] {
            assert!(supported(Path::new(path)));
        }
        for path in ["a.rs", "a.txt", "a.png", "README", "a.md.txt"] {
            assert!(supported(Path::new(path)));
        }
        assert_eq!(
            supported(Path::new("a.excalidraw")),
            cfg!(feature = "excalidraw")
        );
    }

    #[gpui::test]
    fn window_covers_reject_closed_surface_and_snapshot_completions(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.html"), "<h1>First</h1>").unwrap();
        let (entity, cx) = cx.add_window_view(|window, cx| {
            let entity = cx.new(|cx| ContentWindow::new(window, cx));
            TestView(entity)
        });
        let content = cx.read(|cx| entity.read(cx).0.clone());
        cx.update(|window, cx| {
            content.update(cx, |this, cx| {
                this.open(
                    "a".into(),
                    ContentRequest {
                        root: root.path().into(),
                        relative: "a.html".into(),
                        workspace: 1,
                        pane: 7,
                        dark: true,
                    },
                    window,
                    cx,
                );
                this.select(Some("a".into()));
                let entry = this.entries.get_mut("a").unwrap();
                let ticket = entry.viewer.begin_content(Origin {
                    workspace: 1,
                    pane: 0,
                    source: root.path().join("a.html"),
                });
                entry.viewer.complete_content(
                    &ticket,
                    Document::local_file(root.path(), Path::new("a.html"), true),
                );
                entry.viewer.complete_navigation(&ticket, Ok(()));
                let geometry = Geometry {
                    x: 0.,
                    y: 0.,
                    width: 100.,
                    height: 80.,
                    scale: 2.,
                };
                entry.viewer.select(true, Some(geometry));
                let scope = cx.focus_handle();
                let fallback = cx.focus_handle();
                let unmounted = cx.focus_handle();
                unmounted.focus(window, cx);
                this.set_focus_target(scope, fallback.clone());
                this.picker = true;
                this.sync_covers(window, cx);
                let old = this.entries["a"].viewer.pending_ticket_for_test();
                assert_eq!(this.entries["a"].viewer.state(), State::SnapshotPending);
                // A deferred popup nested over the picker must outlive picker cancellation.
                let popup = gpui::base::GlobalState::register_deferred_popover(cx);
                this.sync_covers(window, cx);
                this.picker = false;
                this.sync_covers(window, cx);
                assert_ne!(this.entries["a"].viewer.state(), State::Visible);
                drop(popup);
                this.sync_covers(window, cx);
                assert!(fallback.is_focused(window));
                let entry = this.entries.get_mut("a").unwrap();
                assert!(
                    !entry
                        .viewer
                        .complete_snapshot(&old, Err("late".into()), Instant::now())
                );
                assert_eq!(entry.viewer.state(), State::Hidden); // fresh prepaint required
                entry.viewer.select(true, Some(geometry));
                assert_eq!(entry.viewer.state(), State::Visible);
                this.close("a");
                assert!(!this.entries.contains_key("a"));
                this.open(
                    "a".into(),
                    ContentRequest {
                        root: root.path().into(),
                        relative: "a.html".into(),
                        workspace: 1,
                        pane: 7,
                        dark: true,
                    },
                    window,
                    cx,
                );
                assert!(
                    !this
                        .entries
                        .get_mut("a")
                        .unwrap()
                        .viewer
                        .complete_content(&ticket, Err("old load".into()))
                );
            })
        });
        let popup = cx.update(|_, cx| gpui::base::GlobalState::register_deferred_popover(cx));
        cx.run_until_parked();
        cx.read(|cx| assert!(content.read(cx).covered[1]));
        drop(popup);
    }
    struct TestView(Entity<ContentWindow>);
    impl Render for TestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
}
