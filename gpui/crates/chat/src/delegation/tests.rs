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
        "Reviewed at 1234567, dirty · vs v6\n\nVerdict: needs changes\n\nOne issue"
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
            .controls(false, &Default::default())
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
        let controls = card.controls(false, &Default::default());
        assert_eq!(
            controls
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(controls.iter().all(|c| c.label == "Open worker tab"
            || matches!(&c.action, TranscriptAction::SendDelegation { id, .. } | TranscriptAction::RerunDelegation { id } if id == "review-1")
            || matches!(&c.action,TranscriptAction::Dismiss(request) if request.label==c.label
                && request.id=="review-1"
                && request.active==card.delegation.status.is_active())));
    }
    let mut card = card("review", "completed");
    card.held = true;
    let control = &card.controls(false, &Default::default())[0];
    assert_eq!(control.label, "Queued, sends when Claude finishes");
    assert!(control.disabled);
    card.delegation.delivered = true;
    assert_eq!(
        card.controls(false, &Default::default())
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
        card.controls(false, &Default::default())[0].tooltip,
        "Switch to the worker's terminal tab"
    );
    card.worker.as_mut().unwrap()["tab_open"] = json!(false);
    let control = &card.controls(false, &Default::default())[0];
    assert_eq!(control.tooltip, "The worker tab is not open");
    assert_eq!(
        control.action,
        TranscriptAction::OpenWorker {
            id: "review-1".into()
        }
    );
}

/// D02/D17: Iced's `shortHead` keeps seven characters, `?` for none.
#[test]
fn d02_d17_commit_target_and_reviewed_head_use_seven_characters() {
    let mut card = with_findings();
    card.delegation.target.as_mut().unwrap().mode = ReviewTargetMode::Commit {
        sha: "abcdef0123456".into(),
    };
    card.delegation.target.as_mut().unwrap().focus = None;
    assert_eq!(card.target(), "commit abcdef0");
    assert_eq!(
        card.parts()[0],
        CardPart::Line("Reviewed at 1234567, dirty · vs v6".into())
    );
    card.delegation
        .result
        .as_mut()
        .unwrap()
        .reviewed
        .as_mut()
        .unwrap()
        .head = String::new();
    assert_eq!(
        card.parts()[0],
        CardPart::Line("Reviewed at ?, dirty · vs v6".into())
    );
}

/// D17: renderResult's banner when the checkout's HEAD has moved on.
#[test]
fn d17_branch_moved_banner_follows_the_checkout_head() {
    let banner = CardPart::Banner(
        "The branch has moved since (now at fedcba9); line numbers may have drifted.".into(),
    );
    let mut card = with_findings();
    assert!(!card.parts().contains(&banner), "no head known yet");
    card.checkout_head = Some("123456789abc".into());
    assert!(!card.parts().contains(&banner), "same commit");
    card.checkout_head = Some("fedcba9876543".into());
    let parts = card.parts();
    assert_eq!(parts[1], banner, "right after the Reviewed line");
    assert!(plain_markdown_body(&card).contains("The branch has moved since (now at fedcba9)"));
    card.delegation
        .result
        .as_mut()
        .unwrap()
        .reviewed
        .as_mut()
        .unwrap()
        .head = String::new();
    assert!(!card.parts().contains(&banner), "no reviewed head");
}
fn plain_markdown_body(card: &DelegationCard) -> String {
    crate::model::plain_markdown(&card.body())
}

/// D07: an empty failure message falls back as Iced's `|| 'Codex failed'`.
#[test]
fn d07_failed_without_a_message_uses_iceds_fallback() {
    let mut review = card("review", "completed");
    review.delegation.status = DelegationStatus::Failed {
        message: String::new(),
    };
    assert_eq!(review.parts(), [CardPart::Error("Codex failed".into())]);
    assert_eq!(review.body(), "Codex failed");
    let mut worker = card("implement", "completed");
    worker.worker.as_mut().unwrap()["session_state"] = "failed".into();
    worker.delegation.status = DelegationStatus::Failed {
        message: String::new(),
    };
    assert!(
        worker
            .parts()
            .contains(&CardPart::Error("The worker failed".into()))
    );
}

/// D09: renderCard has no body branch for a blocked review or consult,
/// even with a result.
#[test]
fn d09_blocked_review_shows_no_result() {
    let mut card = with_findings();
    card.delegation.status = DelegationStatus::Blocked;
    assert_eq!(card.parts(), []);
    assert_eq!(card.body(), "");
}

/// D23: refreshCardSend disables Send with findings and none checked, and
/// sets the title of every sendable card, a worker's too; held has none.
#[test]
fn d23_send_follows_the_finding_selection() {
    let send = |card: &DelegationCard, busy, selected: &[&str]| {
        let selected = selected.iter().map(|s| s.to_string()).collect();
        card.controls(busy, &selected)
            .into_iter()
            .find(|c| c.label.starts_with("Send") || c.label.starts_with("Queued"))
            .expect("send")
    };
    let card = with_findings();
    let none = send(&card, false, &[]);
    assert!(none.disabled);
    assert_eq!(none.tooltip, "Select at least one finding");
    let one = send(&card, false, &["F1"]);
    assert!(!one.disabled);
    assert_eq!(one.tooltip, "Send the selected findings as a message");
    assert_eq!(
        send(&card, true, &["F1"]).tooltip,
        "Claude is working; it is sent when this turn ends"
    );
    let worker = super::tests::card("implement", "completed");
    let report = send(&worker, false, &[]);
    assert!(!report.disabled);
    assert_eq!(report.tooltip, "Send the selected findings as a message");
    let mut held = card.clone();
    held.held = true;
    let queued = send(&held, false, &["F1"]);
    assert!(queued.disabled);
    assert_eq!(queued.tooltip, "");
}

/// D27/D28: `button.primary` is Send (held too) and Open worker tab while
/// the worker waits; nothing else.
#[test]
fn d27_d28_send_and_a_waiting_workers_open_are_primary() {
    let primary = |card: &DelegationCard| {
        card.controls(false, &["F1".to_string()].into())
            .into_iter()
            .filter(|c| c.primary)
            .map(|c| c.label)
            .collect::<Vec<_>>()
    };
    assert_eq!(primary(&with_findings()), ["Send to Claude"]);
    let mut held = with_findings();
    held.held = true;
    assert_eq!(primary(&held), ["Queued, sends when Claude finishes"]);
    let mut waiting = card("implement", "running");
    assert!(primary(&waiting).is_empty());
    waiting.worker.as_mut().unwrap()["session_state"] = "waiting".into();
    assert_eq!(primary(&waiting), ["Open worker tab"]);
    assert_eq!(primary(&card("implement", "completed")), ["Send to Claude"]);
}
