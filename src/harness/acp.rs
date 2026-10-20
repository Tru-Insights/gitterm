//! Native ACP session (TRU-153): one long-lived agent process that speaks
//! the Agent Client Protocol, JSON-RPC 2.0 over stdio, protocol version 1
//! (agentclientprotocol.com). Gemini CLI (`gemini --acp`) and OpenCode
//! (`opencode acp`) run on this adapter.
//!
//! GitTerm is the ACP *client*: it calls `initialize`, `session/new` or
//! `session/load`, `session/prompt`, `session/cancel` and the setting
//! methods, and answers the agent's `session/request_permission`. It
//! advertises no file-system or terminal capability, so the agent runs its
//! own tools.
//!
//! `UpdateMapper` turns `session/update` notifications into `HarnessEvent`s
//! and is pure, so it is unit-tested against recorded frames in
//! `tests/fixtures/acp/`. `AcpSession` owns the process on a dedicated
//! thread, like `ClaudeSession`, and adds what ACP leaves to the client:
//! a prompt sent while a turn runs waits in GitTerm's queue and goes out
//! when the turn ends, and a resumed session's replayed history comes back
//! as one `HistoryReplayed` event.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

use super::transcript::{TranscriptEntry, MAX_RESULT_CHARS};
use super::{
    HarnessCommand, HarnessEvent, ItemKind, MessageState, RuntimeDecision, RuntimeRequestKind,
    TurnStatus, UserPrompt,
};

/// The ACP protocol version GitTerm speaks.
pub const ACP_PROTOCOL_VERSION: u64 = 1;
/// How long a closed-stdin agent gets to exit before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Lines of stderr kept for the exit report.
const STDERR_TAIL_LINES: usize = 20;
/// `session/load` replays history as notifications. The spec has them all
/// before the response, but Gemini CLI 0.41.2 starts its replay without
/// awaiting it, so the replay counts as done once no history update has
/// arrived for this long after the response.
const HISTORY_QUIET: Duration = Duration::from_millis(400);
/// The macOS keychain item (generic password, by service) holding the
/// Gemini API key.
pub const GEMINI_KEYCHAIN_SERVICE: &str = "gemini-api-key";
const GEMINI_API_KEY_ENV: &str = "GEMINI_API_KEY";
/// Gemini CLI reads its system-level settings from this path when set.
const GEMINI_SYSTEM_SETTINGS_ENV: &str = "GEMINI_CLI_SYSTEM_SETTINGS_PATH";
/// Gemini CLI trusts the workspace folder of a process with this set to `true`.
const GEMINI_TRUST_WORKSPACE_ENV: &str = "GEMINI_CLI_TRUST_WORKSPACE";
/// Where Gemini CLI looks for system settings on macOS without the override.
const GEMINI_DEFAULT_SYSTEM_SETTINGS: &str = "/Library/Application Support/GeminiCli/settings.json";
/// OpenCode merges this JSON into its configuration (verified with 2.0.32:
/// an agent defined here shows up in `session/new`'s mode options).
const OPENCODE_CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// ACP's "authentication required" error code.
pub const ERR_AUTH_REQUIRED: i64 = -32000;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
/// Set to a directory to log every ACP stdin/stdout frame there (MCP
/// header values are redacted).
pub const ACP_WIRE_LOG_ENV: &str = "GITTERM_ACP_WIRE_LOG_DIR";

fn acp_log(msg: &str) {
    eprintln!("[acp] {msg}");
}

fn trim_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

fn truncate_output(mut s: String) -> String {
    if s.len() > MAX_RESULT_CHARS {
        let mut end = MAX_RESULT_CHARS;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// Which ACP agent a chat runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpAgent {
    Gemini,
    OpenCode,
}

impl AcpAgent {
    pub const ALL: [AcpAgent; 2] = [AcpAgent::Gemini, AcpAgent::OpenCode];

    pub fn label(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini",
            Self::OpenCode => "OpenCode",
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
        }
    }

    /// The session mode a new chat starts in: Gemini's `default` prompts
    /// for approval; OpenCode's `build` agent asks before edits and shell
    /// under GitTerm's permission config.
    pub fn default_mode(self) -> &'static str {
        match self {
            Self::Gemini => "default",
            Self::OpenCode => "build",
        }
    }

    /// The read-only planning mode both agents report as `plan`.
    pub fn plan_mode(self) -> &'static str {
        "plan"
    }

    /// The MCP server name this agent sees. Gemini CLI's policy engine
    /// splits `mcp_<server>_<tool>` at the first underscore after `mcp_`,
    /// so its server names use hyphens.
    pub fn mcp_server_name(self, name: &str) -> String {
        match self {
            Self::Gemini => name.replace('_', "-"),
            Self::OpenCode => name.to_string(),
        }
    }
}

/// The OpenCode configuration GitTerm layers over the user's own through
/// `OPENCODE_CONFIG_CONTENT`: edits and shell commands ask first (OpenCode
/// otherwise runs them unprompted in `build`). The user's config files are
/// never written.
pub fn opencode_config_content() -> Value {
    json!({"permission": {"edit": "ask", "bash": "ask"}})
}

/// Gemini CLI system settings for GitTerm's chats: whatever the machine's
/// own system settings say, with the auth type pinned to the API key so
/// neither `session/new` nor `session/load` needs `authenticate` (which
/// would write the choice into the user's `~/.gemini/settings.json`).
pub fn gemini_system_settings(existing: Option<Value>) -> Value {
    let mut settings = match existing {
        Some(Value::Object(map)) => Value::Object(map),
        _ => json!({}),
    };
    if !settings["security"].is_object() {
        settings["security"] = json!({});
    }
    if !settings["security"]["auth"].is_object() {
        settings["security"]["auth"] = json!({});
    }
    settings["security"]["auth"]["selectedType"] = json!("gemini-api-key");
    settings
}

/// The chat's message for a Gemini chat started without a key.
pub fn gemini_missing_key_message(detail: &str) -> String {
    format!(
        "Gemini needs an API key, read from the macOS keychain item \"{GEMINI_KEYCHAIN_SERVICE}\" \
         ({detail}). Add it in Terminal with\n\n    \
         security add-generic-password -s {GEMINI_KEYCHAIN_SERVICE} -a \"$USER\" -w\n\n\
         (it prompts for the key), then send the message again."
    )
}

/// Read the Gemini API key from the keychain. Never logged.
pub async fn gemini_api_key_from_keychain() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", GEMINI_KEYCHAIN_SERVICE, "-w"])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("could not run /usr/bin/security: {e}"))?;
        if !output.status.success() {
            return Err(match output.status.code() {
                Some(44) => "no such keychain item".to_string(),
                code => format!("security exited with {code:?}"),
            });
        }
        let key = String::from_utf8(output.stdout)
            .map_err(|_| "the keychain item is not UTF-8".to_string())?
            .trim()
            .to_string();
        if key.is_empty() {
            return Err("the keychain item is empty".to_string());
        }
        Ok(key)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("GitTerm reads it from the macOS keychain, which this platform lacks".to_string())
    }
}

// ---------------------------------------------------------------------------
// Session config
// ---------------------------------------------------------------------------

/// One HTTP MCP server handed to the agent in `session/new` / `session/load`.
/// ACP carries header values literally, so the bearer token travels in the
/// JSON-RPC frame on the child's stdin, never argv, logs or `Debug`.
#[derive(Clone)]
pub struct AcpMcpServer {
    pub name: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for AcpMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpMcpServer")
            .field("name", &self.name)
            .field("url", &self.url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl AcpMcpServer {
    /// The ACP `McpServer` (http variant) for `agent`.
    pub fn to_acp(&self, agent: AcpAgent) -> Value {
        json!({
            "type": "http",
            "name": agent.mcp_server_name(&self.name),
            "url": self.url,
            "headers": self
                .headers
                .iter()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone)]
pub struct AcpSessionConfig {
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
    /// A directory GitTerm owns where the adapter writes the agent's
    /// support files (the Gemini system settings).
    pub support_dir: PathBuf,
    /// When set, every stdin/stdout frame is appended to
    /// `<dir>/acp-<agent>-<pid>-{stdin,stdout}.jsonl`.
    pub wire_log_dir: Option<PathBuf>,
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
            .field("support_dir", &self.support_dir)
            .field("wire_log_dir", &self.wire_log_dir)
            .finish()
    }
}

impl AcpSessionConfig {
    pub fn args(&self) -> Vec<String> {
        match self.agent {
            AcpAgent::Gemini => vec!["--acp".to_string()],
            AcpAgent::OpenCode => vec!["acp".to_string()],
        }
    }

    fn gemini_settings_path(&self) -> PathBuf {
        self.support_dir.join("gemini-system-settings.json")
    }
}

/// Locate `program` on the child PATH.
pub fn resolve_program(program: &str) -> Result<PathBuf, String> {
    let path = super::claude::child_path();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "could not find the `{program}` executable on PATH ({})",
                path.to_string_lossy()
            )
        })
}

/// Write the agent's support files and return the environment that points
/// the agent at them (plus the Gemini key).
async fn prepare_agent_env(config: &AcpSessionConfig) -> Result<Vec<(String, String)>, String> {
    match config.agent {
        AcpAgent::Gemini => {
            let key = gemini_api_key_from_keychain()
                .await
                .map_err(|detail| gemini_missing_key_message(&detail))?;
            std::fs::create_dir_all(&config.support_dir).map_err(|e| {
                format!(
                    "create the Gemini support dir {}: {e}",
                    config.support_dir.display()
                )
            })?;
            let existing = match std::fs::read_to_string(GEMINI_DEFAULT_SYSTEM_SETTINGS) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) => Some(value),
                    Err(e) => {
                        return Err(format!(
                        "{GEMINI_DEFAULT_SYSTEM_SETTINGS} is not JSON ({e}); GitTerm layers its \
                             Gemini settings over it and will not drop it silently"
                    ))
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("read {GEMINI_DEFAULT_SYSTEM_SETTINGS}: {e}")),
            };
            let settings = gemini_system_settings(existing);
            let settings_path = config.gemini_settings_path();
            std::fs::write(&settings_path, settings.to_string())
                .map_err(|e| format!("write {}: {e}", settings_path.display()))?;
            Ok(vec![
                (GEMINI_API_KEY_ENV.to_string(), key),
                (
                    GEMINI_SYSTEM_SETTINGS_ENV.to_string(),
                    settings_path.display().to_string(),
                ),
                // Gemini CLI attaches no MCP server and refuses its edit modes
                // in a folder it does not trust. The chat runs where the human
                // opened it, so the folder is trusted for this process only
                // (the user's trustedFolders.json is not written).
                (GEMINI_TRUST_WORKSPACE_ENV.to_string(), "true".to_string()),
            ])
        }
        AcpAgent::OpenCode => Ok(vec![(
            OPENCODE_CONFIG_CONTENT_ENV.to_string(),
            opencode_config_content().to_string(),
        )]),
    }
}

// ---------------------------------------------------------------------------
// Session settings (models, modes, config options)
// ---------------------------------------------------------------------------

/// A setting a chat can change while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Model,
    Mode,
    Effort,
}

impl Setting {
    fn label(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Mode => "mode",
            Self::Effort => "effort",
        }
    }

    /// The `SessionConfigOption.category` ACP reserves for this setting.
    fn category(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Mode => "mode",
            Self::Effort => "thought_level",
        }
    }
}

/// The effort value OpenCode lists for "the model's own default".
const DEFAULT_EFFORT_VALUE: &str = "default";

/// What the agent reported about a session's settings: ACP's
/// `configOptions` (OpenCode), or the older `models` / `modes` states
/// (Gemini CLI). Updated from `session/set_config_option` replies and
/// `config_option_update` / `current_mode_update` notifications.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSettings {
    config_options: Vec<Value>,
    current_model: Option<String>,
    models: Vec<Value>,
    current_mode: Option<String>,
    modes: Vec<Value>,
}

impl SessionSettings {
    /// From a `session/new` or `session/load` result.
    pub fn from_result(result: &Value) -> Self {
        let mut settings = Self::default();
        if let Some(models) = result.get("models").filter(|m| m.is_object()) {
            settings.current_model = models["currentModelId"].as_str().map(str::to_string);
            settings.models = models["availableModels"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
        if let Some(modes) = result.get("modes").filter(|m| m.is_object()) {
            settings.current_mode = modes["currentModeId"].as_str().map(str::to_string);
            settings.modes = modes["availableModes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
        if let Some(options) = result["configOptions"].as_array() {
            settings.config_options = options.clone();
        }
        settings
    }

    fn option(&self, setting: Setting) -> Option<&Value> {
        self.config_options
            .iter()
            .find(|o| o["category"] == setting.category())
            .or_else(|| {
                // OpenCode names its effort option `effort`.
                let id = match setting {
                    Setting::Model => "model",
                    Setting::Mode => "mode",
                    Setting::Effort => "effort",
                };
                self.config_options.iter().find(|o| o["id"] == id)
            })
    }

    fn option_values(option: &Value) -> Vec<String> {
        option["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|o| o["value"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn current(&self, setting: Setting) -> Option<String> {
        if let Some(option) = self.option(setting) {
            let value = option["currentValue"].as_str().map(str::to_string);
            return match setting {
                Setting::Effort => value.filter(|v| v != DEFAULT_EFFORT_VALUE),
                _ => value,
            };
        }
        match setting {
            Setting::Model => self.current_model.clone(),
            Setting::Mode => self.current_mode.clone(),
            Setting::Effort => None,
        }
    }

    /// Whether the agent lets the chat pick this setting.
    pub fn supports(&self, setting: Setting) -> bool {
        self.option(setting).is_some()
            || match setting {
                Setting::Model => !self.models.is_empty(),
                Setting::Mode => !self.modes.is_empty(),
                Setting::Effort => false,
            }
    }

    /// Effort levels the agent offers, without its "default".
    pub fn effort_levels(&self) -> Vec<String> {
        self.option(Setting::Effort)
            .map(Self::option_values)
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v != DEFAULT_EFFORT_VALUE)
            .collect()
    }

    /// The model list in the chat page's shape (`value`, `displayName`,
    /// `description`, and effort support).
    pub fn page_models(&self) -> Vec<Value> {
        let levels = self.effort_levels();
        let effort = |mut entry: Value| {
            if !levels.is_empty() {
                entry["supportsEffort"] = json!(true);
                entry["supportedEffortLevels"] = json!(levels);
            }
            entry
        };
        if let Some(option) = self.option(Setting::Model) {
            return option["options"]
                .as_array()
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|o| {
                            let value = o["value"].as_str()?;
                            let mut entry = json!({
                                "value": value,
                                "displayName": o["name"].as_str().unwrap_or(value),
                            });
                            if let Some(d) = o["description"].as_str() {
                                entry["description"] = json!(d);
                            }
                            Some(effort(entry))
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        self.models
            .iter()
            .filter_map(|m| {
                let value = m["modelId"].as_str()?;
                let mut entry = json!({
                    "value": value,
                    "displayName": m["name"].as_str().unwrap_or(value),
                });
                if let Some(d) = m["description"].as_str() {
                    entry["description"] = json!(d);
                }
                Some(effort(entry))
            })
            .collect()
    }

    /// The mode list in the chat page's shape (`value`, `displayName`,
    /// `description`).
    pub fn page_modes(&self) -> Vec<Value> {
        if let Some(option) = self.option(Setting::Mode) {
            return option["options"]
                .as_array()
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|o| {
                            let value = o["value"].as_str()?;
                            Some(json!({
                                "value": value,
                                "displayName": o["name"].as_str().unwrap_or(value),
                                "description": o["description"],
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        self.modes
            .iter()
            .filter_map(|m| {
                let value = m["id"].as_str()?;
                Some(json!({
                    "value": value,
                    "displayName": m["name"].as_str().unwrap_or(value),
                    "description": m["description"],
                }))
            })
            .collect()
    }

    /// The JSON-RPC method and params that set `setting` to `value`
    /// (`None`: the agent's default, effort only).
    pub fn change_request(
        &self,
        session_id: &str,
        setting: Setting,
        value: Option<&str>,
    ) -> Result<(&'static str, Value), String> {
        if let Some(option) = self.option(setting) {
            let values = Self::option_values(option);
            let value = match (setting, value) {
                (_, Some(v)) => v.to_string(),
                (Setting::Effort, None) if values.iter().any(|v| v == DEFAULT_EFFORT_VALUE) => {
                    DEFAULT_EFFORT_VALUE.to_string()
                }
                (_, None) => {
                    return Err(format!(
                        "the agent has no default {} to return to",
                        setting.label()
                    ))
                }
            };
            if !values.is_empty() && !values.contains(&value) {
                return Err(format!(
                    "the agent does not offer {} {value:?}",
                    setting.label()
                ));
            }
            return Ok((
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": option["id"], "value": value}),
            ));
        }
        let Some(value) = value else {
            return Err(format!(
                "the agent has no default {} to return to",
                setting.label()
            ));
        };
        match setting {
            Setting::Model if !self.models.is_empty() => Ok((
                "session/set_model",
                json!({"sessionId": session_id, "modelId": value}),
            )),
            Setting::Mode if !self.modes.is_empty() => {
                if !self.modes.iter().any(|m| m["id"] == value) {
                    return Err(format!("the agent does not offer mode {value:?}"));
                }
                Ok((
                    "session/set_mode",
                    json!({"sessionId": session_id, "modeId": value}),
                ))
            }
            _ => Err(format!("the agent has no {} setting", setting.label())),
        }
    }

    /// Record a confirmed change made through `session/set_model` or
    /// `session/set_mode` (`set_config_option` replies carry the options).
    pub fn set_current(&mut self, setting: Setting, value: &str) {
        match setting {
            Setting::Model => self.current_model = Some(value.to_string()),
            Setting::Mode => self.current_mode = Some(value.to_string()),
            Setting::Effort => {}
        }
    }

    pub fn set_config_options(&mut self, options: Vec<Value>) {
        self.config_options = options;
    }
}

/// The events for settings that differ between `before` and `after`.
fn settings_changes(before: &SessionSettings, after: &SessionSettings) -> Vec<HarnessEvent> {
    let mut events = Vec::new();
    if let (Some(model), true) = (
        after.current(Setting::Model),
        after.current(Setting::Model) != before.current(Setting::Model),
    ) {
        events.push(HarnessEvent::ModelChanged(model));
    }
    if let (Some(mode), true) = (
        after.current(Setting::Mode),
        after.current(Setting::Mode) != before.current(Setting::Mode),
    ) {
        events.push(HarnessEvent::PermissionModeChanged(mode));
    }
    if after.supports(Setting::Effort)
        && after.current(Setting::Effort) != before.current(Setting::Effort)
    {
        events.push(HarnessEvent::EffortChanged {
            effort: after.current(Setting::Effort),
            applied: None,
        });
    }
    events
}

// ---------------------------------------------------------------------------
// Update mapping
// ---------------------------------------------------------------------------

/// One mapped `session/update`.
#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    Event(HarnessEvent),
    /// A user message chunk (only replayed history carries these; a live
    /// turn's own prompt is already on the page).
    UserChunk {
        message_id: Option<String>,
        text: String,
    },
    /// `current_mode_update`.
    Mode(String),
    /// `config_option_update`: the session's full option list.
    ConfigOptions(Vec<Value>),
}

#[derive(Debug, Default)]
struct ToolState {
    input: Value,
    /// The full input went out as an `ItemInputDelta` (once per call).
    input_sent: bool,
    done: bool,
}

/// Maps `session/update` notifications and permission requests onto
/// harness events. Tool calls are tracked by id because the agents differ
/// in what they send: OpenCode announces each call with `tool_call` and
/// fills in its input with a later `tool_call_update`; Gemini CLI sends no
/// `tool_call` at all for a call that needs permission, so the permission
/// request or the first update starts the item.
#[derive(Debug, Default)]
pub struct UpdateMapper {
    tools: HashMap<String, ToolState>,
    logged_kinds: HashSet<String>,
    /// The session's cumulative cost from the last `usage_update`, in USD.
    pub cost_usd: Option<f64>,
}

fn content_text(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str().map(str::to_string),
        "image" => Some("[image]".to_string()),
        "audio" => Some("[audio]".to_string()),
        "resource_link" => block["uri"].as_str().map(|uri| format!("[{uri}]")),
        "resource" => block["resource"]["text"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                block["resource"]["uri"]
                    .as_str()
                    .map(|uri| format!("[{uri}]"))
            }),
        _ => None,
    }
}

/// A tool call's output text: its `content` blocks (text, diffs,
/// terminals), else its `rawOutput`.
pub fn tool_output(update: &Value) -> String {
    let mut parts = Vec::new();
    for item in update["content"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("content") => {
                if let Some(text) = content_text(&item["content"]) {
                    parts.push(text);
                }
            }
            Some("diff") => {
                let path = item["path"].as_str().unwrap_or("?");
                let new_text = item["newText"].as_str().unwrap_or("");
                parts.push(match item["oldText"].as_str() {
                    Some(old) => format!("{path}\n--- before\n{old}\n+++ after\n{new_text}"),
                    None => format!("{path} (new file)\n{new_text}"),
                });
            }
            Some("terminal") => parts.push(format!(
                "[terminal {}]",
                item["terminalId"].as_str().unwrap_or("?")
            )),
            _ => {}
        }
    }
    if parts.is_empty() {
        match &update["rawOutput"] {
            Value::Null => {}
            Value::String(s) => parts.push(s.clone()),
            other => {
                if let Some(output) = other["output"].as_str() {
                    parts.push(output.to_string());
                } else {
                    parts.push(other.to_string());
                }
            }
        }
    }
    truncate_output(parts.join("\n"))
}

/// The tool's name as the chat shows it: ACP's `name` (OpenCode), else
/// the call's `title` (Gemini CLI's display title), else its `kind`.
fn tool_name(tool_call: &Value) -> String {
    ["name", "title", "kind"]
        .iter()
        .find_map(|key| tool_call[*key].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("tool")
        .to_string()
}

/// The tool's input: `rawInput` when the agent sends one, else what the
/// call says about itself (title, kind, paths).
fn tool_input(tool_call: &Value) -> Value {
    if let Some(raw) = tool_call["rawInput"].as_object() {
        if !raw.is_empty() {
            return Value::Object(raw.clone());
        }
    }
    let mut input = serde_json::Map::new();
    if let Some(title) = tool_call["title"].as_str() {
        input.insert("title".into(), json!(title));
    }
    if let Some(kind) = tool_call["kind"].as_str() {
        input.insert("kind".into(), json!(kind));
    }
    let paths: Vec<Value> = tool_call["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["path"].as_str().map(|p| json!(p)))
        .chain(
            tool_call["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["type"] == "diff")
                .filter_map(|c| c["path"].as_str().map(|p| json!(p))),
        )
        .collect();
    if !paths.is_empty() {
        input.insert("paths".into(), Value::Array(paths));
    }
    Value::Object(input)
}

impl UpdateMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Log a kind once per session, so nothing is dropped unseen.
    fn log_once(&mut self, kind: &str, what: &str) {
        if self.logged_kinds.insert(kind.to_string()) {
            acp_log(&format!("{what} {kind:?} (logged once per session)"));
        }
    }

    /// Start, update or finish one tool call.
    fn tool(&mut self, update: &Value) -> Vec<HarnessEvent> {
        let Some(id) = update["toolCallId"].as_str() else {
            acp_log(&format!(
                "tool call without toolCallId: {}",
                trim_for_log(&update.to_string(), 200)
            ));
            return Vec::new();
        };
        let mut events = Vec::new();
        let input = tool_input(update);
        match self.tools.get_mut(id) {
            None => {
                events.push(HarnessEvent::ItemStarted {
                    id: id.to_string(),
                    kind: ItemKind::ToolCall {
                        name: tool_name(update),
                        input: input.clone(),
                    },
                });
                self.tools.insert(
                    id.to_string(),
                    ToolState {
                        input,
                        input_sent: false,
                        done: false,
                    },
                );
            }
            Some(state) => {
                // A later frame with a fuller input (OpenCode's command
                // arrives after the call is announced) replaces it once.
                let has_raw = update["rawInput"]
                    .as_object()
                    .is_some_and(|o| !o.is_empty());
                if has_raw && input != state.input && !state.input_sent && !state.done {
                    events.push(HarnessEvent::ItemInputDelta {
                        id: id.to_string(),
                        partial_json: input.to_string(),
                    });
                    state.input = input;
                    state.input_sent = true;
                }
            }
        }
        let status = update["status"].as_str();
        if matches!(status, Some("completed" | "failed")) {
            if let Some(state) = self.tools.get_mut(id) {
                if !state.done {
                    state.done = true;
                    events.push(HarnessEvent::ItemCompleted {
                        id: id.to_string(),
                        output: tool_output(update),
                        is_error: status == Some("failed"),
                    });
                }
            }
        }
        events
    }

    /// Map one `session/update` notification's `update` object.
    pub fn map(&mut self, update: &Value) -> Vec<Mapped> {
        let Some(kind) = update["sessionUpdate"].as_str() else {
            acp_log(&format!(
                "session/update without sessionUpdate: {}",
                trim_for_log(&update.to_string(), 200)
            ));
            return Vec::new();
        };
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" => {
                let Some(text) = content_text(&update["content"]) else {
                    self.log_once(&format!("{kind}/{}", update["content"]["type"]), "skipping");
                    return Vec::new();
                };
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Mapped::Event(if kind == "agent_message_chunk" {
                    HarnessEvent::TextDelta(text)
                } else {
                    HarnessEvent::ThinkingDelta(text)
                })]
            }
            "user_message_chunk" => content_text(&update["content"])
                .map(|text| {
                    vec![Mapped::UserChunk {
                        message_id: update["messageId"].as_str().map(str::to_string),
                        text,
                    }]
                })
                .unwrap_or_default(),
            "tool_call" | "tool_call_update" => {
                self.tool(update).into_iter().map(Mapped::Event).collect()
            }
            "usage_update" => {
                let cost = &update["cost"];
                if cost["currency"] == "USD" {
                    self.cost_usd = cost["amount"].as_f64();
                } else if !cost.is_null() {
                    self.log_once(
                        &format!("usage_update cost in {}", cost["currency"]),
                        "not showing",
                    );
                }
                Vec::new()
            }
            "current_mode_update" => update["currentModeId"]
                .as_str()
                .map(|mode| vec![Mapped::Mode(mode.to_string())])
                .unwrap_or_default(),
            "config_option_update" => update["configOptions"]
                .as_array()
                .map(|options| vec![Mapped::ConfigOptions(options.clone())])
                .unwrap_or_default(),
            // Known, nothing to show in the chat (yet).
            "available_commands_update" | "session_info_update" => Vec::new(),
            "plan" | "notice" | "compaction_update" | "compaction_summary_chunk" => {
                self.log_once(kind, "not rendering session/update kind");
                Vec::new()
            }
            other => {
                self.log_once(other, "unknown session/update kind");
                Vec::new()
            }
        }
    }

    /// A `session/request_permission` request: the item events that make
    /// sure its tool call is on the timeline, the tool call id, and the
    /// runtime request. The agent's `allow_always` options ride along as
    /// the request's suggestions (`{"type": "acp_option", ...}`).
    pub fn permission(
        &mut self,
        params: &Value,
    ) -> (Vec<HarnessEvent>, String, RuntimeRequestKind) {
        let tool_call = &params["toolCall"];
        let tool_use_id = tool_call["toolCallId"].as_str().unwrap_or("").to_string();
        // The request carries no completion; strip any status so it cannot
        // end the item.
        let mut announce = tool_call.clone();
        if let Some(obj) = announce.as_object_mut() {
            obj.remove("status");
        }
        let events = if tool_use_id.is_empty() {
            Vec::new()
        } else {
            self.tool(&announce)
        };
        let input = self
            .tools
            .get(&tool_use_id)
            .map(|state| state.input.clone())
            .unwrap_or_else(|| tool_input(tool_call));
        let suggestions = params["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| o["kind"] == "allow_always")
            .map(|o| {
                json!({
                    "type": "acp_option",
                    "optionId": o["optionId"],
                    "name": o["name"],
                    "kind": o["kind"],
                })
            })
            .collect();
        let kind = RuntimeRequestKind::Permission {
            tool_name: tool_name(tool_call),
            input,
            suggestions,
            decision_reason: None,
        };
        (events, tool_use_id, kind)
    }
}

/// The `RequestPermissionResponse` for a decision. Allow without
/// `remember` picks the agent's `allow_once` option; Allow with `remember`
/// picks that `allow_always` option (or the first one); Deny picks
/// `reject_once`, or answers `cancelled` when the agent offers none. ACP
/// carries no deny reason or edited input; both are logged and dropped.
pub fn permission_outcome(decision: &RuntimeDecision, options: &[Value]) -> Result<Value, String> {
    let by_kind = |kind: &str| {
        options
            .iter()
            .find(|o| o["kind"] == kind)
            .and_then(|o| o["optionId"].as_str())
    };
    let selected = |id: &str| json!({"outcome": {"outcome": "selected", "optionId": id}});
    match decision {
        RuntimeDecision::Allow {
            updated_input,
            remember,
        } => {
            if updated_input.is_some() {
                acp_log("ACP cannot change a tool call's input; the original input runs");
            }
            match remember {
                None => by_kind("allow_once").map(selected).ok_or_else(|| {
                    "the agent offers no one-time allow for this call; pick one of its \
                     \"always\" options instead"
                        .to_string()
                }),
                Some(remember) => {
                    let wanted = remember["optionId"].as_str();
                    options
                        .iter()
                        .filter(|o| o["kind"] == "allow_always")
                        .find(|o| wanted.is_none() || o["optionId"].as_str() == wanted)
                        .and_then(|o| o["optionId"].as_str())
                        .map(selected)
                        .ok_or_else(|| {
                            format!("the agent offers no \"always allow\" option {wanted:?}")
                        })
                }
            }
        }
        RuntimeDecision::Deny { message } => {
            acp_log(&format!(
                "deny reason not sent (ACP has no field for it): {}",
                trim_for_log(message, 120)
            ));
            Ok(by_kind("reject_once")
                .map(selected)
                .unwrap_or_else(|| json!({"outcome": {"outcome": "cancelled"}})))
        }
        RuntimeDecision::AnswerQuestion { .. } => {
            Err("ACP agents ask no structured questions; nothing to answer".to_string())
        }
    }
}

/// How a `session/prompt` result's `stopReason` ends the turn.
/// `interrupted`: GitTerm sent `session/cancel` for this turn.
pub fn turn_status(stop_reason: Option<&str>, interrupted: bool) -> TurnStatus {
    match stop_reason {
        Some("end_turn") => TurnStatus::Completed,
        Some("cancelled") => TurnStatus::Interrupted,
        _ if interrupted => TurnStatus::Interrupted,
        Some("max_tokens") => {
            TurnStatus::Failed("The model stopped at its output token limit.".into())
        }
        Some("max_turn_requests") => TurnStatus::Failed(
            "The agent stopped at its limit of model requests for one turn.".into(),
        ),
        Some("refusal") => TurnStatus::Failed("The model refused to continue.".into()),
        Some(other) => TurnStatus::Failed(format!("The turn stopped: {other}")),
        None => TurnStatus::Failed("The agent ended the turn without a stop reason.".into()),
    }
}

/// How a `session/prompt` reply ends the turn, with the turn's usage. An
/// error reply is a failed turn whose message is the agent's own (Gemini
/// CLI turns a model API failure, such as 402 "prepayment credits are
/// depleted", into an error reply carrying the API's status and message),
/// unless GitTerm had cancelled the turn.
pub fn prompt_reply(msg: &Value, interrupted: bool) -> (TurnStatus, Value) {
    match msg.get("error").filter(|e| !e.is_null()) {
        Some(error) if interrupted => {
            acp_log(&format!(
                "prompt failed after cancel: {}",
                rpc_error_text(error)
            ));
            (TurnStatus::Interrupted, Value::Null)
        }
        Some(error) => (TurnStatus::Failed(rpc_error_text(error)), Value::Null),
        None => (
            turn_status(msg["result"]["stopReason"].as_str(), interrupted),
            turn_usage(&msg["result"]["usage"]),
        ),
    }
}

/// A prompt result's `usage` (OpenCode: camelCase token counts), with the
/// snake_case `input_tokens` / `output_tokens` the chat page reads added.
pub fn turn_usage(usage: &Value) -> Value {
    let Some(map) = usage.as_object() else {
        return Value::Null;
    };
    let mut out = map.clone();
    for (from, to) in [
        ("outputTokens", "output_tokens"),
        ("inputTokens", "input_tokens"),
    ] {
        if let (Some(n), false) = (map.get(from).and_then(Value::as_u64), map.contains_key(to)) {
            out.insert(to.into(), json!(n));
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// History replay
// ---------------------------------------------------------------------------

/// Builds a resumed session's timeline from its `session/load` replay:
/// user chunks become prompts (one per message id), each earlier turn is
/// closed with a `TurnCompleted` so the page shows it finished, and
/// streamed text and thinking are merged.
#[derive(Debug, Default)]
pub struct HistoryBuilder {
    entries: Vec<TranscriptEntry>,
    user_message: Option<Option<String>>,
    turn_open: bool,
}

impl HistoryBuilder {
    pub fn user_chunk(&mut self, message_id: Option<String>, text: &str) {
        if let (Some(current), Some(TranscriptEntry::UserPrompt(prompt))) =
            (&self.user_message, self.entries.last_mut())
        {
            if !self.turn_open && *current == message_id {
                prompt.push_str(text);
                return;
            }
        }
        self.close_turn();
        self.entries
            .push(TranscriptEntry::UserPrompt(text.to_string()));
        self.user_message = Some(message_id);
    }

    pub fn event(&mut self, event: HarnessEvent) {
        self.turn_open = true;
        if let Some(TranscriptEntry::Harness(last)) = self.entries.last_mut() {
            match (last, &event) {
                (HarnessEvent::TextDelta(a), HarnessEvent::TextDelta(b))
                | (HarnessEvent::ThinkingDelta(a), HarnessEvent::ThinkingDelta(b)) => {
                    a.push_str(b);
                    return;
                }
                _ => {}
            }
        }
        self.entries.push(TranscriptEntry::Harness(event));
    }

    fn close_turn(&mut self) {
        if self.turn_open {
            self.entries
                .push(TranscriptEntry::Harness(HarnessEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: Value::Null,
                    cost_usd: None,
                }));
            self.turn_open = false;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn finish(mut self) -> Vec<TranscriptEntry> {
        self.close_turn();
        self.entries
    }
}

// ---------------------------------------------------------------------------
// Prompt queue
// ---------------------------------------------------------------------------

/// Prompts waiting for the running turn (or the session's startup) to end.
/// ACP has no mid-turn queue, so GitTerm holds them and sends them one turn
/// at a time, reporting each through `MessageLifecycle` the way Claude
/// reports its own queue, so the page's queued-message UI applies as is.
#[derive(Debug, Default)]
pub struct PromptQueue {
    waiting: VecDeque<UserPrompt>,
}

impl PromptQueue {
    /// A submitted prompt: returned to send now when the session is
    /// `idle` (ready, no turn running) and nothing is waiting ahead of it;
    /// otherwise queued, with its `Queued` lifecycle event.
    pub fn submit(
        &mut self,
        prompt: UserPrompt,
        idle: bool,
    ) -> (Option<UserPrompt>, Vec<HarnessEvent>) {
        if idle && self.waiting.is_empty() {
            return (Some(prompt), Vec::new());
        }
        let events = prompt
            .id
            .iter()
            .map(|id| HarnessEvent::MessageLifecycle {
                id: id.clone(),
                state: MessageState::Queued,
            })
            .collect();
        self.waiting.push_back(prompt);
        (None, events)
    }

    /// Withdraw a waiting prompt by id: `Cancelled`, or a refusal when it
    /// was already sent (or never queued).
    pub fn withdraw(&mut self, id: &str) -> HarnessEvent {
        let before = self.waiting.len();
        self.waiting.retain(|p| p.id.as_deref() != Some(id));
        if self.waiting.len() < before {
            HarnessEvent::MessageLifecycle {
                id: id.to_string(),
                state: MessageState::Cancelled,
            }
        } else {
            HarnessEvent::MessageWithdrawRefused {
                id: id.to_string(),
                reason: "it was already sent".to_string(),
            }
        }
    }

    /// The next prompt to send, oldest first.
    pub fn pop_next(&mut self) -> Option<UserPrompt> {
        self.waiting.pop_front()
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// The `session/prompt` content blocks for a prompt. Images go only to an
/// agent that advertised `promptCapabilities.image`.
pub fn prompt_blocks(prompt: &UserPrompt, images_supported: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    if images_supported {
        for image in &prompt.images {
            blocks.push(json!({"type": "image", "mimeType": image.media_type, "data": image.data}));
        }
    }
    if !prompt.text.is_empty() || blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": prompt.text}));
    }
    blocks
}

fn request_frame(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification_frame(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn response_frame(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_frame(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
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

/// A JSON-RPC error object as a sentence.
fn rpc_error_text(error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("unknown error");
    match error["data"].get("details").and_then(Value::as_str) {
        Some(details) => format!("{message} ({details})"),
        None => match &error["data"] {
            Value::Null => message.to_string(),
            data => format!("{message} ({})", trim_for_log(&data.to_string(), 200)),
        },
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Handle to a running ACP session. Dropping it shuts the process down.
pub struct AcpSession {
    commands: mpsc::UnboundedSender<HarnessCommand>,
}

impl AcpSession {
    /// Start the agent on a dedicated thread. Spawn failures (including a
    /// missing Gemini key) arrive on the event channel as `Error` followed
    /// by `ProcessExited`.
    pub fn spawn(config: AcpSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let spawn_result = std::thread::Builder::new()
            .name("acp-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(HarnessEvent::Error(format!(
                            "could not start the ACP session runtime: {e}"
                        )));
                        let _ = ev_tx.send(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                };
                rt.block_on(run_session(config, cmd_rx, ev_tx));
            });
        if let Err(e) = spawn_result {
            acp_log(&format!("could not spawn session thread: {e}"));
        }
        (Self { commands: cmd_tx }, ev_rx)
    }

    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "the agent session has exited".to_string())
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        let _ = self.commands.send(HarnessCommand::Shutdown);
    }
}

/// A request GitTerm sent and is waiting on.
#[derive(Debug)]
enum Call {
    Initialize,
    NewSession,
    LoadSession,
    /// Applying the configured model/mode/effort before `Ready`.
    Setup(Setting, Option<String>),
    /// A change the human asked for.
    Change(Setting, Option<String>),
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initializing,
    Creating,
    /// `session/load` answered; its replay may still be arriving.
    History,
    Setup,
    Ready,
    /// The session could not be created; the process is being stopped.
    Failed,
}

struct PendingPermission {
    rpc_id: Value,
    options: Vec<Value>,
}

struct WireLog {
    stdin: std::fs::File,
    stdout: std::fs::File,
}

impl WireLog {
    fn open(dir: &Path, agent: AcpAgent, pid: u32) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create wire log dir {}: {e}", dir.display()))?;
        let open = |name: String| {
            let path = dir.join(name);
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("open wire log {}: {e}", path.display()))
        };
        let agent = agent.program();
        Ok(Self {
            stdin: open(format!("acp-{agent}-{pid}-stdin.jsonl"))?,
            stdout: open(format!("acp-{agent}-{pid}-stdout.jsonl"))?,
        })
    }
}

struct Session {
    config: AcpSessionConfig,
    stdin: Option<ChildStdin>,
    wire: Option<WireLog>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    pid: u32,
    next_id: u64,
    calls: HashMap<u64, Call>,
    phase: Phase,
    session_id: Option<String>,
    settings: SessionSettings,
    mapper: UpdateMapper,
    history: HistoryBuilder,
    history_deadline: Option<tokio::time::Instant>,
    setup: VecDeque<(Setting, Option<String>)>,
    load_session: bool,
    http_mcp: bool,
    images: bool,
    queue: PromptQueue,
    in_turn: bool,
    /// The id of the prompt the running turn answers.
    turn_message: Option<String>,
    interrupted: bool,
    permissions: HashMap<String, PendingPermission>,
    next_request: u64,
    logged_methods: HashSet<String>,
}

impl Session {
    fn emit(&self, ev: HarnessEvent) {
        // The receiver is gone only when the tab is; nothing to report to.
        let _ = self.events.send(ev);
    }

    fn error(&self, msg: String) {
        acp_log(&msg);
        self.emit(HarnessEvent::Error(msg));
    }

    async fn write(&mut self, frame: Value) -> Result<(), String> {
        if let Some(wire) = self.wire.as_mut() {
            if let Err(e) = writeln!(wire.stdin, "{}", redact_frame(&frame)) {
                acp_log(&format!("wire log write failed: {e}"));
            }
        }
        let stdin = self.stdin.as_mut().ok_or("the agent's stdin is closed")?;
        let mut bytes = frame.to_string().into_bytes();
        bytes.push(b'\n');
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| format!("write to the agent's stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush the agent's stdin: {e}"))
    }

    async fn call(&mut self, method: &str, params: Value, call: Call) -> Result<(), String> {
        self.next_id += 1;
        let id = self.next_id;
        acp_log(&format!("-> {method} ({id}) {call:?}"));
        self.calls.insert(id, call);
        self.write(request_frame(id, method, params)).await
    }

    fn mcp_servers(&self) -> Vec<Value> {
        if self.config.mcp_servers.is_empty() {
            return Vec::new();
        }
        if !self.http_mcp {
            self.error(format!(
                "{} does not take HTTP MCP servers, so GitTerm's task and browser tools are not \
                 attached to this chat.",
                self.config.agent.label()
            ));
            return Vec::new();
        }
        self.config
            .mcp_servers
            .iter()
            .map(|server| server.to_acp(self.config.agent))
            .collect()
    }

    async fn create_session(&mut self) -> Result<(), String> {
        self.phase = Phase::Creating;
        let cwd = self.config.cwd.display().to_string();
        let mcp_servers = self.mcp_servers();
        match self.config.resume.clone() {
            Some(id) if self.load_session => {
                self.call(
                    "session/load",
                    json!({"sessionId": id, "cwd": cwd, "mcpServers": mcp_servers}),
                    Call::LoadSession,
                )
                .await
            }
            resume => {
                if let Some(id) = resume {
                    self.error(format!(
                        "{} cannot resume sessions; session {id} starts over as a new session.",
                        self.config.agent.label()
                    ));
                }
                self.call(
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": mcp_servers}),
                    Call::NewSession,
                )
                .await
            }
        }
    }

    /// Queue the configured settings the agent does not already have.
    async fn begin_setup(&mut self) -> Result<(), String> {
        self.phase = Phase::Setup;
        let wanted = [
            (Setting::Model, self.config.model.clone()),
            (Setting::Mode, self.config.mode.clone()),
            (Setting::Effort, self.config.effort.clone()),
        ];
        for (setting, value) in wanted {
            let Some(value) = value else { continue };
            if self.settings.current(setting).as_deref() == Some(value.as_str()) {
                continue;
            }
            if !self.settings.supports(setting) {
                acp_log(&format!(
                    "{} has no {} setting; ignoring configured {value:?}",
                    self.config.agent.label(),
                    setting.label()
                ));
                continue;
            }
            self.setup.push_back((setting, Some(value)));
        }
        self.next_setup().await
    }

    async fn next_setup(&mut self) -> Result<(), String> {
        while let Some((setting, value)) = self.setup.pop_front() {
            let session_id = self.session_id.clone().unwrap_or_default();
            match self
                .settings
                .change_request(&session_id, setting, value.as_deref())
            {
                Ok((method, params)) => {
                    return self.call(method, params, Call::Setup(setting, value)).await;
                }
                Err(e) => self.error(format!(
                    "Could not start on {} {}: {e}",
                    setting.label(),
                    value.as_deref().unwrap_or("default")
                )),
            }
        }
        self.ready().await
    }

    async fn ready(&mut self) -> Result<(), String> {
        self.phase = Phase::Ready;
        let session_id = self.session_id.clone();
        acp_log(&format!(
            "{} session {} ready (model {:?}, mode {:?}, effort {:?})",
            self.config.agent.label(),
            session_id.as_deref().unwrap_or("?"),
            self.settings.current(Setting::Model),
            self.settings.current(Setting::Mode),
            self.settings.current(Setting::Effort),
        ));
        self.emit(HarnessEvent::Ready {
            session_id,
            permission_mode: self.settings.current(Setting::Mode),
            models: self.settings.page_models(),
            modes: self.settings.page_modes(),
        });
        // The tab and page start from the tab's config; where the agent
        // ended up elsewhere (a rejected setting), they follow the agent.
        if let (Some(_), Some(current)) =
            (&self.config.model, self.settings.current(Setting::Model))
        {
            if self.config.model.as_deref() != Some(current.as_str()) {
                self.emit(HarnessEvent::ModelChanged(current));
            }
        }
        if let (Some(_), Some(current)) = (&self.config.mode, self.settings.current(Setting::Mode))
        {
            if self.config.mode.as_deref() != Some(current.as_str()) {
                self.emit(HarnessEvent::PermissionModeChanged(current));
            }
        }
        if self.settings.supports(Setting::Effort)
            && self.config.effort != self.settings.current(Setting::Effort)
        {
            self.emit(HarnessEvent::EffortChanged {
                effort: self.settings.current(Setting::Effort),
                applied: None,
            });
        }
        self.pump_queue().await
    }

    async fn pump_queue(&mut self) -> Result<(), String> {
        if self.phase != Phase::Ready || self.in_turn {
            return Ok(());
        }
        let Some(prompt) = self.queue.pop_next() else {
            return Ok(());
        };
        self.send_prompt(prompt).await
    }

    async fn send_prompt(&mut self, prompt: UserPrompt) -> Result<(), String> {
        let session_id = self.session_id.clone().unwrap_or_default();
        if !prompt.images.is_empty() && !self.images {
            self.error(format!(
                "{} takes no images; this message went without its {} image(s).",
                self.config.agent.label(),
                prompt.images.len()
            ));
        }
        acp_log(&format!(
            "prompt ({} chars, {} images, id {:?})",
            prompt.text.len(),
            prompt.images.len(),
            prompt.id
        ));
        let blocks = prompt_blocks(&prompt, self.images);
        self.in_turn = true;
        self.interrupted = false;
        self.turn_message = prompt.id.clone();
        if let Some(id) = &prompt.id {
            self.emit(HarnessEvent::MessageLifecycle {
                id: id.clone(),
                state: MessageState::Started,
            });
        }
        self.emit(HarnessEvent::TurnStarted {
            session_id: Some(session_id.clone()),
            model: self.settings.current(Setting::Model),
        });
        let result = self
            .call(
                "session/prompt",
                json!({"sessionId": session_id, "prompt": blocks}),
                Call::Prompt,
            )
            .await;
        if let Err(e) = &result {
            self.end_turn(TurnStatus::Failed(e.clone()), Value::Null);
        }
        result
    }

    fn resolve_permissions(&mut self) {
        let ids: Vec<String> = self.permissions.drain().map(|(id, _)| id).collect();
        for request_id in ids {
            self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
        }
    }

    fn end_turn(&mut self, status: TurnStatus, usage: Value) {
        acp_log(&format!(
            "turn completed: {status:?} cost={:?}",
            self.mapper.cost_usd
        ));
        self.in_turn = false;
        self.interrupted = false;
        self.resolve_permissions();
        self.emit(HarnessEvent::TurnCompleted {
            status,
            usage,
            cost_usd: self.mapper.cost_usd,
        });
        if let Some(id) = self.turn_message.take() {
            self.emit(HarnessEvent::MessageLifecycle {
                id,
                state: MessageState::Completed,
            });
        }
    }

    async fn finish_history(&mut self) -> Result<(), String> {
        self.history_deadline = None;
        let entries = std::mem::take(&mut self.history).finish();
        acp_log(&format!(
            "session {} replayed {} history entries",
            self.session_id.as_deref().unwrap_or("?"),
            entries.len()
        ));
        self.emit(HarnessEvent::HistoryReplayed { entries });
        self.begin_setup().await
    }

    /// Stop after a failure that leaves no usable session.
    fn fail(&mut self, msg: String) {
        self.error(msg);
        self.phase = Phase::Failed;
        self.stdin = None;
    }

    async fn on_response(&mut self, id: u64, msg: &Value) -> Result<(), String> {
        let Some(call) = self.calls.remove(&id) else {
            acp_log(&format!("response to unknown request {id}"));
            return Ok(());
        };
        let error = msg.get("error").filter(|e| !e.is_null());
        let result = &msg["result"];
        match call {
            Call::Initialize => {
                if let Some(error) = error {
                    self.fail(format!(
                        "{} refused the ACP handshake: {}",
                        self.config.agent.label(),
                        rpc_error_text(error)
                    ));
                    return Ok(());
                }
                let caps = &result["agentCapabilities"];
                self.load_session = caps["loadSession"] == true;
                self.http_mcp = caps["mcpCapabilities"]["http"] == true;
                self.images = caps["promptCapabilities"]["image"] == true;
                acp_log(&format!(
                    "initialized {} {} (protocol {}, loadSession {}, http MCP {}, images {})",
                    result["agentInfo"]["name"].as_str().unwrap_or("?"),
                    result["agentInfo"]["version"].as_str().unwrap_or("?"),
                    result["protocolVersion"],
                    self.load_session,
                    self.http_mcp,
                    self.images
                ));
                self.create_session().await
            }
            Call::NewSession => {
                if let Some(error) = error {
                    let text = rpc_error_text(error);
                    let msg = if error["code"].as_i64() == Some(ERR_AUTH_REQUIRED) {
                        format!(
                            "{} could not start a session: {text}{}",
                            self.config.agent.label(),
                            match self.config.agent {
                                AcpAgent::Gemini => format!(
                                    ". Check the API key in the keychain item \"{GEMINI_KEYCHAIN_SERVICE}\"."
                                ),
                                AcpAgent::OpenCode =>
                                    ". Sign in with `opencode auth login` in a terminal.".to_string(),
                            }
                        )
                    } else {
                        format!(
                            "{} could not start a session: {text}",
                            self.config.agent.label()
                        )
                    };
                    self.fail(msg);
                    return Ok(());
                }
                let Some(id) = result["sessionId"].as_str() else {
                    self.fail(format!(
                        "{} answered session/new without a sessionId",
                        self.config.agent.label()
                    ));
                    return Ok(());
                };
                self.session_id = Some(id.to_string());
                self.settings = SessionSettings::from_result(result);
                self.begin_setup().await
            }
            Call::LoadSession => {
                if let Some(error) = error {
                    let resume = self.config.resume.take().unwrap_or_default();
                    self.error(format!(
                        "Could not resume {} session {resume}: {}. Starting a new session.",
                        self.config.agent.label(),
                        rpc_error_text(error)
                    ));
                    self.history = HistoryBuilder::default();
                    return self.create_session().await;
                }
                self.session_id = self.config.resume.clone();
                self.settings = SessionSettings::from_result(result);
                self.phase = Phase::History;
                self.history_deadline = Some(tokio::time::Instant::now() + HISTORY_QUIET);
                Ok(())
            }
            Call::Setup(setting, value) | Call::Change(setting, value) if error.is_some() => {
                let during_setup = self.phase == Phase::Setup;
                self.error(format!(
                    "Could not change the {} to {}: {}",
                    setting.label(),
                    value.as_deref().unwrap_or("default"),
                    rpc_error_text(error.unwrap_or(&Value::Null))
                ));
                if during_setup {
                    return self.next_setup().await;
                }
                Ok(())
            }
            Call::Setup(setting, value) | Call::Change(setting, value) => {
                let before = self.settings.clone();
                match result["configOptions"].as_array() {
                    Some(options) => self.settings.set_config_options(options.clone()),
                    None => {
                        if let Some(value) = &value {
                            self.settings.set_current(setting, value);
                        }
                    }
                }
                if self.phase == Phase::Setup {
                    return self.next_setup().await;
                }
                // Confirm what was asked for; a set_config_option reply
                // can also move another setting (a new model's efforts).
                let mut changes = settings_changes(&before, &self.settings);
                let confirmed = match setting {
                    Setting::Model => value.clone().map(HarnessEvent::ModelChanged),
                    Setting::Mode => value.clone().map(HarnessEvent::PermissionModeChanged),
                    Setting::Effort => Some(HarnessEvent::EffortChanged {
                        effort: value.clone(),
                        applied: None,
                    }),
                };
                if let Some(confirmed) = confirmed {
                    if !changes.contains(&confirmed) {
                        changes.retain(|c| {
                            std::mem::discriminant(c) != std::mem::discriminant(&confirmed)
                        });
                        changes.push(confirmed);
                    }
                }
                for change in changes {
                    self.emit(change);
                }
                Ok(())
            }
            Call::Prompt => {
                let (status, usage) = prompt_reply(msg, self.interrupted);
                self.end_turn(status, usage);
                self.pump_queue().await
            }
        }
    }

    async fn on_agent_request(&mut self, msg: &Value) -> Result<(), String> {
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg["id"].clone();
        if method != "session/request_permission" {
            acp_log(&format!(
                "agent called {method:?}, which GitTerm does not offer"
            ));
            return self
                .write(error_frame(
                    &id,
                    ERR_METHOD_NOT_FOUND,
                    &format!("GitTerm does not implement {method}"),
                ))
                .await;
        }
        let params = &msg["params"];
        let (items, tool_use_id, kind) = self.mapper.permission(params);
        for item in items {
            self.emit(item);
        }
        self.next_request += 1;
        let request_id = format!("acp_{}_{}", self.pid, self.next_request);
        acp_log(&format!(
            "permission request {request_id} for {tool_use_id}: {}",
            trim_for_log(&serde_json::to_string(&kind).unwrap_or_default(), 300)
        ));
        self.permissions.insert(
            request_id.clone(),
            PendingPermission {
                rpc_id: id,
                options: params["options"].as_array().cloned().unwrap_or_default(),
            },
        );
        self.emit(HarnessEvent::RuntimeRequest {
            request_id,
            tool_use_id,
            kind,
        });
        Ok(())
    }

    fn on_update(&mut self, params: &Value) {
        let update = &params["update"];
        for mapped in self.mapper.map(update) {
            if self.phase == Phase::History || self.phase == Phase::Creating {
                match mapped {
                    Mapped::Event(ev) => {
                        self.history.event(ev);
                        self.extend_history();
                    }
                    Mapped::UserChunk { message_id, text } => {
                        self.history.user_chunk(message_id, &text);
                        self.extend_history();
                    }
                    Mapped::Mode(mode) => self.settings.set_current(Setting::Mode, &mode),
                    Mapped::ConfigOptions(options) => self.settings.set_config_options(options),
                }
                continue;
            }
            match mapped {
                Mapped::Event(ev) => self.emit(ev),
                Mapped::UserChunk { .. } => {}
                Mapped::Mode(mode) => {
                    if self.settings.current(Setting::Mode).as_deref() != Some(mode.as_str()) {
                        self.settings.set_current(Setting::Mode, &mode);
                        self.emit(HarnessEvent::PermissionModeChanged(mode));
                    }
                }
                Mapped::ConfigOptions(options) => {
                    let before = self.settings.clone();
                    self.settings.set_config_options(options);
                    if self.phase == Phase::Ready {
                        for change in settings_changes(&before, &self.settings) {
                            self.emit(change);
                        }
                    }
                }
            }
        }
    }

    fn extend_history(&mut self) {
        if self.history_deadline.is_some() {
            self.history_deadline = Some(tokio::time::Instant::now() + HISTORY_QUIET);
        }
    }

    async fn on_line(&mut self, line: &str) -> Result<(), String> {
        if let Some(wire) = self.wire.as_mut() {
            if let Err(e) = writeln!(wire.stdout, "{line}") {
                acp_log(&format!("wire log write failed: {e}"));
            }
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                acp_log(&format!("non-JSON stdout: {}", trim_for_log(line, 200)));
                return Ok(());
            }
        };
        let has_method = msg["method"].is_string();
        let has_id = msg.get("id").is_some_and(|id| !id.is_null());
        match (has_method, has_id) {
            (true, true) => self.on_agent_request(&msg).await,
            (true, false) => {
                let method = msg["method"].as_str().unwrap_or("");
                if method == "session/update" {
                    self.on_update(&msg["params"]);
                } else if self.logged_methods.insert(method.to_string()) {
                    acp_log(&format!(
                        "ignoring notification {method:?} (logged once per session)"
                    ));
                }
                Ok(())
            }
            (false, true) => match msg["id"].as_u64() {
                Some(id) => self.on_response(id, &msg).await,
                None => {
                    acp_log(&format!("response with a foreign id {}", msg["id"]));
                    Ok(())
                }
            },
            (false, false) => {
                acp_log(&format!("unrecognized frame: {}", trim_for_log(line, 200)));
                Ok(())
            }
        }
    }

    async fn on_command(&mut self, cmd: HarnessCommand) -> Result<(), String> {
        match cmd {
            HarnessCommand::SendUserMessage(prompt) => {
                if self.phase == Phase::Failed {
                    return Err(format!(
                        "Not sent: the {} session did not start.",
                        self.config.agent.label()
                    ));
                }
                let idle = self.phase == Phase::Ready && !self.in_turn;
                let (now, events) = self.queue.submit(prompt, idle);
                for ev in events {
                    self.emit(ev);
                }
                match now {
                    Some(prompt) => self.send_prompt(prompt).await,
                    None => {
                        acp_log(&format!("queued a prompt ({} waiting)", self.queue.len()));
                        Ok(())
                    }
                }
            }
            HarnessCommand::WithdrawQueuedMessage(id) => {
                let ev = self.queue.withdraw(&id);
                acp_log(&format!("withdraw {id}: {ev:?}"));
                self.emit(ev);
                Ok(())
            }
            HarnessCommand::Answer {
                request_id,
                decision,
            } => {
                let Some(pending) = self.permissions.remove(&request_id) else {
                    return Err(format!(
                        "no pending permission request {request_id} (already answered or withdrawn)"
                    ));
                };
                match permission_outcome(&decision, &pending.options) {
                    Ok(outcome) => {
                        acp_log(&format!("answer {request_id}: {outcome}"));
                        let written = self.write(response_frame(&pending.rpc_id, outcome)).await;
                        self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                        written
                    }
                    Err(e) => {
                        // Keep it answerable; the page can pick again.
                        self.permissions.insert(request_id, pending);
                        Err(e)
                    }
                }
            }
            HarnessCommand::Interrupt => {
                if !self.in_turn {
                    acp_log("interrupt with no turn running; nothing to cancel");
                    return Ok(());
                }
                acp_log("session/cancel");
                self.interrupted = true;
                let session_id = self.session_id.clone().unwrap_or_default();
                let result = self
                    .write(notification_frame(
                        "session/cancel",
                        json!({"sessionId": session_id}),
                    ))
                    .await;
                // The spec has the client answer every open permission
                // request of the cancelled turn with `cancelled`.
                let open: Vec<(String, PendingPermission)> = self.permissions.drain().collect();
                for (request_id, pending) in open {
                    self.write(response_frame(
                        &pending.rpc_id,
                        json!({"outcome": {"outcome": "cancelled"}}),
                    ))
                    .await?;
                    self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                }
                result
            }
            HarnessCommand::SetPermissionMode(mode) => self.change(Setting::Mode, Some(mode)).await,
            HarnessCommand::SetModel(model) => self.change(Setting::Model, Some(model)).await,
            HarnessCommand::SetEffort(effort) => self.change(Setting::Effort, effort).await,
            HarnessCommand::Shutdown => {
                acp_log(&format!("shutdown requested (pid {})", self.pid));
                self.stdin = None;
                Ok(())
            }
        }
    }

    async fn change(&mut self, setting: Setting, value: Option<String>) -> Result<(), String> {
        if self.phase != Phase::Ready {
            return Err(format!(
                "The {} session is still starting; change the {} once it is ready.",
                self.config.agent.label(),
                setting.label()
            ));
        }
        let session_id = self.session_id.clone().unwrap_or_default();
        let (method, params) = self
            .settings
            .change_request(&session_id, setting, value.as_deref())
            .map_err(|e| {
                format!(
                    "Could not change the {} to {}: {e}",
                    setting.label(),
                    value.as_deref().unwrap_or("default")
                )
            })?;
        self.call(method, params, Call::Change(setting, value))
            .await
    }
}

async fn run_session(
    config: AcpSessionConfig,
    mut commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) {
    let label = config.agent.label();
    let fail = |msg: String| {
        acp_log(&msg);
        let _ = events.send(HarnessEvent::Error(msg));
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
    };
    let program = match resolve_program(config.agent.program()) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let agent_env = match prepare_agent_env(&config).await {
        Ok(env) => env,
        Err(e) => return fail(e),
    };
    let args = config.args();
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("PATH", super::claude::child_path());
    // Secrets (the Gemini key, MCP tokens) travel only through the
    // environment and stdin.
    cmd.envs(agent_env.iter().map(|(k, v)| (k, v)));
    cmd.envs(config.env.iter().map(|(k, v)| (k, v)));
    acp_log(&format!(
        "spawn: {} {} (cwd {}, env {:?})",
        program.display(),
        args.join(" "),
        config.cwd.display(),
        agent_env
            .iter()
            .chain(config.env.iter())
            .map(|(k, _)| k)
            .collect::<Vec<_>>()
    ));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return fail(format!(
                "failed to start {} in {}: {e}",
                program.display(),
                config.cwd.display()
            ))
        }
    };
    let pid = child.id().unwrap_or(0);
    let (Some(stdout), Some(stderr), Some(stdin)) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take())
    else {
        return fail(format!("the {label} child is missing a stdio pipe"));
    };
    let wire = config.wire_log_dir.as_deref().and_then(|dir| {
        match WireLog::open(dir, config.agent, pid) {
            Ok(w) => Some(w),
            Err(e) => {
                acp_log(&e);
                None
            }
        }
    });
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
    {
        let tail = stderr_tail.clone();
        let agent = config.agent.program();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        acp_log(&format!(
                            "{agent} stderr[{pid}]: {}",
                            trim_for_log(&line, 400)
                        ));
                        if let Ok(mut t) = tail.lock() {
                            if t.len() == STDERR_TAIL_LINES {
                                t.pop_front();
                            }
                            t.push_back(line);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        acp_log(&format!("{agent} stderr[{pid}] read failed: {e}"));
                        break;
                    }
                }
            }
        });
    }

    let mut session = Session {
        config,
        stdin: Some(stdin),
        wire,
        events,
        pid,
        next_id: 0,
        calls: HashMap::new(),
        phase: Phase::Initializing,
        session_id: None,
        settings: SessionSettings::default(),
        mapper: UpdateMapper::new(),
        history: HistoryBuilder::default(),
        history_deadline: None,
        setup: VecDeque::new(),
        load_session: false,
        http_mcp: false,
        images: false,
        queue: PromptQueue::default(),
        in_turn: false,
        turn_message: None,
        interrupted: false,
        permissions: HashMap::new(),
        next_request: 0,
        logged_methods: HashSet::new(),
    };
    if let Err(e) = session
        .call(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {"name": "gitterm", "version": env!("CARGO_PKG_VERSION")},
            }),
            Call::Initialize,
        )
        .await
    {
        session.error(e);
    }

    let mut lines = BufReader::new(stdout).lines();
    let mut shutting_down = false;
    let mut kill_deadline: Option<tokio::time::Instant> = None;
    loop {
        if session.stdin.is_none() && kill_deadline.is_none() {
            // Shutdown, a dropped handle or a failed start: the agent gets
            // a grace period to exit on its closed stdin.
            shutting_down = true;
            kill_deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
        }
        let history_deadline = session.history_deadline;
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        session.error(format!("read {label} stdout: {e}"));
                        break;
                    }
                };
                if let Err(e) = session.on_line(&line).await {
                    session.error(e);
                }
            }
            cmd = commands.recv(), if !shutting_down => {
                let Some(cmd) = cmd else {
                    // Handle dropped without an explicit Shutdown.
                    session.stdin = None;
                    continue;
                };
                if let Err(e) = session.on_command(cmd).await {
                    session.error(e);
                }
            }
            _ = tokio::time::sleep_until(history_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if history_deadline.is_some() => {
                if let Err(e) = session.finish_history().await {
                    session.error(e);
                }
            }
            _ = tokio::time::sleep_until(kill_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if kill_deadline.is_some() => {
                acp_log(&format!("{label} {pid} did not exit after stdin closed; killing"));
                if let Err(e) = child.start_kill() {
                    acp_log(&format!("kill {label} {pid}: {e}"));
                }
                break;
            }
        }
    }

    let code = match child.wait().await {
        Ok(status) => status.code(),
        Err(e) => {
            acp_log(&format!("wait for {label} {pid}: {e}"));
            None
        }
    };
    acp_log(&format!("{label} {pid} exited with {code:?}"));
    let failed_start = session.phase == Phase::Failed;
    if !shutting_down && !failed_start && code != Some(0) {
        let tail = stderr_tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        session.emit(HarnessEvent::Error(if tail.is_empty() {
            format!("{label} exited unexpectedly (code {code:?})")
        } else {
            format!("{label} exited unexpectedly (code {code:?}):\n{tail}")
        }));
    }
    if session.in_turn {
        session.end_turn(
            TurnStatus::Failed(format!("{label} exited mid-turn")),
            Value::Null,
        );
    }
    session.resolve_permissions();
    session.emit(HarnessEvent::ProcessExited { code });
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENCODE_TURN: &str =
        include_str!("../../tests/fixtures/acp/opencode_turn_bash_allow.jsonl");
    const OPENCODE_LOAD: &str = include_str!("../../tests/fixtures/acp/opencode_load_replay.jsonl");
    const GEMINI_NEW: &str = include_str!("../../tests/fixtures/acp/gemini_session_new.jsonl");
    const GEMINI_TURN: &str = include_str!("../../tests/fixtures/acp/gemini_turn_permission.jsonl");

    fn frames(text: &str) -> Vec<Value> {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Feed a recorded agent→client stream through the mapper the way the
    /// session does: updates map to events, permission requests to
    /// runtime requests.
    fn run(mapper: &mut UpdateMapper, text: &str) -> (Vec<HarnessEvent>, Vec<Value>) {
        let mut events = Vec::new();
        let mut options = Vec::new();
        for frame in frames(text) {
            match frame["method"].as_str() {
                Some("session/update") => {
                    for m in mapper.map(&frame["params"]["update"]) {
                        match m {
                            Mapped::Event(ev) => events.push(ev),
                            other => events.push(HarnessEvent::Error(format!("{other:?}"))),
                        }
                    }
                }
                Some("session/request_permission") => {
                    let (items, tool_use_id, kind) = mapper.permission(&frame["params"]);
                    events.extend(items);
                    events.push(HarnessEvent::RuntimeRequest {
                        request_id: "r".into(),
                        tool_use_id,
                        kind,
                    });
                    options = frame["params"]["options"].as_array().cloned().unwrap();
                }
                _ => {}
            }
        }
        (events, options)
    }

    #[test]
    fn opencode_turn_maps_thinking_tool_permission_and_text() {
        let mut mapper = UpdateMapper::new();
        let (events, options) = run(&mut mapper, OPENCODE_TURN);
        assert!(matches!(&events[0], HarnessEvent::ThinkingDelta(t) if t.contains("Analyzing")));
        assert_eq!(
            events[1],
            HarnessEvent::ItemStarted {
                id: "call_533277".into(),
                kind: ItemKind::ToolCall {
                    name: "shell".into(),
                    input: json!({"cwd": "/tmp/acp-work"}),
                },
            }
        );
        // The command arrives with the first update and replaces the input
        // once; repeats of it add nothing.
        assert_eq!(
            events[2],
            HarnessEvent::ItemInputDelta {
                id: "call_533277".into(),
                partial_json: json!({"command": "echo acp-capture", "cwd": "/tmp/acp-work"})
                    .to_string(),
            }
        );
        let HarnessEvent::RuntimeRequest {
            tool_use_id, kind, ..
        } = &events[3]
        else {
            panic!("{:?}", events[3]);
        };
        assert_eq!(tool_use_id, "call_533277");
        let RuntimeRequestKind::Permission {
            tool_name,
            input,
            suggestions,
            ..
        } = kind
        else {
            panic!("{kind:?}");
        };
        assert_eq!(tool_name, "shell");
        assert_eq!(input["command"], "echo acp-capture");
        assert_eq!(
            suggestions,
            &vec![
                json!({"type": "acp_option", "optionId": "always", "name": "Always allow", "kind": "allow_always"})
            ]
        );
        assert_eq!(
            events[4],
            HarnessEvent::ItemCompleted {
                id: "call_533277".into(),
                output: "acp-capture\n".into(),
                is_error: false,
            }
        );
        assert_eq!(events[5], HarnessEvent::TextDelta("acp-capture".into()));
        assert_eq!(events.len(), 6, "{events:?}");
        assert!(mapper
            .cost_usd
            .is_some_and(|cost| (cost - 0.01460715).abs() < 1e-12));

        // The answers pick the agent's own options.
        let allow = RuntimeDecision::Allow {
            updated_input: None,
            remember: None,
        };
        assert_eq!(
            permission_outcome(&allow, &options).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "once"}})
        );
        let always = RuntimeDecision::Allow {
            updated_input: None,
            remember: Some(suggestions[0].clone()),
        };
        assert_eq!(
            permission_outcome(&always, &options).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "always"}})
        );
        let deny = RuntimeDecision::Deny {
            message: "no".into(),
        };
        assert_eq!(
            permission_outcome(&deny, &options).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "reject"}})
        );
        // No reject option: the request is cancelled instead.
        assert_eq!(
            permission_outcome(&deny, &options[..2]).unwrap(),
            json!({"outcome": {"outcome": "cancelled"}})
        );
        assert!(permission_outcome(
            &RuntimeDecision::AnswerQuestion {
                answers: Default::default()
            },
            &options
        )
        .is_err());
        assert!(permission_outcome(&allow, &options[1..]).is_err());

        // The prompt result.
        let result = frames(OPENCODE_TURN).pop().unwrap();
        assert_eq!(
            turn_status(result["result"]["stopReason"].as_str(), false),
            TurnStatus::Completed
        );
        let usage = turn_usage(&result["result"]["usage"]);
        assert_eq!(usage["output_tokens"], 22);
        assert_eq!(usage["input_tokens"], 16985);
        assert_eq!(usage["thoughtTokens"], 282);
    }

    #[test]
    fn opencode_settings_come_from_config_options() {
        let new = &frames(OPENCODE_TURN)[0]["result"];
        let settings = SessionSettings::from_result(new);
        assert_eq!(
            settings.current(Setting::Model).as_deref(),
            Some("opencode/minimax-m3.1-flash-free")
        );
        assert_eq!(settings.current(Setting::Mode).as_deref(), Some("build"));
        // "default" effort is the model's own: no pinned level.
        assert_eq!(settings.current(Setting::Effort), None);
        assert!(settings.supports(Setting::Effort));
        assert_eq!(
            settings.effort_levels(),
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        let models = settings.page_models();
        assert!(models
            .iter()
            .any(|m| m["value"] == "openrouter/google/gemini-3.6-flash"
                && m["supportsEffort"] == true));
        let page_modes = settings.page_modes();
        let modes: Vec<&str> = page_modes
            .iter()
            .map(|m| m["value"].as_str().unwrap())
            .collect();
        assert_eq!(modes, vec!["build", "plan"]);
        assert_eq!(
            settings
                .change_request(
                    "s",
                    Setting::Model,
                    Some("openrouter/google/gemini-3.6-flash")
                )
                .unwrap(),
            (
                "session/set_config_option",
                json!({"sessionId": "s", "configId": "model", "value": "openrouter/google/gemini-3.6-flash"})
            )
        );
        assert_eq!(
            settings
                .change_request("s", Setting::Effort, None)
                .unwrap()
                .1,
            json!({"sessionId": "s", "configId": "effort", "value": "default"})
        );
        assert_eq!(
            settings
                .change_request("s", Setting::Mode, Some("plan"))
                .unwrap()
                .1["configId"],
            "mode"
        );
        assert!(settings
            .change_request("s", Setting::Mode, Some("yolo"))
            .is_err());
        // A set_config_option reply that moves the model confirms it.
        let mut after = settings.clone();
        after.set_config_options(
            frames(OPENCODE_TURN)[1]["result"]["configOptions"]
                .as_array()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            settings_changes(&settings, &after),
            vec![HarnessEvent::ModelChanged(
                "openrouter/google/gemini-3.6-flash".into()
            )]
        );
    }

    #[test]
    fn gemini_settings_use_models_and_modes() {
        let new = &frames(GEMINI_NEW)[0]["result"];
        let settings = SessionSettings::from_result(new);
        assert_eq!(
            settings.current(Setting::Model).as_deref(),
            Some("auto-gemini-3")
        );
        assert_eq!(settings.current(Setting::Mode).as_deref(), Some("default"));
        assert!(!settings.supports(Setting::Effort));
        let models = settings.page_models();
        assert_eq!(models.len(), 8);
        assert_eq!(models[2]["value"], "gemini-3.1-pro-preview-customtools");
        assert_eq!(models[2]["displayName"], "gemini-3.1-pro-preview");
        assert!(models[2].get("supportsEffort").is_none());
        assert_eq!(
            settings
                .change_request("g", Setting::Model, Some("gemini-2.5-flash"))
                .unwrap(),
            (
                "session/set_model",
                json!({"sessionId": "g", "modelId": "gemini-2.5-flash"})
            )
        );
        assert_eq!(
            settings
                .change_request("g", Setting::Mode, Some("plan"))
                .unwrap(),
            (
                "session/set_mode",
                json!({"sessionId": "g", "modeId": "plan"})
            )
        );
        assert!(settings
            .change_request("g", Setting::Effort, Some("high"))
            .unwrap_err()
            .contains("no effort"));
    }

    #[test]
    fn gemini_permission_starts_the_item_and_failed_updates_are_errors() {
        let mut mapper = UpdateMapper::new();
        let (events, options) = run(&mut mapper, GEMINI_TURN);
        // No tool_call before the permission: the request starts the item,
        // named by its title, with what the call says about itself.
        assert_eq!(
            events[1],
            HarnessEvent::ItemStarted {
                id: "run_shell_command-1792461000000-1".into(),
                kind: ItemKind::ToolCall {
                    name: "echo hi".into(),
                    input: json!({"title": "echo hi", "kind": "execute"}),
                },
            }
        );
        let HarnessEvent::RuntimeRequest { kind, .. } = &events[2] else {
            panic!("{:?}", events[2]);
        };
        let RuntimeRequestKind::Permission { suggestions, .. } = kind else {
            panic!();
        };
        assert_eq!(suggestions.len(), 1);
        assert_eq!(
            permission_outcome(
                &RuntimeDecision::Allow {
                    updated_input: None,
                    remember: None
                },
                &options
            )
            .unwrap()["outcome"]["optionId"],
            "proceed_once"
        );
        assert_eq!(
            events[3],
            HarnessEvent::ItemCompleted {
                id: "run_shell_command-1792461000000-1".into(),
                output: "hi".into(),
                is_error: false,
            }
        );
        assert_eq!(
            events[4],
            HarnessEvent::ItemStarted {
                id: "write_file-1792461000001-2".into(),
                kind: ItemKind::ToolCall {
                    name: "Writing to a.txt".into(),
                    input: json!({"title": "Writing to a.txt", "kind": "edit", "paths": ["/tmp/a.txt"]}),
                },
            }
        );
        assert_eq!(
            events[5],
            HarnessEvent::ItemCompleted {
                id: "write_file-1792461000001-2".into(),
                output: "Permission denied".into(),
                is_error: true,
            }
        );
        assert_eq!(events[6], HarnessEvent::TextDelta("hi".into()));
        // `plan` is logged (once), not rendered; the mode update surfaces.
        assert!(matches!(&events[7], HarnessEvent::Error(m) if m.contains("autoEdit")));
        assert_eq!(events.len(), 8, "{events:?}");
        assert!(mapper.logged_kinds.contains("plan"));
        let mut unknown = UpdateMapper::new();
        assert!(unknown
            .map(&json!({"sessionUpdate": "brand_new_kind"}))
            .is_empty());
        assert!(unknown.logged_kinds.contains("brand_new_kind"));
    }

    #[test]
    fn load_replay_becomes_a_history_timeline() {
        let mut mapper = UpdateMapper::new();
        let mut history = HistoryBuilder::default();
        for frame in frames(OPENCODE_LOAD) {
            if frame["method"] != "session/update" {
                continue;
            }
            for m in mapper.map(&frame["params"]["update"]) {
                match m {
                    Mapped::Event(ev) => history.event(ev),
                    Mapped::UserChunk { message_id, text } => history.user_chunk(message_id, &text),
                    _ => {}
                }
            }
        }
        let entries = history.finish();
        assert!(
            matches!(&entries[0], TranscriptEntry::UserPrompt(t) if t.contains("echo acp-capture"))
        );
        assert!(matches!(
            &entries[1],
            TranscriptEntry::Harness(HarnessEvent::ThinkingDelta(_))
        ));
        // History reports the call already done: started and completed.
        assert!(matches!(
            &entries[2],
            TranscriptEntry::Harness(HarnessEvent::ItemStarted { kind: ItemKind::ToolCall { input, .. }, .. })
                if input["command"] == "echo acp-capture"
        ));
        assert!(matches!(
            &entries[3],
            TranscriptEntry::Harness(HarnessEvent::ItemCompleted { output, is_error: false, .. })
                if output == "acp-capture\n"
        ));
        assert_eq!(
            entries[4],
            TranscriptEntry::Harness(HarnessEvent::TextDelta("acp-capture".into()))
        );
        assert!(matches!(
            entries.last(),
            Some(TranscriptEntry::Harness(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }))
        ));
        assert_eq!(entries.len(), 6, "{entries:?}");
    }

    #[test]
    fn history_closes_each_turn_and_joins_chunks_of_one_message() {
        let mut history = HistoryBuilder::default();
        history.user_chunk(Some("u1".into()), "hel");
        history.user_chunk(Some("u1".into()), "lo");
        history.event(HarnessEvent::TextDelta("a".into()));
        history.event(HarnessEvent::TextDelta("b".into()));
        history.user_chunk(Some("u2".into()), "again");
        history.event(HarnessEvent::TextDelta("c".into()));
        let entries = history.finish();
        let done = TranscriptEntry::Harness(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Value::Null,
            cost_usd: None,
        });
        assert_eq!(
            entries,
            vec![
                TranscriptEntry::UserPrompt("hello".into()),
                TranscriptEntry::Harness(HarnessEvent::TextDelta("ab".into())),
                done.clone(),
                TranscriptEntry::UserPrompt("again".into()),
                TranscriptEntry::Harness(HarnessEvent::TextDelta("c".into())),
                done,
            ]
        );
    }

    #[test]
    fn a_model_api_error_fails_the_turn_with_the_agents_message() {
        // Gemini CLI 0.41.2 answers a failed model call with
        // `RequestError(status, message)` (gemini-KXTGCWBT.js, Session.prompt).
        let reply = json!({"jsonrpc": "2.0", "id": 5, "error": {
            "code": 402,
            "message": "[402 RESOURCE_EXHAUSTED] Your prepayment credits are depleted."}});
        let (status, usage) = prompt_reply(&reply, false);
        assert_eq!(
            status,
            TurnStatus::Failed(
                "[402 RESOURCE_EXHAUSTED] Your prepayment credits are depleted.".into()
            )
        );
        assert_eq!(usage, Value::Null);
        // After a cancel, the error is the cancel's.
        assert_eq!(prompt_reply(&reply, true).0, TurnStatus::Interrupted);
        let details = json!({"error": {"code": -32603, "message": "Internal error",
            "data": {"details": "Directory does not exist: /x"}}});
        assert_eq!(
            prompt_reply(&details, false).0,
            TurnStatus::Failed("Internal error (Directory does not exist: /x)".into())
        );
        let ok = json!({"result": {"stopReason": "end_turn", "usage": {"outputTokens": 3}}});
        assert_eq!(prompt_reply(&ok, false).0, TurnStatus::Completed);
        assert_eq!(prompt_reply(&ok, false).1["output_tokens"], 3);
    }

    #[test]
    fn stop_reasons_map_to_turn_status() {
        assert_eq!(turn_status(Some("end_turn"), false), TurnStatus::Completed);
        assert_eq!(
            turn_status(Some("cancelled"), false),
            TurnStatus::Interrupted
        );
        assert_eq!(turn_status(Some("refusal"), true), TurnStatus::Interrupted);
        assert!(matches!(
            turn_status(Some("max_tokens"), false),
            TurnStatus::Failed(_)
        ));
        assert!(matches!(turn_status(None, false), TurnStatus::Failed(_)));
        assert_eq!(turn_usage(&Value::Null), Value::Null);
    }

    #[test]
    fn mcp_servers_carry_their_headers_but_logs_and_debug_do_not() {
        let server = AcpMcpServer {
            name: "gitterm_tasks".into(),
            url: "http://127.0.0.1:1/mcp?caller=x".into(),
            headers: vec![("Authorization".into(), "Bearer secret-token".into())],
        };
        let gemini = server.to_acp(AcpAgent::Gemini);
        assert_eq!(gemini["name"], "gitterm-tasks");
        assert_eq!(gemini["type"], "http");
        assert_eq!(
            gemini["headers"],
            json!([{"name": "Authorization", "value": "Bearer secret-token"}])
        );
        assert_eq!(server.to_acp(AcpAgent::OpenCode)["name"], "gitterm_tasks");
        let frame = request_frame(
            2,
            "session/new",
            json!({"cwd": "/", "mcpServers": [gemini]}),
        );
        assert!(!redact_frame(&frame).to_string().contains("secret-token"));
        assert!(!format!("{server:?}").contains("secret-token"));
        let config = AcpSessionConfig {
            agent: AcpAgent::Gemini,
            cwd: "/".into(),
            resume: None,
            model: None,
            mode: None,
            effort: None,
            mcp_servers: vec![server],
            env: vec![("GEMINI_API_KEY".into(), "key-value".into())],
            support_dir: "/tmp/s".into(),
            wire_log_dir: None,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("key-value") && !debug.contains("secret-token"));
        assert_eq!(config.args(), vec!["--acp"]);
    }

    #[test]
    fn gemini_support_files_pin_api_key_auth() {
        let settings = gemini_system_settings(Some(json!({"general": {"vimMode": true}})));
        assert_eq!(settings["general"]["vimMode"], true);
        assert_eq!(
            settings["security"]["auth"]["selectedType"],
            "gemini-api-key"
        );
        assert_eq!(
            gemini_system_settings(None)["security"]["auth"]["selectedType"],
            "gemini-api-key"
        );
        assert!(gemini_missing_key_message("no such keychain item")
            .contains("security add-generic-password -s gemini-api-key"));
        assert_eq!(
            opencode_config_content(),
            json!({"permission": {"edit": "ask", "bash": "ask"}})
        );
    }

    fn prompt(id: &str) -> UserPrompt {
        UserPrompt {
            text: id.into(),
            images: Vec::new(),
            id: Some(id.into()),
        }
    }

    #[test]
    fn prompts_wait_for_the_turn_and_go_out_in_order() {
        let mut queue = PromptQueue::default();
        // Idle: straight out, no lifecycle noise.
        let (now, events) = queue.submit(prompt("a"), true);
        assert_eq!(now, Some(prompt("a")));
        assert!(events.is_empty());
        // Mid-turn: queued, and the page is told.
        let (now, events) = queue.submit(prompt("b"), false);
        assert_eq!(now, None);
        assert_eq!(
            events,
            vec![HarnessEvent::MessageLifecycle {
                id: "b".into(),
                state: MessageState::Queued
            }]
        );
        queue.submit(prompt("c"), false);
        // Even when idle, a newer prompt does not jump the queue.
        let (now, _) = queue.submit(prompt("d"), true);
        assert_eq!(now, None);
        assert_eq!(queue.len(), 3);
        assert_eq!(
            queue.withdraw("c"),
            HarnessEvent::MessageLifecycle {
                id: "c".into(),
                state: MessageState::Cancelled
            }
        );
        assert!(matches!(
            queue.withdraw("a"),
            HarnessEvent::MessageWithdrawRefused { .. }
        ));
        assert_eq!(queue.pop_next(), Some(prompt("b")));
        assert_eq!(queue.pop_next(), Some(prompt("d")));
        assert_eq!(queue.pop_next(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn prompts_carry_images_only_when_the_agent_takes_them() {
        let prompt = UserPrompt {
            text: "look".into(),
            images: vec![super::super::ImageAttachment {
                media_type: "image/png".into(),
                data: "QQ==".into(),
            }],
            id: None,
        };
        assert_eq!(
            prompt_blocks(&prompt, true),
            vec![
                json!({"type": "image", "mimeType": "image/png", "data": "QQ=="}),
                json!({"type": "text", "text": "look"}),
            ]
        );
        assert_eq!(
            prompt_blocks(&prompt, false),
            vec![json!({"type": "text", "text": "look"})]
        );
    }
}
