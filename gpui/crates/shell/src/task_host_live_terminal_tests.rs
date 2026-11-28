// Production routes, isolated by live_writes' synthetic HOME/V6 subprocess.
fn golive10_shell<'a>(
    fixture: &Fixture,
    host: &Entity<TaskHostOwner>,
    cx: &'a mut TestAppContext,
) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_global(crate::task_host::GpuiTaskHost(host.clone()));
        crate::chat::policy::install_live(host.read(cx).host().services().clone(), cx).unwrap();
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

fn golive10_workspace(
    fixture: &Fixture,
    repo: &Path,
    tabs: serde_json::Value,
    bottom: serde_json::Value,
) {
    std::fs::write(fixture.iced.join("workspaces.json"), serde_json::to_vec(&json!({
        "active_workspace":0,"workspaces":[{"name":"Placeholder","abbrev":"FX","dir":repo,
        "color":"blue","guidelines_off":true,"env_overrides":{"PLACEHOLDER_ENV":"workspace"},
        "tabs":tabs,"bottom_terminals":bottom}],"repos":[{"dir":repo,"env":{"PLACEHOLDER_ENV":"repo","PLACEHOLDER_REPO":"yes"},"run_command":"printf placeholder"}]
    })).unwrap()).unwrap();
}

#[gpui::test]
fn golive10_cli_presets_shortcuts_and_plain_callers(cx: &mut TestAppContext) {
    if child("golive10_cli_presets_shortcuts_and_plain_callers") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let bin = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/bin");
    for name in ["pi", "placeholder"] {
        std::fs::copy(bin.join("codex"), bin.join(name)).unwrap();
    }
    let (fixture, git) = fixture();
    golive10_workspace(
        &fixture,
        &git.repo,
        json!([{"dir":git.repo,"session_uid":"plain-placeholder"}]),
        json!([]),
    );
    let commands = [
        "codex -m placeholder",
        "claude --model placeholder",
        "pi --no-tools",
        "placeholder --route four",
        "placeholder --route five",
        "placeholder --route six",
        "placeholder --route seven",
        "placeholder --route eight",
        "placeholder --route nine",
    ];
    std::fs::write(fixture.iced.join("settings.json"), serde_json::to_vec(&json!({"agent_presets":commands.iter().enumerate().map(|(i,c)|json!({"name":format!("Preset {i}"),"command":c})).collect::<Vec<_>>()})).unwrap()).unwrap();
    let (host, sender) = start(&fixture, cx);
    let specs = Rc::new(RefCell::new(Vec::new()));
    let captured = specs.clone();
    host.update(cx, |host, _| {
        host.terminal_spec_hook = Some(Box::new(move |spec| {
            captured.borrow_mut().push(spec.clone())
        }))
    });
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    vcx.run_until_parked();
    shell.update_in(vcx, |shell, _, cx| shell.sync_task_panes(cx));
    host.read_with(vcx, |host, _| {
        let caller = host
            .terminal_callers
            .iter()
            .find(|c| c.target.session_uid == "plain-placeholder")
            .expect("plain terminal caller");
        assert_eq!(caller.tab.cwd, git.repo);
        assert!(
            host.host()
                .require_issued_caller(Some("forged-placeholder"), "Terminal")
                .is_err()
        );
    });
    let plain = specs.borrow()[0].clone();
    assert!(plain.env["GITTERM_V6_TASK_MCP_URL"].ends_with("?caller=plain-placeholder"));
    assert_eq!(plain.env["PLACEHOLDER_ENV"], "workspace");
    assert_eq!(plain.env["PLACEHOLDER_REPO"], "yes");
    for (index, command) in commands.iter().enumerate() {
        shell.update_in(vcx, |shell, window, cx| {
            shell.focus.focus(window, cx);
            window.dispatch_action(Box::new(crate::app_menus::LaunchAgentPreset { index }), cx);
        });
        wait_until(vcx, |_| fake.file("ready", index + 1).exists());
        let spec = specs.borrow().last().unwrap().clone();
        let saved = shell.read_with(vcx, |shell, cx| {
            shell
                .active(cx)
                .store
                .terminals
                .as_ref()
                .unwrap()
                .last()
                .unwrap()
                .clone()
        });
        let expected = host.read_with(vcx, |host, cx| {
            let mut command = saved.startup_command.clone().unwrap();
            if gitterm_core::guidelines::starts_a_harness(&command) {
                if let Some(g) = host.launch_instructions(
                    &git.repo,
                    gitterm_core::guidelines::instruction_file_for_command(&command),
                    "Placeholder",
                    true,
                    cx,
                ) {
                    command = gitterm_core::guidelines::configure_terminal_command(&command, &g);
                }
            }
            command = gitterm_core::harness::claude::configure_terminal_chrome(&command, false);
            let caller = saved.session_uid.as_deref().unwrap();
            assert_eq!(
                spec.env["GITTERM_V6_TASK_MCP_URL"],
                format!("{}?caller={caller}", host.task_endpoint().unwrap())
            );
            assert_eq!(
                spec.env["GITTERM_V6_BROWSER_MCP_URL"],
                format!("{}?caller={caller}", host.browser_endpoint().unwrap())
            );
            let mut env = std::collections::BTreeMap::from([
                ("PLACEHOLDER_ENV".to_string(), "workspace".to_string()),
                ("PLACEHOLDER_REPO".to_string(), "yes".to_string()),
            ]);
            for key in [
                "GITTERM_V6_TASK_MCP_URL",
                "GITTERM_V6_TASK_MCP_TOKEN",
                "GITTERM_V6_BROWSER_MCP_URL",
                "GITTERM_V6_BROWSER_MCP_TOKEN",
            ] {
                env.insert(key.to_string(), spec.env[key].clone());
            }
            let refs: Vec<_> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            gitterm_core::task_host::command::terminal_command(
                &git.repo,
                Some(&command),
                &refs,
                &fixture.iced,
                gitterm_browser::browser_mcp::configure_browser_command,
            )
        });
        assert_eq!(spec, expected, "Iced terminal command for preset {index}");
        let startup = &spec.env["GITTERM_STARTUP_CMD"];
        let words = shell_words::split(startup).unwrap();
        assert_eq!(
            fake.text("args", index + 1),
            format!("{}\n", words[1..].join("\n"))
        );
        let env = fake.text("env", index + 1);
        for key in [
            "PLACEHOLDER_ENV",
            "PLACEHOLDER_REPO",
            "GITTERM_V6_TASK_MCP_URL",
            "GITTERM_V6_TASK_MCP_TOKEN",
        ] {
            assert!(
                env.lines()
                    .any(|line| line == format!("{key}={}", spec.env[key])),
                "missing {key}"
            );
        }
        assert_eq!(
            PathBuf::from(fake.text("pwd", index + 1).trim()),
            git.repo.canonicalize().unwrap()
        );
        let saved = shell.read_with(vcx, |shell, cx| {
            shell
                .active(cx)
                .store
                .terminals
                .as_ref()
                .unwrap()
                .last()
                .unwrap()
                .startup_command
                .clone()
                .unwrap()
        });
        if index != 1 {
            assert_eq!(&saved, command);
        } else {
            assert!(saved.starts_with(command));
            assert!(saved.contains("--session-id "));
        }
    }
    let created = dispatch_as(
        vcx,
        &sender,
        create_in(&git.repo),
        Some("plain-placeholder"),
    )
    .unwrap();
    assert_eq!(created["created_by"]["session_id"], "plain-placeholder");
    // Closing a terminal revokes its issued caller before another operation.
    shell.update_in(vcx, |shell, _, cx| {
        let state = shell.active(cx);
        let id = crate::data::iced_sessions(
            shell
                .owner
                .read(cx)
                .iced_workspace(state.workspace.id)
                .unwrap(),
        )[0]
        .id;
        state.panes[&crate::session_pane_id(id)].update(cx, |pane, cx| pane.close_terminal(cx));
    });
    assert!(
        dispatch_as(
            vcx,
            &sender,
            create_in(&git.repo),
            Some("plain-placeholder")
        )
        .is_err()
    );
    assert!(
        dispatch_as(
            vcx,
            &sender,
            create_in(&git.repo),
            Some("foreign-placeholder")
        )
        .is_err()
    );
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn golive10_task_terminal_restart_resumes_without_objective(cx: &mut TestAppContext) {
    if child("golive10_task_terminal_restart_resumes_without_objective") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (host, sender) = start(&fixture, cx);
    fake.prepare_recorder(&host, cx);
    let created = create(cx, &sender, &git.repo);
    let task_id = created["task_id"].as_str().unwrap();
    let result = launch_live(cx, &sender, task_id, "Codex").unwrap();
    let session = result["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |_| fake.file("ready", 1).exists());
    let cwd = PathBuf::from(created["worktree"]["path"].as_str().unwrap());
    let mut task = host.read_with(cx, |host, _| {
        host.host()
            .store()
            .as_ref()
            .unwrap()
            .get(task_id)
            .unwrap()
            .clone()
    });
    task.sessions[0].conversation = Some(gitterm_core::tasks::HarnessConversationRef {
        backend: gitterm_core::tasks::HarnessConversationBackend::Codex,
        session_id: "placeholder-recorded".into(),
    });
    let delivery = task.sessions[0].objective_delivery;
    host.update(cx, |host, cx| host.quit(cx));
    drop(host);
    fixture.write_iced(vec![task], vec![]);
    let (restored, restored_sender) = start(&fixture, cx);
    fake.prepare_recorder(&restored, cx);
    let saved = crate::layout::TerminalSession {
        id: 1,
        cwd: cwd.clone(),
        startup_command: Some("codex -m placeholder".into()),
        task_id: Some(task_id.into()),
        task_session_id: Some(session.clone()),
        session_uid: Some(session.clone()),
        chat_session_id: None,
        bottom: false,
    };
    let terminal = restored
        .update(cx, |host, cx| {
            host.restore_terminal("Placeholder", &saved, cx)
        })
        .expect("production task restore");
    assert_eq!(
        terminal.target.task_session_id.as_deref(),
        Some(session.as_str())
    );
    wait_until(cx, |_| fake.file("ready", 2).exists());
    let spec = fake.spec.borrow().clone().unwrap();
    let command = spec.env["GITTERM_STARTUP_CMD"]
        .split_once("&& {\n")
        .unwrap()
        .1
        .strip_suffix("\n} )")
        .unwrap();
    let words = shell_words::split(command).unwrap();
    assert_eq!(fake.text("args", 2), format!("{}\n", words[1..].join("\n")));
    let env = fake.text("env", 2);
    for key in [
        "HOME",
        "GITTERM_V6_TASK_MCP_URL",
        "GITTERM_V6_TASK_MCP_TOKEN",
        gitterm_core::worker_process::PID_FILE_ENV,
    ] {
        assert!(
            env.lines()
                .any(|line| line == format!("{key}={}", spec.env[key])),
            "missing {key}"
        );
    }
    assert!(spec.env["GITTERM_V6_TASK_MCP_URL"].ends_with(&format!("?caller={session}")));
    let args = fake.text("args", 2);
    assert!(
        args.lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|a| a == ["resume", "placeholder-recorded"]),
        "{args}"
    );
    assert!(!args.contains("Placeholder objective"));
    assert!(!args.contains("task-briefs"));
    assert_eq!(
        restored.read_with(cx, |host, _| host
            .host()
            .store()
            .as_ref()
            .unwrap()
            .get(task_id)
            .unwrap()
            .sessions[0]
            .objective_delivery),
        delivery
    );
    let event = |task: &str, session: &str| {
        TaskControlOperation::SessionEvent(req(
            json!({"task_id":task,"session_id":session,"event_type":"agent-turn-complete"}),
        ))
    };
    assert!(dispatch(cx, &restored_sender, event(task_id, "foreign-placeholder")).is_err());
    assert!(dispatch(cx, &restored_sender, event("foreign-task", &session)).is_err());
    assert_eq!(
        dispatch(cx, &restored_sender, event(task_id, &session)).unwrap()["waiting_for_input"],
        true
    );
    restored.update(cx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn golive10_startup_bottom_terminals_and_cli_restore(cx: &mut TestAppContext) {
    if child("golive10_startup_bottom_terminals_and_cli_restore") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    let nested = git.repo.join("placeholder-nested");
    std::fs::create_dir(&nested).unwrap();
    golive10_workspace(
        &fixture,
        &git.repo,
        json!([{"dir":nested,"startup_command":"codex resume placeholder-saved","session_uid":"saved-cli"}]),
        json!([{"dir":git.repo},{"dir":git.outside}]),
    );
    let (host, _) = start(&fixture, cx);
    let specs = Rc::new(RefCell::new(Vec::new()));
    let captured = specs.clone();
    host.update(cx, |host, _| {
        host.terminal_spec_hook = Some(Box::new(move |spec| {
            captured.borrow_mut().push(spec.clone())
        }))
    });
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    wait_until(vcx, |_| fake.file("ready", 1).exists());
    assert!(
        fake.text("args", 1)
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|a| a == ["resume", "placeholder-saved"])
    );
    shell.read_with(vcx, |shell, cx| {
        let state = shell.active(cx);
        let cli = state
            .panes
            .values()
            .find(|pane| {
                pane.read(cx)
                    .session
                    .as_ref()
                    .is_some_and(|t| t.session_uid == "saved-cli")
            })
            .unwrap();
        assert_eq!(cli.read(cx).terminal_cwd.as_deref(), Some(nested.as_path()));
        assert_eq!(crate::bottom_pane_ids(state, cx).len(), 2);
        assert!(shell.run_tabs.is_empty());
        assert_eq!(
            shell
                .live_workspace_bottom_terminals(state.runtime_uid, cx)
                .iter()
                .map(|t| t.dir.as_str())
                .collect::<Vec<_>>(),
            [git.repo.to_str().unwrap(), git.outside.to_str().unwrap()]
        );
    });
    for spec in specs
        .borrow()
        .iter()
        .filter(|s| !s.env.contains_key("GITTERM_STARTUP_CMD"))
    {
        assert!(!spec.env["GITTERM_V6_TASK_MCP_URL"].contains("caller="));
        assert!(spec.env["GITTERM_V6_BROWSER_MCP_URL"].contains("caller="));
    }
    let before = specs.borrow().len();
    shell.update_in(vcx, |shell, window, cx| {
        shell.load_active_iced_history(window, cx)
    });
    assert_eq!(
        specs.borrow().len(),
        before,
        "mount once, including bottom panes"
    );
    shell.update_in(vcx, |shell, window, cx| {
        shell.run_repo(
            gitterm_core::workspace_config::RepoKey::Local(git.repo.clone()),
            window,
            cx,
        );
        let state = shell.active(cx);
        assert_eq!(shell.run_tabs.len(), 1);
        assert_eq!(crate::bottom_pane_ids(state, cx).len(), 3);
        assert_eq!(
            shell
                .live_workspace_bottom_terminals(state.runtime_uid, cx)
                .len(),
            2
        );
        let run = shell.run_tabs.keys().next().unwrap();
        assert!(
            !state
                .store
                .terminals
                .as_ref()
                .unwrap()
                .iter()
                .any(|t| crate::session_pane_id(crate::data::SessionId(t.id)) == *run)
        );
        state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
        let mut persisted = crate::layout::LayoutStore::new(state.store.path.clone());
        let terminals = persisted.load_terminals().unwrap().unwrap();
        assert_eq!(terminals.len(), 2);
        assert!(terminals.iter().all(|t| t.bottom));
    });
    let layout_cli = crate::layout::TerminalSession {
        id: 17,
        cwd: git.repo.clone(),
        startup_command: Some("codex resume placeholder-layout".into()),
        task_id: None,
        task_session_id: None,
        session_uid: Some("layout-caller".into()),
        chat_session_id: Some("placeholder-layout".into()),
        bottom: false,
    };
    let layout_restored = host
        .update(vcx, |host, cx| {
            host.restore_terminal("Placeholder", &layout_cli, cx)
        })
        .expect("non-task layout CLI restore");
    assert_eq!(layout_restored.target.session_uid, "layout-caller");
    wait_until(vcx, |_| fake.file("ready", 2).exists());
    assert!(
        fake.text("args", 2)
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|a| a == ["resume", "placeholder-layout"])
    );
    host.update(vcx, |host, cx| host.quit(cx));
}
