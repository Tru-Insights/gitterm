use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

use super::protocol::v5::git_term_agent_client::GitTermAgentClient;
use super::protocol::v5::GitDiffRequest;
use super::protocol::v5::GitStatusRequest;
use super::protocol::v5::HandshakeRequest;
use super::protocol::v5::ListDirRequest;
use super::protocol::v5::ReadFileRequest;
use super::protocol::v5::{
    claude_chat_client_message, terminal_input, AttachRequest, ChatEntry, ChatPreviewRequest,
    ClaudeChatAttach, ClaudeChatClientMessage, ClaudeChatHistoryRequest, ClaudeChatState,
    CreatePhonePairingRequest, EnvVar, ListChatsRequest, ListClaudeChatsRequest, ListPhonesRequest,
    ListSessionsRequest, Resize, RevokePhoneRequest, StartSessionRequest, StopSessionRequest,
    TerminalInput,
};
use super::server::PROTOCOL_VERSION;

/// The agent's side of a remote Claude chat: one harness event as JSON
/// per item, in order (TRU-152).
pub type ClaudeChatEventStream = std::pin::Pin<
    Box<dyn tokio_stream::Stream<Item = Result<String, RemoteAgentClientError>> + Send>,
>;

/// The agent's side of an attached Claude chat ("claude_chat_persistent"):
/// `opened` first, then events with their seq.
pub type ClaudeChatFrameStream = std::pin::Pin<
    Box<dyn tokio_stream::Stream<Item = Result<ClaudeChatFrame, RemoteAgentClientError>> + Send>,
>;

/// How an attached chat stream opens: a chat by id after a seq, or a start
/// (which the agent turns into an attach when its resume id is running).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeChatOpen {
    Chat {
        chat_id: String,
        after_seq: Option<u64>,
    },
    Start {
        start_json: String,
    },
}

/// The agent's answer to an attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeChatOpened {
    pub chat_id: String,
    /// Joined a chat that was already running.
    pub attached: bool,
    /// Empty until Claude reports it.
    pub claude_session_id: String,
    pub head_seq: u64,
    /// The events after the requested seq are gone: reload the history.
    pub gap: bool,
}

/// One message of an attached chat stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeChatFrame {
    Opened(ClaudeChatOpened),
    /// A harness event as JSON. `seq` 0 is a notice for this stream only;
    /// `redelivered` repeats an event the stream's position covers.
    Event {
        json: String,
        seq: u64,
        redelivered: bool,
    },
}

/// What a running chat is doing, from `ListClaudeChats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningClaudeChatState {
    Idle,
    TurnRunning,
    AwaitingInput,
}

/// A Claude chat the agent is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningClaudeChat {
    pub chat_id: String,
    /// Empty until Claude reports it.
    pub claude_session_id: String,
    pub cwd: String,
    pub state: RunningClaudeChatState,
    pub created_unix_secs: u64,
    pub last_activity_unix_secs: u64,
    pub attached_clients: u32,
}

/// A one-time phone pairing from `CreatePhonePairing` (TRU-151).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhonePairing {
    /// Open on the phone; the code is in its fragment.
    pub pairing_url: String,
    pub expires_unix_secs: u64,
    /// The scope the phone gets ("view" in stage 1).
    pub scope: String,
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
    /// Agent user's $HOME; empty from agents that predate the field.
    pub home_dir: String,
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
    /// False from agents that predate the field.
    pub is_git_repo: bool,
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
            client_name: "GitTerm V5 desktop".to_string(),
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
            home_dir: response.home_dir,
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
                    is_git_repo: entry.is_git_repo,
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

        let map = |file: super::protocol::v5::GitFileStatus| RemoteAgentGitFile {
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

        use super::protocol::v5::GitDiffLineKind;
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

    /// The agent machine's chat index (TRU-78): entries in the same
    /// shape the desktop builds locally, with paths that are only
    /// meaningful on the agent's machine.
    pub async fn list_chats(
        &self,
    ) -> Result<Vec<crate::chats::ChatIndexEntry>, RemoteAgentClientError> {
        let mut request = Request::new(ListChatsRequest {});
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .list_chats(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("list_chats failed: {err:?}")))?
            .into_inner();
        Ok(response
            .chats
            .into_iter()
            .filter_map(chat_entry_from_proto)
            .collect())
    }

    pub async fn chat_preview(
        &self,
        path: String,
        backend: crate::chats::ChatBackend,
    ) -> Result<crate::chats::ChatPreview, RemoteAgentClientError> {
        let mut request = Request::new(ChatPreviewRequest {
            path,
            backend: backend.label().to_string(),
        });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .chat_preview(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("chat_preview failed: {err:?}")))?
            .into_inner();
        Ok(crate::chats::ChatPreview {
            messages: response
                .messages
                .into_iter()
                .map(|message| crate::chats::ChatPreviewMessage {
                    is_user: message.is_user,
                    text: message.text,
                })
                .collect(),
            message_count: response.message_count,
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
        let channel = connect_long_lived_channel(&self.config.endpoint).await?;

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

        use super::protocol::v5::terminal_output;
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

    /// Open a native Claude chat on the agent (TRU-152): `start_json` is
    /// the first message, each string from `commands_json` one command
    /// after it. Returns the agent's events as JSON, in order. Closing
    /// `commands_json` ends the stream; an agent without
    /// "claude_chat_persistent" then stops its process, one with it only
    /// detaches. The connection has no request timeout; it lives as long
    /// as the chat.
    pub async fn claude_chat(
        &self,
        start_json: String,
        commands_json: tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatEventStream, RemoteAgentClientError> {
        use tokio_stream::StreamExt;

        let channel = connect_long_lived_channel(&self.config.endpoint).await?;
        let start = ClaudeChatClientMessage {
            message: Some(claude_chat_client_message::Message::StartJson(start_json)),
        };
        let outbound = tokio_stream::once(start).chain(
            tokio_stream::wrappers::UnboundedReceiverStream::new(commands_json).map(|json| {
                ClaudeChatClientMessage {
                    message: Some(claude_chat_client_message::Message::CommandJson(json)),
                }
            }),
        );
        let mut request = Request::new(outbound);
        self.authorize(&mut request)?;
        let stream = GitTermAgentClient::new(channel)
            .claude_chat(request)
            .await
            .map_err(|err| RemoteAgentClientError::new(format!("claude_chat failed: {err:?}")))?
            .into_inner();
        Ok(Box::pin(stream.map(|item| {
            item.map(|message| message.event_json)
                .map_err(|err| RemoteAgentClientError::new(format!("claude_chat stream: {err:?}")))
        })))
    }

    /// Attach to a Claude chat the agent owns ("claude_chat_persistent"),
    /// or start one: `open` is the first message, each string from
    /// `commands_json` one command after it. The stream yields `Opened`
    /// first, then events with their seq. Closing `commands_json` (or
    /// dropping the stream) detaches; the chat keeps running. A chat id
    /// that is no longer running fails with `is_not_found()`.
    pub async fn claude_chat_attach(
        &self,
        open: ClaudeChatOpen,
        commands_json: tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> Result<ClaudeChatFrameStream, RemoteAgentClientError> {
        use tokio_stream::StreamExt;

        let channel = connect_long_lived_channel(&self.config.endpoint).await?;
        let attach = match open {
            ClaudeChatOpen::Chat { chat_id, after_seq } => ClaudeChatAttach {
                chat_id,
                after_seq,
                start_json: String::new(),
            },
            ClaudeChatOpen::Start { start_json } => ClaudeChatAttach {
                chat_id: String::new(),
                after_seq: None,
                start_json,
            },
        };
        let first = ClaudeChatClientMessage {
            message: Some(claude_chat_client_message::Message::Attach(attach)),
        };
        let outbound = tokio_stream::once(first).chain(
            tokio_stream::wrappers::UnboundedReceiverStream::new(commands_json).map(|json| {
                ClaudeChatClientMessage {
                    message: Some(claude_chat_client_message::Message::CommandJson(json)),
                }
            }),
        );
        let mut request = Request::new(outbound);
        self.authorize(&mut request)?;
        let stream = match GitTermAgentClient::new(channel).claude_chat(request).await {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Err(RemoteAgentClientError::not_found(format!(
                    "claude_chat attach: {}",
                    status.message()
                )))
            }
            Err(err) => {
                return Err(RemoteAgentClientError::new(format!(
                    "claude_chat attach failed: {err:?}"
                )))
            }
        };
        Ok(Box::pin(stream.map(|item| {
            item.map(|message| match message.opened {
                Some(opened) => ClaudeChatFrame::Opened(ClaudeChatOpened {
                    chat_id: opened.chat_id,
                    attached: opened.attached,
                    claude_session_id: opened.claude_session_id,
                    head_seq: opened.head_seq,
                    gap: opened.gap,
                }),
                None => ClaudeChatFrame::Event {
                    json: message.event_json,
                    seq: message.seq,
                    redelivered: message.redelivered,
                },
            })
            .map_err(|err| RemoteAgentClientError::new(format!("claude_chat stream: {err:?}")))
        })))
    }

    /// The Claude chats the agent is running ("claude_chat_persistent").
    pub async fn list_claude_chats(
        &self,
    ) -> Result<Vec<RunningClaudeChat>, RemoteAgentClientError> {
        let mut request = Request::new(ListClaudeChatsRequest {});
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .list_claude_chats(request)
            .await
            .map_err(|err| {
                RemoteAgentClientError::new(format!("list_claude_chats failed: {err:?}"))
            })?
            .into_inner();
        response
            .chats
            .into_iter()
            .map(|chat| {
                let state = match ClaudeChatState::try_from(chat.state) {
                    Ok(ClaudeChatState::Idle) => RunningClaudeChatState::Idle,
                    Ok(ClaudeChatState::TurnRunning) => RunningClaudeChatState::TurnRunning,
                    Ok(ClaudeChatState::AwaitingInput) => RunningClaudeChatState::AwaitingInput,
                    Ok(ClaudeChatState::Unspecified) | Err(_) => {
                        return Err(RemoteAgentClientError::new(format!(
                            "list_claude_chats: chat {} has unknown state {}",
                            chat.chat_id, chat.state
                        )))
                    }
                };
                Ok(RunningClaudeChat {
                    chat_id: chat.chat_id,
                    claude_session_id: chat.claude_session_id,
                    cwd: chat.cwd,
                    state,
                    created_unix_secs: chat.created_unix_secs,
                    last_activity_unix_secs: chat.last_activity_unix_secs,
                    attached_clients: chat.attached_clients,
                })
            })
            .collect()
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
        let channel = connect_channel(&self.config.endpoint).await?;
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
        // The code itself is only ever used inside the URL.
        Ok(PhonePairing {
            pairing_url: response.pairing_url,
            expires_unix_secs: response.expires_unix_secs,
            scope: response.scope,
        })
    }

    /// The phones paired with the agent (capability "phone").
    pub async fn list_phones(&self) -> Result<Vec<PairedPhoneInfo>, RemoteAgentClientError> {
        let mut request = Request::new(ListPhonesRequest {});
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
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
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = GitTermAgentClient::new(channel)
            .revoke_phone(request)
            .await
            .map_err(|err| {
                RemoteAgentClientError::new(format!("revoke_phone failed: {}", err.message()))
            })?
            .into_inner();
        Ok(response.revoked)
    }

    /// A Claude session's timeline from its transcript on the agent's
    /// machine. `None` when the agent has no transcript for the id.
    pub async fn claude_chat_history(
        &self,
        session_id: String,
    ) -> Result<Option<Vec<crate::harness::transcript::TimedEntry>>, RemoteAgentClientError> {
        let mut request = Request::new(ClaudeChatHistoryRequest {
            session_id: session_id.clone(),
        });
        self.authorize(&mut request)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let response = match GitTermAgentClient::new(channel)
            .claude_chat_history(request)
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(err) => {
                return Err(RemoteAgentClientError::new(format!(
                    "claude_chat_history failed: {err:?}"
                )))
            }
        };
        serde_json::from_str(&response.timeline_json)
            .map(Some)
            .map_err(|err| {
                RemoteAgentClientError::new(format!(
                    "claude_chat_history for {session_id}: undecodable timeline: {err}"
                ))
            })
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

/// Decode a wire chat entry. Entries with an unknown backend label
/// (from a newer agent) are skipped rather than failing the whole list.
fn chat_entry_from_proto(entry: ChatEntry) -> Option<crate::chats::ChatIndexEntry> {
    let backend = crate::chats::ChatBackend::from_label(&entry.backend)?;
    Some(crate::chats::ChatIndexEntry {
        id: entry.id,
        backend,
        path: PathBuf::from(entry.path),
        cwd: PathBuf::from(entry.cwd),
        repo_root: (!entry.repo_root.is_empty()).then(|| PathBuf::from(entry.repo_root)),
        is_worktree: entry.is_worktree,
        branch: (!entry.branch.is_empty()).then_some(entry.branch),
        title: entry.title,
        mtime: std::time::UNIX_EPOCH + Duration::from_secs(entry.mtime_unix_secs.max(0) as u64),
        size: entry.size_bytes,
        dead_cwd: entry.dead_cwd,
    })
}

fn session_from_proto(session: super::protocol::v5::Session) -> RemoteAgentSession {
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

/// A channel for a stream that lives as long as its session or chat: a
/// connect timeout but no per-request timeout. HTTP/2 pings, even while
/// idle, notice a connection that died silently (the Mac slept, the
/// network changed) so a chat can reconnect.
async fn connect_long_lived_channel(endpoint: &str) -> Result<Channel, RemoteAgentClientError> {
    let builder = Endpoint::from_shared(endpoint.to_string())
        .map_err(|err| RemoteAgentClientError::new(format!("invalid endpoint {endpoint}: {err}")))?
        .connect_timeout(Duration::from_secs(5))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(20))
        .keep_alive_while_idle(true);
    let builder = if endpoint.starts_with("https://") {
        builder
            .tls_config(ClientTlsConfig::new().with_enabled_roots())
            .map_err(|err| RemoteAgentClientError::new(format!("invalid TLS config: {err}")))?
    } else {
        builder
    };
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
    not_found: bool,
}

impl RemoteAgentClientError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            not_found: false,
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            not_found: true,
        }
    }

    /// The agent answered, and what was asked for does not exist there
    /// (as opposed to a transport or auth failure).
    pub fn is_not_found(&self) -> bool {
        self.not_found
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

    use super::super::protocol::v5::git_term_agent_server::GitTermAgentServer;
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

    #[tokio::test]
    async fn chats_round_trip_over_grpc() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_token = Arc::new("chats-secret".to_string());
        let service = GitTermAgentServer::with_interceptor(
            GitTermAgentService::new("chats-agent".to_string()),
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

        std::env::set_var("GITTERM_AGENT_CHATS_TEST_TOKEN", "chats-secret");
        let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
            remote_id: "chats".to_string(),
            name: "chats".to_string(),
            endpoint: format!("http://{addr}"),
            token_ref: "env:GITTERM_AGENT_CHATS_TEST_TOKEN".to_string(),
        });

        // Handshake advertises the new capability.
        let handshake = backend.handshake().await.unwrap();
        assert!(handshake.capabilities.contains(&"chats".to_string()));

        // The index round-trips (contents depend on the machine; the
        // decode path and RPC plumbing are what's under test).
        let entries = backend.list_chats().await.unwrap();

        // Preview is path-restricted to the agent home; /etc must fail.
        let denied = backend
            .chat_preview("/etc/hosts".to_string(), crate::chats::ChatBackend::Claude)
            .await;
        assert!(denied.is_err(), "preview outside home must be rejected");

        // Preview a real transcript through the wire when one exists.
        if let Some(entry) = entries.first() {
            let preview = backend
                .chat_preview(entry.path.to_string_lossy().into_owned(), entry.backend)
                .await
                .unwrap();
            assert!(!preview.messages.is_empty() || preview.message_count.is_none());
        }

        server.abort();
    }
}
