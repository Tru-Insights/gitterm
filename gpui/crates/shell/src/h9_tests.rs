//! H9 dispatch uses temporary source/preview roots and never starts an agent.
use super::*;
use app_menus::*;
use gpui::{TestAppContext, VisualTestContext};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn assert_key_action(key: &'static str, action: impl gpui::Action, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let input = gpui::Keystroke::parse(key).unwrap();
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let (bindings, _) = keymap.bindings_for_input(&[input], &window.context_stack());
        assert!(
            bindings[0].action().partial_eq(&action),
            "{key}: {}",
            bindings[0].action().name()
        );
    });
}
fn dispatch(action: impl gpui::Action, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(action), cx));
    draw(cx);
}

#[gpui::test]
fn width_and_navigation_dispatch_and_only_persist_in_preview(cx: &mut TestAppContext) {
    let (root, shell, cx) = h7_tests::fixture(cx);
    let iced = root.path().join("iced");
    let before =
        ["settings.json", "workspaces.json"].map(|name| std::fs::read(iced.join(name)).unwrap());
    for width in gitterm_core::settings::ChatWidth::ALL {
        dispatch(SetChatWidth { width }, cx);
        cx.read(|cx| {
            assert_eq!(theme::chat_width(cx), width);
            assert_eq!(
                shell.read(cx).active(cx).store.regions.chat_width,
                Some(width)
            );
        });
    }
    dispatch(ChatsOverviewOpen, cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).active(cx).store.regions.sidebar_mode,
            SidebarMode::Find
        );
        assert!(!shell.read(cx).active(cx).store.regions.sidebar_collapsed);
        assert!(
            shell
                .read(cx)
                .find_state
                .borrow()
                .selected(&shell.read(cx).find_owner(cx))
                .is_none()
        );
    });
    dispatch(OpenPlansViewer, cx);
    assert!(cx.debug_bounds("plans-viewer").is_some());
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(!cx.read(|cx| shell.read(cx).plans_open));
    dispatch(OpenPlansViewer, cx);
    dispatch(OpenPlansViewer, cx);
    assert!(!cx.read(|cx| shell.read(cx).plans_open));
    let (path, writer) = cx.read(|cx| {
        let shell = shell.read(cx);
        (
            shell.active(cx).store.path.clone(),
            shell.owner.read(cx).writer().unwrap(),
        )
    });
    writer.close().unwrap().blocking_recv().unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["regions"]["chat_width"], "full");
    assert_eq!(
        before,
        ["settings.json", "workspaces.json"].map(|name| std::fs::read(iced.join(name)).unwrap())
    );
    assert_eq!(std::fs::read_dir(iced).unwrap().count(), 2);
}

#[gpui::test]
fn launch_adapters_report_preview_error_without_creating_sessions(cx: &mut TestAppContext) {
    let (_root, shell, cx) = h7_tests::fixture(cx);
    let count = cx.read(|cx| shell.read(cx).active(cx).sessions.len());
    for action in [
        Box::new(DebugSpawnAgentTab) as Box<dyn gpui::Action>,
        Box::new(LaunchAgentPreset { index: 0 }),
        Box::new(TaskChildLaunchPreset { index: 0 }),
    ] {
        cx.update(|window, cx| window.dispatch_action(action, cx));
        draw(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(
                shell
                    .owner
                    .read(cx)
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("not available in preview")
            );
            assert_eq!(shell.active(cx).sessions.len(), count);
        });
    }
    // No open sheet: the adapter is a no-op.
    dispatch(ParkSubmit, cx);
}

#[gpui::test]
fn composer_and_slash_actions_dispatch_once_through_native_input(cx: &mut TestAppContext) {
    use gitterm_gpui_chat::{
        ChatIntent, ChatView,
        adapter::Receipt,
        events::{Entry, HarnessEvent, Record},
    };
    let (_root, shell, cx) = h6_tests::sample(cx);
    dispatch(TabSelect { index: 0 }, cx);
    let intents = Rc::new(RefCell::new(Vec::new()));
    let captured = intents.clone();
    let chat = cx.update(|window, cx| {
        cx.new(|cx| {
            let mut chat = ChatView::new(
                vec![Record {
                    at_ms: None,
                    entry: Entry::Harness(HarnessEvent::Ready {
                        session_id: None,
                        permission_mode: Some("default".into()),
                        models: vec![],
                        modes: vec![],
                        commands: vec![
                            serde_json::json!({"name":"clear"}),
                            serde_json::json!({"name":"checkpoint","argumentHint":"[message]"}),
                        ],
                    }),
                }],
                window,
                cx,
            );
            chat.set_read_only(None, cx);
            chat.set_intent_callback(
                Rc::new(move |intent, _| {
                    captured.borrow_mut().push(intent);
                    Receipt::rejected("synthetic")
                }),
                cx,
            );
            chat
        })
    });
    cx.update(|_, cx| {
        let state = shell.read(cx).active(cx);
        let pane = state.panes[&session_pane_id(state.active_session.unwrap())].clone();
        pane.update(cx, |pane, cx| {
            pane.content = pane::PaneContent::Chat(chat.clone());
            cx.notify();
        });
    });
    draw(cx);
    let bounds = cx.debug_bounds("composer-input").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
    cx.simulate_input("hello");
    draw(cx);
    assert_key_action("enter", SubmitPrompt, cx);
    assert_key_action("shift-enter", Newline, cx);
    cx.simulate_keystrokes("shift-enter");
    draw(cx);
    assert!(intents.borrow().is_empty());
    cx.simulate_input("world");
    draw(cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(intents.borrow().len(), 1, "Enter sends exactly once");
    assert!(
        matches!(&intents.borrow()[0], ChatIntent::Send { text, .. } if text == "hello\nworld")
    );
    dispatch(Newline, cx);
    dispatch(SubmitPrompt, cx);
    assert_eq!(intents.borrow().len(), 2);
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("/");
    draw(cx);
    assert_key_action("down", SlashNext, cx);
    assert_key_action("up", SlashPrevious, cx);
    assert_key_action("tab", FillSlash, cx);
    assert_key_action("enter", SubmitSlash, cx);
    assert_key_action("escape", CloseSlash, cx);
    dispatch(SlashNext, cx);
    dispatch(SlashPrevious, cx);
    dispatch(FillSlash, cx);
    assert_eq!(intents.borrow().len(), 2, "fill edits only");
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("/cl");
    draw(cx);
    dispatch(SubmitSlash, cx);
    assert_eq!(intents.borrow().len(), 3);
    assert!(matches!(&intents.borrow()[2], ChatIntent::Send { text, .. } if text == "/clear "));
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("/cl");
    draw(cx);
    dispatch(CloseSlash, cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(intents.borrow().len(), 4);
    assert!(matches!(&intents.borrow()[3], ChatIntent::Send { text, .. } if text == "/cl"));
}

#[gpui::test]
fn park_submit_uses_sheet_once_and_preserves_its_note(cx: &mut TestAppContext) {
    let (_root, shell, cx) = h6_tests::sample(cx);
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let captured = outcomes.clone();
    let _subscription = cx.update(|window, cx| {
        let (state, uid) = shell.read_with(cx, |shell, cx| {
            (shell.park_projection(cx), shell.active(cx).runtime_uid)
        });
        let mut model = park_sheet::WorkspaceSheet::open(
            &state,
            uid,
            park_sheet::WorkspaceFacts {
                owns_chats: true,
                ..Default::default()
            },
        )
        .unwrap();
        model.set_note("keep this note".into());
        let sheet = cx.new(|cx| park_sheet::ParkSheet::workspace(model, window, cx));
        let subscription = cx.subscribe(&sheet, move |_, outcome: &park_sheet::Outcome, _| {
            captured.borrow_mut().push(outcome.clone())
        });
        shell.update(cx, |shell, cx| {
            shell.park_sheet = Some(sheet);
            cx.notify();
        });
        subscription
    });
    draw(cx);
    assert_key_action("enter", ParkSubmit, cx);
    dispatch(ParkSubmit, cx);
    dispatch(ParkSubmit, cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        outcomes.borrow().as_slice(),
        &[park_sheet::Outcome::Park {
            note: "keep this note".into(),
            keep_running: false
        }]
    );
}

#[gpui::test]
fn plans_document_uses_existing_read_only_file_viewer(cx: &mut TestAppContext) {
    use gitterm_core::work::{
        documents::{CopySource, Document, DocumentCopy, ScanResult},
        state::{Effect, Intent},
    };
    let (root, shell, cx) = h7_tests::fixture(cx);
    let path = root.path().join("work/file.txt");
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            let workspace = shell.active(cx).workspace.id.0;
            let effects = shell
                .work_state
                .borrow_mut()
                .reduce(Intent::ScanDocuments(workspace));
            let stamp = effects
                .into_iter()
                .find_map(|effect| match effect {
                    Effect::ScanDocuments { stamp, .. } => Some(stamp),
                    _ => None,
                })
                .unwrap();
            shell
                .work_state
                .borrow_mut()
                .reduce(Intent::DocumentsFinished {
                    stamp,
                    result: Ok(ScanResult {
                        documents: vec![Document {
                            key: "fixture".into(),
                            title: "Fixture document".into(),
                            rel: ".plans/fixture.md".into(),
                            differ: false,
                            copies: vec![DocumentCopy {
                                path: path.clone(),
                                source: CopySource::WorkDocs,
                                modified: std::time::SystemTime::now(),
                            }],
                        }],
                        truncated: false,
                    }),
                    now: Instant::now(),
                });
            cx.notify();
        })
    });
    dispatch(OpenPlansViewer, cx);
    let bounds = cx.debug_bounds("plan-0").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(!shell.plans_open);
        assert_eq!(
            shell.selected_files[&shell.active(cx).workspace.id].path,
            path
        );
    });
}

#[gpui::test]
fn retained_preview_history_changes_width_without_rebuilding(cx: &mut TestAppContext) {
    let (root, shell, cx) = h6_tests::sample(cx);
    dispatch(TabSelect { index: 0 }, cx);
    let preview = cx.update(|window, cx| {
        let shutdown = shell.read(cx).owner.read(cx).chat_shutdown.clone();
        let preview = cx.new(|cx| {
            chat::ChatPane::history(
                shutdown,
                chat::SavedChat {
                    uid: "width-history".into(),
                    cwd: root.path().into(),
                    provider: root.path().join("provider"),
                    session_id: None,
                    records: vec![],
                },
                window,
                cx,
            )
        });
        let state = shell.read(cx).active(cx);
        let pane = state.panes[&session_pane_id(state.active_session.unwrap())].clone();
        pane.update(cx, |pane, cx| {
            pane.content = pane::PaneContent::PreviewChat(preview.clone());
            cx.notify();
        });
        preview
    });
    dispatch(
        SetChatWidth {
            width: gitterm_core::settings::ChatWidth::Narrow,
        },
        cx,
    );
    assert_eq!(cx.debug_bounds("native-chat").unwrap().size.width, px(764.));
    dispatch(
        SetChatWidth {
            width: gitterm_core::settings::ChatWidth::Full,
        },
        cx,
    );
    assert!(cx.debug_bounds("native-chat").unwrap().size.width > px(764.));
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        let pane = &state.panes[&session_pane_id(state.active_session.unwrap())];
        let pane::PaneContent::PreviewChat(current) = &pane.read(cx).content else {
            panic!("preview history");
        };
        assert_eq!(current.entity_id(), preview.entity_id());
    });
}
