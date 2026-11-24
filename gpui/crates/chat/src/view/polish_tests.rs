//! TRU-274 "polish" lane: copy, keyboard, sizing and history-window rows
//! against Iced's `assets/agent_chat.html`.
use super::*;
use crate::events::{Entry, HarnessEvent, ItemKind, RuntimeRequestKind, TurnStatus};
use gpui::{TestAppContext, VisualTestContext};
use std::cell::Cell;

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
    cx.simulate_resize(size(px(1200.), px(900.)));
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
fn prompt(text: String) -> Record {
    Record {
        at_ms: None,
        entry: Entry::UserPrompt(text),
    }
}
fn clipboard(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap())
}
fn pixels(value: impl Into<f32>) -> f32 {
    value.into()
}

/// X05: Iced's Cmd+C is the browser's own selection text, never trimmed.
#[gpui::test]
fn x05_plain_selection_keeps_surrounding_whitespace(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![harness(HarnessEvent::TextDelta(
            "```text\n  padded  \n```".into(),
        ))],
        cx,
    );
    draw(cx);
    let text = chat.read_with(cx, |view, cx| {
        view.row_views[0].as_ref().unwrap().read(cx).text.clone()
    });
    text.update_in(cx, |text, window, cx| {
        text.focus_handle().focus(window, cx);
        text.select_all(cx);
    });
    draw(cx);
    cx.simulate_keystrokes("cmd-c");
    draw(cx);
    let copied = clipboard(cx);
    assert!(copied.contains("  padded  "), "trimmed: {copied:?}");
}

/// U11: an ambiguous delivery is marked on its bubble only; Iced adds nothing
/// to the footer for it, while a refusal (nothing was written) still shows.
#[gpui::test]
fn u11_uncertain_delivery_is_not_repeated_in_the_footer(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(Rc::new(|_, _| Receipt::rejected("closed channel")), cx);
        view.set_read_only(None, cx);
        view.composer
            .update(cx, |input, cx| input.set_value("hello", window, cx));
        view.send(window, cx);
    });
    draw(cx);
    assert!(
        cx.debug_bounds("chat-command-error").is_some(),
        "a refused send still shows its reason"
    );
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(|_, _| {
                let mut receipt = Receipt::rejected("synthetic");
                receipt.state = ReceiptState::Uncertain("partial write".into());
                receipt
            }),
            cx,
        );
        view.composer
            .update(cx, |input, cx| input.set_value("hello", window, cx));
        view.send(window, cx);
    });
    draw(cx);
    assert!(
        cx.debug_bounds("chat-command-error").is_none(),
        "the uncertain delivery belongs to the bubble"
    );
}

/// X13: Escape is the page's only when a layer of it is open (find, a menu,
/// the request panel); otherwise Iced forwards it to the host.
#[gpui::test]
fn x13_escape_reaches_the_host_unless_a_page_layer_is_open(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![harness(HarnessEvent::TextDelta("hello needle".into()))],
        cx,
    );
    let hits = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        let hits = hits.clone();
        cx.on_action(move |_: &CloseFind, _| hits.set(hits.get() + 1));
    });
    chat.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert_eq!(hits.get(), 1, "no layer open: the host gets Escape");
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    assert!(chat.read_with(cx, |view, _| view.find_open));
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(!chat.read_with(cx, |view, _| view.find_open));
    assert_eq!(hits.get(), 1, "Escape closed find and went no further");
    chat.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert_eq!(hits.get(), 2, "find is closed again: back to the host");
}

#[gpui::test]
fn x13_escape_stays_with_a_pending_request_panel(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![
            harness(HarnessEvent::ItemStarted {
                id: "t".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            }),
            harness(HarnessEvent::RuntimeRequest {
                request_id: "p".into(),
                tool_use_id: "t".into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    suggestions: vec![],
                    decision_reason: None,
                },
            }),
        ],
        cx,
    );
    let hits = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        let hits = hits.clone();
        cx.on_action(move |_: &CloseFind, _| hits.set(hits.get() + 1));
    });
    chat.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    draw(cx);
    assert!(cx.debug_bounds("pending-request-panel").is_some());
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert_eq!(
        hits.get(),
        0,
        "the request panel keeps Escape from the host"
    );
}

/// W07: Iced's `.earlier`: a centred pill (block fill, strong border, radius
/// 12, padding 3×12, `s(12)`), 10px above the first block.
#[gpui::test]
fn w07_show_earlier_is_a_centred_pill(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        (0..250).map(|i| prompt(format!("prompt {i}"))).collect(),
        cx,
    );
    draw(cx);
    chat.update_in(cx, |view, _, cx| {
        view.scroller
            .update(cx, |state, cx| _ = state.scroll_to_item(99, cx));
    });
    draw(cx);
    let pill = cx.debug_bounds("show-earlier").expect("Show earlier");
    let transcript = cx.debug_bounds("chat-transcript").unwrap();
    let height = pixels(pill.size.height);
    // 12/14 of 15px text at line-height 1.5, plus 3px padding and a 1px border.
    let expected = 15. * 12. / 14. * 1.5 + 6. + 2.;
    assert!(
        (height - expected).abs() < 0.6,
        "pill height {height}, expected {expected}"
    );
    assert!(
        pixels(pill.size.width) < pixels(transcript.size.width) / 2.,
        "the pill is not a full-width button"
    );
    assert!(
        (pixels(pill.center().x) - pixels(transcript.center().x)).abs() < 1.5,
        "the pill is centred"
    );
    let (quads, scale) = cx.update(|window, _| (window.painted_quads(), window.scale_factor()));
    let fill: gpui::Background = gpui::rgb(0x313244).into();
    assert!(
        quads.iter().any(|quad| quad.background == fill
            && (quad.bounds.size.height.0 / scale - height).abs() < 1.),
        "block-colour fill"
    );
}

/// W04: `#messages::-webkit-scrollbar` is 10px wide with a strong-border,
/// radius-4 thumb, always shown while the transcript overflows.
#[gpui::test]
fn w04_transcript_scrollbar_is_ten_px_with_a_strong_border_thumb(cx: &mut TestAppContext) {
    let (_chat, cx) = setup((0..80).map(|i| prompt(format!("prompt {i}"))).collect(), cx);
    draw(cx);
    draw(cx);
    let thumb: gpui::Background = gpui::rgb(0x585b70).into();
    let (quads, scale) = cx.update(|window, _| (window.painted_quads(), window.scale_factor()));
    let found = quads
        .iter()
        .filter(|quad| quad.background == thumb)
        // The bubbles' neighbours share nothing with the thumb's colour; the
        // thumb is the tall, narrow one.
        .find(|quad| quad.bounds.size.height.0 > quad.bounds.size.width.0 * 3.);
    let quad = found.expect("a strong-border scrollbar thumb");
    assert!(
        (quad.bounds.size.width.0 / scale - 10.).abs() < 0.5,
        "thumb width {:?}",
        quad.bounds.size.width
    );
    assert!(
        (quad.corner_radii.top_left.0 / scale - 4.).abs() < 0.5,
        "thumb radius {:?}",
        quad.corner_radii.top_left
    );
}

/// W08: a fold frees room in the document, so whole detached chunks that fit
/// come back (Iced's `foldSegment` tail).
#[gpui::test]
fn w08_a_fold_restores_the_chunks_that_now_fit(cx: &mut TestAppContext) {
    let mut records: Vec<Record> = (0..100).map(|i| prompt(format!("old {i}"))).collect();
    records.push(prompt("work".into()));
    for i in 0..160 {
        records.push(harness(HarnessEvent::ThinkingDelta(format!("think {i}"))));
        records.push(harness(HarnessEvent::ItemStarted {
            id: format!("t{i}"),
            kind: ItemKind::ToolCall {
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("c{i}")}),
            },
        }));
        records.push(harness(HarnessEvent::ItemCompleted {
            id: format!("t{i}"),
            output: "ok".into(),
            is_error: false,
        }));
    }
    let (chat, cx) = setup(records, cx);
    draw(cx);
    let hidden_before = chat.read_with(cx, |view, _| view.history_start);
    assert!(
        hidden_before > 200,
        "the live turn is windowed: {hidden_before}"
    );
    chat.update_in(cx, |view, _, cx| {
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
    chat.read_with(cx, |view, _| {
        let folds = turns::work_folds(&view.model);
        assert_eq!(folds.len(), 1, "the finished turn folds");
        assert_eq!(
            view.history_start, 0,
            "the fold left ~100 blocks, so every hidden chunk fits again"
        );
    });
}

/// X14: a browser scrolls Iced's `#messages` with the cursor, page, Home and
/// End keys; the transcript here has no other key map.
#[gpui::test]
fn x14_keys_scroll_the_transcript_like_a_browser_scroll_box(cx: &mut TestAppContext) {
    let (chat, cx) = setup((0..80).map(|i| prompt(format!("prompt {i}"))).collect(), cx);
    draw(cx);
    let top = |chat: &Entity<ChatView>, cx: &mut VisualTestContext| {
        chat.read_with(cx, |view, cx| {
            let state = view.scroller.read(cx);
            (state.logical_scroll_top(), state.is_following_tail())
        })
    };
    chat.update_in(cx, |view, window, cx| {
        view.scroller
            .update(cx, |state, cx| _ = state.scroll_to_item(30, cx));
        view.focus.focus(window, cx);
    });
    draw(cx);
    let (start, following) = top(&chat, cx);
    assert!(!following);
    cx.simulate_keystrokes("down");
    draw(cx);
    let (after_line, _) = top(&chat, cx);
    let advanced = |from: gpui::ListOffset, to: gpui::ListOffset| {
        (to.item_ix, f32::from(to.offset_in_item)) > (from.item_ix, f32::from(from.offset_in_item))
    };
    assert!(advanced(start, after_line), "down scrolls a line");
    cx.simulate_keystrokes("pagedown");
    draw(cx);
    let (after_page, _) = top(&chat, cx);
    assert!(advanced(after_line, after_page), "pagedown scrolls a page");
    cx.simulate_keystrokes("pageup up");
    draw(cx);
    let (back, _) = top(&chat, cx);
    assert!(advanced(back, after_page), "pageup and up scroll back");
    cx.simulate_keystrokes("home");
    draw(cx);
    let (home, following) = top(&chat, cx);
    assert_eq!(
        (home.item_ix, following),
        (0, false),
        "home goes to the top"
    );
    cx.simulate_keystrokes("end");
    draw(cx);
    assert!(top(&chat, cx).1, "end goes to the bottom and follows");
}

/// Margin gap: Iced's CSS collapses a card's 8px margins into its neighbours';
/// GPUI adds them, so a card read 8px further from the reply above it.
#[gpui::test]
fn cards_collapse_their_margins_into_their_neighbours(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![harness(HarnessEvent::TextDelta("first".into()))], cx);
    chat.update_in(cx, |view, _, cx| {
        view.set_delegations(
            vec![crate::delegation::tests::card("implement", "running")],
            cx,
        )
    });
    chat.update_in(cx, |view, _, cx| {
        view.append_record(&prompt("second".into()), cx)
    });
    draw(cx);
    let ids: Vec<u64> = chat.read_with(cx, |view, _| {
        view.model.rows.iter().map(|row| row.id).collect()
    });
    assert_eq!(ids.len(), 3, "reply, card, prompt");
    let bounds = |id: u64, cx: &mut VisualTestContext| {
        let selector: &'static str = Box::leak(format!("transcript-row-{id}").into_boxed_str());
        cx.debug_bounds(selector).expect(selector)
    };
    let (reply, card, after) = (bounds(ids[0], cx), bounds(ids[1], cx), bounds(ids[2], cx));
    // max(reply 10, card 8), not 10 + 8.
    assert_eq!(f32::from(card.top() - reply.bottom()), 10.);
    // max(card 8, prompt 4), not 8 + 4.
    assert_eq!(f32::from(after.top() - card.bottom()), 8.);
}
