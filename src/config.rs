use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const APP_NAME: &str = "GitTerm V4";
pub const CONFIG_DIR_NAME: &str = "gitterm-v4";
pub const CONFIG_DIR_ENV: &str = "GITTERM_V4_CONFIG_DIR";
pub const INSTANCE_ID_ENV: &str = "GITTERM_V4_INSTANCE_ID";

// Global instance ID for this process
static INSTANCE_ID: OnceLock<String> = OnceLock::new();

// Config directory override, read once from GITTERM_V4_CONFIG_DIR
static CONFIG_DIR_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Get or generate the unique instance ID for this GitTerm process
pub fn instance_id() -> &'static str {
    INSTANCE_ID.get_or_init(|| {
        // Allow override via environment variable for testing
        std::env::var(INSTANCE_ID_ENV).unwrap_or_else(|_| std::process::id().to_string())
    })
}

/// The config directory override from GITTERM_V4_CONFIG_DIR, if set.
/// Dev/test instances set this so they can never read or write the
/// real ~/.config/gitterm-v4/* state of a running V4 instance.
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
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join(CONFIG_DIR_NAME)
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
    fn default_config_dir_is_isolated_from_v3() {
        let home = PathBuf::from("/test-home");
        let path = home.join(".config").join(CONFIG_DIR_NAME);
        assert_eq!(path, PathBuf::from("/test-home/.config/gitterm-v4"));
        assert_ne!(path, PathBuf::from("/test-home/.config/gitterm"));
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

fn default_log_server_enabled() -> bool {
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
    #[serde(default = "default_log_server_enabled")]
    pub log_server_enabled: bool,
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
            log_server_enabled: true,
            #[cfg(feature = "stt")]
            stt_enabled: true,
            #[cfg(feature = "stt")]
            stt_model_path: None,
            agent_presets: default_agent_presets(),
            quick_commands: Vec::new(),
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
