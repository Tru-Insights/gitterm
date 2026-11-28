// Included in live_writes: the child harness supplies synthetic HOME and V6 roots.
fn tru274_document(
    fixture: &Fixture,
    repo: &Path,
) -> gitterm_core::workspace_config::WorkspacesFile {
    let workspace = |name: &str, active: bool| {
        json!({
            "name":name,"abbrev":"FX","dir":repo,"color":"blue","active":active,
            "future_workspace":{"keep":true},"active_tab":0,"guidelines_off":true,
            "tabs":[{"dir":repo,"repo_dir":repo,"session_uid":format!("{name}-first"),"future_tab":7},
                    {"dir":repo,"repo_dir":repo,"session_uid":format!("{name}-second")}],
            "bottom_terminals":[{"dir":repo}]
        })
    };
    let mut file: gitterm_core::workspace_config::WorkspacesFile = serde_json::from_value(json!({"future_root":42,"active_workspace":0,
        "workspaces":[workspace("First", true), workspace("Closed", false), workspace("Second", true)]})).unwrap();
    file.normalize_repos();
    std::fs::write(
        fixture.iced.join("workspaces.json"),
        serde_json::to_vec_pretty(&file).unwrap(),
    )
    .unwrap();
    file
}

fn tru274_wait(shell: &Entity<crate::Shell>, cx: &mut TestAppContext) {
    let desktop = cx.read(|cx| {
        cx.global::<crate::workspace_config::LiveWorkspaces>()
            .0
            .clone()
    });
    desktop.drain().wait().unwrap();
    wait_until(cx, |cx| {
        !shell.read_with(cx, |s, cx| s.owner.read(cx).workspace_save_pending)
    });
    shell.read_with(cx, |s, cx| {
        assert!(
            s.owner.read(cx).error.is_none(),
            "{:?}",
            s.owner.read(cx).error
        )
    });
}

fn tru274_assert_iced_bytes(
    fixture: &Fixture,
    expected: &gitterm_core::workspace_config::WorkspacesFile,
) {
    // The shared records use Iced's field order and omission rules, checked
    // against its actual wrapper in settings_facade. No real path is consulted.
    assert_eq!(
        std::fs::read(fixture.iced.join("workspaces.json")).unwrap(),
        serde_json::to_vec_pretty(expected).unwrap()
    );
}

fn tru274_isolate_processes() {
    // These tests never resolve a locally installed harness or host shell rc.
    unsafe {
        std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        std::env::set_var("SHELL", "/bin/sh");
    }
}

#[gpui::test]
fn tru274_live_new_local_shell_cli_and_bottom_match_iced(cx: &mut TestAppContext) {
    if child("tru274_live_new_local_shell_cli_and_bottom_match_iced") {
        return;
    }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut expected = tru274_document(&fixture, &git.repo);
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    shell.update_in(cx, |shell, window, cx| shell.new_terminal(window, cx));
    tru274_wait(&shell, cx);
    let (new_id, uid) = shell.read_with(cx, |s, cx| {
        let id = s.active(cx).active_session.unwrap();
        let pane = s.active(cx).panes[&crate::session_pane_id(id)].read(cx);
        (id, pane.session.as_ref().unwrap().session_uid.clone())
    });
    // Iced launch_session_tab -> add_tab_with_command -> workspace_config.
    expected.workspaces[0].tabs.push(
        serde_json::from_value(json!({
            "dir":git.repo,"repo_dir":git.repo,"session_uid":uid
        }))
        .unwrap(),
    );
    expected.workspaces[0].active_tab = Some(2);
    tru274_assert_iced_bytes(&fixture, &expected);

    shell.update_in(cx, |shell, window, cx| {
        shell.new_terminal_in(crate::DockPlacement::Bottom, window, cx)
    });
    tru274_wait(&shell, cx);
    expected.workspaces[0].bottom_terminals.push(
        gitterm_core::workspace_config::BottomTerminalConfig {
            dir: git.repo.to_string_lossy().into_owned(),
        },
    );
    assert_eq!(
        shell.read_with(cx, |s, cx| s.active(cx).active_session),
        Some(new_id)
    );
    tru274_assert_iced_bytes(&fixture, &expected);
    let settings: gitterm_core::settings::DesktopSettings =
        serde_json::from_slice(&std::fs::read(fixture.iced.join("settings.json")).unwrap())
            .unwrap();
    assert!(settings.console_expanded);

    let before_cli = specs.borrow().len();
    shell.update_in(cx, |s, window, cx| {
        s.agent_presets = vec![gitterm_core::settings::AgentPreset {
            name: "Placeholder CLI".into(),
            command: "placeholder-cli --example".into(),
            resume_command: Some("placeholder-cli --resume".into()),
            icon: "terminal".into(),
            color: gitterm_core::settings::WorkspaceColor::Blue,
        }];
        s.launch_agent_preset(0, window, cx);
    });
    tru274_wait(&shell, cx);
    let uid = shell.read_with(cx, |s, cx| {
        let id = s.active(cx).active_session.unwrap();
        s.active(cx).panes[&crate::session_pane_id(id)]
            .read(cx)
            .session
            .as_ref()
            .unwrap()
            .session_uid
            .clone()
    });
    assert_eq!(
        specs.borrow().len(),
        before_cli + 1,
        "the CLI adopts exactly one host terminal"
    );
    expected.workspaces[0].tabs.push(serde_json::from_value(json!({
        "dir":git.repo,"repo_dir":git.repo,"session_uid":uid,"startup_command":"placeholder-cli --example"
    })).unwrap());
    expected.workspaces[0].active_tab = Some(3);
    tru274_assert_iced_bytes(&fixture, &expected);
    shell.read_with(cx, |s, cx| assert_eq!(s.active(cx).sessions.len(), 6));
    golive19_stop(&shell, cx);
    let (restored, cx) = golive19_shell(&fixture, &host, cx);
    restored.read_with(cx, |s, cx| {
        assert_eq!(
            s.active(cx).sessions.len(),
            6,
            "layout copies must not duplicate shared tabs"
        );
        let id = s.active(cx).active_session.unwrap();
        assert_eq!(
            s.active(cx).panes[&crate::session_pane_id(id)]
                .read(cx)
                .startup_command
                .as_deref(),
            Some("placeholder-cli --example")
        );
    });
    tru274_assert_iced_bytes(&fixture, &expected);
    golive19_stop(&restored, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_live_selection_matches_iced_and_restart_restores(cx: &mut TestAppContext) {
    if child("tru274_live_selection_matches_iced_and_restart_restores") {
        return;
    }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut expected = tru274_document(&fixture, &git.repo);
    let (host, _) = golive19_start(&fixture, cx);
    golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    shell.update_in(cx, |s, window, cx| {
        s.select_workspace_id(crate::WorkspaceId(2), window, cx)
    });
    tru274_wait(&shell, cx);
    expected.active_workspace = 1; // Iced selection Persist: open ordinal.
    tru274_assert_iced_bytes(&fixture, &expected);
    shell.update_in(cx, |s, window, cx| {
        let id = s.active(cx).sessions[1].id;
        s.select_session_id(id, window, cx);
        s.select_workspace_id(crate::WorkspaceId(1), window, cx);
        s.select_workspace_id(crate::WorkspaceId(2), window, cx);
        let first = s.active(cx).sessions[0].id;
        s.select_session_id(first, window, cx);
        s.select_session_id(id, window, cx);
    });
    tru274_wait(&shell, cx);
    expected.workspaces[2].active_tab = Some(1);
    tru274_assert_iced_bytes(&fixture, &expected);
    golive19_stop(&shell, cx);
    // Rebuild through the startup loader and a fresh owner/dock. Its only
    // selection input is V6; GPUI layout files contain no workspace selection.
    let (restored, cx) = golive19_shell(&fixture, &host, cx);
    restored.read_with(cx, |s, cx| {
        assert_eq!(s.active_workspace(cx), Some(crate::WorkspaceId(2)));
        let state = s.active(cx);
        assert_eq!(state.active_session, Some(state.sessions[1].id));
        assert!(crate::pane_id_visible(
            state,
            &crate::session_pane_id(state.sessions[1].id),
            cx
        ));
        assert_eq!(state.sessions.len(), 3);
    });
    tru274_assert_iced_bytes(&fixture, &expected);
    golive19_stop(&restored, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_preview_local_tabs_and_selection_never_write_v6(cx: &mut TestAppContext) {
    if child("tru274_preview_local_tabs_and_selection_never_write_v6") {
        return;
    }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let expected = tru274_document(&fixture, &git.repo);
    cx.update(gpui_component::init);
    let mut store = crate::preview::tests::guarded_store(fixture.roots.root());
    let model = crate::workspace_config::load_iced_workspaces_from(&mut store, &fixture.iced);
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        crate::Shell::with_owner(
            std::rc::Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            fixture.roots.root(),
            true,
            window,
            cx,
        )
        .unwrap()
    });
    shell.update_in(cx, |s, window, cx| {
        s.select_workspace_id(crate::WorkspaceId(2), window, cx);
        let id = s.active(cx).sessions[1].id;
        s.select_session_id(id, window, cx);
        s.new_terminal(window, cx);
        s.new_terminal_in(crate::DockPlacement::Bottom, window, cx);
        s.launch_agent_preset(0, window, cx);
    });
    cx.run_until_parked();
    tru274_assert_iced_bytes(&fixture, &expected);
    assert!(!fixture.iced.join("settings.json").exists());
    golive19_stop(&shell, cx);
}

#[gpui::test]
fn tru274_live_empty_inventory_keeps_iced_default_shell(cx: &mut TestAppContext) {
    if child("tru274_live_empty_inventory_keeps_iced_default_shell") {
        return;
    }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut expected = tru274_document(&fixture, &git.repo);
    expected.workspaces.truncate(1);
    expected.workspaces[0].tabs.clear();
    expected.workspaces[0].bottom_terminals.clear();
    std::fs::write(
        fixture.iced.join("workspaces.json"),
        serde_json::to_vec_pretty(&expected).unwrap(),
    )
    .unwrap();
    let (host, _) = golive19_start(&fixture, cx);
    golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    let uid = shell.read_with(cx, |s, cx| {
        let state = s.active(cx);
        state.panes[&crate::session_pane_id(state.sessions[0].id)]
            .read(cx)
            .session
            .as_ref()
            .unwrap()
            .session_uid
            .clone()
    });
    // Restore itself is read-only, even when it creates Iced's default shell.
    tru274_assert_iced_bytes(&fixture, &expected);
    shell.update_in(cx, |s, window, cx| {
        s.new_terminal_in(crate::DockPlacement::Bottom, window, cx)
    });
    tru274_wait(&shell, cx);
    expected.workspaces[0].tabs.push(
        serde_json::from_value(json!({"dir":git.repo,"repo_dir":git.repo,"session_uid":uid}))
            .unwrap(),
    );
    expected.workspaces[0].bottom_terminals.push(
        gitterm_core::workspace_config::BottomTerminalConfig {
            dir: git.repo.to_string_lossy().into_owned(),
        },
    );
    tru274_assert_iced_bytes(&fixture, &expected);
    golive19_stop(&shell, cx);
    host.update(cx, |host, cx| host.quit(cx));
}
