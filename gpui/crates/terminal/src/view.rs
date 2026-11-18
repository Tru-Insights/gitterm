// Adapted from gpui_xterm 0.3.0; see README.md and licenses/gpui_xterm-MIT.
use crate::backend::{Backend, Command, LinkAction, MouseButton as ReportButton};
use crate::colors::ColorPalette;
use crate::input::keystroke_to_bytes;
use crate::mouse::pixel_to_cell;
use crate::render::TerminalRenderer;
use crate::{Action, SessionState};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::{Edges, ElementInputHandler, EntityInputHandler, UTF16Selection, *};
use std::sync::Arc;
use std::time::Duration;

const CONTEXT: &str = "GitTermTerminal";

actions!(gitterm_terminal, [SendTab, SendBacktab]);

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub cols: usize,

    pub rows: usize,

    pub font_family: String,

    pub font_size: Pixels,

    pub scrollback: usize,

    pub line_height_multiplier: f32,

    pub padding: Edges<Pixels>,

    pub colors: ColorPalette,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            font_family: "Menlo".into(),
            font_size: px(14.0),
            scrollback: 100_000,
            line_height_multiplier: 1.0,
            padding: Edges::all(px(0.0)),
            colors: ColorPalette::default(),
        }
    }
}

pub type ResizeCallback = Box<dyn Fn(usize, usize) + Send + Sync>;

pub type KeyHandler = Box<dyn Fn(&KeyDownEvent) -> bool + Send + Sync>;

pub type BellCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

pub type TitleCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

pub type ClipboardStoreCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

pub type UrlOpener = Box<dyn Fn(&str)>;

pub type ExitCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

#[derive(Clone, Copy, Debug, Default)]
struct LayoutMetrics {
    origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
    cols: usize,
    rows: usize,
}

pub struct TerminalView {
    backend: Backend,

    renderer: TerminalRenderer,

    focus_handle: FocusHandle,

    config: TerminalConfig,

    _notification_task: Task<()>,

    resize_callback: Option<Arc<ResizeCallback>>,

    key_handler: Option<Arc<KeyHandler>>,

    bell_callback: Option<BellCallback>,

    title_callback: Option<TitleCallback>,

    clipboard_store_callback: Option<ClipboardStoreCallback>,

    exit_callback: Option<ExitCallback>,
    url_opener: Option<UrlOpener>,
    link_position: Option<Point<Pixels>>,
    link_modifier: bool,
    link_click: bool,

    layout_metrics: Arc<parking_lot::Mutex<LayoutMetrics>>,

    dragging_selection: bool,
    pending_copy: bool,

    ime_text: String,
    ime_marked_range: Option<std::ops::Range<usize>>,
}

fn should_defer_text_input_to_ime(event: &KeyDownEvent) -> bool {
    event.keystroke.key_char.is_some()
        && !event.keystroke.modifiers.control
        && !event.keystroke.modifiers.alt
        && !event.keystroke.modifiers.platform
        && !event.keystroke.modifiers.function
}

impl TerminalView {
    pub fn new_deferred(
        settings: crate::BackendSettings,
        config: TerminalConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::initialize(Backend::new_deferred(settings), config, cx)
    }

    pub fn new(mut backend: Backend, config: TerminalConfig, cx: &mut Context<Self>) -> Self {
        backend.resize_cells(config.cols, config.rows);
        Self::initialize(backend, config, cx)
    }

    fn initialize(mut backend: Backend, config: TerminalConfig, cx: &mut Context<Self>) -> Self {
        backend.set_history_limit(config.scrollback);
        cx.bind_keys([
            KeyBinding::new("tab", SendTab, Some(CONTEXT)),
            KeyBinding::new("shift-tab", SendBacktab, Some(CONTEXT)),
        ]);
        let renderer = TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
        );
        let focus_handle = cx.focus_handle().tab_stop(false);
        // Only notification crosses onto the foreground. Parsing/replies continue
        // when this task is paused, the entity is hidden, or no frame is painted.
        let reader_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if this
                    .update(cx, |view: &mut Self, cx| {
                        if view.backend.needs_sync() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            backend,
            renderer,
            focus_handle,
            config,
            _notification_task: reader_task,
            resize_callback: None,
            key_handler: None,
            bell_callback: None,
            title_callback: None,
            clipboard_store_callback: None,
            exit_callback: None,
            url_opener: None,
            link_position: None,
            link_modifier: false,
            link_click: false,
            layout_metrics: Arc::new(parking_lot::Mutex::new(LayoutMetrics::default())),
            dragging_selection: false,
            pending_copy: false,
            ime_text: String::new(),
            ime_marked_range: None,
        }
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }
    pub fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }

    pub fn with_resize_callback(
        mut self,
        callback: impl Fn(usize, usize) + Send + Sync + 'static,
    ) -> Self {
        self.resize_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    pub fn with_key_handler(
        mut self,
        handler: impl Fn(&KeyDownEvent) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.key_handler = Some(Arc::new(Box::new(handler)));
        self
    }

    pub fn with_bell_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.bell_callback = Some(Box::new(callback));
        self
    }

    pub fn with_title_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.title_callback = Some(Box::new(callback));
        self
    }

    pub fn with_clipboard_store_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.clipboard_store_callback = Some(Box::new(callback));
        self
    }

    pub fn with_exit_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.exit_callback = Some(Box::new(callback));
        self
    }

    /// The embedding app owns URL policy and opening. The terminal never launches URLs.
    pub fn with_url_opener(mut self, opener: impl Fn(&str) + 'static) -> Self {
        self.url_opener = Some(Box::new(opener));
        self
    }

    pub fn status_message(&self) -> Option<String> {
        self.backend.status_message()
    }

    fn update_link_hover(&mut self) {
        let point = self
            .link_modifier
            .then_some(self.link_position)
            .flatten()
            .and_then(|position| self.link_cell(position));
        self.backend.update_link(point);
    }

    fn link_cell(&self, position: Point<Pixels>) -> Option<alacritty_terminal::index::Point> {
        let m = *self.layout_metrics.lock();
        if position.x >= m.origin.x + m.cell_width * m.cols as f32
            || position.y >= m.origin.y + m.cell_height * m.rows as f32
        {
            return None;
        }
        self.hit_test_cell(position)
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.link_modifier = event.modifiers.platform;
        self.update_link_hover();
        cx.notify();
    }

    fn on_action_send_tab(&mut self, _: &SendTab, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.write_input_bytes(b"\t") {
            _cx.notify();
        }
    }

    fn on_action_send_backtab(
        &mut self,
        _: &SendBacktab,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self.write_input_bytes(b"\x1b[Z") {
            _cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(ref handler) = self.key_handler
            && handler(event)
        {
            return;
        }

        let key = event.keystroke.key.to_lowercase();
        if (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
            && key == "c"
            && (self.copy_selection() || event.keystroke.modifiers.platform)
        {
            return;
        }

        if (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
            && key == "v"
            && self.paste_from_clipboard()
        {
            _cx.notify();
            return;
        }

        if should_defer_text_input_to_ime(event) {
            return;
        }

        if key == "tab" {
            let wrote = self.write_input_bytes(if event.keystroke.modifiers.shift {
                b"\x1b[Z"
            } else {
                b"\t"
            });
            if wrote {
                _cx.notify();
            }
            return;
        }

        if let Some(bytes) = keystroke_to_bytes(&event.keystroke, self.backend.mode())
            && self.write_input_bytes(&bytes)
        {
            _cx.notify();
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.mouse_down(event);
        cx.notify();
    }

    pub(crate) fn mouse_down(&mut self, event: &MouseDownEvent) {
        let Some(point) = self.hit_test_cell(event.position) else {
            return;
        };
        if event.button == MouseButton::Left && event.modifiers.platform {
            self.link_click = true;
            self.dragging_selection = false;
            if let Some(point) = self.link_cell(event.position)
                && let Action::OpenUrl(url) = self
                    .backend
                    .handle(Command::ProcessLink(LinkAction::Open, point))
                && let Some(opener) = &self.url_opener
            {
                opener(&url);
            }
            return;
        }
        if self.reporting_mouse() {
            self.report_mouse(event.button, event.modifiers, point, true);
            self.dragging_selection = false;
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let selection_type = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.backend
            .select(point, selection_type, event.modifiers.shift);
        self.dragging_selection = true;
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.mouse_up(event);
        cx.notify();
    }

    pub(crate) fn mouse_up(&mut self, event: &MouseUpEvent) {
        if event.button == MouseButton::Left && std::mem::take(&mut self.link_click) {
            return;
        }
        if let Some(point) = self.hit_test_cell(event.position) {
            if self.reporting_mouse() {
                self.report_mouse(event.button, event.modifiers, point, false);
            } else if self.dragging_selection {
                self.update_selection_endpoint(point);
            }
        }
        if self.dragging_selection {
            let _ = self.copy_selection();
        }
        self.dragging_selection = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.link_position = Some(event.position);
        self.link_modifier = event.modifiers.platform;
        self.update_link_hover();
        if event.modifiers.platform {
            cx.notify();
            return;
        }
        if let Some(point) = self.hit_test_cell(event.position) {
            let mode = self.backend.mode();
            if mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
                && self.reporting_mouse()
            {
                let button = match event.pressed_button {
                    Some(MouseButton::Left) => ReportButton::LeftMove,
                    Some(MouseButton::Middle) => ReportButton::MiddleMove,
                    Some(MouseButton::Right) => ReportButton::RightMove,
                    _ if mode.contains(TermMode::MOUSE_MOTION) => ReportButton::NoneMove,
                    _ => return,
                };
                self.backend.handle(Command::MouseReport(
                    button,
                    event.modifiers,
                    self.viewport_point(point),
                    true,
                ));
            } else if self.dragging_selection && event.dragging() {
                self.update_selection_endpoint(point);
            }
            cx.notify();
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll(event);
        cx.notify();
    }

    pub(crate) fn scroll(&mut self, event: &ScrollEvent) {
        let metrics = *self.layout_metrics.lock();
        let delta = match event.delta {
            ScrollDelta::Lines(delta) => delta.y.round() as i32,
            ScrollDelta::Pixels(delta) => {
                (f32::from(delta.y) / f32::from(metrics.cell_height.max(px(1.)))).round() as i32
            }
        };
        if self.reporting_mouse() {
            if let Some(point) = self.hit_test_cell(event.position) {
                for _ in 0..delta.unsigned_abs() {
                    let button = if delta > 0 {
                        ReportButton::ScrollUp
                    } else {
                        ReportButton::ScrollDown
                    };
                    self.backend.handle(Command::MouseReport(
                        button,
                        event.modifiers,
                        self.viewport_point(point),
                        true,
                    ));
                }
            }
        } else {
            self.backend.handle(Command::Scroll(delta));
        }
    }

    fn reporting_mouse(&self) -> bool {
        self.backend.mode().intersects(TermMode::MOUSE_MODE)
    }
    fn viewport_point(
        &self,
        mut point: alacritty_terminal::index::Point,
    ) -> alacritty_terminal::index::Point {
        point.line.0 += self.backend.renderable_content().display_offset as i32;
        point
    }
    fn report_mouse(
        &mut self,
        button: MouseButton,
        modifiers: Modifiers,
        point: alacritty_terminal::index::Point,
        pressed: bool,
    ) {
        let button = match button {
            MouseButton::Left => ReportButton::LeftButton,
            MouseButton::Middle => ReportButton::MiddleButton,
            MouseButton::Right => ReportButton::RightButton,
            _ => return,
        };
        self.backend.handle(Command::MouseReport(
            button,
            modifiers,
            self.viewport_point(point),
            pressed,
        ));
    }

    fn hit_test_cell(&self, position: Point<Pixels>) -> Option<alacritty_terminal::index::Point> {
        let metrics = *self.layout_metrics.lock();
        if metrics.cell_width <= px(0.0) || metrics.cell_height <= px(0.0) {
            return None;
        }
        if position.x < metrics.origin.x || position.y < metrics.origin.y {
            return None;
        }
        let mut point = pixel_to_cell(
            position,
            metrics.origin,
            metrics.cell_width,
            metrics.cell_height,
        );
        if point.column.0 >= metrics.cols {
            point.column = alacritty_terminal::index::Column(metrics.cols.saturating_sub(1));
        }
        point.line.0 = point.line.0.min(metrics.rows.saturating_sub(1) as i32);
        let offset = self.backend.renderable_content().display_offset as i32;
        point.line = alacritty_terminal::index::Line(point.line.0 - offset);
        Some(point)
    }

    fn update_selection_endpoint(&mut self, point: alacritty_terminal::index::Point) {
        self.backend.extend_selection(point);
    }

    fn process_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.backend.take_events();
        if events.bell
            && let Some(callback) = &self.bell_callback
        {
            callback(window, cx);
        }
        if let Some(title) = events.title
            && let Some(callback) = &self.title_callback
        {
            callback(window, cx, &title);
        }
        if let Some(text) = events.clipboard_store
            && let Some(callback) = &self.clipboard_store_callback
        {
            callback(window, cx, &text);
        }
        if events.exited
            && let Some(callback) = &self.exit_callback
        {
            callback(window, cx);
        }
        if let Some(error) = events.error {
            eprintln!("[gitterm-terminal] {error}");
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        self.backend.dimensions()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.backend.resize_cells(cols, rows);
    }

    pub fn config(&self) -> &TerminalConfig {
        &self.config
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn update_config(&mut self, config: TerminalConfig, cx: &mut Context<Self>) {
        self.renderer.font_family = config.font_family.clone();
        self.renderer.font_size = config.font_size;
        self.renderer.line_height_multiplier = config.line_height_multiplier;
        self.renderer.palette = config.colors.clone();

        self.backend.set_history_limit(config.scrollback);
        self.config = config;

        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.backend.clear_screen();
        self.ime_text.clear();
        self.ime_marked_range = None;
        cx.notify();
    }

    pub fn has_selection(&self) -> bool {
        self.backend.selectable_content().map_or_else(
            || self.backend.renderable_content().selectable_range.is_some(),
            |text| !text.is_empty(),
        )
    }

    pub fn copy_selection(&mut self) -> bool {
        if !self.backend.sync() {
            self.pending_copy |= self.dragging_selection || self.has_selection();
            return self.pending_copy;
        }
        self.pending_copy = false;
        if let Some(text) = self.backend.selectable_content()
            && !text.is_empty()
            && let Ok(mut clipboard) = crate::clipboard::Clipboard::new()
        {
            return clipboard.set_text(text).is_ok();
        }
        false
    }

    pub fn paste_from_clipboard(&self) -> bool {
        if let Ok(mut clipboard) = crate::clipboard::Clipboard::new()
            && let Ok(text) = clipboard.get_text()
        {
            return self.backend.paste(&text);
        }
        false
    }

    fn write_input_bytes(&self, bytes: &[u8]) -> bool {
        self.backend.input_bytes(bytes)
    }

    fn build_context_menu(
        view: WeakEntity<Self>,
        has_selection: bool,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        menu.item(
            PopupMenuItem::new("Copy")
                .disabled(!has_selection)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, _| {
                            let _ = this.copy_selection();
                        });
                    }
                }),
        )
        .item(PopupMenuItem::new("Paste").on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    let _ = this.paste_from_clipboard();
                    cx.notify();
                });
            }
        }))
        .item(PopupMenuItem::separator())
        .item(PopupMenuItem::new("Clear").on_click({
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.clear(cx);
                });
            }
        }))
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self
            .ime_marked_range
            .clone()
            .unwrap_or(0..self.ime_text.encode_utf16().count());
        *adjusted_range = Some(range);
        Some(self.ime_text.clone())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.ime_marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.ime_marked_range = None;
        self.ime_text.clear();
        _cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_marked_range = None;
        self.ime_text.clear();
        if !text.is_empty() {
            let _ = self.write_input_bytes(text.as_bytes());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.backend.state() != SessionState::Running {
            return;
        }
        self.ime_text = new_text.to_string();
        self.ime_marked_range = if new_text.is_empty() {
            None
        } else {
            new_selected_range.or_else(|| Some(0..new_text.encode_utf16().count()))
        };
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let metrics = *self.layout_metrics.lock();
        let snapshot = self.backend.renderable_content();
        let cursor = snapshot.cursor_point;
        let visible_row = cursor.line.0 + snapshot.display_offset as i32;
        Some(Bounds {
            origin: Point {
                x: metrics.origin.x + metrics.cell_width * cursor.column.0 as f32,
                y: metrics.origin.y + metrics.cell_height * visible_row as f32,
            },
            size: size(
                metrics.cell_width.max(px(1.)),
                metrics.cell_height.max(px(1.)),
            ),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.process_events(window, cx);

        if self.backend.sync() && self.pending_copy {
            self.copy_selection();
        }
        self.update_link_hover();
        let link_hovered = self
            .backend
            .renderable_content()
            .hovered_hyperlink
            .is_some();
        let status = self.status_message();
        if self.backend.state() != SessionState::Running {
            self.ime_text.clear();
            self.ime_marked_range = None;
        }
        let snapshot = Arc::new(self.backend.renderable_content().clone());
        let renderer = self.renderer.clone();
        let resize_callback = self.resize_callback.clone();
        let padding = self.config.padding;
        let layout_metrics = self.layout_metrics.clone();
        let focus_handle = self.focus_handle.clone();
        let entity = cx.entity();
        let ime_text = self.ime_text.clone();
        let ime_marked_range = self.ime_marked_range.clone();

        div()
            .size_full()
            .relative()
            .cursor(if link_hovered {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            })
            .bg(self.config.colors.background())
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_send_tab))
            .on_action(cx.listener(Self::on_action_send_backtab))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_mouse_exit(cx.listener(|view, _: &MouseExitEvent, _, cx| {
                view.link_position = None;
                view.update_link_hover();
                cx.notify();
            }))
            .on_scroll(cx.listener(Self::on_scroll))
            .context_menu({
                let view = cx.entity().downgrade();
                let has_selection = self.has_selection();
                move |menu, window, cx| {
                    Self::build_context_menu(view.clone(), has_selection, menu, window, cx)
                }
            })
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );

                        let mut measured_renderer = renderer.clone();
                        measured_renderer.measure_cell(window);

                        let available_width: f32 =
                            (bounds.size.width - padding.left - padding.right).into();
                        let available_height: f32 =
                            (bounds.size.height - padding.top - padding.bottom).into();
                        let cell_width_f32: f32 = measured_renderer.cell_width.into();
                        let cell_height_f32: f32 = measured_renderer.cell_height.into();

                        let cols = ((available_width / cell_width_f32) as usize).max(1);
                        let rows = ((available_height / cell_height_f32) as usize).max(1);

                        if (cols, rows) != entity.read(cx).dimensions()
                            && let Some(callback) = &resize_callback
                        {
                            callback(cols, rows);
                        }
                        entity.update(cx, |view, _| {
                            if view.backend.is_deferred() {
                                if let Err(error) = view.backend.initialize_deferred(
                                    cols,
                                    rows,
                                    cell_width_f32.round() as u16,
                                    cell_height_f32.round() as u16,
                                ) {
                                    view.backend.record_error(format!(
                                        "initial terminal PTY startup: {error}"
                                    ));
                                }
                            } else {
                                view.backend.resize_cells(cols, rows);
                            }
                        });

                        let origin = Point {
                            x: bounds.origin.x + padding.left,
                            y: bounds.origin.y + padding.top,
                        };
                        *layout_metrics.lock() = LayoutMetrics {
                            origin,
                            cell_width: measured_renderer.cell_width,
                            cell_height: measured_renderer.cell_height,
                            cols,
                            rows,
                        };

                        measured_renderer.paint(
                            bounds,
                            padding,
                            &snapshot,
                            window,
                            cx,
                            (!ime_text.is_empty())
                                .then_some((ime_text.as_str(), ime_marked_range.as_ref())),
                        );
                    },
                )
                .size_full(),
            )
            .children(status.map(|message| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .px_2()
                    .py_1()
                    .bg(self.config.colors.background())
                    .text_color(self.config.colors.foreground())
                    .child(message)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalConfig;

    #[::core::prelude::v1::test]
    fn test_terminal_config_default_uses_compact_line_height() {
        let config = TerminalConfig::default();

        assert_eq!(config.line_height_multiplier, 1.0);
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod gate;
