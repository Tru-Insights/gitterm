use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tonic::metadata::MetadataMap;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use super::claude_chats::{
    ChatActivity, ChatAttachment, ChatInfo, ChatLimits, ClaudeChatRegistry, Gap, SeqEvent,
};
use super::phone_http::{PhoneServerConfig, PhoneState};
use super::phones::{PhoneRegistry, PhoneScope};
use super::protocol::v5::git_term_agent_server::{GitTermAgent, GitTermAgentServer};
use super::protocol::v5::{
    claude_chat_client_message, terminal_input, terminal_output, ChatEntry, ChatPreviewMessage,
    ChatPreviewRequest, ChatPreviewResponse, ClaudeChatClientMessage, ClaudeChatHistoryRequest,
    ClaudeChatHistoryResponse, ClaudeChatInfo, ClaudeChatOpened, ClaudeChatServerMessage,
    ClaudeChatState, CreatePhonePairingRequest, CreatePhonePairingResponse, DirEntry,
    GitDiffRequest, GitDiffResponse, GitStatusRequest, GitStatusResponse, HandshakeRequest,
    HandshakeResponse, ListChatsRequest, ListChatsResponse, ListClaudeChatsRequest,
    ListClaudeChatsResponse, ListDirRequest, ListDirResponse, ListPhonesRequest,
    ListPhonesResponse, ListSessionsRequest, ListSessionsResponse, PairedPhone, ReadFileChunk,
    ReadFileRequest, RevokePhoneRequest, RevokePhoneResponse, Session, SessionExited,
    StartSessionRequest, StopSessionRequest, StopSessionResponse, TerminalInput, TerminalOutput,
};
use super::sessions::{SessionManager, SessionOutput};
use crate::harness::claude::{ClaudeSessionConfig, RemoteClaudeChatStart};
use crate::harness::{HarnessCommand, HarnessEvent};

pub const PROTOCOL_VERSION: u32 = 1;

/// Handshake capability of an agent that owns its Claude chats (TRU-152):
/// streams attach and detach, events carry a seq, `ListClaudeChats`.
pub const CLAUDE_CHAT_PERSISTENT_CAPABILITY: &str = "claude_chat_persistent";

/// Handshake capability of an agent serving the phone web app (TRU-151):
/// `CreatePhonePairing`, `ListPhones`, `RevokePhone`.
pub const PHONE_CAPABILITY: &str = "phone";

/// Hard ceiling for one ReadFile stream; requests may ask for less.
const READ_FILE_DEFAULT_MAX_BYTES: u64 = 2_000_000;
const READ_FILE_CHUNK_BYTES: usize = 64 * 1024;

/// How often idle Claude chats are looked for.
const CLAUDE_CHAT_REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// HTTP/2 pings, so a client that vanished without closing its connection
/// (a laptop lid, a dropped network) is detached rather than counted as
/// attached forever.
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct AgentServerConfig {
    pub bind_addr: SocketAddr,
    pub auth_token: String,
    pub agent_name: String,
    /// Replay buffer bounds and idle timeout of agent-owned Claude chats.
    pub claude_chat_limits: ChatLimits,
    /// The phone web app (TRU-151); `None` leaves it off.
    pub phone: Option<PhoneServerConfig>,
}

/// The phone web app as the gRPC service sees it: its paired phones and
/// the base URL pairing links point at.
#[derive(Clone)]
pub struct PhoneAccess {
    pub phones: Arc<PhoneRegistry>,
    /// `http://host:port`, no trailing slash.
    pub base_url: String,
}

#[derive(Clone)]
pub struct GitTermAgentService {
    agent_name: String,
    sessions: Arc<SessionManager>,
    claude_chats: Arc<ClaudeChatRegistry>,
    phone: Option<PhoneAccess>,
}

impl GitTermAgentService {
    pub fn new(agent_name: String) -> Self {
        Self::with_claude_chat_limits(agent_name, ChatLimits::default())
    }

    pub fn with_claude_chat_limits(agent_name: String, limits: ChatLimits) -> Self {
        Self {
            agent_name,
            sessions: Arc::new(SessionManager::default()),
            claude_chats: ClaudeChatRegistry::new(limits),
            phone: None,
        }
    }

    /// The agent's running Claude chats, shared with the phone server.
    pub fn claude_chats(&self) -> Arc<ClaudeChatRegistry> {
        self.claude_chats.clone()
    }

    /// Serve the phone RPCs for this phone web app (and advertise
    /// `PHONE_CAPABILITY`).
    pub fn with_phone_access(mut self, phone: PhoneAccess) -> Self {
        self.phone = Some(phone);
        self
    }

    #[allow(clippy::result_large_err)]
    fn phone_access(&self) -> Result<&PhoneAccess, Status> {
        self.phone.as_ref().ok_or_else(|| {
            Status::failed_precondition("phone access is off on this agent (--phone-addr off)")
        })
    }
}

#[tonic::async_trait]
impl GitTermAgent for GitTermAgentService {
    async fn handshake(
        &self,
        request: Request<HandshakeRequest>,
    ) -> Result<Response<HandshakeResponse>, Status> {
        let request = request.into_inner();
        if request.protocol_version > PROTOCOL_VERSION {
            return Err(Status::failed_precondition(format!(
                "client protocol {} is newer than agent protocol {}",
                request.protocol_version, PROTOCOL_VERSION
            )));
        }

        // Optional by contract: an empty home_dir makes the desktop picker
        // start at "/", so log instead of failing the handshake.
        let home_dir = match dirs::home_dir() {
            Some(home) => home.to_string_lossy().to_string(),
            None => {
                eprintln!("[agent] handshake: home directory unavailable; sending empty home_dir");
                String::new()
            }
        };

        let mut capabilities = vec![
            "handshake".to_string(),
            "list_dir".to_string(),
            "read_file".to_string(),
            "git_status".to_string(),
            "git_diff".to_string(),
            "sessions".to_string(),
            "chats".to_string(),
            "claude_chat".to_string(),
            CLAUDE_CHAT_PERSISTENT_CAPABILITY.to_string(),
        ];
        if self.phone.is_some() {
            capabilities.push(PHONE_CAPABILITY.to_string());
        }
        Ok(Response::new(HandshakeResponse {
            agent_name: self.agent_name.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            capabilities,
            home_dir,
        }))
    }

    async fn list_dir(
        &self,
        request: Request<ListDirRequest>,
    ) -> Result<Response<ListDirResponse>, Status> {
        let request = request.into_inner();
        let response = list_dir_response(request)?;
        Ok(Response::new(response))
    }

    type ReadFileStream = tokio_stream::wrappers::ReceiverStream<Result<ReadFileChunk, Status>>;

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        let request = request.into_inner();
        let (path, total_size, read_limit) = validate_read_file_request(&request)?;

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let mut file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(err) => {
                    let _ = tx.blocking_send(Err(Status::internal(format!(
                        "could not open {}: {err}",
                        path.display()
                    ))));
                    return;
                }
            };
            let truncated = total_size > read_limit;
            let mut remaining = read_limit;
            let mut buf = vec![0u8; READ_FILE_CHUNK_BYTES];
            loop {
                let want = buf.len().min(remaining as usize);
                if want == 0 {
                    break;
                }
                match std::io::Read::read(&mut file, &mut buf[..want]) {
                    Ok(0) => break,
                    Ok(n) => {
                        remaining -= n as u64;
                        let chunk = ReadFileChunk {
                            data: buf[..n].to_vec(),
                            total_size,
                            truncated,
                        };
                        if tx.blocking_send(Ok(chunk)).is_err() {
                            return; // client went away
                        }
                    }
                    Err(err) => {
                        let _ = tx.blocking_send(Err(Status::internal(format!(
                            "read failed for {}: {err}",
                            path.display()
                        ))));
                        return;
                    }
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn git_status(
        &self,
        request: Request<GitStatusRequest>,
    ) -> Result<Response<GitStatusResponse>, Status> {
        let request = request.into_inner();
        if request.workspace_id.trim().is_empty() {
            return Err(Status::invalid_argument("workspace_id must not be empty"));
        }
        let root = canonical_dir(&request.root, "root")?;

        let response = tokio::task::spawn_blocking(move || {
            let status = super::git::collect_repo_status(&root);
            GitStatusResponse {
                workspace_id: request.workspace_id,
                root: status.root.to_string_lossy().to_string(),
                is_git_repo: status.is_git_repo,
                repo_name: status.repo_name,
                branch_name: status.branch_name,
                staged: status.staged.into_iter().map(proto_file_status).collect(),
                unstaged: status.unstaged.into_iter().map(proto_file_status).collect(),
                untracked: status
                    .untracked
                    .into_iter()
                    .map(proto_file_status)
                    .collect(),
            }
        })
        .await
        .map_err(|err| Status::internal(format!("git status task failed: {err}")))?;

        Ok(Response::new(response))
    }

    async fn git_diff(
        &self,
        request: Request<GitDiffRequest>,
    ) -> Result<Response<GitDiffResponse>, Status> {
        let request = request.into_inner();
        if request.workspace_id.trim().is_empty() {
            return Err(Status::invalid_argument("workspace_id must not be empty"));
        }
        if request.file_path.trim().is_empty() {
            return Err(Status::invalid_argument("file_path must not be empty"));
        }
        let file = Path::new(&request.file_path);
        if file.is_absolute()
            || file
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Status::invalid_argument(
                "file_path must be repo-relative without parent components",
            ));
        }
        let root = canonical_dir(&request.root, "root")?;

        let response = tokio::task::spawn_blocking(move || {
            let lines =
                super::git::collect_file_diff(&root, &request.file_path, request.staged, 3000);
            GitDiffResponse {
                workspace_id: request.workspace_id,
                file_path: request.file_path,
                staged: request.staged,
                lines: lines.into_iter().map(proto_diff_line).collect(),
            }
        })
        .await
        .map_err(|err| Status::internal(format!("git diff task failed: {err}")))?;

        Ok(Response::new(response))
    }

    async fn list_sessions(
        &self,
        request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let request = request.into_inner();
        let sessions = self
            .sessions
            .list_sessions(&request.workspace_id)
            .into_iter()
            .map(proto_session)
            .collect();
        Ok(Response::new(ListSessionsResponse { sessions }))
    }

    async fn list_chats(
        &self,
        _request: Request<ListChatsRequest>,
    ) -> Result<Response<ListChatsResponse>, Status> {
        let entries = tokio::task::spawn_blocking(crate::chats::build_local_index)
            .await
            .map_err(|err| Status::internal(format!("chat index task failed: {err}")))?;
        let chats = entries.into_iter().map(chat_entry_to_proto).collect();
        Ok(Response::new(ListChatsResponse { chats }))
    }

    async fn chat_preview(
        &self,
        request: Request<ChatPreviewRequest>,
    ) -> Result<Response<ChatPreviewResponse>, Status> {
        let request = request.into_inner();
        let backend = crate::chats::ChatBackend::from_label(&request.backend).ok_or_else(|| {
            Status::invalid_argument(format!("unknown chat backend {:?}", request.backend))
        })?;
        let path = PathBuf::from(&request.path);
        let canonical = path
            .canonicalize()
            .map_err(|err| Status::not_found(format!("transcript {}: {err}", path.display())))?;
        // Transcripts only ever live under the agent user's home; refuse
        // anything else so this RPC can't become an arbitrary file reader.
        let home =
            dirs::home_dir().ok_or_else(|| Status::internal("agent home directory unavailable"))?;
        if !canonical.starts_with(&home) {
            return Err(Status::permission_denied(
                "transcript path must be under the agent home directory",
            ));
        }
        let preview =
            tokio::task::spawn_blocking(move || crate::chats::load_preview(&canonical, backend))
                .await
                .map_err(|err| Status::internal(format!("chat preview task failed: {err}")))?;
        Ok(Response::new(ChatPreviewResponse {
            messages: preview
                .messages
                .into_iter()
                .map(|message| ChatPreviewMessage {
                    is_user: message.is_user,
                    text: message.text,
                })
                .collect(),
            message_count: preview.message_count,
        }))
    }

    async fn start_session(
        &self,
        request: Request<StartSessionRequest>,
    ) -> Result<Response<Session>, Status> {
        let request = request.into_inner();
        if request.workspace_id.trim().is_empty() {
            return Err(Status::invalid_argument("workspace_id must not be empty"));
        }
        let env = request
            .env
            .into_iter()
            .map(|var| (var.name, var.value))
            .collect();
        let info = self
            .sessions
            .start_session(
                request.workspace_id,
                request.cwd,
                request.kind,
                request.command,
                env,
                request.cols.min(u16::MAX as u32) as u16,
                request.rows.min(u16::MAX as u32) as u16,
            )
            .map_err(Status::invalid_argument)?;
        Ok(Response::new(proto_session(info)))
    }

    async fn stop_session(
        &self,
        request: Request<StopSessionRequest>,
    ) -> Result<Response<StopSessionResponse>, Status> {
        let request = request.into_inner();
        let stopped = self
            .sessions
            .stop_session(&request.session_id)
            .map_err(Status::not_found)?;
        Ok(Response::new(StopSessionResponse { stopped }))
    }

    type AttachTerminalStream =
        tokio_stream::wrappers::ReceiverStream<Result<TerminalOutput, Status>>;

    async fn attach_terminal(
        &self,
        request: Request<tonic::Streaming<TerminalInput>>,
    ) -> Result<Response<Self::AttachTerminalStream>, Status> {
        let mut input = request.into_inner();

        let first = input
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("input stream closed before attach"))?;
        let Some(terminal_input::Input::Attach(attach)) = first.input else {
            return Err(Status::invalid_argument(
                "first message must be an attach request",
            ));
        };

        let handles = self
            .sessions
            .attach(
                &attach.session_id,
                attach.cols.min(u16::MAX as u32) as u16,
                attach.rows.min(u16::MAX as u32) as u16,
            )
            .map_err(Status::not_found)?;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<TerminalOutput, Status>>(64);

        // Replay recent scrollback, then either report the exit (session
        // already finished) or stream live output.
        if !handles.replay.is_empty() {
            let _ = tx
                .send(Ok(TerminalOutput {
                    output: Some(terminal_output::Output::Data(handles.replay.clone())),
                }))
                .await;
        }
        if let Some(code) = handles.already_exited {
            let _ = tx
                .send(Ok(TerminalOutput {
                    output: Some(terminal_output::Output::Exited(SessionExited {
                        exit_code: code,
                    })),
                }))
                .await;
            return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )));
        }

        let mut output_rx = handles.output_rx;
        let output_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                match output_rx.recv().await {
                    Ok(SessionOutput::Data(chunk)) => {
                        if output_tx
                            .send(Ok(TerminalOutput {
                                output: Some(terminal_output::Output::Data(chunk)),
                            }))
                            .await
                            .is_err()
                        {
                            break; // client detached
                        }
                    }
                    Ok(SessionOutput::Exited(code)) => {
                        let _ = output_tx
                            .send(Ok(TerminalOutput {
                                output: Some(terminal_output::Output::Exited(SessionExited {
                                    exit_code: code,
                                })),
                            }))
                            .await;
                        break;
                    }
                    // Slow consumer: chunks were dropped; keep streaming.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let sessions = self.sessions.clone();
        let session_id = attach.session_id.clone();
        tokio::spawn(async move {
            while let Ok(Some(message)) = input.message().await {
                match message.input {
                    Some(terminal_input::Input::Data(data)) => {
                        let sessions = sessions.clone();
                        let session_id = session_id.clone();
                        // PTY writes can block when the child stops reading;
                        // keep them off the async runtime.
                        let result = tokio::task::spawn_blocking(move || {
                            sessions.write_input(&session_id, &data)
                        })
                        .await;
                        if !matches!(result, Ok(Ok(()))) {
                            break;
                        }
                    }
                    Some(terminal_input::Input::Resize(resize)) => {
                        let _ = sessions.resize(
                            &session_id,
                            resize.cols.min(u16::MAX as u32) as u16,
                            resize.rows.min(u16::MAX as u32) as u16,
                        );
                    }
                    Some(terminal_input::Input::Attach(_)) | None => {}
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type ClaudeChatStream =
        tokio_stream::wrappers::ReceiverStream<Result<ClaudeChatServerMessage, Status>>;

    async fn claude_chat(
        &self,
        request: Request<tonic::Streaming<ClaudeChatClientMessage>>,
    ) -> Result<Response<Self::ClaudeChatStream>, Status> {
        let mut input = request.into_inner();
        let (attachment, sequenced) = match claude_chat_opening(input.message().await?)? {
            ChatOpening::Start(config) => (self.claude_chats.start_or_attach(config, None), false),
            ChatOpening::StartOrAttach { config, after_seq } => {
                (self.claude_chats.start_or_attach(config, after_seq), true)
            }
            ChatOpening::Attach { chat_id, after_seq } => {
                let attachment =
                    self.claude_chats
                        .attach(&chat_id, after_seq)
                        .ok_or_else(|| {
                            eprintln!("[agent] claude_chat: attach to {chat_id}: not running");
                            Status::not_found(format!("no running Claude chat {chat_id}"))
                        })?;
                eprintln!(
                    "[agent] claude_chat {chat_id}: a client reattached after seq {after_seq:?}"
                );
                (attachment, true)
            }
        };

        // Commands are read on their own task so the relay selects over
        // two channels rather than over the gRPC stream.
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<ClaudeChatInput>();
        tokio::spawn(async move {
            loop {
                let next = match input.message().await {
                    Ok(Some(message)) => ClaudeChatInput::Message(message),
                    Ok(None) => ClaudeChatInput::Closed(None),
                    Err(status) => ClaudeChatInput::Closed(Some(status.to_string())),
                };
                let closed = matches!(next, ClaudeChatInput::Closed(_));
                if input_tx.send(next).is_err() || closed {
                    break;
                }
            }
        });

        let (tx, rx) = tokio::sync::mpsc::channel(256);
        tokio::spawn(relay_claude_chat(attachment, sequenced, input_rx, tx));
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn list_claude_chats(
        &self,
        _request: Request<ListClaudeChatsRequest>,
    ) -> Result<Response<ListClaudeChatsResponse>, Status> {
        let chats = self
            .claude_chats
            .list()
            .into_iter()
            .map(proto_claude_chat)
            .collect();
        Ok(Response::new(ListClaudeChatsResponse { chats }))
    }

    async fn create_phone_pairing(
        &self,
        request: Request<CreatePhonePairingRequest>,
    ) -> Result<Response<CreatePhonePairingResponse>, Status> {
        let phone = self.phone_access()?;
        let request = request.into_inner();
        let requested = PhoneScope::parse(&request.scope).map_err(Status::invalid_argument)?;
        let pairing = phone
            .phones
            .create_pairing(&request.device_name, requested)
            .map_err(|err| {
                eprintln!("[phone] could not create a pairing: {err}");
                Status::invalid_argument(err)
            })?;
        let expires_unix_secs = pairing
            .expires
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        eprintln!(
            "[phone] created a pairing for {:?} ({} scope{}), expiring at {expires_unix_secs}",
            request.device_name.trim(),
            pairing.scope.label(),
            if pairing.scope != requested {
                format!(", {} was asked", requested.label())
            } else {
                String::new()
            }
        );
        Ok(Response::new(CreatePhonePairingResponse {
            pairing_url: format!("{}/pair#{}", phone.base_url, pairing.code),
            code: pairing.code,
            expires_unix_secs,
            scope: pairing.scope.label().to_string(),
        }))
    }

    async fn list_phones(
        &self,
        _request: Request<ListPhonesRequest>,
    ) -> Result<Response<ListPhonesResponse>, Status> {
        let phone = self.phone_access()?;
        let phones = phone
            .phones
            .list()
            .into_iter()
            .map(|phone| PairedPhone {
                id: phone.id,
                name: phone.name,
                scope: phone.scope.label().to_string(),
                created_unix_secs: phone.created_unix_secs,
                last_seen_unix_secs: phone.last_seen_unix_secs,
            })
            .collect();
        Ok(Response::new(ListPhonesResponse { phones }))
    }

    async fn revoke_phone(
        &self,
        request: Request<RevokePhoneRequest>,
    ) -> Result<Response<RevokePhoneResponse>, Status> {
        let phone = self.phone_access()?;
        let id = request.into_inner().id;
        let revoked = phone.phones.revoke(&id).map_err(|err| {
            eprintln!("[phone] revoking phone {id}: {err}");
            Status::internal(err)
        })?;
        if revoked {
            eprintln!("[phone] revoked phone {id}");
        } else {
            eprintln!("[phone] revoke: no paired phone {id}");
        }
        Ok(Response::new(RevokePhoneResponse { revoked }))
    }

    #[allow(clippy::result_large_err)]
    async fn claude_chat_history(
        &self,
        request: Request<ClaudeChatHistoryRequest>,
    ) -> Result<Response<ClaudeChatHistoryResponse>, Status> {
        let session_id = request.into_inner().session_id;
        validate_claude_session_id(&session_id)?;
        let timeline_json = tokio::task::spawn_blocking(move || claude_history_json(&session_id))
            .await
            .map_err(|err| Status::internal(format!("chat history task failed: {err}")))??;
        Ok(Response::new(ClaudeChatHistoryResponse { timeline_json }))
    }
}

/// A Claude session's timeline as JSON, read from its transcript on this
/// machine. Blocking.
#[allow(clippy::result_large_err)]
fn claude_history_json(session_id: &str) -> Result<String, Status> {
    let path = crate::chats::claude_session_path(session_id).ok_or_else(|| {
        Status::not_found(format!("no transcript for Claude session {session_id}"))
    })?;
    let entries = crate::harness::transcript::load_claude_history(&path).map_err(|err| {
        Status::internal(format!(
            "could not read transcript {}: {err}",
            path.display()
        ))
    })?;
    serde_json::to_string(&entries).map_err(|err| {
        Status::internal(format!(
            "could not encode the timeline of {session_id}: {err}"
        ))
    })
}

/// What the command reader hands the relay: a client message, or the end
/// of the client's stream (with its error, when it failed).
enum ClaudeChatInput {
    Message(ClaudeChatClientMessage),
    Closed(Option<String>),
}

/// What a `ClaudeChat` stream's first message asks for.
#[derive(Debug)]
enum ChatOpening {
    /// A plain `start_json` (clients from before "claude_chat_persistent"):
    /// start, or attach by resume id; no `opened` message.
    Start(ClaudeSessionConfig),
    /// An `attach` without a chat id: the same, answered with `opened`.
    StartOrAttach {
        config: ClaudeSessionConfig,
        after_seq: Option<u64>,
    },
    /// An `attach` to a chat by id, answered with `opened`.
    Attach {
        chat_id: String,
        after_seq: Option<u64>,
    },
}

/// Decode a `ClaudeChat` stream's first message: a start, or an attach
/// carrying a chat id or a start.
#[allow(clippy::result_large_err)]
fn claude_chat_opening(first: Option<ClaudeChatClientMessage>) -> Result<ChatOpening, Status> {
    let first = first
        .ok_or_else(|| Status::invalid_argument("input stream closed before the start message"))?;
    match first.message {
        Some(claude_chat_client_message::Message::StartJson(json)) => {
            Ok(ChatOpening::Start(claude_chat_start_config(&json)?))
        }
        Some(claude_chat_client_message::Message::Attach(attach)) => {
            if !attach.chat_id.is_empty() {
                // Chat ids are agent-assigned uuids; anything else names no chat.
                validate_claude_session_id(&attach.chat_id)
                    .map_err(|_| Status::invalid_argument("invalid chat id"))?;
                return Ok(ChatOpening::Attach {
                    chat_id: attach.chat_id,
                    after_seq: attach.after_seq,
                });
            }
            if attach.start_json.is_empty() {
                return Err(Status::invalid_argument(
                    "an attach needs a chat_id or a start_json",
                ));
            }
            Ok(ChatOpening::StartOrAttach {
                config: claude_chat_start_config(&attach.start_json)?,
                after_seq: attach.after_seq,
            })
        }
        Some(claude_chat_client_message::Message::CommandJson(_)) | None => Err(
            Status::invalid_argument("first message must be a start message"),
        ),
    }
}

/// The session config a start carries: JSON that decodes, with a non-empty
/// permission mode, a well-formed resume id, and a cwd that is an existing
/// absolute directory on this machine.
#[allow(clippy::result_large_err)]
fn claude_chat_start_config(json: &str) -> Result<ClaudeSessionConfig, Status> {
    let start: RemoteClaudeChatStart = serde_json::from_str(json)
        .map_err(|err| Status::invalid_argument(format!("invalid start message: {err}")))?;
    if start.permission_mode.trim().is_empty() {
        return Err(Status::invalid_argument(
            "permission_mode must not be empty",
        ));
    }
    if let Some(resume) = &start.resume {
        validate_claude_session_id(resume)?;
    }
    let cwd = canonical_dir(&start.cwd, "cwd")?;
    Ok(start.into_config(cwd))
}

/// A Claude session id names a transcript file; refuse anything that could
/// step outside the projects directory.
#[allow(clippy::result_large_err)]
fn validate_claude_session_id(session_id: &str) -> Result<(), Status> {
    let valid = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "invalid Claude session id {session_id:?}"
        )))
    }
}

type ChatTx = tokio::sync::mpsc::Sender<Result<ClaudeChatServerMessage, Status>>;

/// A relayed event as this stream sends it.
fn chat_event_message(event: &SeqEvent, redelivered: bool) -> ClaudeChatServerMessage {
    ClaudeChatServerMessage {
        event_json: event.json.to_string(),
        seq: event.seq,
        opened: None,
        redelivered,
    }
}

/// An event for this stream only (seq 0), such as a refused command.
async fn send_stream_notice(tx: &ChatTx, event: &HarnessEvent) -> Result<(), ()> {
    let message = match serde_json::to_string(event) {
        Ok(event_json) => Ok(ClaudeChatServerMessage {
            event_json,
            ..Default::default()
        }),
        Err(err) => {
            eprintln!("[agent] claude_chat: could not encode {event:?}: {err}");
            Err(Status::internal(format!(
                "could not encode an event: {err}"
            )))
        }
    };
    tx.send(message).await.map_err(|_| ())
}

/// Send live events after `position`, advancing it. `Ok(true)` once the
/// chat's exit has been sent; `Err` when the client went away.
async fn send_live(
    tx: &ChatTx,
    events: impl IntoIterator<Item = SeqEvent>,
    position: &mut u64,
) -> Result<bool, ()> {
    for event in events {
        if event.seq <= *position {
            continue;
        }
        *position = event.seq;
        tx.send(Ok(chat_event_message(&event, false)))
            .await
            .map_err(|_| ())?;
        if event.exited {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Relay one stream's attachment to a chat: `opened` (for an `attach`
/// stream), the redelivered standing events, the replay, then live events
/// and the client's commands, until the chat exits or the client goes.
/// Ending the stream, from either side, only detaches; the chat runs on.
async fn relay_claude_chat(
    mut attachment: ChatAttachment,
    sequenced: bool,
    mut input: tokio::sync::mpsc::UnboundedReceiver<ClaudeChatInput>,
    tx: ChatTx,
) {
    let chat_id = attachment.opened.chat_id.clone();
    let gone = |why: &str| eprintln!("[agent] claude_chat {chat_id}: {why}; detaching");
    if sequenced {
        let opened = &attachment.opened;
        let message = ClaudeChatServerMessage {
            opened: Some(ClaudeChatOpened {
                chat_id: opened.chat_id.clone(),
                attached: opened.attached,
                claude_session_id: opened.claude_session_id.clone().unwrap_or_default(),
                head_seq: opened.head_seq,
                gap: opened.gap,
            }),
            ..Default::default()
        };
        if tx.send(Ok(message)).await.is_err() {
            return gone("client went away before it was opened");
        }
    }
    for event in std::mem::take(&mut attachment.redelivered) {
        if tx.send(Ok(chat_event_message(&event, true))).await.is_err() {
            return gone("client went away during redelivery");
        }
    }
    let mut position = 0;
    match send_live(&tx, std::mem::take(&mut attachment.replay), &mut position).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(()) => return gone("client went away during replay"),
    }
    position = attachment.position;

    loop {
        tokio::select! {
            next = input.recv() => {
                let message = match next {
                    Some(ClaudeChatInput::Message(message)) => message,
                    Some(ClaudeChatInput::Closed(Some(error))) => {
                        return gone(&format!("client stream failed ({error})"));
                    }
                    Some(ClaudeChatInput::Closed(None)) => {
                        return gone("client closed the stream");
                    }
                    None => return gone("command reader ended"),
                };
                let error = match message.message {
                    Some(claude_chat_client_message::Message::CommandJson(json)) => {
                        match serde_json::from_str::<HarnessCommand>(&json) {
                            Ok(command) => attachment.chat.send(command).err(),
                            Err(err) => Some(format!("the agent could not decode a command: {err}")),
                        }
                    }
                    Some(claude_chat_client_message::Message::StartJson(_))
                    | Some(claude_chat_client_message::Message::Attach(_)) => {
                        Some("the chat has already started; a second start was ignored".to_string())
                    }
                    None => Some("an empty chat message was ignored".to_string()),
                };
                if let Some(error) = error {
                    eprintln!("[agent] claude_chat {chat_id}: {error}");
                    if send_stream_notice(&tx, &HarnessEvent::Error(error)).await.is_err() {
                        return gone("client went away");
                    }
                }
            }
            _ = tx.closed() => return gone("client went away"),
            live = attachment.live.recv() => {
                let events = match live {
                    Ok(event) => vec![event],
                    // The stream fell behind the fan-out: catch up from
                    // the buffer, or end it so the client reattaches and
                    // learns of the gap.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        match attachment.chat.events_after(position) {
                            Ok(events) => events,
                            Err(Gap) => {
                                eprintln!(
                                    "[agent] claude_chat {chat_id}: a client fell {skipped} events behind, past the buffer; ending its stream"
                                );
                                let _ = tx
                                    .send(Err(Status::data_loss(
                                        "fell behind the chat's event buffer; attach again",
                                    )))
                                    .await;
                                return;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                match send_live(&tx, events, &mut position).await {
                    Ok(true) => {
                        eprintln!("[agent] claude_chat {chat_id}: claude exited; ending the stream");
                        return;
                    }
                    Ok(false) => {}
                    Err(()) => return gone("client went away"),
                }
            }
        }
    }
}

fn proto_claude_chat(info: ChatInfo) -> ClaudeChatInfo {
    let state = match info.activity {
        ChatActivity::Idle => ClaudeChatState::Idle,
        ChatActivity::TurnRunning => ClaudeChatState::TurnRunning,
        ChatActivity::AwaitingInput => ClaudeChatState::AwaitingInput,
    };
    ClaudeChatInfo {
        chat_id: info.chat_id,
        claude_session_id: info.claude_session_id.unwrap_or_default(),
        cwd: info.cwd,
        state: state as i32,
        created_unix_secs: info.created_unix_secs,
        last_activity_unix_secs: info.last_activity_unix_secs,
        attached_clients: info.attached_clients,
    }
}

fn chat_entry_to_proto(entry: crate::chats::ChatIndexEntry) -> ChatEntry {
    ChatEntry {
        id: entry.id,
        backend: entry.backend.label().to_string(),
        path: entry.path.to_string_lossy().into_owned(),
        cwd: entry.cwd.to_string_lossy().into_owned(),
        repo_root: entry
            .repo_root
            .map(|root| root.to_string_lossy().into_owned())
            .unwrap_or_default(),
        is_worktree: entry.is_worktree,
        branch: entry.branch.unwrap_or_default(),
        title: entry.title,
        mtime_unix_secs: entry
            .mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        size_bytes: entry.size,
        dead_cwd: entry.dead_cwd,
    }
}

fn proto_session(info: super::sessions::SessionInfo) -> Session {
    Session {
        session_id: info.session_id,
        workspace_id: info.workspace_id,
        kind: info.kind,
        command: info.command,
        cwd: info.cwd,
        running: info.running,
        exit_code: info.exit_code,
        created_unix_secs: info.created_unix_secs,
    }
}

fn proto_diff_line(line: super::git::FileDiffLine) -> super::protocol::v5::GitDiffLine {
    use super::git::DiffLineKind;
    use super::protocol::v5::GitDiffLineKind;
    super::protocol::v5::GitDiffLine {
        content: line.content,
        kind: match line.kind {
            DiffLineKind::Context => GitDiffLineKind::Context,
            DiffLineKind::Addition => GitDiffLineKind::Addition,
            DiffLineKind::Deletion => GitDiffLineKind::Deletion,
            DiffLineKind::Header => GitDiffLineKind::Header,
        } as i32,
        old_line: line.old_line.unwrap_or(0),
        new_line: line.new_line.unwrap_or(0),
    }
}

fn proto_file_status(status: super::git::GitFileStatus) -> super::protocol::v5::GitFileStatus {
    super::protocol::v5::GitFileStatus {
        path: status.path,
        status: status.status,
        is_staged: status.is_staged,
    }
}

/// Validate a ReadFile request: path must be an existing regular file whose
/// canonical location stays under the canonical root. Returns the canonical
/// path, the file's total size, and the effective read limit.
#[allow(clippy::result_large_err)]
fn validate_read_file_request(request: &ReadFileRequest) -> Result<(PathBuf, u64, u64), Status> {
    if request.workspace_id.trim().is_empty() {
        return Err(Status::invalid_argument("workspace_id must not be empty"));
    }
    if request.root.trim().is_empty() {
        return Err(Status::invalid_argument("root must not be empty"));
    }
    if request.path.trim().is_empty() {
        return Err(Status::invalid_argument("path must not be empty"));
    }

    let root = canonical_dir(&request.root, "root")?;
    let path = Path::new(&request.path);
    if !path.is_absolute() {
        return Err(Status::invalid_argument("path must be absolute"));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| Status::not_found(format!("could not resolve path: {err}")))?;
    if !canonical.starts_with(&root) {
        return Err(Status::permission_denied(format!(
            "path {} is outside root {}",
            canonical.display(),
            root.display()
        )));
    }
    let metadata = std::fs::metadata(&canonical).map_err(|err| {
        Status::not_found(format!("could not stat {}: {err}", canonical.display()))
    })?;
    if !metadata.is_file() {
        return Err(Status::invalid_argument(format!(
            "{} is not a regular file",
            canonical.display()
        )));
    }

    let read_limit = if request.max_bytes == 0 {
        READ_FILE_DEFAULT_MAX_BYTES
    } else {
        request.max_bytes.min(READ_FILE_DEFAULT_MAX_BYTES)
    };
    Ok((canonical, metadata.len(), read_limit))
}

#[allow(clippy::result_large_err)]
fn list_dir_response(request: ListDirRequest) -> Result<ListDirResponse, Status> {
    if request.workspace_id.trim().is_empty() {
        return Err(Status::invalid_argument("workspace_id must not be empty"));
    }
    if request.root.trim().is_empty() {
        return Err(Status::invalid_argument("root must not be empty"));
    }
    if request.current_dir.trim().is_empty() {
        return Err(Status::invalid_argument("current_dir must not be empty"));
    }

    let root = canonical_dir(&request.root, "root")?;
    let current_dir = canonical_dir(&request.current_dir, "current_dir")?;
    if !current_dir.starts_with(&root) {
        return Err(Status::permission_denied(format!(
            "current_dir {} is outside root {}",
            current_dir.display(),
            root.display()
        )));
    }

    let mut dirs: Vec<DirEntry> = Vec::new();
    let mut files: Vec<DirEntry> = Vec::new();
    let read_dir = std::fs::read_dir(&current_dir).map_err(|err| {
        Status::not_found(format!("could not read {}: {err}", current_dir.display()))
    })?;

    for entry in read_dir {
        let entry = entry.map_err(|err| {
            Status::internal(format!(
                "could not read entry in {}: {err}",
                current_dir.display()
            ))
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "node_modules" || name == "target" {
            continue;
        }
        if !request.show_hidden && name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type().map_err(|err| {
            Status::internal(format!(
                "could not inspect {}: {err}",
                entry.path().display()
            ))
        })?;
        let is_dir = file_type.is_dir();
        let path = entry.path();
        let item = DirEntry {
            name,
            // A `.git` file (worktree/submodule) counts as well as a dir.
            is_git_repo: is_dir && path.join(".git").exists(),
            path: path.to_string_lossy().to_string(),
            is_dir,
        };
        if item.is_dir {
            dirs.push(item);
        } else {
            files.push(item);
        }
    }

    dirs.sort_by_key(|entry| entry.name.to_lowercase());
    files.sort_by_key(|entry| entry.name.to_lowercase());
    dirs.extend(files);

    Ok(ListDirResponse {
        workspace_id: request.workspace_id,
        root: root.to_string_lossy().to_string(),
        current_dir: current_dir.to_string_lossy().to_string(),
        entries: dirs,
    })
}

#[allow(clippy::result_large_err)]
fn canonical_dir(path: &str, label: &str) -> Result<PathBuf, Status> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(Status::invalid_argument(format!(
            "{label} must be absolute"
        )));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| Status::not_found(format!("could not resolve {label}: {err}")))?;
    if !canonical.is_dir() {
        return Err(Status::invalid_argument(format!(
            "{label} is not a directory"
        )));
    }
    Ok(canonical)
}

#[allow(clippy::result_large_err)]
pub async fn serve(config: AgentServerConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    if config.auth_token.is_empty() {
        return Err("auth token must not be empty".into());
    }

    let expected_token = Arc::new(config.auth_token.clone());
    let mut service = GitTermAgentService::with_claude_chat_limits(
        config.agent_name.clone(),
        config.claude_chat_limits,
    );
    if let Some(phone_config) = &config.phone {
        match start_phone_server(phone_config, &config.agent_name, service.claude_chats()) {
            Ok(access) => service = service.with_phone_access(access),
            Err(err) if phone_config.required => {
                return Err(format!("phone access: {err}").into());
            }
            Err(err) => eprintln!(
                "[phone] {err}; phone access is off (pass --phone-addr HOST:PORT to choose another address)"
            ),
        }
    }
    let claude_chats = service.claude_chats.clone();
    let reaper = tokio::spawn(async move {
        let mut tick = tokio::time::interval(CLAUDE_CHAT_REAP_INTERVAL);
        loop {
            tick.tick().await;
            claude_chats.reap_idle();
        }
    });
    let authenticated_service =
        GitTermAgentServer::with_interceptor(service, move |request: Request<()>| {
            if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                Ok(request)
            } else {
                Err(Status::unauthenticated("missing or invalid bearer token"))
            }
        });

    let served = Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .add_service(authenticated_service)
        .serve(config.bind_addr)
        .await;
    reaper.abort();
    served?;

    Ok(())
}

/// Load the paired phones and start the phone web app.
fn start_phone_server(
    config: &PhoneServerConfig,
    agent_name: &str,
    claude_chats: Arc<ClaudeChatRegistry>,
) -> Result<PhoneAccess, String> {
    let phones = PhoneRegistry::load(&config.store_dir)?;
    let state = PhoneState::new(agent_name.to_string(), phones.clone(), claude_chats)?;
    let addr = super::phone_http::spawn(state, config.bind_addr)?;
    if addr.ip().is_unspecified() {
        eprintln!(
            "[phone] listening on {addr}, a wildcard address: pairing links will name it as is; bind a specific address for links a phone can open"
        );
    }
    eprintln!(
        "[phone] serving the phone app on http://{addr} ({} paired phone(s), {})",
        phones.list().len(),
        phones.path().display()
    );
    Ok(PhoneAccess {
        phones,
        base_url: format!("http://{addr}"),
    })
}

pub fn is_authorized_metadata(metadata: &MetadataMap, expected_token: &str) -> bool {
    let Some(value) = metadata.get("authorization") else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    !expected_token.is_empty() && token == expected_token
}

#[cfg(test)]
mod tests {
    use super::super::protocol::v5::git_term_agent_client::GitTermAgentClient;
    use super::*;
    use tokio_stream::wrappers::TcpListenerStream;

    #[test]
    fn auth_rejects_missing_header() {
        let metadata = MetadataMap::new();
        assert!(!is_authorized_metadata(&metadata, "secret"));
    }

    #[test]
    fn auth_rejects_wrong_scheme() {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", "Token secret".parse().unwrap());
        assert!(!is_authorized_metadata(&metadata, "secret"));
    }

    #[test]
    fn auth_rejects_wrong_token() {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(!is_authorized_metadata(&metadata, "secret"));
    }

    #[test]
    fn auth_accepts_bearer_token() {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(is_authorized_metadata(&metadata, "secret"));
    }

    #[test]
    fn grpc_service_namespace_is_isolated_from_v4() {
        type Server = GitTermAgentServer<GitTermAgentService>;
        assert_eq!(
            <Server as tonic::server::NamedService>::NAME,
            "gitterm.agent.v5.GitTermAgent"
        );
        assert_ne!(
            <Server as tonic::server::NamedService>::NAME,
            "gitterm.agent.v1.GitTermAgent"
        );
    }

    #[tokio::test]
    async fn handshake_returns_agent_capabilities() {
        let service = GitTermAgentService::new("test-agent".to_string());
        let response = service
            .handshake(Request::new(HandshakeRequest {
                client_name: "test-client".to_string(),
                client_version: "0.0.0".to_string(),
                protocol_version: PROTOCOL_VERSION,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.agent_name, "test-agent");
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        assert!(response.capabilities.contains(&"handshake".to_string()));
        assert_eq!(
            response.home_dir,
            dirs::home_dir().unwrap().to_string_lossy().to_string()
        );
    }

    #[tokio::test]
    async fn handshake_rejects_newer_client_protocol() {
        let service = GitTermAgentService::new("test-agent".to_string());
        let status = service
            .handshake(Request::new(HandshakeRequest {
                client_name: "test-client".to_string(),
                client_version: "0.0.0".to_string(),
                protocol_version: PROTOCOL_VERSION + 1,
            }))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn list_dir_returns_sorted_filtered_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("z-dir")).unwrap();
        std::fs::create_dir(root.path().join("a-dir")).unwrap();
        std::fs::create_dir(root.path().join("node_modules")).unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("b.txt"), "b").unwrap();
        std::fs::write(root.path().join("a.txt"), "a").unwrap();
        std::fs::write(root.path().join(".hidden"), "hidden").unwrap();

        let service = GitTermAgentService::new("test-agent".to_string());
        let response = service
            .list_dir(Request::new(ListDirRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                current_dir: root.path().to_string_lossy().to_string(),
                show_hidden: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let entries: Vec<(String, bool)> = response
            .entries
            .into_iter()
            .map(|entry| (entry.name, entry.is_dir))
            .collect();
        assert_eq!(
            entries,
            vec![
                ("a-dir".to_string(), true),
                ("z-dir".to_string(), true),
                ("a.txt".to_string(), false),
                ("b.txt".to_string(), false),
            ]
        );
    }

    #[tokio::test]
    async fn list_dir_marks_git_repos() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("repo").join(".git")).unwrap();
        // Linked worktrees and submodules carry a `.git` file, not a dir.
        std::fs::create_dir(root.path().join("worktree")).unwrap();
        std::fs::write(
            root.path().join("worktree").join(".git"),
            "gitdir: /elsewhere",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("plain")).unwrap();
        std::fs::write(root.path().join("file.txt"), "f").unwrap();

        let service = GitTermAgentService::new("test-agent".to_string());
        let response = service
            .list_dir(Request::new(ListDirRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                current_dir: root.path().to_string_lossy().to_string(),
                show_hidden: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let marks: Vec<(String, bool)> = response
            .entries
            .into_iter()
            .map(|entry| (entry.name, entry.is_git_repo))
            .collect();
        assert_eq!(
            marks,
            vec![
                ("plain".to_string(), false),
                ("repo".to_string(), true),
                ("worktree".to_string(), true),
                ("file.txt".to_string(), false),
            ]
        );
    }

    #[tokio::test]
    async fn list_dir_rejects_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let service = GitTermAgentService::new("test-agent".to_string());
        let status = service
            .list_dir(Request::new(ListDirRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                current_dir: outside.path().to_string_lossy().to_string(),
                show_hidden: false,
            }))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn git_status_reports_changes_for_real_repo() {
        let root = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = crate::agentd::git::git_command()
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(root.path().join("new.txt"), "n").unwrap();

        let service = GitTermAgentService::new("test-agent".to_string());
        let response = service
            .git_status(Request::new(GitStatusRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(response.is_git_repo);
        assert_eq!(response.branch_name, "main");
        assert_eq!(response.untracked.len(), 1);
        assert_eq!(response.untracked[0].path, "new.txt");
    }

    #[tokio::test]
    async fn git_status_rejects_missing_root() {
        let service = GitTermAgentService::new("test-agent".to_string());
        let status = service
            .git_status(Request::new(GitStatusRequest {
                workspace_id: "workspace".to_string(),
                root: "/definitely/not/a/real/dir".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn read_file_streams_content_within_root() {
        use tokio_stream::StreamExt;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("hello.txt"), "hello remote").unwrap();

        let service = GitTermAgentService::new("test-agent".to_string());
        let response = service
            .read_file(Request::new(ReadFileRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                path: root.path().join("hello.txt").to_string_lossy().to_string(),
                max_bytes: 0,
            }))
            .await
            .unwrap()
            .into_inner();

        let mut data = Vec::new();
        let mut stream = response;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            assert_eq!(chunk.total_size, 12);
            assert!(!chunk.truncated);
            data.extend_from_slice(&chunk.data);
        }
        assert_eq!(data, b"hello remote");
    }

    #[tokio::test]
    async fn read_file_marks_truncation_at_max_bytes() {
        use tokio_stream::StreamExt;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("big.txt"), vec![b'x'; 1000]).unwrap();

        let service = GitTermAgentService::new("test-agent".to_string());
        let mut stream = service
            .read_file(Request::new(ReadFileRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                path: root.path().join("big.txt").to_string_lossy().to_string(),
                max_bytes: 100,
            }))
            .await
            .unwrap()
            .into_inner();

        let mut data = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            assert_eq!(chunk.total_size, 1000);
            truncated = chunk.truncated;
            data.extend_from_slice(&chunk.data);
        }
        assert_eq!(data.len(), 100);
        assert!(truncated);
    }

    #[tokio::test]
    async fn read_file_rejects_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();

        let service = GitTermAgentService::new("test-agent".to_string());
        let status = service
            .read_file(Request::new(ReadFileRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                path: outside_file.to_string_lossy().to_string(),
                max_bytes: 0,
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn read_file_rejects_directories() {
        let root = tempfile::tempdir().unwrap();
        let service = GitTermAgentService::new("test-agent".to_string());
        let status = service
            .read_file(Request::new(ReadFileRequest {
                workspace_id: "workspace".to_string(),
                root: root.path().to_string_lossy().to_string(),
                path: root.path().to_string_lossy().to_string(),
                max_bytes: 0,
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    fn start_message(json: serde_json::Value) -> Option<ClaudeChatClientMessage> {
        Some(ClaudeChatClientMessage {
            message: Some(claude_chat_client_message::Message::StartJson(
                json.to_string(),
            )),
        })
    }

    fn start_json(cwd: &str) -> serde_json::Value {
        serde_json::json!({"cwd": cwd, "model": "haiku", "permission_mode": "default",
                           "effort": null, "resume": null, "chrome": false})
    }

    /// The config of a plain `start_json` opening.
    #[allow(clippy::result_large_err)]
    fn claude_chat_config(
        first: Option<ClaudeChatClientMessage>,
    ) -> Result<ClaudeSessionConfig, Status> {
        match claude_chat_opening(first)? {
            ChatOpening::Start(config) => Ok(config),
            other => panic!("not a plain start: {other:?}"),
        }
    }

    fn attach_message(
        chat_id: &str,
        after_seq: Option<u64>,
        start: &str,
    ) -> ClaudeChatClientMessage {
        ClaudeChatClientMessage {
            message: Some(claude_chat_client_message::Message::Attach(
                super::super::protocol::v5::ClaudeChatAttach {
                    chat_id: chat_id.to_string(),
                    after_seq,
                    start_json: start.to_string(),
                },
            )),
        }
    }

    #[test]
    fn claude_chat_attach_openings_decode() {
        let dir = tempfile::tempdir().unwrap();
        let start = start_json(&dir.path().to_string_lossy()).to_string();
        assert!(matches!(
            claude_chat_opening(Some(attach_message("0b6c-chat_1", Some(7), ""))).unwrap(),
            ChatOpening::Attach { chat_id, after_seq: Some(7) } if chat_id == "0b6c-chat_1"
        ));
        assert!(matches!(
            claude_chat_opening(Some(attach_message("", None, &start))).unwrap(),
            ChatOpening::StartOrAttach {
                after_seq: None,
                ..
            }
        ));
        let code = |message| claude_chat_opening(Some(message)).unwrap_err().code();
        assert_eq!(
            code(attach_message("", Some(1), "")),
            tonic::Code::InvalidArgument,
            "neither a chat id nor a start"
        );
        assert_eq!(
            code(attach_message("../chat", None, "")),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(attach_message("", None, r#"{"cwd": "/tmp"}"#)),
            tonic::Code::InvalidArgument,
            "the start is validated like a plain one"
        );
    }

    /// A chat process the test drives (see `claude_chats` for the same
    /// fake at the registry level).
    struct FakeChatProcess(tokio::sync::mpsc::UnboundedSender<HarnessCommand>);

    impl super::super::claude_chats::ChatProcess for FakeChatProcess {
        fn send(&self, command: HarnessCommand) -> Result<(), String> {
            self.0.send(command).map_err(|_| "fake gone".to_string())
        }
    }

    type FakeChat = (
        tokio::sync::mpsc::UnboundedSender<HarnessEvent>,
        tokio::sync::mpsc::UnboundedReceiver<HarnessCommand>,
    );

    /// A served agent whose Claude chats are fakes, and a client for it.
    async fn fake_chat_agent() -> (
        GitTermAgentClient<tonic::transport::Channel>,
        Arc<ClaudeChatRegistry>,
        Arc<std::sync::Mutex<Vec<FakeChat>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let spawned: Arc<std::sync::Mutex<Vec<FakeChat>>> = Arc::default();
        let registry = {
            let spawned = spawned.clone();
            ClaudeChatRegistry::with_parts(
                ChatLimits::default(),
                Arc::new(move |_config| {
                    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
                    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
                    spawned.lock().unwrap().push((event_tx, command_rx));
                    (
                        Box::new(FakeChatProcess(command_tx))
                            as Box<dyn super::super::claude_chats::ChatProcess>,
                        event_rx,
                    )
                }),
                Arc::new(std::time::SystemTime::now),
            )
        };
        let service = GitTermAgentService {
            agent_name: "test-agent".to_string(),
            sessions: Arc::new(SessionManager::default()),
            claude_chats: registry.clone(),
            phone: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(GitTermAgentServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        (GitTermAgentClient::new(channel), registry, spawned, server)
    }

    async fn open_chat(
        client: &mut GitTermAgentClient<tonic::transport::Channel>,
        first: ClaudeChatClientMessage,
    ) -> Result<
        (
            tokio::sync::mpsc::UnboundedSender<ClaudeChatClientMessage>,
            tonic::Streaming<ClaudeChatServerMessage>,
        ),
        Status,
    > {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(first).unwrap();
        let stream = client
            .claude_chat(Request::new(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
            .await?
            .into_inner();
        Ok((tx, stream))
    }

    async fn next_message(
        stream: &mut tonic::Streaming<ClaudeChatServerMessage>,
    ) -> ClaudeChatServerMessage {
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
            .await
            .expect("timed out waiting for a chat message")
            .expect("stream failed")
            .expect("stream ended")
    }

    async fn wait_for_attached(registry: &ClaudeChatRegistry, attached: u32) {
        for _ in 0..400 {
            if registry.list().first().map(|chat| chat.attached_clients) == Some(attached) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "attached clients never reached {attached}: {:?}",
            registry.list()
        );
    }

    fn event_of(message: &ClaudeChatServerMessage) -> HarnessEvent {
        serde_json::from_str(&message.event_json).unwrap()
    }

    #[tokio::test]
    async fn a_plain_start_from_an_older_client_gets_bare_events_and_only_detaches() {
        let (mut client, registry, spawned, server) = fake_chat_agent().await;
        let dir = tempfile::tempdir().unwrap();
        let start = start_message(start_json(&dir.path().to_string_lossy())).unwrap();
        let (commands, mut stream) = open_chat(&mut client, start).await.unwrap();
        // The process starts before the stream is answered.
        let events = spawned.lock().unwrap()[0].0.clone();
        events.send(HarnessEvent::TextDelta("hi".into())).unwrap();
        // No `opened`: the first message is the event, as before.
        let first = next_message(&mut stream).await;
        assert!(first.opened.is_none());
        assert_eq!(event_of(&first), HarnessEvent::TextDelta("hi".into()));
        // Closing the stream no longer stops the process.
        drop(commands);
        drop(stream);
        wait_for_attached(&registry, 0).await;
        assert!(
            spawned.lock().unwrap()[0].1.try_recv().is_err(),
            "no shutdown"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_dropped_stream_detaches_and_a_reattach_gets_the_open_request() {
        let (mut client, registry, spawned, server) = fake_chat_agent().await;
        let dir = tempfile::tempdir().unwrap();
        let start = start_json(&dir.path().to_string_lossy()).to_string();

        let (commands, mut stream) = open_chat(&mut client, attach_message("", None, &start))
            .await
            .unwrap();
        let opened = next_message(&mut stream)
            .await
            .opened
            .expect("opened first");
        assert!(!opened.attached && !opened.gap);
        assert_eq!(opened.head_seq, 0);
        let events = spawned.lock().unwrap()[0].0.clone();
        events
            .send(HarnessEvent::TurnStarted {
                session_id: Some("s1".into()),
                model: None,
            })
            .unwrap();
        events
            .send(HarnessEvent::RuntimeRequest {
                request_id: "r1".into(),
                tool_use_id: "t1".into(),
                kind: crate::harness::RuntimeRequestKind::Question {
                    questions: serde_json::json!([]),
                },
            })
            .unwrap();
        let first = next_message(&mut stream).await;
        let second = next_message(&mut stream).await;
        assert_eq!((first.seq, second.seq), (1, 2));
        assert!(matches!(
            event_of(&second),
            HarnessEvent::RuntimeRequest { .. }
        ));

        // The client goes away without answering: only a detach.
        drop(commands);
        drop(stream);
        wait_for_attached(&registry, 0).await;
        assert!(
            spawned.lock().unwrap()[0].1.try_recv().is_err(),
            "no shutdown"
        );
        let listed = client
            .list_claude_chats(Request::new(ListClaudeChatsRequest {}))
            .await
            .unwrap()
            .into_inner()
            .chats;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].chat_id, opened.chat_id);
        assert_eq!(listed[0].claude_session_id, "s1");
        assert_eq!(listed[0].state, ClaudeChatState::AwaitingInput as i32);

        // Back after seq 2: the open request is redelivered.
        let (commands, mut stream) =
            open_chat(&mut client, attach_message(&opened.chat_id, Some(2), ""))
                .await
                .unwrap();
        let reopened = next_message(&mut stream)
            .await
            .opened
            .expect("opened first");
        assert!(reopened.attached && !reopened.gap);
        assert_eq!(reopened.head_seq, 2);
        let turn = next_message(&mut stream).await;
        let request = next_message(&mut stream).await;
        assert!(turn.redelivered && request.redelivered);
        assert!(matches!(
            event_of(&request),
            HarnessEvent::RuntimeRequest { request_id, .. } if request_id == "r1"
        ));

        // An explicit Shutdown stops it; the exit ends the stream.
        commands
            .send(ClaudeChatClientMessage {
                message: Some(claude_chat_client_message::Message::CommandJson(
                    serde_json::to_string(&HarnessCommand::Shutdown).unwrap(),
                )),
            })
            .unwrap();
        for _ in 0..400 {
            if let Ok(command) = spawned.lock().unwrap()[0].1.try_recv() {
                assert_eq!(command, HarnessCommand::Shutdown);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        events
            .send(HarnessEvent::ProcessExited { code: Some(0) })
            .unwrap();
        let exit = next_message(&mut stream).await;
        assert_eq!(exit.seq, 3);
        assert!(matches!(
            event_of(&exit),
            HarnessEvent::ProcessExited { .. }
        ));
        assert!(stream.message().await.unwrap().is_none(), "stream ends");
        for _ in 0..400 {
            if registry.list().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(registry.list().is_empty());
        match open_chat(&mut client, attach_message(&opened.chat_id, Some(3), "")).await {
            Ok(_) => panic!("attach to a stopped chat succeeded"),
            Err(status) => assert_eq!(status.code(), tonic::Code::NotFound),
        }
        server.abort();
    }

    #[test]
    fn claude_chat_start_must_come_first_and_decode() {
        let code = |first| claude_chat_config(first).unwrap_err().code();
        assert_eq!(code(None), tonic::Code::InvalidArgument);
        assert_eq!(
            code(Some(ClaudeChatClientMessage {
                message: Some(claude_chat_client_message::Message::CommandJson(
                    r#"{"type":"interrupt"}"#.into()
                )),
            })),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(Some(ClaudeChatClientMessage { message: None })),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(start_message(serde_json::json!({"cwd": "/tmp"}))),
            tonic::Code::InvalidArgument,
            "a start missing fields does not decode"
        );
    }

    #[test]
    fn claude_chat_start_validates_its_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let code = |json| claude_chat_config(start_message(json)).unwrap_err().code();

        let mut empty_mode = start_json(&cwd);
        empty_mode["permission_mode"] = " ".into();
        assert_eq!(code(empty_mode), tonic::Code::InvalidArgument);

        let mut traversal = start_json(&cwd);
        traversal["resume"] = "../../etc/passwd".into();
        assert_eq!(code(traversal), tonic::Code::InvalidArgument);

        assert_eq!(
            code(start_json("relative/dir")),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(start_json("/definitely/not/a/real/dir")),
            tonic::Code::NotFound
        );
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            code(start_json(&file.to_string_lossy())),
            tonic::Code::InvalidArgument
        );

        let mut ok = start_json(&cwd);
        ok["resume"] = "67838915-705b-4a13-af63-4d08a1d7da10".into();
        ok["chrome"] = true.into();
        let config = claude_chat_config(start_message(ok)).unwrap();
        assert_eq!(config.cwd, dir.path().canonicalize().unwrap());
        assert_eq!(config.model.as_deref(), Some("haiku"));
        assert_eq!(
            config.resume.as_deref(),
            Some("67838915-705b-4a13-af63-4d08a1d7da10")
        );
        assert!(config.chrome);
        assert!(config.mcp_servers.is_empty());
        assert!(config.wire_log_dir.is_none());
    }

    #[test]
    fn claude_session_ids_are_plain_names() {
        assert!(validate_claude_session_id("67838915-705b-4a13-af63-4d08a1d7da10").is_ok());
        assert!(validate_claude_session_id("agent_a1").is_ok());
        for bad in ["", "../x", "a/b", "a b", "a.jsonl", "a\\b"] {
            assert!(validate_claude_session_id(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn handshake_advertises_claude_chat() {
        let service = GitTermAgentService::new("test-agent".to_string());
        let response = service
            .handshake(Request::new(HandshakeRequest {
                client_name: "test-client".to_string(),
                client_version: "0.0.0".to_string(),
                protocol_version: PROTOCOL_VERSION,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(response.capabilities.contains(&"claude_chat".to_string()));
        assert!(response
            .capabilities
            .contains(&"claude_chat_persistent".to_string()));
        assert_eq!(response.protocol_version, 1);
    }

    async fn capabilities_of(service: &GitTermAgentService) -> Vec<String> {
        service
            .handshake(Request::new(HandshakeRequest {
                client_name: "test-client".to_string(),
                client_version: "0.0.0".to_string(),
                protocol_version: PROTOCOL_VERSION,
            }))
            .await
            .unwrap()
            .into_inner()
            .capabilities
    }

    #[tokio::test]
    async fn phone_rpcs_are_off_without_the_phone_app() {
        let service = GitTermAgentService::new("test-agent".to_string());
        assert!(!capabilities_of(&service)
            .await
            .contains(&PHONE_CAPABILITY.to_string()));
        let status = service
            .create_phone_pairing(Request::new(CreatePhonePairingRequest {
                device_name: "phone".into(),
                scope: "view".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let status = service
            .list_phones(Request::new(ListPhonesRequest {}))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn phone_rpcs_pair_list_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let phones = PhoneRegistry::load(dir.path()).unwrap();
        let service =
            GitTermAgentService::new("test-agent".to_string()).with_phone_access(PhoneAccess {
                phones: phones.clone(),
                base_url: "http://100.64.0.1:7778".into(),
            });
        assert!(capabilities_of(&service)
            .await
            .contains(&PHONE_CAPABILITY.to_string()));

        let status = service
            .create_phone_pairing(Request::new(CreatePhonePairingRequest {
                device_name: "phone".into(),
                scope: "admin".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);

        // Asking for act still pairs a viewer in stage 1.
        let pairing = service
            .create_phone_pairing(Request::new(CreatePhonePairingRequest {
                device_name: "Tracey's iPhone".into(),
                scope: "act".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(pairing.scope, "view");
        assert_eq!(
            pairing.pairing_url,
            format!("http://100.64.0.1:7778/pair#{}", pairing.code)
        );
        let paired = phones.redeem(&pairing.code).unwrap();

        let listed = service
            .list_phones(Request::new(ListPhonesRequest {}))
            .await
            .unwrap()
            .into_inner()
            .phones;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Tracey's iPhone");
        assert_eq!(listed[0].scope, "view");

        let revoked = service
            .revoke_phone(Request::new(RevokePhoneRequest {
                id: listed[0].id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(revoked.revoked);
        assert!(phones.authenticate(&paired.token).is_none());
        let again = service
            .revoke_phone(Request::new(RevokePhoneRequest {
                id: listed[0].id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!again.revoked);
    }

    #[tokio::test]
    async fn claude_chat_history_rejects_bad_ids_and_misses() {
        let service = GitTermAgentService::new("test-agent".to_string());
        let status = service
            .claude_chat_history(Request::new(ClaudeChatHistoryRequest {
                session_id: "../../secrets".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        let status = service
            .claude_chat_history(Request::new(ClaudeChatHistoryRequest {
                session_id: uuid::Uuid::new_v4().to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn claude_chat_rpcs_require_the_bearer_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_token = Arc::new("secret".to_string());
        let authenticated_service = GitTermAgentServer::with_interceptor(
            GitTermAgentService::new("test-agent".to_string()),
            move |request: Request<()>| {
                if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                    Ok(request)
                } else {
                    Err(Status::unauthenticated("missing or invalid bearer token"))
                }
            },
        );
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(authenticated_service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = GitTermAgentClient::new(channel);

        let status = client
            .claude_chat_history(Request::new(ClaudeChatHistoryRequest {
                session_id: "abc".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let start = ClaudeChatClientMessage {
            message: Some(claude_chat_client_message::Message::StartJson(
                start_json("/tmp").to_string(),
            )),
        };
        let status = client
            .claude_chat(Request::new(tokio_stream::once(start)))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        // Authorized, a bad start is refused before any process starts.
        let bad = ClaudeChatClientMessage {
            message: Some(claude_chat_client_message::Message::CommandJson(
                r#"{"type":"interrupt"}"#.to_string(),
            )),
        };
        let mut request = Request::new(tokio_stream::once(bad));
        request
            .metadata_mut()
            .insert("authorization", "Bearer secret".parse().unwrap());
        let status = client.claude_chat(request).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);

        server.abort();
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn grpc_client_can_call_authenticated_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let expected_token = Arc::new("secret".to_string());
        let service = GitTermAgentService::new("test-agent".to_string());
        let authenticated_service =
            GitTermAgentServer::with_interceptor(service, move |request: Request<()>| {
                if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                    Ok(request)
                } else {
                    Err(Status::unauthenticated("missing or invalid bearer token"))
                }
            });

        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(authenticated_service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(HandshakeRequest {
            client_name: "test-client".to_string(),
            client_version: "0.0.0".to_string(),
            protocol_version: PROTOCOL_VERSION,
        });
        request
            .metadata_mut()
            .insert("authorization", "Bearer secret".parse().unwrap());

        let response = client.handshake(request).await.unwrap().into_inner();
        assert_eq!(response.agent_name, "test-agent");
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);

        server.abort();
    }
}
