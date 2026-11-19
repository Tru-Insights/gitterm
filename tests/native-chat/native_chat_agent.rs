use super::*;
use serde_json::json;
#[cfg(unix)]
#[path = "native_chat_process.rs"]
mod process;
#[path = "native_chat_support.rs"]
mod support;
std::thread_local! {
    static PROGRAM: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn override_command(command: Command) -> Command {
    PROGRAM.with(|p| match p.borrow().as_ref() {
        Some(program) => {
            let mut fake = Command::new(program);
            fake.args(command.as_std().get_args());
            fake
        }
        None => command,
    })
}
fn pi_config(model: &str) -> AgentBackendConfig {
    AgentBackendConfig::Pi {
        model: model.into(),
        session_path: Some("/synthetic/session.jsonl".into()),
        thinking: Some(true),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_pi_stream_and_process_lifecycle_golden() {
    for model in ["normal", "exit7", "stop", "cancel", "detach"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stream.jsonl"),
            include_str!("../fixtures/native-chat/pi/stream.jsonl"),
        )
        .unwrap();
        let program = process::script(
            dir.path(),
            "fake-pi",
            include_str!("../fixtures/native-chat/pi/process.py"),
        );
        PROGRAM.with(|p| *p.borrow_mut() = Some(program));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let mut stop_tx = Some(stop_tx);
        let config = pi_config(model);
        let cwd = dir.path().to_path_buf();
        let run = run_turn(&config, &cwd, None, "Synthetic prompt", &event_tx, stop_rx);
        let drive = async {
            let mut values = Vec::new();
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            values.push(first.webview_payload().unwrap());
            if model == "stop" {
                stop_tx.take().unwrap().send(()).unwrap();
            } else if model == "cancel" {
                drop(stop_tx.take());
            } else if model == "detach" {
                drop(event_rx);
                std::fs::write(dir.path().join("release"), "").unwrap();
                return values;
            }
            // Retain the sender in ordinary turns: cancellation of the sender also stops Pi.
            loop {
                let e = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .webview_payload()
                    .unwrap();
                let end = e["type"] == "done" || e["type"] == "stopped";
                values.push(e);
                if end {
                    break;
                }
            }
            values
        };
        let (result, values) = tokio::join!(run, drive);
        result.unwrap();
        PROGRAM.with(|p| *p.borrow_mut() = None);
        let pid = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        process::assert_reaped(pid);
        support::golden(&format!("pi/{model}.json"), json!(values));
        support::golden(
            "pi/argv.json",
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("argv.json")).unwrap())
                .unwrap(),
        );
    }
}

#[test]
fn native_chat_session_history_and_submit_gates_golden() {
    let configs = [
        AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        },
        AgentBackendConfig::Acp {
            agent: AcpAgent::OpenCode,
            model: String::new(),
            mode: None,
            effort: None,
        },
        pi_config("synthetic-model"),
    ];
    let mut out = Vec::new();
    for config in configs {
        let mut session = AgentSession::new(config);
        session.session_id = Some("synthetic-session".into());
        let mut gates = Vec::new();
        for history in [
            HistoryLoad::NotLoaded,
            HistoryLoad::Loading,
            HistoryLoad::Loaded,
        ] {
            session.history = history;
            gates.push(json!({"history":format!("{history:?}"),"load":session.history_to_load()}));
        }
        let queue: Vec<_> = [AgentSessionState::Idle,AgentSessionState::Streaming,AgentSessionState::Stopped].iter().map(|state|json!({"state":format!("{state:?}"),"with_process":submit_queues(session.backend(),true,state),"without_process":submit_queues(session.backend(),false,state)})).collect();
        out.push(json!({"backend":session.backend(),"history":gates,"queue":queue,"mode":session.configured_permission_mode()}));
    }
    support::golden("history/resume-gates.json", json!(out));
}

#[test]
fn native_chat_iced_record_and_replay_trace_golden() {
    let mut session = AgentSession::new(AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: None,
        effort: None,
    });
    let prompt = UserPrompt {
        text: "Synthetic prompt".into(),
        images: Vec::new(),
        id: Some("prompt-1".into()),
    };
    session.record(AgentEvent::submitted_prompt(&prompt, false));
    let events: Vec<HarnessEvent> = serde_json::from_str(include_str!(
        "../fixtures/native-chat/claude/iced-events.json"
    ))
    .unwrap();
    for event in events {
        session.record(AgentEvent::Harness(event));
    }
    session.record(AgentEvent::submitted_prompt(
        &UserPrompt {
            text: "Queued synthetic prompt".into(),
            id: Some("queued-1".into()),
            images: Vec::new(),
        },
        true,
    ));
    session.record(AgentEvent::Harness(HarnessEvent::MessageLifecycle {
        id: "queued-1".into(),
        state: gitterm::harness::MessageState::Cancelled,
    }));
    let mut payloads: Vec<_> = session
        .conversation
        .iter()
        .filter_map(AgentEvent::webview_payload)
        .collect();
    for payload in &mut payloads {
        // Only wall-clock metadata varies; retain marker presence and ordering.
        if payload["kind"] == "turn_timing" {
            for key in ["start_ms", "end_ms", "split_ms"] {
                if payload.get(key).is_some() {
                    payload[key] = json!(0);
                }
            }
        }
    }
    support::golden(
        "claude/iced-replay.json",
        json!({"revision":session.revision,"shows_prompt":session.shows_prompt("prompt-1"),"shows_queue":session.shows_prompt("queued-1"),"payloads":payloads}),
    );
}

#[tokio::test]
async fn native_chat_pi_handle_receiver_answer_and_stop_limits() {
    let (input_tx, mut inputs) = mpsc::unbounded_channel();
    let (_events, events) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let handle = AgentTaskHandle {
        snapshots_closed: tokio_util::sync::CancellationToken::new(),
        input_tx,
        stop_slot: Arc::new(Mutex::new(Some(stop_tx))),
        pending_event_rx: Mutex::new(Some(events)),
    };
    assert!(handle.take_event_receiver().is_some());
    assert!(handle.take_event_receiver().is_none());
    handle.submit_prompt("Synthetic prompt".into()).unwrap();
    assert!(matches!(inputs.recv().await,Some(AgentInput::Prompt(p)) if p == "Synthetic prompt"));
    handle.request_stop();
    handle.request_stop();
    assert_eq!(stop_rx.await, Ok(()));
    assert!(handle.stop_slot.lock().unwrap().is_none());
    drop(inputs);
    assert_eq!(
        handle.submit_prompt("Another".into()).unwrap_err(),
        "agent subprocess manager has exited"
    );
    // Pi has only Prompt input: no mid-turn Answer or Shutdown protocol.
    assert!(build_command(
        &AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None
        },
        "hi",
        None
    )
    .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_pi_startup_failure() {
    let dir = tempfile::tempdir().unwrap();
    PROGRAM.with(|p| *p.borrow_mut() = Some(dir.path().join("absent-fake")));
    let (events, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = oneshot::channel();
    let error = run_turn(
        &pi_config("synthetic-model"),
        &dir.path().into(),
        None,
        "Synthetic prompt",
        &events,
        stop_rx,
    )
    .await
    .unwrap_err();
    PROGRAM.with(|p| *p.borrow_mut() = None);
    assert!(error.starts_with("failed to spawn agent subprocess:"));
    assert!(rx.try_recv().is_err()); // agent_loop, not run_turn, turns the error into JSON.
}

#[test]
fn native_chat_iced_update_trace() {
    const NAME: &str = "tab::agent::native_chat_tests::native_chat_iced_update_trace";
    // Config helpers have process-global state. Follow the existing baseline's
    // child-test pattern so picker persistence can only reach a temporary root.
    if std::env::var("GITTERM_V6_CHATC0_CHILD").as_deref() != Ok(NAME) {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(crate::config::CONFIG_DIR_ENV, dir.path())
            .env("HOME", dir.path().join("home"))
            .env("GITTERM_V6_CHATC0_CHILD", NAME)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    fn capture(app: &crate::App, label: &str) -> serde_json::Value {
        let session = app.workspaces[0].tabs[0].agent_session().unwrap();
        let mut payloads: Vec<_> = session
            .conversation
            .iter()
            .filter_map(AgentEvent::webview_payload)
            .collect();
        for payload in &mut payloads {
            if payload["kind"] == "turn_timing" {
                for key in ["start_ms", "end_ms", "split_ms"] {
                    if payload.get(key).is_some() {
                        payload[key] = json!(0);
                    }
                }
            }
        }
        json!({"step":label,"state":format!("{:?}",session.state),"config":session.config,"history":format!("{:?}",session.history),"pending":session.pending_requests,"revision":session.revision,"payloads":payloads})
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = crate::baseline_tests::app_with_tabs(dir.path());
    let (input_tx, mut inputs) = mpsc::unbounded_channel();
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let (_events, events) = mpsc::unbounded_channel();
    let mut session = AgentSession::new(pi_config("synthetic-model"));
    session.task_handle = Some(AgentTaskHandle {
        snapshots_closed: tokio_util::sync::CancellationToken::new(),
        input_tx,
        stop_slot: Arc::new(Mutex::new(Some(stop_tx))),
        pending_event_rx: Mutex::new(Some(events)),
    });
    app.workspaces[0].tabs[0].kind = crate::tab::TabKind::Agent(session);
    let mut trace = Vec::new();
    let _ = app.update_event(crate::Event::AgentSubmitPrompt(
        10,
        UserPrompt {
            text: "Synthetic submit".into(),
            images: Vec::new(),
            id: Some("prompt-1".into()),
        },
    ));
    assert!(matches!(inputs.try_recv(),Ok(AgentInput::Prompt(p)) if p == "Synthetic submit"));
    trace.push(capture(&app, "submit-pi"));
    let _ = app.update_event(crate::Event::AgentStopRequested(10));
    assert_eq!(stop_rx.try_recv(), Ok(()));
    trace.push(capture(&app, "stop-pi"));
    let s = app.workspaces[0].tabs[0].agent_session_mut().unwrap();
    s.config = AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: None,
        effort: None,
    };
    s.task_handle = None;
    s.history = HistoryLoad::Loading;
    s.session_id = Some("synthetic-chat".into());
    let sequence = [
        HarnessEvent::TurnStarted {
            session_id: Some("synthetic-chat".into()),
            model: Some("synthetic-model".into()),
        },
        HarnessEvent::RuntimeRequest {
            request_id: "p1".into(),
            tool_use_id: "tool-1".into(),
            kind: gitterm::harness::RuntimeRequestKind::Permission {
                tool_name: "Bash".into(),
                input: json!({"command":"true"}),
                suggestions: Vec::new(),
                decision_reason: None,
            },
        },
        HarnessEvent::RuntimeRequest {
            request_id: "p2".into(),
            tool_use_id: "tool-2".into(),
            kind: gitterm::harness::RuntimeRequestKind::Question {
                questions: json!([{"question":"Pick a shape"}]),
            },
        },
    ];
    for event in sequence {
        let _ = app.update_event(crate::Event::AgentEventReceived(
            10,
            AgentEvent::Harness(event),
        ));
    }
    trace.push(capture(&app, "pending-requests"));
    let _ = app.update_event(crate::Event::AgentAnswerRequest(
        10,
        "p1".into(),
        gitterm::harness::RuntimeDecision::Deny {
            message: "Synthetic denial".into(),
        },
    ));
    // No harness: an answer attempt does not optimistically close the card.
    trace.push(capture(&app, "answer-without-process"));
    let _ = app.update_event(crate::Event::AgentEventReceived(
        10,
        AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved {
            request_id: "p1".into(),
        }),
    ));
    trace.push(capture(&app, "one-request-resolved"));
    let history = gitterm::harness::transcript::parse_claude_transcript(include_str!(
        "../fixtures/native-chat/history/claude.jsonl"
    ))
    .into_iter()
    .map(AgentEvent::from)
    .collect();
    let _ = app.update_event(crate::Event::AgentHistoryLoaded(10, history));
    trace.push(capture(&app, "history-before-live"));
    let _ = app.update_event(crate::Event::AgentEventReceived(
        10,
        AgentEvent::Harness(HarnessEvent::HistoryReplayed {
            entries: vec![gitterm::harness::transcript::TranscriptEntry::UserPrompt(
                "Repeated replay must be dropped".into(),
            )],
        }),
    ));
    trace.push(capture(&app, "repeated-acp-replay"));
    for event in [
        HarnessEvent::ModelChanged("synthetic-model".into()),
        HarnessEvent::PermissionModeChanged("plan".into()),
        HarnessEvent::EffortChanged {
            effort: Some("high".into()),
            applied: Some("high".into()),
        },
    ] {
        let _ = app.update_event(crate::Event::AgentEventReceived(
            10,
            AgentEvent::Harness(event),
        ));
    }
    trace.push(capture(&app, "confirmed-pickers"));
    let _ = app.update_event(crate::Event::AgentEventReceived(
        10,
        AgentEvent::Harness(HarnessEvent::ProcessExited { code: Some(7) }),
    ));
    trace.push(capture(&app, "process-exit-with-pending"));
    let _ = app.update_event(crate::Event::AgentEventReceived(
        10,
        AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved {
            request_id: "p2".into(),
        }),
    ));
    trace.push(capture(&app, "last-request-resolved"));
    let _ = app.update_event(crate::Event::TabClose(0));
    trace.push(json!({"step":"close-last-tab","tabs_remaining":app.workspaces[0].tabs.len()}));
    app.workspaces[0]
        .tabs
        .push(crate::TabState::new(11, dir.path().to_path_buf()));
    let _ = app.update_event(crate::Event::TabClose(0));
    trace.push(json!({"step":"close-with-sibling","tabs_remaining":app.workspaces[0].tabs.len(),"remaining_id":app.workspaces[0].tabs[0].id}));
    support::golden("claude/iced-update.json", json!(trace));
}
