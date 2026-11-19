//! Claude stream-json parsing and frame encoding, independent of the process host.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::chat::types::{
    HarnessEvent, ItemKind, MessageState, RateLimitReading, RuntimeDecision, RuntimeRequestKind,
    TurnStatus, UserPrompt,
};

/// The tool whose `can_use_tool` request is a question for the user rather
/// than a permission prompt.
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";
/// A control request GitTerm sent to the CLI and is waiting on. The parser
/// matches `control_response` frames to these by `request_id`, never by
/// order (responses interleave with turn frames).
#[derive(Debug, Clone, PartialEq)]
pub enum HostRequest {
    /// The handshake. `resume` is the session id being resumed, if any.
    Initialize {
        resume: Option<String>,
    },
    Interrupt,
    SetPermissionMode(String),
    /// `set_model`; its success reply carries no payload.
    SetModel(String),
    /// `apply_flag_settings {effortLevel}`. Its success reply carries no
    /// payload and the CLI accepts unknown levels without applying them, so
    /// the session confirms with a `ConfirmEffort` read afterwards.
    SetEffort(Option<String>),
    /// `get_settings`, sent after a successful `SetEffort` to read back what
    /// the session's flag settings now hold.
    ConfirmEffort(Option<String>),
    /// `cancel_async_message {message_uuid}`: withdraw a queued user
    /// message. The reply is `{cancelled: bool}`; a successful cancel also
    /// arrives as a `command_lifecycle` `cancelled` frame.
    WithdrawMessage(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotHook {
    Submit,
    Tool,
    Stop,
}

/// One thing the parser found in a CLI stdout line.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedFrame {
    Event(HarnessEvent),
    /// A `can_use_tool` request. The session keeps `input` so it can build
    /// the answer, and forwards `event` (a `RuntimeRequest`) to the UI.
    PermissionRequest {
        request_id: String,
        tool_use_id: String,
        input: Value,
        event: HarnessEvent,
    },
    /// A synchronous SDK callback, acknowledged only after capture finishes.
    SnapshotBoundary {
        request_id: String,
        session_id: String,
        hook: SnapshotHook,
    },
    /// The registered SessionStart callback, including clear and compact.
    WorkspaceContext {
        request_id: String,
    },
    /// Any other CLI-initiated control request; the session answers it the
    /// way the SDK does when no callback is configured.
    ControlRequest {
        request_id: String,
        subtype: String,
    },
    /// The CLI withdrew one of its pending requests.
    ControlCancel {
        request_id: String,
    },
    /// A host request completed; carried so the session can log it.
    HostRequestDone {
        request_id: String,
        request: HostRequest,
        result: Result<Value, String>,
    },
    /// The line was not JSON.
    Unparsed(String),
}

/// `can_use_tool` request body (CLI -> host). Typed control envelope;
/// tool input and suggestions stay passthrough.
#[derive(Deserialize, Debug)]
struct CanUseTool {
    tool_name: String,
    input: Value,
    #[serde(default)]
    permission_suggestions: Option<Vec<Value>>,
    tool_use_id: String,
    #[serde(default)]
    decision_reason: Option<String>,
}

/// Stateful line parser for the CLI's stdout. State is limited to what the
/// wire forces on us: stream deltas address tool calls by content-block
/// index, and control responses must be matched to host requests.
#[derive(Debug, Default)]
pub struct ClaudeFrameParser {
    /// Content block index -> tool_use id, for the current message.
    tool_blocks: HashMap<u64, String>,
    host_requests: HashMap<String, HostRequest>,
    /// Agent calls whose subagent reported `task_started` and has not
    /// finished -> whether the subagent has sent text since its last tool
    /// call. Background Bash commands are tasks too; only subagents'
    /// completions become `SubagentEvent`s.
    agent_tasks: HashMap<String, bool>,
    /// Subagent task id -> the Agent call it runs under. A finished
    /// background subagent resumed with `SendMessage` reports its
    /// `task_started` and `task_notification` under the SendMessage call's
    /// id, while its own frames keep the Agent call as their parent.
    agent_roots: HashMap<String, String>,
    /// `SendMessage` calls seen in this stream; one can resume a subagent.
    resume_calls: HashSet<String>,
    /// Resumed subagents whose Agent call ran before this process, so it is
    /// not in `agent_roots`. Their start waits for their first frame, which
    /// names the Agent call.
    pending_resumes: HashSet<String>,
    /// Last permission mode confirmed by the CLI, for deduplicating the
    /// status frame that follows an acknowledged set_permission_mode.
    permission_mode: Option<String>,
    /// The latest `rate_limit_event` since the last turn ended; the next
    /// `result` carries it out on its `TurnCompleted` (TRU-211).
    rate_limit: Option<RateLimitReading>,
}

impl ClaudeFrameParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an outgoing control request so its response can be matched.
    pub fn register_host_request(&mut self, request_id: String, request: HostRequest) {
        self.host_requests.insert(request_id, request);
    }

    pub fn parse_line(&mut self, line: &str) -> Vec<ParsedFrame> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![ParsedFrame::Unparsed(line.to_string())];
        };
        let str_at = |p: &str| v.pointer(p).and_then(Value::as_str);
        // Plan limits (TRU-211), whoever's API call they followed; the
        // latest frame wins.
        if str_at("/type") == Some("rate_limit_event") {
            if let Some(info) = v.get("rate_limit_info").filter(|info| info.is_object()) {
                self.rate_limit = Some(RateLimitReading {
                    observed_at: unix_now(),
                    info: info.clone(),
                });
            }
            return Vec::new();
        }
        // Subagent traffic (Agent tool) carries the parent's tool_use id.
        // It is wrapped in `SubagentEvent`s so the chat nests it under the
        // parent's tool card instead of mixing it into this turn.
        if let Some(parent) = v.get("parent_tool_use_id").and_then(Value::as_str) {
            let mut frames = parse_subagent_frame(parent, &v);
            if let Some(task) = v.get("agent_id").and_then(Value::as_str) {
                if self.pending_resumes.remove(task) {
                    self.agent_roots
                        .insert(task.to_string(), parent.to_string());
                    frames.insert(0, self.start_subagent(parent));
                }
            }
            if let Some(text_seen) = self.agent_tasks.get_mut(parent) {
                for frame in &frames {
                    if let ParsedFrame::Event(HarnessEvent::SubagentEvent { event, .. }) = frame {
                        match **event {
                            HarnessEvent::TextDelta(_) => *text_seen = true,
                            HarnessEvent::ItemStarted { .. } => *text_seen = false,
                            _ => {}
                        }
                    }
                }
            }
            return frames;
        }
        match str_at("/type").unwrap_or("") {
            "system" if str_at("/subtype") == Some("init") => {
                vec![ParsedFrame::Event(HarnessEvent::TurnStarted {
                    session_id: str_at("/session_id").map(str::to_string),
                    model: str_at("/model").map(str::to_string),
                })]
            }
            // Subagent lifecycle; `tool_use_id` is the parent's Agent call.
            // A resumed subagent's start and end carry the SendMessage call's
            // id; both are reported under the Agent call it runs under.
            "system" if str_at("/subtype") == Some("task_started") => {
                let Some(tool_use_id) = str_at("/tool_use_id") else {
                    return Vec::new();
                };
                if str_at("/task_type") != Some("local_agent") {
                    return Vec::new();
                }
                let task = str_at("/task_id");
                let resume = self.resume_calls.remove(tool_use_id);
                let root = match task.and_then(|t| self.agent_roots.get(t)) {
                    Some(root) => root.clone(),
                    None if resume => {
                        if let Some(task) = task {
                            self.pending_resumes.insert(task.to_string());
                        }
                        return Vec::new();
                    }
                    None => {
                        if let Some(task) = task {
                            self.agent_roots
                                .insert(task.to_string(), tool_use_id.to_string());
                        }
                        tool_use_id.to_string()
                    }
                };
                vec![self.start_subagent(&root)]
            }
            "system" if str_at("/subtype") == Some("task_notification") => {
                let Some(tool_use_id) = str_at("/tool_use_id") else {
                    return Vec::new();
                };
                let task = str_at("/task_id");
                if let Some(task) = task {
                    self.pending_resumes.remove(task);
                }
                let parent = match task.and_then(|t| self.agent_roots.get(t)) {
                    Some(root) => root.clone(),
                    None => tool_use_id.to_string(),
                };
                let parent = parent.as_str();
                let Some(text_seen) = self.agent_tasks.remove(parent) else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                // A foreground subagent's final message reaches the parent
                // only as the Agent tool result; the notification's summary
                // is that message, so it stands in as the subagent's text.
                match str_at("/summary") {
                    Some(summary) if !text_seen && !summary.is_empty() => out.push(subagent_event(
                        parent,
                        HarnessEvent::TextDelta(summary.to_string()),
                    )),
                    _ => {}
                }
                out.push(subagent_event(parent, task_completion(&v)));
                out
            }
            "stream_event" => self.parse_stream_event(&v["event"]),
            // Slash-command replies (`/model sonnet`, `/effort high`, ...) are
            // synthetic assistant messages with no stream deltas; surface
            // their text or the turn looks like it produced nothing.
            "assistant" if str_at("/message/model") == Some("<synthetic>") => {
                parse_synthetic_assistant(&v)
            }
            "user" => parse_tool_results(&v),
            "result" => {
                let mut event = parse_result(&v);
                if let HarnessEvent::TurnCompleted { rate_limit, .. } = &mut event {
                    *rate_limit = self.rate_limit.take();
                }
                vec![ParsedFrame::Event(event)]
            }
            // Queue state of a user message sent with a `uuid` (CLI
            // capability `msg_lifecycle_v1`). Other states are ignored.
            "command_lifecycle" => {
                let state = match str_at("/state") {
                    Some("queued") => MessageState::Queued,
                    Some("started") => MessageState::Started,
                    Some("completed") => MessageState::Completed,
                    Some("cancelled") => MessageState::Cancelled,
                    _ => return Vec::new(),
                };
                match str_at("/command_uuid") {
                    Some(id) => vec![ParsedFrame::Event(HarnessEvent::MessageLifecycle {
                        id: id.to_string(),
                        state,
                    })],
                    None => Vec::new(),
                }
            }
            "control_request" => parse_control_request(&v),
            "control_response" => self.parse_control_response(&v),
            "control_cancel_request" => match str_at("/request_id") {
                Some(id) => vec![ParsedFrame::ControlCancel {
                    request_id: id.to_string(),
                }],
                None => Vec::new(),
            },
            // The CLI reports permission-mode changes it makes on its own
            // (leaving plan mode after an approved plan, shift-tab in
            // another client) in status frames. Without this the chip and
            // the saved tab config keep claiming a mode the CLI left.
            "system" if str_at("/subtype") == Some("status") => match str_at("/permissionMode") {
                Some(mode) => self.permission_mode_changed(mode),
                None => Vec::new(),
            },
            "conversation_reset" => vec![ParsedFrame::Event(HarnessEvent::ConversationReset {
                trigger: str_at("/trigger").map(str::to_string),
            })],
            // assistant (duplicates the stream deltas),
            // session_state_changed, keep_alive, ...
            _ => Vec::new(),
        }
    }

    /// The start of a run of the subagent under Agent call `parent`.
    fn start_subagent(&mut self, parent: &str) -> ParsedFrame {
        self.agent_tasks.insert(parent.to_string(), false);
        subagent_event(
            parent,
            HarnessEvent::TurnStarted {
                session_id: None,
                model: None,
            },
        )
    }

    /// Record a confirmed permission mode and emit the change event once:
    /// repeated confirmations of the same mode are dropped so a requested
    /// change is not announced a second time by its status frame.
    fn permission_mode_changed(&mut self, mode: &str) -> Vec<ParsedFrame> {
        if self.permission_mode.as_deref() == Some(mode) {
            return Vec::new();
        }
        self.permission_mode = Some(mode.to_string());
        vec![ParsedFrame::Event(HarnessEvent::PermissionModeChanged(
            mode.to_string(),
        ))]
    }

    fn parse_stream_event(&mut self, event: &Value) -> Vec<ParsedFrame> {
        let index = event.get("index").and_then(Value::as_u64);
        let ev = |e| vec![ParsedFrame::Event(e)];
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                self.tool_blocks.clear();
                Vec::new()
            }
            "content_block_start" => {
                let block = &event["content_block"];
                match block.get("type").and_then(Value::as_str).unwrap_or("") {
                    "tool_use" => {
                        let (Some(id), Some(index)) = (block["id"].as_str(), index) else {
                            return ev(HarnessEvent::Error(format!(
                                "tool_use block without id or index: {block}"
                            )));
                        };
                        self.tool_blocks.insert(index, id.to_string());
                        if block["name"] == "SendMessage" {
                            self.resume_calls.insert(id.to_string());
                        }
                        ev(HarnessEvent::ItemStarted {
                            id: id.to_string(),
                            kind: ItemKind::ToolCall {
                                name: block["name"].as_str().unwrap_or("").to_string(),
                                input: block.get("input").cloned().unwrap_or(Value::Null),
                            },
                        })
                    }
                    "thinking" => ev(HarnessEvent::ThinkingDelta(
                        block["thinking"].as_str().unwrap_or("").to_string(),
                    )),
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => match delta["text"].as_str() {
                        Some(t) if !t.is_empty() => ev(HarnessEvent::TextDelta(t.to_string())),
                        _ => Vec::new(),
                    },
                    "thinking_delta" => match delta["thinking"].as_str() {
                        Some(t) if !t.is_empty() => ev(HarnessEvent::ThinkingDelta(t.to_string())),
                        _ => Vec::new(),
                    },
                    "input_json_delta" => {
                        let partial = delta["partial_json"].as_str().unwrap_or("");
                        let id = index.and_then(|i| self.tool_blocks.get(&i));
                        match id {
                            Some(id) if !partial.is_empty() => ev(HarnessEvent::ItemInputDelta {
                                id: id.clone(),
                                partial_json: partial.to_string(),
                            }),
                            _ => Vec::new(),
                        }
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn parse_control_response(&mut self, v: &Value) -> Vec<ParsedFrame> {
        let response = &v["response"];
        let Some(request_id) = response["request_id"].as_str() else {
            return vec![ParsedFrame::Event(HarnessEvent::Error(format!(
                "control_response without request_id: {v}"
            )))];
        };
        let Some(request) = self.host_requests.remove(request_id) else {
            // Not ours (or already handled); nothing to match.
            return Vec::new();
        };
        let result = if response["subtype"] == "success" {
            Ok(response.get("response").cloned().unwrap_or(Value::Null))
        } else {
            Err(response["error"]
                .as_str()
                .unwrap_or("control request failed without an error message")
                .to_string())
        };
        let mut out = Vec::new();
        match (&request, &result) {
            (HostRequest::Initialize { resume }, Ok(body)) => {
                out.push(ParsedFrame::Event(HarnessEvent::Ready {
                    session_id: resume.clone(),
                    permission_mode: body["current_permission_mode"].as_str().map(str::to_string),
                    models: body["models"].as_array().cloned().unwrap_or_default(),
                    modes: Vec::new(),
                    commands: body["commands"].as_array().cloned().unwrap_or_default(),
                }));
            }
            (HostRequest::Initialize { .. }, Err(e)) => out.push(ParsedFrame::Event(
                HarnessEvent::Error(format!("Claude initialize failed: {e}")),
            )),
            (HostRequest::Interrupt, Err(e)) => out.push(ParsedFrame::Event(HarnessEvent::Error(
                format!("Claude interrupt failed: {e}"),
            ))),
            (HostRequest::SetPermissionMode(mode), Err(e)) => out.push(ParsedFrame::Event(
                HarnessEvent::Error(format!("Claude rejected permission mode {mode:?}: {e}")),
            )),
            (HostRequest::SetPermissionMode(requested), Ok(body)) => {
                // The CLI echoes the mode it switched to; a success without
                // the echo still confirms the mode that was asked for.
                let mode = body["mode"].as_str().unwrap_or(requested).to_string();
                out.extend(self.permission_mode_changed(&mode));
            }
            (HostRequest::SetModel(model), Err(e)) => out.push(ParsedFrame::Event(
                HarnessEvent::Error(format!("Claude rejected model {model:?}: {e}")),
            )),
            (HostRequest::SetModel(model), Ok(_)) => {
                out.push(ParsedFrame::Event(HarnessEvent::ModelChanged(
                    model.clone(),
                )));
            }
            (HostRequest::SetEffort(effort), Err(e))
            | (HostRequest::ConfirmEffort(effort), Err(e)) => {
                out.push(ParsedFrame::Event(HarnessEvent::Error(format!(
                    "Claude did not change the effort to {}: {e}",
                    effort.as_deref().unwrap_or("auto")
                ))))
            }
            // The session follows up with `ConfirmEffort`.
            (HostRequest::SetEffort(_), Ok(_)) => {}
            (HostRequest::ConfirmEffort(requested), Ok(body)) => {
                out.push(ParsedFrame::Event(confirm_effort(requested, body)));
            }
            (HostRequest::Interrupt, Ok(_)) => {}
            // A cancel that took is reported by its `cancelled` lifecycle.
            (HostRequest::WithdrawMessage(_), Ok(body)) if body["cancelled"] == true => {}
            (HostRequest::WithdrawMessage(id), Ok(_)) => {
                out.push(ParsedFrame::Event(HarnessEvent::MessageWithdrawRefused {
                    id: id.clone(),
                    reason: "Claude had already picked it up".into(),
                }))
            }
            (HostRequest::WithdrawMessage(id), Err(e)) => {
                out.push(ParsedFrame::Event(HarnessEvent::MessageWithdrawRefused {
                    id: id.clone(),
                    reason: format!("Claude refused: {e}"),
                }))
            }
        }
        out.push(ParsedFrame::HostRequestDone {
            request_id: request_id.to_string(),
            request,
            result,
        });
        out
    }
}

/// Reads a `get_settings` reply after `apply_flag_settings {effortLevel}`.
/// The flag-settings source holds the session's pinned level (absent once it
/// is reset); `applied.effort` is what the model actually runs at.
fn confirm_effort(requested: &Option<String>, body: &Value) -> HarnessEvent {
    let pinned = body["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|source| source["source"] == "flagSettings")
        .and_then(|source| source["settings"]["effortLevel"].as_str())
        .map(str::to_string);
    if &pinned != requested {
        return HarnessEvent::Error(format!(
            "Claude did not apply effort {}: its session settings hold {}",
            requested.as_deref().unwrap_or("auto"),
            pinned.as_deref().unwrap_or("no effort level")
        ));
    }
    HarnessEvent::EffortChanged {
        effort: pinned,
        applied: body["applied"]["effort"].as_str().map(str::to_string),
    }
}

/// Wraps one normalized event as activity of the subagent whose Agent call
/// is `parent`.
fn subagent_event(parent: &str, event: HarnessEvent) -> ParsedFrame {
    ParsedFrame::Event(HarnessEvent::SubagentEvent {
        parent_tool_use_id: parent.to_string(),
        event: Box::new(event),
    })
}

/// A frame the subagent produced. The CLI sends subagents' turns as whole
/// `assistant` messages (one content block each) and `user` tool results;
/// any subagent `stream_event`s would duplicate those, so they are dropped,
/// which also keeps their block indexes away from the parent's.
fn parse_subagent_frame(parent: &str, v: &Value) -> Vec<ParsedFrame> {
    match v["type"].as_str().unwrap_or("") {
        "assistant" => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| {
                let event = match block["type"].as_str()? {
                    "text" => HarnessEvent::TextDelta(block["text"].as_str()?.to_string()),
                    "thinking" => {
                        HarnessEvent::ThinkingDelta(block["thinking"].as_str()?.to_string())
                    }
                    "tool_use" => HarnessEvent::ItemStarted {
                        id: block["id"].as_str()?.to_string(),
                        kind: ItemKind::ToolCall {
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        },
                    },
                    _ => return None,
                };
                Some(subagent_event(parent, event))
            })
            .collect(),
        "user" => parse_tool_results(v)
            .into_iter()
            .map(|frame| match frame {
                ParsedFrame::Event(event) => subagent_event(parent, event),
                other => other,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A subagent's `system/task_notification` as the end of its work. For a
/// background agent this, not the Agent tool result, is when it is done.
fn task_completion(v: &Value) -> HarnessEvent {
    let status = match v["status"].as_str().unwrap_or("") {
        "completed" => TurnStatus::Completed,
        "failed" | "error" => TurnStatus::Failed(
            v["summary"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("the subagent failed")
                .to_string(),
        ),
        _ => TurnStatus::Interrupted,
    };
    HarnessEvent::TurnCompleted {
        status,
        usage: v.get("usage").cloned().unwrap_or(Value::Null),
        cost_usd: None,
        rate_limit: None,
    }
}

fn parse_tool_results(v: &Value) -> Vec<ParsedFrame> {
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| {
            let id = b["tool_use_id"].as_str()?;
            Some(ParsedFrame::Event(HarnessEvent::ItemCompleted {
                id: id.to_string(),
                output: tool_result_text(&b["content"]),
                is_error: b["is_error"].as_bool().unwrap_or(false),
            }))
        })
        .collect()
}

/// tool_result content is either a string or an array of content blocks.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                Some(other) => format!("[{other} block]"),
                None => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Text blocks of a synthetic assistant message, as deltas.
fn parse_synthetic_assistant(v: &Value) -> Vec<ParsedFrame> {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .filter(|t| !t.is_empty())
                .map(|t| ParsedFrame::Event(HarnessEvent::TextDelta(t.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Seconds since the Unix epoch, now.
fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn parse_result(v: &Value) -> HarnessEvent {
    let subtype = v["subtype"].as_str().unwrap_or("");
    let is_error = v["is_error"].as_bool().unwrap_or(false);
    let terminal_reason = v["terminal_reason"].as_str().unwrap_or("");
    let status = if subtype == "success" && !is_error {
        TurnStatus::Completed
    } else if terminal_reason.starts_with("aborted") {
        TurnStatus::Interrupted
    } else {
        let message = v["result"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                v["errors"].as_array().map(|errs| {
                    errs.iter()
                        .map(|e| e.as_str().map(str::to_string).unwrap_or(e.to_string()))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("turn ended with {subtype} ({terminal_reason})"));
        TurnStatus::Failed(message)
    };
    HarnessEvent::TurnCompleted {
        status,
        usage: v.get("usage").cloned().unwrap_or(Value::Null),
        cost_usd: v["total_cost_usd"].as_f64(),
        rate_limit: None,
    }
}

fn parse_control_request(v: &Value) -> Vec<ParsedFrame> {
    let Some(request_id) = v["request_id"].as_str() else {
        return vec![ParsedFrame::Event(HarnessEvent::Error(format!(
            "control_request without request_id: {v}"
        )))];
    };
    let subtype = v.pointer("/request/subtype").and_then(Value::as_str);
    if subtype == Some("hook_callback") {
        if v["request"]["callback_id"] == "gitterm_workspace_context"
            && v["request"]["input"]["hook_event_name"] == "SessionStart"
        {
            return vec![ParsedFrame::WorkspaceContext {
                request_id: request_id.into(),
            }];
        }
        let hook = match (
            v["request"]["callback_id"].as_str(),
            v["request"]["input"]["hook_event_name"].as_str(),
        ) {
            (Some("gitterm_turn_start"), Some("UserPromptSubmit")) => Some(SnapshotHook::Submit),
            (Some("gitterm_turn_tool"), Some("PreToolUse")) => Some(SnapshotHook::Tool),
            (Some("gitterm_turn_stop"), Some("Stop")) => Some(SnapshotHook::Stop),
            _ => None,
        };
        if let (Some(hook), Some(session_id)) = (hook, v["request"]["input"]["session_id"].as_str())
        {
            return vec![ParsedFrame::SnapshotBoundary {
                request_id: request_id.into(),
                session_id: session_id.into(),
                hook,
            }];
        }
    }
    if subtype != Some("can_use_tool") {
        return vec![ParsedFrame::ControlRequest {
            request_id: request_id.to_string(),
            subtype: subtype.unwrap_or("").to_string(),
        }];
    }
    let req: CanUseTool = match serde_json::from_value(v["request"].clone()) {
        Ok(req) => req,
        Err(e) => {
            return vec![ParsedFrame::Event(HarnessEvent::Error(format!(
                "malformed can_use_tool request {request_id}: {e}"
            )))]
        }
    };
    let kind = if req.tool_name == ASK_USER_QUESTION_TOOL {
        RuntimeRequestKind::Question {
            questions: req.input.get("questions").cloned().unwrap_or(Value::Null),
        }
    } else {
        RuntimeRequestKind::Permission {
            tool_name: req.tool_name.clone(),
            input: req.input.clone(),
            suggestions: req.permission_suggestions.unwrap_or_default(),
            decision_reason: req.decision_reason,
        }
    };
    vec![ParsedFrame::PermissionRequest {
        request_id: request_id.to_string(),
        tool_use_id: req.tool_use_id.clone(),
        input: req.input,
        event: HarnessEvent::RuntimeRequest {
            request_id: request_id.to_string(),
            tool_use_id: req.tool_use_id,
            kind,
        },
    }]
}

// ---------------------------------------------------------------------------
// Outgoing frames
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ControlRequestOut<'a> {
    request_id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    request: Value,
}

#[derive(Serialize)]
struct ControlResponseOut {
    #[serde(rename = "type")]
    kind: &'static str,
    response: ControlResponseBody,
}

#[derive(Serialize)]
#[serde(tag = "subtype", rename_all = "lowercase")]
pub enum ControlResponseBody {
    Success { request_id: String, response: Value },
    Error { request_id: String, error: String },
}

/// The stream-json `user` line for one prompt. Without images the content
/// is a single text block (the shape every fixture records). With images it
/// is the text block first (left out when the text is empty, since the API
/// rejects empty text blocks) followed by one `image` block per attachment:
/// `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`.
/// A prompt with an `id` carries it as the frame's `uuid`, which turns on
/// the CLI's `command_lifecycle` reports for it.
pub fn user_message_frame(prompt: &UserPrompt) -> String {
    let mut content = Vec::with_capacity(1 + prompt.images.len());
    if prompt.images.is_empty() || !prompt.text.is_empty() {
        content.push(json!({"type": "text", "text": prompt.text}));
    }
    for image in &prompt.images {
        content.push(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": image.media_type, "data": image.data},
        }));
    }
    let mut frame = json!({
        "type": "user",
        "session_id": "",
        "message": {"role": "user", "content": content},
        "parent_tool_use_id": null
    });
    if let Some(id) = &prompt.id {
        frame["uuid"] = json!(id);
    }
    frame.to_string()
}

pub fn control_request_frame(request_id: &str, request: Value) -> String {
    serde_json::to_string(&ControlRequestOut {
        request_id,
        kind: "control_request",
        request,
    })
    .expect("control_request serializes")
}

pub fn control_response_frame(body: ControlResponseBody) -> String {
    serde_json::to_string(&ControlResponseOut {
        kind: "control_response",
        response: body,
    })
    .expect("control_response serializes")
}

/// The `PermissionResult` the SDK writes for a decision (`{...result,
/// toolUseID}`). `input` is the tool input the CLI sent with the request.
pub fn permission_result(
    decision: &RuntimeDecision,
    tool_use_id: &str,
    input: &Value,
) -> Result<Value, String> {
    Ok(match decision {
        RuntimeDecision::Allow {
            updated_input,
            remember,
        } => {
            let mut body = json!({
                "behavior": "allow",
                "toolUseID": tool_use_id,
                "updatedInput": updated_input.clone().unwrap_or_else(|| input.clone()),
            });
            if let Some(update) = remember {
                body["updatedPermissions"] = json!([update]);
            }
            body
        }
        RuntimeDecision::Deny { message } => json!({
            "behavior": "deny",
            "message": message,
            "toolUseID": tool_use_id,
        }),
        RuntimeDecision::AnswerQuestion { answers } => {
            let Value::Object(mut updated) = input.clone() else {
                return Err(format!(
                    "cannot answer questions: request input is not an object: {input}"
                ));
            };
            updated.insert("answers".to_string(), json!(answers));
            json!({
                "behavior": "allow",
                "toolUseID": tool_use_id,
                "updatedInput": Value::Object(updated),
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::types::ImageAttachment;

    #[test]
    fn text_only_prompt_keeps_the_single_text_block_line() {
        assert_eq!(
            user_message_frame(&"hi".into()),
            r#"{"message":{"content":[{"text":"hi","type":"text"}],"role":"user"},"parent_tool_use_id":null,"session_id":"","type":"user"}"#
        );
    }

    #[test]
    fn image_prompt_puts_text_first_then_base64_image_blocks() {
        let prompt = UserPrompt {
            text: "What colour?".into(),
            images: vec![
                ImageAttachment {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
                ImageAttachment {
                    media_type: "image/jpeg".into(),
                    data: "BBBB".into(),
                },
            ],
            id: None,
        };
        let v: Value = serde_json::from_str(&user_message_frame(&prompt)).unwrap();
        assert_eq!(
            v["message"]["content"],
            json!([
                {"type": "text", "text": "What colour?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
            ])
        );
        assert_eq!(v["type"], "user");
    }

    #[test]
    fn image_only_prompt_leaves_out_the_empty_text_block() {
        let prompt = UserPrompt {
            text: String::new(),
            images: vec![ImageAttachment {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
            id: None,
        };
        let v: Value = serde_json::from_str(&user_message_frame(&prompt)).unwrap();
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "image");
    }

    fn parse_fixture(parser: &mut ClaudeFrameParser, fixture: &str) -> Vec<ParsedFrame> {
        fixture.lines().flat_map(|l| parser.parse_line(l)).collect()
    }

    fn events(frames: &[ParsedFrame]) -> Vec<HarnessEvent> {
        frames
            .iter()
            .filter_map(|f| match f {
                ParsedFrame::Event(e) => Some(e.clone()),
                ParsedFrame::PermissionRequest { event, .. } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    const INIT_REPLY: &str =
        include_str!("../../../../tests/fixtures/claude/initialize_reply.jsonl");
    const TURN_TEXT: &str = include_str!("../../../../tests/fixtures/claude/turn_text.jsonl");
    const TURN_SLASH_COMMAND: &str =
        include_str!("../../../../tests/fixtures/claude/turn_slash_command.jsonl");
    // Captured from `/clear` in stream-json mode (TRU-206), tools trimmed.
    const TURN_CLEAR: &str = include_str!("../../../../tests/fixtures/claude/turn_clear.jsonl");
    const TURN_BASH_ALLOW: &str =
        include_str!("../../../../tests/fixtures/claude/turn_bash_allow.jsonl");
    const TURN_BASH_DENY: &str =
        include_str!("../../../../tests/fixtures/claude/turn_bash_deny.jsonl");
    const TURN_QUESTION: &str =
        include_str!("../../../../tests/fixtures/claude/turn_question.jsonl");
    const TURN_INTERRUPT: &str =
        include_str!("../../../../tests/fixtures/claude/turn_interrupt.jsonl");
    // Captured from `claude_harness_smoke --scenario midturn` (TRU-140): a
    // Bash turn with three messages sent after its first tool call starts
    // (…b, …c, then …d withdrawn at once via request gitterm_5166_2).
    // Trimmed to the lifecycle frames, the cancel reply, tool results and
    // the result.
    const TURN_MIDTURN_MESSAGES: &str =
        include_str!("../../../../tests/fixtures/claude/turn_midturn_messages.jsonl");
    const SET_PERMISSION_MODE_REPLY: &str =
        include_str!("../../../../tests/fixtures/claude/set_permission_mode_reply.jsonl");
    // Captured from `claude_harness_smoke --scenario model` (TRU-143):
    // set_model, apply_flag_settings low, get_settings, apply_flag_settings
    // null, get_settings. The get_settings replies keep only effortLevel
    // (the rest is the user's own settings).
    const MODEL_EFFORT_REPLIES: &str =
        include_str!("../../../../tests/fixtures/claude/model_effort_replies.jsonl");
    // Captured from `claude_harness_smoke --scenario review` (TRU-142):
    // consecutive deltas on one block merged, signatures and paths redacted.
    const TURN_SUBAGENT_REVIEW: &str =
        include_str!("../../../../tests/fixtures/claude/turn_subagent_review.jsonl");
    const TURN_SUBAGENT_BACKGROUND: &str =
        include_str!("../../../../tests/fixtures/claude/turn_subagent_background.jsonl");

    /// The subagent events for `parent`, unwrapped, plus the number of
    /// subagent events addressed to any other parent.
    fn nested(evs: &[HarnessEvent], parent: &str) -> (Vec<HarnessEvent>, usize) {
        let mut mine = Vec::new();
        let mut others = 0;
        for ev in evs {
            if let HarnessEvent::SubagentEvent {
                parent_tool_use_id,
                event,
            } = ev
            {
                if parent_tool_use_id == parent {
                    mine.push((**event).clone());
                } else {
                    others += 1;
                }
            }
        }
        (mine, others)
    }

    fn top_level_text(evs: &[HarnessEvent]) -> String {
        evs.iter()
            .filter_map(|e| match e {
                HarnessEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn initialize_reply_becomes_ready() {
        let mut p = ClaudeFrameParser::new();
        p.register_host_request(
            "probe_2_1".into(),
            HostRequest::Initialize {
                resume: Some("abc".into()),
            },
        );
        let frames = parse_fixture(&mut p, INIT_REPLY);
        let evs = events(&frames);
        let [HarnessEvent::Ready {
            session_id,
            permission_mode,
            models,
            commands,
            ..
        }] = evs.as_slice()
        else {
            panic!("expected one Ready, got {evs:?}");
        };
        assert_eq!(session_id.as_deref(), Some("abc"));
        assert_eq!(permission_mode.as_deref(), Some("default"));
        assert!(!models.is_empty());
        assert_eq!(commands[0]["name"], "deep-research");
        assert!(matches!(
            frames.last(),
            Some(ParsedFrame::HostRequestDone {
                request: HostRequest::Initialize { .. },
                result: Ok(_),
                ..
            })
        ));
        // A response nobody asked for is ignored.
        assert!(parse_fixture(&mut p, INIT_REPLY).is_empty());
    }

    #[test]
    fn slash_command_reply_surfaces_synthetic_text() {
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_SLASH_COMMAND));
        assert!(evs.contains(&HarnessEvent::TextDelta(
            "Set model to `Sonnet 5.5` for this session only".into()
        )));
        assert!(matches!(
            evs.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    #[test]
    fn clear_resets_then_starts_the_new_session() {
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_CLEAR));
        assert_eq!(
            evs[0],
            HarnessEvent::ConversationReset {
                trigger: Some("clear".into())
            }
        );
        assert!(matches!(
            &evs[1],
            HarnessEvent::TurnStarted { session_id: Some(id), .. }
                if id == "212e1a1e-350e-4933-97c9-dbb15c62178a"
        ));
    }

    #[test]
    fn text_turn_streams_deltas_and_completes() {
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_TEXT));
        assert_eq!(
            evs[0],
            HarnessEvent::TurnStarted {
                session_id: Some("5823851f-5473-48fe-8eef-559ab094b2ad".into()),
                model: Some("claude-haiku-5-5".into()),
            }
        );
        assert_eq!(evs[1], HarnessEvent::TextDelta("ready".into()));
        let HarnessEvent::TurnCompleted {
            status, cost_usd, ..
        } = &evs[2]
        else {
            panic!("expected TurnCompleted, got {:?}", evs[2]);
        };
        assert_eq!(*status, TurnStatus::Completed);
        assert!(cost_usd.unwrap() > 0.0);
        assert_eq!(evs.len(), 3, "{evs:?}");
    }

    #[test]
    fn rate_limit_event_rides_on_the_turns_completion() {
        let before = chrono::Utc::now().timestamp();
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_TEXT));
        let HarnessEvent::TurnCompleted {
            rate_limit: Some(reading),
            ..
        } = &evs[2]
        else {
            panic!("expected TurnCompleted with a rate limit, got {:?}", evs[2]);
        };
        assert!(reading.observed_at >= before);
        assert_eq!(reading.info["status"], "allowed");
        assert_eq!(
            reading.info["unifiedWindows"]["five_hour"]["utilization"],
            json!(0.03)
        );
        // Taken once: the next turn without a frame carries none.
        let evs = events(&parse_fixture(&mut p, TURN_SLASH_COMMAND));
        assert!(matches!(
            evs.last(),
            Some(HarnessEvent::TurnCompleted {
                rate_limit: None,
                ..
            })
        ));
        // The latest frame wins; a subagent's frame counts too; a frame
        // without an info object is ignored.
        let frame = |utilization: f64, parent: Option<&str>| {
            json!({
                "type": "rate_limit_event",
                "parent_tool_use_id": parent,
                "rate_limit_info": {"unifiedWindows": {"seven_day": {"utilization": utilization}}}
            })
            .to_string()
        };
        assert!(p.parse_line(&frame(0.1, None)).is_empty());
        assert!(p.parse_line(&frame(0.2, Some("toolu_agent"))).is_empty());
        assert!(p
            .parse_line(r#"{"type":"rate_limit_event","rate_limit_info":null}"#)
            .is_empty());
        let result = r#"{"type":"result","subtype":"success","is_error":false,"usage":{}}"#;
        let evs = events(&p.parse_line(result));
        let HarnessEvent::TurnCompleted {
            rate_limit: Some(reading),
            ..
        } = &evs[0]
        else {
            panic!("expected a rate limit, got {evs:?}");
        };
        assert_eq!(
            reading.info["unifiedWindows"]["seven_day"]["utilization"],
            json!(0.2)
        );
    }

    #[test]
    fn bash_turn_has_thinking_tool_item_permission_and_result() {
        let mut p = ClaudeFrameParser::new();
        let frames = parse_fixture(&mut p, TURN_BASH_ALLOW);
        let evs = events(&frames);
        assert!(evs.contains(&HarnessEvent::ThinkingDelta(String::new())));
        let tool_id = "toolu_0191EpUQ2DRnCvbK3qZBa9C1";
        assert!(evs.contains(&HarnessEvent::ItemStarted {
            id: tool_id.into(),
            kind: ItemKind::ToolCall {
                name: "Bash".into(),
                input: json!({}),
            },
        }));
        let input_json: String = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemInputDelta { id, partial_json } if id == tool_id => {
                    Some(partial_json.as_str())
                }
                _ => None,
            })
            .collect();
        let input: Value = serde_json::from_str(&input_json).unwrap();
        assert_eq!(input["command"], "touch probe-ok.txt");

        let (request_id, req_input) = frames
            .iter()
            .find_map(|f| match f {
                ParsedFrame::PermissionRequest {
                    request_id,
                    tool_use_id,
                    input,
                    event:
                        HarnessEvent::RuntimeRequest {
                            kind:
                                RuntimeRequestKind::Permission {
                                    tool_name,
                                    suggestions,
                                    ..
                                },
                            ..
                        },
                } => {
                    assert_eq!(tool_use_id, tool_id);
                    assert_eq!(tool_name, "Bash");
                    assert!(suggestions
                        .iter()
                        .any(|s| s["destination"] == "localSettings"));
                    Some((request_id.clone(), input.clone()))
                }
                _ => None,
            })
            .expect("Bash permission request");
        assert_eq!(request_id, "8bf8cc76-1397-45b9-92d3-ed723f651aa1");
        assert_eq!(req_input, input);

        assert!(evs.contains(&HarnessEvent::ItemCompleted {
            id: tool_id.into(),
            output: "(Bash completed with no output)".into(),
            is_error: false,
        }));
        assert!(matches!(
            evs.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    #[test]
    fn denied_tool_result_is_an_error_item() {
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_BASH_DENY));
        assert!(evs.contains(&HarnessEvent::ItemCompleted {
            id: "toolu_01KYpLmapCZBT1znMdSZY84T".into(),
            output: "PROBE-DENY: the user declined this command".into(),
            is_error: true,
        }));
    }

    #[test]
    fn ask_user_question_is_a_question_request() {
        let mut p = ClaudeFrameParser::new();
        let frames = parse_fixture(&mut p, TURN_QUESTION);
        let (input, questions) = frames
            .iter()
            .find_map(|f| match f {
                ParsedFrame::PermissionRequest {
                    request_id,
                    input,
                    event:
                        HarnessEvent::RuntimeRequest {
                            kind: RuntimeRequestKind::Question { questions },
                            ..
                        },
                    ..
                } => {
                    assert_eq!(request_id, "a257c7b1-6b11-4c63-b37f-5899197dc98a");
                    Some((input.clone(), questions.clone()))
                }
                _ => None,
            })
            .expect("AskUserQuestion request");
        assert_eq!(questions[0]["question"], "Do you prefer red or blue?");
        assert_eq!(questions[0]["options"][1]["label"], "Blue");

        // Answering builds {questions, answers} as updatedInput.
        let decision = RuntimeDecision::AnswerQuestion {
            answers: [("Do you prefer red or blue?".to_string(), "Blue".to_string())].into(),
        };
        let body = permission_result(&decision, "toolu_01Y2JN3z2xhafPBN9nM9uRwJ", &input).unwrap();
        assert_eq!(body["behavior"], "allow");
        assert_eq!(body["toolUseID"], "toolu_01Y2JN3z2xhafPBN9nM9uRwJ");
        assert_eq!(body["updatedInput"]["questions"], questions);
        assert_eq!(
            body["updatedInput"]["answers"]["Do you prefer red or blue?"],
            "Blue"
        );

        let evs = events(&frames);
        assert!(evs.iter().any(|e| matches!(
            e,
            HarnessEvent::ItemCompleted { output, is_error: false, .. }
                if output.contains("\"Blue\"")
        )));
    }

    #[test]
    fn interrupt_receipt_and_aborted_result() {
        let mut p = ClaudeFrameParser::new();
        p.register_host_request("probe_2_2".into(), HostRequest::Interrupt);
        let frames = parse_fixture(&mut p, TURN_INTERRUPT);
        assert!(frames.iter().any(|f| matches!(
            f,
            ParsedFrame::HostRequestDone {
                request: HostRequest::Interrupt,
                result: Ok(_),
                ..
            }
        )));
        let evs = events(&frames);
        assert!(evs.contains(&HarnessEvent::TextDelta("one".into())));
        assert!(matches!(
            evs.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
        // The "[Request interrupted by user]" echo is not an item.
        assert!(!evs
            .iter()
            .any(|e| matches!(e, HarnessEvent::ItemCompleted { .. })));
    }

    #[test]
    fn foreground_subagent_nests_under_its_agent_call() {
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_SUBAGENT_REVIEW));
        let agent = "toolu_01Y8qfELQtVXDWYpWrA2LVpU";
        assert!(evs.iter().any(|e| matches!(
            e,
            HarnessEvent::ItemStarted { id, kind: ItemKind::ToolCall { name, .. } }
                if id == agent && name == "Agent"
        )));
        let (sub, others) = nested(&evs, agent);
        assert_eq!(others, 0, "{evs:?}");
        let kinds: Vec<&str> = sub
            .iter()
            .map(|e| match e {
                HarnessEvent::TurnStarted { .. } => "started",
                HarnessEvent::ItemStarted { .. } => "item",
                HarnessEvent::ItemCompleted { .. } => "result",
                HarnessEvent::TextDelta(_) => "text",
                HarnessEvent::TurnCompleted { .. } => "completed",
                _ => "other",
            })
            .collect();
        // The brief the parent sent (a subagent `user` text frame) is not
        // an event; task_progress and task_updated add nothing.
        assert_eq!(kinds, ["started", "item", "result", "text", "completed"]);
        assert!(matches!(
            &sub[1],
            HarnessEvent::ItemStarted { id, kind: ItemKind::ToolCall { name, input } }
                if id == "toolu_01V55VdwsBegzZmpoUhcm6DG"
                    && name == "Bash"
                    && input["command"].as_str().unwrap().contains("git diff HEAD")
        ));
        assert!(matches!(
            &sub[2],
            HarnessEvent::ItemCompleted { id, output, is_error: false }
                if id == "toolu_01V55VdwsBegzZmpoUhcm6DG" && output.contains("values[1:]")
        ));
        // A foreground subagent's last message only exists as the
        // notification summary; it becomes the subagent's text.
        assert!(
            matches!(&sub[3], HarnessEvent::TextDelta(t) if t.starts_with("Verdict: needs_changes") && t.contains("F1 [P1] calc.py:4"))
        );
        assert!(matches!(
            &sub[4],
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                cost_usd: None,
                ..
            }
        ));
        // The subagent's text never reaches the parent's reply stream; the
        // parent relays the report itself.
        assert!(!top_level_text(&evs).contains("Model: haiku"));
        assert!(top_level_text(&evs).contains("F1"));
        assert!(evs.iter().any(|e| matches!(
            e,
            HarnessEvent::ItemCompleted { id, output, is_error: false }
                if id == agent && output.contains("F1 [P1] calc.py:4")
        )));
        assert!(matches!(
            evs.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    #[test]
    fn background_subagent_text_is_not_repeated_by_its_summary() {
        let mut p = ClaudeFrameParser::new();
        let evs = events(&parse_fixture(&mut p, TURN_SUBAGENT_BACKGROUND));
        let agent = "toolu_01Dd3gSqnmMHifQuUBFNNzgL";
        // The Agent call returns at once; the subagent outlives the turn.
        assert!(evs.iter().any(|e| matches!(
            e,
            HarnessEvent::ItemCompleted { id, output, .. }
                if id == agent && output.starts_with("Async agent launched")
        )));
        let (sub, others) = nested(&evs, agent);
        assert_eq!(others, 0);
        let tools: Vec<&str> = sub
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemStarted {
                    kind: ItemKind::ToolCall { name, .. },
                    ..
                } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tools, ["Bash", "Read"]);
        let texts: Vec<&String> = sub
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::TextDelta(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].starts_with("Verdict: needs_changes"));
        assert!(sub.contains(&HarnessEvent::ThinkingDelta(String::new())));
        assert!(matches!(
            sub.first(),
            Some(HarnessEvent::TurnStarted { .. })
        ));
        assert!(matches!(
            sub.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
        // Claude starts a follow-up turn on its own when the agent is done.
        let turns = evs
            .iter()
            .filter(|e| matches!(e, HarnessEvent::TurnStarted { .. }))
            .count();
        assert_eq!(turns, 2);
    }

    #[test]
    fn background_bash_tasks_are_not_subagents() {
        let mut p = ClaudeFrameParser::new();
        let started = r#"{"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"toolu_bash","task_type":"local_bash"}"#;
        let done = r#"{"type":"system","subtype":"task_notification","task_id":"b1","tool_use_id":"toolu_bash","status":"completed","summary":"exit 0"}"#;
        assert!(p.parse_line(started).is_empty());
        assert!(p.parse_line(done).is_empty());
        // A subagent that failed reports the summary as the failure.
        let started = r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"toolu_agent","task_type":"local_agent"}"#;
        let failed = r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_agent","status":"failed","summary":"API error"}"#;
        p.parse_line(started);
        let evs = events(&p.parse_line(failed));
        assert_eq!(
            evs.last(),
            Some(&HarnessEvent::SubagentEvent {
                parent_tool_use_id: "toolu_agent".into(),
                event: Box::new(HarnessEvent::TurnCompleted {
                    status: TurnStatus::Failed("API error".into()),
                    usage: Value::Null,
                    cost_usd: None,
                    rate_limit: None,
                }),
            })
        );
    }

    /// The parent and inner event type of each subagent event.
    fn sub_starts_and_ends(evs: &[HarnessEvent]) -> Vec<(String, &'static str)> {
        evs.iter()
            .filter_map(|e| match e {
                HarnessEvent::SubagentEvent {
                    parent_tool_use_id,
                    event,
                } => Some((
                    parent_tool_use_id.clone(),
                    match **event {
                        HarnessEvent::TurnStarted { .. } => "start",
                        HarnessEvent::TurnCompleted { .. } => "end",
                        HarnessEvent::ItemStarted { .. } => "tool",
                        _ => "other",
                    },
                )),
                _ => None,
            })
            .collect()
    }

    const SEND_MESSAGE_BLOCK: &str = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_send","name":"SendMessage","input":{}}},"parent_tool_use_id":null}"#;
    const RESUMED_TOOL: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_b","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":"toolu_agent","agent_id":"a1"}"#;

    #[test]
    fn a_subagent_resumed_by_send_message_runs_under_its_agent_call() {
        // Wire shape from a real session: the resumed run's start and end
        // carry the SendMessage call's id; its frames keep the Agent call's.
        let mut p = ClaudeFrameParser::new();
        let frames = [
            r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"toolu_agent","task_type":"local_agent"}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_agent","status":"completed","summary":"first"}"#,
            SEND_MESSAGE_BLOCK,
            r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"toolu_send","task_type":"local_agent"}"#,
            RESUMED_TOOL,
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_send","status":"completed","summary":"second"}"#,
        ];
        let evs = events(
            &frames
                .iter()
                .flat_map(|l| p.parse_line(l))
                .collect::<Vec<_>>(),
        );
        let agent = |kind| ("toolu_agent".to_string(), kind);
        assert_eq!(
            sub_starts_and_ends(&evs),
            vec![
                agent("start"),
                agent("other"),
                agent("end"),
                agent("start"),
                agent("tool"),
                // No text from the run: its summary stands in.
                agent("other"),
                agent("end"),
            ]
        );
    }

    #[test]
    fn a_subagent_resumed_from_before_this_process_starts_at_its_first_frame() {
        // The Agent call ran under an earlier CLI process, so only the
        // resumed run's first frame names it.
        let mut p = ClaudeFrameParser::new();
        p.parse_line(SEND_MESSAGE_BLOCK);
        let started = r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"toolu_send","task_type":"local_agent"}"#;
        assert!(p.parse_line(started).is_empty());
        let evs = events(&p.parse_line(RESUMED_TOOL));
        assert_eq!(
            sub_starts_and_ends(&evs),
            vec![
                ("toolu_agent".to_string(), "start"),
                ("toolu_agent".to_string(), "tool")
            ]
        );
        let done = r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_send","status":"completed","summary":"s"}"#;
        let evs = events(&p.parse_line(done));
        assert_eq!(
            sub_starts_and_ends(&evs).last(),
            Some(&("toolu_agent".to_string(), "end"))
        );
    }

    #[test]
    fn permission_results_match_the_sdk_shape() {
        let input = json!({"command": "touch x"});
        let suggestion = json!({"type": "addRules", "destination": "localSettings",
            "rules": [{"toolName": "Bash", "ruleContent": "touch x"}], "behavior": "allow"});
        let allow = permission_result(
            &RuntimeDecision::Allow {
                updated_input: None,
                remember: Some(suggestion.clone()),
            },
            "toolu_1",
            &input,
        )
        .unwrap();
        assert_eq!(
            allow,
            json!({"behavior": "allow", "toolUseID": "toolu_1", "updatedInput": input,
                "updatedPermissions": [suggestion]})
        );
        let deny = permission_result(
            &RuntimeDecision::Deny {
                message: "no".into(),
            },
            "toolu_1",
            &input,
        )
        .unwrap();
        assert_eq!(
            deny,
            json!({"behavior": "deny", "message": "no", "toolUseID": "toolu_1"})
        );
    }

    #[test]
    fn cli_initiated_permission_mode_change_updates_the_mode() {
        let mut p = ClaudeFrameParser::new();
        let status = concat!(
            r#"{"type":"system","subtype":"status","status":null,"#,
            r#""permissionMode":"default","uuid":"u1","session_id":"s1"}"#,
        );
        // The CLI left plan mode on its own (for example after an approved
        // plan): the status frame is the only notice GitTerm gets.
        assert_eq!(
            events(&p.parse_line(status)),
            vec![HarnessEvent::PermissionModeChanged("default".into())]
        );
        // The same mode again (keep-alive status) stays quiet.
        assert_eq!(events(&p.parse_line(status)), vec![]);
    }

    #[test]
    fn set_permission_mode_reply_confirms_the_mode() {
        let mut p = ClaudeFrameParser::new();
        p.register_host_request(
            "gitterm_5121_2".into(),
            HostRequest::SetPermissionMode("plan".into()),
        );
        let frames = parse_fixture(&mut p, SET_PERMISSION_MODE_REPLY);
        // The follow-up system/status frame does not confirm a second time.
        assert_eq!(
            events(&frames),
            vec![HarnessEvent::PermissionModeChanged("plan".into())]
        );
        assert!(matches!(
            frames.last(),
            Some(ParsedFrame::HostRequestDone {
                request: HostRequest::SetPermissionMode(_),
                result: Ok(_),
                ..
            })
        ));
    }

    #[test]
    fn set_model_reply_confirms_the_model() {
        let mut p = ClaudeFrameParser::new();
        p.register_host_request(
            "gitterm_30932_2".into(),
            HostRequest::SetModel("sonnet".into()),
        );
        let line = MODEL_EFFORT_REPLIES.lines().next().unwrap();
        let frames = p.parse_line(line);
        assert_eq!(
            events(&frames),
            vec![HarnessEvent::ModelChanged("sonnet".into())]
        );
        assert!(matches!(
            frames.last(),
            Some(ParsedFrame::HostRequestDone {
                request: HostRequest::SetModel(_),
                result: Ok(_),
                ..
            })
        ));
        // An unknown model is the CLI's error, not a confirmation.
        p.register_host_request("m2".into(), HostRequest::SetModel("nonsense".into()));
        let frames = p.parse_line(
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"m2","error":"Model 'nonsense' not found","error_code":"catalog_unknown"}}"#,
        );
        assert!(
            matches!(&frames[0], ParsedFrame::Event(HarnessEvent::Error(m)) if m.contains("not found"))
        );
    }

    #[test]
    fn effort_is_confirmed_by_the_settings_read_back() {
        let mut p = ClaudeFrameParser::new();
        let low = Some("low".to_string());
        p.register_host_request(
            "gitterm_30932_3".into(),
            HostRequest::SetEffort(low.clone()),
        );
        p.register_host_request(
            "gitterm_30932_4".into(),
            HostRequest::ConfirmEffort(low.clone()),
        );
        p.register_host_request("gitterm_30932_5".into(), HostRequest::SetEffort(None));
        p.register_host_request("gitterm_30932_6".into(), HostRequest::ConfirmEffort(None));
        let frames = parse_fixture(&mut p, MODEL_EFFORT_REPLIES);
        // The apply_flag_settings replies themselves confirm nothing; the
        // user's own effortLevel (userSettings) is not mistaken for the pin.
        assert_eq!(
            events(&frames),
            vec![
                HarnessEvent::EffortChanged {
                    effort: low.clone(),
                    applied: low.clone()
                },
                HarnessEvent::EffortChanged {
                    effort: None,
                    applied: Some("medium".into())
                },
            ]
        );
        // A level the CLI accepted but did not pin is reported, not shown.
        p.register_host_request(
            "c1".into(),
            HostRequest::ConfirmEffort(Some("bogus".into())),
        );
        let frames = p.parse_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"c1","response":{"effective":{},"sources":[{"source":"flagSettings","settings":{}}],"applied":{"effort":"low"}}}}"#,
        );
        assert!(
            matches!(&frames[0], ParsedFrame::Event(HarnessEvent::Error(m)) if m.contains("bogus"))
        );
    }

    #[test]
    fn prompt_id_rides_as_the_frame_uuid() {
        let prompt = UserPrompt {
            text: "hi".into(),
            images: Vec::new(),
            id: Some("00000000-0000-4000-8000-00000000000b".into()),
        };
        let v: Value = serde_json::from_str(&user_message_frame(&prompt)).unwrap();
        assert_eq!(v["uuid"], "00000000-0000-4000-8000-00000000000b");
        assert_eq!(
            v["message"]["content"],
            json!([{"type": "text", "text": "hi"}])
        );
        let plain: Value = serde_json::from_str(&user_message_frame(&"hi".into())).unwrap();
        assert!(plain.get("uuid").is_none());
    }

    #[test]
    fn midturn_messages_report_their_queue_state() {
        use crate::chat::types::MessageState::*;
        let mut parser = ClaudeFrameParser::new();
        parser.register_host_request(
            "gitterm_5166_2".into(),
            HostRequest::WithdrawMessage("00000000-0000-4000-8000-00000000000d".into()),
        );
        let frames = parse_fixture(&mut parser, TURN_MIDTURN_MESSAGES);
        let lifecycle: Vec<(char, MessageState)> = events(&frames)
            .into_iter()
            .filter_map(|ev| match ev {
                HarnessEvent::MessageLifecycle { id, state } => {
                    Some((id.chars().last().unwrap(), state))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lifecycle,
            [
                ('b', Queued),
                ('c', Queued),
                ('d', Queued),
                ('d', Cancelled),
                ('b', Started),
                ('c', Started),
                ('b', Completed),
                ('c', Completed),
            ]
        );
        // The cancel took: its lifecycle says so, the reply adds nothing.
        assert!(!events(&frames)
            .iter()
            .any(|ev| matches!(ev, HarnessEvent::MessageWithdrawRefused { .. })));
        assert!(frames.iter().any(|f| matches!(
            f,
            ParsedFrame::HostRequestDone {
                request: HostRequest::WithdrawMessage(_),
                result: Ok(_),
                ..
            }
        )));
        // Folded into the running turn: one result for all of it.
        let results = events(&frames)
            .iter()
            .filter(|ev| matches!(ev, HarnessEvent::TurnCompleted { .. }))
            .count();
        assert_eq!(results, 1);
    }

    #[test]
    fn a_withdraw_too_late_or_refused_is_reported() {
        let mut parser = ClaudeFrameParser::new();
        parser.register_host_request("w1".into(), HostRequest::WithdrawMessage("m1".into()));
        parser.register_host_request("w2".into(), HostRequest::WithdrawMessage("m2".into()));
        let frames = parse_fixture(
            &mut parser,
            concat!(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"w1","response":{"cancelled":false}}}"#,
                "\n",
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"w2","error":"Unsupported control request subtype: cancel_async_message"}}"#,
            ),
        );
        assert_eq!(
            events(&frames),
            [
                HarnessEvent::MessageWithdrawRefused {
                    id: "m1".into(),
                    reason: "Claude had already picked it up".into()
                },
                HarnessEvent::MessageWithdrawRefused {
                    id: "m2".into(),
                    reason:
                        "Claude refused: Unsupported control request subtype: cancel_async_message"
                            .into()
                },
            ]
        );
    }

    #[test]
    fn unknown_lifecycle_states_are_ignored() {
        let mut parser = ClaudeFrameParser::new();
        assert!(parser
            .parse_line(r#"{"type":"command_lifecycle","command_uuid":"m1","state":"folded"}"#)
            .is_empty());
    }

    #[test]
    fn control_errors_and_cancels() {
        let mut p = ClaudeFrameParser::new();
        p.register_host_request("m1".into(), HostRequest::SetPermissionMode("x".into()));
        let frames = p.parse_line(
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"m1","error":"bad mode"}}"#,
        );
        assert!(
            matches!(&frames[0], ParsedFrame::Event(HarnessEvent::Error(m)) if m.contains("bad mode"))
        );
        assert_eq!(
            p.parse_line(r#"{"type":"control_cancel_request","request_id":"r9"}"#),
            vec![ParsedFrame::ControlCancel {
                request_id: "r9".into()
            }]
        );
        assert_eq!(
            p.parse_line(r#"{"type":"control_request","request_id":"r1","request":{"subtype":"elicitation"}}"#),
            vec![ParsedFrame::ControlRequest {
                request_id: "r1".into(),
                subtype: "elicitation".into()
            }]
        );
        assert_eq!(
            p.parse_line("not json"),
            vec![ParsedFrame::Unparsed("not json".into())]
        );
    }
}
