//! Iced's per-turn Pi command and executor, shared by desktop hosts.
use super::parse_line;
use crate::chat::{state::AgentEvent, types::AgentBackendConfig};
use std::sync::{Arc, Mutex};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
};

/// The debug tab config: no prompt is submitted until the user sends one.
pub fn debug_config(model: Result<String, std::env::VarError>, pid: u32) -> AgentBackendConfig {
    AgentBackendConfig::Pi {
        model: model.unwrap_or_else(|_| "openai-codex/gpt-5.4".to_string()),
        session_path: Some(format!("/tmp/gitterm-v6-agent-debug-{pid}.jsonl")),
        thinking: Some(true),
    }
}

/// Execute the already planned turn with the host's cwd, stream and stop signal.
pub async fn execute_turn(
    mut cmd: Command,
    repo_path: &PathBuf,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
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

pub fn build_command(
    config: &AgentBackendConfig,
    prompt: &str,
    guidelines_file: Option<&Path>,
) -> Result<Command, String> {
    build_command_for(Path::new("pi"), config, prompt, guidelines_file)
}

pub fn build_command_for(
    program: &Path,
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
            let mut cmd = Command::new(program);
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

#[derive(Debug)]
pub enum AgentInput {
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
pub struct AgentTaskHandle {
    snapshots_closed: tokio_util::sync::CancellationToken,
    input_tx: mpsc::UnboundedSender<AgentInput>,
    stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    pending_event_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
}

impl AgentTaskHandle {
    pub fn close_turn_snapshots(&self) {
        self.snapshots_closed.cancel();
    }

    /// Send a user prompt to the subprocess manager. Returns `Err` if the
    /// background task has dropped its receiver (which should only happen
    /// after a fatal error or shutdown).
    pub fn submit_prompt(&self, prompt: String) -> Result<(), String> {
        self.input_tx
            .send(AgentInput::Prompt(prompt))
            .map_err(|_| "agent subprocess manager has exited".to_string())
    }

    /// Fire the current turn's stop signal, if a turn is in flight. No-op
    /// otherwise (slot is empty between turns).
    pub fn request_stop(&self) {
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
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.pending_event_rx.lock().ok().and_then(|mut g| g.take())
    }
}

/// Complete per-turn launch inputs. The Pi child inherits the host environment.
pub struct Launch {
    pub config: AgentBackendConfig,
    pub cwd: PathBuf,
    pub guidelines_file: Option<PathBuf>,
    pub program: PathBuf,
}

/// Iced's debug-tab launch plan, including its active-tab cwd fallback.
pub fn debug_launch(
    workspace_dir: &Path,
    active_dir: Option<&Path>,
    model: Result<String, std::env::VarError>,
    pid: u32,
) -> Launch {
    Launch {
        config: debug_config(model, pid),
        cwd: active_dir.unwrap_or(workspace_dir).to_owned(),
        guidelines_file: None,
        program: "pi".into(),
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
pub fn spawn_agent_task<S: crate::harness::claude::SnapshotRecorder + 'static>(
    launch: Launch,
    snapshots: impl FnOnce(tokio_util::sync::CancellationToken) -> S + Send + 'static,
) -> AgentTaskHandle {
    let snapshots_closed = tokio_util::sync::CancellationToken::new();
    let close_token = snapshots_closed.clone();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));

    let thread = {
        let stop_slot = stop_slot.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("agent tokio runtime");
            rt.block_on(agent_loop(
                snapshots(close_token),
                launch,
                input_rx,
                event_tx,
                stop_slot,
            ));
        })
    };

    // Retain the supervisor join from launch through runtime teardown.
    crate::shutdown::spawn(
        crate::shutdown::Kind::Session,
        "Pi session I/O and snapshots",
        move || {
            thread
                .join()
                .map_err(|_| "Pi session thread panicked".to_string())
        },
    );
    AgentTaskHandle {
        snapshots_closed,
        input_tx,
        stop_slot,
        pending_event_rx: Mutex::new(Some(event_rx)),
    }
}

/// Per-turn loop: wait for a prompt, spawn the subprocess, stream events, repeat.
pub async fn agent_loop(
    mut snapshots: impl crate::harness::claude::SnapshotRecorder,
    launch: Launch,
    mut input_rx: mpsc::UnboundedReceiver<AgentInput>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    stop_slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
) {
    let Launch {
        config,
        cwd: repo_path,
        guidelines_file,
        program,
    } = launch;
    while let Some(input) = input_rx.recv().await {
        match input {
            AgentInput::Prompt(prompt) => {
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                if let Ok(mut g) = stop_slot.lock() {
                    *g = Some(stop_tx);
                }

                snapshots.begin().await;
                let result =
                    match build_command_for(&program, &config, &prompt, guidelines_file.as_deref())
                    {
                        Ok(cmd) => execute_turn(cmd, &repo_path, &event_tx, stop_rx).await,
                        Err(error) => Err(error),
                    };
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_agent_tab_config_and_argv_match_iced_without_environment_overrides() {
        let config = debug_config(Err(std::env::VarError::NotPresent), 42);
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::json!({
                "backend": "pi", "model": "openai-codex/gpt-5.4", "session_path": "/tmp/gitterm-v6-agent-debug-42.jsonl", "thinking": true,
            })
        );
        let command = build_command(&config, "Synthetic prompt", None).unwrap();
        assert_eq!(command.as_std().get_program(), "pi");
        assert_eq!(command.as_std().get_envs().count(), 0);
        assert_eq!(
            command
                .as_std()
                .get_args()
                .map(|s| s.to_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "--print",
                "--mode",
                "json",
                "--model",
                "openai-codex/gpt-5.4",
                "--session",
                "/tmp/gitterm-v6-agent-debug-42.jsonl",
                "--thinking",
                "medium",
                "Synthetic prompt"
            ]
        );
        let workspace = Path::new("/tmp/synthetic-workspace");
        let nested = workspace.join("nested");
        assert_eq!(
            debug_launch(workspace, Some(&nested), Ok("m".into()), 42).cwd,
            nested
        );
        assert_eq!(
            debug_launch(workspace, None, Ok("m".into()), 42).cwd,
            workspace
        );
        for model in ["synthetic/model", ""] {
            let AgentBackendConfig::Pi { model: actual, .. } = debug_config(Ok(model.into()), 42)
            else {
                unreachable!()
            };
            assert_eq!(actual, model);
        }
    }
}
