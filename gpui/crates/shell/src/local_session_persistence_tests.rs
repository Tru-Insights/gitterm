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
            extra: Default::default(),
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
            extra: Default::default(),
            dir: git.repo.to_string_lossy().into_owned(),
        },
    );
    tru274_assert_iced_bytes(&fixture, &expected);
    golive19_stop(&shell, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_f1_open_unmounted_worktree_reuses_persisted_identity(cx: &mut TestAppContext) {
    if child("tru274_f1_open_unmounted_worktree_reuses_persisted_identity") { return; }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut iced = tru274_document(&fixture, &git.repo);
    super::git(&git.repo, &["worktree", "add", "-q", "-b", "placeholder-worktree", git.worktree.to_str().unwrap()]);
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    let before = specs.borrow().len();
    let sub = git.repo.join("placeholder-subdir");
    std::fs::create_dir(&sub).unwrap();
    iced.workspaces[0].tabs[0].dir = sub.to_string_lossy().into_owned();
    shell.update_in(cx, |s, window, cx| {
        let workspace = s.active(cx).workspace.id;
        let id = s.active(cx).active_session.unwrap();
        let pane = s.active(cx).panes[&crate::session_pane_id(id)].clone();
        pane.update(cx, |pane, cx| {
            pane.terminal_cwd = Some(sub.clone());
            pane.terminal_saved_dir = None;
            pane.git = Some(cx.new(|cx| crate::pane::git_view::GitView::new(workspace, s.data.clone(), cx)));
        });
        s.live_worktree_action(workspace, &crate::pane::git_view::GitAction::OpenWorktree(git.worktree.clone()), window, cx);
    });
    wait_until(cx, |cx| shell.read_with(cx, |s, cx| {
        s.active(cx).active_session.and_then(|id| s.active(cx).panes.get(&crate::session_pane_id(id)))
            .and_then(|pane| pane.read(cx).task_folder(cx)).as_deref() == Some(git.worktree.as_path())
    }));
    tru274_wait(&shell, cx);
    let uid = shell.read_with(cx, |s, cx| {
        let id = s.active(cx).active_session.unwrap();
        s.active(cx).panes[&crate::session_pane_id(id)].read(cx).session.as_ref().unwrap().session_uid.clone()
    });
    // Iced WorktreeOpen -> add_tab_with_command -> workspace_config: one tab.
    iced.workspaces[0].tabs.push(serde_json::from_value(json!({"dir":git.worktree,"repo_dir":git.worktree,"session_uid":uid})).unwrap());
    iced.workspaces[0].active_tab = Some(2);
    tru274_assert_iced_bytes(&fixture, &iced);
    assert_eq!(specs.borrow().len(), before + 1);
    golive19_stop(&shell, cx);
    let (restored, cx) = golive19_shell(&fixture, &host, cx);
    restored.read_with(cx, |s, cx| assert_eq!(s.active(cx).sessions.len(), 4));
    tru274_assert_iced_bytes(&fixture, &iced);
    golive19_stop(&restored, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_f2_close_bottom_removes_descriptor_and_does_not_restore(cx: &mut TestAppContext) {
    if child("tru274_f2_close_bottom_removes_descriptor_and_does_not_restore") { return; }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut iced = tru274_document(&fixture, &git.repo);
    iced.workspaces[0].bottom_terminals[0].extra.insert("future_bottom".into(), json!("original"));
    std::fs::write(fixture.iced.join("workspaces.json"), serde_json::to_vec_pretty(&iced).unwrap()).unwrap();
    let (host, _) = golive19_start(&fixture, cx);
    golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    shell.update_in(cx, |s, window, cx| s.new_terminal_in(crate::DockPlacement::Bottom, window, cx));
    tru274_wait(&shell, cx);
    // Close the just-added shell, then the original restored shell. Iced
    // BottomTerminalClose removes the descriptor at exactly that ordinal.
    for _ in 0..2 {
        shell.update_in(cx, |s, window, cx| {
            let state = s.active(cx);
            let terminal = state.store.terminals.as_ref().unwrap().iter().rev().find(|t| t.bottom).unwrap();
            let target = state.panes[&crate::session_pane_id(crate::SessionId(terminal.id))].read(cx).session.clone().unwrap();
            s.close_target_with(&target, true, window, cx);
        });
        tru274_wait(&shell, cx);
        tru274_assert_iced_bytes(&fixture, &iced);
        iced.workspaces[0].bottom_terminals.clear();
    }
    // Orderly restart waits for the independent GPUI layout queue too.
    let layout = shell.read_with(cx, |s, cx| s.active(cx).store.path.clone());
    wait_until(cx, |_| {
        std::fs::read(&layout).ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| value["terminal_sessions"].as_array().is_some_and(Vec::is_empty))
    });
    golive19_stop(&shell, cx);
    let (restored, cx) = golive19_shell(&fixture, &host, cx);
    restored.read_with(cx, |s, cx| assert!(crate::bottom_pane_ids(s.active(cx), cx).is_empty()));
    tru274_assert_iced_bytes(&fixture, &iced);
    golive19_stop(&restored, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_f3_new_tab_projects_mounted_cwd_command_and_legacy_uid(cx: &mut TestAppContext) {
    if child("tru274_f3_new_tab_projects_mounted_cwd_command_and_legacy_uid") { return; }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut iced = tru274_document(&fixture, &git.repo);
    iced.workspaces[0].tabs[0].session_uid = None;
    iced.workspaces[0].tabs[0].repo_dir = None;
    iced.workspaces[0].tabs[0].tab_kind = Some("terminal".into());
    iced.workspaces[0].tabs[1].startup_command = Some("placeholder-cli --old".into());
    std::fs::write(fixture.iced.join("workspaces.json"), serde_json::to_vec_pretty(&iced).unwrap()).unwrap();
    let (host, _) = golive19_start(&fixture, cx);
    golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    let sub = git.repo.join("placeholder-subdir");
    std::fs::create_dir(&sub).unwrap();
    let uid = shell.update_in(cx, |s, window, cx| {
        let state = s.active(cx);
        let first = state.panes[&crate::session_pane_id(state.sessions[0].id)].clone();
        let second = state.panes[&crate::session_pane_id(state.sessions[1].id)].clone();
        first.update(cx, |pane, _| { pane.terminal_cwd = Some(sub.clone()); pane.terminal_saved_dir = None; });
        second.update(cx, |pane, _| { pane.startup_command = Some("placeholder-cli --resume synthetic".into()); });
        let uid = first.read(cx).session.as_ref().unwrap().session_uid.clone();
        s.new_terminal_at(crate::DockPlacement::Center, Some(git.repo.clone()), window, cx);
        uid
    });
    tru274_wait(&shell, cx);
    iced.workspaces[0].tabs[0].dir = sub.to_string_lossy().into_owned();
    iced.workspaces[0].tabs[0].repo_dir = Some(git.repo.to_string_lossy().into_owned());
    iced.workspaces[0].tabs[0].tab_kind = None;
    iced.workspaces[0].tabs[0].session_uid = Some(uid);
    iced.workspaces[0].tabs[1].startup_command = Some("placeholder-cli --resume synthetic".into());
    let new_uid = shell.read_with(cx, |s, cx| {
        let state = s.active(cx);
        state.panes[&crate::session_pane_id(state.active_session.unwrap())].read(cx).session.as_ref().unwrap().session_uid.clone()
    });
    iced.workspaces[0].tabs.push(serde_json::from_value(json!({"dir":git.repo,"repo_dir":git.repo,"session_uid":new_uid})).unwrap());
    iced.workspaces[0].active_tab = Some(2);
    tru274_assert_iced_bytes(&fixture, &iced);
    golive19_stop(&shell, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_f6_explicit_local_location_saves_main_and_bottom(cx: &mut TestAppContext) {
    if child("tru274_f6_explicit_local_location_saves_main_and_bottom") { return; }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut iced = tru274_document(&fixture, &git.repo);
    iced.workspaces[0].location = Some(gitterm_core::remote::config::WorkspaceLocationConfig::Local { root: git.repo.to_string_lossy().into_owned() });
    std::fs::write(fixture.iced.join("workspaces.json"), serde_json::to_vec_pretty(&iced).unwrap()).unwrap();
    let (host, _) = golive19_start(&fixture, cx);
    golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    shell.update_in(cx, |s, window, cx| s.new_terminal(window, cx));
    tru274_wait(&shell, cx);
    let uid = shell.read_with(cx, |s, cx| {
        let state = s.active(cx);
        state.panes[&crate::session_pane_id(state.active_session.unwrap())].read(cx).session.as_ref().unwrap().session_uid.clone()
    });
    iced.workspaces[0].tabs.push(serde_json::from_value(json!({"dir":git.repo,"repo_dir":git.repo,"session_uid":uid})).unwrap());
    iced.workspaces[0].active_tab = Some(2);
    shell.update_in(cx, |s, window, cx| s.new_terminal_in(crate::DockPlacement::Bottom, window, cx));
    tru274_wait(&shell, cx);
    iced.workspaces[0].bottom_terminals.push(serde_json::from_value(json!({"dir":git.repo})).unwrap());
    tru274_assert_iced_bytes(&fixture, &iced);
    golive19_stop(&shell, cx);
    host.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn tru274_f7_failed_inventory_save_is_retried_by_drain(cx: &mut TestAppContext) {
    if child("tru274_f7_failed_inventory_save_is_retried_by_drain") { return; }
    tru274_isolate_processes();
    cx.background_executor.allow_parking();
    let (fixture, git) = fixture();
    let mut iced = tru274_document(&fixture, &git.repo);
    let (host, _) = golive19_start(&fixture, cx);
    golive19_capture(&host, cx);
    let (shell, cx) = golive19_shell(&fixture, &host, cx);
    let path = fixture.iced.join("workspaces.json");
    let original = std::fs::read(&path).unwrap();
    let desktop = cx.read(|cx| cx.global::<crate::workspace_config::LiveWorkspaces>().0.clone());
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let hold = desktop.try_edit_workspaces(move |_| {
        entered.send(()).unwrap();
        blocked.recv().unwrap();
        Ok(())
    });
    ready.recv().unwrap();
    // The host still reads valid launch metadata. Block replacement only
    // after the terminal exists and its inventory edit has been queued.
    shell.update_in(cx, |s, window, cx| s.new_terminal(window, cx));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    release.send(()).unwrap();
    hold.wait().unwrap();
    wait_until(cx, |cx| shell.read_with(cx, |s, cx| {
        !s.owner.read(cx).workspace_save_pending && s.owner.read(cx).error.is_some()
    }));
    let desktop = cx.read(|cx| cx.global::<crate::workspace_config::LiveWorkspaces>().0.clone());
    assert!(desktop.snapshot().wait().unwrap().workspaces.dirty);
    let uid = shell.read_with(cx, |s, cx| {
        let state = s.active(cx);
        state.panes[&crate::session_pane_id(state.active_session.unwrap())].read(cx).session.as_ref().unwrap().session_uid.clone()
    });
    iced.workspaces[0].tabs.push(serde_json::from_value(json!({"dir":git.repo,"repo_dir":git.repo,"session_uid":uid})).unwrap());
    iced.workspaces[0].active_tab = Some(2);
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, original).unwrap();
    desktop.drain().wait().unwrap();
    assert!(!desktop.snapshot().wait().unwrap().workspaces.dirty);
    tru274_assert_iced_bytes(&fixture, &iced);
    golive19_stop(&shell, cx);
    host.update(cx, |host, cx| host.quit(cx));
}
