//! Local Claude stream-json process service. Launch policy and all roots come
//! from the host; this module never discovers a home, config, executable or env.
use super::claude_parser::{
    control_request_frame, control_response_frame, permission_result, user_message_frame,
    ClaudeFrameParser, ControlResponseBody, HostRequest, ParsedFrame, SnapshotHook,
};
use crate::chat::runtime::{reject_queued, DeliveryOutcome, EventSink, RuntimeCommand};
use crate::chat::types::{
    HarnessCommand, HarnessEvent, RuntimeDecision, RuntimeRequestKind, TurnStatus,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

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

/// Awaited on the process loop, including before replying to synchronous hooks.
/// The host supplies an already rooted C4 recorder; it retains root-discovery
/// policy and the recorder's cancellation token. No callback is detached.
pub type SnapshotFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
pub trait SnapshotRecorder: Send {
    fn observe(&mut self, event: &HarnessEvent);
    fn invalidate_end(&mut self);
    fn discard_turn(&mut self);
    fn bind<'a>(&'a mut self, chat: &'a str) -> SnapshotFuture<'a>;
    fn begin(&mut self) -> SnapshotFuture<'_>;
    fn capture_end(&mut self) -> SnapshotFuture<'_>;
    fn finish(&mut self) -> SnapshotFuture<'_>;
    fn close(&mut self) -> SnapshotFuture<'_>;
}

impl SnapshotRecorder for crate::turn_snapshots::Recorder {
    fn observe(&mut self, event: &HarnessEvent) {
        self.observe(event);
    }
    fn invalidate_end(&mut self) {
        self.invalidate_end();
    }
    fn discard_turn(&mut self) {
        self.discard_turn();
    }
    fn bind<'a>(&'a mut self, chat: &'a str) -> SnapshotFuture<'a> {
        Box::pin(self.bind(chat))
    }
    fn begin(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.begin())
    }
    fn capture_end(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.capture_end())
    }
    fn finish(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.finish())
    }
    fn close(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.close())
    }
}

/// Complete local launch inputs. Environment values can include credentials:
/// deliberately no Debug implementation. The complete environment, including
/// HOME and the provider's CLAUDE_CONFIG_DIR policy, is explicit; nothing is
/// inherited by the child. MCP environment overrides are already applied by
/// the host in server order. This is not a sandbox or a preview activation gate.
pub struct Launch {
    pub flags: LaunchFlags,
    pub config: ClaudeSessionConfig,
    pub program: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    pub temporary_dir: PathBuf,
    pub snapshots: Box<dyn SnapshotRecorder>,
}

/// Typed host launch policy; callers cannot inject arbitrary CLI arguments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LaunchFlags {
    #[default]
    Standard,
    FreshPreview,
    /// `FreshPreview` plus `mcp_servers` (with their `allowed_tools`); every
    /// other restriction and flag is the same. A server's credential env
    /// values (every key not in its `public_env`) must reach the child only
    /// through env, as `${VAR}` in config.
    PreviewTools,
}
impl LaunchFlags {
    pub fn args(self, config: &ClaudeSessionConfig) -> Result<Vec<String>, String> {
        self.args_with_guidelines_file(config, config.guidelines_file.as_deref())
    }
    fn args_with_guidelines_file(
        self,
        config: &ClaudeSessionConfig,
        guidelines: Option<&Path>,
    ) -> Result<Vec<String>, String> {
        let mut args = config.args_with_guidelines_file(guidelines);
        if self == Self::PreviewTools {
            preview_env_stays_out_of_argv(&config.mcp_servers)?;
        }
        if matches!(self, Self::FreshPreview | Self::PreviewTools) {
            if config.resume.is_some()
                || config.chrome
                || (self == Self::FreshPreview && !config.mcp_servers.is_empty())
                || config.snapshot_facts_dir.is_some()
                || config.wire_log_dir.is_some()
                || guidelines.is_some()
                || config.guidelines_content.is_some()
                || config.model.is_some()
                || config.effort.is_some()
                || config.permission_mode != "default"
            {
                return Err("fresh preview requires the fixed launch configuration".into());
            }
            args.extend(["--restricted", "--strict-mcp-config", "--safe-mode"].map(str::to_owned));
        }
        Ok(args)
    }
}

/// Refuses a preview launch in which a credential, that is any server env
/// value whose key is not in that server's `public_env`, appears literally
/// in any server's `--mcp-config` or `--allowedTools`. Errors name the key,
/// never the value.
fn preview_env_stays_out_of_argv(servers: &[ClaudeMcpServer]) -> Result<(), String> {
    let mut argv = Vec::new();
    for server in servers {
        if let Some(name) = server
            .public_env
            .iter()
            .find(|name| !server.env.iter().any(|(key, _)| key == *name))
        {
            return Err(format!(
                "preview MCP public env {name} is not in the server's env"
            ));
        }
        argv.push(server.config.to_string());
        argv.extend(server.allowed_tools.iter().cloned());
    }
    for server in servers {
        for (name, value) in &server.env {
            if server.public_env.contains(name) {
                continue;
            }
            if value.is_empty() {
                return Err(format!("preview MCP env {name} is empty"));
            }
            // The JSON-escaped form is what a literal looks like inside the
            // serialized config.
            let escaped = Value::String(value.clone()).to_string();
            let escaped = &escaped[1..escaped.len() - 1];
            if argv
                .iter()
                .any(|arg| arg.contains(value.as_str()) || arg.contains(escaped))
            {
                return Err(format!(
                    "preview MCP env {name} appears literally in argv; reference it as ${{{name}}}"
                ));
            }
        }
    }
    Ok(())
}

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
    /// Keys of `env` whose values are not credentials (a caller URL) and so
    /// may also appear literally in `config`. Every other key is treated as
    /// a credential: an unlisted value is refused from argv, not allowed
    /// (`LaunchFlags::PreviewTools`).
    pub public_env: Vec<String>,
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
            .field("public_env", &self.public_env)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeSessionConfig {
    /// Local snapshot metadata directory; None disables snapshots (remote/test sessions).
    pub snapshot_facts_dir: Option<PathBuf>,
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
    /// Claude in Chrome: `--chrome` when on, `--no-chrome` when off
    /// (TRU-181). Off unless the chat's workspace switches it on: GitTerm's
    /// own browser tools are the agents' browser. Always explicit, so the
    /// CLI's own default never decides.
    pub chrome: bool,
    /// GitTerm's change guidelines (TRU-174), appended to Claude's system
    /// prompt with `--append-system-prompt-file`. The caller checks the
    /// file is readable: a missing one makes the CLI refuse to start.
    pub guidelines_file: Option<PathBuf>,
    /// Remote-start guidelines supplied by the desktop. Written to a
    /// temporary file for the Claude process lifetime on the agent.
    pub guidelines_content: Option<String>,
}

impl ClaudeSessionConfig {
    pub fn args(&self) -> Vec<String> {
        self.args_with_guidelines_file(self.guidelines_file.as_deref())
    }

    fn args_with_guidelines_file(&self, guidelines_file: Option<&Path>) -> Vec<String> {
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
        args.push(chrome_flag(self.chrome).into());
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
        if let Some(path) = guidelines_file {
            args.push("--append-system-prompt-file".into());
            args.push(path.to_string_lossy().into_owned());
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

/// The Claude in Chrome option for a launch (TRU-181).
pub fn chrome_flag(chrome: bool) -> &'static str {
    if chrome {
        "--chrome"
    } else {
        "--no-chrome"
    }
}

/// A terminal startup command with the Claude in Chrome option added right
/// after the executable when it starts `claude` (TRU-181). A command that
/// already passes `--chrome` or `--no-chrome` keeps that choice; any other
/// command is returned unchanged.
pub fn configure_terminal_chrome(command: &str, chrome: bool) -> String {
    let trimmed = command.trim();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let (executable, rest) = trimmed.split_at(end);
    let name = executable
        .trim_matches(['\'', '"'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable);
    let chosen = rest
        .split_whitespace()
        .any(|word| word == "--chrome" || word == "--no-chrome");
    if name != "claude" || chosen {
        return command.to_string();
    }
    format!("{executable} {}{rest}", chrome_flag(chrome))
}

struct RemoteGuidelinesFile(PathBuf);

impl RemoteGuidelinesFile {
    fn create(temporary_dir: &Path, text: &str) -> Result<Self, String> {
        let path = temporary_dir.join(format!(
            "gitterm-v6-remote-guidelines-{}.md",
            uuid::Uuid::new_v4()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("create {}: {error}", path.display()))?;
        if let Err(error) = file.write_all(text.as_bytes()).and_then(|_| file.flush()) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("write {}: {error}", path.display()));
        }
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RemoteGuidelinesFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                harness_log(&format!(
                    "remove temporary remote guidelines {}: {error}",
                    self.0.display()
                ));
            }
        }
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
    kind: RuntimeRequestKind,
    delivery_uncertain: bool,
}

fn record_pending(
    pending: &mut HashMap<String, PendingRequest>,
    seen_request_ids: &mut HashSet<String>,
    request_id: String,
    tool_use_id: String,
    input: Value,
    kind: RuntimeRequestKind,
) -> Result<(), String> {
    match pending.entry(request_id) {
        std::collections::hash_map::Entry::Occupied(entry) => Err(format!(
            "duplicate pending Claude request {}; original request retained",
            entry.key()
        )),
        std::collections::hash_map::Entry::Vacant(entry) => {
            // Answers can already be queued when a request is withdrawn. Never
            // let their bare ID refer to another request in this runtime.
            if !seen_request_ids.insert(entry.key().clone()) {
                return Err(format!(
                    "reused Claude request {}; IDs must be unique for the runtime; request rejected",
                    entry.key()
                ));
            }
            entry.insert(PendingRequest {
                tool_use_id,
                input,
                kind,
                delivery_uncertain: false,
            });
            Ok(())
        }
    }
}

#[derive(Debug)]
struct WriteFailure {
    message: String,
    uncertain: bool,
}

impl From<String> for WriteFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            uncertain: false,
        }
    }
}

impl WriteFailure {
    fn outcome(&self) -> DeliveryOutcome {
        if self.uncertain {
            DeliveryOutcome::Uncertain(self.message.clone())
        } else {
            DeliveryOutcome::Rejected(self.message.clone())
        }
    }
}

struct SessionIo<W = ChildStdin> {
    stdin: Option<W>,
    wire: Option<WireLog>,
    events: EventSink,
}

impl<W: AsyncWrite + Unpin> SessionIo<W> {
    async fn write(&mut self, line: String) -> Result<(), String> {
        self.write_delivery(line).await.map_err(|e| e.message)
    }

    async fn write_delivery(&mut self, line: String) -> Result<(), WriteFailure> {
        if let Some(wire) = self.wire.as_mut() {
            if let Err(e) = writeln!(wire.stdin, "{line}") {
                harness_log(&format!("wire log write failed: {e}"));
            }
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| WriteFailure {
            message: "claude stdin is closed".into(),
            uncertain: false,
        })?;
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let mut written = 0;
        while written < bytes.len() {
            match stdin.write(&bytes[written..]).await {
                Ok(0) => {
                    return Err(WriteFailure {
                        message: "write to claude stdin: write zero".into(),
                        uncertain: written != 0,
                    })
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(WriteFailure {
                        message: format!("write to claude stdin: {e}"),
                        uncertain: written != 0,
                    })
                }
            }
        }
        stdin.flush().await.map_err(|e| WriteFailure {
            message: format!("flush claude stdin: {e}"),
            uncertain: true,
        })
    }

    async fn answer(
        &mut self,
        pending: &mut HashMap<String, PendingRequest>,
        request_id: String,
        decision: RuntimeDecision,
    ) -> Result<(), WriteFailure> {
        let req = pending.get_mut(&request_id).ok_or_else(|| {
            format!("no pending Claude request {request_id} (already answered or withdrawn)")
        })?;
        if req.delivery_uncertain {
            return Err(
                format!("answer {request_id}: delivery uncertain; resending is blocked").into(),
            );
        }
        if let RuntimeDecision::Allow {
            remember: Some(update),
            ..
        } = &decision
        {
            if !matches!(&req.kind, RuntimeRequestKind::Permission { suggestions, .. } if suggestions.contains(update))
            {
                return Err(format!("answer {request_id}: remember choice was not offered for this request; not delivered").into());
            }
        }
        let body = permission_result(&decision, &req.tool_use_id, &req.input)?;
        harness_log(&format!(
            "answer {request_id}: {}",
            trim_for_log(&body.to_string(), 300)
        ));
        let frame = control_response_frame(ControlResponseBody::Success {
            request_id: request_id.clone(),
            response: body,
        });
        match self.write_delivery(frame).await {
            Ok(()) => {
                pending.remove(&request_id);
                self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                Ok(())
            }
            Err(e) => {
                req.delivery_uncertain = e.uncertain;
                Err(WriteFailure {
                    uncertain: e.uncertain,
                    message: format!(
                        "answer {request_id}: {}; {}",
                        e.message,
                        if e.uncertain {
                            "delivery uncertain; resending is blocked"
                        } else {
                            "not delivered; request remains pending"
                        }
                    ),
                })
            }
        }
    }

    fn emit(&self, ev: HarnessEvent) {
        // The receiver is gone only when the tab is; nothing to report to.
        self.events.emit(ev);
    }
}

// SessionStart SDK callbacks and additionalContext are documented at
// https://code.claude.com/docs/en/agent-sdk/hooks and /docs/en/hooks#sessionstart.
fn session_hooks(snapshots: bool, workspace_context: bool) -> Value {
    let mut hooks = json!({});
    if snapshots {
        hooks["UserPromptSubmit"] =
            json!([{"hookCallbackIds": ["gitterm_turn_start"], "timeout": 60}]);
        hooks["PreToolUse"] = json!([{"hookCallbackIds": ["gitterm_turn_tool"], "timeout": 60}]);
        hooks["Stop"] = json!([{"hookCallbackIds": ["gitterm_turn_stop"], "timeout": 60}]);
    }
    if workspace_context {
        hooks["SessionStart"] =
            json!([{"hookCallbackIds": ["gitterm_workspace_context"], "timeout": 60}]);
    }
    hooks
}

fn workspace_context_response(
    instructions: Option<&str>,
    system_checkpoint: Option<&Path>,
) -> Value {
    let context = instructions
        .and_then(crate::guidelines::workspace_context)
        .map(|text| {
            if instructions.and_then(crate::guidelines::workspace_checkpoint) != system_checkpoint {
                format!(
                    "This chat now belongs to the following workspace. This note, including its \
                     checkpoint, replaces the workspace section in the system prompt.\n\n{text}"
                )
            } else {
                text
            }
        });
    context.map_or_else(
        || json!({}),
        |text| {
            json!({
                "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": text}
            })
        },
    )
}

/// Compatibility entry point: uncorrelated harness commands and events only.
pub async fn run_session_program(
    launch: Launch,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    close_token: tokio_util::sync::CancellationToken,
    process_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    run_correlated_session_program(
        launch,
        commands,
        EventSink::Harness(events),
        close_token,
        process_id,
    )
    .await;
}

pub(crate) async fn run_correlated_session_program<C: Into<RuntimeCommand>>(
    launch: Launch,
    mut commands: mpsc::UnboundedReceiver<C>,
    events: EventSink,
    close_token: tokio_util::sync::CancellationToken,
    process_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    let Launch {
        flags,
        config,
        program,
        environment,
        temporary_dir,
        mut snapshots,
    } = launch;
    let mut fail = async |msg: String| {
        harness_log(&msg);
        events.emit(HarnessEvent::Error(msg));
        reject_queued(&mut commands, &events).await;
        events.emit(HarnessEvent::ProcessExited { code: None });
    };
    if let Err(error) = flags.args(&config) {
        return fail(error).await;
    }
    let remote_guidelines = match config.guidelines_content.as_deref() {
        Some(text) => match RemoteGuidelinesFile::create(&temporary_dir, text) {
            Ok(file) => Some(file),
            Err(error) => return fail(error).await,
        },
        None => None,
    };
    let guidelines_file = remote_guidelines
        .as_ref()
        .map(|file| file.path())
        .or(config.guidelines_file.as_deref());
    let mut workspace_instructions = match guidelines_file {
        Some(path) => match std::fs::read_to_string(path) {
            // Kept whole: each SessionStart rebuilds the checkpoint status
            // and fresh-read instruction from disk (TRU-237/TRU-259).
            Ok(text) => crate::guidelines::workspace_context(&text).map(|_| text),
            Err(error) => {
                return fail(format!(
                    "cannot read chat instructions {}: {error}",
                    path.display()
                ))
                .await
            }
        },
        None => None,
    };
    // Claude retains this workspace section in its system prompt across
    // /clear; a moved chat's callback must explicitly supersede it.
    let system_workspace_checkpoint = workspace_instructions
        .as_deref()
        .and_then(crate::guidelines::workspace_checkpoint)
        .map(Path::to_path_buf);
    let args = match flags.args_with_guidelines_file(&config, guidelines_file) {
        Ok(args) => args,
        Err(error) => return fail(error).await,
    };
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear()
        .envs(environment);

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
            .await
        }
    };
    let pid = child.id().unwrap_or(0);
    let (Some(stdout), Some(stderr), Some(stdin)) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take())
    else {
        return fail("claude child is missing a stdio pipe".to_string()).await;
    };
    process_id.store(pid, std::sync::atomic::Ordering::Relaxed);
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
    // Keep IDs after every resolution/cancellation, until this process exits.
    let mut seen_request_ids = HashSet::new();
    let mut next_id: u64 = 0;
    let mut new_request_id = || {
        next_id += 1;
        format!("gitterm_{pid}_{next_id}")
    };
    let mut interrupt_requested = false;
    // Between `system/init` and `result`. A message sent mid-turn must not
    // clear an interrupt that turn is still answering.
    let mut in_turn = false;
    let mut snapshot_hooks_applied = false;

    // Handshake. Its reply becomes `Ready`. Register SessionStart even
    // without a workspace note: this chat can move into a work workspace.
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
            json!({"subtype": "initialize", "hooks": session_hooks(
                config.snapshot_facts_dir.is_some(), true
            )}),
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
                                    if let Some(id) = &session_id { snapshots.bind(id).await; }
                                    in_turn = true;
                                    HarnessEvent::TurnStarted { session_id, model }
                                }
                                HarnessEvent::TurnCompleted {
                                    status,
                                    usage,
                                    cost_usd,
                                    rate_limit,
                                } => {
                                    // A queued hook's deadline may already be running.
                                    // Leave room for its bounded baseline capture.
                                    if tokio::time::timeout(Duration::from_secs(5), snapshots.finish()).await.is_err() {
                                        snapshots.discard_turn();
                                        io.emit(HarnessEvent::Error("Turn snapshot publication timed out; history may be incomplete".into()));
                                    }
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
                                    HarnessEvent::TurnCompleted {
                                        status,
                                        usage,
                                        cost_usd,
                                        rate_limit,
                                    }
                                }
                                HarnessEvent::Error(msg) => {
                                    harness_log(&format!("error: {msg}"));
                                    HarnessEvent::Error(msg)
                                }
                                other => other,
                            };
                            snapshots.observe(&ev);
                            io.emit(ev);
                        }
                        ParsedFrame::WorkspaceContext { request_id } => {
                            if let Err(error) = io.write(control_response_frame(ControlResponseBody::Success {
                                request_id, response: workspace_context_response(workspace_instructions.as_deref(), system_workspace_checkpoint.as_deref()),
                            })).await {
                                io.emit(HarnessEvent::Error(error));
                            }
                        }
                        ParsedFrame::SnapshotBoundary { request_id, session_id, hook } => {
                            if snapshot_hooks_applied {
                                let capture = async {
                                    snapshots.bind(&session_id).await;
                                    if hook == SnapshotHook::Tool { snapshots.invalidate_end(); }
                                    snapshots.begin().await;
                                    if hook == SnapshotHook::Stop { snapshots.capture_end().await; }
                                };
                                // Leave headroom under the registered SDK callback timeout.
                                let result = tokio::select! {
                                    _ = close_token.cancelled() => Err("snapshot chat closed".to_string()),
                                    result = tokio::time::timeout(Duration::from_secs(45), capture) => result.map_err(|_| "snapshot callback timed out; turn history is incomplete".to_string()),
                                };
                                if let Err(e) = result {
                                    snapshots.invalidate_end();
                                    // Do not put cleanup/GC ahead of the callback reply
                                    // or Shutdown. Sweep after the child is reaped.
                                    if close_token.is_cancelled() { snapshots.discard_turn(); }
                                    else { io.emit(HarnessEvent::Error(e)); }
                                }
                            }
                            in_turn = true;
                            if let Err(e) = io.write(control_response_frame(ControlResponseBody::Success {
                                request_id, response: json!({}),
                            })).await {
                                io.emit(HarnessEvent::Error(e));
                            }
                        }
                        ParsedFrame::PermissionRequest { request_id, tool_use_id, input, event } => {
                            harness_log(&format!(
                                "runtime request {request_id}: {}",
                                trim_for_log(&serde_json::to_string(&event).unwrap_or_default(), 300)
                            ));
                            if let HarnessEvent::RuntimeRequest { kind, .. } = &event {
                                match record_pending(&mut pending, &mut seen_request_ids, request_id, tool_use_id, input, kind.clone()) {
                                    Ok(()) => io.emit(event),
                                    Err(e) => io.emit(HarnessEvent::Error(e)),
                                }
                            } else {
                                io.emit(HarnessEvent::Error(format!("invalid Claude permission request event {request_id}")));
                            }
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
                            if let (HostRequest::Initialize { .. }, Ok(body)) = (&request, &result) {
                                snapshot_hooks_applied = config.snapshot_facts_dir.is_some()
                                    && body["hooks_applied"] == true;
                                if workspace_instructions.is_some() && body["hooks_applied"] != true {
                                    io.emit(HarnessEvent::Error("Workspace context reinjection unavailable: Claude did not register SessionStart hooks".into()));
                                }
                                if config.snapshot_facts_dir.is_some() && !snapshot_hooks_applied {
                                    io.emit(HarnessEvent::Error("Turn snapshots unavailable: Claude did not register synchronous snapshot hooks".into()));
                                }
                            }
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
                let RuntimeCommand { command, correlation } = cmd.into();
                let writes_stdin = !matches!(&command, HarnessCommand::SetWorkspaceContext(_) | HarnessCommand::Shutdown);
                let result = match command {
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
                        io.write_delivery(user_message_frame(&prompt)).await
                    }
                    HarnessCommand::WithdrawQueuedMessage(message_id) => {
                        let id = new_request_id();
                        harness_log(&format!("cancel_async_message {message_id} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::WithdrawMessage(message_id.clone()));
                        io.write_delivery(control_request_frame(
                            &id,
                            json!({"subtype": "cancel_async_message", "message_uuid": message_id}),
                        ))
                        .await
                    }
                    HarnessCommand::Answer { request_id, decision } => {
                        io.answer(&mut pending, request_id, decision).await
                    }
                    HarnessCommand::Interrupt => {
                        let id = new_request_id();
                        harness_log(&format!("interrupt ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::Interrupt);
                        interrupt_requested = true;
                        io.write_delivery(control_request_frame(&id, json!({"subtype": "interrupt"}))).await
                    }
                    HarnessCommand::SetPermissionMode(mode) => {
                        let id = new_request_id();
                        harness_log(&format!("set_permission_mode {mode:?} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::SetPermissionMode(mode.clone()));
                        io.write_delivery(control_request_frame(
                            &id,
                            json!({"subtype": "set_permission_mode", "mode": mode}),
                        ))
                        .await
                    }
                    HarnessCommand::SetModel(model) => {
                        let id = new_request_id();
                        harness_log(&format!("set_model {model:?} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::SetModel(model.clone()));
                        io.write_delivery(control_request_frame(
                            &id,
                            json!({"subtype": "set_model", "model": model}),
                        ))
                        .await
                    }
                    HarnessCommand::SetEffort(effort) => {
                        let id = new_request_id();
                        harness_log(&format!("apply_flag_settings effortLevel={effort:?} ({id})"));
                        parser.register_host_request(id.clone(), HostRequest::SetEffort(effort.clone()));
                        io.write_delivery(control_request_frame(
                            &id,
                            json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": effort}}),
                        ))
                        .await
                    }
                    HarnessCommand::SetWorkspaceContext(context) => {
                        workspace_instructions = context;
                        Ok(())
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
                let outcome = match &result {
                    Ok(()) if writes_stdin => DeliveryOutcome::Written,
                    Ok(()) => DeliveryOutcome::Rejected("command does not write to Claude stdin".into()),
                    Err(error) => error.outcome(),
                };
                io.events.outcome(correlation, outcome);
                if let Err(e) = result {
                    harness_log(&e.message);
                    io.emit(HarnessEvent::Error(e.message));
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

    reject_queued(&mut commands, &io.events).await;
    let code = match child.wait().await {
        Ok(status) => status.code(),
        Err(e) => {
            harness_log(&format!("wait for claude {pid}: {e}"));
            None
        }
    };
    harness_log(&format!("claude {pid} exited with {code:?}"));
    process_id.store(0, std::sync::atomic::Ordering::Relaxed);
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
    snapshots.finish().await;
    if close_token.is_cancelled() {
        snapshots.close().await;
    }
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
    #[test]
    fn terminal_chrome_option_follows_the_switch() {
        assert_eq!(configure_terminal_chrome("claude", true), "claude --chrome");
        assert_eq!(
            configure_terminal_chrome("/usr/local/bin/claude --model m", false),
            "/usr/local/bin/claude --no-chrome --model m"
        );
        // An explicit choice is kept; another harness is untouched.
        assert_eq!(
            configure_terminal_chrome("claude --no-chrome", true),
            "claude --no-chrome"
        );
        assert_eq!(configure_terminal_chrome("codex", true), "codex");
    }

    use super::*;
    #[test]
    fn remote_guidelines_use_a_temporary_prompt_file() {
        let text = "Guidelines supplied by the desktop.";
        let dir = tempfile::tempdir().unwrap();
        let file = RemoteGuidelinesFile::create(dir.path(), text).unwrap();
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), text);
        let config = ClaudeSessionConfig {
            cwd: PathBuf::from("/tmp"),
            snapshot_facts_dir: None,
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: None,
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        };
        let args = config.args_with_guidelines_file(Some(file.path()));
        let at = args
            .iter()
            .position(|arg| arg == "--append-system-prompt-file")
            .unwrap();
        assert_eq!(args[at + 1], file.path().to_string_lossy());
    }

    #[test]
    fn standard_argv_is_unchanged_and_preview_is_exactly_fenced() {
        let mut config = ClaudeSessionConfig {
            cwd: PathBuf::from("/clone"),
            snapshot_facts_dir: None,
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: None,
            wire_log_dir: None,
            mcp_servers: vec![],
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        };
        let standard = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--include-partial-messages",
            "--no-chrome",
            "--permission-mode",
            "default",
        ];
        assert_eq!(LaunchFlags::default().args(&config).unwrap(), standard);
        let preview: Vec<_> = standard
            .into_iter()
            .chain(["--restricted", "--strict-mcp-config", "--safe-mode"])
            .collect();
        assert_eq!(LaunchFlags::FreshPreview.args(&config).unwrap(), preview);
        config.model = Some("model".into());
        config.effort = Some("high".into());
        config.resume = Some("existing-id".into());
        config.chrome = true;
        assert_eq!(LaunchFlags::Standard.args(&config).unwrap(), config.args());
        assert!(LaunchFlags::FreshPreview.args(&config).is_err());
        config.model = None;
        config.effort = None;
        config.chrome = false;
        assert!(
            LaunchFlags::FreshPreview.args(&config).is_err(),
            "resume alone refused"
        );
        config.resume = None;
        config.permission_mode = "bypassPermissions".into();
        assert!(LaunchFlags::FreshPreview.args(&config).is_err());
    }

    fn preview_config() -> ClaudeSessionConfig {
        ClaudeSessionConfig {
            cwd: PathBuf::from("/clone"),
            snapshot_facts_dir: None,
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: None,
            wire_log_dir: None,
            mcp_servers: vec![],
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        }
    }

    const FIXTURE_URL: &str = "http://127.0.0.1:1/mcp?caller=fixture";

    /// Shaped like the real descriptors: the caller URL is public and is in
    /// the config literally, the token only as `${VAR}`.
    fn task_server(token: &str) -> ClaudeMcpServer {
        ClaudeMcpServer {
            config: json!({"mcpServers": {"tasks": {
                "type": "http",
                "url": FIXTURE_URL,
                "headers": {"Authorization": "Bearer ${FIXTURE_TASK_TOKEN}"},
            }}}),
            allowed_tools: vec!["mcp__tasks__task_get".into()],
            env: vec![
                ("FIXTURE_TASK_URL".into(), FIXTURE_URL.into()),
                ("FIXTURE_TASK_TOKEN".into(), token.into()),
            ],
            public_env: vec!["FIXTURE_TASK_URL".into()],
        }
    }

    #[test]
    fn preview_tools_allows_only_mcp_servers() {
        let mut config = preview_config();
        let fresh = LaunchFlags::FreshPreview.args(&config).unwrap();
        assert_eq!(LaunchFlags::PreviewTools.args(&config).unwrap(), fresh);
        config.mcp_servers = vec![task_server("synthetic-token")];
        let server = &config.mcp_servers[0];
        let mut expected = fresh.clone();
        let fence = expected.len() - 3;
        expected.splice(
            fence..fence,
            [
                format!("--mcp-config={}", server.config),
                "--allowedTools=mcp__tasks__task_get".into(),
            ],
        );
        let args = LaunchFlags::PreviewTools.args(&config).unwrap();
        assert_eq!(args, expected);
        assert!(args
            .ends_with(&["--restricted", "--strict-mcp-config", "--safe-mode"].map(String::from)));
        let refused: [fn(&mut ClaudeSessionConfig); 9] = [
            |c| c.resume = Some("existing-id".into()),
            |c| c.chrome = true,
            |c| c.snapshot_facts_dir = Some("/facts".into()),
            |c| c.wire_log_dir = Some("/wire".into()),
            |c| c.guidelines_file = Some("/guidelines.md".into()),
            |c| c.guidelines_content = Some("text".into()),
            |c| c.model = Some("model".into()),
            |c| c.effort = Some("high".into()),
            |c| c.permission_mode = "bypassPermissions".into(),
        ];
        for (index, change) in refused.into_iter().enumerate() {
            let mut changed = config.clone();
            change(&mut changed);
            assert!(
                LaunchFlags::PreviewTools.args(&changed).is_err(),
                "restriction {index} must stay"
            );
        }
    }

    #[test]
    fn preview_tools_keeps_tokens_out_of_argv() {
        let token = "SYNTHETIC_TOKEN_MUST_NOT_APPEAR";
        let mut config = preview_config();
        config.mcp_servers = vec![task_server(token)];
        let args = LaunchFlags::PreviewTools.args(&config).unwrap();
        assert!(args.iter().all(|arg| !arg.contains(token)));
        assert!(args
            .iter()
            .any(|arg| arg.contains("Bearer ${FIXTURE_TASK_TOKEN}")));
        // A literal token, raw or JSON-escaped, in config or allowed tools is refused
        // without echoing it.
        for literal in [token, "tok\"en\\with-escapes"] {
            let mut server = task_server(literal);
            server.config["mcpServers"]["tasks"]["headers"]["Authorization"] =
                json!(format!("Bearer {literal}"));
            config.mcp_servers = vec![server];
            let error = LaunchFlags::PreviewTools.args(&config).unwrap_err();
            assert!(error.contains("${FIXTURE_TASK_TOKEN}"), "{error}");
            assert!(!error.contains(literal), "token leaked into the error");
        }
        let mut server = task_server(token);
        server.allowed_tools.push(format!("mcp__{token}"));
        config.mcp_servers = vec![server];
        assert!(LaunchFlags::PreviewTools.args(&config).is_err());
        config.mcp_servers = vec![task_server("")];
        assert!(LaunchFlags::PreviewTools.args(&config).is_err());

        // A token leaked into another server's config is refused too.
        let mut other = task_server("other-token");
        other.config["mcpServers"]["tasks"]["headers"]["Authorization"] =
            json!(format!("Bearer {token}"));
        config.mcp_servers = vec![task_server(token), other];
        let error = LaunchFlags::PreviewTools.args(&config).unwrap_err();
        assert!(!error.contains(token), "token leaked into the error");
    }

    #[test]
    fn preview_tools_lets_only_public_env_appear_in_argv() {
        let mut config = preview_config();
        config.mcp_servers = vec![task_server("synthetic-token")];
        let args = LaunchFlags::PreviewTools.args(&config).unwrap();
        assert!(args.iter().any(|arg| arg.contains(FIXTURE_URL)));
        // Unmarked, the URL counts as a credential: fail closed.
        config.mcp_servers[0].public_env.clear();
        let error = LaunchFlags::PreviewTools.args(&config).unwrap_err();
        assert!(error.contains("FIXTURE_TASK_URL"), "{error}");
        // A public key must name an env entry.
        config.mcp_servers[0].public_env = vec!["FIXTURE_TASK_URL".into(), "MISSING".into()];
        let error = LaunchFlags::PreviewTools.args(&config).unwrap_err();
        assert!(error.contains("MISSING"), "{error}");
    }

    #[test]
    fn fresh_preview_still_rejects_servers() {
        let mut config = preview_config();
        config.mcp_servers = vec![task_server("synthetic-token")];
        assert!(LaunchFlags::FreshPreview.args(&config).is_err());
        config.mcp_servers[0].allowed_tools.clear();
        assert!(LaunchFlags::FreshPreview.args(&config).is_err());
        assert!(LaunchFlags::PreviewTools.args(&config).is_ok());
    }

    #[test]
    fn no_hooks_without_context_or_snapshots() {
        assert!(session_hooks(false, false).as_object().unwrap().is_empty());
    }
}

#[cfg(test)]
#[path = "claude_answer_tests.rs"]
mod answer_tests;
