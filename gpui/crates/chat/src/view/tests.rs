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
                match edit {
                    0 => "",
                    1 => "newer draft",
                    _ => "submitted draft",
                }
            );
            assert_eq!(view.images, if edit == 1 { images } else { vec![] });
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
        view.composer
            .update(cx, |input, cx| input.set_value("/cl", window, cx))
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
        view.composer
            .update(cx, |input, cx| input.set_value("/cl", window, cx))
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
