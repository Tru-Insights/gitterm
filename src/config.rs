use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const APP_NAME: &str = "GitTerm V5";
pub const WINDOW_TITLE: &str = "GitTerm V5 Development";
pub const CONFIG_DIR_NAME: &str = "gitterm-v5";
pub const CONFIG_DIR_ENV: &str = "GITTERM_V5_CONFIG_DIR";
pub const INSTANCE_ID_ENV: &str = "GITTERM_V5_INSTANCE_ID";

// Global instance ID for this process
static INSTANCE_ID: OnceLock<String> = OnceLock::new();

// Config directory override, read once from GITTERM_V5_CONFIG_DIR
static CONFIG_DIR_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Get or generate the unique instance ID for this GitTerm process
pub fn instance_id() -> &'static str {
    INSTANCE_ID.get_or_init(|| {
        // Allow override via environment variable for testing
        std::env::var(INSTANCE_ID_ENV).unwrap_or_else(|_| std::process::id().to_string())
    })
}

/// The config directory override from GITTERM_V5_CONFIG_DIR, if set.
/// Dev/test instances set this so they can never read or write the
/// real ~/.config/gitterm-v5/* state of a running V5 instance.
pub fn config_dir_override() -> Option<&'static PathBuf> {
    CONFIG_DIR_OVERRIDE
        .get_or_init(|| resolve_config_dir_override(std::env::var_os(CONFIG_DIR_ENV)))
        .as_ref()
}

fn resolve_config_dir_override(raw: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let raw = raw?;
    if raw.is_empty() {
        eprintln!("{CONFIG_DIR_ENV} is set but empty; ignoring override");
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_relative() {
        eprintln!(
            "{CONFIG_DIR_ENV} is relative ({}); ignoring override",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// Get the shared (non-instance-specific) config directory
pub fn global_config_dir() -> PathBuf {
    if let Some(dir) = config_dir_override() {
        return dir.clone();
    }
    default_config_dir(&dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".config").join(CONFIG_DIR_NAME)
}

/// Get the base config directory for this instance
pub fn instance_config_dir() -> PathBuf {
    global_config_dir().join(format!("instance-{}", instance_id()))
}

/// Print instance info on startup
pub fn print_instance_info() {
    eprintln!("{APP_NAME} instance: {}", instance_id());
    eprintln!("Config directory: {}", instance_config_dir().display());
    if let Some(dir) = config_dir_override() {
        eprintln!("Config dir override ({CONFIG_DIR_ENV}): {}", dir.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance_id_generation() {
        let id = instance_id();
        assert!(!id.is_empty());
        // Should be consistent across calls
        assert_eq!(instance_id(), id);
    }

    #[test]
    fn test_instance_config_dir() {
        let dir = instance_config_dir();
        assert!(dir.to_string_lossy().contains("instance-"));
        assert!(dir.to_string_lossy().contains(instance_id()));
    }

    #[test]
    fn default_config_dir_is_isolated_from_earlier_versions() {
        let home = PathBuf::from("/test-home");
        let path = default_config_dir(&home);
        assert_eq!(path, PathBuf::from("/test-home/.config/gitterm-v5"));
        assert_ne!(path, PathBuf::from("/test-home/.config/gitterm-v4"));
        assert_ne!(path, PathBuf::from("/test-home/.config/gitterm"));
    }

    #[test]
    fn runtime_identity_environment_is_v5_only() {
        assert_eq!(APP_NAME, "GitTerm V5");
        assert_eq!(WINDOW_TITLE, "GitTerm V5 Development");
        assert_eq!(CONFIG_DIR_ENV, "GITTERM_V5_CONFIG_DIR");
        assert_eq!(INSTANCE_ID_ENV, "GITTERM_V5_INSTANCE_ID");
        assert_ne!(CONFIG_DIR_ENV, "GITTERM_V4_CONFIG_DIR");
        assert_ne!(INSTANCE_ID_ENV, "GITTERM_V4_INSTANCE_ID");
    }

    #[test]
    fn task_worktree_root_defaults_beneath_the_v5_config_root() {
        let root = PathBuf::from("/test-home/.config/gitterm-v5");
        assert_eq!(
            task_worktree_root_for_config_root(&root),
            PathBuf::from("/test-home/.config/gitterm-v5/worktrees")
        );

        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("task_worktree_root");
        let existing_config: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            existing_config.task_worktree_root,
            default_task_worktree_root()
        );
    }

    #[test]
    fn workspace_tab_task_link_is_optional_and_round_trips() {
        let existing: WorkspaceTabConfig =
            serde_json::from_value(serde_json::json!({ "dir": "/repo" })).unwrap();
        assert_eq!(existing.task_id, None);

        let linked: WorkspaceTabConfig = serde_json::from_value(serde_json::json!({
            "dir": "/worktrees/task-1",
            "task_id": "task-1"
        }))
        .unwrap();
        assert_eq!(linked.task_id.as_deref(), Some("task-1"));
        assert_eq!(linked.task_session_id, None);
        assert_eq!(
            serde_json::to_value(linked).unwrap()["task_id"],
            serde_json::json!("task-1")
        );

        let session: WorkspaceTabConfig = serde_json::from_value(serde_json::json!({
            "dir": "/worktrees/task-1",
            "task_id": "task-1",
            "task_session_id": "session-1"
        }))
        .unwrap();
        assert_eq!(session.task_session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn workspace_tab_session_uid_is_optional_and_round_trips() {
        let existing: WorkspaceTabConfig =
            serde_json::from_value(serde_json::json!({ "dir": "/repo" })).unwrap();
        assert_eq!(existing.session_uid, None);
        assert!(serde_json::to_value(&existing)
            .unwrap()
            .get("session_uid")
            .is_none());

        let tab: WorkspaceTabConfig = serde_json::from_value(serde_json::json!({
            "dir": "/repo",
            "session_uid": "7d1c2a4e-0b3f-4e8a-9c55-2f1e6d7a8b90"
        }))
        .unwrap();
        let encoded = serde_json::to_string(&tab).unwrap();
        let decoded: WorkspaceTabConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded.session_uid.as_deref(),
            Some("7d1c2a4e-0b3f-4e8a-9c55-2f1e6d7a8b90")
        );
    }

    #[test]
    fn test_resolve_config_dir_override_unset() {
        assert_eq!(resolve_config_dir_override(None), None);
    }

    #[test]
    fn test_resolve_config_dir_override_empty_ignored() {
        assert_eq!(resolve_config_dir_override(Some("".into())), None);
    }

    #[test]
    fn test_resolve_config_dir_override_relative_ignored() {
        assert_eq!(resolve_config_dir_override(Some("rel/path".into())), None);
    }

    #[test]
    fn test_resolve_config_dir_override_absolute() {
        assert_eq!(
            resolve_config_dir_override(Some("/tmp/gitterm-dev".into())),
            Some(PathBuf::from("/tmp/gitterm-dev"))
        );
    }

    #[test]
    fn review_defaults_to_an_opus_claude_subagent_and_old_configs_load() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            serialized["review"],
            serde_json::json!({
                "default_reviewer": "claude-subagent",
                "subagent_model": "opus",
                "max_concurrent_reviews": 2
            })
        );
        serialized.as_object_mut().unwrap().remove("review");
        let existing: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(existing.review, ReviewConfig::default());

        serialized["review"] = serde_json::json!({"default_reviewer": "codex"});
        let codex: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(codex.review.default_reviewer, ReviewerKind::Codex);
        assert_eq!(codex.review.subagent_model, "opus");
        assert_eq!(codex.review.codex_model, None);
        assert_eq!(codex.review.max_concurrent_reviews, 2);
    }

    #[test]
    fn the_worker_model_policy_is_seeded_and_old_configs_load() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            serialized["policy"],
            serde_json::json!({
                "claude": {"coordinator": "opus", "scoped": "sonnet", "judgment": "opus", "specialist": "fable"},
                "codex": {"coordinator": "gpt-6.1-sol", "scoped": "gpt-6-luna", "judgment": "gpt-6.1-sol", "specialist": "gpt-6-astra"}
            })
        );
        serialized.as_object_mut().unwrap().remove("policy");
        let existing: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(existing.policy, gitterm::workers::ModelPolicy::default());
    }

    #[test]
    fn usage_price_overrides_load_and_survive_a_save() {
        // An older config without `usage` has no overrides.
        let config: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(config.usage, gitterm::usage::UsageConfig::default());
        let config: Config = serde_json::from_str(
            r#"{"theme":"dark","usage":{"pricing":{
                "gpt-6-astra":{"input":1.0,"output":8.0,"cache_read":0.1,"cache_write":0.0},
                "claude-haiku-5-5":null}}}"#,
        )
        .unwrap();
        let astra = config.usage.pricing["gpt-6-astra"].unwrap();
        assert_eq!((astra.input, astra.output), (1.0, 8.0));
        assert_eq!(config.usage.pricing["claude-haiku-5-5"], None);
        let saved: Config = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(saved.usage, config.usage);
    }

    #[test]
    fn needs_you_notifications_default_on_and_can_be_switched_off() {
        // An older config without `notifications` notifies.
        let config: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(config.notifications.needs_you);
        let config: Config =
            serde_json::from_str(r#"{"theme":"dark","notifications":{}}"#).unwrap();
        assert!(config.notifications.needs_you);
        let config: Config =
            serde_json::from_str(r#"{"theme":"dark","notifications":{"needs_you":false}}"#)
                .unwrap();
        assert!(!config.notifications.needs_you);
        assert!(Config::default().notifications.needs_you);
    }

    #[test]
    fn new_chats_use_the_defaults_until_something_is_picked() {
        // An older config without `chat` gets the defaults.
        let config: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(config.chat, ChatDefaults::default());
        let mut chat = ChatDefaults {
            default_effort: Some("high".into()),
            ..ChatDefaults::default()
        };
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "opus".into(),
                effort: Some("high".into())
            }
        );
        // Picking a model keeps the effort a new chat would have had.
        chat.remember_model("sonnet");
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "sonnet".into(),
                effort: Some("high".into())
            }
        );
        chat.remember_effort(None);
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "sonnet".into(),
                effort: None
            }
        );
        // With remembering off, the configured defaults win again.
        chat.remember_last = false;
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "opus".into(),
                effort: Some("high".into())
            }
        );
        let json = serde_json::to_value(&chat).unwrap();
        assert_eq!(json["last"]["model"], "sonnet");
        let back: ChatDefaults = serde_json::from_value(json).unwrap();
        assert_eq!(back, chat);
    }

    #[test]
    fn terminal_log_mirroring_is_opt_in() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        let object = serialized.as_object_mut().unwrap();
        object.remove("terminal_log_mirroring_enabled");
        // The retired setting controlled the whole local content server. It
        // must not silently opt existing users into expensive history scans.
        object.insert("log_server_enabled".into(), serde_json::Value::Bool(true));

        let existing_config: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert!(!existing_config.terminal_log_mirroring_enabled);

        serialized["terminal_log_mirroring_enabled"] = serde_json::Value::Bool(true);
        let explicitly_enabled: Config = serde_json::from_value(serialized).unwrap();
        assert!(explicitly_enabled.terminal_log_mirroring_enabled);
    }

    #[cfg(feature = "stt")]
    #[test]
    fn stt_defaults_enabled_for_existing_and_new_configs() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        serialized.as_object_mut().unwrap().remove("stt_enabled");

        let existing_config: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert!(existing_config.stt_enabled);

        serialized["stt_enabled"] = serde_json::Value::Bool(false);
        let explicitly_disabled: Config = serde_json::from_value(serialized).unwrap();
        assert!(!explicitly_disabled.stt_enabled);
    }
}

/// Clean up this instance's config directory on exit
pub fn cleanup_instance_config() {
    let instance_dir = instance_config_dir();
    if instance_dir.exists() && instance_dir.to_string_lossy().contains(instance_id()) {
        let _ = std::fs::remove_dir_all(&instance_dir);
        eprintln!(
            "{APP_NAME} instance {} cleaned up config: {}",
            instance_id(),
            instance_dir.display()
        );
    }
}

// Default functions for serde
fn default_agent_color() -> WorkspaceColor {
    WorkspaceColor::Lavender
}

fn default_agent_presets() -> Vec<AgentPreset> {
    vec![
        AgentPreset {
            name: "Pi".to_string(),
            command: "pi".to_string(),
            resume_command: Some("pi --resume".to_string()),
            icon: "\u{03c0}".to_string(), // π
            color: WorkspaceColor::Pink,
        },
        AgentPreset {
            name: "Claude Code".to_string(),
            command: "claude".to_string(),
            resume_command: Some("claude --resume".to_string()),
            icon: "\u{276f}".to_string(),
            color: WorkspaceColor::Peach,
        },
        AgentPreset {
            name: "Codex".to_string(),
            command: "codex".to_string(),
            resume_command: Some("codex resume".to_string()),
            icon: "\u{2261}".to_string(),
            color: WorkspaceColor::Green,
        },
        AgentPreset {
            name: "Gemini".to_string(),
            command: "gemini".to_string(),
            resume_command: Some("gemini --resume".to_string()),
            icon: "G".to_string(),
            color: WorkspaceColor::Blue,
        },
    ]
}

pub fn default_task_worktree_root() -> PathBuf {
    task_worktree_root_for_config_root(&global_config_dir())
}

pub fn task_worktree_root_for_config_root(config_root: &Path) -> PathBuf {
    config_root.join("worktrees")
}

fn default_terminal_font() -> f32 {
    14.0
}

fn default_ui_font() -> f32 {
    13.0
}

fn default_sidebar_width() -> f32 {
    280.0
}

fn default_scrollback_lines() -> usize {
    100_000
}

fn default_console_height() -> f32 {
    200.0
}

fn default_console_expanded() -> bool {
    true
}

fn default_remote_session_shell() -> String {
    "/bin/zsh".to_string()
}

fn default_remote_session_tmux_path() -> String {
    "/opt/homebrew/bin/tmux".to_string()
}

pub fn default_remote_sessions() -> Vec<RemoteSessionConfig> {
    Vec::new()
}

pub fn default_remote_agents() -> Vec<RemoteAgentConfig> {
    Vec::new()
}

#[cfg(feature = "stt")]
fn default_stt_enabled() -> bool {
    true
}

// Local task slots gate agent launches, not tabs or sessions. The number is
// a guard against a runaway dispatch fan-out rather than a throughput
// budget — the task system's value is worktree organization, so the cap
// should rarely be the thing a user bumps into (TRU-129).
fn default_max_concurrent_local_tasks() -> usize {
    4
}

// Persistent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_terminal_font")]
    pub terminal_font_size: f32,
    #[serde(default = "default_ui_font")]
    pub ui_font_size: f32,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: usize,
    // Legacy field for migration
    #[serde(default)]
    pub font_size: Option<f32>,
    pub theme: String,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default = "default_console_height")]
    pub console_height: f32,
    #[serde(default = "default_console_expanded")]
    pub console_expanded: bool,
    /// Whether full terminal scrollback is mirrored into the local HTTP viewer.
    /// This is intentionally opt-in because collecting long histories is expensive.
    #[serde(default)]
    pub terminal_log_mirroring_enabled: bool,
    #[cfg(feature = "stt")]
    #[serde(default = "default_stt_enabled")]
    pub stt_enabled: bool,
    #[cfg(feature = "stt")]
    #[serde(default)]
    pub stt_model_path: Option<String>,
    #[serde(default = "default_agent_presets")]
    pub agent_presets: Vec<AgentPreset>,
    #[serde(default)]
    pub quick_commands: Vec<QuickCommand>,
    #[serde(default = "default_task_worktree_root")]
    pub task_worktree_root: PathBuf,
    /// How many local tasks may run agent sessions at once. Dispatching past
    /// the limit queues the task; capacity release starts the next in FIFO
    /// order. Zero is treated as 1 — a limit that can never start anything
    /// would strand every dispatch.
    #[serde(default = "default_max_concurrent_local_tasks")]
    pub max_concurrent_local_tasks: usize,
    /// Defaults for the chat tab's Review… button (TRU-142).
    #[serde(default)]
    pub review: ReviewConfig,
    /// Model and effort for new chats, and the last selection (TRU-143).
    #[serde(default)]
    pub chat: ChatDefaults,
    /// Which model each worker role runs on, per provider, for
    /// `delegate_task` (TRU-142 S6, TRU-144). Seeded with the routing table;
    /// see `gitterm::workers::ModelPolicy`.
    #[serde(default)]
    pub policy: gitterm::workers::ModelPolicy,
    /// Usage panel settings: per-model price overrides (TRU-145); see
    /// `gitterm::usage::UsageConfig`.
    #[serde(default)]
    pub usage: gitterm::usage::UsageConfig,
    /// macOS notifications (TRU-148).
    #[serde(default)]
    pub notifications: NotificationsConfig,
}

/// Which macOS notifications GitTerm posts (TRU-148). Edit the
/// `notifications` object in `config.json`:
///
/// ```json
/// "notifications": { "needs_you": true }
/// ```
///
/// `needs_you`: notify when a tab that is not in front starts waiting on the
/// human (an input or approval request, a blocked worker, a task needing
/// input).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub needs_you: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { needs_you: true }
    }
}

fn default_chat_model() -> String {
    "opus".to_string()
}

/// What a new chat tab starts with (TRU-143). The app has no settings
/// panel; edit the `chat` object in `config.json`:
///
/// ```json
/// "chat": { "default_model": "opus", "default_effort": "high", "remember_last": true }
/// ```
///
/// `default_model` is a Claude model alias (`default` leaves the choice to
/// the user's Claude settings); `default_effort` is one of the CLI's effort
/// levels (`low`, `medium`, `high`, `xhigh`, `max`), or absent for the
/// model's own default. With `remember_last`, a new chat starts with the
/// model and effort last picked on any chat (`last`, written by the app)
/// instead of the defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatDefaults {
    #[serde(default = "default_chat_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default = "default_true")]
    pub remember_last: bool,
    /// The model and effort last picked on any chat. One global selection,
    /// not per workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<ChatSelection>,
}

/// A chat's model and effort (`effort: None` is the model's default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSelection {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Default for ChatDefaults {
    fn default() -> Self {
        Self {
            default_model: default_chat_model(),
            default_effort: None,
            remember_last: true,
            last: None,
        }
    }
}

impl ChatDefaults {
    /// The configured defaults, ignoring any remembered selection.
    fn defaults(&self) -> ChatSelection {
        ChatSelection {
            model: self.default_model.clone(),
            effort: self.default_effort.clone(),
        }
    }

    /// What a new chat starts with: the last selection when remembering is
    /// on and one exists, else the configured defaults.
    pub fn new_chat_selection(&self) -> ChatSelection {
        match &self.last {
            Some(last) if self.remember_last => last.clone(),
            _ => self.defaults(),
        }
    }

    /// Record a model picked on a chat; the effort stays as the next new
    /// chat would have it.
    pub fn remember_model(&mut self, model: &str) {
        let mut selection = self.new_chat_selection();
        selection.model = model.to_string();
        self.last = Some(selection);
    }

    /// Record an effort picked on a chat.
    pub fn remember_effort(&mut self, effort: Option<&str>) {
        let mut selection = self.new_chat_selection();
        selection.effort = effort.map(str::to_string);
        self.last = Some(selection);
    }
}

/// Who reviews code when the chat tab's Review… button is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewerKind {
    /// A subagent spawned by the chat's own Claude through its Agent tool.
    #[default]
    ClaudeSubagent,
    /// An independent `codex exec review` run owned by GitTerm (TRU-142).
    Codex,
}

impl ReviewerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeSubagent => "claude-subagent",
            Self::Codex => "codex",
        }
    }
}

fn default_review_subagent_model() -> String {
    "opus".to_string()
}

/// Concurrent Codex runs (reviews and consults) before new requests queue
/// (TRU-142 decision D7).
pub const DEFAULT_MAX_CONCURRENT_REVIEWS: usize = 2;

fn default_max_concurrent_reviews() -> usize {
    DEFAULT_MAX_CONCURRENT_REVIEWS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewConfig {
    #[serde(default)]
    pub default_reviewer: ReviewerKind,
    /// The Agent tool `model` a Claude-subagent review runs on.
    #[serde(default = "default_review_subagent_model")]
    pub subagent_model: String,
    /// `codex -m <model>` for Codex reviews and consults; `None` keeps
    /// Codex's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_model: Option<String>,
    /// Codex runs (reviews and consults) that may run at once; further
    /// requests wait as `requested` and start when a slot frees. Zero is
    /// treated as 1.
    #[serde(default = "default_max_concurrent_reviews")]
    pub max_concurrent_reviews: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            default_reviewer: ReviewerKind::default(),
            subagent_model: default_review_subagent_model(),
            codex_model: None,
            max_concurrent_reviews: default_max_concurrent_reviews(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommand {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPreset {
    pub name: String,
    pub command: String,
    /// Command to resume the last session (e.g. "claude --resume", "codex resume")
    #[serde(default)]
    pub resume_command: Option<String>,
    #[serde(default)]
    pub icon: String,
    #[serde(default = "default_agent_color")]
    pub color: WorkspaceColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceColor {
    Lavender,
    Blue,
    Green,
    Peach,
    Pink,
    Yellow,
    Red,
    Teal,
}

impl WorkspaceColor {
    pub fn color(&self, theme: &crate::theme::AppTheme) -> iced::Color {
        use iced::color;
        match theme {
            crate::theme::AppTheme::Dark => match self {
                Self::Lavender => color!(0xb4befe),
                Self::Blue => color!(0x89b4fa),
                Self::Green => color!(0xa6e3a1),
                Self::Peach => color!(0xfab387),
                Self::Pink => color!(0xf5c2e7),
                Self::Yellow => color!(0xf9e2af),
                Self::Red => color!(0xf38ba8),
                Self::Teal => color!(0x94e2d5),
            },
            crate::theme::AppTheme::Light => match self {
                Self::Lavender => color!(0x7287fd),
                Self::Blue => color!(0x1e66f5),
                Self::Green => color!(0x40a02b),
                Self::Peach => color!(0xfe640b),
                Self::Pink => color!(0xea76cb),
                Self::Yellow => color!(0xdf8e1d),
                Self::Red => color!(0xd20f39),
                Self::Teal => color!(0x179299),
            },
        }
    }

    pub const ALL: [Self; 8] = [
        Self::Lavender,
        Self::Blue,
        Self::Green,
        Self::Peach,
        Self::Pink,
        Self::Yellow,
        Self::Red,
        Self::Teal,
    ];

    pub fn from_index(idx: usize) -> Self {
        Self::ALL[idx % Self::ALL.len()]
    }

    /// Pick the first color not already used by existing workspaces
    pub fn next_available(used: &[Self]) -> Self {
        Self::ALL
            .iter()
            .find(|c| !used.contains(c))
            .copied()
            .unwrap_or_else(|| Self::from_index(used.len()))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal_font_size: 14.0,
            ui_font_size: 13.0,
            sidebar_width: 280.0,
            scrollback_lines: 30_000,
            font_size: None,
            theme: "dark".to_string(),
            show_hidden: true,
            console_height: 200.0,
            console_expanded: true,
            terminal_log_mirroring_enabled: false,
            #[cfg(feature = "stt")]
            stt_enabled: true,
            #[cfg(feature = "stt")]
            stt_model_path: None,
            agent_presets: default_agent_presets(),
            quick_commands: Vec::new(),
            task_worktree_root: default_task_worktree_root(),
            max_concurrent_local_tasks: default_max_concurrent_local_tasks(),
            review: ReviewConfig::default(),
            chat: ChatDefaults::default(),
            policy: gitterm::workers::ModelPolicy::default(),
            usage: gitterm::usage::UsageConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        instance_config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(config) = serde_json::from_str(&contents) {
                    return config;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

// Workspace persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesFile {
    pub workspaces: Vec<WorkspaceConfig>,
    pub active_workspace: usize,
    #[serde(default = "default_remote_sessions")]
    pub remote_sessions: Vec<RemoteSessionConfig>,
    #[serde(
        default = "default_remote_agents",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remote_agents: Vec<RemoteAgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteSessionsFile {
    #[serde(default = "default_remote_sessions")]
    pub remote_sessions: Vec<RemoteSessionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteAgentsFile {
    #[serde(default = "default_remote_agents")]
    pub remote_agents: Vec<RemoteAgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionConfig {
    pub label: String,
    pub host_name: String,
    pub ssh_target: String,
    pub identity_file: String,
    #[serde(default = "default_remote_session_tmux_path")]
    pub tmux_path: String,
    pub session_name: String,
    pub remote_dir: String,
    #[serde(default = "default_remote_session_shell")]
    pub shell_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub auth: RemoteAgentAuthConfig,
    /// Shell command for plain sessions; run via `/bin/sh -lc` on the
    /// remote. Defaults to a login zsh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command: Option<String>,
    /// Launch commands per agent-preset name (lowercased), e.g.
    /// {"claude": "/Users/me/.local/bin/claude"}. Unlisted presets fall
    /// back to their lowercase name on the remote PATH.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub session_commands: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteAgentAuthConfig {
    Token { token_ref: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceLocationConfig {
    Local {
        root: String,
    },
    RemoteAgent {
        remote_id: String,
        workspace_id: String,
        root: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: String,
    pub abbrev: String,
    pub dir: String,
    pub color: WorkspaceColor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<WorkspaceLocationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_session: Option<RemoteSessionConfig>,
    /// Active tab index for this workspace. Older config files omit this; restore
    /// code falls back to the workspace-root tab when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<usize>,
    pub tabs: Vec<WorkspaceTabConfig>,
    #[serde(default)]
    pub run_command: Option<String>,
    #[serde(default)]
    pub bottom_terminals: Vec<BottomTerminalConfig>,
    /// Environment variables to inject into all terminal sessions in this workspace.
    /// Edit workspaces.json to add any vars without recompiling, e.g.:
    /// "env": { "LINEAR_WORKSPACE": "truinsights", "LINEAR_TEAM": "TRU", "GH_TOKEN": "..." }
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Whether this workspace is currently open. Closed workspaces are kept in
    /// workspaces.json so their env vars and settings are preserved for reopening.
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTabConfig {
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<String>,
    /// Tab kind discriminator. `None` or `Some("terminal")` means a terminal tab
    /// (the implicit default — older `workspaces.json` files predate this field).
    /// `Some("agent")` means a Claude Code / pi agent tab; see `agent_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_kind: Option<String>,
    /// Backend config for `tab_kind: "agent"`. Required when `tab_kind == Some("agent")`;
    /// ignored otherwise. Schema: `{"backend": "pi"|"claude", ...backend-specific fields}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<crate::tab::AgentBackendConfig>,
    /// Harness conversation owned by this tab (claude session uuid) — set
    /// when the tab was spawned by the Chats panel resume flow or by a
    /// picker launch with a pre-assigned id. Keys the one-live-tab-per-
    /// conversation registry rule (TRU-78) and survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
    /// Durable task owned by this view. Closing the tab does not remove the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Durable identity for this child view inside a task. Several tabs may
    /// share one task id; this id distinguishes their sessions across restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_session_id: Option<String>,
    /// Durable caller identity of this tab (uuid v4). Every tab carries one
    /// on its task MCP URL (`?caller=<uid>`) so the task server can tell
    /// which tab made a call. Older files predate it; a missing value is
    /// generated when the tab is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_uid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BottomTerminalConfig {
    pub dir: String,
}

impl WorkspacesFile {
    pub fn file_path() -> PathBuf {
        global_config_dir().join("workspaces.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    pub fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

impl RemoteSessionsFile {
    pub fn file_path() -> PathBuf {
        global_config_dir().join("remotes.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    pub fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

impl RemoteAgentsFile {
    pub fn file_path() -> PathBuf {
        global_config_dir().join("remote-agents.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    pub fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
