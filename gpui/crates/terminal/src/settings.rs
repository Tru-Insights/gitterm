use std::{collections::HashMap, path::PathBuf};
#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_directory: Option<PathBuf>,
    /// Scrollback history in lines (default: 100,000 for Claude Code sessions)
    pub scrollback_lines: usize,
}

impl Default for BackendSettings {
    fn default() -> Self {
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

        Self {
            program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            args: vec![],
            env,
            working_directory: None,
            scrollback_lines: 100_000, // 10x default for Claude Code
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutSize {
    pub width: f32,
    pub height: f32,
}
impl LayoutSize {
    pub fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}
