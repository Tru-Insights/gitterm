//! Host facade: credential lookup and phone pairing remain desktop-owned.
use super::protocol::v6::git_term_agent_client::GitTermAgentClient;
use super::protocol::v6::{CreatePhonePairingRequest, ListPhonesRequest, RevokePhoneRequest};
use gitterm_core::remote::client::connect_channel;
#[cfg(test)]
use gitterm_core::remote::client::{rpc_failed, PROTOCOL_VERSION};
pub use gitterm_core::remote::client::{
    ClaudeChatEventStream, ClaudeChatFrame, ClaudeChatFrameStream, ClaudeChatOpen,
    ClaudeChatOpened, RemoteAgentClientConfig, RemoteAgentClientError, RemoteAgentDiff,
    RemoteAgentDirEntry, RemoteAgentDirectory, RemoteAgentFileContent, RemoteAgentGitFile,
    RemoteAgentGitStatus, RemoteAgentHandshake, RemoteAgentSession, RunningClaudeChat,
    RunningClaudeChatState, TerminalAttachment, TerminalRecv, TerminalSend, OLDER_AGENT_MESSAGE,
};
use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration;
use tonic::Request;

/// A one-time phone pairing from `CreatePhonePairing` (TRU-151).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhonePairing {
    /// Open on the phone; the code is in its fragment.
    pub pairing_url: String,
    pub expires_unix_secs: u64,
    /// The scope the phone gets ("view" or "act").
    pub scope: String,
    /// The same pairing, typed into the phone's home-screen app
    /// (`XXXX-XXXX`). Empty from an agent that predates short codes.
    pub short_code: String,
}

/// A phone paired with an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPhoneInfo {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub created_unix_secs: u64,
    /// 0: not used since pairing.
    pub last_seen_unix_secs: u64,
}

#[derive(Debug, Clone)]
pub struct RemoteAgentBackend {
    inner: gitterm_core::remote::client::RemoteAgentBackend,
}
impl std::ops::Deref for RemoteAgentBackend {
    type Target = gitterm_core::remote::client::RemoteAgentBackend;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl RemoteAgentBackend {
    pub fn new(config: RemoteAgentClientConfig) -> Self {
        Self {
            inner: gitterm_core::remote::client::RemoteAgentBackend::new(
                config,
                resolve_token_ref,
                env!("CARGO_PKG_VERSION"),
            ),
        }
    }
    /// Transfer the configured transport to a source adapter without resolving a token.
    pub fn into_core(self) -> gitterm_core::remote::client::RemoteAgentBackend {
        self.inner
    }

    /// A one-time pairing for a phone named `device_name` (capability
    /// "phone").
    pub async fn create_phone_pairing(
        &self,
        device_name: String,
        scope: String,
    ) -> Result<PhonePairing, RemoteAgentClientError> {
        let mut request = Request::new(CreatePhonePairingRequest { device_name, scope });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.inner.config().endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .create_phone_pairing(request)
            .await
            .map_err(|err| {
                RemoteAgentClientError::new(format!(
                    "create_phone_pairing failed: {}",
                    err.message()
                ))
            })?
            .into_inner();
        // The long code itself is only ever used inside the URL.
        Ok(PhonePairing {
            pairing_url: response.pairing_url,
            expires_unix_secs: response.expires_unix_secs,
            scope: response.scope,
            short_code: response.short_code,
        })
    }

    /// The phones paired with the agent (capability "phone").
    pub async fn list_phones(&self) -> Result<Vec<PairedPhoneInfo>, RemoteAgentClientError> {
        let mut request = Request::new(ListPhonesRequest {});
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.inner.config().endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .list_phones(request)
            .await
            .map_err(|err| {
                RemoteAgentClientError::new(format!("list_phones failed: {}", err.message()))
            })?
            .into_inner();
        Ok(response
            .phones
            .into_iter()
            .map(|phone| PairedPhoneInfo {
                id: phone.id,
                name: phone.name,
                scope: phone.scope,
                created_unix_secs: phone.created_unix_secs,
                last_seen_unix_secs: phone.last_seen_unix_secs,
            })
            .collect())
    }

    /// Revoke a paired phone; `false` when the agent had no phone `id`.
    pub async fn revoke_phone(&self, id: String) -> Result<bool, RemoteAgentClientError> {
        let mut request = Request::new(RevokePhoneRequest { id });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.inner.config().endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .revoke_phone(request)
            .await
            .map_err(|err| {
                RemoteAgentClientError::new(format!("revoke_phone failed: {}", err.message()))
            })?
            .into_inner();
        Ok(response.revoked)
    }
}

pub(crate) fn resolve_token_ref(token_ref: &str) -> Result<String, RemoteAgentClientError> {
    let token_ref = token_ref.trim();
    if token_ref.is_empty() {
        return Err(RemoteAgentClientError::new("token_ref must not be empty"));
    }

    if let Some(name) = token_ref.strip_prefix("env:") {
        let name = name.trim();
        if name.is_empty() {
            return Err(RemoteAgentClientError::new(
                "env token_ref is missing a variable name",
            ));
        }
        return std::env::var(name).map_err(|err| {
            RemoteAgentClientError::new(format!("could not read token from env:{name}: {err}"))
        });
    }

    if let Some(path) = token_ref.strip_prefix("file:") {
        let path = expand_home_path(path.trim());
        let token = std::fs::read_to_string(&path).map_err(|err| {
            RemoteAgentClientError::new(format!(
                "could not read token file {}: {err}",
                path.display()
            ))
        })?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(RemoteAgentClientError::new(format!(
                "token file {} is empty",
                path.display()
            )));
        }
        return Ok(token);
    }

    if let Some(keychain_ref) = token_ref.strip_prefix("keychain:") {
        return read_keychain_token(keychain_ref.trim());
    }

    Err(RemoteAgentClientError::new(format!(
        "unsupported token_ref {token_ref}; expected env:, file:, or keychain:"
    )))
}

fn expand_home_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(target_os = "macos")]
fn read_keychain_token(keychain_ref: &str) -> Result<String, RemoteAgentClientError> {
    let (service, account) = keychain_ref.rsplit_once('/').ok_or_else(|| {
        RemoteAgentClientError::new("keychain token_ref must be keychain:service/account")
    })?;
    if service.is_empty() || account.is_empty() {
        return Err(RemoteAgentClientError::new(
            "keychain token_ref must include service and account",
        ));
    }

    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|err| RemoteAgentClientError::new(format!("could not run security: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            output.status.to_string()
        } else {
            stderr
        };
        return Err(RemoteAgentClientError::new(format!(
            "could not read keychain token for {service}/{account}: {detail}"
        )));
    }

    let token = String::from_utf8(output.stdout)
        .map_err(|err| RemoteAgentClientError::new(format!("keychain token is not UTF-8: {err}")))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(RemoteAgentClientError::new(format!(
            "keychain token for {service}/{account} is empty"
        )));
    }
    Ok(token)
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_token(_keychain_ref: &str) -> Result<String, RemoteAgentClientError> {
    Err(RemoteAgentClientError::new(
        "keychain token refs are only supported on macOS",
    ))
}

#[cfg(test)]
mod wire {
    pub use crate::agentd::protocol::v6::*;
}
#[cfg(test)]
#[path = "../../tests/support/remote_fixture.rs"]
mod remote_fixture;

#[cfg(test)]
#[allow(clippy::result_large_err)] // Tonic interceptors return `Status` directly in test services.
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;

    use super::super::protocol::v6::git_term_agent_server::GitTermAgentServer;
    use super::super::server::{is_authorized_metadata, GitTermAgentService};

    #[test]
    fn unimplemented_rpc_names_the_older_agent() {
        let err = rpc_failed("handshake", tonic::Status::unimplemented("no such service"));
        assert!(err.to_string().contains(OLDER_AGENT_MESSAGE), "{err}");
        assert!(err.to_string().contains("handshake"), "{err}");

        let err = rpc_failed("handshake", tonic::Status::unavailable("down"));
        assert!(!err.to_string().contains(OLDER_AGENT_MESSAGE), "{err}");
        assert!(err.to_string().starts_with("handshake failed"), "{err}");
    }

    /// An agent without the `gitterm.agent.v6` service (a V5 agent serves
    /// only `gitterm.agent.v5`) answers `Unimplemented`; the handshake must
    /// fail promptly with the upgrade message rather than hang or misparse.
    #[tokio::test]
    async fn handshake_against_an_agent_without_the_v6_service_says_upgrade() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_routes(tonic::service::Routes::default())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        std::env::set_var("GITTERM_AGENT_OLDER_TEST_TOKEN", "older-secret");
        let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
            remote_id: "older".to_string(),
            name: "Older".to_string(),
            endpoint: format!("http://{addr}"),
            token_ref: "env:GITTERM_AGENT_OLDER_TEST_TOKEN".to_string(),
        });
        let err = tokio::time::timeout(Duration::from_secs(10), backend.handshake())
            .await
            .expect("handshake against an older agent must not hang")
            .unwrap_err();
        assert!(err.to_string().contains(OLDER_AGENT_MESSAGE), "{err}");

        server.abort();
    }

    #[test]
    fn token_ref_rejects_empty_value() {
        let err = resolve_token_ref("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn token_ref_reads_env_value() {
        std::env::set_var("GITTERM_AGENT_CLIENT_TEST_TOKEN", "from-env");
        let token = resolve_token_ref("env:GITTERM_AGENT_CLIENT_TEST_TOKEN").unwrap();
        assert_eq!(token, "from-env");
    }

    #[test]
    fn token_ref_reads_file_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "from-file\n").unwrap();

        let token = resolve_token_ref(&format!("file:{}", path.display())).unwrap();
        assert_eq!(token, "from-file");
    }

    #[test]
    fn token_ref_rejects_unknown_scheme() {
        let err = resolve_token_ref("secret").unwrap_err();
        assert!(err.to_string().contains("unsupported token_ref"));
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn backend_handshake_uses_endpoint_and_token_ref() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let expected_token = Arc::new("phase3-secret".to_string());
        let service = GitTermAgentService::new("phase3-agent".to_string());
        let authenticated_service =
            GitTermAgentServer::with_interceptor(service, move |request: Request<()>| {
                if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                    Ok(request)
                } else {
                    Err(tonic::Status::unauthenticated(
                        "missing or invalid bearer token",
                    ))
                }
            });

        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(authenticated_service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        std::env::set_var("GITTERM_AGENT_PHASE3_TEST_TOKEN", "phase3-secret");
        let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
            remote_id: "phase3".to_string(),
            name: "Phase 3".to_string(),
            endpoint: format!("http://{addr}"),
            token_ref: "env:GITTERM_AGENT_PHASE3_TEST_TOKEN".to_string(),
        });

        let handshake = backend.handshake().await.unwrap();
        assert_eq!(handshake.remote_id, "phase3");
        assert_eq!(handshake.agent_name, "phase3-agent");
        assert_eq!(handshake.protocol_version, PROTOCOL_VERSION);
        assert!(handshake.capabilities.contains(&"handshake".to_string()));

        server.abort();
    }

    #[tokio::test]
    async fn backend_session_lifecycle_over_grpc() {
        use tokio_stream::StreamExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_token = Arc::new("sess-secret".to_string());
        let service = GitTermAgentServer::with_interceptor(
            GitTermAgentService::new("sess-agent".to_string()),
            move |request: Request<()>| {
                if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                    Ok(request)
                } else {
                    Err(tonic::Status::unauthenticated("bad token"))
                }
            },
        );
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        std::env::set_var("GITTERM_AGENT_SESS_TEST_TOKEN", "sess-secret");
        let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
            remote_id: "sess".to_string(),
            name: "sess".to_string(),
            endpoint: format!("http://{addr}"),
            token_ref: "env:GITTERM_AGENT_SESS_TEST_TOKEN".to_string(),
        });

        let session = backend
            .start_session(
                "ws".to_string(),
                std::env::temp_dir().to_string_lossy().to_string(),
                "shell".to_string(),
                vec!["/bin/cat".to_string()],
                Vec::new(),
                80,
                24,
            )
            .await
            .unwrap();
        assert!(session.running);

        let listed = backend.list_sessions("ws".to_string()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, session.session_id);

        let (input_tx, output) = backend
            .attach_terminal(session.session_id.clone(), 80, 24)
            .await
            .unwrap();
        input_tx
            .send(TerminalSend::Data(b"round-trip\n".to_vec()))
            .await
            .unwrap();

        tokio::pin!(output);
        let mut seen = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !seen.contains("round-trip") {
            let item = tokio::time::timeout_at(deadline, output.next())
                .await
                .expect("echo before deadline")
                .expect("stream open");
            match item.unwrap() {
                TerminalRecv::Data(data) => seen.push_str(&String::from_utf8_lossy(&data)),
                TerminalRecv::Exited(code) => panic!("unexpected exit {code}: {seen:?}"),
            }
        }

        assert!(backend
            .stop_session(session.session_id.clone())
            .await
            .unwrap());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let item = tokio::time::timeout_at(deadline, output.next())
                .await
                .expect("exit before deadline");
            match item {
                Some(Ok(TerminalRecv::Exited(_))) | None => break,
                Some(Ok(TerminalRecv::Data(_))) => continue,
                Some(Err(err)) => panic!("stream error: {err}"),
            }
        }

        server.abort();
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn backend_list_dir_uses_endpoint_and_token_ref() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\n").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let expected_token = Arc::new("phase4-secret".to_string());
        let service = GitTermAgentService::new("phase4-agent".to_string());
        let authenticated_service =
            GitTermAgentServer::with_interceptor(service, move |request: Request<()>| {
                if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                    Ok(request)
                } else {
                    Err(tonic::Status::unauthenticated(
                        "missing or invalid bearer token",
                    ))
                }
            });

        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(authenticated_service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        std::env::set_var("GITTERM_AGENT_PHASE4_TEST_TOKEN", "phase4-secret");
        let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
            remote_id: "phase4".to_string(),
            name: "Phase 4".to_string(),
            endpoint: format!("http://{addr}"),
            token_ref: "env:GITTERM_AGENT_PHASE4_TEST_TOKEN".to_string(),
        });

        let directory = backend
            .list_dir(
                "workspace".to_string(),
                root.path().to_string_lossy().to_string(),
                root.path().to_string_lossy().to_string(),
                false,
            )
            .await
            .unwrap();

        assert_eq!(directory.remote_id, "phase4");
        assert_eq!(directory.workspace_id, "workspace");
        let entries: Vec<(String, bool)> = directory
            .entries
            .into_iter()
            .map(|entry| (entry.name, entry.is_dir))
            .collect();
        assert_eq!(
            entries,
            vec![("src".to_string(), true), ("Cargo.toml".to_string(), false),]
        );

        server.abort();
    }

    #[tokio::test]
    async fn chats_round_trip_over_grpc() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = super::remote_fixture::Fixture::default();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(GitTermAgentServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        std::env::set_var("GITTERM_AGENT_CHATS_TEST_TOKEN", "fixture-token");
        let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
            remote_id: "chats".to_string(),
            name: "chats".to_string(),
            endpoint: format!("http://{addr}"),
            token_ref: "env:GITTERM_AGENT_CHATS_TEST_TOKEN".to_string(),
        });

        // Handshake and index round-trip against a disposable fixture.
        let handshake = backend.handshake().await.unwrap();
        assert!(handshake.capabilities.contains(&"read_file".to_string()));

        // The index and preview contain synthetic data only.
        let entries = backend.list_chats().await.unwrap();

        // The fixture refuses paths outside its synthetic index.
        let denied = backend
            .chat_preview("/etc/hosts".to_string(), crate::chats::ChatBackend::Claude)
            .await;
        assert!(denied.is_err(), "preview outside home must be rejected");

        // Preview the fixture transcript through the wire.
        assert_eq!(entries.len(), 1);
        {
            let entry = &entries[0];
            let preview = backend
                .chat_preview(entry.path.to_string_lossy().into_owned(), entry.backend)
                .await
                .unwrap();
            assert_eq!(preview.messages.len(), 1);
            assert_eq!(preview.messages[0].text, "fixture message");
            assert_eq!(preview.message_count, Some(1));
        }

        server.abort();
    }
}
