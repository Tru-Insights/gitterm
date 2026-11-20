//! Disposable composer projection. Settings are acknowledged display values only;
//! no picker or checkout command is constructed here.
use gitterm_core::chat::{
    state::{AgentEvent, EventEffects, PromptStatus, State},
    types::{AgentSessionState, HarnessEvent},
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
    pub commands: Vec<SlashCommand>,
    pub notice: Option<String>,
    pub(crate) slash_index: usize,
    pub(crate) slash_dismissed: bool,
}
impl ComposerState {
    pub fn project(&mut self, state: &State, effects: Option<&EventEffects>) {
        self.pending_request = !state.pending_requests.is_empty();
        self.streaming = matches!(state.state, AgentSessionState::Streaming);
        self.status = if self.pending_request {
            "Needs you · answer the pending request before sending".into()
        } else {
            match &state.state {
                AgentSessionState::Idle => "Ready".into(),
                AgentSessionState::Streaming => "Running · new prompts queue".into(),
                AgentSessionState::Stopped => "Stopped".into(),
                AgentSessionState::Errored(error) => format!("Failed · {error}"),
            }
        };
        self.model = state
            .acknowledged
            .model
            .clone()
            .unwrap_or_else(|| "Not reported".into());
        self.mode = state
            .acknowledged
            .mode
            .clone()
            .unwrap_or_else(|| "Not reported".into());
        self.effort = state.acknowledged.effort.as_ref().map_or_else(
            || "Not reported".into(),
            |effort| {
                format!(
                    "{} · applied: {}",
                    effort.effort.as_deref().unwrap_or("Auto"),
                    effort.applied.as_deref().unwrap_or("not reported")
                )
            },
        );
        self.checkout = state
            .checkout
            .as_ref()
            .map_or_else(|| "Host-owned".into(), |path| path.display().to_string());
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
                .child(div().text_color(rgb(0x7f849c)).child(self.status.clone()))
                .child(div().flex().flex_wrap().gap(px(6.))
                    .child(Button::new("composer-model").label(format!("Model: {} · disabled", self.model)).disabled(true))
                    .child(Button::new("composer-effort").label(format!("Effort: {} · disabled", self.effort)).disabled(true))
                    .child(Button::new("composer-mode").label(format!("Mode: {} · disabled", self.mode)).disabled(true))
                    .child(Button::new("composer-checkout").label(format!("Checkout: {} · locked", self.checkout)).disabled(true)))
                .child(div().text_size(px(12.)).child("Settings unavailable in this preview · checkout locked · Shift+Tab mode cycling disabled"))
                .child(div().id("composer-queue").max_h(px(160.)).overflow_y_scroll().children(self.queue.iter().map(|row| {
                    let id = row.id.clone();
                    div().flex().gap(px(8.)).child(div().flex_1().child(format!("{} · {} · {}", row.id, row.label(), row.text)))
                        .child(Button::new(gpui::SharedString::from(format!("withdraw-{}", row.id)))
                            .label("Withdraw").disabled(disabled || !row.withdrawable())
                            .on_click(cx.listener(move |view, _, _, cx| view.withdraw(id.clone(), cx))))
                })))
                .children(self.notice.iter().map(|notice| div().child(notice.clone())))
                .child(Textarea::new(input).disabled(disabled))
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
                .child(div().flex().gap(px(8.))
                    .child(Button::new("send").debug_selector(|| "composer-send".into())
                        .label(if self.streaming { "Queue" } else { "Send" })
                        .disabled(send_blocked)
                        .on_click(cx.listener(|view, _, window, cx| view.send(window, cx))))
                    .child(Button::new("stop").label("Stop").disabled(disabled || !self.streaming)
                        .on_click(cx.listener(|view, _, _, cx| view.stop(cx))))
                    .child(unavailable.unwrap_or_else(|| if self.pending_request {
                        "Answer the pending request before sending".into()
                    } else { "Enter sends · Shift+Enter adds a newline".into() })))
        }
    }
}

#[cfg(test)]
#[path = "composer/tests.rs"]
mod tests;
