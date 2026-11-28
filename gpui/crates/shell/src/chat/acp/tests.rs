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
        assert!(Instant::now() < until, "ACP fixture timed out");
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
        ChatPane::resume(shutdown.clone(), preview, workspace, tab, window, cx).unwrap()
    });
    wait(&pane, cx, |pane| pane.controller.ready);
    pane.read_with(cx, |pane, _| {
        assert_eq!(pane.saved.session_id.as_deref(), Some("placeholder-recorded-session"));
        assert_eq!(pane.target().session_uid, "placeholder-caller");
        assert!(pane.saved.records.iter().any(|record| matches!(&record.entry, Entry::UserPrompt(text) if text == "placeholder saved prompt")));
        assert!(pane.saved.records.iter().any(|record| matches!(&record.entry, Entry::Harness(HarnessEvent::TextDelta(text)) if text == "placeholder saved reply")));
        assert!(!pane.history_loading);
        assert!(pane.resume_error.is_none());
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
    pane.update(cx, |pane, cx| pane.shutdown_runtime(cx));
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
        ChatPane::resume(shutdown.clone(), preview, workspace, tab, window, cx).unwrap()
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
    pane.update(cx, |pane, cx| pane.shutdown_runtime(cx));
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
            window,
            cx,
        )
        .unwrap()
    });
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
        ChatPane::resume(shutdown.clone(), preview, workspace, tab, window, cx).unwrap()
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
        let result = ChatPane::resume(RuntimeShutdown::new(), preview.clone(), workspace, saved(&root, Some("placeholder-preview-session")), window, cx);
        assert!(matches!(result, Err(error) if error.to_string() == "Resume for this recorded backend is unavailable"));
        let mut pane = ChatPane::history(RuntimeShutdown::new(), SavedChat { uid:"placeholder-preview".into(), cwd:root.clone(), provider:preview.provider(), session_id:None, records:vec![] }, window, cx);
        pane.controller.config = saved(&root, None).agent_config.unwrap();
        let (receipt, _) = pane.controller.dispatch(ChatIntent::Permission { request_id:"placeholder-request".into(), decision:gitterm_gpui_chat::PermissionDecision::AllowAlways { suggestion:serde_json::json!({"type":"acp_option","optionId":"placeholder-always"}) } });
        assert!(matches!(receipt.state, gitterm_gpui_chat::adapter::ReceiptState::Rejected(error) if error == "Allow and remember · not saved in preview"));
        pane
    });
}
