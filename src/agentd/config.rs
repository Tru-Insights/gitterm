use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

impl AgentConfigFile {
    pub fn file_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("gitterm-agent")
            .join("config.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }
}
