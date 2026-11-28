//! Whether a typed key reaches a terminal behind Iced's Park and New Work
//! sheets. Iced drops every PTY write while either is open (main.rs
//! `Event::TerminalEvent` and `Event::BottomTerminalEvent`).
use super::*;
use app_menus::*;
use gpui::{TestAppContext, VisualTestContext};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

fn terminal(
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> Entity<gitterm_terminal::TerminalView> {
    cx.read(|cx| {
        let shell = shell.read(cx);
        let pane =
            &shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())];
        let pane::PaneContent::Terminal(view) = &pane.read(cx).content else {
            panic!("terminal")
        };
        view.clone()
    })
}

fn screen(view: &Entity<gitterm_terminal::TerminalView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, _| {
        view.backend().get_all_text().unwrap_or_default()
    })
}

/// Runs the executor until `done`, or fails after 10 seconds.
fn wait_until(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        draw(cx);
        if done(cx) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn dispatch(action: impl gpui::Action, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(action), cx));
    draw(cx);
}

/// Everything Iced's PTY capture covers, and how to open and close it.
#[derive(Clone, Copy, Debug)]
enum Blocker {
    Park,
    NewWork,
    TaskSwitcher,
    TaskStopAndClose,
    TaskPublish,
    PrAction,
}

impl Blocker {
    const ALL: [Blocker; 6] = [
        Blocker::Park,
        Blocker::NewWork,
        Blocker::TaskSwitcher,
        Blocker::TaskStopAndClose,
        Blocker::TaskPublish,
        Blocker::PrAction,
    ];

    fn open(self, shell: &Entity<Shell>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| match self {
                Blocker::Park => s.open_park_sheet(window, cx),
                Blocker::NewWork => s.open_new_work_sheet(window, cx),
                Blocker::TaskSwitcher => s.open_task_switcher(window, cx),
                Blocker::TaskStopAndClose | Blocker::TaskPublish => {
                    let panel = s.ensure_task_panel(window, cx);
                    panel.update(cx, |panel, cx| {
                        panel.modal = Some(if matches!(self, Blocker::TaskPublish) {
                            task_panel::Modal::Publish {
                                task_id: "fixture".into(),
                                action: gitterm_core::task_commands::TaskPublish::PushBranch,
                                error: None,
                            }
                        } else {
                            task_panel::Modal::StopAndClose {
                                target: gitterm_core::session::Target {
                                    tab_id: 1,
                                    session_uid: "fixture".into(),
                                    task_id: Some("fixture".into()),
                                    task_session_id: None,
                                },
                                error: None,
                            }
                        });
                        cx.notify();
                    });
                }
                Blocker::PrAction => {
                    s.ensure_work_sidebars(window, cx);
                    for sidebar in s.work_sidebars.values() {
                        sidebar.update(cx, |sidebar, cx| {
                            sidebar.open_confirm(
                                gitterm_core::merge_plan::PrKey::new("owner/repo", 1),
                                gitterm_core::work::pr_actions::ActionKind::Merge,
                                window,
                                cx,
                            )
                        });
                    }
                }
            })
        });
        draw(cx);
    }

    fn close(self, shell: &Entity<Shell>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| match self {
                Blocker::Park => {
                    s.park_sheet = None;
                    s.park_subscription = None;
                    s.cover_sheet(false, window, cx);
                }
                Blocker::NewWork => {
                    s.new_work_sheet = None;
                    s.new_work_subscription = None;
                    s.cover_sheet(false, window, cx);
                }
                Blocker::TaskSwitcher => {
                    s.task_switcher_outcome(&task_switcher::Outcome::Close, window, cx)
                }
                Blocker::TaskStopAndClose | Blocker::TaskPublish => {
                    let panel = s.ensure_task_panel(window, cx);
                    panel.update(cx, |panel, cx| panel.dismiss(cx));
                }
                Blocker::PrAction => {
                    for sidebar in s.work_sidebars.values() {
                        sidebar.update(cx, |sidebar, cx| sidebar.confirm_close(cx));
                    }
                }
            })
        });
        draw(cx);
    }
}

fn focus_terminal(view: &Entity<gitterm_terminal::TerminalView>, cx: &mut VisualTestContext) {
    let handle = cx.read(|cx| view.read(cx).focus_handle().clone());
    cx.update(|window, cx| handle.focus(window, cx));
    draw(cx);
}

/// Iced drops every PTY write while one of its capture layers is open
/// (main.rs `Event::TerminalEvent`, `Event::BottomTerminalEvent`). Probe
/// result: with the terminal focused (its focus set in code, as when a
/// sheet's field loses it), typed keys reached the PTY behind the Park and
/// New Work sheets before the guard in `guard_app_shortcut`. A click on the
/// terminal behind either sheet does not focus it.
#[gpui::test]
fn no_key_reaches_a_terminal_behind_iceds_capture_layers(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    for blocker in Blocker::ALL {
        // Each capture-layer case owns a separate window and its PTYs.
        let _fixture_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        dispatch(TabSelect { index: 1 }, cx);
        let view = terminal(&shell, cx);
        blocker.open(&shell, cx);
        cx.read(|cx| assert!(shell.read(cx).pty_blocked(cx), "{blocker:?}"));
        focus_terminal(&view, cx);
        cx.simulate_keystrokes("q w e");
        draw(cx);
        blocker.close(&shell, cx);
        cx.read(|cx| assert!(!shell.read(cx).pty_blocked(cx), "{blocker:?} closed"));
        // The PTY handles input in order: once `klm` is echoed, anything
        // typed behind the surface would be on screen before it.
        focus_terminal(&view, cx);
        cx.simulate_keystrokes("k l m");
        wait_until(cx, |cx| screen(&view, cx).contains("klm"));
        assert!(
            !screen(&view, cx).contains("qwe"),
            "{blocker:?}: a key reached the PTY"
        );
    }
}

/// Iced catches only Escape for the Parked and Ready lists (and the
/// surfaces below), so they neither guard background commands nor drop PTY
/// writes. GPUI mounts the lists in the Park sheet's overlay; the PTY gate
/// must tell them from the Park sheet itself.
#[gpui::test]
fn escape_only_surfaces_neither_guard_commands_nor_block_the_terminal(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    type Open = fn(&mut Shell, &mut gpui::Window, &mut gpui::Context<Shell>);
    let surfaces: [(&str, Open); 5] = [
        ("parked list", |s, w, cx| s.open_parked_list(w, cx)),
        ("ready list", |s, w, cx| s.open_ready_list(w, cx)),
        ("bar menu", |s, _, _| {
            s.bar_menu = Some(workspace_bar::Menu::More)
        }),
        ("folder picker", |s, w, cx| {
            s.open_folder_picker(
                workspace_sheet::FolderPickerPurpose::OpenFolder,
                None,
                w,
                cx,
            )
        }),
        ("connect form", |s, w, cx| s.open_connect_form(w, cx)),
    ];
    for (name, open) in surfaces {
        let _fixture_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        dispatch(TabSelect { index: 1 }, cx);
        let view = terminal(&shell, cx);
        cx.update(|window, cx| shell.update(cx, |s, cx| open(s, window, cx)));
        draw(cx);
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                assert_eq!(s.shell_modal(window, cx), None, "{name}");
                assert!(!s.pty_blocked(cx), "{name}");
                assert!(s.menu_context(window, cx).contains("IcedMain"), "{name}");
            })
        });
        focus_terminal(&view, cx);
        cx.simulate_keystrokes("k l m");
        wait_until(cx, |cx| screen(&view, cx).contains("klm"));
    }
}

/// Every task prompt Iced consumes keys for (new-task form, tab close,
/// worktree delete, publish) is a `task_panel::Modal`, which is `Modal::Task`.
#[gpui::test]
fn task_prompts_are_the_task_modal(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    for blocker in [Blocker::TaskStopAndClose, Blocker::TaskPublish] {
        let _fixture_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        blocker.open(&shell, cx);
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                assert_eq!(s.shell_modal(window, cx), Some(Modal::Task), "{blocker:?}")
            })
        });
    }
}

/// Iced closes the surface on Escape wherever focus is: its key handler
/// (main.rs `Event::KeyPressed`) runs before any terminal write is dropped.
/// With a terminal holding focus behind the surface, GPUI consumed the key
/// without closing anything.
#[gpui::test]
fn escape_closes_a_surface_while_a_terminal_holds_focus(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    for blocker in Blocker::ALL {
        let _fixture_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        dispatch(TabSelect { index: 1 }, cx);
        let view = terminal(&shell, cx);
        blocker.open(&shell, cx);
        focus_terminal(&view, cx);
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                assert!(
                    s.focused_terminal(window, cx).is_some(),
                    "{blocker:?}: focus"
                )
            })
        });
        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                assert!(!s.pty_blocked(cx), "{blocker:?}: Escape left it open");
                assert_eq!(s.shell_modal(window, cx), None, "{blocker:?}");
            })
        });
        // Nothing was typed behind it.
        focus_terminal(&view, cx);
        cx.simulate_keystrokes("k l m");
        wait_until(cx, |cx| screen(&view, cx).contains("klm"));
    }
    for sheet in Sheet::ALL {
        let _fixture_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h7_tests::fixture(cx);
        dispatch(NewTerminal, cx);
        let view = terminal(&shell, cx);
        cx.update(|window, cx| match sheet {
            Sheet::Park => shell.update(cx, |s, cx| s.open_park_sheet(window, cx)),
            Sheet::NewWork => shell.update(cx, |s, cx| s.open_new_work_sheet(window, cx)),
            Sheet::Settings => shell.update(cx, |s, cx| {
                let id = s.active_workspace(cx).unwrap();
                s.open_settings_sheet(id, window, cx)
            }),
            Sheet::Phone => {
                shell.update(cx, |s, cx| s.open_phone_panel("agent:fixture", window, cx))
            }
            Sheet::Plans => shell.update(cx, |s, cx| s.toggle_plans(window, cx)),
        });
        draw(cx);
        assert_eq!(cx.read(|cx| shell.read(cx).open_sheet()), Some(sheet));
        focus_terminal(&view, cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert_eq!(
            cx.read(|cx| shell.read(cx).open_sheet()),
            None,
            "{sheet:?}: Escape left it open"
        );
    }
}

/// Iced's terminal ignores right-click (iced_term `handle_mouse_event` acts
/// on the left button and the wheel only), so it has no right-click Paste;
/// its paste is Cmd+V or Shift+Insert, which the PTY gate drops while a
/// capture layer is open. GPUI's right-click context menu offers Paste, and a
/// menu item is not a key the shell's guard sees, so the terminal's own paste
/// checks the gate the shell refreshes each frame.
#[gpui::test]
fn paste_is_dropped_behind_iceds_capture_layers(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    for blocker in Blocker::ALL {
        let _fixture_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        dispatch(TabSelect { index: 1 }, cx);
        let view = terminal(&shell, cx);
        let blocked = |cx: &mut VisualTestContext| cx.read(gitterm_terminal::InputGate::blocked);
        assert!(!blocked(cx), "{blocker:?}: gate open before");
        blocker.open(&shell, cx);
        assert!(blocked(cx), "{blocker:?}: gate closed behind it");
        assert!(
            !cx.read(|cx| view.read(cx).paste_unless_blocked(cx)),
            "{blocker:?}: a paste reached the PTY"
        );
        blocker.close(&shell, cx);
        assert!(!blocked(cx), "{blocker:?}: gate open after");
    }
}
