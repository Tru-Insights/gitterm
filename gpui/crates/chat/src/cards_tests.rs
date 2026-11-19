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

#[gpui::test]
fn permission_decisions_lock_locally_and_preserve_offered_rule(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let intents = Rc::new(RefCell::new(vec![]));
    for mode in 0..3 {
        cx.update(|window, cx| {
            let captured = intents.clone();
            let callback: IntentCallback = Rc::new(move |intent, _| captured.borrow_mut().push(intent));
            let card = cx.new(|cx| CardView::new(permission_row(), Some(callback), false, window, cx));
            card.update(cx, |card, cx| {
                let decision = match mode { 0 => PermissionDecision::Allow, 1 => PermissionDecision::AllowAlways { suggestion: json!({"type":"addRules", "destination":"localSettings", "rules":[{"toolName":"Bash", "ruleContent":"echo synthetic"}]}) }, _ => PermissionDecision::Deny };
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
        let callback: IntentCallback = Rc::new(move |intent, _| captured.borrow_mut().push(intent));
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
            card.choose(0, Some("Fixed".into()), cx);
            card.submit(cx);
            assert_eq!(intents.borrow().len(), 1);
        });
    });
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
