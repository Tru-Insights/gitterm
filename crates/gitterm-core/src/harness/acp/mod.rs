//! Neutral ACP protocol, settings, update mapping and in-memory replay/queue leaves.
//! Process ownership, environment construction and MCP wiring live in the host;
//! the MCP server descriptor (`AcpMcpServer`) is shared here.

use super::transcript::TranscriptEntry;
use crate::chat::history::MAX_RESULT_CHARS;
pub use crate::chat::types::{AcpAgent, GEMINI_REPLACED_NOTE};
use crate::chat::types::{
    HarnessEvent, ItemKind, MessageState, RuntimeDecision, RuntimeRequestKind, TurnStatus,
    UserPrompt,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};

/// The ACP protocol version GitTerm speaks.
pub const ACP_PROTOCOL_VERSION: u64 = 1;
/// ACP's "authentication required" error code.
pub const ERR_AUTH_REQUIRED: i64 = -32000;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;

pub fn acp_log(msg: &str) {
    eprintln!("[acp] {msg}");
}

pub fn trim_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

fn truncate_output(mut s: String) -> String {
    if crate::content::html_render::is_render_output(&s) {
        return s;
    }
    if s.len() > MAX_RESULT_CHARS {
        let mut end = MAX_RESULT_CHARS;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

pub fn supports_system_prompt_append(result: &Value) -> bool {
    result["agentCapabilities"]["_meta"]["antigravity"]["systemPrompt"]["append"] == true
}

pub fn auth_hint(agent: AcpAgent) -> &'static str {
    match agent {
        AcpAgent::Antigravity => ". Run `agy` in a terminal and sign in.",
        AcpAgent::OpenCode => ". Sign in with `opencode auth login` in a terminal.",
        AcpAgent::Gemini => GEMINI_REPLACED_NOTE,
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
    pub fn label(self) -> &'static str {
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
    antigravity: bool,
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

    pub fn for_agent(agent: AcpAgent, result: &Value) -> Self {
        let mut settings = Self::from_result(result);
        settings.antigravity = agent == AcpAgent::Antigravity;
        settings
    }

    fn option(&self, setting: Setting) -> Option<&Value> {
        // agy's configOptions model is a base id; models contains the full
        // effort-qualified ids accepted by session/set_model and the CLI.
        if self.antigravity && setting == Setting::Model {
            return None;
        }
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
        if self.antigravity && setting == Setting::Model {
            return true;
        }
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
        if self.antigravity && setting == Setting::Model {
            let value = value
                .filter(|v| crate::workers::is_model_name(v))
                .ok_or("Antigravity needs a well-formed model id")?;
            return Ok((
                "session/set_model",
                json!({"sessionId": session_id, "modelId": value}),
            ));
        }
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
        if self.antigravity {
            let base = self
                .config_options
                .iter()
                .find(|o| o["category"] == "model")
                .and_then(|o| o["currentValue"].as_str());
            let effort = self.current(Setting::Effort);
            if let (Some(base), Some(effort)) = (base, effort) {
                let full = format!("{base}-{effort}");
                // Only resolve names the agent actually paired; an unlisted
                // explicit id is retained until the agent confirms another.
                if self.models.iter().any(|m| m["modelId"] == full) {
                    self.current_model = Some(full);
                } else if self.models.iter().any(|m| m["modelId"] == base) {
                    self.current_model = Some(base.to_string());
                }
            }
        }
    }
}

/// The events for settings that differ between `before` and `after`.
pub fn settings_changes(before: &SessionSettings, after: &SessionSettings) -> Vec<HarnessEvent> {
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
                    rate_limit: None,
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

pub fn request_frame(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn notification_frame(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn response_frame(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_frame(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A JSON-RPC error object as a sentence.
pub fn rpc_error_text(error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("unknown error");
    match error["data"].get("details").and_then(Value::as_str) {
        Some(details) => format!("{message} ({details})"),
        None => match &error["data"] {
            Value::Null => message.to_string(),
            data => format!("{message} ({})", trim_for_log(&data.to_string(), 200)),
        },
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    const OPENCODE_TURN: &str =
        include_str!("../../../../../tests/fixtures/acp/opencode_turn_bash_allow.jsonl");
    const OPENCODE_LOAD: &str =
        include_str!("../../../../../tests/fixtures/acp/opencode_load_replay.jsonl");
    const GEMINI_NEW: &str =
        include_str!("../../../../../tests/fixtures/acp/gemini_session_new.jsonl");
    const GEMINI_TURN: &str =
        include_str!("../../../../../tests/fixtures/acp/gemini_turn_permission.jsonl");

    #[test]
    fn visual_results_keep_complete_html_with_or_without_raw_output() {
        let visual = json!({"html_render":{"html":"x".repeat(100_000),"height":600}});
        let output = visual.to_string();
        for update in [
            json!({"rawOutput":visual}),
            json!({"content":[{"type":"content","content":{"type":"text","text":output}}]}),
        ] {
            assert_eq!(tool_output(&update), output);
        }
        assert_eq!(truncate_output("x".repeat(100_000)).len(), MAX_RESULT_CHARS);
    }

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
            rate_limit: None,
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
    fn antigravity_models_use_full_ids_and_accept_an_unlisted_id() {
        let fixture = frames(include_str!(
            "../../../../../tests/fixtures/acp/antigravity_session_new.jsonl"
        ));
        let mut settings = SessionSettings::for_agent(AcpAgent::Antigravity, &fixture[0]["result"]);
        assert_eq!(
            settings.current(Setting::Model).as_deref(),
            Some("gemini-4-argon-medium")
        );
        assert!(settings
            .page_models()
            .iter()
            .any(|model| model["value"] == "gemini-4-argon-medium"));
        for model in ["gemini-4-argon-high", "new-plan-model-high"] {
            assert_eq!(
                settings
                    .change_request("synthetic", Setting::Model, Some(model))
                    .unwrap(),
                (
                    "session/set_model",
                    json!({"sessionId":"synthetic","modelId":model})
                )
            );
        }
        assert!(settings
            .change_request("s", Setting::Model, Some("bad id"))
            .is_err());
        assert!(settings
            .change_request("s", Setting::Mode, Some("yolo"))
            .is_err());
        assert!(settings
            .change_request("s", Setting::Effort, Some("invalid"))
            .is_err());
        let modes: Vec<_> = settings
            .page_modes()
            .iter()
            .map(|mode| mode["value"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(modes, ["default", "accept-edits", "auto", "plan"]);
        let mut options = fixture[0]["result"]["configOptions"]
            .as_array()
            .unwrap()
            .clone();
        options
            .iter_mut()
            .find(|o| o["category"] == "thought_level")
            .unwrap()["currentValue"] = json!("high");
        settings.set_config_options(options);
        assert_eq!(
            settings.current(Setting::Model).as_deref(),
            Some("gemini-4-argon-high")
        );
        settings.set_current(Setting::Model, "new-plan-model-high");
        assert_eq!(
            settings.current(Setting::Model).as_deref(),
            Some("new-plan-model-high")
        );
        let opencode = SessionSettings::from_result(&fixture[0]["result"]);
        assert!(opencode
            .change_request("s", Setting::Model, Some("new-plan-model-high"))
            .is_err());
    }

    #[test]
    fn recorded_antigravity_turn_maps_tools_permissions_and_reply() {
        let mut mapper = UpdateMapper::new();
        let (events, options) = run(
            &mut mapper,
            include_str!("../../../../../tests/fixtures/acp/antigravity_turn_tools.jsonl"),
        );
        assert!(events.iter().any(|e| matches!(e, HarnessEvent::ItemStarted { kind: ItemKind::ToolCall { name, .. }, .. } if name == "mcp_gitterm-tasks_task_list")));
        assert!(events.iter().any(|e| matches!(e, HarnessEvent::ItemStarted { kind: ItemKind::ToolCall { name, .. }, .. } if name == "mcp_gitterm-browser_browser_status")));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    HarnessEvent::ItemCompleted {
                        is_error: false,
                        ..
                    }
                ))
                .count(),
            2
        );
        assert!(events.iter().any(|e| matches!(e, HarnessEvent::TextDelta(text) if text.contains("native-append-token-17f2"))));
        assert!(permission_outcome(
            &RuntimeDecision::Allow {
                updated_input: None,
                remember: None
            },
            &options
        )
        .is_ok());
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
            images: vec![crate::chat::types::ImageAttachment {
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
