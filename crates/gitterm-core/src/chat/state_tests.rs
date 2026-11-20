use super::*;
use serde_json::json;

fn target() -> session::Target {
    session::Target {
        tab_id: 10,
        session_uid: "synthetic-caller".into(),
        task_id: None,
        task_session_id: None,
    }
}
fn config() -> AgentBackendConfig {
    AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: None,
        effort: None,
    }
}
fn request(id: &str, question: bool) -> AgentEvent {
    AgentEvent::Harness(HarnessEvent::RuntimeRequest {
        request_id: id.into(),
        tool_use_id: format!("tool-{id}"),
        kind: if question {
            RuntimeRequestKind::Question {
                questions: json!([]),
            }
        } else {
            RuntimeRequestKind::Permission {
                tool_name: "Bash".into(),
                input: json!({"command":"true"}),
                suggestions: vec![],
                decision_reason: None,
            }
        },
    })
}
fn load(state: &mut State) -> HistoryTicket {
    assert!(
        matches!(state.reduce(Intent::BeginHistory(config())), Effect::LoadHistory(id) if id == "synthetic-session")
    );
    state
        .history_ticket(target(), AgentBackend::Claude)
        .unwrap()
}
fn answer(state: &mut State, runtime: RuntimeStamp, id: &str) -> Effect {
    state.reduce(Intent::Answer {
        runtime,
        current: target(),
        request_id: id.into(),
        decision: RuntimeDecision::Deny {
            message: "Synthetic denial".into(),
        },
        process_running: true,
    })
}

#[test]
fn submit_echo_ids_queue_and_channel_acceptance_match_iced() {
    let mut state = State::default();
    for (running, spawned, queued) in [
        (false, true, false),
        (true, false, true),
        (true, true, false),
    ] {
        state.reduce(Intent::SubmissionAccepted);
        let Effect::Submit(prepared) = state.reduce(Intent::Submit {
            prompt: "Synthetic prompt".into(),
            backend: AgentBackend::Claude,
            process_running: running,
            just_spawned: spawned,
            message_id: "prompt-1".into(),
        }) else {
            panic!("missing submission")
        };
        assert_eq!(prepared.prompt.id.as_deref(), Some("prompt-1"));
        assert_eq!(prepared.echo.prompt_id(), Some("prompt-1"));
        assert_eq!(
            prepared
                .echo
                .webview_payload()
                .unwrap()
                .get("queued")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            queued
        );
    }
    let Effect::Submit(prepared) = state.reduce(Intent::Submit {
        prompt: UserPrompt {
            text: "Synthetic image".into(),
            images: vec![ImageAttachment {
                media_type: "image/png".into(),
                data: "AA==".into(),
            }],
            id: Some("retain-id".into()),
        },
        backend: AgentBackend::Pi,
        process_running: true,
        just_spawned: false,
        message_id: String::new(),
    }) else {
        panic!("missing submission")
    };
    assert_eq!(prepared.images_not_sent, 1);
    assert_eq!(
        prepared.echo.webview_payload().unwrap(),
        history::user_prompt_payload("Synthetic image")
    );
    assert!(state.conversation.is_empty()); // Preparation never claims delivery or records twice.
}

#[test]
fn answer_keeps_order_and_waits_for_authoritative_resolution() {
    let mut state = State::default();
    for event in [request("permission-1", false), request("question-1", true)] {
        state.reduce(Intent::Event(event));
    }
    let stamp = state.runtime_stamp(target());
    for id in ["permission-1", "question-1"] {
        assert!(
            matches!(answer(&mut state, stamp.clone(), id), Effect::Send { runtime, command: HarnessCommand::Answer { request_id, .. } } if runtime == stamp && request_id == id)
        );
        assert_eq!(state.pending_requests, ["permission-1", "question-1"]);
    }
    assert!(matches!(
        answer(&mut state, stamp.clone(), "unknown"),
        Effect::Rejected(_)
    ));
    state.reduce(Intent::Event(AgentEvent::Harness(
        HarnessEvent::RuntimeRequestResolved {
            request_id: "permission-1".into(),
        },
    )));
    assert!(matches!(
        answer(&mut state, stamp.clone(), "permission-1"),
        Effect::Rejected(_)
    ));
    state.begin_runtime();
    assert!(state.pending_requests.is_empty());
    state.reduce(Intent::Event(request("question-1", true)));
    assert!(matches!(
        answer(&mut state, stamp, "question-1"),
        Effect::Rejected(_)
    ));
    let current = state.runtime_stamp(target());
    assert!(matches!(
        answer(&mut state, current, "question-1"),
        Effect::Send { .. }
    ));
}

#[test]
fn runtime_stamps_reject_replacements_and_every_session_binding() {
    let mut state = State::default();
    let stamp = state.runtime_stamp(target());
    assert!(state.accepts_runtime(&stamp, &target()));
    for field in 0..4 {
        let mut changed = target();
        match field {
            0 => changed.tab_id += 1,
            1 => changed.session_uid = "other".into(),
            2 => changed.task_id = Some("other".into()),
            _ => changed.task_session_id = Some("other".into()),
        }
        assert!(!state.accepts_runtime(&stamp, &changed));
    }
    state.begin_runtime();
    assert!(!state.accepts_runtime(&stamp, &target()));
    let replacement = State::default();
    assert!(!replacement.accepts_runtime(&state.runtime_stamp(target()), &target()));
}

#[test]
fn history_ticket_survives_initial_attach_and_same_receiver_gap_reset() {
    let mut state = State::default();
    state.reduce(Intent::BindSession(Some("synthetic-session".into())));
    let initial = load(&mut state);
    state.begin_runtime(); // Remote attach installs its receiver after starting history.
    let stamp = state.runtime_stamp(target());
    assert!(state.accepts_history(&initial, &target(), AgentBackend::Claude));
    state.reduce(Intent::ResetHistory);
    let gap = load(&mut state);
    assert!(!state.accepts_history(&initial, &target(), AgentBackend::Claude));
    assert!(state.accepts_runtime(&stamp, &target()));
    assert!(state.accepts_history(&gap, &target(), AgentBackend::Claude));
    state.reduce(Intent::Record {
        event: AgentEvent::user_prompt("Repeated synthetic prompt"),
        now_ms: Some(20),
    });
    let Effect::ReplayHistory = state.reduce(Intent::HistoryLoaded(vec![AgentEvent::user_prompt(
        "Repeated synthetic prompt",
    )])) else {
        panic!("missing history replay")
    };
    // ID-less history has no overlap boundary: preserve both legitimate repeats.
    assert_eq!(
        state
            .conversation
            .iter()
            .filter(|e| e
                .webview_payload()
                .is_some_and(|v| v["type"] == "user_prompt"))
            .count(),
        2
    );
    assert!(!state.accepts_history(&gap, &target(), AgentBackend::Claude));
}

#[test]
fn history_rejects_source_changes_failures_and_deferred_acp_replays() {
    let mut state = State::default();
    state.reduce(Intent::BindSession(Some("synthetic-session".into())));
    let old = load(&mut state);
    state.reduce(Intent::Event(AgentEvent::Harness(
        HarnessEvent::TurnStarted {
            session_id: Some("replacement-session".into()),
            model: None,
        },
    )));
    assert_eq!(state.history, HistoryLoad::NotLoaded);
    assert!(!state.accepts_history(&old, &target(), AgentBackend::Claude));
    state.reduce(Intent::BindSession(Some("synthetic-session".into())));
    let ticket = load(&mut state);
    assert!(!state.accepts_history(&ticket, &target(), AgentBackend::Acp(AcpAgent::OpenCode)));
    assert!(!State::default().accepts_history(&ticket, &target(), AgentBackend::Claude));
    assert!(matches!(
        state.reduce(Intent::HistoryUnavailable("Synthetic read failure".into())),
        Effect::Rejected(_)
    ));
    assert_eq!(state.history, HistoryLoad::Loaded);
    assert!(!state.accepts_history(&ticket, &target(), AgentBackend::Claude));
    state.reduce(Intent::ResetHistory);
    let acp = AgentBackend::Acp(AcpAgent::OpenCode);
    let cfg = AgentBackendConfig::Acp {
        agent: AcpAgent::OpenCode,
        model: String::new(),
        mode: None,
        effort: None,
    };
    assert!(matches!(
        state.reduce(Intent::BeginHistory(cfg)),
        Effect::LoadHistory(_)
    ));
    let replay = state.history_ticket(target(), acp).unwrap();
    assert!(state.accepts_history(&replay, &target(), acp));
    state.begin_runtime();
    assert!(!state.accepts_history(&replay, &target(), acp));
}

#[test]
fn stop_keeps_harness_turn_live_and_legacy_stop_immediate() {
    let mut state = State::default();
    state.reduce(Intent::SubmissionAccepted);
    let runtime = state.runtime_stamp(target());
    let authority = |control| session::State {
        target: target(),
        control,
        task_live: false,
        last_tab: false,
    };
    assert!(matches!(
        state.reduce(Intent::Stop {
            runtime: runtime.clone(),
            authority: authority(session::TurnControl::Harness),
            target: target()
        }),
        Effect::Send {
            command: HarnessCommand::Interrupt,
            ..
        }
    ));
    assert!(matches!(state.state, AgentSessionState::Streaming));
    state.begin_runtime();
    assert!(matches!(
        state.reduce(Intent::Stop {
            runtime,
            authority: authority(session::TurnControl::TaskHandle),
            target: target()
        }),
        Effect::None
    ));
    assert!(matches!(state.state, AgentSessionState::Streaming));
    assert!(matches!(
        state.reduce(Intent::Stop {
            runtime: state.runtime_stamp(target()),
            authority: authority(session::TurnControl::TaskHandle),
            target: target()
        }),
        Effect::CancelLegacyTurn
    ));
    assert!(matches!(state.state, AgentSessionState::Stopped));
}

#[cfg(feature = "runtime")]
#[test]
fn local_drop_shuts_down_and_remote_drop_only_detaches() {
    for local in [true, false] {
        let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let port = super::super::runtime::Session::from_port(commands, local);
        drop(port);
        if local {
            assert!(matches!(receiver.try_recv(), Ok(HarnessCommand::Shutdown)));
        } else {
            assert!(receiver.try_recv().is_err());
        }
    }
}

#[test]
fn remote_echo_deduplicates_ids_and_never_collapses_equal_text() {
    let mut state = State::default();
    for id in ["own-prompt", "phone-prompt", "phone-prompt"] {
        let effect = state.reduce(Intent::RelayedPrompt {
            id: Some(id.into()),
            text: "Same synthetic text".into(),
            image_count: 2,
            backend: AgentBackend::Claude,
            process_running: true,
        });
        if let Effect::RelayedPrompt { echo, running } = effect {
            assert!(running);
            state.reduce(Intent::Record {
                event: echo,
                now_ms: Some(10),
            });
        } else {
            assert_eq!(id, "phone-prompt");
        }
    }
    assert_eq!(
        state
            .conversation
            .iter()
            .filter(|e| e.prompt_id().is_some())
            .count(),
        2
    );
    assert!(matches!(
        state.reduce(Intent::Submit {
            prompt: "Synthetic prompt".into(),
            backend: AgentBackend::Acp(AcpAgent::Gemini),
            process_running: false,
            just_spawned: false,
            message_id: "prompt-1".into()
        }),
        Effect::Rejected(_)
    ));
}
