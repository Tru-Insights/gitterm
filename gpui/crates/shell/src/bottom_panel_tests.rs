//! Headless GPUI only; no native GUI, config files or terminals are launched.
use super::*;
use crate::settings::FixtureSettings;
use gpui::{Entity, TestAppContext, VisualTestContext, size};
use gpui_component::Root;
use std::cell::{Cell, RefCell};

fn key(name: &str) -> RepoKey {
    RepoKey::Local(format!("/fixture/{name}").into())
}
fn input() -> Input {
    let mut input = Input::from_settings(&FixtureSettings::default(), 800.);
    input.opened = true;
    input.repos = [
        ("api", "npm run dev", RunState::Run),
        ("web", "cargo run", RunState::Focus),
        ("docs", "make serve", RunState::Restart),
    ]
    .into_iter()
    .map(|(name, command, run_state)| Repo {
        key: key(name),
        name: name.into(),
        command: Some(command.into()),
        run_state,
        url: (name == "web").then(|| "http://localhost:3000".into()),
    })
    .collect();
    input.tabs = vec![
        TerminalTab {
            id: 4,
            title: None,
            alive: true,
            run_repo: None,
        },
        TerminalTab {
            id: 7,
            title: Some("cargo run".into()),
            alive: true,
            run_repo: Some(key("web")),
        },
        TerminalTab {
            id: 9,
            title: Some("make serve".into()),
            alive: false,
            run_repo: Some(key("docs")),
        },
    ];
    input.active = Some(7);
    input.quick_commands = vec![QuickCommand {
        name: "Check".into(),
        command: "cargo check".into(),
    }];
    input
}

#[test]
fn fixture_controls_match_iced_labels_glyphs_and_order() {
    let input = input();
    let controls = input.controls(false);
    assert_eq!(
        controls
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        [
            "▼",
            "Terminal 1",
            "×",
            "cargo run",
            "×",
            "make serve",
            "×",
            "+",
            "▾",
            "⚡",
            "",
            "🔗 http://localhost:3000",
            "▶ npm run dev",
            "▶ cargo run",
            "▶ make serve",
            "Mirror:off",
        ]
    );
    assert_eq!(controls[1].glyph, Some(">_"));
    assert_eq!(controls[3].glyph, Some("▶"));
    assert_eq!(controls[5].glyph, Some("■"));
    assert_eq!(
        input.controls(true)[9..12]
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        ["+ api", "+ web", "+ docs"]
    );
    assert_eq!(
        controls[12].action,
        Action::Emit(Outcome::Run { repo: key("api") })
    );
    assert_eq!(
        controls[13].action,
        Action::Emit(Outcome::Focus { repo: key("web") })
    );
    assert_eq!(
        controls[14].action,
        Action::Emit(Outcome::Restart { repo: key("docs") })
    );
}

#[test]
fn saved_settings_explicit_visibility_and_iced_clamps() {
    let mut settings = FixtureSettings::default();
    let mut input = Input::from_settings(&settings, 800.);
    assert_eq!(input.height, 200.);
    assert!(input.expanded);
    assert!(
        !input.visible(),
        "saved expanded=true never manufactures a terminal"
    );
    input.opened = true;
    assert!(
        input.visible(),
        "explicitly toggled panel supports Iced's empty state"
    );
    input.expanded = false;
    assert!(!input.visible());
    input.opened = false;
    input.tabs = self::input().tabs;
    input.expanded = true;
    assert!(
        input.visible(),
        "restored explicit bottom terminals can show"
    );
    for (saved, window, expected) in [
        (1., 800., 32.),
        (999., 1000., 600.),
        (500., 400., 260.),
        (200., 100., 32.),
    ] {
        settings.console_height = saved;
        assert_eq!(Input::from_settings(&settings, window).height, expected);
    }
    settings.console_expanded = false;
    settings.quick_commands = self::input().quick_commands;
    settings.terminal_log_mirroring_enabled = true;
    let restored = Input::from_settings(&settings, 800.);
    assert!(!restored.expanded);
    assert!(restored.mirroring);
    assert_eq!(restored.quick_commands[0].command, "cargo check");
    assert_eq!(
        clamp_height(900., 1200.),
        900.,
        "drag has no restore-only 600px cap"
    );
    assert_eq!(clamp_height(900., 800.), 660.);
    assert_eq!(clamp_height(-10., 800.), 32.);
}

#[test]
fn optional_controls_and_selected_run_url_only() {
    let mut input = input();
    input.active = Some(4);
    input.quick_commands.clear();
    input.repos.truncate(1);
    input.repos[0].command = None;
    input.mirroring = true;
    input.repos.push(Repo {
        key: RepoKey::RemoteAgent {
            remote_id: "remote".into(),
            root: "/repo".into(),
        },
        name: "remote".into(),
        command: Some("must not run".into()),
        run_state: RunState::Run,
        url: None,
    });
    let controls = input.controls(true);
    assert!(!controls.iter().any(|c| matches!(
        c.action,
        Action::RepoPicker
            | Action::QuickPicker
            | Action::Emit(Outcome::Run { .. } | Outcome::OpenUrl { .. })
    )));
    assert_eq!(controls.last().unwrap().label, "Mirror:on");
}

struct Harness(Entity<BottomPanel>);
impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && this
                        .0
                        .update(cx, |panel, cx| panel.dismiss_quick_picker(cx))
                {
                    cx.stop_propagation();
                }
            }))
            .child(div().flex_1())
            .child(self.0.clone())
    }
}
type Outcomes = Rc<RefCell<Vec<Outcome>>>;
fn fixture(
    cx: &mut TestAppContext,
    input: Input,
) -> (Entity<BottomPanel>, &mut VisualTestContext, Outcomes) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let outcomes = Outcomes::default();
    let mut panel = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| BottomPanel::new(input));
        panel = Some(view.clone());
        let sink = outcomes.clone();
        cx.subscribe(&view, move |_, _, outcome: &Outcome, _| {
            sink.borrow_mut().push(outcome.clone())
        })
        .detach();
        let harness = cx.new(|_| Harness(view));
        Root::new(harness, window, cx)
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    (panel.unwrap(), cx, outcomes)
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}
fn click(selector: &'static str, cx: &mut VisualTestContext) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

#[gpui::test]
fn rendered_controls_emit_every_outcome_and_menus_dismiss(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, input());
    for (selector, expected) in [
        ("bottom-control-1", Outcome::SelectTab { id: 4 }),
        ("bottom-control-2", Outcome::CloseTab { id: 4 }),
        ("bottom-control-11", Outcome::OpenUrl { repo: key("web") }),
        ("bottom-control-12", Outcome::Run { repo: key("api") }),
        ("bottom-control-13", Outcome::Focus { repo: key("web") }),
        ("bottom-control-14", Outcome::Restart { repo: key("docs") }),
        ("bottom-control-15", Outcome::ToggleMirroring),
        ("bottom-control-7", Outcome::AddTerminal { repo: None }),
        ("bottom-control-0", Outcome::Toggle),
    ] {
        click(selector, cx);
        assert_eq!(outcomes.borrow().last(), Some(&expected));
    }
    click("bottom-control-8", cx);
    click("bottom-control-10", cx); // + web in the expanded repo picker
    assert_eq!(
        outcomes.borrow().last(),
        Some(&Outcome::AddTerminal {
            repo: Some(key("web"))
        })
    );
    cx.read(|cx| assert!(!view.read(cx).repo_picker_open));
    click("bottom-control-9", cx);
    click("bottom-quick-0", cx);
    assert_eq!(
        outcomes.borrow().last(),
        Some(&Outcome::QuickCommand { id: 0 })
    );
    assert!(cx.debug_bounds("bottom-quick-0").is_none());
    click("bottom-control-9", cx);
    let content = cx.debug_bounds("standalone-bottom-panel").unwrap();
    cx.simulate_click(
        content.bottom_right() - gpui::point(px(5.), px(5.)),
        gpui::Modifiers::none(),
    );
    draw(cx);
    assert!(cx.debug_bounds("bottom-quick-0").is_none());
}

#[gpui::test]
fn dimensions_slot_focus_and_hidden_panel(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, input());
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    view.update(cx, |view, cx| {
        view.set_content(
            Some(Rc::new(move |_, _| {
                count.set(count.get() + 1);
                div()
                    .id("fixture-native-slot")
                    .debug_selector(|| "fixture-native-slot".into())
                    .size_full()
                    .into_any_element()
            })),
            cx,
        )
    });
    for dark in [true, false] {
        cx.update(|window, cx| crate::theme::apply(dark, Some(window), cx));
        draw(cx);
        assert_eq!(
            cx.debug_bounds("standalone-bottom-panel")
                .unwrap()
                .size
                .height,
            px(200.)
        );
        assert_eq!(
            cx.debug_bounds("bottom-header").unwrap().size.height,
            px(32.)
        );
        let divider = cx.debug_bounds("standalone-bottom-divider").unwrap();
        assert_eq!(divider.size.height, px(3.));
        assert_eq!(
            divider.bottom(),
            cx.debug_bounds("standalone-bottom-panel").unwrap().top()
        );
        assert!(cx.debug_bounds("fixture-native-slot").is_some());
    }
    click("fixture-native-slot", cx);
    assert_eq!(
        outcomes.borrow().last(),
        Some(&Outcome::SelectTab { id: 7 })
    );
    view.update(cx, |view, cx| {
        let mut input = view.input.clone();
        input.expanded = false;
        view.set_input(input, cx);
    });
    draw(cx);
    let before = calls.get();
    draw(cx);
    assert_eq!(
        calls.get(),
        before,
        "hidden terminal slot is never rendered"
    );
    assert!(cx.debug_bounds("standalone-bottom-panel").is_none());
    assert!(cx.debug_bounds("standalone-bottom-divider").is_none());
    view.update(cx, |view, cx| {
        view.set_input(Input::from_settings(&FixtureSettings::default(), 800.), cx)
    });
    draw(cx);
    assert!(cx.debug_bounds("standalone-bottom-panel").is_none());
    view.update(cx, |view, cx| {
        view.input.opened = true;
        cx.notify();
    });
    draw(cx);
    assert!(
        cx.debug_bounds("standalone-bottom-panel").is_some(),
        "empty-capable after explicit toggle"
    );
    assert!(cx.debug_bounds("fixture-native-slot").is_none());
}

#[gpui::test]
fn divider_drag_emits_clamped_height_even_outside_the_panel(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, input());
    let start = cx
        .debug_bounds("standalone-bottom-divider")
        .unwrap()
        .center();
    cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::none());
    draw(cx);
    cx.simulate_mouse_move(
        gpui::point(start.x, px(50.)),
        MouseButton::Left,
        gpui::Modifiers::none(),
    );
    draw(cx);
    assert_eq!(
        outcomes.borrow().last(),
        Some(&Outcome::Resize { height: 660. })
    );
    cx.simulate_mouse_move(
        gpui::point(start.x, px(790.)),
        MouseButton::Left,
        gpui::Modifiers::none(),
    );
    draw(cx);
    assert_eq!(
        outcomes.borrow().last(),
        Some(&Outcome::Resize { height: 32. })
    );
    cx.simulate_mouse_up(
        gpui::point(start.x, px(50.)),
        MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.read(|cx| assert!(!view.read(cx).dragging));
    let before = outcomes.borrow().len();
    cx.simulate_mouse_move(start, MouseButton::Left, gpui::Modifiers::none());
    assert_eq!(outcomes.borrow().len(), before);
}

#[test]
fn chevron_follows_expansion_even_though_collapsed_header_is_hidden() {
    let mut input = input();
    for (expanded, glyph) in [(true, "▼"), (false, "▶")] {
        input.expanded = expanded;
        assert_eq!(input.controls(false)[0].label, glyph);
        assert_eq!(input.visible(), expanded);
    }
}

#[gpui::test]
fn tab_click_closes_repo_picker_without_launch_outcome(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, input());
    click("bottom-control-8", cx);
    cx.read(|cx| assert!(view.read(cx).repo_picker_open));
    click("bottom-control-1", cx);
    cx.read(|cx| assert!(!view.read(cx).repo_picker_open));
    assert_eq!(&*outcomes.borrow(), &[Outcome::SelectTab { id: 4 }]);
}

#[gpui::test]
fn escape_dismisses_quick_picker_before_focused_content_and_keeps_repo_picker(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, input());
    let received = Rc::new(Cell::new(0));
    let keys = received.clone();
    let focus = cx.update(|_, cx| cx.focus_handle());
    let slot_focus = focus.clone();
    view.update(cx, |view, cx| {
        view.set_content(
            Some(Rc::new(move |_, _| {
                let keys = keys.clone();
                div()
                    .size_full()
                    .track_focus(&slot_focus)
                    .on_key_down(move |_, _, _| keys.set(keys.get() + 1))
                    .into_any_element()
            })),
            cx,
        )
    });
    click("bottom-control-8", cx);
    // The inserted repo controls shift the quick picker button to index 12.
    click("bottom-control-12", cx);
    cx.update(|window, cx| focus.focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| {
        assert!(!view.read(cx).quick_picker_open);
        assert!(
            view.read(cx).repo_picker_open,
            "Iced leaves the repo picker open"
        );
    });
    assert_eq!(
        received.get(),
        0,
        "Escape must not reach terminal/find first"
    );
    assert!(
        outcomes.borrow().is_empty(),
        "dismissal cannot launch anything"
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(
        received.get(),
        1,
        "Escape propagates when quick picker is closed"
    );
}

#[gpui::test]
fn host_escape_routes_quick_picker_before_plans_and_search(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let dir = tempfile::tempdir().unwrap();
    let handle = cx.add_window(|window, cx| {
        crate::Shell::new(Rc::new(crate::data::SampleData), dir.path(), window, cx).unwrap()
    });
    handle
        .update(cx, |shell, window, cx| {
            // Cancel every deferred terminal before any window draw can launch it.
            let mut search = None;
            for workspace in &shell.workspaces {
                for pane in workspace.panes.values() {
                    if let crate::pane::PaneContent::Terminal(view) = &pane.read(cx).content {
                        let view = view.clone();
                        view.update(cx, |view, cx| {
                            view.backend_mut().cancel();
                            view.open_find(cx);
                        });
                        search = Some(view);
                    }
                }
            }
            let search = search.expect("fixture terminal");
            shell.plans_open = true;
            for raw in [true, false] {
                shell.bottom.update(cx, |bottom, _| {
                    bottom.input = input();
                    bottom.quick_picker_open = true;
                    bottom.repo_picker_open = true;
                });
                if raw {
                    shell.guard_app_shortcut(
                        &gpui::KeyDownEvent {
                            keystroke: gpui::Keystroke::parse("escape").unwrap(),
                            is_held: false,
                            prefer_character_input: false,
                        },
                        window,
                        cx,
                    );
                } else {
                    shell.escape(&crate::app_menus::Escape, window, cx);
                }
                assert!(shell.plans_open, "quick picker precedes plans dismissal");
                assert!(
                    search.read(cx).is_find_open(),
                    "quick picker precedes search dismissal"
                );
                assert!(!shell.bottom.read(cx).quick_picker_open);
                assert!(shell.bottom.read(cx).repo_picker_open);
            }
        })
        .unwrap();
}
