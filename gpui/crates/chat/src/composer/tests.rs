use super::*;
use gitterm_core::chat::state::ConfirmedEffort;
use serde_json::json;

#[test]
fn acknowledged_auto_and_applied_effort_and_slash_catalog_are_projected() {
    let mut state = State::default();
    state.acknowledged.model = Some("confirmed-model".into());
    state.acknowledged.mode = Some("default".into());
    state.acknowledged.effort = Some(ConfirmedEffort {
        effort: None,
        applied: Some("high".into()),
    });
    state.conversation.push(AgentEvent::Harness(HarnessEvent::Ready {
        session_id: None, permission_mode: None, models: vec![], modes: vec![],
        commands: vec![json!({"name":"checkpoint","description":"Save progress","argumentHint":"[message]"}), json!({"name":"bad name"}), json!({"description":"no name"})],
    }));
    let mut view = ComposerState::default();
    view.project(&state, None);
    assert_eq!(view.model, "confirmed-model");
    assert_eq!(view.mode, "Default");
    assert_eq!(view.effort, "Auto · 200k");
    assert_eq!(view.slash_matches("/check")[0].name, "checkpoint");
    assert!(view.slash_matches("/checkpoint arg").is_empty());
    assert!(view.slash_matches("text/check").is_empty());
    assert_eq!(view.commands.len(), 1);
    state.pending_requests.push("request".into());
    view.project(&state, None);
    assert!(view.pending_request);
    assert!(view.status.contains("Needs you"));
}

#[test]
fn iced_chips_use_acknowledged_labels_in_reference_order() {
    let mut state = State::default();
    state.acknowledged.model = Some("opus".into());
    state.acknowledged.mode = Some("auto".into());
    state.acknowledged.effort = Some(ConfirmedEffort {
        effort: None,
        applied: Some("high".into()),
    });
    state.checkout = Some("/tmp/current".into());
    state
        .conversation
        .push(AgentEvent::Harness(HarnessEvent::Ready {
            session_id: None,
            permission_mode: None,
            models: vec![
                json!({"value": "opus", "displayName": "Opus 5.5", "supportsEffort":true}),
                json!({"value":"opus[1m]", "supportsEffort":true}),
            ],
            modes: vec![],
            commands: vec![],
        }));
    let mut view = ComposerState::default();
    view.project(&state, None);
    assert_eq!(
        view.chips().map(|(id, _)| id),
        [
            "review",
            "model",
            "effort",
            "mode",
            "checkout",
            "guidelines"
        ]
    );
    assert_eq!(
        view.chips().map(|(_, label)| label),
        [
            "Review / Consult…",
            "Opus 5.5",
            "Auto · 200k",
            "Auto mode",
            "Current checkout",
            "guidelines"
        ]
    );
    assert_eq!(view.checkout_path.as_deref(), Some("/tmp/current"));
    state.acknowledged.model = Some("opus[1m]".into());
    state.acknowledged.effort.as_mut().unwrap().effort = Some("high".into());
    view.project(&state, None);
    assert_eq!(view.model, "Opus 5.5");
    assert_eq!(view.effort, "High · 1M");
    view.project(&State::default(), None);
    assert_eq!(
        view.model, "Model",
        "never invent a provider acknowledgement"
    );
    assert_eq!(view.status_at(400_000), "Ready");
}

#[test]
fn c20_model_chip_uses_iced_catalog_names_and_fallbacks() {
    use gitterm_core::chat::types::{AcpAgent, AgentBackend};

    let claude = Some(AgentBackend::Claude);
    let empty_catalog: &[serde_json::Value] = &[];
    let catalog = [
        json!({"value":"opus", "displayName":"Opus 5.5"}),
        json!({"value":"gpt-5.1-codex", "displayName":"GPT-5.1-Codex"}),
        json!({"value":"default", "description":"Opus 5.5 · Your Claude settings decide"}),
    ];
    assert_eq!(iced_model_label("opus", claude, None), "opus");
    assert_eq!(iced_model_label("default", claude, None), "Claude settings");
    assert_eq!(
        iced_model_label("opus", claude, Some(empty_catalog)),
        "Opus"
    );
    assert_eq!(
        iced_model_label("default", claude, Some(empty_catalog)),
        "Your Claude settings decide"
    );
    assert_eq!(iced_model_label("opus", claude, Some(&catalog)), "Opus 5.5");
    assert_eq!(
        iced_model_label("opus[1m]", claude, Some(&catalog)),
        "Opus 5.5"
    );
    assert_eq!(
        iced_model_label("default", claude, Some(&catalog)),
        "Opus 5.5"
    );
    assert_eq!(
        iced_model_label("unknown-model", claude, Some(&catalog)),
        "unknown-model"
    );
    assert_eq!(
        iced_model_label("gpt-5.1-codex", claude, Some(&catalog)),
        "GPT-5.1-Codex"
    );

    let acp = Some(AgentBackend::Acp(AcpAgent::OpenCode));
    assert_eq!(iced_model_label("default", acp, None), "OpenCode default");
    assert_eq!(
        iced_model_label("gpt-5.1-codex", acp, Some(&catalog)),
        "GPT-5.1-Codex"
    );
    assert_eq!(
        iced_model_label("unlisted", acp, Some(&catalog)),
        "unlisted"
    );
}

#[test]
fn c21_mode_chip_tones_match_iced_css_for_every_permission_mode() {
    for (mode, expected) in [
        ("default", ModeTone::Muted),
        ("acceptEdits", ModeTone::Accent),
        ("plan", ModeTone::Result),
        ("auto", ModeTone::Warn),
        ("dontAsk", ModeTone::Error),
        ("bypassPermissions", ModeTone::Error),
        ("custom", ModeTone::Muted),
    ] {
        assert_eq!(mode_tone(mode), expected, "mode {mode}");
    }

    let mut state = State::default();
    state.acknowledged.mode = Some("auto".into());
    let mut composer = ComposerState::default();
    composer.project(&state, None);
    assert_eq!(composer.mode, "Auto mode");
    assert_eq!(mode_tone(&composer.mode_value), ModeTone::Warn);
}

#[test]
fn activity_uses_turn_timing_and_freezes_on_completion() {
    let mut state = State::default();
    state.state = AgentSessionState::Streaming;
    state.conversation = vec![
        AgentEvent::Harness(HarnessEvent::TurnStarted {
            session_id: None,
            model: None,
        }),
        AgentEvent::Other(json!({"kind":"turn_timing", "start_ms":1000})),
        AgentEvent::Harness(HarnessEvent::ItemStarted {
            id: "bash".into(),
            kind: ItemKind::ToolCall {
                name: "Bash".into(),
                input: json!({}),
            },
        }),
    ];
    let mut view = ComposerState::default();
    view.project(&state, None);
    assert_eq!(view.status_at(187000), "Running Bash 3m 6s");
    assert_eq!(view.status_at(188000), "Running Bash 3m 7s");
    assert_eq!(view.status_at(0), "Running Bash 0.0s");
    state.state = AgentSessionState::Idle;
    state.conversation.push(AgentEvent::Other(
        json!({"kind":"turn_timing", "end_ms":187000}),
    ));
    view.project(&state, None);
    assert_eq!(view.status_at(400000), "Done 3m 6s");
    state
        .conversation
        .push(AgentEvent::Harness(HarnessEvent::ConversationReset {
            trigger: None,
        }));
    view.project(&state, None);
    assert_eq!(view.status_at(400000), "Ready");
}

#[test]
fn normal_chat_sources_do_not_render_fixture_or_capability_banners() {
    let sources = [
        include_str!("../view.rs"),
        include_str!("../composer.rs"),
        include_str!("../../../shell/src/chat.rs"),
    ];
    for source in sources {
        for banned in [
            "Claude preview ·",
            " rows{}",
            "Images, dictation, review/delegation cards",
            "Fresh local chat ·",
            "Replaying ·",
            "Settings unavailable in this preview",
        ] {
            assert!(!source.contains(banned), "chat still contains {banned}");
        }
    }
}

#[test]
fn slash_matches_iced_exact_prefix_within_case_and_catalog_order() {
    let state = ComposerState {
        commands: ["pr-ready", "help", "pr", "checkpoint", "PRint", "ship-pr"]
            .into_iter()
            .map(|name| SlashCommand {
                name: name.into(),
                description: String::new(),
                argument_hint: String::new(),
            })
            .collect(),
        ..Default::default()
    };
    let names = |draft: &str| {
        state
            .slash_matches(draft)
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names("/"),
        ["pr-ready", "help", "pr", "checkpoint", "PRint", "ship-pr"]
    );
    assert_eq!(names("/PR"), ["pr", "pr-ready", "PRint", "ship-pr"]);
    assert_eq!(names("/e"), ["pr-ready", "help", "checkpoint"]);
    for draft in ["text /pr", "/pr ", "/pr\n", "/missing"] {
        assert!(names(draft).is_empty());
    }
}

#[test]
fn slash_navigation_wraps_escape_hides_tab_fills_and_enter_obeys_argument_hint() {
    let mut state = ComposerState {
        commands: vec![
            SlashCommand {
                name: "clear".into(),
                description: String::new(),
                argument_hint: String::new(),
            },
            SlashCommand {
                name: "checkpoint".into(),
                description: String::new(),
                argument_hint: "[message]".into(),
            },
        ],
        ..Default::default()
    };
    assert_eq!(state.slash_key("/", "up"), SlashOutcome::Handled);
    assert_eq!(state.slash_index, 1);
    assert_eq!(
        state.slash_key("/", "enter"),
        SlashOutcome::Fill {
            name: "checkpoint".into(),
            submit: false
        }
    );
    state.slash_key("/", "down");
    assert_eq!(state.slash_index, 0);
    assert_eq!(
        state.slash_key("/", "tab"),
        SlashOutcome::Fill {
            name: "clear".into(),
            submit: false
        }
    );
    assert_eq!(
        state.slash_key("/", "enter"),
        SlashOutcome::Fill {
            name: "clear".into(),
            submit: true
        }
    );
    assert_eq!(state.slash_key("/", "escape"), SlashOutcome::Handled);
    assert!(state.slash_matches("/").is_empty());
    assert_eq!(state.slash_key("/", "enter"), SlashOutcome::Ignored);
}

#[test]
fn completion_usage_lives_in_activity_with_iced_wording_and_no_missing_placeholders() {
    let mut state = State::default();
    state
        .conversation
        .push(AgentEvent::Harness(HarnessEvent::TurnCompleted {
            status: gitterm_core::chat::types::TurnStatus::Completed,
            usage: json!({"output_tokens":42}),
            cost_usd: Some(0.1234),
            rate_limit: None,
        }));
    let mut view = ComposerState::default();
    view.project(&state, None);
    assert_eq!(view.status_at(1000), "Done ↓ 42 tokens · $0.1234 session");
    state
        .conversation
        .push(AgentEvent::Harness(HarnessEvent::TurnCompleted {
            status: gitterm_core::chat::types::TurnStatus::Interrupted,
            usage: serde_json::Value::Null,
            cost_usd: None,
            rate_limit: None,
        }));
    view.project(&state, None);
    assert_eq!(view.status_at(1000), "Interrupted");
    state
        .conversation
        .push(AgentEvent::Harness(HarnessEvent::ConversationReset {
            trigger: None,
        }));
    view.project(&state, None);
    assert_eq!(view.status_at(1000), "Ready");
}

#[test]
fn c04_withdraw_tooltip_keeps_the_current_harness_working() {
    let mut composer = ComposerState::default();
    assert_eq!(
        composer.withdraw_tooltip(),
        "Withdraw this message (the agent keeps working)"
    );
    composer.backend = Some(gitterm_core::chat::types::AgentBackend::Claude);
    assert_eq!(
        composer.withdraw_tooltip(),
        "Withdraw this message (Claude keeps working)"
    );
    composer.backend = Some(gitterm_core::chat::types::AgentBackend::Pi);
    assert_eq!(
        composer.withdraw_tooltip(),
        "Withdraw this message (the agent keeps working)"
    );
    composer.backend = Some(gitterm_core::chat::types::AgentBackend::Acp(
        gitterm_core::chat::types::AcpAgent::OpenCode,
    ));
    assert_eq!(
        composer.withdraw_tooltip(),
        "Withdraw this message (OpenCode keeps working)"
    );
}

#[test]
fn c05_slash_keeps_full_iced_description_for_the_tooltip() {
    let mut state = State::default();
    state.conversation.push(AgentEvent::Harness(HarnessEvent::Ready {
        session_id:None,permission_mode:None,models:vec![],modes:vec![],
        commands:vec![json!({"name":"review","description":"Review changes\nwith context","argumentHint":"[focus]"})],
    }));
    let mut composer = ComposerState::default();
    composer.project(&state, None);
    assert_eq!(
        composer.slash_matches("/rev")[0].description,
        "Review changes\nwith context"
    );
}

#[test]
fn c19_guidelines_explains_the_system_prompt() {
    assert_eq!(
        GUIDELINES_TOOLTIP,
        "GitTerm adds its change guidelines to this agent's system prompt (workspace settings switch them off)"
    );
    assert!(crate::pickers::Picker::from_chip("guidelines").is_none());
}
