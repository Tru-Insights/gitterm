// Adapted from iced_term 261234b; see README.md and licenses/iced_term-MIT.
use crate::events::SessionState;
use crate::events::{Action, EventProxy};
use crate::session::Session;
use crate::settings::BackendSettings;
use crate::settings::LayoutSize as Size;
use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::grid::{BidirectionalIterator, Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{
    self, Term, TermMode, cell::Cell, test::TermSize, viewport_to_point,
};
use alacritty_terminal::tty;
use gpui_kit::Modifiers;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::RangeInclusive;
use std::sync::Arc;

const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#;

/// Escape special regex characters for literal string matching
pub(crate) fn escape_regex(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        match c {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Represents a search match in the terminal scrollback
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub start: Point,
    pub end: Point,
}

#[derive(Debug, Clone)]
pub enum Command {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Option<Size>, Option<Size>),
    SelectStart(SelectionType, (f32, f32)),
    SelectUpdate((f32, f32)),
    ClearSelection,
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
    ProcessAlacrittyEvent(Event),
}

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    num_cols: u16,
    num_lines: u16,
    layout_width: f32,
    layout_height: f32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1,
            cell_height: 1,
            num_cols: 80,
            num_lines: 50,
            layout_width: 80.0,
            layout_height: 50.0,
        }
    }
}

impl TerminalSize {
    fn for_grid(cols: usize, rows: usize, cell_width: u16, cell_height: u16) -> Self {
        let num_cols = cols.clamp(2, u16::MAX as usize) as u16;
        let num_lines = rows.clamp(1, u16::MAX as usize) as u16;
        Self {
            cell_width: cell_width.max(1),
            cell_height: cell_height.max(1),
            num_cols,
            num_lines,
            layout_width: f32::from(num_cols) * f32::from(cell_width.max(1)),
            layout_height: f32::from(num_lines) * f32::from(cell_height.max(1)),
        }
    }

    fn update(&mut self, layout_size: Option<Size>, font_measure: Option<Size>) {
        if let Some(size) = layout_size {
            self.layout_height = size.height;
            self.layout_width = size.width;
        }

        if let Some(size) = font_measure {
            self.cell_height = (size.height as u16).max(1);
            self.cell_width = (size.width as u16).max(1);
        }

        let lines = (self.layout_height / self.cell_height as f32).floor() as u16;
        let cols = (self.layout_width / self.cell_width as f32).floor() as u16;
        if lines > 0 && cols > 0 {
            self.num_lines = lines;
            self.num_cols = cols;
        }
    }

    fn same_terminal_geometry(&self, other: &Self) -> bool {
        self.cell_width == other.cell_width
            && self.cell_height == other.cell_height
            && self.num_cols == other.num_cols
            && self.num_lines == other.num_lines
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

pub struct Backend {
    pub(crate) term: Arc<FairMutex<Term<EventProxy>>>,
    pub(crate) integration: Option<tempfile::TempDir>,
    size: TerminalSize,
    applied_size: TerminalSize,
    session: Option<Session>,
    pending_settings: Option<BackendSettings>,
    startup_output_pending: bool,
    deferred_resize: Option<(usize, usize)>,
    pub(crate) events: EventProxy,
    pub(crate) service_busy: Arc<std::sync::atomic::AtomicBool>,
    find_results: Option<crate::SearchResults>,
    find_index: usize,
    pending_selection: Option<(Point, SelectionType, bool)>,
    pending_endpoint: Option<Point>,
    pending_selection_revision: u64,
    snapshot_revision: u64,
    pending_scroll: i32,
    pending_history: Option<usize>,
    pending_clear: bool,
    pending_bottom: std::sync::atomic::AtomicBool,
    pub(crate) last_content: RenderableContent,
    pub(crate) url_regex: RegexSearch,
}

impl Backend {
    pub fn new(settings: BackendSettings) -> Result<Self> {
        Self::new_sized(settings, 80, 50, 1, 1)
    }

    /// Spawn a shell with the geometry measured from its first view layout.
    pub(crate) fn new_sized(
        settings: BackendSettings,
        cols: usize,
        rows: usize,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<Self> {
        let size = TerminalSize::for_grid(cols, rows, cell_width, cell_height);
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(
                settings.program.clone(),
                settings.args.clone(),
            )),
            working_directory: settings.working_directory.clone(),
            env: settings.env.clone(),
            drain_on_exit: true,
            #[cfg(windows)]
            escape_args: true,
        };
        let pty = tty::new(&pty_config, size.into(), 278)?;
        #[cfg(unix)]
        let pty = crate::session::OwnedPty::new(pty);
        Self::with_pty_sized(settings, pty, size)
    }

    /// Construct the terminal state without starting a shell. The view starts
    /// the PTY once it has measured its first real layout.
    pub fn new_deferred(settings: BackendSettings) -> Self {
        let size = TerminalSize::default();
        let events = EventProxy::default();
        let term = Term::new(term::Config::default(), &size, events.clone());
        let mut initial_content = RenderableContent::default();
        initial_content.sync_from(&term, size);
        Self {
            term: Arc::new(FairMutex::new(term)),
            size,
            applied_size: size,
            session: None,
            integration: None,
            pending_settings: Some(settings),
            startup_output_pending: false,
            deferred_resize: None,
            events,
            service_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            find_results: None,
            find_index: 0,
            pending_selection: None,
            pending_endpoint: None,
            pending_selection_revision: 0,
            snapshot_revision: 0,
            pending_scroll: 0,
            pending_history: None,
            pending_clear: false,
            pending_bottom: std::sync::atomic::AtomicBool::new(false),
            last_content: initial_content,
            url_regex: RegexSearch::new(URL_REGEX).expect("invalid URL regexp"),
        }
    }

    pub(crate) fn initialize_deferred(
        &mut self,
        cols: usize,
        rows: usize,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<bool> {
        let Some(settings) = self.pending_settings.take() else {
            return Ok(false);
        };
        let size = TerminalSize::for_grid(cols, rows, cell_width, cell_height);
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(
                settings.program.clone(),
                settings.args.clone(),
            )),
            working_directory: settings.working_directory.clone(),
            env: settings.env.clone(),
            drain_on_exit: true,
            #[cfg(windows)]
            escape_args: true,
        };
        let pty = tty::new(&pty_config, size.into(), 278)?;
        #[cfg(unix)]
        let pty = crate::session::OwnedPty::new(pty);
        let config = term::Config {
            scrolling_history: settings.scrollback_lines,
            ..Default::default()
        };
        {
            let mut term = self.term.lock_unfair();
            *term = Term::new(config, &size, self.events.clone());
            self.events.invalidate();
        }
        self.size = size;
        self.applied_size = size;
        self.startup_output_pending = true;
        self.deferred_resize = None;
        self.last_content = RenderableContent::default();
        self.last_content.sync_from(&self.term.lock_unfair(), size);
        self.session = Some(Session::start(self.term.clone(), self.events.clone(), pty)?);
        Ok(true)
    }

    pub(crate) fn is_deferred(&self) -> bool {
        self.pending_settings.is_some()
    }

    pub(crate) fn fail_launch(&mut self, error: String) {
        self.pending_settings = None;
        self.record_error(error);
    }

    pub(crate) fn record_error(&self, error: String) {
        self.events.record_error(error);
    }

    #[cfg(test)]
    pub(crate) fn with_pty<T>(settings: BackendSettings, pty: T) -> Result<Self>
    where
        T: tty::EventedPty + OnResize + Send + 'static,
    {
        Self::with_pty_sized(settings, pty, TerminalSize::default())
    }

    fn with_pty_sized<T>(settings: BackendSettings, pty: T, size: TerminalSize) -> Result<Self>
    where
        T: tty::EventedPty + OnResize + Send + 'static,
    {
        let config = term::Config {
            scrolling_history: settings.scrollback_lines,
            ..Default::default()
        };
        let events = EventProxy::default();
        let term = Term::new(config, &size, events.clone());
        let mut initial_content = RenderableContent::default();
        initial_content.sync_from(&term, size);
        let term = Arc::new(FairMutex::new(term));
        let session = Session::start(term.clone(), events.clone(), pty)?;
        Ok(Self {
            term,
            size,
            applied_size: size,
            session: Some(session),
            integration: None,
            pending_settings: None,
            startup_output_pending: false,
            deferred_resize: None,
            events,
            service_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            find_results: None,
            find_index: 0,
            last_content: initial_content,
            pending_selection: None,
            pending_endpoint: None,
            pending_selection_revision: 0,
            snapshot_revision: 0,
            pending_scroll: 0,
            pending_history: None,
            pending_clear: false,
            pending_bottom: std::sync::atomic::AtomicBool::new(false),
            url_regex: RegexSearch::new(URL_REGEX).expect("invalid URL regexp"),
        })
    }

    pub fn select(&mut self, point: Point, kind: SelectionType, extend: bool) {
        self.pending_selection_revision = self.snapshot_revision;
        self.pending_selection = Some((point, kind, extend));
        self.pending_endpoint = None;
        self.events.mark_dirty();
    }
    pub fn extend_selection(&mut self, point: Point) {
        if self.pending_selection.is_none() {
            self.pending_selection_revision = self.snapshot_revision;
        }
        self.pending_endpoint = Some(point);
        self.events.mark_dirty();
    }
    pub fn set_history_limit(&mut self, lines: usize) {
        self.pending_history = Some(lines);
        self.events.mark_dirty();
    }
    fn apply_pending(&mut self, term: &mut Term<EventProxy>) {
        if self.pending_history.is_some() || self.pending_clear {
            self.events.invalidate();
        }
        if let Some(lines) = self.pending_history.take() {
            term.set_options(term::Config {
                scrolling_history: lines,
                ..Default::default()
            });
        }
        if std::mem::take(&mut self.pending_clear) {
            use alacritty_terminal::vte::ansi::Processor;
            let mut parser: Processor = Processor::new();
            parser.advance(term, b"\x1b[3J\x1b[H\x1b[2J");
            term.selection = None;
        }
        if self.pending_selection_revision != self.revision() {
            self.pending_selection = None;
            self.pending_endpoint = None;
        }
        if let Some((point, kind, extend)) = self.pending_selection.take() {
            if extend && let Some(selection) = term.selection.as_mut() {
                selection.update(point, Side::Right);
            } else {
                term.selection = Some(Selection::new(kind, point, Side::Left));
            }
        }
        if let Some(point) = self.pending_endpoint.take()
            && let Some(selection) = term.selection.as_mut()
        {
            selection.update(point, Side::Right);
        }
        if self
            .pending_bottom
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            term.scroll_display(Scroll::Bottom);
        }
        let delta = std::mem::take(&mut self.pending_scroll);
        self.scroll(term, delta);
    }
    pub fn resize_cells(&mut self, cols: usize, rows: usize) {
        if self.startup_output_pending && !self.events.output_ready() {
            self.deferred_resize = Some((cols, rows));
            return;
        }
        self.size.num_cols = cols.clamp(2, u16::MAX as usize) as u16;
        self.size.num_lines = rows.clamp(1, u16::MAX as usize) as u16;
        if !self.size.same_terminal_geometry(&self.applied_size) {
            self.events.mark_dirty();
        }
    }
    pub fn clear_screen(&mut self) {
        self.pending_clear = true;
        self.events.mark_dirty();
    }

    pub fn needs_sync(&self) -> bool {
        self.events.is_dirty()
    }
    pub fn take_events(&self) -> crate::events::PendingEvents {
        self.events.take()
    }
    pub fn mode(&self) -> TermMode {
        self.last_content.terminal_mode
    }
    pub fn dimensions(&self) -> (usize, usize) {
        (self.size.columns(), self.size.screen_lines())
    }

    /// The same mode-aware path is used by keyboard, menu, and programmatic paste.
    pub fn paste(&self, text: &str) -> bool {
        if self.state() != SessionState::Running {
            return false;
        }
        // Reject before normalization/copying; foreground work and allocation are bounded.
        if text.len() > crate::session::INPUT_LIMIT.saturating_sub(12) {
            self.events.reject_input(true);
            return false;
        }
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        // Strip ESC so clipboard content cannot close the bracketed-paste envelope.
        let normalized = normalized.replace('\x1b', "");
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        let mut input = Vec::new();
        if bracketed {
            input.extend_from_slice(b"\x1b[200~");
        }
        input.extend_from_slice(normalized.as_bytes());
        if bracketed {
            input.extend_from_slice(b"\x1b[201~");
        }
        self.input(input)
    }

    pub(crate) fn input_bytes(&self, input: &[u8]) -> bool {
        if self.state() != SessionState::Running {
            return false;
        }
        if input.len() > crate::session::INPUT_LIMIT {
            self.events.reject_input(true);
            return false;
        }
        self.input(input.to_vec())
    }

    pub(crate) fn input(&self, input: Vec<u8>) -> bool {
        if !self.write(input) {
            return false;
        }
        self.pending_bottom
            .store(true, std::sync::atomic::Ordering::Release);
        self.events.mark_dirty();
        true
    }

    pub fn handle(&mut self, cmd: Command) -> Action {
        // Handle commands that don't need the terminal lock first.
        // This avoids blocking the main thread when the PTY event loop
        // holds the FairMutex lease during bursts of terminal output.
        match cmd {
            Command::ProcessAlacrittyEvent(event) => {
                return match event {
                    Event::Exit => Action::Shutdown,
                    Event::Title(title) => Action::ChangeTitle(title),
                    Event::PtyWrite(pty) => {
                        self.write(pty.into_bytes());
                        Action::default()
                    }
                    Event::ClipboardStore(_clipboard_type, text) => Action::ClipboardStore(text),
                    Event::ClipboardLoad(_clipboard_type, formatter) => {
                        Action::ClipboardLoad(formatter)
                    }
                    _ => Action::default(),
                };
            }
            Command::MouseReport(button, modifiers, point, pressed) => {
                self.process_mouse_report(button, modifiers, point, pressed);
                return Action::default();
            }
            Command::Write(input) => {
                self.input(input);
                return Action::default();
            }
            Command::Resize(layout_size, font_measure) => {
                self.resize(layout_size, font_measure);
                return Action::default();
            }
            Command::ProcessLink(LinkAction::Open, point) => {
                self.update_link(Some(point));
                return self
                    .last_content
                    .hovered_url
                    .clone()
                    .map(Action::OpenUrl)
                    .unwrap_or_default();
            }
            Command::ProcessLink(LinkAction::Clear, _) => {
                self.update_link(None);
                return Action::default();
            }
            Command::Scroll(delta) => {
                self.pending_scroll = self.pending_scroll.saturating_add(delta);
                self.events.mark_dirty();
                return Action::default();
            }
            // Commands that need the terminal lock — fall through below.
            _ => {}
        }

        // Scroll, Select, Link — need the terminal lock.
        // Use try_lock_unfair to avoid blocking the main thread if the PTY
        // event loop is holding the lease during a burst of output.
        let term_arc = self.term.clone();
        if let Some(mut term) = term_arc.try_lock_unfair() {
            match cmd {
                Command::Scroll(delta) => {
                    self.scroll(&mut term, delta);
                }
                Command::SelectStart(selection_type, (x, y)) => {
                    self.start_selection(&mut term, selection_type, x, y);
                }
                Command::SelectUpdate((x, y)) => {
                    self.update_selection(&mut term, x, y);
                }
                Command::ClearSelection => {
                    term.selection = None;
                }
                Command::ProcessLink(link_action, point) => {
                    self.process_link_action(&term, link_action, point);
                }
                // Already handled above — can't reach here.
                Command::ProcessAlacrittyEvent(_)
                | Command::MouseReport(..)
                | Command::Write(_)
                | Command::Resize(..) => {}
            };
        }
        // If try_lock_unfair() failed, the command is silently dropped.
        // This is acceptable: scroll/select/link events are continuous and
        // will be retried on the next mouse/keyboard event. Resize state is
        // retained and applied by the next successful sync.

        Action::default()
    }

    /// A busy parser clears stale hover/click state; the view retries hover on render.
    pub(crate) fn update_link(&mut self, point: Option<Point>) {
        self.last_content.hovered_hyperlink = None;
        self.last_content.hovered_url = None;
        if let Some(point) = point {
            let term = self.term.clone();
            if let Some(term) = term.try_lock_unfair() {
                self.process_link_action(&term, LinkAction::Hover, point);
            }
        }
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                // OSC 8 labels need not look like URLs. Resolve the current cell's
                // destination before falling back to plain-text URL detection.
                let viewport_start = Line(-(terminal.grid().display_offset() as i32));
                let viewport_end = viewport_start + terminal.bottommost_line();
                if point.column.0 >= terminal.columns()
                    || !(viewport_start..=viewport_end).contains(&point.line)
                {
                    return;
                }
                if let Some(link) = terminal.grid()[point].hyperlink() {
                    let mut start = point;
                    let mut end = point;
                    let mut before = terminal.grid().iter_from(point);
                    while let Some(cell) = before.prev() {
                        if cell.point.line < viewport_start
                            || cell.hyperlink().as_ref() != Some(&link)
                        {
                            break;
                        }
                        start = cell.point;
                    }
                    for cell in terminal.grid().iter_from(point) {
                        if cell.point.line > viewport_end
                            || cell.hyperlink().as_ref() != Some(&link)
                        {
                            break;
                        }
                        end = cell.point;
                    }
                    self.last_content.hovered_hyperlink = Some(start..=end);
                    self.last_content.hovered_url = Some(link.uri().to_owned());
                    return;
                }
                let hovered_hyperlink =
                    self.regex_match_at(terminal, point, &mut self.url_regex.clone());
                let hovered_url = hovered_hyperlink
                    .as_ref()
                    .map(|range| Self::text_for_range(terminal, range));
                self.last_content.hovered_hyperlink = hovered_hyperlink;
                self.last_content.hovered_url = hovered_url;
            }
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
                self.last_content.hovered_url = None;
            }
            LinkAction::Open => {
                // Handled before locking in handle().
            }
        };
    }

    fn text_for_range<T: EventListener>(
        terminal: &Term<T>,
        range: &RangeInclusive<Point>,
    ) -> String {
        let start = *range.start();
        let end = *range.end();
        let mut text = String::from(terminal.grid()[start].c);
        if start == end {
            return text;
        }

        for indexed in terminal.grid().iter_from(*range.start()) {
            text.push(indexed.c);
            if indexed.point == end {
                break;
            }
        }
        text
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.shift {
            mods += 4;
        }
        if modifiers.alt {
            mods += 8;
        }
        if modifiers.control {
            mods += 16;
        }

        match MouseMode::from(self.last_content.terminal_mode) {
            MouseMode::Sgr => self.sgr_mouse_report(point, button as u8 + mods, pressed),
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(point, button as u8 + mods, is_utf8)
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            }
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.write(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.write(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(x, y, &self.size, terminal.grid().display_offset());
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(&mut self, terminal: &mut Term<EventProxy>, x: f32, y: f32) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location = Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x as usize) / (terminal_size.cell_width as usize);
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y as usize) / (terminal_size.cell_height as usize);
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x as usize % self.size.cell_width as usize;
        let half_cell_width = (self.size.cell_width as f32 / 2.0) as usize;

        if cell_x > half_cell_width {
            Side::Right
        } else {
            Side::Left
        }
    }

    pub(crate) fn resize(&mut self, layout_size: Option<Size>, font_measure: Option<Size>) -> bool {
        self.size.update(layout_size, font_measure);
        let resize_pending = !self.size.same_terminal_geometry(&self.applied_size);
        if resize_pending {
            let term = self.term.clone();
            if let Some(mut term) = term.try_lock_unfair() {
                self.apply_resize(&mut term);
            };
        }
        resize_pending
    }

    fn apply_resize(&mut self, terminal: &mut Term<EventProxy>) {
        if self.startup_output_pending {
            if !self.events.output_ready() {
                return;
            }
            self.startup_output_pending = false;
            if let Some((cols, rows)) = self.deferred_resize.take() {
                self.size.num_cols = cols.clamp(2, u16::MAX as usize) as u16;
                self.size.num_lines = rows.clamp(1, u16::MAX as usize) as u16;
            }
        }
        if self.size.same_terminal_geometry(&self.applied_size) {
            return;
        }

        if let Some(session) = &self.session {
            session.control.resize(self.size.into());
        }
        if self.size.num_cols != self.applied_size.num_cols
            || self.size.num_lines != self.applied_size.num_lines
        {
            let previous = self.revision();
            self.events.invalidate();
            if self.pending_selection_revision == previous {
                self.pending_selection_revision = self.revision();
            }
            terminal.resize(TermSize::new(
                self.size.num_cols as usize,
                self.size.num_lines as usize,
            ));
        }
        self.applied_size = self.size;
    }

    pub(crate) fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) -> bool {
        let input = input.into();
        if self.state() != SessionState::Running {
            return false;
        }
        if input.is_empty() {
            return true;
        }
        let Some(session) = &self.session else {
            return false;
        };
        let accepted = session.control.input(input, false);
        self.events.reject_input(!accepted);
        accepted
    }

    pub fn state(&self) -> SessionState {
        self.events.state()
    }

    pub fn status_message(&self) -> Option<String> {
        self.state().message().or_else(|| {
            self.events
                .input_rejected()
                .then(|| "Input queue full: input was not sent; retry a smaller paste".into())
        })
    }

    /// Idempotent: stops and joins the reader, and reaps the owned child.
    pub fn cancel(&mut self) {
        self.pending_settings = None;
        self.events.set_state(SessionState::Cancelled);
        if let Some(mut session) = self.session.take() {
            session.cancel();
        }
    }

    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.write(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

    pub fn selectable_content(&self) -> Option<String> {
        // Use alacritty's selection_to_string() which properly handles
        // newlines, trailing whitespace, wrapped lines, wide chars, and tabs.
        let term_arc = self.term.clone();
        if let Some(term) = term_arc.try_lock_unfair() {
            if let Some(text) = term.selection_to_string() {
                return Some(text);
            }
            return Some(String::new());
        }
        None
    }

    pub fn sync(&mut self) -> bool {
        self.events.clear_dirty();
        let term_arc = self.term.clone();
        // Use try_lock_unfair to avoid blocking the main thread.
        // If the PTY event loop holds the lock, we skip this sync —
        // the content will be slightly stale until the next frame.
        let Some(mut term) = term_arc.try_lock_unfair() else {
            self.events.mark_dirty();
            if self
                .find_results
                .as_ref()
                .is_some_and(|r| r.revision != self.revision())
            {
                self.clear_find();
            }
            return false;
        };
        self.apply_resize(&mut term);
        self.apply_pending(&mut term);
        self.internal_sync(&term);
        true
    }

    fn internal_sync(&mut self, terminal: &Term<EventProxy>) {
        self.snapshot_revision = self.revision();
        self.last_content.sync_from(terminal, self.size);
        self.last_content.find_match = self
            .find_results
            .as_ref()
            .filter(|r| r.revision == self.events.revision())
            .and_then(|r| r.matches.get(self.find_index))
            .cloned();
    }

    pub fn revision(&self) -> u64 {
        self.events.revision()
    }

    pub fn clear_find(&mut self) {
        self.find_results = None;
        self.last_content.find_match = None;
    }

    pub fn set_find_results(&mut self, results: crate::SearchResults) -> bool {
        if results.revision != self.revision() {
            self.clear_find();
            return false;
        }
        self.find_results = Some(results);
        self.find_index = 0;
        self.navigate_find(false, false)
    }

    pub fn next_find(&mut self, previous: bool) -> bool {
        self.navigate_find(previous, true)
    }

    fn navigate_find(&mut self, previous: bool, advance: bool) -> bool {
        let term_arc = self.term.clone();
        let Some(mut term) = term_arc.try_lock_unfair() else {
            return false;
        };
        let Some(results) = self.find_results.as_ref() else {
            return false;
        };
        if results.revision != self.revision() {
            self.clear_find();
            return false;
        }
        let len = results.matches.len();
        if len == 0 {
            self.last_content.find_match = None;
            return true;
        }
        if advance {
            self.find_index = if previous {
                (self.find_index + len - 1) % len
            } else {
                (self.find_index + 1) % len
            };
        }
        let found = &results.matches[self.find_index];
        let offset = (-found.start.line.0).max(0) as usize;
        let delta = offset as i32 - term.grid().display_offset() as i32;
        term.scroll_display(Scroll::Delta(delta));
        self.internal_sync(&term);
        true
    }

    pub fn renderable_content(&self) -> &RenderableContent {
        &self.last_content
    }

    /// Search for all occurrences of a pattern in the terminal scrollback
    pub fn search_all(&self, pattern: &str) -> Option<Vec<SearchMatch>> {
        if pattern.is_empty() {
            return Some(Vec::new());
        }

        // Escape special regex characters for literal search
        let escaped = escape_regex(pattern);
        let Ok(mut regex) = RegexSearch::new(&escaped) else {
            return Some(Vec::new());
        };

        let term = self.term.clone();
        let term = term.try_lock_unfair()?;

        let mut matches = Vec::new();

        // Search from the beginning of history to the end of the viewport
        let history_start = Line(-(term.grid().history_size() as i32));
        let viewport_end = term.bottommost_line();

        let start = Point::new(history_start, Column(0));
        let end = Point::new(viewport_end, term.last_column());

        for rm in RegexIter::new(start, end, Direction::Right, &term, &mut regex) {
            matches.push(SearchMatch {
                start: *rm.start(),
                end: *rm.end(),
            });
        }

        Some(matches)
    }

    /// Get all terminal text content (including scrollback history)
    pub fn get_all_text(&self) -> Option<String> {
        let term = self.term.try_lock_unfair()?;
        Some(Self::all_text(&term))
    }

    pub(crate) fn all_text<T: EventListener>(term: &Term<T>) -> String {
        // iter_from advances before yielding, so it skips the first retained cell.
        // bounds_to_string includes both endpoints and handles wraps/Unicode/tabs.
        term.bounds_to_string(
            Point::new(term.topmost_line(), Column(0)),
            Point::new(term.bottommost_line(), term.last_column()),
        )
    }

    /// Scroll the terminal to show a specific line
    pub fn scroll_to_line(&mut self, line: i32) {
        let term = self.term.clone();
        let Some(mut term) = term.try_lock_unfair() else {
            return;
        };

        // Line is in history (negative) or viewport (positive)
        // We want to scroll so the line is near the top of the viewport
        let target_offset = if line < 0 {
            // Line is in history - convert to display offset
            (-line) as usize
        } else {
            0
        };

        // Calculate delta from current position
        let current_offset = term.grid().display_offset();
        if target_offset != current_offset {
            let delta = target_offset as i32 - current_offset as i32;
            term.grid_mut().scroll_display(Scroll::Delta(delta));
        }
    }

    /// Based on alacritty/src/display/hint.rs > regex_match_at
    /// Retrieve the match, if the specified point is inside the content matching the regex.
    fn regex_match_at(
        &self,
        terminal: &Term<EventProxy>,
        point: Point,
        regex: &mut RegexSearch,
    ) -> Option<Match> {
        visible_regex_match_iter(terminal, regex).find(|rm| rm.contains(&point))
    }
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start = term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

#[derive(Clone)]
pub struct RenderableCell {
    pub point: Point,
    pub cell: Cell,
}

#[derive(Clone)]
pub struct RenderableContent {
    pub colors: alacritty_terminal::term::color::Colors,
    pub cells: Vec<RenderableCell>,
    pub display_offset: usize,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub find_match: Option<SearchMatch>,
    pub cursor_point: Point,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
    hovered_url: Option<String>,
}

impl RenderableContent {
    fn sync_from<T: EventListener>(&mut self, terminal: &Term<T>, terminal_size: TerminalSize) {
        let renderable = terminal.renderable_content();
        let cursor_point = renderable.cursor.point;
        let cursor = terminal.grid()[cursor_point].clone();
        let selectable_range = renderable.selection;
        let display_offset = renderable.display_offset;
        let terminal_mode = renderable.mode;

        self.colors = *terminal.colors();
        self.cells.clear();
        self.cells
            .extend(renderable.display_iter.map(|indexed| RenderableCell {
                point: indexed.point,
                cell: indexed.cell.clone(),
            }));
        self.display_offset = display_offset;
        self.selectable_range = selectable_range;
        self.cursor_point = cursor_point;
        self.cursor = cursor;
        self.terminal_mode = terminal_mode;
        self.terminal_size = terminal_size;
    }
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            colors: Default::default(),
            cells: Vec::new(),
            display_offset: 0,
            hovered_hyperlink: None,
            selectable_range: None,
            find_match: None,
            cursor_point: Point::default(),
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
            hovered_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::Color;

    fn test_term(columns: usize, lines: usize, history_limit: usize) -> Term<VoidListener> {
        let config = term::Config {
            scrolling_history: history_limit,
            ..term::Config::default()
        };
        let size = TermSize::new(columns, lines);
        Term::new(config, &size, VoidListener)
    }

    fn add_history(term: &mut Term<VoidListener>, lines: usize) {
        let screen_lines = term.screen_lines() as i32;
        let region = Line(0)..Line(screen_lines);
        for _ in 0..lines {
            term.grid_mut().scroll_up::<Color>(&region, 1);
        }
    }

    fn snapshot(term: &Term<VoidListener>) -> RenderableContent {
        let mut snapshot = RenderableContent::default();
        let size = TerminalSize {
            num_cols: term.columns() as u16,
            num_lines: term.screen_lines() as u16,
            ..TerminalSize::default()
        };
        snapshot.sync_from(term, size);
        snapshot
    }

    #[test]
    fn viewport_snapshot_size_is_independent_of_history() {
        let empty = test_term(4, 3, 30_000);
        let empty_snapshot = snapshot(&empty);

        let mut with_history = test_term(4, 3, 30_000);
        add_history(&mut with_history, 30_000);
        let history_snapshot = snapshot(&with_history);

        assert_eq!(with_history.grid().history_size(), 30_000);
        assert_eq!(empty_snapshot.cells.len(), 12);
        assert_eq!(history_snapshot.cells.len(), empty_snapshot.cells.len());
    }

    #[test]
    fn viewport_snapshot_reuses_capacity_at_the_same_size() {
        let mut term = test_term(4, 3, 30_000);
        let mut snapshot = snapshot(&term);
        let initial_capacity = snapshot.cells.capacity();

        add_history(&mut term, 100);
        snapshot.sync_from(&term, snapshot.terminal_size);

        assert_eq!(snapshot.cells.len(), 12);
        assert_eq!(snapshot.cells.capacity(), initial_capacity);
    }

    #[test]
    fn viewport_snapshot_uses_vi_mode_cursor() {
        let mut term = test_term(4, 3, 0);
        term.toggle_vi_mode();
        term.vi_mode_cursor.point = Point::new(Line(1), Column(2));

        let snapshot = snapshot(&term);

        assert_eq!(snapshot.cursor_point, Point::new(Line(1), Column(2)));
    }

    #[test]
    fn viewport_snapshot_normalizes_wide_character_spacer_cursor() {
        let mut term = test_term(4, 3, 0);
        let spacer = Point::new(Line(0), Column(1));
        term.grid_mut().cursor.point = spacer;
        term.grid_mut()[spacer]
            .flags
            .insert(Flags::WIDE_CHAR_SPACER);

        let snapshot = snapshot(&term);

        assert_eq!(snapshot.cursor_point, Point::new(Line(0), Column(0)));
    }

    #[test]
    fn text_for_range_includes_each_cell_once() {
        let mut term = test_term(4, 2, 0);
        term.grid_mut()[Line(0)][Column(0)].c = 'h';
        term.grid_mut()[Line(0)][Column(1)].c = 't';
        term.grid_mut()[Line(0)][Column(2)].c = 't';
        term.grid_mut()[Line(0)][Column(3)].c = 'p';
        let range = Point::new(Line(0), Column(0))..=Point::new(Line(0), Column(3));

        assert_eq!(Backend::text_for_range(&term, &range), "http");
    }

    #[test]
    fn terminal_size_tracks_only_effective_geometry_changes() {
        let mut size = TerminalSize::default();
        let original = size;

        size.update(Some(Size::new(80.9, 50.9)), Some(Size::new(1.0, 1.0)));
        assert!(size.same_terminal_geometry(&original));

        size.update(Some(Size::new(81.0, 50.0)), None);
        assert!(!size.same_terminal_geometry(&original));
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod gate;
