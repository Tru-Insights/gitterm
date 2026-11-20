//! Standalone port of Iced's `task_switcher_entries` / `view_task_switcher`.
//!
//! Hookup (deliberately not installed in Shell): the host's
//! `app_menus::TaskSwitcherOpen` handler creates/opens this entity and subscribes
//! to `Outcome`. Close/Next/Previous/ActivateFirst and Escape are handled here.
//! Mount last in a full-size relative overlay; restore host focus on an outcome.
//! While mounted, the host must replace its IcedMain/IcedChat contexts with
//! IcedTaskSwitcher (and suppress its background shortcut capture), as Iced
//! consumes background commands while this modal input is active.
//! `Activate` maps to Iced's TaskSelected, `ContextBack` to TaskContextBack.
//! TaskOverview (Cmd+0) is a separate host navigation action, absent from this
//! overlay in Iced: focus the General host tab with the current task selected.
//!
//! Build Inputs from `data::IcedWorkData::tasks` (already workspace-scoped),
//! mapping core TaskRecords through `TaskRow::from_task`. Supply Iced's live
//! `task_state_copy` detail, ordinary workspace-tab count, active task context,
//! and context MRU (including None for General). The #250 snapshot's task-rail
//! order is NOT switcher order; Model sorts using Iced's comparator. Detail
//! needs live open-session/delivery, queue and elapsed facts, which data.rs
//! alone does not supply. This view neither loads nor mutates a task store.
use gitterm_core::tasks::{TaskLifecycle, TaskRecord};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    Role, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::{app_menus, theme::Palette};

pub const PLACEHOLDER: &str = "Jump to General or a task…";
pub const EMPTY_LABEL: &str = "No matching task";
pub const KEY_HINT: &str = "↑↓ selects · Enter opens · Esc closes";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRow {
    pub task_id: String,
    pub label: String,
    /// Iced's task_state_copy, supplied by the host with its live session facts.
    pub detail: String,
    pub needs_attention: bool,
}

impl TaskRow {
    pub fn from_task(task: &TaskRecord, detail: String) -> Option<Self> {
        (task.lifecycle != TaskLifecycle::Archived).then(|| Self {
            task_id: task.task_id.clone(),
            label: task.issue.as_ref().map_or_else(
                || task.title.clone(),
                |issue| format!("{}  {}", issue.key, task.title),
            ),
            detail,
            needs_attention: task.attention.reason.is_some() || task.attention.unread,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub tasks: Vec<TaskRow>,
    pub general_session_count: usize,
    pub current_task_id: Option<String>,
    /// Most recently visited first. None denotes General, as in Iced.
    pub context_mru: Vec<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Activate { task_id: String },
    ContextBack,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub task_id: Option<String>,
    pub label: String,
    pub detail: String,
    pub needs_attention: bool,
    pub is_current: bool,
    pub is_highlighted: bool,
}

impl Row {
    pub fn marker(&self) -> &'static str {
        if self.task_id.is_some() { "◇" } else { "●" }
    }

    pub fn badge(&self) -> &'static str {
        if self.is_current { "CURRENT" } else { "" }
    }
}

/// Pure presentation state. All ordering/filtering rules here are copied from
/// Iced; core owns the task records and lifecycle, not switcher presentation.
pub struct Model {
    inputs: Inputs,
    query: String,
    selection: Option<usize>,
    visible: bool,
}

impl Model {
    pub fn new(inputs: Inputs) -> Self {
        Self {
            inputs,
            query: String::new(),
            selection: None,
            visible: false,
        }
    }

    pub fn open(&mut self) {
        self.visible = true;
        self.query.clear();
        self.selection = None;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.selection = None;
    }

    pub fn rows(&self) -> Vec<Row> {
        let count = self.inputs.general_session_count;
        let mut rows = vec![Row {
            task_id: None,
            label: "General".into(),
            detail: format!(
                "{count} workspace session{}",
                if count == 1 { "" } else { "s" }
            ),
            needs_attention: false,
            is_current: self.inputs.current_task_id.is_none(),
            is_highlighted: false,
        }];
        rows.extend(self.inputs.tasks.iter().map(|task| Row {
            task_id: Some(task.task_id.clone()),
            label: task.label.clone(),
            detail: task.detail.clone(),
            needs_attention: task.needs_attention,
            is_current: self.inputs.current_task_id.as_ref() == Some(&task.task_id),
            is_highlighted: false,
        }));
        let mut scored: Vec<_> = rows
            .into_iter()
            .filter_map(|row| fuzzy_score(&row.label, &self.query).map(|score| (row, score)))
            .collect();
        let rank = |row: &Row| {
            self.inputs
                .context_mru
                .iter()
                .position(|id| id == &row.task_id)
                .unwrap_or(usize::MAX - 1)
        };
        scored.sort_by(|(left, ls), (right, rs)| {
            right
                .needs_attention
                .cmp(&left.needs_attention)
                .then_with(|| rs.cmp(ls))
                .then_with(|| left.is_current.cmp(&right.is_current))
                .then_with(|| rank(left).cmp(&rank(right)))
                .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
        });
        let mut rows: Vec<_> = scored.into_iter().map(|(row, _)| row).collect();
        let default = if self.query.trim().is_empty() {
            self.inputs
                .context_mru
                .iter()
                .find_map(|id| {
                    if id == &self.inputs.current_task_id {
                        return None;
                    }
                    rows.iter().position(|row| &row.task_id == id)
                })
                .unwrap_or(0)
        } else {
            0
        };
        let highlighted = self
            .selection
            .unwrap_or(default)
            .min(rows.len().saturating_sub(1));
        if let Some(row) = rows.get_mut(highlighted) {
            row.is_highlighted = true;
        }
        rows
    }

    /// Iced clamps at both ends; there is deliberately no wrap-around.
    pub fn next(&mut self) {
        self.navigate(true);
    }

    pub fn previous(&mut self) {
        self.navigate(false);
    }

    fn navigate(&mut self, down: bool) {
        if !self.visible {
            return;
        }
        let rows = self.rows();
        if let Some(index) = rows.iter().position(|row| row.is_highlighted) {
            self.selection = Some(if down {
                (index + 1).min(rows.len() - 1)
            } else {
                index.saturating_sub(1)
            });
        }
    }

    pub fn activate_first(&mut self) -> Option<Outcome> {
        let index = self.rows().iter().position(|row| row.is_highlighted)?;
        self.activate(index)
    }

    pub fn activate(&mut self, index: usize) -> Option<Outcome> {
        if !self.visible {
            return None;
        }
        let target = self.rows().get(index)?.task_id.clone();
        self.close();
        Some(
            target.map_or(Outcome::ContextBack, |task_id| Outcome::Activate {
                task_id,
            }),
        )
    }

    pub fn close(&mut self) -> Option<Outcome> {
        if !self.visible {
            return None;
        }
        self.visible = false;
        self.query.clear();
        self.selection = None;
        Some(Outcome::Close)
    }
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    if candidate == query {
        return Some(100);
    }
    if candidate.starts_with(&query) {
        return Some(80);
    }
    if candidate.contains(&query) {
        return Some(60);
    }
    let mut chars = candidate.chars();
    query
        .chars()
        .all(|wanted| chars.by_ref().any(|c| c == wanted))
        .then_some(20)
}

pub struct TaskSwitcher {
    model: Model,
    input: Entity<InputState>,
    focus: FocusHandle,
    _subscription: Subscription,
}

impl EventEmitter<Outcome> for TaskSwitcher {}
impl Focusable for TaskSwitcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TaskSwitcher {
    pub fn new(inputs: Inputs, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        let subscription = cx.subscribe(&input, |this, input, event, cx| {
            match event {
                InputEvent::Change => {
                    let query = input.read(cx).value().to_string();
                    if this.model.query() != query {
                        this.model.set_query(query);
                    }
                }
                InputEvent::PressEnter { .. } => {
                    let outcome = this.model.activate_first();
                    this.emit(outcome, cx);
                }
                _ => {}
            }
            cx.notify();
        });
        Self {
            model: Model::new(inputs),
            input,
            focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    pub fn open(&mut self, inputs: Inputs, window: &mut Window, cx: &mut Context<Self>) {
        self.model = Model::new(inputs);
        self.model.open();
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn emit(&mut self, outcome: Option<Outcome>, cx: &mut Context<Self>) {
        if let Some(outcome) = outcome {
            cx.emit(outcome);
        }
        cx.notify();
    }

    // Capture arrows before the text input's cursor handlers, as Iced does.
    fn capture_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                let outcome = self.model.close();
                self.emit(outcome, cx);
            }
            "t" | "T" if event.keystroke.modifiers.platform => {
                let outcome = self.model.close();
                self.emit(outcome, cx);
            }
            "down" => self.model.next(),
            "up" => self.model.previous(),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for TaskSwitcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.model.is_visible() {
            return div().into_any_element();
        }
        let p = Palette::current(cx);
        let dark = gpui_component::Theme::global(cx).mode.is_dark();
        let hover = if dark { 0x45475a } else { 0xbcc0cc }; // Iced surface1.
        let mut results = div()
            .id("task-switcher-results")
            .h(px(330.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.));
        let rows = self.model.rows();
        if rows.is_empty() {
            results = results.child(
                div()
                    .px(px(10.))
                    .py(px(18.))
                    .text_size(px(12.))
                    .text_color(rgb(p.subtext0))
                    .child(EMPTY_LABEL),
            );
        }
        for (index, row) in rows.into_iter().enumerate() {
            let accent = if row.needs_attention {
                p.peach
            } else if row.task_id.is_some() {
                p.blue
            } else {
                p.green
            };
            let marker = row.marker();
            let badge = row.badge();
            results = results.child(
                div()
                    .id(("task-switcher-row", index))
                    .debug_selector(move || format!("task-switcher-row-{index}"))
                    .role(Role::Button)
                    .aria_label(row.label.clone())
                    .aria_description(row.detail.clone())
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(7.))
                    .flex_shrink_0()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(rgb(if row.is_highlighted {
                        p.surface
                    } else {
                        p.mantle
                    }))
                    .when(row.is_highlighted, |row| {
                        row.border_1().border_color(rgb(p.border))
                    })
                    .hover(move |s| s.bg(rgb(hover)))
                    .child(
                        div()
                            .w(px(18.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(rgb(accent))
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(12.)).child(row.label))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(if row.needs_attention {
                                        p.peach
                                    } else {
                                        p.subtext0
                                    }))
                                    .child(row.detail),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted))
                            .child(badge),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let outcome = this.model.activate(index);
                        this.emit(outcome, cx);
                        cx.stop_propagation();
                    })),
            );
        }
        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let card = div()
            .id("task-switcher-card")
            .debug_selector(|| "task-switcher-card".into())
            .role(Role::Group)
            .aria_label("Switch context")
            .w(px(480.))
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .bg(rgb(p.mantle))
            .border_1()
            .border_color(rgb(p.border))
            .rounded(px(7.))
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(5.), gpui::Hsla::from(rgb(0)).opacity(0.4))
                    .blur_radius(px(18.)),
            ])
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .child("SWITCH CONTEXT")
                    .child("⌘T"),
            )
            .child(
                div().key_context("IcedTaskSwitcherInput").child(
                    Input::new(&self.input)
                        .appearance(false)
                        .h(px(32.9))
                        .px(px(9.))
                        .py(px(7.))
                        .text_size(px(13.))
                        .line_height(gpui::relative(1.3))
                        .bg(rgb(p.base))
                        .border_1()
                        .border_color(rgb(if input_focused { p.blue } else { p.border }))
                        .rounded(px(4.)),
                ),
            )
            .child(results)
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted))
                    .child(KEY_HINT),
            );
        let mut overlay = div()
            .id("task-switcher")
            .debug_selector(|| "task-switcher".into())
            .absolute()
            .inset_0()
            .occlude()
            .track_focus(&self.focus)
            .key_context("GitTermShell IcedTaskSwitcher")
            .font_family("Menlo")
            .line_height(gpui::relative(1.3))
            .text_color(rgb(p.text))
            .bg(gpui::Hsla::from(rgb(p.crust)).opacity(0.42))
            .capture_key_down(cx.listener(Self::capture_key))
            .on_action(
                cx.listener(|this, _: &app_menus::TaskSwitcherClose, _, cx| {
                    let outcome = this.model.close();
                    this.emit(outcome, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(cx.listener(|this, _: &app_menus::Escape, _, cx| {
                let outcome = this.model.close();
                this.emit(outcome, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &app_menus::TaskSwitcherNext, _, cx| {
                this.model.next();
                cx.notify();
                cx.stop_propagation();
            }))
            .on_action(
                cx.listener(|this, _: &app_menus::TaskSwitcherPrevious, _, cx| {
                    this.model.previous();
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &app_menus::TaskSwitcherActivateFirst, _, cx| {
                    let outcome = this.model.activate_first();
                    this.emit(outcome, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let outcome = this.model.close();
                    this.emit(outcome, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .pt(px(56.))
                    .flex()
                    .justify_center()
                    .items_start()
                    .child(card),
            );
        // GPUI resolves bindings before raw key listeners. Consume background
        // app actions here, retaining native window/menu and input editing.
        for binding in app_menus::key_bindings() {
            if !matches!(
                binding.action().name().rsplit("::").next(),
                Some(
                    "TaskSwitcherOpen"
                        | "TaskSwitcherClose"
                        | "TaskSwitcherNext"
                        | "TaskSwitcherPrevious"
                        | "TaskSwitcherActivateFirst"
                        | "Escape"
                        | "Copy"
                        | "Cut"
                        | "Paste"
                        | "SelectAll"
                        | "Undo"
                        | "Redo"
                        | "About"
                        | "Hide"
                        | "HideOthers"
                        | "ShowAll"
                        | "Quit"
                        | "Minimize"
                        | "Maximize"
                        | "Fullscreen"
                )
            ) {
                overlay =
                    overlay.on_boxed_action(binding.action(), |_, _, cx| cx.stop_propagation());
            }
        }
        overlay.into_any_element()
    }
}

#[cfg(test)]
#[path = "task_switcher_tests.rs"]
mod tests;
