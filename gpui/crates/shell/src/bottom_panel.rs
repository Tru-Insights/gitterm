//! Standalone Iced bottom-panel chrome; no terminal/process ownership.
//!
//! Host hookup: create an entity with `BottomPanel::new(Input::from_settings(...))`,
//! subscribe to `Outcome`, and render the entity above the workspace bar. Set
//! `opened` on an explicit terminal open, panel toggle, or restored bottom tabs;
//! settings alone never create a terminal. Refresh `input` with `set_input` and
//! provide the selected native terminal using `set_content`. The slot callback
//! returns an element referencing the host's retained terminal entity, never a
//! new PTY. RunState is resolved by the host's Iced `run_tab_action` policy (not
//! yet in core). Outcomes enter existing host launch/focus/close/URL policy;
//! Toggle and Resize must update and persist console_expanded/console_height.
//! On collapse or active-tab close the host restores main-terminal focus, and
//! on SelectTab it focuses that bottom terminal. Keeping `opened` after the last
//! close preserves Iced's empty panel. Cmd+J/the workspace-bar toggle is owned
//! by the host so a folded panel occupies zero space, including its divider.
use std::rc::Rc;

use gitterm_core::{
    settings::{QuickCommand, Settings},
    workspace_config::RepoKey,
};
use gpui::{
    AnyElement, App, Context, EventEmitter, MouseButton, SharedString, Window, div, prelude::*, px,
    rgb,
};

use crate::theme::Palette;

pub const HEADER_HEIGHT: f32 = 32.;
pub const DIVIDER_HEIGHT: f32 = 3.;
pub const EMPTY_MESSAGE: &str = "Press + for a terminal, or ▶ to run a repo's command";
pub const UNAVAILABLE_MESSAGE: &str = "Terminal unavailable";

/// Resolved by the host: absent tab, live matching command, or stale/exited tab.
/// The label remains `▶ command` in all three cases, just as in Iced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Run,
    Focus,
    Restart,
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub key: RepoKey,
    pub name: String,
    pub command: Option<String>,
    pub run_state: RunState,
    pub url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TerminalTab {
    /// Stable host terminal ID, not its current vector index.
    pub id: usize,
    pub title: Option<String>,
    pub alive: bool,
    pub run_repo: Option<RepoKey>,
}

#[derive(Clone, Debug)]
pub struct Input {
    /// Explicit panel activation, independent of saved expanded=true. Remains
    /// true after the last tab closes so the explicitly opened panel can be empty.
    pub opened: bool,
    pub expanded: bool,
    pub height: f32,
    /// Current client height, used by Iced's resize/window-resize clamp.
    pub window_height: f32,
    /// Listed local repos, in host order. Remote repos cannot launch locally.
    pub repos: Vec<Repo>,
    pub quick_commands: Vec<QuickCommand>,
    /// Explicitly opened bottom terminals only; never centre terminals.
    pub tabs: Vec<TerminalTab>,
    pub active: Option<usize>,
    pub mirroring: bool,
}

impl Input {
    pub fn from_settings<C, U, R>(settings: &Settings<C, U, R>, window_height: f32) -> Self {
        Self {
            opened: false,
            expanded: settings.console_expanded,
            // Iced clamps restored settings to 32..600, then to the window.
            height: clamp_height(settings.console_height.clamp(32., 600.), window_height),
            window_height,
            repos: Vec::new(),
            quick_commands: settings.quick_commands.clone(),
            tabs: Vec::new(),
            active: None,
            mirroring: settings.terminal_log_mirroring_enabled,
        }
    }

    pub fn visible(&self) -> bool {
        self.expanded && (self.opened || !self.tabs.is_empty())
    }

    pub fn panel_height(&self) -> f32 {
        clamp_height(self.height, self.window_height)
    }

    fn active_tab(&self) -> Option<&TerminalTab> {
        self.tabs.iter().find(|tab| Some(tab.id) == self.active)
    }

    /// Same sequence as Iced view_bottom_tab_bar; a spacer separates URL/runs.
    pub fn controls(&self, repo_picker_open: bool) -> Vec<Control> {
        let mut controls = vec![Control::new("▼", Action::Emit(Outcome::Toggle))];
        for (index, tab) in self.tabs.iter().enumerate() {
            let glyph = match (&tab.run_repo, tab.alive) {
                (Some(_), true) => "▶",
                (Some(_), false) => "■",
                (None, _) => ">_",
            };
            let title = tab
                .title
                .clone()
                .unwrap_or_else(|| format!("Terminal {}", index + 1));
            let mut control = Control::new(title, Action::Emit(Outcome::SelectTab { id: tab.id }));
            control.glyph = Some(glyph);
            controls.push(control);
            controls.push(Control::new(
                "×",
                Action::Emit(Outcome::CloseTab { id: tab.id }),
            ));
        }
        controls.push(Control::new(
            "+",
            Action::Emit(Outcome::AddTerminal { repo: None }),
        ));
        let repos: Vec<_> = self
            .repos
            .iter()
            .filter(|repo| repo.key.is_local())
            .collect();
        if repos.len() > 1 {
            controls.push(Control::new("▾", Action::RepoPicker));
            if repo_picker_open {
                for repo in &repos {
                    controls.push(Control::new(
                        format!("+ {}", repo.name),
                        Action::Emit(Outcome::AddTerminal {
                            repo: Some(repo.key.clone()),
                        }),
                    ));
                }
            }
        }
        if !self.quick_commands.is_empty() {
            controls.push(Control::new("⚡", Action::QuickPicker));
        }
        controls.push(Control::new("", Action::Spacer));
        if let Some(repo) = self
            .active_tab()
            .and_then(|tab| tab.run_repo.as_ref())
            .and_then(|key| repos.iter().find(|repo| &repo.key == key))
            && let Some(url) = &repo.url
        {
            controls.push(Control::new(
                format!("🔗 {url}"),
                Action::Emit(Outcome::OpenUrl {
                    repo: repo.key.clone(),
                }),
            ));
        }
        for repo in repos {
            if let Some(command) = &repo.command {
                let key = repo.key.clone();
                let outcome = match repo.run_state {
                    RunState::Run => Outcome::Run { repo: key },
                    RunState::Focus => Outcome::Focus { repo: key },
                    RunState::Restart => Outcome::Restart { repo: key },
                };
                controls.push(Control::new(format!("▶ {command}"), Action::Emit(outcome)));
            }
        }
        controls.push(Control::new(
            if self.mirroring {
                "Mirror:on"
            } else {
                "Mirror:off"
            },
            Action::Emit(Outcome::ToggleMirroring),
        ));
        controls
    }
}

/// Iced mouse/window-resize clamp. Unlike initial restore, dragging can exceed
/// 600 on a tall window. max(32) matches its window-resize path on tiny windows.
pub fn clamp_height(height: f32, window_height: f32) -> f32 {
    height.clamp(32., (window_height - 140.).max(32.))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Run {
        repo: RepoKey,
    },
    Focus {
        repo: RepoKey,
    },
    Restart {
        repo: RepoKey,
    },
    OpenUrl {
        repo: RepoKey,
    },
    /// Index in the supplied core QuickCommand vector, as in Iced.
    QuickCommand {
        id: usize,
    },
    SelectTab {
        id: usize,
    },
    CloseTab {
        id: usize,
    },
    Toggle,
    Resize {
        height: f32,
    },
    /// Iced's + / + repo controls; the host uses its workspace home for None.
    AddTerminal {
        repo: Option<RepoKey>,
    },
    ToggleMirroring,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Emit(Outcome),
    RepoPicker,
    QuickPicker,
    Spacer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub label: String,
    pub glyph: Option<&'static str>,
    pub action: Action,
}
impl Control {
    fn new(label: impl Into<String>, action: Action) -> Self {
        Self {
            label: label.into(),
            glyph: None,
            action,
        }
    }
}

/// Called only while visible. Return the host's selected retained terminal view.
pub type ContentSlot = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

pub struct BottomPanel {
    pub input: Input,
    content: Option<ContentSlot>,
    repo_picker_open: bool,
    quick_picker_open: bool,
    dragging: bool,
}
impl EventEmitter<Outcome> for BottomPanel {}
impl BottomPanel {
    pub fn new(input: Input) -> Self {
        Self {
            input,
            content: None,
            repo_picker_open: false,
            quick_picker_open: false,
            dragging: false,
        }
    }

    pub fn set_input(&mut self, input: Input, cx: &mut Context<Self>) {
        self.input = input;
        if !self.input.visible() {
            self.quick_picker_open = false;
            self.repo_picker_open = false;
            self.dragging = false;
        }
        if self.input.quick_commands.is_empty() {
            self.quick_picker_open = false;
        }
        cx.notify();
    }

    pub fn set_content(&mut self, content: Option<ContentSlot>, cx: &mut Context<Self>) {
        self.content = content;
        cx.notify();
    }

    fn activate(&mut self, action: Action, cx: &mut Context<Self>) {
        match action {
            Action::Emit(outcome) => {
                if matches!(outcome, Outcome::AddTerminal { .. }) {
                    self.repo_picker_open = false;
                }
                cx.emit(outcome);
            }
            Action::RepoPicker => self.repo_picker_open = !self.repo_picker_open,
            Action::QuickPicker => self.quick_picker_open = !self.input.quick_commands.is_empty(),
            Action::Spacer => {}
        }
        cx.notify();
    }

    fn quick_command(&mut self, id: usize, cx: &mut Context<Self>) {
        self.quick_picker_open = false;
        if id < self.input.quick_commands.len() {
            cx.emit(Outcome::QuickCommand { id });
        }
        cx.notify();
    }

    /// Coordinates match Iced: client height minus mouse y, including the bar.
    fn resize(&mut self, y: f32, cx: &mut Context<Self>) {
        let height = clamp_height(self.input.window_height - y, self.input.window_height);
        self.input.height = height;
        cx.emit(Outcome::Resize { height });
        cx.notify();
    }

    fn control(
        &self,
        index: usize,
        control: Control,
        p: Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        if control.action == Action::Spacer {
            return div().flex_1().into_any_element();
        }
        let selected = matches!(control.action, Action::Emit(Outcome::SelectTab { id }) if self.input.active == Some(id));
        let tab = matches!(control.action, Action::Emit(Outcome::SelectTab { .. }));
        let color = match &control.action {
            Action::Emit(Outcome::Run { .. } | Outcome::Focus { .. } | Outcome::Restart { .. }) => {
                p.green
            }
            Action::Emit(Outcome::OpenUrl { .. }) => p.blue,
            Action::QuickPicker => p.peach,
            Action::Emit(Outcome::ToggleMirroring) if self.input.mirroring => p.green,
            Action::Emit(Outcome::Toggle | Outcome::ToggleMirroring | Outcome::CloseTab { .. }) => {
                p.muted
            }
            _ if selected => p.text,
            _ => p.subtext0,
        };
        let label = control.label.clone();
        let close = matches!(control.action, Action::Emit(Outcome::CloseTab { .. }));
        let toggle = matches!(control.action, Action::Emit(Outcome::Toggle));
        let hint = match &control.action {
            Action::Emit(Outcome::Toggle) => "Collapse bottom panel".into(),
            Action::Emit(Outcome::CloseTab { id }) => format!("Close terminal {id}"),
            Action::Emit(Outcome::AddTerminal { repo: None }) => "New terminal".into(),
            Action::RepoPicker => "New terminal in repo".into(),
            Action::QuickPicker => "Quick commands".into(),
            _ => label.clone(),
        };
        let action = control.action;
        div()
            .id(SharedString::from(format!("bottom-control-{index}")))
            .debug_selector(move || format!("bottom-control-{index}"))
            .role(if tab {
                gpui::Role::Tab
            } else {
                gpui::Role::Button
            })
            .aria_label(hint)
            .when(tab, |s| s.aria_selected(selected))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.))
            .px(px(if close {
                2.
            } else if toggle {
                6.
            } else {
                8.
            }))
            .py(px(if close {
                0.
            } else if tab || toggle {
                4.
            } else {
                2.
            }))
            .rounded(px(3.))
            .text_size(px(if toggle { 10. } else { 12. }))
            .text_color(rgb(color))
            .when(selected, |s| s.bg(rgb(p.overlay)))
            .when(!selected && !close && !toggle, |s| {
                s.hover(|s| s.bg(rgb(p.surface)))
            })
            .when(close, |s| s.hover(|s| s.text_color(rgb(p.text))))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.activate(action.clone(), cx);
            }))
            .children(control.glyph.map(|glyph| {
                div()
                    .text_size(px(10.))
                    .text_color(rgb(match glyph {
                        "▶" => p.green,
                        "■" => p.muted,
                        _ => color,
                    }))
                    .child(glyph)
            }))
            .child(label)
            .into_any_element()
    }
}

impl Render for BottomPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.input.visible() {
            return div().into_any_element();
        }
        let p = Palette::current(cx);
        let mut header = div()
            .id("bottom-header")
            .debug_selector(|| "bottom-header".into())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .px(px(8.))
            .w_full()
            .h(px(HEADER_HEIGHT))
            .bg(rgb(p.mantle))
            .border_1()
            .border_color(rgb(p.surface));
        let mut controls = self
            .input
            .controls(self.repo_picker_open)
            .into_iter()
            .enumerate();
        while let Some((index, control)) = controls.next() {
            if let Action::Emit(Outcome::SelectTab { id }) = &control.action {
                let active = self.input.active == Some(*id);
                // Iced underlines the entire tab, including its separate close button.
                let (close_index, close) = controls.next().expect("tab has its close control");
                header = header.child(
                    div()
                        .flex()
                        .flex_none()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .child(self.control(index, control, p, cx))
                                .child(self.control(close_index, close, p, cx)),
                        )
                        .child(div().w_full().h(px(2.)).when(active, |s| s.bg(rgb(p.blue)))),
                );
            } else {
                header = header.child(self.control(index, control, p, cx));
            }
        }
        let active_id = self.input.active_tab().map(|tab| tab.id);
        let content = match (active_id, &self.content) {
            (Some(_), Some(slot)) => slot(window, cx),
            _ => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(rgb(p.muted))
                .child(if active_id.is_some() {
                    UNAVAILABLE_MESSAGE
                } else {
                    EMPTY_MESSAGE
                })
                .into_any_element(),
        };
        let mut panel = div()
            .id("standalone-bottom-panel")
            .debug_selector(|| "standalone-bottom-panel".into())
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .h(px(self.input.panel_height()))
            .flex_none()
            .bg(rgb(p.crust))
            .child(header)
            .child(
                div()
                    .id("bottom-content")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .py(px(2.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |_, _, _, cx| {
                            if let Some(id) = active_id {
                                cx.emit(Outcome::SelectTab { id });
                            }
                        }),
                    )
                    .child(content),
            );
        if self.quick_picker_open {
            let backdrop = div()
                .id("bottom-quick-backdrop")
                .absolute()
                .inset_0()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.quick_picker_open = false;
                    cx.notify();
                }));
            let mut menu = div()
                .id("bottom-quick-menu")
                .absolute()
                .top(px(4.))
                .left(px(8.))
                .w(px(268.))
                .max_h_full()
                .overflow_y_scroll()
                .p(px(4.))
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(p.border))
                .bg(rgb(p.mantle))
                .shadow_md();
            for (id, command) in self.input.quick_commands.iter().enumerate() {
                menu = menu.child(
                    div()
                        .id(SharedString::from(format!("bottom-quick-{id}")))
                        .debug_selector(move || format!("bottom-quick-{id}"))
                        .role(gpui::Role::Button)
                        .aria_label(command.name.clone())
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(6.))
                        .px(px(10.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(p.surface)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.quick_command(id, cx);
                        }))
                        .child(
                            div()
                                .w(px(20.))
                                .text_size(px(12.))
                                .text_color(rgb(p.peach))
                                .child("▸"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(1.))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(rgb(p.text))
                                        .child(command.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(p.subtext0))
                                        .child(command.command.clone()),
                                ),
                        ),
                );
            }
            panel = panel.child(backdrop).child(menu);
        }
        // Move/up listeners must remain active above the divider while growing
        // the panel. Element-local mouse-move handlers stop at its hitbox.
        let moving = cx.entity().downgrade();
        let released = moving.clone();
        let drag_events = gpui::canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        _ = moving.update(cx, |this, cx| {
                            if this.dragging {
                                this.resize(f32::from(event.position.y), cx);
                            }
                        });
                    }
                });
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture && event.button == MouseButton::Left {
                        _ = released.update(cx, |this, cx| {
                            if this.dragging {
                                this.dragging = false;
                                cx.notify();
                            }
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full();
        div()
            .id("bottom-panel-region")
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .font_family(crate::theme::LABEL_FONT)
            .child(drag_events)
            .child(
                div()
                    .id("standalone-bottom-divider")
                    .debug_selector(|| "standalone-bottom-divider".into())
                    .w_full()
                    .h(px(DIVIDER_HEIGHT))
                    .flex_none()
                    .bg(rgb(if self.dragging { p.blue } else { p.surface }))
                    .cursor_row_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dragging = true;
                            cx.notify();
                        }),
                    ),
            )
            .child(panel)
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "bottom_panel_tests.rs"]
mod tests;
