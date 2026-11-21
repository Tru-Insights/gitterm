//! Native Claude Code session: one long-lived `claude` process driven over
//! the stream-json control protocol in both directions.
//!
//! Wire contract: `.plans/tru-140-claude-control-protocol.md` (Phase A).
//! `ClaudeFrameParser` turns CLI stdout lines into `HarnessEvent`s and is
//! pure (no IO), so it is unit-tested against recorded frames in
//! `tests/fixtures/claude/`. `ClaudeSession` owns the process on a
//! dedicated thread and maps `HarnessCommand`s onto stdin frames.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{HarnessCommand, HarnessEvent};
#[cfg(test)]
use super::{RuntimeDecision, RuntimeRequestKind, UserPrompt};
use crate::agentd::client::{
    ClaudeChatFrame, ClaudeChatOpen, ClaudeChatOpened, RemoteAgentBackend, RemoteAgentClientConfig,
};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub use gitterm_core::harness::claude_parser::{
    permission_result, user_message_frame, ClaudeFrameParser, HostRequest, ParsedFrame,
    SnapshotHook,
};

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

use gitterm_core::chat::runtime::Session;
pub use gitterm_core::harness::claude::{chrome_flag, ClaudeMcpServer, ClaudeSessionConfig};
use gitterm_core::harness::claude::{Launch, SnapshotFuture, SnapshotRecorder};

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
    inner: Session,
}

impl ClaudeSession {
    /// Wrap a host-owned transport port without spawning a local process.
    pub fn from_port(commands: mpsc::UnboundedSender<HarnessCommand>) -> Self {
        Self {
            inner: Session::from_port(commands, false),
        }
    }

    pub fn spawn(config: ClaudeSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (inner, events) = Session::spawn(move |token| {
            let program = resolve_claude_program()?;
            Ok(local_launch(config, program, token))
        });
        (Self { inner }, events)
    }
    /// Iced consumes delivery receipts and harness events in the same order.
    pub fn spawn_runtime(
        config: ClaudeSessionConfig,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
    ) {
        let (inner, events) = Session::spawn_runtime(move |token| {
            let program = resolve_claude_program()?;
            Ok(local_launch(config, program, token))
        });
        (Self { inner }, events)
    }
    /// Returns whether actual write receipts will follow channel acceptance.
    pub fn send_chat(
        &self,
        command: HarnessCommand,
        correlation: gitterm_core::chat::types::CommandCorrelation,
    ) -> Result<bool, String> {
        if self.inner.supports_correlation() {
            self.inner
                .send_correlated(command, correlation)
                .map(|()| true)
        } else {
            self.inner.send(command).map(|()| false)
        }
    }
    pub fn close_turn_snapshots(&self) {
        self.inner.close_turn_snapshots();
    }
    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.inner.send(command)
    }
    pub fn local_pid(&self) -> Option<u32> {
        self.inner.local_pid()
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
        (Self::from_port(cmd_tx), ev_rx)
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

fn trim_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

// Keep discovery on the existing session thread. The core receives the same
// rooted Recorder and shares its close token, including its legacy invalid-root
// disabling behavior and private thread-local roots in tests.
fn local_launch(
    config: ClaudeSessionConfig,
    program: PathBuf,
    token: tokio_util::sync::CancellationToken,
) -> Launch {
    let snapshots = crate::turn_snapshots::Recorder::new(
        config.cwd.clone(),
        config.resume.clone(),
        config.snapshot_facts_dir.clone(),
    )
    .with_close_token(token);
    let environment = child_environment(std::env::vars_os(), child_path(), &config.mcp_servers);
    Launch {
        flags: Default::default(),
        config,
        program,
        environment,
        temporary_dir: std::env::temp_dir(),
        snapshots: Box::new(snapshots),
    }
}

fn child_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    path: OsString,
    servers: &[ClaudeMcpServer],
) -> Vec<(OsString, OsString)> {
    // Command preserves platform environment-key semantics (case-insensitive
    // on Windows). It is only used to build the explicit map, never executed.
    let inherited: Vec<_> = inherited.into_iter().collect();
    let mut command = std::process::Command::new("claude");
    command
        .env_clear()
        .envs(inherited.iter().cloned())
        .env("PATH", path);
    for (key, _) in inherited {
        let key = key.to_string_lossy();
        if key == "CLAUDECODE" || key.starts_with("CLAUDE_CODE_") || key == "CLAUDE_PID" {
            command.env_remove(key.as_ref());
        }
    }
    command
        .env_remove("NODE_OPTIONS")
        .env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")
        .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1");
    for server in servers {
        command.envs(server.env.iter().map(|(key, value)| (key, value)));
    }
    command
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect()
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

#[cfg(test)]
async fn run_session_program(
    config: ClaudeSessionConfig,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    token: tokio_util::sync::CancellationToken,
    program: PathBuf,
    pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    let root = config.cwd.clone();
    let snapshots = crate::turn_snapshots::Recorder::new(
        root.clone(),
        config.resume.clone(),
        config.snapshot_facts_dir.clone(),
    )
    .with_close_token(token.clone());
    let launch = Launch {
        flags: Default::default(),
        config,
        program,
        environment: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), root.join("home").into_os_string()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.join("provider").into_os_string(),
            ),
        ],
        temporary_dir: root,
        snapshots: Box::new(snapshots),
    };
    gitterm_core::harness::claude::run_session_program(launch, commands, events, token, pid).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_child_environment_preserves_legacy_sanitization_and_mcp_order() {
        let inherited = [
            ("HOME", "/synthetic/home"),
            ("CLAUDE_CONFIG_DIR", "/synthetic/provider"),
            ("PATH", "/old/path"),
            ("CLAUDECODE", "parent"),
            ("CLAUDE_CODE_SESSION_ID", "parent"),
            ("CLAUDE_PID", "123"),
            ("NODE_OPTIONS", "parent"),
            ("CLAUDE_CODE_ENTRYPOINT", "parent"),
            ("UNRELATED", "preserved"),
        ]
        .map(|(key, value)| (key.into(), value.into()));
        let servers = ["first", "last"].map(|value| ClaudeMcpServer {
            config: json!({"mcpServers": {}}),
            allowed_tools: Vec::new(),
            env: vec![("SYNTHETIC_TOKEN".into(), value.into())],
            public_env: Vec::new(),
        });
        let env: std::collections::BTreeMap<_, _> =
            child_environment(inherited, "/child/path".into(), &servers)
                .into_iter()
                .collect();
        let expected = [
            ("HOME", "/synthetic/home"),
            ("CLAUDE_CONFIG_DIR", "/synthetic/provider"),
            ("PATH", "/child/path"),
            ("UNRELATED", "preserved"),
            ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
            ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1"),
            ("SYNTHETIC_TOKEN", "last"),
        ]
        .map(|(key, value)| (key.into(), value.into()))
        .into_iter()
        .collect();
        assert_eq!(env, expected);
    }

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
            public_env: Vec::new(),
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
#[allow(clippy::duplicate_mod)] // Shared characterization sources are loaded by multiple focused test modules.
#[path = "../../tests/native-chat/native_chat_claude.rs"]
mod native_chat_tests;

#[cfg(all(test, unix))]
#[allow(clippy::duplicate_mod)] // Reuses the process fixture from native chat characterization tests.
#[path = "../../tests/claude-process/characterization.rs"]
mod process_characterization;
