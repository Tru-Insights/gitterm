use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tonic::metadata::MetadataMap;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use super::protocol::v1::git_term_agent_server::{GitTermAgent, GitTermAgentServer};
use super::protocol::v1::{
    DirEntry, HandshakeRequest, HandshakeResponse, ListDirRequest, ListDirResponse,
};

pub const PROTOCOL_VERSION: u32 = 1;

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
            capabilities: vec!["handshake".to_string(), "list_dir".to_string()],
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

pub(crate) fn is_authorized_metadata(metadata: &MetadataMap, expected_token: &str) -> bool {
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
