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

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

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
    /// across turns until the tab is closed.
    pub(crate) task_handle: Option<AgentTaskHandle>,
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
        }
    }

    pub(crate) fn backend(&self) -> AgentBackend {
        self.config.backend()
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
    let mut cmd = build_command(config, prompt);
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

fn build_command(config: &AgentBackendConfig, prompt: &str) -> Command {
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
            cmd
        }
        AgentBackendConfig::Claude {
            model,
            permission_mode,
            effort,
        } => {
            let mut cmd = Command::new("claude");
            cmd.arg("--print")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--model")
                .arg(model);
            if let Some(mode) = permission_mode {
                cmd.arg("--permission-mode").arg(mode);
            }
            if let Some(e) = effort {
                cmd.arg("--effort").arg(e);
            }
            cmd.arg(prompt);
            cmd
        }
    }
}
