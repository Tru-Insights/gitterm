use super::*;

#[test]
fn options_and_labels_match_iced() {
    let iced = include_str!("../../../../../assets/agent_chat.html");
    for label in KINDS
        .into_iter()
        .map(|(_, label)| label)
        .chain(REVIEWERS)
        .chain(CLAUDE_MODELS)
        .chain(TARGETS.into_iter().map(|(_, label)| label))
        .chain([
            "Reviewer",
            "Consultant",
            "Target",
            "Focus (optional)",
            "Brief",
            "Cancel",
            "Start review",
            "Ask",
            "base branch",
            "SHA",
            "Codex default model",
            FOCUS_PLACEHOLDER,
            BRIEF_PLACEHOLDER,
        ])
    {
        assert!(iced.contains(label), "Iced label missing: {label}");
    }
    assert_eq!(
        TARGETS.map(|(_, label)| label),
        ["Branch vs", "Uncommitted changes", "Commit"]
    );
    assert_eq!(ReviewKind::Review.submit_label(), "Start review");
    assert_eq!(ReviewKind::Consult.submit_label(), "Ask");
    assert_eq!(ReviewKind::Consult.reviewer_label(), "Consultant");
    assert_eq!(ReviewKind::Review.note_label(), "Focus (optional)");
}

#[test]
fn initial_target_base_hints_and_busy_validation_match_iced() {
    let mut context = ReviewContext {
        base: Some("v6".into()),
        base_source: Some("task".into()),
        ahead: Some(1),
        dirty: true,
        ..Default::default()
    };
    assert_eq!(
        context.base_hint(),
        "Base from this task's base · 1 commit ahead"
    );
    let mut draft = ReviewDraft::from_context(ReviewKind::Review, &context);
    assert_eq!(draft.target, Target::Branch);
    assert_eq!(
        draft.blocked_reason(true),
        Some("Waits until Claude finishes this turn")
    );
    draft.reviewer = ReviewerKind::Codex;
    assert!(draft.submit(true).is_some());
    draft.base = "  ".into();
    assert_eq!(
        draft.blocked_reason(false),
        Some("Fill in the target first")
    );
    draft.target = Target::Commit;
    assert!(draft.submit(false).is_none());
    draft.sha = " abc1234 ".into();
    draft.note = " check errors ".into();
    draft.codex_model = " gpt-6.1-sol ".into();
    assert_eq!(
        draft.submit(false),
        Some(ReviewOutcome::Submit {
            kind: ReviewKind::Review,
            scope: ReviewScope {
                reviewer: ReviewerKind::Codex,
                model: "gpt-6.1-sol".into(),
                target: Some(ReviewTarget::Commit {
                    sha: "abc1234".into()
                })
            },
            note: "check errors".into(),
        })
    );
    assert_eq!(draft.note, " check errors ", "submitting retains the draft");
    context.ahead = Some(0);
    assert_eq!(
        ReviewDraft::from_context(ReviewKind::Review, &context).target,
        Target::Uncommitted
    );
    context.dirty = false;
    assert_eq!(
        ReviewDraft::from_context(ReviewKind::Review, &context).target,
        Target::Branch
    );
    context.base = None;
    assert_eq!(
        ReviewDraft::from_context(ReviewKind::Review, &context).target,
        Target::Uncommitted
    );
    assert_eq!(context.base_hint(), "No base branch found; enter one.");
}

#[test]
fn consult_requires_brief_and_emits_no_diff_target() {
    let mut draft = ReviewDraft::from_context(ReviewKind::Consult, &ReviewContext::default());
    assert_eq!(draft.blocked_reason(false), Some("Write the brief first"));
    draft.note = "  Explain the lifecycle  ".into();
    assert_eq!(
        draft.submit(false),
        Some(ReviewOutcome::Submit {
            kind: ReviewKind::Consult,
            scope: ReviewScope {
                reviewer: ReviewerKind::ClaudeSubagent,
                model: "opus".into(),
                target: None
            },
            note: "Explain the lifecycle".into(),
        })
    );
}
