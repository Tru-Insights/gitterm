// TRU-140 Phase A: drive ONE long-lived `claude` process over stream-json in
// both directions (stdin and stdout) and prove the Agent SDK control protocol
// works natively from Rust, with no Node in the loop.
//
// The wire contract mirrors what `@anthropic-ai/claude-agent-sdk` 0.3.293
// sends (see `.plans/tru-140-claude-control-protocol.md` for the derivation):
//   * CLI args: --output-format stream-json --verbose --input-format stream-json
//     --permission-prompt-tool stdio (what the SDK adds when a canUseTool
//     callback is set), plus --model / --permission-mode / --resume= /
//     --include-partial-messages.
//   * First stdin line: {"request_id", "type":"control_request",
//     "request":{"subtype":"initialize", ...}}.
//   * User turns: {"type":"user","session_id":"","message":{"role":"user",
//     "content":[{"type":"text","text":...}]},"parent_tool_use_id":null}.
//   * can_use_tool requests are answered with a control_response carrying a
//     PermissionResult plus toolUseID (the SDK adds that field).
//
// Usage:
//   cargo run --example claude_control_probe -- \
//       --workdir <empty dir> [--model haiku] [--scenarios 0,1,2,6,3,4,7,5]
//
// The workdir should be a fresh directory outside any repo so project hooks,
// CLAUDE.md and MCP config are not loaded. Logs land in `<workdir>-logs/`:
//   stdout-<n>.jsonl  every line the CLI wrote, verbatim
//   stdin-<n>.jsonl   every line the probe wrote
//   stderr-<n>.log    CLI stderr
// where <n> is the process number (one per spawned `claude`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Overall ceiling for one turn; a turn that exceeds it is a probe failure.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
/// Ceiling for a client-initiated control request round trip.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
/// Control subtypes the SDK deliberately leaves unanswered ("for the machine
/// serving this session's tools"), copied from sdk.mjs `hq`.
const SDK_UNANSWERED_SUBTYPES: [&str; 4] = [
    "remote_tool_call",
    "remote_plumbing_call",
    "remote_tools_probe",
    "remote_tools_reannounce",
];

// ---------------------------------------------------------------------------
// Typed control messages (confirmed against sdk.d.ts / sdk.mjs and the live
// CLI). Everything still exploratory stays serde_json::Value.
// ---------------------------------------------------------------------------

/// Outgoing `control_request` envelope (host -> CLI).
#[derive(Serialize)]
struct ControlRequestOut<'a> {
    request_id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    request: &'a Value,
}

/// Incoming `can_use_tool` request body (CLI -> host). Only the fields the
/// probe reads are typed; the full frame is logged verbatim.
#[derive(Deserialize, Debug, Clone)]
struct CanUseTool {
    tool_name: String,
    input: Value,
    #[serde(default)]
    permission_suggestions: Option<Vec<Value>>,
    tool_use_id: String,
    #[serde(default)]
    blocked_path: Option<String>,
    #[serde(default)]
    decision_reason: Option<String>,
    #[serde(default)]
    decision_reason_type: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// PermissionResult as the SDK writes it back (`{...result, toolUseID}`).
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "behavior", rename_all = "lowercase")]
enum PermissionResult {
    Allow {
        #[serde(rename = "updatedInput")]
        updated_input: Value,
        #[serde(rename = "updatedPermissions", skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Vec<Value>>,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    Deny {
        message: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
}

/// Outgoing `control_response` envelope (host -> CLI).
#[derive(Serialize)]
struct ControlResponseOut<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    response: ControlResponseBody<'a>,
}

#[derive(Serialize)]
#[serde(tag = "subtype", rename_all = "lowercase")]
enum ControlResponseBody<'a> {
    Success {
        request_id: &'a str,
        response: Value,
    },
    Error {
        request_id: &'a str,
        error: String,
    },
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

enum Event {
    Line(Instant, Value),
    Unparsed(String),
    Eof,
}

struct ProbeConfig {
    workdir: PathBuf,
    logdir: PathBuf,
    model: String,
}

struct Session {
    child: Child,
    stdin_tx: Option<mpsc::UnboundedSender<String>>,
    events: mpsc::UnboundedReceiver<Event>,
    spawned_at: Instant,
    next_id: u64,
    proc_no: usize,
    session_id: Option<String>,
    init_at: Option<Instant>,
    /// control_responses that arrived while the probe was waiting on something else.
    stray_responses: HashMap<String, Value>,
}

/// What a turn's permission policy decides for one can_use_tool request.
type Policy<'a> = dyn FnMut(&CanUseTool) -> PermissionResult + 'a;

#[derive(Default)]
struct TurnOutcome {
    result: Option<Value>,
    first_delta_ms: Option<u128>,
    permission_requests: Vec<(CanUseTool, Value)>,
    permission_answers: Vec<PermissionResult>,
    tool_results: Vec<Value>,
    streamed_text: String,
    interrupt_response: Option<Value>,
    interrupt_sent_ms: Option<u128>,
    other_control_requests: Vec<Value>,
    eof: bool,
}

impl TurnOutcome {
    fn result_text(&self) -> String {
        self.result
            .as_ref()
            .and_then(|r| r.get("result"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
    fn result_field(&self, field: &str) -> String {
        self.result
            .as_ref()
            .and_then(|r| r.get(field))
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<none>".into())
    }
}

impl Session {
    async fn spawn(
        cfg: &ProbeConfig,
        proc_no: usize,
        resume: Option<&str>,
    ) -> Result<Self, String> {
        let mut args: Vec<String> = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--model",
            &cfg.model,
            "--permission-prompt-tool",
            "stdio",
            // The user's ~/.claude/settings.json sets defaultMode=auto; a host
            // that wants prompts must pass the mode explicitly.
            "--permission-mode",
            "default",
            "--include-partial-messages",
            // Isolate from user-level settings (allow rules such as
            // Bash(echo:*), Stop/Notification hooks). project+local still load,
            // so a localSettings permission update written by the CLI applies.
            "--setting-sources=project,local",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(id) = resume {
            // The SDK passes resume as a single `--resume=<id>` argument.
            args.push(format!("--resume={id}"));
        }

        let mut cmd = Command::new("claude");
        cmd.args(&args)
            .current_dir(&cfg.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Do not leak the parent Claude Code session's identity into the child
        // (the probe is often run from inside a Claude Code session).
        for (key, _) in std::env::vars() {
            if key == "CLAUDECODE" || key.starts_with("CLAUDE_CODE_") || key == "CLAUDE_PID" {
                cmd.env_remove(&key);
            }
        }
        // What the SDK sets on every spawn.
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")
            .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1");

        log_line(&format!(
            "spawn #{proc_no}: claude {}  (cwd {})",
            args.join(" "),
            cfg.workdir.display()
        ));
        let spawned_at = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn claude with args {args:?}: {e}"))?;

        let stdout = child.stdout.take().ok_or("child stdout missing")?;
        let stderr = child.stderr.take().ok_or("child stderr missing")?;
        let mut stdin = child.stdin.take().ok_or("child stdin missing")?;

        let open = |name: String| -> Result<std::fs::File, String> {
            let path = cfg.logdir.join(&name);
            std::fs::File::create(&path).map_err(|e| format!("create log {}: {e}", path.display()))
        };
        let mut stdout_log = open(format!("stdout-{proc_no}.jsonl"))?;
        let mut stdin_log = open(format!("stdin-{proc_no}.jsonl"))?;
        let mut stderr_log = open(format!("stderr-{proc_no}.log"))?;

        let (ev_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Err(e) = writeln!(stdout_log, "{line}") {
                            eprintln!("[probe] stdout log write failed: {e}");
                        }
                        let ev = match serde_json::from_str::<Value>(&line) {
                            Ok(v) => Event::Line(Instant::now(), v),
                            Err(_) => Event::Unparsed(line),
                        };
                        if ev_tx.send(ev).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = ev_tx.send(Event::Eof);
                        return;
                    }
                    Err(e) => {
                        eprintln!("[probe] stdout read failed: {e}");
                        let _ = ev_tx.send(Event::Eof);
                        return;
                    }
                }
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Err(e) = writeln!(stderr_log, "{line}") {
                    eprintln!("[probe] stderr log write failed: {e}");
                }
            }
        });
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if let Err(e) = writeln!(stdin_log, "{line}") {
                    eprintln!("[probe] stdin log write failed: {e}");
                }
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                if let Err(e) = stdin.write_all(&bytes).await {
                    eprintln!("[probe] stdin write failed (child exited?): {e}");
                    return;
                }
                if let Err(e) = stdin.flush().await {
                    eprintln!("[probe] stdin flush failed: {e}");
                    return;
                }
            }
            // Sender dropped: closing stdin tells the CLI to finish and exit.
        });

        Ok(Self {
            child,
            stdin_tx: Some(stdin_tx),
            events,
            spawned_at,
            next_id: 0,
            proc_no,
            session_id: None,
            init_at: None,
            stray_responses: HashMap::new(),
        })
    }

    fn elapsed(&self, at: Instant) -> String {
        format!("{:>8.3}s", at.duration_since(self.spawned_at).as_secs_f64())
    }

    fn write(&self, line: String) -> Result<(), String> {
        self.stdin_tx
            .as_ref()
            .ok_or("stdin already closed")?
            .send(line)
            .map_err(|_| "stdin writer task has exited".to_string())
    }

    fn send_user(&self, text: &str) -> Result<(), String> {
        let msg = json!({
            "type": "user",
            "session_id": "",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": null
        });
        log_line(&format!("#{} >> user: {text:?}", self.proc_no));
        self.write(msg.to_string())
    }

    /// Writes a host-initiated control_request and returns its request_id.
    fn send_control(&mut self, request: Value) -> Result<String, String> {
        self.next_id += 1;
        let request_id = format!("probe_{}_{}", self.proc_no, self.next_id);
        let frame = ControlRequestOut {
            request_id: &request_id,
            kind: "control_request",
            request: &request,
        };
        let line =
            serde_json::to_string(&frame).map_err(|e| format!("serialize control_request: {e}"))?;
        log_line(&format!(
            "#{} >> control_request {}",
            self.proc_no,
            trim(&line, 300)
        ));
        self.write(line)?;
        Ok(request_id)
    }

    fn send_control_response(&self, body: ControlResponseBody<'_>) -> Result<(), String> {
        let frame = ControlResponseOut {
            kind: "control_response",
            response: body,
        };
        let line = serde_json::to_string(&frame)
            .map_err(|e| format!("serialize control_response: {e}"))?;
        log_line(&format!(
            "#{} >> control_response {}",
            self.proc_no,
            trim(&line, 400)
        ));
        self.write(line)
    }

    async fn next_event(&mut self, deadline: Instant) -> Result<Event, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, self.events.recv()).await {
            Ok(Some(ev)) => Ok(ev),
            Ok(None) => Ok(Event::Eof),
            Err(_) => Err("timed out waiting for CLI output".into()),
        }
    }

    /// Prints a one-line summary and updates session bookkeeping shared by
    /// every loop (session id, init time).
    fn observe(&mut self, at: Instant, v: &Value) {
        if v["type"] == "system" && v["subtype"] == "init" {
            self.session_id = v["session_id"].as_str().map(str::to_string);
            self.init_at.get_or_insert(at);
        }
        let summary = summarize(v);
        if !summary.is_empty() {
            println!("[#{} {}] {}", self.proc_no, self.elapsed(at), summary);
        }
    }

    /// Answers control requests other than can_use_tool the way the SDK does
    /// when the corresponding callback is not configured.
    fn answer_other_control(&self, v: &Value) -> Result<(), String> {
        let request_id = v["request_id"]
            .as_str()
            .ok_or("control_request without request_id")?;
        let subtype = v["request"]["subtype"].as_str().unwrap_or("");
        if SDK_UNANSWERED_SUBTYPES.contains(&subtype) || subtype == "request_user_dialog" {
            log_line(&format!(
                "#{} (leaving {subtype} {request_id} unanswered, as the SDK does)",
                self.proc_no
            ));
            return Ok(());
        }
        if subtype == "elicitation" {
            return self.send_control_response(ControlResponseBody::Success {
                request_id,
                response: json!({"action": "decline"}),
            });
        }
        self.send_control_response(ControlResponseBody::Error {
            request_id,
            error: format!("Unsupported control request subtype: {subtype}"),
        })
    }

    /// Waits for the control_response to `request_id`, logging anything else.
    async fn await_control_response(&mut self, request_id: &str) -> Result<Value, String> {
        if let Some(v) = self.stray_responses.remove(request_id) {
            return Ok(v);
        }
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        loop {
            match self.next_event(deadline).await? {
                Event::Line(at, v) => {
                    self.observe(at, &v);
                    if v["type"] == "control_response" {
                        let id = v["response"]["request_id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        if id == request_id {
                            return Ok(v);
                        }
                        self.stray_responses.insert(id, v);
                    } else if v["type"] == "control_request" {
                        if v["request"]["subtype"] == "can_use_tool" {
                            let req: CanUseTool = serde_json::from_value(v["request"].clone())
                                .map_err(|e| format!("bad can_use_tool frame: {e}"))?;
                            let id = v["request_id"]
                                .as_str()
                                .ok_or("can_use_tool without request_id")?;
                            let answer = PermissionResult::Deny {
                                message: "probe was not expecting a permission prompt here".into(),
                                tool_use_id: req.tool_use_id.clone(),
                            };
                            self.send_control_response(ControlResponseBody::Success {
                                request_id: id,
                                response: to_value(&answer)?,
                            })?;
                        } else {
                            self.answer_other_control(&v)?;
                        }
                    }
                }
                Event::Unparsed(line) => {
                    println!("[#{}] (non-JSON) {}", self.proc_no, trim(&line, 200))
                }
                Event::Eof => {
                    return Err(format!(
                        "CLI exited while waiting for control_response {request_id}"
                    ))
                }
            }
        }
    }

    async fn initialize(&mut self) -> Result<(Value, u128), String> {
        let started = Instant::now();
        let id = self.send_control(json!({"subtype": "initialize", "hooks": {}}))?;
        let resp = self.await_control_response(&id).await?;
        Ok((resp, started.elapsed().as_millis()))
    }

    /// Sends one user turn and drives it to its `result` frame.
    async fn run_turn(
        &mut self,
        prompt: &str,
        policy: &mut Policy<'_>,
        interrupt_after: Option<Duration>,
    ) -> Result<TurnOutcome, String> {
        let mut out = TurnOutcome::default();
        let sent_at = Instant::now();
        self.send_user(prompt)?;
        let deadline = sent_at + TURN_TIMEOUT;
        let mut first_delta_at: Option<Instant> = None;
        // Interrupt timing is measured from the first *text* delta: models
        // with adaptive thinking stream sparse thinking deltas first.
        let mut first_text_at: Option<Instant> = None;
        let mut interrupt_id: Option<String> = None;
        loop {
            let ev = match self.next_event(deadline).await {
                Ok(ev) => ev,
                Err(e) => return Err(format!("turn {prompt:?}: {e}")),
            };
            let (at, v) = match ev {
                Event::Line(at, v) => (at, v),
                Event::Unparsed(line) => {
                    println!("[#{}] (non-JSON) {}", self.proc_no, trim(&line, 200));
                    continue;
                }
                Event::Eof => {
                    out.eof = true;
                    return Ok(out);
                }
            };
            self.observe(at, &v);
            match v["type"].as_str().unwrap_or("") {
                "stream_event" => {
                    let event = &v["event"];
                    if event["type"] == "content_block_delta" {
                        if first_delta_at.is_none() {
                            first_delta_at = Some(at);
                            out.first_delta_ms = Some(at.duration_since(sent_at).as_millis());
                        }
                        if let Some(t) = event["delta"]["text"].as_str() {
                            first_text_at.get_or_insert(at);
                            out.streamed_text.push_str(t);
                        }
                    }
                }
                "user" => {
                    if let Some(blocks) = v["message"]["content"].as_array() {
                        for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                            out.tool_results.push(b.clone());
                        }
                    }
                }
                "control_request" => {
                    let request_id = v["request_id"]
                        .as_str()
                        .ok_or("control_request without request_id")?;
                    if v["request"]["subtype"] == "can_use_tool" {
                        let req: CanUseTool = serde_json::from_value(v["request"].clone())
                            .map_err(|e| {
                                format!("bad can_use_tool frame {}: {e}", trim(&v.to_string(), 300))
                            })?;
                        let answer = policy(&req);
                        self.send_control_response(ControlResponseBody::Success {
                            request_id,
                            response: to_value(&answer)?,
                        })?;
                        out.permission_answers.push(answer);
                        out.permission_requests.push((req, v.clone()));
                    } else {
                        out.other_control_requests.push(v.clone());
                        self.answer_other_control(&v)?;
                    }
                }
                "control_response" => {
                    let id = v["response"]["request_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    if interrupt_id.as_deref() == Some(id.as_str()) {
                        out.interrupt_response = Some(v.clone());
                    } else {
                        self.stray_responses.insert(id, v.clone());
                    }
                }
                "result" => {
                    out.result = Some(v.clone());
                    // The interrupt receipt is written before the result on a
                    // clean interrupt, but tolerate it arriving later.
                    if let Some(id) = interrupt_id.as_deref() {
                        if out.interrupt_response.is_none() {
                            out.interrupt_response = self.stray_responses.remove(id);
                        }
                    }
                    return Ok(out);
                }
                _ => {}
            }
            if let (Some(after), Some(fd), None) =
                (interrupt_after, first_text_at, interrupt_id.as_ref())
            {
                if at.duration_since(fd) >= after {
                    out.interrupt_sent_ms =
                        Some(Instant::now().duration_since(sent_at).as_millis());
                    interrupt_id = Some(self.send_control(json!({"subtype": "interrupt"}))?);
                }
            }
        }
    }

    /// Closes stdin and waits for the CLI to exit on its own.
    async fn shutdown(mut self) -> Result<(), String> {
        self.stdin_tx = None;
        // Drain remaining output so the reader task can finish.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.next_event(deadline).await {
                Ok(Event::Line(at, v)) => self.observe(at, &v),
                Ok(Event::Unparsed(_)) => {}
                Ok(Event::Eof) => break,
                Err(e) => {
                    log_line(&format!("#{} shutdown: {e}; killing", self.proc_no));
                    self.child
                        .kill()
                        .await
                        .map_err(|e| format!("kill claude #{}: {e}", self.proc_no))?;
                    break;
                }
            }
        }
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| format!("wait claude #{}: {e}", self.proc_no))?;
        log_line(&format!("#{} exited: {status}", self.proc_no));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn log_line(s: &str) {
    println!("[probe] {s}");
}

fn trim(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}...")
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("serialize: {e}"))
}

fn summarize(v: &Value) -> String {
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("");
    match s("/type") {
        "system" => match s("/subtype") {
            "init" => format!(
                "system/init session_id={} model={} permissionMode={} tools={}",
                s("/session_id"),
                s("/model"),
                s("/permissionMode"),
                v["tools"].as_array().map(|a| a.len()).unwrap_or(0)
            ),
            "session_state_changed" => format!("system/session_state_changed state={}", s("/state")),
            other => format!("system/{other}"),
        },
        "stream_event" => {
            let e = &v["event"];
            match e["type"].as_str().unwrap_or("") {
                "content_block_delta" => match e["delta"]["type"].as_str().unwrap_or("") {
                    "text_delta" => format!("delta text {:?}", trim(e["delta"]["text"].as_str().unwrap_or(""), 60)),
                    "input_json_delta" => "delta input_json".into(),
                    other => format!("delta {other}"),
                },
                "content_block_start" => format!("stream content_block_start {}", e["content_block"]["type"].as_str().unwrap_or("")),
                other => format!("stream {other}"),
            }
        }
        "assistant" => {
            let blocks: Vec<String> = v["message"]["content"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|b| match b["type"].as_str().unwrap_or("") {
                            "text" => format!("text {:?}", trim(b["text"].as_str().unwrap_or(""), 80)),
                            "tool_use" => format!("tool_use {} {}", b["name"].as_str().unwrap_or(""), trim(&b["input"].to_string(), 120)),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!("assistant [{}]", blocks.join(", "))
        }
        "user" => {
            let blocks: Vec<String> = v["message"]["content"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|b| match b["type"].as_str().unwrap_or("") {
                            "tool_result" => format!(
                                "tool_result is_error={} {}",
                                b["is_error"],
                                trim(&b["content"].to_string(), 160)
                            ),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!("user [{}]", blocks.join(", "))
        }
        "result" => format!(
            "result subtype={} is_error={} terminal_reason={} stop_reason={} num_turns={} duration_ms={} cost=${} text={:?}",
            s("/subtype"),
            v["is_error"],
            v["terminal_reason"],
            v["stop_reason"],
            v["num_turns"],
            v["duration_ms"],
            v["total_cost_usd"],
            trim(s("/result"), 120)
        ),
        "control_request" => format!(
            "<< control_request {} id={} {}",
            s("/request/subtype"),
            s("/request_id"),
            trim(&v["request"].to_string(), 400)
        ),
        "control_response" => format!(
            "<< control_response {} id={} {}",
            s("/response/subtype"),
            s("/response/request_id"),
            trim(&v["response"].to_string(), 300)
        ),
        "control_cancel_request" => format!("<< control_cancel_request id={}", s("/request_id")),
        "keep_alive" => String::new(),
        "rate_limit_event" => "rate_limit_event".into(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

struct ScenarioResult {
    name: String,
    pass: bool,
    notes: Vec<String>,
}

impl ScenarioResult {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            pass: false,
            notes: Vec::new(),
        }
    }
    fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        println!("[probe]   note: {s}");
        self.notes.push(s);
    }
}

fn allow_as_is(req: &CanUseTool) -> PermissionResult {
    PermissionResult::Allow {
        updated_input: req.input.clone(),
        updated_permissions: None,
        tool_use_id: req.tool_use_id.clone(),
    }
}

fn describe_requests(r: &mut ScenarioResult, out: &TurnOutcome) {
    for (req, frame) in &out.permission_requests {
        r.note(format!(
            "can_use_tool tool={} title={:?} decision_reason_type={:?} decision_reason={:?} blocked_path={:?} suggestions={}",
            req.tool_name,
            req.title,
            req.decision_reason_type,
            req.decision_reason,
            req.blocked_path,
            req.permission_suggestions
                .as_ref()
                .map(|s| Value::Array(s.clone()).to_string())
                .unwrap_or_else(|| "<absent>".into())
        ));
        r.note(format!("frame: {}", trim(&frame.to_string(), 700)));
    }
    for a in &out.permission_answers {
        r.note(format!(
            "answered: {}",
            trim(&to_value(a).map(|v| v.to_string()).unwrap_or_default(), 500)
        ));
    }
    for t in &out.tool_results {
        r.note(format!("tool_result: {}", trim(&t.to_string(), 300)));
    }
    r.note(format!(
        "result: subtype={} is_error={} terminal_reason={} cost={} text={:?}",
        out.result_field("subtype"),
        out.result_field("is_error"),
        out.result_field("terminal_reason"),
        out.result_field("total_cost_usd"),
        trim(&out.result_text(), 200)
    ));
    if let Some(ms) = out.first_delta_ms {
        r.note(format!("prompt -> first delta: {ms} ms"));
    }
    for o in &out.other_control_requests {
        r.note(format!(
            "other control_request: {}",
            trim(&o.to_string(), 300)
        ));
    }
}

/// Scenario 0: no initialize at all. Does a can_use_tool still arrive?
async fn scenario0(cfg: &ProbeConfig, proc_no: usize) -> Result<ScenarioResult, String> {
    let mut r = ScenarioResult::new("0 can_use_tool without initialize");
    let mut s = Session::spawn(cfg, proc_no, None).await?;
    let out = s
        .run_turn(
            "Use the Bash tool to run `touch probe-zero.txt`. Then reply with done.",
            &mut allow_as_is,
            None,
        )
        .await?;
    describe_requests(&mut r, &out);
    r.pass = !out.permission_requests.is_empty() && out.result.is_some();
    if out.permission_requests.is_empty() {
        r.note("no can_use_tool arrived without initialize");
    }
    s.shutdown().await?;
    Ok(r)
}

async fn run_main_process(
    cfg: &ProbeConfig,
    scenarios: &[String],
    proc_no: usize,
) -> Result<(Vec<ScenarioResult>, Option<String>), String> {
    let mut results = Vec::new();
    let wants = |n: &str| scenarios.iter().any(|s| s == n);
    let mut s = Session::spawn(cfg, proc_no, None).await?;

    // Scenario 1: initialize + first turn.
    let mut r1 = ScenarioResult::new("1 initialize + first turn");
    let (init, init_ms) = s.initialize().await?;
    let resp = &init["response"];
    r1.note(format!(
        "initialize reply: subtype={} {} ms keys={:?}",
        resp["subtype"],
        init_ms,
        resp["response"]
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    ));
    r1.note(format!(
        "initialize: current_permission_mode={} session_state={} capabilities={} pending_permission_requests={}",
        resp["response"]["current_permission_mode"],
        resp["response"]["session_state"],
        resp["response"]["capabilities"],
        resp["pending_permission_requests"]
    ));
    r1.note(format!(
        "spawn -> initialize reply: {} ms",
        s.spawned_at.elapsed().as_millis()
    ));
    let out = s
        .run_turn("Reply with exactly: ready", &mut allow_as_is, None)
        .await?;
    if let Some(at) = s.init_at {
        r1.note(format!("spawn -> system/init: {} ms (system/init is only emitted once the first user message arrives)", at.duration_since(s.spawned_at).as_millis()));
    }
    describe_requests(&mut r1, &out);
    r1.note(format!(
        "session_id={:?} streamed_text={:?}",
        s.session_id,
        trim(&out.streamed_text, 80)
    ));
    r1.pass = resp["subtype"] == "success"
        && s.session_id.is_some()
        && out.first_delta_ms.is_some()
        && out.result_text().contains("ready");
    results.push(r1);

    if wants("2") {
        let mut r = ScenarioResult::new("2a Bash allow with updatedInput");
        let out = s
            .run_turn(
                "Use the Bash tool to run `touch probe-ok.txt`. Then reply with done.",
                &mut allow_as_is,
                None,
            )
            .await?;
        describe_requests(&mut r, &out);
        let ran = !out.tool_results.is_empty()
            && out.tool_results.iter().all(|t| t["is_error"] != true)
            && cfg.workdir.join("probe-ok.txt").exists();
        r.note(format!(
            "probe-ok.txt exists: {}",
            cfg.workdir.join("probe-ok.txt").exists()
        ));
        r.pass = out
            .permission_requests
            .iter()
            .any(|(q, _)| q.tool_name == "Bash")
            && ran
            && out.result.is_some();
        results.push(r);

        let mut r = ScenarioResult::new("2b Bash deny with message");
        let mut deny = |req: &CanUseTool| PermissionResult::Deny {
            message: "PROBE-DENY: the user declined this command".into(),
            tool_use_id: req.tool_use_id.clone(),
        };
        let out = s
            .run_turn(
                "Use the Bash tool to run `touch probe-denied.txt`. If it is not allowed, tell me the exact reason you were given.",
                &mut deny,
                None,
            )
            .await?;
        describe_requests(&mut r, &out);
        let seen = out
            .tool_results
            .iter()
            .any(|t| t.to_string().contains("PROBE-DENY"))
            || out.result_text().contains("PROBE-DENY");
        r.pass = !out.permission_requests.is_empty() && seen && out.result.is_some();
        results.push(r);
    }

    if wants("6") {
        let mut r =
            ScenarioResult::new("6 permission_suggestions + localSettings updatedPermissions");
        let mut chosen: Option<Value> = None;
        let mut allow_persist = |req: &CanUseTool| {
            let suggestions = req.permission_suggestions.clone().unwrap_or_default();
            let local = suggestions
                .iter()
                .find(|s| s["destination"] == "localSettings")
                .cloned();
            chosen = local.clone();
            PermissionResult::Allow {
                updated_input: req.input.clone(),
                updated_permissions: local.map(|l| vec![l]),
                tool_use_id: req.tool_use_id.clone(),
            }
        };
        let out = s
            .run_turn(
                "Use the Bash tool to run `touch probe-six.txt`. Then reply with done.",
                &mut allow_persist,
                None,
            )
            .await?;
        describe_requests(&mut r, &out);
        r.note(format!(
            "echoed localSettings suggestion: {}",
            chosen
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "<none present>".into())
        ));
        let settings_path = cfg.workdir.join(".claude").join("settings.local.json");
        r.note(format!(
            "{} after allow: {}",
            settings_path.display(),
            std::fs::read_to_string(&settings_path)
                .map(|c| trim(&c, 300))
                .unwrap_or_else(|e| format!("<unreadable: {e}>"))
        ));
        let mut repeat_policy = allow_as_is;
        let out2 = s
            .run_turn("Use the Bash tool to run `touch probe-six.txt` again, exactly the same command. Then reply with done.", &mut repeat_policy, None)
            .await?;
        let prompted_again = !out2.permission_requests.is_empty();
        let ran_again = out2.tool_results.iter().any(|t| t["is_error"] != true);
        r.note(format!(
            "repeat: prompted_again={prompted_again} ran_again={ran_again}"
        ));
        describe_requests(&mut r, &out2);
        r.pass = !out.permission_requests.is_empty()
            && (chosen.is_none() || (!prompted_again && ran_again));
        results.push(r);
    }

    if wants("3") {
        let mut r = ScenarioResult::new("3 AskUserQuestion answered via can_use_tool");
        let mut answered_with: Option<String> = None;
        let mut ask = |req: &CanUseTool| {
            if req.tool_name != "AskUserQuestion" {
                return allow_as_is(req);
            }
            let questions = req.input["questions"].clone();
            let mut answers = serde_json::Map::new();
            for q in questions.as_array().into_iter().flatten() {
                let options: Vec<&str> = q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o["label"].as_str())
                    .collect();
                let pick = options
                    .iter()
                    .find(|l| l.to_lowercase().contains("blue"))
                    .or(options.first())
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "Blue".into());
                answered_with = Some(pick.clone());
                answers.insert(
                    q["question"].as_str().unwrap_or("").to_string(),
                    Value::String(pick),
                );
            }
            PermissionResult::Allow {
                updated_input: json!({"questions": questions, "answers": answers}),
                updated_permissions: None,
                tool_use_id: req.tool_use_id.clone(),
            }
        };
        let out = s
            .run_turn(
                "Before answering, use the AskUserQuestion tool to ask me whether I prefer red or blue, with two options. Then tell me which colour I picked.",
                &mut ask,
                None,
            )
            .await?;
        describe_requests(&mut r, &out);
        r.note(format!("answered_with={answered_with:?}"));
        let asked = out
            .permission_requests
            .iter()
            .any(|(q, _)| q.tool_name == "AskUserQuestion");
        r.pass = asked && out.result_text().to_lowercase().contains("blue");
        results.push(r);
    }

    if wants("4") {
        let mut r = ScenarioResult::new("4 interrupt mid-stream, then a new turn");
        let out = s
            .run_turn(
                "Count from 1 to 1000, writing each number out in English words, one per line. Do not use any tools.",
                &mut allow_as_is,
                Some(Duration::from_secs(2)),
            )
            .await?;
        describe_requests(&mut r, &out);
        r.note(format!(
            "interrupt sent at {:?} ms after prompt; interrupt response: {}",
            out.interrupt_sent_ms,
            out.interrupt_response
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".into())
        ));
        r.note(format!(
            "streamed text tail before stop: {:?}",
            trim(
                &out.streamed_text
                    .chars()
                    .rev()
                    .take(40)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>(),
                40
            )
        ));
        let interrupted = out
            .interrupt_response
            .as_ref()
            .map(|v| v["response"]["subtype"] == "success")
            .unwrap_or(false);
        let after = s
            .run_turn(
                "Reply with exactly: after-interrupt",
                &mut allow_as_is,
                None,
            )
            .await?;
        r.note(format!(
            "follow-up turn: subtype={} text={:?}",
            after.result_field("subtype"),
            trim(&after.result_text(), 80)
        ));
        r.pass = interrupted
            && out.result.is_some()
            && !out.streamed_text.to_lowercase().contains("one thousand")
            && after.result_text().contains("after-interrupt");
        results.push(r);
    }

    if wants("7") {
        let mut r = ScenarioResult::new("7 set_permission_mode / set_model / error subtype");
        let id = s.send_control(json!({"subtype": "set_permission_mode", "mode": "plan"}))?;
        let v = s.await_control_response(&id).await?;
        r.note(format!("set_permission_mode plan -> {}", v));
        let ok1 = v["response"]["subtype"] == "success";
        let id = s.send_control(json!({"subtype": "set_permission_mode", "mode": "default"}))?;
        let v = s.await_control_response(&id).await?;
        r.note(format!("set_permission_mode default -> {}", v));
        let ok2 = v["response"]["subtype"] == "success";
        let id = s.send_control(json!({"subtype": "set_model", "model": "haiku"}))?;
        let v = s.await_control_response(&id).await?;
        r.note(format!("set_model haiku -> {}", v));
        let ok3 = v["response"]["subtype"] == "success";
        let id = s.send_control(json!({"subtype": "set_permission_mode", "mode": "not-a-mode"}))?;
        let v = s.await_control_response(&id).await?;
        r.note(format!("set_permission_mode not-a-mode -> {}", v));
        let err = v["response"]["subtype"] == "error";
        let id = s.send_control(json!({"subtype": "no_such_subtype"}))?;
        let v = s.await_control_response(&id).await?;
        r.note(format!("unknown subtype -> {}", v));
        r.pass = ok1 && ok2 && ok3 && err;
        results.push(r);
    }

    let session_id = s.session_id.clone();
    s.shutdown().await?;
    Ok((results, session_id))
}

async fn scenario5(
    cfg: &ProbeConfig,
    proc_no: usize,
    session_id: &str,
) -> Result<ScenarioResult, String> {
    let mut r = ScenarioResult::new("5 resume in a new process");
    let mut s = Session::spawn(cfg, proc_no, Some(session_id)).await?;
    let (init, init_ms) = s.initialize().await?;
    r.note(format!(
        "initialize on resume: subtype={} {} ms",
        init["response"]["subtype"], init_ms
    ));
    let out = s
        .run_turn(
            "What was the FIRST file name I asked you to create with touch in this conversation? Answer with just the file name.",
            &mut allow_as_is,
            None,
        )
        .await?;
    if let Some(at) = s.init_at {
        r.note(format!(
            "spawn -> system/init: {} ms",
            at.duration_since(s.spawned_at).as_millis()
        ));
    }
    describe_requests(&mut r, &out);
    r.note(format!(
        "resumed session_id={:?} (original {session_id})",
        s.session_id
    ));
    r.pass = out.result_text().contains("probe-ok") && out.permission_requests.is_empty();
    s.shutdown().await?;
    Ok(r)
}

fn parse_args() -> Result<(ProbeConfig, Vec<String>), String> {
    let mut workdir: Option<PathBuf> = None;
    let mut model = "haiku".to_string();
    let mut scenarios: Vec<String> = ["0", "1", "2", "6", "3", "4", "7", "5"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workdir" => {
                workdir = Some(PathBuf::from(args.next().ok_or("--workdir needs a value")?))
            }
            "--model" => model = args.next().ok_or("--model needs a value")?,
            "--scenarios" => {
                scenarios = args
                    .next()
                    .ok_or("--scenarios needs a value")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let workdir = workdir.ok_or("usage: claude_control_probe --workdir <empty dir> [--model haiku] [--scenarios 0,1,2,6,3,4,7,5]")?;
    std::fs::create_dir_all(&workdir)
        .map_err(|e| format!("create workdir {}: {e}", workdir.display()))?;
    let workdir = workdir
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", workdir.display()))?;
    let logdir = PathBuf::from(format!("{}-logs", workdir.display()));
    std::fs::create_dir_all(&logdir)
        .map_err(|e| format!("create logdir {}: {e}", logdir.display()))?;
    Ok((
        ProbeConfig {
            workdir,
            logdir,
            model,
        },
        scenarios,
    ))
}

fn print_table(results: &[ScenarioResult], logdir: &Path) {
    println!("\n================ claude control probe summary ================");
    for r in results {
        println!("{} {}", if r.pass { "PASS" } else { "FAIL" }, r.name);
    }
    println!("logs: {}", logdir.display());
}

#[tokio::main]
async fn main() {
    let (cfg, scenarios) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut results: Vec<ScenarioResult> = Vec::new();
    let mut proc_no = 0;
    let wants = |n: &str| scenarios.iter().any(|s| s == n);

    if wants("0") {
        proc_no += 1;
        match scenario0(&cfg, proc_no).await {
            Ok(r) => results.push(r),
            Err(e) => {
                let mut r = ScenarioResult::new("0 can_use_tool without initialize");
                r.note(format!("error: {e}"));
                results.push(r);
            }
        }
    }

    proc_no += 1;
    let session_id = match run_main_process(&cfg, &scenarios, proc_no).await {
        Ok((rs, sid)) => {
            results.extend(rs);
            sid
        }
        Err(e) => {
            let mut r = ScenarioResult::new("main process");
            r.note(format!("error: {e}"));
            results.push(r);
            None
        }
    };

    if wants("5") {
        proc_no += 1;
        let r = match session_id.as_deref() {
            Some(id) => scenario5(&cfg, proc_no, id).await.unwrap_or_else(|e| {
                let mut r = ScenarioResult::new("5 resume in a new process");
                r.note(format!("error: {e}"));
                r
            }),
            None => {
                let mut r = ScenarioResult::new("5 resume in a new process");
                r.note("no session_id captured from the main process");
                r
            }
        };
        results.push(r);
    }

    print_table(&results, &cfg.logdir);
    let mut summary = String::new();
    for r in &results {
        summary.push_str(&format!(
            "{} {}\n",
            if r.pass { "PASS" } else { "FAIL" },
            r.name
        ));
        for n in &r.notes {
            summary.push_str(&format!("    {n}\n"));
        }
    }
    let summary_path = cfg.logdir.join("summary.txt");
    if let Err(e) = std::fs::write(&summary_path, summary) {
        eprintln!("[probe] write {}: {e}", summary_path.display());
    }
    if results.iter().any(|r| !r.pass) {
        std::process::exit(1);
    }
}
