//! Iced's subagent block (`ensureSubagent`/`subagentEvent`) in the model.
use super::*;

pub(crate) fn harness(event: HarnessEvent) -> Record {
    Record {
        at_ms: None,
        entry: Entry::Harness(event),
    }
}
pub(crate) fn tool(id: &str, name: &str, input: Value) -> HarnessEvent {
    HarnessEvent::ItemStarted {
        id: id.into(),
        kind: ItemKind::ToolCall {
            name: name.into(),
            input,
        },
    }
}
pub(crate) fn done(id: &str, is_error: bool) -> HarnessEvent {
    HarnessEvent::ItemCompleted {
        id: id.into(),
        output: "ok".into(),
        is_error,
    }
}
pub(crate) fn turn_done(status: TurnStatus) -> HarnessEvent {
    HarnessEvent::TurnCompleted {
        status,
        usage: Value::Null,
        cost_usd: None,
        rate_limit: None,
    }
}
/// `event` as it arrives from inside the subagents `path` (outermost first).
pub(crate) fn inside(path: &[&str], event: HarnessEvent) -> HarnessEvent {
    path.iter()
        .rev()
        .fold(event, |event, parent| HarnessEvent::SubagentEvent {
            parent_tool_use_id: (*parent).into(),
            event: Box::new(event),
        })
}
fn sub(model: &ChatModel, row: usize) -> &Subagent {
    model.rows[row].tool.as_ref().unwrap().sub.as_ref().unwrap()
}

/// S01–S05: everything a subagent and its own subagents do lands in the
/// root Agent card's block; nothing becomes a transcript row.
#[test]
fn s01_s05_subagent_events_nest_in_the_agent_card_with_iced_state() {
    let records = [
        harness(tool(
            "agent",
            "Agent",
            serde_json::json!({"description": "Review retry", "subagent_type": "reviewer"}),
        )),
        harness(inside(
            &["agent"],
            HarnessEvent::TurnStarted {
                session_id: None,
                model: Some("placeholder-model".into()),
            },
        )),
        harness(inside(
            &["agent"],
            HarnessEvent::TextDelta("First look.".into()),
        )),
        harness(inside(
            &["agent"],
            HarnessEvent::ThinkingDelta("hidden".into()),
        )),
        harness(inside(
            &["agent"],
            tool("read", "Read", serde_json::json!({"file_path": "src/a.rs"})),
        )),
        harness(inside(&["agent"], done("read", false))),
        harness(inside(
            &["agent"],
            tool("child", "Task", serde_json::json!({})),
        )),
        // A grandchild rolls up into the same block, through `subTools`.
        harness(inside(
            &["agent", "child"],
            tool("grep", "Grep", serde_json::json!({"pattern": "retry"})),
        )),
        harness(inside(&["agent", "child"], done("grep", true))),
        harness(inside(
            &["agent", "child"],
            HarnessEvent::TextDelta("  Found it.  \n\n".into()),
        )),
        // An event for a call this chat never saw is dropped.
        harness(inside(&["unknown"], HarnessEvent::TextDelta("lost".into()))),
    ];
    let mut model = ChatModel::from_records(&records);
    assert_eq!(model.rows.len(), 1, "no scoped rows: {:?}", model.rows);
    let block = sub(&model, 0);
    assert_eq!(
        block.description(&model.rows[0].tool.as_ref().unwrap().input),
        "Review retry · reviewer · placeholder-model"
    );
    assert_eq!(block.state, SubState::Running);
    assert!(block.tracked);
    assert_eq!(block.state_text(), "running · 3 tools");
    assert_eq!(
        block
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.tip.as_str(), tool.result))
            .collect::<Vec<_>>(),
        [
            ("Read", "src/a.rs", Some(false)),
            ("Task", "Task", None),
            ("Grep", "retry", Some(true)),
        ]
    );
    assert_eq!(block.text, "First look.\n  Found it.  \n\n");
    assert_eq!(block.latest().as_deref(), Some("Found it."));
    // Tracked: the parent's result does not end it, its own completion does.
    model.apply(&harness(done("agent", false)));
    assert_eq!(sub(&model, 0).state, SubState::Running);
    model.apply(&harness(inside(
        &["agent"],
        turn_done(TurnStatus::Failed("Out of budget".into())),
    )));
    let block = sub(&model, 0);
    assert_eq!(block.state, SubState::Failed);
    assert_eq!(block.state_text(), "failed · 3 tools");
    assert!(block.text.ends_with("\n\nOut of budget"));
    assert_eq!(model.rows.len(), 1, "no notes for a subagent's turn");
}

/// S05: an untracked (foreground) subagent ends with its Agent call; a
/// grandchild's turn moves the root block's state (Iced rolls it up).
#[test]
fn s05_foreground_subagent_ends_with_its_call_and_states_use_iced_words() {
    let mut model =
        ChatModel::from_records(&[harness(tool("agent", "Agent", serde_json::json!({})))]);
    assert_eq!(sub(&model, 0).state_text(), "running");
    assert_eq!(sub(&model, 0).latest(), None);
    model.apply(&harness(done("agent", true)));
    assert_eq!(sub(&model, 0).state, SubState::Failed);
    let mut model = ChatModel::from_records(&[
        harness(tool("agent", "Agent", serde_json::json!({}))),
        harness(inside(&["agent"], tool("child", "Agent", Value::Null))),
        harness(inside(
            &["agent", "child"],
            HarnessEvent::TurnStarted {
                session_id: None,
                model: None,
            },
        )),
    ]);
    assert!(sub(&model, 0).tracked);
    model.apply(&harness(inside(
        &["agent", "child"],
        turn_done(TurnStatus::Interrupted),
    )));
    assert_eq!(sub(&model, 0).state_text(), "stopped · 1 tool");
    let long = "x".repeat(250);
    model.apply(&harness(inside(&["agent"], HarnessEvent::TextDelta(long))));
    assert_eq!(
        sub(&model, 0).latest().unwrap(),
        format!("{}…", "x".repeat(200))
    );
}

/// S02: a request from inside the subagent says `waiting for approval`
/// until it resolves, and asks the view to open the folds around the card.
#[test]
fn s02_subagent_request_shows_waiting_until_resolved() {
    let request = HarnessEvent::RuntimeRequest {
        request_id: "r".into(),
        tool_use_id: "bash".into(),
        kind: RuntimeRequestKind::Permission {
            tool_name: "Bash".into(),
            input: serde_json::json!({}),
            suggestions: vec![],
            decision_reason: None,
        },
    };
    let mut model =
        ChatModel::from_records(&[harness(tool("agent", "Agent", serde_json::json!({})))]);
    model.apply(&harness(inside(
        &["agent"],
        tool("bash", "Bash", serde_json::json!({"command": "ls"})),
    )));
    model.apply(&harness(inside(&["agent"], request)));
    assert_eq!(sub(&model, 0).state_text(), "waiting for approval · 1 tool");
    assert_eq!(model.reopened, [model.rows[0].id]);
    // The request keeps its own (hidden) row for the footer panel.
    assert_eq!(model.pending_rows(), [1]);
    model.apply(&harness(inside(
        &["agent"],
        HarnessEvent::RuntimeRequestResolved {
            request_id: "r".into(),
        },
    )));
    assert_eq!(sub(&model, 0).state_text(), "running · 1 tool");
}
