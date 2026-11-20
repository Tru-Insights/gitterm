use super::*;
use gitterm_core::settings::{ChatDefaults, ReviewConfig};

fn fixture() -> FixtureSettings {
    FixtureSettings {
        chat_width: ChatWidth::Wide,
        chat: ChatDefaults {
            default_model: "fixture-model".into(),
            default_effort: Some("high".into()),
            ..ChatDefaults::default()
        },
        review: ReviewConfig {
            default_reviewer: ReviewerKind::Codex,
            codex_model: Some("codex-fixture".into()),
            ..ReviewConfig::default()
        },
        delegations: gitterm_core::settings::DelegationsConfig { auto_tell: false },
        notifications: gitterm_core::settings::NotificationsConfig { needs_you: false },
        pr_watch: gitterm_core::settings::PrWatchConfig { auto_tell: false },
        work_notes: gitterm_core::settings::WorkNotesConfig { auto_commit: false },
        agent_presets: vec![gitterm_core::settings::AgentPreset {
            name: "fixture-agent".into(),
            command: "run-agent".into(),
            resume_command: None,
            icon: "A".into(),
            color: WorkspaceColor::Teal,
        }],
        quick_commands: vec![gitterm_core::settings::QuickCommand {
            name: "fixture-command".into(),
            command: "echo fixture".into(),
        }],
        ..FixtureSettings::default()
    }
}

#[test]
fn renders_fixture_values_in_each_settings_section() {
    let sections = section_rows(&fixture());
    let names: Vec<_> = sections.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            "Chat",
            "Review",
            "Delegations",
            "Notifications",
            "PR watch",
            "Work notes",
            "Agent presets",
            "Quick commands"
        ]
    );
    let rows = |name| {
        sections
            .iter()
            .find(|(section, _)| *section == name)
            .unwrap()
            .1
            .join("\n")
    };
    assert!(rows("Chat").contains("Width: Wide"));
    assert!(rows("Chat").contains("Default model: fixture-model"));
    assert!(rows("Review").contains("Reviewer: Codex"));
    assert!(rows("Review").contains("Codex model: codex-fixture"));
    assert!(rows("Delegations").contains("Auto tell: false"));
    assert!(rows("Notifications").contains("Needs you: false"));
    assert!(rows("PR watch").contains("Auto tell: false"));
    assert!(rows("Work notes").contains("Auto commit: false"));
    assert!(rows("Agent presets").contains("fixture-agent · run-agent · Teal"));
    assert!(rows("Quick commands").contains("fixture-command · echo fixture"));
}

#[test]
fn every_edit_records_its_typed_intent_and_targets_stable_names() {
    let mut form = SettingsForm::new(fixture());
    let intents = [
        SettingsIntent::SetChatWidth(ChatWidth::Full),
        SettingsIntent::SetChatFontSize(16.0),
        SettingsIntent::SetChatDefaultModel("next-model".into()),
        SettingsIntent::SetChatDefaultEffort(Some("xhigh".into())),
        SettingsIntent::SetRememberLast(false),
        SettingsIntent::SetAntigravityModel("antigravity-next".into()),
        SettingsIntent::SetOpenCodeModel("open-code-next".into()),
        SettingsIntent::SetReviewer(ReviewerKind::ClaudeSubagent),
        SettingsIntent::SetSubagentModel("sonnet".into()),
        SettingsIntent::SetCodexModel(None),
        SettingsIntent::SetMaxConcurrentReviews(3),
        SettingsIntent::SetDelegationsAutoTell(true),
        SettingsIntent::SetNotificationsNeedsYou(true),
        SettingsIntent::SetPrWatchAutoTell(true),
        SettingsIntent::SetWorkNotesAutoCommit(true),
        SettingsIntent::EditPreset {
            name: "fixture-agent".into(),
            command: "new-agent".into(),
            resume_command: Some("resume".into()),
            icon: "B".into(),
            color: WorkspaceColor::Blue,
        },
        SettingsIntent::EditQuickCommand {
            name: "fixture-command".into(),
            command: "echo changed".into(),
        },
    ];
    for expected in intents.iter().cloned() {
        form.edit(expected);
    }
    assert_eq!(form.intents, intents);
    assert_eq!(form.settings.agent_presets[0].command, "new-agent");
    assert_eq!(form.settings.quick_commands[0].command, "echo changed");
    assert_eq!(form.key(SettingsKey::Enter), Some(intents.to_vec()));
}

#[test]
fn cancel_discards_edits_and_font_uses_core_clamp() {
    let mut form = SettingsForm::new(fixture());
    form.edit(SettingsIntent::SetChatWidth(ChatWidth::Narrow));
    assert_eq!(form.key(SettingsKey::Escape), None);
    assert!(form.intents.is_empty());
    assert_eq!(form.key(SettingsKey::Enter), None);

    let settings = FixtureSettings {
        chat_font_size: 99.0,
        ..fixture()
    };
    let form = SettingsForm::validated(settings);
    assert_eq!(
        form.settings.chat_font_size,
        gitterm_core::settings::MAX_CHAT_FONT_SIZE
    );
    let mut form = SettingsForm::new(fixture());
    form.edit(SettingsIntent::SetChatFontSize(99.0));
    assert_eq!(
        form.settings.chat_font_size,
        gitterm_core::settings::MAX_CHAT_FONT_SIZE
    );
    assert_eq!(form.intents, [SettingsIntent::SetChatFontSize(22.0)]);
}

#[test]
fn tab_follows_visual_order_and_wraps() {
    let mut form = SettingsForm::new(fixture());
    assert_eq!(FOCUS_ORDER[form.focus], "chat.width");
    assert_eq!(form.key(SettingsKey::Tab), None);
    assert_eq!(FOCUS_ORDER[form.focus], "chat.font_size");
    form.focus = FOCUS_ORDER.len() - 1;
    form.key(SettingsKey::Tab);
    assert_eq!(form.focus, 0);
}
