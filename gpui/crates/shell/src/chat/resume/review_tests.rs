use super::*;
use gitterm_core::{chat::types::HarnessCommand, workspace_config::WorkspacesFile};

// Each regression runs in a subprocess with synthetic HOME, provider and state roots.
#[test]
fn review_findings_in_isolated_process() {
    let dir = tempfile::tempdir_in("/private/tmp").unwrap();
    for name in ["home", "provider", "config", "bin"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    super::super::tests::script(
        &dir.path().join("bin"),
        "claude",
        include_str!("review_fake.py"),
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "chat::resume::review_tests::finding_",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env("HOME", dir.path().join("home"))
        .env("GITTERM_V6_CONFIG_DIR", dir.path().join("config"))
        .env("CLAUDE_CONFIG_DIR", dir.path().join("provider"))
        .env("PATH", dir.path().join("bin"))
        .env("GITTERM_RESUME_REVIEW_ROOT", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn root() -> Option<PathBuf> {
    std::env::var_os("GITTERM_RESUME_REVIEW_ROOT").map(PathBuf::from)
}
fn prepare(
    cx: &mut gpui::TestAppContext,
    name: &str,
) -> Option<(
    PathBuf,
    Arc<gitterm_core::preview::Preview>,
    ServiceContext,
    WorkspaceConfig,
)> {
    cx.background_executor.allow_parking();
    let root = root()?.join(name);
    std::fs::create_dir_all(&root).unwrap();
    cx.update(gpui_component::init);
    let (_, preview) = super::super::tests::fixture();
    let services = ServiceContext::production(&root, name).unwrap();
    cx.update(|cx| policy::install_live(services.clone(), cx).unwrap());
    let workspace = super::tests::workspace(&root);
    std::fs::write(
        root.join("workspaces.json"),
        serde_json::to_vec(
            &serde_json::json!({"active_workspace":0,"workspaces":[workspace.clone()]}),
        )
        .unwrap(),
    )
    .unwrap();
    Some((root, preview, services, workspace))
}
fn history(id: &str) -> PathBuf {
    let projects = root().unwrap().join("provider/projects");
    std::fs::create_dir_all(projects.join("synthetic")).unwrap();
    std::fs::write(
        projects.join("synthetic").join(format!("{id}.jsonl")),
        "{\"type\":\"user\",\"message\":{\"content\":\"first turn\"}}\n",
    )
    .unwrap();
    projects
}
fn runtime(
    pane: &mut ChatPane,
    services: &ServiceContext,
) -> (
    OwnedRuntime,
    tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
) {
    let settings = pane
        .deferred_live
        .as_ref()
        .unwrap()
        .settings
        .borrow()
        .clone();
    let mut config = live_config(pane.saved.cwd.clone(), &settings, vec![], false);
    config.resume = pane.saved.session_id.clone();
    let claim = match &pane.live_claim {
        Some(claim) => claim.clone(),
        None => Arc::new(provider::ConversationClaim::claim(services, &pane.saved.uid).unwrap()),
    };
    pane.live_claim = Some(claim.clone());
    let (status, _) = std::sync::mpsc::channel();
    provider::start_live_claimed(
        services,
        config,
        root().unwrap().join("unused-preview"),
        claim,
        provider::LiveInstructions {
            workspace_off: true,
            places: None,
            status,
        },
    )
    .unwrap()
}
fn collect(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
    until: impl Fn(&RuntimeEvent) -> bool,
) -> Vec<RuntimeEvent> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut collected = Vec::new();
            loop {
                let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let done = until(&event);
                collected.push(event);
                if done {
                    return collected;
                }
            }
        })
}
fn replay(
    pane: &mut ChatPane,
    runtime: OwnedRuntime,
    events: Vec<RuntimeEvent>,
    cx: &mut Context<ChatPane>,
) -> tokio::sync::mpsc::UnboundedSender<RuntimeEvent> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    for event in events {
        sender.send(event).unwrap();
    }
    pane.attach_runtime(runtime, receiver, cx);
    sender
}
fn saved(root: &Path) -> WorkspacesFile {
    serde_json::from_slice(&std::fs::read(root.join("workspaces.json")).unwrap()).unwrap()
}

#[gpui::test]
fn finding_f1_launch_directory_matches_iced(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, services, workspace)) = prepare(cx, "cwd") else {
        return;
    };
    let repo = git2::Repository::init(root.join("repo")).unwrap();
    let repo_dir = repo.workdir().unwrap().to_path_buf();
    let browser = repo_dir.join("nested");
    let explicit = root.join("explicit");
    let plain = root.join("plain");
    for dir in [&browser, &explicit, &plain] {
        std::fs::create_dir_all(dir).unwrap();
    }
    for (index, (dir, repo_dir, expected)) in [
        (browser.clone(), Some(explicit.clone()), explicit),
        (browser, None, repo_dir),
        (plain.clone(), None, plain),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("cwd-session-{index}");
        let mut tab = tab(&dir, format!("cwd-caller-{index}"), Some(id.clone()));
        tab.repo_dir = repo_dir.map(|dir| dir.to_string_lossy().into_owned());
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::resume_from(
                RuntimeShutdown::new(),
                preview.clone(),
                workspace.clone(),
                tab,
                Some(history(&id)),
                window,
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            assert_eq!(pane.saved.cwd, expected);
            let (runtime, mut events) = runtime(pane, &services);
            runtime
                .send(HarnessCommand::SendUserMessage("first turn".into()))
                .unwrap();
            collect(&mut events, |event| {
                matches!(
                    event,
                    RuntimeEvent::Harness(HarnessEvent::TurnStarted { .. })
                )
            });
            let launch: serde_json::Value =
                serde_json::from_slice(&std::fs::read(expected.join("launch.json")).unwrap())
                    .unwrap();
            assert_eq!(
                launch["cwd"],
                expected.to_str().unwrap().trim_end_matches('/')
            );
            assert!(
                launch["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == &format!("--resume={id}"))
            );
            pane.attach_runtime(runtime, events, cx);
            pane.close(cx);
            pane.shutdown.drain().unwrap();
        });
        cx.run_until_parked();
    }
}

#[gpui::test]
fn finding_f2_new_and_fresh_tabs_store_provider_directory(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, services, workspace)) = prepare(cx, "fresh") else {
        return;
    };
    let new_tab = tab(&root, "new-caller".into(), None);
    assert_eq!(new_tab.repo_dir.as_deref(), root.to_str());
    let old_tab = tab(
        &root.join("removed-checkout"),
        "old-caller".into(),
        Some("old-id".into()),
    );
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume_from(
            RuntimeShutdown::new(),
            preview,
            workspace,
            old_tab,
            Some(history("old-id")),
            window,
            cx,
        )
        .unwrap()
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        assert!(pane.resume_error.is_some());
        pane.fresh_chat(cx);
        assert_eq!(
            pane.live_tab.as_ref().unwrap().tab.repo_dir.as_deref(),
            root.to_str()
        );
        let (runtime, mut events) = runtime(pane, &services);
        runtime
            .send(HarnessCommand::SendUserMessage("first turn".into()))
            .unwrap();
        let events = collect(&mut events, |event| {
            matches!(
                event,
                RuntimeEvent::Harness(HarnessEvent::TurnStarted { .. })
            )
        });
        replay(pane, runtime, events, cx);
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        pane.shutdown.drain().unwrap();
        let file = saved(&root);
        assert_eq!(
            file.workspaces[0].tabs[0].repo_dir.as_deref(),
            root.to_str()
        );
        assert_ne!(
            file.workspaces[0].tabs[0].session_uid.as_deref(),
            Some("old-caller")
        );
        pane.close(cx);
        pane.shutdown.drain().unwrap();
    });
}

#[test]
fn finding_f3_nested_unknown_settings_follow_iced_typed_save() {
    let Some(root) = root() else {
        return;
    };
    let root = root.join("unknowns");
    std::fs::create_dir(&root).unwrap();
    let workspace = super::tests::workspace(&root);
    let document = serde_json::json!({"active_workspace":0,"workspaces":[{"name":workspace.name,"dir":workspace.dir,"abbrev":"S","color":"blue","tabs":[{"dir":root,"tab_kind":"agent","session_uid":"unknown-caller","chat_session_id":"unknown-id","agent_config":{"backend":"claude","model":"sonnet","future":{"nested":true}},"future_tab":true}]}]});
    let path = root.join("workspaces.json");
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let iced = saved(&root);
    let tab = iced.workspaces[0].tabs[0].clone();
    let iced_config = serde_json::to_value(&tab.agent_config).unwrap();
    assert!(iced_config.get("future").is_none());
    LiveTab::new(
        ServiceContext::production(&root, "unknown-test").unwrap(),
        &workspace,
        tab,
    )
    .persist()
    .unwrap();
    let gpui: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        gpui["workspaces"][0]["tabs"][0]["agent_config"],
        iced_config
    );
    assert_eq!(gpui["workspaces"][0]["tabs"][0]["future_tab"], true);
}

#[gpui::test]
fn finding_f4_acknowledged_settings_survive_restart(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, services, workspace)) = prepare(cx, "settings") else {
        return;
    };
    let tab = tab(&root, "settings-caller".into(), Some("settings-id".into()));
    LiveTab::new(services.clone(), &workspace, tab.clone())
        .persist()
        .unwrap();
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume_from(
            RuntimeShutdown::new(),
            preview.clone(),
            workspace.clone(),
            tab,
            Some(history("settings-id")),
            window,
            cx,
        )
        .unwrap()
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        let (runtime, mut events) = runtime(pane, &services);
        // An existing saved session must save acknowledgements even before TurnStarted.
        let mut collected = collect(&mut events, |event| matches!(event, RuntimeEvent::Harness(HarnessEvent::Ready { .. })));
        for (command, changed) in [(HarnessCommand::SetModel("opus".into()), "model"), (HarnessCommand::SetPermissionMode("plan".into()), "mode"), (HarnessCommand::SetEffort(Some("low".into())), "effort")] {
            runtime.send(command).unwrap();
            collected.extend(collect(&mut events, |event| matches!((changed, event), ("model", RuntimeEvent::Harness(HarnessEvent::ModelChanged(value))) if value == "opus") || matches!((changed, event), ("mode", RuntimeEvent::Harness(HarnessEvent::PermissionModeChanged(value))) if value == "plan") || matches!((changed, event), ("effort", RuntimeEvent::Harness(HarnessEvent::EffortChanged { effort: Some(value), .. })) if value == "low")));
        }
        runtime.send(HarnessCommand::SetModel("rejected".into())).unwrap();
        collected.extend(collect(&mut events, |event| matches!(event, RuntimeEvent::Harness(HarnessEvent::Error(_)))));
        replay(pane, runtime, collected, cx);
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        pane.shutdown.drain().unwrap();
        let file = saved(&root);
        let expected = AgentBackendConfig::Claude {
            model: "opus".into(),
            permission_mode: Some("plan".into()),
            effort: Some("low".into()),
        };
        assert_eq!(
            serde_json::to_value(&file.workspaces[0].tabs[0].agent_config).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert!(pane.resume_error.is_none());
        pane.close(cx);
        pane.shutdown.drain().unwrap();
    });
    let file = saved(&root);
    let (restart, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume_from(
            RuntimeShutdown::new(),
            preview,
            workspace,
            file.workspaces[0].tabs[0].clone(),
            Some(history("settings-id")),
            window,
            cx,
        )
        .unwrap()
    });
    cx.run_until_parked();
    restart.update(cx, |pane, cx| {
        let (runtime, mut events) = runtime(pane, &services);
        collect(&mut events, |event| {
            matches!(event, RuntimeEvent::Harness(HarnessEvent::Ready { .. }))
        });
        let launch: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("launch.json")).unwrap()).unwrap();
        let args: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
        for pair in [
            ["--model", "opus"],
            ["--permission-mode", "plan"],
            ["--effort", "low"],
        ] {
            assert!(args.windows(2).any(|args| args == pair));
        }
        pane.attach_runtime(runtime, events, cx);
        pane.close(cx);
        pane.shutdown.drain().unwrap();
    });
}

#[gpui::test]
fn finding_f5_recoverable_error_and_exit_before_turn(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, services, workspace)) = prepare(cx, "errors") else {
        return;
    };
    let tab = tab(&root, "errors-caller".into(), Some("errors-id".into()));
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume_from(
            RuntimeShutdown::new(),
            preview,
            workspace,
            tab,
            Some(history("errors-id")),
            window,
            cx,
        )
        .unwrap()
    });
    cx.run_until_parked();
    let mut remaining = None;
    let mut replay_sender = None;
    pane.update(cx, |pane, cx| {
        let (runtime, mut events) = runtime(pane, &services);
        runtime
            .send(HarnessCommand::SetModel("rejected".into()))
            .unwrap();
        let collected = collect(&mut events, |event| {
            matches!(event, RuntimeEvent::Harness(HarnessEvent::Error(_)))
        });
        // Keep this synthetic stream open while the real runtime remains alive.
        replay_sender = Some(replay(pane, runtime, collected, cx));
        remaining = Some(events);
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        assert!(
            pane.resume_error.is_none(),
            "picker rejection poisoned resume"
        );
        assert!(pane.controller.runtime.is_some());
        let runtime = pane.controller.runtime.take().unwrap();
        runtime.send(HarnessCommand::Shutdown).unwrap();
        let events = collect(remaining.as_mut().unwrap(), |event| {
            matches!(
                event,
                RuntimeEvent::Harness(HarnessEvent::ProcessExited { .. })
            )
        });
        replay(pane, runtime, events, cx);
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        assert!(
            pane.resume_error
                .as_deref()
                .unwrap()
                .contains("before the turn started")
        );
        assert!(pane.controller.runtime.is_none());
        pane.fresh_chat(cx);
        assert!(pane.resume_error.is_none());
        assert!(pane.saved.session_id.is_none());
        assert_ne!(pane.saved.uid, "errors-caller");
        pane.shutdown.drain().unwrap();
    });
    drop(replay_sender);
}

#[gpui::test]
fn finding_f6_turn_started_clears_resume_before_exit(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, services, workspace)) = prepare(cx, "started") else {
        return;
    };
    let tab = tab(&root, "started-caller".into(), Some("started-id".into()));
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume_from(
            RuntimeShutdown::new(),
            preview,
            workspace,
            tab,
            Some(history("started-id")),
            window,
            cx,
        )
        .unwrap()
    });
    cx.run_until_parked();
    let mut stream = None;
    pane.update(cx, |pane, cx| {
        let (runtime, mut events) = runtime(pane, &services);
        runtime
            .send(HarnessCommand::SendUserMessage("resume".into()))
            .unwrap();
        let collected = collect(&mut events, |event| {
            matches!(
                event,
                RuntimeEvent::Harness(HarnessEvent::TurnStarted { .. })
            )
        });
        stream = Some(replay(pane, runtime, collected, cx));
        assert!(pane.awaiting_resume);
    });
    cx.run_until_parked();
    pane.update(cx, |pane, cx| {
        assert!(!pane.awaiting_resume);
        assert!(pane.resume_error.is_none());
        assert_eq!(pane.persisted_id.as_deref(), Some("started-id"));
        pane.receive_runtime_event(
            &pane.controller.source.clone(),
            RuntimeEvent::Harness(HarnessEvent::ProcessExited { code: Some(0) }),
            cx,
        );
        assert!(pane.resume_error.is_none());
        assert!(pane.controller.runtime.is_none());
        pane.shutdown.drain().unwrap();
        assert_eq!(
            saved(&root).workspaces[0].tabs[0]
                .chat_session_id
                .as_deref(),
            Some("started-id")
        );
    });
    drop(stream);
}
