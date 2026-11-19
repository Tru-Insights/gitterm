//! Guidelines carried by a runner, resolved by the application before launch.

use std::path::PathBuf;

/// The guidelines a launch carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guidelines {
    pub path: PathBuf,
    pub text: String,
}

/// The `-c` value for Codex. Codex parses the value as TOML; a JSON string
/// literal is a valid TOML basic string (serde_json escapes `"`, `\` and
/// control characters with escapes TOML shares, and never emits `\/`).
pub fn codex_config_value(guidelines: &Guidelines) -> String {
    let literal =
        serde_json::to_string(&guidelines.text).expect("serializing a string to JSON cannot fail");
    format!("developer_instructions={literal}")
}

pub fn codex_args(guidelines: &Guidelines) -> Vec<String> {
    vec!["-c".to_string(), codex_config_value(guidelines)]
}
