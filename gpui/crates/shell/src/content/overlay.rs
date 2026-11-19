use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{Image, IntoElement, ParentElement, Styled, div, img, rgba};

use super::{Document, Origin, host::SurfaceHost};

pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(500);
static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(1);

/// Window-relative logical points; display scale affects raster dimensions only.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

impl Geometry {
    pub fn pixel_size(self) -> Option<(u32, u32)> {
        if ![self.x, self.y, self.width, self.height, self.scale]
            .iter()
            .all(|n| n.is_finite())
            || self.width <= 0.
            || self.height <= 0.
            || self.scale <= 0.
        {
            return None;
        }
        let w = (self.width * self.scale).ceil();
        let h = (self.height * self.scale).ceil();
        (w >= 1. && h >= 1. && w <= u32::MAX as f64 && h <= u32::MAX as f64)
            .then_some((w as u32, h as u32))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoverId(u64);

/// All window covers share this inventory. Nested modal sheets produce one dim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cover {
    Modal,
    Menu,
    Replacement,
    DockMotion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadTicket {
    surface: u64,
    generation: u64,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotTicket {
    pub load: LoadTicket,
    presentation: u64,
    pub geometry: Geometry,
}

#[derive(Clone)]
pub struct Snapshot {
    pub image: Arc<Image>,
    pub pixel_size: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Empty,
    Loading,
    Hidden,
    Visible,
    SnapshotPending,
    Covered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreFocus {
    None,
    PriorOrActivePane,
}

#[derive(PartialEq, Eq)]
enum LoadPhase {
    Preparing,
    Navigating,
    Ready,
    Failed,
}

/// Owns exactly one ephemeral native surface. The host never restores itself in
/// an async callback: all callbacks come back through these generation checks.
pub struct RetainedViewer<H: SurfaceHost> {
    host: H,
    surface: u64,
    load: Option<LoadTicket>,
    generation: u64,
    presentation: u64,
    next_cover: u64,
    covers: BTreeMap<CoverId, Cover>,
    geometry: Option<Geometry>,
    selected: bool,
    phase: LoadPhase,
    state: State,
    pending: Option<(SnapshotTicket, Instant)>,
    snapshot: Option<Snapshot>,
    error: Option<String>,
}

impl<H: SurfaceHost> RetainedViewer<H> {
    pub fn new(mut host: H) -> Self {
        host.hide();
        Self {
            host,
            surface: NEXT_SURFACE_ID
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("surface id exhausted"),
            load: None,
            generation: 0,
            presentation: 0,
            next_cover: 0,
            covers: BTreeMap::new(),
            geometry: None,
            selected: false,
            phase: LoadPhase::Failed,
            state: State::Empty,
            pending: None,
            snapshot: None,
            error: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Invalidates even same-path loads (reload/theme/plans-root changes).
    pub fn begin_content(&mut self, origin: Origin) -> LoadTicket {
        self.invalidate();
        self.generation = self
            .generation
            .checked_add(1)
            .expect("content generation exhausted");
        let ticket = LoadTicket {
            surface: self.surface,
            generation: self.generation,
            origin,
        };
        self.load = Some(ticket.clone());
        self.phase = LoadPhase::Preparing;
        self.error = None;
        self.state = State::Loading;
        ticket
    }

    pub fn complete_content(
        &mut self,
        ticket: &LoadTicket,
        document: Result<Document, String>,
    ) -> bool {
        if self.load.as_ref() != Some(ticket) || self.phase != LoadPhase::Preparing {
            return false;
        }
        match document.and_then(|document| self.host.load(ticket.clone(), &document)) {
            Ok(()) => self.phase = LoadPhase::Navigating,
            Err(error) => {
                self.phase = LoadPhase::Failed;
                self.error = Some(error);
            }
        }
        self.reconcile();
        true
    }

    /// Injection starts asynchronous WebKit navigation. Never show/capture the
    /// previous DOM as the new generation while waiting for its didFinish event.
    pub fn complete_navigation(&mut self, ticket: &LoadTicket, result: Result<(), String>) -> bool {
        if self.load.as_ref() != Some(ticket) || self.phase != LoadPhase::Navigating {
            return false;
        }
        match result {
            Ok(()) => self.phase = LoadPhase::Ready,
            Err(error) => {
                self.phase = LoadPhase::Failed;
                self.error = Some(error);
            }
        }
        self.reconcile();
        true
    }

    /// Hide without destroying the document or its DOM/scroll state.
    pub fn select(&mut self, selected: bool, geometry: Option<Geometry>) {
        if self.selected != selected || self.geometry != geometry {
            self.invalidate();
            self.selected = selected;
            self.geometry = geometry;
            self.reconcile();
        }
    }

    pub fn clear(&mut self) {
        self.invalidate();
        self.load = None;
        self.phase = LoadPhase::Failed;
        self.selected = false;
        self.error = None;
        self.host.clear();
        self.state = State::Empty;
    }

    /// Must run before the native overlay is drawn. Begin capture while visible,
    /// then hide synchronously even when capture fails or never calls back.
    pub fn open_cover(&mut self, cover: Cover, now: Instant) -> CoverId {
        self.next_cover = self.next_cover.checked_add(1).expect("cover id exhausted");
        let id = CoverId(self.next_cover);
        let capture = self.covers.is_empty()
            && self.state == State::Visible
            && matches!(cover, Cover::Modal | Cover::Menu);
        self.covers.insert(id, cover);
        if capture {
            let ticket = SnapshotTicket {
                load: self.load.clone().expect("visible content"),
                presentation: self.presentation,
                geometry: self.geometry.expect("visible geometry"),
            };
            let result = self.host.snapshot(ticket.clone());
            self.host.hide();
            match result {
                Ok(()) => {
                    self.pending = Some((ticket, now + SNAPSHOT_TIMEOUT));
                    self.state = State::SnapshotPending;
                }
                Err(error) => {
                    self.error = Some(error);
                    self.state = State::Covered;
                }
            }
        } else {
            if matches!(cover, Cover::Replacement | Cover::DockMotion) {
                self.invalidate();
            }
            self.host.hide();
            self.reconcile();
        }
        id
    }

    /// Geometry is freshly recomputed by the pane after its last cover closes.
    /// Unknown/duplicate closes cannot restore content or focus.
    pub fn close_cover(&mut self, id: CoverId, geometry: Option<Geometry>) -> RestoreFocus {
        if self.covers.remove(&id).is_none() {
            return RestoreFocus::None;
        }
        if self.covers.is_empty() {
            self.invalidate();
            self.geometry = geometry;
            self.reconcile();
            return RestoreFocus::PriorOrActivePane;
        }
        self.reconcile();
        RestoreFocus::None
    }

    pub fn complete_snapshot(
        &mut self,
        ticket: &SnapshotTicket,
        result: Result<Snapshot, String>,
        now: Instant,
    ) -> bool {
        let Some((expected, deadline)) = self.pending.as_ref() else {
            return false;
        };
        if expected != ticket {
            return false;
        }
        let timed_out = now >= *deadline;
        self.pending = None;
        if timed_out {
            self.error = Some("Retained content snapshot timed out".into());
        } else {
            match result {
                Ok(snapshot) if Some(snapshot.pixel_size) == ticket.geometry.pixel_size() => {
                    self.snapshot = Some(snapshot)
                }
                Ok(_) => self.error = Some("Retained content snapshot scale mismatch".into()),
                Err(error) => self.error = Some(error),
            }
        }
        self.reconcile();
        true
    }

    pub fn tick(&mut self, now: Instant) {
        if let Some((ticket, deadline)) = &self.pending
            && now >= *deadline
        {
            self.complete_snapshot(&ticket.clone(), Err("snapshot timed out".into()), now);
        }
    }

    pub fn snapshot_image(&self) -> Option<&Arc<Image>> {
        if !self.selected || self.replacing() {
            return None;
        }
        self.snapshot.as_ref().map(|snapshot| &snapshot.image)
    }

    pub fn dimmed(&self) -> bool {
        self.covers.values().any(|cover| *cover == Cover::Modal)
    }

    /// Purely painted backdrop: no mouse listeners, focus handle, or accessibility
    /// role. Controls and the single window dim layer are composed above it.
    pub fn backdrop(&self) -> impl IntoElement {
        let mut layer = div().size_full().bg(rgba(0x24242eff));
        if let Some(image) = self.snapshot_image() {
            layer = layer.child(img(image.clone()).size_full());
        }
        layer
    }

    pub fn dim_layer(&self) -> impl IntoElement {
        div().size_full().bg(rgba(if self.dimmed() {
            0x00000066
        } else {
            0x00000000
        }))
    }

    fn replacing(&self) -> bool {
        self.covers
            .values()
            .any(|cover| matches!(cover, Cover::Replacement | Cover::DockMotion))
    }

    fn invalidate(&mut self) {
        self.host.hide();
        self.presentation = self
            .presentation
            .checked_add(1)
            .expect("presentation generation exhausted");
        self.pending = None;
        self.snapshot = None;
    }

    fn reconcile(&mut self) {
        if self.load.is_none() {
            self.state = State::Empty;
            return;
        }
        if self.phase != LoadPhase::Ready {
            self.state = if matches!(self.phase, LoadPhase::Preparing | LoadPhase::Navigating) {
                State::Loading
            } else {
                State::Hidden
            };
        } else if !self.selected || self.geometry.and_then(Geometry::pixel_size).is_none() {
            self.state = State::Hidden;
        } else if !self.covers.is_empty() {
            self.state = if self.pending.is_some() {
                State::SnapshotPending
            } else {
                State::Covered
            };
        } else {
            match self.host.show(self.geometry.expect("valid geometry")) {
                Ok(()) => {
                    self.state = State::Visible;
                    return;
                }
                Err(error) => {
                    self.error = Some(error);
                    self.state = State::Hidden;
                }
            }
        }
        self.host.hide();
    }
}
