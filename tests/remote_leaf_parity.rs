//! Pre-extraction wire/source characterization. Only disposable loopback fixtures.
#[path = "../src/source.rs"]
mod source;
mod wire {
    pub use gitterm::agentd::protocol::v6::*;
}
#[path = "support/remote_fixture.rs"]
mod remote_fixture;
use gitterm::agentd::{client::*, protocol::v6::*};
use remote_fixture::Fixture;
use source::{SourcePath, WorkspaceSource};
use tokio_stream::wrappers::TcpListenerStream;

#[tokio::test]
async fn remote_leaf_wire_identity_and_source_trace() {
    let fixture = Fixture::default();
    let requests = fixture.requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(git_term_agent_server::GitTermAgentServer::new(fixture))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("token");
    std::fs::write(&token_path, "fixture-token\n").unwrap();
    let config = RemoteAgentClientConfig {
        remote_id: "saved-remote".into(),
        name: "Saved name".into(),
        endpoint: endpoint.clone(),
        token_ref: format!("file:{}", token_path.display()),
    };
    let backend = RemoteAgentBackend::new(config.clone());
    let handshake = backend.handshake().await.unwrap();
    assert_eq!(handshake.remote_id, "saved-remote");
    assert_eq!(handshake.endpoint, endpoint);
    assert_eq!(handshake.agent_name, "fixture-agent");
    assert_eq!(handshake.agent_version, "fixture-version");
    assert_eq!(
        handshake.protocol_version,
        gitterm::agentd::server::PROTOCOL_VERSION
    );
    assert_eq!(handshake.home_dir, "Q:/opaque/home");
    // Preserve unknown capabilities, their order and duplicates; callers gate features.
    assert_eq!(
        handshake.capabilities,
        ["read_file", "future_capability", "read_file"]
    );
    assert_eq!(backend.handshake().await.unwrap(), handshake);
    let mut other = config.clone();
    other.remote_id = "other-saved-remote".into();
    assert_eq!(
        RemoteAgentBackend::new(other)
            .handshake()
            .await
            .unwrap()
            .remote_id,
        "other-saved-remote"
    );
    // Each operation re-reads the reference; failed auth is not a cached handshake.
    std::fs::write(&token_path, "").unwrap();
    assert!(backend
        .handshake()
        .await
        .unwrap_err()
        .to_string()
        .contains("is empty"));
    std::fs::write(&token_path, "fixture-token\n").unwrap();
    let source = WorkspaceSource::RemoteAgent {
        workspace_id: "fixture-ws".into(),
        root: "Q:/opaque/../repo".into(),
        client: config,
    };
    assert_eq!(
        source.root(),
        SourcePath::Remote("Q:/opaque/../repo".into())
    );
    assert!(source.root().as_local().is_none());
    assert!(source.capabilities().open_file);
    assert!(!source.capabilities().edit_file);
    assert!(!source.capabilities().git_worktrees);
    let listing = source.list_dir(19, source.root(), true).await;
    assert_eq!(listing.error, None);
    assert_eq!(listing.tab_id, 19);
    assert_eq!(
        listing.current_dir,
        SourcePath::Remote("Q:/canonical/repo/sub".into())
    );
    assert_eq!(
        listing.parent,
        Some(SourcePath::Remote("Q:/canonical/repo".into()))
    );
    assert_eq!(
        listing.entries[0].path,
        SourcePath::Remote("Q:/opaque/../bytes.bin".into())
    );
    let content = source.read_file(&listing.entries[0].path, 3).await.unwrap();
    assert_eq!(content.data, [0, 255, 128]);
    assert_eq!(content.total_size, 9);
    assert!(content.truncated);
    assert!(source
        .read_file(&SourcePath::Local("/fixture/file".into()), 3)
        .await
        .unwrap_err()
        .contains("does not belong"));
    let actual: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|(op, bytes)| serde_json::json!({"operation":op,"bytes":bytes}))
        .collect();
    let expected: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/stage45/remote/requests.json")).unwrap();
    assert_eq!(actual, expected);
    server.abort();
}
