//! Lane "scrollfind": Iced's follow, Latest, reflow and fold-position rules.
use super::*;
use crate::events::{Entry, HarnessEvent, parse_fixture};
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
    (chat, cx)
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}
fn bounds(selector: &str, cx: &mut VisualTestContext) -> Option<gpui::Bounds<gpui::Pixels>> {
    cx.debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
}
fn top(selector: &str, cx: &mut VisualTestContext) -> f32 {
    f32::from(
        bounds(selector, cx)
            .unwrap_or_else(|| panic!("missing {selector}"))
            .top(),
    )
}
fn click(selector: &str, cx: &mut VisualTestContext) {
    let at = bounds(selector, cx)
        .unwrap_or_else(|| panic!("missing {selector}"))
        .center();
    cx.simulate_click(at, gpui::Modifiers::none());
    draw(cx);
}

/// `count` finished turns: prompt, thinking, `tools` tool calls, a final answer.
pub(super) fn turns(count: usize, tools: usize) -> Vec<Record> {
    let mut lines = Vec::new();
    for turn in 0..count {
        let at = turn as u64 * 10_000;
        lines.push(format!(
            r#"{{"at_ms":{at},"entry":{{"type":"user_prompt","data":"Question {turn}"}}}}"#
        ));
        lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"turn_started","data":{{"session_id":null,"model":null}}}}}}}}"#, at + 100));
        lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"thinking_delta","data":"Thought {turn}"}}}}}}"#, at + 200));
        for tool in 0..tools {
            lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"item_started","data":{{"id":"t{tool}-{turn}","kind":{{"type":"tool_call","name":"Bash","input":{{"command":"check {tool}"}}}}}}}}}}}}"#, at + 300));
            lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"item_completed","data":{{"id":"t{tool}-{turn}","output":"ok\nok\nok","is_error":false}}}}}}}}"#, at + 400));
            if tool % 3 == 2 {
                lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"text_delta","data":"Step {tool} of turn {turn} went fine, so the next step follows.\n\n"}}}}}}"#, at + 450));
            }
        }
        lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"text_delta","data":"Answer {turn}"}}}}}}"#, at + 500));
        lines.push(format!(r#"{{"at_ms":{},"entry":{{"type":"harness","data":{{"type":"turn_completed","data":{{"status":{{"state":"completed"}},"usage":null,"cost_usd":null}}}}}}}}"#, at + 1400));
    }
    parse_fixture(&lines.join("\n")).unwrap()
}

/// The owner's mid-history jump on a "Worked for…" click in a long chat.
/// Closed folds still lay out each hidden row (0px), so a frame can draw
/// more than the 96 row views kept, and dropping the first ones drawn
/// rebuilt the reply at the top of the view every frame. A rebuilt reply
/// over 4 KB parses in the background, so for a frame it had no height and
/// the list clamped the reader's place in it: a jump.
#[gpui::test]
fn rows_drawn_in_one_frame_keep_their_views(cx: &mut TestAppContext) {
    let (chat, cx) = setup(turns(4, 60), cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    let (answer, fold) = chat.read_with(cx, |view, _| {
        let fold = turns::work_folds(&view.model)[1].range.start;
        let answer = (0..fold)
            .rev()
            .find(|i| view.model.rows[*i].kind == RowKind::Assistant)
            .unwrap();
        (answer, fold)
    });
    // A reply at the top, the next turns' closed folds below it.
    chat.update(cx, |view, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_item(answer, cx))
    });
    draw(cx);
    let view_of = |cx: &mut VisualTestContext| {
        chat.read_with(cx, |view, _| {
            view.row_views[answer].as_ref().map(|row| row.entity_id())
        })
    };
    let kept = view_of(cx);
    assert!(kept.is_some());
    let id = chat.read_with(cx, |view, _| view.model.rows[fold].id);
    let selector = format!("work-fold-{id}");
    let before = top(&selector, cx);
    for _ in 0..2 {
        draw(cx);
        assert_eq!(view_of(cx), kept, "the reply's row view was rebuilt");
    }
    click(&selector, cx);
    assert_eq!(view_of(cx), kept, "the reply's row view was rebuilt");
    assert_eq!(top(&selector, cx), before, "the clicked fold moved");
}

fn following(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> bool {
    chat.read_with(cx, |view, cx| view.scroller.read(cx).is_following_tail())
}
fn list_following(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> bool {
    chat.read_with(cx, |view, cx| {
        view.scroller.read(cx).list.is_following_tail()
    })
}
fn distance(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> f32 {
    chat.read_with(cx, |view, cx| {
        let list = &view.scroller.read(cx).list;
        f32::from(list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y)
    })
}
fn wheel(dy: f32, cx: &mut VisualTestContext) {
    let at = bounds("chat-transcript", cx).unwrap().center();
    cx.simulate_event(gpui::ScrollEvent {
        position: at,
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(dy))),
        ..Default::default()
    });
    draw(cx);
}
fn output(text: &str, chat: &Entity<ChatView>, cx: &mut VisualTestContext) {
    chat.update(cx, |view, cx| {
        view.append_event(HarnessEvent::TextDelta(text.into()), cx)
    });
    draw(cx);
}

/// V05: Iced's tool, thinking and delegation disclosures have no
/// `watchOpen`: opening one at the tail leaves the view and `follow` as they
/// are (Latest stays hidden) until the next output scrolls to the bottom.
#[gpui::test]
fn opening_a_card_at_the_tail_keeps_follow_and_hides_latest(cx: &mut TestAppContext) {
    let (chat, cx) = setup(turns(12, 2), cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    chat.update(cx, |view, cx| {
        view.set_delegations(vec![crate::delegation::tests::with_findings()], cx)
    });
    draw(cx);
    let id = chat.read_with(cx, |view, _| view.model.rows.last().unwrap().id);
    let selector = format!("delegation-fold-{id}");
    // The card starts open; close it, still following.
    click(&selector, cx);
    assert!(following(&chat, cx));
    let before = top(&selector, cx);
    click(&selector, cx);
    assert!(
        bounds(&format!("delegation-body-{id}"), cx).is_some(),
        "opened"
    );
    assert!((top(&selector, cx) - before).abs() <= 1., "the card moved");
    assert!(distance(&chat, cx) >= 40., "it opened past the bottom");
    assert!(following(&chat, cx), "follow stays on");
    assert!(bounds("chat-latest", cx).is_none(), "Latest stays hidden");
    // The next output scrolls to the bottom and follows it.
    output("More", &chat, cx);
    assert!(list_following(&chat, cx));
    assert!(distance(&chat, cx) < 1.);
}

/// V04/V05: a work fold is Iced's `watchOpen`: opening it stops following
/// only when what opened runs 40px or more past the bottom.
#[gpui::test]
fn opening_a_fold_at_the_tail_stops_follow_only_past_40px(cx: &mut TestAppContext) {
    for (tools, past) in [(0, false), (6, true)] {
        let (chat, cx) = setup(turns(12, tools), cx);
        cx.simulate_resize(size(px(900.), px(700.)));
        draw(cx);
        assert!(following(&chat, cx));
        let id = chat.read_with(cx, |view, _| {
            let fold = turns::work_folds(&view.model).last().unwrap().range.start;
            view.model.rows[fold].id
        });
        let selector = format!("work-fold-{id}");
        let before = top(&selector, cx);
        click(&selector, cx);
        draw(cx);
        let grew = distance(&chat, cx);
        assert_eq!(grew >= 40., past, "{tools} tools grew {grew}px");
        assert!((top(&selector, cx) - before).abs() <= 1., "the fold moved");
        assert_eq!(following(&chat, cx), !past, "{tools} tools grew {grew}px");
        assert_eq!(bounds("chat-latest", cx).is_some(), past);
    }
}

/// V02: within 40px of the bottom the reader still follows (Latest hidden;
/// the next output scrolls down); further up, a human scroll stops it.
#[gpui::test]
fn wheel_within_40px_of_the_bottom_keeps_follow(cx: &mut TestAppContext) {
    let (chat, cx) = setup(turns(12, 2), cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    assert!(following(&chat, cx));
    wheel(30., cx);
    assert!((distance(&chat, cx) - 30.).abs() < 1.);
    assert!(following(&chat, cx), "30px up still follows");
    assert!(bounds("chat-latest", cx).is_none());
    wheel(60., cx);
    assert!(!following(&chat, cx), "90px up stops following");
    assert!(bounds("chat-latest", cx).is_some());
    output("While reading", &chat, cx);
    assert!(distance(&chat, cx) > 80., "output leaves the reader alone");
    let away = distance(&chat, cx);
    wheel(-(away - 20.), cx);
    assert!(following(&chat, cx), "back within 40px follows again");
    assert!(bounds("chat-latest", cx).is_none());
    output("Next", &chat, cx);
    assert!(list_following(&chat, cx));
    assert!(distance(&chat, cx) < 1.);
}

/// V07: Iced's `showUserPrompt` sets follow even while reading history.
#[gpui::test]
fn a_new_prompt_while_reading_sets_follow(cx: &mut TestAppContext) {
    let (chat, cx) = setup(turns(12, 2), cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    chat.update(cx, |view, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_item(3, cx))
    });
    draw(cx);
    assert!(!following(&chat, cx));
    let top_row = chat.read_with(cx, |view, cx| view.scroller.read(cx).logical_scroll_top());
    chat.update(cx, |view, cx| {
        view.append_record(
            &Record {
                at_ms: None,
                entry: Entry::UserPrompt("Another question".into()),
            },
            cx,
        )
    });
    draw(cx);
    assert!(following(&chat, cx), "a prompt sets follow");
    assert!(bounds("chat-latest", cx).is_none());
    assert_eq!(
        chat.read_with(cx, |view, cx| view.scroller.read(cx).logical_scroll_top()),
        top_row,
        "the prompt alone does not scroll"
    );
    output("Reply", &chat, cx);
    assert!(list_following(&chat, cx));
    assert!(distance(&chat, cx) < 1.);
}

/// V09: Latest during find scrolls to the bottom once; output does not
/// follow while the bar shows, and Latest stays offered.
#[gpui::test]
fn latest_during_find_scrolls_once_without_following(cx: &mut TestAppContext) {
    let (chat, cx) = setup(turns(12, 2), cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    chat.update(cx, |view, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_item(0, cx))
    });
    draw(cx);
    click("chat-latest", cx);
    assert!(distance(&chat, cx) < 1., "Latest reached the bottom");
    assert!(bounds("chat-latest", cx).is_some(), "still not following");
    for _ in 0..3 {
        output("Streaming while find shows\n\n", &chat, cx);
    }
    assert!(distance(&chat, cx) > 20., "output did not follow");
    assert!(!list_following(&chat, cx));
}

fn width(selector: &str, cx: &mut VisualTestContext) -> f32 {
    f32::from(bounds(selector, cx).unwrap().size.width)
}

/// Q03, Q06, Q07: Iced's find label, `index of count` text (`0 of 0`
/// without hits) and its `.find-hit` colours.
#[gpui::test]
fn find_bar_counts_0_of_0_and_uses_iced_label_and_colors(cx: &mut TestAppContext) {
    use crate::view::find::{FIND_TOOLS_LABEL, find_count, find_hit_colors};
    assert_eq!(FIND_TOOLS_LABEL, "Include tool output");
    let (hit, current) = find_hit_colors();
    assert_eq!(
        gpui::Rgba::from(hit),
        gpui::Rgba {
            a: 0.35,
            ..gpui::rgb(0xf9e2af)
        }
    );
    assert_eq!(
        gpui::Rgba::from(current),
        gpui::Rgba {
            a: 0.5,
            ..gpui::rgb(0xfab387)
        }
    );
    let (chat, cx) = setup(turns(3, 1), cx);
    draw(cx);
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, _| find_count(&view.find)),
        "0 of 0"
    );
    cx.simulate_input("zzz");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, _| find_count(&view.find)),
        "0 of 0"
    );
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("Answer");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, _| find_count(&view.find)),
        "1 of 3"
    );
}

/// Q13: Iced's `#chat-find` box: 6×10 padding inside a 1px border, an
/// 8em query and a centred 5em count, wrapping when narrow.
#[gpui::test]
fn find_bar_has_iced_box_minimums_and_wraps(cx: &mut TestAppContext) {
    let (_chat, cx) = setup(turns(3, 1), cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    let em = ChatAppearance::default().size(12.);
    let bar = bounds("find-bar", cx).unwrap();
    let query = bounds("find-query", cx).unwrap();
    assert_eq!(
        f32::from(query.left() - bar.left()),
        11.,
        "10px padding + border"
    );
    assert!(width("find-count", cx) >= 5. * em - 0.5);
    let wide = f32::from(bar.size.height);
    cx.simulate_resize(size(px(260.), px(700.)));
    draw(cx);
    assert!(width("find-query", cx) >= 8. * em - 0.5);
    let narrow = f32::from(bounds("find-bar", cx).unwrap().size.height);
    assert!(narrow > wide + 10., "wraps: {narrow} vs {wide}");
}

/// Q14: while find shows, Iced keeps "Show earlier" at the top, disabled.
#[gpui::test]
fn show_earlier_stays_disabled_while_find_shows(cx: &mut TestAppContext) {
    let records = (0..250)
        .map(|i| Record {
            at_ms: None,
            entry: Entry::UserPrompt(format!("prompt {i}")),
        })
        .collect();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    assert_eq!(chat.read_with(cx, |view, _| view.history_start), 100);
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    chat.update(cx, |view, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_item(0, cx))
    });
    draw(cx);
    let earlier = bounds("show-earlier", cx).expect("Show earlier stays");
    let first = chat.read_with(cx, |view, _| view.model.rows[0].id);
    assert!(
        earlier.bottom()
            <= bounds(&format!("transcript-row-{first}"), cx)
                .unwrap()
                .top()
    );
    click("show-earlier", cx);
    assert_eq!(
        chat.read_with(cx, |view, _| view.history_start),
        100,
        "disabled"
    );
}
