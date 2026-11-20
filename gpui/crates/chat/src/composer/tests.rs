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
