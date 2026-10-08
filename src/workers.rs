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
/// `config.json`). Seeded with Tracey's table (2026-10-08): coordinator
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
                     config.json",
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(error.contains("fix policy in config.json"), "{error}");
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
