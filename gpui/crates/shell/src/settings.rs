//! Fixture-backed settings presentation. Edits are collected as typed intents;
//! the view deliberately has no persistence or dispatch path yet.
use std::path::PathBuf;

use gitterm_core::settings::{
    ChatWidth, ReviewerKind, SettingsWithVoice as Settings, WorkspaceColor,
};
use gpui::{div, prelude::*, px};

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;

pub type FixtureSettings = Settings<WorkspaceColor, (), PathBuf>;

#[derive(Clone, Debug, PartialEq)]
pub enum SettingsIntent {
    SetChatWidth(ChatWidth),
    SetChatFontSize(f32),
    SetChatDefaultModel(String),
    SetChatDefaultEffort(Option<String>),
    SetRememberLast(bool),
    SetAntigravityModel(String),
    SetOpenCodeModel(String),
    SetReviewer(ReviewerKind),
    SetSubagentModel(String),
    SetCodexModel(Option<String>),
    SetMaxConcurrentReviews(usize),
    SetDelegationsAutoTell(bool),
    SetNotificationsNeedsYou(bool),
    SetPrWatchAutoTell(bool),
    SetWorkNotesAutoCommit(bool),
    EditPreset {
        name: String,
        command: String,
        resume_command: Option<String>,
        icon: String,
        color: WorkspaceColor,
    },
    EditQuickCommand {
        name: String,
        command: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsKey {
    Escape,
    Enter,
    Tab,
}

#[derive(Clone, Debug)]
pub struct SettingsForm {
    pub settings: FixtureSettings,
    pub focus: usize,
    pub intents: Vec<SettingsIntent>,
    pub cancelled: bool,
}

impl SettingsForm {
    pub fn new(settings: FixtureSettings) -> Self {
        Self {
            settings,
            focus: 0,
            intents: Vec::new(),
            cancelled: false,
        }
    }

    /// Applies the core serde validator, including its chat-font clamp.
    pub fn validated(settings: FixtureSettings) -> Self {
        let value = serde_json::to_value(settings).expect("settings serialize");
        let settings = serde_json::from_value(value).expect("settings deserialize");
        Self::new(settings)
    }

    pub fn edit(&mut self, intent: SettingsIntent) {
        if self.cancelled {
            return;
        }
        let intent = match intent {
            SettingsIntent::SetChatFontSize(value) => {
                let mut candidate = self.settings.clone();
                candidate.chat_font_size = value;
                let value = serde_json::to_value(candidate).expect("settings serialize");
                self.settings = serde_json::from_value(value).expect("settings deserialize");
                SettingsIntent::SetChatFontSize(self.settings.chat_font_size)
            }
            intent => {
                match &intent {
                    SettingsIntent::SetChatWidth(value) => self.settings.chat_width = *value,
                    SettingsIntent::SetChatFontSize(_) => unreachable!(),
                    SettingsIntent::SetChatDefaultModel(value) => {
                        self.settings.chat.default_model = value.clone()
                    }
                    SettingsIntent::SetChatDefaultEffort(value) => {
                        self.settings.chat.default_effort = value.clone()
                    }
                    SettingsIntent::SetRememberLast(value) => {
                        self.settings.chat.remember_last = *value
                    }
                    SettingsIntent::SetAntigravityModel(value) => {
                        self.settings.chat.antigravity_model = value.clone()
                    }
                    SettingsIntent::SetOpenCodeModel(value) => {
                        self.settings.chat.opencode_model = value.clone()
                    }
                    SettingsIntent::SetReviewer(value) => {
                        self.settings.review.default_reviewer = *value
                    }
                    SettingsIntent::SetSubagentModel(value) => {
                        self.settings.review.subagent_model = value.clone()
                    }
                    SettingsIntent::SetCodexModel(value) => {
                        self.settings.review.codex_model = value.clone()
                    }
                    SettingsIntent::SetMaxConcurrentReviews(value) => {
                        self.settings.review.max_concurrent_reviews = *value
                    }
                    SettingsIntent::SetDelegationsAutoTell(value) => {
                        self.settings.delegations.auto_tell = *value
                    }
                    SettingsIntent::SetNotificationsNeedsYou(value) => {
                        self.settings.notifications.needs_you = *value
                    }
                    SettingsIntent::SetPrWatchAutoTell(value) => {
                        self.settings.pr_watch.auto_tell = *value
                    }
                    SettingsIntent::SetWorkNotesAutoCommit(value) => {
                        self.settings.work_notes.auto_commit = *value
                    }
                    SettingsIntent::EditPreset {
                        name,
                        command,
                        resume_command,
                        icon,
                        color,
                    } => {
                        if let Some(preset) = self
                            .settings
                            .agent_presets
                            .iter_mut()
                            .find(|p| p.name == *name)
                        {
                            preset.command = command.clone();
                            preset.resume_command = resume_command.clone();
                            preset.icon = icon.clone();
                            preset.color = *color;
                        }
                    }
                    SettingsIntent::EditQuickCommand {
                        name,
                        command: edited_command,
                    } => {
                        if let Some(command) = self
                            .settings
                            .quick_commands
                            .iter_mut()
                            .find(|c| c.name == *name)
                        {
                            command.command = edited_command.clone();
                        }
                    }
                }
                intent
            }
        };
        self.intents.push(intent);
    }

    pub fn key(&mut self, key: SettingsKey) -> Option<Vec<SettingsIntent>> {
        match key {
            SettingsKey::Escape => {
                self.cancelled = true;
                self.intents.clear();
                None
            }
            SettingsKey::Enter if !self.cancelled => Some(std::mem::take(&mut self.intents)),
            SettingsKey::Enter => None,
            SettingsKey::Tab => {
                self.focus = (self.focus + 1) % FOCUS_ORDER.len();
                None
            }
        }
    }
}

/// Tab order follows the visible fields, including repeated preset and command rows.
pub const FOCUS_ORDER: &[&str] = &[
    "chat.width",
    "chat.font_size",
    "chat.default_model",
    "chat.default_effort",
    "chat.remember_last",
    "review.default_reviewer",
    "review.subagent_model",
    "review.codex_model",
    "review.max_concurrent_reviews",
    "delegations.auto_tell",
    "notifications.needs_you",
    "pr_watch.auto_tell",
    "work_notes.auto_commit",
    "agent_presets",
    "quick_commands",
];

fn section<'a>(title: &'static str, rows: impl IntoIterator<Item = String> + 'a) -> gpui::Div {
    let mut result = div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .child(div().text_sm().child(title));
    for row in rows {
        result = result.child(div().text_xs().child(row));
    }
    result
}

/// Native fixture entry point used by the shell's settings dialog integration.
pub fn fixture_view(settings: &FixtureSettings) -> impl IntoElement {
    div().flex().flex_col().gap_2().w(px(620.)).p_3().children(
        section_rows(settings)
            .into_iter()
            .map(|(title, rows)| section(title, rows)),
    )
}

fn section_rows(settings: &FixtureSettings) -> Vec<(&'static str, Vec<String>)> {
    let chat = &settings.chat;
    let review = &settings.review;
    let preset_rows = settings
        .agent_presets
        .iter()
        .map(|p| format!("{} · {} · {}", p.name, p.command, color_name(p.color)));
    let command_rows = settings
        .quick_commands
        .iter()
        .map(|c| format!("{} · {}", c.name, c.command));
    vec![
        (
            "Chat",
            vec![
                format!("Width: {}", settings.chat_width),
                format!("Font size: {}", settings.chat_font_size),
                format!("Default model: {}", chat.default_model),
                format!(
                    "Default effort: {}",
                    chat.default_effort.as_deref().unwrap_or("model default")
                ),
                format!("Remember last: {}", chat.remember_last),
                format!("Antigravity model: {}", chat.antigravity_model),
                format!("OpenCode model: {}", chat.opencode_model),
            ],
        ),
        (
            "Review",
            vec![
                format!("Reviewer: {:?}", review.default_reviewer),
                format!("Subagent model: {}", review.subagent_model),
                format!(
                    "Codex model: {}",
                    review
                        .codex_model
                        .as_deref()
                        .unwrap_or("configured default")
                ),
                format!("Concurrent reviews: {}", review.max_concurrent_reviews),
            ],
        ),
        (
            "Delegations",
            vec![format!("Auto tell: {}", settings.delegations.auto_tell)],
        ),
        (
            "Notifications",
            vec![format!("Needs you: {}", settings.notifications.needs_you)],
        ),
        (
            "PR watch",
            vec![format!("Auto tell: {}", settings.pr_watch.auto_tell)],
        ),
        (
            "Work notes",
            vec![format!("Auto commit: {}", settings.work_notes.auto_commit)],
        ),
        ("Agent presets", preset_rows.collect()),
        ("Quick commands", command_rows.collect()),
    ]
}

fn color_name(color: WorkspaceColor) -> &'static str {
    match color {
        WorkspaceColor::Lavender => "Lavender",
        WorkspaceColor::Blue => "Blue",
        WorkspaceColor::Green => "Green",
        WorkspaceColor::Peach => "Peach",
        WorkspaceColor::Pink => "Pink",
        WorkspaceColor::Yellow => "Yellow",
        WorkspaceColor::Red => "Red",
        WorkspaceColor::Teal => "Teal",
    }
}
