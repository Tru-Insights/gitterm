//! Chats overview (TRU-154): the pure parts.
//!
//! With the Chats panel open and no conversation selected, the main pane
//! shows everything running across this Mac and the connected remote
//! agents. The app builds one [`ChatSnapshot`] per open chat (and per
//! remote persistent chat no tab holds) from its tabs, delegations and the
//! remotes' `ListClaudeChats`; TRU-155's per-chat view reads the same
//! snapshot. This module owns the snapshot types, the conversation walk
//! that says what a chat is doing ([`conversation_activity`]), the plain
//! words for a tool call ([`describe_tool`]) and the split into the
//! overview's sections ([`sections`]). No iced types, no app state.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gitterm::chats::{ChatBackend, ChatRef};
use gitterm::harness::{HarnessEvent, ItemKind, RuntimeRequestKind, TurnStatus};
use gitterm::tasks::{
    Delegation, DelegationChild, DelegationKind, DelegationStatus, ReviewTargetMode, ReviewVerdict,
};
use serde_json::Value;

use crate::tab::AgentEvent;

/// How far back "Recently finished" reaches.
pub(crate) const RECENT_WINDOW: Duration = Duration::from_secs(4 * 60 * 60);
/// Longest command, path or pattern quoted in a step.
const STEP_DETAIL_CHARS: usize = 60;
/// Longest one-line result or ask.
const LINE_CHARS: usize = 140;

/// What runs the chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    /// A native Claude chat tab.
    Claude,
    Gemini,
    OpenCode,
    Pi,
    /// `codex` in a terminal tab.
    CodexCli,
    /// `claude` in a terminal tab.
    ClaudeCli,
    /// A plain terminal that wants the human.
    Terminal,
}

impl Harness {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
            Self::CodexCli => "Codex CLI",
            Self::ClaudeCli => "Claude CLI",
            Self::Terminal => "Terminal",
        }
    }
}

/// Which CLI a terminal tab runs, as far as GitTerm can tell: the
/// conversation the Chats registry ties it to, then its startup command,
/// then its title (Claude Code writes "✳ <topic>" or a spinner glyph).
pub(crate) fn terminal_harness(
    chat_backend: Option<ChatBackend>,
    startup_command: Option<&str>,
    title: Option<&str>,
) -> Harness {
    match chat_backend {
        Some(ChatBackend::Claude) => return Harness::ClaudeCli,
        Some(ChatBackend::Codex) => return Harness::CodexCli,
        Some(ChatBackend::Pi) => return Harness::Pi,
        None => {}
    }
    let program = |name: &str| {
        startup_command.is_some_and(|command| {
            command.split_whitespace().any(|token| {
                let token = token.trim_matches(|c| c == '\'' || c == '"');
                Path::new(token).file_name().and_then(|n| n.to_str()) == Some(name)
            })
        })
    };
    if program("claude") {
        return Harness::ClaudeCli;
    }
    if program("codex") {
        return Harness::CodexCli;
    }
    let Some(title) = title else {
        return Harness::Terminal;
    };
    let lower = title.to_lowercase();
    if lower.contains("codex") {
        Harness::CodexCli
    } else if lower.contains("claude")
        || title.trim_start().chars().next().is_some_and(|ch| {
            matches!(ch, '✳' | '◐' | '◑' | '◒' | '◓' | '✢' | '✶' | '✻' | '✽')
                || ('\u{2800}'..='\u{28FF}').contains(&ch)
        })
    {
        Harness::ClaudeCli
    } else {
        Harness::Terminal
    }
}

/// Where the chat is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChatState {
    /// A permission prompt, a question or a blocked worker waits on the
    /// human. `what` is the ask in plain words.
    NeedsYou {
        what: String,
    },
    Working,
    Idle,
    /// Its process went away and nothing has restarted it.
    Exited,
}

/// Where a click on a row goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavTarget {
    /// An open tab, by id.
    Tab(usize),
    /// An open chat tab with one roster row's activity opened: `key` is
    /// the page's roster key (`sub:<tool_use_id>` or `dlg:<delegation id>`).
    Roster {
        tab_id: usize,
        key: String,
    },
    /// A conversation no tab holds: the Chats panel's Open Chat path
    /// (attaches to a remote persistent chat, resumes a local one).
    OpenChat(ChatRef),
    /// A task's context.
    Task(String),
    Nowhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildKind {
    /// Claude's own Agent tool inside the chat.
    Subagent,
    CodexReview,
    CodexConsult,
    /// A worker session in a GitTerm task.
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildStatus {
    Queued,
    Running,
    /// A worker reported it cannot go on without the human.
    Blocked,
    Done,
    Failed,
    Stopped,
}

impl ChildStatus {
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// A subagent or delegation working for a chat.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChildActivity {
    pub(crate) kind: ChildKind,
    pub(crate) label: String,
    pub(crate) status: ChildStatus,
    /// What it is doing now, while active.
    pub(crate) step: Option<String>,
    pub(crate) started_at: Option<SystemTime>,
    pub(crate) finished_at: Option<SystemTime>,
    /// One line, once it has finished (or is blocked).
    pub(crate) result: Option<String>,
    pub(crate) target: NavTarget,
}

/// The chat's last finished turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FinishedTurn {
    /// Known only for a turn that finished while GitTerm watched.
    pub(crate) at: Option<SystemTime>,
    pub(crate) result: String,
    pub(crate) failed: bool,
    /// What the turn cost, when the harness reports cost.
    pub(crate) cost_usd: Option<f64>,
}

/// One chat as the overview (and TRU-155's per-chat view) sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatSnapshot {
    /// The conversation, once its id is known.
    pub(crate) chat: Option<ChatRef>,
    pub(crate) title: String,
    pub(crate) machine: String,
    pub(crate) repo: String,
    pub(crate) branch: Option<String>,
    pub(crate) harness: Harness,
    pub(crate) model: Option<String>,
    pub(crate) state: ChatState,
    /// What it is doing now, in plain words.
    pub(crate) step: Option<String>,
    pub(crate) turn_started_at: Option<SystemTime>,
    /// The session's cost so far (Claude reports it per process).
    pub(crate) cost_usd: Option<f64>,
    pub(crate) children: Vec<ChildActivity>,
    pub(crate) last_finished: Option<FinishedTurn>,
    pub(crate) target: NavTarget,
}

/// A delegation whose parent chat is not open in a tab.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DetachedActivity {
    /// The parent chat's title as far as it is known.
    pub(crate) parent: String,
    /// The parent conversation, when the Chats index knows it: the
    /// selected-chat summary lists the delegations of a chat no tab holds
    /// (TRU-155).
    pub(crate) parent_chat: Option<ChatRef>,
    pub(crate) machine: String,
    pub(crate) child: ChildActivity,
}

/// Everything the overview shows, built by the app.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct OverviewSnapshot {
    pub(crate) chats: Vec<ChatSnapshot>,
    pub(crate) detached: Vec<DetachedActivity>,
}

impl OverviewSnapshot {
    pub(crate) fn is_empty(&self) -> bool {
        self.chats.is_empty() && self.detached.is_empty()
    }
}

/// Times a chat tab learns only by watching its events live: the
/// conversation buffer carries no clock. Fed by `AgentSession::record`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LiveTimes {
    pub(crate) turn_started: Option<SystemTime>,
    pub(crate) turn_finished: Option<SystemTime>,
    /// Agent tool call id -> when its subagent finished.
    pub(crate) children_finished: HashMap<String, SystemTime>,
    turn_open: bool,
    agent_calls: HashSet<String>,
}

impl LiveTimes {
    pub(crate) fn observe(&mut self, event: &AgentEvent, now: SystemTime) {
        let AgentEvent::Harness(event) = event else {
            return;
        };
        match event {
            HarnessEvent::TurnStarted { .. } if !self.turn_open => {
                self.turn_open = true;
                self.turn_started = Some(now);
            }
            HarnessEvent::TurnCompleted { .. } => {
                self.turn_open = false;
                self.turn_finished = Some(now);
            }
            HarnessEvent::ProcessExited { .. } if self.turn_open => {
                self.turn_open = false;
                self.turn_finished = Some(now);
            }
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, .. },
            } if is_subagent_tool(name) => {
                self.agent_calls.insert(id.clone());
            }
            // A background subagent's call completes at once and the
            // subagent's own completion (below) overrides this.
            HarnessEvent::ItemCompleted { id, .. } if self.agent_calls.contains(id) => {
                self.children_finished.entry(id.clone()).or_insert(now);
            }
            HarnessEvent::SubagentEvent {
                parent_tool_use_id,
                event,
            } if matches!(**event, HarnessEvent::TurnCompleted { .. }) => {
                self.children_finished
                    .insert(parent_tool_use_id.clone(), now);
            }
            _ => {}
        }
    }
}

/// A subagent the conversation started, as far as its events tell.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentActivity {
    pub(crate) tool_use_id: String,
    pub(crate) label: String,
    pub(crate) status: ChildStatus,
    pub(crate) step: Option<String>,
    pub(crate) started_at: Option<SystemTime>,
}

/// The chat's last finished turn as the conversation tells it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnOutcome {
    pub(crate) result: String,
    pub(crate) failed: bool,
    pub(crate) cost_usd: Option<f64>,
}

/// What a chat's conversation buffer says about it right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ConversationActivity {
    pub(crate) model: Option<String>,
    pub(crate) turn_open: bool,
    /// The current step while a turn is open.
    pub(crate) step: Option<String>,
    /// Open permission prompts and questions, oldest first, in plain words.
    pub(crate) asks: Vec<String>,
    /// The session's cost so far (cumulative per process).
    pub(crate) cost_usd: Option<f64>,
    pub(crate) last_turn: Option<TurnOutcome>,
    pub(crate) subagents: Vec<SubagentActivity>,
    /// The process exited after its last turn started.
    pub(crate) exited: bool,
}

/// The latest thing a turn (or subagent) did that a step can name.
#[derive(Debug, Clone, PartialEq)]
enum Latest<'a> {
    Nothing,
    Item(&'a str),
    Thinking,
    Text,
}

/// A tool call whose input may still be streaming in. Borrows from the
/// conversation: a walk copies no tool input (a Write's can be a whole
/// file) and parses only the calls a step or label reads.
#[derive(Debug, Clone)]
pub(crate) struct ToolCall<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    input: &'a Value,
    /// `ItemInputDelta` fragments: concatenated they are the whole input
    /// (Claude), or one full document (ACP).
    pub(crate) deltas: Vec<&'a str>,
}

impl<'a> ToolCall<'a> {
    pub(crate) fn new(id: &'a str, name: &'a str, input: &'a Value) -> Self {
        Self {
            id,
            name,
            input,
            deltas: Vec::new(),
        }
    }

    /// The input as far as it has arrived: the streamed document once it
    /// parses, else what the call was announced with.
    pub(crate) fn input(&self) -> std::borrow::Cow<'a, Value> {
        if !self.deltas.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(&self.deltas.concat()) {
                return std::borrow::Cow::Owned(value);
            }
        }
        std::borrow::Cow::Borrowed(self.input)
    }
}

#[derive(Debug, Clone)]
struct SubagentState<'a> {
    call: ToolCall<'a>,
    finished: Option<ChildStatus>,
    /// The Agent call's own result: `Some(is_error)`.
    call_completed: Option<bool>,
    /// Subagent events arrived after the call's result (a background
    /// subagent still at work).
    active_after_call: bool,
    open: Vec<ToolCall<'a>>,
    latest: Latest<'a>,
    started_at: Option<SystemTime>,
}

/// Whether a tool call starts a subagent (Claude's Agent tool; `Task` in
/// older CLIs).
fn is_subagent_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

/// The step `latest` names, given the calls still open.
fn step_for(latest: &Latest<'_>, open: &[ToolCall<'_>], cwd: Option<&Path>) -> Option<String> {
    let call = |id: &str| open.iter().find(|call| call.id == id);
    match latest {
        Latest::Item(id) => call(id)
            .or(open.last())
            .map(|call| describe_tool(call.name, &call.input(), cwd)),
        Latest::Thinking => Some("Thinking".to_string()),
        Latest::Text => Some("Writing a reply".to_string()),
        Latest::Nothing => open
            .last()
            .map(|call| describe_tool(call.name, &call.input(), cwd)),
    }
}

/// Walk a chat's conversation buffer once: its model, whether a turn is
/// open and what it is doing, the asks waiting on the human, cost, the
/// last turn's result, and its subagents. `cwd` shortens paths in steps.
pub(crate) fn conversation_activity(
    conversation: &[AgentEvent],
    cwd: Option<&Path>,
) -> ConversationActivity {
    let mut activity = ConversationActivity::default();
    let mut open: Vec<ToolCall> = Vec::new();
    let mut latest = Latest::Nothing;
    // (request id, tool_use id, ask)
    let mut requests: Vec<(String, String, String)> = Vec::new();
    // The turn's latest reply segment, and whether a tool call came after
    // it (the next text then starts a new segment).
    let mut segment = String::new();
    let mut tool_since_text = false;
    let mut subagents: Vec<SubagentState<'_>> = Vec::new();
    // Tool call id inside a subagent -> its root Agent call id.
    let mut sub_tools: HashMap<&str, &str> = HashMap::new();

    for event in conversation {
        match event {
            AgentEvent::Harness(event) => match event {
                HarnessEvent::TurnStarted { model, .. } => {
                    if !activity.turn_open {
                        segment.clear();
                        tool_since_text = false;
                    }
                    activity.turn_open = true;
                    activity.exited = false;
                    if let Some(model) = model.as_deref().filter(|m| !m.is_empty()) {
                        activity.model = Some(model.to_string());
                    }
                }
                HarnessEvent::ModelChanged(model) if model != "default" && !model.is_empty() => {
                    activity.model = Some(model.clone());
                }
                HarnessEvent::TextDelta(text) => {
                    // A reply after a tool call starts a new segment; the
                    // last segment is the turn's answer.
                    if tool_since_text {
                        segment.clear();
                        tool_since_text = false;
                    }
                    segment.push_str(text);
                    latest = Latest::Text;
                }
                HarnessEvent::ThinkingDelta(_) => latest = Latest::Thinking,
                HarnessEvent::ItemStarted {
                    id,
                    kind: ItemKind::ToolCall { name, input },
                } => {
                    let call = ToolCall::new(id, name, input);
                    if is_subagent_tool(name) {
                        subagents.push(SubagentState {
                            call: call.clone(),
                            finished: None,
                            call_completed: None,
                            active_after_call: false,
                            open: Vec::new(),
                            latest: Latest::Nothing,
                            started_at: None,
                        });
                    }
                    open.push(call);
                    latest = Latest::Item(id);
                    tool_since_text = true;
                }
                HarnessEvent::ItemInputDelta { id, partial_json } => {
                    if let Some(call) = open.iter_mut().find(|call| call.id == id) {
                        call.deltas.push(partial_json);
                    }
                    if let Some(sub) = subagents.iter_mut().find(|sub| sub.call.id == id) {
                        sub.call.deltas.push(partial_json);
                    }
                }
                HarnessEvent::ItemCompleted { id, is_error, .. } => {
                    open.retain(|call| call.id != id);
                    if latest == Latest::Item(id) {
                        latest = Latest::Nothing;
                    }
                    if let Some(sub) = subagents.iter_mut().find(|sub| sub.call.id == id) {
                        sub.call_completed = Some(*is_error);
                        sub.active_after_call = false;
                    }
                }
                HarnessEvent::RuntimeRequest {
                    request_id,
                    tool_use_id,
                    kind,
                } => requests.push((
                    request_id.clone(),
                    tool_use_id.clone(),
                    describe_ask(kind, cwd),
                )),
                HarnessEvent::RuntimeRequestResolved { request_id } => {
                    requests.retain(|(id, _, _)| id != request_id);
                }
                HarnessEvent::TurnCompleted {
                    status, cost_usd, ..
                } => {
                    // Claude reports cost cumulatively per process: a
                    // turn's own cost is the step from the last report.
                    let turn_cost = match (*cost_usd, activity.cost_usd) {
                        (Some(now), Some(before)) if now >= before => Some(now - before),
                        (now, _) => now,
                    };
                    if cost_usd.is_some() {
                        activity.cost_usd = *cost_usd;
                    }
                    activity.last_turn = Some(TurnOutcome {
                        result: turn_result(&segment, status),
                        failed: matches!(status, TurnStatus::Failed(_)),
                        cost_usd: turn_cost,
                    });
                    activity.turn_open = false;
                    open.clear();
                    requests.clear();
                    latest = Latest::Nothing;
                }
                HarnessEvent::ProcessExited { .. } => {
                    activity.exited = true;
                    activity.turn_open = false;
                    open.clear();
                    requests.clear();
                    latest = Latest::Nothing;
                }
                HarnessEvent::SubagentEvent {
                    parent_tool_use_id,
                    event,
                } => subagent_event(&mut subagents, &mut sub_tools, parent_tool_use_id, event),
                _ => {}
            },
            AgentEvent::Other(value)
                if value.get("kind").and_then(Value::as_str) == Some("subagent_started_at") =>
            {
                let id = value.get("id").and_then(Value::as_str);
                let at = value
                    .get("at_ms")
                    .and_then(Value::as_i64)
                    .and_then(|ms| u64::try_from(ms).ok())
                    .map(|ms| UNIX_EPOCH + Duration::from_millis(ms));
                if let (Some(id), Some(at)) = (id, at) {
                    if let Some(sub) = subagents.iter_mut().find(|sub| sub.call.id == id) {
                        sub.started_at.get_or_insert(at);
                    }
                }
            }
            _ => {}
        }
    }

    if activity.turn_open {
        activity.step = step_for(&latest, &open, cwd).or_else(|| Some("Working".to_string()));
    }
    activity.asks = requests.iter().map(|(_, _, ask)| ask.clone()).collect();
    let waiting_roots: HashSet<&str> = requests
        .iter()
        .filter_map(|(_, tool_use_id, _)| sub_tools.get(tool_use_id.as_str()).copied())
        .collect();
    activity.subagents = subagents
        .iter()
        .map(|sub| {
            let status = match (sub.finished, sub.call_completed) {
                (Some(status), _) => status,
                (None, Some(is_error)) if !sub.active_after_call => {
                    if is_error {
                        ChildStatus::Failed
                    } else {
                        ChildStatus::Done
                    }
                }
                _ if activity.exited => ChildStatus::Stopped,
                _ => ChildStatus::Running,
            };
            let step = (status == ChildStatus::Running).then(|| {
                if waiting_roots.contains(sub.call.id) {
                    "Waiting for approval".to_string()
                } else {
                    step_for(&sub.latest, &sub.open, cwd).unwrap_or_else(|| "Starting".into())
                }
            });
            SubagentActivity {
                tool_use_id: sub.call.id.to_string(),
                label: subagent_label(&sub.call.input()),
                status,
                step,
                started_at: sub.started_at,
            }
        })
        .collect();
    activity
}

/// Apply one subagent event to the subagent it belongs to. A subagent's
/// own subagent rolls up into the same root, as the chat page does.
fn subagent_event<'a>(
    subagents: &mut [SubagentState<'a>],
    sub_tools: &mut HashMap<&'a str, &'a str>,
    parent: &'a str,
    event: &'a HarnessEvent,
) {
    let root: &'a str = if let Some(sub) = subagents.iter().find(|sub| sub.call.id == parent) {
        sub.call.id
    } else if let Some(root) = sub_tools.get(parent) {
        root
    } else {
        return;
    };
    if let HarnessEvent::SubagentEvent {
        parent_tool_use_id,
        event,
    } = event
    {
        subagent_event(subagents, sub_tools, parent_tool_use_id, event);
        return;
    }
    let Some(sub) = subagents.iter_mut().find(|sub| sub.call.id == root) else {
        return;
    };
    let direct = parent == root;
    if sub.call_completed.is_some() {
        sub.active_after_call = true;
    }
    match event {
        HarnessEvent::TurnStarted { .. } if direct => sub.finished = None,
        HarnessEvent::TurnCompleted { status, .. } if direct => {
            sub.finished = Some(match status {
                TurnStatus::Completed => ChildStatus::Done,
                TurnStatus::Interrupted => ChildStatus::Stopped,
                TurnStatus::Failed(_) => ChildStatus::Failed,
            });
        }
        HarnessEvent::TextDelta(_) => sub.latest = Latest::Text,
        HarnessEvent::ThinkingDelta(_) => sub.latest = Latest::Thinking,
        HarnessEvent::ItemStarted {
            id,
            kind: ItemKind::ToolCall { name, input },
        } => {
            sub_tools.insert(id, root);
            sub.open.push(ToolCall::new(id, name, input));
            sub.latest = Latest::Item(id);
        }
        HarnessEvent::ItemCompleted { id, .. } => {
            sub.open.retain(|call| call.id != id);
            if sub.latest == Latest::Item(id) {
                sub.latest = Latest::Nothing;
            }
        }
        _ => {}
    }
}

/// A subagent's row label from its Agent call input: "type · description".
fn subagent_label(input: &Value) -> String {
    let field = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match (field("subagent_type"), field("description")) {
        (Some(kind), Some(description)) => {
            format!("{kind} · {}", one_line(description, LINE_CHARS))
        }
        (None, Some(description)) => one_line(description, LINE_CHARS),
        (Some(kind), None) => kind.to_string(),
        (None, None) => "Subagent".to_string(),
    }
}

/// The turn's result in one line: the first line of its last reply, or
/// how it ended when it did not finish.
fn turn_result(last_reply: &str, status: &TurnStatus) -> String {
    let reply = first_line(last_reply);
    match status {
        TurnStatus::Completed if reply.is_empty() => "Done".to_string(),
        TurnStatus::Completed => one_line(reply, LINE_CHARS),
        TurnStatus::Interrupted => "Interrupted".to_string(),
        TurnStatus::Failed(message) => {
            one_line(&format!("Failed: {}", first_line(message)), LINE_CHARS)
        }
    }
}

/// The first line with text, without markdown's leading markers.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(|line| line.trim().trim_start_matches(['#', '>', '*', '-']).trim())
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// `text` collapsed onto one line and cut to `max` characters.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// A path for a step: relative to `cwd` when under it, else the last two
/// components.
fn short_path(path: &str, cwd: Option<&Path>) -> String {
    let full = Path::new(path);
    if let Some(relative) = cwd.and_then(|cwd| full.strip_prefix(cwd).ok()) {
        if !relative.as_os_str().is_empty() {
            return one_line(&relative.to_string_lossy(), STEP_DETAIL_CHARS);
        }
    }
    let components: Vec<_> = full.components().collect();
    let tail: std::path::PathBuf = components
        .iter()
        .skip(components.len().saturating_sub(2))
        .collect();
    one_line(&tail.to_string_lossy(), STEP_DETAIL_CHARS)
}

/// What a tool call is doing, in plain words: "Running `cargo test`",
/// "Editing src/main.rs", "Subagent: find the callers", ... Unknown tools
/// read "Using <tool>".
pub(crate) fn describe_tool(name: &str, input: &Value, cwd: Option<&Path>) -> String {
    let field = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let path = || {
        field("file_path")
            .or_else(|| field("notebook_path"))
            .or_else(|| field("path"))
            .map(|p| short_path(p, cwd))
    };
    let quoted = |text: &str| format!("`{}`", one_line(first_line_raw(text), STEP_DETAIL_CHARS));
    // MCP tools are `mcp__<server>__<tool>`.
    if let Some(rest) = name.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or(("", rest));
        if server.contains("gitterm_tasks") || server.contains("gitterm-tasks") {
            return match tool {
                "review_request" => "Waiting for Codex review".to_string(),
                "consult_request" => "Waiting for Codex consult".to_string(),
                "delegate_task" => match field("title") {
                    Some(title) => format!("Delegating: {}", one_line(title, STEP_DETAIL_CHARS)),
                    None => "Delegating a task".to_string(),
                },
                "delegation_get" | "delegation_list" => "Checking delegations".to_string(),
                "task_update_handoff" => "Writing a handoff".to_string(),
                _ => format!("Using {tool}"),
            };
        }
        return format!("Using {tool}");
    }
    match name {
        "Bash" => match field("command") {
            Some(command) => format!("Running {}", quoted(command)),
            None => "Running a command".to_string(),
        },
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => match path() {
            Some(path) => format!("Editing {path}"),
            None => "Editing a file".to_string(),
        },
        "Read" => match path() {
            Some(path) => format!("Reading {path}"),
            None => "Reading a file".to_string(),
        },
        "Grep" | "Glob" => match field("pattern") {
            Some(pattern) => format!("Searching for {}", quoted(pattern)),
            None => "Searching".to_string(),
        },
        "Agent" | "Task" => match field("description").or_else(|| field("subagent_type")) {
            Some(description) => {
                format!("Subagent: {}", one_line(description, STEP_DETAIL_CHARS))
            }
            None => "Subagent".to_string(),
        },
        "WebSearch" => match field("query") {
            Some(query) => format!("Searching the web for {}", quoted(query)),
            None => "Searching the web".to_string(),
        },
        "WebFetch" => match field("url") {
            Some(url) => format!("Fetching {}", one_line(url, STEP_DETAIL_CHARS)),
            None => "Fetching a page".to_string(),
        },
        "TodoWrite" => "Updating the plan".to_string(),
        // An ACP agent's shell call carries its command line.
        _ => match field("command") {
            Some(command) => format!("Running {}", quoted(command)),
            None => format!("Using {}", one_line(name, STEP_DETAIL_CHARS)),
        },
    }
}

fn first_line_raw(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// A permission prompt or question in plain words.
pub(crate) fn describe_ask(kind: &RuntimeRequestKind, cwd: Option<&Path>) -> String {
    match kind {
        RuntimeRequestKind::Permission {
            tool_name, input, ..
        } => format!("Permission: {}", describe_tool(tool_name, input, cwd)),
        RuntimeRequestKind::Question { questions } => {
            let list = questions.as_array();
            let first = list
                .and_then(|list| list.first())
                .and_then(|q| q.get("question"))
                .and_then(Value::as_str)
                .map(|q| one_line(q, LINE_CHARS));
            let more = list.map_or(0, |list| list.len().saturating_sub(1));
            match (first, more) {
                (Some(question), 0) => format!("Question: {question}"),
                (Some(question), more) => format!("Question: {question} (+{more} more)"),
                (None, _) => "A question for you".to_string(),
            }
        }
    }
}

/// An RFC 3339 timestamp as a `SystemTime`.
fn parse_time(at: &str) -> Option<SystemTime> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    let ms = u64::try_from(at.timestamp_millis()).ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(ms))
}

/// A delegation as a child row. `task_title` names a worker's task.
pub(crate) fn delegation_activity(
    delegation: &Delegation,
    task_title: Option<&str>,
    target: NavTarget,
) -> ChildActivity {
    let brief = || one_line(first_line_raw(&delegation.brief), LINE_CHARS);
    let (kind, label) = match delegation.kind {
        DelegationKind::Review => {
            let what = match delegation.target.as_ref().map(|target| &target.mode) {
                Some(ReviewTargetMode::Uncommitted) | None => "uncommitted changes".to_string(),
                Some(ReviewTargetMode::Base { reference }) => format!("changes vs {reference}"),
                Some(ReviewTargetMode::Commit { sha }) => {
                    format!("commit {}", sha.chars().take(7).collect::<String>())
                }
            };
            (ChildKind::CodexReview, format!("Codex review · {what}"))
        }
        DelegationKind::Consult => (
            ChildKind::CodexConsult,
            format!("Codex consult · {}", brief()),
        ),
        DelegationKind::Implement => (
            ChildKind::Worker,
            format!(
                "Worker · {}",
                task_title.map_or_else(brief, |title| one_line(title, LINE_CHARS))
            ),
        ),
    };
    let status = match &delegation.status {
        DelegationStatus::Requested => ChildStatus::Queued,
        DelegationStatus::Running => ChildStatus::Running,
        DelegationStatus::Completed => ChildStatus::Done,
        DelegationStatus::Blocked => ChildStatus::Blocked,
        DelegationStatus::Failed { .. } => ChildStatus::Failed,
        DelegationStatus::Interrupted | DelegationStatus::Cancelled => ChildStatus::Stopped,
    };
    let step = match status {
        ChildStatus::Queued => Some("Queued".to_string()),
        ChildStatus::Running => Some(
            match kind {
                ChildKind::CodexReview => "Reviewing",
                ChildKind::CodexConsult => "Thinking it over",
                _ => "Working",
            }
            .to_string(),
        ),
        _ => None,
    };
    let result = delegation.result.as_ref();
    let handoff = result.and_then(|result| result.handoff.as_ref());
    let result_line = match &delegation.status {
        DelegationStatus::Completed => Some(
            match (result.and_then(|result| result.findings.as_ref()), handoff) {
                (Some(findings), _) => {
                    let verdict = match findings.verdict {
                        ReviewVerdict::Correct => "Looks right",
                        ReviewVerdict::NeedsChanges => "Needs changes",
                        ReviewVerdict::Unknown => "Reviewed",
                    };
                    let count = findings.findings.len();
                    let summary = first_line(&findings.summary);
                    let mut line = verdict.to_string();
                    if count > 0 {
                        line.push_str(&format!(
                            " · {count} finding{}",
                            if count == 1 { "" } else { "s" }
                        ));
                    }
                    if !summary.is_empty() {
                        line.push_str(" · ");
                        line.push_str(summary);
                    }
                    one_line(&line, LINE_CHARS)
                }
                (None, Some(handoff)) if !first_line(&handoff.summary).is_empty() => {
                    one_line(first_line(&handoff.summary), LINE_CHARS)
                }
                _ => "Done".to_string(),
            },
        ),
        DelegationStatus::Blocked => {
            let why = handoff.and_then(|handoff| {
                handoff
                    .blockers
                    .first()
                    .map(String::as_str)
                    .or(Some(first_line(&handoff.summary)))
                    .filter(|s| !s.trim().is_empty())
            });
            Some(one_line(
                &format!("Blocked: {}", why.unwrap_or("needs you")),
                LINE_CHARS,
            ))
        }
        DelegationStatus::Failed { message } => Some(one_line(
            &format!("Failed: {}", first_line(message)),
            LINE_CHARS,
        )),
        DelegationStatus::Interrupted => Some("Interrupted".to_string()),
        DelegationStatus::Cancelled => Some("Cancelled".to_string()),
        DelegationStatus::Requested | DelegationStatus::Running => None,
    };
    let finished = !status.is_active();
    ChildActivity {
        kind,
        label,
        status,
        step,
        started_at: parse_time(&delegation.created_at),
        finished_at: finished
            .then(|| parse_time(&delegation.updated_at))
            .flatten(),
        result: result_line,
        target,
    }
}

/// A native chat tab's session, as the snapshot needs it.
pub(crate) struct SessionInput<'a> {
    pub(crate) harness: Harness,
    pub(crate) conversation: &'a [AgentEvent],
    /// Runtime requests the session holds open.
    pub(crate) pending_requests: usize,
    pub(crate) streaming: bool,
    pub(crate) stopped: bool,
    /// The model the tab is configured with (`None` or "default": the
    /// harness's own choice).
    pub(crate) configured_model: Option<&'a str>,
    pub(crate) live_times: &'a LiveTimes,
}

/// A terminal tab, as far as its title adapter knows it.
pub(crate) struct TerminalInput<'a> {
    pub(crate) harness: Harness,
    /// Its attention says it wants the human: the label.
    pub(crate) needs_you: Option<&'a str>,
    pub(crate) running: bool,
}

/// What a tab runs.
pub(crate) enum TabRuns<'a> {
    Chat(SessionInput<'a>),
    Terminal(TerminalInput<'a>),
}

/// One open tab, gathered by the app.
pub(crate) struct TabInput<'a> {
    pub(crate) tab_id: usize,
    pub(crate) chat: Option<ChatRef>,
    /// The Chats index's title for its conversation.
    pub(crate) index_title: Option<&'a str>,
    /// What the tab strip calls it (harness topic, else repo).
    pub(crate) display_title: &'a str,
    pub(crate) machine: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) branch: Option<&'a str>,
    /// Where the agent works; shortens paths in steps.
    pub(crate) cwd: Option<&'a Path>,
    pub(crate) runs: TabRuns<'a>,
    /// Delegations this tab requested, each with its worker's task title.
    pub(crate) delegations: Vec<(&'a Delegation, Option<String>)>,
}

/// Whether a delegation belongs in the snapshot at `now`: not dismissed,
/// and active, blocked or finished inside the window.
fn keep_delegation(delegation: &Delegation, child: &ChildActivity, now: SystemTime) -> bool {
    delegation.dismissed_at.is_none()
        && (child.status.is_active()
            || child.status == ChildStatus::Blocked
            || child.finished_at.is_some_and(|at| is_recent(at, now)))
}

/// One open tab's snapshot. A chat tab's state comes from its session (a
/// held request is Needs you, a streaming turn is Working); a terminal's
/// from its attention and title. Subagents and delegations nest under it;
/// on a chat page they open in its roster, else the row goes to the tab.
pub(crate) fn tab_snapshot(input: TabInput<'_>, now: SystemTime) -> ChatSnapshot {
    let tab_id = input.tab_id;
    let mut children = Vec::new();
    let (harness, state, activity, live_times, configured_model) = match &input.runs {
        TabRuns::Chat(session) => {
            let activity = conversation_activity(session.conversation, input.cwd);
            let state = if session.pending_requests > 0 {
                ChatState::NeedsYou {
                    what: activity
                        .asks
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "Input or approval needed".to_string()),
                }
            } else if session.streaming {
                ChatState::Working
            } else if session.stopped || activity.exited {
                ChatState::Exited
            } else {
                ChatState::Idle
            };
            for sub in &activity.subagents {
                let finished_at = (!sub.status.is_active())
                    .then(|| session.live_times.children_finished.get(&sub.tool_use_id))
                    .flatten()
                    .copied();
                // Subagents read back from a transcript carry no times;
                // only running or live-finished ones are news.
                if !sub.status.is_active() && finished_at.is_none() {
                    continue;
                }
                children.push(ChildActivity {
                    kind: ChildKind::Subagent,
                    label: sub.label.clone(),
                    status: sub.status,
                    step: sub.step.clone(),
                    started_at: sub.started_at,
                    finished_at,
                    result: None,
                    target: NavTarget::Roster {
                        tab_id,
                        key: format!("sub:{}", sub.tool_use_id),
                    },
                });
            }
            (
                session.harness,
                state,
                Some(activity),
                Some(session.live_times),
                session.configured_model,
            )
        }
        TabRuns::Terminal(terminal) => {
            let state = match terminal.needs_you {
                Some(what) => ChatState::NeedsYou {
                    what: what.to_string(),
                },
                None if terminal.running => ChatState::Working,
                None => ChatState::Idle,
            };
            (terminal.harness, state, None, None, None)
        }
    };
    let chat_page = matches!(input.runs, TabRuns::Chat(_));
    for (delegation, task_title) in &input.delegations {
        let target = if chat_page {
            NavTarget::Roster {
                tab_id,
                key: format!("dlg:{}", delegation.delegation_id),
            }
        } else {
            NavTarget::Tab(tab_id)
        };
        let child = delegation_activity(delegation, task_title.as_deref(), target);
        if keep_delegation(delegation, &child, now) {
            children.push(child);
        }
    }
    let working = state == ChatState::Working;
    let last_prompt = match &input.runs {
        TabRuns::Chat(session) => session
            .conversation
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::Other(value)
                    if value.get("type").and_then(Value::as_str) == Some("user_prompt") =>
                {
                    value.get("text").and_then(Value::as_str)
                }
                _ => None,
            }),
        TabRuns::Terminal(_) => None,
    };
    let title = input
        .index_title
        .map(str::to_string)
        .or_else(|| {
            last_prompt
                .map(|prompt| one_line(prompt, 80))
                .filter(|prompt| !prompt.is_empty())
        })
        .unwrap_or_else(|| input.display_title.to_string());
    let model = activity
        .as_ref()
        .and_then(|activity| activity.model.clone())
        .or_else(|| {
            configured_model
                .map(str::trim)
                .filter(|model| !model.is_empty() && *model != "default")
                .map(str::to_string)
        });
    let last_finished = activity.as_ref().and_then(|activity| {
        activity.last_turn.as_ref().map(|turn| FinishedTurn {
            at: live_times.and_then(|times| times.turn_finished),
            result: turn.result.clone(),
            failed: turn.failed,
            cost_usd: turn.cost_usd,
        })
    });
    ChatSnapshot {
        chat: input.chat,
        title,
        machine: input.machine.to_string(),
        repo: input.repo.to_string(),
        branch: input.branch.map(str::to_string),
        harness,
        model,
        step: if working {
            activity.as_ref().and_then(|activity| activity.step.clone())
        } else {
            None
        },
        turn_started_at: if working {
            live_times.and_then(|times| times.turn_started)
        } else {
            None
        },
        cost_usd: activity.as_ref().and_then(|activity| activity.cost_usd),
        state,
        children,
        last_finished,
        target: NavTarget::Tab(tab_id),
    }
}

/// How a remote agent reports a persistent Claude chat it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteChatState {
    Idle,
    TurnRunning,
    AwaitingInput,
}

/// A persistent chat a remote runs that no tab holds (TRU-152). Its row
/// opens it through the Open Chat attach path; without a Claude session id
/// yet there is nothing to attach by.
pub(crate) fn remote_chat_snapshot(
    chat: Option<ChatRef>,
    index_title: Option<&str>,
    cwd: &str,
    machine: &str,
    branch: Option<&str>,
    state: RemoteChatState,
) -> ChatSnapshot {
    let cwd_name = Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string());
    ChatSnapshot {
        title: index_title.map_or_else(|| cwd_name.clone(), str::to_string),
        machine: machine.to_string(),
        repo: cwd_name,
        branch: branch.map(str::to_string),
        harness: Harness::Claude,
        model: None,
        state: match state {
            RemoteChatState::TurnRunning => ChatState::Working,
            // The ask itself is in the chat; the agent only says it waits.
            RemoteChatState::AwaitingInput => ChatState::NeedsYou {
                what: "Input or approval needed".to_string(),
            },
            RemoteChatState::Idle => ChatState::Idle,
        },
        step: None,
        turn_started_at: None,
        cost_usd: None,
        children: Vec::new(),
        last_finished: None,
        target: chat.clone().map_or(NavTarget::Nowhere, NavTarget::OpenChat),
        chat,
    }
}

/// A delegation whose requesting chat is not open. A worker goes to its
/// task; a review or consult reopens the requesting conversation when the
/// Chats index knows it (`parent_chat`: its ref and title). `None` when it
/// does not belong in the snapshot at `now`.
pub(crate) fn detached_activity(
    delegation: &Delegation,
    task_title: Option<&str>,
    parent_chat: Option<(ChatRef, &str)>,
    machine: &str,
    now: SystemTime,
) -> Option<DetachedActivity> {
    let target = match (&delegation.child, &parent_chat) {
        (DelegationChild::TaskSession { task_id, .. }, _) => NavTarget::Task(task_id.clone()),
        (_, Some((chat, _))) => NavTarget::OpenChat(chat.clone()),
        _ => NavTarget::Nowhere,
    };
    let child = delegation_activity(delegation, task_title, target);
    keep_delegation(delegation, &child, now).then(|| DetachedActivity {
        parent: parent_chat
            .as_ref()
            .map(|(_, title)| title.to_string())
            .unwrap_or_else(|| delegation.parent.workspace.clone()),
        parent_chat: parent_chat.map(|(chat, _)| chat),
        machine: machine.to_string(),
        child,
    })
}

/// Inside the "Recently finished" window at `now`.
fn is_recent(at: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(at) {
        Ok(age) => age <= RECENT_WINDOW,
        // A clock a little ahead (another machine's) still counts.
        Err(_) => true,
    }
}

/// One "Needs you" row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NeedsYouRow<'a> {
    pub(crate) title: &'a str,
    /// The chat a blocked worker belongs to.
    pub(crate) parent: Option<&'a str>,
    pub(crate) machine: &'a str,
    pub(crate) what: &'a str,
    pub(crate) harness: Option<Harness>,
    pub(crate) target: &'a NavTarget,
}

/// One "Working now" row: a chat (its children nest under it) or a
/// delegation whose parent chat is not open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WorkingRow<'a> {
    Chat(&'a ChatSnapshot),
    Detached(&'a DetachedActivity),
}

/// One "Recently finished" row: a chat's turn or a child's run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FinishedRow<'a> {
    pub(crate) title: &'a str,
    /// The chat a child worked for.
    pub(crate) parent: Option<&'a str>,
    pub(crate) machine: &'a str,
    pub(crate) at: SystemTime,
    pub(crate) result: &'a str,
    pub(crate) failed: bool,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) target: &'a NavTarget,
}

/// The overview's sections, top to bottom.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Sections<'a> {
    pub(crate) needs_you: Vec<NeedsYouRow<'a>>,
    pub(crate) working: Vec<WorkingRow<'a>>,
    pub(crate) finished: Vec<FinishedRow<'a>>,
    pub(crate) idle: Vec<&'a ChatSnapshot>,
}

/// Split a snapshot into sections at `now`. Each chat is in exactly one of
/// Needs you / Working now / Idle; a blocked worker adds a Needs you row
/// of its own; finished turns and children from the last
/// [`RECENT_WINDOW`] are listed newest first.
pub(crate) fn sections(snapshot: &OverviewSnapshot, now: SystemTime) -> Sections<'_> {
    let mut out = Sections::default();
    let failed = |status: ChildStatus| matches!(status, ChildStatus::Failed | ChildStatus::Stopped);
    for chat in &snapshot.chats {
        match &chat.state {
            ChatState::NeedsYou { what } => out.needs_you.push(NeedsYouRow {
                title: &chat.title,
                parent: None,
                machine: &chat.machine,
                what,
                harness: Some(chat.harness),
                target: &chat.target,
            }),
            ChatState::Working => out.working.push(WorkingRow::Chat(chat)),
            ChatState::Idle | ChatState::Exited => out.idle.push(chat),
        }
        if let Some(turn) = &chat.last_finished {
            if let Some(at) = turn.at.filter(|at| is_recent(*at, now)) {
                out.finished.push(FinishedRow {
                    title: &chat.title,
                    parent: None,
                    machine: &chat.machine,
                    at,
                    result: &turn.result,
                    failed: turn.failed,
                    cost_usd: turn.cost_usd,
                    target: &chat.target,
                });
            }
        }
        for child in &chat.children {
            if child.status == ChildStatus::Blocked {
                out.needs_you.push(NeedsYouRow {
                    title: &child.label,
                    parent: Some(&chat.title),
                    machine: &chat.machine,
                    what: child.result.as_deref().unwrap_or("Blocked"),
                    harness: None,
                    target: &child.target,
                });
            }
            // A blocked worker is listed under Needs you instead.
            let finished_at = child
                .finished_at
                .filter(|at| child.status != ChildStatus::Blocked && is_recent(*at, now));
            if let Some(at) = finished_at {
                out.finished.push(FinishedRow {
                    title: &child.label,
                    parent: Some(&chat.title),
                    machine: &chat.machine,
                    at,
                    result: child.result.as_deref().unwrap_or(child.status.label()),
                    failed: failed(child.status),
                    cost_usd: None,
                    target: &child.target,
                });
            }
        }
    }
    for detached in &snapshot.detached {
        let child = &detached.child;
        if child.status.is_active() {
            out.working.push(WorkingRow::Detached(detached));
        }
        if child.status == ChildStatus::Blocked {
            out.needs_you.push(NeedsYouRow {
                title: &child.label,
                parent: Some(&detached.parent),
                machine: &detached.machine,
                what: child.result.as_deref().unwrap_or("Blocked"),
                harness: None,
                target: &child.target,
            });
        }
        let finished_at = child
            .finished_at
            .filter(|at| child.status != ChildStatus::Blocked && is_recent(*at, now));
        if let Some(at) = finished_at {
            out.finished.push(FinishedRow {
                title: &child.label,
                parent: Some(&detached.parent),
                machine: &detached.machine,
                at,
                result: child.result.as_deref().unwrap_or(child.status.label()),
                failed: failed(child.status),
                cost_usd: None,
                target: &child.target,
            });
        }
    }
    // Longest-running first; rows without a start keep their order last.
    out.working.sort_by_key(|row| {
        let started = match row {
            WorkingRow::Chat(chat) => chat.turn_started_at,
            WorkingRow::Detached(detached) => detached.child.started_at,
        };
        (started.is_none(), started)
    });
    out.finished.sort_by_key(|row| std::cmp::Reverse(row.at));
    out
}

/// Elapsed time as the overview shows it: "42s", "3m 12s", "1h 4m".
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// A cost in dollars: two decimals, or "<$0.01".
pub(crate) fn format_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.01 {
        "<$0.01".to_string()
    } else {
        format!("${usd:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm::tasks::{DelegationChild, DelegationParent, DelegationResult, TaskHandoff};
    use serde_json::json;
    use std::path::PathBuf;

    fn h(event: HarnessEvent) -> AgentEvent {
        AgentEvent::Harness(event)
    }

    fn turn_started(model: Option<&str>) -> AgentEvent {
        h(HarnessEvent::TurnStarted {
            session_id: Some("s1".into()),
            model: model.map(str::to_string),
        })
    }

    fn tool(id: &str, name: &str, input: Value) -> AgentEvent {
        h(HarnessEvent::ItemStarted {
            id: id.into(),
            kind: ItemKind::ToolCall {
                name: name.into(),
                input,
            },
        })
    }

    fn done(id: &str) -> AgentEvent {
        h(HarnessEvent::ItemCompleted {
            id: id.into(),
            output: String::new(),
            is_error: false,
        })
    }

    fn completed(cost: Option<f64>) -> AgentEvent {
        h(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Value::Null,
            cost_usd: cost,
        })
    }

    fn sub(parent: &str, event: HarnessEvent) -> AgentEvent {
        h(HarnessEvent::SubagentEvent {
            parent_tool_use_id: parent.into(),
            event: Box::new(event),
        })
    }

    #[test]
    fn describe_tool_names_common_tools_in_plain_words() {
        let cwd = Path::new("/repo");
        let cases = [
            (
                "Bash",
                json!({"command": "cargo test --features x"}),
                "Running `cargo test --features x`",
            ),
            (
                "Bash",
                json!({"command": "  \ncd src\nls"}),
                "Running `cd src`",
            ),
            ("Bash", json!({}), "Running a command"),
            (
                "Edit",
                json!({"file_path": "/repo/src/main.rs"}),
                "Editing src/main.rs",
            ),
            (
                "Write",
                json!({"file_path": "/elsewhere/deep/dir/notes.md"}),
                "Editing dir/notes.md",
            ),
            (
                "Read",
                json!({"file_path": "/repo/Cargo.toml"}),
                "Reading Cargo.toml",
            ),
            (
                "Grep",
                json!({"pattern": "fn main"}),
                "Searching for `fn main`",
            ),
            (
                "Glob",
                json!({"pattern": "**/*.rs"}),
                "Searching for `**/*.rs`",
            ),
            (
                "Agent",
                json!({"description": "find callers", "subagent_type": "Explore"}),
                "Subagent: find callers",
            ),
            ("Task", json!({"subagent_type": "Plan"}), "Subagent: Plan"),
            (
                "mcp__gitterm_tasks__review_request",
                json!({}),
                "Waiting for Codex review",
            ),
            (
                "mcp__gitterm_tasks__delegate_task",
                json!({"title": "Fix the parser"}),
                "Delegating: Fix the parser",
            ),
            (
                "mcp__gitterm_browser__browser_click",
                json!({}),
                "Using browser_click",
            ),
            (
                "execute",
                json!({"command": "npm test"}),
                "Running `npm test`",
            ),
            ("Frobnicate", json!({}), "Using Frobnicate"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(
                describe_tool(name, &input, Some(cwd)),
                expected,
                "{name} {input}"
            );
        }
        let long = "x".repeat(200);
        let step = describe_tool("Bash", &json!({ "command": long }), None);
        assert!(step.chars().count() < 80, "{step}");
        assert!(step.ends_with("…`"), "{step}");
    }

    #[test]
    fn step_follows_the_latest_open_activity() {
        let cwd = Some(Path::new("/repo"));
        let mut conversation = vec![turn_started(Some("claude-opus"))];
        assert_eq!(
            conversation_activity(&conversation, cwd).step.as_deref(),
            Some("Working")
        );
        conversation.push(h(HarnessEvent::ThinkingDelta("hmm".into())));
        assert_eq!(
            conversation_activity(&conversation, cwd).step.as_deref(),
            Some("Thinking")
        );
        // Claude streams a tool's input after announcing the call.
        conversation.push(tool("t1", "Bash", json!({})));
        assert_eq!(
            conversation_activity(&conversation, cwd).step.as_deref(),
            Some("Running a command")
        );
        conversation.push(h(HarnessEvent::ItemInputDelta {
            id: "t1".into(),
            partial_json: r#"{"command":"cargo"#.into(),
        }));
        conversation.push(h(HarnessEvent::ItemInputDelta {
            id: "t1".into(),
            partial_json: r#" test"}"#.into(),
        }));
        let activity = conversation_activity(&conversation, cwd);
        assert_eq!(activity.step.as_deref(), Some("Running `cargo test`"));
        assert_eq!(activity.model.as_deref(), Some("claude-opus"));
        assert!(activity.turn_open);
        conversation.push(done("t1"));
        conversation.push(h(HarnessEvent::TextDelta("All green.".into())));
        assert_eq!(
            conversation_activity(&conversation, cwd).step.as_deref(),
            Some("Writing a reply")
        );
        conversation.push(completed(Some(0.5)));
        let activity = conversation_activity(&conversation, cwd);
        assert!(!activity.turn_open);
        assert_eq!(activity.step, None);
    }

    #[test]
    fn asks_and_results_and_cost_come_from_the_conversation() {
        let mut conversation = vec![
            turn_started(None),
            h(HarnessEvent::TextDelta("Looking.".into())),
            tool("t1", "Read", json!({"file_path": "/repo/a.rs"})),
            done("t1"),
            h(HarnessEvent::TextDelta(
                "## Fixed the bug\n\nDetails…".into(),
            )),
            completed(Some(0.25)),
            turn_started(None),
            h(HarnessEvent::RuntimeRequest {
                request_id: "r1".into(),
                tool_use_id: "t2".into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: json!({"command": "rm -rf target"}),
                    suggestions: Vec::new(),
                    decision_reason: None,
                },
            }),
        ];
        let activity = conversation_activity(&conversation, None);
        assert_eq!(activity.asks, vec!["Permission: Running `rm -rf target`"]);
        let last = activity.last_turn.expect("a finished turn");
        assert_eq!(last.result, "Fixed the bug");
        assert_eq!(last.cost_usd, Some(0.25));
        assert_eq!(activity.cost_usd, Some(0.25));

        conversation.push(h(HarnessEvent::RuntimeRequestResolved {
            request_id: "r1".into(),
        }));
        conversation.push(h(HarnessEvent::RuntimeRequest {
            request_id: "r2".into(),
            tool_use_id: "t3".into(),
            kind: RuntimeRequestKind::Question {
                questions: json!([{"question": "Which branch?"}, {"question": "Push?"}]),
            },
        }));
        assert_eq!(
            conversation_activity(&conversation, None).asks,
            vec!["Question: Which branch? (+1 more)"]
        );
        // Cumulative cost: the second turn cost the difference.
        conversation.push(completed(Some(0.75)));
        let activity = conversation_activity(&conversation, None);
        assert!(activity.asks.is_empty());
        assert_eq!(activity.cost_usd, Some(0.75));
        assert_eq!(activity.last_turn.unwrap().cost_usd, Some(0.5));

        let failed = vec![
            turn_started(None),
            h(HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed("rate limited\nretry later".into()),
                usage: Value::Null,
                cost_usd: None,
            }),
        ];
        let last = conversation_activity(&failed, None).last_turn.unwrap();
        assert!(last.failed);
        assert_eq!(last.result, "Failed: rate limited");

        let exited = vec![
            turn_started(None),
            h(HarnessEvent::ProcessExited { code: Some(1) }),
        ];
        let activity = conversation_activity(&exited, None);
        assert!(activity.exited);
        assert!(!activity.turn_open);
    }

    #[test]
    fn subagents_nest_with_their_own_status_step_and_start() {
        let conversation = vec![
            turn_started(None),
            tool("a1", "Agent", json!({})),
            h(HarnessEvent::ItemInputDelta {
                id: "a1".into(),
                partial_json: r#"{"description":"map the code","subagent_type":"Explore"}"#.into(),
            }),
            sub(
                "a1",
                HarnessEvent::TurnStarted {
                    session_id: None,
                    model: None,
                },
            ),
            AgentEvent::Other(json!({"kind": "subagent_started_at", "id": "a1", "at_ms": 1_000})),
            sub(
                "a1",
                HarnessEvent::ItemStarted {
                    id: "s1".into(),
                    kind: ItemKind::ToolCall {
                        name: "Grep".into(),
                        input: json!({"pattern": "TabKind"}),
                    },
                },
            ),
            tool("a2", "Agent", json!({"description": "write tests"})),
            sub(
                "a2",
                HarnessEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: Value::Null,
                    cost_usd: None,
                },
            ),
            // A subagent's own subagent rolls up; its end is not a2's.
            sub(
                "s1",
                HarnessEvent::SubagentEvent {
                    parent_tool_use_id: "s1".into(),
                    event: Box::new(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Completed,
                        usage: Value::Null,
                        cost_usd: None,
                    }),
                },
            ),
        ];
        let activity = conversation_activity(&conversation, None);
        assert_eq!(activity.subagents.len(), 2);
        let first = &activity.subagents[0];
        assert_eq!(first.label, "Explore · map the code");
        assert_eq!(first.status, ChildStatus::Running);
        assert_eq!(first.step.as_deref(), Some("Searching for `TabKind`"));
        assert_eq!(first.started_at, Some(UNIX_EPOCH + Duration::from_secs(1)));
        let second = &activity.subagents[1];
        assert_eq!(second.status, ChildStatus::Done);
        assert_eq!(second.step, None);
        // The chat's own step is its latest open call.
        assert_eq!(activity.step.as_deref(), Some("Subagent: write tests"));
    }

    #[test]
    fn a_background_subagent_runs_on_after_its_call_returns() {
        let mut conversation = vec![
            turn_started(None),
            tool("a1", "Agent", json!({"description": "bg"})),
            done("a1"),
        ];
        // The call returned and nothing followed: a foreground subagent
        // that finished.
        assert_eq!(
            conversation_activity(&conversation, None).subagents[0].status,
            ChildStatus::Done
        );
        conversation.push(sub("a1", HarnessEvent::TextDelta("still going".into())));
        let activity = conversation_activity(&conversation, None);
        assert_eq!(activity.subagents[0].status, ChildStatus::Running);
        assert_eq!(
            activity.subagents[0].step.as_deref(),
            Some("Writing a reply")
        );
    }

    #[test]
    fn live_times_stamp_turns_and_subagent_ends() {
        let t0 = UNIX_EPOCH + Duration::from_secs(100);
        let t1 = UNIX_EPOCH + Duration::from_secs(160);
        let mut times = LiveTimes::default();
        times.observe(&turn_started(None), t0);
        // A second start inside the same turn keeps the first stamp.
        times.observe(&turn_started(None), t1);
        assert_eq!(times.turn_started, Some(t0));
        times.observe(&tool("a1", "Agent", json!({})), t0);
        times.observe(&done("a1"), t0);
        times.observe(
            &sub(
                "a1",
                HarnessEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: Value::Null,
                    cost_usd: None,
                },
            ),
            t1,
        );
        assert_eq!(times.children_finished.get("a1"), Some(&t1));
        times.observe(&completed(None), t1);
        assert_eq!(times.turn_finished, Some(t1));
        times.observe(&turn_started(None), t1);
        assert_eq!(times.turn_started, Some(t1));
    }

    #[test]
    fn terminal_harness_reads_registry_command_then_title() {
        assert_eq!(
            terminal_harness(Some(ChatBackend::Codex), Some("claude"), None),
            Harness::CodexCli
        );
        assert_eq!(
            terminal_harness(None, Some("/opt/bin/claude --resume abc"), None),
            Harness::ClaudeCli
        );
        assert_eq!(
            terminal_harness(None, Some("codex"), None),
            Harness::CodexCli
        );
        assert_eq!(
            terminal_harness(None, None, Some("✳ Fix tests")),
            Harness::ClaudeCli
        );
        assert_eq!(
            terminal_harness(None, None, Some("codex — repo")),
            Harness::CodexCli
        );
        assert_eq!(terminal_harness(None, None, Some("zsh")), Harness::Terminal);
        assert_eq!(terminal_harness(None, None, None), Harness::Terminal);
    }

    fn delegation(kind: DelegationKind, status: DelegationStatus) -> Delegation {
        Delegation {
            delegation_id: "d1".into(),
            kind,
            parent: DelegationParent {
                session_uid: "uid".into(),
                chat_session_id: None,
                workspace: "ws".into(),
                cwd: PathBuf::from("/repo"),
            },
            child: DelegationChild::CodexReview {
                thread_id: None,
                model: None,
            },
            brief: "Check the parser\nmore".into(),
            target: None,
            previous: None,
            status,
            result: None,
            created_at: "2026-10-20T10:00:00Z".into(),
            updated_at: "2026-10-20T10:05:00Z".into(),
            delivered_at: None,
            dismissed_at: None,
        }
    }

    #[test]
    fn delegations_become_children_with_status_times_and_result() {
        let target = NavTarget::Roster {
            tab_id: 3,
            key: "dlg:d1".into(),
        };
        let running = delegation_activity(
            &delegation(DelegationKind::Review, DelegationStatus::Running),
            None,
            target.clone(),
        );
        assert_eq!(running.kind, ChildKind::CodexReview);
        assert_eq!(running.label, "Codex review · uncommitted changes");
        assert_eq!(running.status, ChildStatus::Running);
        assert_eq!(running.step.as_deref(), Some("Reviewing"));
        assert_eq!(running.finished_at, None);
        assert_eq!(running.target, target);
        assert!(running.started_at.is_some());

        let mut blocked = delegation(DelegationKind::Implement, DelegationStatus::Blocked);
        blocked.result = Some(DelegationResult {
            handoff: Some(TaskHandoff {
                summary: "Stopped".into(),
                decisions: Vec::new(),
                next_steps: Vec::new(),
                blockers: vec!["Needs the API key".into()],
                updated_by_session_id: None,
                updated_at: "2026-10-20T10:05:00Z".into(),
            }),
            findings: None,
            reviewed: None,
        });
        let blocked = delegation_activity(&blocked, Some("TRU-9 Wire it"), NavTarget::Nowhere);
        assert_eq!(blocked.label, "Worker · TRU-9 Wire it");
        assert_eq!(blocked.status, ChildStatus::Blocked);
        assert_eq!(
            blocked.result.as_deref(),
            Some("Blocked: Needs the API key")
        );
        assert!(blocked.finished_at.is_some());

        let failed = delegation_activity(
            &delegation(
                DelegationKind::Consult,
                DelegationStatus::Failed {
                    message: "codex exited 1".into(),
                },
            ),
            None,
            NavTarget::Nowhere,
        );
        assert_eq!(failed.label, "Codex consult · Check the parser");
        assert_eq!(failed.result.as_deref(), Some("Failed: codex exited 1"));
    }

    fn chat(title: &str, state: ChatState) -> ChatSnapshot {
        ChatSnapshot {
            chat: Some(ChatRef::local(title)),
            title: title.into(),
            machine: "This Mac".into(),
            repo: "repo".into(),
            branch: None,
            harness: Harness::Claude,
            model: None,
            state,
            step: None,
            turn_started_at: None,
            cost_usd: None,
            children: Vec::new(),
            last_finished: None,
            target: NavTarget::Tab(1),
        }
    }

    fn child(status: ChildStatus, finished_at: Option<SystemTime>) -> ChildActivity {
        ChildActivity {
            kind: ChildKind::Worker,
            label: "Worker · w".into(),
            status,
            step: None,
            started_at: None,
            finished_at,
            result: Some("Blocked: key".into()),
            target: NavTarget::Roster {
                tab_id: 1,
                key: "dlg:x".into(),
            },
        }
    }

    #[test]
    fn sections_split_states_window_and_blocked_workers() {
        let now = UNIX_EPOCH + Duration::from_secs(10 * 3600);
        let hour = Duration::from_secs(3600);
        let mut working = chat("working", ChatState::Working);
        working.turn_started_at = Some(now - hour);
        working.children = vec![
            child(ChildStatus::Blocked, Some(now - hour)),
            ChildActivity {
                label: "Codex review".into(),
                result: Some("Looks right".into()),
                ..child(ChildStatus::Done, Some(now - hour / 2))
            },
        ];
        let mut earlier = chat("earlier", ChatState::Working);
        earlier.turn_started_at = Some(now - 2 * hour);
        let mut idle = chat("idle", ChatState::Idle);
        idle.last_finished = Some(FinishedTurn {
            at: Some(now - 3 * hour),
            result: "did it".into(),
            failed: false,
            cost_usd: Some(0.1),
        });
        let mut old = chat("old", ChatState::Exited);
        old.last_finished = Some(FinishedTurn {
            at: Some(now - 5 * hour),
            result: "long ago".into(),
            failed: false,
            cost_usd: None,
        });
        let asking = chat(
            "asking",
            ChatState::NeedsYou {
                what: "Permission: Running `ls`".into(),
            },
        );
        let snapshot = OverviewSnapshot {
            chats: vec![working, earlier, idle, old, asking],
            detached: vec![DetachedActivity {
                parent: "closed chat".into(),
                parent_chat: None,
                machine: "This Mac".into(),
                child: ChildActivity {
                    status: ChildStatus::Running,
                    finished_at: None,
                    ..child(ChildStatus::Running, None)
                },
            }],
        };
        let out = sections(&snapshot, now);
        let needs: Vec<_> = out
            .needs_you
            .iter()
            .map(|row| (row.title, row.what))
            .collect();
        assert_eq!(
            needs,
            vec![
                ("Worker · w", "Blocked: key"),
                ("asking", "Permission: Running `ls`"),
            ]
        );
        assert_eq!(out.needs_you[0].parent, Some("working"));
        let working: Vec<_> = out
            .working
            .iter()
            .map(|row| match row {
                WorkingRow::Chat(chat) => chat.title.as_str(),
                WorkingRow::Detached(detached) => detached.parent.as_str(),
            })
            .collect();
        assert_eq!(working, vec!["earlier", "working", "closed chat"]);
        let finished: Vec<_> = out.finished.iter().map(|row| row.title).collect();
        // Newest first; "old" is outside the four-hour window and the
        // blocked worker is under Needs you.
        assert_eq!(finished, vec!["Codex review", "idle"]);
        assert_eq!(out.finished[0].parent, Some("working"));
        let idle: Vec<_> = out.idle.iter().map(|chat| chat.title.as_str()).collect();
        assert_eq!(idle, vec!["idle", "old"]);
    }

    fn tab_input<'a>(
        runs: TabRuns<'a>,
        delegations: Vec<(&'a Delegation, Option<String>)>,
    ) -> TabInput<'a> {
        TabInput {
            tab_id: 7,
            chat: Some(ChatRef::local("s1")),
            index_title: None,
            display_title: "repo",
            machine: "This Mac",
            repo: "repo",
            branch: Some("main"),
            cwd: Some(Path::new("/repo")),
            runs,
            delegations,
        }
    }

    #[test]
    fn a_chat_tab_snapshot_carries_state_children_and_where_to_go() {
        let now = UNIX_EPOCH + Duration::from_secs(1_792_526_400);
        let mut times = LiveTimes::default();
        let conversation = vec![
            AgentEvent::user_prompt("Fix the flaky test"),
            turn_started(None),
            tool("a1", "Agent", json!({"description": "find it"})),
            sub(
                "a1",
                HarnessEvent::TurnStarted {
                    session_id: None,
                    model: None,
                },
            ),
            tool("b1", "Bash", json!({"command": "cargo test"})),
        ];
        for event in &conversation {
            times.observe(event, now - Duration::from_secs(90));
        }
        let review = delegation(DelegationKind::Review, DelegationStatus::Running);
        let mut dismissed = delegation(DelegationKind::Consult, DelegationStatus::Completed);
        dismissed.delegation_id = "d2".into();
        dismissed.dismissed_at = Some("2026-10-20T10:06:00Z".into());
        let session = |pending| {
            TabRuns::Chat(SessionInput {
                harness: Harness::Claude,
                conversation: &conversation,
                pending_requests: pending,
                streaming: true,
                stopped: false,
                configured_model: Some("sonnet"),
                live_times: &times,
            })
        };
        let snapshot = tab_snapshot(
            tab_input(session(0), vec![(&review, None), (&dismissed, None)]),
            now,
        );
        assert_eq!(snapshot.state, ChatState::Working);
        assert_eq!(snapshot.title, "Fix the flaky test");
        assert_eq!(snapshot.model.as_deref(), Some("sonnet"));
        assert_eq!(snapshot.step.as_deref(), Some("Running `cargo test`"));
        assert_eq!(
            snapshot.turn_started_at,
            Some(now - Duration::from_secs(90))
        );
        assert_eq!(snapshot.target, NavTarget::Tab(7));
        let targets: Vec<_> = snapshot.children.iter().map(|c| &c.target).collect();
        assert_eq!(
            targets,
            vec![
                &NavTarget::Roster {
                    tab_id: 7,
                    key: "sub:a1".into()
                },
                &NavTarget::Roster {
                    tab_id: 7,
                    key: "dlg:d1".into()
                },
            ]
        );

        // A held request is Needs you, whatever else runs.
        let asking = tab_snapshot(tab_input(session(1), Vec::new()), now);
        assert_eq!(
            asking.state,
            ChatState::NeedsYou {
                what: "Input or approval needed".into()
            }
        );
        assert_eq!(asking.step, None);
    }

    #[test]
    fn a_terminal_tab_snapshot_reads_its_adapter() {
        let now = UNIX_EPOCH + Duration::from_secs(1_792_526_400);
        let review = delegation(DelegationKind::Review, DelegationStatus::Running);
        let terminal = |needs_you, running| {
            TabRuns::Terminal(TerminalInput {
                harness: Harness::ClaudeCli,
                needs_you,
                running,
            })
        };
        let waiting = tab_snapshot(
            tab_input(
                terminal(Some("Input or approval needed"), false),
                vec![(&review, None)],
            ),
            now,
        );
        assert_eq!(waiting.harness, Harness::ClaudeCli);
        assert_eq!(waiting.title, "repo");
        assert!(matches!(waiting.state, ChatState::NeedsYou { .. }));
        // No chat page: the delegation row goes to the tab itself.
        assert_eq!(waiting.children[0].target, NavTarget::Tab(7));
        let working = tab_snapshot(tab_input(terminal(None, true), Vec::new()), now);
        assert_eq!(working.state, ChatState::Working);
        assert_eq!(working.last_finished, None);
        let idle = tab_snapshot(tab_input(terminal(None, false), Vec::new()), now);
        assert_eq!(idle.state, ChatState::Idle);
    }

    #[test]
    fn remote_and_detached_rows_point_at_open_chat_or_task() {
        let chat = ChatRef::new(Some("mini"), "abc");
        let running = remote_chat_snapshot(
            Some(chat.clone()),
            Some("Port the parser"),
            "/Users/me/src/app",
            "Mac mini",
            None,
            RemoteChatState::TurnRunning,
        );
        assert_eq!(running.title, "Port the parser");
        assert_eq!(running.repo, "app");
        assert_eq!(running.state, ChatState::Working);
        assert_eq!(running.target, NavTarget::OpenChat(chat.clone()));
        let unnamed = remote_chat_snapshot(
            None,
            None,
            "/srv/app",
            "Mac mini",
            None,
            RemoteChatState::AwaitingInput,
        );
        assert_eq!(unnamed.title, "app");
        assert_eq!(unnamed.target, NavTarget::Nowhere);
        assert!(matches!(unnamed.state, ChatState::NeedsYou { .. }));

        let now = UNIX_EPOCH + Duration::from_secs(1_792_526_400);
        let mut worker = delegation(DelegationKind::Implement, DelegationStatus::Running);
        worker.child = DelegationChild::TaskSession {
            task_id: "task-1".into(),
            task_session_id: None,
            conversation: None,
            preset_name: None,
            model: None,
            role: None,
        };
        let detached =
            detached_activity(&worker, Some("Wire it"), None, "This Mac", now).expect("active");
        assert_eq!(detached.child.target, NavTarget::Task("task-1".into()));
        assert_eq!(detached.parent, "ws");
        let review = delegation(DelegationKind::Review, DelegationStatus::Running);
        let detached = detached_activity(
            &review,
            None,
            Some((ChatRef::local("p1"), "Parent chat")),
            "This Mac",
            now,
        )
        .expect("active");
        assert_eq!(
            detached.child.target,
            NavTarget::OpenChat(ChatRef::local("p1"))
        );
        assert_eq!(detached.parent, "Parent chat");
        assert_eq!(detached.parent_chat, Some(ChatRef::local("p1")));
        // Finished long before the window: not news.
        let old = delegation(DelegationKind::Review, DelegationStatus::Completed);
        assert_eq!(detached_activity(&old, None, None, "This Mac", now), None);
    }

    #[test]
    fn formats_elapsed_and_cost() {
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(192)), "3m 12s");
        assert_eq!(format_elapsed(Duration::from_secs(3840)), "1h 4m");
        assert_eq!(format_cost(0.004), "<$0.01");
        assert_eq!(format_cost(1.5), "$1.50");
    }
}
