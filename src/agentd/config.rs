use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const AGENT_CONFIG_DIR_NAME: &str = "gitterm-v5-agent";
pub const AGENT_SERVICE_LABEL: &str = "com.cree8.gitterm.v5.agent";
pub const AGENT_TOKEN_SERVICE: &str = "gitterm-v5-agent";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

impl AgentConfigFile {
    pub fn file_path() -> PathBuf {
        Self::file_path_under(&dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
    }

    fn file_path_under(home: &std::path::Path) -> PathBuf {
        home.join(".config")
            .join(AGENT_CONFIG_DIR_NAME)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_is_isolated_from_v4() {
        let path = AgentConfigFile::file_path_under(std::path::Path::new("/test-home"));
        assert_eq!(
            path,
            PathBuf::from("/test-home/.config/gitterm-v5-agent/config.json")
        );
        assert_ne!(
            path,
            PathBuf::from("/test-home/.config/gitterm-v4-agent/config.json")
        );
        assert_eq!(AGENT_SERVICE_LABEL, "com.cree8.gitterm.v5.agent");
        assert_eq!(AGENT_TOKEN_SERVICE, "gitterm-v5-agent");
        assert_ne!(AGENT_SERVICE_LABEL, "com.cree8.gitterm.v4.agent");
        assert_ne!(AGENT_TOKEN_SERVICE, "gitterm-v4-agent");
    }
}
