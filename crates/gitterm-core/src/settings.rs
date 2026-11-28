//! Frontend-neutral settings data and the persisted JSON field contract.
//!
//! Hosts supply their color adapter, usage DTO, and a transparent worktree-root
//! type whose Default/PartialEq implement their path omission policy. Filesystem
//! selection, environment resolution, and write authorization stay with the host.

use serde::{Deserialize, Serialize};

/// Iced's settings writer: preserve its serialization and atomic replacement exactly.
#[cfg(feature = "runtime")]
pub fn save_to(dir: &std::path::Path, config: &impl Serialize) -> Result<(), String> {
    let path = dir.join("settings.json");
    let json = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    crate::atomic_file::write_atomically(&path, &json)
}

/// Live's owner edits known fields without reformatting other fields or dropping
/// keys added by another desktop version. No second settings snapshot is cached.
#[cfg(feature = "runtime")]
pub(crate) fn save_desktop_to(
    path: &std::path::Path,
    config: &DesktopSettings,
) -> Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return save_to(path.parent().expect("settings parent"), config);
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let encode = |error| format!("encode {}: {error}", path.display());
    let parse = |error| format!("parse {}: {error}", path.display());
    let before: DesktopSettings = serde_json::from_str(&original).map_err(parse)?;
    let before = serde_json::to_value(before).map_err(encode)?;
    let after = serde_json::to_value(config).map_err(encode)?;
    if before == after {
        return Ok(());
    }
    // The original unflattened Serde schema also accepts sequence inputs.
    if !original.trim_start().starts_with('{') {
        return save_to(path.parent().expect("settings parent"), config);
    }
    let fields: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
        serde_json::from_str(&original).map_err(parse)?;
    let mut replacements = Vec::new();
    let mut additions = Vec::new();
    for (key, value) in after.as_object().expect("settings object") {
        if before.get(key) == Some(value) {
            continue;
        }
        let value = serde_json::to_string_pretty(value).map_err(encode)?;
        if let Some(raw) = fields.get(key) {
            let start = raw.get().as_ptr() as usize - original.as_ptr() as usize;
            replacements.push((start..start + raw.get().len(), value));
        } else {
            additions.push(format!(
                "{}: {value}",
                serde_json::to_string(key).map_err(encode)?
            ));
        }
    }
    let removals: Vec<_> = before
        .as_object()
        .expect("settings object")
        .keys()
        .filter(|key| after.get(*key).is_none() && fields.contains_key(*key))
        .cloned()
        .collect();
    if replacements.is_empty() && additions.is_empty() && removals.is_empty() {
        return Ok(());
    }
    replacements.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    drop(fields);
    let mut result = original;
    for (range, value) in replacements {
        result.replace_range(range, &value);
    }
    // Reparse between omissions: adjacent removed members share separators.
    // Removing one at a time keeps every intermediate document valid.
    for key in removals {
        let fields: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
            serde_json::from_str(&result).map_err(parse)?;
        let raw = fields[&key];
        let start = raw.get().as_ptr() as usize - result.as_ptr() as usize;
        let end = start + raw.get().len();
        let key_end = result[..start].rfind('"').expect("key end");
        let key_start = result[..key_end].rfind('"').expect("key start");
        let next = result[end..]
            .find(|c: char| !c.is_whitespace())
            .map(|n| end + n)
            .unwrap();
        let range = if result.as_bytes()[next] == b',' {
            key_start..next + 1
        } else if let Some(comma) = result[..key_start].rfind(',') {
            comma..end
        } else {
            key_start..end
        };
        result.replace_range(range, "");
    }
    if !additions.is_empty() {
        let end = result.rfind('}').expect("settings object end");
        let fields: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
            serde_json::from_str(&result).map_err(parse)?;
        let comma = if fields.is_empty() { "" } else { "," };
        result.insert_str(end, &format!("{comma}\n  {}\n", additions.join(",\n  ")));
    }
    // Validate the assembled document before replacing anything on disk.
    serde_json::from_str::<DesktopSettings>(&result)
        .map_err(|error| format!("validate edited {}: {error}", path.display()))?;
    crate::atomic_file::write_atomically(path, result.as_bytes())
}

// Default functions for serde
fn default_agent_color<C: From<WorkspaceColor>>() -> C {
    WorkspaceColor::Lavender.into()
}

fn default_agent_presets<C: From<WorkspaceColor>>() -> Vec<AgentPreset<C>> {
    vec![
        AgentPreset {
            name: "Pi".to_string(),
            command: "pi".to_string(),
            resume_command: Some("pi --resume".to_string()),
            icon: "\u{03c0}".to_string(), // π
            color: WorkspaceColor::Pink.into(),
        },
        AgentPreset {
            name: "Claude Code".to_string(),
            command: "claude".to_string(),
            resume_command: Some("claude --resume".to_string()),
            icon: "\u{276f}".to_string(),
            color: WorkspaceColor::Peach.into(),
        },
        AgentPreset {
            name: "Codex".to_string(),
            command: "codex".to_string(),
            resume_command: Some("codex resume".to_string()),
            icon: "\u{2261}".to_string(),
            color: WorkspaceColor::Green.into(),
        },
        AgentPreset {
            name: "Antigravity".to_string(),
            command: "agy".to_string(),
            resume_command: Some("agy --continue".to_string()),
            icon: "\u{2727}".to_string(),
            color: WorkspaceColor::Blue.into(),
        },
    ]
}

/// Field-level migration also covers older instance settings carried over
/// on first load. Only the exact retired executable is replaced.
fn deserialize_agent_presets<'de, D, C>(deserializer: D) -> Result<Vec<AgentPreset<C>>, D::Error>
where
    D: serde::Deserializer<'de>,
    C: Deserialize<'de> + From<WorkspaceColor>,
{
    let mut presets = Vec::<AgentPreset<C>>::deserialize(deserializer)?;
    for preset in &mut presets {
        if preset.command == "gemini" {
            *preset = default_agent_presets()
                .into_iter()
                .find(|preset| preset.command == "agy")
                .expect("default Antigravity preset");
        }
    }
    Ok(presets)
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_terminal_font() -> f32 {
    14.0
}

fn default_ui_font() -> f32 {
    13.0
}

pub const MIN_CHAT_FONT_SIZE: f32 = 11.0;
pub const MAX_CHAT_FONT_SIZE: f32 = 22.0;

fn default_chat_font() -> f32 {
    15.0
}

fn deserialize_chat_font<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f32, D::Error> {
    Ok(f32::deserialize(deserializer)?.clamp(MIN_CHAT_FONT_SIZE, MAX_CHAT_FONT_SIZE))
}

fn default_sidebar_width() -> f32 {
    360.0
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatWidth {
    Narrow,
    #[default]
    Medium,
    Wide,
    Full,
}

impl ChatWidth {
    pub const ALL: [Self; 4] = [Self::Narrow, Self::Medium, Self::Wide, Self::Full];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Narrow => "narrow",
            Self::Medium => "medium",
            Self::Wide => "wide",
            Self::Full => "full",
        }
    }
}

impl std::fmt::Display for ChatWidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Narrow => f.write_str("Narrow"),
            Self::Medium => f.write_str("Medium"),
            Self::Wide => f.write_str("Wide"),
            Self::Full => f.write_str("Full"),
        }
    }
}

/// The PR watch (TRU-197). Edit the `pr_watch` object in `settings.json`:
///
/// ```json
/// "pr_watch": { "auto_tell": true }
/// ```
///
/// `auto_tell`: send actionable PR news (merge conflicts, failed checks,
/// review comments, changes requested) to the coordinator's open chat by
/// itself when its turn is idle. Off, news only shows on the overview and
/// the chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrWatchConfig {
    #[serde(default = "default_true")]
    pub auto_tell: bool,
}

impl Default for PrWatchConfig {
    fn default() -> Self {
        Self { auto_tell: true }
    }
}

/// `"delegations": { "auto_tell": false }` in settings.json leaves
/// results on their cards for manual delivery. Defaults on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationsConfig {
    #[serde(default = "default_true")]
    pub auto_tell: bool,
}

impl Default for DelegationsConfig {
    fn default() -> Self {
        Self { auto_tell: true }
    }
}

/// The work-notes folder (TRU-201). Edit the `work_notes` object in
/// `settings.json`:
///
/// ```json
/// "work_notes": { "auto_commit": true }
/// ```
///
/// `auto_commit`: when `~/Work` is a git repo with an `origin` remote,
/// commit what changed there about ten minutes after the last change, and
/// at once when a work workspace is parked or closed, then push it. Off,
/// GitTerm runs no git there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkNotesConfig {
    #[serde(default = "default_true")]
    pub auto_commit: bool,
}

impl Default for WorkNotesConfig {
    fn default() -> Self {
        Self { auto_commit: true }
    }
}

/// Which macOS notifications GitTerm posts (TRU-148). Edit the
/// `notifications` object in `settings.json`:
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

/// Antigravity's effort-qualified chat default (TRU-273).
fn default_antigravity_model() -> String {
    "gemini-4-argon-medium".to_string()
}

/// What a new chat tab starts with (TRU-143). The app has no settings
/// panel; edit the `chat` object in `settings.json`:
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
///
/// Claude in Chrome is per workspace (`WorkspaceConfig::claude_in_chrome`,
/// TRU-181); a `claude_in_chrome` key here, from an earlier build, is
/// ignored.
///
/// `antigravity_model` and `opencode_model` are what new Antigravity and OpenCode
/// chats start on (TRU-153): a model id as the agent lists it, such as
/// `gemini-4-argon-medium` or `openrouter/google/gemini-3.6-flash`. An empty
/// `opencode_model` (the default) keeps the model OpenCode itself picks.
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
    #[serde(default = "default_antigravity_model")]
    pub antigravity_model: String,
    #[serde(default)]
    pub opencode_model: String,
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
            antigravity_model: default_antigravity_model(),
            opencode_model: String::new(),
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
#[serde(bound(deserialize = "C: Deserialize<'de> + From<WorkspaceColor>"))]
pub struct AgentPreset<C = WorkspaceColor> {
    pub name: String,
    pub command: String,
    /// Command to resume the last session (e.g. "claude --resume", "codex resume")
    #[serde(default)]
    pub resume_command: Option<String>,
    #[serde(default)]
    pub icon: String,
    #[serde(default = "default_agent_color")]
    pub color: C,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

// Both host field sets use the original, unflattened Serde layout. Flattening
// would stop accepting sequence inputs and some ignored unknown values.
macro_rules! settings_data {
    ($name:ident; $( $(#[$voice_attr:meta])* $voice_field:ident: $voice_type:ty = $voice_default:expr, )*) => {
        // Persistent configuration: the shared `settings.json` (TRU-181), see
        // `SETTINGS_FILE`. Every field is a setting the user expects to keep.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(rename = "Config", bound(
            serialize = "C: Serialize, U: Serialize, R: Serialize + Default + PartialEq",
            deserialize = "C: Deserialize<'de> + From<WorkspaceColor>, U: Deserialize<'de> + Default, R: Deserialize<'de> + Default + PartialEq"
        ))]
        pub struct $name<C, U, R> {
            /// Editor used by "Open in editor"; custom values are argv templates.
            #[serde(default = "default_editor")]
            pub editor: String,
            #[serde(default = "default_terminal_font")]
            pub terminal_font_size: f32,
            #[serde(default = "default_ui_font")]
            pub ui_font_size: f32,
            #[serde(
                default = "default_chat_font",
                deserialize_with = "deserialize_chat_font"
            )]
            pub chat_font_size: f32,
            #[serde(default)]
            pub chat_width: ChatWidth,
            #[serde(default = "default_sidebar_width")]
            pub sidebar_width: f32,
            #[serde(default = "default_scrollback_lines")]
            pub scrollback_lines: usize,
            // Legacy field for migration
            #[serde(default)]
            pub font_size: Option<f32>,
            /// Defaulted so a hand-written `settings.json` may leave it out.
            #[serde(default = "default_theme")]
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
            $(
                $(#[$voice_attr])*
                pub $voice_field: $voice_type,
            )*
            #[serde(
                default = "default_agent_presets",
                deserialize_with = "deserialize_agent_presets"
            )]
            pub agent_presets: Vec<AgentPreset<C>>,
            #[serde(default)]
            pub quick_commands: Vec<QuickCommand>,
            /// Written only when changed from the default: the shared
            /// `settings.json` must not pin one config dir's worktree root, so a
            /// copy of it in another (isolated) config dir keeps its own.
            #[serde(
                default,
                skip_serializing_if = "is_default_task_worktree_root"
            )]
            pub task_worktree_root: R,
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
            /// see `crate::workers::ModelPolicy`.
            #[serde(default)]
            pub policy: crate::workers::ModelPolicy,
            /// Usage panel settings: per-model price overrides (TRU-145); see
            /// the host usage DTO.
            #[serde(default)]
            pub usage: U,
            /// macOS notifications (TRU-148).
            #[serde(default)]
            pub notifications: NotificationsConfig,
            /// The PR watch (TRU-197).
            #[serde(default)]
            pub pr_watch: PrWatchConfig,
            /// Automatic delegation results in chat (TRU-218).
            #[serde(default)]
            pub delegations: DelegationsConfig,
            /// ~/Work committing and pushing itself (TRU-201).
            #[serde(default)]
            pub work_notes: WorkNotesConfig,
        }

        impl<C: From<WorkspaceColor>, U: Default, R: Default> Default for $name<C, U, R> {
            fn default() -> Self {
                Self {
                    editor: default_editor(),
                    terminal_font_size: 14.0,
                    ui_font_size: 13.0,
                    chat_font_size: default_chat_font(),
                    chat_width: ChatWidth::default(),
                    sidebar_width: 360.0,
                    scrollback_lines: 30_000,
                    font_size: None,
                    theme: "dark".to_string(),
                    show_hidden: true,
                    console_height: 200.0,
                    console_expanded: true,
                    terminal_log_mirroring_enabled: false,
                    $($voice_field: $voice_default,)*
                    agent_presets: default_agent_presets(),
                    quick_commands: Vec::new(),
                    task_worktree_root: R::default(),
                    max_concurrent_local_tasks: default_max_concurrent_local_tasks(),
                    review: ReviewConfig::default(),
                    chat: ChatDefaults::default(),
                    policy: crate::workers::ModelPolicy::default(),
                    usage: U::default(),
                    notifications: NotificationsConfig::default(),
                    pr_watch: PrWatchConfig::default(),
                    delegations: DelegationsConfig::default(),
                    work_notes: WorkNotesConfig::default(),
                }
            }
        }

    };
}

settings_data!(Settings;);
settings_data!(SettingsWithVoice;
    #[serde(default = "default_stt_enabled")]
    stt_enabled: bool = true,
    #[serde(default)]
    stt_model_path: Option<String> = None,
);

/// Complete desktop wire format, including voice fields even in a host without
/// voice support. `None` remembers an absent field; `Some(None)` remembers null.
/// An absent worktree root resolves to the owner's root joined with `worktrees`.
/// It must stay absent on save, just as Iced omits its default root.
pub type DesktopSettings =
    DesktopSettingsData<WorkspaceColor, crate::usage::UsageConfig, DesktopWorktreeRoot>;

/// `None` is the omitted, root-relative default; explicit paths use Iced's
/// non-null string contract. The owner resolves the default without global IO.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DesktopWorktreeRoot(pub Option<std::path::PathBuf>);

impl<'de> Deserialize<'de> for DesktopWorktreeRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        std::path::PathBuf::deserialize(deserializer).map(|path| Self(Some(path)))
    }
}

settings_data!(DesktopSettingsData;
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_bool")]
    stt_enabled: Option<bool> = Some(true),
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_nullable")]
    stt_model_path: Option<Option<String>> = Some(None),
);

fn present_bool<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

fn present_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

fn default_editor() -> String {
    "vscode".to_string()
}

fn default_true() -> bool {
    true
}

fn is_default_task_worktree_root<R: Default + PartialEq>(root: &R) -> bool {
    *root == R::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    // The concrete usage model remains in the Iced host. These test-only DTOs
    // check the generic seam against the original complete persisted bytes.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct Usage {
        #[serde(default)]
        pricing: BTreeMap<String, Option<Price>>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Price {
        input: f64,
        output: f64,
        cache_read: f64,
        cache_write: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_1h: Option<f64>,
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/stage45/settings")
                .join(name),
        )
        .unwrap()
    }

    macro_rules! pin_shape {
        ($test:ident, $shape:ident, $suffix:literal) => {
            #[test]
            fn $test() {
                type Data = $shape<WorkspaceColor, Usage, PathBuf>;
                for name in ["full", "legacy", "unknown", "missing"] {
                    let data: Data =
                        serde_json::from_str(&fixture(&format!("{name}.input.json"))).unwrap();
                    let expected = fixture(&format!("{name}{}.json", $suffix));
                    let saved = serde_json::to_string_pretty(&data).unwrap();
                    assert_eq!(saved, expected, "{name}");
                    let back: Data = serde_json::from_str(&saved).unwrap();
                    assert_eq!(serde_json::to_string_pretty(&back).unwrap(), saved);
                }
                assert_eq!(
                    serde_json::to_string_pretty(&Data::default()).unwrap(),
                    fixture(concat!("default", $suffix, ".json"))
                );
                for input in ["[]", r#"{"future":1e400}"#] {
                    let data: Data = serde_json::from_str(input).unwrap();
                    assert_eq!(
                        serde_json::to_string_pretty(&data).unwrap(),
                        fixture(concat!("missing", $suffix, ".json"))
                    );
                }
                let sequence: Data = serde_json::from_str(&fixture(concat!(
                    "full-sequence",
                    $suffix,
                    ".input.json"
                )))
                .unwrap();
                assert_eq!(
                    serde_json::to_string_pretty(&sequence).unwrap(),
                    fixture(concat!("full", $suffix, ".json"))
                );
                let mut refused: Vec<String> =
                    serde_json::from_str(&fixture("refused.input.json")).unwrap();
                refused.push(fixture("malformed.input.json"));
                for input in refused {
                    assert!(serde_json::from_str::<Data>(&input).is_err(), "{input}");
                }
            }
        };
    }

    pin_shape!(voice_enabled_fixtures, SettingsWithVoice, "");
    pin_shape!(voice_disabled_fixtures, Settings, ".no-stt");
}

#[cfg(all(test, feature = "runtime"))]
mod view_preferences_tests {
    use super::*;
    use crate::desktop_store::DesktopStore;

    #[test]
    fn iced_settings_writer_keeps_original_saved_bytes() {
        let root = tempfile::tempdir().unwrap();
        let settings = DesktopSettings::default();
        save_to(root.path(), &settings).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("settings.json")).unwrap(),
            serde_json::to_vec_pretty(&settings).unwrap()
        );
    }

    #[test]
    fn view_preferences_preserve_unknown_and_untouched_bytes_and_noop_saves() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let original = "{\n \"theme\" : \"dark\", \"sidebar_width\":360,\n \"future\" : {\"escaped\": \"雪 \\\" }\", \"values\":[1e2,2]},\n \"usage\" : { \"future\" : [true, null] }\n}\n";
        std::fs::write(&path, original).unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        store.edit_settings(|_| {}).wait().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        store
            .edit_settings(|s| {
                s.theme = "light".into();
                s.sidebar_width = 450.;
            })
            .wait()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original
                .replace("\"dark\"", "\"light\"")
                .replace(":360,", ":450.0,")
        );
        store.drain().wait().unwrap();
    }

    #[test]
    fn view_preferences_add_only_changed_fields_and_migrate_legacy_font() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        std::fs::write(&path, "{ \"font_size\" : 18, \"future\":42 }\n").unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        store
            .edit_settings(|s| {
                s.font_size = None;
                s.ui_font_size = 17.;
                s.terminal_font_size = 19.;
            })
            .wait()
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"future\":42"));
        let actual: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            actual,
            serde_json::json!({"font_size":null,"future":42,"ui_font_size":17.0,"terminal_font_size":19.0})
        );
    }

    #[test]
    fn view_preferences_normalize_root_without_changing_other_bytes() {
        for extra in ["", ", \"future\": {\"commas\":[1,2]}"] {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let path = root.path().join("settings.json");
            let configured = serde_json::to_string(&root.path().join("worktrees")).unwrap();
            std::fs::write(
                &path,
                format!("{{ \"task_worktree_root\":{configured}{extra} }}"),
            )
            .unwrap();
            let store = DesktopStore::open(root.path()).unwrap();
            store
                .edit_settings(|s| s.theme = "light".into())
                .wait()
                .unwrap();
            let actual: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(actual["theme"], "light");
            assert!(actual.get("task_worktree_root").is_none());
            if !extra.is_empty() {
                assert_eq!(actual["future"], serde_json::json!({"commas":[1,2]}));
            }
        }
    }

    #[test]
    fn view_preferences_preserve_optional_voice_field_omission() {
        for original in [
            r#"{"stt_enabled":true,"stt_model_path":"fixture-model","future" : [1, 2]}"#,
            r#"{"future" : [1, 2],"stt_enabled":true,"stt_model_path":null}"#,
            r#"{"stt_enabled":true,"future" : [1, 2],"stt_model_path":null}"#,
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("settings.json");
            std::fs::write(&path, original).unwrap();
            let store = DesktopStore::open(root.path()).unwrap();
            store
                .edit_settings(|settings| {
                    settings.stt_enabled = None;
                    settings.stt_model_path = None;
                })
                .wait()
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                r#"{"future" : [1, 2]}"#
            );
        }
    }

    #[test]
    fn view_preferences_keep_iced_sequence_input_compatibility() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        std::fs::write(&path, "[\"fixture-editor\"]").unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        store
            .edit_settings(|settings| settings.theme = "light".into())
            .wait()
            .unwrap();
        let config: DesktopSettings =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(config.editor, "fixture-editor");
        assert_eq!(config.theme, "light");
    }

    #[test]
    fn view_preferences_refuse_newly_malformed_settings_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        std::fs::write(&path, "{ malformed").unwrap();
        assert!(store
            .edit_settings(|s| s.theme = "light".into())
            .wait()
            .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{ malformed");
    }
}
