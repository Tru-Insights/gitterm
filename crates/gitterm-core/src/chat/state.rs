//! Authoritative, frontend-independent chat transitions and timeline.
//!
//! The host owns handles and the session catalog. It must validate the complete
//! ingress with `accepts_runtime`/`accepts_history` before applying host effects.
//! Channel acceptance is not a harness write receipt. No types here are persisted.
use super::{history, types::*};
use crate::{harness::transcript::TranscriptEntry, session};
use std::sync::atomic::{AtomicU64, Ordering};

// Process-wide incarnations never repeat when a tab replaces its chat state.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
fn generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
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
            runtime_generation: generation(),
            history_generation: generation(),
        }
    }
}

#[derive(Debug)]
pub enum Intent {
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
}

#[derive(Debug)]
pub enum Effect {
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
        self.pending_requests.clear();
    }

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
        ticket.target == *current
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

    /// `Event` applies reported lifecycle; `Record` appends once and returns
    /// before/after markers for the host's ordered presentation effects.
    pub fn reduce(&mut self, intent: Intent) -> Effect {
        match intent {
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
            AgentEvent::Harness(event) => match event {
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
#[path = "state_tests.rs"]
mod tests;
