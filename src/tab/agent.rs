//! Agent tab kind: a Claude Code or pi subprocess driving a wry-hosted chat UI.
//!
//! This module defines the data shape only. Step 3 (TRU-29) ports the spike's
//! subprocess manager (background tokio task + mpsc + stop signal) on top of these
//! types. Step 4 wires the chat UI in.
//!
//! The conversation buffer (`AgentSession::conversation`) is the source of truth:
//! the webview is reinitialized from it on tab activation, not the other way around.
//! See `.plans/agent-tab-integration.md` for the full v1 design.

// Many of these types are referenced only by future steps (3 and 4). Suppress
// dead-code warnings until those steps land — re-evaluate this attribute when
// Step 4 ships.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use gitterm::harness::claude::{ClaudeMcpServer, ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::transcript::TranscriptEntry;
use gitterm::harness::HarnessEvent;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// Permission mode for a Claude tab whose config does not name one. Always
/// passed explicitly so a user-level `defaultMode` cannot pre-empt prompts.
pub(crate) const DEFAULT_CLAUDE_PERMISSION_MODE: &str = "default";
/// Every mode the CLI accepts for `set_permission_mode` (its `invalid_mode`
/// error lists these; `claude --help` also accepts `manual`, an alias it
/// reports back as `default`).
pub(crate) const CLAUDE_PERMISSION_MODES: [&str; 6] = [
    "default",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];
/// Set to a directory to log every Claude stdin/stdout frame there.
const CLAUDE_WIRE_LOG_ENV: &str = "GITTERM_CLAUDE_WIRE_LOG_DIR";

/// Which agent backend this tab is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentBackend {
    Pi,
    Claude,
}

/// Per-backend configuration. Tagged by the `backend` discriminator so this
/// round-trips cleanly through `workspaces.json` next to the existing fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub(crate) enum AgentBackendConfig {
    Pi {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<bool>,
    },
    Claude {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
}

impl AgentBackendConfig {
    pub(crate) fn backend(&self) -> AgentBackend {
        match self {
            Self::Pi { .. } => AgentBackend::Pi,
            Self::Claude { .. } => AgentBackend::Claude,
        }
    }
}

/// High-level lifecycle of the agent subprocess as observed by the UI.
///
/// State transitions (Step 3 will own these):
/// - `Idle` → `Streaming` on first `SystemInit` after a prompt is submitted
/// - `Streaming` → `Idle` on `Result` event (turn complete)
/// - any → `Stopped` on user stop request
/// - any → `Errored` on subprocess crash or fatal parse failure
#[derive(Debug, Clone, Default)]
pub(crate) enum AgentSessionState {
    #[default]
    Idle,
    Streaming,
    Stopped,
    Errored(String),
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
// `pub` (rather than `pub(crate)`) because it's a payload of `Event::AgentEventReceived`,
// and the `Event` enum is `pub`. Binary-crate-only — no external surface.
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
    pub(crate) fn user_prompt(text: &str) -> Self {
        Self::Other(serde_json::json!({"type": "user_prompt", "text": text}))
    }

    /// The JSON the chat webview's `__appendEvent` receives. Harness events
    /// are wrapped so the page can tell them from pi's raw stream shapes.
    pub(crate) fn webview_payload(&self) -> Option<serde_json::Value> {
        match self {
            Self::Other(value) => Some(value.clone()),
            Self::Harness(ev) => match serde_json::to_value(ev) {
                Ok(event) => Some(serde_json::json!({"kind": "harness", "event": event})),
                Err(e) => {
                    eprintln!("[agent] could not serialize harness event {ev:?}: {e}");
                    None
                }
            },
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

/// Whether a resumed session's earlier timeline has been read back from
/// its transcript (Claude backend; TRU-140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryLoad {
    NotLoaded,
    Loading,
    Loaded,
}

/// Live agent-tab session state. The conversation buffer here is the source of
/// truth — the webview is reinitialized from it on tab activation.
pub(crate) struct AgentSession {
    pub(crate) config: AgentBackendConfig,
    pub(crate) conversation: Vec<AgentEvent>,
    /// Claude `--resume` ID, or pi session-file path. `None` until the first
    /// `SystemInit` event populates it.
    pub(crate) session_id: Option<String>,
    pub(crate) state: AgentSessionState,
    /// Background task that owns the subprocess, if one has been spawned.
    /// Lazy: `None` until the first prompt submit, then created and reused
    /// across turns until the tab is closed. Pi only.
    pub(crate) task_handle: Option<AgentTaskHandle>,
    /// Long-lived native Claude process (Claude backend only). Lazy like
    /// `task_handle`; dropped (and respawned with `--resume`) when the
    /// process exits. Dropping it shuts the process down.
    pub(crate) claude: Option<ClaudeSession>,
    /// Runtime requests (permission prompts, questions) awaiting the human.
    pub(crate) pending_requests: Vec<String>,
    /// Transcript read-back for a session restored from `workspaces.json`.
    pub(crate) history: HistoryLoad,
}

impl AgentSession {
    /// Build a fresh session for a backend config. Empty conversation buffer,
    /// no session_id yet, Idle, no background task spawned.
    pub(crate) fn new(config: AgentBackendConfig) -> Self {
        Self {
            config,
            conversation: Vec::new(),
            session_id: None,
            state: AgentSessionState::Idle,
            task_handle: None,
            claude: None,
            pending_requests: Vec::new(),
            history: HistoryLoad::NotLoaded,
        }
    }

    /// The session id whose transcript should be read back before this tab
    /// is shown: a resumed session whose timeline is still empty.
    pub(crate) fn history_to_load(&self) -> Option<&str> {
        (self.backend() == AgentBackend::Claude
            && self.history == HistoryLoad::NotLoaded
            && self.conversation.is_empty())
        .then_some(self.session_id.as_deref())
        .flatten()
    }

    /// Append an event to the conversation buffer, merging consecutive
    /// streaming fragments so tab switches replay a compact buffer.
    pub(crate) fn record(&mut self, ev: AgentEvent) {
        if let (Some(AgentEvent::Harness(last)), AgentEvent::Harness(next)) =
            (self.conversation.last_mut(), &ev)
        {
            if merge_fragment(last, next) {
                return;
            }
        }
        self.conversation.push(ev);
    }

    /// The Claude process settings for this tab. `model` "default" (or
    /// empty) leaves the model to the user's Claude settings. `mcp_servers`
    /// are the GitTerm MCP servers the app attaches (task and browser).
    pub(crate) fn claude_session_config(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<ClaudeMcpServer>,
    ) -> Option<ClaudeSessionConfig> {
        let AgentBackendConfig::Claude {
            model,
            permission_mode,
            effort,
        } = &self.config
        else {
            return None;
        };
        let model = model.trim();
        Some(ClaudeSessionConfig {
            cwd,
            model: (!model.is_empty() && model != "default").then(|| model.to_string()),
            permission_mode: permission_mode
                .clone()
                .unwrap_or_else(|| DEFAULT_CLAUDE_PERMISSION_MODE.to_string()),
            effort: effort.clone(),
            resume: self.session_id.clone(),
            wire_log_dir: std::env::var_os(CLAUDE_WIRE_LOG_ENV).map(PathBuf::from),
            mcp_servers,
        })
    }

    pub(crate) fn backend(&self) -> AgentBackend {
        self.config.backend()
    }

    /// The permission mode the next Claude spawn passes; `None` for pi.
    pub(crate) fn configured_permission_mode(&self) -> Option<String> {
        match &self.config {
            AgentBackendConfig::Claude {
                permission_mode, ..
            } => Some(
                permission_mode
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CLAUDE_PERMISSION_MODE.to_string()),
            ),
            AgentBackendConfig::Pi { .. } => None,
        }
    }
}

/// Fold `next` into `last` when both are fragments of the same stream:
/// text, thinking, one tool call's input, or any of those from the same
/// subagent. Returns whether `next` was absorbed.
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

// ---- Subprocess manager (Step 3 of TRU-29) -------------------------------

/// Inputs the UI can send into the agent subprocess. New variants will be added
/// as features land (e.g. `ApprovePermission { id, allowed }` once Claude's
/// permission flow ships).
#[derive(Debug)]
pub(crate) enum AgentInput {
    /// User submitted a prompt. The subprocess manager spawns one subprocess
    /// per prompt (matching the spike's per-turn pattern); multi-turn
    /// continuity is preserved via `--session` (pi) or `--resume` (Claude).
    Prompt(String),
}

/// Handle to the background tokio task that owns the agent subprocess.
/// Lives on `AgentSession.task_handle` for the lifetime of the tab.
///
/// Per-turn stop signaling: each turn parks a fresh `oneshot::Sender<()>` in
/// `stop_slot`. The UI fires it via `request_stop()`; the manager drops it
/// into the `tokio::select!` arm against subprocess output. Cleared at turn end.
///
/// `pending_event_rx` is consumed exactly once by the subscription bridge; after
/// that it stays `None` and the subscription's stream lives independently of the
/// handle. See `take_event_receiver`.
pub(crate) struct AgentTaskHandle {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    pending_event_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
}

impl AgentTaskHandle {
    /// Send a user prompt to the subprocess manager. Returns `Err` if the
    /// background task has dropped its receiver (which should only happen
    /// after a fatal error or shutdown).
    pub(crate) fn submit_prompt(&self, prompt: String) -> Result<(), String> {
        self.input_tx
            .send(AgentInput::Prompt(prompt))
            .map_err(|_| "agent subprocess manager has exited".to_string())
    }

    /// Fire the current turn's stop signal, if a turn is in flight. No-op
    /// otherwise (slot is empty between turns).
    pub(crate) fn request_stop(&self) {
        if let Ok(mut slot) = self.stop_slot.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(());
            }
        }
    }

    /// Take the event receiver for wiring into an Iced subscription. Returns
    /// `Some` exactly once per handle; subsequent calls return `None`. The
    /// caller owns the receiver after this and is responsible for keeping the
    /// subscription alive until the tab is dropped.
    pub(crate) fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.pending_event_rx.lock().ok().and_then(|mut g| g.take())
    }
}

/// Spawn the per-tab subprocess manager.
///
/// Returns the handle (parked on the session) plus the receiver side of the
/// event channel. The caller wraps the receiver in an Iced `Task::run` so
/// each `AgentEvent` becomes an `Event::AgentEventReceived(tab_id, ev)`.
///
/// Lifecycle: a dedicated thread runs a current-thread tokio runtime that
/// loops on the input channel. Each `AgentInput::Prompt` spawns one subprocess
/// turn, streams its stdout JSON, and emits a terminating `Other(...)` event
/// when the turn completes (so the UI can flip state from Streaming to Idle).
/// Closing the receiver / dropping the handle ends the task naturally.
pub(crate) fn spawn_agent_task(config: AgentBackendConfig, repo_path: PathBuf) -> AgentTaskHandle {
    let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));

    {
        let stop_slot = stop_slot.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("agent tokio runtime");
            rt.block_on(agent_loop(config, repo_path, input_rx, event_tx, stop_slot));
        });
    }

    AgentTaskHandle {
        input_tx,
        stop_slot,
        pending_event_rx: Mutex::new(Some(event_rx)),
    }
}

/// Per-turn loop: wait for a prompt, spawn the subprocess, stream events, repeat.
async fn agent_loop(
    config: AgentBackendConfig,
    repo_path: PathBuf,
    mut input_rx: mpsc::UnboundedReceiver<AgentInput>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
) {
    while let Some(input) = input_rx.recv().await {
        match input {
            AgentInput::Prompt(prompt) => {
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                if let Ok(mut g) = stop_slot.lock() {
                    *g = Some(stop_tx);
                }

                let result = run_turn(&config, &repo_path, &prompt, &event_tx, stop_rx).await;

                if let Ok(mut g) = stop_slot.lock() {
                    g.take();
                }

                if let Err(err) = result {
                    let _ = event_tx.send(AgentEvent::Other(serde_json::json!({
                        "type": "error",
                        "message": err,
                    })));
                }
            }
        }
    }
    // input channel closed: tab being dropped — exit
}

/// Spawn one subprocess turn and stream its stdout as `AgentEvent::Other(line)`
/// for each JSON line. Step 3 keeps parsing minimal — Step 4 will refine each
/// line into the typed variants (`AssistantText`, `ToolCallStart`, etc.).
async fn run_turn(
    config: &AgentBackendConfig,
    repo_path: &PathBuf,
    prompt: &str,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut cmd = build_command(config, prompt)?;
    cmd.current_dir(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn agent subprocess: {}", e))?;
    let stdout = child.stdout.take().ok_or("child stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut stopped = false;
    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                match maybe_line {
                    Ok(Some(line)) => {
                        // Step 3: emit each line as `Other(value)`. The parser that
                        // turns these into `AssistantText` / `ToolCallStart` / etc.
                        // lands in Step 4, alongside the chat UI that consumes them.
                        let value = serde_json::from_str::<serde_json::Value>(&line)
                            .unwrap_or(serde_json::Value::String(line));
                        // Drop high-volume / low-value events at the source. The
                        // tool-call info we want is in `turn_end.message.content`,
                        // which already renders. Filtering here keeps Iced's
                        // event channel from saturating (caused a panic in Step 3
                        // testing) and keeps the conversation buffer compact.
                        let drop = value
                            .get("type")
                            .and_then(|v| v.as_str())
                            .map(|t| {
                                matches!(
                                    t,
                                    "message_update"
                                        | "message_start"
                                        | "message_end"
                                        | "tool_execution_start"
                                        | "tool_execution_end"
                                )
                            })
                            .unwrap_or(false);
                        if drop {
                            continue;
                        }
                        if event_tx.send(AgentEvent::Other(value)).is_err() {
                            // Receiver dropped — tab is gone, abort.
                            let _ = child.start_kill();
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(format!("stdout read error: {}", e));
                    }
                }
            }
            _ = &mut stop_rx => {
                stopped = true;
                let _ = child.start_kill();
                break;
            }
        }
    }

    let _ = child.wait().await;
    // Sentinel event so the UI knows the turn ended. Refined into a typed
    // variant in Step 4 (likely a state-transition event rather than a raw blob).
    let _ = event_tx.send(AgentEvent::Other(serde_json::json!({
        "type": if stopped { "stopped" } else { "done" },
    })));
    Ok(())
}

fn build_command(config: &AgentBackendConfig, prompt: &str) -> Result<Command, String> {
    match config {
        AgentBackendConfig::Pi {
            model,
            session_path,
            thinking,
        } => {
            let mut cmd = Command::new("pi");
            cmd.arg("--print")
                .arg("--mode")
                .arg("json")
                .arg("--model")
                .arg(model);
            if let Some(path) = session_path {
                cmd.arg("--session").arg(path);
            }
            if let Some(t) = thinking.as_ref() {
                if *t {
                    cmd.arg("--thinking").arg("medium");
                }
            }
            cmd.arg(prompt);
            Ok(cmd)
        }
        // Claude tabs run one long-lived process through
        // `gitterm::harness::claude::ClaudeSession` (TRU-140).
        AgentBackendConfig::Claude { .. } => {
            Err("Claude tabs use the native harness session, not per-turn processes".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(parent: &str, event: HarnessEvent) -> AgentEvent {
        AgentEvent::Harness(HarnessEvent::SubagentEvent {
            parent_tool_use_id: parent.into(),
            event: Box::new(event),
        })
    }

    fn harness(events: &[AgentEvent]) -> Vec<&HarnessEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Harness(h) => Some(h),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn record_coalesces_subagent_deltas_per_parent() {
        let mut session = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        session.record(AgentEvent::Harness(HarnessEvent::TextDelta(
            "parent ".into(),
        )));
        session.record(sub("a", HarnessEvent::TextDelta("one ".into())));
        session.record(sub("a", HarnessEvent::TextDelta("two".into())));
        // A different subagent, or a non-delta, starts a new entry.
        session.record(sub("b", HarnessEvent::TextDelta("other".into())));
        session.record(sub("b", HarnessEvent::ThinkingDelta("hm".into())));
        session.record(sub("b", HarnessEvent::ThinkingDelta("m".into())));
        session.record(sub(
            "b",
            HarnessEvent::ItemCompleted {
                id: "t".into(),
                output: String::new(),
                is_error: false,
            },
        ));
        session.record(sub(
            "b",
            HarnessEvent::ItemCompleted {
                id: "t2".into(),
                output: String::new(),
                is_error: false,
            },
        ));
        // Subagent text does not merge into the parent's text either way.
        session.record(AgentEvent::Harness(HarnessEvent::TextDelta("more".into())));

        let got = harness(&session.conversation);
        assert_eq!(got.len(), 7, "{got:?}");
        assert_eq!(got[0], &HarnessEvent::TextDelta("parent ".into()));
        let AgentEvent::Harness(expected) = sub("a", HarnessEvent::TextDelta("one two".into()))
        else {
            unreachable!()
        };
        assert_eq!(got[1], &expected);
        let AgentEvent::Harness(expected) = sub("b", HarnessEvent::ThinkingDelta("hmm".into()))
        else {
            unreachable!()
        };
        assert_eq!(got[3], &expected);
        assert_eq!(got[6], &HarnessEvent::TextDelta("more".into()));
    }
}
