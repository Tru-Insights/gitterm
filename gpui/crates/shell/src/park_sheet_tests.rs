use super::*;
use gitterm_core::{chats::ChatBackend, session};
use gpui::{TestAppContext, VisualTestContext, rgb};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, UNIX_EPOCH},
};

fn state() -> park::State {
    park::State {
        open: (1..=2)
            .map(|uid| park::OpenWorkspace {
                target: park::Target {
                    uid,
                    tabs: vec![session::Target {
                        tab_id: uid as usize,
                        session_uid: format!("session-{uid}"),
                        task_id: None,
                        task_session_id: None,
                    }],
                },
                key: park::Key {
                    name: format!("Work {uid}"),
                    dir: format!("/fixture/{uid}"),
                },
                running: None,
                remote_id: None,
                conversations: vec![],
            })
            .collect(),
        closed: vec![],
    }
}
fn sheet(owns_chats: bool) -> WorkspaceSheet {
    WorkspaceSheet::open(
        &state(),
        1,
        WorkspaceFacts {
            owns_chats,
            ..Default::default()
        },
    )
    .unwrap()
}
fn chat(id: &str, latest: Option<&str>, seconds: u64, root: Option<&str>) -> ChatIndexEntry {
    ChatIndexEntry {
        id: id.into(),
        backend: ChatBackend::Claude,
        path: "/fixture/chat.jsonl".into(),
        cwd: "/fixture".into(),
        repo_root: None,
        is_worktree: false,
        branch: None,
        title: format!("Title {id}"),
        latest: latest.map(String::from),
        mtime: UNIX_EPOCH + Duration::from_secs(seconds),
        size: 0,
        dead_cwd: false,
        facts: None,
        root_id: root.map(String::from),
    }
}
fn row() -> ParkedRow {
    ParkedRow {
        id: state().open[0].key.clone(),
        chats: 1,
        info: Some(park::Info::new("Last line", "2026-11-20T00:00:00Z".into())),
        running: false,
        color: rgb(0x89b4fa).into(),
        pr_news: None,
    }
}

#[test]
fn owns_chats_selects_park_and_folder_selects_close_with_iced_order() {
    let close = sheet(false);
    assert_eq!(close.kind, Kind::Close);
    assert_eq!(close.title(), "Close Work 1?");
    assert_eq!(
        close
            .buttons()
            .iter()
            .map(|action| action.label())
            .collect::<Vec<_>>(),
        ["Cancel", "Park", "Close"]
    );
    let park = sheet(true);
    assert_eq!(park.kind, Kind::Park);
    assert_eq!(park.title(), "Park Work 1");
    assert_eq!(
        park.buttons()
            .iter()
            .map(|action| action.label())
            .collect::<Vec<_>>(),
        ["Cancel", "Park"]
    );
    assert!(!park.keep_running());
    assert_eq!(park.lines(), [STOP_DESCRIPTION]);
    assert!(park.note().is_empty());
}

#[test]
fn core_guard_counts_visible_workspaces_and_rejects_missing_targets() {
    let mut state = state();
    for owns_chats in [false, true] {
        assert!(
            WorkspaceSheet::open(
                &state,
                99,
                WorkspaceFacts {
                    owns_chats,
                    ..Default::default()
                }
            )
            .is_none()
        );
    }
    state.open[1].running = Some(park::Info::new("busy", "stamp".into()));
    for owns_chats in [false, true] {
        assert!(
            WorkspaceSheet::open(
                &state,
                1,
                WorkspaceFacts {
                    owns_chats,
                    ..Default::default()
                }
            )
            .is_none()
        );
    }
    // Running parked work is governed by core rather than a duplicated guard.
    assert!(WorkspaceSheet::open(&state, 2, WorkspaceFacts::default()).is_some());
}

#[test]
fn note_uses_latest_root_and_preserves_edits_when_index_lands() {
    let old = chat("old", Some("old line"), 1, None);
    let recent = chat("recent", Some("new line"), 2, None);
    let child = chat("child", Some("child line"), 3, Some("recent"));
    let mut model = WorkspaceSheet::open(
        &state(),
        1,
        WorkspaceFacts {
            owns_chats: true,
            note_chats: &[&old, &recent, &child],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(model.note(), "new line");
    let titled = chat("titled", None, 4, None);
    model.refill(&[&titled]);
    assert_eq!(model.note(), "Title titled");
    model.set_note("My edited note".into());
    model.refill(&[&recent]);
    assert_eq!(model.note(), "My edited note");
    assert_eq!(prefill(&[&child]), "");
}

#[test]
fn note_scope_never_borrows_another_workspaces_chat() {
    let entries = vec![
        chat("mine", Some("mine"), 1, None),
        chat("other", Some("other"), 2, None),
        chat("child", Some("child"), 3, Some("other")),
    ];
    let selected = note_chats(&entries, &["mine".into()], &[], &["other".into()]);
    assert_eq!(prefill(&selected), "mine");
    let selected = note_chats(
        &entries,
        &[],
        &["mine".into(), "other".into()],
        &["other".into()],
    );
    assert_eq!(prefill(&selected), "mine");
    assert!(note_chats(&entries, &[], &["other".into()], &["other".into()]).is_empty());
}

#[test]
fn working_chat_warning_and_keep_running_text_match_iced() {
    let mut model = WorkspaceSheet::open(
        &state(),
        1,
        WorkspaceFacts {
            owns_chats: true,
            working_chat: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        model.warning(),
        Some("● a chat is still working; parking stops it")
    );
    model.set_keep_running(true);
    assert_eq!(model.warning(), None);
    assert_eq!(model.lines(), [RUN_DESCRIPTION]);
    model.set_keep_running(false);
    assert!(model.warning().is_some());
    assert_eq!(sheet(false).warning(), None);
    assert_eq!(sheet(true).warning(), None);
}

#[test]
fn close_copy_explains_real_counts_and_continuations_in_order() {
    for (t, c, expected) in [
        (0, 0, "Nothing is running in it."),
        (1, 0, "Its 1 terminal stops."),
        (5, 0, "Its 5 terminals stop."),
        (0, 1, "Its 1 chat stops."),
        (0, 2, "Its 2 chats stop."),
        (2, 1, "Its 2 terminals and 1 chat stop."),
    ] {
        assert_eq!(stops_line(t, c), expected);
    }
    let model = WorkspaceSheet::open(
        &state(),
        1,
        WorkspaceFacts {
            terminals: 2,
            chats: 1,
            others_have_chats: true,
            shared_repos: &[("repo".into(), "Other".into())],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        model.lines(),
        [
            "Its 2 terminals and 1 chat stop.",
            "Chats in other workspaces keep running.",
            "repo stays open in Other.",
            ALTERNATIVE
        ]
    );
}

#[test]
fn all_workspace_outcomes_and_keyboard_defaults_match_iced() {
    let mut close = sheet(false);
    close.set_note("hidden edit".into());
    close.set_keep_running(true);
    assert_eq!(
        close.activate(SheetAction::Close),
        Some(Outcome::Close {
            target: close.target.clone()
        })
    );
    assert_eq!(
        close.activate(SheetAction::Park),
        Some(Outcome::Park {
            note: "".into(),
            keep_running: false
        })
    );
    assert_eq!(close.key("enter"), None);
    assert_eq!(close.key("escape"), Some(Outcome::Cancel));
    assert_eq!(close.activate(SheetAction::Cancel), Some(Outcome::Cancel));
    let mut park = sheet(true);
    park.set_note("  My note  ".into());
    park.set_keep_running(true);
    let outcome = Outcome::Park {
        note: "  My note  ".into(),
        keep_running: true,
    };
    assert_eq!(park.key("enter"), Some(outcome.clone()));
    assert_eq!(park.activate(SheetAction::Park), Some(outcome));
    assert_eq!(park.key("escape"), Some(Outcome::Cancel));
    assert_eq!(park.activate(SheetAction::Close), None);
    assert_eq!(park.key("tab"), None);
}

#[test]
fn outcomes_translate_to_core_intents_and_revalidate_guards() {
    let model = sheet(true);
    let outcome = Outcome::Park {
        note: " note ".into(),
        keep_running: true,
    };
    assert_eq!(outcome.intent(None, "stamp".into()), None);
    let intent = outcome.intent(Some(&model.target), "stamp".into()).unwrap();
    assert_eq!(
        state().reduce(intent.clone()),
        park::Effect::Park {
            target: model.target.clone(),
            mode: park::Mode::KeepRunning,
            info: park::Info::new("note", "stamp".into())
        }
    );
    let mut changed = state();
    changed.open.pop();
    assert_eq!(changed.reduce(intent), park::Effect::None);
    let mut rebound = model.target.clone();
    rebound.tabs[0].session_uid.push('x');
    assert_eq!(state().resolve(&rebound), None);
    assert_eq!(
        Outcome::Close {
            target: model.target
        }
        .intent(None, "".into()),
        Some(park::Intent::Close(1))
    );
    assert_eq!(Outcome::Cancel.intent(None, "".into()), None);
}

#[test]
fn parked_rows_keep_stable_key_and_offer_resume_then_done_consequences() {
    let mut row = row();
    assert_eq!(row.owned_label(), "1 chat");
    assert_eq!(row.note(), "Last line");
    let mut state = state();
    state.open[0].running = row.info.clone();
    assert_eq!(row.resume(), Outcome::Resume { id: row.id.clone() });
    assert_eq!(row.done(), Outcome::Done { id: row.id.clone() });
    assert!(matches!(
        state.reduce(row.resume().intent(None, "".into()).unwrap()),
        park::Effect::Return {
            uid: 1,
            activate: true,
            ..
        }
    ));
    assert!(matches!(
        state.reduce(row.done().intent(None, "".into()).unwrap()),
        park::Effect::Done { stop: Some(_), .. }
    ));
    state.open.remove(0);
    state.closed.push(park::ClosedWorkspace {
        key: row.id.clone(),
        parked: true,
    });
    assert_eq!(
        state.reduce(row.resume().intent(None, "".into()).unwrap()),
        park::Effect::RestoreStopped { index: 0 }
    );
    assert_eq!(
        state.reduce(row.done().intent(None, "".into()).unwrap()),
        park::Effect::Done {
            stop: None,
            finish: 0
        }
    );
    row.chats = 0;
    row.info = None;
    assert_eq!(row.owned_label(), "folder");
    assert_eq!(row.note(), "No note");
    row.chats = 2;
    assert_eq!(row.owned_label(), "2 chats");
}

// Test-platform windows only; no native GUI, OS input or production config.
fn setup(
    model: WorkspaceSheet,
    cx: &mut TestAppContext,
) -> (Entity<ParkSheet>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let sheet = cx.new(|cx| ParkSheet::workspace(model, window, cx));
        gpui_component::Root::new(sheet, window, cx)
    });
    let sheet = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<ParkSheet>().unwrap()
    });
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
    (sheet, cx)
}
fn outcomes(view: &Entity<ParkSheet>, cx: &mut VisualTestContext) -> Rc<RefCell<Vec<Outcome>>> {
    let events = Rc::new(RefCell::new(vec![]));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(view, move |_, event, _| {
            events.borrow_mut().push(event.clone())
        })
        .detach();
    });
    events
}

#[gpui::test]
fn native_note_is_selected_editable_and_enter_emits_once(cx: &mut TestAppContext) {
    let initial = chat("mine", Some("Prefilled note"), 1, None);
    let model = WorkspaceSheet::open(
        &state(),
        1,
        WorkspaceFacts {
            owns_chats: true,
            note_chats: &[&initial],
            ..Default::default()
        },
    )
    .unwrap();
    let (view, cx) = setup(model, cx);
    let events = outcomes(&view, cx);
    cx.simulate_input("Replacement");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        [Outcome::Park {
            note: "Replacement".into(),
            keep_running: false
        }]
    );
    view.update_in(cx, |view, _, cx| view.activate(SheetAction::Park, cx));
    assert_eq!(events.borrow().len(), 1);
}

#[gpui::test]
fn native_close_enter_does_nothing_and_escape_cancels(cx: &mut TestAppContext) {
    let (view, cx) = setup(sheet(false), cx);
    let events = outcomes(&view, cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(events.borrow().is_empty());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(*events.borrow(), [Outcome::Cancel]);
}

#[gpui::test]
fn native_park_escape_cancels_from_note_focus(cx: &mut TestAppContext) {
    let (view, cx) = setup(sheet(true), cx);
    let events = outcomes(&view, cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(*events.borrow(), [Outcome::Cancel]);
}

#[gpui::test]
fn native_parked_list_done_can_repeat_until_close(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let sheet = cx.new(|cx| ParkSheet::parked(vec![row()], window, cx));
        gpui_component::Root::new(sheet, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<ParkSheet>().unwrap()
    });
    let events = outcomes(&view, cx);
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    view.update_in(cx, |view, _, cx| view.emit(Some(row().done()), cx));
    view.update_in(cx, |view, _, cx| view.emit(Some(row().done()), cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        [row().done(), row().done(), Outcome::Cancel]
    );
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
    let bounds = cx.debug_bounds(selector).expect("visible sheet control");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn native_close_button_emits_captured_target(cx: &mut TestAppContext) {
    let model = sheet(false);
    let target = model.target.clone();
    let (view, cx) = setup(model, cx);
    let events = outcomes(&view, cx);
    click("Close", cx);
    assert_eq!(*events.borrow(), [Outcome::Close { target }]);
}

#[gpui::test]
fn native_folder_park_button_emits_stopped_empty_note(cx: &mut TestAppContext) {
    let (view, cx) = setup(sheet(false), cx);
    let events = outcomes(&view, cx);
    click("Park", cx);
    assert_eq!(
        *events.borrow(),
        [Outcome::Park {
            note: "".into(),
            keep_running: false
        }]
    );
}

#[gpui::test]
fn native_keep_running_and_park_button_emit_edited_values(cx: &mut TestAppContext) {
    let (view, cx) = setup(sheet(true), cx);
    let events = outcomes(&view, cx);
    cx.simulate_input("Come back here");
    click("keep-running", cx);
    click("Park", cx);
    assert_eq!(
        *events.borrow(),
        [Outcome::Park {
            note: "Come back here".into(),
            keep_running: true
        }]
    );
}

#[gpui::test]
fn native_parked_row_buttons_emit_stable_key(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let sheet = cx.new(|cx| ParkSheet::parked(vec![row()], window, cx));
        gpui_component::Root::new(sheet, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<ParkSheet>().unwrap()
    });
    let events = outcomes(&view, cx);
    click("parked-done-0", cx);
    // Done leaves the list open. The host replaces its rows from the catalog.
    view.update_in(cx, |view, _, cx| view.set_rows(vec![row()], cx));
    click("parked-resume-0", cx);
    assert_eq!(*events.borrow(), [row().done(), row().resume()]);
}

#[gpui::test]
fn native_enter_parks_when_keep_running_has_focus(cx: &mut TestAppContext) {
    let (view, cx) = setup(sheet(true), cx);
    let events = outcomes(&view, cx);
    click("keep-running", cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        [Outcome::Park {
            note: "".into(),
            keep_running: true
        }]
    );
}
