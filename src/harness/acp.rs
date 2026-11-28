//! Native ACP session (TRU-153): one long-lived agent process that speaks
//! the Agent Client Protocol, JSON-RPC 2.0 over stdio, protocol version 1
//! (agentclientprotocol.com). Antigravity (`agy acp`) and OpenCode
//! (`opencode acp`) run on this adapter.
//!
//! GitTerm is the ACP *client*: it calls `initialize`, `session/new` or
//! `session/load`, `session/prompt`, `session/cancel` and the setting
//! methods, and answers the agent's `session/request_permission`. It
//! advertises no file-system or terminal capability, so the agent runs its
//! own tools.
//!
//! `UpdateMapper` turns `session/update` notifications into `HarnessEvent`s
//! and is pure, so it is unit-tested against recorded frames in
//! `tests/fixtures/acp/`. `AcpSession` owns the process on a dedicated
//! thread, like `ClaudeSession`, and adds what ACP leaves to the client:
//! a prompt sent while a turn runs waits in GitTerm's queue and goes out
//! when the turn ends, and a resumed session's replayed history comes back
//! as one `HistoryReplayed` event.

use super::{HarnessCommand, HarnessEvent};
#[cfg(test)]
use super::{RuntimeDecision, UserPrompt};
pub use gitterm_core::harness::acp::*;
pub use gitterm_core::harness::acp_launch::*;
#[cfg(test)]
use serde_json::{json, Value};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Handle to a running ACP session. Dropping it shuts the process down.
pub struct AcpSession {
    snapshots_closed: tokio_util::sync::CancellationToken,
    commands: mpsc::UnboundedSender<HarnessCommand>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AcpSession {
    /// Start the agent on a dedicated thread. Spawn failures (including a
    /// legacy saved Gemini chat) arrive on the event channel as `Error` followed
    /// by `ProcessExited`.
    pub fn spawn(config: AcpSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let snapshots_closed = tokio_util::sync::CancellationToken::new();
        let close_token = snapshots_closed.clone();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let spawn_result = std::thread::Builder::new()
            .name("acp-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(HarnessEvent::Error(format!(
                            "could not start the ACP session runtime: {e}"
                        )));
                        let _ = ev_tx.send(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                };
                rt.block_on(run_session(config, cmd_rx, ev_tx, close_token));
            });
        let thread = match spawn_result {
            Ok(thread) => Some(thread),
            Err(e) => {
                acp_log(&format!("could not spawn session thread: {e}"));
                None
            }
        };
        (
            Self {
                commands: cmd_tx,
                snapshots_closed,
                thread,
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
            .map_err(|_| "the agent session has exited".to_string())
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        let _ = self.commands.send(HarnessCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            crate::shutdown::spawn(
                crate::shutdown::Kind::Session,
                "ACP session I/O and snapshots",
                move || {
                    thread
                        .join()
                        .map_err(|_| "ACP session thread panicked".to_string())
                },
            );
        }
    }
}

async fn run_session(
    config: AcpSessionConfig,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    close_token: tokio_util::sync::CancellationToken,
) {
    if config.agent == AcpAgent::Gemini {
        acp_log(GEMINI_REPLACED_NOTE);
        let _ = events.send(HarnessEvent::Error(GEMINI_REPLACED_NOTE.into()));
        let _ = events.send(HarnessEvent::ProcessExited { code: None });
        return;
    }
    #[cfg(test)]
    let program = native_chat_tests::program()
        .map(Ok)
        .unwrap_or_else(|| resolve_program(config.agent.program()));
    #[cfg(not(test))]
    let program = resolve_program(config.agent.program());
    let program = match program {
        Ok(program) => program,
        Err(error) => {
            acp_log(&error);
            let _ = events.send(HarnessEvent::Error(error));
            let _ = events.send(HarnessEvent::ProcessExited { code: None });
            return;
        }
    };
    let snapshots = crate::turn_snapshots::Recorder::new(
        config.cwd.clone(),
        config.resume.clone(),
        config.snapshot_facts_dir.clone(),
    )
    .with_close_token(close_token.clone());
    gitterm_core::harness::acp::run_session_harness(
        local_launch(
            env!("CARGO_PKG_VERSION"),
            config,
            program,
            Box::new(snapshots),
        ),
        commands,
        events,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(text: &str) -> Vec<Value> {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn mcp_servers_carry_their_headers_but_logs_and_debug_do_not() {
        let server = AcpMcpServer {
            name: "gitterm_tasks".into(),
            url: "http://127.0.0.1:1/mcp?caller=x".into(),
            headers: vec![("Authorization".into(), "Bearer secret-token".into())],
        };
        let antigravity = server.to_acp(AcpAgent::Antigravity);
        assert_eq!(antigravity["name"], "gitterm-tasks");
        assert_eq!(antigravity["type"], "http");
        assert_eq!(
            antigravity["headers"],
            json!([{"name": "Authorization", "value": "Bearer secret-token"}])
        );
        assert_eq!(server.to_acp(AcpAgent::OpenCode)["name"], "gitterm_tasks");
        let frame = request_frame(
            2,
            "session/new",
            json!({"cwd": "/", "mcpServers": [antigravity]}),
        );
        assert!(!redact_frame(&frame).to_string().contains("secret-token"));
        assert!(!format!("{server:?}").contains("secret-token"));
        let config = AcpSessionConfig {
            snapshot_facts_dir: None,
            agent: AcpAgent::Antigravity,
            cwd: "/".into(),
            resume: None,
            model: None,
            mode: None,
            effort: None,
            mcp_servers: vec![server],
            env: vec![("TEST_SECRET".into(), "key-value".into())],
            wire_log_dir: None,
            guidelines: None,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("key-value") && !debug.contains("secret-token"));
        assert_eq!(config.args(), vec!["acp"]);
    }

    fn antigravity_config() -> AcpSessionConfig {
        AcpSessionConfig {
            snapshot_facts_dir: None,
            agent: AcpAgent::Antigravity,
            cwd: "/synthetic/repo".into(),
            resume: None,
            model: Some("gemini-4-argon-medium".into()),
            mode: Some("default".into()),
            effort: None,
            mcp_servers: Vec::new(),
            env: Vec::new(),
            wire_log_dir: None,
            guidelines: None,
        }
    }

    #[test]
    fn native_append_contains_instructions_only_when_enabled_and_needs_no_env() {
        let mut config = antigravity_config();
        assert!(session_params(&config, Vec::new()).get("_meta").is_none());
        config.guidelines = Some(crate::guidelines::Guidelines {
            path: "/synthetic/instructions.md".into(),
            text: "Follow the repository rules.\n".into(),
        });
        assert_eq!(
            session_params(&config, Vec::new())["_meta"],
            json!({"antigravity":{"systemPrompt":{"append":"Follow the repository rules.\n"}}})
        );
        assert!(prepare_agent_env(&config).is_empty());
        let prompt = prompt("unchanged prompt");
        assert_eq!(
            prompt_blocks(&prompt, true),
            vec![json!({"type":"text","text":"unchanged prompt"})]
        );
        config.agent = AcpAgent::OpenCode;
        assert!(session_params(&config, Vec::new()).get("_meta").is_none());
        assert_eq!(
            opencode_config_content_with_guidelines(config.guidelines.as_ref())["instructions"],
            json!(["/synthetic/instructions.md"])
        );
    }

    #[tokio::test]
    async fn legacy_gemini_is_refused_before_any_process_is_started() {
        let mut config = antigravity_config();
        config.agent = AcpAgent::Gemini;
        let (_session, mut events) = AcpSession::spawn(config);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap(),
            Some(HarnessEvent::Error(GEMINI_REPLACED_NOTE.into()))
        );
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::ProcessExited { code: None })
        ));
    }

    #[test]
    fn native_instructions_and_capabilities() {
        let init = frames(include_str!(
            "../../tests/fixtures/acp/antigravity_initialize.jsonl"
        ));
        assert!(supports_system_prompt_append(&init[0]["result"]));
        assert!(supports_http_mcp(AcpAgent::Antigravity, &init[0]["result"]));
        assert!(!supports_http_mcp(AcpAgent::OpenCode, &init[0]["result"]));
        assert!(auth_hint(AcpAgent::Antigravity).contains("Run `agy` in a terminal and sign in"));
        assert!(prepare_agent_env(&antigravity_config()).is_empty());
        assert_eq!(
            opencode_config_content(),
            json!({"permission": {"edit": "ask", "bash": "ask"}})
        );
    }

    fn prompt(id: &str) -> UserPrompt {
        UserPrompt {
            text: id.into(),
            images: Vec::new(),
            id: Some(id.into()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod)] // Shared characterization sources are loaded by multiple focused test modules.
#[path = "../../tests/native-chat/native_chat_acp.rs"]
mod native_chat_tests;
