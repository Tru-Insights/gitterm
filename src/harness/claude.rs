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

use super::{HarnessCommand, HarnessEvent, TurnStatus};
#[cfg(test)]
use super::{RuntimeDecision, RuntimeRequestKind, UserPrompt};
use crate::agentd::client::{
    ClaudeChatFrame, ClaudeChatOpen, ClaudeChatOpened, RemoteAgentBackend, RemoteAgentClientConfig,
};

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

use gitterm_core::harness::claude_parser::{
    control_request_frame, control_response_frame, ControlResponseBody,
};
pub use gitterm_core::harness::claude_parser::{
    permission_result, user_message_frame, ClaudeFrameParser, HostRequest, ParsedFrame,
    SnapshotHook,
};

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

struct RemoteGuidelinesFile(PathBuf);

impl RemoteGuidelinesFile {
    fn create(text: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
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
    snapshots_closed: tokio_util::sync::CancellationToken,
    commands: mpsc::UnboundedSender<HarnessCommand>,
    shutdown_on_drop: bool,
    /// The local `claude` process id while it runs; 0 before the spawn,
    /// after it exits, and always for a remote chat.
    pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl ClaudeSession {
    /// Start `claude` on a dedicated thread. Spawn failures arrive on the
    /// event channel as `Error` followed by `ProcessExited`.
    pub fn spawn(config: ClaudeSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let snapshots_closed = tokio_util::sync::CancellationToken::new();
        let close_token = snapshots_closed.clone();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let pid = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let session_pid = pid.clone();
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
                rt.block_on(run_session(config, cmd_rx, ev_tx, close_token, session_pid));
            });
        if let Err(e) = spawn_result {
            eprintln!("[claude-harness] could not spawn session thread: {e}");
        }
        (
            Self {
                commands: cmd_tx,
                shutdown_on_drop: true,
                snapshots_closed,
                pid,
            },
            ev_rx,
        )
    }

    pub fn close_turn_snapshots(&self) {
        self.snapshots_closed.cancel();
    }

    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "the Claude session has exited".to_string())
    }

    /// The local `claude` process id while it runs (TRU-212: the root of
    /// the `codex exec` runs the chat starts). `None` before it has
    /// spawned, once it has exited, and for a remote chat.
    pub fn local_pid(&self) -> Option<u32> {
        match self.pid.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            pid => Some(pid),
        }
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
    /// Guideline text read by the desktop from its shared source file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidelines: Option<String>,
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
        let guidelines = config
            .guidelines_file
            .as_ref()
            .map(|path| {
                std::fs::read_to_string(path).map_err(|error| {
                    format!(
                        "read guidelines {} for remote chat: {error}",
                        path.display()
                    )
                })
            })
            .transpose()?;
        Ok(Self {
            cwd: config.cwd.to_string_lossy().into_owned(),
            model: config.model.clone(),
            permission_mode: config.permission_mode.clone(),
            effort: config.effort.clone(),
            resume: config.resume.clone(),
            chrome: config.chrome,
            guidelines,
        })
    }

    /// The agent side's session config, run in `cwd` (the validated,
    /// canonical form of `self.cwd`).
    pub fn into_config(self, cwd: PathBuf) -> ClaudeSessionConfig {
        ClaudeSessionConfig {
            snapshot_facts_dir: None,
            cwd,
            model: self.model,
            permission_mode: self.permission_mode,
            effort: self.effort,
            resume: self.resume,
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: self.chrome,
            guidelines_file: None,
            guidelines_content: self.guidelines,
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
                snapshots_closed: tokio_util::sync::CancellationToken::new(),
                pid: Default::default(),
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
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    close_token: tokio_util::sync::CancellationToken,
    process_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    match resolve_claude_program() {
        Ok(program) => {
            run_session_program(config, commands, events, close_token, program, process_id).await
        }
        Err(msg) => {
            harness_log(&msg);
            let _ = events.send(HarnessEvent::Error(msg));
            let _ = events.send(HarnessEvent::ProcessExited { code: None });
        }
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

async fn run_session_program(
    config: ClaudeSessionConfig,
    mut commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    close_token: tokio_util::sync::CancellationToken,
    program: PathBuf,
    process_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    let fail = |msg: String| {
        harness_log(&msg);
        let _ = events.send(HarnessEvent::Error(msg));
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
    };
    let mut snapshots = crate::turn_snapshots::Recorder::new(
        config.cwd.clone(),
        config.resume.clone(),
        config.snapshot_facts_dir.clone(),
    )
    .with_close_token(close_token.clone());
    let remote_guidelines = match config.guidelines_content.as_deref() {
        Some(text) => match RemoteGuidelinesFile::create(text) {
            Ok(file) => Some(file),
            Err(error) => return fail(error),
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
    let args = config.args_with_guidelines_file(guidelines_file);
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
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn session_start_reinjects_workspace_context_after_clear_compact_and_move() {
        check_session_start_workspace_context(true, false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn never_moved_workspace_prompt_and_session_start_match_v6() {
        for resume in [false, true] {
            check_session_start_workspace_context(false, resume).await;
        }
    }

    #[cfg(unix)]
    async fn check_session_start_workspace_context(move_chat: bool, resume: bool) {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("notes/example-work");
        std::fs::create_dir_all(home.join("docs")).unwrap();
        git2::Repository::init(home.parent().unwrap()).unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        std::fs::write(
            home.join("docs/checkpoint.md"),
            "INITIAL_CHECKPOINT_CONTENT_TRU259",
        )
        .unwrap();
        let places = crate::guidelines::document_places(
            true,
            true,
            &home,
            None,
            "Example workspace",
            &[PathBuf::from("/code/example-api")],
        )
        .unwrap();
        let launch = crate::guidelines::launch_instructions(
            Some(crate::guidelines::Guidelines {
                path: fixture.path().join("guidelines.md"),
                text: "Static change guidelines.".into(),
            }),
            Some(&places),
            true,
            fixture.path(),
        )
        .unwrap();
        // Frozen from origin/v6 (936976d), with TRU-259/265's intentional
        // wording changes. Never-moved chats retain this launch prompt;
        // callbacks add only the fresh checkpoint read instruction.
        let expected = include_str!("../../tests/fixtures/claude/workspace-prompt-v6.md")
            .replace("@HOME@", home.to_str().unwrap());
        assert_eq!(launch.text, expected);
        std::fs::write(home.join("expected-system.md"), expected).unwrap();
        if move_chat {
            std::fs::write(home.join("move-chat"), "").unwrap();
        }
        if resume {
            std::fs::write(home.join("resuming"), "").unwrap();
        }
        let program = fixture.path().join("mock-claude");
        std::fs::write(&program, r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
system_file = Path(sys.argv[sys.argv.index('--append-system-prompt-file') + 1])
system = system_file.read_bytes()
assert system == Path('expected-system.md').read_bytes()
assert ('--resume=example-session' in sys.argv) == Path('resuming').exists()
expected_context = system.decode().split('# Documents for this work (added by GitTerm)\n', 1)[1]
expected_context = '# Documents for this work (added by GitTerm)\n' + expected_context.split('# Starting Codex in GitTerm', 1)[0].rstrip()
expected_context += f'\nRe-read the workspace checkpoint `{Path.cwd() / "docs/checkpoint.md"}` now, before continuing: it may have changed since this chat started.'
def emit(value):
    print(json.dumps(value), flush=True)
def read():
    line = sys.stdin.readline()
    assert line, 'host closed before callback response'
    return json.loads(line)
init = read()
assert init['request']['hooks']['SessionStart'][0]['hookCallbackIds'] == ['gitterm_workspace_context']
assert 'UserPromptSubmit' not in init['request']['hooks'] # independent of snapshots
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{'hooks_applied':True}}})
# Real Claude 2.1.322 sends these callbacks only for clear/compact.
for source in ['clear', 'compact']:
    emit({'type':'control_request','request_id':source,'request':{'subtype':'hook_callback','callback_id':'gitterm_workspace_context','input':{'hook_event_name':'SessionStart','source':source,'session_id':'example-session'}}})
    response = read()['response']
    assert response['request_id'] == source
    output = response['response']['hookSpecificOutput']
    assert output['hookEventName'] == 'SessionStart'
    text = output['additionalContext']
    assert 'Workspace: `Example workspace`' in text
    assert '/code/example-api' in text
    assert str(Path.cwd() / 'docs/checkpoint.md') in text
    assert "resume only this workspace's work; if unsure which work to continue, ask." in text
    assert text.count('Re-read the workspace checkpoint') == 1
    assert 'The start of the workspace checkpoint' not in text
    assert 'INITIAL_CHECKPOINT_CONTENT_TRU259' not in text
    assert 'Starting Codex' not in text
    assert text == expected_context
emit({'type':'result','subtype':'success'})
if Path('move-chat').exists():
    read() # the host moves this running chat, then sends /clear
    for source in ['clear', 'compact']:
        emit({'type':'control_request','request_id':'moved-' + source,'request':{'subtype':'hook_callback','callback_id':'gitterm_workspace_context','input':{'hook_event_name':'SessionStart','source':source,'session_id':'example-session'}}})
        text = read()['response']['response']['hookSpecificOutput']['additionalContext']
        assert 'Workspace: `Destination workspace`' in text
        assert str(Path.cwd().parent / 'destination/docs/checkpoint.md') in text
        assert 'Destination checkpoint written after move' not in text
        assert 'docs/checkpoint.md` (exists)' in text
        assert text.count('Re-read the workspace checkpoint') == 1
        assert f'Re-read the workspace checkpoint `{Path.cwd().parent / "destination/docs/checkpoint.md"}` now' in text
        assert 'This chat now belongs to the following workspace' in text
        assert 'replaces the workspace section in the system prompt' in text
        assert 'Example workspace' not in text and 'Example work' not in text
        assert system_file.read_bytes() == system
    emit({'type':'result','subtype':'success'})
    read() # moving back to the launch workspace removes the override
    emit({'type':'control_request','request_id':'returned-clear','request':{'subtype':'hook_callback','callback_id':'gitterm_workspace_context','input':{'hook_event_name':'SessionStart','source':'clear','session_id':'example-session'}}})
    assert read()['response']['response']['hookSpecificOutput']['additionalContext'] == expected_context
    emit({'type':'result','subtype':'success'})
for line in sys.stdin:
    pass
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_session_program(
            ClaudeSessionConfig {
                cwd: home.clone(),
                snapshot_facts_dir: None,
                model: None,
                permission_mode: "default".into(),
                effort: None,
                resume: resume.then(|| "example-session".into()),
                wire_log_dir: None,
                mcp_servers: Vec::new(),
                chrome: false,
                guidelines_file: Some(launch.path),
                guidelines_content: None,
            },
            rx,
            tx,
            tokio_util::sync::CancellationToken::new(),
            program,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        let destination = home.parent().unwrap().join("destination");
        std::fs::create_dir_all(destination.join("docs")).unwrap();
        let places = crate::guidelines::document_places(
            true,
            true,
            &destination,
            None,
            "Destination workspace",
            &[],
        )
        .unwrap();
        let mut completed = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                HarnessEvent::TurnCompleted { .. } if move_chat && completed == 0 => {
                    commands
                        .send(HarnessCommand::SetWorkspaceContext(Some(
                            crate::guidelines::documents_note(&places),
                        )))
                        .unwrap();
                    std::fs::write(
                        destination.join("docs/checkpoint.md"),
                        "Destination checkpoint written after move",
                    )
                    .unwrap();
                    commands
                        .send(HarnessCommand::SendUserMessage(UserPrompt::from("/clear")))
                        .unwrap();
                    completed += 1;
                }
                HarnessEvent::TurnCompleted { .. } if move_chat && completed == 1 => {
                    commands
                        .send(HarnessCommand::SetWorkspaceContext(Some(
                            launch.text.clone(),
                        )))
                        .unwrap();
                    commands
                        .send(HarnessCommand::SendUserMessage(UserPrompt::from("/clear")))
                        .unwrap();
                    completed += 1;
                }
                HarnessEvent::TurnCompleted { .. } => break,
                HarnessEvent::Error(error) => panic!("mock SessionStart callback: {error}"),
                HarnessEvent::ProcessExited { code } => {
                    panic!("callback mock exited early: {code:?}")
                }
                _ => {}
            }
        }
        commands.send(HarnessCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(session_hooks(false, false).as_object().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_owned_queued_turn_waits_for_snapshot_boundary() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        let facts = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-qb", "main"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(crate::agentd::git::git_command()
                .current_dir(repo.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let program = facts.path().join("claude-mock");
        std::fs::write(&program, r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
def emit(x):
    print(json.dumps(x), flush=True)
def read():
    return json.loads(sys.stdin.readline())
init = read()
hooks = init['request']['hooks']
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{'hooks_applied':True}}})
def boundary(event='UserPromptSubmit'):
    matchers = hooks.get(event, [])
    if matchers:
        callback = matchers[0]['hookCallbackIds'][0]
        emit({'type':'control_request','request_id':'boundary','request':{'subtype':'hook_callback','callback_id':callback,'input':{'hook_event_name':event,'session_id':'queued-chat'}}})
        reply = read()
        assert reply['response']['request_id'] == 'boundary'
read() # first user message
boundary()
emit({'type':'system','subtype':'init','session_id':'queued-chat','model':'fixture'})
read() # second user message queued in the CLI, not the host
boundary() # prompt hooks may run at submission, while turn one is active
boundary('PreToolUse')
Path('file.txt').write_text('one')
boundary('Stop')
emit({'type':'result','subtype':'success'})
# No sleeping: only the callback response can prevent racing capture.
boundary('PreToolUse')
Path('file.txt').write_text('two')
boundary('Stop')
emit({'type':'result','subtype':'success'})
for line in sys.stdin:
    pass
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_session_program(
            ClaudeSessionConfig {
                cwd: repo.path().into(),
                snapshot_facts_dir: Some(facts.path().into()),
                model: None,
                permission_mode: "default".into(),
                effort: None,
                resume: None,
                wire_log_dir: None,
                mcp_servers: Vec::new(),
                chrome: false,
                guidelines_file: None,
                guidelines_content: None,
            },
            rx,
            tx,
            tokio_util::sync::CancellationToken::new(),
            program,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        let mut completed = 0;
        while completed < 2 {
            let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                HarnessEvent::Ready { .. } => commands
                    .send(HarnessCommand::SendUserMessage(UserPrompt::from("one")))
                    .unwrap(),
                HarnessEvent::TurnStarted { .. } => commands
                    .send(HarnessCommand::SendUserMessage(UserPrompt::from("two")))
                    .unwrap(),
                HarnessEvent::TurnCompleted { .. } => completed += 1,
                HarnessEvent::Error(e) => panic!("mock session: {e}"),
                _ => {}
            }
        }
        commands.send(HarnessCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let turns = crate::turn_snapshots::turns(repo.path(), "queued-chat")
            .await
            .unwrap();
        assert_eq!(
            turns.len(),
            2,
            "each CLI-owned turn needs its own snapshots"
        );
        let store = crate::turn_snapshots::store_path(repo.path())
            .await
            .unwrap();
        let show = |oid: &str| {
            let output = crate::agentd::git::git_command()
                .current_dir(&store)
                .args(["show", &format!("{oid}:file.txt")])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(show(turns[1].end.as_deref().unwrap()), "one");
        assert_eq!(show(turns[0].start.as_deref().unwrap()), "one");
        assert_eq!(show(turns[0].end.as_deref().unwrap()), "two");
    }

    #[test]
    fn args_follow_the_phase_a_contract() {
        let cfg = ClaudeSessionConfig {
            snapshot_facts_dir: None,
            cwd: PathBuf::from("/tmp"),
            model: Some("haiku".into()),
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: true,
            guidelines_file: None,
            guidelines_content: None,
        };
        let args = cfg.args().join(" ");
        assert!(cfg.args().contains(&"--chrome".to_string()));
        assert!(!cfg.args().contains(&"--no-chrome".to_string()));
        let off = ClaudeSessionConfig {
            snapshot_facts_dir: None,
            chrome: false,
            ..cfg.clone()
        }
        .args();
        assert!(off.contains(&"--no-chrome".to_string()));
        assert!(!off.contains(&"--chrome".to_string()));
        assert!(args.contains("--permission-prompt-tool stdio"));
        assert!(args.contains("--input-format stream-json"));
        assert!(args.contains("--include-partial-messages"));
        assert!(args.contains("--model haiku"));
        assert!(args.contains("--permission-mode default"));
        assert!(args.ends_with("--resume=sid"));
        assert!(!args.contains("--print"));
        assert!(!args.contains("--setting-sources"));
    }

    #[test]
    fn a_resumed_or_remote_chat_carries_the_chrome_choice() {
        // A resume (a chat reopened from the Chats panel or a restart).
        let resumed = config_with(Vec::new());
        assert!(resumed.args().contains(&"--no-chrome".to_string()));
        assert!(resumed.args().contains(&"--resume=sid".to_string()));
        assert!(ClaudeSessionConfig {
            snapshot_facts_dir: None,
            chrome: true,
            ..resumed.clone()
        }
        .args()
        .contains(&"--chrome".to_string()));
        // A remote chat: the agent side starts with the same option.
        let start = RemoteClaudeChatStart::from_config(&resumed).unwrap();
        let agent = start.into_config(PathBuf::from("/tmp"));
        assert!(agent.args().contains(&"--no-chrome".to_string()));
        // An agent started by a build before TRU-181 sent `chrome: false`.
        let older: RemoteClaudeChatStart = serde_json::from_value(json!({
            "cwd": "/tmp", "model": null, "permission_mode": "default",
            "effort": null, "resume": null, "chrome": false
        }))
        .unwrap();
        let older_config = older.into_config(PathBuf::from("/tmp"));
        assert!(older_config.args().contains(&"--no-chrome".to_string()));
        assert!(older_config.guidelines_content.is_none());
    }

    #[test]
    fn terminal_claude_launches_get_the_chrome_option() {
        assert_eq!(
            configure_terminal_chrome("claude", false),
            "claude --no-chrome"
        );
        assert_eq!(configure_terminal_chrome("claude", true), "claude --chrome");
        // Resumes and worker launches with a brief.
        assert_eq!(
            configure_terminal_chrome("claude --resume abc", false),
            "claude --no-chrome --resume abc"
        );
        assert_eq!(
            configure_terminal_chrome("claude --session-id abc \"$(cat '/b.md')\"", false),
            "claude --no-chrome --session-id abc \"$(cat '/b.md')\""
        );
        assert_eq!(
            configure_terminal_chrome("/opt/bin/claude --model opus", true),
            "/opt/bin/claude --chrome --model opus"
        );
        assert_eq!(
            configure_terminal_chrome("'/opt/bin/claude' --resume abc", false),
            "'/opt/bin/claude' --no-chrome --resume abc"
        );
        // A command that already chooses keeps its choice.
        assert_eq!(
            configure_terminal_chrome("claude --chrome --resume abc", false),
            "claude --chrome --resume abc"
        );
        assert_eq!(
            configure_terminal_chrome("claude --no-chrome", true),
            "claude --no-chrome"
        );
        // Other harnesses and commands are untouched.
        for command in ["codex resume --last", "pi --model x", "npm run dev", ""] {
            assert_eq!(configure_terminal_chrome(command, false), command);
        }
        assert_eq!(
            configure_terminal_chrome("claude-helper --x", false),
            "claude-helper --x"
        );
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
            snapshot_facts_dir: None,
            cwd: PathBuf::from("/tmp"),
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers,
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
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
    fn remote_start_carries_guideline_text_and_old_payloads_still_decode() {
        let dir = tempfile::tempdir().unwrap();
        let guidelines_path = dir.path().join("agent-guidelines.md");
        std::fs::write(&guidelines_path, "Shared guideline text.\n").unwrap();
        let mut config = config_with(Vec::new());
        config.guidelines_file = Some(guidelines_path);
        let start = RemoteClaudeChatStart::from_config(&config).unwrap();
        assert_eq!(
            start.guidelines.as_deref(),
            Some("Shared guideline text.\n")
        );
        let payload = serde_json::to_value(&start).unwrap();
        assert_eq!(payload["guidelines"], "Shared guideline text.\n");
        #[derive(Deserialize)]
        struct OlderAgentStart {
            cwd: String,
            model: Option<String>,
            permission_mode: String,
            effort: Option<String>,
            resume: Option<String>,
            chrome: bool,
        }
        let older_agent: OlderAgentStart = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(older_agent.cwd, "/tmp");
        assert_eq!(older_agent.permission_mode, "default");
        assert!(older_agent.model.is_none());
        assert!(older_agent.effort.is_none());
        assert_eq!(older_agent.resume.as_deref(), Some("sid"));
        assert!(!older_agent.chrome);
        let back: RemoteClaudeChatStart = serde_json::from_value(payload).unwrap();
        let remote = back.into_config(PathBuf::from("/tmp"));
        assert_eq!(
            remote.guidelines_content.as_deref(),
            Some("Shared guideline text.\n")
        );

        let older: RemoteClaudeChatStart = serde_json::from_value(json!({
            "cwd": "/tmp", "model": null, "permission_mode": "default",
            "effort": null, "resume": null, "chrome": false
        }))
        .unwrap();
        assert!(older.guidelines.is_none());
        assert!(older
            .into_config(PathBuf::from("/tmp"))
            .guidelines_content
            .is_none());
    }

    #[test]
    fn remote_guidelines_use_a_temporary_prompt_file() {
        let text = "Guidelines supplied by the desktop.";
        let file = RemoteGuidelinesFile::create(text).unwrap();
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
            commands: Vec::new(),
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

#[cfg(test)]
#[path = "../../tests/native-chat/native_chat_claude.rs"]
mod native_chat_tests;
