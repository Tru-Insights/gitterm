//! Implementation workers a chat tab delegates (TRU-142 slice S6, model
//! policy TRU-144): which configured preset runs the worker, which model it
//! gets, and how the model reaches the preset's CLI.
//!
//! Pure rules only. The Iced app and the headless smoke resolve a
//! `delegate_task` request through `resolve_worker` and launch the preset
//! command through `with_model_flag`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::review::is_model_name;

/// What kind of work a worker is trusted with. The coordinator (the chat
/// tab's own Claude) is not a worker role: Tracey picks that model herself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    /// Routine coding with resolved decisions and established patterns.
    #[default]
    Scoped,
    /// Harder coding that needs technical judgment or new design.
    Judgment,
    /// Review, investigation, brainstorming.
    Specialist,
}

impl WorkerRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Judgment => "judgment",
            Self::Specialist => "specialist",
        }
    }
}

/// Which CLI a preset command starts, by its executable's basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerProvider {
    Claude,
    Codex,
    /// pi, gemini or a user-defined command: GitTerm does not know its
    /// model flag, so it gets no model.
    Other,
}

impl WorkerProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Other => "other",
        }
    }

    /// The CLI's model option: `claude --model <m>`, `codex -m <m>`
    /// (both checked against `--help` of claude 2.1.295 and codex-cli
    /// 0.161.0).
    pub fn model_flag(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("--model"),
            Self::Codex => Some("-m"),
            Self::Other => None,
        }
    }
}

fn executable_name(command: &str) -> String {
    let executable = command.split_whitespace().next().unwrap_or_default();
    let executable = executable.trim_matches(['\'', '"']);
    executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase()
}

pub fn provider_for_command(command: &str) -> WorkerProvider {
    match executable_name(command).as_str() {
        "claude" => WorkerProvider::Claude,
        "codex" => WorkerProvider::Codex,
        _ => WorkerProvider::Other,
    }
}

/// One provider's model per role. A missing role means the CLI's own
/// default model. `coordinator` is recorded for reference only: GitTerm
/// never launches the coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoleModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specialist: Option<String>,
}

impl RoleModels {
    pub fn for_role(&self, role: WorkerRole) -> Option<&str> {
        match role {
            WorkerRole::Scoped => self.scoped.as_deref(),
            WorkerRole::Judgment => self.judgment.as_deref(),
            WorkerRole::Specialist => self.specialist.as_deref(),
        }
    }
}

fn seed(coordinator: &str, scoped: &str, judgment: &str, specialist: &str) -> RoleModels {
    RoleModels {
        coordinator: Some(coordinator.to_string()),
        scoped: Some(scoped.to_string()),
        judgment: Some(judgment.to_string()),
        specialist: Some(specialist.to_string()),
    }
}

/// Claude aliases the CLI accepts (`claude --help`: "an alias for the
/// latest model (e.g. 'fable', 'opus', or 'sonnet')").
fn claude_seed() -> RoleModels {
    seed("opus", "sonnet", "opus", "fable")
}

/// Codex model slugs, as listed by `codex debug models` (codex-cli 0.161.0,
/// 2026-10-08): GPT-6.1 Sol, GPT-6 Luna, GPT-6 Astra.
fn codex_seed() -> RoleModels {
    seed("gpt-6.1-sol", "gpt-6-luna", "gpt-6.1-sol", "gpt-6-astra")
}

/// Which model each worker role gets, per provider (`policy` in
/// `settings.json`). Seeded with Tracey's table (2026-10-08): coordinator
/// Opus 5.5 / GPT-6.1 Sol; scoped worker Sonnet / GPT-6 Luna; judgment
/// worker Opus 5.5 / GPT-6.1 Sol; specialist Fable / GPT-6 Astra.
///
/// ```json
/// "policy": {
///   "claude": { "coordinator": "opus", "scoped": "sonnet", "judgment": "opus", "specialist": "fable" },
///   "codex":  { "coordinator": "gpt-6.1-sol", "scoped": "gpt-6-luna", "judgment": "gpt-6.1-sol", "specialist": "gpt-6-astra" }
/// }
/// ```
///
/// A provider object that is present replaces that provider's seed: a role
/// left out of it runs on the CLI's default model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPolicy {
    #[serde(default = "claude_seed")]
    pub claude: RoleModels,
    #[serde(default = "codex_seed")]
    pub codex: RoleModels,
}

impl Default for ModelPolicy {
    fn default() -> Self {
        Self {
            claude: claude_seed(),
            codex: codex_seed(),
        }
    }
}

impl ModelPolicy {
    pub fn model_for(&self, provider: WorkerProvider, role: WorkerRole) -> Option<&str> {
        match provider {
            WorkerProvider::Claude => self.claude.for_role(role),
            WorkerProvider::Codex => self.codex.for_role(role),
            WorkerProvider::Other => None,
        }
    }
}

/// A configured agent preset, as far as worker choice needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetRef<'a> {
    pub name: &'a str,
    pub command: &'a str,
}

/// Where a worker's model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// The `model` the caller asked for.
    Requested,
    /// The policy's model for the provider and role.
    Policy,
    /// The preset command already names a model; it is left alone.
    Preset,
    /// No model: the CLI's own default.
    CliDefault,
}

/// The preset and model a `delegate_task` request resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerChoice {
    pub preset_index: usize,
    pub preset_name: String,
    pub provider: WorkerProvider,
    pub role: WorkerRole,
    /// The model GitTerm adds to the launch; `None` leaves the command's
    /// own choice (a pinned preset or the CLI default).
    pub model: Option<String>,
    pub model_source: ModelSource,
}

/// The preset a worker runs on when the request names none: the first
/// Codex preset, else the first Claude one.
pub fn default_worker_preset(presets: &[PresetRef<'_>]) -> Option<usize> {
    let first = |provider| {
        presets
            .iter()
            .position(|preset| provider_for_command(preset.command) == provider)
    };
    first(WorkerProvider::Codex).or_else(|| first(WorkerProvider::Claude))
}

/// Whether the command already passes a model (`--model x`, `--model=x`,
/// or `-m x`). Such a preset keeps its own model.
pub fn command_pins_model(command: &str) -> bool {
    command.split_whitespace().skip(1).any(|word| {
        word == "--model" || word.starts_with("--model=") || word == "-m" || word.starts_with("-m=")
    })
}

/// Resolves which preset and model carry out a worker. `preset_name`
/// matches case-insensitively; `role` defaults to scoped; an explicit
/// `model` overrides the policy and is an error where GitTerm cannot pass
/// it (a provider without a known model flag, or a preset that pins its
/// own model).
pub fn resolve_worker(
    presets: &[PresetRef<'_>],
    preset_name: Option<&str>,
    role: Option<WorkerRole>,
    model: Option<&str>,
    policy: &ModelPolicy,
) -> Result<WorkerChoice, String> {
    let available = || {
        presets
            .iter()
            .map(|preset| preset.name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let preset_index = match preset_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => presets
            .iter()
            .position(|preset| preset.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                format!(
                    "agent preset {name:?} is not configured. Configured presets: {}",
                    available()
                )
            })?,
        None => default_worker_preset(presets).ok_or_else(|| {
            format!(
                "no Codex or Claude Code preset is configured to run a worker; name one with \
                 preset_name. Configured presets: {}",
                available()
            )
        })?,
    };
    let preset = presets[preset_index];
    let provider = provider_for_command(preset.command);
    let role = role.unwrap_or_default();
    let requested = model.map(str::trim).filter(|model| !model.is_empty());
    let pinned = command_pins_model(preset.command);
    let (model, model_source) = match requested {
        Some(model) => {
            if !is_model_name(model) {
                return Err(format!(
                    "model {model:?} is not a model name (lowercase letters, digits, '-', '.', '_')"
                ));
            }
            if provider == WorkerProvider::Other {
                return Err(format!(
                    "preset {:?} runs {:?}, whose model flag GitTerm does not know; drop model, \
                     or pick a Claude or Codex preset",
                    preset.name,
                    executable_name(preset.command)
                ));
            }
            if pinned {
                return Err(format!(
                    "preset {:?} already names a model in its command ({}); drop model or edit \
                     the preset",
                    preset.name, preset.command
                ));
            }
            (Some(model.to_string()), ModelSource::Requested)
        }
        None if pinned => (None, ModelSource::Preset),
        None => match policy.model_for(provider, role) {
            Some(model) if is_model_name(model) => (Some(model.to_string()), ModelSource::Policy),
            Some(model) => {
                return Err(format!(
                    "the model policy's {} {} model {model:?} is not a model name; fix policy in \
                     settings.json",
                    provider.label(),
                    role.label()
                ))
            }
            None => (None, ModelSource::CliDefault),
        },
    };
    Ok(WorkerChoice {
        preset_index,
        preset_name: preset.name.to_string(),
        provider,
        role,
        model,
        model_source,
    })
}

/// The launch command with the model option inserted right after the
/// executable (`claude --model sonnet …`, `codex -m gpt-6-luna …`). It never
/// rewrites the preset's own arguments: a command that already names a
/// model, or a CLI whose model flag GitTerm does not know, is an error.
pub fn with_model_flag(command: &str, model: &str) -> Result<String, String> {
    if !is_model_name(model) {
        return Err(format!("model {model:?} is not a model name"));
    }
    let provider = provider_for_command(command);
    let Some(flag) = provider.model_flag() else {
        return Err(format!(
            "GitTerm does not know the model option of {:?}",
            executable_name(command)
        ));
    };
    if command_pins_model(command) {
        return Err(format!("the command already names a model: {command}"));
    }
    let trimmed = command.trim();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    Ok(format!(
        "{} {flag} {model}{}",
        &trimmed[..end],
        &trimmed[end..]
    ))
}

/// A worker's or Codex run's reasoning effort (TRU-212).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Codex's `-c` value: `model_reasoning_effort` is the config key
    /// (`~/.codex/config.toml` uses it; codex-cli 0.164.0 knows low,
    /// medium, high and xhigh), the value a TOML string.
    pub fn codex_config_value(self) -> String {
        format!("model_reasoning_effort=\"{}\"", self.label())
    }
}

/// `command` with the worker's effort and extra writable folders added
/// after the executable (TRU-212). Codex: `-c model_reasoning_effort="…"`
/// and `--add-dir <dir>` (codex-cli 0.164.0 `--help`); Claude:
/// `--effort <level>` and `--add-dir <dir>` (claude 2.1.318 `--help`).
/// Other CLIs: an error, since GitTerm does not know their options. The
/// folders are shell-quoted; with neither option the command is unchanged.
pub fn with_launch_options(
    command: &str,
    effort: Option<ReasoningEffort>,
    add_dirs: &[std::path::PathBuf],
) -> Result<String, String> {
    with_launch_options_for_shell(command, effort, add_dirs, cfg!(unix))
}

fn with_launch_options_for_shell(
    command: &str,
    effort: Option<ReasoningEffort>,
    add_dirs: &[std::path::PathBuf],
    posix_shell: bool,
) -> Result<String, String> {
    if !posix_shell {
        let option = if !add_dirs.is_empty() {
            Some("add_dirs")
        } else {
            effort.map(|_| "effort")
        };
        if let Some(option) = option {
            return Err(format!(
                "{option} requires a POSIX shell; GitTerm cannot safely pass it to this worker shell"
            ));
        }
    }
    if effort.is_none() && add_dirs.is_empty() {
        return Ok(command.to_string());
    }
    let provider = provider_for_command(command);
    let mut options: Vec<String> = Vec::new();
    match (provider, effort) {
        (_, None) => {}
        (WorkerProvider::Codex, Some(effort)) => {
            options.push("-c".to_string());
            options.push(crate::guidelines::shell_word(&effort.codex_config_value()));
        }
        (WorkerProvider::Claude, Some(effort)) => {
            options.push("--effort".to_string());
            options.push(effort.label().to_string());
        }
        (WorkerProvider::Other, Some(_)) => {
            return Err(format!(
                "GitTerm does not know the reasoning effort option of {:?}; effort works with \
                 Codex and Claude Code presets",
                executable_name(command)
            ))
        }
    }
    if !add_dirs.is_empty() && provider == WorkerProvider::Other {
        return Err(format!(
            "GitTerm does not know the extra folder option of {:?}; add_dirs works with Codex \
             and Claude Code presets",
            executable_name(command)
        ));
    }
    for dir in add_dirs {
        options.push("--add-dir".to_string());
        options.push(crate::guidelines::shell_word(&dir.to_string_lossy()));
    }
    let trimmed = command.trim();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    Ok(format!(
        "{} {}{}",
        &trimmed[..end],
        options.join(" "),
        &trimmed[end..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_posix_shells_refuse_interpolated_launch_options() {
        let dirs = [std::path::PathBuf::from(r"C:\work\a&whoami&b")];
        for command in ["codex", "claude"] {
            assert!(with_launch_options_for_shell(command, None, &dirs, false)
                .unwrap_err()
                .contains("add_dirs"));
            assert!(with_launch_options_for_shell(
                command,
                Some(ReasoningEffort::High),
                &[],
                false
            )
            .unwrap_err()
            .contains("effort"));
            assert_eq!(
                with_launch_options_for_shell(command, None, &[], false).unwrap(),
                command
            );
        }
    }

    #[test]
    fn launch_options_reach_codex_and_claude_and_refuse_other_clis() {
        let dirs = [
            std::path::PathBuf::from("/work/notes"),
            std::path::PathBuf::from("/repos/api wt"),
        ];
        assert_eq!(
            with_launch_options_for_shell(
                "codex -m gpt-6-luna",
                Some(ReasoningEffort::Xhigh),
                &dirs,
                true
            )
            .unwrap(),
            "codex -c 'model_reasoning_effort=\"xhigh\"' --add-dir /work/notes --add-dir \
             '/repos/api wt' -m gpt-6-luna"
        );
        assert_eq!(
            with_launch_options_for_shell("claude", Some(ReasoningEffort::High), &dirs[..1], true)
                .unwrap(),
            "claude --effort high --add-dir /work/notes"
        );
        assert_eq!(
            with_launch_options("pi", None, &[]).unwrap(),
            "pi",
            "no options leave any command alone"
        );
        assert!(with_launch_options("pi", Some(ReasoningEffort::Low), &[])
            .unwrap_err()
            .contains("effort"));
        assert!(with_launch_options("gemini", None, &dirs)
            .unwrap_err()
            .contains("add_dirs"));
        let effort: ReasoningEffort = serde_json::from_str("\"xhigh\"").unwrap();
        assert_eq!(effort, ReasoningEffort::Xhigh);
        assert!(serde_json::from_str::<ReasoningEffort>("\"max\"").is_err());
    }

    fn presets() -> Vec<PresetRef<'static>> {
        vec![
            PresetRef {
                name: "Pi",
                command: "pi",
            },
            PresetRef {
                name: "Claude Code",
                command: "claude",
            },
            PresetRef {
                name: "Codex",
                command: "/opt/homebrew/bin/codex",
            },
            PresetRef {
                name: "Gemini",
                command: "gemini",
            },
        ]
    }

    #[test]
    fn the_policy_is_seeded_with_the_routing_table_and_partial_configs_load() {
        let policy = ModelPolicy::default();
        let models = |provider| {
            [
                WorkerRole::Scoped,
                WorkerRole::Judgment,
                WorkerRole::Specialist,
            ]
            .map(|role| policy.model_for(provider, role).map(str::to_string))
        };
        assert_eq!(
            models(WorkerProvider::Claude),
            [
                Some("sonnet".into()),
                Some("opus".into()),
                Some("fable".into())
            ]
        );
        assert_eq!(
            models(WorkerProvider::Codex),
            [
                Some("gpt-6-luna".into()),
                Some("gpt-6.1-sol".into()),
                Some("gpt-6-astra".into())
            ]
        );
        assert_eq!(policy.claude.coordinator.as_deref(), Some("opus"));
        assert_eq!(policy.codex.coordinator.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(
            policy.model_for(WorkerProvider::Other, WorkerRole::Scoped),
            None
        );

        // A config without `policy` fields gets the seed; a provider object
        // replaces that provider's seed, and a missing role is the CLI default.
        let loaded: ModelPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded, policy);
        let custom: ModelPolicy =
            serde_json::from_str(r#"{"codex":{"scoped":"gpt-6-sol"}}"#).unwrap();
        assert_eq!(custom.claude, policy.claude);
        assert_eq!(
            custom.model_for(WorkerProvider::Codex, WorkerRole::Scoped),
            Some("gpt-6-sol")
        );
        assert_eq!(
            custom.model_for(WorkerProvider::Codex, WorkerRole::Judgment),
            None
        );
    }

    #[test]
    fn workers_default_to_codex_then_claude_and_to_the_scoped_role() {
        let policy = ModelPolicy::default();
        let choice = resolve_worker(&presets(), None, None, None, &policy).unwrap();
        assert_eq!(choice.preset_name, "Codex");
        assert_eq!(choice.preset_index, 2);
        assert_eq!(choice.provider, WorkerProvider::Codex);
        assert_eq!(choice.role, WorkerRole::Scoped);
        assert_eq!(choice.model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(choice.model_source, ModelSource::Policy);

        // Without a Codex preset the first Claude preset runs the worker.
        let no_codex: Vec<_> = presets()
            .into_iter()
            .filter(|preset| preset.name != "Codex")
            .collect();
        let choice =
            resolve_worker(&no_codex, None, Some(WorkerRole::Judgment), None, &policy).unwrap();
        assert_eq!(choice.preset_name, "Claude Code");
        assert_eq!(choice.model.as_deref(), Some("opus"));

        let only_pi = [PresetRef {
            name: "Pi",
            command: "pi",
        }];
        let error = resolve_worker(&only_pi, None, None, None, &policy).unwrap_err();
        assert!(error.contains("Configured presets: Pi"), "{error}");
    }

    #[test]
    fn a_named_preset_and_an_explicit_model_override_the_policy() {
        let policy = ModelPolicy::default();
        let choice = resolve_worker(
            &presets(),
            Some("claude code"),
            Some(WorkerRole::Specialist),
            None,
            &policy,
        )
        .unwrap();
        assert_eq!(
            (choice.preset_name.as_str(), choice.model.as_deref()),
            ("Claude Code", Some("fable"))
        );
        let choice = resolve_worker(
            &presets(),
            Some("Claude Code"),
            Some(WorkerRole::Specialist),
            Some(" haiku "),
            &policy,
        )
        .unwrap();
        assert_eq!(choice.model.as_deref(), Some("haiku"));
        assert_eq!(choice.model_source, ModelSource::Requested);

        let unknown = resolve_worker(&presets(), Some("Aider"), None, None, &policy).unwrap_err();
        assert!(unknown.contains("\"Aider\" is not configured"), "{unknown}");
        let bad = resolve_worker(&presets(), None, None, Some("gpt 6; rm"), &policy).unwrap_err();
        assert!(bad.contains("is not a model name"), "{bad}");
        // pi and gemini have no known model flag: no policy model, and an
        // explicit one is refused.
        let pi = resolve_worker(&presets(), Some("Pi"), None, None, &policy).unwrap();
        assert_eq!((pi.model, pi.model_source), (None, ModelSource::CliDefault));
        let pi_model =
            resolve_worker(&presets(), Some("Pi"), None, Some("x"), &policy).unwrap_err();
        assert!(pi_model.contains("model flag"), "{pi_model}");
    }

    #[test]
    fn codex_names_fall_back_to_the_cli_default_when_the_policy_has_none() {
        let mut policy = ModelPolicy::default();
        policy.codex = RoleModels::default();
        let choice = resolve_worker(&presets(), None, None, None, &policy).unwrap();
        assert_eq!(choice.provider, WorkerProvider::Codex);
        assert_eq!(
            (choice.model, choice.model_source),
            (None, ModelSource::CliDefault)
        );
        policy.codex.scoped = Some("GPT 6".into());
        let error = resolve_worker(&presets(), None, None, None, &policy).unwrap_err();
        assert!(error.contains("fix policy in settings.json"), "{error}");
    }

    #[test]
    fn a_preset_that_pins_its_model_keeps_it() {
        let policy = ModelPolicy::default();
        let pinned = [PresetRef {
            name: "Codex",
            command: "codex -m gpt-5.5 --search",
        }];
        let choice = resolve_worker(&pinned, None, None, None, &policy).unwrap();
        assert_eq!(
            (choice.model, choice.model_source),
            (None, ModelSource::Preset)
        );
        let error = resolve_worker(&pinned, None, None, Some("gpt-6-luna"), &policy).unwrap_err();
        assert!(error.contains("already names a model"), "{error}");
        assert!(command_pins_model("claude --model=opus"));
        assert!(!command_pins_model("claude --resume x"));
        // The executable itself is never read as a flag.
        assert!(!command_pins_model("-m"));
    }

    #[test]
    fn the_model_flag_goes_right_after_the_executable() {
        assert_eq!(
            with_model_flag("claude", "sonnet").unwrap(),
            "claude --model sonnet"
        );
        assert_eq!(
            with_model_flag("claude --dangerously-skip-permissions", "opus").unwrap(),
            "claude --model opus --dangerously-skip-permissions"
        );
        assert_eq!(
            with_model_flag("/opt/homebrew/bin/codex --search", "gpt-6-luna").unwrap(),
            "/opt/homebrew/bin/codex -m gpt-6-luna --search"
        );
        assert!(with_model_flag("pi", "x")
            .unwrap_err()
            .contains("model option"));
        assert!(with_model_flag("codex -m a", "b")
            .unwrap_err()
            .contains("already names a model"));
        assert!(with_model_flag("claude", "a b").is_err());
        assert_eq!(
            provider_for_command("'/usr/bin/claude' -c"),
            WorkerProvider::Claude
        );
    }
}

/// Stable prompt text, rather than PTY redraw traffic, drives the stall timer.
/// shortcut: recognizes common harness prompts, extend when a new prompt is observed.
pub fn waiting_prompt(screen: &str) -> Option<String> {
    let lower = screen.to_lowercase();
    let question_overlay = lower.contains("enter to submit answer")
        && screen.lines().any(|line| {
            let text = line.trim();
            let lower = text.to_lowercase();
            lower.starts_with("question ")
                || selected_option_line(text)
                || lower.starts_with("› type your answer")
                || lower.starts_with("❯ type your answer")
        });
    if !question_overlay && (lower.contains("esc to interrupt") || lower.contains("esc interrupt"))
    {
        return None;
    }
    if question_overlay {
        return Some(
            screen
                .lines()
                .map(str::trim)
                .find(|line| line.to_lowercase().starts_with("question "))
                .unwrap_or("request_user_input")
                .to_string(),
        );
    }
    screen.lines().find_map(|line| {
        let text = line.trim();
        let lower = text.to_lowercase();
        ([
            "trust this folder?",
            "do you trust",
            "would you like to",
            "do you want to",
            "press enter",
            "enter to confirm",
            "waiting for input",
            "waiting for approval",
            "select an option",
            "which option",
            "allow this",
            "approve this",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
            || selected_option_line(text))
        .then(|| text.to_string())
    })
}

fn selected_option_line(text: &str) -> bool {
    ["› ", "❯ "].iter().any(|marker| {
        text.strip_prefix(marker)
            .and_then(|option| option.split_once('.'))
            .is_some_and(|(number, _)| {
                !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
            })
    })
}

pub const STARTUP_TIMEOUT_SECS: u64 = 30;
pub const STALL_TIMEOUT_SECS: u64 = 60;

pub fn startup_timed_out(elapsed_secs: u64, session_found: bool) -> bool {
    !session_found && elapsed_secs >= STARTUP_TIMEOUT_SECS
}

#[derive(Default)]
pub struct PromptWatch {
    prompt: Option<(String, std::time::Instant)>,
}

impl PromptWatch {
    pub fn observe(&mut self, screen: &str, now: std::time::Instant) -> bool {
        let Some(prompt) = waiting_prompt(screen) else {
            self.prompt = None;
            return false;
        };
        match &self.prompt {
            Some((previous, since)) if previous == &prompt => {
                now.saturating_duration_since(*since).as_secs() >= STALL_TIMEOUT_SECS
            }
            _ => {
                self.prompt = Some((prompt, now));
                false
            }
        }
    }
}

/// Strict counterpart of workspace_repos::main_checkout for authorization.
/// No fallback can turn a failed worktree lookup into a trust grant.
pub fn trust_main_repository(dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let root = crate::workspace_repos::checkout_root(dir)
        .ok_or_else(|| format!("{} is not a git checkout", dir.display()))?;
    let root = std::fs::canonicalize(&root)
        .map_err(|error| format!("resolve trust checkout {}: {error}", root.display()))?;
    let repository = git2::Repository::open(&root)
        .map_err(|error| format!("open trust checkout {}: {error}", root.display()))?;
    let main = if repository.is_worktree() {
        let common_file = repository.path().join("commondir");
        let common = std::fs::read_to_string(&common_file)
            .map_err(|error| format!("read {} for trust: {error}", common_file.display()))?;
        let common = std::fs::canonicalize(
            repository
                .path()
                .join(common.trim_end_matches(['\r', '\n'])),
        )
        .map_err(|error| {
            format!(
                "resolve common git directory for {}: {error}",
                root.display()
            )
        })?;
        if common.file_name().is_none_or(|name| name != ".git") {
            return Err(format!(
                "cannot resolve main checkout for trust from {}",
                common.display()
            ));
        }
        let main = common
            .parent()
            .ok_or_else(|| "git common directory has no parent".to_string())?
            .to_path_buf();
        verify_worktree_registration(&root, &common, repository.path())?;
        main
    } else {
        root
    };
    std::fs::canonicalize(&main)
        .map_err(|error| format!("resolve trust repository {}: {error}", main.display()))
}

fn verify_worktree_registration(
    checkout: &std::path::Path,
    main_git_dir: &std::path::Path,
    worktree_git_dir: &std::path::Path,
) -> Result<(), String> {
    let git_entry = checkout.join(".git");
    let git_entry_metadata = std::fs::symlink_metadata(&git_entry).map_err(|error| {
        format!(
            "inspect worktree .git entry {}: {error}",
            git_entry.display()
        )
    })?;
    if !git_entry_metadata.file_type().is_file() {
        return Err(format!(
            "worktree .git entry {} must be a regular file",
            git_entry.display()
        ));
    }
    let pointer = std::fs::read_to_string(&git_entry)
        .map_err(|error| format!("read worktree gitdir {}: {error}", git_entry.display()))?;
    let pointer = pointer
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| format!("invalid worktree gitdir file {}", git_entry.display()))?;
    let pointer_path = std::path::Path::new(pointer);
    let pointer_path = std::fs::canonicalize(if pointer_path.is_absolute() {
        pointer_path.to_path_buf()
    } else {
        checkout.join(pointer_path)
    })
    .map_err(|error| format!("resolve worktree gitdir {}: {error}", git_entry.display()))?;
    let registrations = main_git_dir.join("worktrees");
    let registrations = std::fs::canonicalize(&registrations).map_err(|error| {
        format!(
            "resolve worktree registrations {}: {error}",
            registrations.display()
        )
    })?;
    if pointer_path.parent() != Some(registrations.as_path()) {
        return Err(format!(
            "worktree gitdir {} is not registered in {}",
            pointer_path.display(),
            registrations.display()
        ));
    }
    let back_pointer = pointer_path.join("gitdir");
    let back_pointer = std::fs::read_to_string(&back_pointer)
        .map_err(|error| format!("read worktree backlink {}: {error}", back_pointer.display()))?;
    let back_pointer = std::path::Path::new(back_pointer.trim());
    let back_pointer = std::fs::canonicalize(if back_pointer.is_absolute() {
        back_pointer.to_path_buf()
    } else {
        pointer_path.join(back_pointer)
    })
    .map_err(|error| {
        format!(
            "resolve worktree backlink for {}: {error}",
            checkout.display()
        )
    })?;
    let expected = std::fs::canonicalize(&git_entry).map_err(|error| {
        format!(
            "resolve worktree .git file {}: {error}",
            git_entry.display()
        )
    })?;
    if back_pointer != expected {
        return Err(format!(
            "worktree registration {} points to {}, expected {}",
            pointer_path.display(),
            back_pointer.display(),
            expected.display()
        ));
    }
    // git2 has already resolved this directory from the checkout's .git file;
    // require the parsed pointer to identify that same registered directory.
    let worktree_git_dir = std::fs::canonicalize(worktree_git_dir).map_err(|error| {
        format!(
            "resolve worktree git directory {}: {error}",
            worktree_git_dir.display()
        )
    })?;
    if pointer_path != worktree_git_dir {
        return Err(format!(
            "worktree pointer {} does not match opened git directory {}",
            pointer_path.display(),
            worktree_git_dir.display()
        ));
    }
    Ok(())
}

/// shell-words splits arguments but does not reject shell control syntax.
/// Permit ordinary quoting and GitTerm's quoted initial-prompt substitution;
/// refuse other expansions/operators instead of inferring their effects.
fn validate_trust_shell_syntax(command: &str, provider: &str) -> Result<(), String> {
    let command = match command.rsplit_once(" \"$(cat '") {
        Some((prefix, suffix))
            if suffix
                .strip_suffix("')\"")
                .is_some_and(|path| !path.contains('\'')) =>
        {
            prefix
        }
        _ => command,
    };
    let mut quote = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => {
                chars.next();
            }
            (None, '\'' | '"') => quote = Some(ch),
            (_, '$' | '`') | (None, ';' | '&' | '|' | '<' | '>' | '(' | ')' | '\n' | '\r') => {
                return Err(format!("{provider} task preset contains shell expansion/control syntax; use a single {provider} command so GitTerm can verify its checkout and trust"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refuse preset forms whose effective checkout/config GitTerm cannot verify.
/// Generated initial-prompt and notify arguments are retained as shell words.
pub fn validate_codex_trust_command(command: &str) -> Result<(), String> {
    validate_trust_shell_syntax(command, "Codex")?;
    let words =
        shell_words::split(command).map_err(|error| format!("parse Codex preset: {error}"))?;
    let mut args = words.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-C"
            || arg.starts_with("-C")
            || arg == "--cd"
            || arg.starts_with("--cd=")
            || arg == "-p"
            || arg.starts_with("-p")
            || arg == "--profile"
            || arg.starts_with("--profile=")
            || arg == "--worktree"
            || arg == "--remote"
            || arg.starts_with("--remote=")
        {
            return Err("Codex task preset changes its checkout/config source; remove checkout/profile/remote options before GitTerm can verify per-run trust".into());
        }
        let value = if arg == "-c" || arg == "--config" {
            Some(
                args.next()
                    .ok_or_else(|| "Codex config option has no value".to_string())?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("--config=").or_else(|| {
                arg.strip_prefix("-c")
                    .map(|value| value.strip_prefix('=').unwrap_or(value))
            })
        };
        if let Some(value) = value {
            let key = value.split('=').next().unwrap_or_default().trim();
            if key == "projects"
                || key.starts_with("projects.")
                || key == "profile"
                || key == "profiles"
                || key.starts_with("profiles.")
            {
                return Err("Codex task presets cannot override projects/profile: GitTerm must honor the read-only trust list in ~/.codex/config.toml".into());
            }
        }
    }
    Ok(())
}

/// A run-local config overlay. Codex 0.164.0 splits override *keys* on dots
/// literally; the quoted repo key must be inside an inline TOML *value*.
/// Its config-layer merge recursively preserves other projects and settings.
pub fn codex_trust_value(main: &std::path::Path) -> Result<String, String> {
    let path = main
        .to_str()
        .ok_or_else(|| "Codex trust requires a UTF-8 repository path".to_string())?;
    let key =
        serde_json::to_string(path).map_err(|error| format!("quote Codex trust path: {error}"))?;
    Ok(format!("projects={{{key}={{trust_level=\"trusted\"}}}}"))
}

pub fn with_codex_trust(command: &str, value: &str) -> Result<String, String> {
    if !cfg!(unix) {
        return Err("Codex per-run trust requires a POSIX worker shell".into());
    }
    let command = command.trim();
    let end = command.find(char::is_whitespace).unwrap_or(command.len());
    Ok(format!(
        "{} -c {}{}",
        &command[..end],
        crate::guidelines::shell_word(value),
        &command[end..]
    ))
}

/// Read-only trust decision, recomputed for every process launch. A config
/// entry explicitly denying this checkout/repository (or an ancestor) wins.
pub fn codex_task_trust(
    command: &str,
    cwd: &std::path::Path,
    workspace_repos: &[std::path::PathBuf],
    config_path: &std::path::Path,
) -> Result<Option<String>, String> {
    validate_codex_trust_command(command)?;
    let config = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "read Codex trust list {}: {error}",
                config_path.display()
            ))
        }
    };
    let config: toml::Value = toml::from_str(&config).map_err(|error: toml::de::Error| {
        // TOML error Display includes source lines, which may hold secrets.
        let location = error
            .span()
            .map(|span| format!(" at byte {}", span.start))
            .unwrap_or_default();
        format!(
            "parse Codex trust list {}: invalid TOML{location}",
            config_path.display()
        )
    })?;
    let cwd = std::fs::canonicalize(cwd)
        .map_err(|error| format!("resolve Codex launch directory {}: {error}", cwd.display()))?;
    let main = trust_main_repository(&cwd)?;
    if let Some(projects) = config.get("projects") {
        let projects = projects
            .as_table()
            .ok_or_else(|| format!("projects in {} must be a table", config_path.display()))?;
        for (path, project) in projects {
            let Some(trust) = project.get("trust_level") else {
                continue;
            };
            let trust = trust
                .as_str()
                .filter(|trust| matches!(*trust, "trusted" | "untrusted"))
                .ok_or_else(|| {
                    format!("invalid project trust_level in {}", config_path.display())
                })?;
            if trust != "untrusted" {
                continue;
            }
            let path = std::path::Path::new(path);
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if cwd.starts_with(&canonical)
                || main.starts_with(&canonical)
                || trust_main_repository(&canonical).is_ok_and(|repo| repo == main)
            {
                return Err(format!("Refusing Codex worker: repository {} is explicitly untrusted in {}. Change its trust_level there before launching", main.display(), config_path.display()));
            }
        }
    }
    let member = workspace_repos
        .iter()
        .any(|repo| trust_main_repository(repo).is_ok_and(|repo| repo == main));
    if !member {
        return Ok(None);
    }
    codex_trust_value(&main).map(Some)
}

/// Alternate homes are refused rather than reading one list and launching
/// against another. No environment values are included in error messages.
pub fn validate_codex_trust_home(
    home: &std::path::Path,
    extra_env: &[(String, String)],
) -> Result<(), String> {
    for key in ["HOME", "CODEX_HOME"] {
        let expected = if key == "HOME" {
            home.to_path_buf()
        } else {
            home.join(".codex")
        };
        let value = extra_env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(key).ok());
        if let Some(value) = value {
            let actual =
                std::fs::canonicalize(&value).unwrap_or_else(|_| std::path::PathBuf::from(value));
            let expected = std::fs::canonicalize(&expected).unwrap_or(expected);
            if actual != expected {
                return Err(format!("Codex task {key} points at an alternate config source; remove the override so GitTerm can verify ~/.codex/config.toml read-only"));
            }
        }
    }
    Ok(())
}

/// Claude's global trust file follows HOME, or CLAUDE_CONFIG_DIR when set.
/// Refuse alternate sources rather than persist trust in the wrong file.
/// Even CLAUDE_CONFIG_DIR=HOME moves transcripts away from ~/.claude/projects.
/// As with Codex, user shell startup files are a trusted launch boundary.
pub fn validate_claude_trust_launch(
    command: &str,
    home: &std::path::Path,
    extra_env: &[(String, String)],
) -> Result<(), String> {
    validate_trust_shell_syntax(command, "Claude")?;
    let words =
        shell_words::split(command).map_err(|_| "Cannot parse Claude task preset".to_string())?;
    for arg in words.iter().skip(1) {
        if arg == "-w"
            || arg.starts_with("-w")
            || arg == "--worktree"
            || arg.starts_with("--worktree=")
            || arg == "--teleport"
            || arg.starts_with("--teleport=")
            || arg == "--remote"
            || arg.starts_with("--remote=")
        {
            return Err("Claude task preset changes its checkout; remove worktree/remote/teleport options before GitTerm can verify trust".into());
        }
    }
    for key in [
        "HOME",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_CUSTOM_OAUTH_URL",
        "CLAUDE_CODE_IGNORE_PERSISTED_TRUST",
    ] {
        let value = extra_env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(key).ok());
        if let Some(value) = value.filter(|value| key == "HOME" || !value.is_empty()) {
            let expected = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
            let actual = std::fs::canonicalize(&value).unwrap_or_else(|_| value.into());
            if key != "HOME" || actual != expected {
                return Err(format!("Claude task {key} changes its trust source; remove the override so GitTerm can verify ~/.claude.json"));
            }
        }
    }
    Ok(())
}

/// Persist only the canonical workspace repository's trust bit. False and
/// missing both mean not yet trusted (Tracey's TRU-225 decision).
/// An already trusted file and an outside repository are never rewritten.
pub fn claude_task_trust(
    cwd: &std::path::Path,
    workspace_repos: &[std::path::PathBuf],
    config_path: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, String> {
    claude_task_trust_before_replace(cwd, workspace_repos, config_path, |_, _| Ok(()))
}

fn claude_task_trust_before_replace(
    cwd: &std::path::Path,
    workspace_repos: &[std::path::PathBuf],
    config_path: &std::path::Path,
    mut before_replace: impl FnMut(usize, &std::path::Path) -> Result<(), String>,
) -> Result<Option<std::path::PathBuf>, String> {
    use std::io::Write;
    let main = trust_main_repository(cwd)?;
    if !workspace_repos
        .iter()
        .any(|repo| trust_main_repository(repo).is_ok_and(|repo| repo == main))
    {
        return Ok(None);
    }
    let key = main
        .to_str()
        .ok_or_else(|| "Claude trust requires a UTF-8 repository path".to_string())?;
    let metadata = || -> Result<(std::time::SystemTime, u64, std::fs::Permissions), String> {
        let info = std::fs::symlink_metadata(config_path).map_err(|error| {
            format!(
                "inspect Claude trust file {}: {error}",
                config_path.display()
            )
        })?;
        if !info.file_type().is_file() {
            return Err(format!(
                "Claude trust file {} must be a regular file, not a symlink",
                config_path.display()
            ));
        }
        let modified = info.modified().map_err(|error| {
            format!(
                "read Claude trust file mtime {}: {error}",
                config_path.display()
            )
        })?;
        Ok((modified, info.len(), info.permissions()))
    };
    for attempt in 0..2 {
        let (modified, size, permissions) = metadata()?;
        let bytes = std::fs::read(config_path).map_err(|error| {
            format!("read Claude trust file {}: {error}", config_path.display())
        })?;
        let unchanged = || metadata().map(|(mtime, len, _)| mtime == modified && len == size);
        if !unchanged()? {
            continue;
        }
        let mut config: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "parse Claude trust file {}: invalid JSON at line {}, column {}",
                config_path.display(),
                error.line(),
                error.column()
            )
        })?;
        let object = config.as_object_mut().ok_or_else(|| {
            format!(
                "Claude trust file {} must contain an object",
                config_path.display()
            )
        })?;
        let projects = object
            .entry("projects")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| format!("projects in {} must be an object", config_path.display()))?;
        let project = projects
            .entry(key)
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                format!(
                    "Claude project trust entry in {} must be an object",
                    config_path.display()
                )
            })?;
        if project.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true)) {
            return Ok(None);
        }
        if project
            .get("hasTrustDialogAccepted")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(format!(
                "hasTrustDialogAccepted in {} must be a boolean",
                config_path.display()
            ));
        }
        project.insert("hasTrustDialogAccepted".into(), true.into());
        let mut encoded = serde_json::to_vec_pretty(&config).map_err(|error| {
            format!(
                "encode Claude trust file {}: {error}",
                config_path.display()
            )
        })?;
        encoded.push(b'\n');
        let parent = config_path
            .parent()
            .ok_or_else(|| "Claude trust file has no parent directory".to_string())?;
        let temporary = parent.join(format!(
            ".gitterm-v6-claude-trust-{}.tmp",
            uuid::Uuid::new_v4()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Only clean up a file that we created; never unlink a name collision.
        let mut file = options.open(&temporary).map_err(|error| {
            format!(
                "create Claude trust temporary file {}: {error}",
                temporary.display()
            )
        })?;
        let result = (|| -> Result<bool, String> {
            file.set_permissions(permissions)
                .map_err(|error| format!("preserve Claude trust mode: {error}"))?;
            file.write_all(&encoded)
                .map_err(|error| format!("write Claude trust temporary file: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("sync Claude trust temporary file: {error}"))?;
            drop(file);
            before_replace(attempt, &temporary)?;
            if !unchanged()? {
                return Ok(false);
            }
            // shortcut: metadata checking cannot close the final compare/rename
            // race; use a shared lock protocol if Claude exposes one to callers.
            crate::tasks::replace_file_atomically(&temporary, config_path).map_err(|error| {
                format!(
                    "replace Claude trust file {}: {error}",
                    config_path.display()
                )
            })?;
            Ok(true)
        })();
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cleanup Claude trust temporary file {}: {error}; update result: {result:?}",
                    temporary.display()
                ))
            }
        }
        if result? {
            return Ok(Some(main));
        }
    }
    Err(format!(
        "Claude trust file {} changed during both update attempts; refusing launch",
        config_path.display()
    ))
}

#[cfg(test)]
mod reliability_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn repo(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        git2::Repository::init(dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn workspace_trust_is_run_local_and_rechecks_explicit_denials() {
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("sample.repo"));
        let outside = repo(&temp.path().join("outside"));
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            "approval_policy='on-request'\nsandbox_mode='workspace-write'\n",
        )
        .unwrap();
        let original = std::fs::read(&config).unwrap();
        let value = codex_task_trust(
            "codex -m example-model",
            &root,
            std::slice::from_ref(&root),
            &config,
        )
        .unwrap()
        .unwrap();
        let parsed: toml::Value = toml::from_str(&value).unwrap();
        assert_eq!(
            parsed["projects"][root.to_str().unwrap()]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(std::fs::read(&config).unwrap(), original);
        assert!(
            codex_task_trust("codex", &outside, std::slice::from_ref(&root), &config)
                .unwrap()
                .is_none()
        );
        // An ancestor workspace does not authorize an unrelated nested repo.
        assert!(
            codex_task_trust("codex", &root, &[temp.path().to_path_buf()], &config)
                .unwrap()
                .is_none()
        );
        std::fs::write(
            &config,
            format!(
                "[projects.{}]\ntrust_level='untrusted'\n",
                serde_json::to_string(root.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        let denied = std::fs::read(&config).unwrap();
        let error =
            codex_task_trust("codex", &root, std::slice::from_ref(&root), &config).unwrap_err();
        assert!(error.contains("explicitly untrusted"), "{error}");
        assert!(error.contains("Change its trust_level"), "{error}");
        assert_eq!(std::fs::read(&config).unwrap(), denied);
        assert!(!codex_trust_value(&root)
            .unwrap()
            .contains("approval_policy"));
    }

    #[test]
    fn worktree_trust_resolves_main_and_denies_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("main"));
        let repository = git2::Repository::open(&root).unwrap();
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        let checkout = temp.path().join("linked");
        repository.worktree("worker", &checkout, None).unwrap();
        assert_eq!(trust_main_repository(&checkout).unwrap(), root);
        let config = temp.path().join("config.toml");
        assert_eq!(
            codex_task_trust("codex", &checkout, std::slice::from_ref(&root), &config).unwrap(),
            Some(codex_trust_value(&root).unwrap())
        );
        #[cfg(unix)]
        {
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            std::fs::write(
                &config,
                format!(
                    "[projects.{}]\ntrust_level='untrusted'\n",
                    serde_json::to_string(alias.to_str().unwrap()).unwrap()
                ),
            )
            .unwrap();
            assert!(codex_task_trust("codex", &checkout, &[root], &config)
                .unwrap_err()
                .contains("explicitly untrusted"));
        }
    }

    #[test]
    fn forged_worktree_pointer_without_reciprocal_registration_gets_no_trust() {
        let temp = tempfile::tempdir().unwrap();
        let (root, linked) = linked_repo(temp.path());
        let forged = temp.path().join("forged");
        std::fs::create_dir(&forged).unwrap();
        std::fs::copy(linked.join(".git"), forged.join(".git")).unwrap();
        assert!(trust_main_repository(&forged)
            .unwrap_err()
            .contains("registration"));
        let config = temp.path().join("codex.toml");
        assert!(codex_task_trust("codex", &forged, std::slice::from_ref(&root), &config).is_err());
        let claude_config = temp.path().join("claude.json");
        std::fs::write(&claude_config, "{}").unwrap();
        assert!(claude_task_trust(&forged, std::slice::from_ref(&root), &claude_config).is_err());
        let original_config = std::fs::read(&claude_config).unwrap();
        #[cfg(unix)]
        {
            let symlinked = temp.path().join("symlinked-forgery");
            std::fs::create_dir(&symlinked).unwrap();
            std::os::unix::fs::symlink(linked.join(".git"), symlinked.join(".git")).unwrap();
            assert!(trust_main_repository(&symlinked).is_err());
            assert!(
                codex_task_trust("codex", &symlinked, std::slice::from_ref(&root), &config)
                    .is_err()
            );
            assert!(
                claude_task_trust(&symlinked, std::slice::from_ref(&root), &claude_config).is_err()
            );
            assert_eq!(std::fs::read(&claude_config).unwrap(), original_config);
        }
    }

    #[test]
    fn malformed_trust_and_redirected_presets_fail_loudly() {
        for command in [
            "codex -C /elsewhere",
            "codex --cd=/elsewhere",
            "codex -C/elsewhere",
            "codex -p other",
            "codex --profile=other",
            "codex -c 'projects={}'",
            "codex --config='projects={}'",
            "codex -cprojects={}",
            "codex -c=projects={}",
            "codex -c=profile=other",
            "codex --no-alt-screen;codex",
            "codex $(printf -- -C/elsewhere)",
            "codex --no-alt-screen&&codex",
            "codex --config 'profile=other'",
        ] {
            assert!(validate_codex_trust_command(command).is_err(), "{command}");
        }
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("repo"));
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "[projects\n").unwrap();
        assert!(codex_task_trust("codex", &root, &[], &config)
            .unwrap_err()
            .contains("parse Codex trust list"));
        for key in ["HOME", "CODEX_HOME"] {
            let mut env = vec![
                ("HOME".into(), temp.path().display().to_string()),
                (
                    "CODEX_HOME".into(),
                    temp.path().join(".codex").display().to_string(),
                ),
            ];
            env.push((key.into(), "/alternate-source".into()));
            assert!(validate_codex_trust_home(temp.path(), &env)
                .unwrap_err()
                .contains(key));
        }
    }

    #[test]
    fn malformed_private_config_does_not_expose_source_text() {
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("repo"));
        let config = temp.path().join("config.toml");
        let secret = "SYNTHETIC_SECRET_MUST_NOT_APPEAR";
        std::fs::write(&config, format!("api_key = \"{secret}\n")).unwrap();
        let error = codex_task_trust("codex", &root, &[root.clone()], &config).unwrap_err();
        assert!(error.contains("invalid TOML"));
        assert!(!error.contains(secret), "private config source leaked");
    }

    #[test]
    fn generated_prompt_and_notify_are_safe_shell_words() {
        assert!(validate_codex_trust_command("codex --config 'notify=[\"sh\",\"-c\",\"echo $1 >/dev/null\"]' \"$(cat '/tmp/brief.md')\"").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn inline_path_and_shell_quoting_preserve_special_characters() {
        let path = Path::new("/tmp/sample.repo space \"quote\" ' $value");
        let value = codex_trust_value(path).unwrap();
        let parsed: toml::Value = toml::from_str(&value).unwrap();
        assert_eq!(
            parsed["projects"][path.to_str().unwrap()]["trust_level"].as_str(),
            Some("trusted")
        );
        let command = with_codex_trust("codex", &value).unwrap();
        assert_eq!(
            shell_words::split(&command).unwrap(),
            vec!["codex", "-c", &value]
        );
    }

    #[test]
    fn startup_requires_a_session_and_prompt_stalls_ignore_redraws() {
        assert!(!startup_timed_out(29, false));
        assert!(startup_timed_out(30, false));
        assert!(!startup_timed_out(1000, true));
        for screen in [
            "Folder access\nTrust this folder?\n› 1. Trust and continue\n  2. Quit",
            "Would you like to run the following command?\n› 1. Yes\n 2. No",
            "Select an option\n❯ 1. Continue",
        ] {
            assert!(waiting_prompt(screen).is_some(), "{screen}");
        }
        // From Codex's published request_user_input snapshot:
        // https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_options.snap
        let request_user_input_snapshot = r#"
  Question 1/1 (1 unanswered)
  Choose an option.

  › 1. Option 1           First choice.
    2. Option 2           Second choice.
    3. Option 3           Third choice.
    4. None of the above  Optionally, add details in notes (tab)

  tab to add notes | enter to submit answer | esc to interrupt
"#;
        assert!(waiting_prompt(request_user_input_snapshot).is_some());
        let moved_selection = request_user_input_snapshot
            .replace("› 1.", "  1.")
            .replace("    2.", "  › 2.");
        assert!(waiting_prompt(&moved_selection).is_some());
        assert_eq!(
            waiting_prompt(request_user_input_snapshot),
            waiting_prompt(&moved_selection)
        );
        let freeform_snapshot = r#"
  Question 1/1 (1 unanswered)
  Share details.
  › Type your answer (optional)
  enter to submit answer | esc to interrupt
"#;
        assert!(waiting_prompt(freeform_snapshot).is_some());
        for screen in [
            "Reading sources\nWorking (esc to interrupt)",
            "› Ask Codex to do anything\n? for shortcuts",
            "build finished\n$ ",
        ] {
            assert!(waiting_prompt(screen).is_none(), "{screen}");
        }
        let now = Instant::now();
        let mut watch = PromptWatch::default();
        assert!(!watch.observe("spinner 1\nWould you like to proceed?", now));
        assert!(!watch.observe(
            "spinner 2\nWould you like to proceed?",
            now + Duration::from_secs(59)
        ));
        assert!(watch.observe(
            "spinner 3\nWould you like to proceed?",
            now + Duration::from_secs(60)
        ));
        assert!(!watch.observe("Working (esc to interrupt)", now + Duration::from_secs(301)));
        assert!(!watch.observe("Would you like to proceed?", now + Duration::from_secs(302)));
    }

    fn linked_repo(temp: &Path) -> (PathBuf, PathBuf) {
        let root = repo(&temp.join("main.repo space ' $quote"));
        let repository = git2::Repository::open(&root).unwrap();
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        let linked = temp.join("linked worker");
        repository.worktree("worker", &linked, None).unwrap();
        (root, linked)
    }

    #[test]
    fn claude_trust_only_changes_workspace_main_key_and_preserves_fields() {
        use serde_json::json;
        let temp = tempfile::tempdir().unwrap();
        let (root, linked) = linked_repo(temp.path());
        let outside = repo(&temp.path().join("outside"));
        let config = temp.path().join(".claude.json");
        for project in [
            json!({"hasTrustDialogAccepted":false,"allowedTools":["Read"]}),
            json!({"allowedTools":["Read"]}),
        ] {
            let original = json!({"future":{"nested":[null,12.5,true]},"projects":{
                root.to_str().unwrap():project, outside.to_str().unwrap():{"hasTrustDialogAccepted":false,"extra":"keep"}
            }});
            let bytes = serde_json::to_vec(&original).unwrap();
            std::fs::write(&config, &bytes).unwrap();
            assert_eq!(
                claude_task_trust(&outside, std::slice::from_ref(&root), &config).unwrap(),
                None
            );
            assert_eq!(std::fs::read(&config).unwrap(), bytes);
            assert_eq!(
                claude_task_trust(&linked, std::slice::from_ref(&root), &config).unwrap(),
                Some(root.clone())
            );
            let mut expected = original;
            expected["projects"][root.to_str().unwrap()]["hasTrustDialogAccepted"] = json!(true);
            let updated = std::fs::read(&config).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&updated).unwrap(),
                expected
            );
            assert_eq!(
                claude_task_trust(&root, std::slice::from_ref(&linked), &config).unwrap(),
                None
            );
            assert_eq!(std::fs::read(&config).unwrap(), updated);
        }
        std::fs::write(&config, "{}\n").unwrap();
        assert!(
            claude_task_trust(&root, std::slice::from_ref(&root), &config)
                .unwrap()
                .is_some()
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(value["projects"].as_object().unwrap().len(), 1);
        assert_eq!(
            value["projects"][root.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn claude_trust_refuses_missing_invalid_and_redirected_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("repo"));
        let config = temp.path().join(".claude.json");
        let update = || claude_task_trust(&root, std::slice::from_ref(&root), &config);
        assert!(update().unwrap_err().contains("inspect Claude trust file"));
        assert!(!config.exists());
        for value in ["{secret invalid", "[]", r#"{"projects":null}"#] {
            std::fs::write(&config, value).unwrap();
            let error = update().unwrap_err();
            assert!(!error.contains("secret"));
            assert_eq!(std::fs::read_to_string(&config).unwrap(), value);
        }
        for project in [
            serde_json::json!([]),
            serde_json::json!({"hasTrustDialogAccepted":"false"}),
        ] {
            let value =
                serde_json::json!({"projects":{root.to_str().unwrap():project}}).to_string();
            std::fs::write(&config, &value).unwrap();
            assert!(update().is_err());
            assert_eq!(std::fs::read_to_string(&config).unwrap(), value);
        }
        #[cfg(unix)]
        {
            let target = temp.path().join("target");
            std::fs::write(&target, "{}").unwrap();
            std::fs::remove_file(&config).unwrap();
            std::os::unix::fs::symlink(&target, &config).unwrap();
            assert!(update().unwrap_err().contains("regular file"));
            assert_eq!(std::fs::read_to_string(target).unwrap(), "{}");
        }
    }

    #[test]
    fn claude_trust_retries_once_and_replaces_atomically_with_mode() {
        use std::io::Read;
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("repo"));
        let config = temp.path().join(".claude.json");
        std::fs::write(&config, "{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let mut old_file = std::fs::File::open(&config).unwrap();
        let mut attempts = vec![];
        assert!(claude_task_trust_before_replace(
            &root,
            std::slice::from_ref(&root),
            &config,
            |attempt, temporary| {
                attempts.push(attempt);
                assert_eq!(temporary.parent(), config.parent());
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(temporary).unwrap()).unwrap();
                assert_eq!(
                    value["projects"][root.to_str().unwrap()]["hasTrustDialogAccepted"],
                    true
                );
                if attempt == 0 {
                    std::fs::write(&config, r#"{"concurrent":"must survive"}"#).unwrap();
                }
                Ok(())
            }
        )
        .unwrap()
        .is_some());
        assert_eq!(attempts, [0, 1]);
        let updated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(updated["concurrent"], "must survive");
        let mut original = String::new();
        old_file.read_to_string(&mut original).unwrap();
        assert_eq!(original, r#"{"concurrent":"must survive"}"#); // rename kept the old inode intact
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        std::fs::write(&config, "{}").unwrap();
        let error = claude_task_trust_before_replace(
            &root,
            std::slice::from_ref(&root),
            &config,
            |_, _| Err("injected pre-replace failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("injected pre-replace failure"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "{}");
        std::fs::write(&config, "{}").unwrap();
        let error = claude_task_trust_before_replace(
            &root,
            std::slice::from_ref(&root),
            &config,
            |attempt, _| {
                std::fs::write(
                    &config,
                    format!(r#"{{"writer":"{}"}}"#, "x".repeat(attempt + 1)),
                )
                .unwrap();
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.contains("both update attempts"));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            r#"{"writer":"xx"}"#
        );
        assert!(!std::fs::read_dir(temp.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }

    #[test]
    fn claude_trust_validates_launch_config_without_leaking_values() {
        let home = tempfile::tempdir().unwrap();
        for command in [
            "claude -w",
            "claude --worktree=elsewhere",
            "claude --teleport id",
            "claude;cd /elsewhere",
            "claude $(echo secret)",
        ] {
            assert!(
                validate_claude_trust_launch(command, home.path(), &[]).is_err(),
                "{command}"
            );
        }
        for key in [
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_CUSTOM_OAUTH_URL",
            "CLAUDE_CODE_IGNORE_PERSISTED_TRUST",
        ] {
            let env = [
                ("HOME".into(), home.path().to_str().unwrap().into()),
                (key.into(), "/private-secret-source".into()),
            ];
            let error = validate_claude_trust_launch("claude", home.path(), &env).unwrap_err();
            assert!(error.contains(key), "{error}");
            assert!(!error.contains("private-secret-source"));
        }
        let home_value = home.path().to_str().unwrap().to_string();
        let error = validate_claude_trust_launch(
            "claude",
            home.path(),
            &[
                ("HOME".into(), home_value.clone()),
                ("CLAUDE_CONFIG_DIR".into(), home_value.clone()),
            ],
        )
        .unwrap_err();
        assert!(error.contains("CLAUDE_CONFIG_DIR"), "{error}");
        let env = [
            ("HOME".into(), home_value),
            ("CLAUDE_CONFIG_DIR".into(), String::new()),
        ];
        validate_claude_trust_launch(
            "/bin/claude --resume fixture --permission-mode manual",
            home.path(),
            &env,
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires Claude Code 2.1.318 and python3; isolated PTY, synthetic auth, no model request"]
    fn claude_worker_trust_pty_smoke() {
        let temp = tempfile::tempdir().unwrap();
        let (root, linked) = linked_repo(temp.path());
        let isolated = temp.path().join("isolated-config");
        std::fs::create_dir(&isolated).unwrap();
        let config = isolated.join(".claude.json");
        for source in ["config-dir", "home"] {
            let fixture = serde_json::json!({
                "hasCompletedOnboarding":true,"lastOnboardingVersion":"2.1.318","theme":"dark",
                "customApiKeyResponses":{"approved":["gitterm-v6-fixture-not-a-real-key".chars().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<String>()],"rejected":[]},
                "projects":{root.to_str().unwrap():{"hasTrustDialogAccepted":false}}
            });
            std::fs::write(&config, serde_json::to_vec(&fixture).unwrap()).unwrap();
            let probe = |trusted| {
                let status = std::process::Command::new("python3")
                    .arg(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/claude_worker_trust_pty.py"
                    ))
                    .arg(&linked)
                    .arg(&config)
                    .arg(trusted)
                    .arg(source)
                    .status()
                    .unwrap();
                assert!(status.success(), "isolated Claude PTY smoke failed");
            };
            probe("false");
            assert_eq!(
                claude_task_trust(&linked, std::slice::from_ref(&root), &config).unwrap(),
                Some(root.clone())
            );
            probe("true");
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires codex-cli 0.164.0, python3 and local Codex auth; isolated PTY, no model request"]
    fn codex_worker_trust_pty_smoke() {
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("sample.repo space \"quote\" ' $value"));
        let value = codex_trust_value(&root).unwrap();
        let command = with_codex_trust("codex", &value).unwrap();
        let status = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/codex_worker_trust_pty.py"
            ))
            .arg(&root)
            .arg(command)
            .status()
            .unwrap();
        assert!(status.success(), "isolated Codex PTY smoke failed");
    }
}
