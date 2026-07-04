use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

use super::protocol::v1::git_term_agent_client::GitTermAgentClient;
use super::protocol::v1::GitDiffRequest;
use super::protocol::v1::GitStatusRequest;
use super::protocol::v1::HandshakeRequest;
use super::protocol::v1::ListDirRequest;
use super::protocol::v1::ReadFileRequest;
use super::protocol::v1::{
    terminal_input, AttachRequest, EnvVar, ListSessionsRequest, Resize, StartSessionRequest,
    StopSessionRequest, TerminalInput,
};
use super::server::PROTOCOL_VERSION;

#[derive(Debug, Clone)]
pub struct RemoteAgentClientConfig {
    pub remote_id: String,
    pub name: String,
    pub endpoint: String,
    pub token_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentHandshake {
    pub remote_id: String,
    pub endpoint: String,
    pub agent_name: String,
    pub agent_version: String,
    pub protocol_version: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentDirectory {
    pub remote_id: String,
    pub workspace_id: String,
    pub root: String,
    pub current_dir: String,
    pub entries: Vec<RemoteAgentDirEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentDirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentGitStatus {
    pub remote_id: String,
    pub root: String,
    pub is_git_repo: bool,
    pub repo_name: String,
    pub branch_name: String,
    pub staged: Vec<RemoteAgentGitFile>,
    pub unstaged: Vec<RemoteAgentGitFile>,
    pub untracked: Vec<RemoteAgentGitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentGitFile {
    pub path: String,
    pub status: String,
    pub is_staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentDiff {
    pub remote_id: String,
    pub file_path: String,
    pub staged: bool,
    pub lines: Vec<crate::agentd::git::FileDiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentSession {
    pub session_id: String,
    pub workspace_id: String,
    pub kind: String,
    pub command: String,
    pub cwd: String,
    pub running: bool,
    pub exit_code: i32,
    pub created_unix_secs: u64,
}

/// An attached session: the input/resize sender plus the live output
/// stream (scrollback replay arrives first).
pub type TerminalAttachment = (
    tokio::sync::mpsc::Sender<TerminalSend>,
    std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<TerminalRecv, RemoteAgentClientError>> + Send>,
    >,
);

/// Messages the attach bridge sends after the initial attach.
#[derive(Debug, Clone)]
pub enum TerminalSend {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Output events received from an attached session.
#[derive(Debug, Clone)]
pub enum TerminalRecv {
    Data(Vec<u8>),
    Exited(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentFileContent {
    pub remote_id: String,
    pub path: String,
    pub data: Vec<u8>,
    /// Full on-disk size, which can exceed `data.len()` when truncated.
    pub total_size: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteAgentBackend {
    config: RemoteAgentClientConfig,
}

impl RemoteAgentBackend {
    pub fn new(config: RemoteAgentClientConfig) -> Self {
        Self { config }
    }

    pub async fn handshake(&self) -> Result<RemoteAgentHandshake, RemoteAgentClientError> {
        let token = resolve_token_ref(&self.config.token_ref)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(HandshakeRequest {
            client_name: "GitTerm desktop".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        });

        let auth_header = format!("Bearer {token}");
        let auth_value = auth_header
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);

        let response = client
            .handshake(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("handshake failed: {err:?}")))?
            .into_inner();

        Ok(RemoteAgentHandshake {
            remote_id: self.config.remote_id.clone(),
            endpoint: self.config.endpoint.clone(),
            agent_name: response.agent_name,
            agent_version: response.agent_version,
            protocol_version: response.protocol_version,
            capabilities: response.capabilities,
        })
    }

    pub async fn list_dir(
        &self,
        workspace_id: String,
        root: String,
        current_dir: String,
        show_hidden: bool,
    ) -> Result<RemoteAgentDirectory, RemoteAgentClientError> {
        let token = resolve_token_ref(&self.config.token_ref)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(ListDirRequest {
            workspace_id,
            root,
            current_dir,
            show_hidden,
        });

        let auth_header = format!("Bearer {token}");
        let auth_value = auth_header
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);

        let response = client
            .list_dir(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("list_dir failed: {err:?}")))?
            .into_inner();

        Ok(RemoteAgentDirectory {
            remote_id: self.config.remote_id.clone(),
            workspace_id: response.workspace_id,
            root: response.root,
            current_dir: response.current_dir,
            entries: response
                .entries
                .into_iter()
                .map(|entry| RemoteAgentDirEntry {
                    name: entry.name,
                    path: entry.path,
                    is_dir: entry.is_dir,
                })
                .collect(),
        })
    }

    pub async fn read_file(
        &self,
        workspace_id: String,
        root: String,
        path: String,
        max_bytes: u64,
    ) -> Result<RemoteAgentFileContent, RemoteAgentClientError> {
        let token = resolve_token_ref(&self.config.token_ref)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(ReadFileRequest {
            workspace_id,
            root,
            path: path.clone(),
            max_bytes,
        });

        let auth_header = format!("Bearer {token}");
        let auth_value = auth_header
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);

        let mut stream = client
            .read_file(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("read_file failed: {err:?}")))?
            .into_inner();

        let mut data = Vec::new();
        let mut total_size = 0u64;
        let mut truncated = false;
        while let Some(chunk) = stream.message().await.map_err(|err| {
            RemoteAgentClientError::new(format!("read_file stream failed: {err:?}"))
        })? {
            total_size = chunk.total_size;
            truncated = chunk.truncated;
            data.extend_from_slice(&chunk.data);
        }

        Ok(RemoteAgentFileContent {
            remote_id: self.config.remote_id.clone(),
            path,
            data,
            total_size,
            truncated,
        })
    }

    pub async fn git_status(
        &self,
        workspace_id: String,
        root: String,
    ) -> Result<RemoteAgentGitStatus, RemoteAgentClientError> {
        let token = resolve_token_ref(&self.config.token_ref)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(GitStatusRequest { workspace_id, root });

        let auth_header = format!("Bearer {token}");
        let auth_value = auth_header
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);

        let response = client
            .git_status(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("git_status failed: {err:?}")))?
            .into_inner();

        let map = |file: super::protocol::v1::GitFileStatus| RemoteAgentGitFile {
            path: file.path,
            status: file.status,
            is_staged: file.is_staged,
        };
        Ok(RemoteAgentGitStatus {
            remote_id: self.config.remote_id.clone(),
            root: response.root,
            is_git_repo: response.is_git_repo,
            repo_name: response.repo_name,
            branch_name: response.branch_name,
            staged: response.staged.into_iter().map(map).collect(),
            unstaged: response.unstaged.into_iter().map(map).collect(),
            untracked: response.untracked.into_iter().map(map).collect(),
        })
    }

    pub async fn git_diff(
        &self,
        workspace_id: String,
        root: String,
        file_path: String,
        staged: bool,
    ) -> Result<RemoteAgentDiff, RemoteAgentClientError> {
        let token = resolve_token_ref(&self.config.token_ref)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(GitDiffRequest {
            workspace_id,
            root,
            file_path,
            staged,
        });

        let auth_header = format!("Bearer {token}");
        let auth_value = auth_header
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);

        let response = client
            .git_diff(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("git_diff failed: {err:?}")))?
            .into_inner();

        use super::protocol::v1::GitDiffLineKind;
        let lines = response
            .lines
            .into_iter()
            .map(|line| crate::agentd::git::FileDiffLine {
                content: line.content,
                kind: match GitDiffLineKind::try_from(line.kind) {
                    Ok(GitDiffLineKind::Addition) => crate::agentd::git::DiffLineKind::Addition,
                    Ok(GitDiffLineKind::Deletion) => crate::agentd::git::DiffLineKind::Deletion,
                    Ok(GitDiffLineKind::Header) => crate::agentd::git::DiffLineKind::Header,
                    _ => crate::agentd::git::DiffLineKind::Context,
                },
                old_line: (line.old_line > 0).then_some(line.old_line),
                new_line: (line.new_line > 0).then_some(line.new_line),
            })
            .collect();

        Ok(RemoteAgentDiff {
            remote_id: self.config.remote_id.clone(),
            file_path: response.file_path,
            staged: response.staged,
            lines,
        })
    }

    pub async fn list_sessions(
        &self,
        workspace_id: String,
    ) -> Result<Vec<RemoteAgentSession>, RemoteAgentClientError> {
        let mut request = Request::new(ListSessionsRequest { workspace_id });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .list_sessions(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("list_sessions failed: {err:?}")))?
            .into_inner();
        Ok(response
            .sessions
            .into_iter()
            .map(session_from_proto)
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_session(
        &self,
        workspace_id: String,
        cwd: String,
        kind: String,
        command: Vec<String>,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    ) -> Result<RemoteAgentSession, RemoteAgentClientError> {
        let mut request = Request::new(StartSessionRequest {
            workspace_id,
            cwd,
            kind,
            command,
            env: env
                .into_iter()
                .map(|(name, value)| EnvVar { name, value })
                .collect(),
            cols: cols as u32,
            rows: rows as u32,
        });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .start_session(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("start_session failed: {err:?}")))?
            .into_inner();
        Ok(session_from_proto(response))
    }

    pub async fn stop_session(&self, session_id: String) -> Result<bool, RemoteAgentClientError> {
        let mut request = Request::new(StopSessionRequest { session_id });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .stop_session(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("stop_session failed: {err:?}")))?
            .into_inner();
        Ok(response.stopped)
    }

    /// Attach to a session: returns a sender for input/resizes and the
    /// live output stream (recent scrollback replays first). The connection
    /// has no request timeout — it lives as long as the session/attach.
    pub async fn attach_terminal(
        &self,
        session_id: String,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalAttachment, RemoteAgentClientError> {
        use tokio_stream::StreamExt;

        let token = resolve_token_ref(&self.config.token_ref)?;
        // No per-request timeout: this stream is long-lived by design.
        let endpoint = Endpoint::from_shared(self.config.endpoint.clone())
            .map_err(|err| {
                RemoteAgentClientError::new(format!(
                    "invalid endpoint {}: {err}",
                    self.config.endpoint
                ))
            })?
            .connect_timeout(Duration::from_secs(5));
        let endpoint = if self.config.endpoint.starts_with("https://") {
            endpoint
                .tls_config(ClientTlsConfig::new().with_enabled_roots())
                .map_err(|err| RemoteAgentClientError::new(format!("invalid TLS config: {err}")))?
        } else {
            endpoint
        };
        let channel = endpoint.connect().await.map_err(|err| {
            RemoteAgentClientError::new(format!(
                "could not connect to {}: {err:?}",
                self.config.endpoint
            ))
        })?;

        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<TerminalSend>(64);
        let attach = TerminalInput {
            input: Some(terminal_input::Input::Attach(AttachRequest {
                session_id,
                cols: cols as u32,
                rows: rows as u32,
            })),
        };
        let outbound = tokio_stream::once(attach).chain(
            tokio_stream::wrappers::ReceiverStream::new(input_rx).map(|send| match send {
                TerminalSend::Data(data) => TerminalInput {
                    input: Some(terminal_input::Input::Data(data)),
                },
                TerminalSend::Resize { cols, rows } => TerminalInput {
                    input: Some(terminal_input::Input::Resize(Resize {
                        cols: cols as u32,
                        rows: rows as u32,
                    })),
                },
            }),
        );

        let mut request = Request::new(outbound);
        let auth_value = format!("Bearer {token}")
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);

        let stream = GitTermAgentClient::new(channel)
            .attach_terminal(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("attach_terminal failed: {err:?}")))?
            .into_inner();

        use super::protocol::v1::terminal_output;
        let output = stream.map(|item| {
            item.map_err(|err| RemoteAgentClientError::new(format!("attach stream: {err:?}")))
                .map(|out| match out.output {
                    Some(terminal_output::Output::Data(data)) => TerminalRecv::Data(data),
                    Some(terminal_output::Output::Exited(exited)) => {
                        TerminalRecv::Exited(exited.exit_code)
                    }
                    None => TerminalRecv::Data(Vec::new()),
                })
        });

        Ok((input_tx, Box::pin(output)))
    }

    fn authorize<T>(&self, request: &mut Request<T>) -> Result<(), RemoteAgentClientError> {
        let token = resolve_token_ref(&self.config.token_ref)?;
        let auth_value = format!("Bearer {token}")
            .parse()
            .map_err(|err| RemoteAgentClientError::new(format!("invalid token metadata: {err}")))?;
        request.metadata_mut().insert("authorization", auth_value);
        Ok(())
    }
}

fn session_from_proto(session: super::protocol::v1::Session) -> RemoteAgentSession {
    RemoteAgentSession {
        session_id: session.session_id,
        workspace_id: session.workspace_id,
        kind: session.kind,
        command: session.command,
        cwd: session.cwd,
        running: session.running,
        exit_code: session.exit_code,
        created_unix_secs: session.created_unix_secs,
    }
}

async fn connect_channel(endpoint: &str) -> Result<Channel, RemoteAgentClientError> {
    let mut builder = Endpoint::from_shared(endpoint.to_string())
        .map_err(|err| RemoteAgentClientError::new(format!("invalid endpoint {endpoint}: {err}")))?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10));

    if endpoint.starts_with("https://") {
        builder = builder
            .tls_config(ClientTlsConfig::new().with_enabled_roots())
            .map_err(|err| {
                RemoteAgentClientError::new(format!("invalid TLS config for {endpoint}: {err}"))
            })?;
    }

    builder.connect().await.map_err(|err| {
        RemoteAgentClientError::new(format!("could not connect to {endpoint}: {err:?}"))
    })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentClientError {
    message: String,
}

impl RemoteAgentClientError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteAgentClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RemoteAgentClientError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;

    use super::super::protocol::v1::git_term_agent_server::GitTermAgentServer;
    use super::super::server::{is_authorized_metadata, GitTermAgentService};

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
}
