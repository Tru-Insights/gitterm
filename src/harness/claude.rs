//! Native Claude Code session: one long-lived `claude` process driven over
//! the stream-json control protocol in both directions.
//!
//! Wire contract: `.plans/tru-140-claude-control-protocol.md` (Phase A).
//! `ClaudeFrameParser` turns CLI stdout lines into `HarnessEvent`s and is
//! pure (no IO), so it is unit-tested against recorded frames in
//! `tests/fixtures/claude/`. `ClaudeSession` owns the process on a
//! dedicated thread and maps `HarnessCommand`s onto stdin frames.

use std::collections::{HashMap, VecDeque};
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
    HarnessCommand, HarnessEvent, ItemKind, RuntimeDecision, RuntimeRequestKind, TurnStatus,
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
        // Subagent traffic (Task tool) carries a parent tool_use id. The
        // chat shows the parent tool call only; its internals are not items
        // of this turn and their block indexes would collide with ours.
        let from_subagent = v.get("parent_tool_use_id").is_some_and(|p| !p.is_null());
        match str_at("/type").unwrap_or("") {
            "system" if str_at("/subtype") == Some("init") => {
                vec![ParsedFrame::Event(HarnessEvent::TurnStarted {
                    session_id: str_at("/session_id").map(str::to_string),
                    model: str_at("/model").map(str::to_string),
                })]
            }
            "stream_event" if !from_subagent => self.parse_stream_event(&v["event"]),
            // Slash-command replies (`/model sonnet`, `/effort high`, ...) are
            // synthetic assistant messages with no stream deltas; surface
            // their text or the turn looks like it produced nothing.
            "assistant" if !from_subagent && str_at("/message/model") == Some("<synthetic>") => {
                parse_synthetic_assistant(&v)
            }
            "user" if !from_subagent => parse_tool_results(&v),
            "result" => vec![ParsedFrame::Event(parse_result(&v))],
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
            (HostRequest::Interrupt | HostRequest::SetPermissionMode(_), Ok(_)) => {}
        }
        out.push(ParsedFrame::HostRequestDone {
            request_id: request_id.to_string(),
            request,
            result,
        });
        out
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

fn user_message_frame(text: &str) -> String {
    json!({
        "type": "user",
        "session_id": "",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "parent_tool_use_id": null
    })
    .to_string()
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
fn child_path() -> OsString {
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

/// Handle to a running Claude session. Dropping it shuts the process down.
pub struct ClaudeSession {
    commands: mpsc::UnboundedSender<HarnessCommand>,
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
        (Self { commands: cmd_tx }, ev_rx)
    }

    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "the Claude session has exited".to_string())
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        let _ = self.commands.send(HarnessCommand::Shutdown);
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
                                HarnessEvent::TurnCompleted { status, usage, cost_usd } => {
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
                                    Ok(v) => format!("ok {}", trim_for_log(&v.to_string(), 160)),
                                    Err(e) => format!("error {e}"),
                                }
                            ));
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
                    HarnessCommand::SendUserMessage(text) => {
                        harness_log(&format!("user message ({} chars)", text.len()));
                        interrupt_requested = false;
                        io.write(user_message_frame(&text)).await
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
                        parser.register_host_request(id.clone(), HostRequest::SetPermissionMode(mode.clone()));
                        io.write(control_request_frame(
                            &id,
                            json!({"subtype": "set_permission_mode", "mode": mode}),
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
        };
        let args = cfg.args().join(" ");
        assert!(args.contains("--permission-prompt-tool stdio"));
        assert!(args.contains("--input-format stream-json"));
        assert!(args.contains("--include-partial-messages"));
        assert!(args.contains("--model haiku"));
        assert!(args.contains("--permission-mode default"));
        assert!(args.ends_with("--resume=sid"));
        assert!(!args.contains("--print"));
        assert!(!args.contains("--setting-sources"));
    }
}
