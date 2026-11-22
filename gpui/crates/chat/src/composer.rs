//! Disposable composer projection, with acknowledged settings and catalog pickers.
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
}

/// Iced's `.queued-tag` inside a queued bubble: its line and its `×`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTag {
    pub id: String,
    pub label: String,
    /// `Some(enabled)` while the bubble shows the `×`.
    pub withdraw: Option<bool>,
    pub tooltip: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
}
impl SlashCommand {
    /// Iced's row name: `/name` plus the argument hint when there is one.
    pub fn label(&self) -> String {
        if self.argument_hint.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.argument_hint)
        }
    }
    /// Iced's row `title`; a command without a description has none.
    pub fn tooltip(&self) -> Option<&str> {
        Some(self.description.as_str()).filter(|text| !text.is_empty())
    }
}

/// One entry of Iced's checkout menu: a label, its detail line and the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutChoice {
    pub label: String,
    pub detail: String,
    pub path: String,
}
/// Source of the menu entries, asked again each time the menu opens.
#[cfg(target_os = "macos")]
pub type CheckoutSource = std::rc::Rc<dyn Fn(&gpui::App) -> Vec<CheckoutChoice>>;

pub const MODE_TOOLTIP: &str = "Permission mode (Shift+Tab in the message box cycles)";
pub const REVIEW_TOOLTIP: &str =
    "Ask a Claude subagent or Codex to review this checkout, or consult them";
pub const CHECKOUT_LOCKED_REASON: &str = "Set when the chat started; start a new chat to change it";
pub const NOT_IN_PREVIEW: &str = "not available in preview";

#[derive(Debug, Default)]
pub struct ComposerState {
    pub queue: Vec<QueuedRow>,
    /// Queued prompts the harness's exit left unsent (Iced's `dropQueued`).
    exited_unsent: std::collections::HashSet<String>,
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
    pub catalog: crate::pickers::Catalog,
    pub picker: Option<crate::pickers::Picker>,
    pub pending_setting: Option<gitterm_core::chat::types::SettingChange>,
    pub backend: Option<gitterm_core::chat::types::AgentBackend>,
    /// Picks made before the session was ready. They show as the chip value at
    /// once and are sent when it starts, as Iced applies a pre-start choice.
    pub prestart: Vec<gitterm_core::chat::types::SettingChange>,
    /// One picker change is in flight; the next waits for its acknowledgement.
    pub(crate) prestart_sent: bool,
    pub(crate) confirmed: gitterm_core::chat::state::AcknowledgedSettings,
    pub model_tooltip: String,
    pub effort_tooltip: String,
    /// Why the checkout can no longer change; `None` until the chat has started.
    pub checkout_locked: Option<String>,
    pub checkout_choices: Vec<CheckoutChoice>,
    /// Preview shows a pick that Iced would persist as `<label> · not available in preview`.
    pub checkout_notice: Option<String>,
    started_at_ms: Option<u64>,
    ended_at_ms: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<f64>,
    pub(crate) slash_index: usize,
    pub(crate) slash_dismissed: bool,
}
pub const GUIDELINES_TOOLTIP: &str = "GitTerm adds its change guidelines to this agent's system prompt (workspace settings switch them off)";

impl ComposerState {
    /// Iced's `harnessName()`.
    fn harness_name(&self) -> &'static str {
        match self.backend {
            Some(gitterm_core::chat::types::AgentBackend::Claude) => "Claude",
            Some(gitterm_core::chat::types::AgentBackend::Pi) => "the agent",
            Some(gitterm_core::chat::types::AgentBackend::Acp(agent)) => agent.label(),
            None => "the agent",
        }
    }
    pub fn withdraw_tooltip(&self) -> String {
        format!(
            "Withdraw this message ({} keeps working)",
            self.harness_name()
        )
    }

    /// Iced's `queuedText()`.
    fn queued_text(&self) -> String {
        match self.backend {
            Some(gitterm_core::chat::types::AgentBackend::Acp(agent)) => {
                format!("Queued · sent when {} finishes this turn", agent.label())
            }
            _ => "Queued · Claude reads it at its next step".into(),
        }
    }
    /// The tag Iced shows on a bubble while its message is queued; none once
    /// it is delivered or withdrawn. Read-only leaves the `×` inert.
    pub fn queued_tag(&self, id: &str, read_only: bool) -> Option<QueuedTag> {
        let row = self.queue.iter().find(|row| row.id == id)?;
        let (label, withdraw) = match row.status {
            _ if row.withdraw_pending => ("Withdrawing…".into(), Some(false)),
            PromptStatus::Pending | PromptStatus::Queued => {
                (self.queued_text(), Some(!read_only && row.withdrawable()))
            }
            PromptStatus::Unsent if self.exited_unsent.contains(id) => {
                (format!("Not sent: {} exited before reading it", self.harness_name()), None)
            }
            PromptStatus::Unsent => ("Not sent".into(), None),
            PromptStatus::Uncertain => ("Delivery uncertain: the agent may have read this message. Check the conversation before sending it again.".into(), None),
            PromptStatus::Started | PromptStatus::Completed | PromptStatus::Cancelled => return None,
        };
        Some(QueuedTag {
            id: id.into(),
            label,
            withdraw,
            tooltip: self.withdraw_tooltip(),
        })
    }

    /// Iced's `title` for a chip; the checkout one adds its lock reason below the path.
    pub fn chip_tooltip(&self, id: &str, read_only: Option<&str>) -> Option<String> {
        match id {
            "review" => Some(REVIEW_TOOLTIP.into()),
            "model" => Some(self.model_tooltip.clone()),
            "effort" => Some(self.effort_tooltip.clone()),
            "mode" => Some(MODE_TOOLTIP.into()),
            "checkout" => Some(format!(
                "{}\n{}",
                self.checkout_path.as_deref().unwrap_or_default(),
                read_only
                    .map(str::to_owned)
                    .or_else(|| self.checkout_locked.clone())
                    .unwrap_or_else(|| "Where this chat runs".into())
            )),
            _ => None,
        }
    }
    /// Iced sets the Send title only while it can queue a message.
    pub fn send_tooltip(&self, blocked: bool) -> Option<String> {
        (self.streaming && !blocked).then(|| match self.backend {
            Some(gitterm_core::chat::types::AgentBackend::Acp(agent)) => format!(
                "{} is working: the message is queued and sent when this turn ends",
                agent.label()
            ),
            _ => "Claude is working: the message is queued and Claude reads it at its next step"
                .to_string(),
        })
    }
    fn is_acp(&self) -> bool {
        matches!(
            self.backend,
            Some(gitterm_core::chat::types::AgentBackend::Acp(_))
        )
    }
    pub fn project(&mut self, state: &State, effects: Option<&EventEffects>) {
        let was_pending = self.pending_setting.is_some();
        let mut acknowledged = state.acknowledged.clone();
        for change in &self.prestart {
            use gitterm_core::chat::types::SettingChange as C;
            match change {
                C::Model(value) => acknowledged.model = Some(value.clone()),
                C::Mode(value) => acknowledged.mode = Some(value.clone()),
                C::Effort(value) => {
                    acknowledged.effort = Some(gitterm_core::chat::state::ConfirmedEffort {
                        effort: value.clone(),
                        applied: None,
                    })
                }
            }
        }
        self.catalog = crate::pickers::Catalog::project_with(state, &acknowledged, self.is_acp());
        self.pending_setting = state.pending_change().cloned();
        if was_pending && self.pending_setting.is_none() && self.prestart_sent {
            self.prestart_sent = false;
            if !self.prestart.is_empty() {
                self.prestart.remove(0);
            }
        }
        self.confirmed = state.acknowledged.clone();
        if (was_pending && self.pending_setting.is_none())
            || self.picker.is_some_and(|picker| {
                picker != crate::pickers::Picker::Checkout
                    && self.catalog.options(picker).is_empty()
            })
            || effects.is_some_and(|effects| {
                !effects.notices.is_empty() || effects.settings_changed.is_some()
            })
        {
            self.picker = None;
        }
        self.pending_request = state.has_pending_requests();
        self.streaming = matches!(state.state, AgentSessionState::Streaming);
        let was_streaming = self.started_at_ms.is_some() && self.ended_at_ms.is_none();
        let mut reset = state.conversation.is_empty();
        let mut activity = "Working".to_string();
        let mut started = None;
        let mut ended = None;
        let mut completed = None;
        self.output_tokens = None;
        self.cost_usd = None;
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
                    activity = "Working".into();
                    completed = None;
                    self.output_tokens = None;
                    self.cost_usd = None;
                }
                AgentEvent::Harness(HarnessEvent::TurnCompleted {
                    status,
                    usage,
                    cost_usd,
                    ..
                }) => {
                    completed = Some(match status {
                        gitterm_core::chat::types::TurnStatus::Completed => "Done",
                        gitterm_core::chat::types::TurnStatus::Interrupted => "Interrupted",
                        gitterm_core::chat::types::TurnStatus::Failed(_) => "Failed",
                    });
                    self.output_tokens = usage["output_tokens"].as_u64();
                    self.cost_usd = *cost_usd;
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
                    completed = None;
                    self.output_tokens = None;
                    self.cost_usd = None;
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
                AgentSessionState::Idle => completed
                    .unwrap_or(if self.started_at_ms.is_some() {
                        "Done"
                    } else {
                        "Ready"
                    })
                    .into(),
                AgentSessionState::Streaming => activity,
                AgentSessionState::Stopped => completed.unwrap_or("Stopped").into(),
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
        let confirmed_model = acknowledged.model.as_deref();
        let base_model = confirmed_model.map(|value| {
            if self.is_acp() {
                value
            } else {
                value.strip_suffix("[1m]").unwrap_or(value)
            }
        });
        let model_label = |value: &str| -> String {
            {
                let info =
                    ready.and_then(|(models, _)| models.iter().find(|info| info["value"] == value));
                if self.is_acp() && value == "default" {
                    if let Some(gitterm_core::chat::types::AgentBackend::Acp(agent)) = self.backend
                    {
                        format!("{} default", agent.label())
                    } else {
                        value.into()
                    }
                } else if value == "default" {
                    info.and_then(|info| info["description"].as_str())
                        .map(|description| {
                            let end = [" · ", " — ", " - "]
                                .iter()
                                .filter_map(|separator| description.find(separator))
                                .min()
                                .unwrap_or(description.len());
                            &description[..end]
                        })
                        .map(str::trim)
                        .filter(|description| !description.is_empty())
                        .unwrap_or("Claude settings")
                        .into()
                } else {
                    info.and_then(|info| info["displayName"].as_str())
                        .unwrap_or(value)
                        .into()
                }
            }
        };
        // Iced's chips read `current → requested…` while one change is pending.
        use gitterm_core::chat::types::SettingChange as Change;
        let pending = state.pending_change();
        let pending_model = match pending {
            Some(Change::Model(value)) => Some(model_label(if self.is_acp() {
                value
            } else {
                value.strip_suffix("[1m]").unwrap_or(value)
            })),
            _ => None,
        };
        self.model = base_model.map_or_else(|| "Model".into(), model_label);
        if let Some(requested) = pending_model {
            self.model = format!("{} → {requested}…", self.model);
        }
        let mode_label = |value: &str| -> String {
            {
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
            }
        };
        self.mode = acknowledged
            .mode
            .as_deref()
            .map_or_else(|| "Mode".into(), mode_label);
        if let Some(Change::Mode(requested)) = pending {
            self.mode = format!("{} → {}…", self.mode, mode_label(requested));
        }
        self.effort = acknowledged.effort.as_ref().map_or_else(
            || "Effort".into(),
            |effort| {
                let mut label = crate::pickers::effort_name(effort.effort.as_deref()).to_string();
                let supported = self
                    .catalog
                    .efforts
                    .iter()
                    .any(|row| matches!(row.change, Change::Effort(_)));
                if supported && let Some(Change::Effort(requested)) = pending {
                    label.push_str(&format!(
                        " → {}…",
                        crate::pickers::effort_name(requested.as_deref())
                    ));
                }
                let context = if confirmed_model.is_some_and(|value| value.ends_with("[1m]")) {
                    "1M"
                } else {
                    "200k"
                };
                if self.is_acp() {
                    label
                } else if supported {
                    format!("{label} · {context}")
                } else {
                    context.into()
                }
            },
        );
        // The reducer owns the acknowledged path; the host's current-checkout
        // label is stable even when its absolute path changes.
        self.checkout = "Current checkout".into();
        self.checkout_locked = (state.session_id.is_some()
            || state.conversation.iter().any(|event| {
                matches!(event, AgentEvent::Harness(HarnessEvent::TurnStarted { .. }))
                    || matches!(event, AgentEvent::Other(value) if value["type"] == "user_prompt")
            }))
        .then(|| CHECKOUT_LOCKED_REASON.to_string());
        self.model_tooltip = format!("Model: {}", confirmed_model.unwrap_or_default());
        let effort_supported = self.catalog.efforts.iter().any(|row| {
            matches!(
                row.change,
                gitterm_core::chat::types::SettingChange::Effort(_)
            )
        });
        self.effort_tooltip = {
            let mut text = if !effort_supported {
                "This model has no effort setting".to_string()
            } else {
                let mut text = "Effort".to_string();
                if let Some(effort) = &acknowledged.effort
                    && effort.effort.is_none()
                    && let Some(applied) = &effort.applied
                {
                    text.push_str(&format!(" (auto runs at {applied})"));
                }
                text
            };
            if !self.is_acp() {
                let context = if confirmed_model.is_some_and(|value| value.ends_with("[1m]")) {
                    "1M"
                } else {
                    "200k"
                };
                text.push_str(&format!(" · context window {context}"));
            }
            text
        };
        self.checkout_path = state
            .checkout
            .as_ref()
            .map(|path| path.display().to_string());
        // Core marks the exit's queue Unsent; a prompt refused earlier keeps plain "Not sent".
        let waiting: Vec<_> = self
            .queue
            .iter()
            .filter(|row| matches!(row.status, PromptStatus::Pending | PromptStatus::Queued))
            .map(|row| row.id.clone())
            .collect();
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
        if effects.is_some_and(|effects| effects.process_exited) {
            self.exited_unsent.extend(waiting.into_iter().filter(|id| {
                self.queue
                    .iter()
                    .any(|row| &row.id == id && row.status == PromptStatus::Unsent)
            }));
        }
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

    /// The same chip sequence as agent_chat.html.
    pub fn chips(&self) -> [(&'static str, String); 6] {
        [
            ("review", "Review / Consult…".into()),
            ("model", self.model.clone()),
            ("effort", self.effort.clone()),
            ("mode", self.mode.clone()),
            (
                "checkout",
                self.checkout_notice
                    .clone()
                    .unwrap_or_else(|| self.checkout.clone()),
            ),
            ("guidelines", "guidelines".into()),
        ]
    }

    pub fn status_at(&self, now: u64) -> String {
        let mut status = self.status.clone();
        let mut meta = Vec::new();
        if let Some(start) = self.started_at_ms {
            meta.push(elapsed(
                self.ended_at_ms.unwrap_or(now).saturating_sub(start),
            ));
        }
        if let Some(tokens) = self.output_tokens.filter(|tokens| *tokens > 0) {
            meta.push(format!("↓ {tokens} tokens"));
        }
        if let Some(cost) = self.cost_usd {
            meta.push(format!("${cost:.4} session"));
        }
        if !meta.is_empty() {
            status.push(' ');
            status.push_str(&meta.join(" · "));
        }
        status
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
        StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, rgb,
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
            review: &crate::view::review_ui::ReviewComposer,
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
                .on_click(cx.listener(|view, _, window, cx| { if view.review.open { view.close_review(window, cx); } view.close_picker(cx); view.reopen_slash(cx); cx.stop_propagation(); }))
                .on_drop(cx.listener(|view, paths: &gpui::ExternalPaths, window, cx| {
                    view.attach_paths(paths.0.to_vec(), cx);
                    view.focus_composer(window, cx);
                }))
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
                                let row = Button::new(gpui::SharedString::from(format!("slash-{name}")))
                                    .debug_selector(move || selector.clone());
                                let row = match command.tooltip() {
                                    Some(tip) => row.tooltip(tip.to_string()),
                                    None => row,
                                };
                                row
                                    .px(px(8.)).py(px(5.)).cursor_pointer()
                                    .w_full().h_auto().justify_start()
                                    .bg(rgb(if index == selected { palette.border } else { palette.block }))
                                    .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| { window.prevent_default(); cx.stop_propagation(); })
                                    .on_click(cx.listener(move |view, _, window, cx| view.select_slash(&name, window, cx)))
                                    .child(div().flex().flex_col().items_start()
                                        .child(div().child(command.label()))
                                        .child(div().text_size(px(appearance.size(12.))).text_color(rgb(palette.muted)).child(command.description.clone())))
                            }))
                    })
                }).flatten())
                .child(div().flex().items_center().gap(px(6.))
                    .child(div().flex_1())
                    .child({ let send = Button::new("send").debug_selector(|| "composer-send".into())
                        .child(div().text_size(px(appearance.size(13.))).child("Send")).px(px(14.)).py(px(5.)).h_auto().bg(rgb(palette.accent)).text_color(rgb(palette.base))
                        .disabled(send_blocked)
                        // The instance `bg` outlives the disabled style, so a
                        // read-only chat dims Send and Stop as Iced's `button:disabled` does.
                        .when(disabled, |send| send.opacity(0.4))
                        .on_click(cx.listener(|view, _, window, cx| view.send(window, cx)));
                        match self.send_tooltip(disabled || self.pending_request) { Some(tip) => send.tooltip(tip), None => send } })
                    .child(Button::new("stop").debug_selector(|| "composer-stop".into()).child(div().text_size(px(appearance.size(13.))).child("Stop")).px(px(14.)).py(px(5.)).h_auto().bg(rgb(palette.err)).text_color(rgb(palette.base))
                        .disabled(disabled || !self.streaming)
                        .when(disabled, |stop| stop.opacity(0.4))
                        .on_click(cx.listener(|view, _, _, cx| view.stop(cx)))))
                .child(div().id("composer-chips").relative().flex().flex_wrap().gap(px(4.))
                    .font_family("Menlo").text_size(px(appearance.size(12.))).text_color(rgb(palette.muted))
                    .children(self.chips().into_iter().map(|(id, label)| {
                        if id == "guidelines" {
                            return div().id("composer-guidelines").debug_selector(|| "composer-guidelines".into())
                                .px(px(8.)).py(px(4.)).cursor_default().child(label)
                                .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new(GUIDELINES_TOOLTIP).build(window, cx))
                                .into_any_element();
                        }
                        let mut chip = Button::new(gpui::SharedString::from(format!("composer-{id}")))
                            .debug_selector(move || format!("composer-{id}"))
                            .child(div().font_family("Menlo").text_size(px(appearance.size(12.))).child(label))
                            .disabled(match id {
                                "review" => !matches!(self.backend, Some(gitterm_core::chat::types::AgentBackend::Claude)),
                                "checkout" => disabled,
                                _ => disabled || crate::pickers::Picker::from_chip(id).is_none_or(|picker| self.catalog.options(picker).is_empty()),
                            })
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if id == "review" { view.toggle_review(window, cx); }
                                else if let Some(picker) = crate::pickers::Picker::from_chip(id) { view.toggle_picker(picker, window, cx); }
                                cx.stop_propagation();
                            }))
                            .px(px(8.)).py(px(4.)).h_auto().bg(rgb(palette.block))
                            .text_color(rgb(palette.muted)).border_1().border_color(rgb(palette.strong)).rounded(px(6.));
                        let icon = match id {
                            "model" => Some(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="currentColor"><path d="M8 0.8l1.7 4.6 4.7 1.6-4.7 1.7L8 13.4 6.3 8.7 1.6 7l4.7-1.6z"/></svg>"#.as_slice()),
                            "effort" => Some(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M2.5 11.5a5.5 5.5 0 1 1 11 0"/><path d="M8 11.5l2.6-3.4"/></svg>"#.as_slice()),
                            "mode" => Some(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="3" y="7" width="10" height="7.5" rx="1.5"/><path d="M5.2 7V5a2.8 2.8 0 0 1 5.6 0v2"/></svg>"#.as_slice()),
                            _ => None,
                        };
                        if let Some(icon) = icon {
                            chip = chip.icon(gpui_component::Icon::default().data(icon).size(px(12.)));
                        }
                        if let Some(tip) = self.chip_tooltip(id, unavailable.as_deref()) { chip = chip.tooltip(tip); }
                        chip.into_any_element()
                    }))
                    .children(review.open.then(|| review.render(self.streaming || self.pending_request, appearance, cx))))
                .children(self.picker.filter(|picker| !disabled && *picker == crate::pickers::Picker::Checkout).map(|_| {
                    div().id("checkout-picker").debug_selector(|| "checkout-picker".into())
                        .absolute().bottom(gpui::relative(1.)).left_0().w(px(380.)).max_h(px(360.)).overflow_y_scroll()
                        .flex().flex_col().p(px(6.)).bg(rgb(palette.block)).border_1().border_color(rgb(palette.strong)).rounded(px(6.))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .children(self.checkout_locked.is_none().then(|| div().text_color(rgb(palette.muted)).child("Run this chat in")))
                        .children(self.checkout_choices.iter().filter(|_| self.checkout_locked.is_none()).enumerate().map(|(index, choice)| {
                            let (path, label) = (choice.path.clone(), choice.label.clone());
                            let current = self.checkout_path.as_deref() == Some(choice.path.as_str());
                            Button::new(gpui::SharedString::from(format!("checkout-option-{index}")))
                                .debug_selector(move || format!("checkout-option-{index}"))
                                .w_full().h_auto().justify_start()
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.choose_checkout(&path, &label, window, cx);
                                    cx.stop_propagation();
                                }))
                                .child(div().flex().flex_col().items_start()
                                    .child(format!("{} {}", if current { "✓" } else { " " }, choice.label))
                                    .children((!choice.detail.is_empty()).then(|| div().text_color(rgb(palette.muted)).child(choice.detail.clone()))))
                        }))
                        .child(Button::new("checkout-open-editor")
                            .debug_selector(|| "checkout-open-editor".into())
                            .w_full().h_auto().justify_start()
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.open_checkout_in_editor(window, cx);
                                cx.stop_propagation();
                            }))
                            .child(div().flex().flex_col().items_start()
                                .child("  Open in editor")
                                .child(div().text_color(rgb(palette.muted)).child("Open this checkout folder"))))
                }))
                .children(self.picker.filter(|picker| !disabled && *picker != crate::pickers::Picker::Checkout).map(|picker| {
                    let options = self.catalog.options(picker);
                    div().id("setting-picker").debug_selector(|| "setting-picker".into())
                        .absolute().bottom(gpui::relative(1.)).left_0().w(px(380.)).max_h(px(360.)).overflow_y_scroll()
                        .flex().flex_col().p(px(6.)).bg(rgb(palette.block)).border_1().border_color(rgb(palette.strong)).rounded(px(6.))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .children(options.iter().enumerate().map(|(index, row)| {
                            let change = row.change.clone();
                            div().flex().flex_col()
                                .children((index == 0 || options[index - 1].section != row.section).then(|| div().text_color(rgb(palette.muted)).child(row.section)))
                                .child(Button::new(gpui::SharedString::from(format!("setting-option-{index}")))
                                    .debug_selector(move || format!("setting-option-{index}"))
                                    .w_full().h_auto().justify_start().disabled(row.disabled)
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.choose_setting(change.clone(), window, cx);
                                        cx.stop_propagation();
                                    }))
                                    .child(div().flex().flex_col().items_start()
                                        .child(format!("{} {}", if row.selected { "✓" } else if self.pending_setting.as_ref() == Some(&row.change) { "…" } else { " " }, row.label))
                                        .children((!row.description.is_empty()).then(|| div().text_color(rgb(palette.muted)).child(row.description.clone())))))
                        }))
                }))
        }
    }
}

#[cfg(test)]
#[path = "composer/tests.rs"]
mod tests;
