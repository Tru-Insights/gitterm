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

fn review19_document() -> serde_json::Value {
    json!({"active_workspace":0,"remote_agents":[{"id":"placeholder","name":"Placeholder Mac","endpoint":"http://remote.example:7777","auth":{"type":"token","token_ref":"env:PLACEHOLDER_TOKEN"}}],"workspaces":[{"name":"Placeholder","abbrev":"PH","dir":"/srv/root","color":"blue","location":{"kind":"remote_agent","remote_id":"placeholder","workspace_id":"placeholder-workspace","root":"/srv/root"},"tabs":[]}]})
}

fn review19_write(fixture: &Fixture, document: &serde_json::Value) {
    serde_json::from_value::<gitterm_core::workspace_config::WorkspacesFile>(document.clone())
        .unwrap();
    std::fs::write(
        fixture.iced.join("workspaces.json"),
        serde_json::to_vec(document).unwrap(),
    )
    .unwrap();
}

fn review19_wait_save(shell: &Entity<crate::Shell>, cx: &mut gpui::VisualTestContext) {
    wait_until(cx, |cx| {
        cx.executor().advance_clock(Duration::from_millis(20));
        !shell.read_with(cx, |shell, cx| shell.owner.read(cx).workspace_save_pending)
    });
}

#[gpui::test]
fn review19_f1_legacy_close_keeps_unmounted_claude_fields(cx: &mut TestAppContext) {
    if child("review19_f1_legacy_close_keeps_unmounted_claude_fields") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    let home = std::env::var("HOME").unwrap();
    let agent = json!({"dir":home,"tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"placeholder-chat"});
    let document = json!({"active_workspace":0,"workspaces":[{"name":"Legacy","abbrev":"L","dir":home,"color":"blue","remote_session":{"label":"Placeholder","host_name":"Unconfigured","ssh_target":"user@remote.example","identity_file":"/tmp/placeholder-key","session_name":"placeholder-session","remote_dir":"/srv/root","shell_command":"/bin/sh"},"tabs":[{"dir":home,"startup_command":"placeholder-attach"},agent]}]});
    review19_write(&fixture, &document);
    let (host, _) = golive19_start(&fixture, cx);
    let _specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    shell.update_in(vcx, |shell, window, cx| {
        let state = shell.active(cx);
        assert!(
            state.panes[&crate::session_pane_id(state.sessions[1].id)]
                .read(cx)
                .session
                .is_none()
        );
        let target = state.panes[&crate::session_pane_id(state.sessions[0].id)]
            .read(cx)
            .session
            .clone()
            .unwrap();
        shell.close_target(&target, window, cx);
    });
    review19_wait_save(&shell, vcx);
    // Publishing the first save shifts the legacy index. A subsequent save
    // must still address the same unmounted native Claude DTO.
    shell.update_in(vcx, |shell, _, cx| {
        shell.persist_remote_tabs(shell.active(cx).workspace.id, cx)
    });
    review19_wait_save(&shell, vcx);
    let tabs = golive19_disk_tabs(&fixture);
    assert_eq!(tabs.len(), 1);
    assert_eq!(
        serde_json::to_value(&tabs[0]).unwrap()["agent_config"],
        agent["agent_config"]
    );
    assert_eq!(tabs[0].chat_session_id.as_deref(), Some("placeholder-chat"));
    assert_eq!(tabs[0].tab_kind.as_deref(), Some("agent"));
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn review19_f2_parked_running_keeps_inflight_completion(cx: &mut TestAppContext) {
    if child("review19_f2_parked_running_keeps_inflight_completion") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    let mut document = review19_document();
    document["workspaces"][0]["tabs"] = json!([{"dir":std::env::var("HOME").unwrap(),"startup_command":"placeholder-existing-attach","session_uid":"placeholder-existing"}]);
    document["workspaces"].as_array_mut().unwrap().push(json!({"name":"Other","abbrev":"O","dir":std::env::var("HOME").unwrap(),"color":"blue","location":{"kind":"remote_agent","remote_id":"placeholder","workspace_id":"other","root":"/srv/other"},"tabs":[]}));
    review19_write(&fixture, &document);
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    let remote = Arc::new(Golive19Remote::default());
    shell.update_in(vcx, |shell, window, cx| {
        shell.set_remote_client(remote.clone());
        shell.new_terminal(window, cx);
        // Hold completion at the same admission boundary as a park save.
        shell
            .owner
            .update(cx, |owner, _| owner.workspace_save_pending = true);
    });
    wait_until(vcx, |_| remote.starts.lock().unwrap().len() == 1);
    let store = vcx.update(|_, cx| {
        cx.global::<crate::workspace_config::LiveWorkspaces>()
            .0
            .clone()
    });
    // The base branch still rejects remote parking in its UI writer. Seed
    // the persisted Keep-running state here to isolate launch completion.
    store
        .edit_workspaces(|file| {
            file.workspaces[0] = gitterm_core::park::parked_config(
                file.workspaces[0].clone(),
                gitterm_core::workspace_config::ParkedInfo {
                    note: String::new(),
                    parked_at: "2026-01-01T00:00:00Z".into(),
                },
            );
        })
        .wait()
        .unwrap();
    let workspace = shell.update_in(vcx, |shell, _, cx| {
        let id = shell.active(cx).workspace.id;
        shell
            .workspaces
            .iter_mut()
            .find(|state| state.workspace.id == id)
            .unwrap()
            .parked = Some(crate::park::Parked {
            info: gitterm_core::park::Info::new("", "2026-01-01T00:00:00Z".into()),
            running: true,
            done: false,
        });
        shell
            .owner
            .update(cx, |owner, _| owner.workspace_save_pending = false);
        shell.select_bar_workspace(cx);
        id
    });
    review19_wait_save(&shell, vcx);
    shell.read_with(vcx, |shell, cx| {
        assert!(
            shell
                .workspace(workspace)
                .parked
                .as_ref()
                .is_some_and(|p| p.running),
            "park did not apply: {:?}",
            shell.owner.read(cx).error
        );
        assert_ne!(shell.active(cx).workspace.id, workspace);
    });
    wait_until(vcx, |cx| {
        cx.executor().advance_clock(Duration::from_millis(20));
        specs.borrow().len() == 2
            && !shell.read_with(cx, |s, cx| s.owner.read(cx).workspace_save_pending)
    });
    let disk: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_slice(&std::fs::read(fixture.iced.join("workspaces.json")).unwrap())
            .unwrap();
    let saved = disk
        .workspaces
        .iter()
        .find(|w| w.name == "Placeholder")
        .unwrap();
    assert!(!saved.active);
    let tabs = &saved.tabs;
    assert_eq!(tabs.len(), 2);
    assert!(
        tabs[1]
            .startup_command
            .as_ref()
            .unwrap()
            .contains("--session 'placeholder-session'")
    );
    shell.update_in(vcx, |shell, window, cx| {
        assert!(shell.workspace(workspace).parked.as_ref().unwrap().running);
        assert_ne!(
            shell.active(cx).workspace.id,
            workspace,
            "completion must not steal focus"
        );
        let state = shell.workspace(workspace);
        let pane = &state.panes[&crate::session_pane_id(state.sessions.last().unwrap().id)];
        assert!(
            state
                .dock
                .read(cx)
                .panel(gpui_component::dock::PanelId::from(pane.entity_id()))
                .is_some()
        );
        let uid = shell.workspace(workspace).runtime_uid;
        shell.apply_park(
            gitterm_core::park::Effect::Return {
                uid,
                woke: None,
                activate: true,
            },
            window,
            cx,
        );
        shell.restore_live_workspace_sessions(workspace, window, cx);
    });
    assert_eq!(
        remote.starts.lock().unwrap().len(),
        1,
        "resume never starts a second agent"
    );
    assert_eq!(specs.borrow().len(), 2, "resume keeps the existing bridges");
    golive19_stop(&shell, vcx);
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn review19_f3_ambiguous_workspace_rejected_before_rpc(cx: &mut TestAppContext) {
    if child("review19_f3_ambiguous_workspace_rejected_before_rpc") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    review19_write(&fixture, &review19_document());
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    let store = vcx.update(|_, cx| {
        cx.global::<crate::workspace_config::LiveWorkspaces>()
            .0
            .clone()
    });
    store
        .edit_workspaces(|file| file.workspaces.push(file.workspaces[0].clone()))
        .wait()
        .unwrap();
    let bytes = std::fs::read(fixture.iced.join("workspaces.json")).unwrap();
    let remote = Arc::new(Golive19Remote::default());
    shell.update_in(vcx, |shell, window, cx| {
        shell.set_remote_client(remote.clone());
        shell.new_terminal(window, cx);
    });
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| s.owner.read(cx).error.is_some())
    });
    assert!(remote.starts.lock().unwrap().is_empty());
    assert!(specs.borrow().is_empty());
    assert_eq!(
        std::fs::read(fixture.iced.join("workspaces.json")).unwrap(),
        bytes
    );
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn review19_f4_legacy_plain_tab_is_local(cx: &mut TestAppContext) {
    if child("review19_f4_legacy_plain_tab_is_local") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    review19_write(
        &fixture,
        &json!({"active_workspace":0,"workspaces":[{"name":"Legacy","abbrev":"L","dir":std::env::var("HOME").unwrap(),"color":"blue","remote_session":{"label":"Placeholder","host_name":"Unconfigured","ssh_target":"user@remote.example","identity_file":"/tmp/placeholder-key","session_name":"placeholder-session","remote_dir":"/srv/root","shell_command":"/bin/sh"},"tabs":[]}]}),
    );
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    shell.update_in(vcx, |shell, window, cx| shell.new_terminal(window, cx));
    review19_wait_save(&shell, vcx);
    shell.read_with(vcx, |shell, cx| {
        assert_eq!(shell.active(cx).sessions.len(), 1);
        let pane = shell.active(cx).panes[&crate::session_pane_id(shell.active(cx).sessions[0].id)]
            .read(cx);
        assert!(pane.startup_command.is_none());
        assert!(pane.session.is_some());
    });
    assert_eq!(specs.borrow().len(), 1);
    assert!(
        !specs.borrow()[0].env.contains_key("GITTERM_STARTUP_CMD"),
        "plain tab does not open an SSH bridge"
    );
    assert!(golive19_disk_tabs(&fixture)[0].startup_command.is_none());
    golive19_stop(&shell, vcx);
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn review19_f5_legacy_agent_host_infers_launch_source(cx: &mut TestAppContext) {
    if child("review19_f5_legacy_agent_host_infers_launch_source") {
        return;
    }
    cx.background_executor.allow_parking();
    let (fixture, _) = fixture();
    let mut document = review19_document();
    document["workspaces"][0]
        .as_object_mut()
        .unwrap()
        .remove("location");
    document["workspaces"][0]["remote_session"] = json!({"label":"Placeholder","host_name":"placeholder","ssh_target":"user@remote.example","identity_file":"/tmp/placeholder-key","session_name":"legacy-workspace","remote_dir":"/srv/legacy","shell_command":"/bin/sh"});
    let mut by_name = document["workspaces"][0].clone();
    by_name["name"] = json!("By name");
    by_name["remote_session"]["host_name"] = json!("pLaCeHoLdEr MaC");
    by_name["remote_session"]["session_name"] = json!("legacy-by-name");
    document["workspaces"].as_array_mut().unwrap().push(by_name);
    review19_write(&fixture, &document);
    let (host, _) = golive19_start(&fixture, cx);
    let specs = golive19_capture(&host, cx);
    let (shell, vcx) = golive19_shell(&fixture, &host, cx);
    let remote = Arc::new(Golive19Remote::default());
    for (index, workspace_id) in ["legacy-workspace", "legacy-by-name"].iter().enumerate() {
        shell.update_in(vcx, |shell, window, cx| {
            shell.set_remote_client(remote.clone());
            shell.select_workspace_id(crate::WorkspaceId(index as u64 + 1), window, cx);
            assert!(shell.launch_remote_agent_terminal(None, window, cx));
        });
        wait_until(vcx, |cx| {
            cx.executor().advance_clock(Duration::from_millis(20));
            specs.borrow().len() == index + 1
                && !shell.read_with(cx, |s, cx| s.owner.read(cx).workspace_save_pending)
        });
        let calls = remote.starts.lock().unwrap();
        assert_eq!(calls[index].0, *workspace_id);
        assert_eq!(calls[index].1.cwd, "/srv/legacy");
    }
    assert!(
        specs
            .borrow()
            .iter()
            .all(|s| s.env["GITTERM_STARTUP_CMD"].contains(" attach --endpoint "))
    );
    let disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.iced.join("workspaces.json")).unwrap())
            .unwrap();
    assert!(
        disk["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w.get("location").is_none())
    );
    for id in [crate::WorkspaceId(1), crate::WorkspaceId(2)] {
        shell.update_in(vcx, |shell, window, cx| {
            shell.select_workspace_id(id, window, cx)
        });
        golive19_stop(&shell, vcx);
    }
    host.update(vcx, |host, cx| host.quit(cx));
}
