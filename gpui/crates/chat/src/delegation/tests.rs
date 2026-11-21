use super::*;
use serde_json::json;
pub(crate) fn card(kind: &str, state: &str) -> DelegationCard {
    serde_json::from_value(json!({
        "delegation": {
            "delegation_id":"review-1","kind":kind,
            "parent":{"session_uid":"parent","workspace":"sample","cwd":"/private/tmp/sample"},
            "child": if kind=="implement" {json!({"runner":"task_session","task_id":"task","task_session_id":"worker-session","preset_name":"Codex","model":"test-model"})}else{json!({"runner":"codex_review","model":"test-model"})},
            "brief":"Inspect the change","status":{"state":state},
            "created_at":"2026-11-20T00:00:00Z","updated_at":"2026-11-20T00:01:00Z",
            "target":{"mode":{"type":"base","reference":"v6"},"focus":"edge cases"}
        },
        "worker":if kind=="implement" {json!({"preset":"Codex","model":"test-model","role":"scoped","title":"Implement change","issue_key":"TRU-274","branch":"task/change","worktree_path":"/private/tmp/sample","session_state":"running","tab_open":true,"progress":"Checking tests"})} else {Value::Null}
    })).unwrap()
}
pub(crate) fn with_findings() -> DelegationCard {
    let mut card = card("review", "completed");
    card.delegation.result=Some(serde_json::from_value(json!({
        "reviewed":{"head":"123456789abc","dirty":true,"target_description":"vs v6"},
        "findings":{"verdict":"needs_changes","structured":true,"summary":"One issue","reviewer":{"harness":"Codex"},
            "findings":[{"id":"F1","severity":"P1","title":"Handle empty input","body":"Return early.","file":"src/lib.rs","line_start":12,"line_end":14}]}
    })).unwrap());
    card
}
#[test]
fn h11_iced_delegation_header_target_status_result_and_dismissed_state() {
    let card = with_findings();
    assert_eq!(card.header(), "Codex review · test-model");
    assert_eq!(card.target(), "vs v6 (+ uncommitted) · focus: edge cases");
    assert_eq!(card.state_text(0), "1 finding (1 P1)");
    assert_eq!(
        card.body(),
        "Reviewed at 12345678, dirty · vs v6\n\nVerdict: needs changes\n\nOne issue"
    );
    let worker = super::tests::card("implement", "running");
    assert_eq!(worker.header(), "Worker · Codex · test-model · scoped");
    assert_eq!(worker.target(), "TRU-274 · Implement change");
    assert!(
        crate::model::plain_markdown(&worker.body()).contains("task/change · /private/tmp/sample")
    );
    assert!(worker.body().contains("Latest: Checking tests"));
    let mut dismissed = card.clone();
    dismissed.delegation.dismissed_at = Some("now".into());
    assert_eq!(dismissed.state_text(0), "dismissed · 1 finding (1 P1)");
    assert_eq!(
        dismissed
            .controls(false)
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        ["Re-run"]
    );
}
#[test]
fn h13_finding_location_preserves_file_and_line_range() {
    let card = with_findings();
    let f = &card.findings().unwrap().findings[0];
    assert_eq!(finding_location(f).as_deref(), Some("src/lib.rs:12-14"));
    let mut f = f.clone();
    f.line_end = Some(12);
    assert_eq!(finding_location(&f).as_deref(), Some("src/lib.rs:12"));
    f.line_start = None;
    assert_eq!(finding_location(&f).as_deref(), Some("src/lib.rs"));
}
#[test]
fn h14_h15_h16_h16a_h16b_iced_control_states_and_order() {
    for (kind, state, expected) in [
        ("review", "running", vec!["Cancel"]),
        ("consult", "requested", vec!["Cancel"]),
        (
            "review",
            "completed",
            vec!["Send to Claude", "Re-run", "Dismiss"],
        ),
        (
            "implement",
            "running",
            vec!["Open worker tab", "Stop tracking"],
        ),
        (
            "implement",
            "blocked",
            vec!["Open worker tab", "Send to Claude", "Dismiss"],
        ),
        (
            "implement",
            "completed",
            vec!["Open worker tab", "Send to Claude", "Dismiss"],
        ),
    ] {
        let card = card(kind, state);
        let controls = card.controls(false);
        assert_eq!(
            controls
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(controls.iter().all(|c| c.label == "Open worker tab"
            || matches!(&c.action,TranscriptAction::PreviewOnly {label} if label==&c.label)));
    }
    let mut card = card("review", "completed");
    card.held = true;
    let control = &card.controls(false)[0];
    assert_eq!(control.label, "Queued, sends when Claude finishes");
    assert!(control.disabled);
    card.delegation.delivered = true;
    assert_eq!(
        card.controls(false)
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        ["Re-run", "Dismiss"]
    );
}
#[test]
fn h13a_worker_open_is_a_navigation_request_even_when_closed() {
    let mut card = card("implement", "running");
    assert_eq!(
        card.controls(false)[0].tooltip,
        "Switch to the worker's terminal tab"
    );
    card.worker.as_mut().unwrap()["tab_open"] = json!(false);
    let control = &card.controls(false)[0];
    assert_eq!(control.tooltip, "The worker tab is not open");
    assert_eq!(
        control.action,
        TranscriptAction::OpenWorker {
            id: "review-1".into()
        }
    );
}
