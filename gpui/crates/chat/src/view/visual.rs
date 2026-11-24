//! H10 visual replies: an html_render tool result shown as Iced's sandboxed
//! frame (`showHtmlRender` in assets/agent_chat.html) with `Expand` ↔
//! `Close expanded view`; Esc collapses. The host supplies the native surface
//! (`VisualHost`); without one the reply shows Iced's unavailable note.
use super::*;
use gitterm_core::chat::html_render::HtmlRenderResult;
use gpui::{Pixels, canvas};

/// Claude's MCP prefix, Antigravity's FQN, OpenCode's server_tool name, and
/// the unprefixed MCP name, as Iced. Other tool output is never rendered.
const TOOL_NAMES: [&str; 4] = [
    "html_render",
    "mcp__gitterm_tasks__html_render",
    "mcp_gitterm-tasks_html_render",
    "gitterm_tasks_html_render",
];

/// The validated document and height of an html_render result.
pub(super) fn visual_request(tool: &Tool) -> Option<(String, u32)> {
    // Iced calls showHtmlRender only for a result that is not an error.
    if tool.is_error || !TOOL_NAMES.contains(&tool.name.as_str()) {
        return None;
    }
    let result: HtmlRenderResult = serde_json::from_str(tool.output.as_deref()?).ok()?;
    result.html_render.validate().ok()?;
    Some((result.html_render.html, result.html_render.height))
}

/// html_render.js `httpUrl`: only HTTP(S) URLs with a host and no
/// credentials leave the frame, to the system browser.
pub fn visual_link(url: &str) -> bool {
    if url.len() > 8192 || !crate::interactions::http_link(url) {
        return false;
    }
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && !authority
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualMessage {
    /// A trusted click inside the frame, already checked by html_render.js.
    Link(String),
    Escape,
}

/// Window-space bounds of the frame and the part of it the transcript shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualPlacement {
    pub bounds: Bounds<Pixels>,
    pub visible: Bounds<Pixels>,
}

/// One native frame. `place` is called in prepaint each frame it is painted;
/// the host hides a frame that was not placed in the frame just drawn.
pub trait VisualSurface {
    fn place(&mut self, placement: VisualPlacement);
    fn drain(&mut self) -> Vec<VisualMessage>;
    /// Iced's `render.setTheme`: restyle the loaded frame in place.
    fn set_theme(&mut self, light: bool);
}

/// Builds a surface for (document, height, light).
pub type VisualFactory =
    Rc<dyn Fn(&mut Window, &str, u32, bool) -> Result<Box<dyn VisualSurface>, String>>;

#[derive(Clone)]
pub struct VisualHost(pub VisualFactory);
impl gpui::Global for VisualHost {}

pub(super) type VisualExpand = Rc<dyn Fn(Option<Entity<VisualReply>>, &mut App)>;

pub(super) struct VisualReply {
    row: u64,
    html: String,
    height: u32,
    pub(super) light: bool,
    surface: Option<Box<dyn VisualSurface>>,
    error: Option<String>,
    pub(super) expanded: bool,
    focus: FocusHandle,
    on_expand: VisualExpand,
    pump: Option<Task<()>>,
}

impl VisualReply {
    pub(super) fn new(
        row: u64,
        (html, height): (String, u32),
        light: bool,
        on_expand: VisualExpand,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            row,
            html,
            height,
            light,
            surface: None,
            error: None,
            expanded: false,
            focus: cx.focus_handle(),
            on_expand,
            pump: None,
        }
    }

    pub(super) fn set_light(&mut self, light: bool, cx: &mut Context<Self>) {
        if self.light != light {
            // Iced's `__setAppearance`: every frame keeps its document.
            self.light = light;
            if let Some(surface) = &mut self.surface {
                surface.set_theme(light);
            }
            cx.notify();
        }
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        if self.expanded {
            self.focus.focus(window, cx);
        }
        (self.on_expand)(self.expanded.then(|| cx.entity()), cx);
        cx.notify();
    }

    pub(super) fn collapse(&mut self, cx: &mut Context<Self>) {
        if self.expanded {
            self.expanded = false;
            (self.on_expand)(None, cx);
            cx.notify();
        }
    }

    pub(super) fn pump(&mut self, cx: &mut Context<Self>) {
        let messages = self
            .surface
            .as_mut()
            .map(|surface| surface.drain())
            .unwrap_or_default();
        for message in messages {
            match message {
                VisualMessage::Link(url) if visual_link(&url) => cx.open_url(&url),
                VisualMessage::Link(_) => eprintln!("Visual reply link refused: not HTTP(S)"),
                VisualMessage::Escape => self.collapse(cx),
            }
        }
    }

    fn ensure_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.surface.is_some() || self.error.is_some() {
            return;
        }
        let Some(host) = cx.try_global::<VisualHost>().cloned() else {
            return;
        };
        match (host.0)(window, &self.html, self.height, self.light) {
            Ok(surface) => {
                self.surface = Some(surface);
                self.pump = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(30))
                            .await;
                        if this.update(cx, |this, cx| this.pump(cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(error) => self.error = Some(format!("Visual reply unavailable: {error}")),
        }
    }
}

impl Render for VisualReply {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = ChatAppearance {
            light: self.light,
            ..Default::default()
        }
        .palette();
        let id = self.row;
        let block = div()
            .id(("visual-reply", id))
            .debug_selector(move || format!("visual-reply-{id}"))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.expanded {
                    this.collapse(cx);
                    cx.stop_propagation();
                }
            }))
            .flex()
            .flex_col()
            .border_1()
            .border_color(rgb(palette.border))
            .rounded(px(8.))
            .overflow_hidden()
            .bg(rgb(palette.base))
            .when(self.expanded, |block| block.size_full())
            .when(!self.expanded, |block| block.w_full().my(px(10.)));
        if cx.try_global::<VisualHost>().is_none() {
            // Iced: pages without the native subframe policy show a note,
            // a bare `.html-render` box in the transcript's own text.
            return block
                .text_color(rgb(palette.fg))
                .child(
                    div()
                        .debug_selector(move || format!("visual-note-{id}"))
                        .child("Visual reply available in GitTerm for macOS."),
                )
                .into_any_element();
        }
        self.ensure_surface(window, cx);
        let label = if self.expanded {
            "Close expanded view"
        } else {
            "Expand"
        };
        let entity = cx.entity();
        let frame = div()
            .debug_selector(move || format!("visual-frame-{id}"))
            .w_full()
            .when(self.expanded, |frame| frame.flex_1().min_h_0())
            .when(!self.expanded, |frame| frame.h(px(self.height as f32)))
            .relative()
            .children(self.error.clone().map(|error| div().p(px(8.)).child(error)))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let visible = bounds.intersect(&window.content_mask().bounds);
                        entity.update(cx, |this, _| {
                            if let Some(surface) = &mut this.surface {
                                surface.place(VisualPlacement { bounds, visible });
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        block
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(8.))
                    .py(px(5.))
                    .text_color(rgb(palette.muted))
                    .child("Visual reply")
                    .child(
                        Button::new(("visual-expand", id))
                            .ghost()
                            .small()
                            .label(label)
                            .debug_selector(move || format!("visual-{id}-{label}"))
                            .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx))),
                    ),
            )
            .child(frame)
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "visual_tests.rs"]
mod tests;
