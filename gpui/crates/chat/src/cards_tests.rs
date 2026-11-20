use super::*;
use crate::{
    events::{Entry, HarnessEvent, Record},
    model::ChatModel,
};
use gpui::TestAppContext;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn row(kind: RuntimeRequestKind) -> Row {
    ChatModel::from_records(&[Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::RuntimeRequest {
            request_id: "synthetic-request".into(),
            tool_use_id: "synthetic-tool".into(),
            kind,
        }),
    }])
    .rows
    .remove(0)
}
fn permission_row() -> Row {
    row(RuntimeRequestKind::Permission {
        tool_name: "Bash".into(),
        input: json!({"command":"echo synthetic"}),
        suggestions: vec![
            json!({"type":"addRules", "destination":"localSettings", "rules":[{"toolName":"Bash", "ruleContent":"echo synthetic"}]}),
        ],
        decision_reason: None,
    })
}

#[test]
fn permission_presentation_matches_iced_offered_order_and_salient_text() {
    let suggestions = vec![
        json!({"type":"acp_option", "name":"Allow for session", "option_id":"session"}),
        json!({"type":"addRules", "destination":"userSettings"}),
        json!({"type":"addRules", "destination":"localSettings", "rules":[{"toolName":"Bash", "ruleContent":"echo synthetic"}, {"toolName":"Read"}]}),
        json!({"type":"addRules", "destination":"localSettings", "rules":[]}),
        json!({"type":"acp_option", "option_id":"always"}),
    ];
    let offered = offered_options(&suggestions);
    assert_eq!(
        offered
            .iter()
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>(),
        vec!["Allow and remember", "Allow for session", "Always allow"]
    );
    assert_eq!(offered[0].0, &suggestions[2]);
    assert_eq!(describe_rule(offered[0].0), "Bash(echo synthetic), Read");
    assert_eq!(
        permission_salient(
            "Bash",
            &json!({"command":"echo synthetic", "description":"Fixture"})
        ),
        "echo synthetic"
    );
    assert_eq!(
        permission_salient("Read", &json!({"file_path":"fixture.rs"})),
        "fixture.rs"
    );
    assert_eq!(
        permission_salient("NotebookEdit", &json!({"notebook_path":"fixture.ipynb"})),
        "fixture.ipynb"
    );
    assert_eq!(
        permission_salient("WebFetch", &json!({"url":"https://example.invalid"})),
        "{\n  \"url\": \"https://example.invalid\"\n}"
    );
}

#[gpui::test]
fn permission_decisions_lock_locally_and_preserve_offered_rule(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let intents = Rc::new(RefCell::new(vec![]));
    for mode in 0..3 {
        cx.update(|window, cx| {
            let captured = intents.clone();
            let callback: IntentCallback = Rc::new(move |intent, _| { captured.borrow_mut().push(intent); let mut r = Receipt::rejected("synthetic"); r.state = crate::adapter::ReceiptState::ChannelAccepted; r });
            let card = cx.new(|cx| CardView::new(permission_row(), Some(callback), false, window, cx));
            card.update(cx, |card, cx| {
                let decision = match mode { 0 => PermissionDecision::Allow, 1 => PermissionDecision::AllowAlways { suggestion: json!({"type":"addRules", "destination":"localSettings", "rules":[{"toolName":"Bash", "ruleContent":"echo synthetic"}]}) }, _ => PermissionDecision::Deny { reason: String::new() } };
                card.permission(decision.clone(), cx);
                assert_eq!(intents.borrow().last(), Some(&ChatIntent::Permission { request_id:"synthetic-request".into(), decision }));
                assert!(!card.enabled());
                card.permission(PermissionDecision::Allow, cx);
                assert!(card.row.decision.is_none());
                assert!(!card.row.request.as_ref().unwrap().resolved);
            });
        });
    }
    assert_eq!(intents.borrow().len(), 3);
}

#[gpui::test]
fn questionnaire_single_multi_other_and_validation(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let intents = Rc::new(RefCell::new(vec![]));
    cx.update(|window, cx| {
        let captured = intents.clone();
        let callback: IntentCallback = Rc::new(move |intent, _| { captured.borrow_mut().push(intent); let mut r = Receipt::rejected("synthetic"); r.state = crate::adapter::ReceiptState::ChannelAccepted; r });
        let source = row(RuntimeRequestKind::Question { questions: json!([
            {"header":"Policy", "question":"Which policy?", "options":[{"label":"Fixed"},{"label":"Backoff"}]},
            {"question":"Which checks?", "multiSelect":true, "options":[{"label":"Unit"},{"label":"Integration"}]}
        ]) });
        let card = cx.new(|cx| CardView::new(source, Some(callback), false, window, cx));
        card.update(cx, |card, cx| {
            card.submit(cx);
            assert!(intents.borrow().is_empty());
            card.choose(0, Some("Fixed".into()), cx);
            card.choose(0, Some("Backoff".into()), cx);
            assert_eq!(card.drafts[0].selected, vec!["Backoff"]);
            card.choose(0, None, cx);
            assert!(card.drafts[0].selected.is_empty());
            card.drafts[0].other.update(cx, |input, cx| input.set_value(" Custom policy ", window, cx));
            card.choose(1, Some("Integration".into()), cx);
            card.choose(1, Some("Unit".into()), cx);
            card.choose(1, Some("Unit".into()), cx);
            card.choose(1, Some("Unit".into()), cx);
            card.choose(1, None, cx);
            card.drafts[1].other.update(cx, |input, cx| input.set_value(" Manual ", window, cx));
            card.submit(cx);
            assert_eq!(*intents.borrow(), vec![ChatIntent::AnswerQuestion { request_id:"synthetic-request".into(), answers:vec![
                QuestionAnswer { question:"Which policy?".into(), selected:vec![], free_text:Some("Custom policy".into()) },
                QuestionAnswer { question:"Which checks?".into(), selected:vec!["Unit".into(),"Integration".into()], free_text:Some("Manual".into()) },
            ] }]);
            assert_question_encoding(
                intents.borrow()[0].clone(),
                card.row.request.as_ref().unwrap().kind.clone(),
            );
            card.choose(0, Some("Fixed".into()), cx);
            card.submit(cx);
            assert_eq!(intents.borrow().len(), 1);
        });
    });
}

fn assert_question_encoding(intent: ChatIntent, kind: RuntimeRequestKind) {
    use crate::adapter::{Admission, ChatAdapter};
    use gitterm_core::{
        chat::{
            state::AgentEvent,
            types::{AgentBackendConfig, HarnessCommand, RuntimeDecision},
        },
        session,
    };
    let target = session::Target {
        tab_id: 7,
        session_uid: "synthetic-chat".into(),
        task_id: None,
        task_session_id: None,
    };
    let mut adapter = ChatAdapter::default();
    let stamp = adapter.begin_runtime(target.clone());
    adapter
        .receive(
            &stamp,
            &target,
            AgentEvent::Harness(HarnessEvent::RuntimeRequest {
                request_id: "synthetic-request".into(),
                tool_use_id: "synthetic-tool".into(),
                kind,
            }),
            None,
            |_, _| {},
        )
        .unwrap();
    let prepared = adapter
        .admit(
            ChatIntent::Answer {
                identity: adapter
                    .state()
                    .request_identity(&stamp, &[], "synthetic-request")
                    .unwrap(),
                answer: Box::new(intent),
            },
            Admission {
                authority: session::State {
                    target: target.clone(),
                    control: session::TurnControl::Harness,
                    task_live: false,
                    last_tab: false,
                },
                config: AgentBackendConfig::Claude {
                    model: String::new(),
                    permission_mode: Some("default".into()),
                    effort: None,
                },
                process_running: true,
                just_spawned: false,
                message_id: "synthetic-message".into(),
            },
        )
        .unwrap_or_else(|receipt| panic!("{receipt:?}"));
    let mut delivered = false;
    adapter.execute(prepared, &target, 1, |command| {
        let HarnessCommand::Answer {
            request_id,
            decision: RuntimeDecision::AnswerQuestion { answers },
        } = command
        else {
            panic!("wrong command: {command:?}")
        };
        assert_eq!(request_id, "synthetic-request");
        assert_eq!(
            serde_json::to_value(answers).unwrap(),
            json!({"Which policy?":"Custom policy", "Which checks?":"Unit, Integration, Manual"})
        );
        delivered = true;
        Ok(())
    });
    assert!(delivered);
}

#[gpui::test]
fn read_only_recorded_resolved_and_worker_cards_cannot_emit(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let callback: IntentCallback = Rc::new(|_, _| panic!("disabled card emitted"));
        for mode in 0..4 {
            let mut source = permission_row();
            if mode == 1 {
                source.decision = Some(crate::events::DecisionState::Answered);
            }
            if mode == 2 {
                source.request.as_mut().unwrap().resolved = true;
            }
            if mode == 3 {
                source.scope = "worker".into();
            }
            let card =
                cx.new(|cx| CardView::new(source, Some(callback.clone()), mode == 0, window, cx));
            card.update(cx, |card, cx| {
                assert!(!card.enabled());
                card.permission(PermissionDecision::Allow, cx);
            });
        }
    });
}

#[gpui::test]
fn receipts_unlock_only_definitive_rejection_and_resolution_wins(cx: &mut TestAppContext) {
    use crate::adapter::ReceiptState;
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let callback: IntentCallback = Rc::new(|_, _| {
            let mut receipt = Receipt::rejected("synthetic");
            receipt.state = ReceiptState::ChannelAccepted;
            receipt
        });
        let card = cx.new(|cx| CardView::new(permission_row(), Some(callback), false, window, cx));
        card.update(cx, |card, cx| {
            card.permission(
                PermissionDecision::Deny {
                    reason: String::new(),
                },
                cx,
            );
            let mut rejected = card.receipt.clone().unwrap();
            rejected.state = ReceiptState::Rejected("not written".into());
            card.update_receipt(&rejected, cx);
            assert!(card.enabled());
            assert!(!card.row.request.as_ref().unwrap().resolved);
            card.permission(PermissionDecision::Allow, cx);
            let mut uncertain = card.receipt.clone().unwrap();
            uncertain.state = ReceiptState::Uncertain("partial write".into());
            card.update_receipt(&uncertain, cx);
            assert!(!card.enabled());
            let mut late = uncertain.clone();
            late.state = ReceiptState::Rejected("late".into());
            card.update_receipt(&late, cx);
            assert!(!card.enabled());
            assert!(matches!(
                card.receipt.as_ref().unwrap().state,
                ReceiptState::Uncertain(_)
            ));
            let mut row = card.row.clone();
            row.request.as_mut().unwrap().resolved = true;
            card.sync(row, false, cx);
            card.update_receipt(&rejected, cx);
            assert!(!card.enabled());
            assert!(card.row.decision.is_none());
        });
    });
}

#[gpui::test]
fn card_rejects_wrong_request_kind_and_unoffered_rule(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let callback: IntentCallback = Rc::new(|_, _| panic!("invalid card intent emitted"));
        let card = cx.new(|cx| CardView::new(permission_row(), Some(callback), false, window, cx));
        card.update(cx, |card, cx| {
            card.emit(ChatIntent::Permission { request_id: "another-request".into(), decision: PermissionDecision::Allow }, cx);
            card.dismiss(cx);
            card.emit(ChatIntent::AnswerQuestion { request_id:"synthetic-request".into(), answers:vec![] }, cx);
            card.emit(ChatIntent::Permission { request_id:"synthetic-request".into(), decision:PermissionDecision::AllowAlways { suggestion:json!({"type":"addRules","destination":"userSettings"}) } }, cx);
            card.permission(PermissionDecision::AllowAlways { suggestion:json!({"type":"addRules","destination":"localSettings","rules":[{"toolName":"Bash","ruleContent":"different"}]}) }, cx);
            assert!(card.enabled());
            assert!(card.receipt.is_none());
        });
    });
}

#[gpui::test]
fn malformed_question_can_dismiss_and_retry_only_after_non_delivery(cx: &mut TestAppContext) {
    use crate::adapter::ReceiptState;
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let intents = Rc::new(RefCell::new(vec![]));
    cx.update(|window, cx| {
        let captured = intents.clone();
        let callback: IntentCallback = Rc::new(move |intent, _| {
            captured.borrow_mut().push(intent);
            let mut receipt = Receipt::rejected("synthetic");
            receipt.state = ReceiptState::ChannelAccepted;
            receipt
        });
        let card = cx.new(|cx| {
            CardView::new(
                row(RuntimeRequestKind::Question {
                    questions: json!({"invalid":"array required"}),
                }),
                Some(callback),
                false,
                window,
                cx,
            )
        });
        card.update(cx, |card, cx| {
            assert!(card.questions.is_err());
            card.submit(cx);
            assert!(intents.borrow().is_empty());
            card.dismiss(cx);
            card.dismiss(cx);
            assert_eq!(intents.borrow().len(), 1);
            let mut receipt = card.receipt.clone().unwrap();
            receipt.state = ReceiptState::Rejected("not written".into());
            card.update_receipt(&receipt, cx);
            card.dismiss(cx);
            assert_eq!(
                intents.borrow().as_slice(),
                &[
                    ChatIntent::DismissQuestion {
                        request_id: "synthetic-request".into()
                    },
                    ChatIntent::DismissQuestion {
                        request_id: "synthetic-request".into()
                    },
                ]
            );
            let mut receipt = card.receipt.clone().unwrap();
            receipt.state = ReceiptState::Uncertain("partial write".into());
            card.update_receipt(&receipt, cx);
            card.dismiss(cx);
            assert_eq!(intents.borrow().len(), 2);
        });
    });
}

#[gpui::test]
fn question_description_fits_selected_and_unselected_options(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let callback: IntentCallback = Rc::new(|_, _| panic!("selection must not submit"));
        let card = cx.new(|cx| CardView::new(row(RuntimeRequestKind::Question { questions:json!([
            {"question":"Which checks?", "multiSelect":true, "options":[{"label":"Unit", "description":"Run the focused checks and inspect the resulting output before continuing."}]}
        ]) }), Some(callback), false, window, cx));
        gpui_component::Root::new(card, window, cx)
    });
    let card = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<CardView>().unwrap()
    });
    cx.simulate_resize(gpui::size(px(320.), px(600.)));
    for selected in [false, true] {
        if selected {
            card.update(cx, |card, cx| {
                card.choose(0, Some("Unit".into()), cx);
                card.choose(0, None, cx);
            });
        }
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let button = cx.debug_bounds("question-option-0-0").unwrap();
        let description = cx.debug_bounds("question-option-description-0-0").unwrap();
        assert!(button.size.height > px(24.));
        assert!(description.size.height > px(16.));
        assert!(description.top() >= button.top());
        assert!(description.bottom() <= button.bottom());
        assert!(description.left() >= button.left());
        assert!(description.right() <= button.right());
    }
}

#[gpui::test]
fn deny_reason_enter_emits_typed_text_without_submitting_composer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let captured = Rc::new(RefCell::new(vec![]));
    cx.update(|window, cx| {
        let output = captured.clone();
        let callback: IntentCallback = Rc::new(move |intent, _| {
            output.borrow_mut().push(intent);
            Receipt::rejected("fixture")
        });
        let card = cx.new(|cx| CardView::new(permission_row(), Some(callback), false, window, cx));
        card.update(cx, |card, cx| {
            card.deny_reason.update(cx, |input, cx| {
                input.set_value("  preserve this reason  ", window, cx)
            });
            card.deny(cx);
        });
    });
    assert!(
        matches!(&captured.borrow()[0], ChatIntent::Permission { decision: PermissionDecision::Deny { reason }, .. } if reason == "  preserve this reason  ")
    );
}

#[gpui::test]
fn preview_remember_stays_disabled_at_card_boundary(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let callback: IntentCallback = Rc::new(|_, _| panic!("preview remember reached dispatch"));
        let card = cx.new(|cx| CardView::new(permission_row(), Some(callback), false, window, cx));
        card.update(cx, |card, cx| {
            card.preview = true;
            let RuntimeRequestKind::Permission { suggestions, .. } =
                &card.row.request.as_ref().unwrap().kind
            else {
                unreachable!()
            };
            card.permission(
                PermissionDecision::AllowAlways {
                    suggestion: suggestions[0].clone(),
                },
                cx,
            );
            assert!(card.submitted.is_none());
        });
    });
}
