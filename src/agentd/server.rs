use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tonic::metadata::MetadataMap;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use super::protocol::v1::git_term_agent_server::{GitTermAgent, GitTermAgentServer};
use super::protocol::v1::{
    DirEntry, HandshakeRequest, HandshakeResponse, ListDirRequest, ListDirResponse, ReadFileChunk,
    ReadFileRequest,
};

pub const PROTOCOL_VERSION: u32 = 1;

/// Hard ceiling for one ReadFile stream; requests may ask for less.
const READ_FILE_DEFAULT_MAX_BYTES: u64 = 2_000_000;
const READ_FILE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct AgentServerConfig {
    pub bind_addr: SocketAddr,
    pub auth_token: String,
    pub agent_name: String,
}

#[derive(Debug, Clone)]
pub struct GitTermAgentService {
    agent_name: String,
}

impl GitTermAgentService {
    pub fn new(agent_name: String) -> Self {
        Self { agent_name }
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

        Ok(Response::new(HandshakeResponse {
            agent_name: self.agent_name.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            capabilities: vec![
                "handshake".to_string(),
                "list_dir".to_string(),
                "read_file".to_string(),
            ],
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
        let item = DirEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_dir: file_type.is_dir(),
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
    let service = GitTermAgentService::new(config.agent_name);
    let authenticated_service =
        GitTermAgentServer::with_interceptor(service, move |request: Request<()>| {
            if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                Ok(request)
            } else {
                Err(Status::unauthenticated("missing or invalid bearer token"))
            }
        });

    Server::builder()
        .add_service(authenticated_service)
        .serve(config.bind_addr)
        .await?;

    Ok(())
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
    use super::super::protocol::v1::git_term_agent_client::GitTermAgentClient;
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
