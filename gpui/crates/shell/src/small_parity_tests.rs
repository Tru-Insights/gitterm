//! TRU-274 small parity rows (A12c/M09/V08, M06, U04) on GPUI's headless
//! platform: fixtures only, no network, no Iced writes, no agent launch.
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

#[gpui::test]
fn open_folder_shortcut_opens_the_folder_picker_not_the_workspace_sheet(cx: &mut TestAppContext) {
    let (_root, shell, cx) = h6_tests::sample(cx);
    let sessions = cx.read(|cx| shell.read(cx).active(cx).sessions.len());
    // A12c / M09: Iced's ⌥⇧T is `Event::OpenFolder`, the in-app folder picker.
    cx.simulate_keystrokes("alt-shift-t");
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.folder_picker().is_some());
        assert!(shell.new_work_sheet.is_none());
        assert_eq!(shell.active(cx).sessions.len(), sessions);
    });
    assert!(cx.debug_bounds("folder-picker-native").is_some());
}

#[gpui::test]
fn empty_content_open_folder_button_opens_the_same_folder_flow(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::apply_settings(&workspace_config::IcedSettings::default(), None, cx);
        install_app_menus(cx);
    });
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let home = root.path().to_owned();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(WorkspaceStatus::default(), None, cx));
        let mut shell = Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            root.path(),
            true,
            window,
            cx,
        )
        .unwrap();
        shell.set_local_home(&home);
        shell
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    let bounds = cx
        .debug_bounds("empty-open-folder")
        .expect("Open Folder button in the empty content area");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.folder_picker().is_some());
        assert!(shell.new_work_sheet.is_none());
        assert!(shell.workspaces.is_empty());
    });
}

#[gpui::test]
fn clear_terminal_targets_the_active_main_tab_only(cx: &mut TestAppContext) {
    let (_root, shell, cx) = h6_tests::sample(cx);
    // A chat is active: Iced's active tab has no terminal to write to.
    dispatch(TabSelect { index: 0 }, cx);
    cx.read(|cx| assert!(shell.read(cx).active_main_terminal(cx).is_none()));
    dispatch(TabSelect { index: 1 }, cx);
    let (terminal, active) = cx.read(|cx| {
        let shell = shell.read(cx);
        let state = shell.active(cx);
        let id = session_pane_id(state.active_session.unwrap());
        let pane::PaneContent::Terminal(view) = &state.panes[&id].read(cx).content else {
            panic!("terminal")
        };
        (view.clone(), shell.active_main_terminal(cx).unwrap())
    });
    assert_eq!(active.entity_id(), terminal.entity_id());
    let sessions = cx.read(|cx| shell.read(cx).active(cx).sessions.len());
    dispatch(ClearTerminal, cx);
    cx.read(|cx| assert_eq!(shell.read(cx).active(cx).sessions.len(), sessions));
}

#[gpui::test]
fn usage_refresh_reads_fresh_codex_limits_without_writing(cx: &mut TestAppContext) {
    let (root, shell, cx) = h6_tests::sample(cx);
    let day = root.path().join(".codex/sessions/2026/11/21");
    std::fs::create_dir_all(&day).unwrap();
    let line = serde_json::json!({
        "timestamp": "2026-11-21T10:00:00Z", "type": "event_msg",
        "payload": {"type": "token_count", "info": null, "rate_limits": {
            "limit_id": "codex",
            "primary": {"used_percent": 42.0, "window_minutes": 10080, "resets_at": 4_000_000_000i64},
            "secondary": null, "plan_type": "pro"}}
    });
    std::fs::write(day.join("rollout-fixture.jsonl"), format!("{line}\n")).unwrap();
    shell.update(cx, |shell, _| {
        shell.usage_home = Some(root.path().to_owned())
    });
    cx.read(|cx| {
        let held = &shell.read(cx).usage_input.plan_limits;
        assert!(
            held.get(gitterm_core::plan_limits::Provider::Codex)
                .is_none()
        );
    });
    shell.update(cx, |shell, cx| {
        shell.usage_intent(usage::UsageIntent::Refresh, cx)
    });
    draw(cx);
    cx.read(|cx| {
        let reading = shell
            .read(cx)
            .usage_input
            .plan_limits
            .get(gitterm_core::plan_limits::Provider::Codex)
            .cloned()
            .expect("fresh Codex reading");
        assert_eq!(reading.windows[0].used_percent, Some(42.0));
    });
    // Iced saves its plan-limits file after a read; GPUI keeps it in memory.
    assert!(!root.path().join("plan-limits.json").exists());
}
