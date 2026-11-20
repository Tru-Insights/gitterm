//! Authoritative, frontend-independent chat transitions and timeline.
//!
//! The host owns handles and the session catalog. It must validate the complete
//! ingress with `accepts_stream`/`accepts_history` before applying host effects;
//! deferred commands additionally require the full `accepts_runtime` binding.
//! Channel acceptance is not a harness write receipt. No types here are persisted.
use super::{history, types::*};
use crate::{harness::transcript::TranscriptEntry, session};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::BTreeMap, path::PathBuf};

// Process-wide incarnations never repeat when a tab replaces its chat state.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
fn generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

fn same_chat(a: &session::Target, b: &session::Target) -> bool {
    a.tab_id == b.tab_id && a.session_uid == b.session_uid
}

/// Parsed stream-json events from the agent subprocess. The variant taxonomy is
/// the union of pi and Claude Code event shapes; the parser (Step 3) decides
/// which variant each line maps to. Backend-specific details that don't fit the
/// shared shape are kept on the raw JSON via `Other`.
///
/// pi uses `toolcall_start/delta/end` (not `tool_use_*`), `_end` not `_stop`
/// suffixes, and tool results arrive as `role: "toolResult"`. Claude uses
/// content-block-style events embedded in `assistant` messages. The parser
/// normalizes both into these variants.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    SystemInit(serde_json::Value),
    AssistantText(String),
    AssistantThinking(String),
    ToolCallStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCallDelta {
        id: String,
        partial: String,
    },
    ToolCallEnd {
        id: String,
    },
    ToolResult {
        tool_use_id: String,
        output: String,
        is_error: bool,
    },
    Result(serde_json::Value),
    /// Backend-specific event that doesn't fit the normalized variants above.
    /// Kept as raw JSON so the UI layer can decide whether to render it.
    Other(serde_json::Value),
    /// Normalized event from a native harness session (Claude, TRU-140).
    Harness(HarnessEvent),
}

impl AgentEvent {
    /// The echo of a prompt the human submitted, as the chat page renders it.
    pub fn user_prompt(text: &str) -> Self {
        Self::Other(history::user_prompt_payload(text))
    }

    /// The echo of a submitted prompt. Attached images ride along as
    /// `images: [{media_type, data}]` so the page can show thumbnails in
    /// the human's own message. A prompt with an id carries it as `id`, and
    /// one sent while a turn runs is marked `queued: true`: the page shows
    /// it as queued, with a withdraw button, until its `message_lifecycle`
    /// says Claude picked it up. A plain text prompt is the plain echo.
    pub fn submitted_prompt(prompt: &UserPrompt, queued: bool) -> Self {
        if prompt.images.is_empty() && prompt.id.is_none() && !queued {
            return Self::user_prompt(&prompt.text);
        }
        let mut echo = history::user_prompt_payload(&prompt.text);
        if !prompt.images.is_empty() {
            echo["images"] = serde_json::json!(prompt.images);
        }
        if let Some(id) = &prompt.id {
            echo["id"] = serde_json::json!(id);
        }
        if queued {
            echo["queued"] = serde_json::json!(true);
        }
        Self::Other(echo)
    }

    /// The echo of a prompt another client of an agent-owned chat sent (a
    /// phone, TRU-151), as the page's own `user_prompt`: `id` lets the
    /// page and the tab tell it from their own echoes, `queued` marks it
    /// sent while a turn runs, and `image_count` says how many images it
    /// carried (their bytes are never relayed).
    pub fn relayed_prompt(id: Option<&str>, text: &str, image_count: usize, queued: bool) -> Self {
        let mut echo = history::user_prompt_payload(text);
        if let Some(id) = id {
            echo["id"] = serde_json::json!(id);
            if queued {
                echo["queued"] = serde_json::json!(true);
            }
        }
        if image_count > 0 {
            echo["image_count"] = serde_json::json!(image_count);
        }
        Self::Other(echo)
    }

    /// The id of a `user_prompt` echo that carries one.
    pub fn prompt_id(&self) -> Option<&str> {
        match self {
            Self::Other(value)
                if value.get("type").and_then(|v| v.as_str()) == Some("user_prompt") =>
            {
                value.get("id").and_then(|v| v.as_str())
            }
            _ => None,
        }
    }

    /// The JSON the chat webview's `__appendEvent` receives. Harness events
    /// are wrapped so the page can tell them from pi's raw stream shapes.
    pub fn webview_payload(&self) -> Option<serde_json::Value> {
        match self {
            Self::Other(value) => Some(value.clone()),
            Self::Harness(ev) => history::harness_payload(ev),
            // The typed pi variants are not produced yet.
            _ => None,
        }
    }
}

impl From<TranscriptEntry> for AgentEvent {
    fn from(entry: TranscriptEntry) -> Self {
        match entry {
            TranscriptEntry::UserPrompt(text) => Self::user_prompt(&text),
            TranscriptEntry::Harness(ev) => Self::Harness(ev),
        }
    }
}

fn event_subagent_started(ev: &AgentEvent) -> Option<&str> {
    match ev {
        AgentEvent::Harness(ev) => history::subagent_started(ev),
        _ => None,
    }
}

/// Capture at receiver installation or answer/stop admission, never by pane focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStamp {
    pub target: session::Target,
    generation: u64,
}

/// A source-bound read. Claude file/remote reads survive process installation;
/// ACP replay is additionally bound to the receiver which supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTicket {
    pub target: session::Target,
    generation: u64,
    backend: AgentBackend,
    session_id: String,
    runtime: Option<u64>,
}

/// Last values confirmed by the harness, never optimistic picker selections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcknowledgedSettings {
    pub model: Option<String>,
    pub mode: Option<String>,
    /// None means not yet reported; Some with effort=None means model default.
    pub effort: Option<ConfirmedEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedEffort {
    pub effort: Option<String>,
    pub applied: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingChange {
    Model(String),
    Effort(Option<String>),
    Mode(String),
}

impl SettingChange {
    fn command(&self) -> HarnessCommand {
        match self {
            Self::Model(value) => HarnessCommand::SetModel(value.clone()),
            Self::Effort(value) => HarnessCommand::SetEffort(value.clone()),
            Self::Mode(value) => HarnessCommand::SetPermissionMode(value.clone()),
        }
    }

    fn valid(&self, backend: AgentBackend) -> bool {
        let token = |s: &str| {
            !s.is_empty()
                && s.len() <= 256
                && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        match self {
            Self::Model(value) => token(value),
            Self::Effort(value) => value.as_deref().is_none_or(|v| {
                if backend == AgentBackend::Claude {
                    ["low", "medium", "high", "xhigh", "max"].contains(&v)
                } else {
                    token(v)
                }
            }),
            Self::Mode(value) if backend == AgentBackend::Claude => [
                "default",
                "acceptEdits",
                "plan",
                "auto",
                "dontAsk",
                "bypassPermissions",
            ]
            .contains(&value.as_str()),
            Self::Mode(value) => token(value),
        }
    }
}

/// View state only. The harness owns the executable queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStatus {
    Pending,
    Queued,
    Started,
    Completed,
    Cancelled,
    Unsent,
    Uncertain,
}

#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub prompt: UserPrompt,
    pub status: PromptStatus,
    pub withdraw_pending: bool,
    /// A write alone cannot establish whether the provider started the prompt.
    written: bool,
}

#[derive(Debug)]
enum PendingCommand {
    Queue(String),
    Withdraw(String),
    Setting,
}

/// Host-owned checkout authority captured at dispatch, not supplied by a view.
/// Choices must already be normalized workspace/prepared-worktree paths.
#[derive(Debug)]
pub struct CheckoutContext {
    pub choices: Vec<PathBuf>,
    pub locked_reason: Option<String>,
    pub process_running: bool,
}

#[derive(Debug)]
pub struct State {
    pub conversation: Vec<AgentEvent>,
    pub session_id: Option<String>,
    pub state: AgentSessionState,
    pub pending_requests: Vec<String>,
    pub history: HistoryLoad,
    /// Record counter, including merged fragments. History replacement/reset
    /// uses explicit effects; retain Iced's existing revision semantics.
    pub revision: u64,
    pub acknowledged: AcknowledgedSettings,
    pub queued_prompts: BTreeMap<String, QueuedPrompt>,
    pub checkout: Option<PathBuf>,
    pending_change: Option<(CommandCorrelation, SettingChange)>,
    commands: BTreeMap<u64, PendingCommand>,
    runtime_exited: bool,
    runtime_generation: u64,
    history_generation: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            conversation: Vec::new(),
            session_id: None,
            state: AgentSessionState::Idle,
            pending_requests: Vec::new(),
            history: HistoryLoad::NotLoaded,
            revision: 0,
            acknowledged: AcknowledgedSettings::default(),
            queued_prompts: BTreeMap::new(),
            checkout: None,
            pending_change: None,
            commands: BTreeMap::new(),
            runtime_exited: false,
            runtime_generation: generation(),
            history_generation: generation(),
        }
    }
}

#[derive(Debug)]
pub enum Intent {
    Queue {
        runtime: RuntimeStamp,
        current: session::Target,
        config: AgentBackendConfig,
        process_running: bool,
        prompt: UserPrompt,
        message_id: String,
    },
    Withdraw {
        runtime: RuntimeStamp,
        current: session::Target,
        process_running: bool,
        message_id: String,
    },
    ChangeSetting {
        runtime: RuntimeStamp,
        current: session::Target,
        config: AgentBackendConfig,
        process_running: bool,
        change: SettingChange,
    },
    /// Channel send failures must also return here as Rejected. Written is
    /// a transport receipt only; it never acknowledges a picker or withdraw.
    CommandOutcome {
        runtime: RuntimeStamp,
        current: session::Target,
        correlation: CommandCorrelation,
        outcome: DeliveryOutcome,
    },
    ChooseCheckout {
        runtime: RuntimeStamp,
        current: session::Target,
        context: CheckoutContext,
        path: PathBuf,
    },
    Submit {
        prompt: UserPrompt,
        backend: AgentBackend,
        process_running: bool,
        just_spawned: bool,
        message_id: String,
    },
    /// Only reports acceptance into the command channel, never actual delivery.
    SubmissionAccepted,
    RelayedPrompt {
        id: Option<String>,
        text: String,
        image_count: usize,
        backend: AgentBackend,
        process_running: bool,
    },
    Event(AgentEvent),
    Record {
        event: AgentEvent,
        now_ms: Option<i64>,
    },
    BindSession(Option<String>),
    BeginHistory(AgentBackendConfig),
    HistoryLoaded(Vec<AgentEvent>),
    HistoryUnavailable(String),
    ResetHistory,
    FinishHistory,
    Answer {
        runtime: RuntimeStamp,
        current: session::Target,
        request_id: String,
        decision: RuntimeDecision,
        process_running: bool,
    },
    Stop {
        runtime: RuntimeStamp,
        authority: session::State,
        target: session::Target,
    },
}

#[derive(Debug)]
pub struct Submission {
    pub prompt: UserPrompt,
    pub echo: AgentEvent,
    pub images_not_sent: usize,
}

#[derive(Debug, Default)]
pub struct EventEffects {
    pub session_id: Option<String>,
    pub awaiting_input: Option<bool>,
    pub outcome: Option<TurnStatus>,
    pub process_exited: bool,
    /// Host may persist only these acknowledged changes, once per value.
    pub settings_changed: Option<SettingChange>,
    pub unsent_prompts: Vec<UserPrompt>,
    pub notices: Vec<String>,
}

#[derive(Debug)]
pub enum Effect {
    /// Send once, in session order, then feed the correlated delivery outcome
    /// back. Revalidate the full runtime binding immediately before sending.
    Dispatch {
        runtime: RuntimeStamp,
        correlation: CommandCorrelation,
        command: HarnessCommand,
        echo: Option<AgentEvent>,
    },
    CheckoutBound {
        runtime: RuntimeStamp,
        path: PathBuf,
    },
    Submit(Submission),
    RelayedPrompt {
        echo: AgentEvent,
        running: bool,
    },
    /// Execute synchronously against this stamp, revalidating before send.
    Send {
        runtime: RuntimeStamp,
        command: HarnessCommand,
    },
    Event(EventEffects),
    Recorded {
        before: Vec<AgentEvent>,
        after: Vec<AgentEvent>,
    },
    LoadHistory(String),
    ReplayHistory,
    CancelLegacyTurn,
    Rejected(String),
    None,
}

impl State {
    pub fn shows_prompt(&self, id: &str) -> bool {
        self.conversation
            .iter()
            .rev()
            .any(|ev| ev.prompt_id() == Some(id))
    }

    pub fn runtime_stamp(&self, target: session::Target) -> RuntimeStamp {
        RuntimeStamp {
            target,
            generation: self.runtime_generation,
        }
    }

    /// Call exactly once when installing a new sole receiver (including Pi).
    pub fn begin_runtime(&mut self) {
        self.runtime_generation = generation();
        self.runtime_exited = false;
        self.pending_change = None;
        self.commands.clear();
        self.acknowledged = AcknowledgedSettings::default();
        self.finish_queue();
        self.pending_requests.clear();
    }

    /// Task attachment/resume changes attribution without replacing the chat's
    /// receiver. Stream ownership follows the stable caller and incarnation;
    /// host effects use task attribution from the current catalog entry.
    pub fn accepts_stream(&self, stamp: &RuntimeStamp, current: &session::Target) -> bool {
        same_chat(&stamp.target, current) && stamp.generation == self.runtime_generation
    }

    /// Commands admitted before a task rebind must not act under its new binding.
    pub fn accepts_runtime(&self, stamp: &RuntimeStamp, current: &session::Target) -> bool {
        stamp.target == *current && stamp.generation == self.runtime_generation
    }

    pub fn history_ticket(
        &self,
        target: session::Target,
        backend: AgentBackend,
    ) -> Option<HistoryTicket> {
        if self.history != HistoryLoad::Loading {
            return None;
        }
        Some(HistoryTicket {
            target,
            generation: self.history_generation,
            backend,
            session_id: self.session_id.clone()?,
            runtime: matches!(backend, AgentBackend::Acp(_)).then_some(self.runtime_generation),
        })
    }

    pub fn accepts_history(
        &self,
        ticket: &HistoryTicket,
        current: &session::Target,
        backend: AgentBackend,
    ) -> bool {
        same_chat(&ticket.target, current)
            && ticket.generation == self.history_generation
            && ticket.backend == backend
            && self.session_id.as_ref() == Some(&ticket.session_id)
            && self.history == HistoryLoad::Loading
            && ticket
                .runtime
                .is_none_or(|runtime| runtime == self.runtime_generation)
    }

    pub fn history_to_load(&self, config: &AgentBackendConfig) -> Option<&str> {
        (config.read_only_note().is_none()
            && config.backend().is_native()
            && self.history == HistoryLoad::NotLoaded
            && self.conversation.is_empty())
        .then_some(self.session_id.as_deref())
        .flatten()
    }

    pub fn submit_queues(&self, backend: AgentBackend, process_running: bool) -> bool {
        submit_queues(backend, process_running, &self.state)
    }

    pub fn pending_change(&self) -> Option<&SettingChange> {
        self.pending_change.as_ref().map(|(_, change)| change)
    }

    fn command_allowed(
        &self,
        runtime: &RuntimeStamp,
        current: &session::Target,
        running: bool,
    ) -> bool {
        running && !self.runtime_exited && self.accepts_runtime(runtime, current)
    }

    fn dispatch(
        &mut self,
        runtime: RuntimeStamp,
        command: HarnessCommand,
        pending: PendingCommand,
        echo: Option<AgentEvent>,
    ) -> Effect {
        let correlation = CommandCorrelation(generation());
        self.commands.insert(correlation.0, pending);
        Effect::Dispatch {
            runtime,
            correlation,
            command,
            echo,
        }
    }

    fn finish_queue(&mut self) -> Vec<UserPrompt> {
        self.queued_prompts
            .values_mut()
            .filter_map(|queued| {
                queued.withdraw_pending = false;
                if queued.status == PromptStatus::Pending && queued.written {
                    queued.status = PromptStatus::Uncertain;
                    None
                } else if matches!(queued.status, PromptStatus::Pending | PromptStatus::Queued) {
                    queued.status = PromptStatus::Unsent;
                    Some(queued.prompt.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn acknowledge(
        &mut self,
        change: SettingChange,
        applied: Option<String>,
    ) -> Option<SettingChange> {
        if self
            .pending_change
            .as_ref()
            .is_some_and(|(_, pending)| pending == &change)
        {
            self.pending_change = None;
            self.commands
                .retain(|_, command| !matches!(command, PendingCommand::Setting));
        }
        let changed = match &change {
            SettingChange::Model(value) => {
                self.acknowledged.model.replace(value.clone()).as_ref() != Some(value)
            }
            SettingChange::Mode(value) => {
                self.acknowledged.mode.replace(value.clone()).as_ref() != Some(value)
            }
            SettingChange::Effort(effort) => {
                let value = ConfirmedEffort {
                    effort: effort.clone(),
                    applied,
                };
                self.acknowledged.effort.replace(value.clone()).as_ref() != Some(&value)
            }
        };
        changed.then_some(change)
    }

    /// `Event` applies reported lifecycle; `Record` appends once and returns
    /// before/after markers for the host's ordered presentation effects.
    pub fn reduce(&mut self, intent: Intent) -> Effect {
        match intent {
            Intent::Queue {
                runtime,
                current,
                config,
                process_running,
                mut prompt,
                message_id,
            } => {
                if !self.command_allowed(&runtime, &current, process_running)
                    || config.read_only_note().is_some()
                    || !self.submit_queues(config.backend(), process_running)
                    || !self.pending_requests.is_empty()
                {
                    return Effect::Rejected("Cannot queue a prompt on this chat runtime".into());
                }
                let id = prompt.id.get_or_insert(message_id).clone();
                if id.is_empty() || self.queued_prompts.contains_key(&id) || self.shows_prompt(&id)
                {
                    return Effect::Rejected(format!(
                        "Cannot queue duplicate or empty message ID {id:?}"
                    ));
                }
                if let Err(error) = validate_images(&prompt.images) {
                    return Effect::Rejected(error);
                }
                let echo = AgentEvent::submitted_prompt(&prompt, true);
                self.queued_prompts.insert(
                    id.clone(),
                    QueuedPrompt {
                        prompt: prompt.clone(),
                        status: PromptStatus::Pending,
                        withdraw_pending: false,
                        written: false,
                    },
                );
                self.dispatch(
                    runtime,
                    HarnessCommand::SendUserMessage(prompt),
                    PendingCommand::Queue(id),
                    Some(echo),
                )
            }
            Intent::Withdraw {
                runtime,
                current,
                process_running,
                message_id,
            } => {
                if !self.command_allowed(&runtime, &current, process_running) {
                    return Effect::Rejected(format!(
                        "Not withdrawn: message {message_id} has no live owning runtime"
                    ));
                }
                let Some(prompt) = self.queued_prompts.get_mut(&message_id) else {
                    return Effect::Rejected(format!(
                        "Not withdrawn: message {message_id} is not queued"
                    ));
                };
                if !matches!(prompt.status, PromptStatus::Pending | PromptStatus::Queued)
                    || prompt.withdraw_pending
                {
                    return Effect::Rejected(format!("Not withdrawn: message {message_id} has already started or is no longer withdrawable"));
                }
                prompt.withdraw_pending = true;
                self.dispatch(
                    runtime,
                    HarnessCommand::WithdrawQueuedMessage(message_id.clone()),
                    PendingCommand::Withdraw(message_id),
                    None,
                )
            }
            Intent::ChangeSetting {
                runtime,
                current,
                config,
                process_running,
                change,
            } => {
                if !self.command_allowed(&runtime, &current, process_running)
                    || !config.backend().is_native()
                    || config.read_only_note().is_some()
                    || !change.valid(config.backend())
                {
                    return Effect::Rejected(format!(
                        "Cannot apply {change:?} on this chat runtime"
                    ));
                }
                // Admit one picker at a time until its acknowledgement or
                // correlated rejection; generic harness errors cannot settle it.
                if self.pending_change.is_some() {
                    return Effect::Rejected(
                        "A picker change is still awaiting acknowledgement".into(),
                    );
                }
                // The parser suppresses repeated mode acknowledgements.
                if matches!(&change, SettingChange::Mode(value) if self.acknowledged.mode.as_ref() == Some(value))
                {
                    return Effect::None;
                }
                let effect =
                    self.dispatch(runtime, change.command(), PendingCommand::Setting, None);
                if let Effect::Dispatch { correlation, .. } = &effect {
                    self.pending_change = Some((*correlation, change));
                }
                effect
            }
            Intent::CommandOutcome {
                runtime,
                current,
                correlation,
                outcome,
            } => {
                if !self.accepts_runtime(&runtime, &current) {
                    return Effect::None;
                }
                let Some(command) = self.commands.remove(&correlation.0) else {
                    return Effect::None;
                };
                let reason = match outcome {
                    DeliveryOutcome::Written => {
                        if let PendingCommand::Queue(id) = command {
                            if let Some(prompt) = self.queued_prompts.get_mut(&id) {
                                prompt.written = true;
                            }
                        }
                        return Effect::None;
                    }
                    DeliveryOutcome::Rejected(reason) => (reason, false),
                    DeliveryOutcome::Uncertain(reason) => (reason, true),
                };
                match command {
                    PendingCommand::Queue(id) => {
                        if let Some(prompt) = self.queued_prompts.get_mut(&id) {
                            if matches!(prompt.status, PromptStatus::Pending | PromptStatus::Queued)
                            {
                                prompt.status = if reason.1 {
                                    PromptStatus::Uncertain
                                } else {
                                    PromptStatus::Unsent
                                };
                            }
                        }
                    }
                    PendingCommand::Withdraw(id) => {
                        if let Some(prompt) = self.queued_prompts.get_mut(&id) {
                            prompt.withdraw_pending = false;
                        }
                    }
                    PendingCommand::Setting => {
                        if !reason.1
                            && self
                                .pending_change
                                .as_ref()
                                .is_some_and(|(id, _)| *id == correlation)
                        {
                            self.pending_change = None;
                        }
                    }
                }
                // An uncertain picker remains pending: it may have reached the
                // provider. Never retry it automatically or treat it as saved.
                Effect::Rejected(format!(
                    "Command {correlation:?} delivery {}: {}",
                    if reason.1 { "uncertain" } else { "rejected" },
                    reason.0
                ))
            }
            Intent::ChooseCheckout {
                runtime,
                current,
                context,
                path,
            } => {
                if !self.accepts_runtime(&runtime, &current) {
                    return Effect::Rejected(
                        "Checkout request belongs to an old chat runtime".into(),
                    );
                }
                if context.process_running
                    || self.session_id.is_some()
                    || current.task_id.is_some()
                    || matches!(self.state, AgentSessionState::Streaming)
                    || self.pending_change.is_some()
                {
                    return Effect::Rejected("Checkout is locked to the chat's session".into());
                }
                if let Some(reason) = context.locked_reason {
                    return Effect::Rejected(reason);
                }
                if !path.is_absolute() || !context.choices.contains(&path) {
                    return Effect::Rejected(format!(
                        "Checkout {} is no longer offered by this workspace",
                        path.display()
                    ));
                }
                if self.checkout.as_ref() == Some(&path) {
                    return Effect::None;
                }
                self.checkout = Some(path.clone());
                self.runtime_generation = generation();
                self.history_generation = generation();
                Effect::CheckoutBound {
                    runtime: self.runtime_stamp(current),
                    path,
                }
            }
            Intent::Submit {
                mut prompt,
                backend,
                process_running,
                just_spawned,
                message_id,
            } => {
                if backend == AgentBackend::Acp(AcpAgent::Gemini) {
                    return Effect::Rejected(GEMINI_REPLACED_NOTE.into());
                }
                let queued = !just_spawned && self.submit_queues(backend, process_running);
                if backend.is_native() && prompt.id.is_none() {
                    prompt.id = Some(message_id);
                }
                let images_not_sent = if backend == AgentBackend::Pi {
                    prompt.images.len()
                } else {
                    0
                };
                let echo = if images_not_sent > 0 {
                    AgentEvent::user_prompt(&prompt.text)
                } else {
                    AgentEvent::submitted_prompt(&prompt, queued)
                };
                Effect::Submit(Submission {
                    prompt,
                    echo,
                    images_not_sent,
                })
            }
            Intent::RelayedPrompt {
                id,
                text,
                image_count,
                backend,
                process_running,
            } => {
                if id.as_deref().is_some_and(|id| self.shows_prompt(id)) {
                    return Effect::None;
                }
                let queued = self.submit_queues(backend, process_running);
                let echo = AgentEvent::relayed_prompt(id.as_deref(), &text, image_count, queued);
                if process_running {
                    self.state = AgentSessionState::Streaming;
                }
                Effect::RelayedPrompt {
                    echo,
                    running: process_running,
                }
            }
            Intent::SubmissionAccepted => {
                self.state = AgentSessionState::Streaming;
                Effect::None
            }
            Intent::Event(event) => Effect::Event(self.apply_event(&event)),
            Intent::Record { event, now_ms } => {
                let (before, after) = self.record(event, now_ms);
                Effect::Recorded { before, after }
            }
            Intent::BindSession(id) => {
                if self.session_id != id {
                    self.history_generation = generation();
                    if self.history == HistoryLoad::Loading {
                        self.history = HistoryLoad::NotLoaded;
                    }
                }
                self.session_id = id;
                Effect::None
            }
            Intent::BeginHistory(config) => {
                if config.read_only_note().is_some() {
                    self.history = HistoryLoad::Loaded;
                    return Effect::None;
                }
                let Some(id) = self.history_to_load(&config).map(str::to_owned) else {
                    return Effect::None;
                };
                self.history_generation = generation();
                self.history = HistoryLoad::Loading;
                Effect::LoadHistory(id)
            }
            Intent::HistoryLoaded(history) => {
                self.history = HistoryLoad::Loaded;
                if history.is_empty() {
                    return Effect::None;
                }
                // Preserve Iced's prepend semantics. ID-less transcript entries
                // cannot prove overlap with live events; never dedup by text.
                let live = std::mem::replace(&mut self.conversation, history);
                self.conversation.extend(live);
                Effect::ReplayHistory
            }
            Intent::HistoryUnavailable(error) => {
                self.history = HistoryLoad::Loaded;
                Effect::Rejected(error)
            }
            Intent::ResetHistory => {
                self.conversation.clear();
                self.pending_requests.clear();
                self.state = AgentSessionState::Idle;
                self.history = HistoryLoad::NotLoaded;
                self.history_generation = generation();
                Effect::None
            }
            Intent::FinishHistory => {
                self.history = HistoryLoad::Loaded;
                Effect::None
            }
            Intent::Answer {
                runtime,
                current,
                request_id,
                decision,
                process_running,
            } => {
                if !self.accepts_runtime(&runtime, &current)
                    || !process_running
                    || !self.pending_requests.contains(&request_id)
                {
                    return Effect::Rejected(format!(
                        "Request {request_id} is no longer pending on this chat runtime"
                    ));
                }
                // Only the authoritative runtime event closes a request.
                Effect::Send {
                    runtime,
                    command: HarnessCommand::Answer {
                        request_id,
                        decision,
                    },
                }
            }
            Intent::Stop {
                runtime,
                authority,
                target,
            } => {
                if !self.accepts_runtime(&runtime, &authority.target) {
                    return Effect::None;
                }
                match authority.reduce(session::Intent::StopTurn { target }) {
                    session::Effect::InterruptHarness(_) => Effect::Send {
                        runtime,
                        command: HarnessCommand::Interrupt,
                    },
                    session::Effect::CancelTaskTurn(_) => {
                        self.state = AgentSessionState::Stopped;
                        Effect::CancelLegacyTurn
                    }
                    _ => Effect::None,
                }
            }
        }
    }

    fn apply_event(&mut self, event: &AgentEvent) -> EventEffects {
        let mut effects = EventEffects::default();
        match event {
            // Keep C6 presentation/lifecycle semantics, but a dead process
            // cannot subsequently acknowledge a control or start queued input.
            AgentEvent::Harness(
                HarnessEvent::MessageLifecycle { .. }
                | HarnessEvent::ModelChanged(_)
                | HarnessEvent::PermissionModeChanged(_)
                | HarnessEvent::EffortChanged { .. },
            ) if self.runtime_exited => {}
            AgentEvent::Harness(event) => match event {
                HarnessEvent::MessageLifecycle { id, state } => {
                    if let Some(prompt) = self.queued_prompts.get_mut(id) {
                        let status = match state {
                            MessageState::Queued => PromptStatus::Queued,
                            MessageState::Started => PromptStatus::Started,
                            MessageState::Completed => PromptStatus::Completed,
                            MessageState::Cancelled => PromptStatus::Cancelled,
                        };
                        // Once started, cancellation cannot remove a sent
                        // prompt, even if an old queue/cancel frame arrives.
                        let allowed = match prompt.status {
                            PromptStatus::Pending
                            | PromptStatus::Queued
                            | PromptStatus::Uncertain => true,
                            PromptStatus::Started => {
                                matches!(state, MessageState::Started | MessageState::Completed)
                            }
                            PromptStatus::Completed
                            | PromptStatus::Cancelled
                            | PromptStatus::Unsent => false,
                        };
                        if allowed {
                            if prompt.withdraw_pending
                                && matches!(state, MessageState::Started | MessageState::Completed)
                            {
                                effects.notices.push(format!(
                                    "Not withdrawn ({id}): the message has already started"
                                ));
                            }
                            prompt.status = status;
                            if *state != MessageState::Queued {
                                prompt.withdraw_pending = false;
                            }
                        }
                    }
                }
                HarnessEvent::MessageWithdrawRefused { id, reason } => {
                    if let Some(prompt) = self.queued_prompts.get_mut(id) {
                        prompt.withdraw_pending = false;
                    }
                    effects
                        .notices
                        .push(format!("Not withdrawn ({id}): {reason}"));
                }
                HarnessEvent::ModelChanged(value) => {
                    effects.settings_changed =
                        self.acknowledge(SettingChange::Model(value.clone()), None);
                }
                HarnessEvent::PermissionModeChanged(value) => {
                    effects.settings_changed =
                        self.acknowledge(SettingChange::Mode(value.clone()), None);
                }
                HarnessEvent::EffortChanged { effort, applied } => {
                    effects.settings_changed =
                        self.acknowledge(SettingChange::Effort(effort.clone()), applied.clone());
                }
                HarnessEvent::TurnStarted { session_id, .. } => {
                    self.state = AgentSessionState::Streaming;
                    if let Some(id) = session_id {
                        if self.session_id.as_ref() != Some(id) {
                            self.reduce(Intent::BindSession(Some(id.clone())));
                            effects.session_id = Some(id.clone());
                        }
                    }
                }
                HarnessEvent::RuntimeRequest { request_id, .. } => {
                    self.pending_requests.push(request_id.clone());
                    effects.awaiting_input = Some(true);
                }
                HarnessEvent::RuntimeRequestResolved { request_id } => {
                    self.pending_requests.retain(|id| id != request_id);
                    if self.pending_requests.is_empty() {
                        effects.awaiting_input = Some(false);
                    }
                }
                HarnessEvent::TurnCompleted { status, .. } => {
                    self.pending_requests.clear();
                    self.state = match status {
                        TurnStatus::Failed(message) => AgentSessionState::Errored(message.clone()),
                        _ => AgentSessionState::Idle,
                    };
                    effects.outcome = Some(status.clone());
                }
                HarnessEvent::ProcessExited { code } => {
                    effects.process_exited = true;
                    self.runtime_exited = true;
                    effects.unsent_prompts = self.finish_queue();
                    if let Some((_, change)) = self.pending_change.take() {
                        effects.notices.push(format!(
                            "Change {change:?} was not acknowledged before the agent exited"
                        ));
                    }
                    self.commands.clear();
                    self.pending_requests.clear();
                    if matches!(self.state, AgentSessionState::Streaming) {
                        let message = format!("The chat's agent exited mid-turn (code {code:?})");
                        self.state = AgentSessionState::Errored(message.clone());
                        effects.outcome = Some(TurnStatus::Failed(message));
                    }
                }
                _ => {}
            },
            AgentEvent::Result(_) => {
                self.state = AgentSessionState::Idle;
                effects.outcome = Some(TurnStatus::Completed);
            }
            AgentEvent::Other(value) => match value.get("type").and_then(|v| v.as_str()) {
                Some("done") => {
                    self.state = AgentSessionState::Idle;
                    effects.outcome = Some(TurnStatus::Completed);
                }
                Some("stopped") => {
                    self.state = AgentSessionState::Stopped;
                    effects.outcome = Some(TurnStatus::Interrupted);
                }
                Some("error" | "failed") => {
                    let message = "Agent reported an error".to_string();
                    self.state = AgentSessionState::Errored(message.clone());
                    effects.outcome = Some(TurnStatus::Failed(message));
                }
                _ => {}
            },
            _ => {}
        }
        effects
    }
    /// Append an event to the conversation buffer, merging consecutive
    /// streaming fragments so tab switches replay a compact buffer. A
    /// subagent's start is followed by when it started, so a rebuilt page
    /// still knows its elapsed time and where it falls in the roster.
    fn record(
        &mut self,
        ev: AgentEvent,
        now_ms: Option<i64>,
    ) -> (Vec<AgentEvent>, Vec<AgentEvent>) {
        self.revision = self.revision.wrapping_add(1);
        let turn_started = matches!(&ev, AgentEvent::Harness(HarnessEvent::TurnStarted { .. }))
            || matches!(
                &ev,
                AgentEvent::Other(value)
                    if value.get("type").and_then(|v| v.as_str()) == Some("user_prompt")
                        && value.get("queued").and_then(|v| v.as_bool()) != Some(true)
            );
        let queued_prompt = matches!(
            &ev,
            AgentEvent::Other(value)
                if value.get("type").and_then(|v| v.as_str()) == Some("user_prompt")
                    && value.get("queued").and_then(|v| v.as_bool()) == Some(true)
        );
        let turn_ended = matches!(
            &ev,
            AgentEvent::Harness(
                HarnessEvent::TurnCompleted { .. } | HarnessEvent::ProcessExited { .. }
            )
        );
        let timing = match (turn_started, turn_ended, now_ms) {
            (true, false, Some(ms)) => {
                Some(AgentEvent::Other(history::turn_timing(Some(ms), None)))
            }
            (false, true, Some(ms)) => {
                Some(AgentEvent::Other(history::turn_timing(None, Some(ms))))
            }
            _ => None,
        };
        let mut before = Vec::new();
        let mut after = Vec::new();
        if turn_ended {
            if let Some(timing) = timing.clone() {
                self.conversation.push(timing.clone());
                before.push(timing);
            }
        }
        if queued_prompt {
            if let Some(ms) = now_ms {
                let marker =
                    AgentEvent::Other(serde_json::json!({"kind":"turn_timing", "split_ms":ms}));
                self.conversation.push(marker.clone());
                before.push(marker);
            }
        }
        if let (Some(AgentEvent::Harness(last)), AgentEvent::Harness(next)) =
            (self.conversation.last_mut(), &ev)
        {
            if merge_fragment(last, next) {
                return (before, after);
            }
        }
        let started = event_subagent_started(&ev).map(str::to_string);
        self.conversation.push(ev);
        if turn_started {
            if let Some(timing) = timing {
                self.conversation.push(timing.clone());
                after.push(timing);
            }
        }
        if let Some(parent) = started {
            if let Some(now_ms) = now_ms {
                self.conversation
                    .push(AgentEvent::Other(history::subagent_started_at(
                        &parent, now_ms,
                    )));
            }
        }
        (before, after)
    }
}

pub fn submit_queues(
    backend: AgentBackend,
    process_running: bool,
    state: &AgentSessionState,
) -> bool {
    backend.is_native() && process_running && matches!(state, AgentSessionState::Streaming)
}

fn merge_fragment(last: &mut HarnessEvent, next: &HarnessEvent) -> bool {
    use HarnessEvent as H;
    match (last, next) {
        (H::TextDelta(a), H::TextDelta(b)) | (H::ThinkingDelta(a), H::ThinkingDelta(b)) => {
            a.push_str(b);
            true
        }
        (
            H::ItemInputDelta {
                id: a_id,
                partial_json: a,
            },
            H::ItemInputDelta {
                id: b_id,
                partial_json: b,
            },
        ) if a_id == b_id => {
            a.push_str(b);
            true
        }
        (
            H::SubagentEvent {
                parent_tool_use_id: a_parent,
                event: a,
            },
            H::SubagentEvent {
                parent_tool_use_id: b_parent,
                event: b,
            },
        ) if a_parent == b_parent => merge_fragment(a, b),
        _ => false,
    }
}

#[cfg(test)]
#[path = "state_controls_tests.rs"]
mod controls_tests;

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
