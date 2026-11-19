use crate::{events::Record, model::*};
use gpui::{
    App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    HighlightStyle, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    StyleRefinement, Styled, StyledText, Subscription, Task, TitlebarOptions, Window, WindowBounds,
    WindowOptions, actions, base::TextSelection, div, prelude::FluentBuilder, px, rgb, size,
};
use gpui_component::{
    Root, Sizable,
    button::Button,
    collapsible::Collapsible,
    diff::{Diff, DiffFile, DiffState},
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{RangeHighlight, SelectionFormat, TextView, TextViewMotion, TextViewState},
};
use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};

const BASE: u32 = 0x1e1e2e;
const MANTLE: u32 = 0x181825;
const FG: u32 = 0xbac2de;
const MUTED: u32 = 0x7f849c;
const BORDER: u32 = 0x45475a;

actions!(
    native_chat,
    [
        Find,
        NextMatch,
        PreviousMatch,
        CloseFind,
        CopyPlain,
        CopyMarkdown
    ]
);

pub fn run(records: Vec<Record>) {
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        init(cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(850.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("GitTerm · Native chat fixture".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let chat = cx.new(|cx| ChatView::new(records, window, cx));
                cx.new(|cx| Root::new(chat, window, cx))
            },
        )
        .expect("open native chat fixture viewer");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}

/// Register chat shortcuts, scoped to the focused NativeChat subtree.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-f", Find, Some("NativeChat")),
        KeyBinding::new("cmd-g", NextMatch, Some("NativeChat")),
        KeyBinding::new("cmd-shift-g", PreviousMatch, Some("NativeChat")),
        KeyBinding::new("escape", CloseFind, Some("NativeChat")),
        KeyBinding::new("cmd-c", CopyPlain, Some("NativeChat")),
        KeyBinding::new("cmd-shift-c", CopyMarkdown, Some("NativeChat")),
    ]);
}

/// Owns the document model and retained selection/fold state independently of list layout.
/// Rich-text state is initialized only for visited rows; it is not the event source of truth.
pub struct ChatView {
    records: Arc<Vec<Record>>,
    model: ChatModel,
    row_views: Vec<Option<Entity<RowView>>>,
    scroller: Entity<MessageScrollerState>,
    find_input: Entity<InputState>,
    find: FindState,
    find_open: bool,
    focus: FocusHandle,
    replay: Option<Task<()>>,
    replay_position: usize,
    _subscriptions: Vec<Subscription>,
}
impl ChatView {
    pub fn new(records: Vec<Record>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = ChatModel::from_records(&records);
        let scroller = cx.new(|cx| MessageScrollerState::new(model.rows.len(), cx));
        let find_input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in chat"));
        let subscriptions = vec![cx.subscribe_in(
            &find_input,
            window,
            |this, input, event, window, cx| match event {
                InputEvent::Change => {
                    this.find
                        .search(&this.model, input.read(cx).value().to_string());
                    this.reveal_match(window, cx);
                }
                InputEvent::PressEnter { secondary, .. } => this.navigate(*secondary, window, cx),
                _ => {}
            },
        )];
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            records: Arc::new(records),
            row_views: vec![None; model.rows.len()],
            model,
            scroller,
            find_input,
            find: FindState::default(),
            find_open: false,
            focus,
            replay: None,
            replay_position: 0,
            _subscriptions: subscriptions,
        }
    }
    /// Retained scroll state for embedding hosts (independent of dock layout).
    pub fn scroller(&self) -> &Entity<MessageScrollerState> {
        &self.scroller
    }

    fn row(&mut self, index: usize, cx: &mut Context<Self>) -> Entity<RowView> {
        if let Some(view) = &self.row_views[index] {
            return view.clone();
        }
        let row = self.model.rows[index].clone();
        let scroller = self.scroller.clone();
        let view = cx.new(|cx| RowView::new(row, index, scroller, cx));
        self.row_views[index] = Some(view.clone());
        self.update_row_find(index, cx);
        view
    }
    fn update_row_find(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(view) = &self.row_views[index] {
            let query = if self.find_open {
                self.find.query.clone()
            } else {
                String::new()
            };
            let active = self
                .find
                .active()
                .filter(|found| found.row == index)
                .map(|found| found.occurrence);
            view.update(cx, |view, cx| {
                view.query = query;
                view.active = active;
                view.highlight(cx);
                cx.notify();
            });
        }
    }
    fn reveal_match(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.row_views.len() {
            self.update_row_find(index, cx);
        }
        if let Some(found) = self.find.active().cloned() {
            let view = self.row(found.row, cx);
            view.update(cx, |view, cx| {
                view.open = true;
                view.reveal_pending = true;
                view.highlight_signature = None;
                view.highlight(cx);
            });
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(found.row..found.row + 1, cx);
                state.scroll_to_item(found.row, cx);
            });
        }
        cx.notify();
    }
    fn navigate(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.find.step(backwards);
        self.reveal_match(window, cx);
    }
    fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.find_open = true;
        self.find_input
            .update(cx, |state, cx| state.focus(window, cx));
        self.reveal_match(window, cx);
    }
    fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        self.find_open = false;
        self.focus.focus(window, cx);
        for index in 0..self.row_views.len() {
            self.update_row_find(index, cx);
        }
        cx.notify();
    }
    fn copy(&mut self, markdown: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Let the find input keep the platform's normal editing clipboard behavior.
        if self.find_input.read(cx).focus_handle(cx).is_focused(window) {
            cx.propagate();
            return;
        }
        self.copy_selection(markdown, window, cx);
    }
    fn copy_selection(&self, markdown: bool, window: &mut Window, cx: &mut App) {
        for row in self.row_views.iter().flatten() {
            row.read(cx).text.clone().update(cx, |text, cx| {
                text.set_selection_format(
                    if markdown {
                        SelectionFormat::Source
                    } else {
                        SelectionFormat::Plain
                    },
                    cx,
                )
            });
        }
        let text = TextSelection::selected_text(window, cx);
        // Match TextView's standard Cmd+C handler, including its outer whitespace policy.
        let text = if markdown {
            text
        } else {
            text.trim().to_owned()
        };
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        for row in self.row_views.iter().flatten() {
            row.read(cx).text.clone().update(cx, |text, cx| {
                text.set_selection_format(SelectionFormat::Plain, cx)
            });
        }
    }
    fn start_replay(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.replay.take();
        self.model = ChatModel::default();
        self.row_views.clear();
        self.replay_position = 0;
        self.scroller.update(cx, |state, cx| state.reset(0, cx));
        self.find.search(&self.model, self.find.query.clone());
        let records = self.records.clone();
        self.replay = Some(cx.spawn(async move |this, cx| {
            for record in records.iter() {
                cx.background_executor()
                    .timer(Duration::from_millis(35))
                    .await;
                if this
                    .update(cx, |this, cx| this.append_record(record, cx))
                    .is_err()
                {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| {
                this.replay = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn append_record(&mut self, record: &Record, cx: &mut Context<Self>) {
        let count = self.model.rows.len();
        let changed = self.model.apply(record);
        self.row_views.resize(self.model.rows.len(), None);
        self.scroller.update(cx, |state, cx| {
            state.append(self.model.rows.len() - count, cx);
            for &index in &changed {
                state.remeasure_items(index..index + 1, cx);
            }
        });
        for &index in &changed {
            if let Some(view) = &self.row_views[index] {
                let row = self.model.rows[index].clone();
                view.update(cx, |view, cx| view.set_row(row, cx));
            }
        }
        if !self.find.query.is_empty() {
            self.find.refresh(&self.model, &changed);
            for index in 0..self.row_views.len() {
                self.update_row_find(index, cx);
            }
        }
        self.replay_position += 1;
        cx.notify();
    }
}
impl Focusable for ChatView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        let copy_owner = owner.clone();
        div().id("native-chat").key_context("NativeChat").track_focus(&self.focus)
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| {
                // A child input/selection may already have claimed focus during bubbling.
                if !this.focus.contains_focused(window, cx) {
                    this.focus.focus(window, cx);
                }
            }))
            .on_action(cx.listener(Self::open_find)).on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(|this, _: &NextMatch, window, cx| this.navigate(false, window, cx)))
            .on_action(cx.listener(|this, _: &PreviousMatch, window, cx| this.navigate(true, window, cx)))
            .on_action(cx.listener(|this, _: &CopyPlain, window, cx| this.copy(false, window, cx)))
            .on_action(cx.listener(|this, _: &CopyMarkdown, window, cx| this.copy(true, window, cx)))
            .size_full().flex().flex_col().gap(px(8.)).p(px(10.)).bg(rgb(BASE)).text_color(rgb(FG)).text_size(px(15.))
            .child(div().flex().items_center().justify_between()
                .child(div().text_color(rgb(MUTED)).child(format!("Native chat · {} rows · synthetic fixture", self.model.rows.len())))
                .child(Button::new("replay").small().label(if self.replay.is_some() { "Restart replay" } else { "Replay" }).on_click(cx.listener(Self::start_replay))))
            .when(self.find_open, |view| view.child(div().flex().items_center().gap(px(6.)).p(px(6.)).bg(rgb(0x2a2b3d)).rounded(px(8.))
                .child(div().flex_1().child(Input::new(&self.find_input)))
                .child(format!("{} of {}", self.find.current.map_or(0, |index| index + 1), self.find.matches.len()))
                .child(Button::new("previous").small().label("Previous").on_click(cx.listener(|this, _, window, cx| this.navigate(true, window, cx))))
                .child(Button::new("next").small().label("Next").on_click(cx.listener(|this, _, window, cx| this.navigate(false, window, cx))))
                .child(Button::new("close-find").small().label("Close").on_click(cx.listener(|this, _, window, cx| this.close_find(&CloseFind, window, cx))))))
            .child(div().flex_1().min_h_0().bg(rgb(MANTLE)).rounded(px(8.)).child(
                MessageScroller::new("messages", self.scroller.clone(), move |index, _, cx| {
                    owner.update(cx, |this, cx| this.row(index, cx)).map(IntoElement::into_any_element).unwrap_or_else(|_| div().into_any_element())
                }).with_content_style(StyleRefinement::default().px(px(14.)).py(px(12.)))
            ).context_menu(move |menu, _, _| {
                let plain = copy_owner.clone(); let markdown = copy_owner.clone();
                menu.item(PopupMenuItem::new("Copy").on_click(move |_, window, cx| { _ = plain.update(cx, |view, cx| view.copy_selection(false, window, cx)); }))
                    .item(PopupMenuItem::new("Copy as Markdown").on_click(move |_, window, cx| { _ = markdown.update(cx, |view, cx| view.copy_selection(true, window, cx)); }))
            }))
            .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(if self.replay.is_some() { format!("Replaying · {} / {} events", self.replay_position, self.records.len()) } else { "Display only · Cmd+F Find · Cmd+G Next · Cmd+C Copy · Cmd+Shift+C Copy as Markdown".into() }))
    }
}

struct RowView {
    row: Row,
    text: Entity<TextViewState>,
    diffs: Vec<Entity<DiffState>>,
    diff_error: Option<String>,
    open: bool,
    query: String,
    active: Option<usize>,
    reveal_pending: bool,
    highlight_signature: Option<u64>,
    copied: HashSet<usize>,
    index: usize,
    scroller: Entity<MessageScrollerState>,
    _subscription: Subscription,
}
impl RowView {
    fn new(
        row: Row,
        index: usize,
        scroller: Entity<MessageScrollerState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| {
            TextViewState::markdown(&row.body, cx)
                .selectable(true)
                .scrollable(false)
                .motion(TextViewMotion::default().with_stream_fade(Duration::from_millis(280)))
        });
        let subscription = cx.observe(&text, |this, _, cx| {
            this.scroller.update(cx, |state, cx| {
                state.remeasure_items(this.index..this.index + 1, cx);
            });
            cx.notify();
        });
        let open = !row.collapsible();
        let mut view = Self {
            row,
            text,
            diffs: vec![],
            diff_error: None,
            open,
            query: String::new(),
            active: None,
            reveal_pending: false,
            highlight_signature: None,
            copied: HashSet::new(),
            index,
            scroller,
            _subscription: subscription,
        };
        view.make_diffs(cx);
        view
    }
    fn set_row(&mut self, row: Row, cx: &mut Context<Self>) {
        let edits_changed = self.row.edits() != row.edits();
        self.text
            .update(cx, |text, cx| text.set_text(&row.body, cx));
        self.row = row;
        if edits_changed {
            self.make_diffs(cx);
        }
        cx.notify();
    }
    fn make_diffs(&mut self, cx: &mut Context<Self>) {
        self.diffs.clear();
        self.diff_error = None;
        for edit in self.row.edits() {
            let patch = edit_patch(&edit);
            match DiffFile::parse(&patch) {
                Ok(files) => self.diffs.push(cx.new(|cx| DiffState::new(files, cx))),
                Err(error) => {
                    self.diff_error =
                        Some(format!("Cannot display diff for {}: {error}", edit.path))
                }
            }
        }
    }
    fn highlight(&mut self, cx: &mut Context<Self>) {
        let query = self.query.clone();
        let title_count = find_ranges(&self.row.title, &query).len();
        let active = self.active.and_then(|index| index.checked_sub(title_count));
        let reveal = self.reveal_pending;
        let rendered = self.text.read(cx).rendered_text();
        let mut signature = std::hash::DefaultHasher::new();
        (rendered.as_str(), &query, active).hash(&mut signature);
        let signature = signature.finish();
        // Native highlight updates notify observers; avoid a render/notify cycle.
        if self.highlight_signature == Some(signature) {
            return;
        }
        self.highlight_signature = Some(signature);
        let revealed = self.text.update(cx, |text, cx| {
            let rendered = text.rendered_text();
            let ranges = find_ranges(rendered.as_str(), &query);
            let available = active.is_none() || active.is_some_and(|index| index < ranges.len());
            let mut highlights: Vec<_> = ranges
                .iter()
                .cloned()
                .map(|range| RangeHighlight::new(range, rgb(0x665c32)))
                .collect();
            if let Some(range) = active.and_then(|index| ranges.get(index)) {
                highlights.push(RangeHighlight::new(range.clone(), rgb(0x9a6a28)));
                if reveal {
                    _ = text.reveal_range(range.clone(), cx);
                }
            }
            // All ranges were computed against this very revision, never source byte offsets.
            text.set_range_highlights(highlights, cx)
                .expect("current rendered ranges");
            available
        });
        self.reveal_pending = reveal && !revealed;
    }
    fn copy_code(&mut self, key: usize, code: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(code));
        self.copied.insert(key);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            _ = this.update(cx, |view, cx| {
                view.copied.remove(&key);
                cx.notify();
            });
        })
        .detach();
    }
    fn toggle(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open = !self.open;
        self.scroller.update(cx, |state, cx| {
            state.remeasure_items(self.index..self.index + 1, cx);
        });
        cx.notify();
    }
}
impl Render for RowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.highlight(cx);
        let color = match self.row.kind {
            RowKind::User => 0xa6e3a1,
            RowKind::Thinking | RowKind::Status => MUTED,
            RowKind::Tool if self.row.tool.as_ref().is_some_and(|tool| tool.is_error) => 0xf38ba8,
            RowKind::Tool
                if self
                    .row
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.output.is_some()) =>
            {
                0x94e2d5
            }
            RowKind::Tool => 0xfab387,
            RowKind::Error => 0xf38ba8,
            RowKind::Permission | RowKind::Question => 0xf9e2af,
            RowKind::Worker | RowKind::Delegation => 0x94e2d5,
            _ => FG,
        };
        let owner = cx.entity().downgrade();
        let copied = self.copied.clone();
        let content = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .children(
                self.diff_error
                    .clone()
                    .map(|error| div().text_color(rgb(0xf38ba8)).child(error)),
            )
            .children(
                self.diffs
                    .iter()
                    .map(|diff| div().h(px(260.)).child(Diff::new(diff))),
            )
            .child(
                TextView::new(&self.text)
                    .selectable(true)
                    .scrollable(false)
                    .stream_fade(true)
                    .code_block_actions(move |block, _, _| {
                        let mut hash = std::hash::DefaultHasher::new();
                        block.code().hash(&mut hash);
                        block.lang().hash(&mut hash);
                        let key = hash.finish() as usize;
                        let code = block.code().to_string();
                        let owner = owner.clone();
                        Button::new(("copy-code", key))
                            .debug_selector({
                                let owner = owner.clone();
                                move || format!("copy-code-{}", owner.entity_id())
                            })
                            .small()
                            .label(if copied.contains(&key) {
                                "Copied"
                            } else {
                                "Copy"
                            })
                            .on_click(move |_, _, cx| {
                                _ = owner
                                    .update(cx, |view, cx| view.copy_code(key, code.clone(), cx));
                            })
                    }),
            );
        let header_highlights =
            find_ranges(&self.row.title, &self.query)
                .into_iter()
                .map(|range| {
                    (
                        range,
                        HighlightStyle {
                            background_color: Some(rgb(0x665c32).into()),
                            ..Default::default()
                        },
                    )
                });
        let title = StyledText::new(self.row.title.clone()).with_highlights(header_highlights);
        let header = div().text_size(px(13.)).text_color(rgb(color)).child(title);
        div()
            .id(("row", self.row.id))
            .w_full()
            .max_w(px(960.))
            .mx_auto()
            .mb(px(10.))
            .when(!self.row.scope.is_empty(), |view| {
                view.ml(px(18.)).border_l_2().border_color(rgb(BORDER))
            })
            .when(self.row.kind == RowKind::User, |view| {
                view.max_w(px(816.))
                    .ml_auto()
                    .mr_0()
                    .bg(rgb(0x313244))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded(px(10.))
            })
            .when(
                matches!(
                    self.row.kind,
                    RowKind::Tool | RowKind::Permission | RowKind::Question | RowKind::Delegation
                ),
                |view| {
                    view.bg(rgb(BASE))
                        .border_1()
                        .border_l_3()
                        .border_color(rgb(color))
                        .rounded(px(6.))
                },
            )
            .child(if self.row.collapsible() {
                Collapsible::new()
                    .open(self.open)
                    .child(
                        Button::new("fold")
                            .small()
                            .when(
                                !find_ranges(&self.row.title, &self.query).is_empty(),
                                |button| button.bg(rgb(0x665c32)),
                            )
                            .label(format!(
                                "{} {}",
                                if self.open { "▾" } else { "▸" },
                                self.row.title
                            ))
                            .text_color(rgb(color))
                            .on_click(cx.listener(Self::toggle)),
                    )
                    .content(content)
                    .into_any_element()
            } else {
                div().child(header).child(content).into_any_element()
            })
    }
}

/// Full replacement fragments, matching the HTML Edit/MultiEdit/Write preview.
/// No filesystem read is needed, and no full-file context is invented.
fn edit_patch(edit: &EditDiff) -> String {
    let old_path = serde_json::to_string(&format!("a/{}", edit.path)).expect("string JSON");
    let new_path = serde_json::to_string(&format!("b/{}", edit.path)).expect("string JSON");
    let old: Vec<_> = edit.old.lines().collect();
    let new: Vec<_> = edit.new.lines().collect();
    let mut patch = format!(
        "--- {old_path}\n+++ {new_path}\n@@ -{},{} +{},{} @@\n",
        usize::from(!old.is_empty()),
        old.len(),
        usize::from(!new.is_empty()),
        new.len()
    );
    for (prefix, lines) in [('-', old), ('+', new)] {
        for line in lines {
            patch.push(prefix);
            patch.push_str(line);
            patch.push('\n');
        }
    }
    patch
}

#[cfg(test)]
mod tests;
