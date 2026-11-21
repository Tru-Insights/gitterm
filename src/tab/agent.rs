//! Agent tab kind: a Claude Code or pi subprocess driving a wry-hosted chat UI.
//!
//! The Iced adapter retains process handles and live presentation timing;
//! `gitterm_core::chat::state` owns chat transitions and the sole timeline.
//!
//! The conversation buffer (`AgentSession::conversation`) is the source of truth:
//! the webview is reinitialized from it on tab activation, not the other way around.
//! See `.plans/agent-tab-integration.md` for the full v1 design.

// Many of these types are referenced only by future steps (3 and 4). Suppress
// dead-code warnings until those steps land — re-evaluate this attribute when
// Step 4 ships.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use gitterm::harness::acp::AcpAgent;
use gitterm::harness::acp::{AcpMcpServer, AcpSessionConfig, ACP_WIRE_LOG_ENV};
use gitterm::harness::claude::{ClaudeMcpServer, ClaudeSessionConfig};
#[cfg(test)]
use gitterm::harness::transcript::TranscriptEntry;
use gitterm::harness::HarnessSession;
#[cfg(test)]
use gitterm::harness::{HarnessEvent, UserPrompt};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// Permission mode for a Claude tab whose config does not name one. Always
/// passed explicitly so a user-level `defaultMode` cannot pre-empt prompts.
pub(crate) const DEFAULT_CLAUDE_PERMISSION_MODE: &str = "default";
/// Every mode the CLI accepts for `set_permission_mode` (its `invalid_mode`
/// error lists these; `claude --help` also accepts `manual`, an alias it
/// reports back as `default`).
pub(crate) const CLAUDE_PERMISSION_MODES: [&str; 6] = [
    "default",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];
/// Effort levels the CLI accepts (`claude --help`: `--effort <level>`, and
/// every model's `supportedEffortLevels` in the initialize reply).
pub(crate) const CLAUDE_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Set to a directory to log every Claude stdin/stdout frame there.
const CLAUDE_WIRE_LOG_ENV: &str = "GITTERM_V6_CLAUDE_WIRE_LOG_DIR";

pub(crate) use gitterm_core::chat::types::{
    AgentBackend, AgentBackendConfig, AgentSessionState, HistoryLoad,
};

pub use gitterm_core::chat::state::AgentEvent;
pub(crate) use gitterm_core::chat::state::{Effect as ChatEffect, Intent as ChatIntent};
use gitterm_core::harness::pi::parse_line;

/// The Agent call whose subagent `ev` starts, if it is a subagent start.
pub(crate) fn subagent_started(ev: &AgentEvent) -> Option<&str> {
    match ev {
        AgentEvent::Harness(event) => gitterm::chat_page::subagent_started(event),
        _ => None,
    }
}

/// The timeline marker for when Agent call `parent`'s subagent started
/// (Unix ms). The chat page uses it when the start was not seen live.
pub(crate) fn subagent_started_at(parent: &str, at_ms: i64) -> serde_json::Value {
    gitterm::chat_page::subagent_started_at(parent, at_ms)
}

fn unix_ms(now: std::time::SystemTime) -> Option<i64> {
    now.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as i64)
}

/// Live agent-tab session state. The conversation buffer here is the source of
/// truth — the webview is reinitialized from it on tab activation.
pub(crate) struct AgentSession {
    pub(crate) config: AgentBackendConfig,
    chat: gitterm_core::chat::state::State,
    /// Background task that owns the subprocess, if one has been spawned.
    /// Lazy: `None` until the first prompt submit, then created and reused
    /// across turns until the tab is closed. Pi only.
    pub(crate) task_handle: Option<AgentTaskHandle>,
    /// Long-lived native harness process (Claude, or an ACP agent). Lazy
    /// like `task_handle`; dropped (and respawned resuming the session)
    /// when the process exits. Dropping a local one shuts the process down;
    /// dropping a remote Claude one detaches and leaves the chat running on
    /// its machine (closing the tab sends `Shutdown` first, TRU-152).
    pub(crate) harness: Option<HarnessSession>,
    /// A remote chat's "reconnecting" notice while its connection is down
    /// (TRU-152). Not in the conversation, so a page rebuilt meanwhile is
    /// given it again.
    pub(crate) remote_disconnected: Option<AgentEvent>,
    /// Turn and subagent times seen live; the buffer carries no clock.
    pub(crate) live_times: crate::chat_overview::LiveTimes,
}

// Read-only compatibility for timeline consumers; all writes use intents.
impl std::ops::Deref for AgentSession {
    type Target = gitterm_core::chat::state::State;
    fn deref(&self) -> &Self::Target {
        &self.chat
    }
}

impl AgentSession {
    /// Build a fresh session for a backend config. Empty conversation buffer,
    /// no session_id yet, Idle, no background task spawned.
    pub(crate) fn new(config: AgentBackendConfig) -> Self {
        Self {
            config,
            chat: Default::default(),
            task_handle: None,
            harness: None,
            remote_disconnected: None,
            live_times: crate::chat_overview::LiveTimes::default(),
        }
    }

    pub(crate) fn reduce(&mut self, intent: ChatIntent) -> ChatEffect {
        self.chat.reduce(intent)
    }

    pub(crate) fn begin_runtime(&mut self) {
        self.chat.begin_runtime();
    }

    pub(crate) fn history_to_load(&self) -> Option<&str> {
        self.chat.history_to_load(&self.config)
    }

    pub(crate) fn bind_session(&mut self, id: Option<String>) {
        self.reduce(ChatIntent::BindSession(id));
    }

    pub(crate) fn record(&mut self, ev: AgentEvent) -> (Vec<AgentEvent>, Vec<AgentEvent>) {
        let now = std::time::SystemTime::now();
        self.live_times.observe(&ev, now);
        match self.reduce(ChatIntent::Record {
            event: ev,
            now_ms: unix_ms(now),
        }) {
            ChatEffect::Recorded { before, after } => (before, after),
            _ => unreachable!(),
        }
    }

    /// The Claude process settings for this tab. `model` "default" (or
    /// empty) leaves the model to the user's Claude settings. `mcp_servers`
    /// are the GitTerm MCP servers the app attaches (task and browser);
    /// `chrome` attaches Claude in Chrome, the chat's workspace switch
    /// (`claude_in_chrome` in workspaces.json, off by default, TRU-181);
    /// `guidelines_file` appends GitTerm's change guidelines (TRU-174).
    pub(crate) fn claude_session_config(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<ClaudeMcpServer>,
        chrome: bool,
        guidelines_file: Option<PathBuf>,
    ) -> Option<ClaudeSessionConfig> {
        let AgentBackendConfig::Claude {
            model,
            permission_mode,
            effort,
        } = &self.config
        else {
            return None;
        };
        let model = model.trim();
        Some(ClaudeSessionConfig {
            snapshot_facts_dir: Some(gitterm::chat_facts::facts_dir(
                &crate::config::global_config_dir(),
            )),
            cwd,
            model: (!model.is_empty() && model != "default").then(|| model.to_string()),
            permission_mode: permission_mode
                .clone()
                .unwrap_or_else(|| DEFAULT_CLAUDE_PERMISSION_MODE.to_string()),
            effort: effort.clone(),
            resume: self.session_id.clone(),
            wire_log_dir: std::env::var_os(CLAUDE_WIRE_LOG_ENV).map(PathBuf::from),
            mcp_servers,
            chrome,
            guidelines_file,
            guidelines_content: None,
        })
    }

    /// The ACP agent settings for this tab (`None` for other backends).
    pub(crate) fn acp_session_config(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<AcpMcpServer>,
        guidelines: Option<gitterm::guidelines::Guidelines>,
    ) -> Option<AcpSessionConfig> {
        let AgentBackendConfig::Acp {
            agent,
            model,
            mode,
            effort,
        } = &self.config
        else {
            return None;
        };
        if self.config.read_only_note().is_some() {
            return None;
        }
        let model = model.trim();
        Some(AcpSessionConfig {
            snapshot_facts_dir: Some(gitterm::chat_facts::facts_dir(
                &crate::config::global_config_dir(),
            )),
            agent: *agent,
            cwd,
            resume: self.session_id.clone(),
            model: (!model.is_empty()).then(|| model.to_string()),
            mode: Some(
                mode.clone()
                    .unwrap_or_else(|| agent.default_mode().to_string()),
            ),
            effort: effort.clone(),
            mcp_servers,
            env: Vec::new(),
            wire_log_dir: std::env::var_os(ACP_WIRE_LOG_ENV).map(PathBuf::from),
            guidelines,
        })
    }

    pub(crate) fn backend(&self) -> AgentBackend {
        self.config.backend()
    }

    /// Whether a prompt submitted now joins Claude's queue instead of
    /// starting a turn: a Claude process is running a turn. The CLI takes
    /// it at the running turn's next tool boundary, or as the next turn.
    /// A pi tab never gets here mid-turn (its composer waits for the turn).
    pub(crate) fn submit_queues(&self) -> bool {
        submit_queues(self.backend(), self.harness.is_some(), &self.state)
    }

    /// The permission mode the next Claude spawn passes; `None` for pi.
    pub(crate) fn configured_permission_mode(&self) -> Option<String> {
        match &self.config {
            AgentBackendConfig::Claude {
                permission_mode, ..
            } => Some(
                permission_mode
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CLAUDE_PERMISSION_MODE.to_string()),
            ),
            AgentBackendConfig::Acp { agent, mode, .. } => Some(
                mode.clone()
                    .unwrap_or_else(|| agent.default_mode().to_string()),
            ),
            AgentBackendConfig::Pi { .. } => None,
        }
    }
}

pub(crate) use gitterm_core::chat::state::submit_queues;

pub(crate) use gitterm_core::chat::checkout::{
    checkout_choices, checkout_label, CheckoutChoice, TaskWorktreeInfo,
};

// ---- Subprocess manager (Step 3 of TRU-29) -------------------------------

/// Inputs the UI can send into the agent subprocess. New variants will be added
/// as features land (e.g. `ApprovePermission { id, allowed }` once Claude's
/// permission flow ships).
#[derive(Debug)]
pub(crate) enum AgentInput {
    /// User submitted a prompt. The subprocess manager spawns one subprocess
    /// per prompt (matching the spike's per-turn pattern); multi-turn
    /// continuity is preserved via `--session` (pi) or `--resume` (Claude).
    Prompt(String),
}

/// Handle to the background tokio task that owns the agent subprocess.
/// Lives on `AgentSession.task_handle` for the lifetime of the tab.
///
/// Per-turn stop signaling: each turn parks a fresh `oneshot::Sender<()>` in
/// `stop_slot`. The UI fires it via `request_stop()`; the manager drops it
/// into the `tokio::select!` arm against subprocess output. Cleared at turn end.
///
/// `pending_event_rx` is consumed exactly once by the subscription bridge; after
/// that it stays `None` and the subscription's stream lives independently of the
/// handle. See `take_event_receiver`.
pub(crate) struct AgentTaskHandle {
    snapshots_closed: tokio_util::sync::CancellationToken,
    input_tx: mpsc::UnboundedSender<AgentInput>,
    stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    pending_event_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
}

impl AgentTaskHandle {
    pub(crate) fn close_turn_snapshots(&self) {
        self.snapshots_closed.cancel();
    }

    /// Send a user prompt to the subprocess manager. Returns `Err` if the
    /// background task has dropped its receiver (which should only happen
    /// after a fatal error or shutdown).
    pub(crate) fn submit_prompt(&self, prompt: String) -> Result<(), String> {
        self.input_tx
            .send(AgentInput::Prompt(prompt))
            .map_err(|_| "agent subprocess manager has exited".to_string())
    }

    /// Fire the current turn's stop signal, if a turn is in flight. No-op
    /// otherwise (slot is empty between turns).
    pub(crate) fn request_stop(&self) {
        if let Ok(mut slot) = self.stop_slot.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(());
            }
        }
    }

    /// Take the event receiver for wiring into an Iced subscription. Returns
    /// `Some` exactly once per handle; subsequent calls return `None`. The
    /// caller owns the receiver after this and is responsible for keeping the
    /// subscription alive until the tab is dropped.
    pub(crate) fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.pending_event_rx.lock().ok().and_then(|mut g| g.take())
    }
}

/// Spawn the per-tab subprocess manager.
///
/// Returns the handle (parked on the session) plus the receiver side of the
/// event channel. The caller wraps the receiver in an Iced `Task::run` so
/// each `AgentEvent` becomes an `Event::AgentEventReceived(tab_id, ev)`.
///
/// Lifecycle: a dedicated thread runs a current-thread tokio runtime that
/// loops on the input channel. Each `AgentInput::Prompt` spawns one subprocess
/// turn, streams its stdout JSON, and emits a terminating `Other(...)` event
/// when the turn completes (so the UI can flip state from Streaming to Idle).
/// Closing the receiver / dropping the handle ends the task naturally.
/// `guidelines_file` is appended to every turn's system prompt (TRU-174).
pub(crate) fn spawn_agent_task(
    snapshot_chat: String,
    config: AgentBackendConfig,
    repo_path: PathBuf,
    guidelines_file: Option<PathBuf>,
) -> AgentTaskHandle {
    let snapshots_closed = tokio_util::sync::CancellationToken::new();
    let close_token = snapshots_closed.clone();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));

    {
        let stop_slot = stop_slot.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("agent tokio runtime");
            let snapshots = gitterm::turn_snapshots::Recorder::new(
                repo_path.clone(),
                Some(snapshot_chat),
                Some(gitterm::chat_facts::facts_dir(
                    &crate::config::global_config_dir(),
                )),
            )
            .with_close_token(close_token);
            rt.block_on(agent_loop(
                snapshots,
                config,
                repo_path,
                guidelines_file,
                input_rx,
                event_tx,
                stop_slot,
            ));
        });
    }

    AgentTaskHandle {
        snapshots_closed,
        input_tx,
        stop_slot,
        pending_event_rx: Mutex::new(Some(event_rx)),
    }
}

/// Per-turn loop: wait for a prompt, spawn the subprocess, stream events, repeat.
async fn agent_loop(
    mut snapshots: gitterm::turn_snapshots::Recorder,
    config: AgentBackendConfig,
    repo_path: PathBuf,
    guidelines_file: Option<PathBuf>,
    mut input_rx: mpsc::UnboundedReceiver<AgentInput>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
) {
    while let Some(input) = input_rx.recv().await {
        match input {
            AgentInput::Prompt(prompt) => {
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                if let Ok(mut g) = stop_slot.lock() {
                    *g = Some(stop_tx);
                }

                snapshots.begin().await;
                let result = run_turn(
                    &config,
                    &repo_path,
                    guidelines_file.as_deref(),
                    &prompt,
                    &event_tx,
                    stop_rx,
                )
                .await;
                snapshots.end().await;

                if let Ok(mut g) = stop_slot.lock() {
                    g.take();
                }

                if let Err(err) = result {
                    let _ = event_tx.send(AgentEvent::Other(serde_json::json!({
                        "type": "error",
                        "message": err,
                    })));
                }
            }
        }
    }
    snapshots.end().await;
    // input channel closed: tab being dropped — exit
}

/// Spawn one subprocess turn and stream its stdout as `AgentEvent::Other(line)`
/// for each JSON line. Step 3 keeps parsing minimal — Step 4 will refine each
/// line into the typed variants (`AssistantText`, `ToolCallStart`, etc.).
async fn run_turn(
    config: &AgentBackendConfig,
    repo_path: &PathBuf,
    guidelines_file: Option<&Path>,
    prompt: &str,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut cmd = build_command(config, prompt, guidelines_file)?;
    #[cfg(test)]
    {
        cmd = native_chat_tests::override_command(cmd);
    }
    cmd.current_dir(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn agent subprocess: {}", e))?;
    let stdout = child.stdout.take().ok_or("child stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut stopped = false;
    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                match maybe_line {
                    Ok(Some(line)) => {
                        let Some(value) = parse_line(line) else {
                            continue;
                        };
                        if event_tx.send(AgentEvent::Other(value)).is_err() {
                            // Receiver dropped — tab is gone, abort.
                            let _ = child.start_kill();
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(format!("stdout read error: {}", e));
                    }
                }
            }
            _ = &mut stop_rx => {
                stopped = true;
                let _ = child.start_kill();
                break;
            }
        }
    }

    let _ = child.wait().await;
    // Sentinel event so the UI knows the turn ended. Refined into a typed
    // variant in Step 4 (likely a state-transition event rather than a raw blob).
    let _ = event_tx.send(AgentEvent::Other(serde_json::json!({
        "type": if stopped { "stopped" } else { "done" },
    })));
    Ok(())
}

fn build_command(
    config: &AgentBackendConfig,
    prompt: &str,
    guidelines_file: Option<&Path>,
) -> Result<Command, String> {
    match config {
        AgentBackendConfig::Pi {
            model,
            session_path,
            thinking,
        } => {
            let mut cmd = Command::new("pi");
            cmd.arg("--print")
                .arg("--mode")
                .arg("json")
                .arg("--model")
                .arg(model);
            if let Some(path) = session_path {
                cmd.arg("--session").arg(path);
            }
            if let Some(t) = thinking.as_ref() {
                if *t {
                    cmd.arg("--thinking").arg("medium");
                }
            }
            // pi reads an existing file given as the value.
            if let Some(path) = guidelines_file {
                cmd.arg("--append-system-prompt").arg(path);
            }
            cmd.arg(prompt);
            Ok(cmd)
        }
        // Claude tabs run one long-lived process through
        // `gitterm::harness::claude::ClaudeSession` (TRU-140).
        AgentBackendConfig::Claude { .. } => {
            Err("Claude tabs use the native harness session, not per-turn processes".to_string())
        }
        AgentBackendConfig::Acp { .. } => {
            Err("ACP tabs use the native harness session, not per-turn processes".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_turns_append_the_guidelines_file_before_the_prompt() {
        let config = AgentBackendConfig::Pi {
            model: "m".into(),
            session_path: None,
            thinking: None,
        };
        let args = |guidelines: Option<&Path>| -> Vec<String> {
            build_command(&config, "hi", guidelines)
                .unwrap()
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            args(Some(Path::new("/cfg/agent-guidelines.md"))),
            [
                "--print",
                "--mode",
                "json",
                "--model",
                "m",
                "--append-system-prompt",
                "/cfg/agent-guidelines.md",
                "hi"
            ]
        );
        assert!(!args(None).contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn claude_chats_pass_the_guidelines_file() {
        let session = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        let config = session
            .claude_session_config(
                PathBuf::from("/tmp"),
                Vec::new(),
                false,
                Some(PathBuf::from("/cfg/agent-guidelines.md")),
            )
            .unwrap();
        assert!(config
            .args()
            .join(" ")
            .contains("--append-system-prompt-file /cfg/agent-guidelines.md"));
        let without = session
            .claude_session_config(PathBuf::from("/tmp"), Vec::new(), false, None)
            .unwrap();
        assert!(!without.args().join(" ").contains("--append-system-prompt"));
    }

    fn sub(parent: &str, event: HarnessEvent) -> AgentEvent {
        AgentEvent::Harness(HarnessEvent::SubagentEvent {
            parent_tool_use_id: parent.into(),
            event: Box::new(event),
        })
    }

    fn harness(events: &[AgentEvent]) -> Vec<&HarnessEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Harness(h) => Some(h),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn saved_gemini_is_read_only_and_cannot_start_or_load_a_harness() {
        let json = serde_json::json!({"backend":"acp", "agent":"gemini", "model":"gemini-4-argon"});
        let config: AgentBackendConfig = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&config).unwrap(), json);
        assert!(config
            .read_only_note()
            .unwrap()
            .contains("can't be continued"));
        let mut session = AgentSession::new(config);
        session.bind_session(Some("legacy-session".into()));
        assert_eq!(session.history_to_load(), None);
        assert!(session
            .acp_session_config(PathBuf::from("/tmp"), Vec::new(), None)
            .is_none());
        assert_eq!(session.backend(), AgentBackend::Acp(AcpAgent::Gemini));
        assert!(!AcpAgent::ALL.contains(&AcpAgent::Gemini));
    }

    #[test]
    fn acp_tab_config_round_trips_and_builds_the_session_config() {
        let config = AgentBackendConfig::Acp {
            agent: AcpAgent::Antigravity,
            model: "gemini-4-argon-medium".into(),
            mode: None,
            effort: None,
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"backend": "acp", "agent": "antigravity", "model": "gemini-4-argon-medium"})
        );
        let back: AgentBackendConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.backend(), AgentBackend::Acp(AcpAgent::Antigravity));
        let mut session = AgentSession::new(back);
        session.bind_session(Some("g-1".into()));
        assert_eq!(session.history_to_load(), Some("g-1"));
        assert_eq!(
            session.configured_permission_mode().as_deref(),
            Some("default")
        );
        let acp = session
            .acp_session_config(PathBuf::from("/tmp/repo"), Vec::new(), None)
            .unwrap();
        assert_eq!(acp.resume.as_deref(), Some("g-1"));
        assert_eq!(acp.model.as_deref(), Some("gemini-4-argon-medium"));
        assert_eq!(acp.mode.as_deref(), Some("default"));
        // An empty OpenCode model keeps OpenCode's own choice.
        let opencode = AgentSession::new(AgentBackendConfig::Acp {
            agent: AcpAgent::OpenCode,
            model: String::new(),
            mode: Some("plan".into()),
            effort: Some("high".into()),
        });
        let acp = opencode
            .acp_session_config(PathBuf::from("/tmp"), Vec::new(), None)
            .unwrap();
        assert_eq!(acp.model, None);
        assert_eq!(acp.mode.as_deref(), Some("plan"));
        assert_eq!(acp.effort.as_deref(), Some("high"));
        assert!(submit_queues(
            AgentBackend::Acp(AcpAgent::OpenCode),
            true,
            &AgentSessionState::Streaming
        ));
        assert!(!submit_queues(
            AgentBackend::Acp(AcpAgent::OpenCode),
            false,
            &AgentSessionState::Streaming
        ));
    }

    #[test]
    fn relayed_prompts_are_user_prompts_known_by_id() {
        let mut session = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        let own = UserPrompt {
            text: "mine".into(),
            images: Vec::new(),
            id: Some("u1".into()),
        };
        session.record(AgentEvent::submitted_prompt(&own, false));
        session.record(AgentEvent::Harness(HarnessEvent::TextDelta("ok".into())));
        assert!(session.shows_prompt("u1"));
        assert!(!session.shows_prompt("u2"));

        let relayed = AgentEvent::relayed_prompt(Some("u2"), "from the phone", 2, true);
        assert_eq!(
            relayed.webview_payload().unwrap(),
            serde_json::json!({"type": "user_prompt", "text": "from the phone",
                               "id": "u2", "queued": true, "image_count": 2})
        );
        session.record(relayed);
        assert!(session.shows_prompt("u2"));
        // No id: never marked queued (nothing could withdraw it).
        assert_eq!(
            AgentEvent::relayed_prompt(None, "hi", 0, true)
                .webview_payload()
                .unwrap(),
            serde_json::json!({"type": "user_prompt", "text": "hi"})
        );
    }

    #[test]
    fn recorded_turn_times_round_trip_into_replay_payloads() {
        let mut session = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        session.record(AgentEvent::user_prompt("timed"));
        session.record(AgentEvent::Harness(HarnessEvent::TurnStarted {
            session_id: None,
            model: None,
        }));
        session.record(AgentEvent::Harness(HarnessEvent::ItemStarted {
            id: "tool-1".into(),
            kind: gitterm::harness::ItemKind::ToolCall {
                name: "Bash".into(),
                input: serde_json::json!({"command":"true"}),
            },
        }));
        session.record(AgentEvent::Harness(HarnessEvent::TurnCompleted {
            status: gitterm::harness::TurnStatus::Completed,
            usage: serde_json::Value::Null,
            cost_usd: None,
            rate_limit: None,
        }));

        let payloads: Vec<_> = session
            .conversation
            .iter()
            .filter_map(AgentEvent::webview_payload)
            .collect();
        let encoded = serde_json::to_string(&payloads).unwrap();
        let replay: Vec<serde_json::Value> = serde_json::from_str(&encoded).unwrap();
        let start = replay
            .iter()
            .find(|payload| payload["kind"] == "turn_timing" && payload.get("start_ms").is_some())
            .unwrap();
        let end = replay
            .iter()
            .find(|payload| payload["kind"] == "turn_timing" && payload.get("end_ms").is_some())
            .unwrap();
        assert!(start["start_ms"].as_i64().is_some());
        assert!(end["end_ms"].as_i64().is_some());
        assert!(replay
            .iter()
            .any(|payload| payload["event"]["type"] == "turn_started"));
        assert!(replay
            .iter()
            .any(|payload| payload["event"]["type"] == "turn_completed"));
    }

    #[test]
    fn queued_prompt_and_process_exit_timing_survive_conversation_replay() {
        let mut session = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        session.record(AgentEvent::user_prompt("first"));
        session.record(AgentEvent::Harness(HarnessEvent::ItemStarted {
            id: "tool-1".into(),
            kind: gitterm::harness::ItemKind::ToolCall {
                name: "Bash".into(),
                input: serde_json::json!({"command":"true"}),
            },
        }));
        session.record(AgentEvent::relayed_prompt(
            Some("queued-1"),
            "second",
            0,
            true,
        ));
        session.record(AgentEvent::Harness(HarnessEvent::ProcessExited {
            code: Some(1),
        }));

        let replay: Vec<_> = session
            .conversation
            .iter()
            .filter_map(AgentEvent::webview_payload)
            .collect();
        let split = replay
            .iter()
            .position(|payload| {
                payload["kind"] == "turn_timing" && payload.get("split_ms").is_some()
            })
            .expect("queued prompt split timestamp is persisted");
        let queued = replay
            .iter()
            .position(|payload| payload["type"] == "user_prompt" && payload["id"] == "queued-1")
            .expect("queued prompt is persisted");
        assert!(split < queued, "split time precedes its prompt boundary");
        assert!(
            replay.iter().any(|payload| {
                payload["kind"] == "turn_timing" && payload["end_ms"].as_i64().is_some()
            }),
            "process exit persists a turn end time"
        );
    }

    #[test]
    fn legacy_conversation_without_turn_times_still_has_replay_events() {
        let old_conversation = [
            AgentEvent::user_prompt("legacy"),
            AgentEvent::Harness(HarnessEvent::TurnCompleted {
                status: gitterm::harness::TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            }),
        ];
        let payloads: Vec<_> = old_conversation
            .iter()
            .filter_map(AgentEvent::webview_payload)
            .collect();
        assert_eq!(payloads.len(), 2);
        assert!(payloads
            .iter()
            .all(|payload| payload["kind"] != "turn_timing"));

        let mut queued = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        queued.record(AgentEvent::relayed_prompt(
            Some("queued-1"),
            "next",
            0,
            true,
        ));
        let queued_timing: Vec<_> = queued
            .conversation
            .iter()
            .filter_map(AgentEvent::webview_payload)
            .filter(|payload| payload["kind"] == "turn_timing")
            .collect();
        assert_eq!(queued_timing.len(), 1);
        assert!(queued_timing[0]["split_ms"].as_i64().is_some());
    }

    #[test]
    fn record_coalesces_subagent_deltas_per_parent() {
        let mut session = AgentSession::new(AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        session.record(AgentEvent::Harness(HarnessEvent::TextDelta(
            "parent ".into(),
        )));
        session.record(sub("a", HarnessEvent::TextDelta("one ".into())));
        session.record(sub("a", HarnessEvent::TextDelta("two".into())));
        // A different subagent, or a non-delta, starts a new entry.
        session.record(sub("b", HarnessEvent::TextDelta("other".into())));
        session.record(sub("b", HarnessEvent::ThinkingDelta("hm".into())));
        session.record(sub("b", HarnessEvent::ThinkingDelta("m".into())));
        session.record(sub(
            "b",
            HarnessEvent::ItemCompleted {
                id: "t".into(),
                output: String::new(),
                is_error: false,
            },
        ));
        session.record(sub(
            "b",
            HarnessEvent::ItemCompleted {
                id: "t2".into(),
                output: String::new(),
                is_error: false,
            },
        ));
        // Subagent text does not merge into the parent's text either way.
        session.record(AgentEvent::Harness(HarnessEvent::TextDelta("more".into())));

        let got = harness(&session.conversation);
        assert_eq!(got.len(), 7, "{got:?}");
        assert_eq!(got[0], &HarnessEvent::TextDelta("parent ".into()));
        let AgentEvent::Harness(expected) = sub("a", HarnessEvent::TextDelta("one two".into()))
        else {
            unreachable!()
        };
        assert_eq!(got[1], &expected);
        let AgentEvent::Harness(expected) = sub("b", HarnessEvent::ThinkingDelta("hmm".into()))
        else {
            unreachable!()
        };
        assert_eq!(got[3], &expected);
        assert_eq!(got[6], &HarnessEvent::TextDelta("more".into()));
    }
}

#[cfg(test)]
#[path = "../../tests/native-chat/native_chat_agent.rs"]
mod native_chat_tests;
