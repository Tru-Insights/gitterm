//! Installed H6 keymap and host routing on GPUI's headless platform.
use super::*;
use app_menus::*;
use gpui::{Action, TestAppContext, VisualTestContext};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn dispatch(action: impl Action, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(action), cx));
    draw(cx);
}
fn sample(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::apply_settings(&workspace_config::IcedSettings::default(), None, cx);
        install_app_menus(cx);
    });
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let shell =
            cx.new(|cx| Shell::new(Rc::new(data::SampleData), root.path(), window, cx).unwrap());
        Root::new(shell, window, cx)
    });
    let shell = view.read_with(cx, |root, _| {
        root.view().clone().downcast::<Shell>().unwrap()
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    cx.update(|w, cx| shell.focus_handle(cx).focus(w, cx));
    draw(cx);
    (root, shell, cx)
}

#[gpui::test]
fn installed_keymap_is_iced_table_and_old_bindings_are_absent(cx: &mut TestAppContext) {
    let (_root, _shell, cx) = sample(cx);
    cx.read(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let installed: Vec<_> = keymap
            .bindings()
            .filter(|b| b.action().name().starts_with("gitterm_iced::"))
            .collect();
        let expected = key_bindings();
        assert_eq!(installed.len(), iced_shortcuts().len());
        for (actual, expected) in installed.into_iter().zip(expected) {
            assert_eq!(actual.keystrokes(), expected.keystrokes());
            assert_eq!(actual.predicate(), expected.predicate());
            assert!(actual.action().partial_eq(expected.action()));
        }
        for binding in keymap.bindings() {
            let action = binding.action().name();
            assert!(
                ![
                    "SelectSession",
                    "SelectWorkspace",
                    "CloseWorkspace",
                    "ShowShortcuts"
                ]
                .iter()
                .any(|name| action.ends_with(name))
            );
            assert!(
                !action.starts_with("native_chat::"),
                "obsolete chat keymap: {action}"
            );
            if binding
                .keystrokes()
                .first()
                .is_some_and(|key| key.key() == "t" && key.modifiers().platform)
            {
                assert!(!action.ends_with("NewTerminal"));
            }
        }
    });
}

#[gpui::test]
fn registered_actions_are_reachable_and_missing_actions_are_unavailable(cx: &mut TestAppContext) {
    let (_root, _shell, cx) = sample(cx);
    dispatch(TabSelect { index: 1 }, cx);
    cx.update(|window, cx| {
        for binding in key_bindings() {
            let action = binding.action();
            let name = action.name().rsplit("::").next().unwrap();
            let available = window.is_action_available(action, cx)
                || window
                    .available_actions(cx)
                    .iter()
                    .any(|registered| registered.name() == action.name());
            assert_eq!(available, !MISSING_HOST_ACTIONS.contains(&name), "{name}");
        }
        for action in [
            Box::new(About) as Box<dyn Action>,
            Box::new(ShowAll),
            Box::new(Maximize),
        ] {
            assert!(
                window.is_action_available(action.as_ref(), cx)
                    || window
                        .available_actions(cx)
                        .iter()
                        .any(|registered| registered.name() == action.name())
            );
        }
        assert!(!window.is_action_available(
            &SetChatWidth {
                width: gitterm_core::settings::ChatWidth::ALL[0]
            },
            cx
        ));
    });
}

#[gpui::test]
fn indexed_shortcuts_sidebar_console_sheets_and_escape_reach_host(cx: &mut TestAppContext) {
    let (_root, shell, cx) = sample(cx);
    cx.simulate_keystrokes("ctrl-2");
    draw(cx);
    cx.read(|cx| assert_eq!(shell.read(cx).active_workspace(cx), Some(WorkspaceId(2))));
    cx.simulate_keystrokes("cmd-2");
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).active(cx).active_session,
            Some(SessionId(22))
        )
    });
    cx.simulate_keystrokes("cmd-b");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).active(cx).store.regions.sidebar_collapsed));
    cx.simulate_keystrokes("cmd-b");
    draw(cx);
    cx.read(|cx| assert!(!shell.read(cx).active(cx).store.regions.sidebar_collapsed));
    cx.simulate_keystrokes("cmd-j");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).active(cx).store.regions.bottom_opened));
    dispatch(OpenFolder, cx);
    cx.read(|cx| assert!(shell.read(cx).new_work_sheet.is_some()));
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).new_work_sheet.is_none()));
    cx.simulate_keystrokes("cmd-shift-w");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_some()));
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    let count = cx.read(|cx| shell.read(cx).active(cx).sessions.len());
    cx.simulate_keystrokes("cmd-t");
    draw(cx);
    cx.read(|cx| assert_eq!(shell.read(cx).active(cx).sessions.len(), count));
}

#[gpui::test]
fn fonts_adjust_live_terminals_and_persist_only_preview_regions(cx: &mut TestAppContext) {
    let (root, shell, cx) = h5_tests::fixture(cx);
    let iced = root.path().join("iced");
    let before =
        ["settings.json", "workspaces.json"].map(|name| std::fs::read(iced.join(name)).unwrap());
    cx.update(|window, cx| {
        theme::set_font_sizes(
            theme::FontSizes {
                ui: 13.,
                terminal: 14.,
            },
            cx,
        );
        shell.focus_handle(cx).focus(window, cx);
    });
    draw(cx);
    dispatch(IncreaseUiFont, cx);
    dispatch(IncreaseTerminalFont, cx);
    cx.read(|cx| {
        assert_eq!(
            theme::font_sizes(cx),
            theme::FontSizes {
                ui: 14.,
                terminal: 15.
            }
        )
    });
    dispatch(DecreaseUiFont, cx);
    dispatch(DecreaseTerminalFont, cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.new_terminal(window, cx)));
    draw(cx);
    dispatch(IncreaseTerminalFont, cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        let pane =
            &shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())];
        let pane::PaneContent::Terminal(view) = &pane.read(cx).content else {
            panic!("terminal")
        };
        assert_eq!(view.read(cx).config().font_size, px(15.));
    });
    let (path, writer) = cx.read(|cx| {
        let shell = shell.read(cx);
        (
            shell.active(cx).store.path.clone(),
            shell.owner.read(cx).writer().unwrap(),
        )
    });
    writer.close().unwrap().blocking_recv().unwrap().unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut restored = LayoutStore::new(path);
    restored.load_terminals().unwrap();
    let known = cx.read(|cx| shell.read(cx).active(cx).panes.keys().cloned().collect());
    restored.load(&known).unwrap();
    assert_eq!(
        restored.regions.fonts,
        Some(theme::FontSizes {
            ui: 13.,
            terminal: 15.
        })
    );
    assert_eq!(
        saved["regions"]["fonts"],
        serde_json::json!({"ui":13., "terminal":15.})
    );
    assert_eq!(
        before,
        ["settings.json", "workspaces.json"].map(|name| std::fs::read(iced.join(name)).unwrap())
    );
    assert!(!iced.join("layout.json").exists());
}

#[gpui::test]
fn terminal_find_routes_once_and_reserved_commands_do_not_enter_pty(cx: &mut TestAppContext) {
    let (_root, shell, cx) = sample(cx);
    dispatch(TabSelect { index: 1 }, cx);
    let terminal = cx.read(|cx| {
        let shell = shell.read(cx);
        let pane =
            &shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())];
        let pane::PaneContent::Terminal(view) = &pane.read(cx).content else {
            panic!("terminal")
        };
        view.clone()
    });
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    cx.read(|cx| assert!(terminal.read(cx).is_find_open()));
    cx.simulate_keystrokes("n e e d l e");
    draw(cx);
    cx.read(|cx| assert_eq!(terminal.read(cx).find_query(), "needle"));
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    cx.read(|cx| assert!(!terminal.read(cx).is_find_open()));
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(!terminal.read(cx).is_find_open()));
    dispatch(TabSelect { index: 0 }, cx);
    let before = cx.read(|cx| terminal.read(cx).backend().get_all_text());
    for key in [
        "cmd-t",
        "cmd-shift-g",
        "cmd-shift-l",
        "cmd-shift-p",
        "ctrl-`",
    ] {
        cx.simulate_keystrokes(key);
        draw(cx);
    }
    cx.read(|cx| assert_eq!(terminal.read(cx).backend().get_all_text(), before));
}

#[gpui::test]
fn native_chat_find_uses_canonical_adapter_without_local_binding(cx: &mut TestAppContext) {
    let (_root, shell, cx) = sample(cx);
    dispatch(TabSelect { index: 0 }, cx);
    let chat = cx.read(|cx| {
        let shell = shell.read(cx);
        let pane =
            &shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())];
        let pane::PaneContent::Chat(view) = &pane.read(cx).content else {
            panic!("chat")
        };
        view.clone()
    });
    // Main-shell focus must still find the visible chat, just like IcedMain.
    cx.update(|window, cx| shell.focus_handle(cx).focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    cx.read(|cx| assert!(!shell.read(cx).chat_find.is_empty()));
    cx.update(|window, cx| {
        assert!(window.is_action_available(&IncreaseTerminalFont, cx));
        assert!(window.is_action_available(&DecreaseTerminalFont, cx));
        assert!(window.is_action_available(&IncreaseChatFont, cx));
        assert!(window.is_action_available(&DecreaseChatFont, cx));
    });
    let (chat_size, terminal_size) = cx.read(|cx| {
        (
            theme::chat_appearance(cx).font_size,
            theme::font_sizes(cx).terminal,
        )
    });
    dispatch(IncreaseTerminalFont, cx);
    cx.read(|cx| {
        assert_eq!(
            theme::chat_appearance(cx).font_size,
            (chat_size + 1.).clamp(11., 22.)
        );
        assert_eq!(theme::font_sizes(cx).terminal, terminal_size);
    });
    dispatch(WorkspaceSelect { index: 1 }, cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).chat_find.len(),
            1,
            "hidden chat retains its own find"
        )
    });
    dispatch(WorkspaceSelect { index: 0 }, cx);
    dispatch(TabSelect { index: 0 }, cx);
    cx.update(|window, cx| chat.focus_handle(cx).focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).chat_find.is_empty()));
    dispatch(Copy, cx);
    dispatch(CopyMarkdown, cx);
}

#[gpui::test]
fn file_navigation_edit_and_escape_use_existing_file_open_route(cx: &mut TestAppContext) {
    use gitterm_core::status::{GitFileStatus, GitStatusSnapshot};
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        install_app_menus(cx);
    });
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let other = root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("other.txt"), "other").unwrap();
    std::fs::write(work.join("file.txt"), "first").unwrap();
    std::fs::write(work.join("second.md"), "# second").unwrap();
    // No status executor: these synthetic completion snapshots must not race a real scan.
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let mut model = WorkspaceStatus::default();
        model.add(work.clone(), Instant::now());
        model.add(other.clone(), Instant::now());
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &root.path().join("preview"),
            true,
            window,
            cx,
        )
        .unwrap()
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    let (owner, workspace, session) = cx.read(|cx| {
        let shell = shell.read(cx);
        (
            shell.owner.clone(),
            shell.active_workspace(cx).unwrap(),
            shell.active(cx).active_session,
        )
    });
    owner.update(cx, |owner, cx| {
        owner.dispatch(
            Intent::Completed(GitStatusSnapshot {
                tab_id: workspace.0 as usize,
                repo_path: work.clone(),
                repo_name: "fixture".into(),
                branch_name: "fixture".into(),
                is_git_repo: true,
                staged: vec![],
                unstaged: ["file.txt", "second.md"]
                    .map(|path| GitFileStatus {
                        path: path.into(),
                        status: "M".into(),
                        is_staged: false,
                    })
                    .to_vec(),
                untracked: vec![],
                error: None,
            }),
            Instant::now(),
            cx,
        );
        owner.open_changed_file(
            workspace,
            data::ChangedFile {
                path: "file.txt".into(),
                status: "M".into(),
                staged: false,
            },
            cx,
        );
    });
    draw(cx);
    for (selection, expected) in [
        (FileSelection::Next, "second.md"),
        (FileSelection::Next, "second.md"),
        (FileSelection::Previous, "file.txt"),
        (FileSelection::Previous, "file.txt"),
        (FileSelection::Last, "second.md"),
        (FileSelection::First, "file.txt"),
    ] {
        dispatch(FileSelectByIndex { selection }, cx);
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).selected_files.get(&workspace).unwrap().path,
                work.join(expected)
            )
        });
    }
    dispatch(WorkspaceSelect { index: 1 }, cx);
    let other_id = cx.read(|cx| shell.read(cx).active_workspace(cx).unwrap());
    owner.update(cx, |owner, cx| {
        owner.open_file(other_id, other.join("other.txt"), cx)
    });
    draw(cx);
    dispatch(WorkspaceSelect { index: 0 }, cx);
    dispatch(
        FileSelectByIndex {
            selection: FileSelection::Last,
        },
        cx,
    );
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).selected_files[&workspace].path,
            work.join("second.md")
        )
    });
    dispatch(
        FileSelectByIndex {
            selection: FileSelection::First,
        },
        cx,
    );
    dispatch(EditFile, cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).content.read(cx).selected,
            Some(content_pane_id(workspace, &work.join("file.txt")))
        )
    });
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| {
        assert!(shell.read(cx).content.read(cx).selected.is_none());
        assert_eq!(shell.read(cx).active(cx).active_session, session);
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.git_action(
                workspace,
                &pane::git_view::GitAction::EditFile(work.join("file.txt")),
                window,
                cx,
            );
        })
    });
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).selected.is_none()));
}

#[gpui::test]
fn terminal_secondary_click_has_no_popup_and_hidden_pty_receives_no_app_chords(
    cx: &mut TestAppContext,
) {
    let (_root, shell, cx) = sample(cx);
    dispatch(TabSelect { index: 1 }, cx);
    let terminal = cx.read(|cx| {
        let shell = shell.read(cx);
        let pane =
            &shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())];
        let pane::PaneContent::Terminal(view) = &pane.read(cx).content else {
            panic!("terminal")
        };
        view.clone()
    });
    let bounds = cx.debug_bounds("session-content").unwrap();
    cx.simulate_mouse_down(
        bounds.center(),
        gpui::MouseButton::Right,
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_up(
        bounds.center(),
        gpui::MouseButton::Right,
        gpui::Modifiers::none(),
    );
    draw(cx);
    cx.read(|cx| assert!(!gpui::base::GlobalState::is_in_deferred_context(cx)));
    dispatch(TabSelect { index: 0 }, cx);
    // Deliberately retain stale PTY focus after its pane has been hidden.
    cx.update(|window, cx| terminal.read(cx).focus_handle().clone().focus(window, cx));
    draw(cx);
    let before = cx.read(|cx| terminal.read(cx).backend().get_all_text());
    cx.simulate_keystrokes("cmd-t cmd-k cmd-shift-l cmd-shift-p alt-/");
    draw(cx);
    cx.read(|cx| assert_eq!(terminal.read(cx).backend().get_all_text(), before));
}
