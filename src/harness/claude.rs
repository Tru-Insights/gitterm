//! Native Claude Code session: one long-lived `claude` process driven over
//! the stream-json control protocol in both directions.
//!
//! Wire contract: `.plans/tru-140-claude-control-protocol.md` (Phase A).
//! `ClaudeFrameParser` turns CLI stdout lines into `HarnessEvent`s and is
//! pure (no IO), so it is unit-tested against recorded frames in
//! `tests/fixtures/claude/`. `ClaudeSession` owns the process on a
//! dedicated thread and maps `HarnessCommand`s onto stdin frames.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use super::{
    HarnessCommand, HarnessEvent, ItemKind, MessageState, RuntimeDecision, RuntimeRequestKind,
    TurnStatus, UserPrompt,
};
use crate::agentd::client::{
    ClaudeChatFrame, ClaudeChatOpen, ClaudeChatOpened, RemoteAgentBackend, RemoteAgentClientConfig,
};

/// The tool whose `can_use_tool` request is a question for the user rather
/// than a permission prompt.
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";
/// Control subtypes the Agent SDK deliberately leaves unanswered ("for the
/// machine serving this session's tools"), copied from sdk.mjs.
const SDK_UNANSWERED_SUBTYPES: [&str; 5] = [
    "remote_tool_call",
    "remote_plumbing_call",
    "remote_tools_probe",
    "remote_tools_reannounce",
    "request_user_dialog",
];
/// How long a closed-stdin `claude` gets to exit before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Lines of stderr kept for the exit report.
const STDERR_TAIL_LINES: usize = 20;

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

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
            "result" => vec![ParsedFrame::Event(parse_result(&v))],
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
            // assistant (duplicates the stream deltas), status,
            // session_state_changed, rate_limit_event, keep_alive, ...
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
                out.push(ParsedFrame::Event(HarnessEvent::PermissionModeChanged(
                    mode,
                )));
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
    }
}

fn parse_control_request(v: &Value) -> Vec<ParsedFrame> {
    let Some(request_id) = v["request_id"].as_str() else {
        return vec![ParsedFrame::Event(HarnessEvent::Error(format!(
            "control_request without request_id: {v}"
        )))];
    };
    let subtype = v.pointer("/request/subtype").and_then(Value::as_str);
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
enum ControlResponseBody {
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

fn control_request_frame(request_id: &str, request: Value) -> String {
    serde_json::to_string(&ControlRequestOut {
        request_id,
        kind: "control_request",
        request,
    })
    .expect("control_request serializes")
}

fn control_response_frame(body: ControlResponseBody) -> String {
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

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// One MCP server attached to a session through `--mcp-config`, mirroring
/// what GitTerm injects into terminal-launched `claude` commands.
#[derive(Clone)]
pub struct ClaudeMcpServer {
    /// The `{"mcpServers": {…}}` document passed as `--mcp-config=<json>`.
    /// Secrets must appear only as `${VAR}` references, which Claude expands
    /// from the child environment, never as literal values.
    pub config: Value,
    /// Tools pre-approved through `--allowedTools=`; empty adds no flag.
    pub allowed_tools: Vec<String>,
    /// Child environment the config's `${VAR}` references resolve from.
    /// Values may be secrets: they never reach argv, logs or `Debug`.
    pub env: Vec<(String, String)>,
}

impl std::fmt::Debug for ClaudeMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeMcpServer")
            .field("config", &self.config)
            .field("allowed_tools", &self.allowed_tools)
            .field(
                "env",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeSessionConfig {
    pub cwd: PathBuf,
    /// `None` leaves the model to the user's Claude settings.
    pub model: Option<String>,
    pub permission_mode: String,
    pub effort: Option<String>,
    /// Session id to continue (`--resume=<id>`).
    pub resume: Option<String>,
    /// When set, every stdin/stdout line is appended to
    /// `<dir>/claude-<pid>-{stdin,stdout}.jsonl`.
    pub wire_log_dir: Option<PathBuf>,
    /// GitTerm MCP servers to attach (task and browser controls).
    pub mcp_servers: Vec<ClaudeMcpServer>,
    /// Attach Claude in Chrome (`--chrome`). Headless Claude does not
    /// apply the CLI's own default for it.
    pub chrome: bool,
}

impl ClaudeSessionConfig {
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--include-partial-messages",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if self.chrome {
            args.push("--chrome".into());
        }
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        // Always explicit: a user-level defaultMode (e.g. auto) would
        // otherwise answer prompts before GitTerm sees them.
        args.push("--permission-mode".into());
        args.push(self.permission_mode.clone());
        if let Some(effort) = &self.effort {
            args.push("--effort".into());
            args.push(effort.clone());
        }
        // The `=` form keeps each JSON document a single value of these
        // variadic options; repeated flags accumulate, one pair per server.
        for server in &self.mcp_servers {
            args.push(format!("--mcp-config={}", server.config));
            if !server.allowed_tools.is_empty() {
                args.push(format!("--allowedTools={}", server.allowed_tools.join(",")));
            }
        }
        if let Some(id) = &self.resume {
            args.push(format!("--resume={id}"));
        }
        args
    }
}

/// Directories where `claude` is commonly installed but that a Finder-
/// launched app's PATH lacks. Mirrors the PATH GitTerm gives terminal tabs.
fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// The PATH for the child: the inherited PATH plus the usual install dirs.
pub(super) fn child_path() -> OsString {
    let mut entries: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for dir in extra_bin_dirs() {
        if !entries.contains(&dir) {
            entries.push(dir);
        }
    }
    std::env::join_paths(entries).unwrap_or_default()
}

/// Locate the `claude` executable on the child PATH.
pub fn resolve_claude_program() -> Result<PathBuf, String> {
    let path = child_path();
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "could not find the `claude` executable on PATH ({})",
                path.to_string_lossy()
            )
        })
}

/// Handle to a running Claude session. Dropping a local one shuts the
/// process down; dropping a remote one on an agent that owns its chats
/// only detaches (send `Shutdown` first to stop it).
pub struct ClaudeSession {
    commands: mpsc::UnboundedSender<HarnessCommand>,
    shutdown_on_drop: bool,
}

impl ClaudeSession {
    /// Start `claude` on a dedicated thread. Spawn failures arrive on the
    /// event channel as `Error` followed by `ProcessExited`.
    pub fn spawn(config: ClaudeSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let spawn_result = std::thread::Builder::new()
            .name("claude-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(HarnessEvent::Error(format!(
                            "could not start the Claude session runtime: {e}"
                        )));
                        let _ = ev_tx.send(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                };
                rt.block_on(run_session(config, cmd_rx, ev_tx));
            });
        if let Err(e) = spawn_result {
            eprintln!("[claude-harness] could not spawn session thread: {e}");
        }
        (
            Self {
                commands: cmd_tx,
                shutdown_on_drop: true,
            },
            ev_rx,
        )
    }

    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "the Claude session has exited".to_string())
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        if self.shutdown_on_drop {
            let _ = self.commands.send(HarnessCommand::Shutdown);
        }
    }
}

// ---------------------------------------------------------------------------
// Remote session (TRU-152)
// ---------------------------------------------------------------------------

/// The first message of a remote chat stream: the session config minus
/// what cannot cross machines. MCP servers point at this Mac's GitTerm and
/// the wire log is a local directory, so neither travels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteClaudeChatStart {
    /// A directory on the agent's machine.
    pub cwd: String,
    pub model: Option<String>,
    pub permission_mode: String,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub chrome: bool,
}

impl RemoteClaudeChatStart {
    /// The remote start for a desktop config. A config carrying MCP servers
    /// is refused rather than started without them.
    pub fn from_config(config: &ClaudeSessionConfig) -> Result<Self, String> {
        if !config.mcp_servers.is_empty() {
            return Err(format!(
                "{} MCP server(s) cannot be attached to a chat on a remote machine",
                config.mcp_servers.len()
            ));
        }
        if let Some(dir) = &config.wire_log_dir {
            remote_log(&format!(
                "wire log {} is local-only; a remote chat does not keep one",
                dir.display()
            ));
        }
        Ok(Self {
            cwd: config.cwd.to_string_lossy().into_owned(),
            model: config.model.clone(),
            permission_mode: config.permission_mode.clone(),
            effort: config.effort.clone(),
            resume: config.resume.clone(),
            chrome: config.chrome,
        })
    }

    /// The agent side's session config, run in `cwd` (the validated,
    /// canonical form of `self.cwd`).
    pub fn into_config(self, cwd: PathBuf) -> ClaudeSessionConfig {
        ClaudeSessionConfig {
            cwd,
            model: self.model,
            permission_mode: self.permission_mode,
            effort: self.effort,
            resume: self.resume,
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: self.chrome,
        }
    }
}

impl ClaudeSession {
    /// Start `claude` on a remote machine through its GitTerm agent's
    /// `ClaudeChat` stream, or attach to the chat already running there
    /// for `config.resume`. Same handle as `spawn`: commands go out over
    /// the stream and events come back on the channel.
    ///
    /// On an agent that owns its chats ("claude_chat_persistent") the chat
    /// outlives the connection: a lost connection is retried with backoff
    /// (capped at 30 s) for as long as the handle lives, resuming after the
    /// last event seen, and shows as `RemoteDisconnected` then
    /// `RemoteReconnected` (with `history_gap` when events were missed).
    /// Dropping the handle detaches and leaves the chat running; send
    /// `Shutdown` first to stop it.
    ///
    /// On an older agent the process lives as long as the stream: dropping
    /// the handle stops it, and a lost connection arrives as `Error`
    /// followed by `ProcessExited { code: None }`. Either way, failing to
    /// reach the agent at all does too.
    pub fn spawn_remote(
        client: RemoteAgentClientConfig,
        config: ClaudeSessionConfig,
    ) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        Self::remote(client, config, None)
    }

    /// Attach to chat `chat_id`, which `ListClaudeChats` reported running
    /// on an agent that owns its chats, without replay (the tab reads the
    /// transcript). Never starts a process: a chat that has stopped since
    /// arrives as `Error` then `ProcessExited { code: None }`, and the next
    /// message starts one as usual. `config` is the tab's own, for the
    /// start that follows such an exit. Otherwise as `spawn_remote`.
    pub fn attach_remote(
        client: RemoteAgentClientConfig,
        config: ClaudeSessionConfig,
        chat_id: String,
    ) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        Self::remote(client, config, Some(chat_id))
    }

    fn remote(
        client: RemoteAgentClientConfig,
        config: ClaudeSessionConfig,
        chat_id: Option<String>,
    ) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let spawn_result = std::thread::Builder::new()
            .name("claude-remote-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(HarnessEvent::Error(format!(
                            "could not start the remote Claude session runtime: {e}"
                        )));
                        let _ = ev_tx.send(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                };
                rt.block_on(run_remote_session(client, config, chat_id, cmd_rx, ev_tx));
            });
        if let Err(e) = spawn_result {
            remote_log(&format!("could not spawn remote session thread: {e}"));
        }
        // An older agent stops the process when the stream closes, which
        // dropping the handle does; a newer one keeps it running.
        (
            Self {
                commands: cmd_tx,
                shutdown_on_drop: false,
            },
            ev_rx,
        )
    }
}

fn remote_log(msg: &str) {
    eprintln!("[remote-chat] {msg}");
}

/// First wait before reconnecting a remote chat; doubles per failure.
const RECONNECT_FIRST_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// How long a dropped handle keeps reconnecting to deliver a `Shutdown`
/// the connection lost.
const SHUTDOWN_DELIVERY_WINDOW: Duration = Duration::from_secs(15);

/// Relay one remote chat: commands out as JSON, events back from JSON.
/// Picks the agent's protocol from its handshake. `chat_id`: attach to
/// that running chat instead of starting (or attaching by resume id).
async fn run_remote_session(
    client: RemoteAgentClientConfig,
    config: ClaudeSessionConfig,
    chat_id: Option<String>,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) {
    let remote = format!("{} ({})", client.name, client.endpoint);
    let fail = |msg: String| {
        remote_log(&msg);
        let _ = events.send(HarnessEvent::Error(msg));
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
    };
    let start = match RemoteClaudeChatStart::from_config(&config) {
        Ok(start) => start,
        Err(e) => return fail(format!("could not start Claude on {remote}: {e}")),
    };
    let start_json = match serde_json::to_string(&start) {
        Ok(json) => json,
        Err(e) => return fail(format!("could not encode the start for {remote}: {e}")),
    };
    remote_log(&format!(
        "start on {remote}: cwd {} model {:?} mode {} resume {:?}",
        start.cwd, start.model, start.permission_mode, start.resume
    ));
    let machine = client.name.clone();
    let backend = RemoteAgentBackend::new(client);
    let persistent = match backend.handshake().await {
        Ok(handshake) => handshake
            .capabilities
            .iter()
            .any(|c| c == crate::agentd::server::CLAUDE_CHAT_PERSISTENT_CAPABILITY),
        Err(e) => return fail(format!("could not reach {remote}: {e}")),
    };
    if persistent {
        RemoteChatLink {
            backend,
            machine,
            remote,
            start_json,
            commands,
            events,
            chat_id,
            attached_once: false,
            last_seq: 0,
            replay_only: false,
            open_requests: std::collections::HashSet::new(),
            queued: VecDeque::new(),
            handle_dropped: false,
            shutdown_requested: false,
            disconnected: false,
            backoff: RECONNECT_FIRST_DELAY,
        }
        .run()
        .await;
    } else if let Some(chat_id) = chat_id {
        fail(format!(
            "cannot attach to chat {chat_id}: {remote} predates claude_chat_persistent"
        ));
    } else {
        remote_log(&format!(
            "{remote} predates claude_chat_persistent; the chat lives as long as this connection"
        ));
        run_legacy_remote_session(backend, remote, start_json, commands, events).await;
    }
}

/// How one attached stream of a persistent remote chat ended.
enum StreamEnd {
    /// The chat's process exited; its exit was forwarded.
    Exited,
    /// The handle was dropped and the agent closed the stream: detached.
    Detached,
    /// The tab stopped listening.
    UiGone,
    /// The connection failed; reconnect.
    Lost(String),
}

/// A remote chat on an agent that owns its chats: attach, relay, and on a
/// lost connection reattach after the last seq seen, for as long as the
/// handle lives.
struct RemoteChatLink {
    backend: RemoteAgentBackend,
    /// The machine's display name, for the reconnect notice.
    machine: String,
    /// Name and endpoint, for logs.
    remote: String,
    start_json: String,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    /// The chat: given when attaching to a listed one, otherwise known
    /// once the agent has answered the first start.
    chat_id: Option<String>,
    /// The agent has answered an attach on this link; later opens resume
    /// after `last_seq`.
    attached_once: bool,
    /// The last sequenced event forwarded to the tab.
    last_seq: u64,
    /// The current stream resumed without a gap: the tab already has every
    /// redelivered event except, possibly, a runtime request.
    replay_only: bool,
    /// Runtime requests forwarded and not yet resolved, so a redelivered
    /// one the tab is already showing is not shown twice.
    open_requests: std::collections::HashSet<String>,
    /// Commands not yet handed to a stream (sent while connecting).
    queued: VecDeque<HarnessCommand>,
    handle_dropped: bool,
    shutdown_requested: bool,
    /// A `RemoteDisconnected` notice is showing.
    disconnected: bool,
    backoff: Duration,
}

impl RemoteChatLink {
    async fn run(mut self) {
        let mut shutdown_deadline: Option<tokio::time::Instant> = None;
        loop {
            if self.handle_dropped {
                if !self.shutdown_requested {
                    remote_log(&format!("{}: handle dropped; detached", self.remote));
                    return;
                }
                // The connection may have taken the Shutdown with it;
                // sending it again is harmless.
                if !self.queued.contains(&HarnessCommand::Shutdown) {
                    self.queued.push_back(HarnessCommand::Shutdown);
                }
                let deadline = *shutdown_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + SHUTDOWN_DELIVERY_WINDOW);
                if tokio::time::Instant::now() >= deadline {
                    remote_log(&format!(
                        "{}: could not deliver Shutdown within {}s; the chat keeps running there until its idle stop",
                        self.remote,
                        SHUTDOWN_DELIVERY_WINDOW.as_secs()
                    ));
                    return;
                }
            }
            let open = match &self.chat_id {
                None => ClaudeChatOpen::Start {
                    start_json: self.start_json.clone(),
                },
                Some(chat_id) => ClaudeChatOpen::Chat {
                    chat_id: chat_id.clone(),
                    after_seq: self.attached_once.then_some(self.last_seq),
                },
            };
            let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
            let error = match self.backend.claude_chat_attach(open, out_rx).await {
                Ok(stream) => match self.relay(stream, out_tx).await {
                    StreamEnd::Exited | StreamEnd::Detached | StreamEnd::UiGone => return,
                    StreamEnd::Lost(error) => error,
                },
                Err(e) if e.is_not_found() => {
                    if self.handle_dropped {
                        remote_log(&format!("{}: the chat has already stopped", self.remote));
                        return;
                    }
                    let msg = format!(
                        "The chat on {} is no longer running (stopped there, or its agent restarted): {e}",
                        self.machine
                    );
                    remote_log(&msg);
                    let _ = self.events.send(HarnessEvent::Error(msg));
                    let _ = self.events.send(HarnessEvent::ProcessExited { code: None });
                    return;
                }
                Err(e) => {
                    if !self.attached_once {
                        let msg = format!("could not start Claude on {}: {e}", self.remote);
                        remote_log(&msg);
                        let _ = self.events.send(HarnessEvent::Error(msg));
                        let _ = self.events.send(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                    e.to_string()
                }
            };
            if self.handle_dropped && !self.shutdown_requested {
                remote_log(&format!(
                    "{}: connection lost ({error}) after the handle was dropped; detached",
                    self.remote
                ));
                return;
            }
            remote_log(&format!(
                "{}: connection lost ({error}); retrying in {}s",
                self.remote,
                self.backoff.as_secs()
            ));
            if !self.disconnected && !self.handle_dropped {
                self.disconnected = true;
                if self
                    .events
                    .send(HarnessEvent::RemoteDisconnected {
                        machine: self.machine.clone(),
                        error,
                    })
                    .is_err()
                {
                    return;
                }
            }
            if !self.wait_to_retry().await {
                return;
            }
            self.backoff = (self.backoff * 2).min(RECONNECT_MAX_DELAY);
        }
    }

    /// Sleep out the backoff, queueing commands meanwhile. `false` when
    /// the handle was dropped with nothing left to deliver.
    async fn wait_to_retry(&mut self) -> bool {
        let wake = tokio::time::Instant::now() + self.backoff;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(wake) => return true,
                cmd = self.commands.recv(), if !self.handle_dropped => match cmd {
                    Some(cmd) => {
                        self.shutdown_requested |= cmd == HarnessCommand::Shutdown;
                        self.queued.push_back(cmd);
                    }
                    None => {
                        self.handle_dropped = true;
                        if !self.shutdown_requested {
                            remote_log(&format!(
                                "{}: handle dropped while reconnecting; detached",
                                self.remote
                            ));
                            return false;
                        }
                    }
                },
            }
        }
    }

    /// Hand one command to the stream. `false` when the stream is closed.
    fn send_command(
        &mut self,
        out_tx: &mpsc::UnboundedSender<String>,
        cmd: HarnessCommand,
    ) -> bool {
        self.shutdown_requested |= cmd == HarnessCommand::Shutdown;
        match serde_json::to_string(&cmd) {
            Ok(json) => {
                if out_tx.send(json).is_err() {
                    // Kept for the next stream.
                    self.queued.push_front(cmd);
                    return false;
                }
                true
            }
            Err(e) => {
                let msg = format!("could not encode {cmd:?} for {}: {e}", self.remote);
                remote_log(&msg);
                let _ = self.events.send(HarnessEvent::Error(msg));
                true
            }
        }
    }

    /// Relay one attached stream until it ends.
    async fn relay(
        &mut self,
        mut stream: crate::agentd::client::ClaudeChatFrameStream,
        out_tx: mpsc::UnboundedSender<String>,
    ) -> StreamEnd {
        use tokio_stream::StreamExt as _;

        let mut out_tx = Some(out_tx);
        let mut opened = false;
        let mut exited = false;
        loop {
            tokio::select! {
                cmd = self.commands.recv(), if !self.handle_dropped => match cmd {
                    Some(cmd) => match out_tx.as_ref().filter(|_| opened) {
                        Some(tx) => {
                            let tx = tx.clone();
                            self.send_command(&tx, cmd);
                        }
                        None => {
                            self.shutdown_requested |= cmd == HarnessCommand::Shutdown;
                            self.queued.push_back(cmd);
                        }
                    },
                    None => {
                        // Handle dropped: closing our half detaches (after
                        // anything queued, such as a Shutdown, goes out).
                        self.handle_dropped = true;
                        if opened {
                            out_tx = None;
                        }
                    }
                },
                item = stream.next() => match item {
                    Some(Ok(ClaudeChatFrame::Opened(info))) => {
                        if opened {
                            remote_log(&format!("{}: a second opened message was ignored", self.remote));
                            continue;
                        }
                        opened = true;
                        if !self.on_opened(info) {
                            return StreamEnd::UiGone;
                        }
                        if let Some(tx) = out_tx.clone() {
                            while let Some(cmd) = self.queued.pop_front() {
                                if !self.send_command(&tx, cmd) {
                                    break;
                                }
                            }
                        }
                        if self.handle_dropped {
                            out_tx = None;
                        }
                    }
                    Some(Ok(ClaudeChatFrame::Event { json, seq, redelivered })) => {
                        match self.on_event(&json, seq, redelivered) {
                            Ok(true) => exited = true,
                            Ok(false) => {}
                            Err(()) => return StreamEnd::UiGone,
                        }
                    }
                    Some(Err(e)) => {
                        if exited {
                            return StreamEnd::Exited;
                        }
                        return StreamEnd::Lost(e.to_string());
                    }
                    None => {
                        if exited {
                            return StreamEnd::Exited;
                        }
                        if self.handle_dropped && opened {
                            remote_log(&format!("{}: detached", self.remote));
                            return StreamEnd::Detached;
                        }
                        return StreamEnd::Lost(format!("{} ended the chat stream", self.remote));
                    }
                }
            }
        }
    }

    /// Take in the agent's answer to an attach. `false` when the tab
    /// stopped listening.
    fn on_opened(&mut self, info: ClaudeChatOpened) -> bool {
        let first = !self.attached_once;
        self.attached_once = true;
        remote_log(&format!(
            "{}: {} chat {} (session {:?}, head seq {}, gap {})",
            self.remote,
            match (first, info.attached) {
                (true, true) => "attached to running",
                (true, false) => "started",
                (false, _) => "reattached to",
            },
            info.chat_id,
            info.claude_session_id,
            info.head_seq,
            info.gap
        ));
        self.chat_id = Some(info.chat_id);
        self.backoff = RECONNECT_FIRST_DELAY;
        // From the head on, unless the agent is replaying what we missed.
        self.replay_only = !first && !info.gap;
        if !self.replay_only {
            self.last_seq = info.head_seq;
            self.open_requests.clear();
        }
        if first {
            return true;
        }
        self.disconnected = false;
        self.events
            .send(HarnessEvent::RemoteReconnected {
                machine: self.machine.clone(),
                history_gap: info.gap,
            })
            .is_ok()
    }

    /// Forward one event to the tab unless it already has it. `Ok(true)`
    /// for the chat's exit; `Err` when the tab stopped listening.
    fn on_event(&mut self, json: &str, seq: u64, redelivered: bool) -> Result<bool, ()> {
        let ev = match serde_json::from_str::<HarnessEvent>(json) {
            Ok(ev) => ev,
            Err(e) => {
                let msg = format!(
                    "undecodable event from {}: {e}: {}",
                    self.remote,
                    trim_for_log(json, 200)
                );
                remote_log(&msg);
                return self
                    .events
                    .send(HarnessEvent::Error(msg))
                    .map(|_| false)
                    .map_err(|_| ());
            }
        };
        if redelivered {
            let showing = match &ev {
                HarnessEvent::RuntimeRequest { request_id, .. } => {
                    self.open_requests.contains(request_id)
                }
                // Ready, mode, model, effort and the turn start are
                // already on a tab that missed nothing.
                _ => self.replay_only,
            };
            if showing {
                return Ok(false);
            }
        } else if seq != 0 {
            if seq <= self.last_seq {
                return Ok(false);
            }
            if seq != self.last_seq + 1 {
                remote_log(&format!(
                    "{}: event seq {seq} after {}; events in between were not received",
                    self.remote, self.last_seq
                ));
            }
            self.last_seq = seq;
        }
        let exited = matches!(ev, HarnessEvent::ProcessExited { .. });
        match &ev {
            HarnessEvent::RuntimeRequest { request_id, .. } => {
                self.open_requests.insert(request_id.clone());
            }
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                self.open_requests.remove(request_id);
            }
            HarnessEvent::TurnCompleted { .. } | HarnessEvent::ProcessExited { .. } => {
                self.open_requests.clear();
            }
            _ => {}
        }
        self.events.send(ev).map(|_| exited).map_err(|_| ())
    }
}

/// Relay a remote chat on an agent from before "claude_chat_persistent":
/// the process lives as long as this stream.
async fn run_legacy_remote_session(
    backend: RemoteAgentBackend,
    remote: String,
    start_json: String,
    mut commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) {
    use tokio_stream::StreamExt as _;

    let fail = |msg: String| {
        remote_log(&msg);
        let _ = events.send(HarnessEvent::Error(msg));
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
    };
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let mut stream = match backend.claude_chat(start_json, out_rx).await {
        Ok(stream) => stream,
        Err(e) => return fail(format!("could not start Claude on {remote}: {e}")),
    };

    let mut out_tx = Some(out_tx);
    let mut exited = false;
    loop {
        tokio::select! {
            cmd = commands.recv(), if out_tx.is_some() => {
                let Some(cmd) = cmd else {
                    // Handle dropped: closing our half ends the stream, and
                    // the agent shuts its process down.
                    out_tx = None;
                    continue;
                };
                let sent = match serde_json::to_string(&cmd) {
                    Ok(json) => out_tx.as_ref().is_some_and(|tx| tx.send(json).is_ok()),
                    Err(e) => {
                        let msg = format!("could not encode {cmd:?} for {remote}: {e}");
                        remote_log(&msg);
                        let _ = events.send(HarnessEvent::Error(msg));
                        continue;
                    }
                };
                if !sent {
                    let msg = format!("the chat stream to {remote} is closed; {cmd:?} was not sent");
                    remote_log(&msg);
                    let _ = events.send(HarnessEvent::Error(msg));
                }
            }
            item = stream.next() => match item {
                Some(Ok(json)) => match serde_json::from_str::<HarnessEvent>(&json) {
                    Ok(ev) => {
                        exited |= matches!(ev, HarnessEvent::ProcessExited { .. });
                        if events.send(ev).is_err() {
                            // The tab is gone; dropping the stream cancels
                            // the call and the agent stops its process.
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = format!(
                            "undecodable event from {remote}: {e}: {}",
                            trim_for_log(&json, 200)
                        );
                        remote_log(&msg);
                        let _ = events.send(HarnessEvent::Error(msg));
                    }
                },
                Some(Err(e)) => {
                    let msg = format!("lost the connection to {remote}: {e}");
                    remote_log(&msg);
                    let _ = events.send(HarnessEvent::Error(msg));
                    break;
                }
                None => {
                    if !exited {
                        let msg = format!("{remote} ended the chat stream before Claude exited");
                        remote_log(&msg);
                        let _ = events.send(HarnessEvent::Error(msg));
                    }
                    break;
                }
            }
        }
    }
    if !exited {
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
    }
}

fn harness_log(msg: &str) {
    eprintln!("[claude-harness] {msg}");
}

fn trim_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

struct WireLog {
    stdin: std::fs::File,
    stdout: std::fs::File,
}

impl WireLog {
    fn open(dir: &Path, pid: u32) -> Result<Self, String> {
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
        Ok(Self {
            stdin: open(format!("claude-{pid}-stdin.jsonl"))?,
            stdout: open(format!("claude-{pid}-stdout.jsonl"))?,
        })
    }
}

/// A `can_use_tool` request waiting on the human.
struct PendingRequest {
    tool_use_id: String,
    input: Value,
}

struct SessionIo {
    stdin: Option<ChildStdin>,
    wire: Option<WireLog>,
    events: mpsc::UnboundedSender<HarnessEvent>,
}

impl SessionIo {
    async fn write(&mut self, line: String) -> Result<(), String> {
        if let Some(wire) = self.wire.as_mut() {
            if let Err(e) = writeln!(wire.stdin, "{line}") {
                harness_log(&format!("wire log write failed: {e}"));
            }
        }
        let stdin = self.stdin.as_mut().ok_or("claude stdin is closed")?;
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| format!("write to claude stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush claude stdin: {e}"))
    }

    fn emit(&self, ev: HarnessEvent) {
        // The receiver is gone only when the tab is; nothing to report to.
        let _ = self.events.send(ev);
    }
}

async fn run_session(
    config: ClaudeSessionConfig,
    mut commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) {
    let fail = |msg: String| {
        harness_log(&msg);
        let _ = events.send(HarnessEvent::Error(msg));
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
    };
    let program = match resolve_claude_program() {
        Ok(p) => p,
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
        .env("PATH", child_path());
    // Do not leak a parent Claude Code session's identity into the child
    // (GitTerm is often started from inside one), then set what the Agent
    // SDK sets on every spawn.
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key == "CLAUDECODE" || key.starts_with("CLAUDE_CODE_") || key == "CLAUDE_PID" {
            cmd.env_remove(key.as_ref());
        }
    }
    cmd.env_remove("NODE_OPTIONS")
        .env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")
        .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1");
    // MCP bearer tokens travel only through the environment, exactly as
    // terminal launches carry them.
    for server in &config.mcp_servers {
        cmd.envs(server.env.iter().map(|(key, value)| (key, value)));
    }

    harness_log(&format!(
        "spawn: {} {} (cwd {})",
        program.display(),
        args.join(" "),
        config.cwd.display()
    ));
    let mut child: Child = match cmd.spawn() {
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
        return fail("claude child is missing a stdio pipe".to_string());
    };
    let wire = config
        .wire_log_dir
        .as_deref()
        .and_then(|dir| match WireLog::open(dir, pid) {
            Ok(w) => Some(w),
            Err(e) => {
                harness_log(&e);
                None
            }
        });

    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
    {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        harness_log(&format!("stderr[{pid}]: {line}"));
                        if let Ok(mut t) = tail.lock() {
                            if t.len() == STDERR_TAIL_LINES {
                                t.pop_front();
                            }
                            t.push_back(line);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        harness_log(&format!("stderr[{pid}] read failed: {e}"));
                        break;
                    }
                }
            }
        });
    }

    let mut io = SessionIo {
        stdin: Some(stdin),
        wire,
        events,
    };
    let mut parser = ClaudeFrameParser::new();
    let mut pending: HashMap<String, PendingRequest> = HashMap::new();
    let mut next_id: u64 = 0;
    let mut new_request_id = || {
        next_id += 1;
        format!("gitterm_{pid}_{next_id}")
    };
    let mut interrupt_requested = false;
    // Between `system/init` and `result`. A message sent mid-turn must not
    // clear an interrupt that turn is still answering.
    let mut in_turn = false;

    // Handshake. Its reply becomes `Ready`.
    let init_id = new_request_id();
    parser.register_host_request(
        init_id.clone(),
        HostRequest::Initialize {
            resume: config.resume.clone(),
        },
    );
    if let Err(e) = io
        .write(control_request_frame(
            &init_id,
            json!({"subtype": "initialize", "hooks": {}}),
        ))
        .await
    {
        io.emit(HarnessEvent::Error(e));
    }

    let mut lines = BufReader::new(stdout).lines();
    let mut shutting_down = false;
    let mut shutdown_deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        io.emit(HarnessEvent::Error(format!("read claude stdout: {e}")));
                        break;
                    }
                };
                if let Some(wire) = io.wire.as_mut() {
                    if let Err(e) = writeln!(wire.stdout, "{line}") {
                        harness_log(&format!("wire log write failed: {e}"));
                    }
                }
                for frame in parser.parse_line(&line) {
                    match frame {
                        ParsedFrame::Event(ev) => {
                            let ev = match ev {
                                HarnessEvent::TurnStarted { session_id, model } => {
                                    in_turn = true;
                                    HarnessEvent::TurnStarted { session_id, model }
                                }
                                HarnessEvent::TurnCompleted { status, usage, cost_usd } => {
                                    in_turn = false;
                                    let status = match status {
                                        TurnStatus::Failed(_) if interrupt_requested => {
                                            TurnStatus::Interrupted
                                        }
                                        other => other,
                                    };
                                    interrupt_requested = false;
                                    harness_log(&format!("turn completed: {status:?} cost={cost_usd:?}"));
                                    // A request still open when its turn ends is moot.
                                    for (request_id, _) in pending.drain() {
                                        io.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                                    }
                                    HarnessEvent::TurnCompleted { status, usage, cost_usd }
                                }
                                HarnessEvent::Error(msg) => {
                                    harness_log(&format!("error: {msg}"));
                                    HarnessEvent::Error(msg)
                                }
                                other => other,
                            };
                            io.emit(ev);
                        }
                        ParsedFrame::PermissionRequest { request_id, tool_use_id, input, event } => {
                            harness_log(&format!(
                                "runtime request {request_id}: {}",
                                trim_for_log(&serde_json::to_string(&event).unwrap_or_default(), 300)
                            ));
                            pending.insert(request_id, PendingRequest { tool_use_id, input });
                            io.emit(event);
                        }
                        ParsedFrame::ControlRequest { request_id, subtype } => {
                            if let Err(e) = answer_unhandled_control(&mut io, &request_id, &subtype).await {
                                io.emit(HarnessEvent::Error(e));
                            }
                        }
                        ParsedFrame::ControlCancel { request_id } => {
                            if pending.remove(&request_id).is_some() {
                                harness_log(&format!("CLI withdrew request {request_id}"));
                                io.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                            }
                        }
                        ParsedFrame::HostRequestDone { request_id, request, result } => {
                            harness_log(&format!(
                                "{request:?} ({request_id}) -> {}",
                                match &result {
                                    // get_settings carries the user's whole
                                    // settings (env, tokens); never log it.
                                    Ok(_) if matches!(request, HostRequest::ConfirmEffort(_)) => {
                                        "ok (settings not logged)".to_string()
                                    }
                                    Ok(v) => format!("ok {}", trim_for_log(&v.to_string(), 160)),
                                    Err(e) => format!("error {e}"),
                                }
                            ));
                            // An accepted effort change is confirmed by
                            // reading the session's settings back.
                            if let (HostRequest::SetEffort(effort), Ok(_)) = (&request, &result) {
                                let id = new_request_id();
                                parser.register_host_request(id.clone(), HostRequest::ConfirmEffort(effort.clone()));
                                if let Err(e) = io
                                    .write(control_request_frame(&id, json!({"subtype": "get_settings"})))
                                    .await
                                {
                                    harness_log(&e);
                                    io.emit(HarnessEvent::Error(e));
                                }
                            }
                        }
                        ParsedFrame::Unparsed(line) => {
                            harness_log(&format!("non-JSON stdout: {}", trim_for_log(&line, 200)));
                        }
                    }
                }
            }
            cmd = commands.recv(), if !shutting_down => {
                let Some(cmd) = cmd else {
                    // Handle dropped without an explicit Shutdown.
                    shutting_down = true;
                    shutdown_deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                    io.stdin = None;
                    continue;
                };
                let result = match cmd {
                    HarnessCommand::SendUserMessage(prompt) => {
                        harness_log(&format!(
                            "user message ({} chars, {} images, id {:?})",
                            prompt.text.len(),
                            prompt.images.len(),
                            prompt.id
                        ));
                        if !in_turn {
                            interrupt_requested = false;
                        }
                        io.write(user_message_frame(&prompt)).await
                    }
                    HarnessCommand::WithdrawQueuedMessage(message_id) => {
                        let id = new_request_id();
                        harness_log(&format!("cancel_async_message {message_id} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::WithdrawMessage(message_id.clone()));
                        io.write(control_request_frame(
                            &id,
                            json!({"subtype": "cancel_async_message", "message_uuid": message_id}),
                        ))
                        .await
                    }
                    HarnessCommand::Answer { request_id, decision } => {
                        match pending.remove(&request_id) {
                            None => Err(format!(
                                "no pending Claude request {request_id} (already answered or withdrawn)"
                            )),
                            Some(req) => {
                                match permission_result(&decision, &req.tool_use_id, &req.input) {
                                    Ok(body) => {
                                        harness_log(&format!(
                                            "answer {request_id}: {}",
                                            trim_for_log(&body.to_string(), 300)
                                        ));
                                        let frame = control_response_frame(ControlResponseBody::Success {
                                            request_id: request_id.clone(),
                                            response: body,
                                        });
                                        let written = io.write(frame).await;
                                        io.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                                        written
                                    }
                                    Err(e) => {
                                        // Keep it answerable; the UI can retry.
                                        pending.insert(request_id, req);
                                        Err(e)
                                    }
                                }
                            }
                        }
                    }
                    HarnessCommand::Interrupt => {
                        let id = new_request_id();
                        harness_log(&format!("interrupt ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::Interrupt);
                        interrupt_requested = true;
                        io.write(control_request_frame(&id, json!({"subtype": "interrupt"}))).await
                    }
                    HarnessCommand::SetPermissionMode(mode) => {
                        let id = new_request_id();
                        harness_log(&format!("set_permission_mode {mode:?} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::SetPermissionMode(mode.clone()));
                        io.write(control_request_frame(
                            &id,
                            json!({"subtype": "set_permission_mode", "mode": mode}),
                        ))
                        .await
                    }
                    HarnessCommand::SetModel(model) => {
                        let id = new_request_id();
                        harness_log(&format!("set_model {model:?} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::SetModel(model.clone()));
                        io.write(control_request_frame(
                            &id,
                            json!({"subtype": "set_model", "model": model}),
                        ))
                        .await
                    }
                    HarnessCommand::SetEffort(effort) => {
                        let id = new_request_id();
                        harness_log(&format!("apply_flag_settings effortLevel={effort:?} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::SetEffort(effort.clone()));
                        io.write(control_request_frame(
                            &id,
                            json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": effort}}),
                        ))
                        .await
                    }
                    HarnessCommand::Shutdown => {
                        harness_log(&format!("shutdown requested (pid {pid})"));
                        shutting_down = true;
                        shutdown_deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                        // Closing stdin tells the CLI to finish and exit.
                        io.stdin = None;
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    harness_log(&e);
                    io.emit(HarnessEvent::Error(e));
                }
            }
            _ = tokio::time::sleep_until(shutdown_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if shutdown_deadline.is_some() => {
                harness_log(&format!("claude {pid} did not exit after stdin closed; killing"));
                if let Err(e) = child.start_kill() {
                    harness_log(&format!("kill claude {pid}: {e}"));
                }
                break;
            }
        }
    }

    let code = match child.wait().await {
        Ok(status) => status.code(),
        Err(e) => {
            harness_log(&format!("wait for claude {pid}: {e}"));
            None
        }
    };
    harness_log(&format!("claude {pid} exited with {code:?}"));
    if !shutting_down && code != Some(0) {
        let tail = stderr_tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        io.emit(HarnessEvent::Error(if tail.is_empty() {
            format!("claude exited unexpectedly (code {code:?})")
        } else {
            format!("claude exited unexpectedly (code {code:?}):\n{tail}")
        }));
    }
    for (request_id, _) in pending.drain() {
        io.emit(HarnessEvent::RuntimeRequestResolved { request_id });
    }
    io.emit(HarnessEvent::ProcessExited { code });
}

/// Answer a CLI control request GitTerm has no handler for, the way the SDK
/// does when the matching callback is not configured.
async fn answer_unhandled_control(
    io: &mut SessionIo,
    request_id: &str,
    subtype: &str,
) -> Result<(), String> {
    if SDK_UNANSWERED_SUBTYPES.contains(&subtype) {
        harness_log(&format!(
            "leaving {subtype} {request_id} unanswered, as the SDK does"
        ));
        return Ok(());
    }
    let body = if subtype == "elicitation" {
        ControlResponseBody::Success {
            request_id: request_id.to_string(),
            response: json!({"action": "decline"}),
        }
    } else {
        harness_log(&format!(
            "unsupported control request {subtype} ({request_id})"
        ));
        ControlResponseBody::Error {
            request_id: request_id.to_string(),
            error: format!("Unsupported control request subtype: {subtype}"),
        }
    };
    io.write(control_response_frame(body)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::ImageAttachment;

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

    const INIT_REPLY: &str = include_str!("../../tests/fixtures/claude/initialize_reply.jsonl");
    const TURN_TEXT: &str = include_str!("../../tests/fixtures/claude/turn_text.jsonl");
    const TURN_SLASH_COMMAND: &str =
        include_str!("../../tests/fixtures/claude/turn_slash_command.jsonl");
    const TURN_BASH_ALLOW: &str = include_str!("../../tests/fixtures/claude/turn_bash_allow.jsonl");
    const TURN_BASH_DENY: &str = include_str!("../../tests/fixtures/claude/turn_bash_deny.jsonl");
    const TURN_QUESTION: &str = include_str!("../../tests/fixtures/claude/turn_question.jsonl");
    const TURN_INTERRUPT: &str = include_str!("../../tests/fixtures/claude/turn_interrupt.jsonl");
    // Captured from `claude_harness_smoke --scenario midturn` (TRU-140): a
    // Bash turn with three messages sent after its first tool call starts
    // (…b, …c, then …d withdrawn at once via request gitterm_5166_2).
    // Trimmed to the lifecycle frames, the cancel reply, tool results and
    // the result.
    const TURN_MIDTURN_MESSAGES: &str =
        include_str!("../../tests/fixtures/claude/turn_midturn_messages.jsonl");
    const SET_PERMISSION_MODE_REPLY: &str =
        include_str!("../../tests/fixtures/claude/set_permission_mode_reply.jsonl");
    // Captured from `claude_harness_smoke --scenario model` (TRU-143):
    // set_model, apply_flag_settings low, get_settings, apply_flag_settings
    // null, get_settings. The get_settings replies keep only effortLevel
    // (the rest is the user's own settings).
    const MODEL_EFFORT_REPLIES: &str =
        include_str!("../../tests/fixtures/claude/model_effort_replies.jsonl");
    // Captured from `claude_harness_smoke --scenario review` (TRU-142):
    // consecutive deltas on one block merged, signatures and paths redacted.
    const TURN_SUBAGENT_REVIEW: &str =
        include_str!("../../tests/fixtures/claude/turn_subagent_review.jsonl");
    const TURN_SUBAGENT_BACKGROUND: &str =
        include_str!("../../tests/fixtures/claude/turn_subagent_background.jsonl");

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
            ..
        }] = evs.as_slice()
        else {
            panic!("expected one Ready, got {evs:?}");
        };
        assert_eq!(session_id.as_deref(), Some("abc"));
        assert_eq!(permission_mode.as_deref(), Some("default"));
        assert!(!models.is_empty());
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
        use crate::harness::MessageState::*;
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

    #[test]
    fn args_follow_the_phase_a_contract() {
        let cfg = ClaudeSessionConfig {
            cwd: PathBuf::from("/tmp"),
            model: Some("haiku".into()),
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: true,
        };
        let args = cfg.args().join(" ");
        assert!(args.contains("--chrome"));
        assert!(!ClaudeSessionConfig {
            chrome: false,
            ..cfg.clone()
        }
        .args()
        .contains(&"--chrome".to_string()));
        assert!(args.contains("--permission-prompt-tool stdio"));
        assert!(args.contains("--input-format stream-json"));
        assert!(args.contains("--include-partial-messages"));
        assert!(args.contains("--model haiku"));
        assert!(args.contains("--permission-mode default"));
        assert!(args.ends_with("--resume=sid"));
        assert!(!args.contains("--print"));
        assert!(!args.contains("--setting-sources"));
    }

    const SECRET: &str = "s3cret-bearer-value";

    fn mcp_server(name: &str, token_env: &str, allowed: &[&str]) -> ClaudeMcpServer {
        ClaudeMcpServer {
            config: json!({
                "mcpServers": {
                    name: {
                        "type": "http",
                        "url": "http://127.0.0.1:1/mcp",
                        "headers": { "Authorization": format!("Bearer ${{{token_env}}}") },
                    },
                },
            }),
            allowed_tools: allowed.iter().map(|tool| tool.to_string()).collect(),
            env: vec![(token_env.to_string(), SECRET.to_string())],
        }
    }

    fn config_with(mcp_servers: Vec<ClaudeMcpServer>) -> ClaudeSessionConfig {
        ClaudeSessionConfig {
            cwd: PathBuf::from("/tmp"),
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers,
            chrome: false,
        }
    }

    #[test]
    fn remote_start_carries_the_config_and_round_trips() {
        let mut config = config_with(Vec::new());
        config.model = Some("haiku".into());
        config.effort = Some("low".into());
        config.chrome = true;
        config.wire_log_dir = Some(PathBuf::from("/tmp/wire"));
        let start = RemoteClaudeChatStart::from_config(&config).unwrap();
        assert_eq!(
            serde_json::to_value(&start).unwrap(),
            json!({"cwd": "/tmp", "model": "haiku", "permission_mode": "default",
                   "effort": "low", "resume": "sid", "chrome": true})
        );
        let back: RemoteClaudeChatStart =
            serde_json::from_str(&serde_json::to_string(&start).unwrap()).unwrap();
        assert_eq!(back, start);
        // The agent side never gets a wire log or MCP servers.
        let agent = back.into_config(PathBuf::from("/private/tmp"));
        assert_eq!(agent.cwd, PathBuf::from("/private/tmp"));
        assert_eq!(agent.wire_log_dir, None);
        assert!(agent.mcp_servers.is_empty());
        assert_eq!(agent.args(), {
            let mut local = config.clone();
            local.wire_log_dir = None;
            local.args()
        });
    }

    #[test]
    fn remote_start_refuses_mcp_servers() {
        let config = config_with(vec![mcp_server("gitterm-tasks", "TOKEN", &[])]);
        let err = RemoteClaudeChatStart::from_config(&config).unwrap_err();
        assert!(err.contains("remote machine"), "{err}");
    }

    fn link() -> (RemoteChatLink, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (_commands_tx, commands) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let link = RemoteChatLink {
            backend: RemoteAgentBackend::new(RemoteAgentClientConfig {
                remote_id: "mini".into(),
                name: "mini".into(),
                endpoint: "http://127.0.0.1:1".into(),
                token_ref: "env:UNUSED".into(),
            }),
            machine: "mini".into(),
            remote: "mini (test)".into(),
            start_json: "{}".into(),
            commands,
            events,
            chat_id: None,
            attached_once: false,
            last_seq: 0,
            replay_only: false,
            open_requests: std::collections::HashSet::new(),
            queued: VecDeque::new(),
            handle_dropped: false,
            shutdown_requested: false,
            disconnected: false,
            backoff: RECONNECT_FIRST_DELAY,
        };
        (link, events_rx)
    }

    fn opened(head_seq: u64, gap: bool) -> ClaudeChatOpened {
        ClaudeChatOpened {
            chat_id: "chat-1".into(),
            attached: true,
            claude_session_id: "s1".into(),
            head_seq,
            gap,
        }
    }

    fn wire(ev: &HarnessEvent) -> String {
        serde_json::to_string(ev).unwrap()
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<HarnessEvent>) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn a_reattached_link_forwards_each_event_once() {
        let (mut link, mut rx) = link();
        let ready = HarnessEvent::Ready {
            session_id: Some("s1".into()),
            permission_mode: None,
            models: Vec::new(),
            modes: Vec::new(),
        };
        let request = HarnessEvent::RuntimeRequest {
            request_id: "r1".into(),
            tool_use_id: "t1".into(),
            kind: RuntimeRequestKind::Question {
                questions: json!([]),
            },
        };
        let delta = |s: &str| HarnessEvent::TextDelta(s.into());

        // First attach to a running chat: its standing state comes through.
        assert!(link.on_opened(opened(5, false)));
        assert_eq!(link.on_event(&wire(&ready), 1, true), Ok(false));
        assert_eq!(link.on_event(&wire(&request), 5, true), Ok(false));
        assert_eq!(link.on_event(&wire(&delta("a")), 6, false), Ok(false));
        assert_eq!(link.on_event(&wire(&delta("a")), 6, false), Ok(false));
        assert_eq!(
            drain(&mut rx),
            vec![ready.clone(), request.clone(), delta("a")]
        );

        // Back without a gap: the tab has all of it; only the replay shows.
        assert!(link.on_opened(opened(7, false)));
        assert_eq!(link.on_event(&wire(&ready), 1, true), Ok(false));
        assert_eq!(link.on_event(&wire(&request), 5, true), Ok(false));
        assert_eq!(link.on_event(&wire(&delta("b")), 7, false), Ok(false));
        assert_eq!(
            drain(&mut rx),
            vec![
                HarnessEvent::RemoteReconnected {
                    machine: "mini".into(),
                    history_gap: false
                },
                delta("b")
            ]
        );

        // Back past the buffer: the tab reloads, so the open prompt is
        // shown again, and only events after the head follow.
        assert!(link.on_opened(opened(20, true)));
        assert_eq!(link.on_event(&wire(&request), 5, true), Ok(false));
        assert_eq!(link.on_event(&wire(&delta("old")), 15, false), Ok(false));
        assert_eq!(link.on_event(&wire(&delta("c")), 21, false), Ok(false));
        assert_eq!(
            link.on_event(
                &wire(&HarnessEvent::ProcessExited { code: Some(0) }),
                22,
                false
            ),
            Ok(true)
        );
        assert_eq!(
            drain(&mut rx),
            vec![
                HarnessEvent::RemoteReconnected {
                    machine: "mini".into(),
                    history_gap: true
                },
                request,
                delta("c"),
                HarnessEvent::ProcessExited { code: Some(0) },
            ]
        );
        // A notice for this stream only (seq 0) always reaches the tab.
        let notice = HarnessEvent::Error("refused".into());
        assert_eq!(link.on_event(&wire(&notice), 0, false), Ok(false));
        assert_eq!(drain(&mut rx), vec![notice]);
    }

    fn flag_values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        let prefix = format!("{flag}=");
        args.iter()
            .filter_map(|arg| arg.strip_prefix(prefix.as_str()))
            .collect()
    }

    #[test]
    fn args_without_mcp_servers_have_no_mcp_flags() {
        let args = config_with(Vec::new()).args();
        assert!(args
            .iter()
            .all(|arg| !arg.starts_with("--mcp-config") && !arg.starts_with("--allowedTools")));
    }

    #[test]
    fn args_attach_one_mcp_server_with_the_token_left_in_the_environment() {
        let args = config_with(vec![mcp_server(
            "gitterm_tasks",
            "TASK_TOKEN",
            &["mcp__gitterm_tasks"],
        )])
        .args();
        let configs = flag_values(&args, "--mcp-config");
        assert_eq!(configs.len(), 1);
        let config: Value = serde_json::from_str(configs[0]).expect("config is one JSON value");
        assert_eq!(
            config["mcpServers"]["gitterm_tasks"]["headers"]["Authorization"],
            "Bearer ${TASK_TOKEN}"
        );
        assert_eq!(flag_values(&args, "--allowedTools"), ["mcp__gitterm_tasks"]);
        assert_eq!(args.last().map(String::as_str), Some("--resume=sid"));
        assert!(args.iter().all(|arg| !arg.contains(SECRET)));
    }

    #[test]
    fn args_attach_two_mcp_servers_as_repeated_flags() {
        let servers = vec![
            mcp_server("gitterm_tasks", "TASK_TOKEN", &["mcp__gitterm_tasks"]),
            mcp_server(
                "gitterm_browser",
                "BROWSER_TOKEN",
                &[
                    "mcp__gitterm_browser__browser_status",
                    "mcp__gitterm_browser__browser_snapshot",
                ],
            ),
        ];
        let cfg = config_with(servers);
        let args = cfg.args();
        let names: Vec<String> = flag_values(&args, "--mcp-config")
            .into_iter()
            .map(|raw| {
                let config: Value = serde_json::from_str(raw).expect("config is one JSON value");
                let servers = config["mcpServers"].as_object().expect("mcpServers object");
                assert_eq!(servers.len(), 1);
                servers.keys().next().cloned().unwrap_or_default()
            })
            .collect();
        assert_eq!(names, ["gitterm_tasks", "gitterm_browser"]);
        assert_eq!(
            flag_values(&args, "--allowedTools"),
            [
                "mcp__gitterm_tasks",
                "mcp__gitterm_browser__browser_status,mcp__gitterm_browser__browser_snapshot",
            ]
        );
        assert!(args.iter().all(|arg| !arg.contains(SECRET)));
        assert!(
            !format!("{cfg:?}").contains(SECRET),
            "Debug must redact env values"
        );
    }
}
