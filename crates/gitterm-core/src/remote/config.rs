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

/// `remote-agents.json` in the desktop config dir: the user-maintained list
/// of remote agents. Hosts read it; only Iced's connect form writes it.
pub const REMOTE_AGENTS_FILE: &str = "remote-agents.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteAgentsFile {
    #[serde(default)]
    pub remote_agents: Vec<RemoteAgentConfig>,
}

/// Add `agent` unless one with its id is already known: the first source
/// read wins (remote-agents.json before workspaces.json's copy).
pub fn push_remote_agent_config(
    definitions: &mut Vec<RemoteAgentConfig>,
    agent: RemoteAgentConfig,
) {
    if !definitions.iter().any(|existing| existing.id == agent.id) {
        definitions.push(agent);
    }
}

/// The configured remotes in the order Iced builds them at startup:
/// remote-agents.json, then those saved in workspaces.json.
pub fn configured_remote_agents(
    remote_agents_file: Option<RemoteAgentsFile>,
    workspaces_file: &[RemoteAgentConfig],
) -> Vec<RemoteAgentConfig> {
    let mut agents = Vec::new();
    for agent in remote_agents_file
        .into_iter()
        .flat_map(|file| file.remote_agents)
        .chain(workspaces_file.iter().cloned())
    {
        push_remote_agent_config(&mut agents, agent);
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str) -> RemoteAgentConfig {
        RemoteAgentConfig {
            id: id.into(),
            name: name.into(),
            endpoint: format!("http://{id}:7777"),
            auth: RemoteAgentAuthConfig::Token {
                token_ref: format!("env:{id}"),
            },
            shell_command: None,
            session_commands: Default::default(),
        }
    }

    #[test]
    fn remote_agents_file_wins_and_workspaces_copies_append_in_order() {
        let file = RemoteAgentsFile {
            remote_agents: vec![agent("a", "From file"), agent("b", "B")],
        };
        let saved = [agent("a", "Stale copy"), agent("c", "C")];
        let agents = configured_remote_agents(Some(file), &saved);
        let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["From file", "B", "C"]);
        assert_eq!(configured_remote_agents(None, &saved).len(), 2);
    }
}
