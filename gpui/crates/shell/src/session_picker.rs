//! Standalone port of Iced's `view_tab_picker` in `src/main.rs`.
//!
//! The host supplies a snapshot, opens/focuses this entity, and subscribes to
//! `SessionPickerEvent`. No settings, task store, clipboard or launch I/O lives
//! here. Preset indices refer to the supplied snapshot, just as in Iced.
use gitterm_core::{
    chat::types::AcpAgent,
    settings::{AgentPreset, ChatSelection, WorkspaceColor},
    tasks::TaskRecord,
};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, ParentElement as _, Render, Role, StatefulInteractiveElement as _,
    Styled as _, Window, div, px, rgb,
};

use crate::theme::Palette;

const HANDOFF_HINT: &str =
    "Agents start briefed automatically — clipboard holds a copy for terminals";

#[derive(Clone, Debug)]
pub struct AcpChat {
    pub id: AcpAgent,
    /// Empty means the agent chooses its own default, as in Iced.
    pub model: String,
}

/// Presentation facts only; task identity and title come from the core record.
#[derive(Clone, Debug)]
pub struct TaskContext {
    pub task_id: String,
    pub title: String,
    pub visible_session_count: usize,
    pub handoff_copied: bool,
}

impl TaskContext {
    pub fn from_task(
        task: &TaskRecord,
        visible_session_count: usize,
        handoff_copied: bool,
    ) -> Self {
        Self {
            task_id: task.task_id.clone(),
            title: task.title.clone(),
            visible_session_count,
            handoff_copied,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PickerInputs {
    pub chat: ChatSelection,
    pub local_workspace: bool,
    /// Available chats, displayed in `AcpAgent::ALL` order (never legacy Gemini).
    pub acp_chats: Vec<AcpChat>,
    /// Resume rows are derived from each core preset's `resume_command`.
    pub presets: Vec<AgentPreset>,
    pub task: Option<TaskContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPickerOutcome {
    NewChat,
    AcpChat {
        id: AcpAgent,
    },
    Cli {
        preset: usize,
    },
    CliResume {
        preset: usize,
        command: String,
    },
    Terminal,
    /// `None` launches a task terminal; `Some` launches that task agent preset.
    Continue {
        task_id: String,
        preset: Option<usize>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPickerEvent {
    Selected(SessionPickerOutcome),
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAction {
    OpenCli,
    Back,
    Select(SessionPickerOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowAccent {
    Primary,
    Secondary,
    Preset(WorkspaceColor),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PickerRow {
    pub label: String,
    pub description: String,
    pub icon: String,
    pub accent: RowAccent,
    pub action: RowAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PickerEntry {
    TaskHeader { title: String, handoff_copied: bool },
    Section(&'static str),
    Row(PickerRow),
}

/// Pure menu state, also used by the native view. Iced has no disabled action
/// rows or separator lines; the task headers are the only non-action entries.
pub struct PickerModel {
    inputs: PickerInputs,
    cli_open: bool,
    visible: bool,
}

impl PickerModel {
    pub fn new(inputs: PickerInputs) -> Self {
        Self {
            inputs,
            cli_open: false,
            visible: false,
        }
    }

    pub fn affordance_label(&self) -> &'static str {
        match &self.inputs.task {
            Some(task) if task.visible_session_count == 0 => "Start with ▾",
            Some(_) => "+ Continue ▾",
            None => "+ Session",
        }
    }

    pub fn open(&mut self) {
        self.cli_open = false;
        self.visible = true;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn entries(&self) -> Vec<PickerEntry> {
        let mut entries = vec![];
        let row = |label: String, description: String, icon: String, accent, action| {
            PickerEntry::Row(PickerRow {
                label,
                description,
                icon,
                accent,
                action,
            })
        };
        let select = RowAction::Select;
        let secondary = RowAccent::Secondary;
        if let Some(task) = &self.inputs.task {
            entries.push(PickerEntry::TaskHeader {
                title: task.title.clone(),
                handoff_copied: task.handoff_copied,
            });
            entries.push(PickerEntry::Section("AGENTS"));
        }
        if self.inputs.task.is_none() && !self.cli_open {
            let chat = &self.inputs.chat;
            let description = match &chat.effort {
                Some(effort) => format!("Claude · {} · {effort} effort", chat.model),
                None => format!("Claude · {}", chat.model),
            };
            entries.push(row(
                "New chat".into(),
                description,
                "✳".into(),
                RowAccent::Primary,
                select(SessionPickerOutcome::NewChat),
            ));
            if self.inputs.local_workspace {
                for id in AcpAgent::ALL {
                    if let Some(chat) = self.inputs.acp_chats.iter().find(|chat| chat.id == id) {
                        let model = if chat.model.is_empty() {
                            "its own default model"
                        } else {
                            &chat.model
                        };
                        entries.push(row(
                            format!("New {} chat", id.label()),
                            format!("{} · {model}", id.label()),
                            if id == AcpAgent::OpenCode {
                                "◇"
                            } else {
                                "✧"
                            }
                            .into(),
                            secondary,
                            select(SessionPickerOutcome::AcpChat { id }),
                        ));
                    }
                }
            }
            let names: Vec<_> = self
                .inputs
                .presets
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            entries.push(row(
                "CLI ›".into(),
                if names.is_empty() {
                    "No agent presets configured".into()
                } else {
                    names.join(", ")
                },
                "❯".into(),
                secondary,
                RowAction::OpenCli,
            ));
            entries.push(row(
                "Terminal".into(),
                "Plain shell".into(),
                "▸".into(),
                secondary,
                select(SessionPickerOutcome::Terminal),
            ));
        } else {
            if self.inputs.task.is_none() {
                entries.push(row(
                    "‹ CLI".into(),
                    "Back".into(),
                    String::new(),
                    secondary,
                    RowAction::Back,
                ));
            }
            for (index, preset) in self.inputs.presets.iter().enumerate() {
                let icon = if preset.icon.is_empty() {
                    preset.name.chars().next().unwrap_or('?').to_string()
                } else {
                    preset.icon.clone()
                };
                let outcome = match &self.inputs.task {
                    Some(task) => SessionPickerOutcome::Continue {
                        task_id: task.task_id.clone(),
                        preset: Some(index),
                    },
                    None => SessionPickerOutcome::Cli { preset: index },
                };
                entries.push(row(
                    preset.name.clone(),
                    preset.command.clone(),
                    icon,
                    RowAccent::Preset(preset.color),
                    select(outcome),
                ));
                if self.inputs.task.is_none()
                    && let Some(command) = &preset.resume_command
                {
                    entries.push(row(
                        format!("{} (resume)", preset.name),
                        command.clone(),
                        "↺".into(),
                        RowAccent::Preset(preset.color),
                        select(SessionPickerOutcome::CliResume {
                            preset: index,
                            command: command.clone(),
                        }),
                    ));
                }
            }
            if let Some(task) = &self.inputs.task {
                entries.push(PickerEntry::Section("TOOLS"));
                entries.push(row(
                    "Terminal".into(),
                    "Plain shell".into(),
                    "▸".into(),
                    secondary,
                    select(SessionPickerOutcome::Continue {
                        task_id: task.task_id.clone(),
                        preset: None,
                    }),
                ));
            }
        }
        entries
    }

    pub fn activate(&mut self, index: usize) -> Option<SessionPickerEvent> {
        if !self.visible {
            return None;
        }
        let PickerEntry::Row(row) = self.entries().get(index)?.clone() else {
            return None;
        };
        match row.action {
            RowAction::OpenCli => {
                self.cli_open = true;
                None
            }
            RowAction::Back => {
                self.cli_open = false;
                None
            }
            RowAction::Select(outcome) => {
                self.visible = false;
                Some(SessionPickerEvent::Selected(outcome))
            }
        }
    }

    pub fn dismiss(&mut self) -> Option<SessionPickerEvent> {
        if !self.visible {
            return None;
        }
        self.visible = false;
        Some(SessionPickerEvent::Dismissed)
    }

    /// Iced only handles Escape here: arrows/Enter/Tab do not select rows,
    /// and Escape closes the whole picker even inside CLI (not just Back).
    pub fn key(&mut self, key: &str) -> Option<SessionPickerEvent> {
        if key.eq_ignore_ascii_case("escape") {
            self.dismiss()
        } else {
            None
        }
    }
}

/// Place this entity last in a full-size, relatively positioned host overlay.
/// Call `open` after subscribing, and restore host focus on either emitted event.
pub struct SessionPicker {
    model: PickerModel,
    focus: FocusHandle,
}

impl EventEmitter<SessionPickerEvent> for SessionPicker {}

impl Focusable for SessionPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SessionPicker {
    pub fn new(inputs: PickerInputs, cx: &mut Context<Self>) -> Self {
        Self {
            model: PickerModel::new(inputs),
            focus: cx.focus_handle(),
        }
    }

    pub fn affordance_label(&self) -> &'static str {
        self.model.affordance_label()
    }

    /// Replaces the snapshot on every opening, so no store or stale catalog is retained.
    pub fn open(&mut self, inputs: PickerInputs, window: &mut Window, cx: &mut Context<Self>) {
        self.model = PickerModel::new(inputs);
        self.model.open();
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn emit(&mut self, event: Option<SessionPickerEvent>, cx: &mut Context<Self>) {
        if let Some(event) = event {
            cx.emit(event);
        }
        cx.notify();
    }
}

impl Render for SessionPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.model.is_visible() {
            return div().into_any_element();
        }
        let p = Palette::current(cx);
        let dark = gpui_component::Theme::global(cx).mode.is_dark();
        let mut card = div()
            .id("session-picker-card")
            .role(Role::Group)
            .aria_label("Session picker")
            .absolute()
            .top(px(32.))
            .left(px(56.))
            .w(px(270.))
            .p(px(4.))
            .flex()
            .flex_col()
            .bg(rgb(p.mantle))
            .border_1()
            .border_color(rgb(if dark { 0x45475a } else { p.surface }))
            .rounded(px(6.))
            .shadow_md()
            .text_color(rgb(p.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for (index, entry) in self.model.entries().into_iter().enumerate() {
            card = match entry {
                PickerEntry::TaskHeader {
                    title,
                    handoff_copied,
                } => {
                    let mut header = div()
                        .px(px(10.))
                        .pt(px(7.))
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .font_family("Menlo")
                                .text_size(px(9.))
                                .text_color(rgb(p.blue))
                                .child("CONTINUE TASK"),
                        )
                        .child(div().font_family("Menlo").text_size(px(12.)).child(title));
                    if handoff_copied {
                        header = header.child(
                            div()
                                .text_size(px(9.))
                                .text_color(rgb(p.muted))
                                .child(HANDOFF_HINT),
                        );
                    }
                    card.child(header)
                }
                PickerEntry::Section(label) => card.child(
                    div()
                        .px(px(10.))
                        .pt(px(if label == "TOOLS" { 8. } else { 3. }))
                        .pb(px(3.))
                        .font_family("Menlo")
                        .text_size(px(9.))
                        .text_color(rgb(p.muted))
                        .child(label),
                ),
                PickerEntry::Row(row) => {
                    let color = match row.accent {
                        RowAccent::Primary => p.blue,
                        RowAccent::Secondary => p.muted,
                        RowAccent::Preset(color) => preset_color(color, dark),
                    };
                    card.child(
                        div()
                            .id(("session-picker-row", index))
                            .role(Role::Button)
                            .aria_label(row.label.clone())
                            .aria_description(row.description.clone())
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(10.))
                            .py(px(6.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(rgb(p.surface)))
                            .child(
                                div()
                                    .w(px(24.))
                                    .flex_shrink_0()
                                    .font_family("Menlo")
                                    .text_size(px(14.))
                                    .text_color(rgb(color))
                                    .child(row.icon),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.))
                                    .child(div().text_size(px(13.)).child(row.label))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(p.muted))
                                            .child(row.description),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let event = this.model.activate(index);
                                this.emit(event, cx);
                                cx.stop_propagation();
                            })),
                    )
                }
            };
        }
        div()
            .id("session-picker")
            .absolute()
            .inset_0()
            .occlude()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let event = this.model.dismiss();
                    this.emit(event, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if let Some(event) = this.model.key(&event.keystroke.key) {
                    this.emit(Some(event), cx);
                    cx.stop_propagation();
                }
            }))
            .child(card)
            .into_any_element()
    }
}

fn preset_color(color: WorkspaceColor, dark: bool) -> u32 {
    // Same Catppuccin values as Iced's config::WorkspaceColor::color.
    let (mocha, latte) = match color {
        WorkspaceColor::Lavender => (0xb4befe, 0x7287fd),
        WorkspaceColor::Blue => (0x89b4fa, 0x1e66f5),
        WorkspaceColor::Green => (0xa6e3a1, 0x40a02b),
        WorkspaceColor::Peach => (0xfab387, 0xfe640b),
        WorkspaceColor::Pink => (0xf5c2e7, 0xea76cb),
        WorkspaceColor::Yellow => (0xf9e2af, 0xdf8e1d),
        WorkspaceColor::Red => (0xf38ba8, 0xd20f39),
        WorkspaceColor::Teal => (0x94e2d5, 0x179299),
    };
    if dark { mocha } else { latte }
}

#[cfg(test)]
#[path = "session_picker_tests.rs"]
mod tests;
