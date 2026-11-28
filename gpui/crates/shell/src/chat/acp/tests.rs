use super::*;
use gitterm_core::{
    chat::types::AcpAgent,
    workspace_config::{WorkspaceConfig, WorkspaceTabConfig},
};
use std::{
    cell::RefCell,
    path::Path,
    time::{Duration, Instant},
};
std::thread_local! {
    pub(super) static PROGRAM: RefCell<Option<std::result::Result<PathBuf, String>>> = const { RefCell::new(None) };
    pub(super) static SERVERS: RefCell<Option<Vec<AcpMcpServer>>> = const { RefCell::new(None) };
}

#[test]
fn acp_live_regressions_in_isolated_process() {
    let dir = tempfile::tempdir_in("/private/tmp").unwrap();
    for name in ["home", "bin", "state", "temp"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    super::super::tests::script(&dir.path().join("bin"), "agy", include_str!("fake.py"));
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["chat::acp::tests::case_", "--nocapture", "--test-threads=1"])
        .env_clear()
        .env("HOME", dir.path().join("home"))
        .env("PATH", dir.path().join("bin"))
        .env("TMPDIR", dir.path().join("temp"))
        .env("GITTERM_V6_CONFIG_DIR", dir.path().join("state"))
        .env("GITTERM_ACP_FIXTURE_ROOT", dir.path())
        .env("PLACEHOLDER_INHERITED", "preserved")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn prepare(
    cx: &mut gpui::TestAppContext,
    name: &str,
) -> Option<(
    PathBuf,
    Arc<gitterm_core::preview::Preview>,
    WorkspaceConfig,
)> {
    let root = PathBuf::from(std::env::var_os("GITTERM_ACP_FIXTURE_ROOT")?).join(name);
    std::fs::create_dir_all(&root).unwrap();
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (_, preview) = super::super::tests::fixture();
    let state = root.join("state");
    std::fs::create_dir(&state).unwrap();
    let services = ServiceContext::production(&state, name).unwrap();
    let workspace: WorkspaceConfig = serde_json::from_value(serde_json::json!({"name":"Placeholder", "abbrev":"P", "color":"blue", "dir":root, "tabs":[]})).unwrap();
    std::fs::write(
        state.join("workspaces.json"),
        serde_json::to_vec(
            &serde_json::json!({"active_workspace":0, "workspaces":[workspace.clone()]}),
        )
        .unwrap(),
    )
    .unwrap();
    cx.update(|cx| {
        policy::install_live(services, cx).unwrap();
        cx.set_global(crate::workspace_config::LiveWorkspaces(
            gitterm_core::desktop_store::DesktopStore::open(&state).unwrap(),
        ));
    });
    SERVERS.with(|servers| {
        *servers.borrow_mut() = Some(vec![AcpMcpServer {
            name: "gitterm_tasks".into(),
            url: "http://127.0.0.1:1/mcp?caller=placeholder-caller".into(),
            headers: vec![("Authorization".into(), "Bearer placeholder-token".into())],
        }])
    });
    Some((root, preview, workspace))
}
fn saved(root: &Path, id: Option<&str>) -> WorkspaceTabConfig {
    let mut tab = resume::tab(root, "placeholder-caller".into(), id.map(str::to_owned));
    tab.agent_config = Some(AgentBackendConfig::Acp {
        agent: AcpAgent::Antigravity,
        model: "placeholder-retired-model".into(),
        mode: Some("default".into()),
        effort: None,
    });
    tab
}
fn wait(
    pane: &Entity<ChatPane>,
    cx: &mut gpui::VisualTestContext,
    done: impl Fn(&ChatPane) -> bool,
) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if pane.read_with(cx, |pane, _| done(pane)) {
            return;
        }
        assert!(
            Instant::now() < until,
            "ACP fixture timed out: {:?}",
            pane.read_with(cx, |pane, _| pane.resume_error.clone())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn wire(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(root.join("wire.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[gpui::test]
fn case_saved_acp_restores_and_resumes_recorded_session(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, workspace)) = prepare(cx, "restore") else {
        return;
    };
    let shutdown = RuntimeShutdown::new();
    let tab: WorkspaceTabConfig = serde_json::from_value(
        serde_json::to_value(saved(&root, Some("placeholder-recorded-session"))).unwrap(),
    )
    .unwrap();
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume(shutdown.clone(), preview, workspace, tab, None, window, cx).unwrap()
    });
    pane.update(cx, |pane, cx| pane.load_acp_history(cx));
    wait(&pane, cx, |pane| pane.controller.ready);
    pane.read_with(cx, |pane, _| {
        assert_eq!(pane.saved.session_id.as_deref(), Some("placeholder-recorded-session"));
        assert_eq!(pane.target().session_uid, "placeholder-caller");
        assert!(pane.saved.records.iter().any(|record| matches!(&record.entry, Entry::UserPrompt(text) if text == "placeholder saved prompt")));
        assert!(pane.saved.records.iter().any(|record| matches!(&record.entry, Entry::Harness(HarnessEvent::TextDelta(text)) if text == "placeholder saved reply")));
        assert!(!pane.history_loading);
        assert!(pane.resume_error.is_none());
        let other = provider::ConversationClaim::claim(
            &pane.deferred_live.as_ref().unwrap().services,
            "placeholder-other-caller",
        ).unwrap();
        let id = "placeholder-recorded-session";
        assert!(other.acp_id(AcpAgent::Antigravity, id).is_err());
        other.provider_id(id).unwrap();
        other.remote_provider_id("placeholder-remote", id).unwrap();
    });
    std::fs::write(root.join("permission"), "").unwrap();
    pane.update(cx, |pane, cx| {
        pane.dispatch(
            ChatIntent::Send {
                text: "placeholder next turn".into(),
                images: vec![],
            },
            cx,
        );
    });
    wait(&pane, cx, |pane| {
        pane.controller.adapter.state().has_pending_requests()
    });
    pane.update(cx, |pane, cx| {
        let request = pane.controller.adapter.state().pending_requests[0].clone();
        let identity = pane
            .controller
            .adapter
            .state()
            .request_identity(&pane.controller.source, &[], &request)
            .unwrap();
        let suggestion = pane
            .saved
            .records
            .iter()
            .find_map(|record| match &record.entry {
                Entry::Harness(HarnessEvent::RuntimeRequest {
                    kind:
                        gitterm_core::chat::types::RuntimeRequestKind::Permission {
                            suggestions, ..
                        },
                    ..
                }) => suggestions.first().cloned(),
                _ => None,
            })
            .unwrap();
        let receipt = pane.dispatch(
            ChatIntent::Answer {
                identity,
                answer: Box::new(ChatIntent::Permission {
                    request_id: request,
                    decision: gitterm_gpui_chat::PermissionDecision::AllowAlways { suggestion },
                }),
            },
            cx,
        );
        assert!(matches!(
            receipt.state,
            gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted
        ));
    });
    wait(&pane, cx, |pane| {
        pane.saved.records.iter().any(|record| matches!(&record.entry, Entry::Harness(HarnessEvent::TextDelta(text)) if text == "placeholder continued reply"))
    });
    let frames = wire(&root);
    assert!(frames.iter().any(|f| f["method"] == "session/load"
        && f["params"]["sessionId"] == "placeholder-recorded-session"));
    assert!(!frames.iter().any(|f| f["method"] == "session/new"));
    assert!(frames.iter().any(|f| f["method"] == "session/set_model"
        && f["params"]["modelId"] == "placeholder-retired-model"));
    assert!(frames.iter().any(|f| f["method"] == "session/prompt"
        && f["params"]["sessionId"] == "placeholder-recorded-session"));
    pane.update(cx, |pane, cx| pane.close(cx));
    shutdown.drain().unwrap();
}
#[gpui::test]
fn case_new_acp_uses_iced_argv_env_and_chat_config(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, workspace)) = prepare(cx, "new") else {
        return;
    };
    let defaults = gitterm_core::settings::ChatDefaults {
        antigravity_model: " placeholder-model ".into(),
        ..Default::default()
    };
    assert!(
        live_acp_catalog(&defaults)
            .iter()
            .any(|chat| chat.id == AcpAgent::Antigravity && chat.model == "placeholder-model")
    );
    let mut tab = saved(&root, None);
    tab.agent_config = Some(acp_launch::new_chat_config(
        &defaults,
        AcpAgent::Antigravity,
    ));
    let shutdown = RuntimeShutdown::new();
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume(shutdown.clone(), preview, workspace, tab, None, window, cx).unwrap()
    });
    assert!(pane.read_with(cx, |pane, _| pane.controller.runtime.is_none()));
    pane.update(cx, |pane, cx| {
        pane.dispatch(
            ChatIntent::Send {
                text: "placeholder first turn".into(),
                images: vec![],
            },
            cx,
        );
    });
    wait(&pane, cx, |pane| pane.controller.ready);
    let child: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("child.json")).unwrap()).unwrap();
    assert_eq!(child["argv"], serde_json::json!(["acp"]));
    assert_eq!(child["cwd"], root.to_string_lossy().as_ref());
    assert_eq!(child["env"]["PLACEHOLDER_INHERITED"], "preserved");
    assert_eq!(child["env"]["HOME"], std::env::var("HOME").unwrap());
    assert_eq!(
        child["env"]["PATH"],
        gitterm_core::harness::local_launch::child_path()
            .to_string_lossy()
            .as_ref()
    );
    assert!(child["env"].get("OPENCODE_CONFIG_CONTENT").is_none());
    assert!(child["env"].get("CLAUDE_CODE_ENTRYPOINT").is_none());
    // Apple's Python launcher adds SDK/locale variables itself. Compare the
    // complete environment to the same interpreter launched directly with
    // Iced's PATH, so these additions cannot hide transport overrides.
    let baseline = std::process::Command::new("/usr/bin/python3")
        .args(["-c", "import json,os; print(json.dumps(dict(os.environ)))"])
        .env("PATH", gitterm_core::harness::local_launch::child_path())
        .output()
        .unwrap();
    assert!(baseline.status.success());
    assert_eq!(
        child["env"],
        serde_json::from_slice::<serde_json::Value>(&baseline.stdout).unwrap()
    );
    let frames = wire(&root);
    let new = frames
        .iter()
        .find(|f| f["method"] == "session/new")
        .unwrap();
    assert_eq!(new["params"]["cwd"], root.to_string_lossy().as_ref());
    assert_eq!(new["params"]["mcpServers"][0]["name"], "gitterm-tasks");
    assert!(!frames.iter().any(|f| f["method"] == "session/load"));
    pane.update(cx, |pane, cx| pane.close(cx));
    shutdown.drain().unwrap();
    let document: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_slice(&std::fs::read(root.join("state/workspaces.json")).unwrap())
            .unwrap();
    let tab = &document.workspaces[0].tabs[0];
    assert_eq!(
        tab.chat_session_id.as_deref(),
        Some("placeholder-new-session")
    );
    assert!(matches!(
        tab.agent_config,
        Some(AgentBackendConfig::Acp {
            agent: AcpAgent::Antigravity,
            ..
        })
    ));
}
#[gpui::test]
fn case_missing_acp_reports_iced_message_and_retired_gemini_is_read_only(
    cx: &mut gpui::TestAppContext,
) {
    let Some((root, preview, workspace)) = prepare(cx, "missing") else {
        return;
    };
    let expected = acp_launch::resolve_program_on_path("agy", root.as_os_str()).unwrap_err();
    PROGRAM.with(|program| *program.borrow_mut() = Some(Err(expected.clone())));
    let shutdown = RuntimeShutdown::new();
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume(
            shutdown.clone(),
            preview.clone(),
            workspace.clone(),
            saved(&root, Some("placeholder-missing-session")),
            None,
            window,
            cx,
        )
        .unwrap()
    });
    pane.update(cx, |pane, cx| pane.load_acp_history(cx));
    wait(&pane, cx, |pane| {
        pane.resume_error.is_some() && pane.controller.runtime.is_none()
    });
    pane.read_with(cx, |pane, _| {
        assert_eq!(pane.resume_error.as_deref(), Some(expected.as_str()))
    });
    let mut tab = saved(&root, Some("placeholder-retired-session"));
    tab.session_uid = Some("placeholder-retired-caller".into());
    tab.agent_config = Some(AgentBackendConfig::Acp {
        agent: AcpAgent::Gemini,
        model: "retired-model".into(),
        mode: None,
        effort: None,
    });
    let (retired, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume(shutdown.clone(), preview, workspace, tab, None, window, cx).unwrap()
    });
    retired.read_with(cx, |pane, _| {
        assert!(pane.controller.runtime.is_none());
        assert!(pane.controller.config.read_only_note().is_some());
    });
    shutdown.drain().unwrap();
}

#[gpui::test]
fn case_preview_keeps_acp_resume_and_persistent_permissions_unavailable(
    cx: &mut gpui::TestAppContext,
) {
    let Some(root) = std::env::var_os("GITTERM_ACP_FIXTURE_ROOT").map(PathBuf::from) else {
        return;
    };
    cx.update(gpui_component::init);
    let (_fixture, preview) = super::super::tests::fixture();
    let workspace: WorkspaceConfig = serde_json::from_value(serde_json::json!({"name":"Placeholder", "abbrev":"P", "color":"blue", "dir":root, "tabs":[]})).unwrap();
    let (_pane, _) = cx.add_window_view(|window, cx| {
        let result = ChatPane::resume(RuntimeShutdown::new(), preview.clone(), workspace, saved(&root, Some("placeholder-preview-session")), None, window, cx);
        assert!(matches!(result, Err(error) if error.to_string() == "Resume for this recorded backend is unavailable"));
        let mut pane = ChatPane::history(RuntimeShutdown::new(), SavedChat { uid:"placeholder-preview".into(), cwd:root.clone(), provider:preview.provider(), session_id:None, records:vec![] }, window, cx);
        pane.controller.config = saved(&root, None).agent_config.unwrap();
        let (receipt, _) = pane.controller.dispatch(ChatIntent::Permission { request_id:"placeholder-request".into(), decision:gitterm_gpui_chat::PermissionDecision::AllowAlways { suggestion:serde_json::json!({"type":"acp_option","optionId":"placeholder-always"}) } });
        assert!(matches!(receipt.state, gitterm_gpui_chat::adapter::ReceiptState::Rejected(error) if error == "Allow and remember · not available in preview"));
        pane
    });
}

fn send(pane: &mut ChatPane, text: &str, cx: &mut Context<ChatPane>) -> Receipt {
    pane.dispatch(
        ChatIntent::Send {
            text: text.into(),
            images: vec![],
        },
        cx,
    )
}

#[gpui::test]
fn case_f1_initialize_uses_desktop_client_version(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, workspace)) = prepare(cx, "f1-version") else {
        return;
    };
    let shutdown = RuntimeShutdown::new();
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume(
            shutdown.clone(),
            preview,
            workspace,
            saved(&root, None),
            None,
            window,
            cx,
        )
        .unwrap()
    });
    pane.update(cx, |pane, cx| {
        send(pane, "placeholder version prompt", cx);
    });
    wait(&pane, cx, |pane| pane.controller.ready);
    let desktop: toml::Value =
        toml::from_str(include_str!("../../../../../../Cargo.toml")).unwrap();
    let frames = wire(&root);
    let init = frames
        .iter()
        .find(|frame| frame["method"] == "initialize")
        .unwrap();
    assert_eq!(
        init["params"]["clientInfo"]["version"].as_str(),
        desktop["package"]["version"].as_str()
    );
    pane.update(cx, |pane, cx| pane.close(cx));
    shutdown.drain().unwrap();
}

fn shell<'a>(
    cx: &'a mut gpui::TestAppContext,
    root: &Path,
    workspaces: serde_json::Value,
) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
    let state = root.join("state");
    let document =
        serde_json::from_value(serde_json::json!({"active_workspace":0, "workspaces":workspaces}))
            .unwrap();
    cx.read(|cx| {
        cx.global::<crate::workspace_config::LiveWorkspaces>()
            .0
            .transact_workspaces(move |file| {
                *file = document;
                Ok(())
            })
            .wait()
            .unwrap()
    });
    let mut store = crate::preview::tests::guarded_store(&root.join("preview"));
    let model = crate::workspace_config::load_iced_workspaces_from(&mut store, &state);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        crate::Shell::with_owner(
            Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            window,
            cx,
        )
        .unwrap()
    })
}
fn shell_chat(
    shell: &Entity<crate::Shell>,
    workspace: crate::WorkspaceId,
    index: usize,
    cx: &gpui::App,
) -> (crate::data::SessionId, Entity<ChatPane>) {
    let state = shell.read(cx).workspace(workspace);
    let id = state.sessions[index].id;
    let pane = state.panes[&crate::session_pane_id(id)].read(cx);
    let crate::pane::PaneContent::PreviewChat(chat) = &pane.content else {
        panic!("saved ACP tab was not registered")
    };
    (id, chat.clone())
}
fn launches(root: &Path) -> usize {
    std::fs::read_to_string(root.join("launches"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[gpui::test]
fn case_f2_background_restore_launches_only_selected_acp(cx: &mut gpui::TestAppContext) {
    let Some((root, _, workspace)) = prepare(cx, "f2-lazy") else {
        return;
    };
    let first = root.join("first");
    let second = root.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let mut other = saved(&second, Some("placeholder-second-session"));
    other.session_uid = Some("placeholder-second-caller".into());
    let mut background = workspace.clone();
    background.name = "Placeholder background".into();
    background.dir = first.to_string_lossy().into_owned();
    background.tabs = vec![saved(&first, Some("placeholder-first-session")), other];
    background.done_at = Some("2026-01-01T00:00:00Z".into());
    let (shell, cx) = shell(cx, &root, serde_json::json!([workspace, background]));
    shell.update_in(cx, |shell, window, cx| {
        shell.restore_live_workspace_sessions(crate::WorkspaceId(2), window, cx)
    });
    cx.run_until_parked();
    let (selected, first_chat) = cx.read(|cx| shell_chat(&shell, crate::WorkspaceId(2), 0, cx));
    let (_, second_chat) = cx.read(|cx| shell_chat(&shell, crate::WorkspaceId(2), 1, cx));
    assert!(first_chat.read_with(cx, |pane, _| pane.controller.runtime.is_none()));
    assert!(second_chat.read_with(cx, |pane, _| pane.controller.runtime.is_none()));
    assert_eq!(launches(&first) + launches(&second), 0);
    shell.update_in(cx, |shell, window, cx| {
        shell.reopen_workspace(crate::WorkspaceId(2), window, cx);
        shell.select_workspace_id(crate::WorkspaceId(2), window, cx);
        shell.select_session_id(selected, window, cx);
    });
    wait(&first_chat, cx, |pane| pane.controller.ready);
    assert_eq!(launches(&first), 1);
    assert_eq!(launches(&second), 0);
    shell.update_in(cx, |shell, window, cx| {
        shell.load_active_iced_history(window, cx)
    });
    assert_eq!(launches(&first), 1);
    let shutdown = first_chat.read_with(cx, |pane, _| pane.shutdown.clone());
    first_chat.update(cx, |pane, cx| pane.close(cx));
    shutdown.drain().unwrap();
}

#[gpui::test]
fn case_f3_startup_selection_resumes_saved_acp(cx: &mut gpui::TestAppContext) {
    let Some((root, _, mut workspace)) = prepare(cx, "f3-startup") else {
        return;
    };
    let terminal: WorkspaceTabConfig =
        serde_json::from_value(serde_json::json!({"dir":root})).unwrap();
    workspace.tabs = vec![terminal, saved(&root, Some("placeholder-startup-session"))];
    let (shell, cx) = shell(cx, &root, serde_json::json!([workspace]));
    let selected = shell.read_with(cx, |shell, _| {
        shell.workspace(crate::WorkspaceId(1)).sessions[1].id
    });
    assert_eq!(launches(&root), 0);
    shell.update_in(cx, |shell, window, cx| {
        shell.select_session_id(selected, window, cx)
    });
    let (_, pane) = cx.read(|cx| shell_chat(&shell, crate::WorkspaceId(1), 1, cx));
    wait(&pane, cx, |pane| pane.controller.ready);
    assert_eq!(launches(&root), 1);
    assert!(wire(&root).iter().any(|f| f["method"] == "session/load"
        && f["params"]["sessionId"] == "placeholder-startup-session"));
    assert_eq!(
        pane.read_with(cx, |pane, _| pane.saved.session_id.clone())
            .as_deref(),
        Some("placeholder-startup-session")
    );
    let shutdown = pane.read_with(cx, |pane, _| pane.shutdown.clone());
    pane.update(cx, |pane, cx| pane.close(cx));
    shutdown.drain().unwrap();
}

#[gpui::test]
fn case_f4_resume_fallback_finishes_history_and_accepts_send(cx: &mut gpui::TestAppContext) {
    let Some((fixture, preview, workspace)) = prepare(cx, "f4-fallback") else {
        return;
    };
    for marker in ["fail-load", "no-load"] {
        let root = fixture.join(marker);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(marker), "").unwrap();
        let shutdown = RuntimeShutdown::new();
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = ChatPane::resume(
                shutdown.clone(),
                preview.clone(),
                workspace.clone(),
                saved(&root, Some("placeholder-fallback-session")),
                None,
                window,
                cx,
            )
            .unwrap();
            pane.load_acp_history(cx);
            pane
        });
        wait(&pane, cx, |pane| pane.controller.ready);
        pane.update(cx, |pane, cx| {
            assert!(!pane.history_loading);
            assert!(pane.resume_error.is_none());
            assert_eq!(
                pane.controller.adapter.state().history,
                gitterm_core::chat::types::HistoryLoad::Loaded
            );
            assert!(matches!(
                send(pane, "placeholder fallback prompt", cx).state,
                gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted
            ));
        });
        wait(&pane, cx, |pane| {
            pane.saved.records.iter().any(|r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(text)) if text == "placeholder continued reply"))
        });
        assert!(wire(&root).iter().any(|f| f["method"] == "session/new"));
        assert!(wire(&root).iter().any(|f| f["method"] == "session/prompt"
            && f["params"]["sessionId"] == "placeholder-new-session"));
        pane.update(cx, |pane, cx| pane.close(cx));
        shutdown.drain().unwrap();
    }
}

#[gpui::test]
fn case_f5_send_after_exit_resets_runtime_lifecycle(cx: &mut gpui::TestAppContext) {
    let Some((root, preview, workspace)) = prepare(cx, "f5-replacement") else {
        return;
    };
    std::fs::write(root.join("exit-after-prompt"), "").unwrap();
    let shutdown = RuntimeShutdown::new();
    let (pane, cx) = cx.add_window_view(|window, cx| {
        ChatPane::resume(
            shutdown.clone(),
            preview,
            workspace,
            saved(&root, None),
            None,
            window,
            cx,
        )
        .unwrap()
    });
    pane.update(cx, |pane, cx| {
        assert!(matches!(
            send(pane, "placeholder first prompt", cx).state,
            gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted
        ));
    });
    wait(&pane, cx, |pane| pane.controller.runtime.is_none());
    let source = pane.read_with(cx, |pane, _| pane.controller.source.clone());
    pane.update(cx, |pane, cx| {
        let receipt = send(pane, "placeholder replacement prompt", cx);
        assert!(
            matches!(
                receipt.state,
                gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted
            ),
            "{receipt:?}"
        );
        assert_ne!(pane.controller.source, source);
        // An old receiver cannot terminate the replacement.
        pane.receive_runtime_event(
            &source,
            RuntimeEvent::Harness(HarnessEvent::ProcessExited { code: Some(0) }),
            cx,
        );
        assert!(pane.controller.runtime.is_some());
    });
    wait(&pane, cx, |pane| pane.controller.runtime.is_none());
    pane.update(cx, |pane, cx| pane.close(cx));
    assert_eq!(launches(&root), 2);
    assert_eq!(
        wire(&root)
            .iter()
            .filter(|f| f["method"] == "session/prompt")
            .count(),
        2
    );
    shutdown.drain().unwrap();
}
