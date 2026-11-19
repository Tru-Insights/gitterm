use super::*;
use crate::events::{Entry, HarnessEvent, parse_fixture};
use gpui::{TestAppContext, VisualTestContext};
use std::time::Instant;

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

struct ChatPair([Entity<ChatView>; 2]);
impl Render for ChatPair {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .children(self.0.iter().enumerate().map(|(index, chat)| {
                div()
                    .id(index)
                    .debug_selector(move || format!("chat-{index}"))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(chat.clone())
            }))
    }
}

#[gpui::test]
fn embedded_chats_keep_click_focus_find_and_copy_in_their_pane(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        init(cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let pair = cx.new(|cx| ChatPair(["First", "Second"].map(|label| {
            cx.new(|cx| ChatView::new(vec![Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::TextDelta(format!("**{label}** needle needle\n\n```rust\nlet label = \"{label}\";\n```"))),
            }], window, cx))
        })));
        Root::new(pair, window, cx)
    });
    cx.simulate_resize(size(px(1440.), px(900.)));
    draw(cx);
    let chats = root.read_with(cx, |root, cx| {
        root.view()
            .clone()
            .downcast::<ChatPair>()
            .unwrap()
            .read(cx)
            .0
            .clone()
    });
    for (index, selector) in ["chat-0", "chat-1"].into_iter().enumerate() {
        // Click empty padding, not a child with its own focus handler.
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(
            bounds.origin + gpui::point(px(3.), px(100.)),
            gpui::Modifiers::none(),
        );
        draw(cx);
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        cx.simulate_input("needle");
        draw(cx);
        chats[index].read_with(cx, |view, _| {
            assert!(view.find_open);
            assert_eq!(view.find.matches.len(), 2);
        });
        cx.simulate_keystrokes("cmd-g");
        draw(cx);
        assert_eq!(
            chats[index].read_with(cx, |view, _| view.find.current),
            Some(1)
        );
        cx.simulate_keystrokes("cmd-shift-g escape");
        draw(cx);
        assert!(!chats[1 - index].read_with(cx, |view, _| view.find_open));
        let text = chats[index].read_with(cx, |view, cx| {
            view.row_views[0].as_ref().unwrap().read(cx).text.clone()
        });
        text.update_in(cx, |text, window, cx| {
            text.focus_handle().focus(window, cx);
            text.select_all(cx);
        });
        draw(cx);
        cx.simulate_keystrokes("cmd-c");
        draw(cx);
        let plain = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
        let label = ["First", "Second"][index];
        assert!(plain.contains(label) && !plain.contains("**"));
        assert!(!plain.contains(["Second", "First"][index]));
        cx.simulate_keystrokes("cmd-shift-c");
        draw(cx);
        let markdown = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
        assert!(markdown.contains(&format!("**{label}**")) && markdown.contains("```rust"));
        let code = format!("let label = \"{label}\";\n");
        let row_id = chats[index].read_with(cx, |view, _| {
            view.row_views[0].as_ref().unwrap().entity_id()
        });
        let selector = Box::leak(format!("copy-code-{row_id}").into_boxed_str());
        let button = cx.debug_bounds(selector).expect("visible code Copy button");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        draw(cx);
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap()),
            code.trim_end()
        );
        cx.update(|window, cx| {
            assert!(chats[index].focus_handle(cx).contains_focused(window, cx));
            assert!(
                !chats[1 - index]
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            );
        });
    }
}

#[gpui::test]
fn virtual_5000_build_layout_find_and_tail_follow(cx: &mut TestAppContext) {
    let start = Instant::now();
    let records = parse_fixture(include_str!("../../fixtures/long-5000.jsonl")).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    let elapsed = start.elapsed();
    eprintln!(
        "TRU-281 5000 rows: fixture parse + model build + initial virtual layout {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "5000-row build/layout exceeded 5s: {elapsed:?}"
    );
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.model.rows.len(), 5000);
        let visited = view.row_views.iter().flatten().count();
        assert!(visited < 100, "virtualization instantiated {visited} rows");
        assert!(view.scroller.read(cx).is_following_tail());
    });
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    assert!(chat.read_with(cx, |view, _| view.find_open));
    cx.simulate_input("row-0001");
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.find.active().unwrap().row, 1);
        assert!(!view.scroller.read(cx).is_following_tail());
        assert_eq!(view.scroller.read(cx).logical_scroll_top().item_ix, 1);
        assert!(view.row_views[1].as_ref().unwrap().read(cx).open);
    });
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("row-000");
    draw(cx);
    cx.simulate_keystrokes("cmd-g");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, _| view.find.active().unwrap().row),
        3
    );
    cx.simulate_keystrokes("cmd-shift-g");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, _| view.find.active().unwrap().row),
        1
    );
    let before = chat.read_with(cx, |view, cx| view.scroller.read(cx).logical_scroll_top());
    chat.update_in(cx, |view, _, cx| {
        view.append_record(
            &Record {
                at_ms: None,
                entry: Entry::UserPrompt("A new message".into()),
            },
            cx,
        )
    });
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, cx| view
            .scroller
            .read(cx)
            .logical_scroll_top()
            .item_ix),
        before.item_ix
    );
    chat.update_in(cx, |view, _, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx))
    });
    draw(cx);
    assert!(chat.read_with(cx, |view, cx| view.scroller.read(cx).is_following_tail()));
}

#[gpui::test]
fn selection_plain_and_markdown_leave_normal_copy_unchanged(cx: &mut TestAppContext) {
    let source = "**Retry**\n\n| Attempt | Delay |\n| --- | --- |\n| 1 | 100 ms |\n\n```rust\nlet n = 1;\n```";
    let records = vec![Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::TextDelta(source.into())),
    }];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    let row = chat.read_with(cx, |view, _| view.row_views[0].as_ref().unwrap().clone());
    let text = row.read_with(cx, |row, _| row.text.clone());
    text.update_in(cx, |text, window, cx| {
        text.focus_handle().focus(window, cx);
        text.select_all(cx);
    });
    draw(cx);
    cx.simulate_keystrokes("cmd-c");
    draw(cx);
    let plain = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
    assert!(plain.contains("Retry") && plain.contains("let n = 1;"));
    assert!(!plain.contains("```rust") && !plain.contains("**Retry**"));
    // The context menu calls this same handler without changing selection.
    chat.update_in(cx, |view, window, cx| view.copy_selection(true, window, cx));
    let markdown = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
    assert!(markdown.contains("| Attempt | Delay |"), "{markdown}");
    assert!(markdown.contains("```rust"), "{markdown}");
    chat.update_in(cx, |view, window, cx| {
        view.copy_selection(false, window, cx)
    });
    assert_eq!(
        plain,
        cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap())
    );
}

#[gpui::test]
fn collapse_diff_and_replay_preserve_find(cx: &mut TestAppContext) {
    let records = parse_fixture(include_str!("../../fixtures/showcase.jsonl")).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.update_in(cx, |view, window, cx| {
        view.open_find(&Find, window, cx);
    });
    cx.simulate_input("saturating_add");
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[view.find.active().unwrap().row]
            .as_ref()
            .unwrap()
            .read(cx);
        assert!(row.open);
        assert_eq!(row.diffs.len(), 2);
        assert!(row.diff_error.is_none(), "{:?}", row.diff_error);
        assert!(!row.text.read(cx).rendered_text().is_empty());
    });
    let matched_row = chat.read_with(cx, |view, _| view.find.active().unwrap().row);
    let matched_view = chat.read_with(cx, |view, _| {
        view.row_views[matched_row].as_ref().unwrap().clone()
    });
    matched_view.update_in(cx, |row, window, cx| {
        row.toggle(&ClickEvent::default(), window, cx)
    });
    chat.update_in(cx, |view, _, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx))
    });
    draw(cx);
    chat.update_in(cx, |view, _, cx| {
        view.scroller.update(cx, |state, cx| {
            state.scroll_to_item(matched_row, cx);
        })
    });
    draw(cx);
    assert!(!matched_view.read_with(cx, |row, _| row.open));
    // Invoke the real button handler; all timers run on GPUI's deterministic executor.
    chat.update_in(cx, |view, window, cx| {
        view.start_replay(&ClickEvent::default(), window, cx)
    });
    for _ in 0..40 {
        cx.executor().advance_clock(Duration::from_millis(40));
        draw(cx);
    }
    assert!(chat.read_with(cx, |view, _| view.replay.is_none()));
    assert_eq!(
        chat.read_with(cx, |view, _| view.find.query.clone()),
        "saturating_add"
    );
    assert!(chat.read_with(cx, |view, _| !view.find.matches.is_empty()));
}

#[test]
fn edit_fragments_parse_as_component_diffs_including_paths_and_empty_files() {
    for edit in [
        EditDiff {
            path: "src/a.rs".into(),
            old: "old\nline\n".into(),
            new: "new\nline\n".into(),
        },
        EditDiff {
            path: "path with space/é.txt".into(),
            old: String::new(),
            new: "new".into(),
        },
        EditDiff {
            path: "empty.txt".into(),
            old: "remove".into(),
            new: String::new(),
        },
    ] {
        assert!(
            DiffFile::parse(&edit_patch(&edit)).is_ok(),
            "{}",
            edit_patch(&edit)
        );
    }
}

#[gpui::test]
fn code_copy_feedback_and_cross_row_selection(cx: &mut TestAppContext) {
    let records = vec![
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TextDelta("First **paragraph**".into())),
        },
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("Second message".into()),
        },
    ];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    let rows = chat.read_with(cx, |view, _| {
        view.row_views.iter().flatten().cloned().collect::<Vec<_>>()
    });
    let bounds = rows
        .iter()
        .map(|row| row.read_with(cx, |row, cx| row.text.read(cx).bounds()))
        .collect::<Vec<_>>();
    let start = bounds[0].origin + gpui::point(px(1.), px(5.));
    let end = bounds[1].bottom_right() - gpui::point(px(1.), px(5.));
    cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_move(end, gpui::MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::default());
    draw(cx);
    chat.update_in(cx, |view, window, cx| {
        view.copy_selection(false, window, cx)
    });
    let plain = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
    assert!(
        plain.contains("First paragraph") && plain.contains("Second message"),
        "{plain:?}"
    );
    chat.update_in(cx, |view, window, cx| view.copy_selection(true, window, cx));
    let markdown = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
    assert!(
        markdown.contains("**paragraph**") && markdown.contains("Second message"),
        "{markdown:?}"
    );
    rows[0].update_in(cx, |row, _, cx| {
        row.copy_code(17, "let n = 1;\n".into(), cx)
    });
    assert_eq!(
        cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap()),
        "let n = 1;\n"
    );
    assert!(rows[0].read_with(cx, |row, _| row.copied.contains(&17)));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(1));
    draw(cx);
    assert!(!rows[0].read_with(cx, |row, _| row.copied.contains(&17)));
    assert!(
        cx.update(|_, cx| gpui::base::TextViewDefaults::global(cx).has_code_block_highlighter())
    );
}
