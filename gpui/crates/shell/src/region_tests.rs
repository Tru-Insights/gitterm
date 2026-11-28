//! Fixed Iced regions on GPUI's headless test platform.
use super::*;
use gpui::{TestAppContext, VisualTestContext};

fn empty_local(
    folder: &Path,
    config: &Path,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Shell {
    let mut model = WorkspaceStatus::default();
    model.add(folder.to_path_buf(), Instant::now());
    let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
    Shell::with_owner(
        Rc::new(data::LocalData(owner.clone())),
        owner,
        config,
        true,
        window,
        cx,
    )
    .unwrap()
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn region_order_sizes_and_explicit_bottom_terminal(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| empty_local(&folder, config.path(), window, cx));
    cx.simulate_resize(size(px(layout::WINDOW_WIDTH), px(layout::WINDOW_HEIGHT)));
    draw(cx);
    let spine = cx.debug_bounds("pstrip-spine").unwrap();
    let strip = cx.debug_bounds("pstrip-session-strip").unwrap();
    let sidebar = cx.debug_bounds("sidebar").unwrap();
    let content = cx.debug_bounds("session-content").unwrap();
    let bar = cx.debug_bounds("pstrip-workspace-bar").unwrap();
    assert_eq!(spine.size, size(px(37.), px(800.)));
    assert_eq!(strip.origin.x, spine.right());
    assert_eq!(strip.size.height, px(37.));
    assert_eq!(sidebar.origin.y, strip.bottom());
    assert_eq!(sidebar.size.width, px(360.));
    assert_eq!(content.origin.x, sidebar.right() + px(4.));
    assert_eq!(bar.size.height, px(28.));
    assert_eq!(bar.bottom(), px(800.));
    assert_eq!(content.bottom(), bar.origin.y);
    assert!(cx.debug_bounds("standalone-bottom-panel").is_none());
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        assert!(state.sessions.is_empty());
        assert_eq!(state.panes.len(), 2, "only retained Git and Files entities");
        assert!(state.dock.read(cx).layout(DockPlacement::Right).is_none());
    });
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_bottom_terminal(window, cx)));
    draw(cx);
    let bottom = cx.debug_bounds("standalone-bottom-panel").unwrap();
    let divider = cx.debug_bounds("standalone-bottom-divider").unwrap();
    assert_eq!(bottom.size.height, px(200.));
    assert_eq!(divider.size.height, px(3.));
    assert_eq!(divider.bottom(), bottom.origin.y);
    assert_eq!(bottom.bottom(), bar.origin.y);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            assert!(
                shell.active(cx).sessions.is_empty(),
                "toggle opens an empty panel"
            );
            shell.bottom_outcome(
                bottom_panel::Outcome::AddTerminal { repo: None },
                window,
                cx,
            );
            shell.bottom_outcome(
                bottom_panel::Outcome::AddTerminal { repo: None },
                window,
                cx,
            );
        })
    });
    draw(cx);
    let (first_id, second_id) = cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        (
            session_pane_id(state.sessions[0].id),
            session_pane_id(state.sessions[1].id),
        )
    });
    let first_id = Box::leak(first_id.into_boxed_str());
    let second_id = Box::leak(second_id.into_boxed_str());
    assert!(cx.debug_bounds(second_id).is_some());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.bottom_outcome(
                bottom_panel::Outcome::SelectTab {
                    id: shell.active(cx).sessions[0].id.0 as usize,
                },
                window,
                cx,
            )
        })
    });
    draw(cx);
    assert!(cx.debug_bounds(first_id).is_some());
    assert!(cx.debug_bounds(second_id).is_none());
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_bottom_terminal(window, cx)));
    draw(cx);
    assert!(cx.debug_bounds("standalone-bottom-panel").is_none());
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).active(cx).sessions.len(),
            2,
            "toggle retains the explicit terminals"
        )
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_bottom_terminal(window, cx);
            let ids: Vec<_> = shell
                .active(cx)
                .sessions
                .iter()
                .map(|s| s.id.0 as usize)
                .collect();
            for id in ids {
                shell.bottom_outcome(bottom_panel::Outcome::CloseTab { id }, window, cx);
            }
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("standalone-bottom-panel").is_some());
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.active(cx).sessions.is_empty());
        assert!(shell.bottom.read(cx).input.tabs.is_empty());
        assert!(shell.bottom.read(cx).input.opened);
    });
}

#[gpui::test]
fn workspace_bar_lists_names_and_click_selects(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        Shell::new(Rc::new(data::SampleData), config.path(), window, cx).unwrap()
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell
                .read(cx)
                .bar_input(cx)
                .workspaces
                .iter()
                .enumerate()
                .map(|(i, w)| (
                    i,
                    w.id,
                    w.label(Some(w.id) == shell.read(cx).active_workspace(cx))
                ))
                .collect::<Vec<_>>(),
            vec![
                (0, WorkspaceId(1), "acme-api".into()),
                (1, WorkspaceId(2), "web-app".into()),
                (2, WorkspaceId(3), "design-system".into()),
            ]
        )
    });
    let chip = cx.debug_bounds("pstrip-workspace-2").unwrap();
    cx.simulate_click(chip.center(), gpui::Modifiers::none());
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.active_workspace(cx), Some(WorkspaceId(2)));
        assert_eq!(shell.bar_input(cx).workspaces[1].label(true), "web-app");
        assert_eq!(shell.bar_input(cx).workspaces[0].label(false), "acme-api");
    });
}

#[gpui::test]
fn sidebar_collapse_and_files_mode_keep_content_in_left_region(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| empty_local(&root, config.path(), window, cx));
    draw(cx);
    let divider = cx.debug_bounds("sidebar-divider").unwrap().center();
    cx.simulate_mouse_down(divider, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).resizing));
    cx.simulate_mouse_move(
        divider + gpui::point(px(900.), px(0.)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_up(
        divider + gpui::point(px(900.), px(0.)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.read(|cx| {
        assert_eq!(shell.read(cx).active(cx).store.regions.sidebar_width, 600.);
        assert!(!shell.read(cx).content.read(cx).resizing);
    });
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.update_regions(|r| r.sidebar_collapsed = true, cx)
        })
    });
    draw(cx);
    assert_eq!(cx.debug_bounds("sidebar").unwrap().size.width, px(36.));
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.update_regions(
                |r| {
                    r.sidebar_collapsed = false;
                    r.sidebar_mode = SidebarMode::Files;
                },
                cx,
            )
        })
    });
    draw(cx);
    assert_eq!(cx.debug_bounds("sidebar").unwrap().size.width, px(600.));
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        assert_eq!(state.panes.len(), 2);
        assert!(!state.dock.read(cx).has_dock(DockPlacement::Right));
    });
}

#[gpui::test]
fn legacy_split_renders_only_the_active_session(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| empty_local(&root, config.path(), window, cx));
    let (first, second) = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_terminal(window, cx);
            shell.new_terminal(window, cx);
            let state = shell.active(cx);
            let first = session_pane_id(state.sessions[0].id);
            let second = session_pane_id(state.sessions[1].id);
            state.dock.update(cx, |dock, cx| {
                dock.set_center(
                    DockLayout::h_split()
                        .child(
                            DockLayout::tabs()
                                .panel_view(panel_handle(state.panes[&first].clone()), cx),
                            None,
                        )
                        .child(
                            DockLayout::tabs()
                                .panel_view(panel_handle(state.panes[&second].clone()), cx),
                            None,
                        ),
                    window,
                    cx,
                )
            });
            (first, second)
        })
    });
    draw(cx);
    assert!(cx.debug_bounds(Box::leak(first.into_boxed_str())).is_none());
    assert!(
        cx.debug_bounds(Box::leak(second.into_boxed_str()))
            .is_some()
    );
}

/// Two sample workspaces whose own layout files say 492 / 200; Iced's global
/// width (fixture settings.json) must win for both.
fn sidebar_shell<'a>(
    iced: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Shell>, &'a mut VisualTestContext, tempfile::TempDir) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let config = tempfile::tempdir().unwrap();
    for (key, width) in [("1", 492.), ("2", 200.)] {
        let mut store = LayoutStore::new(layout::workspace_layout_path(config.path(), key));
        store.regions.sidebar_width = width;
        store
            .save(&gpui_component::dock::DockAreaState {
                version: Some(1),
                center: gpui_component::dock::PanelState {
                    panel_name: "StackPanel".into(),
                    children: vec![],
                    info: gpui_component::dock::PanelInfo::stack(vec![], gpui::Axis::Horizontal),
                },
                ..Default::default()
            })
            .unwrap();
    }
    // Live uses Iced's global width even when old GPUI layouts contain dividers.
    let desktop = gitterm_core::desktop_store::DesktopStore::open(iced).unwrap();
    cx.update(|cx| cx.set_global(workspace_config::LiveWorkspaces(desktop)));
    let settings = workspace_config::load_iced_settings_from(iced);
    let dir = config.path().to_path_buf();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let data: Rc<dyn ShellData> = Rc::new(data::SampleData);
        let model = WorkspaceStatus::sample(data.workspaces(cx).into_iter().map(|w| w.id));
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        Shell::with_owner_settings(data, owner, &dir, false, &settings, window, cx).unwrap()
    });
    (shell, cx, config)
}
fn sidebar_widths(shell: &Entity<Shell>, cx: &VisualTestContext) -> Vec<f32> {
    cx.read(|cx| {
        shell
            .read(cx)
            .workspaces
            .iter()
            .map(|w| w.store.regions.sidebar_width)
            .collect()
    })
}

#[gpui::test]
fn sidebar_width_comes_from_iced_settings_for_every_workspace(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let iced = tempfile::tempdir().unwrap();
    std::fs::write(
        iced.path().join("settings.json"),
        br#"{"sidebar_width": 310}"#,
    )
    .unwrap();
    let (shell, cx, _config) = sidebar_shell(iced.path(), cx);
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    assert_eq!(sidebar_widths(&shell, cx)[..2], [310., 310.]);
    assert_eq!(cx.debug_bounds("sidebar").unwrap().size.width, px(310.));
    // The second workspace renders the same width.
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            let id = shell.workspaces[1].workspace.id;
            shell.owner.update(cx, |owner, cx| {
                owner.dispatch(Intent::Activate(id), Instant::now(), cx)
            });
        })
    });
    draw(cx);
    assert_eq!(cx.debug_bounds("sidebar").unwrap().size.width, px(310.));

    // A Live drag resizes every workspace and persists Iced's shared preference.
    let mut expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(iced.path().join("settings.json")).unwrap()).unwrap();
    let divider = cx.debug_bounds("sidebar-divider").unwrap().center();
    cx.simulate_mouse_down(divider, gpui::MouseButton::Left, gpui::Modifiers::none());
    let target = divider + gpui::point(px(40.), px(0.));
    cx.simulate_mouse_move(target, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_up(target, gpui::MouseButton::Left, gpui::Modifiers::none());
    assert_eq!(sidebar_widths(&shell, cx)[..2], [350., 350.]);
    cx.read(|cx| {
        cx.global::<workspace_config::LiveWorkspaces>()
            .0
            .drain()
            .wait()
            .unwrap()
    });
    expected["sidebar_width"] = serde_json::json!(350.);
    expected["terminal_font_size"] = serde_json::json!(13.);
    expected["ui_font_size"] = serde_json::json!(16.);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(iced.path().join("settings.json")).unwrap()
        )
        .unwrap(),
        expected
    );
}

#[gpui::test]
fn sidebar_width_clamps_like_iced_and_defaults_when_settings_missing(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let iced = tempfile::tempdir().unwrap();
    std::fs::write(
        iced.path().join("settings.json"),
        br#"{"sidebar_width": 900}"#,
    )
    .unwrap();
    let (shell, cx, _config) = sidebar_shell(iced.path(), cx);
    assert_eq!(sidebar_widths(&shell, cx)[..2], [600., 600.]);
    let iced = tempfile::tempdir().unwrap();
    std::fs::write(
        iced.path().join("settings.json"),
        br#"{"sidebar_width": 20}"#,
    )
    .unwrap();
    let (shell, cx, _config) = sidebar_shell(iced.path(), cx);
    assert_eq!(sidebar_widths(&shell, cx)[..2], [150., 150.]);
    let missing = tempfile::tempdir().unwrap();
    let (shell, cx, _config) = sidebar_shell(missing.path(), cx);
    assert_eq!(sidebar_widths(&shell, cx)[..2], [360., 360.]);
}
