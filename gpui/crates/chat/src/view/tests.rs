use super::*;
use crate::events::{Entry, HarnessEvent, ItemKind, TurnStatus, parse_fixture};
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
        view.toggle_find_tools(window, cx);
    });
    cx.simulate_input("saturating_add");
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[view.find.active().unwrap().row]
            .as_ref()
            .unwrap()
            .read(cx);
        assert!(row.open);
        assert_eq!(row.row.edits().len(), 2);
        assert!(
            row.row
                .edits()
                .iter()
                .all(|edit| !crate::interactions::edit_lines(edit).rows.is_empty())
        );
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
fn edit_fragments_preserve_paths_and_empty_file_line_changes() {
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
        let diff = crate::interactions::edit_lines(&edit);
        let removed = diff
            .rows
            .iter()
            .filter(|line| line.sign == '-')
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        let added = diff
            .rows
            .iter()
            .filter(|line| line.sign == '+')
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert!(!edit.path.is_empty());
        if edit.old.is_empty() {
            assert!(removed.is_empty());
        }
        if edit.new.is_empty() {
            assert!(added.is_empty());
        }
        assert!(!removed.is_empty() || !added.is_empty());
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
    assert!(rows[0].read_with(cx, |row, _| row.copied.contains_key(&CopyTarget::Code(17))));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(1200));
    draw(cx);
    assert!(!rows[0].read_with(cx, |row, _| row.copied.contains_key(&CopyTarget::Code(17))));
    assert!(
        cx.update(|_, cx| gpui::base::TextViewDefaults::global(cx).has_code_block_highlighter())
    );
}

#[gpui::test]
fn folded_turn_remains_searchable_and_escape_restores_its_fold(cx: &mut TestAppContext) {
    let records = vec![
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("question".into()),
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TextDelta("folded answer marker".into())),
        },
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("next question".into()),
        },
    ];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    let (id, initial_ids) = chat.read_with(cx, |view, _| {
        (
            view.model.rows[0].id,
            view.model.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        )
    });
    chat.update_in(cx, |view, _, cx| view.toggle_turn(id, cx));
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(view.folded_turns.contains(&id));
        assert!(view.row_views[1].as_ref().unwrap().read(cx).hidden_by_turn);
        assert_eq!(
            view.model.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            initial_ids
        );
    });
    chat.update_in(cx, |view, window, cx| {
        view.open_find(&Find, window, cx);
        view.change_find("marker".into(), window, cx);
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.find.active().unwrap().row, 1);
        assert!(!view.folded_turns.contains(&id));
        assert!(!view.row_views[1].as_ref().unwrap().read(cx).hidden_by_turn);
    });
    chat.update_in(cx, |view, window, cx| {
        view.close_find(&CloseFind, window, cx)
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(view.folded_turns.contains(&id));
        assert!(view.row_views[1].as_ref().unwrap().read(cx).hidden_by_turn);
        assert_eq!(
            view.model.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            initial_ids
        );
    });
}

#[gpui::test]
fn composer_keys_draft_read_only_stop_and_intent_ownership(cx: &mut TestAppContext) {
    use std::{cell::RefCell, rc::Rc};
    let intents = Rc::new(RefCell::new(Vec::new()));
    let (chat, cx) = setup(vec![], cx);
    let captured = intents.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                let mut r = Receipt::rejected("synthetic");
                r.state = ReceiptState::Written;
                r
            }),
            cx,
        );
        view.set_read_only(None, cx);
        view.composer
            .update(cx, |input, cx| input.focus(window, cx));
    });
    draw(cx);
    cx.simulate_input("first");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("second");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "first\nsecond"
    );
    assert!(intents.borrow().is_empty());
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        *intents.borrow(),
        vec![ChatIntent::Send {
            text: "first\nsecond".into(),
            images: vec![]
        }]
    );
    chat.read_with(cx, |view, cx| {
        assert!(
            view.model.rows.is_empty(),
            "send must not echo into transcript"
        );
        assert!(view.composer.read(cx).value().is_empty());
    });
    cx.simulate_keystrokes("enter");
    cx.simulate_input("retained draft");
    chat.update_in(cx, |view, window, cx| {
        view.append_event(
            HarnessEvent::TurnStarted {
                session_id: None,
                model: None,
            },
            cx,
        );
        view.stop(cx);
        view.set_read_only(Some("Archived session".into()), cx);
        view.send(window, cx);
        view.stop(cx);
    });
    draw(cx);
    cx.simulate_input("blocked");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(intents.borrow().len(), 2);
    assert_eq!(intents.borrow()[1], ChatIntent::Stop);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.composer.read(cx).value().as_str(), "retained draft");
        assert_eq!(view.read_only_reason.as_deref(), Some("Archived session"));
    });
    chat.update_in(cx, |view, _, cx| {
        view.append_event(
            HarnessEvent::TurnCompleted {
                status: crate::events::TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            },
            cx,
        );
        assert!(!view.turn_running);
    });
}

#[gpui::test]
fn streaming_deltas_show_jump_without_moving_reader_and_keep_rows_bounded(cx: &mut TestAppContext) {
    let records = parse_fixture(include_str!("../../fixtures/long-5000.jsonl")).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.update_in(cx, |view, _, cx| {
        view.append_event(HarnessEvent::TextDelta(" tail".into()), cx)
    });
    draw(cx);
    assert!(chat.read_with(cx, |view, cx| view.scroller.read(cx).is_following_tail()));
    chat.update_in(cx, |view, _, cx| {
        view.scroller
            .update(cx, |state, cx| state.scroll_to_item(20, cx))
    });
    draw(cx);
    let before = chat.read_with(cx, |view, cx| {
        view.scroller.read(cx).logical_scroll_top().item_ix
    });
    chat.update_in(cx, |view, _, cx| {
        view.append_event(
            HarnessEvent::TextDelta(" delta without a new row".into()),
            cx,
        )
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(view.new_messages);
        assert!(!view.scroller.read(cx).is_following_tail());
        assert_eq!(view.scroller.read(cx).logical_scroll_top().item_ix, before);
    });
    chat.update_in(cx, |view, _, cx| view.jump(cx));
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(!view.new_messages);
        assert!(view.scroller.read(cx).is_following_tail());
    });
    for index in (0..5000).step_by(40) {
        chat.update_in(cx, |view, window, cx| {
            view.row(index, window, cx);
        });
        draw(cx);
    }
    assert!(chat.read_with(cx, |view, _| view.row_views.iter().flatten().count() <= 96));
}

#[gpui::test]
fn rejected_send_retains_images_and_draft_and_receipts_preserve_new_edits(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(Rc::new(|_, _| Receipt::rejected("closed channel")), cx);
        view.set_read_only(None, cx);
        view.composer.update(cx, |input, cx| {
            input.set_value("original draft", window, cx)
        });
        let images = vec![gitterm_core::chat::types::ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        }];
        view.set_images(images.clone(), cx).unwrap();
        view.send(window, cx);
        assert_eq!(view.composer.read(cx).value().as_str(), "original draft");
        assert_eq!(view.images, images);
        assert!(!view.submission_blocked());
        view.set_intent_callback(
            Rc::new(|_, _| {
                let mut r = Receipt::rejected("synthetic");
                r.state = ReceiptState::ChannelAccepted;
                r
            }),
            cx,
        );
        view.send(window, cx);
        let mut receipt = view.submission.clone().unwrap();
        assert!(view.submission_blocked());
        view.composer
            .update(cx, |input, cx| input.set_value("newer draft", window, cx));
        receipt.state = ReceiptState::Rejected("zero bytes written".into());
        view.update_receipt(receipt, window, cx);
        assert_eq!(view.composer.read(cx).value().as_str(), "newer draft");
        assert_eq!(view.images, images);
        view.send(window, cx);
        let mut receipt = view.submission.clone().unwrap();
        receipt.state = ReceiptState::Uncertain("partial write".into());
        view.update_receipt(receipt, window, cx);
        assert!(view.submission_blocked());
        assert_eq!(view.composer.read(cx).value().as_str(), "newer draft");
        assert_eq!(view.images, images);
    });
    draw(cx);
    assert!(
        cx.debug_bounds("chat-command-error").is_some(),
        "delivery errors stay visible after telemetry chrome is removed"
    );
}

#[gpui::test]
fn completed_before_written_reconciles_submitted_draft_and_preserves_new_edits(
    cx: &mut TestAppContext,
) {
    use crate::adapter::{Admission, ChatAdapter, DeliveryOutcome};
    use gitterm_core::{
        chat::{state::AgentEvent, types::*},
        session,
    };
    let (chat, cx) = setup(vec![], cx);
    for edit in 0..3 {
        let mut adapter = ChatAdapter::default();
        let target = session::Target {
            tab_id: 1,
            session_uid: "synthetic".into(),
            task_id: None,
            task_session_id: None,
        };
        let stamp = adapter.begin_runtime(target.clone());
        let images = vec![ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        }];
        let prepared = adapter
            .admit(
                ChatIntent::Send {
                    text: "submitted draft".into(),
                    images: images.clone(),
                },
                Admission {
                    authority: session::State {
                        target: target.clone(),
                        control: session::TurnControl::Harness,
                        task_live: false,
                        last_tab: false,
                    },
                    config: gitterm_core::chat::types::AgentBackendConfig::Claude {
                        model: String::new(),
                        permission_mode: Some("default".into()),
                        effort: None,
                    },
                    process_running: true,
                    just_spawned: false,
                    message_id: "synthetic-message".into(),
                },
            )
            .unwrap_or_else(|r| panic!("{r:?}"));
        let (receipt, _) = adapter.execute(prepared, &target, 1000, |_| Ok(()));
        let attempt_id = receipt.id;
        chat.update_in(cx, |view, window, cx| {
            view.set_intent_callback(Rc::new(move |_, _| receipt.clone()), cx);
            view.set_read_only(None, cx);
            view.composer.update(cx, |input, cx| {
                input.set_value("submitted draft", window, cx)
            });
            view.set_images(images.clone(), cx).unwrap();
            view.send(window, cx);
            if edit == 1 {
                view.composer
                    .update(cx, |input, cx| input.set_value("newer draft", window, cx));
            } else if edit == 2 {
                view.set_images(vec![], cx).unwrap();
            }
            let update = adapter
                .receive(
                    &stamp,
                    &target,
                    AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                        id: "synthetic-message".into(),
                        state: MessageState::Completed,
                    }),
                    None,
                    |_, _| {},
                )
                .unwrap();
            view.present(&stamp, adapter.state(), update, window, cx);
            assert_eq!(
                view.submission.as_ref().unwrap().state,
                ReceiptState::Resolved
            );
            assert!(!view.submission_blocked());
            assert_eq!(
                view.composer.read(cx).value().as_str(),
                // Send already emptied the box (Iced); only a newer edit remains.
                match edit {
                    1 => "newer draft",
                    _ => "",
                }
            );
            assert_eq!(view.images, vec![]);
            assert!(view.submitted_draft.is_none());
        });
        assert!(
            adapter
                .delivery(attempt_id, &target, DeliveryOutcome::Written)
                .is_none()
        );
    }
}

#[gpui::test]
fn replay_without_revision_change_retains_uncertain_card_by_request(cx: &mut TestAppContext) {
    use crate::adapter::ChatAdapter;
    use gitterm_core::{
        chat::{
            state::AgentEvent,
            types::{AgentBackend, AgentBackendConfig},
        },
        session,
    };
    let (chat, cx) = setup(vec![], cx);
    let mut adapter = ChatAdapter::default();
    let target = session::Target {
        tab_id: 1,
        session_uid: "synthetic".into(),
        task_id: None,
        task_session_id: None,
    };
    adapter.bind_session(Some("synthetic-history".into()));
    let stamp = adapter.begin_runtime(target.clone());
    let (ticket, _) = adapter
        .begin_history(
            target.clone(),
            AgentBackendConfig::Claude {
                model: String::new(),
                permission_mode: None,
                effort: None,
            },
        )
        .unwrap();
    let update = adapter.receive(&stamp, &target, AgentEvent::Harness(HarnessEvent::RuntimeRequest {
        request_id: "synthetic-request".into(), tool_use_id: "tool".into(),
        kind: crate::events::RuntimeRequestKind::Question { questions: serde_json::json!([{ "question":"Which?", "options":[{"label":"A"}] }]) },
    }), None, |_, _| {}).unwrap();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let captured = calls.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |_, _| {
                captured.set(captured.get() + 1);
                let mut receipt = Receipt::rejected("synthetic");
                receipt.state = ReceiptState::Uncertain("partial write".into());
                receipt
            }),
            cx,
        );
        view.set_read_only(None, cx);
        view.present(&stamp, adapter.state(), update, window, cx);
        view.row(0, window, cx);
    });
    let original = chat.read_with(cx, |view, _| view.cards[&0].clone());
    original.update(cx, |card, cx| {
        card.emit(
            ChatIntent::DismissQuestion {
                request_id: "synthetic-request".into(),
            },
            cx,
        )
    });
    assert_eq!(calls.get(), 1);
    let revision = adapter.state().revision;
    let update = adapter
        .complete_history(
            &ticket,
            &target,
            AgentBackend::Claude,
            Ok(vec![AgentEvent::user_prompt("earlier history")]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(adapter.state().revision, revision);
    chat.update_in(cx, |view, window, cx| {
        view.present(&stamp, adapter.state(), update, window, cx);
        assert_eq!(view.model.rows.len(), 2);
        assert_eq!(view.cards[&1], original);
        assert!(!view.cards.contains_key(&0));
        view.cards[&1].update(cx, |card, cx| {
            card.emit(
                ChatIntent::DismissQuestion {
                    request_id: "synthetic-request".into(),
                },
                cx,
            )
        });
        assert_eq!(
            calls.get(),
            1,
            "uncertain card must remain locked after prepend"
        );
    });
}

#[gpui::test]
fn pending_request_and_ime_block_enter_and_button_and_shift_tab_never_changes_mode(
    cx: &mut TestAppContext,
) {
    use std::{cell::RefCell, rc::Rc};
    let intents = Rc::new(RefCell::new(Vec::new()));
    let captured = intents.clone();
    let (chat, cx) = setup(vec![], cx);
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                Receipt::rejected("synthetic")
            }),
            cx,
        );
        view.set_read_only(None, cx);
        view.composer.update(cx, |input, cx| {
            input.set_value("retained draft", window, cx);
            input.focus(window, cx);
        });
        view.append_event(
            HarnessEvent::RuntimeRequest {
                request_id: "gate".into(),
                tool_use_id: "tool".into(),
                kind: crate::events::RuntimeRequestKind::Permission {
                    tool_name: "Write".into(),
                    input: serde_json::json!({}),
                    suggestions: vec![],
                    decision_reason: None,
                },
            },
            cx,
        );
    });
    draw(cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    let button = cx.debug_bounds("composer-send").unwrap();
    cx.simulate_click(button.center(), gpui::Modifiers::none());
    draw(cx);
    assert!(intents.borrow().is_empty());
    chat.update_in(cx, |view, window, cx| {
        assert_eq!(view.composer.read(cx).value().as_str(), "retained draft");
        view.append_event(
            HarnessEvent::RuntimeRequestResolved {
                request_id: "gate".into(),
            },
            cx,
        );
        view.composer.update(cx, |input, cx| {
            input.focus(window, cx);
            input.replace_and_mark_text_in_range(None, "漢", Some(1..1), window, cx);
        });
        assert!(view.send_blocked(window, cx));
        // The same method is the button handler, even before the next draw.
        view.send(window, cx);
    });
    draw(cx);
    cx.simulate_keystrokes("enter shift-tab");
    draw(cx);
    assert!(intents.borrow().is_empty());
    chat.update_in(cx, |view, window, cx| {
        view.composer.update(cx, |input, cx| {
            input.unmark_text(window, cx);
            input.focus(window, cx);
            input.set_value("/checkpoint arg", window, cx);
        });
    });
    cx.simulate_keystrokes("shift-tab");
    draw(cx);
    assert!(intents.borrow().is_empty());
    chat.update_in(cx, |view, window, cx| view.send(window, cx));
    assert!(
        matches!(&intents.borrow()[0], ChatIntent::Send {text, ..} if text == "/checkpoint arg")
    );
}

#[gpui::test]
fn slash_completion_only_edits_draft_and_retains_arguments_until_send(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update_in(cx, |view, window, cx| {
        view.set_read_only(None, cx);
        view.select_slash("checkpoint", window, cx);
        assert_eq!(view.composer.read(cx).value().as_str(), "/checkpoint ");
        view.composer.update(cx, |input, cx| {
            input.replace_and_mark_text_in_range(None, "漢", Some(1..1), window, cx);
        });
        let draft = view.composer.read(cx).value();
        view.select_slash("other", window, cx);
        assert_eq!(view.composer.read(cx).value(), draft);
    });
}

#[gpui::test]
fn slash_keyboard_completion_runs_no_arg_commands_and_fills_argument_commands(
    cx: &mut TestAppContext,
) {
    use std::{cell::RefCell, rc::Rc};
    let intents = Rc::new(RefCell::new(Vec::new()));
    let captured = intents.clone();
    let (chat, cx) = setup(
        vec![Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::Ready {
                session_id: None,
                permission_mode: Some("default".into()),
                models: vec![],
                modes: vec![],
                commands: vec![
                    serde_json::json!({"name":"clear","description":"Clear"}),
                    serde_json::json!({"name":"checkpoint","argumentHint":"[message]"}),
                ],
            }),
        }],
        cx,
    );
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                Receipt::rejected("synthetic")
            }),
            cx,
        );
        view.set_read_only(None, cx);
        view.composer.update(cx, |input, cx| {
            input.set_value("/", window, cx);
            input.set_selected_range(1..1, cx);
            input.focus(window, cx);
        });
    });
    draw(cx);
    cx.simulate_keystrokes("down enter");
    draw(cx);
    assert!(intents.borrow().is_empty());
    assert_eq!(
        chat.read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "/checkpoint "
    );
    chat.update_in(cx, |view, window, cx| {
        view.composer.update(cx, |input, cx| {
            input.set_value("/cl", window, cx);
            input.set_selected_range(3..3, cx);
        })
    });
    draw(cx);
    cx.simulate_keystrokes("tab");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "/clear "
    );
    assert!(intents.borrow().is_empty());
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(matches!(&intents.borrow()[0], ChatIntent::Send { text, .. } if text == "/clear "));
    chat.update_in(cx, |view, window, cx| {
        view.composer.update(cx, |input, cx| {
            input.set_value("/cl", window, cx);
            input.set_selected_range(3..3, cx);
        })
    });
    draw(cx);
    cx.simulate_keystrokes("escape enter");
    draw(cx);
    assert!(matches!(&intents.borrow()[1], ChatIntent::Send { text, .. } if text == "/cl"));
}

#[gpui::test]
fn pending_panel_is_independent_of_transcript_and_advances_on_resolution(cx: &mut TestAppContext) {
    use crate::adapter::ChatAdapter;
    use gitterm_core::{chat::state::AgentEvent, session};
    use std::cell::RefCell;
    let (chat, cx) = setup(vec![], cx);
    let mut adapter = ChatAdapter::default();
    let target = session::Target {
        tab_id: 1,
        session_uid: "synthetic-panel".into(),
        task_id: None,
        task_session_id: None,
    };
    let stamp = adapter.begin_runtime(target.clone());
    let intents = Rc::new(RefCell::new(vec![]));
    let captured = intents.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                let mut r = Receipt::rejected("fixture");
                r.state = ReceiptState::ChannelAccepted;
                r
            }),
            cx,
        );
        view.set_read_only(None, cx);
    });
    for id in ["first", "second"] {
        let update = adapter
            .receive(
                &stamp,
                &target,
                AgentEvent::Harness(HarnessEvent::RuntimeRequest {
                    request_id: id.into(),
                    tool_use_id: id.into(),
                    kind: crate::events::RuntimeRequestKind::Permission {
                        tool_name: "Read".into(),
                        input: serde_json::json!({}),
                        suggestions: vec![],
                        decision_reason: None,
                    },
                }),
                None,
                |_, _| {},
            )
            .unwrap();
        chat.update_in(cx, |view, window, cx| {
            view.present(&stamp, adapter.state(), update, window, cx)
        });
    }
    chat.update_in(cx, |view, window, cx| {
        view.row_views.fill(None);
        view.cards.clear();
        view.pending_panel(window, cx);
        assert!(view.row_views.iter().all(Option::is_none));
        assert_eq!(view.cards.len(), 1);
    });
    draw(cx);
    assert!(cx.debug_bounds("pending-request-panel").is_some());
    chat.update_in(cx, |view, _, cx| {
        assert_eq!(view.cards.len(), 1);
        view.cards[&0].update(cx, |card, cx| {
            card.emit(
                ChatIntent::Permission {
                    request_id: "first".into(),
                    decision: crate::PermissionDecision::Allow,
                },
                cx,
            )
        });
        assert_eq!(view.model.pending_rows(), vec![0, 1]);
    });
    assert!(
        matches!(&intents.borrow()[0], ChatIntent::Answer { identity, .. } if identity.request_id == "first" && identity.owner.is_empty())
    );
    let update = adapter
        .receive(
            &stamp,
            &target,
            AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved {
                request_id: "first".into(),
            }),
            None,
            |_, _| {},
        )
        .unwrap();
    chat.update_in(cx, |view, window, cx| {
        view.present(&stamp, adapter.state(), update, window, cx);
        assert_eq!(view.model.pending_rows(), vec![1]);
        assert!(view.cards.contains_key(&1));
    });
}

#[gpui::test]
fn deny_enter_uses_focused_reason_and_preserves_composer_draft(cx: &mut TestAppContext) {
    use std::cell::RefCell;
    let (chat, cx) = setup(vec![], cx);
    let intents = Rc::new(RefCell::new(vec![]));
    let captured = intents.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                Receipt::rejected("fixture")
            }),
            cx,
        );
        view.set_read_only(None, cx);
        view.composer.update(cx, |input, cx| {
            input.set_value("retained draft", window, cx)
        });
        view.append_event(
            HarnessEvent::RuntimeRequest {
                request_id: "deny".into(),
                tool_use_id: "deny".into(),
                kind: crate::events::RuntimeRequestKind::Permission {
                    tool_name: "Read".into(),
                    input: serde_json::json!({}),
                    suggestions: vec![],
                    decision_reason: None,
                },
            },
            cx,
        );
    });
    draw(cx);
    let reason = cx.debug_bounds("deny-reason").unwrap();
    cx.simulate_click(reason.center(), gpui::Modifiers::none());
    cx.simulate_input("  typed reason  ");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(intents.borrow().len(), 1);
    assert!(
        matches!(&intents.borrow()[0], ChatIntent::Permission { decision:crate::PermissionDecision::Deny { reason }, .. } if reason == "  typed reason  ")
    );
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.composer.read(cx).value().as_str(), "retained draft")
    });
}

#[gpui::test]
fn worker_first_panel_keeps_same_id_parent_answer_reachable(cx: &mut TestAppContext) {
    use crate::adapter::ChatAdapter;
    use gitterm_core::{chat::state::AgentEvent, session};
    use std::cell::RefCell;
    let (chat, cx) = setup(vec![], cx);
    let mut adapter = ChatAdapter::default();
    let target = session::Target {
        tab_id: 1,
        session_uid: "worker-first".into(),
        task_id: None,
        task_session_id: None,
    };
    let stamp = adapter.begin_runtime(target.clone());
    let intents = Rc::new(RefCell::new(vec![]));
    let captured = intents.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                Receipt::rejected("fixture")
            }),
            cx,
        );
        view.set_read_only(None, cx);
    });
    let request = || HarnessEvent::RuntimeRequest {
        request_id: "same".into(),
        tool_use_id: "same".into(),
        kind: crate::events::RuntimeRequestKind::Permission {
            tool_name: "Read".into(),
            input: serde_json::json!({}),
            suggestions: vec![],
            decision_reason: None,
        },
    };
    for event in [
        HarnessEvent::SubagentEvent {
            parent_tool_use_id: "worker".into(),
            event: Box::new(request()),
        },
        request(),
    ] {
        let update = adapter
            .receive(&stamp, &target, AgentEvent::Harness(event), None, |_, _| {})
            .unwrap();
        chat.update_in(cx, |view, window, cx| {
            view.present(&stamp, adapter.state(), update, window, cx)
        });
    }
    chat.update_in(cx, |view, _, cx| {
        assert_eq!(view.model.pending_rows(), vec![0, 1]);
        view.cards[&0].update(cx, |card, cx| {
            card.emit(
                ChatIntent::Permission {
                    request_id: "same".into(),
                    decision: crate::PermissionDecision::Allow,
                },
                cx,
            )
        });
        assert!(intents.borrow().is_empty(), "worker remains gated");
        view.cards[&1].update(cx, |card, cx| {
            card.emit(
                ChatIntent::Permission {
                    request_id: "same".into(),
                    decision: crate::PermissionDecision::Allow,
                },
                cx,
            )
        });
    });
    assert_eq!(intents.borrow().len(), 1);
    assert!(
        matches!(&intents.borrow()[0], ChatIntent::Answer { identity, .. } if identity.request_id == "same" && identity.owner.is_empty())
    );
}

#[gpui::test]
fn iced_footer_chip_order_and_host_roster_render_below_input(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    chat.update_in(cx, |view, _, cx| {
        view.set_preview(cx);
        // A writable composer: a read-only one hides the idle activity row.
        view.set_read_only(None, cx);
        view.set_roster(
            vec![roster::RosterEntry {
                id: "worker-1".into(),
                name: "Worker · Codex".into(),
                model: "gpt-6.1-sol".into(),
                checkout: "/tmp/TRU-274".into(),
                activity: "Running Bash".into(),
                state: roster::WorkerState::Running,
                started_at_ms: crate::composer::now_ms().saturating_sub(186_000),
                ended_at_ms: None,
            }],
            cx,
        );
    });
    draw(cx);
    let selectors = [
        "composer-review",
        "composer-model",
        "composer-effort",
        "composer-mode",
        "composer-checkout",
        "composer-guidelines",
    ];
    let bounds = selectors.map(|selector| cx.debug_bounds(selector).expect("Iced chip rendered"));
    for pair in bounds.windows(2) {
        assert!(pair[0].origin.x < pair[1].origin.x);
    }
    let status = cx.debug_bounds("chat-activity").unwrap();
    let send = cx.debug_bounds("composer-send").unwrap();
    let worker = cx
        .debug_bounds("roster-worker-1")
        .expect("host-fed worker rendered");
    assert!(status.origin.y < send.origin.y);
    assert!(send.origin.y < bounds[0].origin.y);
    assert!(bounds[0].origin.y < worker.origin.y);
    let intents = std::rc::Rc::new(std::cell::Cell::new(0));
    let captured = intents.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(
            Rc::new(move |_, _| {
                captured.set(captured.get() + 1);
                Receipt::rejected("must stay display-only")
            }),
            cx,
        )
    });
    for bounds in bounds {
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    }
    draw(cx);
    assert_eq!(
        intents.get(),
        0,
        "chips cannot mutate acknowledged settings"
    );
    chat.update_in(cx, |view, _, cx| view.set_roster(vec![], cx));
    draw(cx);
    assert!(cx.debug_bounds("roster-worker-1").is_none());
    chat.read_with(cx, |view, _| assert!(view.activity_tick.is_none()));
}

#[gpui::test]
fn roster_click_and_dismiss_emit_typed_outcomes_and_dismissal_survives_refresh(
    cx: &mut TestAppContext,
) {
    use roster::RosterOutcome;
    let (chat, cx) = setup(vec![], cx);
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, outcome: &RosterOutcome, _| {
            captured.borrow_mut().push(outcome.clone())
        })
    });
    let row = roster::RosterEntry {
        id: "worker".into(),
        name: "Worker".into(),
        model: String::new(),
        checkout: String::new(),
        activity: "Working".into(),
        state: roster::WorkerState::Running,
        started_at_ms: 0,
        ended_at_ms: None,
    };
    chat.update_in(cx, |view, _, cx| view.set_roster(vec![row.clone()], cx));
    draw(cx);
    let bounds = cx.debug_bounds("roster-worker").unwrap();
    cx.simulate_click(
        bounds.origin + gpui::point(px(20.), px(8.)),
        gpui::Modifiers::none(),
    );
    draw(cx);
    assert_eq!(
        *events.borrow(),
        [RosterOutcome::Open {
            id: "worker".into()
        }]
    );
    let dismiss = cx.debug_bounds("roster-dismiss-worker").unwrap();
    cx.simulate_click(dismiss.center(), gpui::Modifiers::none());
    draw(cx);
    assert_eq!(
        *events.borrow(),
        [
            RosterOutcome::Open {
                id: "worker".into()
            },
            RosterOutcome::Dismiss {
                id: "worker".into()
            }
        ]
    );
    chat.update_in(cx, |view, _, cx| view.set_roster(vec![row], cx));
    draw(cx);
    assert!(cx.debug_bounds("roster-worker").is_none());
}

#[gpui::test]
fn slash_click_fills_without_sending_and_caret_before_end_hides_menu(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::Ready {
                session_id: None,
                permission_mode: None,
                models: vec![],
                modes: vec![],
                commands: vec![serde_json::json!({"name":"clear"})],
            }),
        }],
        cx,
    );
    let sends = Rc::new(std::cell::Cell::new(0));
    let captured = sends.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_read_only(None, cx);
        view.set_intent_callback(
            Rc::new(move |_, _| {
                captured.set(captured.get() + 1);
                Receipt::rejected("fixture")
            }),
            cx,
        );
        view.composer.update(cx, |input, cx| {
            input.set_value("/cl", window, cx);
            input.set_selected_range(3..3, cx);
            input.focus(window, cx);
        });
    });
    draw(cx);
    let outside = cx.debug_bounds("native-chat").unwrap().origin + gpui::point(px(2.), px(2.));
    cx.simulate_click(outside, gpui::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("slash-commands").is_none());
    let composer = cx.debug_bounds("composer-input").unwrap();
    // Click beyond the draft on the text area's first line.
    cx.simulate_click(
        composer.origin + gpui::point(composer.size.width - px(20.), composer.size.height / 2.),
        gpui::Modifiers::none(),
    );
    draw(cx);
    chat.update_in(cx, |view, window, cx| {
        let input = view.composer.read(cx);
        assert!(
            !view.composer_state.slash_dismissed
                && input.selected_range().start == input.value().len()
                && input.focus_handle(cx).is_focused(window),
            "dismissed={} range={:?} focused={} draft={:?}",
            view.composer_state.slash_dismissed,
            input.selected_range(),
            input.focus_handle(cx).is_focused(window),
            input.value()
        );
    });
    let command = cx.debug_bounds("slash-clear").unwrap();
    cx.simulate_click(command.center(), gpui::Modifiers::none());
    draw(cx);
    assert_eq!(sends.get(), 0);
    assert_eq!(
        chat.read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "/clear "
    );
    chat.update_in(cx, |view, window, cx| {
        view.composer.update(cx, |input, cx| {
            input.set_value("/cl", window, cx);
            input.set_selected_range(1..1, cx);
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("slash-commands").is_none());
}

#[gpui::test]
fn image_paste_suppresses_text_adds_thumbnail_sends_payload_and_remove_restores_focus(
    cx: &mut TestAppContext,
) {
    let (chat, cx) = setup(vec![], cx);
    let intents = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = intents.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_read_only(None, cx);
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                Receipt::rejected("synthetic rejection retains images")
            }),
            cx,
        );
        view.focus_composer(window, cx);
        let image = gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            include_bytes!("../../../../../assets/icon.iconset/icon_32x32.png").to_vec(),
        );
        cx.write_to_clipboard(ClipboardItem {
            entries: vec![
                gpui::ClipboardEntry::Image(image),
                gpui::ClipboardEntry::String("image file name".to_string().into()),
            ],
        });
    });
    draw(cx);
    cx.simulate_keystrokes("cmd-v");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        draw(cx);
        if chat.read_with(cx, |view, _| view.preparing_image.is_none()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "image preparation did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.images.len(), 1, "{:?}", view.composer_state.notice);
        assert!(view.image_previews[0].is_some());
        assert!(
            view.composer.read(cx).value().is_empty(),
            "image paste must consume its text form"
        );
    });
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(
        matches!(&intents.borrow()[0], ChatIntent::Send { text, images } if text.is_empty() && images.len() == 1 && images[0].media_type == "image/png")
    );
    let remove = cx
        .debug_bounds("remove-image-0")
        .expect("thumbnail remove control");
    cx.simulate_click(remove.center(), gpui::Modifiers::none());
    draw(cx);
    chat.update_in(cx, |view, window, cx| {
        assert!(view.images.is_empty());
        assert!(view.image_previews.is_empty());
        assert!(view.composer.read(cx).focus_handle(cx).is_focused(window));
    });
}

#[gpui::test]
fn appearance_restyles_retained_rows_and_scales_message_and_composer_text(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TextDelta("A retained **reply**".into())),
        }],
        cx,
    );
    draw(cx);
    let row = chat.read_with(cx, |view, _| view.row_views[0].clone().unwrap());
    chat.update_in(cx, |view, window, cx| {
        view.composer.update(cx, |input, cx| {
            input.set_value("Keep this draft", window, cx)
        });
        view.set_appearance(
            ChatAppearance {
                light: true,
                font_size: 20.,
            },
            cx,
        );
        assert_eq!(view.row_views[0].as_ref().unwrap(), &row);
    });
    draw(cx);
    row.update_in(cx, |row, _, cx| {
        assert!(row.appearance.light);
        let mut text = row.copyable_text(cx);
        let style = gpui::Styled::style(&mut text).text_style();
        assert_eq!(style.font_size, Some(px(20.).into()));
        assert_eq!(style.font_family.as_deref(), Some(".SystemUIFont"));
    });
    chat.update_in(cx, |view, _, cx| {
        assert_eq!(view.composer.read(cx).value().as_str(), "Keep this draft");
        view.set_appearance(
            ChatAppearance {
                light: false,
                font_size: 12.,
            },
            cx,
        );
    });
    draw(cx);
    row.read_with(cx, |row, _| {
        assert!(!row.appearance.light);
        assert_eq!(row.appearance.font_size, 12.);
    });
}

#[gpui::test]
fn sent_image_payloads_render_real_transcript_thumbnails(cx: &mut TestAppContext) {
    use gitterm_core::chat::types::{ImageAttachment, UserPrompt};
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let images = vec![
        ImageAttachment {
            media_type: "image/png".into(),
            data: PNG.into()
        };
        2
    ];
    let (chat, cx) = setup(
        vec![Record {
            at_ms: None,
            entry: Entry::SubmittedPrompt(UserPrompt {
                text: "Sent with images".into(),
                images: images.clone(),
                id: Some("sent".into()),
            }),
        }],
        cx,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        draw(cx);
        let done = chat.read_with(cx, |view, cx| {
            view.row_views[0]
                .as_ref()
                .is_some_and(|row| row.read(cx).preparing_sent_images.is_none())
        });
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sent thumbnails did not finish decoding"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    draw(cx);
    for selector in ["sent-image-0-0", "sent-image-0-1"] {
        let bounds = cx
            .debug_bounds(selector)
            .expect("sent image thumbnail rendered");
        assert!(bounds.size.width > px(0.) && bounds.size.width <= px(160.));
        assert!(bounds.size.height > px(0.) && bounds.size.height <= px(120.));
    }
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.model.rows[0].images, images);
        assert!(!view.model.rows[0].body.contains("not relayed"));
        let row = view.row_views[0].as_ref().unwrap().read(cx);
        assert_eq!(row.sent_images.len(), 2);
        assert!(row.sent_images.iter().all(Result::is_ok));
    });
}

#[gpui::test]
fn host_width_changes_native_column_and_footer_together(cx: &mut TestAppContext) {
    use gitterm_core::settings::ChatWidth;
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1600.), px(900.)));
    for (width, expected) in [
        (ChatWidth::Narrow, 672.),
        (ChatWidth::Medium, 868.),
        (ChatWidth::Wide, 1036.),
        (ChatWidth::Full, 1580.),
    ] {
        chat.update(cx, |view, cx| view.set_width(width, cx));
        draw(cx);
        let bounds = cx.debug_bounds("native-chat").unwrap();
        assert_eq!(bounds.size.width, px(1600.));
        let footer = cx.debug_bounds("chat-footer").unwrap();
        assert_eq!(footer.size.width, px(expected));
        assert_eq!(footer.origin.x, px((1600. - expected) / 2.));
    }
}

#[gpui::test]
fn setting_pickers_open_choose_dismiss_and_keep_acknowledged_labels(cx: &mut TestAppContext) {
    use crate::pickers::Picker;
    use gitterm_core::chat::types::SettingChange;
    let records = [
        HarnessEvent::Ready { session_id: None, permission_mode: Some("default".into()), models: vec![
            serde_json::json!({"value":"opus", "displayName":"Opus", "supportsEffort":true, "supportedEffortLevels":["low","high"]}),
            serde_json::json!({"value":"sonnet", "displayName":"Sonnet", "supportsEffort":true}),
        ], modes: vec![], commands: vec![] },
        HarnessEvent::ModelChanged("opus".into()),
        HarnessEvent::PermissionModeChanged("default".into()),
        HarnessEvent::EffortChanged { effort: None, applied: None },
    ].into_iter().map(|event| Record { at_ms: None, entry: Entry::Harness(event) }).collect();
    let (chat, cx) = setup(records, cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    let intents = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = intents.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                Receipt::rejected("Fixture setting rejected")
            }),
            cx,
        );
        view.set_read_only(None, cx);
    });
    draw(cx);
    for (chip, picker, option, expected) in [
        (
            "composer-model",
            Picker::Model,
            "setting-option-1",
            SettingChange::Model("sonnet".into()),
        ),
        (
            "composer-effort",
            Picker::Effort,
            "setting-option-2",
            SettingChange::Effort(Some("high".into())),
        ),
        (
            "composer-mode",
            Picker::Mode,
            "setting-option-1",
            SettingChange::Mode("acceptEdits".into()),
        ),
    ] {
        let chip_center = cx.debug_bounds(chip).unwrap().center();
        cx.simulate_click(chip_center, gpui::Modifiers::none());
        draw(cx);
        chat.read_with(cx, |view, _| {
            assert_eq!(view.composer_state.picker, Some(picker))
        });
        let option_center = cx
            .debug_bounds(option)
            .expect("picker option visible")
            .center();
        cx.simulate_click(option_center, gpui::Modifiers::none());
        draw(cx);
        assert_eq!(
            intents.borrow().last(),
            Some(&ChatIntent::ChangeSetting { change: expected })
        );
        assert!(cx.debug_bounds("setting-picker").is_none());
        chat.read_with(cx, |view, _| {
            assert_eq!(view.composer_state.model, "Opus");
            assert_eq!(view.composer_state.mode, "Default");
            assert_eq!(view.composer_state.effort, "Auto · 200k");
        });
    }
    // Escape and an outside click close without emitting another setting request.
    let model_chip = cx.debug_bounds("composer-model").unwrap().center();
    cx.simulate_click(model_chip, gpui::Modifiers::none());
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("setting-picker").is_none());
    cx.simulate_click(model_chip, gpui::Modifiers::none());
    draw(cx);
    let input_center = cx.debug_bounds("composer-input").unwrap().center();
    cx.simulate_click(input_center, gpui::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("setting-picker").is_none());
    assert_eq!(intents.borrow().len(), 3);
    // The registered native action follows the same request path.
    cx.update(|window, cx| window.dispatch_action(Box::new(CycleMode), cx));
    assert_eq!(
        intents.borrow().last(),
        Some(&ChatIntent::ChangeSetting {
            change: SettingChange::Mode("acceptEdits".into())
        })
    );
    chat.update_in(cx, |view, _, cx| {
        view.set_read_only(Some("Session stopped".into()), cx)
    });
    cx.simulate_click(model_chip, gpui::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("setting-picker").is_none());
    assert_eq!(intents.borrow().len(), 4);
}

#[gpui::test]
fn review_chip_opens_iced_composer_and_escape_cancel_close_it(cx: &mut TestAppContext) {
    use crate::review::{ReviewContext, ReviewOutcome};
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    chat.update_in(cx, |view, _, cx| {
        view.set_backend(gitterm_core::chat::types::AgentBackend::Claude, cx)
    });
    let requests = Rc::new(std::cell::Cell::new(0));
    let captured = requests.clone();
    cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, outcome: &ReviewOutcome, _| {
            if matches!(outcome, ReviewOutcome::ContextRequested) {
                captured.set(captured.get() + 1);
            }
        })
        .detach();
    });
    draw(cx);
    let chip = cx.debug_bounds("composer-review").unwrap().center();
    cx.simulate_click(chip, gpui::Modifiers::none());
    draw(cx);
    assert_eq!(requests.get(), 1);
    assert!(cx.debug_bounds("review-composer").is_some());
    chat.update_in(cx, |view, window, cx| {
        view.set_review_context(ReviewContext::default(), window, cx)
    });
    draw(cx);
    assert!(cx.debug_bounds("review-submit").is_some());
    cx.simulate_input("retain focus text");
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("review-composer").is_none());
    chat.read_with(cx, |view, _| {
        assert_eq!(view.review.draft.note, "retain focus text")
    });
    cx.simulate_click(chip, gpui::Modifiers::none());
    chat.update_in(cx, |view, window, cx| {
        view.set_review_context(ReviewContext::default(), window, cx)
    });
    draw(cx);
    let button = cx.debug_bounds("review-cancel").unwrap().center();
    cx.simulate_click(button, gpui::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("review-composer").is_none());
}

#[gpui::test]
fn review_submit_emits_typed_outcome_without_sending_chat_or_clearing_draft(
    cx: &mut TestAppContext,
) {
    use crate::review::*;
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    chat.update_in(cx, |view, window, cx| {
        view.set_backend(gitterm_core::chat::types::AgentBackend::Claude, cx);
        view.composer
            .update(cx, |input, cx| input.set_value("chat draft", window, cx));
        view.toggle_review(window, cx);
        view.set_review_context(ReviewContext::default(), window, cx);
    });
    let outcomes = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = outcomes.clone();
    cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, outcome: &ReviewOutcome, _| {
            captured.borrow_mut().push(outcome.clone())
        })
        .detach();
    });
    draw(cx);
    cx.simulate_input("Check");
    cx.simulate_keystrokes("enter shift-enter");
    cx.simulate_input("errors");
    draw(cx);
    assert!(
        outcomes.borrow().is_empty(),
        "plain Enter and Shift+Enter edit the note"
    );
    cx.simulate_keystrokes("cmd-enter");
    draw(cx);
    assert_eq!(outcomes.borrow().len(), 1);

    let button = cx.debug_bounds("review-submit").unwrap().center();
    cx.simulate_click(button, gpui::Modifiers::none());
    draw(cx);
    assert_eq!(outcomes.borrow().len(), 2);
    let expected = ReviewOutcome::Submit {
        kind: ReviewKind::Review,
        scope: ReviewScope {
            reviewer: gitterm_core::settings::ReviewerKind::ClaudeSubagent,
            model: "opus".into(),
            target: Some(ReviewTarget::Uncommitted),
        },
        note: "Check\n\nerrors".into(),
    };
    assert!(outcomes.borrow().iter().all(|outcome| outcome == &expected));
    chat.read_with(cx, |view, cx| {
        assert!(view.review.open);
        assert_eq!(view.review.draft.note, "Check\n\nerrors");
        assert_eq!(view.composer.read(cx).value(), "chat draft");
        assert!(view.records.is_empty());
    });
}

#[gpui::test]
fn iced_transcript_has_one_expandable_run_no_message_chrome_and_a_table(cx: &mut TestAppContext) {
    let records = parse_fixture(r#"{"type":"user_prompt","data":"Check these commands"}
{"type":"text_delta","data":"| Name | Result |\n| --- | --- |\n| alpha | ok |\n"}
{"type":"item_started","data":{"id":"a","kind":{"type":"tool_call","name":"Bash","input":{"command":"one"}}}}
{"type":"item_completed","data":{"id":"a","output":"null","is_error":false}}
{"type":"item_started","data":{"id":"b","kind":{"type":"tool_call","name":"Bash","input":{"command":"two"}}}}
{"type":"item_started","data":{"id":"c","kind":{"type":"tool_call","name":"Bash","input":{"command":"three"}}}}"#).unwrap();
    let (chat, cx) = setup(records, cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    draw(cx);
    assert!(
        cx.debug_bounds("chat-markdown-table").is_some(),
        "GFM table renderer ran"
    );
    for selector in ["copy-reply-markdown", "copy-reply-text", "composer-attach"] {
        assert!(cx.debug_bounds(selector).is_none(), "unexpected {selector}");
    }
    let (first, second, third) = chat.read_with(cx, |view, cx| {
        assert_eq!(view.model.rows[1].title, "");
        let first = view.row_views[2].as_ref().unwrap().read(cx);
        assert_eq!(
            first.run_summary.as_deref(),
            Some("Ran 3 commands · now: three")
        );
        assert!(!first.run_open);
        assert!(!first.row.body.contains("null"));
        for index in [3, 4] {
            let row = view.row_views[index].as_ref().unwrap().read(cx);
            assert!(row.hidden_by_run);
            assert!(row.run_summary.is_none());
        }
        (
            view.model.rows[2].id,
            view.model.rows[3].id,
            view.model.rows[4].id,
        )
    });
    let summary = Box::leak(format!("tool-run-{first}").into_boxed_str());
    let bounds = cx.debug_bounds(summary).unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(view.row_views[2].as_ref().unwrap().read(cx).run_open);
        assert!(!view.row_views[3].as_ref().unwrap().read(cx).hidden_by_run);
    });
    for id in [second, third] {
        let selector = Box::leak(format!("transcript-row-{id}").into_boxed_str());
        assert!(cx.debug_bounds(selector).is_some());
    }
    let bubble = cx.debug_bounds("transcript-row-0").unwrap();
    let footer = cx.debug_bounds("chat-footer").unwrap();
    assert!(bubble.size.width < footer.size.width * 0.85);
    // Messages line up 14px inside the footer's centred outer width.
    assert!((bubble.right() - (footer.right() - px(14.))).abs() < px(2.));
}

#[gpui::test]
fn run_summary_updates_when_later_tools_finish_or_fail(cx: &mut TestAppContext) {
    let records = parse_fixture(r#"{"type":"item_started","data":{"id":"a","kind":{"type":"tool_call","name":"Bash","input":{"command":"one"}}}}
{"type":"item_started","data":{"id":"b","kind":{"type":"tool_call","name":"Bash","input":{"command":"two"}}}}"#).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::ItemCompleted {
                id: "a".into(),
                output: "".into(),
                is_error: false,
            },
            cx,
        );
        view.append_event(
            HarnessEvent::ItemCompleted {
                id: "b".into(),
                output: "failed".into(),
                is_error: true,
            },
            cx,
        );
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let first = view.row_views[0].as_ref().unwrap().read(cx);
        assert_eq!(first.run_summary.as_deref(), Some("Ran 1 command"));
        let failed = view.row_views[1].as_ref().unwrap().read(cx);
        assert!(!failed.hidden_by_run);
        assert!(failed.run_summary.is_none());
    });
}

#[gpui::test]
fn finished_work_uses_iced_fold_and_keeps_final_answer_visible(cx: &mut TestAppContext) {
    let records = parse_fixture(r#"{"at_ms":0,"entry":{"type":"user_prompt","data":"Check"}}
{"at_ms":100,"entry":{"type":"harness","data":{"type":"turn_started","data":{"session_id":null,"model":null}}}}
{"at_ms":200,"entry":{"type":"harness","data":{"type":"thinking_delta","data":"**literal thought**"}}}
{"at_ms":300,"entry":{"type":"harness","data":{"type":"item_started","data":{"id":"a","kind":{"type":"tool_call","name":"Bash","input":{"command":"check"}}}}}}
{"at_ms":400,"entry":{"type":"harness","data":{"type":"item_completed","data":{"id":"a","output":"ok","is_error":false}}}}
{"at_ms":500,"entry":{"type":"harness","data":{"type":"text_delta","data":"Final answer stays visible"}}}
{"at_ms":1400,"entry":{"type":"harness","data":{"type":"turn_completed","data":{"status":{"state":"completed"},"usage":null,"cost_usd":null}}}}"#).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.model.rows.len(), 4, "no synthetic footer");
        let thinking = view.row_views[1].as_ref().unwrap().read(cx);
        assert_eq!(thinking.work_summary.as_deref(), Some("Worked for 1.3s"));
        assert!(!thinking.work_open);
        assert_eq!(
            plain_markdown(&thinking.row.body).trim(),
            "**literal thought**"
        );
        assert!(view.row_views[2].as_ref().unwrap().read(cx).hidden_by_work);
        assert!(!view.row_views[3].as_ref().unwrap().read(cx).hidden_by_work);
    });
    let fold = cx.debug_bounds("work-fold-1").unwrap();
    cx.simulate_click(fold.center(), gpui::Modifiers::none());
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(view.row_views[1].as_ref().unwrap().read(cx).work_open);
        assert!(!view.row_views[2].as_ref().unwrap().read(cx).hidden_by_work);
    });
}

#[gpui::test]
fn history_fold_keeps_short_reply_before_later_tools_and_has_no_empty_tail(
    cx: &mut TestAppContext,
) {
    let mut records = vec![
        Record {
            at_ms: None,
            entry: Entry::UserPrompt("Check".into()),
        },
        Record {
            at_ms: None,
            entry: Entry::TurnTiming {
                start_ms: Some(1000),
                end_ms: None,
                split_ms: None,
            },
        },
    ];
    for index in 0..80 {
        records.push(Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::ItemStarted {
                id: index.to_string(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command":"check"}),
                },
            }),
        });
        records.push(Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::ItemCompleted {
                id: index.to_string(),
                output: "ok".into(),
                is_error: false,
            }),
        });
        if index == 78 {
            records.push(Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::TextDelta("The fix is merged.".into())),
            });
        }
    }
    records.extend([
        Record {
            at_ms: None,
            entry: Entry::TurnTiming {
                start_ms: None,
                end_ms: Some(81_000),
                split_ms: None,
            },
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            }),
        },
    ]);
    // Exercise the persisted envelope too, rather than only live projection.
    let records = parse_fixture(
        &records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let (chat, cx) = setup(records, cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    draw(cx);
    let (reply_id, fold_id) = chat.update_in(cx, |view, window, cx| {
        let reply = view
            .model
            .rows
            .iter()
            .position(|row| row.kind == RowKind::Assistant)
            .unwrap();
        let reply_view = view.row(reply, window, cx);
        assert!(!reply_view.read(cx).hidden_by_work);
        assert!(reply_view.read(cx).work_summary.is_none());
        let fold = turns::work_folds(&view.model);
        assert_eq!(fold.len(), 1);
        assert!(fold.iter().all(|f| f.label == "Worked for 1m 20s"));
        assert!(fold.iter().all(|f| !f.range.contains(&reply)));
        let runs = tool_runs::group(&view.model.rows, &view.model.completed_turns);
        assert_eq!(runs.last().unwrap().label, "Ran 1 command");
        let mid_turn = view.row(runs.last().unwrap().range.start, window, cx);
        assert!(mid_turn.read(cx).work_summary.is_none());
        assert_eq!(
            mid_turn.read(cx).run_summary.as_deref(),
            Some("Ran 1 command")
        );
        assert!(view.scroller.read(cx).is_following_tail());
        (
            view.model.rows[reply].id,
            view.model.rows[fold[0].range.start].id,
        )
    });
    draw(cx);
    assert_eq!((reply_id, fold_id), (80, 1));
    let reply = cx
        .debug_bounds("transcript-row-80")
        .expect("reply drawn at tail");
    let fold = cx.debug_bounds("work-fold-1").expect("fold drawn at tail");
    assert!(reply.origin.y >= fold.origin.y);
    assert!(
        reply.origin.y - fold.origin.y < px(100.),
        "hidden tools must occupy no space"
    );
}

#[gpui::test]
fn read_only_history_chips_use_saved_config_and_recorded_labels(cx: &mut TestAppContext) {
    let records = vec![Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::Ready {
            session_id: None,
            permission_mode: Some("auto".into()),
            models: vec![
                serde_json::json!({"value":"opus", "displayName":"Opus 5.5", "supportsEffort":true}),
                serde_json::json!({"value":"sonnet", "displayName":"Sonnet", "supportsEffort":true}),
            ],
            modes: vec![],
            commands: vec![],
        }),
    }];
    let (chat, cx) = setup(records, cx);
    chat.update_in(cx, |view, _, cx| {
        view.set_history_config(
            &gitterm_core::chat::types::AgentBackendConfig::Claude {
                model: "opus".into(),
                permission_mode: Some("default".into()),
                effort: None,
            },
            cx,
        );
        view.set_read_only(Some("History".into()), cx);
        assert_eq!(view.composer_state.model, "Opus 5.5");
        assert_eq!(view.composer_state.effort, "Auto · 200k");
        assert_eq!(view.composer_state.mode, "Auto mode");
        assert!(!view.settings_enabled());
        view.append_event(HarnessEvent::ModelChanged("sonnet".into()), cx);
        view.set_history_config(
            &gitterm_core::chat::types::AgentBackendConfig::Claude {
                model: "opus".into(),
                permission_mode: Some("default".into()),
                effort: Some("low".into()),
            },
            cx,
        );
        assert_eq!(
            view.composer_state.model, "Sonnet",
            "transcript acknowledgement wins"
        );
        assert_eq!(view.composer_state.mode, "Auto mode");
        assert_eq!(view.composer_state.effort, "Auto · 200k");
    });
    draw(cx);
    let bounds = cx.debug_bounds("composer-model").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
    chat.read_with(cx, |view, _| assert!(view.composer_state.picker.is_none()));
}

#[gpui::test]
fn delivery_uses_a_full_width_collapsible_card_and_separate_id(cx: &mut TestAppContext) {
    let records = vec![Record {
        at_ms: None,
        entry: Entry::UserPrompt("GitTerm delivery (not typed by the user):\nWorker worker-7 (Codex · model) reports done: TRU-274\nFull report\nFull record: delegation_get worker-7".into()),
    }];
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[0].as_ref().unwrap().read(cx);
        assert_eq!(row.row.kind, RowKind::Delegation);
        assert!(!row.open);
        assert_eq!(row.row.delivery.as_ref().unwrap().delegation_id, "worker-7");
        assert!(!row.row.title.contains("Full report"));
    });
    let card = cx.debug_bounds("delivery-card-0").unwrap();
    let footer = cx.debug_bounds("chat-footer").unwrap();
    assert!(card.size.width > footer.size.width * 0.85);
    let fold = cx.debug_bounds("row-fold").unwrap();
    cx.simulate_click(fold.center(), gpui::Modifiers::none());
    draw(cx);
    assert!(chat.read_with(cx, |view, cx| {
        view.row_views[0].as_ref().unwrap().read(cx).open
    }));
}

#[gpui::test]
fn replayed_timed_delivery_turn_shows_worked_for_duration(cx: &mut TestAppContext) {
    let records = vec![
        Record {
            at_ms: None,
            entry: Entry::UserPrompt(
                "Worker worker-7 reports done: TRU-274\nFull record: delegation_get worker-7"
                    .into(),
            ),
        },
        Record {
            at_ms: None,
            entry: Entry::TurnTiming {
                start_ms: Some(1000),
                end_ms: None,
                split_ms: None,
            },
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TurnStarted {
                session_id: None,
                model: None,
            }),
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::ThinkingDelta("Checking the report".into())),
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TextDelta("Merged".into())),
        },
        Record {
            at_ms: None,
            entry: Entry::TurnTiming {
                start_ms: None,
                end_ms: Some(146000),
                split_ms: None,
            },
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            }),
        },
    ];
    let records: Vec<Record> =
        serde_json::from_str(&serde_json::to_string(&records).unwrap()).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let delivery = view.row_views[0].as_ref().unwrap().read(cx);
        assert!(delivery.work_summary.is_none());
        assert!(!delivery.hidden_by_work);
        let work = view.row_views[1].as_ref().unwrap().read(cx);
        assert_eq!(work.work_summary.as_deref(), Some("Worked for 2m 25s"));
    });
}

#[gpui::test]
fn mid_turn_three_commands_use_run_wording_after_history_completion(cx: &mut TestAppContext) {
    let mut records = vec![Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::TextDelta(
            "Checking the implementation".into(),
        )),
    }];
    for id in ["one", "two", "three"] {
        records.extend([
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::ItemStarted {
                    id: id.into(),
                    kind: ItemKind::ToolCall {
                        name: "Bash".into(),
                        input: serde_json::json!({"command":id}),
                    },
                }),
            },
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::ItemCompleted {
                    id: id.into(),
                    output: "ok".into(),
                    is_error: false,
                }),
            },
        ]);
    }
    records.extend([
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TextDelta(
                "The implementation is correct".into(),
            )),
        },
        Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
                rate_limit: None,
            }),
        },
    ]);
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let run = view.row_views[1].as_ref().unwrap().read(cx);
        assert_eq!(run.run_summary.as_deref(), Some("Ran 3 commands"));
        assert!(run.work_summary.is_none());
        assert!(view.row_views[2].as_ref().unwrap().read(cx).hidden_by_run);
        assert!(!view.row_views[4].as_ref().unwrap().read(cx).hidden_by_run);
    });
}

fn install_card(
    card: crate::delegation::DelegationCard,
    chat: &Entity<ChatView>,
    cx: &mut VisualTestContext,
) {
    chat.update_in(cx, |view, _, cx| {
        view.set_delegations(vec![card.clone()], cx);
        view.set_roster(
            vec![roster::RosterEntry {
                id: format!("dlg:{}", card.delegation.delegation_id),
                name: "Codex".into(),
                model: String::new(),
                checkout: String::new(),
                activity: String::new(),
                state: roster::WorkerState::Done,
                started_at_ms: 0,
                ended_at_ms: Some(0),
            }],
            cx,
        );
    });
    draw(cx);
}
fn click_selector(selector: String, cx: &mut VisualTestContext) {
    let selector = Box::leak(selector.into_boxed_str());
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}
#[gpui::test]
fn h12_h14_h15_h16_h16a_h16b_preview_buttons_only_notice(cx: &mut TestAppContext) {
    use crate::delegation::tests::{card, with_findings};
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1400.), px(1000.)));
    let called = Rc::new(std::cell::Cell::new(0));
    let captured = called.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(
            Rc::new(move |_, _| {
                captured.set(captured.get() + 1);
                Receipt::rejected("unexpected")
            }),
            cx,
        )
    });
    for (card, label) in [
        (with_findings(), "Send to Claude"),
        (card("review", "completed"), "Re-run"),
        (card("review", "completed"), "Dismiss"),
        (card("consult", "running"), "Cancel"),
        (card("implement", "running"), "Stop tracking"),
    ] {
        install_card(card.clone(), &chat, cx);
        let id = chat.read_with(cx, |view, _| view.model.rows[0].id);
        click_selector(format!("delegation-action-{id}-{label}"), cx);
        chat.read_with(cx, |view, _| {
            assert_eq!(
                view.composer_state.notice.as_deref(),
                Some(crate::interactions::preview_notice(label).as_str())
            );
            assert_eq!(view.model.rows[0].delegation.as_ref(), Some(&card));
            assert!(view.records.is_empty());
        });
    }
    assert_eq!(called.get(), 0, "no runtime commands or persistence path");
}
#[gpui::test]
fn delegation_dismiss_asks_the_host_gate(cx: &mut TestAppContext) {
    use crate::delegation::tests::card;
    use crate::interactions::DismissRequest;
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1400.), px(1000.)));
    let asked = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = asked.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_dismiss_gate(
            Rc::new(move |request: &DismissRequest, _: &gpui::App| {
                captured.borrow_mut().push(request.clone());
                "host notice".into()
            }),
            cx,
        )
    });
    for (card, label, active) in [
        (card("review", "completed"), "Dismiss", false),
        (card("consult", "running"), "Cancel", true),
        (card("implement", "running"), "Stop tracking", true),
    ] {
        install_card(card, &chat, cx);
        let id = chat.read_with(cx, |view, _| view.model.rows[0].id);
        click_selector(format!("delegation-action-{id}-{label}"), cx);
        chat.read_with(cx, |view, _| {
            assert_eq!(view.composer_state.notice.as_deref(), Some("host notice"))
        });
        assert_eq!(
            asked.borrow().last(),
            Some(&DismissRequest {
                id: "review-1".into(),
                label: label.into(),
                active,
            })
        );
    }
}
#[gpui::test]
fn h13_h13a_card_navigation_emits_only_the_exact_file_or_worker(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1400.), px(1000.)));
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, event: &roster::RosterOutcome, _| {
            captured.borrow_mut().push(event.clone())
        })
    });
    install_card(crate::delegation::tests::with_findings(), &chat, cx);
    click_selector("finding-location-0-F1".into(), cx);
    assert_eq!(
        *events.borrow(),
        [roster::RosterOutcome::OpenFile {
            path: "src/lib.rs".into(),
            review: false
        }]
    );
    install_card(
        crate::delegation::tests::card("implement", "running"),
        &chat,
        cx,
    );
    click_selector("delegation-action-0-Open worker tab".into(), cx);
    assert_eq!(
        events.borrow().last(),
        Some(&roster::RosterOutcome::OpenWorker {
            id: "review-1".into()
        })
    );
}
#[gpui::test]
fn h24_roster_reveals_unfolds_flashes_card_without_worker_navigation(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1400.), px(1000.)));
    install_card(
        crate::delegation::tests::card("implement", "running"),
        &chat,
        cx,
    );
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, event: &roster::RosterOutcome, _| {
            captured.borrow_mut().push(event.clone())
        })
    });
    chat.update_in(cx, |view, window, cx| {
        view.row(0, window, cx)
            .update(cx, |row, _| row.open = false);
    });
    draw(cx);
    click_selector("roster-dlg:review-1".into(), cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.flashed_row, Some(0));
        assert!(view.row_views[0].as_ref().unwrap().read(cx).open);
        assert!(!view.scroller.read(cx).is_following_tail());
    });
    assert!(
        events
            .borrow()
            .iter()
            .all(|event| matches!(event, roster::RosterOutcome::Open { .. }))
    );
    assert!(cx.debug_bounds("roster-dot-dlg:review-1").is_some());
    assert!(cx.debug_bounds("chat-latest").is_some());
    cx.executor().advance_clock(Duration::from_millis(901));
    draw(cx);
    assert_eq!(chat.read_with(cx, |view, _| view.flashed_row), None);
}
#[gpui::test]
fn h03_show_earlier_restores_one_chunk_and_h04_latest_follows_and_trims(cx: &mut TestAppContext) {
    let records = (0..250)
        .map(|i| Record {
            at_ms: None,
            entry: Entry::UserPrompt(format!("prompt {i}")),
        })
        .collect();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    assert_eq!(chat.read_with(cx, |view, _| view.history_start), 100);
    chat.update_in(cx, |view, _, cx| {
        view.scroller.update(cx, |state, cx| {
            state.scroll_to_item(99, cx);
        });
    });
    draw(cx);
    click_selector("show-earlier".into(), cx);
    assert_eq!(chat.read_with(cx, |view, _| view.history_start), 50);
    chat.update_in(cx, |view, _, cx| {
        view.scroller.update(cx, |state, cx| {
            state.scroll_to_item(100, cx);
        });
    });
    draw(cx);
    assert!(cx.debug_bounds("chat-latest").is_some());
    click_selector("chat-latest".into(), cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.history_start, 100);
        assert!(view.scroller.read(cx).is_following_tail());
    });
}
#[gpui::test]
fn h06_markdown_callback_blocks_middle_click_and_unsafe_schemes(cx: &mut TestAppContext) {
    use super::transcript::{link_allowed, open_link};
    let (_, cx) = setup(vec![], cx);
    let left = ClickEvent::Mouse(gpui::MouseClickEvent {
        up: gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            ..Default::default()
        },
        ..Default::default()
    });
    let middle = ClickEvent::Mouse(gpui::MouseClickEvent {
        up: gpui::MouseUpEvent {
            button: gpui::MouseButton::Middle,
            ..Default::default()
        },
        ..Default::default()
    });
    assert!(link_allowed("https://example.com", &left));
    assert!(!link_allowed("https://example.com", &middle));
    assert!(!link_allowed("file:///private/tmp/a", &left));
    cx.update(|window, cx| {
        open_link(&"file:///private/tmp/a".into(), &left, window, cx);
        open_link(&"https://example.com".into(), &middle, window, cx);
    });
    assert!(cx.opened_url().is_none());
    cx.update(|window, cx| open_link(&"HTTPS://example.com".into(), &left, window, cx));
    assert_eq!(cx.opened_url().as_deref(), Some("HTTPS://example.com"));
}
#[gpui::test]
fn h08_edit_show_all_expands_and_h09_review_routes_without_toggling_card(cx: &mut TestAppContext) {
    let records = vec![Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::ItemStarted {
            id: "edit".into(),
            kind: ItemKind::ToolCall {
                name: "Write".into(),
                input: serde_json::json!({"file_path":"src/lib.rs","content":"line\n".repeat(60)}),
            },
        }),
    }];
    let (chat, cx) = setup(records, cx);
    cx.simulate_resize(size(px(1400.), px(2000.)));
    draw(cx);
    click_selector("tool-run-0".into(), cx);
    click_selector("row-fold".into(), cx);
    chat.update_in(cx, |view, _, cx| {
        view.scroller.update(cx, |state, cx| {
            state.scroll_to_item(0, cx);
        });
    });
    draw(cx);
    click_selector("edit-more-0-0".into(), cx);
    chat.read_with(cx, |view, cx| {
        assert!(
            view.row_views[0]
                .as_ref()
                .unwrap()
                .read(cx)
                .expanded_edits
                .contains(&0)
        )
    });
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = events.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, event: &roster::RosterOutcome, _| {
            captured.borrow_mut().push(event.clone())
        })
    });
    // Review stays outside the edit's disclosure and stops propagation.
    chat.update_in(cx, |view, window, cx| {
        view.row(0, window, cx)
            .update(cx, |row, _| row.open = false)
    });
    draw(cx);
    click_selector("edit-review-0".into(), cx);
    assert_eq!(
        *events.borrow(),
        [roster::RosterOutcome::OpenFile {
            path: "src/lib.rs".into(),
            review: true
        }]
    );
    chat.read_with(cx, |view, cx| {
        assert!(!view.row_views[0].as_ref().unwrap().read(cx).open)
    });
}
#[gpui::test]
fn h21_opening_empty_find_preserves_the_exact_retained_pixel_offset(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        parse_fixture(include_str!("../../fixtures/long-5000.jsonl")).unwrap(),
        cx,
    );
    draw(cx);
    chat.update_in(cx, |view, _, cx| {
        view.history_start = 0;
        view.scroller.update(cx, |state, cx| {
            state.remeasure(cx);
            state.scroll_to_item(2500, cx);
        });
    });
    draw(cx);
    let x = cx.debug_bounds("native-chat").unwrap().center().x;
    cx.simulate_event(gpui::ScrollEvent {
        position: gpui::point(x, px(300.)),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-37.))),
        ..Default::default()
    });
    draw(cx);
    let (scroller, top) = chat.read_with(cx, |view, cx| {
        (
            view.scroller.clone(),
            view.scroller.read(cx).logical_scroll_top(),
        )
    });
    assert_ne!(top.offset_in_item, px(0.));
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_ne!(view.scroller, scroller);
        assert_eq!(view.scroller.read(cx).logical_scroll_top(), top);
    });
    chat.update_in(cx, |view, _, cx| {
        view.jump(cx);
        assert_eq!(view.history_start, 0, "find pauses history trimming");
    });
    cx.simulate_keystrokes("escape");
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.scroller.read(cx).logical_scroll_top(), top)
    });
}

#[gpui::test]
fn h26_imported_continue_here_is_inert_and_pending_disables_it(cx: &mut TestAppContext) {
    let offer = crate::imported::ImportedOffer {
        conversations: vec![crate::imported::ImportedConversation {
            id: "imported-1".into(),
            title: "Earlier work".into(),
            origin: "terminal".into(),
        }],
        pending: false,
    };
    let (chat, cx) = setup(
        vec![Record {
            at_ms: None,
            entry: Entry::ImportedOffer(offer.clone()),
        }],
        cx,
    );
    draw(cx);
    click_selector("continue-here-imported-1".into(), cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_state.notice.as_deref(),
            Some("Continue here · not available in preview")
        );
        assert_eq!(view.imported_offer, offer);
        assert!(view.model.rows.is_empty());
        assert!(view.submission.is_none());
    });
    chat.update_in(cx, |view, _, cx| {
        view.set_imported_offer(
            crate::imported::ImportedOffer {
                pending: true,
                ..offer.clone()
            },
            cx,
        );
        view.composer_state.notice = None;
    });
    draw(cx);
    click_selector("continue-here-imported-1".into(), cx);
    assert!(chat.read_with(cx, |view, _| view.composer_state.notice.is_none()));
}

#[gpui::test]
fn h02_tool_run_counts_and_completed_work_keep_reply_visible(cx: &mut TestAppContext) {
    let records=parse_fixture(r#"{"at_ms":0,"entry":{"type":"harness","data":{"type":"turn_started","data":{"session_id":null,"model":null}}}}
{"type":"item_started","data":{"id":"a","kind":{"type":"tool_call","name":"Read","input":{"file_path":"a.rs"}}}}
{"type":"item_completed","data":{"id":"a","output":"one","is_error":false}}
{"type":"item_started","data":{"id":"b","kind":{"type":"tool_call","name":"Read","input":{"file_path":"b.rs"}}}}
{"type":"item_completed","data":{"id":"b","output":"two","is_error":false}}
{"type":"text_delta","data":"Final reply stays visible"}
{"at_ms":2000,"entry":{"type":"harness","data":{"type":"turn_completed","data":{"status":{"state":"completed"},"usage":{},"cost_usd":null,"rate_limit":null}}}}"#).unwrap();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[0].as_ref().unwrap().read(cx);
        assert_eq!(row.work_summary.as_deref(), Some("Worked for 2.0s"));
        assert_eq!(row.run_summary.as_deref(), Some("Read 2 files"));
        assert_eq!(row.run_tooltip, "Read ×2");
        assert!(!row.work_open);
        assert!(
            view.row_views[2]
                .as_ref()
                .unwrap()
                .read(cx)
                .row
                .body
                .contains("Final reply stays visible")
        );
    });
    click_selector("work-fold-0".into(), cx);
    click_selector("tool-run-0".into(), cx);
    chat.read_with(cx, |view, cx| {
        let row = view.row_views[0].as_ref().unwrap().read(cx);
        assert!(row.work_open && row.run_open);
    });
}

#[gpui::test]
fn h03_history_counts_folded_tool_runs_as_one_block(cx: &mut TestAppContext) {
    let records = (0..220)
        .map(|index| Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::ItemStarted {
                id: format!("tool-{index}"),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command":"sample"}),
                },
            }),
        })
        .collect();
    let (chat, cx) = setup(records, cx);
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(view.history_start, 0);
        assert_eq!(scroll::block_starts(&view.model), vec![0]);
    });
    assert!(cx.debug_bounds("show-earlier").is_none());
}

#[gpui::test]
fn h11_dismissed_card_stays_in_transcript_and_closes_on_registry_refresh(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    let mut card = crate::delegation::tests::with_findings();
    install_card(card.clone(), &chat, cx);
    assert!(chat.read_with(cx, |view, cx| {
        view.row_views[0].as_ref().unwrap().read(cx).open
    }));
    card.delegation.dismissed_at = Some("2026-11-20T01:00:00Z".into());
    chat.update_in(cx, |view, _, cx| {
        view.set_delegations(vec![card.clone()], cx);
        view.set_roster(vec![], cx);
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.model.rows.len(), 1);
        assert!(!view.row_views[0].as_ref().unwrap().read(cx).open);
        assert!(view.roster.is_empty());
    });
}

#[gpui::test]
fn h21_find_searches_findings_and_temporarily_expands_long_edits(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    install_card(crate::delegation::tests::with_findings(), &chat, cx);
    chat.update_in(cx, |view, window, cx| {
        view.open_find(&Find, window, cx);
        view.change_find("Return early".into(), window, cx);
    });
    draw(cx);
    assert_eq!(chat.read_with(cx, |view, _| view.find.matches.len()), 1);
    chat.update_in(cx, |view, window, cx| {
        view.close_find(&CloseFind, window, cx)
    });
    let content = (0..60).map(|i| format!("line {i}\n")).collect::<String>();
    chat.update_in(cx, |view, window, cx| {
        view.append_event(
            HarnessEvent::ItemStarted {
                id: "write".into(),
                kind: ItemKind::ToolCall {
                    name: "Write".into(),
                    input: serde_json::json!({"file_path":"sample.rs","content":content}),
                },
            },
            cx,
        );
        view.open_find(&Find, window, cx);
        view.toggle_find_tools(window, cx);
        view.change_find("line 59".into(), window, cx);
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert_eq!(view.find.active().unwrap().row, 1);
        assert!(
            view.row_views[1]
                .as_ref()
                .unwrap()
                .read(cx)
                .expanded_edits
                .contains(&0)
        );
    });
    chat.update_in(cx, |view, window, cx| {
        view.close_find(&CloseFind, window, cx)
    });
    draw(cx);
    chat.read_with(cx, |view, cx| {
        assert!(
            !view.row_views[1]
                .as_ref()
                .unwrap()
                .read(cx)
                .expanded_edits
                .contains(&0)
        )
    });
}

#[gpui::test]
fn h08_expanded_edit_survives_row_eviction_without_changing_source(cx: &mut TestAppContext) {
    let mut records = vec![Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::ItemStarted {
            id: "edit".into(),
            kind: ItemKind::ToolCall {
                name: "Write".into(),
                input: serde_json::json!({"file_path":"sample.rs","content":"line\n".repeat(60)}),
            },
        }),
    }];
    records.extend((0..110).map(|i| Record {
        at_ms: None,
        entry: Entry::UserPrompt(format!("prompt {i}")),
    }));
    let (chat, cx) = setup(records, cx);
    chat.update_in(cx, |view, window, cx| {
        let source = view.model.rows[0].body.clone();
        view.row(0, window, cx)
            .update(cx, |row, cx| row.expand_edit(0, cx));
        for index in 1..view.model.rows.len() {
            view.row(index, window, cx);
        }
        assert!(view.row_views[0].is_none());
        let row = view.row(0, window, cx);
        assert!(row.read(cx).expanded_edits.contains(&0));
        assert_eq!(view.model.rows[0].body, source);
    });
}

#[gpui::test]
fn h12_finding_checkbox_toggles_local_selection_only(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    cx.simulate_resize(size(px(1400.), px(1000.)));
    let card = crate::delegation::tests::with_findings();
    install_card(card.clone(), &chat, cx);
    let selected = |chat: &Entity<ChatView>, cx: &mut gpui::VisualTestContext| {
        chat.read_with(cx, |view, cx| {
            view.row_views[0]
                .as_ref()
                .map(|row| row.read(cx).selected_findings.clone())
        })
    };
    click_selector("finding-select-0-F1".into(), cx);
    let after = selected(&chat, cx).flatten().expect("selection state");
    assert!(!after.contains("F1"), "first click unchecks F1");
    click_selector("finding-select-0-F1".into(), cx);
    let after = selected(&chat, cx).flatten().expect("selection state");
    assert!(after.contains("F1"), "second click re-checks F1");
    chat.read_with(cx, |view, _| {
        assert!(view.composer_state.notice.is_none());
        assert_eq!(view.model.rows[0].delegation.as_ref(), Some(&card));
    });
}
#[gpui::test]
fn h12_delivered_findings_are_checked_and_disabled(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    let mut card = crate::delegation::tests::with_findings();
    card.delegation.delivered = true;
    install_card(card.clone(), &chat, cx);
    click_selector("finding-select-0-F1".into(), cx);
    chat.read_with(cx, |view, _| {
        assert!(view.composer_state.notice.is_none());
        assert_eq!(view.model.rows[0].delegation.as_ref(), Some(&card));
    });
    assert!(
        cx.debug_bounds("delegation-action-0-Send to Claude")
            .is_none()
    );
}

fn ready_event(models: Vec<serde_json::Value>) -> HarnessEvent {
    HarnessEvent::Ready {
        session_id: None,
        permission_mode: Some("default".into()),
        models,
        modes: vec![],
        commands: vec![],
    }
}
fn launch_config_records() -> Vec<Record> {
    [
        HarnessEvent::ModelChanged("opus".into()),
        HarnessEvent::PermissionModeChanged("default".into()),
        HarnessEvent::EffortChanged {
            effort: None,
            applied: None,
        },
    ]
    .into_iter()
    .map(|event| Record {
        at_ms: None,
        entry: Entry::Harness(event),
    })
    .collect()
}
fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let center = cx.debug_bounds(selector).expect(selector).center();
    cx.simulate_click(center, gpui::Modifiers::none());
}
fn written() -> Receipt {
    let mut receipt = Receipt::rejected("synthetic");
    receipt.state = ReceiptState::Written;
    receipt
}

#[gpui::test]
fn c06_c08_choice_before_ready_is_shown_held_and_sent_when_the_session_starts(
    cx: &mut TestAppContext,
) {
    use crate::pickers::Picker;
    use gitterm_core::chat::types::SettingChange;
    let (chat, cx) = setup(launch_config_records(), cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    let intents = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = intents.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                written()
            }),
            cx,
        );
        view.set_read_only(None, cx);
    });
    draw(cx);
    chat.read_with(cx, |view, _| assert!(!view.composer_state.commands_ready));
    // The model chip opens before the session is ready and shows Iced's hint.
    click(cx, "composer-model");
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.picker, Some(Picker::Model));
        assert_eq!(
            view.composer_state.catalog.models[0].section,
            "Model · full list after the session starts"
        );
    });
    let labels = chat.read_with(cx, |view, _| {
        view.composer_state
            .catalog
            .models
            .iter()
            .map(|row| row.label.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(labels, ["Default", "Opus", "Sonnet", "Haiku", "Fable"]);
    click(cx, "setting-option-2");
    draw(cx);
    // Nothing is sent yet; the chip shows the choice at once.
    assert!(intents.borrow().is_empty());
    chat.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.model, "sonnet");
        assert_eq!(
            view.composer_state.prestart,
            [SettingChange::Model("sonnet".into())]
        );
    });
    // The mode chip works the same way; one picker change is sent at a time.
    click(cx, "composer-mode");
    draw(cx);
    click(cx, "setting-option-2");
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.mode, "Plan mode")
    });
    assert!(intents.borrow().is_empty());
    chat.update(cx, |view, cx| {
        view.append_event(
            ready_event(vec![
                serde_json::json!({"value":"opus","displayName":"Opus","supportsEffort":true}),
                serde_json::json!({"value":"sonnet","displayName":"Sonnet","supportsEffort":true}),
            ]),
            cx,
        )
    });
    draw(cx);
    assert_eq!(
        *intents.borrow(),
        [ChatIntent::ChangeSetting {
            change: SettingChange::Model("sonnet".into())
        }]
    );
    chat.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.prestart.len(), 2, "mode waits its turn")
    });
}

#[gpui::test]
fn c06_c08_rejected_prestart_choice_is_dropped_with_a_notice(cx: &mut TestAppContext) {
    use gitterm_core::chat::types::SettingChange;
    let (chat, cx) = setup(launch_config_records(), cx);
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(Rc::new(|_, _| Receipt::rejected("provider said no")), cx);
        view.set_read_only(None, cx);
        view.choose_setting(SettingChange::Model("haiku".into()), window, cx);
        view.append_event(ready_event(vec![]), cx);
    });
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert!(view.composer_state.prestart.is_empty());
        assert_eq!(view.composer_state.model, "opus");
        assert_eq!(
            view.composer_state.notice.as_deref(),
            Some("provider said no")
        );
    });
}

#[gpui::test]
fn c02_draft_is_empty_right_after_send_and_a_rejection_puts_it_back(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    let intents = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = intents.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                let mut receipt = Receipt::rejected("synthetic");
                receipt.state = ReceiptState::ChannelAccepted;
                receipt
            }),
            cx,
        );
        view.set_read_only(None, cx);
        view.composer
            .update(cx, |input, cx| input.set_value("hello", window, cx));
        view.send(window, cx);
        // Before any write or terminal receipt, as Iced empties the box on post.
        assert_eq!(view.composer.read(cx).value().as_str(), "");
        let mut receipt = view.submission.clone().unwrap();
        receipt.state = ReceiptState::Rejected("closed".into());
        view.update_receipt(receipt, window, cx);
        assert_eq!(view.composer.read(cx).value().as_str(), "hello");
    });
    assert_eq!(intents.borrow().len(), 1);
}

#[gpui::test]
fn c02_send_explains_the_queue_only_while_a_turn_runs(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update(cx, |view, _| {
        let state = &mut view.composer_state;
        assert_eq!(state.send_tooltip(false), None);
        state.streaming = true;
        assert_eq!(
            state.send_tooltip(false).as_deref(),
            Some("Claude is working: the message is queued and Claude reads it at its next step")
        );
        assert_eq!(state.send_tooltip(true), None, "disabled Send has no title");
        state.backend = Some(gitterm_core::chat::types::AgentBackend::Acp(
            gitterm_core::chat::types::AcpAgent::OpenCode,
        ));
        assert_eq!(
            state.send_tooltip(false).as_deref(),
            Some("OpenCode is working: the message is queued and sent when this turn ends")
        );
    });
}

#[gpui::test]
fn c06_c07_c08_c14_chip_tooltips_are_iced_titles(cx: &mut TestAppContext) {
    let mut records = launch_config_records();
    records.push(Record {
        at_ms: None,
        entry: Entry::Harness(HarnessEvent::EffortChanged {
            effort: None,
            applied: Some("high".into()),
        }),
    });
    records.push(Record {
        at_ms: None,
        entry: Entry::Harness(ready_event(vec![
            serde_json::json!({"value":"opus","supportsEffort":true}),
            serde_json::json!({"value":"opus[1m]","supportsEffort":true}),
            serde_json::json!({"value":"haiku"}),
        ])),
    });
    let (chat, cx) = setup(records, cx);
    chat.update(cx, |view, _| {
        let state = &mut view.composer_state;
        assert_eq!(state.chip_tooltip("model", None).unwrap(), "Model: opus");
        assert_eq!(
            state.chip_tooltip("effort", None).unwrap(),
            "Effort (auto runs at high) · context window 200k"
        );
        assert_eq!(
            state.chip_tooltip("mode", None).unwrap(),
            "Permission mode (Shift+Tab in the message box cycles)"
        );
        assert_eq!(
            state.chip_tooltip("review", None).unwrap(),
            "Ask a Claude subagent or Codex to review this checkout, or consult them"
        );
        assert_eq!(state.chip_tooltip("guidelines", None), None);
    });
    // A model without effort levels, and a 1M window.
    chat.update(cx, |view, cx| {
        view.append_event(HarnessEvent::ModelChanged("haiku".into()), cx);
        assert_eq!(
            view.composer_state.chip_tooltip("effort", None).unwrap(),
            "This model has no effort setting · context window 200k"
        );
        view.append_event(HarnessEvent::ModelChanged("opus[1m]".into()), cx);
        assert_eq!(
            view.composer_state.chip_tooltip("model", None).unwrap(),
            "Model: opus[1m]"
        );
        assert!(
            view.composer_state
                .chip_tooltip("effort", None)
                .unwrap()
                .ends_with(" · context window 1M")
        );
    });
}

#[gpui::test]
fn c10_checkout_picker_lists_iced_entries_previews_a_choice_and_locks_once_started(
    cx: &mut TestAppContext,
) {
    use crate::composer::CheckoutChoice;
    use crate::pickers::Picker;
    let (chat, cx) = setup(launch_config_records(), cx);
    cx.simulate_resize(size(px(1200.), px(900.)));
    let intents = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = intents.clone();
    let asked = Rc::new(std::cell::Cell::new(0));
    let counted = asked.clone();
    chat.update_in(cx, |view, _, cx| {
        view.fixture_composer.checkout = Some("/repo".into());
        view.reproject();
        view.set_preview(cx);
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                written()
            }),
            cx,
        );
        view.set_checkout_source(
            Rc::new(move |_| {
                counted.set(counted.get() + 1);
                vec![
                    CheckoutChoice {
                        label: "Current checkout".into(),
                        detail: "/repo".into(),
                        path: "/repo".into(),
                    },
                    CheckoutChoice {
                        label: "tru-143-chips".into(),
                        detail: "Add chips".into(),
                        path: "/wt/tru-143-chips".into(),
                    },
                ]
            }),
            cx,
        );
        view.set_read_only(None, cx);
    });
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_state.chip_tooltip("checkout", None).unwrap(),
            "/repo\nWhere this chat runs"
        );
    });
    click(cx, "composer-checkout");
    draw(cx);
    assert_eq!(asked.get(), 1, "choices are asked for each time it opens");
    chat.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.picker, Some(Picker::Checkout))
    });
    assert!(cx.debug_bounds("checkout-picker").is_some());
    assert!(cx.debug_bounds("checkout-option-0").is_some());
    // C11: Open in editor follows the choices while the checkout is unlocked.
    assert!(cx.debug_bounds("checkout-open-editor").is_some());
    // The current checkout does nothing, as in Iced.
    click(cx, "checkout-option-0");
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.chips()[4].1, "Current checkout")
    });
    click(cx, "composer-checkout");
    draw(cx);
    click(cx, "checkout-option-1");
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_state.chips()[4].1,
            "tru-143-chips · not available in preview"
        );
        assert_eq!(view.composer_state.checkout_path.as_deref(), Some("/repo"));
    });
    assert!(cx.debug_bounds("checkout-picker").is_none());
    // Once the chat has started the chip is locked with Iced's reason.
    chat.update(cx, |view, cx| {
        view.append_event(
            HarnessEvent::TurnStarted {
                session_id: Some("s".into()),
                model: None,
            },
            cx,
        )
    });
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_state.chip_tooltip("checkout", None).unwrap(),
            "/repo\nSet when the chat started; start a new chat to change it"
        );
    });
    // C11: the locked chip still opens a menu, with only Open in editor.
    click(cx, "composer-checkout");
    draw(cx);
    assert!(cx.debug_bounds("checkout-picker").is_some());
    assert!(cx.debug_bounds("checkout-option-0").is_none());
    assert!(cx.debug_bounds("checkout-open-editor").is_some());
    assert_eq!(asked.get(), 2, "a locked menu asks for no choices");
    click(cx, "checkout-open-editor");
    draw(cx);
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_state.notice.as_deref(),
            Some("Open in editor · not available in preview")
        );
        assert_eq!(view.composer_state.picker, None);
    });
    assert!(cx.debug_bounds("checkout-picker").is_none());
    // No chip or picker interaction reaches the host's session seam.
    assert!(intents.borrow().is_empty());
}

#[gpui::test]
fn h11_delegation_fold_shows_kind_model_target_state_marker_and_toggles(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    let card = crate::delegation::tests::with_findings();
    // The kind-title tooltip and the one-line summary, closed and open.
    assert_eq!(card.header(), "Codex review · test-model");
    assert_eq!(
        card.fold_label(true, 0),
        "▾ Codex review · test-model · vs v6 (+ uncommitted) · focus: edge cases · 1 finding (1 P1)"
    );
    assert!(card.fold_label(false, 0).starts_with("▸ Codex review"));
    install_card(card, &chat, cx);
    let open = |chat: &Entity<ChatView>, cx: &mut VisualTestContext| {
        chat.read_with(cx, |view, cx| {
            view.row_views[0].as_ref().unwrap().read(cx).open
        })
    };
    assert!(open(&chat, cx));
    click_selector("delegation-fold-0".into(), cx);
    assert!(!open(&chat, cx));
    click_selector("delegation-fold-0".into(), cx);
    assert!(open(&chat, cx));
}

use gitterm_core::chat::types::MessageState;
/// A chat streaming a turn, with `synthetic-message` queued behind it, as
/// the shell drives it: core state through the adapter, presented to the view.
struct QueuedChat {
    adapter: crate::adapter::ChatAdapter,
    stamp: gitterm_core::chat::state::RuntimeStamp,
    target: gitterm_core::session::Target,
    intents: Rc<std::cell::RefCell<Vec<ChatIntent>>>,
}
impl QueuedChat {
    fn facts(&self) -> crate::adapter::Admission {
        use gitterm_core::session;
        crate::adapter::Admission {
            authority: session::State {
                target: self.target.clone(),
                control: session::TurnControl::Harness,
                task_live: false,
                last_tab: false,
            },
            config: gitterm_core::chat::types::AgentBackendConfig::Claude {
                model: String::new(),
                permission_mode: Some("default".into()),
                effort: None,
            },
            process_running: true,
            just_spawned: false,
            message_id: "synthetic-message".into(),
        }
    }
    fn receive(
        &mut self,
        event: HarnessEvent,
        chat: &Entity<ChatView>,
        cx: &mut VisualTestContext,
    ) {
        let update = self
            .adapter
            .receive(
                &self.stamp,
                &self.target,
                gitterm_core::chat::state::AgentEvent::Harness(event),
                None,
                |_, _| {},
            )
            .unwrap();
        self.present(update, chat, cx);
    }
    fn run(&mut self, intent: ChatIntent, chat: &Entity<ChatView>, cx: &mut VisualTestContext) {
        let prepared = self.adapter.admit(intent, self.facts()).unwrap();
        let (_, update) = self
            .adapter
            .execute(prepared, &self.target, 1000, |_| Ok(()));
        self.present(update, chat, cx);
    }
    fn present(
        &self,
        update: crate::adapter::Update,
        chat: &Entity<ChatView>,
        cx: &mut VisualTestContext,
    ) {
        chat.update_in(cx, |view, window, cx| {
            view.present(&self.stamp, self.adapter.state(), update, window, cx)
        });
        draw(cx);
    }
    fn lifecycle(
        &mut self,
        state: MessageState,
        chat: &Entity<ChatView>,
        cx: &mut VisualTestContext,
    ) {
        self.receive(
            HarnessEvent::MessageLifecycle {
                id: "synthetic-message".into(),
                state,
            },
            chat,
            cx,
        );
    }
}
fn queued_chat(cx: &mut TestAppContext) -> (Entity<ChatView>, &mut VisualTestContext, QueuedChat) {
    let (chat, cx) = setup(vec![], cx);
    let mut adapter = crate::adapter::ChatAdapter::default();
    let target = gitterm_core::session::Target {
        tab_id: 1,
        session_uid: "synthetic".into(),
        task_id: None,
        task_session_id: None,
    };
    let stamp = adapter.begin_runtime(target.clone());
    let intents = Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = intents.clone();
    chat.update_in(cx, |view, _, cx| {
        view.set_backend(gitterm_core::chat::types::AgentBackend::Claude, cx);
        view.set_read_only(None, cx);
        view.set_intent_callback(
            Rc::new(move |intent, _| {
                captured.borrow_mut().push(intent);
                written()
            }),
            cx,
        );
    });
    let mut queued = QueuedChat {
        adapter,
        stamp,
        target,
        intents,
    };
    queued.receive(
        HarnessEvent::TurnStarted {
            session_id: None,
            model: None,
        },
        &chat,
        cx,
    );
    queued.run(
        ChatIntent::Send {
            text: "later".into(),
            images: vec![],
        },
        &chat,
        cx,
    );
    queued.lifecycle(MessageState::Queued, &chat, cx);
    (chat, cx, queued)
}
fn bubble_tag(
    chat: &Entity<ChatView>,
    cx: &mut VisualTestContext,
) -> Option<crate::composer::QueuedTag> {
    chat.read_with(cx, |view, cx| {
        let index = view
            .model
            .rows
            .iter()
            .position(|row| row.prompt_id.as_deref() == Some("synthetic-message"))
            .expect("queued bubble");
        view.row_views[index]
            .as_ref()
            .unwrap()
            .read(cx)
            .queued
            .clone()
    })
}

#[gpui::test]
fn queued_bubble_shows_withdraw(cx: &mut TestAppContext) {
    let (chat, cx, _queued) = queued_chat(cx);
    let tag = bubble_tag(&chat, cx).expect("queued tag on the bubble");
    assert_eq!(tag.label, "Queued · Claude reads it at its next step");
    assert_eq!(tag.withdraw, Some(true));
    assert_eq!(tag.tooltip, "Withdraw this message (Claude keeps working)");
    // On the bubble, as Iced: the composer has no queue strip.
    let bubble = cx.debug_bounds("transcript-row-0").expect("bubble");
    let times = cx.debug_bounds("withdraw-synthetic-message").expect("×");
    assert!(bubble.contains(&times.center()));
    assert!(cx.debug_bounds("composer-queue").is_none());
    let input = cx.debug_bounds("composer-input").expect("composer");
    assert!(!input.contains(&times.center()));
}

#[gpui::test]
fn withdraw_from_bubble_removes_queued_message(cx: &mut TestAppContext) {
    let (chat, cx, mut queued) = queued_chat(cx);
    click(cx, "withdraw-synthetic-message");
    draw(cx);
    let intent = ChatIntent::Withdraw {
        message_id: "synthetic-message".into(),
    };
    assert_eq!(*queued.intents.borrow(), std::slice::from_ref(&intent));
    queued.run(intent, &chat, cx);
    let tag = bubble_tag(&chat, cx).unwrap();
    assert_eq!(tag.label, "Withdrawing…");
    assert_eq!(tag.withdraw, Some(false));
    queued.lifecycle(MessageState::Cancelled, &chat, cx);
    assert_eq!(bubble_tag(&chat, cx), None);
    assert!(cx.debug_bounds("withdraw-synthetic-message").is_none());
}

#[gpui::test]
fn delivered_message_has_no_withdraw(cx: &mut TestAppContext) {
    let (chat, cx, mut queued) = queued_chat(cx);
    assert!(cx.debug_bounds("withdraw-synthetic-message").is_some());
    queued.lifecycle(MessageState::Started, &chat, cx);
    assert_eq!(bubble_tag(&chat, cx), None);
    assert!(cx.debug_bounds("withdraw-synthetic-message").is_none());
    click(cx, "transcript-row-0");
    assert!(
        queued
            .intents
            .borrow()
            .iter()
            .all(|intent| !matches!(intent, ChatIntent::Withdraw { .. }))
    );
}

#[gpui::test]
fn read_only_withdraw_inert(cx: &mut TestAppContext) {
    let (chat, cx, queued) = queued_chat(cx);
    chat.update_in(cx, |view, _, cx| {
        view.set_read_only(Some("preview".into()), cx)
    });
    draw(cx);
    assert_eq!(bubble_tag(&chat, cx).unwrap().withdraw, Some(false));
    let sent = queued.intents.borrow().len();
    click(cx, "withdraw-synthetic-message");
    draw(cx);
    assert_eq!(queued.intents.borrow().len(), sent);
}

fn queued_row(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> (usize, RowKind) {
    chat.read_with(cx, |view, _| {
        let index = view
            .model
            .rows
            .iter()
            .position(|row| row.prompt_id.as_deref() == Some("synthetic-message"))
            .expect("queued row");
        (index, view.model.rows[index].kind)
    })
}

#[gpui::test]
fn withdrawn_bubble_removed(cx: &mut TestAppContext) {
    let (chat, cx, mut queued) = queued_chat(cx);
    click(cx, "withdraw-synthetic-message");
    draw(cx);
    queued.run(
        ChatIntent::Withdraw {
            message_id: "synthetic-message".into(),
        },
        &chat,
        cx,
    );
    // Iced keeps the bubble, "Withdrawing…", until the withdrawal is confirmed.
    assert_eq!(queued_row(&chat, cx).1, RowKind::User);
    assert!(cx.debug_bounds("transcript-row-0").is_some());
    queued.lifecycle(MessageState::Cancelled, &chat, cx);
    assert_eq!(queued_row(&chat, cx).1, RowKind::Withdrawn);
    assert!(cx.debug_bounds("transcript-row-0").is_none());
    assert!(cx.debug_bounds("withdraw-synthetic-message").is_none());
    // A rebuild from the conversation drops it too.
    let rebuilt = ChatModel::from_conversation(&queued.adapter.state().conversation);
    assert!(
        rebuilt.rows.iter().all(|row| row.kind != RowKind::User
            || row.prompt_id.as_deref() != Some("synthetic-message"))
    );
}

#[gpui::test]
fn withdrawn_message_not_in_find(cx: &mut TestAppContext) {
    let (chat, cx, mut queued) = queued_chat(cx);
    chat.update_in(cx, |view, window, cx| {
        view.open_find(&Find, window, cx);
        view.change_find("later".into(), window, cx);
    });
    draw(cx);
    let index = queued_row(&chat, cx).0;
    chat.read_with(cx, |view, _| {
        assert_eq!(
            view.find
                .matches
                .iter()
                .map(|found| found.row)
                .collect::<Vec<_>>(),
            [index]
        );
    });
    queued.run(
        ChatIntent::Withdraw {
            message_id: "synthetic-message".into(),
        },
        &chat,
        cx,
    );
    queued.lifecycle(MessageState::Cancelled, &chat, cx);
    // An open find drops the match, and a fresh search does not find it.
    chat.read_with(cx, |view, _| assert!(view.find.matches.is_empty()));
    chat.update_in(cx, |view, window, cx| {
        view.change_find("late".into(), window, cx);
        view.change_find("later".into(), window, cx);
    });
    chat.read_with(cx, |view, _| assert!(view.find.matches.is_empty()));
}

#[gpui::test]
fn unsent_after_exit_shows_iced_text(cx: &mut TestAppContext) {
    let (chat, cx, mut queued) = queued_chat(cx);
    queued.receive(HarnessEvent::ProcessExited { code: Some(1) }, &chat, cx);
    let tag = bubble_tag(&chat, cx).expect("unsent tag on the bubble");
    assert_eq!(tag.label, "Not sent: Claude exited before reading it");
    assert_eq!(tag.withdraw, None);
    // The bubble stays, so the text can be sent again.
    assert_eq!(queued_row(&chat, cx).1, RowKind::User);
}

#[gpui::test]
fn c05_slash_row_names_the_command_and_tooltips_its_description(cx: &mut TestAppContext) {
    let (chat, cx) = setup(
        vec![Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::Ready {
                session_id: None,
                permission_mode: None,
                models: vec![],
                modes: vec![],
                commands: vec![
                    serde_json::json!({"name":"review","description":"Review changes","argumentHint":"[focus]"}),
                    serde_json::json!({"name":"clear"}),
                ],
            }),
        }],
        cx,
    );
    chat.read_with(cx, |view, _| {
        let matches = view.composer_state.slash_matches("/");
        assert_eq!(matches[0].label(), "/review [focus]");
        assert_eq!(matches[0].tooltip(), Some("Review changes"));
        assert_eq!(matches[1].label(), "/clear");
        assert_eq!(matches[1].tooltip(), None, "no description, no title");
    });
    chat.update_in(cx, |view, window, cx| {
        view.set_read_only(None, cx);
        view.set_intent_callback(Rc::new(|_, _| written()), cx);
        view.composer.update(cx, |input, cx| {
            input.set_value("/re", window, cx);
            input.set_selected_range(3..3, cx);
            input.focus(window, cx);
        });
    });
    draw(cx);
    click(cx, "slash-review");
    draw(cx);
    assert_eq!(
        chat.read_with(cx, |view, cx| view.composer.read(cx).value().to_string()),
        "/review "
    );
}

/// A review card whose findings carry long synthetic text: words, one long
/// unbroken token and a deep path.
fn long_findings_card() -> crate::delegation::DelegationCard {
    let mut card = crate::delegation::tests::card("review", "completed");
    let long = "the explanation keeps going so that it is much wider than any card ".repeat(6);
    let token = "x".repeat(240);
    let path = format!("src/{}/module.rs", "nested-directory-name".repeat(8));
    card.delegation.result = Some(
        serde_json::from_value(serde_json::json!({
            "reviewed":{"head":"123456789abc","dirty":false,"target_description":"vs v6"},
            "findings":{"verdict":"needs_changes","structured":true,"summary":"Two issues","reviewer":{"harness":"Codex"},
                "findings":[
                    {"id":"F1","severity":"P2","title":format!("A long finding title {long}"),"body":long,"file":path,"line_start":12,"line_end":14},
                    {"id":"F2","severity":"P3","title":"Unbroken token","body":token}
                ]}
        }))
        .unwrap(),
    );
    card.delegation.target.as_mut().unwrap().focus = Some(format!("focus {long}"));
    card
}
fn right(bounds: gpui::Bounds<gpui::Pixels>) -> f32 {
    f32::from(bounds.origin.x + bounds.size.width)
}

#[gpui::test]
fn transcript_finding_rows_wrap_within_card(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    install_card(long_findings_card(), &chat, cx);
    for width in [900., 600.] {
        cx.simulate_resize(size(px(width), px(900.)));
        draw(cx);
        let card = cx.debug_bounds("delegation-card-0").expect("card drawn");
        assert!(
            right(card) <= width + 0.5,
            "card {card:?} wider than {width}"
        );
        for selector in [
            "delegation-fold-0",
            "finding-main-0-F1",
            "finding-main-0-F2",
            "finding-location-0-F1",
        ] {
            let inner = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"));
            assert!(
                right(inner) <= right(card) + 0.5,
                "{selector} {inner:?} runs past card {card:?} at {width}"
            );
        }
        // Wrapped, not clipped: the long text takes several lines.
        let main = cx.debug_bounds("finding-main-0-F1").unwrap();
        assert!(
            main.size.height > px(80.),
            "{main:?} did not wrap at {width}"
        );
        let token = cx.debug_bounds("finding-main-0-F2").unwrap();
        assert!(
            token.size.height > px(40.),
            "{token:?} did not wrap at {width}"
        );
    }
}

#[gpui::test]
fn composer_worker_rows_stay_inside_composer(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update_in(cx, |view, _, cx| {
        view.set_roster(
            vec![
                roster::RosterEntry {
                    id: "worker-1".into(),
                    name: "Worker · Codex".into(),
                    model: "default model".into(),
                    checkout: format!(
                        "/private/tmp/worktrees/601a3046-{}-default-task-base-fetched-origin-dev",
                        "synthetic-worktree-name".repeat(6)
                    ),
                    activity: "Running Bash".into(),
                    state: roster::WorkerState::Running,
                    started_at_ms: crate::composer::now_ms().saturating_sub(5_000),
                    ended_at_ms: None,
                },
                roster::RosterEntry {
                    id: "worker-2".into(),
                    name: "Review · Codex review".into(),
                    model: "default model".into(),
                    checkout: format!("/private/tmp/{}", "x".repeat(200)),
                    activity: String::new(),
                    state: roster::WorkerState::Done,
                    started_at_ms: 0,
                    ended_at_ms: Some(60_000),
                },
            ],
            cx,
        );
    });
    for width in [900., 600.] {
        cx.simulate_resize(size(px(width), px(900.)));
        draw(cx);
        let footer = cx.debug_bounds("chat-footer").expect("footer drawn");
        for id in ["worker-1", "worker-2"] {
            for part in ["roster", "roster-what", "roster-meta", "roster-dismiss"] {
                let selector: &'static str = Box::leak(format!("{part}-{id}").into_boxed_str());
                let inner = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("missing {selector}"));
                assert!(
                    right(inner) <= right(footer) + 0.5,
                    "{selector} {inner:?} runs past the composer {footer:?} at {width}"
                );
            }
        }
    }
}

/// The reason `ChatPane::history` gives an imported Iced chat.
const IMPORTED_REASON: &str = "Preview history · read only · resume not in preview yet";

#[gpui::test]
fn imported_chat_composer_shows_read_only(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    let intents = Rc::new(std::cell::Cell::new(0));
    let captured = intents.clone();
    chat.update_in(cx, |view, window, cx| {
        view.set_intent_callback(
            Rc::new(move |_, _| {
                captured.set(captured.get() + 1);
                Receipt::rejected("synthetic")
            }),
            cx,
        );
        view.set_read_only(Some(IMPORTED_REASON.into()), cx);
        view.composer
            .update(cx, |input, cx| input.focus(window, cx));
    });
    draw(cx);
    cx.simulate_input("typed into a read-only chat");
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let input = view.composer.read(cx).presentation();
        assert!(input.is_disabled());
        assert_eq!(input.placeholder().as_ref(), "Read-only chat");
        assert_eq!(view.composer.read(cx).value().as_ref(), "");
    });
    assert!(
        cx.debug_bounds("chat-read-only-note").is_some(),
        "Iced's read-only note shows the reason"
    );
    assert!(
        cx.debug_bounds("chat-activity").is_none(),
        "no `Ready` for a chat that cannot run"
    );
    click(cx, "composer-send");
    click(cx, "composer-stop");
    assert_eq!(intents.get(), 0, "Send and Stop are disabled");
}

#[gpui::test]
fn native_chat_composer_unchanged(cx: &mut TestAppContext) {
    let (chat, cx) = setup(vec![], cx);
    chat.update_in(cx, |view, _, cx| {
        view.set_intent_callback(Rc::new(|_, _| Receipt::rejected("synthetic")), cx);
        view.set_read_only(Some(IMPORTED_REASON.into()), cx);
    });
    draw(cx);
    // A chat that becomes writable gets its composer back.
    chat.update_in(cx, |view, _, cx| view.set_read_only(None, cx));
    draw(cx);
    chat.read_with(cx, |view, cx| {
        let input = view.composer.read(cx).presentation();
        assert!(!input.is_disabled());
        assert_eq!(
            input.placeholder().as_ref(),
            "Message… (Enter to send, Shift+Enter for newline)"
        );
    });
    assert!(cx.debug_bounds("chat-read-only-note").is_none());
    assert!(cx.debug_bounds("chat-activity").is_some());
    assert!(cx.debug_bounds("composer-send").is_some());
    assert!(cx.debug_bounds("composer-stop").is_some());
}
