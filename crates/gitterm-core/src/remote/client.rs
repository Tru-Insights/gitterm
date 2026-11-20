//! Neutral V6 remote transport. Credential lookup and host version are supplied by the host.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

use super::protocol::v6::git_term_agent_client::GitTermAgentClient;
use super::protocol::v6::GitDiffRequest;
use super::protocol::v6::GitStatusRequest;
use super::protocol::v6::HandshakeRequest;
use super::protocol::v6::ListDirRequest;
use super::protocol::v6::ReadFileRequest;
use super::protocol::v6::{
    claude_chat_client_message, terminal_input, AttachRequest, ChatEntry, ChatPreviewRequest,
    ClaudeChatAttach, ClaudeChatClientMessage, ClaudeChatHistoryRequest, ClaudeChatState, EnvVar,
    ListChatsRequest, ListClaudeChatsRequest, ListSessionsRequest, Resize, StartSessionRequest,
    StopSessionRequest, TerminalInput,
};
/// V6 wire protocol version, pinned against the host server in fixture tests.
pub const PROTOCOL_VERSION: u32 = 1;

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

#[derive(Debug, Clone)]
pub struct RemoteAgentClientConfig {
    pub remote_id: String,
    pub name: String,
    pub endpoint: String,
    pub token_ref: String,
}

pub use super::state::RemoteAgentHandshake;

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
    pub lines: Vec<crate::git_diff::FileDiffLine>,
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
    resolve_token_ref: fn(&str) -> Result<String, RemoteAgentClientError>,
    client_version: &'static str,
}

impl RemoteAgentBackend {
    pub fn new(
        config: RemoteAgentClientConfig,
        resolve_token_ref: fn(&str) -> Result<String, RemoteAgentClientError>,
        client_version: &'static str,
    ) -> Self {
        Self {
            config,
            resolve_token_ref,
            client_version,
        }
    }

    pub fn config(&self) -> &RemoteAgentClientConfig {
        &self.config
    }

    pub async fn handshake(&self) -> Result<RemoteAgentHandshake, RemoteAgentClientError> {
        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
        let channel = connect_channel(&self.config.endpoint).await?;
        let mut client = GitTermAgentClient::new(channel);
        let mut request = Request::new(HandshakeRequest {
            client_name: "GitTerm V6 desktop".to_string(),
            client_version: self.client_version.to_string(),
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
            .map_err(|err| rpc_failed("handshake", err))?
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
        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
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
            .map_err(|err| rpc_failed("list_dir", err))?
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
        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
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
            .map_err(|err| rpc_failed("read_file", err))?
            .into_inner();

        let mut data = Vec::new();
        let mut total_size = 0u64;
        let mut truncated = false;
        while let Some(chunk) = stream
            .message()
            .await
            .map_err(|err| rpc_failed("read_file stream", err))?
        {
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
        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
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
            .map_err(|err| rpc_failed("git_status", err))?
            .into_inner();

        let map = |file: super::protocol::v6::GitFileStatus| RemoteAgentGitFile {
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
        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
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
            .map_err(|err| rpc_failed("git_diff", err))?
            .into_inner();

        use super::protocol::v6::GitDiffLineKind;
        let lines = response
            .lines
            .into_iter()
            .map(|line| crate::git_diff::FileDiffLine {
                content: line.content,
                kind: match GitDiffLineKind::try_from(line.kind) {
                    Ok(GitDiffLineKind::Addition) => crate::git_diff::DiffLineKind::Addition,
                    Ok(GitDiffLineKind::Deletion) => crate::git_diff::DiffLineKind::Deletion,
                    Ok(GitDiffLineKind::Header) => crate::git_diff::DiffLineKind::Header,
                    _ => crate::git_diff::DiffLineKind::Context,
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
            .map_err(|err| rpc_failed("list_chats", err))?
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
            .map_err(|err| rpc_failed("chat_preview", err))?
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
            .map_err(|err| rpc_failed("list_sessions", err))?
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
            .map_err(|err| rpc_failed("start_session", err))?
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
            .map_err(|err| rpc_failed("stop_session", err))?
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

        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
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
            .map_err(|err| rpc_failed("attach_terminal", err))?
            .into_inner();

        use super::protocol::v6::terminal_output;
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
            .map_err(|err| rpc_failed("claude_chat", err))?
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
            Err(err) => return Err(rpc_failed("claude_chat attach", err)),
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
            .map_err(|err| rpc_failed("list_claude_chats", err))?
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
            Err(err) => return Err(rpc_failed("claude_chat_history", err)),
        };
        serde_json::from_str(&response.timeline_json)
            .map(Some)
            .map_err(|err| {
                RemoteAgentClientError::new(format!(
                    "claude_chat_history for {session_id}: undecodable timeline: {err}"
                ))
            })
    }

    pub fn authorize<T>(&self, request: &mut Request<T>) -> Result<(), RemoteAgentClientError> {
        let token = (self.resolve_token_ref)(&self.config.token_ref)?;
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
        // The wire proto has no latest-topic field yet; remote rows stay
        // single-line (TRU-158 is local-index only).
        latest: None,
        mtime: std::time::UNIX_EPOCH + Duration::from_secs(entry.mtime_unix_secs.max(0) as u64),
        size: entry.size_bytes,
        dead_cwd: entry.dead_cwd,
        // Facts files live on each machine's own config dir and are not on
        // the wire yet; remote rows show no keys, repos or fan-out.
        facts: None,
        root_id: None,
    })
}

fn session_from_proto(session: super::protocol::v6::Session) -> RemoteAgentSession {
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

pub async fn connect_channel(endpoint: &str) -> Result<Channel, RemoteAgentClientError> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentClientError {
    message: String,
    not_found: bool,
}

/// What an older agent answers a V6 desktop with: its gRPC router has no
/// `gitterm.agent.v6.GitTermAgent` service, so every call comes back
/// `Unimplemented`. Said plainly so the user knows to upgrade the agent.
pub const OLDER_AGENT_MESSAGE: &str =
    "remote agent is V5 (or older); upgrade it to V6 (gitterm-v6-agent)";

/// Map a failed RPC to a client error. `Unimplemented` from a V6 agent
/// cannot happen (it serves every method of this proto), so it means the
/// agent speaks an older protocol package.
pub fn rpc_failed(operation: &str, status: tonic::Status) -> RemoteAgentClientError {
    if status.code() == tonic::Code::Unimplemented {
        return RemoteAgentClientError::new(format!(
            "{OLDER_AGENT_MESSAGE} ({operation} is unimplemented there: {})",
            status.message()
        ));
    }
    RemoteAgentClientError::new(format!("{operation} failed: {status:?}"))
}

impl RemoteAgentClientError {
    pub fn new(message: impl Into<String>) -> Self {
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
