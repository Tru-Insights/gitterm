//! Disposable composer projection. Settings are acknowledged display values only;
//! no picker or checkout command is constructed here.
use gitterm_core::chat::{
    state::{AgentEvent, EventEffects, PromptStatus, State},
    types::{AgentSessionState, HarnessEvent, ItemKind},
};

#[derive(Debug, Clone)]
pub struct QueuedRow {
    pub id: String,
    pub text: String,
    pub status: PromptStatus,
    pub withdraw_pending: bool,
}
impl QueuedRow {
    pub fn withdrawable(&self) -> bool {
        !self.withdraw_pending
            && matches!(self.status, PromptStatus::Pending | PromptStatus::Queued)
    }
    pub fn label(&self) -> &'static str {
        if self.withdraw_pending {
            return "Withdrawal pending";
        }
        match self.status {
            PromptStatus::Pending => "Sending to queue",
            PromptStatus::Queued => "Queued",
            PromptStatus::Started => "Started",
            PromptStatus::Completed => "Completed",
            PromptStatus::Cancelled => "Withdrawn",
            PromptStatus::Unsent => "Unsent · retained, no automatic resend",
            PromptStatus::Uncertain => "Uncertain · may have been sent, retry blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
}

#[derive(Debug, Default)]
pub struct ComposerState {
    pub queue: Vec<QueuedRow>,
    pub pending_request: bool,
    pub streaming: bool,
    pub status: String,
    pub model: String,
    pub mode: String,
    pub effort: String,
    pub checkout: String,
    pub checkout_path: Option<String>,
    pub commands: Vec<SlashCommand>,
    pub commands_ready: bool,
    pub notice: Option<String>,
    started_at_ms: Option<u64>,
    ended_at_ms: Option<u64>,
    pub(crate) slash_index: usize,
    pub(crate) slash_dismissed: bool,
}
impl ComposerState {
    pub fn project(&mut self, state: &State, effects: Option<&EventEffects>) {
        self.pending_request = state.has_pending_requests();
        self.streaming = matches!(state.state, AgentSessionState::Streaming);
        let was_streaming = self.started_at_ms.is_some() && self.ended_at_ms.is_none();
        let mut reset = state.conversation.is_empty();
        let mut activity = "Working".to_string();
        let mut started = None;
        let mut ended = None;
        for event in &state.conversation {
            match event {
                AgentEvent::Other(value) if value["kind"] == "turn_timing" => {
                    if let Some(ms) = value["start_ms"].as_u64() {
                        started = Some(ms);
                        ended = None;
                    }
                    if let Some(ms) = value["end_ms"].as_u64() {
                        ended = Some(ms);
                    }
                }
                AgentEvent::Harness(HarnessEvent::TurnStarted { .. }) => {
                    activity = "Working".into()
                }
                AgentEvent::Harness(HarnessEvent::ThinkingDelta(_)) => activity = "Thinking".into(),
                AgentEvent::Harness(HarnessEvent::TextDelta(_)) => activity = "Responding".into(),
                AgentEvent::Harness(HarnessEvent::ItemStarted {
                    kind: ItemKind::ToolCall { name, .. },
                    ..
                }) => activity = format!("Running {name}"),
                AgentEvent::Harness(HarnessEvent::ItemCompleted { .. }) => {
                    activity = "Working".into()
                }
                AgentEvent::Harness(HarnessEvent::ConversationReset { .. }) => {
                    activity = "Working".into();
                    reset = true;
                    started = None;
                    ended = None;
                }
                _ => {}
            }
        }
        self.started_at_ms = started.or_else(|| {
            if self.streaming {
                if was_streaming {
                    self.started_at_ms
                } else {
                    Some(now_ms())
                }
            } else if !reset {
                self.started_at_ms
            } else {
                None
            }
        });
        self.ended_at_ms = ended.or_else(|| {
            if self.streaming || reset {
                None
            } else {
                self.ended_at_ms.or_else(|| was_streaming.then(now_ms))
            }
        });
        self.status = if self.pending_request {
            "Needs you".into()
        } else {
            match &state.state {
                AgentSessionState::Idle => if self.started_at_ms.is_some() {
                    "Done"
                } else {
                    "Ready"
                }
                .into(),
                AgentSessionState::Streaming => activity,
                AgentSessionState::Stopped => "Stopped".into(),
                AgentSessionState::Errored(error) => format!("Failed · {error}"),
            }
        };
        let ready = state
            .conversation
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::Harness(HarnessEvent::Ready { models, modes, .. }) => {
                    Some((models, modes))
                }
                _ => None,
            });
        let confirmed_model = state.acknowledged.model.as_deref();
        let base_model = confirmed_model.map(|value| value.strip_suffix("[1m]").unwrap_or(value));
        self.model = base_model.map_or_else(
            || "Model".into(),
            |value| {
                let info =
                    ready.and_then(|(models, _)| models.iter().find(|info| info["value"] == value));
                if value == "default" {
                    info.and_then(|info| info["description"].as_str())
                        .and_then(|description| description.split(" · ").next())
                        .filter(|description| !description.is_empty())
                        .unwrap_or("Claude settings")
                        .into()
                } else {
                    info.and_then(|info| info["displayName"].as_str())
                        .unwrap_or(value)
                        .into()
                }
            },
        );
        self.mode = state.acknowledged.mode.as_deref().map_or_else(
            || "Mode".into(),
            |value| {
                ready
                    .and_then(|(_, modes)| modes.iter().find(|info| info["value"] == value))
                    .and_then(|info| info["displayName"].as_str())
                    .unwrap_or(match value {
                        "default" => "Default",
                        "acceptEdits" => "Accept edits",
                        "plan" => "Plan mode",
                        "auto" => "Auto mode",
                        "dontAsk" => "Don't ask",
                        "bypassPermissions" => "Bypass permissions",
                        other => other,
                    })
                    .into()
            },
        );
        self.effort = state.acknowledged.effort.as_ref().map_or_else(
            || "Effort".into(),
            |effort| {
                let level = effort.effort.as_deref().unwrap_or("auto");
                let mut label = level.to_string();
                if let Some(first) = label.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                let context = if confirmed_model.is_some_and(|value| value.ends_with("[1m]")) {
                    "1M"
                } else {
                    "200k"
                };
                format!("{label} · {context}")
            },
        );
        // The reducer owns the acknowledged path; the host's current-checkout
        // label is stable even when its absolute path changes.
        self.checkout = "Current checkout".into();
        self.checkout_path = state
            .checkout
            .as_ref()
            .map(|path| path.display().to_string());
        self.queue = state
            .queued_prompts
            .iter()
            .map(|(id, queued)| QueuedRow {
                id: id.clone(),
                text: queued.prompt.text.clone(),
                status: queued.status,
                withdraw_pending: queued.withdraw_pending,
            })
            .collect();
        self.commands_ready = ready.is_some();
        self.commands = state
            .conversation
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::Harness(HarnessEvent::Ready { commands, .. }) => Some(
                    commands
                        .iter()
                        .filter_map(|value| {
                            let name = value["name"].as_str()?.trim_start_matches('/');
                            if name.is_empty()
                                || name.chars().any(|c| c.is_whitespace() || c.is_control())
                            {
                                return None;
                            }
                            Some(SlashCommand {
                                name: name.into(),
                                description: value["description"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .into(),
                                argument_hint: value["argumentHint"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .into(),
                            })
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        if let Some(effects) = effects
            && !effects.notices.is_empty()
        {
            self.notice = Some(effects.notices.join("\n"));
        }
    }

    /// The same chip sequence as agent_chat.html; these remain display-only.
    pub fn chips(&self) -> [(&'static str, String); 6] {
        [
            ("review", "Review / Consult…".into()),
            ("model", self.model.clone()),
            ("effort", self.effort.clone()),
            ("mode", self.mode.clone()),
            ("checkout", self.checkout.clone()),
            ("guidelines", "guidelines".into()),
        ]
    }

    pub fn status_at(&self, now: u64) -> String {
        self.started_at_ms.map_or_else(
            || self.status.clone(),
            |start| {
                format!(
                    "{} {}",
                    self.status,
                    elapsed(self.ended_at_ms.unwrap_or(now).saturating_sub(start))
                )
            },
        )
    }

    pub fn slash_matches(&self, draft: &str) -> Vec<&SlashCommand> {
        if self.slash_dismissed {
            return vec![];
        }
        let Some(prefix) = draft.strip_prefix('/') else {
            return vec![];
        };
        if prefix.chars().any(char::is_whitespace) {
            return vec![];
        }
        let query = prefix.to_lowercase();
        let mut matches: Vec<_> = self
            .commands
            .iter()
            .filter(|command| command.name.to_lowercase().contains(&query))
            .collect();
        // Stable sort keeps the harness order within Iced's exact/prefix/within groups.
        matches.sort_by_key(|command| {
            let name = command.name.to_lowercase();
            if name == query {
                0
            } else if name.starts_with(&query) {
                1
            } else {
                2
            }
        });
        matches
    }

    pub fn slash_key(&mut self, draft: &str, key: &str) -> SlashOutcome {
        let matches = self.slash_matches(draft);
        if matches.is_empty() {
            return SlashOutcome::Ignored;
        }
        let count = matches.len();
        let index = self.slash_index.min(count - 1);
        match key {
            "down" => self.slash_index = (index + 1) % count,
            "up" => self.slash_index = (index + count - 1) % count,
            "escape" => self.slash_dismissed = true,
            "tab" | "enter" => {
                return SlashOutcome::Fill {
                    name: matches[index].name.clone(),
                    submit: key == "enter" && matches[index].argument_hint.is_empty(),
                };
            }
            _ => return SlashOutcome::Ignored,
        }
        SlashOutcome::Handled
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SlashOutcome {
    Ignored,
    Handled,
    Fill { name: String, submit: bool },
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn elapsed(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.)
    } else {
        format!("{}m {}s", ms / 60_000, ms / 1000 % 60)
    }
}

#[cfg(target_os = "macos")]
mod ui {
    use super::*;
    use crate::view::ChatView;
    use gpui::{
        Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
        StatefulInteractiveElement, Styled, div, px, rgb,
    };
    use gpui_component::{
        Disableable,
        button::Button,
        input::{Textarea, TextareaState},
    };

    impl ComposerState {
        pub(crate) fn render_activity(
            &self,
            appearance: crate::ChatAppearance,
        ) -> impl IntoElement {
            let palette = appearance.palette();
            let color = if self.pending_request {
                palette.warn
            } else if self.streaming {
                palette.accent
            } else {
                palette.result
            };
            div()
                .id("chat-activity")
                .debug_selector(|| "chat-activity".into())
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(6.))
                .pt(px(4.))
                .font_family("Menlo")
                .text_size(px(appearance.size(12.)))
                .text_color(rgb(palette.muted))
                .child(div().size(px(7.)).rounded_full().bg(rgb(color)))
                .child(self.status_at(now_ms()))
        }

        // Keep host appearance explicit alongside the retained input and delivery state.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn render(
            &self,
            input: &Entity<TextareaState>,
            unavailable: Option<String>,
            send_blocked: bool,
            slash_scroll: &gpui::ScrollHandle,
            appearance: crate::ChatAppearance,
            window: &gpui::Window,
            cx: &mut Context<ChatView>,
        ) -> impl IntoElement {
            let palette = appearance.palette();
            let disabled = unavailable.is_some();
            let owner = cx.entity().downgrade();
            let draft = input.read(cx).value();
            let show_slash = !disabled
                && input.read(cx).focus_handle(cx).is_focused(window)
                && input.read(cx).selected_range().start == draft.len();
            div().id("composer-controls").debug_selector(|| "composer-controls".into()).relative().flex().flex_col().gap(px(6.))
                .capture_any_mouse_down(cx.listener(|view, _, _, cx| view.reopen_slash(cx)))
                .on_click(cx.listener(|view, _, _, cx| { view.reopen_slash(cx); cx.stop_propagation(); }))
                .on_drop(cx.listener(|view, paths: &gpui::ExternalPaths, window, cx| {
                    view.attach_paths(paths.0.to_vec(), cx);
                    view.focus_composer(window, cx);
                }))
                .child(div().id("composer-queue").max_h(px(160.)).overflow_y_scroll().children(self.queue.iter().map(|row| {
                    let id = row.id.clone();
                    div().flex().gap(px(8.)).child(div().flex_1().child(format!("{} · {} · {}", row.id, row.label(), row.text)))
                        .child(Button::new(gpui::SharedString::from(format!("withdraw-{}", row.id)))
                            .label("Withdraw").disabled(disabled || !row.withdrawable())
                            .on_click(cx.listener(move |view, _, _, cx| view.withdraw(id.clone(), cx))))
                })))
                .children(self.notice.iter().map(|notice| div().text_color(rgb(palette.err)).child(notice.clone())))
                .child(div().id("composer-input").debug_selector(|| "composer-input".into()).child(Textarea::new(input).disabled(disabled)
                    .on_paste(move |item, _, cx| owner.update(cx, |view, cx| view.paste_images(item, cx)).unwrap_or(false))
                    .bg(rgb(palette.alt))
                    .text_size(px(appearance.size(13.5))).border_color(rgb(palette.strong)).rounded(px(6.))))
                .children(show_slash.then(|| {
                    let matches = self.slash_matches(&draft);
                    let waiting = !self.commands_ready && !self.slash_dismissed && draft.starts_with('/') && !draft[1..].chars().any(char::is_whitespace);
                    let selected = self.slash_index.min(matches.len().saturating_sub(1));
                    (!matches.is_empty() || waiting).then(|| {
                        div().id("slash-commands").debug_selector(|| "slash-commands".into())
                            .absolute().bottom(gpui::relative(1.)).left_0().right_0()
                            .max_h(px(360.)).overflow_y_scroll().track_scroll(slash_scroll)
                            .flex().flex_col().p(px(6.)).bg(rgb(palette.block)).border_1().border_color(rgb(palette.strong)).rounded(px(6.))
                            .child(div().text_size(px(appearance.size(12.))).text_color(rgb(palette.muted)).child(if waiting {
                                "Commands · listed after the session starts"
                            } else { "Commands · ↑↓ choose · Enter runs · Tab fills in" }))
                            .children(matches.into_iter().enumerate().map(|(index, command)| {
                                let name = command.name.clone();
                                let selector = format!("slash-{name}");
                                Button::new(gpui::SharedString::from(format!("slash-{name}")))
                                    .debug_selector(move || selector.clone())
                                    .px(px(8.)).py(px(5.)).cursor_pointer()
                                    .w_full().h_auto().justify_start()
                                    .bg(rgb(if index == selected { palette.border } else { palette.block }))
                                    .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| { window.prevent_default(); cx.stop_propagation(); })
                                    .on_click(cx.listener(move |view, _, window, cx| view.select_slash(&name, window, cx)))
                                    .child(div().flex().flex_col().items_start()
                                        .child(div().child(format!("/{} {}", command.name, command.argument_hint)))
                                        .child(div().text_size(px(appearance.size(12.))).text_color(rgb(palette.muted)).child(command.description.clone())))
                            }))
                    })
                }).flatten())
                .child(div().flex().items_center().gap(px(6.))
                    .child(Button::new("attach-image").debug_selector(|| "composer-attach".into())
                        .label("Attach image").tooltip("Attach images, paste, or drop them here")
                        .disabled(disabled).on_click(cx.listener(|view, _, window, cx| view.pick_images(window, cx))))
                    .child(div().flex_1())
                    .child(Button::new("send").debug_selector(|| "composer-send".into())
                        .child(div().text_size(px(appearance.size(13.))).child("Send")).px(px(14.)).py(px(5.)).h_auto().bg(rgb(palette.accent)).text_color(rgb(palette.base))
                        .disabled(send_blocked)
                        .on_click(cx.listener(|view, _, window, cx| view.send(window, cx))))
                    .child(Button::new("stop").child(div().text_size(px(appearance.size(13.))).child("Stop")).px(px(14.)).py(px(5.)).h_auto().bg(rgb(palette.err)).text_color(rgb(palette.base))
                        .disabled(disabled || !self.streaming)
                        .on_click(cx.listener(|view, _, _, cx| view.stop(cx)))))
                .child(div().id("composer-chips").flex().flex_wrap().gap(px(4.))
                    .font_family("Menlo").text_size(px(appearance.size(12.))).text_color(rgb(palette.muted))
                    .children(self.chips().into_iter().map(|(id, label)| {
                        let mut chip = Button::new(gpui::SharedString::from(format!("composer-{id}")))
                            .debug_selector(move || format!("composer-{id}"))
                            .child(div().font_family("Menlo").text_size(px(appearance.size(12.))).child(label)).disabled(true).opacity(0.4)
                            .px(px(8.)).py(px(4.)).h_auto().bg(rgb(palette.block))
                            .text_color(rgb(palette.muted)).border_1().border_color(rgb(palette.strong)).rounded(px(6.));
                        if id == "mode" {
                            chip = chip.icon(gpui_component::Icon::default().data(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="4" y="10" width="16" height="11" rx="2"/><path d="M8 10V6a4 4 0 0 1 8 0v4"/></svg>"#).size(px(12.)));
                        }
                        if id == "checkout" && let Some(path) = &self.checkout_path { chip = chip.tooltip(path.clone()); }
                        chip
                    })))
        }
    }
}

#[cfg(test)]
#[path = "composer/tests.rs"]
mod tests;
