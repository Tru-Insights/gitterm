use super::*;
use gitterm_core::{
    harness::claude::{ClaudeMcpServer, ClaudeSessionConfig},
    preview::service::ServiceContext,
};
use std::path::Path;

fn config(cwd: &Path) -> ClaudeSessionConfig {
    ClaudeSessionConfig {
        cwd: cwd.into(), model: Some("sonnet".into()), permission_mode: "acceptEdits".into(),
        effort: Some("high".into()), resume: None, wire_log_dir: None, snapshot_facts_dir: None,
        mcp_servers: ["tasks", "browser"].into_iter().map(|name| ClaudeMcpServer {
            config: serde_json::json!({"mcpServers": {name: {"type":"http", "url":format!("http://127.0.0.1:1/mcp?caller=fixed-{name}")}}}),
            allowed_tools: vec![format!("mcp__{name}__read")], env: vec![(format!("TEST_{name}"), name.into())], public_env: vec![],
        }).collect(),
        chrome: false, guidelines_file: None, guidelines_content: None,
    }
}

#[gpui::test]
fn policy_defaults_to_preview_and_refuses_preview_authority(cx: &mut gpui::TestAppContext) {
    let (dir, preview) = tests::fixture();
    cx.update(|cx| {
        assert!(matches!(
            policy::current(preview.clone(), cx),
            policy::ChatLaunchPolicy::Preview(_)
        ));
        assert!(
            policy::install_live(ServiceContext::preview(preview.clone()).unwrap(), cx).is_err()
        );
        assert!(matches!(
            policy::current(preview.clone(), cx),
            policy::ChatLaunchPolicy::Preview(_)
        ));
        let services = ServiceContext::production(&dir.path().join("live"), "live-test").unwrap();
        policy::install_live(services.clone(), cx).unwrap();
        let policy::ChatLaunchPolicy::Live {
            services: installed,
        } = policy::current(preview, cx)
        else {
            panic!("not live")
        };
        assert_eq!(installed.task_mcp().token(), services.task_mcp().token());
        assert!(policy::install_live(services, cx).is_err());
    });
}

#[gpui::test]
fn live_pane_defers_and_missing_host_rejects_first_submission(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let (dir, preview) = tests::fixture();
    let services = ServiceContext::production(&dir.path().join("live"), "live-test").unwrap();
    cx.update(|cx| policy::install_live(services.clone(), cx).unwrap());
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::new(
            RuntimeShutdown::new(),
            preview.clone(),
            dir.path().into(),
            window,
            cx,
        )
    });
    pane.read_with(cx, |pane, _| {
        assert!(pane.controller.live);
        assert!(pane.controller.runtime.is_none());
        let deferred = pane.deferred_live.as_ref().unwrap();
        assert_eq!(deferred.caller, pane.target().session_uid);
        assert_eq!(
            *deferred.settings.borrow(),
            gitterm_gpui_chat::composer::LaunchSettings::default()
        );
    });
    // Invoke from inside the borrowed view, as a real composer key event does.
    let view = pane.read_with(cx, |pane, _| pane.view.clone());
    let callback = ChatPane::intent_callback(pane.downgrade());
    view.update(cx, |_, cx| {
        let receipt = callback(ChatIntent::Send { text: "first turn".into(), images: vec![] }, cx);
        assert!(matches!(receipt.state, gitterm_gpui_chat::adapter::ReceiptState::Rejected(ref error) if error.contains("host") && error.contains("unavailable")));
    });
    cx.run_until_parked();
    pane.read_with(cx, |pane, _| {
        assert!(pane.controller.runtime.is_none());
        assert!(pane.deferred_live.is_some());
        assert!(pane.saved.records.is_empty());
    });
    assert!(
        !preview.root().exists(),
        "Live never prepares Preview provider state"
    );
}

#[test]
fn launch_acknowledgements_match_chips_before_provider_ready() {
    let mut controller = Controller::new();
    let settings = gitterm_gpui_chat::composer::LaunchSettings {
        model: "opus".into(),
        permission_mode: "plan".into(),
        effort: Some("max".into()),
    };
    let config = live_config(PathBuf::from("/workspace"), &settings, vec![], false);
    assert_eq!(config.model.as_deref(), Some("opus"));
    assert_eq!(config.permission_mode, "plan");
    assert_eq!(config.effort.as_deref(), Some("max"));
    let defaults = live_config(
        PathBuf::from("/workspace"),
        &Default::default(),
        vec![],
        false,
    );
    assert!(defaults.model.is_none());
    assert!(!defaults.args().iter().any(|arg| arg == "--model"));
    controller.seed_launch(&settings);
    let confirmed = &controller.adapter.state().acknowledged;
    assert_eq!(confirmed.model.as_deref(), Some("opus"));
    assert_eq!(confirmed.mode.as_deref(), Some("plan"));
    assert_eq!(
        confirmed.effort.as_ref().unwrap().effort.as_deref(),
        Some("max")
    );
    assert!(!controller.ready);
}

// A separate test process owns HOME/PATH. No global environment mutation in
// this parallel test binary, and no possible fallback to an installed Claude.
#[test]
fn live_fake_child_matches_iced_and_allows_simultaneous_conversations() {
    let dir = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = dir.path();
    for path in [
        "home",
        "provider",
        "config",
        "bin",
        "folder/nested",
        "worktree",
    ] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    // Real linked worktree, not an independent clone.
    for args in [
        vec!["init", "-q", "repo"],
        vec![
            "-C",
            "repo",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
            "--allow-empty",
        ],
        vec![
            "-C",
            "repo",
            "worktree",
            "add",
            "-q",
            "../worktree",
            "-b",
            "linked",
        ],
    ] {
        assert!(
            std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(root)
                .env_clear()
                .env("HOME", root.join("home"))
                .env("PATH", "/usr/bin:/bin")
                .status()
                .unwrap()
                .success()
        );
    }
    tests::script(&root.join("bin"), "claude", include_str!("live_fake.py"));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "chat::live_tests::live_process_child",
            "--nocapture",
        ])
        .env_clear()
        .env("GITTERM_LIVE_TEST_ROOT", root)
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", root.join("provider"))
        .env("GITTERM_V6_CONFIG_DIR", root.join("config"))
        .env("PATH", root.join("bin"))
        .env("TMPDIR", root)
        .env("CLAUDECODE", "parent")
        .env("CLAUDE_CODE_PARENT", "parent")
        .env("CLAUDE_PID", "1")
        .env("NODE_OPTIONS", "parent")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn live_process_child() {
    let Some(root) = std::env::var_os("GITTERM_LIVE_TEST_ROOT").map(PathBuf::from) else {
        return;
    };
    let services = ServiceContext::production(&root.join("config"), "live-test").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut running = Vec::new();
        for (index, folder) in ["folder", "folder/nested", "worktree"]
            .into_iter()
            .enumerate()
        {
            let cwd = root.join(folder);
            let conversation = format!("conversation-{index}");
            let (status, applied) = std::sync::mpsc::channel();
            let (runtime, mut events) = provider::start_live(
                &services,
                config(&cwd),
                root.join("preview"),
                &conversation,
                provider::LiveInstructions {
                    workspace_off: index != 1,
                    places: None,
                    status,
                },
            )
            .unwrap();
            let (status, _) = std::sync::mpsc::channel();
            assert!(
                provider::start_live(
                    &services,
                    config(&cwd),
                    root.join("preview"),
                    &conversation,
                    provider::LiveInstructions {
                        workspace_off: true,
                        places: None,
                        status
                    }
                )
                .is_err()
            );
            assert!(runtime._lease.is_none());
            runtime
                .send(gitterm_core::chat::types::HarnessCommand::SendUserMessage(
                    "first turn".into(),
                ))
                .unwrap();
            let mut ready = false;
            loop {
                let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    RuntimeEvent::Harness(HarnessEvent::Ready { .. }) => ready = true,
                    RuntimeEvent::Harness(HarnessEvent::TextDelta(text)) => {
                        assert!(ready);
                        assert_eq!(text, "fake reply");
                        break;
                    }
                    RuntimeEvent::Harness(HarnessEvent::Error(error)) => panic!("{error}"),
                    _ => {}
                }
            }
            assert_eq!(applied.recv().unwrap(), index == 1);
            let actual: serde_json::Value =
                serde_json::from_slice(&std::fs::read(cwd.join("launch.json")).unwrap()).unwrap();
            assert_eq!(actual["env"]["HOME"], root.join("home").to_str().unwrap());
            assert_eq!(
                actual["env"]["CLAUDE_CONFIG_DIR"],
                root.join("provider").to_str().unwrap()
            );
            assert_eq!(actual["env"]["CLAUDE_CODE_ENTRYPOINT"], "sdk-ts");
            assert_eq!(actual["env"]["CLAUDE_CODE_SDK_READS_SESSION_STATE"], "1");
            for key in [
                "CLAUDECODE",
                "CLAUDE_CODE_PARENT",
                "CLAUDE_PID",
                "NODE_OPTIONS",
            ] {
                assert!(actual["env"].get(key).is_none());
            }
            let args: Vec<String> = serde_json::from_value(actual["argv"].clone()).unwrap();
            assert!(args.windows(2).any(|pair| pair == ["--model", "sonnet"]));
            assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--permission-mode", "acceptEdits"])
            );
            assert!(!args.iter().any(|arg| arg.contains("restricted")
                || arg == "--safe-mode"
                || arg == "--strict-mcp-config"));
            let servers: Vec<_> = args
                .iter()
                .filter(|arg| arg.starts_with("--mcp-config="))
                .collect();
            assert!(servers[0].contains("tasks") && servers[1].contains("browser"));
            // Same core launch composition as Iced, executed by the same fake
            // child. Use the exact guideline file prepared by the Live path.
            let file = args
                .iter()
                .position(|arg| arg == "--append-system-prompt-file")
                .unwrap();
            let mut expected_config = config(&cwd);
            expected_config.guidelines_file = Some(args[file + 1].clone().into());
            let (mut iced, mut iced_events) = Session::spawn_runtime(move |_| {
                Ok(gitterm_core::harness::local_launch::local_launch(
                    expected_config,
                    gitterm_core::harness::local_launch::resolve_claude_program()?,
                    Box::new(provider::LiveSnapshots),
                ))
            });
            loop {
                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(10), iced_events.recv())
                        .await
                        .unwrap()
                        .unwrap();
                if matches!(event, RuntimeEvent::Harness(HarnessEvent::Ready { .. })) {
                    break;
                }
                if let RuntimeEvent::Harness(HarnessEvent::Error(error)) = event {
                    panic!("{error}");
                }
            }
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(cwd.join("launch.json")).unwrap()).unwrap();
            assert_eq!(actual, expected);
            iced.send(gitterm_core::chat::types::HarnessCommand::Shutdown)
                .unwrap();
            iced.completed().await.unwrap();
            running.push(runtime);
        }
        assert_eq!(running.len(), 3);
        for mut runtime in running {
            runtime
                .send(gitterm_core::chat::types::HarnessCommand::Shutdown)
                .unwrap();
            runtime.completed().await.unwrap();
        }
        resume_round_trip(&services, &root).await;
        assert!(!root.join("preview").exists());
        assert!(!root.join("config/chat-facts").exists());
        assert!(!root.join("config/turn-snapshots").exists());
    });
}

#[test]
fn live_host_must_share_production_authority_and_namespace() {
    let (dir, preview) = tests::fixture();
    let services = ServiceContext::production(&dir.path().join("live"), "test").unwrap();
    assert!(validate_live_host(&services, &services.clone()).is_ok());
    assert!(validate_live_host(&services, &ServiceContext::preview(preview).unwrap()).is_err());
    // Even an identical root and instance label is not the installed host.
    let unrelated = ServiceContext::production(services.state_root(), "test").unwrap();
    assert!(validate_live_host(&services, &unrelated).is_err());
}

#[test]
fn live_first_turn_is_admitted_while_initialization_is_pending() {
    let mut controller = Controller::new();
    controller.live = true;
    let (release, gate) = std::sync::mpsc::sync_channel(0);
    let (runtime, _events) = Session::spawn_runtime(move |_| {
        gate.recv().unwrap();
        Err("fixture initialization stopped".into())
    });
    controller.runtime = Some(runtime.into());
    let (receipt, _) = controller.dispatch(ChatIntent::Send {
        text: "first turn".into(),
        images: vec![],
    });
    assert_eq!(
        receipt.state,
        gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted
    );
    assert!(!controller.ready);
    release.send(()).unwrap();
    let mut runtime = controller.runtime.take().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(runtime.completed())
        .unwrap();
}

async fn resume_round_trip(services: &ServiceContext, root: &Path) {
    use gitterm_core::{chat::types::HarnessCommand, workspace_config::WorkspacesFile};
    let cwd = root.join("roundtrip");
    std::fs::create_dir(&cwd).unwrap();
    let config_file = services.state_root().join("workspaces.json");
    std::fs::write(&config_file, serde_json::to_vec(&serde_json::json!({
        "active_workspace":0,"workspaces":[{"name":"Roundtrip","abbrev":"R","color":"blue","dir":cwd,"tabs":[]}]
    })).unwrap()).unwrap();
    // Iced creates a session through core's normal launcher and the fake child.
    let iced_config = config(&cwd);
    let (mut iced, mut events) = Session::spawn_runtime(move |_| {
        Ok(gitterm_core::harness::local_launch::local_launch(
            iced_config,
            gitterm_core::harness::local_launch::resolve_claude_program()?,
            Box::new(provider::LiveSnapshots),
        ))
    });
    iced.send(HarnessCommand::SendUserMessage("first turn".into()))
        .unwrap();
    let mut confirmed = None;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            RuntimeEvent::Harness(HarnessEvent::TurnStarted {
                session_id: Some(id),
                ..
            }) => confirmed = Some(id),
            RuntimeEvent::Harness(HarnessEvent::TextDelta(_)) => break,
            RuntimeEvent::Harness(HarnessEvent::Error(error)) => panic!("{error}"),
            _ => {}
        }
    }
    let id = confirmed.unwrap();
    iced.send(HarnessCommand::Shutdown).unwrap();
    iced.completed().await.unwrap();
    let file: WorkspacesFile =
        serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
    let mut tab = resume::tab(&cwd, "roundtrip-caller".into(), Some(id.clone()));
    tab.agent_config = Some(gitterm_core::chat::types::AgentBackendConfig::Claude {
        model: "sonnet".into(),
        permission_mode: Some("acceptEdits".into()),
        effort: Some("high".into()),
    });
    resume::LiveTab::new(services.clone(), &file.workspaces[0], tab)
        .persist()
        .unwrap();
    for restart in 0..2 {
        // The same shared DTO is what GPUI restart and Iced's next launch read.
        let saved: WorkspacesFile =
            serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
        let tab = &saved.workspaces[0].tabs[0];
        assert_eq!(tab.chat_session_id.as_deref(), Some(id.as_str()));
        let history = gitterm_core::chat::history::load_claude_session(
            &root.join("provider/projects"),
            &id,
            &cwd,
        )
        .unwrap();
        assert_eq!(history.iter().filter(|event| matches!(event, gitterm_core::chat::history::HistoryEvent::Other(value) if value["type"] == "user_prompt")).count(), restart + 1);
        let claim =
            Arc::new(provider::ConversationClaim::claim(services, "roundtrip-caller").unwrap());
        claim.provider_id(&id).unwrap();
        let competing =
            provider::ConversationClaim::claim(services, "other-roundtrip-caller").unwrap();
        assert!(competing.provider_id(&id).is_err());
        drop(competing);
        let mut launch = config(Path::new(&tab.dir));
        launch.resume = tab.chat_session_id.clone();
        let (status, _) = std::sync::mpsc::channel();
        let (mut runtime, mut events) = provider::start_live_claimed(
            services,
            launch,
            root.join("preview"),
            claim.clone(),
            provider::LiveInstructions {
                workspace_off: true,
                places: None,
                status,
            },
        )
        .unwrap();
        drop(claim);
        runtime
            .send(HarnessCommand::SendUserMessage("first turn".into()))
            .unwrap();
        let mut ready = false;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                RuntimeEvent::Harness(HarnessEvent::Ready { session_id, .. }) => {
                    assert_eq!(session_id.as_deref(), Some(id.as_str()));
                    ready = true;
                }
                RuntimeEvent::Harness(HarnessEvent::TurnStarted { session_id, .. }) => {
                    assert_eq!(session_id.as_deref(), Some(id.as_str()))
                }
                RuntimeEvent::Harness(HarnessEvent::TextDelta(_)) => {
                    assert!(ready);
                    break;
                }
                RuntimeEvent::Harness(HarnessEvent::Error(error)) => panic!("{error}"),
                _ => {}
            }
        }
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(cwd.join("launch.json")).unwrap()).unwrap();
        let argv: Vec<String> = serde_json::from_value(actual["argv"].clone()).unwrap();
        assert!(argv.contains(&format!("--resume={id}")));
        assert!(argv.windows(2).any(|pair| pair == ["--model", "sonnet"]));
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--permission-mode", "acceptEdits"])
        );
        assert!(argv.windows(2).any(|pair| pair == ["--effort", "high"]));
        assert_eq!(
            actual["env"]["CLAUDE_CONFIG_DIR"],
            root.join("provider").to_str().unwrap()
        );
        resume::LiveTab::new(services.clone(), &saved.workspaces[0], tab.clone())
            .persist()
            .unwrap();
        runtime.send(HarnessCommand::Shutdown).unwrap();
        runtime.completed().await.unwrap();
        // Runtime still owns the provider id until its completion handle drops.
        let contender =
            provider::ConversationClaim::claim(services, "post-completion-caller").unwrap();
        assert!(contender.provider_id(&id).is_err());
        drop(runtime);
        contender.provider_id(&id).unwrap();
    }
}
