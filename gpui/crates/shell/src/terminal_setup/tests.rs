use super::*;
use gpui::{AppContext, TestAppContext};

fn init(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(|cx| {
        crate::theme::apply_settings(&crate::workspace_config::IcedSettings::default(), None, cx)
    });
}

#[gpui::test]
fn terminal_palette_follows_dark_light_and_theme_changes(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    let view = cx.new(|cx| {
        TerminalView::new_deferred(
            gitterm_terminal::BackendSettings::default(),
            config(Placement::Main, cx),
            cx,
        )
    });
    cx.update(|cx| {
        for (dark, background, foreground) in [
            (true, 0x1e1e2e, 0xcdd6f4),
            (false, 0xeff1f5, 0x4c4f69),
            (true, 0x1e1e2e, 0xcdd6f4),
        ] {
            crate::theme::apply(dark, None, cx);
            crate::theme::sync_terminal_typography(&view, cx);
            let palette = &view.read(cx).config().colors;
            let rgb24 = |color: gpui::Hsla| {
                let rgba = color.to_rgb();
                ((rgba.r * 255.).round() as u32) << 16
                    | ((rgba.g * 255.).round() as u32) << 8
                    | (rgba.b * 255.).round() as u32
            };
            assert_eq!(rgb24(palette.background()), background);
            assert_eq!(rgb24(palette.foreground()), foreground);
            assert_eq!(*palette, config(Placement::Main, cx).colors);
        }
    });
}

#[gpui::test]
fn empty_start_uses_cwd_workspace_and_terminal_without_writing_iced(cx: &mut TestAppContext) {
    use crate::{
        data::ShellData,
        workspace_config::{IcedCatalog, WorkspaceStore},
    };
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    let fixture = tempfile::tempdir().unwrap();
    let cwd = fixture.path().join("example-project");
    std::fs::create_dir(&cwd).unwrap();
    for bytes in [
        None,
        Some(br#"{"workspaces":[],"active_workspace":0}"#.as_slice()),
    ] {
        let path = fixture.path().join("workspaces.json");
        if let Some(bytes) = bytes {
            std::fs::write(&path, bytes).unwrap();
        }
        let mut file: gitterm_core::workspace_config::WorkspacesFile = bytes
            .map(|b| serde_json::from_slice(b).unwrap())
            .unwrap_or_default();
        file.workspaces
            .push(gitterm_core::workspace_config::initial_workspace(&cwd));
        let catalog = IcedCatalog::from_document(fixture.path(), file);
        let model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
        assert_eq!(model.workspaces.len(), 1);
        let id = model.active().unwrap();
        let mut store = WorkspaceStore::new(fixture.path());
        store.iced = Some(catalog);
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        cx.read(|cx| {
            let data = crate::data::LocalData(owner.clone());
            assert_eq!(data.workspaces(cx)[0].name, "example-project");
            assert_eq!(data.terminal_cwd(id, cx), Some(cwd.clone()));
            let sessions = data.sessions(id, cx);
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].kind, crate::data::SessionKind::Terminal);
        });
        let handle = cx.add_window(|window, cx| {
            let mut shell = crate::Shell::with_owner(
                std::rc::Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                fixture.path(),
                true,
                window,
                cx,
            )
            .unwrap();
            start_initial_terminal(&mut shell, window, cx);
            shell
        });
        handle
            .update(cx, |shell, _, cx| {
                let state = shell.active(cx);
                assert_eq!(state.sessions.len(), 1);
                let pane = state.panes[&crate::session_pane_id(state.sessions[0].id)].read(cx);
                let crate::pane::PaneContent::Terminal(view) = &pane.content else {
                    panic!("initial cwd pane must be a live terminal");
                };
                assert!(view.read(cx).backend().child_pid().is_some());
                assert_eq!(
                    view.read(cx).backend().state(),
                    gitterm_terminal::SessionState::Running
                );
                assert_eq!(pane.terminal_cwd.as_ref(), Some(&cwd));
                assert!(pane.session.is_some());
            })
            .unwrap();
        match bytes {
            Some(bytes) => assert_eq!(std::fs::read(path).unwrap(), bytes),
            None => assert!(!path.exists()),
        }
    }
}

#[gpui::test]
fn terminal_scrollback_comes_from_read_only_v6_settings(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("settings.json");
    let bytes = br#"{"scrollback_lines":137}"#;
    std::fs::write(&path, bytes).unwrap();
    let settings = crate::workspace_config::load_iced_settings_from(root.path());
    cx.update(|cx| {
        crate::theme::apply_settings(&settings, None, cx);
        for placement in [Placement::Main, Placement::Bottom] {
            assert_eq!(config(placement, cx).scrollback, 137);
        }
    });
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[gpui::test]
fn terminal_inset_matches_iced_main_and_bottom(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    cx.read(|cx| {
        assert_eq!(config(Placement::Main, cx).padding, Edges::all(px(4.)));
        assert_eq!(
            config(Placement::Bottom, cx).padding,
            Edges {
                top: px(2.),
                bottom: px(2.),
                left: px(0.),
                right: px(0.)
            }
        );
    });
}

fn output_window<'a>(
    output: &str,
    cx: &'a mut TestAppContext,
) -> (gpui::Entity<TerminalView>, &'a mut gpui::VisualTestContext) {
    use gpui_component::Root;
    let launch_spec = TerminalLaunch {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("printf '{output}'; read fixture")],
        ..Default::default()
    };
    let mut terminal = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| launch(launch_spec, Placement::Main, cx));
        terminal = Some(view.clone());
        Root::new(view, window, cx)
    });
    (terminal.unwrap(), visual)
}

#[gpui::test]
fn terminal_url_opener_installed_and_called_on_cmd_click(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    // TestPlatform records URL opens; no browser or real platform is used.
    let (_, visual) = output_window("https://example.com/path", cx);
    for _ in 0..100 {
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        visual.simulate_click(
            gpui::point(px(24.), px(10.)),
            gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
        );
        if visual.opened_url().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        visual.opened_url().as_deref(),
        Some("https://example.com/path")
    );
}

#[gpui::test]
fn terminal_osc52_callback_stores_text_in_platform_clipboard(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    // Alacritty decodes OSC52; GPUI's TestPlatform supplies the fake clipboard.
    let (_, visual) = output_window("\\033]52;c;Zml4dHVyZSDinJM=\\007", cx);
    for _ in 0..100 {
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        if visual.read_from_clipboard().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        visual
            .read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("fixture ✓")
    );
}

#[gpui::test]
fn terminal_inset_uses_actual_main_and_bottom_dock_placement(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    init(cx);
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let handle = cx.add_window(|window, cx| {
        let mut model = crate::workspace_status::WorkspaceStatus::default();
        model.add(root.path().to_owned(), std::time::Instant::now());
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, None, cx));
        crate::Shell::with_owner(
            std::rc::Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            config.path(),
            true,
            window,
            cx,
        )
        .unwrap()
    });
    handle
        .update(cx, |shell, window, cx| {
            for placement in [crate::DockPlacement::Center, crate::DockPlacement::Bottom] {
                shell.new_terminal_launch(
                    placement,
                    Some(root.path().to_owned()),
                    Some(crate::PaneStart::Launch(TerminalLaunch {
                        program: "/bin/sh".into(),
                        args: vec!["-c".into(), "read fixture".into()],
                        cwd: Some(root.path().to_owned()),
                        ..Default::default()
                    })),
                    window,
                    cx,
                );
            }
            // Execute the host's render-time sync, including its actual dock lookup.
            use gpui::Render;
            drop(shell.render(window, cx));
            let state = shell.active(cx);
            let bottom = crate::bottom_pane_ids(state, cx);
            assert_eq!(bottom.len(), 1);
            let mut terminals = 0;
            for (id, pane) in &state.panes {
                if let crate::pane::PaneContent::Terminal(view) = &pane.read(cx).content {
                    terminals += 1;
                    let expected = if bottom.contains(id) {
                        Placement::Bottom
                    } else {
                        Placement::Main
                    };
                    assert_eq!(view.read(cx).config().padding, padding(expected));
                }
            }
            assert_eq!(terminals, 2);
        })
        .unwrap();
}
