use super::*;
use gitterm_core::chat::state::ConfirmedEffort;
use serde_json::json;

#[test]
fn acknowledged_auto_and_applied_effort_and_slash_catalog_are_projected() {
    let mut state = State::default();
    state.acknowledged.model = Some("confirmed-model".into());
    state.acknowledged.mode = Some("default".into());
    state.acknowledged.effort = Some(ConfirmedEffort {
        effort: None,
        applied: Some("high".into()),
    });
    state.conversation.push(AgentEvent::Harness(HarnessEvent::Ready {
        session_id: None, permission_mode: None, models: vec![], modes: vec![],
        commands: vec![json!({"name":"checkpoint","description":"Save progress","argumentHint":"[message]"}), json!({"name":"bad name"}), json!({"description":"no name"})],
    }));
    let mut view = ComposerState::default();
    view.project(&state, None);
    assert_eq!(view.model, "confirmed-model");
    assert_eq!(view.mode, "default");
    assert_eq!(view.effort, "Auto · applied: high");
    assert_eq!(view.slash_matches("/check")[0].name, "checkpoint");
    assert!(view.slash_matches("/checkpoint arg").is_empty());
    assert!(view.slash_matches("text/check").is_empty());
    assert_eq!(view.commands.len(), 1);
    state.pending_requests.push("request".into());
    view.project(&state, None);
    assert!(view.pending_request);
    assert!(view.status.contains("Needs you"));
}
