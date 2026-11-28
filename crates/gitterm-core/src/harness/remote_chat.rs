//! Remote Claude chat launch and supervision, independent of a frontend.
//! Persistent agents own their processes: explicit Shutdown stops a chat;
//! dropping or shutting down the local Session only detaches. Legacy agents
//! retain their stream-lifetime behavior.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::chat::runtime::Session;
use crate::chat::types::{HarnessCommand, HarnessEvent};
use crate::harness::claude::ClaudeSessionConfig;
use crate::remote::client::{
    ClaudeChatEventStream, ClaudeChatFrame, ClaudeChatFrameStream, ClaudeChatOpen,
    ClaudeChatOpened, RemoteAgentBackend, RemoteAgentClientConfig, RemoteAgentClientError,
    RemoteAgentHandshake,
};

/// Agents advertising this capability keep chats alive across connections.
pub const CLAUDE_CHAT_PERSISTENT_CAPABILITY: &str = "claude_chat_persistent";

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
    backend: impl RemoteChatTransport,
    config: ClaudeSessionConfig,
) -> (Session, mpsc::UnboundedReceiver<HarnessEvent>) {
    remote(backend, config, None)
}

/// Attach to chat `chat_id`, which `ListClaudeChats` reported running
/// on an agent that owns its chats, without replay (the tab reads the
/// transcript). Never starts a process: a chat that has stopped since
/// arrives as `Error` then `ProcessExited { code: None }`, and the next
/// message starts one as usual. `config` is the tab's own, for the
/// start that follows such an exit. Otherwise as `spawn_remote`.
pub fn attach_remote(
    backend: impl RemoteChatTransport,
    config: ClaudeSessionConfig,
    chat_id: String,
) -> (Session, mpsc::UnboundedReceiver<HarnessEvent>) {
    remote(backend, config, Some(chat_id))
}

fn remote(
    backend: impl RemoteChatTransport,
    config: ClaudeSessionConfig,
    chat_id: Option<String>,
) -> (Session, mpsc::UnboundedReceiver<HarnessEvent>) {
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
            rt.block_on(run_remote_session(backend, config, chat_id, cmd_rx, ev_tx));
        });
    let remote_thread = match spawn_result {
        Ok(thread) => Some(thread),
        Err(e) => {
            remote_log(&format!("could not spawn remote session thread: {e}"));
            None
        }
    };
    // An older agent stops the process when the stream closes, which
    // dropping the handle does; a newer one keeps it running.
    (Session::from_remote_port(cmd_tx, remote_thread), ev_rx)
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

/// Remote wire boundary. Frontends may substitute synthetic streams without a server.
#[allow(async_fn_in_trait)]
pub trait RemoteChatTransport: Send + 'static {
    fn config(&self) -> &RemoteAgentClientConfig;
    async fn handshake(&self) -> Result<RemoteAgentHandshake, RemoteAgentClientError>;
    async fn claude_chat_attach(
        &self,
        open: ClaudeChatOpen,
        commands: mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatFrameStream, RemoteAgentClientError>;
    async fn claude_chat(
        &self,
        start: String,
        commands: mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatEventStream, RemoteAgentClientError>;
}

impl RemoteChatTransport for RemoteAgentBackend {
    fn config(&self) -> &RemoteAgentClientConfig {
        RemoteAgentBackend::config(self)
    }
    async fn handshake(&self) -> Result<RemoteAgentHandshake, RemoteAgentClientError> {
        RemoteAgentBackend::handshake(self).await
    }
    async fn claude_chat_attach(
        &self,
        open: ClaudeChatOpen,
        commands: mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatFrameStream, RemoteAgentClientError> {
        RemoteAgentBackend::claude_chat_attach(self, open, commands).await
    }
    async fn claude_chat(
        &self,
        start: String,
        commands: mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatEventStream, RemoteAgentClientError> {
        RemoteAgentBackend::claude_chat(self, start, commands).await
    }
}

/// Relay one remote chat: commands out as JSON, events back from JSON.
/// Picks the agent's protocol from its handshake. `chat_id`: attach to
/// that running chat instead of starting (or attaching by resume id).
async fn run_remote_session(
    backend: impl RemoteChatTransport,
    config: ClaudeSessionConfig,
    chat_id: Option<String>,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) {
    let client = backend.config();
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
    let persistent = match backend.handshake().await {
        Ok(handshake) => handshake
            .capabilities
            .iter()
            .any(|c| c == CLAUDE_CHAT_PERSISTENT_CAPABILITY),
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
struct RemoteChatLink<B> {
    backend: B,
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

impl<B: RemoteChatTransport> RemoteChatLink<B> {
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
        mut stream: ClaudeChatFrameStream,
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
    backend: impl RemoteChatTransport,
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

#[cfg(test)]
#[path = "remote_chat_tests.rs"]
mod remote_chat_tests;
