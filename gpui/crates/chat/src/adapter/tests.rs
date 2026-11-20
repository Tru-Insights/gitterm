use super::*;
use crate::{events::*, model::ChatModel};
use serde_json::json;

fn target() -> session::Target {
    session::Target {
        tab_id: 7,
        session_uid: "synthetic-chat".into(),
        task_id: None,
        task_session_id: None,
    }
}
fn facts() -> Admission {
    Admission {
        authority: session::State {
            target: target(),
            control: session::TurnControl::Harness,
            task_live: false,
            last_tab: false,
        },
        backend: AgentBackend::Claude,
        process_running: true,
        just_spawned: false,
        message_id: "synthetic-message".into(),
    }
}
fn prompt() -> ChatIntent {
    ChatIntent::Send {
        text: "synthetic draft".into(),
        images: vec![ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        }],
    }
}
fn permission() -> ChatIntent {
    ChatIntent::Permission {
        request_id: "synthetic-request".into(),
        decision: PermissionDecision::Allow,
    }
}
fn request(adapter: &mut ChatAdapter, stamp: &RuntimeStamp) {
    adapter
        .receive(
            stamp,
            &target(),
            AgentEvent::Harness(HarnessEvent::RuntimeRequest {
                request_id: "synthetic-request".into(),
                tool_use_id: "synthetic-tool".into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: json!({}),
                    suggestions: vec![],
                    decision_reason: None,
                },
            }),
            Some(1),
            |_, _| {},
        )
        .unwrap();
}
fn accepted(adapter: &mut ChatAdapter, prepared: Prepared) -> Receipt {
    adapter.execute(prepared, &target(), 1000, |_| Ok(())).0
}
#[test]
fn stale_stream_dropped_before_every_observer_and_effect() {
    let mut adapter = ChatAdapter::default();
    let stale = adapter.begin_runtime(target());
    let current = adapter.begin_runtime(target());
    assert!(
        adapter
            .receive(
                &stale,
                &target(),
                AgentEvent::Harness(HarnessEvent::TurnStarted {
                    session_id: Some("stale-session".into()),
                    model: None
                }),
                Some(1),
                |_, _| panic!("stale observer")
            )
            .is_none()
    );
    assert!(adapter.state().conversation.is_empty());
    assert_eq!(adapter.state().revision, 0);
    assert!(adapter.state().session_id.is_none());
    assert!(matches!(adapter.state().state, AgentSessionState::Idle));
    let mut rebound = target();
    rebound.task_id = Some("synthetic-task".into());
    assert!(
        adapter
            .receive(
                &current,
                &rebound,
                AgentEvent::Harness(HarnessEvent::TextDelta("current".into())),
                Some(1),
                |_, _| {}
            )
            .is_some()
    );
}
#[test]
fn prepared_prompt_is_sent_and_echo_recorded_once_with_host_clock() {
    let mut adapter = ChatAdapter::default();
    adapter.begin_runtime(target());
    let command = adapter
        .admit(prompt(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let (receipt, update) = adapter.execute(command, &target(), 1234, |command| {
        let HarnessCommand::SendUserMessage(sent) = command else {
            panic!()
        };
        assert_eq!(sent.id.as_deref(), Some("synthetic-message"));
        assert_eq!(sent.images.len(), 1);
        Ok(())
    });
    assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    assert!(receipt.state.label().contains("Sending to harness"));
    assert!(matches!(update.presentation, Some(Presentation::Append(_))));
    assert!(adapter.state().shows_prompt("synthetic-message"));
    assert_eq!(adapter.state().conversation.len(), 2);
    assert!(
        matches!(&adapter.state().conversation[1], AgentEvent::Other(v) if v["start_ms"] == 1234)
    );
    assert!(
        adapter
            .delivery(receipt.id, &target(), DeliveryOutcome::Written)
            .is_some()
    );
    assert!(
        adapter
            .delivery(receipt.id, &target(), DeliveryOutcome::Written)
            .is_none()
    );
    assert_eq!(adapter.state().revision, 1);
}
#[test]
fn channel_rejection_keeps_prompt_bytes_and_records_no_echo() {
    let mut adapter = ChatAdapter::default();
    adapter.begin_runtime(target());
    let original = prompt();
    let command = adapter
        .admit(original.clone(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let receipt = adapter
        .execute(command, &target(), 10, |_| Err("closed channel".into()))
        .0;
    assert!(receipt.state.retryable());
    assert!(adapter.state().conversation.is_empty());
    assert!(adapter.admit(original, facts()).is_ok());
}
#[test]
fn definitive_rejection_retries_but_uncertainty_and_resolution_never_do() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    let first = adapter
        .admit(permission(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let first = accepted(&mut adapter, first);
    assert!(
        adapter
            .delivery(
                first.id,
                &target(),
                DeliveryOutcome::Rejected("zero bytes written".into())
            )
            .unwrap()
            .state
            .retryable()
    );
    let retry = adapter
        .admit(permission(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let retry = accepted(&mut adapter, retry);
    let uncertain = adapter
        .delivery(
            retry.id,
            &target(),
            DeliveryOutcome::Uncertain("partial write".into()),
        )
        .unwrap();
    assert!(!uncertain.state.retryable());
    assert!(uncertain.state.label().contains("Uncertain"));
    assert!(adapter.admit(permission(), facts()).is_err());
    assert!(
        adapter
            .delivery(
                retry.id,
                &target(),
                DeliveryOutcome::Rejected("late".into())
            )
            .is_none()
    );
    adapter
        .receive(
            &stamp,
            &target(),
            AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved {
                request_id: "synthetic-request".into(),
            }),
            Some(20),
            |_, _| {},
        )
        .unwrap();
    assert_eq!(
        adapter.receipt(retry.id).unwrap().state,
        ReceiptState::Resolved
    );
    assert!(
        adapter
            .delivery(
                retry.id,
                &target(),
                DeliveryOutcome::Rejected("late".into())
            )
            .is_none()
    );
    assert!(adapter.admit(permission(), facts()).is_err());
    assert!(
        ChatModel::from_conversation(&adapter.state().conversation).rows[0]
            .title
            .contains("decision not recorded")
    );
}
#[test]
fn uncertain_answer_stays_blocked_across_task_rebindings() {
    for outcome in [
        Some(DeliveryOutcome::Uncertain("partial write".into())),
        None,
        Some(DeliveryOutcome::Written),
        Some(DeliveryOutcome::Rejected("zero bytes written".into())),
    ] {
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        request(&mut adapter, &stamp);
        let prepared = adapter
            .admit(permission(), facts())
            .unwrap_or_else(|r| panic!("{r:?}"));
        let mut receipt = accepted(&mut adapter, prepared);
        if let Some(outcome) = outcome {
            receipt = adapter.delivery(receipt.id, &target(), outcome).unwrap();
        }
        let mut rebound_facts = facts();
        rebound_facts.authority.target.task_id = Some("synthetic-task".into());
        rebound_facts.authority.target.task_session_id = Some("synthetic-task-session".into());
        assert!(
            adapter
                .state()
                .accepts_stream(&stamp, &rebound_facts.authority.target)
        );
        let result = adapter.admit(permission(), rebound_facts);
        if receipt.state.retryable() {
            assert!(result.is_ok(), "known non-delivery must permit retry");
        } else {
            assert_eq!(result.err(), Some(receipt));
        }
    }
}

#[test]
fn receiver_echo_does_not_record_the_local_submission_twice() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    let prepared = adapter
        .admit(prompt(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    accepted(&mut adapter, prepared);
    let conversation_len = adapter.state().conversation.len();
    let revision = adapter.state().revision;
    for _ in 0..2 {
        let update = adapter
            .receive(
                &stamp,
                &target(),
                AgentEvent::Harness(HarnessEvent::UserMessageEcho {
                    id: Some("synthetic-message".into()),
                    text: "synthetic draft".into(),
                    image_count: 1,
                    delivery: None,
                }),
                Some(2000),
                |_, _| {},
            )
            .unwrap();
        assert_eq!(adapter.state().conversation.len(), conversation_len);
        assert_eq!(adapter.state().revision, revision);
        assert!(update.presentation.is_none());
    }
    assert_eq!(revision, 1);
}

#[test]
fn deferred_commands_revalidate_full_binding_and_pending_request() {
    for intent in [prompt(), permission(), ChatIntent::Stop] {
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        request(&mut adapter, &stamp);
        let prepared = adapter
            .admit(intent, facts())
            .unwrap_or_else(|r| panic!("{r:?}"));
        let mut rebound = target();
        rebound.task_session_id = Some("new-binding".into());
        let receipt = adapter
            .execute(prepared, &rebound, 10, |_| panic!("stale send"))
            .0;
        assert!(receipt.state.retryable());
    }
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    let prepared = adapter
        .admit(permission(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    adapter
        .receive(
            &stamp,
            &target(),
            AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved {
                request_id: "synthetic-request".into(),
            }),
            None,
            |_, _| {},
        )
        .unwrap();
    assert_eq!(
        adapter
            .execute(prepared, &target(), 20, |_| panic!("resolved send"))
            .0
            .state,
        ReceiptState::Resolved
    );
}
#[test]
fn wrong_kind_and_remember_are_rejected_before_dispatch() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    for intent in [
        ChatIntent::DismissQuestion {
            request_id: "synthetic-request".into(),
        },
        ChatIntent::AnswerQuestion {
            request_id: "synthetic-request".into(),
            answers: vec![],
        },
        ChatIntent::Permission {
            request_id: "synthetic-request".into(),
            decision: PermissionDecision::AllowAlways {
                suggestion: json!({"destination":"localSettings"}),
            },
        },
    ] {
        assert!(adapter.admit(intent, facts()).is_err());
    }
}
fn config() -> AgentBackendConfig {
    AgentBackendConfig::Claude {
        model: String::new(),
        permission_mode: None,
        effort: None,
    }
}
#[test]
fn history_source_validates_both_results_and_replay_ignores_revision() {
    let mut adapter = ChatAdapter::default();
    adapter.bind_session(Some("history-a".into()));
    let (stale, _) = adapter.begin_history(target(), config()).unwrap();
    adapter.bind_session(Some("history-b".into()));
    let (ticket, _) = adapter.begin_history(target(), config()).unwrap();
    assert!(
        adapter
            .complete_history(
                &stale,
                &target(),
                AgentBackend::Claude,
                Err("stale failure".into())
            )
            .unwrap()
            .is_none()
    );
    assert!(
        adapter
            .complete_history(
                &stale,
                &target(),
                AgentBackend::Claude,
                Ok(vec![AgentEvent::user_prompt("wrong history")])
            )
            .unwrap()
            .is_none()
    );
    let revision = adapter.state().revision;
    let update = adapter
        .complete_history(
            &ticket,
            &target(),
            AgentBackend::Claude,
            Ok(vec![AgentEvent::user_prompt("earlier prompt")]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(adapter.state().revision, revision);
    assert!(matches!(update.presentation, Some(Presentation::Replay)));
    assert_eq!(
        ChatModel::from_conversation(&adapter.state().conversation)
            .rows
            .len(),
        1
    );
    assert!(
        adapter
            .complete_history(
                &ticket,
                &target(),
                AgentBackend::Claude,
                Ok(vec![AgentEvent::user_prompt("duplicate")])
            )
            .unwrap()
            .is_none()
    );
    let mut adapter = ChatAdapter::default();
    adapter.bind_session(Some("history".into()));
    let (ticket, _) = adapter.begin_history(target(), config()).unwrap();
    assert_eq!(
        adapter
            .complete_history(
                &ticket,
                &target(),
                AgentBackend::Claude,
                Err("read failed".into())
            )
            .err()
            .as_deref(),
        Some("read failed")
    );
}
#[test]
fn c0_canonical_envelopes_replay_without_losing_events() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/fixtures/native-chat");
    for fixture in ["claude/parser.json", "acp/parser.json"] {
        let values: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(root.join(fixture)).unwrap()).unwrap();
        let events: Vec<HarnessEvent> = values
            .iter()
            .filter_map(|v| {
                if fixture.starts_with("acp/") {
                    Some(v)
                } else {
                    v.get("event")
                }
            })
            .map(|v| serde_json::from_value(v.clone()).unwrap())
            .collect();
        assert!(!events.is_empty());
        let records: Vec<_> = events
            .iter()
            .map(|e| Record {
                at_ms: None,
                entry: Entry::Harness(e.clone()),
            })
            .collect();
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        let mut incremental = ChatModel::default();
        for event in events {
            let update = adapter
                .receive(
                    &stamp,
                    &target(),
                    AgentEvent::Harness(event),
                    None,
                    |_, _| {},
                )
                .unwrap();
            let Some(Presentation::Append(events)) = update.presentation else {
                panic!("missing record effect")
            };
            for event in events {
                incremental.apply_agent(&event);
            }
        }
        let legacy = ChatModel::from_records(&records);
        let projection = ChatModel::from_conversation(&adapter.state().conversation);
        assert_eq!(
            incremental
                .rows
                .iter()
                .map(|r| (&r.title, &r.body))
                .collect::<Vec<_>>(),
            projection
                .rows
                .iter()
                .map(|r| (&r.title, &r.body))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            legacy
                .rows
                .iter()
                .map(|r| (&r.title, &r.body))
                .collect::<Vec<_>>(),
            projection
                .rows
                .iter()
                .map(|r| (&r.title, &r.body))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn reset_cannot_retarget_a_previous_request_and_old_receipts_do_nothing() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    let prepared = adapter
        .admit(permission(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let receipt = accepted(&mut adapter, prepared);
    adapter
        .receive(
            &stamp,
            &target(),
            AgentEvent::Harness(HarnessEvent::ConversationReset { trigger: None }),
            None,
            |_, _| {},
        )
        .unwrap();
    assert!(adapter.admit(permission(), facts()).is_err());
    adapter.begin_runtime(target());
    assert!(
        adapter
            .delivery(
                receipt.id,
                &target(),
                DeliveryOutcome::Rejected("stale".into())
            )
            .is_none()
    );
}

#[test]
fn retry_with_the_same_message_id_does_not_duplicate_the_echo() {
    let mut adapter = ChatAdapter::default();
    adapter.begin_runtime(target());
    let prepared = adapter
        .admit(prompt(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let receipt = accepted(&mut adapter, prepared);
    adapter
        .delivery(
            receipt.id,
            &target(),
            DeliveryOutcome::Rejected("not written".into()),
        )
        .unwrap();
    let revision = adapter.state().revision;
    let prepared = adapter
        .admit(prompt(), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    accepted(&mut adapter, prepared);
    assert_eq!(adapter.state().revision, revision);
    assert_eq!(
        adapter
            .state()
            .conversation
            .iter()
            .filter(|event| event.prompt_id() == Some("synthetic-message"))
            .count(),
        1
    );
}

#[test]
fn process_exit_emits_uncertain_receipts_but_not_for_completed_messages() {
    for completed in [false, true] {
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        let prepared = adapter
            .admit(prompt(), facts())
            .unwrap_or_else(|r| panic!("{r:?}"));
        let receipt = accepted(&mut adapter, prepared);
        if completed {
            let update = adapter
                .receive(
                    &stamp,
                    &target(),
                    AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                        id: "synthetic-message".into(),
                        state: MessageState::Completed,
                    }),
                    None,
                    |_, _| {},
                )
                .unwrap();
            assert_eq!(update.receipts[0].state, ReceiptState::Resolved);
        }
        let update = adapter
            .receive(
                &stamp,
                &target(),
                AgentEvent::Harness(HarnessEvent::ProcessExited { code: Some(1) }),
                None,
                |_, _| {},
            )
            .unwrap();
        if completed {
            assert!(update.receipts.is_empty());
        } else {
            assert!(matches!(
                update.receipts[0].state,
                ReceiptState::Uncertain(_)
            ));
        }
        assert!(!adapter.receipt(receipt.id).unwrap().state.retryable());
    }
}

#[test]
fn native_adapter_refuses_pi_instead_of_ignoring_image_limitations() {
    let mut adapter = ChatAdapter::default();
    adapter.begin_runtime(target());
    let mut admission = facts();
    admission.backend = AgentBackend::Pi;
    assert!(adapter.admit(prompt(), admission).is_err());
    assert!(adapter.state().conversation.is_empty());
}
