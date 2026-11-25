//! TRU-274 "folds" lane: turn folds, tool runs and the nested subagent
//! block against Iced's `assets/agent_chat.html`.
use super::*;
use crate::events::{HarnessEvent, TurnStatus};
use crate::model::subagent_tests::{done, harness, inside, tool, turn_done};
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
fn bounds(selector: String, cx: &mut VisualTestContext) -> Option<gpui::Bounds<gpui::Pixels>> {
    cx.debug_bounds(Box::leak(selector.into_boxed_str()))
}
fn click(selector: String, cx: &mut VisualTestContext) {
    let target = bounds(selector.clone(), cx).unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(target.center(), gpui::Modifiers::none());
    draw(cx);
}
fn prompt(text: &str) -> Record {
    Record {
        at_ms: None,
        entry: Entry::UserPrompt(text.into()),
    }
}
fn append(chat: &Entity<ChatView>, records: &[Record], cx: &mut VisualTestContext) {
    chat.update(cx, |view, cx| {
        for record in records {
            view.append_record(record, cx);
        }
    });
    draw(cx);
}
fn bash() -> serde_json::Value {
    serde_json::json!({"command": "check"})
}

/// S01, S03, S06, O05: a running Agent card holds Iced's subagent block
/// before its body, closed at first, with the latest line under it; opened,
/// it shows the tool chips and the child text. No worker rows.
#[gpui::test]
fn s01_s03_s06_o05_agent_card_nests_a_closed_subagent_block(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![
            prompt("Go"),
            harness(tool(
                "agent",
                "Agent",
                serde_json::json!({"description": "Review"}),
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
                HarnessEvent::TextDelta("First\nLatest line".into()),
            )),
            harness(inside(
                &["agent"],
                tool("read", "Read", serde_json::json!({"file_path": "a.rs"})),
            )),
            harness(inside(&["agent"], done("read", false))),
        ],
        cx,
    );
    draw(cx);
    let id = chat.read_with(cx, |view, _| {
        assert_eq!(view.model.rows.len(), 2, "prompt and Agent card only");
        view.model.rows[1].id
    });
    let card = bounds(format!("tool-card-{id}"), cx).expect("Agent card");
    let block = bounds(format!("subagent-{id}"), cx).expect("nested block");
    let latest = bounds(format!("subagent-latest-{id}"), cx).expect("latest line");
    let input = bounds(format!("tool-input-{id}"), cx).expect("card body");
    assert!(card.top() < block.top() && block.bottom() <= card.bottom());
    assert!(block.bottom() <= latest.top() && latest.bottom() <= input.top());
    // `border-left: 3px` at the card's inner edge (after the card's own 3px).
    let edge = bounds(format!("subagent-edge-{id}"), cx).unwrap();
    assert_eq!(edge.size.width, px(3.));
    assert_eq!(edge.left(), card.left() + px(3.));
    // Closed: the chips and the text wait in the block's body.
    assert!(bounds(format!("subagent-tool-{id}-0"), cx).is_none());
    assert!(bounds(format!("subagent-text-{id}"), cx).is_none());
    click(format!("subagent-summary-{id}"), cx);
    let chip = bounds(format!("subagent-tool-{id}-0"), cx).expect("tool chip");
    let text = bounds(format!("subagent-text-{id}"), cx).expect("child text");
    assert!(block.top() < chip.top() && chip.bottom() <= text.top());
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[1].as_ref().unwrap().read(cx);
        assert!(row.sub_open && row.open);
    });
}

/// O05: the block stays closed when replayed, and a closed card hides it.
#[gpui::test]
fn o05_replayed_subagent_block_starts_closed_inside_its_closed_card(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![
            harness(tool("agent", "Agent", serde_json::json!({}))),
            harness(inside(&["agent"], HarnessEvent::TextDelta("Report".into()))),
            harness(done("agent", false)),
        ],
        cx,
    );
    draw(cx);
    let id = chat.read_with(cx, |view, _| view.model.rows[0].id);
    assert!(
        bounds(format!("subagent-{id}"), cx).is_none(),
        "card closed"
    );
    click(format!("tool-name-{id}"), cx);
    assert!(bounds(format!("subagent-{id}"), cx).is_some());
    assert!(bounds(format!("subagent-text-{id}"), cx).is_none());
}

/// S07, S08: a roster reveal opens the subagent block too, and flashes a
/// 1px outline over the card without moving it.
#[gpui::test]
fn s08_roster_flash_outlines_the_card_without_moving_it(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![harness(tool("agent", "Agent", serde_json::json!({})))],
        cx,
    );
    chat.update(cx, |view, cx| {
        view.set_roster(
            vec![roster::RosterEntry {
                id: "sub:agent".into(),
                name: "Subagent".into(),
                model: String::new(),
                checkout: String::new(),
                activity: String::new(),
                state: roster::WorkerState::Running,
                started_at_ms: Some(0),
                ended_at_ms: None,
            }],
            cx,
        )
    });
    draw(cx);
    let id = chat.read_with(cx, |view, _| view.model.rows[0].id);
    let before = bounds(format!("tool-card-{id}"), cx).unwrap();
    click("roster-sub:agent".into(), cx);
    let card = bounds(format!("tool-card-{id}"), cx).unwrap();
    // It grows only by the opened subagent block.
    assert_eq!(
        card.origin, before.origin,
        "the flash does not move the card"
    );
    assert_eq!(card.size.width, before.size.width);
    let outline = bounds(format!("flash-outline-{id}"), cx).expect("outline");
    assert_eq!(outline.left(), card.left() - px(1.));
    assert_eq!(outline.size.width, card.size.width + px(2.));
    chat.read_with(cx, |view, cx| {
        assert!(view.row_views[0].as_ref().unwrap().read(cx).sub_open);
    });
    cx.executor().advance_clock(Duration::from_millis(901));
    draw(cx);
    assert!(bounds(format!("flash-outline-{id}"), cx).is_none());
}

/// G06: a live run opened by hand is gone when the turn ends; the finished
/// turn's fold and run start closed.
#[gpui::test]
fn g06_live_run_opened_by_hand_starts_closed_in_the_finished_fold(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![prompt("Go")], cx);
    append(
        &chat,
        &[
            harness(tool("a", "Bash", bash())),
            harness(done("a", false)),
            harness(tool("b", "Bash", bash())),
        ],
        cx,
    );
    let id = chat.read_with(cx, |view, _| view.model.rows[1].id);
    click(format!("tool-run-{id}"), cx);
    chat.read_with(cx, |view, cx| {
        assert!(view.row_views[1].as_ref().unwrap().read(cx).run_open);
    });
    append(
        &chat,
        &[
            harness(done("b", false)),
            harness(HarnessEvent::TextDelta("Done.".into())),
            harness(turn_done(TurnStatus::Completed)),
        ],
        cx,
    );
    click(format!("work-fold-{id}"), cx);
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[1].as_ref().unwrap().read(cx);
        assert!(row.work_open);
        assert_eq!(row.run_summary.as_deref(), Some("Ran 2 commands"));
        assert!(!row.run_open, "fresh closed run");
    });
}

/// G05: tool runs use Iced's 4px vertical margins and their closed/open
/// disclosure indicator is the same chevron path rotated a quarter turn.
#[gpui::test]
fn g05_tool_run_matches_iced_margin_and_chevron_geometry(cx: &mut TestAppContext) {
    let (chat, cx) = setup(resumable_turn(), cx);
    let id = chat.read_with(cx, |view, _| view.model.rows[1].id);
    click(format!("work-fold-{id}"), cx);
    let work_summary = bounds(format!("work-fold-margin-{id}"), cx).unwrap();
    let closed_run = bounds(format!("tool-run-{id}"), cx).unwrap();
    assert_eq!(closed_run.top() - work_summary.bottom(), px(4.));
    click(format!("tool-run-{id}"), cx);
    let summary = bounds(format!("tool-run-{id}"), cx).unwrap();
    let card = bounds(format!("tool-card-{id}"), cx).unwrap();
    assert_eq!(card.top() - summary.bottom(), px(4.));
    chat.read_with(cx, |view, cx| {
        assert!(view.row_views[1].as_ref().unwrap().read(cx).run_open);
    });
}

/// F10: the work summary, body gutter and trailing fold margin follow Iced's
/// independent 4/8 fold and 4/2 body spacing.
#[gpui::test]
fn f10_work_fold_matches_iced_gutter_and_spacing(cx: &mut TestAppContext) {
    let (chat, cx) = setup(resumable_turn(), cx);
    let (first, last, answer) = chat.read_with(cx, |view, _| {
        let fold = turns::work_folds(&view.model)[0].range.clone();
        (
            view.model.rows[fold.start].id,
            view.model.rows[fold.end - 1].id,
            view.model.rows.last().unwrap().id,
        )
    });
    click(format!("work-fold-{first}"), cx);
    click(format!("tool-run-{first}"), cx);
    let summary = bounds(format!("work-fold-margin-{first}"), cx).unwrap();
    let first_body = bounds(format!("work-body-{first}"), cx).unwrap();
    let first_row = bounds(format!("transcript-row-{first}"), cx).unwrap();
    let last_body = bounds(format!("work-body-{last}"), cx).unwrap();
    let answer = bounds(format!("transcript-row-{answer}"), cx).unwrap();
    assert_eq!(first_row.left() - first_body.left(), px(14.));
    assert_eq!(first_body.top() - summary.bottom(), px(4.));
    assert_eq!(answer.top() - last_body.bottom(), px(8.));
}

fn resumable_turn() -> Vec<Record> {
    vec![
        prompt("Go"),
        harness(tool("one", "Bash", bash())),
        harness(done("one", false)),
        harness(tool("agent", "Agent", serde_json::json!({}))),
        harness(done("agent", false)),
        harness(HarnessEvent::TextDelta("Done.".into())),
        harness(turn_done(TurnStatus::Completed)),
    ]
}
fn resume() -> Record {
    harness(inside(
        &["agent"],
        HarnessEvent::TurnStarted {
            session_id: None,
            model: None,
        },
    ))
}
/// The fold and the run holding the Agent card (rows 1..3) are open.
fn opened(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> (bool, bool) {
    chat.read_with(cx, |view, _| {
        let first = view.model.rows[1].id;
        (
            view.open_work.contains(&first),
            view.open_runs.contains(&first),
        )
    })
}

/// F04: a finished subagent resumed live opens the folds around its card.
#[gpui::test]
fn f04_resumed_subagent_opens_the_folds_around_its_card(cx: &mut TestAppContext) {
    let (chat, cx) = setup(resumable_turn(), cx);
    draw(cx);
    assert_eq!(opened(&chat, cx), (false, false));
    append(&chat, &[resume()], cx);
    assert_eq!(opened(&chat, cx), (true, true));
}

/// F04: a replay that ends with the subagent running opens them too.
#[gpui::test]
fn f04_replay_opens_the_folds_around_a_running_subagent(cx: &mut TestAppContext) {
    let mut records = resumable_turn();
    records.push(resume());
    let (chat, cx) = setup(records, cx);
    draw(cx);
    assert_eq!(opened(&chat, cx), (true, true));
}

/// F05: the fold summary shows Iced's red `N failed`.
#[gpui::test]
fn f05_fold_summary_shows_the_failed_count(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![
            prompt("Go"),
            harness(tool("one", "Bash", bash())),
            harness(done("one", false)),
            harness(tool("two", "Bash", bash())),
            harness(done("two", true)),
            harness(turn_done(TurnStatus::Completed)),
        ],
        cx,
    );
    draw(cx);
    let id = chat.read_with(cx, |view, _| view.model.rows[1].id);
    let label = bounds(format!("work-fold-label-{id}"), cx).unwrap();
    let failed = bounds(format!("work-fold-failed-{id}"), cx).expect("failed count");
    assert!(label.left() < failed.left() && failed.right() <= label.right());
}
