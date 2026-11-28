//! Remote machine access: configured remotes come from Iced's config,
//! and every agent call goes through
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
        phone_panel::{PairedPhoneInfo, PhonePairing},
        source::{SourceDirListing, SourceHost, SourcePath, WorkspaceSource},
        ssh_sessions::{RemoteHostConfig, RemoteSessionsSnapshot},
    },
};

/// Blocking calls, run on the background executor. Errors are user-facing.
pub trait RemoteClient: Send + Sync {
    fn handshake(&self, remote: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String>;
    /// Live only; Preview callers never enter this route. Default fakes stay offline.
    fn start_session(
        &self,
        _remote: &RemoteAgentConfig,
        _workspace: String,
        _launch: gitterm_core::remote::launch::AgentSessionLaunch,
    ) -> Result<String, String> {
        Err("remote session launch unavailable".into())
    }
    /// Live chat supervision; defaults keep Preview and read-only fakes inert.
    fn chat_runtime(
        &self,
        _remote: &RemoteAgentConfig,
        _config: gitterm_core::harness::claude::ClaudeSessionConfig,
        _attach: Option<String>,
    ) -> Result<
        (
            gitterm_core::chat::runtime::Session,
            tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::types::HarnessEvent>,
        ),
        String,
    > {
        Err("Remote chat unavailable".into())
    }
    fn chat_history(
        &self,
        _remote: &RemoteAgentConfig,
        _id: &str,
    ) -> Result<Vec<gitterm_core::chat::state::AgentEvent>, String> {
        Err("Remote chat history unavailable".into())
    }
    fn running_chat(
        &self,
        _remote: &RemoteAgentConfig,
        _id: &str,
    ) -> Result<Option<String>, String> {
        Err("Remote running chats unavailable".into())
    }
    /// Iced's picker source: the remote's whole filesystem (root "/").
    fn list_dir(&self, remote: &RemoteAgentConfig, dir: String) -> SourceDirListing;
    fn list_chats(&self, remote: &RemoteAgentConfig) -> Result<Vec<ChatIndexEntry>, String>;
    /// A legacy SSH host's tmux sessions (Iced's remote-content Refresh).
    fn list_sessions(&self, host: &RemoteHostConfig) -> RemoteSessionsSnapshot;
    /// The phones paired with the agent (Iced's `list_phones`, capability
    /// "phone"). Write calls below require the shell's Live authorization.
    fn list_phones(&self, remote: &RemoteAgentConfig) -> Result<Vec<PairedPhoneInfo>, String>;
    fn create_phone_pairing(
        &self,
        _remote: &RemoteAgentConfig,
        _device_name: String,
        _scope: String,
    ) -> Result<PhonePairing, String> {
        Err("phone pairing unavailable".into())
    }
    fn revoke_phone(&self, _remote: &RemoteAgentConfig, _id: String) -> Result<bool, String> {
        Err("phone revocation unavailable".into())
    }
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
    fn start_session(
        &self,
        remote: &RemoteAgentConfig,
        workspace: String,
        launch: gitterm_core::remote::launch::AgentSessionLaunch,
    ) -> Result<String, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(backend.start_session(
            workspace,
            launch.cwd,
            launch.kind,
            launch.command,
            launch.env,
            launch.cols,
            launch.rows,
        ))?
        .map(|session| session.session_id)
        .map_err(|error| error.to_string())
    }
    fn chat_runtime(
        &self,
        remote: &RemoteAgentConfig,
        config: gitterm_core::harness::claude::ClaudeSessionConfig,
        attach: Option<String>,
    ) -> Result<
        (
            gitterm_core::chat::runtime::Session,
            tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::types::HarnessEvent>,
        ),
        String,
    > {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        Ok(match attach {
            Some(id) => gitterm_core::harness::remote_chat::attach_remote(backend, config, id),
            None => gitterm_core::harness::remote_chat::spawn_remote(backend, config),
        })
    }
    fn chat_history(
        &self,
        remote: &RemoteAgentConfig,
        id: &str,
    ) -> Result<Vec<gitterm_core::chat::state::AgentEvent>, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        let entries = block_on(backend.claude_chat_history(id.into()))?
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        Ok(gitterm_core::chat::history::history_timeline(entries, &[])
            .into_iter()
            .map(|event| match event {
                gitterm_core::chat::history::HistoryEvent::Harness(event) => {
                    gitterm_core::chat::state::AgentEvent::Harness(event)
                }
                gitterm_core::chat::history::HistoryEvent::Other(value) => {
                    gitterm_core::chat::state::AgentEvent::Other(value)
                }
            })
            .collect())
    }
    fn running_chat(&self, remote: &RemoteAgentConfig, id: &str) -> Result<Option<String>, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(async {
            let handshake = backend
                .handshake()
                .await
                .map_err(|error| error.to_string())?;
            if !handshake
                .capabilities
                .iter()
                .any(|c| c == gitterm_core::harness::remote_chat::CLAUDE_CHAT_PERSISTENT_CAPABILITY)
            {
                return Ok(None);
            }
            Ok(backend
                .list_claude_chats()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|chat| chat.claude_session_id == id)
                .map(|chat| chat.chat_id))
        })?
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
    fn list_phones(&self, remote: &RemoteAgentConfig) -> Result<Vec<PairedPhoneInfo>, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(backend.list_phones())?.map_err(|error| error.to_string())
    }
    fn create_phone_pairing(
        &self,
        remote: &RemoteAgentConfig,
        device_name: String,
        scope: String,
    ) -> Result<PhonePairing, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(backend.create_phone_pairing(device_name, scope))?
            .map_err(|error| error.to_string())
    }
    fn revoke_phone(&self, remote: &RemoteAgentConfig, id: String) -> Result<bool, String> {
        let backend = PreviewSourceHost::backend(remote.clone().into());
        block_on(backend.revoke_phone(id))?.map_err(|error| error.to_string())
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
    fn list_phones(&self, _: &RemoteAgentConfig) -> Result<Vec<PairedPhoneInfo>, String> {
        Err("offline in tests".into())
    }
}
