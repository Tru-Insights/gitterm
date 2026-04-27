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

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone)]
pub(crate) enum AgentEvent {
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
///
/// Step 3 (TRU-29) will add:
///     pub(crate) task_handle: Option<AgentTaskHandle>,
/// to own the subprocess. For now the struct holds only data, no I/O.
pub(crate) struct AgentSession {
    pub(crate) config: AgentBackendConfig,
    pub(crate) conversation: Vec<AgentEvent>,
    /// Claude `--resume` ID, or pi session-file path. `None` until the first
    /// `SystemInit` event populates it.
    pub(crate) session_id: Option<String>,
    pub(crate) state: AgentSessionState,
}

impl AgentSession {
    /// Build a fresh session for a backend config. Empty conversation buffer,
    /// no session_id yet, Idle.
    pub(crate) fn new(config: AgentBackendConfig) -> Self {
        Self {
            config,
            conversation: Vec::new(),
            session_id: None,
            state: AgentSessionState::Idle,
        }
    }

    pub(crate) fn backend(&self) -> AgentBackend {
        self.config.backend()
    }
}
