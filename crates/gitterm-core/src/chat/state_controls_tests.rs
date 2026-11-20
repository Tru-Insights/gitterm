use super::*;

fn target() -> session::Target {
    session::Target {
        tab_id: 1,
        session_uid: "synthetic-session".into(),
        task_id: None,
        task_session_id: None,
    }
}
fn config() -> AgentBackendConfig {
    AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: None,
        effort: None,
    }
}
fn event(state: &mut State, event: HarnessEvent) -> EventEffects {
    let Effect::Event(effects) = state.reduce(Intent::Event(AgentEvent::Harness(event))) else {
        panic!("missing event effects")
    };
    effects
}
fn working() -> State {
    let mut state = State::default();
    state.reduce(Intent::SubmissionAccepted);
    state
}
fn queue(state: &mut State, id: &str) -> CommandCorrelation {
    let prompt = UserPrompt {
        text: "Synthetic queued prompt".into(),
        images: vec![ImageAttachment {
            media_type: "image/png".into(),
            data: "AA==".into(),
        }],
        id: None,
    };
    let Effect::Dispatch {
        correlation,
        command: HarnessCommand::SendUserMessage(prompt),
        echo: Some(echo),
        ..
    } = state.reduce(Intent::Queue {
        runtime: state.runtime_stamp(target()),
        current: target(),
        config: config(),
        process_running: true,
        prompt,
        message_id: id.into(),
    })
    else {
        panic!("missing queue dispatch")
    };
    assert_eq!(prompt.id.as_deref(), Some(id));
    assert_eq!(echo.webview_payload().unwrap()["queued"], true);
    correlation
}
fn withdraw(state: &mut State, id: &str) -> Effect {
    state.reduce(Intent::Withdraw {
        runtime: state.runtime_stamp(target()),
        current: target(),
        process_running: true,
        message_id: id.into(),
    })
}
fn change(state: &mut State, change: SettingChange) -> Effect {
    state.reduce(Intent::ChangeSetting {
        runtime: state.runtime_stamp(target()),
        current: target(),
        config: config(),
        process_running: true,
        change,
    })
}
fn outcome(state: &mut State, correlation: CommandCorrelation, outcome: DeliveryOutcome) -> Effect {
    state.reduce(Intent::CommandOutcome {
        runtime: state.runtime_stamp(target()),
        current: target(),
        correlation,
        outcome,
    })
}

#[test]
fn queue_start_race_and_duplicate_lifecycle_cannot_allow_withdraw_after_start() {
    let mut state = working();
    let correlation = queue(&mut state, "prompt-1");
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Pending
    );
    outcome(&mut state, correlation, DeliveryOutcome::Written);
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Pending
    );
    assert!(matches!(
        withdraw(&mut state, "prompt-1"),
        Effect::Dispatch {
            command: HarnessCommand::WithdrawQueuedMessage(_),
            ..
        }
    ));
    let started = event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Started,
        },
    );
    assert!(started.notices[0].contains("already started"));
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Cancelled,
        },
    );
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Started
    );
    let refused = event(
        &mut state,
        HarnessEvent::MessageWithdrawRefused {
            id: "prompt-1".into(),
            reason: "already picked up".into(),
        },
    );
    assert!(refused.notices[0].contains("already picked up"));
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Queued,
        },
    );
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Started
    );
    assert!(
        matches!(withdraw(&mut state, "prompt-1"), Effect::Rejected(reason) if reason.contains("Not withdrawn"))
    );
    assert!(matches!(
        withdraw(&mut state, "unknown"),
        Effect::Rejected(_)
    ));
}

#[test]
fn refused_withdraw_keeps_prompt_and_allows_retry_only_while_queued() {
    let mut state = working();
    queue(&mut state, "prompt-1");
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Queued,
        },
    );
    let Effect::Dispatch { correlation, .. } = withdraw(&mut state, "prompt-1") else {
        panic!()
    };
    assert!(matches!(
        withdraw(&mut state, "prompt-1"),
        Effect::Rejected(_)
    ));
    outcome(&mut state, correlation, DeliveryOutcome::Written);
    assert!(state.queued_prompts["prompt-1"].withdraw_pending);
    assert_eq!(
        event(
            &mut state,
            HarnessEvent::MessageWithdrawRefused {
                id: "prompt-1".into(),
                reason: "harness refused".into()
            }
        )
        .notices
        .len(),
        1
    );
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Queued
    );
    assert!(matches!(
        withdraw(&mut state, "prompt-1"),
        Effect::Dispatch { .. }
    ));
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Cancelled,
        },
    );
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Cancelled
    );
    assert!(matches!(
        withdraw(&mut state, "prompt-1"),
        Effect::Rejected(_)
    ));
}

#[test]
fn process_exit_retains_unsent_text_images_and_never_resends() {
    let mut state = working();
    queue(&mut state, "queued");
    queue(&mut state, "started");
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "started".into(),
            state: MessageState::Started,
        },
    );
    let effects = event(&mut state, HarnessEvent::ProcessExited { code: Some(1) });
    assert!(effects.process_exited);
    assert_eq!(effects.unsent_prompts.len(), 1);
    assert_eq!(effects.unsent_prompts[0].id.as_deref(), Some("queued"));
    assert_eq!(effects.unsent_prompts[0].text, "Synthetic queued prompt");
    assert_eq!(effects.unsent_prompts[0].images.len(), 1);
    assert_eq!(state.queued_prompts["queued"].status, PromptStatus::Unsent);
    assert_eq!(
        state.queued_prompts["started"].status,
        PromptStatus::Started
    );
    assert!(
        event(&mut state, HarnessEvent::ProcessExited { code: Some(1) })
            .unsent_prompts
            .is_empty()
    );
    assert!(matches!(
        change(&mut state, SettingChange::Model("sonnet".into())),
        Effect::Rejected(_)
    ));
    state.begin_runtime();
    assert_eq!(state.queued_prompts["queued"].status, PromptStatus::Unsent);
}

#[test]
fn picker_requests_are_pending_until_acknowledged_and_rejections_are_not_saved() {
    for (old, requested, acknowledged) in [
        (
            HarnessEvent::ModelChanged("default".into()),
            SettingChange::Model("sonnet".into()),
            HarnessEvent::ModelChanged("sonnet".into()),
        ),
        (
            HarnessEvent::PermissionModeChanged("default".into()),
            SettingChange::Mode("plan".into()),
            HarnessEvent::PermissionModeChanged("plan".into()),
        ),
        (
            HarnessEvent::EffortChanged {
                effort: None,
                applied: Some("medium".into()),
            },
            SettingChange::Effort(Some("high".into())),
            HarnessEvent::EffortChanged {
                effort: Some("high".into()),
                applied: Some("high".into()),
            },
        ),
    ] {
        let mut state = State::default();
        event(&mut state, old);
        let before = state.acknowledged.clone();
        let Effect::Dispatch { correlation, .. } = change(&mut state, requested.clone()) else {
            panic!()
        };
        assert_eq!(state.pending_change(), Some(&requested));
        assert_eq!(state.acknowledged, before);
        outcome(&mut state, correlation, DeliveryOutcome::Written);
        assert_eq!(state.acknowledged, before);
        assert!(matches!(
            change(&mut state, requested.clone()),
            Effect::Rejected(_)
        ));
        let effects = event(
            &mut state,
            HarnessEvent::Error("Synthetic rejection".into()),
        );
        assert_eq!(effects.notices.len(), 1);
        assert!(effects.settings_changed.is_none());
        assert!(state.pending_change().is_none());
        assert_eq!(state.acknowledged, before);
        assert!(matches!(
            change(&mut state, requested.clone()),
            Effect::Dispatch { .. }
        ));
        assert_eq!(
            event(&mut state, acknowledged.clone()).settings_changed,
            Some(requested)
        );
        assert!(state.pending_change().is_none());
        assert!(event(&mut state, acknowledged).settings_changed.is_none());
    }
}

#[test]
fn delivery_receipts_do_not_overwrite_a_start_or_confirm_a_picker() {
    let mut state = working();
    let correlation = queue(&mut state, "prompt-1");
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Started,
        },
    );
    assert!(matches!(
        outcome(
            &mut state,
            correlation,
            DeliveryOutcome::Uncertain("broken pipe".into())
        ),
        Effect::Rejected(_)
    ));
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Started
    );
    let Effect::Dispatch { correlation, .. } =
        change(&mut state, SettingChange::Model("sonnet".into()))
    else {
        panic!()
    };
    outcome(
        &mut state,
        correlation,
        DeliveryOutcome::Rejected("closed channel".into()),
    );
    assert!(state.pending_change().is_none());
    assert_eq!(state.acknowledged.model, None);
    let Effect::Dispatch { correlation, .. } =
        change(&mut state, SettingChange::Mode("plan".into()))
    else {
        panic!()
    };
    outcome(
        &mut state,
        correlation,
        DeliveryOutcome::Uncertain("write failed".into()),
    );
    assert_eq!(
        state.pending_change(),
        Some(&SettingChange::Mode("plan".into()))
    );
    assert_eq!(state.acknowledged.mode, None);
    event(
        &mut state,
        HarnessEvent::PermissionModeChanged("plan".into()),
    );
    assert_eq!(state.acknowledged.mode.as_deref(), Some("plan"));
}

#[test]
fn commands_reject_stale_runtime_and_invalid_picker_values() {
    let mut state = working();
    for change_value in [
        SettingChange::Model("bad model".into()),
        SettingChange::Mode("invalid".into()),
        SettingChange::Effort(Some("invalid".into())),
    ] {
        assert!(matches!(
            change(&mut state, change_value),
            Effect::Rejected(_)
        ));
    }
    let old = state.runtime_stamp(target());
    state.begin_runtime();
    assert!(matches!(
        state.reduce(Intent::ChangeSetting {
            runtime: old.clone(),
            current: target(),
            config: config(),
            process_running: true,
            change: SettingChange::Model("sonnet".into())
        }),
        Effect::Rejected(_)
    ));
    queue(&mut state, "prompt-1");
    assert!(matches!(
        state.reduce(Intent::Withdraw {
            runtime: old,
            current: target(),
            process_running: true,
            message_id: "prompt-1".into()
        }),
        Effect::Rejected(_)
    ));
    let mut rebound = target();
    rebound.task_session_id = Some("different-task-session".into());
    assert!(matches!(
        state.reduce(Intent::Withdraw {
            runtime: state.runtime_stamp(target()),
            current: rebound,
            process_running: true,
            message_id: "prompt-1".into()
        }),
        Effect::Rejected(_)
    ));
}

fn checkout_path() -> PathBuf {
    std::env::temp_dir().join("gitterm-chat-c10-fixture")
}

fn checkout(state: &mut State, locked: bool, offered: bool) -> Effect {
    state.reduce(Intent::ChooseCheckout {
        runtime: state.runtime_stamp(target()),
        current: target(),
        context: CheckoutContext {
            choices: if offered {
                vec![checkout_path()]
            } else {
                vec![]
            },
            locked_reason: locked.then(|| "Synthetic locked checkout".into()),
            process_running: false,
        },
        path: checkout_path(),
    })
}

#[test]
fn checkout_binding_requires_unlocked_offered_path_and_invalidates_old_commands() {
    let mut state = State::default();
    assert!(matches!(
        checkout(&mut state, true, true),
        Effect::Rejected(_)
    ));
    assert!(state.checkout.is_none());
    assert!(matches!(
        checkout(&mut state, false, false),
        Effect::Rejected(_)
    ));
    let old = state.runtime_stamp(target());
    let Effect::CheckoutBound { runtime, path } = checkout(&mut state, false, true) else {
        panic!()
    };
    assert_eq!(state.checkout, Some(path));
    assert!(state.accepts_runtime(&runtime, &target()));
    assert!(!state.accepts_runtime(&old, &target()));
    state.reduce(Intent::BindSession(Some("synthetic-session".into())));
    assert!(matches!(
        checkout(&mut state, false, true),
        Effect::Rejected(_)
    ));
    // An unrelated session's offered choice cannot bypass the owning binding.
    let mut other = target();
    other.session_uid = "other".into();
    assert!(matches!(
        state.reduce(Intent::ChooseCheckout {
            runtime,
            current: other,
            context: CheckoutContext {
                choices: vec![checkout_path()],
                locked_reason: None,
                process_running: false
            },
            path: checkout_path()
        }),
        Effect::Rejected(_)
    ));
}

#[test]
fn default_effort_is_distinct_from_not_reported_and_keeps_applied_level() {
    let mut state = State::default();
    assert_eq!(state.acknowledged.effort, None);
    event(
        &mut state,
        HarnessEvent::EffortChanged {
            effort: None,
            applied: Some("high".into()),
        },
    );
    assert_eq!(
        state.acknowledged.effort,
        Some(ConfirmedEffort {
            effort: None,
            applied: Some("high".into())
        })
    );
}

#[test]
fn exited_runtime_cannot_acknowledge_pending_picker_or_start_unsent_prompt() {
    let mut state = working();
    queue(&mut state, "prompt-1");
    change(&mut state, SettingChange::Mode("plan".into()));
    let effects = event(&mut state, HarnessEvent::ProcessExited { code: None });
    assert_eq!(effects.notices.len(), 1);
    assert!(state.pending_change().is_none());
    assert!(event(
        &mut state,
        HarnessEvent::PermissionModeChanged("plan".into())
    )
    .settings_changed
    .is_none());
    assert_eq!(state.acknowledged.mode, None);
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "prompt-1".into(),
            state: MessageState::Started,
        },
    );
    assert_eq!(
        state.queued_prompts["prompt-1"].status,
        PromptStatus::Unsent
    );
}

#[test]
fn failed_queue_and_withdraw_delivery_are_reported_and_preserve_prompt() {
    let mut state = working();
    let first = queue(&mut state, "rejected");
    assert!(matches!(
        outcome(
            &mut state,
            first,
            DeliveryOutcome::Rejected("closed channel".into())
        ),
        Effect::Rejected(_)
    ));
    assert_eq!(
        state.queued_prompts["rejected"].status,
        PromptStatus::Unsent
    );
    assert_eq!(state.queued_prompts["rejected"].prompt.images.len(), 1);
    let second = queue(&mut state, "uncertain");
    assert!(matches!(
        outcome(
            &mut state,
            second,
            DeliveryOutcome::Uncertain("partial write".into())
        ),
        Effect::Rejected(_)
    ));
    assert_eq!(
        state.queued_prompts["uncertain"].status,
        PromptStatus::Uncertain
    );
    assert!(matches!(
        withdraw(&mut state, "uncertain"),
        Effect::Rejected(_)
    ));
    event(
        &mut state,
        HarnessEvent::MessageLifecycle {
            id: "uncertain".into(),
            state: MessageState::Queued,
        },
    );
    let Effect::Dispatch { correlation, .. } = withdraw(&mut state, "uncertain") else {
        panic!()
    };
    assert!(matches!(
        outcome(
            &mut state,
            correlation,
            DeliveryOutcome::Rejected("closed channel".into())
        ),
        Effect::Rejected(_)
    ));
    assert!(!state.queued_prompts["uncertain"].withdraw_pending);
    assert_eq!(
        state.queued_prompts["uncertain"].status,
        PromptStatus::Queued
    );
}

#[test]
fn queue_rejects_duplicates_pending_questions_idle_and_unsupported_backends() {
    let mut state = working();
    queue(&mut state, "prompt-1");
    for config_value in [
        config(),
        AgentBackendConfig::Pi {
            model: "default".into(),
            session_path: None,
            thinking: None,
        },
        AgentBackendConfig::Acp {
            agent: AcpAgent::Gemini,
            model: "default".into(),
            mode: None,
            effort: None,
        },
    ] {
        assert!(matches!(
            state.reduce(Intent::Queue {
                runtime: state.runtime_stamp(target()),
                current: target(),
                config: config_value,
                process_running: true,
                prompt: "Duplicate prompt".into(),
                message_id: "prompt-1".into()
            }),
            Effect::Rejected(_)
        ));
    }
    state.pending_requests.push("question".into());
    assert!(matches!(
        state.reduce(Intent::Queue {
            runtime: state.runtime_stamp(target()),
            current: target(),
            config: config(),
            process_running: true,
            prompt: "Another prompt".into(),
            message_id: "prompt-2".into()
        }),
        Effect::Rejected(_)
    ));
    state.pending_requests.clear();
    state.state = AgentSessionState::Idle;
    assert!(matches!(
        state.reduce(Intent::Queue {
            runtime: state.runtime_stamp(target()),
            current: target(),
            config: config(),
            process_running: true,
            prompt: "Another prompt".into(),
            message_id: "prompt-2".into()
        }),
        Effect::Rejected(_)
    ));
}

#[test]
fn duplicate_old_acknowledgement_cannot_finish_a_new_picker_request() {
    let mut state = State::default();
    event(&mut state, HarnessEvent::ModelChanged("default".into()));
    change(&mut state, SettingChange::Model("sonnet".into()));
    assert!(
        event(&mut state, HarnessEvent::ModelChanged("default".into()))
            .settings_changed
            .is_none()
    );
    assert_eq!(
        state.pending_change(),
        Some(&SettingChange::Model("sonnet".into()))
    );
    assert!(matches!(
        change(&mut state, SettingChange::Mode("plan".into())),
        Effect::Rejected(_)
    ));
    event(&mut state, HarnessEvent::ModelChanged("sonnet".into()));
    assert!(state.pending_change().is_none());
}
