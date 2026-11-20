//! Pins the Iced C10 arms without startup, subprocesses, GUI or user state.
use super::*;
use gitterm::harness::{acp::AcpAgent, HarnessSession, MessageState};

fn fixture(root: &Path, config: AgentBackendConfig) -> App {
    let mut app = baseline_tests::app_with_tabs(root);
    app.settings_refused = Some("headless fixture".into());
    app.workspaces.truncate(1);
    let ws = &mut app.workspaces[0];
    ws.dir = root.into();
    ws.tabs.truncate(1);
    ws.tabs[0].kind = TabKind::Agent(AgentSession::new(config));
    ws.tabs[0].repo_path = root.into();
    app
}
fn claude() -> AgentBackendConfig {
    AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: None,
        effort: None,
    }
}
fn acp() -> AgentBackendConfig {
    AgentBackendConfig::Acp {
        agent: AcpAgent::OpenCode,
        model: "original".into(),
        mode: None,
        effort: None,
    }
}
fn session(app: &App) -> &AgentSession {
    app.workspaces[0].tabs[0].agent_session().unwrap()
}
fn session_mut(app: &mut App) -> &mut AgentSession {
    app.workspaces[0].tabs[0].agent_session_mut().unwrap()
}
fn port(app: &mut App) -> tokio::sync::mpsc::UnboundedReceiver<HarnessCommand> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    session_mut(app).harness = Some(HarnessSession::Claude(
        gitterm::harness::claude::ClaudeSession::from_port(tx),
    ));
    rx
}
fn apply(app: &mut App, event: Event) {
    let _ = app.update(event);
}

#[test]
fn c10_prestart_pickers_configure_the_next_launch_and_reject_invalid_values() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), claude());
    apply(&mut app, Event::AgentSetPermissionMode(10, "plan".into()));
    apply(&mut app, Event::AgentSetModel(10, " sonnet ".into()));
    apply(&mut app, Event::AgentSetEffort(10, Some("high".into())));
    let expected = serde_json::json!({"backend":"claude", "model":"sonnet", "permission_mode":"plan", "effort":"high"});
    assert_eq!(
        serde_json::to_value(&session(&app).config).unwrap(),
        expected
    );
    for event in [
        Event::AgentSetPermissionMode(10, "unknown".into()),
        Event::AgentSetModel(10, "two words".into()),
        Event::AgentSetEffort(10, Some("unknown".into())),
    ] {
        apply(&mut app, event);
    }
    assert_eq!(
        serde_json::to_value(&session(&app).config).unwrap(),
        expected
    );
    assert_eq!(session(&app).conversation.len(), 3);
    assert!(app.workspaces_dirty);
}

#[test]
fn c10_live_pickers_dispatch_and_only_confirmed_values_enter_config() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    for (event, ack, expected) in [
        (
            Event::AgentSetModel(10, "next".into()),
            HarnessEvent::ModelChanged("next".into()),
            "model",
        ),
        (
            Event::AgentSetPermissionMode(10, "plan".into()),
            HarnessEvent::PermissionModeChanged("plan".into()),
            "mode",
        ),
        (
            Event::AgentSetEffort(10, Some("high".into())),
            HarnessEvent::EffortChanged {
                effort: Some("high".into()),
                applied: Some("high".into()),
            },
            "effort",
        ),
    ] {
        let before = serde_json::to_value(&session(&app).config).unwrap();
        apply(&mut app, event);
        let command = rx.try_recv().unwrap();
        assert!(matches!(
            (expected, command),
            ("model", HarnessCommand::SetModel(_))
                | ("mode", HarnessCommand::SetPermissionMode(_))
                | ("effort", HarnessCommand::SetEffort(_))
        ));
        assert_eq!(serde_json::to_value(&session(&app).config).unwrap(), before);
        apply(
            &mut app,
            Event::AgentEventReceived(10, tab::AgentEvent::Harness(ack)),
        );
        assert_ne!(serde_json::to_value(&session(&app).config).unwrap(), before);
    }
}

#[test]
fn c10_busy_submit_keeps_ids_and_queue_echo_then_withdraws_by_id() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    session_mut(&mut app).reduce(ChatIntent::SubmissionAccepted);
    apply(
        &mut app,
        Event::AgentSubmitPrompt(
            10,
            UserPrompt {
                text: "next".into(),
                images: vec![],
                id: Some("queued-id".into()),
            },
        ),
    );
    assert!(
        matches!(rx.try_recv().unwrap(), HarnessCommand::SendUserMessage(UserPrompt { id: Some(id), .. }) if id == "queued-id")
    );
    let echo = session(&app)
        .conversation
        .iter()
        .find(|event| event.prompt_id() == Some("queued-id"))
        .unwrap()
        .webview_payload()
        .unwrap();
    assert_eq!(echo["queued"], true);
    apply(
        &mut app,
        Event::AgentEventReceived(
            10,
            tab::AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                id: "queued-id".into(),
                state: MessageState::Queued,
            }),
        ),
    );
    apply(
        &mut app,
        Event::AgentWebviewIpc(AgentIpcMessage::Withdraw {
            tab_id: 10,
            id: "queued-id".into(),
        }),
    );
    assert!(
        matches!(rx.try_recv().unwrap(), HarnessCommand::WithdrawQueuedMessage(id) if id == "queued-id")
    );
}

#[test]
fn c10_checkout_accepts_only_offered_choices_before_session_binding() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), claude());
    let original = app.workspaces[0].tabs[0].repo_path.clone();
    apply(
        &mut app,
        Event::AgentSetCheckout(10, root.path().join("unoffered")),
    );
    assert_eq!(app.workspaces[0].tabs[0].repo_path, original);
    session_mut(&mut app).bind_session(Some("bound".into()));
    apply(&mut app, Event::AgentSetCheckout(10, original.clone()));
    assert_eq!(app.workspaces[0].tabs[0].repo_path, original);
    assert_eq!(app.chat_checkout_state(10).unwrap()["locked"], true);
}

fn stream(app: &mut App, event: HarnessEvent) {
    let stamp = session(app).runtime_stamp(app.workspaces[0].tabs[0].session_target());
    apply(
        app,
        Event::AgentRuntimeEvent(stamp, tab::AgentEvent::Harness(event)),
    );
}
fn queue_prompt(app: &mut App, id: &str) {
    session_mut(app).reduce(ChatIntent::SubmissionAccepted);
    apply(
        app,
        Event::AgentSubmitPrompt(
            10,
            UserPrompt {
                text: "next".into(),
                images: vec![],
                id: Some(id.into()),
            },
        ),
    );
}

#[test]
fn c10_withdraw_after_start_is_reported_without_dispatch_or_late_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    queue_prompt(&mut app, "race");
    rx.try_recv().unwrap();
    stream(
        &mut app,
        HarnessEvent::MessageLifecycle {
            id: "race".into(),
            state: MessageState::Started,
        },
    );
    apply(
        &mut app,
        Event::AgentWebviewIpc(AgentIpcMessage::Withdraw {
            tab_id: 10,
            id: "race".into(),
        }),
    );
    assert!(rx.try_recv().is_err());
    assert!(session(&app).conversation.iter().any(|event| matches!(event, tab::AgentEvent::Harness(HarnessEvent::MessageWithdrawRefused { id, reason }) if id == "race" && reason.contains("already started"))));
    let count = session(&app).conversation.len();
    stream(
        &mut app,
        HarnessEvent::MessageLifecycle {
            id: "race".into(),
            state: MessageState::Cancelled,
        },
    );
    assert_eq!(
        session(&app).queued_prompts["race"].status,
        PromptStatus::Started
    );
    assert_eq!(session(&app).conversation.len(), count);
}

#[test]
fn c10_picker_ack_is_applied_once_and_stale_runtime_events_are_inert() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    apply(&mut app, Event::AgentSetPermissionMode(10, "plan".into()));
    rx.try_recv().unwrap();
    apply(&mut app, Event::AgentSetModel(10, "blocked".into()));
    assert!(rx.try_recv().is_err());
    stream(
        &mut app,
        HarnessEvent::Error("unrelated interrupt failure".into()),
    );
    assert!(session(&app).pending_change().is_some());
    assert!(
        matches!(&session(&app).config, AgentBackendConfig::Acp { mode: None, model, .. } if model == "original")
    );
    stream(&mut app, HarnessEvent::PermissionModeChanged("plan".into()));
    assert!(session(&app).pending_change().is_none());
    let count = session(&app).conversation.len();
    app.workspaces_dirty = false;
    stream(&mut app, HarnessEvent::PermissionModeChanged("plan".into()));
    assert!(!app.workspaces_dirty);
    assert_eq!(session(&app).conversation.len(), count);
    let stamp = session(&app).runtime_stamp(app.workspaces[0].tabs[0].session_target());
    session_mut(&mut app).begin_runtime();
    apply(
        &mut app,
        Event::AgentRuntimeEvent(
            stamp,
            tab::AgentEvent::Harness(HarnessEvent::PermissionModeChanged(
                "bypassPermissions".into(),
            )),
        ),
    );
    assert!(!app.workspaces_dirty);
    assert!(
        matches!(&session(&app).config, AgentBackendConfig::Acp { mode: Some(mode), .. } if mode == "plan")
    );
}

#[test]
fn c10_exit_keeps_reported_queue_unsent_and_unreceipted_delivery_uncertain() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let _rx = port(&mut app);
    queue_prompt(&mut app, "known-queued");
    stream(
        &mut app,
        HarnessEvent::MessageLifecycle {
            id: "known-queued".into(),
            state: MessageState::Queued,
        },
    );
    queue_prompt(&mut app, "unknown");
    stream(&mut app, HarnessEvent::ProcessExited { code: Some(1) });
    assert_eq!(
        session(&app).queued_prompts["known-queued"].status,
        PromptStatus::Unsent
    );
    assert_eq!(
        session(&app).queued_prompts["unknown"].status,
        PromptStatus::Uncertain
    );
    assert!(session(&app).shows_prompt("known-queued"));
    assert!(session(&app).conversation.iter().any(|event| matches!(event, tab::AgentEvent::Other(value) if value["kind"] == "prompt_uncertain" && value["id"] == "unknown")));
    assert!(session(&app).harness.is_none());
    let before = serde_json::to_value(&session(&app).config).unwrap();
    stream(&mut app, HarnessEvent::ModelChanged("late".into()));
    assert_eq!(serde_json::to_value(&session(&app).config).unwrap(), before);
}

#[test]
fn c10_correlated_channel_rejection_releases_picker_without_saving_it() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let rx = port(&mut app);
    drop(rx);
    let before = serde_json::to_value(&session(&app).config).unwrap();
    apply(&mut app, Event::AgentSetPermissionMode(10, "plan".into()));
    assert!(session(&app).pending_change().is_none());
    assert_eq!(serde_json::to_value(&session(&app).config).unwrap(), before);
    assert!(!app.workspaces_dirty);
}

#[test]
fn c10_written_receipts_are_runtime_bound_and_never_acknowledge_a_picker() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let _rx = port(&mut app);
    let current = app.workspaces[0].tabs[0].session_target();
    let runtime = session(&app).runtime_stamp(current.clone());
    let config = session(&app).config.clone();
    let ChatEffect::Dispatch { correlation, .. } =
        session_mut(&mut app).reduce(ChatIntent::ChangeSetting {
            runtime: runtime.clone(),
            current,
            config,
            process_running: true,
            change: SettingChange::Mode("plan".into()),
        })
    else {
        panic!("expected dispatch")
    };
    apply(
        &mut app,
        Event::AgentCommandOutcome(runtime.clone(), correlation, DeliveryOutcome::Written),
    );
    assert!(session(&app).pending_change().is_some());
    assert!(matches!(
        &session(&app).config,
        AgentBackendConfig::Acp { mode: None, .. }
    ));
    session_mut(&mut app).begin_runtime();
    let count = session(&app).conversation.len();
    apply(
        &mut app,
        Event::AgentCommandOutcome(
            runtime,
            correlation,
            DeliveryOutcome::Rejected("stale".into()),
        ),
    );
    assert_eq!(session(&app).conversation.len(), count);
}

#[test]
fn c10_checkout_binding_changes_only_to_the_host_normalized_choice() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), claude());
    // A background tab avoids scheduling unrelated active-panel reads.
    app.workspaces[0].active_tab = 1;
    app.workspaces[0].tabs[0].repo_path = root.path().join("previous");
    apply(&mut app, Event::AgentSetCheckout(10, root.path().into()));
    assert_eq!(app.workspaces[0].tabs[0].repo_path, root.path());
    assert_eq!(session(&app).checkout.as_deref(), Some(root.path()));
    let stamp = session(&app).runtime_stamp(app.workspaces[0].tabs[0].session_target());
    session_mut(&mut app).bind_session(Some("started-session".into()));
    apply(&mut app, Event::AgentSetCheckout(10, root.path().into()));
    assert!(session(&app).accepts_runtime(&stamp, &app.workspaces[0].tabs[0].session_target()));
}

#[test]
fn c10_relayed_midturn_prompt_keeps_iced_withdraw_authority() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    session_mut(&mut app).reduce(ChatIntent::SubmissionAccepted);
    stream(
        &mut app,
        HarnessEvent::UserMessageEcho {
            id: Some("relayed".into()),
            text: "from another client".into(),
            image_count: 0,
            delivery: None,
        },
    );
    assert!(session(&app).shows_prompt("relayed"));
    apply(
        &mut app,
        Event::AgentWebviewIpc(AgentIpcMessage::Withdraw {
            tab_id: 10,
            id: "relayed".into(),
        }),
    );
    assert!(
        matches!(rx.try_recv().unwrap(), HarnessCommand::WithdrawQueuedMessage(id) if id == "relayed")
    );
    stream(
        &mut app,
        HarnessEvent::MessageLifecycle {
            id: "relayed".into(),
            state: MessageState::Started,
        },
    );
    apply(
        &mut app,
        Event::AgentWebviewIpc(AgentIpcMessage::Withdraw {
            tab_id: 10,
            id: "relayed".into(),
        }),
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn c10_receipts_after_task_attachment_use_each_dispatch_binding() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let _rx = port(&mut app);
    let receiver = session(&app).runtime_stamp(app.workspaces[0].tabs[0].session_target());
    app.workspaces[0].tabs[0].task_id = Some("attached-task".into());
    let current = app.workspaces[0].tabs[0].session_target();
    let config = session(&app).config.clone();
    let runtime = session(&app).runtime_stamp(current.clone());
    let ChatEffect::Dispatch { correlation, .. } =
        session_mut(&mut app).reduce(ChatIntent::ChangeSetting {
            runtime: runtime.clone(),
            current: current.clone(),
            config: config.clone(),
            process_running: true,
            change: SettingChange::Mode("plan".into()),
        })
    else {
        panic!()
    };
    apply(
        &mut app,
        Event::AgentCommandOutcome(
            receiver.clone(),
            correlation,
            DeliveryOutcome::Rejected("not written".into()),
        ),
    );
    assert!(session(&app).pending_change().is_none());
    session_mut(&mut app).reduce(ChatIntent::SubmissionAccepted);
    let ChatEffect::Dispatch {
        correlation,
        echo: Some(echo),
        ..
    } = session_mut(&mut app).reduce(ChatIntent::Queue {
        runtime,
        current,
        config,
        process_running: true,
        prompt: "queued".into(),
        message_id: "rebound".into(),
    })
    else {
        panic!()
    };
    session_mut(&mut app).record(echo);
    apply(
        &mut app,
        Event::AgentCommandOutcome(receiver.clone(), correlation, DeliveryOutcome::Written),
    );
    apply(
        &mut app,
        Event::AgentRuntimeEvent(
            receiver,
            tab::AgentEvent::Harness(HarnessEvent::ProcessExited { code: None }),
        ),
    );
    assert_eq!(
        session(&app).queued_prompts["rebound"].status,
        PromptStatus::Uncertain
    );
}

#[test]
fn c10_queue_rejection_keeps_text_images_and_replayable_unsent_marker() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    session_mut(&mut app).reduce(ChatIntent::SubmissionAccepted);
    stream(
        &mut app,
        HarnessEvent::RuntimeRequest {
            request_id: "permission".into(),
            tool_use_id: "tool".into(),
            kind: gitterm::harness::RuntimeRequestKind::Question {
                questions: serde_json::json!([]),
            },
        },
    );
    apply(
        &mut app,
        Event::AgentSubmitPrompt(
            10,
            UserPrompt {
                text: "recover this".into(),
                images: vec![gitterm::harness::ImageAttachment {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                }],
                id: Some("rejected".into()),
            },
        ),
    );
    assert!(rx.try_recv().is_err());
    let recovery = session(&app)
        .conversation
        .iter()
        .filter_map(tab::AgentEvent::webview_payload)
        .find(|value| value["type"] == "user_prompt" && value["text"] == "recover this")
        .unwrap();
    assert_eq!(recovery["images"][0]["data"], "AA==");
    assert_eq!(recovery["queued"], true);
    assert!(session(&app).conversation.iter().any(|event| matches!(event, tab::AgentEvent::Other(value) if value["kind"] == "prompt_unsent" && value["id"] == recovery["id"])));
}

#[test]
fn c10_rejected_second_picker_and_generic_errors_reproject_pending_mode() {
    let root = tempfile::tempdir().unwrap();
    let mut app = fixture(root.path(), acp());
    let mut rx = port(&mut app);
    apply(&mut app, Event::AgentSetPermissionMode(10, "plan".into()));
    rx.try_recv().unwrap();
    apply(&mut app, Event::AgentSetModel(10, "second".into()));
    assert!(rx.try_recv().is_err());
    let expected = serde_json::json!({"kind":"picker_pending", "setting":"mode", "value":"plan"});
    assert_eq!(
        session(&app)
            .conversation
            .last()
            .unwrap()
            .webview_payload()
            .unwrap(),
        expected
    );
    stream(&mut app, HarnessEvent::Error("uncorrelated error".into()));
    assert_eq!(
        session(&app)
            .conversation
            .last()
            .unwrap()
            .webview_payload()
            .unwrap(),
        expected
    );
    assert_eq!(
        session(&app).pending_change(),
        Some(&SettingChange::Mode("plan".into()))
    );
}
