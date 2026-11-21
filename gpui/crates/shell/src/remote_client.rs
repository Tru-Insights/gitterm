//! Read-only remote machine access for the preview: configured remotes come
//! from Iced's config (never written), and every network read goes through
//! `RemoteClient`, which the real app backs with core's agent client and
//! token lookup, the same calls Iced makes. Tests install a fake.

use std::path::Path;

use anyhow::{Context as _, Result};
use gitterm_core::{
    chats::ChatIndexEntry,
    remote::{
        client::{RemoteAgentBackend, RemoteAgentClientConfig, RemoteAgentHandshake},
        config::{
            REMOTE_AGENTS_FILE, RemoteAgentConfig, RemoteAgentsFile, configured_remote_agents,
        },
        source::{SourceDirListing, SourceHost, SourcePath, WorkspaceSource},
        ssh_sessions::{RemoteHostConfig, RemoteSessionsSnapshot},
    },
};

/// Blocking reads, run on the background executor. Errors are user-facing.
pub trait RemoteClient: Send + Sync {
    fn handshake(&self, remote: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String>;
    /// Iced's picker source: the remote's whole filesystem (root "/").
    fn list_dir(&self, remote: &RemoteAgentConfig, dir: String) -> SourceDirListing;
    fn list_chats(&self, remote: &RemoteAgentConfig) -> Result<Vec<ChatIndexEntry>, String>;
    /// A legacy SSH host's tmux sessions (Iced's remote-content Refresh).
    fn list_sessions(&self, host: &RemoteHostConfig) -> RemoteSessionsSnapshot;
}

/// The agent workspace id Iced's folder picker lists through.
const PICKER_WORKSPACE: &str = "workspace-picker";

/// Core's agent client with core's token lookup.
pub struct AgentClient;

#[derive(Debug, Clone)]
pub struct PreviewSourceHost;

impl SourceHost for PreviewSourceHost {
    type ClientConfig = RemoteAgentClientConfig;
    fn backend(config: Self::ClientConfig) -> RemoteAgentBackend {
        RemoteAgentBackend::new(
            config,
            gitterm_core::remote::token::resolve_token_ref,
            env!("CARGO_PKG_VERSION"),
        )
    }
    fn local_display_dir(_root: &Path, dir: &Path) -> String {
        format!("{}/", dir.display())
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|runtime| runtime.block_on(future))
        .map_err(|error| format!("start remote client runtime: {error}"))
}

impl RemoteClient for AgentClient {
    fn handshake(&self, remote: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(backend.handshake())?.map_err(|error| error.to_string())
    }
    fn list_dir(&self, remote: &RemoteAgentConfig, dir: String) -> SourceDirListing {
        let source = WorkspaceSource::<PreviewSourceHost>::RemoteAgent {
            workspace_id: PICKER_WORKSPACE.into(),
            root: "/".into(),
            client: remote.clone().into(),
        };
        let path = SourcePath::Remote(dir.clone());
        block_on(source.list_dir(0, path, true)).unwrap_or_else(|error| SourceDirListing {
            tab_id: 0,
            current_dir: SourcePath::Remote(dir.clone()),
            display_dir: format!("{dir}/"),
            parent: None,
            entries: vec![],
            error: Some(error),
        })
    }
    fn list_chats(&self, remote: &RemoteAgentConfig) -> Result<Vec<ChatIndexEntry>, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(backend.list_chats())?.map_err(|error| error.to_string())
    }
    fn list_sessions(&self, host: &RemoteHostConfig) -> RemoteSessionsSnapshot {
        gitterm_core::remote::ssh_sessions::collect_remote_sessions(host)
    }
}

/// Iced's configured remotes, read-only: remote-agents.json, then the copies
/// saved in workspaces.json. A missing remote-agents.json is no remotes.
pub fn read_remotes(config: &Path) -> Result<Vec<RemoteAgentConfig>> {
    let path = config.join(REMOTE_AGENTS_FILE);
    let file = match std::fs::read(&path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<RemoteAgentsFile>(&bytes)
                .with_context(|| format!("parse {}", path.display()))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let workspaces = config.join("workspaces.json");
    let saved: gitterm_core::workspace_config::WorkspacesFile = serde_json::from_slice(
        &std::fs::read(&workspaces).with_context(|| format!("read {}", workspaces.display()))?,
    )
    .with_context(|| format!("parse {}", workspaces.display()))?;
    Ok(configured_remote_agents(file, &saved.remote_agents))
}

/// Tests never reach a real remote: unless a test installs its own fake,
/// every read fails here without touching the network or a token.
#[cfg(test)]
pub(crate) struct Offline;

#[cfg(test)]
impl RemoteClient for Offline {
    fn handshake(&self, _: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String> {
        Err("offline in tests".into())
    }
    fn list_dir(&self, _: &RemoteAgentConfig, dir: String) -> SourceDirListing {
        SourceDirListing {
            tab_id: 0,
            current_dir: SourcePath::Remote(dir.clone()),
            display_dir: format!("{dir}/"),
            parent: None,
            entries: vec![],
            error: Some("offline in tests".into()),
        }
    }
    fn list_chats(&self, _: &RemoteAgentConfig) -> Result<Vec<ChatIndexEntry>, String> {
        Err("offline in tests".into())
    }
    fn list_sessions(&self, host: &RemoteHostConfig) -> RemoteSessionsSnapshot {
        RemoteSessionsSnapshot {
            host_name: host.name.clone(),
            sessions: vec![],
            error: Some("offline in tests".into()),
        }
    }
}
