use super::*;
use gitterm_core::tasks::{IssueProvider, IssueReference, TaskAttentionReason};
use gpui::{TestAppContext, VisualTestContext};
use std::{cell::RefCell, rc::Rc};

fn record(id: &str, title: &str) -> TaskRecord {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/stage45/tasks/old.tasks.json"
    ))
    .unwrap();
    let mut task: TaskRecord = serde_json::from_value(fixture["tasks"][0].clone()).unwrap();
    task.task_id = id.into();
    task.title = title.into();
    task
}

fn inputs() -> Inputs {
    let mut attention = record("attention", "Zebra needs input");
    attention.attention.reason = Some(TaskAttentionReason::RequiresInput);
    let mut issue = record("issue", "Alpha task");
    issue.issue = Some(IssueReference {
        provider: IssueProvider::Linear,
        key: "TRU-274".into(),
        url: None,
    });
    Inputs {
        // Deliberately neither alphabetic nor MRU order.
        tasks: [
            (record("current", "Current task"), "Working · 3m"),
            (issue, "Ready · not started"),
            (record("recent", "Recent task"), "Done · review"),
            (attention, "Needs you — input required"),
        ]
        .into_iter()
        .filter_map(|(task, detail)| TaskRow::from_task(&task, detail.into()))
        .collect(),
        general_session_count: 2,
        current_task_id: Some("current".into()),
        context_mru: vec![Some("current".into()), Some("recent".into()), None],
    }
}

fn opened() -> Model {
    let mut model = Model::new(inputs());
    model.open();
    model
}

fn highlighted(model: &Model) -> Row {
    model
        .rows()
        .into_iter()
        .find(|row| row.is_highlighted)
        .unwrap()
}

#[test]
fn fixture_order_labels_details_markers_and_current_badge_match_iced() {
    let model = opened();
    let rows = model.rows();
    assert_eq!(
        rows.iter()
            .map(|r| (r.label.as_str(), r.detail.as_str(), r.marker(), r.badge()))
            .collect::<Vec<_>>(),
        [
            ("Zebra needs input", "Needs you — input required", "◇", ""),
            ("Recent task", "Done · review", "◇", ""),
            ("General", "2 workspace sessions", "●", ""),
            ("TRU-274  Alpha task", "Ready · not started", "◇", ""),
            ("Current task", "Working · 3m", "◇", "CURRENT"),
        ]
    );
    assert!(rows[0].needs_attention);
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("recent"));
    let mut task = record("unread", "Unread");
    task.attention.unread = true;
    assert!(
        TaskRow::from_task(&task, "Done · review".into())
            .unwrap()
            .needs_attention
    );
    task.lifecycle = TaskLifecycle::Archived;
    assert!(TaskRow::from_task(&task, "Archived".into()).is_none());
    let mut facts = inputs();
    facts.general_session_count = 1;
    facts.current_task_id = None;
    let general = Model::new(facts)
        .rows()
        .into_iter()
        .find(|r| r.task_id.is_none())
        .unwrap();
    assert_eq!(general.detail, "1 workspace session");
    assert_eq!(general.badge(), "CURRENT");
}

#[test]
fn attention_then_fuzzy_score_then_current_then_mru_then_case_insensitive_name() {
    let mut facts = inputs();
    facts.context_mru.clear();
    facts.tasks = [
        ("z", "z task", false),
        ("b", "Beta task", false),
        ("a", "alpha task", false),
        ("current", "task", false),
        ("attention", "t a s k", true),
    ]
    .into_iter()
    .map(|(id, label, needs_attention)| TaskRow {
        task_id: id.into(),
        label: label.into(),
        detail: "detail".into(),
        needs_attention,
    })
    .collect();
    let mut model = Model::new(facts);
    model.open();
    model.set_query(" task ".into());
    assert_eq!(
        model
            .rows()
            .iter()
            .map(|r| r.label.as_str())
            .collect::<Vec<_>>(),
        ["t a s k", "task", "alpha task", "Beta task", "z task"]
    );
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("attention"));
    assert_eq!(fuzzy_score("TASK", " task "), Some(100));
    assert_eq!(fuzzy_score("Task one", "task"), Some(80));
    assert_eq!(fuzzy_score("A task", "TASK"), Some(60));
    assert_eq!(fuzzy_score("Two apples, some kiwi", "task"), Some(20));
    assert_eq!(fuzzy_score("task", "tsak"), None);
    assert_eq!(fuzzy_score("Éclair", "écl"), Some(80));
}

#[test]
fn filtering_matches_name_only_and_resets_manual_selection() {
    let mut model = opened();
    model.next();
    model.set_query(" TRU274 ".into());
    assert_eq!(model.rows().len(), 1);
    assert_eq!(highlighted(&model).label, "TRU-274  Alpha task");
    model.set_query("working".into()); // Detail is deliberately not searchable.
    assert!(model.rows().is_empty());
    assert_eq!(model.activate_first(), None);
    assert!(model.is_visible());
    model.next();
    model.previous();
    model.set_query("  ".into());
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("recent"));
    model.set_query("gEn".into());
    assert_eq!(highlighted(&model).marker(), "●");
}

#[test]
fn next_previous_clamp_without_wrap_as_in_iced() {
    let mut model = opened();
    model.previous();
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("attention"));
    model.previous();
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("attention"));
    for _ in 0..10 {
        model.next();
    }
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("current"));
    model.next();
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("current"));
    model.previous();
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("issue"));
}

#[test]
fn activate_first_uses_highlight_general_goes_back_and_close_reopen_reset() {
    let mut model = opened();
    assert_eq!(
        model.activate_first(),
        Some(Outcome::Activate {
            task_id: "recent".into()
        })
    );
    assert_eq!(model.activate_first(), None);
    model.open();
    model.next();
    assert_eq!(model.activate_first(), Some(Outcome::ContextBack));
    model.open();
    model.set_query("Current".into());
    assert_eq!(
        model.activate_first(),
        Some(Outcome::Activate {
            task_id: "current".into()
        })
    );
    model.open();
    model.set_query("General".into());
    assert_eq!(model.close(), Some(Outcome::Close));
    assert_eq!(model.query(), "");
    assert_eq!(model.close(), None);
    model.open();
    assert_eq!(highlighted(&model).task_id.as_deref(), Some("recent"));
    assert_eq!(model.activate(usize::MAX), None);
    assert!(model.is_visible());
}

#[test]
fn missing_mru_falls_back_to_top_and_zero_general_sessions_still_exists() {
    let facts = Inputs {
        context_mru: vec![Some("deleted".into())],
        ..Default::default()
    };
    let mut model = Model::new(facts);
    model.open();
    assert_eq!(highlighted(&model).detail, "0 workspace sessions");
    model.previous();
    model.next();
    assert_eq!(model.activate_first(), Some(Outcome::ContextBack));
}

type Events = Rc<RefCell<Vec<Outcome>>>;

fn setup(cx: &mut TestAppContext) -> (Entity<TaskSwitcher>, &mut VisualTestContext, Events) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(|cx| cx.bind_keys(app_menus::key_bindings()));
    let events = Events::default();
    let mut view = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let switcher = cx.new(|cx| TaskSwitcher::new(inputs(), window, cx));
        let sink = events.clone();
        cx.subscribe(&switcher, move |_, _, event: &Outcome, _| {
            sink.borrow_mut().push(event.clone())
        })
        .detach();
        switcher.update(cx, |this, cx| this.open(inputs(), window, cx));
        view = Some(switcher.clone());
        gpui_component::Root::new(switcher, window, cx)
    });
    draw(cx);
    (view.unwrap(), cx, events)
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn native_input_arrows_enter_and_reopen_emit_exactly_once(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(*events.borrow(), [Outcome::ContextBack]);
    view.update_in(cx, |view, window, cx| view.open(inputs(), window, cx));
    draw(cx);
    cx.simulate_input("TRU274");
    cx.run_until_parked();
    cx.read(|cx| assert_eq!(view.read(cx).model.rows().len(), 1));
    cx.simulate_keystrokes("up down enter");
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        [
            Outcome::ContextBack,
            Outcome::Activate {
                task_id: "issue".into()
            }
        ]
    );
}

#[gpui::test]
fn native_escape_cmd_t_backdrop_and_row_click(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    for key in ["escape", "cmd-t"] {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        cx.read(|cx| assert!(!view.read(cx).model.is_visible()));
        view.update_in(cx, |view, window, cx| view.open(inputs(), window, cx));
        draw(cx);
    }
    cx.simulate_click(gpui::point(px(1.), px(1.)), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        [Outcome::Close, Outcome::Close, Outcome::Close]
    );
    view.update_in(cx, |view, window, cx| view.open(inputs(), window, cx));
    draw(cx);
    let bounds = cx.debug_bounds("task-switcher-row-0").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        events.borrow().last(),
        Some(&Outcome::Activate {
            task_id: "attention".into()
        })
    );
    assert_eq!(events.borrow().len(), 4);
}

#[gpui::test]
fn app_menu_actions_and_bound_enter_use_the_same_selection(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    let background_actions = Rc::new(RefCell::new(0));
    cx.update(|_, cx| {
        let calls = background_actions.clone();
        cx.on_action(move |_: &app_menus::ToggleTheme, _| *calls.borrow_mut() += 1);
    });
    cx.simulate_keystrokes("cmd-shift-t");
    cx.run_until_parked();
    assert_eq!(*background_actions.borrow(), 0);
    assert!(events.borrow().is_empty());
    cx.update(|window, cx| window.dispatch_action(Box::new(app_menus::TaskSwitcherNext), cx));
    cx.run_until_parked();
    cx.read(|cx| assert_eq!(highlighted(&view.read(cx).model).task_id, None));
    cx.update(|window, cx| window.dispatch_action(Box::new(app_menus::TaskSwitcherPrevious), cx));
    cx.run_until_parked();
    cx.simulate_input("TRU274");
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("Recent");
    cx.run_until_parked();
    cx.read(|cx| assert_eq!(view.read(cx).model.query(), "Recent"));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        [Outcome::Activate {
            task_id: "recent".into()
        }]
    );
    view.update_in(cx, |view, window, cx| view.open(inputs(), window, cx));
    draw(cx);
    cx.update(|window, cx| window.dispatch_action(Box::new(app_menus::TaskSwitcherClose), cx));
    cx.run_until_parked();
    assert_eq!(events.borrow().last(), Some(&Outcome::Close));
}
