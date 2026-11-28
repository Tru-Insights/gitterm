use super::*;
use gitterm_core::{desktop_store::DesktopStore, settings::DesktopSettings};
use gpui::{TestAppContext, VisualTestContext};

const UNKNOWN: &str = "\"future\" : { \"keep\" : [3, 2, 1] }";

struct FixtureData(std::path::PathBuf);
impl ShellData for FixtureData {
    fn workspaces(&self, cx: &App) -> Vec<Workspace> {
        data::SampleData.workspaces(cx)
    }
    fn sessions(&self, id: WorkspaceId, cx: &App) -> Vec<Session> {
        data::SampleData.sessions(id, cx)
    }
    fn status(&self, id: WorkspaceId, cx: &App) -> data::GitStatus {
        data::SampleData.status(id, cx)
    }
    fn files(&self, id: WorkspaceId, cx: &App) -> Vec<data::FileNode> {
        data::SampleData.files(id, cx)
    }
    fn terminal_cwd(&self, _: WorkspaceId, _: &App) -> Option<std::path::PathBuf> {
        Some(self.0.clone())
    }
}

fn fixture<'a>(
    cx: &'a mut TestAppContext,
    live: bool,
    settings: &DesktopSettings,
) -> (
    tempfile::TempDir,
    Option<DesktopStore>,
    Entity<Shell>,
    &'a mut VisualTestContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let v6 = root.path().join("v6");
    std::fs::create_dir(&v6).unwrap();
    let mut bytes = serde_json::to_string_pretty(settings).unwrap();
    let end = bytes.rfind('}').unwrap();
    bytes.insert_str(end, &format!(",\n  {UNKNOWN}\n"));
    std::fs::write(v6.join("settings.json"), bytes).unwrap();
    let desktop = live.then(|| DesktopStore::open(&v6).unwrap());
    let iced = workspace_config::load_iced_settings_from(&v6);
    cx.update(|cx| {
        if let Some(store) = &desktop {
            cx.set_global(workspace_config::LiveWorkspaces(store.clone()));
        }
        theme::apply_settings(&iced, None, cx);
        cx.bind_keys(app_menus::key_bindings());
    });
    let mut shell = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| build(root.path(), &iced, window, cx));
        shell = Some(view.clone());
        Root::new(view, window, cx)
    });
    let shell = shell.unwrap();
    shell.update_in(cx, |shell, window, cx| shell.focus.focus(window, cx));
    h2_tests::draw(cx);
    (root, desktop, shell, cx)
}

fn build(
    root: &Path,
    settings: &workspace_config::IcedSettings,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Shell {
    let model = WorkspaceStatus::sample(data::SampleData.workspaces(cx).into_iter().map(|w| w.id));
    let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
    Shell::with_owner_settings(
        Rc::new(FixtureData(root.to_path_buf())),
        owner,
        &root.join("layouts"),
        false,
        settings,
        window,
        cx,
    )
    .unwrap()
}

fn saved(root: &Path, store: &DesktopStore, expected: &DesktopSettings) {
    store.drain().wait().unwrap();
    let bytes = std::fs::read_to_string(root.join("v6/settings.json")).unwrap();
    assert!(bytes.contains(UNKNOWN), "unknown field bytes changed");
    let mut actual: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    actual.as_object_mut().unwrap().remove("future");
    // The same typed snapshot and writer Iced's handlers save.
    let iced_dir = root.join("iced-expected");
    gitterm_core::settings::save_to(&iced_dir, expected).unwrap();
    let iced: serde_json::Value =
        serde_json::from_slice(&std::fs::read(iced_dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(actual, iced);
}

#[gpui::test]
fn gap1_theme_menu_and_shortcut_save_iced_theme(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let mut expected = DesktopSettings::default();
    let (root, store, shell, cx) = fixture(cx, true, &expected);
    cx.update(|window, cx| window.dispatch_action(Box::new(app_menus::ToggleTheme), cx));
    h2_tests::draw(cx);
    expected.theme = "light".into();
    saved(root.path(), store.as_ref().unwrap(), &expected);
    cx.read(|cx| assert_eq!(Palette::current(cx), Palette::for_mode(false)));
    cx.simulate_keystrokes("cmd-shift-t");
    h2_tests::draw(cx);
    expected.theme = "dark".into();
    saved(root.path(), store.as_ref().unwrap(), &expected);
    cx.read(|cx| assert!(shell.read(cx).owner.read(cx).error.is_none()));
}

#[gpui::test]
fn gap2_fonts_and_chat_width_are_global_v6_preferences(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let mut expected = DesktopSettings {
        font_size: Some(18.),
        ..Default::default()
    };
    let (root, store, shell, cx) = fixture(cx, true, &expected);
    shell.update(cx, |shell, cx| {
        shell.resize_font(true, 1., cx);
        shell.resize_font(false, -1., cx);
        shell.resize_chat_font(1., cx);
        shell.set_chat_width(gitterm_core::settings::ChatWidth::Wide, cx);
        assert!(
            shell
                .workspaces
                .iter()
                .all(|state| state.store.regions.fonts.is_none()
                    && state.store.regions.chat_font_size.is_none()
                    && state.store.regions.chat_width.is_none())
        );
    });
    expected.font_size = None;
    expected.ui_font_size = 18.;
    expected.terminal_font_size = 17.;
    expected.chat_font_size = 16.;
    expected.chat_width = gitterm_core::settings::ChatWidth::Wide;
    saved(root.path(), store.as_ref().unwrap(), &expected);
    let iced = workspace_config::load_iced_settings_from(&root.path().join("v6"));
    cx.update(|window, cx| {
        theme::apply_settings(&iced, Some(window), cx);
        assert_eq!(
            theme::font_sizes(cx),
            theme::FontSizes {
                ui: 18.,
                terminal: 17.
            }
        );
        assert_eq!(theme::chat_appearance(cx).font_size, 16.);
        assert_eq!(
            theme::chat_width(cx),
            gitterm_core::settings::ChatWidth::Wide
        );
    });
}

#[gpui::test]
fn gap3_sidebar_divider_restart_uses_iced_settings_over_stale_layout(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let mut expected = DesktopSettings::default();
    let (root, store, shell, cx) = fixture(cx, true, &expected);
    let divider = cx.debug_bounds("sidebar-divider").unwrap().center();
    cx.simulate_mouse_down(divider, gpui::MouseButton::Left, gpui::Modifiers::none());
    let target = divider + gpui::point(px(115.), px(0.));
    cx.simulate_mouse_move(target, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_up(target, gpui::MouseButton::Left, gpui::Modifiers::none());
    shell.update(cx, |shell, cx| {
        for state in &mut shell.workspaces {
            state.store.regions.resize_sidebar(210.);
            state.store.regions.fonts = Some(theme::FontSizes {
                ui: 23.,
                terminal: 23.,
            });
            state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
        }
    });
    expected.sidebar_width = 475.;
    saved(root.path(), store.as_ref().unwrap(), &expected);
    let iced = workspace_config::load_iced_settings_from(&root.path().join("v6"));
    let restored = cx.update(|window, cx| cx.new(|cx| build(root.path(), &iced, window, cx)));
    cx.read(|cx| {
        assert_eq!(restored.read(cx).sidebar_width, 475.);
        assert!(
            restored
                .read(cx)
                .workspaces
                .iter()
                .all(|w| w.store.regions.sidebar_width == 475.)
        );
        assert_eq!(
            theme::font_sizes(cx).ui,
            13.,
            "stale layout fonts overrode V6"
        );
    });
}

#[gpui::test]
fn gap4_bottom_expansion_and_height_follow_iced_globally(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let mut expected = DesktopSettings::default();
    let (root, store, shell, cx) = fixture(cx, true, &expected);
    shell.update_in(cx, |shell, window, cx| {
        shell.update_regions(
            |r| {
                r.bottom_visible = false;
                r.bottom_height = 280.;
            },
            cx,
        );
        shell.select_workspace(1, window, cx);
        shell.refresh_bottom_panel(window, cx);
        assert!(!shell.bottom.read(cx).input.expanded);
        assert_eq!(shell.bottom.read(cx).input.height, 280.);
        assert!(
            shell
                .workspaces
                .iter()
                .all(|w| !w.store.regions.bottom_visible && w.store.regions.bottom_height == 280.)
        );
    });
    expected.console_expanded = false;
    expected.console_height = 280.;
    saved(root.path(), store.as_ref().unwrap(), &expected);
    let iced = workspace_config::load_iced_settings_from(&root.path().join("v6"));
    let restored = cx.update(|window, cx| cx.new(|cx| build(root.path(), &iced, window, cx)));
    cx.read(|cx| {
        assert!(
            restored
                .read(cx)
                .workspaces
                .iter()
                .all(|w| !w.store.regions.bottom_visible && w.store.regions.bottom_height == 280.)
        )
    });
}

#[gpui::test]
fn gap5_mirroring_menu_and_bottom_use_the_same_iced_setting(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let mut expected = DesktopSettings::default();
    let (root, store, shell, cx) = fixture(cx, true, &expected);
    cx.update(|window, cx| {
        window.dispatch_action(Box::new(app_menus::ToggleTerminalLogMirroring), cx)
    });
    h2_tests::draw(cx);
    expected.terminal_log_mirroring_enabled = true;
    saved(root.path(), store.as_ref().unwrap(), &expected);
    cx.read(|cx| assert!(shell.read(cx).bottom.read(cx).input.mirroring));
    shell.update_in(cx, |shell, window, cx| {
        shell.bottom_outcome(bottom_panel::Outcome::ToggleMirroring, window, cx)
    });
    expected.terminal_log_mirroring_enabled = false;
    saved(root.path(), store.as_ref().unwrap(), &expected);
    cx.read(|cx| assert!(!shell.read(cx).bottom.read(cx).input.mirroring));
}

#[gpui::test]
fn preview_view_preferences_never_write_v6_settings(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, _, shell, cx) = fixture(cx, false, &Default::default());
    let path = root.path().join("v6/settings.json");
    let original = std::fs::read(&path).unwrap();
    shell.update_in(cx, |shell, window, cx| {
        shell.toggle_view_theme(window, cx);
        shell.resize_font(true, 1., cx);
        shell.resize_font(false, 1., cx);
        shell.resize_chat_font(1., cx);
        shell.set_chat_width(gitterm_core::settings::ChatWidth::Full, cx);
        shell.update_regions(
            |r| {
                r.bottom_height = 250.;
                r.bottom_visible = false;
            },
            cx,
        );
        shell.sidebar_width = 420.;
        shell.resize = Some((false, 0., 360.));
        shell.finish_preference_resize(cx);
        shell.resize = None;
        shell.bottom_outcome(bottom_panel::Outcome::ToggleMirroring, window, cx);
        assert!(!shell.bottom.read(cx).input.mirroring);
        assert!(
            shell
                .workspaces
                .iter()
                .all(|w| w.store.regions.fonts.is_some())
        );
    });
    h2_tests::draw(cx);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[gpui::test]
fn gap6_terminal_search_targets_main_tab_with_bottom_terminal_focused(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (_root, _, shell, cx) = fixture(cx, false, &Default::default());
    let (main, bottom) = shell.update_in(cx, |shell, window, cx| {
        shell.select_session(1, window, cx);
        let main = shell.active_main_terminal(cx).unwrap();
        shell.local = true;
        let active = shell.active_workspace(cx);
        shell
            .workspaces
            .iter_mut()
            .find(|state| Some(state.workspace.id) == active)
            .unwrap()
            .store
            .terminals = Some(Vec::new());
        shell.new_terminal_at(DockPlacement::Bottom, None, window, cx);
        let state = shell.active(cx);
        let id = bottom_pane_ids(state, cx).into_iter().next().unwrap();
        let pane::PaneContent::Terminal(bottom) = &state.panes[&id].read(cx).content else {
            panic!("bottom terminal")
        };
        (main, bottom.clone())
    });
    // Use a shell without rc files in a temp directory; all text is fixture data.
    main.update(cx, |view, _| {
        view.backend_mut().handle(gitterm_terminal::Command::Write(
            b"printf 'gap6-match\\ngap6-match\\n'\n".to_vec(),
        ))
    });
    bottom.update_in(cx, |view, window, cx| view.focus_handle().focus(window, cx));
    h2_tests::draw(cx);
    cx.simulate_keystrokes("cmd-f");
    h2_tests::draw(cx);
    cx.read(|cx| {
        assert!(main.read(cx).is_find_open());
        assert!(!bottom.read(cx).is_find_open());
    });
    assert!(
        cx.debug_bounds("terminal-find").is_some(),
        "main terminal search was not rendered"
    );
    main.update(cx, |view, cx| view.set_find_query("gap6-match".into(), cx));
    h2_tests::draw(cx);
    cx.simulate_keystrokes("enter");
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(120));
        // The headless platform only redraws invalidated views while the
        // terminal service finishes its background search.
        main.update(cx, |_, cx| cx.notify());
        h2_tests::draw(cx);
        if cx.read(|cx| {
            main.read(cx)
                .backend()
                .renderable_content()
                .find_match
                .is_some()
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "fixture search did not finish");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let first = cx.read(|cx| {
        main.read(cx)
            .backend()
            .renderable_content()
            .find_match
            .as_ref()
            .unwrap()
            .start
    });
    bottom.update_in(cx, |view, window, cx| view.focus_handle().focus(window, cx));
    h2_tests::draw(cx);
    cx.simulate_keystrokes("cmd-g");
    let next = cx.read(|cx| {
        main.read(cx)
            .backend()
            .renderable_content()
            .find_match
            .as_ref()
            .unwrap()
            .start
    });
    assert_ne!(next, first);
    cx.update(|window, cx| window.dispatch_action(Box::new(SearchPrev), cx));
    cx.read(|cx| {
        assert_eq!(
            main.read(cx)
                .backend()
                .renderable_content()
                .find_match
                .as_ref()
                .unwrap()
                .start,
            first
        );
        assert!(!bottom.read(cx).is_find_open());
    });
    cx.simulate_keystrokes("cmd-f");
    cx.read(|cx| assert!(!main.read(cx).is_find_open()));
}

#[gpui::test]
fn gap4_restored_and_resized_height_uses_iced_clamps(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let expected = DesktopSettings {
        console_height: 950.,
        ..Default::default()
    };
    let (root, store, shell, cx) = fixture(cx, true, &expected);
    let path = root.path().join("v6/settings.json");
    let before = std::fs::read(&path).unwrap();
    shell.update(cx, |shell, cx| {
        let restored = bottom_panel::clamp_height(600., shell.bottom.read(cx).input.window_height);
        assert!(
            shell
                .workspaces
                .iter()
                .all(|state| state.store.regions.bottom_height == restored)
        );
        shell.refresh_global_geometry(400., cx);
        assert!(
            shell
                .workspaces
                .iter()
                .all(|state| state.store.regions.bottom_height == 260.)
        );
    });
    store.unwrap().drain().wait().unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "window resize alone saved settings"
    );
}

#[gpui::test]
fn gap3_preview_divider_restart_retains_latest_local_width_without_v6_writes(
    cx: &mut TestAppContext,
) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, _, shell, cx) = fixture(cx, false, &Default::default());
    let path = root.path().join("v6/settings.json");
    let before = std::fs::read(&path).unwrap();
    shell.update(cx, |shell, cx| {
        // Only the dragged workspace's layout is saved on release.
        let state = &mut shell.workspaces[1];
        state.store.regions.resize_sidebar(440.);
        state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
    });
    let iced = workspace_config::load_iced_settings_from(&root.path().join("v6"));
    let restored = cx.update(|window, cx| cx.new(|cx| build(root.path(), &iced, window, cx)));
    cx.read(|cx| {
        assert!(
            restored
                .read(cx)
                .workspaces
                .iter()
                .all(|state| state.store.regions.sidebar_width == 440.)
        )
    });
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[gpui::test]
fn gap3_sidebar_release_outside_window_saves_iced_width(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let mut expected = DesktopSettings::default();
    let (root, store, _shell, cx) = fixture(cx, true, &expected);
    let divider = cx.debug_bounds("sidebar-divider").unwrap().center();
    cx.simulate_mouse_down(divider, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_move(
        divider + gpui::point(px(40.), px(0.)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    let outside =
        cx.update(|window, _| gpui::point(window.viewport_size().width + px(20.), px(20.)));
    cx.simulate_mouse_up(outside, gpui::MouseButton::Left, gpui::Modifiers::none());
    expected.sidebar_width = 400.;
    saved(root.path(), store.as_ref().unwrap(), &expected);
}
