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

#[derive(Debug, Clone)]
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
        self.commands
            .iter()
            .filter(|command| command.name.starts_with(prefix))
            .collect()
    }
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
        Context, Entity, InteractiveElement, IntoElement, ParentElement,
        StatefulInteractiveElement, Styled, div, px, rgb,
    };
    use gpui_component::{
        Disableable,
        button::Button,
        input::{Textarea, TextareaState},
    };

    impl ComposerState {
        pub(crate) fn render_activity(&self) -> impl IntoElement {
            let color = if self.pending_request {
                0xf9e2af
            } else if self.streaming {
                0x89b4fa
            } else {
                0x94e2d5
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
                .text_size(px(15. * 12. / 14.))
                .text_color(rgb(0x7f849c))
                .child(div().size(px(7.)).rounded_full().bg(rgb(color)))
                .child(self.status_at(now_ms()))
        }

        pub(crate) fn render(
            &self,
            input: &Entity<TextareaState>,
            unavailable: Option<String>,
            send_blocked: bool,
            cx: &mut Context<ChatView>,
        ) -> impl IntoElement {
            let disabled = unavailable.is_some();
            let draft = input.read(cx).value();
            div().flex().flex_col().gap(px(6.))
                .child(div().id("composer-queue").max_h(px(160.)).overflow_y_scroll().children(self.queue.iter().map(|row| {
                    let id = row.id.clone();
                    div().flex().gap(px(8.)).child(div().flex_1().child(format!("{} · {} · {}", row.id, row.label(), row.text)))
                        .child(Button::new(gpui::SharedString::from(format!("withdraw-{}", row.id)))
                            .label("Withdraw").disabled(disabled || !row.withdrawable())
                            .on_click(cx.listener(move |view, _, _, cx| view.withdraw(id.clone(), cx))))
                })))
                .children(self.notice.iter().map(|notice| div().text_color(rgb(0xf38ba8)).child(notice.clone())))
                .child(Textarea::new(input).disabled(disabled).bg(rgb(0x181825))
                    .text_size(px(15. * 13.5 / 14.)).border_color(rgb(0x585b70)).rounded(px(6.)))
                .child(div().id("slash-commands").max_h(px(100.)).overflow_y_scroll().children(self.slash_matches(&draft).into_iter().enumerate().map(|(index, command)| {
                    let name = command.name.clone();
                    let arguments = !command.argument_hint.is_empty();
                    Button::new(gpui::SharedString::from(format!("slash-{name}")))
                        .label(format!("{} /{} {} · {}", if index == self.slash_index { "›" } else { "" }, name, command.argument_hint, command.description))
                        .disabled(disabled)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.select_slash(&name, window, cx);
                            if !arguments { view.send(window, cx); }
                        }))
                })))
                .child(div().flex().justify_end().gap(px(6.))
                    .child(Button::new("send").debug_selector(|| "composer-send".into())
                        .child(div().text_size(px(15. * 13. / 14.)).child("Send")).px(px(14.)).py(px(5.)).h_auto().bg(rgb(0x89b4fa)).text_color(rgb(0x1e1e2e))
                        .disabled(send_blocked)
                        .on_click(cx.listener(|view, _, window, cx| view.send(window, cx))))
                    .child(Button::new("stop").child(div().text_size(px(15. * 13. / 14.)).child("Stop")).px(px(14.)).py(px(5.)).h_auto().bg(rgb(0xf38ba8)).text_color(rgb(0x1e1e2e))
                        .disabled(disabled || !self.streaming)
                        .on_click(cx.listener(|view, _, _, cx| view.stop(cx)))))
                .child(div().id("composer-chips").flex().flex_wrap().gap(px(4.))
                    .font_family("Menlo").text_size(px(15. * 12. / 14.)).text_color(rgb(0x7f849c))
                    .children(self.chips().into_iter().map(|(id, label)| {
                        let mut chip = Button::new(gpui::SharedString::from(format!("composer-{id}")))
                            .debug_selector(move || format!("composer-{id}"))
                            .child(div().font_family("Menlo").text_size(px(15. * 12. / 14.)).child(label)).disabled(true).opacity(0.4)
                            .px(px(8.)).py(px(4.)).h_auto().bg(rgb(0x313244))
                            .text_color(rgb(0x7f849c)).border_1().border_color(rgb(0x585b70)).rounded(px(6.));
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
