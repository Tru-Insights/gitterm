//! Disposable gRPC fixture, shared by root and core characterization tests.
use super::wire::*;
use prost::Message;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

#[derive(Clone, Default)]
pub struct Fixture {
    pub requests: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
}
impl Fixture {
    fn record<T: Message>(&self, name: &str, request: Request<T>) -> T {
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            "Bearer fixture-token"
        );
        let request = request.into_inner();
        self.requests
            .lock()
            .unwrap()
            .push((name.into(), request.encode_to_vec()));
        request
    }
}
#[tonic::async_trait]
impl git_term_agent_server::GitTermAgent for Fixture {
    async fn handshake(
        &self,
        request: Request<HandshakeRequest>,
    ) -> Result<Response<HandshakeResponse>, Status> {
        let request = self.record("handshake", request);
        assert_eq!(request.protocol_version, 1);
        Ok(Response::new(HandshakeResponse {
            agent_name: "fixture-agent".into(),
            agent_version: "fixture-version".into(),
            protocol_version: 1,
            capabilities: vec![
                "read_file".into(),
                "future_capability".into(),
                "read_file".into(),
            ],
            home_dir: "Q:/opaque/home".into(),
        }))
    }
    async fn list_dir(
        &self,
        request: Request<ListDirRequest>,
    ) -> Result<Response<ListDirResponse>, Status> {
        let request = self.record("list_dir", request);
        Ok(Response::new(ListDirResponse {
            workspace_id: request.workspace_id,
            root: "Q:/canonical/repo".into(),
            current_dir: "Q:/canonical/repo/sub".into(),
            entries: vec![DirEntry {
                name: "bytes.bin".into(),
                path: "Q:/opaque/../bytes.bin".into(),
                is_dir: false,
                is_git_repo: false,
            }],
        }))
    }
    type ReadFileStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<ReadFileChunk, Status>> + Send>>;
    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        self.record("read_file", request);
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![
            Ok(ReadFileChunk {
                data: vec![0, 255],
                total_size: 9,
                truncated: true,
            }),
            Ok(ReadFileChunk {
                data: vec![128],
                total_size: 9,
                truncated: true,
            }),
        ]))))
    }
    async fn git_status(
        &self,
        _request: Request<GitStatusRequest>,
    ) -> Result<Response<GitStatusResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn git_diff(
        &self,
        _request: Request<GitDiffRequest>,
    ) -> Result<Response<GitDiffResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn start_session(
        &self,
        _request: Request<StartSessionRequest>,
    ) -> Result<Response<Session>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn stop_session(
        &self,
        _request: Request<StopSessionRequest>,
    ) -> Result<Response<StopSessionResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    type AttachTerminalStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<TerminalOutput, Status>> + Send>>;
    async fn attach_terminal(
        &self,
        _request: Request<tonic::Streaming<TerminalInput>>,
    ) -> Result<Response<Self::AttachTerminalStream>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn list_chats(
        &self,
        request: Request<ListChatsRequest>,
    ) -> Result<Response<ListChatsResponse>, Status> {
        self.record("list_chats", request);
        Ok(Response::new(ListChatsResponse {
            chats: vec![ChatEntry {
                id: "fixture-chat".into(),
                backend: "claude".into(),
                path: "Q:/opaque/chat.jsonl".into(),
                cwd: "Q:/opaque/repo".into(),
                title: "Fixture".into(),
                ..Default::default()
            }],
        }))
    }
    async fn chat_preview(
        &self,
        request: Request<ChatPreviewRequest>,
    ) -> Result<Response<ChatPreviewResponse>, Status> {
        let request = self.record("chat_preview", request);
        if request.path != "Q:/opaque/chat.jsonl" {
            return Err(Status::permission_denied("outside fixture"));
        }
        Ok(Response::new(ChatPreviewResponse {
            messages: vec![ChatPreviewMessage {
                is_user: true,
                text: "fixture message".into(),
            }],
            message_count: Some(1),
        }))
    }
    type ClaudeChatStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<ClaudeChatServerMessage, Status>> + Send>,
    >;
    async fn claude_chat(
        &self,
        _request: Request<tonic::Streaming<ClaudeChatClientMessage>>,
    ) -> Result<Response<Self::ClaudeChatStream>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn claude_chat_history(
        &self,
        _request: Request<ClaudeChatHistoryRequest>,
    ) -> Result<Response<ClaudeChatHistoryResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn list_claude_chats(
        &self,
        _request: Request<ListClaudeChatsRequest>,
    ) -> Result<Response<ListClaudeChatsResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn create_phone_pairing(
        &self,
        _request: Request<CreatePhonePairingRequest>,
    ) -> Result<Response<CreatePhonePairingResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn list_phones(
        &self,
        _request: Request<ListPhonesRequest>,
    ) -> Result<Response<ListPhonesResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn revoke_phone(
        &self,
        _request: Request<RevokePhoneRequest>,
    ) -> Result<Response<RevokePhoneResponse>, Status> {
        Err(Status::unimplemented("fixture"))
    }
}
