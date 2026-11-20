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
        config: gitterm_core::chat::types::AgentBackendConfig::Claude {
            model: String::new(),
            permission_mode: Some("default".into()),
            effort: None,
        },
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
fn bound(adapter: &ChatAdapter, intent: ChatIntent) -> ChatIntent {
    if let Some(id) = intent.request_id()
        && let Some(identity) =
            adapter
                .state()
                .request_identity(&adapter.state().runtime_stamp(target()), &[], id)
    {
        return ChatIntent::Answer {
            identity,
            answer: Box::new(intent),
        };
    }
    intent
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
        .admit(bound(&adapter, permission()), facts())
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
        .admit(bound(&adapter, permission()), facts())
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
    assert!(
        adapter
            .admit(bound(&adapter, permission()), facts())
            .is_err()
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
    assert!(
        adapter
            .admit(bound(&adapter, permission()), facts())
            .is_err()
    );
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
            .admit(bound(&adapter, permission()), facts())
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
        let result = adapter.admit(bound(&adapter, permission()), rebound_facts);
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
        if intent.request_id().is_some() {
            request(&mut adapter, &stamp);
        }
        let prepared = adapter
            .admit(bound(&adapter, intent), facts())
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
        .admit(bound(&adapter, permission()), facts())
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
        assert!(adapter.admit(bound(&adapter, intent), facts()).is_err());
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
        .admit(bound(&adapter, permission()), facts())
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
    assert!(
        adapter
            .admit(bound(&adapter, permission()), facts())
            .is_err()
    );
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
    admission.config = AgentBackendConfig::Pi {
        model: String::new(),
        session_path: None,
        thinking: None,
    };
    assert!(adapter.admit(prompt(), admission).is_err());
    assert!(adapter.state().conversation.is_empty());
}

fn streaming(adapter: &mut ChatAdapter, source: &RuntimeStamp) {
    adapter
        .receive(
            source,
            &target(),
            AgentEvent::Harness(HarnessEvent::TurnStarted {
                session_id: None,
                model: None,
            }),
            None,
            |_, _| {},
        )
        .unwrap();
}
fn lifecycle(adapter: &mut ChatAdapter, source: &RuntimeStamp, state: MessageState) -> Update {
    adapter
        .receive(
            source,
            &target(),
            AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                id: "synthetic-message".into(),
                state,
            }),
            None,
            |_, _| {},
        )
        .unwrap()
}
fn withdraw() -> ChatIntent {
    ChatIntent::Withdraw {
        message_id: "synthetic-message".into(),
    }
}

#[test]
fn send_gate_checks_admission_and_execution_and_settles_queued_correlation() {
    for queue in [false, true] {
        let mut adapter = ChatAdapter::default();
        let source = adapter.begin_runtime(target());
        if queue {
            streaming(&mut adapter, &source);
        }
        let prepared = adapter.admit(prompt(), facts()).unwrap();
        request(&mut adapter, &source);
        assert!(adapter.admit(prompt(), facts()).is_err());
        let (receipt, _) =
            adapter.execute(prepared, &target(), 1, |_| panic!("pending request sent"));
        assert!(matches!(receipt.state, ReceiptState::Rejected(_)));
        assert!(!adapter.state().shows_prompt("synthetic-message"));
        if queue {
            assert_eq!(
                adapter.state().queued_prompts["synthetic-message"].status,
                PromptStatus::Unsent
            );
        }
    }
}

#[test]
fn streaming_uses_queue_and_rejected_channel_settles_withdraw_for_explicit_retry() {
    let mut adapter = ChatAdapter::default();
    let source = adapter.begin_runtime(target());
    streaming(&mut adapter, &source);
    let prepared = adapter.admit(prompt(), facts()).unwrap();
    let id = prepared.id();
    assert_eq!(
        adapter.state().queued_prompts["synthetic-message"].status,
        PromptStatus::Pending
    );
    accepted(&mut adapter, prepared);
    adapter
        .delivery(id, &target(), DeliveryOutcome::Written)
        .unwrap();
    lifecycle(&mut adapter, &source, MessageState::Queued);
    let prepared = adapter.admit(withdraw(), facts()).unwrap();
    assert!(adapter.state().queued_prompts["synthetic-message"].withdraw_pending);
    adapter.execute(prepared, &target(), 1, |_| Err("channel closed".into()));
    assert!(!adapter.state().queued_prompts["synthetic-message"].withdraw_pending);
    assert!(adapter.admit(withdraw(), facts()).is_ok());
}

#[test]
fn withdrawal_start_race_is_refused_before_send_and_visible_after_send() {
    for start_before_execute in [false, true] {
        let mut adapter = ChatAdapter::default();
        let source = adapter.begin_runtime(target());
        streaming(&mut adapter, &source);
        let prepared = adapter.admit(prompt(), facts()).unwrap();
        accepted(&mut adapter, prepared);
        lifecycle(&mut adapter, &source, MessageState::Queued);
        let prepared = adapter.admit(withdraw(), facts()).unwrap();
        if start_before_execute {
            let update = lifecycle(&mut adapter, &source, MessageState::Started);
            assert!(update.event.unwrap().notices[0].contains("already started"));
            let receipt = adapter
                .execute(prepared, &target(), 1, |_| {
                    panic!("started withdrawal sent")
                })
                .0;
            assert!(matches!(receipt.state, ReceiptState::Rejected(_)));
        } else {
            accepted(&mut adapter, prepared);
            let update = lifecycle(&mut adapter, &source, MessageState::Started);
            let mut composer = crate::composer::ComposerState::default();
            composer.project(adapter.state(), update.event.as_ref());
            assert!(composer.notice.unwrap().contains("already started"));
            assert!(!composer.queue[0].withdrawable());
            assert!(adapter.admit(withdraw(), facts()).is_err());
        }
    }
}

#[test]
fn exit_projects_core_unsent_versus_uncertain_without_resend() {
    for written in [false, true] {
        let mut adapter = ChatAdapter::default();
        let source = adapter.begin_runtime(target());
        streaming(&mut adapter, &source);
        let prepared = adapter.admit(prompt(), facts()).unwrap();
        let receipt = accepted(&mut adapter, prepared);
        if written {
            adapter
                .delivery(receipt.id, &target(), DeliveryOutcome::Written)
                .unwrap();
        }
        let update = adapter
            .receive(
                &source,
                &target(),
                AgentEvent::Harness(HarnessEvent::ProcessExited { code: Some(1) }),
                None,
                |_, _| {},
            )
            .unwrap();
        let mut composer = crate::composer::ComposerState::default();
        composer.project(adapter.state(), update.event.as_ref());
        assert_eq!(
            composer.queue[0].status,
            if written {
                PromptStatus::Uncertain
            } else {
                PromptStatus::Unsent
            }
        );
        assert!(
            composer.queue[0]
                .label()
                .contains(if written { "Uncertain" } else { "Unsent" })
        );
        assert_eq!(composer.queue[0].text, "synthetic draft");
        assert!(!composer.queue[0].withdrawable());
    }
}

#[test]
fn forged_mode_intent_has_no_adapter_command_path() {
    let mut adapter = ChatAdapter::default();
    adapter.begin_runtime(target());
    for mode in [
        "default",
        "auto",
        "dontAsk",
        "bypassPermissions",
        "plan",
        "made-up",
    ] {
        let receipt = adapter
            .admit(ChatIntent::ChangeMode { mode: mode.into() }, facts())
            .err()
            .unwrap();
        assert!(
            matches!(receipt.state, ReceiptState::Rejected(ref reason) if reason.contains("Mode changes are disabled"))
        );
        assert!(adapter.state().pending_change().is_none());
    }
}

#[test]
fn task_rebind_rejects_deferred_queue_but_settles_its_original_correlation() {
    let mut adapter = ChatAdapter::default();
    let source = adapter.begin_runtime(target());
    streaming(&mut adapter, &source);
    let prepared = adapter.admit(prompt(), facts()).unwrap();
    let mut current = target();
    current.task_id = Some("new-task".into());
    let receipt = adapter
        .execute(prepared, &current, 1, |_| panic!("stale binding sent"))
        .0;
    assert!(matches!(receipt.state, ReceiptState::Rejected(_)));
    assert_eq!(
        adapter.state().queued_prompts["synthetic-message"].status,
        PromptStatus::Unsent
    );
}

#[test]
fn process_exit_between_admission_and_execute_prevents_idle_submit() {
    let mut adapter = ChatAdapter::default();
    let source = adapter.begin_runtime(target());
    let prepared = adapter.admit(prompt(), facts()).unwrap();
    adapter
        .receive(
            &source,
            &target(),
            AgentEvent::Harness(HarnessEvent::ProcessExited { code: Some(1) }),
            None,
            |_, _| {},
        )
        .unwrap();
    assert!(matches!(
        adapter
            .execute(prepared, &target(), 1, |_| panic!("exited runtime sent"))
            .0
            .state,
        ReceiptState::Rejected(_)
    ));
    assert!(adapter.admit(prompt(), facts()).is_err());
}

#[test]
fn provider_withdraw_refusal_settles_visible_control_receipt_after_write() {
    let mut adapter = ChatAdapter::default();
    let source = adapter.begin_runtime(target());
    streaming(&mut adapter, &source);
    let prepared = adapter.admit(prompt(), facts()).unwrap();
    accepted(&mut adapter, prepared);
    lifecycle(&mut adapter, &source, MessageState::Queued);
    let prepared = adapter.admit(withdraw(), facts()).unwrap();
    let receipt = accepted(&mut adapter, prepared);
    let written = adapter
        .delivery(receipt.id, &target(), DeliveryOutcome::Written)
        .unwrap();
    let update = adapter
        .receive(
            &source,
            &target(),
            AgentEvent::Harness(HarnessEvent::MessageWithdrawRefused {
                id: "synthetic-message".into(),
                reason: "already picked up".into(),
            }),
            None,
            |_, _| {},
        )
        .unwrap();
    let refused = update
        .receipts
        .iter()
        .find(|receipt| receipt.id == written.id)
        .unwrap();
    assert!(written.accepts_update(refused));
    assert!(
        matches!(&refused.state, ReceiptState::Rejected(reason) if reason.contains("already picked up"))
    );
    assert!(!adapter.state().queued_prompts["synthetic-message"].withdraw_pending);
    let mut composer = crate::composer::ComposerState::default();
    composer.project(adapter.state(), update.event.as_ref());
    assert!(composer.notice.unwrap().contains("already picked up"));
}

#[test]
fn explicit_same_caller_retry_of_unsent_queue_uses_fresh_tracked_delivery_id() {
    for failure in ["validation", "channel", "delivery"] {
        let mut adapter = ChatAdapter::default();
        let source = adapter.begin_runtime(target());
        streaming(&mut adapter, &source);
        let prepared = adapter.admit(prompt(), facts()).unwrap();
        let receipt = if failure == "validation" {
            request(&mut adapter, &source);
            let receipt = adapter
                .execute(prepared, &target(), 1, |_| panic!("blocked send"))
                .0;
            adapter
                .receive(
                    &source,
                    &target(),
                    AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved {
                        request_id: "synthetic-request".into(),
                    }),
                    None,
                    |_, _| {},
                )
                .unwrap();
            receipt
        } else if failure == "channel" {
            adapter
                .execute(prepared, &target(), 1, |_| Err("closed".into()))
                .0
        } else {
            let receipt = accepted(&mut adapter, prepared);
            adapter
                .delivery(
                    receipt.id,
                    &target(),
                    DeliveryOutcome::Rejected("not written".into()),
                )
                .unwrap()
        };
        assert!(receipt.state.retryable());
        assert_eq!(
            adapter.state().queued_prompts["synthetic-message"].status,
            PromptStatus::Unsent
        );
        let retry = adapter.admit(prompt(), facts()).unwrap();
        let mut delivery_id = String::new();
        let (receipt, _) = adapter.execute(retry, &target(), 2, |command| {
            let HarnessCommand::SendUserMessage(prompt) = command else {
                panic!()
            };
            delivery_id = prompt.id.unwrap();
            assert_ne!(delivery_id, "synthetic-message");
            assert_eq!(prompt.text, "synthetic draft");
            Ok(())
        });
        assert_eq!(adapter.state().queued_prompts.len(), 2);
        assert_eq!(
            adapter.state().queued_prompts[&delivery_id].status,
            PromptStatus::Pending
        );
        assert!(
            adapter.admit(prompt(), facts()).is_err(),
            "caller identity still deduplicates"
        );
        let update = adapter
            .receive(
                &source,
                &target(),
                AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                    id: delivery_id.clone(),
                    state: MessageState::Queued,
                }),
                None,
                |_, _| {},
            )
            .unwrap();
        assert!(update.event.is_some());
        assert_eq!(
            adapter.state().queued_prompts[&delivery_id].status,
            PromptStatus::Queued
        );
        adapter
            .delivery(
                receipt.id,
                &target(),
                DeliveryOutcome::Uncertain("partial write".into()),
            )
            .unwrap();
        let rejected = adapter.admit(prompt(), facts()).err().unwrap();
        assert!(
            matches!(rejected.state, ReceiptState::Uncertain(_)),
            "uncertainty must never retry"
        );
        assert_eq!(adapter.state().queued_prompts.len(), 2);
    }
}

#[test]
fn deny_and_dismiss_match_iced_wire_messages_and_remember_preserves_offer() {
    let offered =
        json!({"destination":"localSettings","type":"addRules","rules":[{"toolName":"Read"}]});
    let kind = RuntimeRequestKind::Permission {
        tool_name: "Read".into(),
        input: json!({}),
        suggestions: vec![offered.clone()],
        decision_reason: None,
    };
    for (reason, expected) in [
        ("  no thanks\n", "no thanks"),
        (" \n ", "The user denied this tool call."),
    ] {
        assert_eq!(
            decision(
                &ChatIntent::Permission {
                    request_id: "r".into(),
                    decision: PermissionDecision::Deny {
                        reason: reason.into()
                    }
                },
                &kind
            )
            .unwrap(),
            RuntimeDecision::Deny {
                message: expected.into()
            }
        );
    }
    assert_eq!(
        decision(
            &ChatIntent::DismissQuestion {
                request_id: "r".into()
            },
            &RuntimeRequestKind::Question {
                questions: json!([])
            }
        )
        .unwrap(),
        RuntimeDecision::Deny {
            message: "The user declined to answer.".into()
        }
    );
    assert_eq!(
        decision(
            &ChatIntent::Permission {
                request_id: "r".into(),
                decision: PermissionDecision::AllowAlways {
                    suggestion: offered.clone()
                }
            },
            &kind
        )
        .unwrap(),
        RuntimeDecision::Allow {
            updated_input: None,
            remember: Some(offered)
        }
    );
    assert!(
        decision(
            &ChatIntent::Permission {
                request_id: "r".into(),
                decision: PermissionDecision::AllowAlways {
                    suggestion: json!({"destination":"userSettings"})
                }
            },
            &kind
        )
        .is_err()
    );
}

fn permission_event(id: &str, command: &str) -> HarnessEvent {
    HarnessEvent::RuntimeRequest {
        request_id: id.into(),
        tool_use_id: format!("tool-{command}"),
        kind: RuntimeRequestKind::Permission {
            tool_name: "Bash".into(),
            input: json!({"command":command}),
            suggestions: vec![],
            decision_reason: None,
        },
    }
}
fn ingress(adapter: &mut ChatAdapter, stamp: &RuntimeStamp, event: HarnessEvent) -> Update {
    adapter
        .receive(
            stamp,
            &target(),
            AgentEvent::Harness(event),
            None,
            |_, _| {},
        )
        .unwrap()
}
#[test]
fn parent_worker_same_id_never_collide_and_worker_answers_stay_gated() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    ingress(
        &mut adapter,
        &stamp,
        permission_event("synthetic-request", "parent"),
    );
    let parent = adapter
        .state()
        .request_identity(&stamp, &[], "synthetic-request")
        .unwrap();
    let prepared = adapter
        .admit(bound(&adapter, permission()), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    ingress(
        &mut adapter,
        &stamp,
        HarnessEvent::SubagentEvent {
            parent_tool_use_id: "worker".into(),
            event: Box::new(permission_event("synthetic-request", "child")),
        },
    );
    let child = adapter
        .state()
        .request_identity(&stamp, &["worker".into()], "synthetic-request")
        .unwrap();
    assert_ne!(parent, child);
    let model = ChatModel::from_conversation(&adapter.state().conversation);
    assert_eq!(model.pending_rows(), vec![0, 1]);
    assert!(
        adapter
            .admit(
                ChatIntent::Answer {
                    identity: child,
                    answer: Box::new(permission())
                },
                facts()
            )
            .is_err()
    );
    let update = ingress(
        &mut adapter,
        &stamp,
        HarnessEvent::SubagentEvent {
            parent_tool_use_id: "worker".into(),
            event: Box::new(HarnessEvent::RuntimeRequestResolved {
                request_id: "synthetic-request".into(),
            }),
        },
    );
    assert!(update.receipts.is_empty());
    assert!(adapter.state().accepts_request(&parent, &target()));
    assert_eq!(
        ChatModel::from_conversation(&adapter.state().conversation).pending_rows(),
        vec![0]
    );
    let (receipt, _) = adapter.execute(prepared, &target(), 1, |command| {
        assert!(matches!(command, HarnessCommand::Answer { request_id, decision: RuntimeDecision::Allow { .. } } if request_id == "synthetic-request")); Ok(())
    });
    assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
}
#[test]
fn replacement_between_display_admit_and_execute_rejects_old_instance() {
    for same_payload in [true, false] {
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        ingress(
            &mut adapter,
            &stamp,
            permission_event("synthetic-request", "original"),
        );
        let old = bound(&adapter, permission());
        let prepared = adapter
            .admit(old.clone(), facts())
            .unwrap_or_else(|r| panic!("{r:?}"));
        ingress(&mut adapter, &stamp, permission_event("second", "second"));
        ingress(
            &mut adapter,
            &stamp,
            permission_event(
                "synthetic-request",
                if same_payload {
                    "original"
                } else {
                    "replacement"
                },
            ),
        );
        assert!(adapter.admit(old, facts()).is_err());
        assert!(
            adapter
                .execute(prepared, &target(), 1, |_| panic!("old request sent"))
                .0
                .state
                .retryable()
        );
        let model = ChatModel::from_conversation(&adapter.state().conversation);
        assert_eq!(model.pending_rows(), vec![2, 1]);
        let fresh = adapter
            .admit(bound(&adapter, permission()), facts())
            .unwrap_or_else(|r| panic!("{r:?}"));
        assert_eq!(
            accepted(&mut adapter, fresh).state,
            ReceiptState::ChannelAccepted
        );
    }
}
#[test]
fn resolution_and_late_delivery_are_instance_scoped_after_replacement() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    let prepared = adapter
        .admit(bound(&adapter, permission()), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let old = accepted(&mut adapter, prepared);
    request(&mut adapter, &stamp);
    let prepared = adapter
        .admit(bound(&adapter, permission()), facts())
        .unwrap_or_else(|r| panic!("{r:?}"));
    let fresh = accepted(&mut adapter, prepared);
    let update = ingress(
        &mut adapter,
        &stamp,
        HarnessEvent::RuntimeRequestResolved {
            request_id: "synthetic-request".into(),
        },
    );
    assert_eq!(
        update.receipts.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![fresh.id]
    );
    assert_eq!(
        adapter.receipt(old.id).unwrap().state,
        ReceiptState::ChannelAccepted
    );
    adapter.delivery(old.id, &target(), DeliveryOutcome::Written);
    assert_eq!(
        adapter.receipt(fresh.id).unwrap().state,
        ReceiptState::Resolved
    );
}
#[test]
fn unbound_and_previous_runtime_answers_are_rejected() {
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    assert!(adapter.admit(permission(), facts()).is_err());
    let stale = bound(&adapter, permission());
    let stamp = adapter.begin_runtime(target());
    request(&mut adapter, &stamp);
    assert!(adapter.admit(stale, facts()).is_err());
}

#[test]
fn nested_owner_segments_are_not_slash_joined_request_identity() {
    fn worker(path: &[&str], event: HarnessEvent) -> HarnessEvent {
        path.iter()
            .rev()
            .fold(event, |event, id| HarnessEvent::SubagentEvent {
                parent_tool_use_id: (*id).into(),
                event: Box::new(event),
            })
    }
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target());
    ingress(
        &mut adapter,
        &stamp,
        worker(&["a/b"], permission_event("same", "one")),
    );
    ingress(
        &mut adapter,
        &stamp,
        worker(&["a", "b"], permission_event("same", "two")),
    );
    let one = adapter
        .state()
        .request_identity(&stamp, &["a/b".into()], "same")
        .unwrap();
    let two = adapter
        .state()
        .request_identity(&stamp, &["a".into(), "b".into()], "same")
        .unwrap();
    assert_ne!(one, two);
    ingress(
        &mut adapter,
        &stamp,
        worker(
            &["a/b"],
            HarnessEvent::RuntimeRequestResolved {
                request_id: "same".into(),
            },
        ),
    );
    assert!(!adapter.state().accepts_request(&one, &target()));
    assert!(adapter.state().accepts_request(&two, &target()));
    let model = ChatModel::from_conversation(&adapter.state().conversation);
    assert_eq!(model.pending_rows(), vec![1]);
}

#[test]
fn offered_remember_options_dispatch_exactly_through_bound_adapter() {
    for suggestion in [
        json!({"destination":"localSettings","type":"addRules","rules":[{"toolName":"Read"}]}),
        json!({"type":"acp_option","name":"Always allow","option_id":"always"}),
    ] {
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        ingress(
            &mut adapter,
            &stamp,
            HarnessEvent::RuntimeRequest {
                request_id: "synthetic-request".into(),
                tool_use_id: "tool".into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Read".into(),
                    input: json!({}),
                    suggestions: vec![suggestion.clone()],
                    decision_reason: None,
                },
            },
        );
        let intent = bound(
            &adapter,
            ChatIntent::Permission {
                request_id: "synthetic-request".into(),
                decision: PermissionDecision::AllowAlways {
                    suggestion: suggestion.clone(),
                },
            },
        );
        let prepared = adapter
            .admit(intent, facts())
            .unwrap_or_else(|r| panic!("{r:?}"));
        let (receipt, _) = adapter.execute(prepared, &target(), 1, |command| { assert!(matches!(command, HarnessCommand::Answer { decision:RuntimeDecision::Allow { remember:Some(ref update), .. }, .. } if *update == suggestion)); Ok(()) });
        assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    }
}

#[test]
fn worker_reset_and_completion_do_not_hide_or_invalidate_parent_request() {
    for lifecycle in [
        HarnessEvent::ConversationReset { trigger: None },
        HarnessEvent::ProcessExited { code: Some(0) },
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: json!({}),
            cost_usd: None,
            rate_limit: None,
        },
    ] {
        let mut adapter = ChatAdapter::default();
        let stamp = adapter.begin_runtime(target());
        ingress(&mut adapter, &stamp, permission_event("same", "parent"));
        let parent = adapter
            .state()
            .request_identity(&stamp, &[], "same")
            .unwrap();
        ingress(
            &mut adapter,
            &stamp,
            HarnessEvent::SubagentEvent {
                parent_tool_use_id: "worker".into(),
                event: Box::new(permission_event("same", "worker")),
            },
        );
        ingress(
            &mut adapter,
            &stamp,
            HarnessEvent::SubagentEvent {
                parent_tool_use_id: "worker".into(),
                event: Box::new(lifecycle),
            },
        );
        assert!(adapter.state().accepts_request(&parent, &target()));
        let model = ChatModel::from_conversation(&adapter.state().conversation);
        assert_eq!(model.pending_rows(), vec![0]);
    }
}
