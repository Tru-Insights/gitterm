fn golive19_start(
    fixture: &Fixture,
    cx: &mut TestAppContext,
) -> (Entity<TaskHostOwner>, TaskControlSender) {
    let (config, sender) = fixture.config();
    let lock = Arc::new(HostLock::acquire(&fixture.iced).unwrap());
    let desktop = gitterm_core::desktop_store::DesktopStore::with_lock(lock.clone()).unwrap();
    let services = Arc::new(
        ServiceContext::production(&fixture.iced, std::process::id().to_string())
            .unwrap()
            .for_gpui_live(),
    );
    let owner = cx
        .update(|cx| {
            cx.set_global(crate::workspace_config::LiveWorkspaces(desktop));
            TaskHostOwner::start_live(config, lock, services, cx)
        })
        .unwrap();
    (owner, sender)
}

fn golive19_disk_tabs(
    fixture: &Fixture,
) -> Vec<gitterm_core::workspace_config::WorkspaceTabConfig> {
    serde_json::from_slice::<gitterm_core::workspace_config::WorkspacesFile>(
        &std::fs::read(fixture.iced.join("workspaces.json")).unwrap(),
    )
    .unwrap()
    .workspaces[0]
        .tabs
        .clone()
}

fn golive19_shell<'a>(
    fixture: &Fixture,
    host: &Entity<TaskHostOwner>,
    cx: &'a mut TestAppContext,
) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_global(crate::task_host::GpuiTaskHost(host.clone()));
        // Reconstruct a new shell/host over saved bytes in the same test app.
        // The process-wide production policy is installed only once.
        if !crate::chat::policy::is_live(cx) {
            crate::chat::policy::install_live(host.read(cx).host().services().clone(), cx).unwrap();
        }
    });
    let mut store = crate::preview::tests::guarded_store(fixture.roots.root());
    let model = crate::workspace_config::load_iced_workspaces_from(&mut store, &fixture.iced);
    let settings = crate::workspace_config::load_iced_settings_from(&fixture.iced);
    cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        crate::Shell::with_owner_settings(
            std::rc::Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            fixture.roots.root(),
            true,
            &settings,
            window,
            cx,
        )
        .unwrap()
    })
}

// Named tests only; each Live case uses the existing synthetic HOME/V6 child.
fn golive19_capture(
    host: &Entity<TaskHostOwner>,
    cx: &mut TestAppContext,
) -> Rc<RefCell<Vec<gitterm_core::task_host::command::TerminalCommand>>> {
    let specs = Rc::new(RefCell::new(Vec::new()));
    let captured = specs.clone();
    host.update(cx, |host, _| {
        host.terminal_spec_hook = Some(Box::new(move |spec| {
            captured.borrow_mut().push(spec.clone());
            // Capture the actual application argv/env, but run only an inert
            // fake PTY bridge. Neither ssh nor an installed agent is executed.
            spec.program = "/bin/sh".into();
            spec.args = vec!["-c".into(), "exec /bin/cat".into()];
        }));
    });
    specs
}

fn golive19_stop(shell: &Entity<crate::Shell>, cx: &mut gpui::VisualTestContext) {
    shell.update_in(cx, |shell, _, cx| {
        let panes: Vec<_> = shell.active(cx).panes.values().cloned().collect();
        for pane in panes {
            if let Some(target) = pane.read(cx).session.clone() {
                pane.update(cx, |pane, cx| pane.stop_target(&target, cx));
            }
        }
    });
}

#[gpui::test]
fn golive19_live_ssh_routes_deduplicate_and_restore_saved_bytes(cx: &mut TestAppContext) {
    if child("golive19_live_ssh_routes_deduplicate_and_restore_saved_bytes") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    let remote = json!({"label":"Placeholder","host_name":"Remote","ssh_target":"user@remote.example","identity_file":"/tmp/placeholder key's file","tmux_path":"/opt/tmux bin/tmux","session_name":"placeholder-session","remote_dir":"/srv/project's folder","shell_command":"/bin/zsh -il","codex_command":"/opt/agent/codex --model placeholder","claude_command":"claude --model placeholder"});
    let document = json!({"active_workspace":0,"workspaces":[{"name":"Remote:Placeholder","abbrev":"RP","dir":"/srv/project's folder","color":"blue","remote_session":remote,"tabs":[]}]});
    std::fs::write(
        fixture.iced.join("workspaces.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    use gitterm_core::remote::launch::{self, SessionKind};
    for kind in [
        SessionKind::Shell,
        SessionKind::Codex,
        SessionKind::Claude,
        SessionKind::Shell,
    ] {
        shell.update_in(vcx, |shell, window, cx| {
            shell.open_remote_session_terminal(kind, window, cx)
        });
        wait_until(vcx, |cx| {
            !shell.read_with(cx, |shell, cx| shell.owner.read(cx).workspace_save_pending)
        });
    }
    assert_eq!(
        specs.borrow().len(),
        3,
        "reopen selects an existing command"
    );
    let definition = launch::RemoteSessionDefinition::from(
        &serde_json::from_value::<gitterm_core::workspace_config::RemoteSessionConfig>(remote)
            .unwrap(),
    );
    for (spec, kind) in
        specs
            .borrow()
            .iter()
            .zip([SessionKind::Shell, SessionKind::Codex, SessionKind::Claude])
    {
        let (name, command, _) = launch::session_command(&definition, kind).unwrap();
        assert_eq!(
            spec.env["GITTERM_STARTUP_CMD"],
            launch::remote_attach_command(&definition, &name, &command)
        );
        assert_eq!(spec.cwd, PathBuf::from(std::env::var_os("HOME").unwrap()));
    }
    let saved = shell.read_with(vcx, |shell, cx| {
        shell.live_workspace_tabs(shell.active(cx).runtime_uid, cx)
    });
    assert_eq!(saved.len(), 3);
    let closed = shell.read_with(vcx, |shell, cx| {
        shell
            .active(cx)
            .panes
            .values()
            .find_map(|p| {
                (p.read(cx).title == "Codex").then(|| p.read(cx).session.clone().unwrap())
            })
            .unwrap()
    });
    shell.update_in(vcx, |shell, window, cx| {
        shell.close_target(&closed, window, cx)
    });
    shell.read_with(vcx, |shell, cx| {
        assert_eq!(shell.active(cx).sessions.len(), 2)
    });
    wait_until(vcx, |cx| {
        !shell.read_with(cx, |shell, cx| shell.owner.read(cx).workspace_save_pending)
    });
    let saved = golive19_disk_tabs(&fixture);
    assert_eq!(saved.len(), 2, "close removes the saved remote tab");
    golive19_stop(&shell, vcx);
    host.update(vcx, |host, cx| host.quit(cx));
    vcx.update(|_, cx| {
        cx.remove_global::<crate::workspace_config::LiveWorkspaces>();
    });
    let (restored_host, _) = golive19_start(&fixture, vcx);
    let restored_specs = golive19_capture(&restored_host, vcx);
    let (restored_shell, vcx) = golive19_shell(&fixture, &restored_host, vcx);
    assert_eq!(restored_specs.borrow().len(), 2);
    for (tab, spec) in saved.iter().zip(restored_specs.borrow().iter()) {
        assert_eq!(
            tab.startup_command.as_deref(),
            Some(spec.env["GITTERM_STARTUP_CMD"].as_str())
        );
    }
    restored_shell.update_in(vcx, |shell, window, cx| {
        let id = shell.active(cx).workspace.id;
        shell.restore_live_workspace(id, false, window, cx);
    });
    assert_eq!(restored_specs.borrow().len(), 2, "mount only once");
    golive19_stop(&restored_shell, vcx);
    restored_host.update(vcx, |host, cx| host.quit(cx));
}

#[derive(Default)]
struct Golive19Remote {
    starts: std::sync::Mutex<Vec<(String, gitterm_core::remote::launch::AgentSessionLaunch)>>,
}
impl crate::remote_client::RemoteClient for Golive19Remote {
    fn start_session(
        &self,
        _: &gitterm_core::remote::config::RemoteAgentConfig,
        workspace: String,
        launch: gitterm_core::remote::launch::AgentSessionLaunch,
    ) -> Result<String, String> {
        self.starts.lock().unwrap().push((workspace, launch));
        Ok("placeholder-session".into())
    }
    fn handshake(
        &self,
        r: &gitterm_core::remote::config::RemoteAgentConfig,
    ) -> Result<gitterm_core::remote::client::RemoteAgentHandshake, String> {
        crate::remote_client::RemoteClient::handshake(&crate::remote_client::Offline, r)
    }
    fn list_dir(
        &self,
        r: &gitterm_core::remote::config::RemoteAgentConfig,
        d: String,
    ) -> gitterm_core::remote::source::SourceDirListing {
        crate::remote_client::RemoteClient::list_dir(&crate::remote_client::Offline, r, d)
    }
    fn list_chats(
        &self,
        r: &gitterm_core::remote::config::RemoteAgentConfig,
    ) -> Result<Vec<gitterm_core::chats::ChatIndexEntry>, String> {
        crate::remote_client::RemoteClient::list_chats(&crate::remote_client::Offline, r)
    }
    fn list_sessions(
        &self,
        h: &gitterm_core::remote::ssh_sessions::RemoteHostConfig,
    ) -> gitterm_core::remote::ssh_sessions::RemoteSessionsSnapshot {
        crate::remote_client::RemoteClient::list_sessions(&crate::remote_client::Offline, h)
    }
    fn list_phones(
        &self,
        r: &gitterm_core::remote::config::RemoteAgentConfig,
    ) -> Result<Vec<gitterm_core::remote::phone_panel::PairedPhoneInfo>, String> {
        crate::remote_client::RemoteClient::list_phones(&crate::remote_client::Offline, r)
    }
}

#[gpui::test]
fn golive19_live_agent_preset_cwd_and_restart_only_attach(cx: &mut TestAppContext) {
    if child("golive19_live_agent_preset_cwd_and_restart_only_attach") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    let document = json!({"active_workspace":0,"remote_agents":[{"id":"placeholder","name":"Placeholder Mac","endpoint":"http://remote.example:7777","auth":{"type":"token","token_ref":"env:PLACEHOLDER_TOKEN"},"shell_command":"/bin/bash -il","session_commands":{"claude code":"claude --model placeholder"}}],"workspaces":[{"name":"Placeholder","abbrev":"PH","dir":"/srv/root","color":"blue","location":{"kind":"remote_agent","remote_id":"placeholder","workspace_id":"placeholder-workspace","root":"/srv/root"},"tabs":[{"dir":"/srv/project's folder","session_uid":"placeholder-files"}]}]});
    std::fs::write(
        fixture.iced.join("workspaces.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    std::fs::write(
        fixture.iced.join("settings.json"),
        br#"{"agent_presets":[{"name":"Claude Code","command":"must-not-run-locally"}]}"#,
    )
    .unwrap();
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    let remote = Arc::new(Golive19Remote::default());
    // Both RPCs start before either completion saves its tab.
    shell.update_in(vcx, |shell, window, cx| {
        shell.set_remote_client(remote.clone());
        shell.launch_agent_preset(0, window, cx);
        shell.new_terminal(window, cx);
    });
    wait_until(vcx, |cx| {
        cx.executor().advance_clock(Duration::from_millis(20));
        specs.borrow().len() == 2
            && !shell.read_with(cx, |shell, cx| shell.owner.read(cx).workspace_save_pending)
    });
    {
        let calls = remote.starts.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let preset = calls
            .iter()
            .find(|(_, launch)| launch.kind == "claude")
            .unwrap();
        assert_eq!(preset.0, "placeholder-workspace");
        assert_eq!(preset.1.cwd, "/srv/project's folder");
        assert_eq!(
            preset.1.command,
            ["/bin/sh", "-lc", "claude --no-chrome --model placeholder"]
        );
        assert!(preset.1.env.is_empty());
        assert!(
            calls
                .iter()
                .all(|(_, launch)| launch.cwd == "/srv/project's folder")
        );
    }
    shell.update_in(vcx, |shell, window, cx| shell.new_terminal(window, cx));
    wait_until(vcx, |cx| {
        specs.borrow().len() == 3
            && !shell.read_with(cx, |shell, cx| shell.owner.read(cx).workspace_save_pending)
    });
    {
        let calls = remote.starts.lock().unwrap();
        assert_eq!(calls[2].1.cwd, "/srv/root", "attach tab's browser is local");
        assert_eq!(calls[2].1.command, ["/bin/sh", "-lc", "/bin/bash -il"]);
    }
    let saved = shell.read_with(vcx, |shell, cx| {
        shell.live_workspace_tabs(shell.active(cx).runtime_uid, cx)
    });
    assert_eq!(saved.len(), 4);
    assert!(saved[1].startup_command.as_ref().unwrap().contains(" attach --endpoint 'http://remote.example:7777' --token-ref 'env:PLACEHOLDER_TOKEN' --session 'placeholder-session'"));
    assert_eq!(
        serde_json::to_value(golive19_disk_tabs(&fixture)).unwrap(),
        serde_json::to_value(&saved).unwrap()
    );
    // Quit saves the selected tab even when no new terminal was launched.
    let (finished, completion) = std::sync::mpsc::channel();
    shell.update_in(vcx, |shell, window, cx| {
        let files = shell.active(cx).sessions[0].id;
        shell.select_session_id(files, window, cx);
        let save = shell.save_remote_tabs_on_quit(cx);
        cx.spawn(async move |_, _| {
            save.await;
            finished.send(()).unwrap();
        })
        .detach();
    });
    wait_until(vcx, |_| completion.try_recv().is_ok());
    let disk: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_slice(&std::fs::read(fixture.iced.join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(disk.workspaces[0].active_tab, Some(0));
    golive19_stop(&shell, vcx);
    host.update(vcx, |host, cx| host.quit(cx));
    vcx.update(|_, cx| {
        cx.remove_global::<crate::workspace_config::LiveWorkspaces>();
    });
    let (restored_host, _) = golive19_start(&fixture, vcx);
    let restored_specs = golive19_capture(&restored_host, vcx);
    let (restored_shell, vcx) = golive19_shell(&fixture, &restored_host, vcx);
    restored_shell.update_in(vcx, |shell, _, _| shell.remote.client = remote.clone());
    assert_eq!(
        remote.starts.lock().unwrap().len(),
        3,
        "restart never calls start_session"
    );
    assert_eq!(restored_specs.borrow().len(), 3);
    for (spec, tab) in restored_specs.borrow().iter().zip(&saved[1..]) {
        assert_eq!(
            Some(&spec.env["GITTERM_STARTUP_CMD"]),
            tab.startup_command.as_ref()
        );
        assert_eq!(spec.cwd, PathBuf::from(std::env::var_os("HOME").unwrap()));
    }
    let again = restored_shell.read_with(vcx, |shell, cx| {
        shell.live_workspace_tabs(shell.active(cx).runtime_uid, cx)
    });
    assert_eq!(
        serde_json::to_value(&again).unwrap(),
        serde_json::to_value(&saved).unwrap()
    );
    golive19_stop(&restored_shell, vcx);
    restored_host.update(vcx, |host, cx| host.quit(cx));
}
