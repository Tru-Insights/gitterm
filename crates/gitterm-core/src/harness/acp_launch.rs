//! Shared Iced/GPUI ACP launch policy. ACP inherits the desktop environment;
//! only OpenCode receives the permission overlay. Hosts supply snapshot roots.
use super::acp::*;
use super::claude::SnapshotRecorder;
use crate::chat::types::AgentBackendConfig;
use serde_json::{json, Value};
use std::path::PathBuf;

/// OpenCode merges this JSON into its configuration (verified with 2.0.32:
/// an agent defined here shows up in `session/new`'s mode options).
const OPENCODE_CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// Set to a directory to log every ACP stdin/stdout frame there (MCP
/// header values are redacted).
pub const ACP_WIRE_LOG_ENV: &str = "GITTERM_V6_ACP_WIRE_LOG_DIR";

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// The OpenCode configuration GitTerm layers over the user's own through
/// `OPENCODE_CONFIG_CONTENT`: edits and shell commands ask first (OpenCode
/// otherwise runs them unprompted in `build`). The user's config files are
/// never written.
pub fn opencode_config_content() -> Value {
    json!({"permission": {"edit": "ask", "bash": "ask"}})
}

pub fn opencode_config_content_with_guidelines(
    guidelines: Option<&crate::guidelines::Guidelines>,
) -> Value {
    let mut config = opencode_config_content();
    if let Some(guidelines) = guidelines {
        config["instructions"] = json!([guidelines.path]);
    }
    config
}

/// Antigravity's native ACP instruction extension (1.5.11). User prompts
/// stay untouched; OpenCode continues to read its instructions config file.
pub fn session_params(config: &AcpSessionConfig, mcp_servers: Vec<Value>) -> Value {
    let mut params = json!({"cwd": config.cwd, "mcpServers": mcp_servers});
    if config.agent == AcpAgent::Antigravity {
        if let Some(guidelines) = &config.guidelines {
            params["_meta"] = json!({"antigravity": {"systemPrompt": {"append": guidelines.text}}});
        }
    }
    params
}

pub fn supports_http_mcp(agent: AcpAgent, result: &Value) -> bool {
    // agy 1.5.11 omits mcpCapabilities, but accepts HTTP MCP servers and
    // initializes/lists/calls their tools (covered by the headless smoke).
    agent == AcpAgent::Antigravity || result["agentCapabilities"]["mcpCapabilities"]["http"] == true
}

// ---------------------------------------------------------------------------
// Session config
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AcpSessionConfig {
    /// Local turn snapshots; None for remote/test processes.
    pub snapshot_facts_dir: Option<PathBuf>,
    pub agent: AcpAgent,
    pub cwd: PathBuf,
    /// Session id to continue through `session/load`.
    pub resume: Option<String>,
    /// Model, mode and effort to apply once the session exists; `None`
    /// keeps what the agent picks.
    pub model: Option<String>,
    pub mode: Option<String>,
    pub effort: Option<String>,
    /// GitTerm MCP servers to attach (task and browser controls).
    pub mcp_servers: Vec<AcpMcpServer>,
    /// Extra child environment. Values may be secrets: they never reach
    /// logs or `Debug`.
    pub env: Vec<(String, String)>,
    /// When set, every stdin/stdout frame is appended to
    /// `<dir>/acp-<agent>-<pid>-{stdin,stdout}.jsonl`.
    pub wire_log_dir: Option<PathBuf>,
    /// GitTerm's change guidelines, when enabled for this repo.
    pub guidelines: Option<crate::guidelines::Guidelines>,
}

impl std::fmt::Debug for AcpSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionConfig")
            .field("agent", &self.agent)
            .field("cwd", &self.cwd)
            .field("resume", &self.resume)
            .field("model", &self.model)
            .field("mode", &self.mode)
            .field("effort", &self.effort)
            .field("mcp_servers", &self.mcp_servers)
            .field(
                "env",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("wire_log_dir", &self.wire_log_dir)
            .field("guidelines", &self.guidelines.as_ref().map(|g| &g.path))
            .finish()
    }
}

impl AcpSessionConfig {
    pub fn args(&self) -> Vec<String> {
        vec!["acp".to_string()]
    }
}

/// Locate `program` on the child PATH.
pub fn resolve_program(program: &str) -> Result<PathBuf, String> {
    resolve_program_on_path(program, &super::local_launch::child_path())
}

/// Explicit PATH seam for host fixtures; the desktop always uses child_path.
pub fn resolve_program_on_path(program: &str, path: &std::ffi::OsStr) -> Result<PathBuf, String> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "could not find the `{program}` executable on PATH ({})",
                path.to_string_lossy()
            )
        })
}

/// Only OpenCode needs a process-local config overlay. Antigravity uses
/// its own sign-in, with no credential/settings overrides from GitTerm.
pub fn prepare_agent_env(config: &AcpSessionConfig) -> Vec<(String, String)> {
    match config.agent {
        AcpAgent::OpenCode => vec![(
            OPENCODE_CONFIG_CONTENT_ENV.to_string(),
            opencode_config_content_with_guidelines(config.guidelines.as_ref()).to_string(),
        )],
        AcpAgent::Antigravity | AcpAgent::Gemini => Vec::new(),
    }
}

/// A frame as it may be logged: MCP header values (bearer tokens) are
/// replaced.
pub fn redact_frame(frame: &Value) -> Value {
    let mut out = frame.clone();
    if let Some(servers) = out["params"]["mcpServers"].as_array_mut() {
        for server in servers {
            if let Some(headers) = server["headers"].as_array_mut() {
                for header in headers {
                    header["value"] = json!("<redacted>");
                }
            }
        }
    }
    out
}

pub struct Launch {
    pub client_version: &'static str,
    pub config: AcpSessionConfig,
    pub program: PathBuf,
    pub snapshots: Box<dyn SnapshotRecorder>,
}

pub fn local_launch(
    client_version: &'static str,
    config: AcpSessionConfig,
    program: PathBuf,
    snapshots: Box<dyn SnapshotRecorder>,
) -> Launch {
    Launch {
        client_version,
        config,
        program,
        snapshots,
    }
}

/// The same config used by Iced for new and restored tabs. Retired Gemini
/// remains read-only and is never redirected to another provider.
pub fn session_config(
    config: &AgentBackendConfig,
    cwd: PathBuf,
    resume: Option<String>,
    mcp_servers: Vec<AcpMcpServer>,
    guidelines: Option<crate::guidelines::Guidelines>,
) -> Option<AcpSessionConfig> {
    let AgentBackendConfig::Acp {
        agent,
        model,
        mode,
        effort,
    } = config
    else {
        return None;
    };
    if config.read_only_note().is_some() {
        return None;
    }
    let model = model.trim();
    Some(AcpSessionConfig {
        snapshot_facts_dir: None,
        agent: *agent,
        cwd,
        resume,
        model: (!model.is_empty()).then(|| model.to_string()),
        mode: Some(
            mode.clone()
                .unwrap_or_else(|| agent.default_mode().to_string()),
        ),
        effort: effort.clone(),
        mcp_servers,
        env: Vec::new(),
        wire_log_dir: std::env::var_os(ACP_WIRE_LOG_ENV).map(PathBuf::from),
        guidelines,
    })
}

pub fn new_chat_config(
    chat: &crate::settings::ChatDefaults,
    agent: AcpAgent,
) -> AgentBackendConfig {
    let model = match agent {
        AcpAgent::Antigravity => chat.antigravity_model.trim().to_string(),
        AcpAgent::Gemini => String::new(),
        AcpAgent::OpenCode => chat.opencode_model.trim().to_string(),
    };
    AgentBackendConfig::Acp {
        agent,
        model,
        mode: Some(agent.default_mode().to_string()),
        effort: None,
    }
}
