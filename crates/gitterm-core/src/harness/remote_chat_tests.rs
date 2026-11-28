//! Synthetic wire streams shared by the pre-extraction Iced and core tests.
use super::*;
use crate::chat::types::{RuntimeDecision, RuntimeRequestKind};
use crate::remote::client::{
    ClaudeChatEventStream, ClaudeChatFrameStream, RemoteAgentClientError, RemoteAgentHandshake,
};
use serde_json::{json, Value};
use tokio_stream::wrappers::UnboundedReceiverStream;

enum Call {
    Attach {
        open: ClaudeChatOpen,
        commands: mpsc::UnboundedReceiver<String>,
        frames: mpsc::UnboundedSender<Result<ClaudeChatFrame, RemoteAgentClientError>>,
    },
    Legacy {
        start: String,
        commands: mpsc::UnboundedReceiver<String>,
        events: mpsc::UnboundedSender<Result<String, RemoteAgentClientError>>,
    },
}

struct Transport {
    config: RemoteAgentClientConfig,
    persistent: bool,
    calls: mpsc::UnboundedSender<Call>,
}

impl RemoteChatTransport for Transport {
    fn config(&self) -> &RemoteAgentClientConfig {
        &self.config
    }
    async fn handshake(&self) -> Result<RemoteAgentHandshake, RemoteAgentClientError> {
        Ok(RemoteAgentHandshake {
            remote_id: self.config.remote_id.clone(),
            endpoint: self.config.endpoint.clone(),
            agent_name: "fixture".into(),
            agent_version: "fixture-version".into(),
            protocol_version: 1,
            capabilities: if self.persistent {
                vec!["claude_chat_persistent".into()]
            } else {
                vec!["claude_chat".into()]
            },
            home_dir: String::new(),
        })
    }
    async fn claude_chat_attach(
        &self,
        open: ClaudeChatOpen,
        commands: mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatFrameStream, RemoteAgentClientError> {
        let (frames, rx) = mpsc::unbounded_channel();
        self.calls
            .send(Call::Attach {
                open,
                commands,
                frames,
            })
            .ok()
            .unwrap();
        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }
    async fn claude_chat(
        &self,
        start: String,
        commands: mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatEventStream, RemoteAgentClientError> {
        let (events, rx) = mpsc::unbounded_channel();
        self.calls
            .send(Call::Legacy {
                start,
                commands,
                events,
            })
            .ok()
            .unwrap();
        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }
}

fn config() -> ClaudeSessionConfig {
    ClaudeSessionConfig {
        cwd: "/fixture/project".into(),
        snapshot_facts_dir: None,
        model: Some("fixture-model".into()),
        permission_mode: "default".into(),
        effort: Some("high".into()),
        resume: None,
        chrome: false,
        wire_log_dir: None,
        mcp_servers: vec![],
        guidelines_file: None,
        guidelines_content: None,
    }
}

struct Running {
    calls: mpsc::UnboundedReceiver<Call>,
    commands: mpsc::UnboundedSender<HarnessCommand>,
    events: mpsc::UnboundedReceiver<HarnessEvent>,
    task: tokio::task::JoinHandle<()>,
}

fn run(persistent: bool, config: ClaudeSessionConfig, chat_id: Option<&str>) -> Running {
    let (call_tx, calls) = mpsc::unbounded_channel();
    let (commands, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, events) = mpsc::unbounded_channel();
    let backend = Transport {
        config: RemoteAgentClientConfig {
            remote_id: "fixture".into(),
            name: "fixture".into(),
            endpoint: "http://fixture.invalid:1234".into(),
            token_ref: "env:UNUSED_FIXTURE".into(),
        },
        persistent,
        calls: call_tx,
    };
    let task = tokio::spawn(run_remote_session(
        backend,
        config,
        chat_id.map(str::to_owned),
        cmd_rx,
        ev_tx,
    ));
    Running {
        calls,
        commands,
        events,
        task,
    }
}

async fn recv<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("synthetic stream must make progress")
        .expect("stream remains open")
}
async fn finished(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}
fn opened(head_seq: u64, gap: bool) -> ClaudeChatFrame {
    ClaudeChatFrame::Opened(ClaudeChatOpened {
        chat_id: "fixture-chat".into(),
        attached: true,
        claude_session_id: "fixture-session".into(),
        head_seq,
        gap,
    })
}
fn event(ev: HarnessEvent, seq: u64, redelivered: bool) -> ClaudeChatFrame {
    ClaudeChatFrame::Event {
        json: serde_json::to_string(&ev).unwrap(),
        seq,
        redelivered,
    }
}
fn permission() -> HarnessEvent {
    HarnessEvent::RuntimeRequest {
        request_id: "fixture-request".into(),
        tool_use_id: "fixture-tool".into(),
        kind: RuntimeRequestKind::Permission {
            tool_name: "Read".into(),
            input: json!({"path": "fixture.txt"}),
            suggestions: vec![json!({"type":"addRules", "rules":[]})],
            decision_reason: Some("fixture reason".into()),
        },
    }
}

#[tokio::test]
async fn remote_chat_start_and_resume_preserve_wire_and_argv() {
    for resume in [None, Some("fixture-session")] {
        let mut cfg = config();
        cfg.resume = resume.map(str::to_owned);
        let mut running = run(true, cfg, None);
        let Call::Attach {
            open,
            mut commands,
            frames,
        } = recv(&mut running.calls).await
        else {
            panic!("persistent open")
        };
        let ClaudeChatOpen::Start { start_json } = open else {
            panic!("start")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&start_json).unwrap(),
            json!({"cwd":"/fixture/project","model":"fixture-model","permission_mode":"default","effort":"high","resume":resume,"chrome":false})
        );
        let start: RemoteClaudeChatStart = serde_json::from_str(&start_json).unwrap();
        let agent = start.into_config("/fixture/project".into());
        let mut expected: Vec<String> = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--include-partial-messages",
            "--no-chrome",
            "--model",
            "fixture-model",
            "--permission-mode",
            "default",
            "--effort",
            "high",
        ]
        .map(str::to_owned)
        .into();
        if let Some(id) = resume {
            expected.push(format!("--resume={id}"));
        }
        assert_eq!(agent.args(), expected);
        assert!(agent.mcp_servers.is_empty());
        assert!(agent.wire_log_dir.is_none());
        frames.send(Ok(opened(0, false))).unwrap();
        drop(running.commands);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), commands.recv())
                .await
                .unwrap()
                .is_none()
        );
        drop(frames);
        finished(running.task).await;
    }
}

#[tokio::test]
async fn remote_chat_attach_never_sends_start_even_when_chat_exits() {
    let mut running = run(true, config(), Some("fixture-chat"));
    let Call::Attach { open, frames, .. } = recv(&mut running.calls).await else {
        panic!("attach")
    };
    assert_eq!(
        open,
        ClaudeChatOpen::Chat {
            chat_id: "fixture-chat".into(),
            after_seq: None
        }
    );
    frames.send(Ok(opened(0, false))).unwrap();
    frames
        .send(Ok(event(
            HarnessEvent::ProcessExited { code: Some(0) },
            1,
            false,
        )))
        .unwrap();
    drop(frames);
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    finished(running.task).await;
    assert!(running.calls.try_recv().is_err());
}

#[tokio::test]
async fn remote_chat_permission_round_trip_preserves_decision_bytes() {
    let mut running = run(true, config(), None);
    let Call::Attach {
        mut commands,
        frames,
        ..
    } = recv(&mut running.calls).await
    else {
        panic!("attach")
    };
    frames.send(Ok(opened(0, false))).unwrap();
    frames.send(Ok(event(permission(), 1, false))).unwrap();
    assert_eq!(recv(&mut running.events).await, permission());
    let answer = HarnessCommand::Answer {
        request_id: "fixture-request".into(),
        decision: RuntimeDecision::Allow {
            updated_input: Some(json!({"path":"approved.txt"})),
            remember: Some(json!({"type":"addRules", "rules":[]})),
        },
    };
    running.commands.send(answer.clone()).unwrap();
    let bytes = recv(&mut commands).await;
    assert_eq!(bytes, serde_json::to_string(&answer).unwrap());
    assert_eq!(
        serde_json::from_str::<HarnessCommand>(&bytes).unwrap(),
        answer
    );
    let resolved = HarnessEvent::RuntimeRequestResolved {
        request_id: "fixture-request".into(),
    };
    frames.send(Ok(event(resolved.clone(), 2, false))).unwrap();
    assert_eq!(recv(&mut running.events).await, resolved);
    frames
        .send(Ok(event(
            HarnessEvent::ProcessExited { code: Some(0) },
            3,
            false,
        )))
        .unwrap();
    drop(frames);
    finished(running.task).await;
}

#[tokio::test]
async fn remote_chat_reconnect_dedups_and_reports_history_gap() {
    let mut running = run(true, config(), None);
    let Call::Attach { frames, .. } = recv(&mut running.calls).await else {
        panic!("attach")
    };
    frames.send(Ok(opened(0, false))).unwrap();
    frames.send(Ok(event(permission(), 1, false))).unwrap();
    frames
        .send(Ok(event(HarnessEvent::TextDelta("a".into()), 2, false)))
        .unwrap();
    assert_eq!(recv(&mut running.events).await, permission());
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::TextDelta("a".into())
    );
    drop(frames);
    assert!(matches!(
        recv(&mut running.events).await,
        HarnessEvent::RemoteDisconnected { .. }
    ));
    let Call::Attach { open, frames, .. } = recv(&mut running.calls).await else {
        panic!("reattach")
    };
    assert_eq!(
        open,
        ClaudeChatOpen::Chat {
            chat_id: "fixture-chat".into(),
            after_seq: Some(2)
        }
    );
    frames.send(Ok(opened(3, false))).unwrap();
    frames.send(Ok(event(permission(), 1, true))).unwrap();
    frames
        .send(Ok(event(
            HarnessEvent::TextDelta("duplicate".into()),
            2,
            false,
        )))
        .unwrap();
    frames
        .send(Ok(event(HarnessEvent::TextDelta("b".into()), 3, false)))
        .unwrap();
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::RemoteReconnected {
            machine: "fixture".into(),
            history_gap: false
        }
    );
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::TextDelta("b".into())
    );
    drop(frames);
    assert!(matches!(
        recv(&mut running.events).await,
        HarnessEvent::RemoteDisconnected { .. }
    ));
    let Call::Attach { open, frames, .. } = recv(&mut running.calls).await else {
        panic!("reattach")
    };
    assert_eq!(
        open,
        ClaudeChatOpen::Chat {
            chat_id: "fixture-chat".into(),
            after_seq: Some(3)
        }
    );
    frames.send(Ok(opened(20, true))).unwrap();
    frames.send(Ok(event(permission(), 1, true))).unwrap();
    frames
        .send(Ok(event(HarnessEvent::TextDelta("old".into()), 10, false)))
        .unwrap();
    frames
        .send(Ok(event(HarnessEvent::TextDelta("c".into()), 21, false)))
        .unwrap();
    frames
        .send(Ok(event(
            HarnessEvent::ProcessExited { code: Some(0) },
            22,
            false,
        )))
        .unwrap();
    drop(frames);
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::RemoteReconnected {
            machine: "fixture".into(),
            history_gap: true
        }
    );
    assert_eq!(recv(&mut running.events).await, permission());
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::TextDelta("c".into())
    );
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    finished(running.task).await;
    assert!(running.events.try_recv().is_err());
}

#[tokio::test]
async fn remote_chat_close_sends_shutdown_but_quit_only_detaches() {
    for close in [false, true] {
        let mut running = run(true, config(), Some("fixture-chat"));
        let Call::Attach {
            mut commands,
            frames,
            ..
        } = recv(&mut running.calls).await
        else {
            panic!("attach")
        };
        let mut session = Session::from_port(running.commands, false);
        if close {
            session.send(HarnessCommand::Shutdown).unwrap();
        }
        session.request_shutdown();
        drop(session);
        // Even a close/quit before Opened must drain commands before detaching.
        frames.send(Ok(opened(0, false))).unwrap();
        if close {
            assert_eq!(
                recv(&mut commands).await,
                serde_json::to_string(&HarnessCommand::Shutdown).unwrap()
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), commands.recv())
                .await
                .unwrap()
                .is_none()
        );
        drop(frames);
        finished(running.task).await;
        assert!(running.calls.try_recv().is_err());
    }
}

#[tokio::test]
async fn remote_chat_old_agent_uses_legacy_stream_and_refuses_attach() {
    let mut running = run(false, config(), None);
    let Call::Legacy {
        start,
        mut commands,
        events,
    } = recv(&mut running.calls).await
    else {
        panic!("legacy")
    };
    assert!(serde_json::from_str::<RemoteClaudeChatStart>(&start).is_ok());
    running.commands.send(HarnessCommand::Interrupt).unwrap();
    assert_eq!(
        recv(&mut commands).await,
        serde_json::to_string(&HarnessCommand::Interrupt).unwrap()
    );
    events
        .send(Ok(serde_json::to_string(&permission()).unwrap()))
        .unwrap();
    assert_eq!(recv(&mut running.events).await, permission());
    drop(running.commands);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), commands.recv())
            .await
            .unwrap()
            .is_none()
    );
    drop(events);
    assert!(
        matches!(recv(&mut running.events).await, HarnessEvent::Error(s) if s.contains("before Claude exited"))
    );
    assert_eq!(
        recv(&mut running.events).await,
        HarnessEvent::ProcessExited { code: None }
    );
    finished(running.task).await;
    assert!(running.calls.try_recv().is_err());

    let mut attach = run(false, config(), Some("fixture-chat"));
    assert!(
        matches!(recv(&mut attach.events).await, HarnessEvent::Error(s) if s.contains("predates claude_chat_persistent"))
    );
    assert_eq!(
        recv(&mut attach.events).await,
        HarnessEvent::ProcessExited { code: None }
    );
    finished(attach.task).await;
    assert!(
        attach.calls.try_recv().is_err(),
        "old agent must never launch on attach"
    );
}

#[tokio::test]
async fn remote_chat_close_retries_shutdown_while_quit_ends_reconnect() {
    for close in [false, true] {
        let mut running = run(true, config(), None);
        let Call::Attach { frames, .. } = recv(&mut running.calls).await else {
            panic!("attach")
        };
        frames.send(Ok(opened(0, false))).unwrap();
        frames
            .send(Ok(event(
                HarnessEvent::TextDelta("fixture".into()),
                1,
                false,
            )))
            .unwrap();
        assert_eq!(
            recv(&mut running.events).await,
            HarnessEvent::TextDelta("fixture".into())
        );
        drop(frames);
        assert!(matches!(
            recv(&mut running.events).await,
            HarnessEvent::RemoteDisconnected { .. }
        ));
        let mut session = Session::from_port(running.commands, false);
        if close {
            session.send(HarnessCommand::Shutdown).unwrap();
        }
        session.request_shutdown();
        drop(session);
        if close {
            let Call::Attach {
                open,
                mut commands,
                frames,
            } = recv(&mut running.calls).await
            else {
                panic!("reattach")
            };
            assert_eq!(
                open,
                ClaudeChatOpen::Chat {
                    chat_id: "fixture-chat".into(),
                    after_seq: Some(1)
                }
            );
            frames.send(Ok(opened(1, false))).unwrap();
            assert_eq!(
                recv(&mut commands).await,
                serde_json::to_string(&HarnessCommand::Shutdown).unwrap()
            );
            assert!(
                tokio::time::timeout(Duration::from_secs(5), commands.recv())
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(frames);
        }
        finished(running.task).await;
        assert!(running.calls.try_recv().is_err());
    }
}

fn link() -> (
    RemoteChatLink<RemoteAgentBackend>,
    mpsc::UnboundedReceiver<HarnessEvent>,
) {
    let (_commands_tx, commands) = mpsc::unbounded_channel();
    let (events, events_rx) = mpsc::unbounded_channel();
    let link = RemoteChatLink {
        backend: RemoteAgentBackend::new(
            RemoteAgentClientConfig {
                remote_id: "mini".into(),
                name: "mini".into(),
                endpoint: "http://127.0.0.1:1".into(),
                token_ref: "env:UNUSED".into(),
            },
            |_| panic!("pure test never resolves credentials"),
            "fixture-version",
        ),
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

fn opened_info(head_seq: u64, gap: bool) -> ClaudeChatOpened {
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
    assert!(link.on_opened(opened_info(5, false)));
    assert_eq!(link.on_event(&wire(&ready), 1, true), Ok(false));
    assert_eq!(link.on_event(&wire(&request), 5, true), Ok(false));
    assert_eq!(link.on_event(&wire(&delta("a")), 6, false), Ok(false));
    assert_eq!(link.on_event(&wire(&delta("a")), 6, false), Ok(false));
    assert_eq!(
        drain(&mut rx),
        vec![ready.clone(), request.clone(), delta("a")]
    );

    // Back without a gap: the tab has all of it; only the replay shows.
    assert!(link.on_opened(opened_info(7, false)));
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
    assert!(link.on_opened(opened_info(20, true)));
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

#[tokio::test]
async fn remote_chat_public_api_reports_handshake_failure_and_joins() {
    for attach in [false, true] {
        let backend = RemoteAgentBackend::new(
            RemoteAgentClientConfig {
                remote_id: "fixture".into(),
                name: "fixture".into(),
                endpoint: "http://fixture.invalid:1234".into(),
                token_ref: "fixture:unused".into(),
            },
            |_| Err(RemoteAgentClientError::new("synthetic credential failure")),
            "fixture-version",
        );
        let (mut session, mut events) = if attach {
            attach_remote(backend, config(), "fixture-chat".into())
        } else {
            spawn_remote(backend, config())
        };
        assert!(!session.supports_correlation());
        assert!(session.local_pid().is_none());
        assert_eq!(recv(&mut events).await, HarnessEvent::Error(
            "could not reach fixture (http://fixture.invalid:1234): synthetic credential failure".into()
        ));
        assert_eq!(
            recv(&mut events).await,
            HarnessEvent::ProcessExited { code: None }
        );
        session.shutdown().await.unwrap();
        assert!(session.send(HarnessCommand::Interrupt).is_err());
        session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn remote_chat_session_detaches_and_joins_supervisor_teardown() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let (commands, mut rx) = mpsc::unbounded_channel();
    let joined = Arc::new(AtomicBool::new(false));
    let done = joined.clone();
    let thread = std::thread::spawn(move || {
        assert_eq!(rx.blocking_recv(), None, "quit detaches without Shutdown");
        done.store(true, Ordering::SeqCst);
    });
    let mut session = Session::from_remote_port(commands, Some(thread));
    session.shutdown().await.unwrap();
    assert!(joined.load(Ordering::SeqCst));
    session.shutdown().await.unwrap();
}
