use gpui::{App, Window, div, prelude::*, px};
use gpui_component::{
    WindowExt,
    button::{Button, ButtonVariants},
    dialog::DialogEntrance,
};

// Keep this inventory in Iced's view_help_modal order, including its wording.
const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("Ctrl + 1-9", "Switch workspace"),
            ("Cmd + 1-9", "Switch session"),
            ("Cmd + T", "Switch General/task context"),
            ("Cmd + ↑", "Return to General"),
            ("Cmd + 0", "Open task overview"),
            ("Cmd + Shift + U", "Open usage view"),
            ("Cmd + Shift + O", "Find: the chats overview"),
            ("Ctrl + `", "Jump to next needs-you"),
            ("Cmd + Shift + W", "Close workspace"),
            ("Cmd + B", "Toggle sidebar"),
        ],
    ),
    (
        "Sessions",
        &[
            ("+ Session", "Choose an agent or terminal"),
            ("Option + Shift + C", "Resume Claude session"),
            ("Option + Shift + X", "New Codex session"),
            ("Option + Shift + T", "New terminal (folder)"),
        ],
    ),
    (
        "Console",
        &[
            ("Cmd + J", "Toggle bottom panel"),
            ("Cmd + Shift + R", "Run the home repo's command"),
        ],
    ),
    (
        "Terminal",
        &[
            ("Cmd + K", "Clear terminal"),
            ("Cmd + F", "Find in terminal"),
            ("Cmd + G", "Next match"),
            ("Cmd + Shift + G", "Previous match"),
        ],
    ),
    (
        "Font Size",
        &[
            ("Cmd + =", "Increase terminal font"),
            ("Cmd + -", "Decrease terminal font"),
            ("Cmd + Shift + =", "Increase UI font"),
            ("Cmd + Shift + -", "Decrease UI font"),
        ],
    ),
    ("Theme", &[("Cmd + Shift + T", "Toggle light/dark")]),
];

fn close_key(event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut App) {
    if event.keystroke.key == "escape"
        || (event.keystroke.modifiers.alt && matches!(event.keystroke.key.as_str(), "/" | "÷"))
    {
        window.close_dialog(cx);
        cx.stop_propagation();
    }
}

pub fn shortcuts(window: &mut Window, cx: &mut App) {
    let focus = cx.focus_handle();
    let content_focus = focus.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let mut inventory = div()
            .id("help-inventory")
            .debug_selector(|| "help-inventory".into())
            .flex()
            .flex_col()
            .gap(px(2.))
            .max_h(px(560.))
            .overflow_y_scroll();
        for (section, rows) in SHORTCUTS {
            inventory = inventory.child(div().pt(px(8.)).pb(px(4.)).child(*section));
            for (key, description) in *rows {
                inventory = inventory.child(
                    div()
                        .flex()
                        .gap(px(12.))
                        .child(div().w(px(180.)).flex_none().child(*key))
                        .child(*description),
                );
            }
        }
        dialog
            .entrance(DialogEntrance::None)
            .title("Keyboard Shortcuts")
            .w(px(460.))
            .centered()
            .close_button(false)
            .overlay_closable(true)
            .child(
                div()
                    .id("keyboard-shortcuts")
                    .debug_selector(|| "keyboard-shortcuts".into())
                    .track_focus(&content_focus)
                    .text_size(px(13.))
                    .on_key_down(close_key)
                    .child(inventory),
            )
            .footer(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .on_key_down(close_key)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .child("Press Option+/ or Esc to close"),
                    )
                    .child(
                        Button::new("help-close")
                            .debug_selector(|| "help-close".into())
                            .ghost()
                            .label("Close")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    });
    focus.focus(window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_glyphs_and_labels_match_iced_in_both_states() {
        assert_eq!(
            crate::Shell::SIDEBAR_MODES.map(|(_, title, glyph)| (title, glyph)),
            [("Work", "▦"), ("Files", "📁"), ("Find", "≋")]
        );
        assert_eq!(crate::Shell::sidebar_chevron(false), "◀");
        assert_eq!(crate::Shell::sidebar_chevron(true), "▶");
    }

    #[test]
    fn help_contains_the_full_iced_inventory_in_order() {
        let source = include_str!("../../../../src/main.rs");
        let source = source
            .split("fn view_help_modal")
            .nth(1)
            .unwrap()
            .split("fn view_new_work_modal")
            .next()
            .unwrap();
        let mut position = 0;
        assert_eq!(
            SHORTCUTS.iter().map(|(_, rows)| rows.len()).sum::<usize>(),
            25
        );
        for (section, rows) in SHORTCUTS {
            assert!(source.contains(&format!("section_header(\"{section}\")")));
            for (key, description) in *rows {
                let next = source[position..]
                    .find(&format!("\"{key}\""))
                    .unwrap_or_else(|| panic!("missing Iced key {key}"));
                position += next + key.len() + 2;
                assert!(source[position..].contains(&format!("\"{description}\"")));
            }
        }
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::{
        Shell,
        data::{self, ShellData, WorkspaceId},
    };
    use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext, point, size};
    use gpui_component::Root;
    use std::rc::Rc;

    // No terminal/chat sessions, runtime owner, source config or filesystem
    // executor. The menu tests can only exercise in-memory chrome.
    struct ChromeData {
        chat: bool,
    }
    impl ShellData for ChromeData {
        fn workspaces(&self, cx: &App) -> Vec<data::Workspace> {
            data::SampleData.workspaces(cx)
        }
        fn sessions(&self, workspace: WorkspaceId, _: &App) -> Vec<data::Session> {
            if self.chat {
                vec![data::Session {
                    id: data::SessionId(workspace.0 * 10 + 1),
                    title: "Fixture chat".into(),
                    kind: data::SessionKind::Chat,
                }]
            } else {
                vec![]
            }
        }
        fn status(&self, id: WorkspaceId, cx: &App) -> data::GitStatus {
            data::SampleData.status(id, cx)
        }
        fn files(&self, _: WorkspaceId, _: &App) -> Vec<data::FileNode> {
            vec![]
        }
    }
    fn sample(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
        mount(cx, false)
    }
    fn mount(
        cx: &mut TestAppContext,
        chat: bool,
    ) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
        cx.background_executor.allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::install_app_menus(cx);
        });
        let dir = tempfile::tempdir_in("/private/tmp").unwrap();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let shell = cx.new(|cx| {
                Shell::new(Rc::new(ChromeData { chat }), dir.path(), window, cx).unwrap()
            });
            Root::new(shell, window, cx)
        });
        let shell = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<Shell>().unwrap()
        });
        cx.simulate_resize(size(px(1400.), px(900.)));
        cx.update(|w, cx| shell.focus_handle(cx).focus(w, cx));
        draw(cx);
        (dir, shell, cx)
    }
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }
    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        draw(cx);
    }
    fn snapshot(path: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn visit(path: &std::path::Path, out: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, out);
                } else {
                    out.push((path.clone(), std::fs::read(path).unwrap()));
                }
            }
        }
        let mut out = vec![];
        visit(path, &mut out);
        out.sort();
        out
    }
    fn notice(shell: &Entity<Shell>, label: &str, cx: &mut VisualTestContext) {
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).owner.read(cx).error.as_deref(),
                Some(format!("{label} · not available in preview").as_str())
            )
        });
    }
    #[gpui::test]
    fn more_and_shortcut_open_help_and_every_dismissal_returns_to_shell(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (dir, shell, cx) = sample(cx);
        let before = snapshot(dir.path());
        click(cx, "pstrip-more");
        assert!(cx.debug_bounds("pstrip-more-list").is_some());
        click(cx, "pstrip-more-item-0");
        assert!(cx.debug_bounds("keyboard-shortcuts").is_some());
        click(cx, "help-close");
        assert!(cx.debug_bounds("keyboard-shortcuts").is_none());
        for key in ["escape", "alt-/"] {
            cx.simulate_keystrokes("alt-/");
            draw(cx);
            assert!(cx.debug_bounds("keyboard-shortcuts").is_some());
            if key == "alt-/" {
                cx.simulate_keystrokes("tab");
            }
            cx.simulate_keystrokes(key);
            draw(cx);
            assert!(
                cx.debug_bounds("keyboard-shortcuts").is_none(),
                "close by {key}"
            );
        }
        cx.simulate_keystrokes("alt-/");
        draw(cx);
        cx.simulate_click(point(px(100.), px(60.)), Modifiers::none());
        draw(cx);
        assert!(cx.debug_bounds("keyboard-shortcuts").is_none());
        assert_eq!(snapshot(dir.path()), before);
        cx.read(|cx| assert!(shell.read(cx).owner.read(cx).chats.is_empty()));
    }
    #[gpui::test]
    fn preview_controls_only_report_the_label_without_writes_or_sessions(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (dir, shell, cx) = sample(cx);
        shell.update(cx, |shell, cx| {
            let mut facts = crate::workspace_bar::SavedFacts {
                ready: 2,
                parked: 1,
                ..Default::default()
            };
            facts.repo_machines.push(crate::workspace_bar::Machine {
                key: "agent:fixture".into(),
                name: "Fixture".into(),
                status: Some(crate::workspace_bar::News {
                    label: "not connected".into(),
                    color: 0x9399b2,
                }),
                connected: false,
                serves_phones: true,
            });
            facts.remote_workspaces.push("Fixture:repo".into());
            shell.saved_facts.replace(facts);
            cx.notify();
        });
        draw(cx);
        let before = snapshot(dir.path());
        for (selector, label) in [
            ("pstrip-settings", "Workspace settings"),
            ("pstrip-browser", "Open browser"),
        ] {
            click(cx, selector);
            notice(&shell, label, cx);
        }
        // "+" and "Open a repo…" open the in-app folder picker (D24, read-only).
        for selector in ["pstrip-machine-add-0", "pstrip-repo-action-0-2"] {
            click(cx, "pstrip-repos");
            click(cx, selector);
            assert!(cx.debug_bounds("folder-picker").is_some(), "{selector}");
            click(cx, "folder-picker-cancel");
            assert!(cx.debug_bounds("folder-picker").is_none(), "{selector}");
        }
        for (selector, label) in [
            ("pstrip-repo-action-0-1", "Add a repo to acme-api…"),
            ("pstrip-repo-action-0-3", "Keep repos in this menu only"),
            ("pstrip-repo-action-1-1", "Reconnect…"),
            ("pstrip-remote-workspace-0", "+ Fixture:repo"),
        ] {
            click(cx, "pstrip-repos");
            click(cx, selector);
            notice(&shell, label, cx);
        }
        click(cx, "pstrip-more");
        click(cx, "pstrip-more-item-1");
        notice(&shell, "Open browser", cx);
        let dark = cx.read(|cx| gpui_component::Theme::global(cx).mode.is_dark());
        for (key, label) in [
            ("cmd-shift-t", "Toggle Light/Dark Theme"),
            ("cmd-shift-l", "Toggle Terminal Log Mirroring"),
        ] {
            cx.simulate_keystrokes(key);
            draw(cx);
            notice(&shell, label, cx);
        }
        assert_eq!(
            cx.read(|cx| gpui_component::Theme::global(cx).mode.is_dark()),
            dark
        );
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(shell.owner.read(cx).chats.is_empty());
            assert!(shell.workspaces.iter().all(|w| w.sessions.is_empty()
                && w.panes.values().all(|p| !matches!(
                    p.read(cx).content,
                    crate::pane::PaneContent::Terminal(_)
                        | crate::pane::PaneContent::PreviewChat(_)
                ))));
            assert!(
                shell.session_picker.is_none()
                    && shell.new_work_sheet.is_none()
                    && shell.park_sheet.is_none()
            );
            assert!(shell.repos_on_bar(cx));
        });
        assert_eq!(snapshot(dir.path()), before);
    }
    #[gpui::test]
    fn rail_expands_to_each_mode_and_collapse_control_moves_to_bottom(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_dir, shell, cx) = sample(cx);
        let header = cx.debug_bounds("Work").unwrap();
        let collapse = cx.debug_bounds("collapse-sidebar").unwrap();
        assert_eq!(header.top(), collapse.top());
        click(cx, "collapse-sidebar");
        let sidebar = cx.debug_bounds("sidebar").unwrap();
        let collapse = cx.debug_bounds("collapse-sidebar").unwrap();
        assert_eq!(collapse.bottom(), sidebar.bottom());
        for (label, mode) in [
            ("Files", crate::SidebarMode::Files),
            ("Find", crate::SidebarMode::Find),
            ("Work", crate::SidebarMode::Work),
        ] {
            click(cx, label);
            cx.read(|cx| {
                let regions = &shell.read(cx).active(cx).store.regions;
                assert_eq!(regions.sidebar_mode, mode);
                assert!(!regions.sidebar_collapsed);
            });
            click(cx, "collapse-sidebar");
        }
    }

    #[gpui::test]
    fn escape_closes_menus_and_help_before_find_preview_then_host_navigation(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_dir, shell, cx) = sample(cx);
        shell.update(cx, |shell, cx| {
            shell.workspaces[0].store.regions.sidebar_mode = crate::SidebarMode::Find;
            let owner = shell.find_owner(cx);
            shell
                .find_state
                .borrow_mut()
                .reduce(gitterm_core::find::state::Intent::Select {
                    owner,
                    chat: gitterm_core::chats::ChatRef::new(None, "fixture"),
                    path: "/private/tmp/nonexistent-i6-transcript".into(),
                    backend: gitterm_core::chats::ChatBackend::Claude,
                });
            cx.notify();
        });
        draw(cx);
        click(cx, "pstrip-more");
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.escape(&crate::app_menus::Escape, window, cx);
                assert_eq!(shell.bar_menu, None);
                assert!(
                    shell
                        .find_state
                        .borrow()
                        .selected(&shell.find_owner(cx))
                        .is_some()
                );
            })
        });
        draw(cx);
        assert!(cx.debug_bounds("pstrip-more-list").is_none());
        click(cx, "pstrip-repos");
        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.bar_menu, None);
            assert!(
                shell
                    .find_state
                    .borrow()
                    .selected(&shell.find_owner(cx))
                    .is_some()
            );
        });
        cx.simulate_keystrokes("alt-/");
        draw(cx);
        assert!(cx.debug_bounds("keyboard-shortcuts").is_some());
        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(
                shell
                    .find_state
                    .borrow()
                    .selected(&shell.find_owner(cx))
                    .is_some()
            );
        });
        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(
                shell
                    .find_state
                    .borrow()
                    .selected(&shell.find_owner(cx))
                    .is_none()
            );
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.plans_open = true;
                shell.usage_open = true;
                shell.escape(&crate::app_menus::Escape, window, cx);
                assert!(!shell.usage_open);
                assert!(shell.plans_open, "Usage precedes plans");
                shell.escape(&crate::app_menus::Escape, window, cx);
                assert!(!shell.plans_open);
            })
        });
    }

    #[gpui::test]
    fn page_local_review_and_find_receive_escape_before_host_quick_commands(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_dir, shell, cx) = mount(cx, true);
        let chat = cx.read(|cx| {
            let pane = &shell.read(cx).workspaces[0].panes["session-11"];
            let crate::pane::PaneContent::Chat(chat) = &pane.read(cx).content else {
                panic!("fixture chat")
            };
            chat.clone()
        });
        chat.update(cx, |chat, cx| {
            chat.set_backend(gitterm_core::chat::types::AgentBackend::Claude, cx)
        });
        shell.update(cx, |shell, cx| {
            shell.workspaces[0].store.regions.bottom_opened = true;
            shell.workspaces[0].store.regions.bottom_visible = true;
            shell.bottom.update(cx, |bottom, cx| {
                bottom.input.quick_commands = vec![gitterm_core::settings::QuickCommand {
                    name: "Fixture".into(),
                    command: "must never run".into(),
                }];
                cx.notify();
            });
            cx.notify();
        });
        draw(cx);
        let control = cx.read(|cx| {
            shell
                .read(cx)
                .bottom
                .read(cx)
                .input
                .controls(false)
                .iter()
                .position(|control| control.action == crate::bottom_panel::Action::QuickPicker)
                .unwrap()
        });
        let selector: &'static str =
            Box::leak(format!("bottom-control-{control}").into_boxed_str());
        click(cx, selector);
        assert!(cx.debug_bounds("bottom-quick-0").is_some());
        cx.update(|window, cx| chat.focus_handle(cx).focus(window, cx));
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        cx.read(|cx| assert!(!shell.read(cx).chat_find.is_empty()));
        click(cx, "composer-review");
        assert!(cx.debug_bounds("review-composer").is_some());
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(cx.debug_bounds("review-composer").is_none());
        assert!(
            cx.debug_bounds("bottom-quick-0").is_some(),
            "page review precedes host quick commands"
        );
        cx.read(|cx| {
            assert!(
                !shell.read(cx).chat_find.is_empty(),
                "review preserves the page find layer"
            )
        });
        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.read(|cx| assert!(shell.read(cx).chat_find.is_empty()));
        assert!(
            cx.debug_bounds("bottom-quick-0").is_some(),
            "page find precedes host quick commands"
        );
        cx.update(|window, cx| shell.focus_handle(cx).focus(window, cx));
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(cx.debug_bounds("bottom-quick-0").is_none());
    }
}

#[gpui::test]
fn tru274_sidebar_tabs_use_proportional_ui_font_at_ui_size(cx: &mut gpui::TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.update(gpui_component::init);
    cx.update(|cx| {
        crate::theme::set_font_sizes(
            crate::theme::FontSizes {
                ui: 17.,
                terminal: 14.,
            },
            cx,
        );
        for title in ["Work", "Files", "Find"] {
            let mut button = crate::sidebar_tab_button(title, title, cx);
            assert_eq!(
                button.style().text.font_family.as_deref(),
                Some(".SystemUIFont")
            );
            assert_eq!(button.style().text.font_size, Some(px(17.).into()));
            assert_eq!(button.style().text.text_align, Some(gpui::TextAlign::Left));
            let mut rail = crate::sidebar_tab_button(title, "▦", cx);
            assert_eq!(rail.style().text.text_align, Some(gpui::TextAlign::Center));
        }
    });
}
