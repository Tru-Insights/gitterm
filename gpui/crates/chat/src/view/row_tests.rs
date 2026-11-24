//! TRU-274 "rows" lane: request, note, thinking, tool and prompt blocks
//! against Iced's `assets/agent_chat.html`.
use super::*;
use crate::events::{Entry, HarnessEvent, ItemKind, RuntimeRequestKind, TurnStatus};
use gpui::{TestAppContext, VisualTestContext};

fn setup(
    records: Vec<Record>,
    cx: &mut TestAppContext,
) -> (Entity<ChatView>, &mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        init(cx);
    });
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| ChatView::new(records, window, cx));
        Root::new(chat, window, cx)
    });
    let chat = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<ChatView>().unwrap()
    });
    cx.simulate_resize(size(px(1200.), px(1600.)));
    (chat, cx)
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}
fn harness(event: HarnessEvent) -> Record {
    Record {
        at_ms: None,
        entry: Entry::Harness(event),
    }
}
fn started(id: &str, name: &str, input: serde_json::Value) -> Record {
    harness(HarnessEvent::ItemStarted {
        id: id.into(),
        kind: ItemKind::ToolCall {
            name: name.into(),
            input,
        },
    })
}
fn completed(id: &str, output: &str) -> HarnessEvent {
    HarnessEvent::ItemCompleted {
        id: id.into(),
        output: output.into(),
        is_error: false,
    }
}
fn bounds(selector: String, cx: &mut VisualTestContext) -> Option<gpui::Bounds<gpui::Pixels>> {
    cx.debug_bounds(Box::leak(selector.into_boxed_str()))
}
fn row_view(chat: &Entity<ChatView>, index: usize, cx: &mut VisualTestContext) -> Entity<RowView> {
    chat.update_in(cx, |view, window, cx| view.row(index, window, cx))
}
fn open_run(chat: &Entity<ChatView>, index: usize, cx: &mut VisualTestContext) {
    chat.update(cx, |view, cx| {
        let id = view.model.rows[index].id;
        view.open_runs.insert(id);
        view.sync_runs(cx);
    });
    draw(cx);
}

/// R01–R03, T03 badge: Iced shows a pending request only in the footer
/// panel; the transcript keeps no request row, and the tool card says it waits.
#[gpui::test]
fn r01_r03_requests_stay_in_the_footer_and_badge_their_tool(cx: &mut TestAppContext) {
    let records = vec![
        started("t", "Bash", serde_json::json!({"command": "ls"})),
        harness(HarnessEvent::RuntimeRequest {
            request_id: "p".into(),
            tool_use_id: "t".into(),
            kind: RuntimeRequestKind::Permission {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                suggestions: vec![serde_json::json!({"type": "addRules"})],
                decision_reason: None,
            },
        }),
        started("q", "AskUserQuestion", serde_json::json!({})),
        harness(HarnessEvent::RuntimeRequest {
            request_id: "qr".into(),
            tool_use_id: "q".into(),
            kind: RuntimeRequestKind::Question {
                questions: serde_json::json!([{"question": "Which?", "options": []}]),
            },
        }),
    ];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    open_run(&chat, 0, cx);
    let ids: Vec<u64> = chat.read_with(cx, |view, _| {
        view.model.rows.iter().map(|row| row.id).collect()
    });
    assert!(cx.debug_bounds("pending-request-panel").is_some());
    for request in [1, 3] {
        let row = row_view(&chat, request, cx);
        draw(cx);
        assert!(row.read_with(cx, |row, _| row.row.hidden()));
        let row_bounds = bounds(format!("transcript-row-{}", ids[request]), cx);
        assert!(
            row_bounds.is_none(),
            "request {request} drew a transcript block"
        );
    }
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.model.rows[0].tool.as_ref().unwrap().badge,
            Some("waiting for approval")
        );
        assert_eq!(
            view.model.rows[2].tool.as_ref().unwrap().badge,
            Some("waiting for your answer")
        );
    });
    assert!(bounds(format!("tool-badge-{}", ids[0]), cx).is_some());
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::RuntimeRequestResolved {
                request_id: "p".into(),
            },
            cx,
        )
    });
    draw(cx);
    assert!(bounds(format!("tool-badge-{}", ids[0]), cx).is_none());
    // R09: a resolved request leaves no receipt in the transcript either.
    draw(cx);
    assert!(bounds(format!("transcript-row-{}", ids[1]), cx).is_none());
    chat.read_with(cx, |view, _| {
        assert_eq!(view.model.rows[0].tool.as_ref().unwrap().badge, None);
        assert!(view.model.rows[2].tool.as_ref().unwrap().badge.is_some());
    });
}

/// R04: Iced's `#request-panel`: warn border, radius 8, padding 10×12.
#[gpui::test]
fn r04_request_panel_pads_its_card_10_by_12(cx: &mut TestAppContext) {
    let (_chat, cx) = setup(
        vec![harness(HarnessEvent::RuntimeRequest {
            request_id: "p".into(),
            tool_use_id: "t".into(),
            kind: RuntimeRequestKind::Permission {
                tool_name: "Read".into(),
                input: serde_json::json!({"file_path": "/fixture/a"}),
                suggestions: vec![],
                decision_reason: None,
            },
        })],
        cx,
    );
    draw(cx);
    let panel = cx.debug_bounds("pending-request-panel").unwrap();
    let card = cx.debug_bounds("pending-request-card").unwrap();
    // 1px border plus the padding.
    assert_eq!(card.origin.x - panel.origin.x, px(13.));
    assert_eq!(card.origin.y - panel.origin.y, px(11.));
}

/// O01/O02: a live thinking block starts open, even when the turn ends
/// before the row is first drawn; a replayed one starts closed.
#[gpui::test]
fn o01_live_thinking_opens_and_replayed_thinking_starts_closed(cx: &mut TestAppContext) {
    let replayed = vec![
        harness(HarnessEvent::TurnStarted {
            session_id: None,
            model: None,
        }),
        harness(HarnessEvent::ThinkingDelta("replayed".into())),
    ];
    let (chat, cx) = setup(replayed, cx);
    draw(cx);
    assert!(!row_view(&chat, 0, cx).read_with(cx, |row, _| row.open));
    chat.update(cx, |view, cx| {
        view.append_event(HarnessEvent::TextDelta("between".into()), cx);
        view.append_event(HarnessEvent::ThinkingDelta("live".into()), cx);
        view.append_event(
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            },
            cx,
        );
    });
    draw(cx);
    assert!(row_view(&chat, 2, cx).read_with(cx, |row, _| row.open));
}

/// O03/O04: a tool card starts open, live or replayed, until its result;
/// the result closes it, even one the reader opened, and nothing else does.
#[gpui::test]
fn o03_o04_tool_opens_until_its_result_and_the_result_closes_it(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![started("a", "Bash", serde_json::json!({"command": "one"}))],
        cx,
    );
    draw(cx);
    assert!(row_view(&chat, 0, cx).read_with(cx, |row, _| row.open));
    chat.update(cx, |view, cx| view.append_event(completed("a", "ok"), cx));
    draw(cx);
    let first = row_view(&chat, 0, cx);
    assert!(!first.read_with(cx, |row, _| row.open));
    // Reopened by hand: a later change to the row leaves it open.
    first.update_in(cx, |row, window, cx| {
        row.toggle(&ClickEvent::default(), window, cx)
    });
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            },
            cx,
        )
    });
    draw(cx);
    assert!(first.read_with(cx, |row, _| row.open));
    // Opened by hand while running, then the result arrives.
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::ItemStarted {
                id: "b".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "two"}),
                },
            },
            cx,
        )
    });
    draw(cx);
    let second = row_view(&chat, 1, cx);
    assert!(second.read_with(cx, |row, _| row.open));
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::ItemCompleted {
                id: "b".into(),
                output: "boom".into(),
                is_error: true,
            },
            cx,
        )
    });
    draw(cx);
    assert!(!second.read_with(cx, |row, _| row.open));
}

/// O04 across row eviction: a running card whose view was dropped still
/// closes when its result arrives.
#[gpui::test]
fn o04_result_closes_an_evicted_running_tool(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![started("a", "Bash", serde_json::json!({"command": "one"}))],
        cx,
    );
    draw(cx);
    chat.update(cx, |view, cx| {
        let view_row = view.row_views[0].take().unwrap();
        let id = view.model.rows[0].id;
        view.folded.insert(id, view_row.read(cx).open);
        view.append_event(completed("a", "ok"), cx);
    });
    draw(cx);
    assert!(!row_view(&chat, 0, cx).read_with(cx, |row, _| row.open));
}

/// T03–T05: the summary is the tool's name, its salient argument and, while
/// a request waits, the badge; edits add counts and an absolute-path review.
#[gpui::test]
fn t03_t05_tool_summary_has_name_brief_counts_review_and_badge(cx: &mut TestAppContext) {
    let records = vec![
        started("a", "Bash", serde_json::json!({"command": "cargo test"})),
        harness(completed("a", "ok")),
        started(
            "e",
            "Edit",
            serde_json::json!({"file_path": "/fixture/src/a.rs", "old_string": "a\n", "new_string": "b\n"}),
        ),
        started(
            "r",
            "Edit",
            serde_json::json!({"file_path": "src/b.rs", "old_string": "a\n", "new_string": "b\n"}),
        ),
    ];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    open_run(&chat, 0, cx);
    let ids: Vec<u64> = chat.read_with(cx, |view, _| {
        view.model.rows.iter().map(|row| row.id).collect()
    });
    for id in &ids {
        assert!(bounds(format!("tool-name-{id}"), cx).is_some());
        assert!(bounds(format!("tool-brief-{id}"), cx).is_some());
        assert!(bounds(format!("tool-border-{id}"), cx).is_some());
    }
    // The left border is a 3px bar.
    let bar = bounds(format!("tool-border-{}", ids[0]), cx).unwrap();
    assert_eq!(bar.size.width, px(3.));
    // T09: the body sits 3px border + 10px padding in, 10px + 1px border from the right.
    let card = bounds(format!("tool-card-{}", ids[1]), cx).unwrap();
    let changes = bounds(format!("edit-block-{}-0", ids[1]), cx).unwrap();
    assert_eq!(changes.origin.x - card.origin.x, px(13.));
    assert_eq!(card.top_right().x - changes.top_right().x, px(11.));
    assert!(bounds(format!("edit-review-{}", ids[1]), cx).is_some());
    assert!(
        bounds(format!("edit-review-{}", ids[2]), cx).is_none(),
        "Iced offers review only for an absolute path"
    );
    assert_eq!(
        tool_runs::salient(&chat.read_with(cx, |view, _| view.model.rows[0].tool.clone().unwrap()))
            .map(str::to_owned),
        Some("cargo test".to_string())
    );
}

/// T06/T07: the result `<pre>` scrolls past 320px and shows the first
/// 20,000 characters with Iced's note; find with tools searches all of it.
#[gpui::test]
fn t06_t07_long_result_is_capped_at_320px_and_truncated_until_find(cx: &mut TestAppContext) {
    let output = "line\n".repeat(5_000);
    let (chat, cx) = setup(
        vec![
            started("a", "Bash", serde_json::json!({"command": "yes"})),
            harness(completed("a", &output)),
        ],
        cx,
    );
    draw(cx);
    open_run(&chat, 0, cx);
    let row = row_view(&chat, 0, cx);
    row.update_in(cx, |row, window, cx| {
        row.toggle(&ClickEvent::default(), window, cx)
    });
    draw(cx);
    let id = row.read_with(cx, |row, _| row.row.id);
    let shown = row.read_with(cx, |row, cx| {
        row.output_text
            .as_ref()
            .unwrap()
            .read(cx)
            .rendered_text()
            .as_str()
            .to_string()
    });
    assert!(
        shown.trim_end().ends_with("… (5000 more characters)"),
        "{}",
        &shown[shown.len() - 60..]
    );
    let pre = bounds(format!("tool-output-{id}"), cx).unwrap();
    // 320px of text plus Iced's 6px vertical padding.
    assert!(pre.size.height <= px(332.), "{:?}", pre.size.height);
    chat.update_in(cx, |view, window, cx| {
        view.open_find(&Find, window, cx);
        view.toggle_find_tools(window, cx);
    });
    cx.simulate_input("line");
    draw(cx);
    let full = row.read_with(cx, |row, cx| {
        row.output_text
            .as_ref()
            .unwrap()
            .read(cx)
            .rendered_text()
            .as_str()
            .to_string()
    });
    assert!(!full.contains("more characters"));
    assert_eq!(full.matches("line").count(), 5_000);
}

/// O10: a delegation placeholder starts open, as Iced's `newCard`.
#[gpui::test]
fn o10_delegation_placeholder_starts_open(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update(cx, |view, cx| {
        let changed = view
            .model
            .apply_agent(&gitterm_core::chat::state::AgentEvent::Other(
                serde_json::json!({"kind": "delegation_anchor", "delegation_id": "d1"}),
            ));
        view.refresh_rows(0, true, changed, cx);
    });
    draw(cx);
    assert!(row_view(&chat, 0, cx).read_with(cx, |row, _| row.open));
}

/// O12: Iced's showEdit replaces the diff, so a new input collapses Show all.
#[gpui::test]
fn o12_new_edit_input_collapses_show_all(cx: &mut TestAppContext) {
    let input = |text: &str| serde_json::json!({"file_path": "/fixture/a.rs", "content": format!("{text}\n").repeat(60)});
    let (chat, cx) = setup(vec![started("w", "Write", input("one"))], cx);
    draw(cx);
    let row = row_view(&chat, 0, cx);
    row.update(cx, |row, cx| row.expand_edit(0, cx));
    assert!(row.read_with(cx, |row, _| row.expanded_edits.contains(&0)));
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::ItemInputDelta {
                id: "w".into(),
                partial_json: input("two").to_string(),
            },
            cx,
        )
    });
    draw(cx);
    assert!(row.read_with(cx, |row, _| row.expanded_edits.is_empty()));
}

/// U02, T01, N01: Iced's bottom margins: a prompt 12, thinking 8, an error
/// box and a note 6.
#[gpui::test]
fn u02_t01_n01_blocks_keep_iced_bottom_margins(cx: &mut TestAppContext) {
    let records = vec![
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("hi".into()),
        },
        harness(HarnessEvent::ThinkingDelta("plan".into())),
        harness(HarnessEvent::Error("boom".into())),
        harness(HarnessEvent::MessageWithdrawRefused {
            id: "m".into(),
            reason: "already read".into(),
        }),
        harness(HarnessEvent::TextDelta("after".into())),
    ];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    let ids: Vec<u64> = chat.read_with(cx, |view, _| {
        view.model.rows.iter().map(|row| row.id).collect()
    });
    let rows: Vec<_> = ids
        .iter()
        .map(|id| bounds(format!("transcript-row-{id}"), cx).unwrap())
        .collect();
    let gap = |index: usize| rows[index + 1].origin.y - rows[index].bottom_left().y;
    assert_eq!(gap(0), px(12.));
    assert_eq!(gap(1), px(8.));
    assert_eq!(gap(2), px(6.));
    assert_eq!(gap(3), px(6.));
}

/// U02: prompts fit their content, stay right-aligned, and wrap at 85% of the row.
#[gpui::test]
fn u02_user_prompt_bubbles_fit_content_and_cap_at_85_percent(cx: &mut TestAppContext) {
    let records = vec![
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("Short prompt".into()),
        },
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("A deliberately long prompt ".repeat(100)),
        },
    ];
    let (_chat, cx) = setup(records, cx);
    draw(cx);

    let column = bounds("chat-footer".into(), cx).expect("centered chat column");
    let short = bounds("transcript-row-0".into(), cx).expect("short prompt bubble");
    let long = bounds("transcript-row-1".into(), cx).expect("long prompt bubble");
    let available = column.size.width - px(28.); // messages' horizontal inset
    let cap = available * 0.85;

    assert!(
        short.size.width < cap,
        "short bubble expanded to its max width: short={} cap={} column={}",
        short.size.width,
        cap,
        column.size.width
    );
    assert!((short.right() - (column.right() - px(14.))).abs() < px(2.));
    assert!(
        long.size.width <= cap + px(1.),
        "long bubble exceeded its 85% cap"
    );
}

/// T12: each edit is Iced's `.ed-block`; its lines scroll past 480px even
/// after Show all.
#[gpui::test]
fn t12_edit_diff_is_a_bordered_block_capped_at_480px(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![started(
            "m",
            "MultiEdit",
            serde_json::json!({"file_path": "/fixture/a.rs", "edits": [
                {"old_string": "", "new_string": "line\n".repeat(200)},
                {"old_string": "a\n", "new_string": "b\n"},
            ]}),
        )],
        cx,
    );
    draw(cx);
    open_run(&chat, 0, cx);
    let row = row_view(&chat, 0, cx);
    row.update(cx, |row, cx| row.expand_edit(0, cx));
    draw(cx);
    let id = row.read_with(cx, |row, _| row.row.id);
    let lines = bounds(format!("edit-lines-{id}-0"), cx).unwrap();
    assert!(lines.size.height <= px(480.), "{:?}", lines.size.height);
    assert!(lines.size.height > px(400.));
    let first = bounds(format!("edit-block-{id}-0"), cx).unwrap();
    let second = bounds(format!("edit-block-{id}-1"), cx).unwrap();
    assert_eq!(second.origin.y - first.bottom_left().y, px(6.));
}
