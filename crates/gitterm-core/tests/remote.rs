use gitterm_core::remote::config::{RemoteAgentConfig, WorkspaceLocationConfig};

#[test]
fn saved_identity_and_opaque_location_keep_their_json_contract() {
    let saved = r#"{"id":"saved-remote","name":"Fixture","endpoint":"http://fixture.invalid:1","auth":{"type":"token","token_ref":"env:FIXTURE_UNUSED_TOKEN"}}"#;
    let config: RemoteAgentConfig = serde_json::from_str(saved).unwrap();
    assert!(config.shell_command.is_none());
    assert!(config.session_commands.is_empty());
    assert_eq!(serde_json::to_string(&config).unwrap(), saved);
    let location = r#"{"kind":"remote_agent","remote_id":"saved-remote","workspace_id":"workspace","root":"Q:/opaque/../repo"}"#;
    let config: WorkspaceLocationConfig = serde_json::from_str(location).unwrap();
    assert_eq!(serde_json::to_string(&config).unwrap(), location);
}

#[cfg(feature = "runtime")]
mod wire {
    pub use gitterm_core::remote::protocol::v6::*;
}
#[cfg(feature = "runtime")]
#[path = "../../../tests/support/remote_fixture.rs"]
mod remote_fixture;

#[cfg(feature = "runtime")]
mod runtime {
    use super::*;
    use gitterm_core::remote::{client::*, source::*};
    use std::path::Path;
    use tokio_stream::wrappers::TcpListenerStream;

    #[derive(Debug, Clone)]
    struct FixtureHost;
    impl SourceHost for FixtureHost {
        type ClientConfig = RemoteAgentClientConfig;
        fn backend(config: Self::ClientConfig) -> RemoteAgentBackend {
            RemoteAgentBackend::new(config, |_| Ok("fixture-token".into()), "6.0.0")
        }
        fn local_display_dir(_root: &Path, dir: &Path) -> String {
            format!("{}/", dir.display())
        }
    }
    type Source = WorkspaceSource<FixtureHost>;

    #[tokio::test]
    async fn core_source_reads_local_and_remote_bytes_without_ambient_credentials() {
        let fixture = remote_fixture::Fixture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(wire::git_term_agent_server::GitTermAgentServer::new(
                    fixture,
                ))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let remote = Source::RemoteAgent {
            workspace_id: "workspace".into(),
            root: "Q:/opaque/../repo".into(),
            client: RemoteAgentClientConfig {
                remote_id: "saved-remote".into(),
                name: "Fixture".into(),
                endpoint,
                token_ref: "fixture-only".into(),
            },
        };
        let listing = remote.list_dir(4, remote.root(), false).await;
        assert_eq!(listing.error, None);
        assert_eq!(
            listing.current_dir,
            SourcePath::Remote("Q:/canonical/repo/sub".into())
        );
        assert!(listing.entries[0].path.as_local().is_none());
        let bytes = remote.read_file(&listing.entries[0].path, 3).await.unwrap();
        assert_eq!(bytes.data, [0, 255, 128]);
        assert_eq!(bytes.total_size, 9);
        assert!(bytes.truncated);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bytes.bin");
        std::fs::write(&file, [0, 255, 128, 10]).unwrap();
        let local = Source::Local {
            root: dir.path().into(),
        };
        let bytes = local.read_file(&SourcePath::Local(file), 3).await.unwrap();
        assert_eq!(bytes.data, [0, 255, 128]);
        assert_eq!(bytes.total_size, 4);
        assert!(bytes.truncated);
        assert!(local.capabilities().edit_file);
        assert!(!remote.capabilities().edit_file);
        server.abort();
    }
}
