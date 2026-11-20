//! Saved remote identity and opaque workspace location; no credential IO.

use serde::{Deserialize, Serialize};

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
