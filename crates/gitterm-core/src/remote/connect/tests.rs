use super::*;

const TOKEN: &str = "synthetic-connect-\"secret";

fn draft<'a>(name: &'a str, endpoint: &'a str, saved: Option<&'a Identity>) -> ConnectDraft<'a> {
    ConnectDraft {
        name,
        endpoint,
        token: TOKEN,
        saved,
    }
}

/// This is the pre-extraction Iced writer, deliberately independent of the new
/// save function. Compare exact file bytes, not just decoded field equality.
fn legacy_iced_write(root: &Path) -> Vec<u8> {
    fs::create_dir_all(root.join("tokens")).unwrap();
    fs::write(root.join("tokens/mac-mini.token"), format!("{TOKEN}\n")).unwrap();
    let file = RemoteAgentsFile {
        remote_agents: vec![RemoteAgentConfig {
            id: "mac-mini".into(),
            name: "Mac mini".into(),
            endpoint: "http://fixture.invalid:7777".into(),
            auth: RemoteAgentAuthConfig::Token {
                token_ref: format!("file:{}", root.join("tokens/mac-mini.token").display()),
            },
            shell_command: None,
            session_commands: Default::default(),
        }],
    };
    let json = serde_json::to_string_pretty(&file).unwrap();
    fs::write(root.join(REMOTE_AGENTS_FILE), json).unwrap();
    fs::read(root.join(REMOTE_AGENTS_FILE)).unwrap()
}

#[test]
fn isolated_connect_contract_child() {
    if std::env::var_os("GITTERM_CONNECT_TEST_CHILD").is_none() {
        return;
    }
    let root = crate::config_paths::desktop_config_dir().unwrap();
    let home = dirs::home_dir().unwrap();
    assert!(root.is_absolute() && root.starts_with(&home));
    let before = legacy_iced_write(root);
    let before_token = fs::read(root.join("tokens/mac-mini.token")).unwrap();
    fs::remove_file(root.join(REMOTE_AGENTS_FILE)).unwrap();
    let agent = save_remote_connect(
        root,
        &[],
        draft(" Mac mini ", "fixture.invalid:7777 ", None),
    )
    .unwrap();
    assert_eq!(fs::read(root.join(REMOTE_AGENTS_FILE)).unwrap(), before);
    assert_eq!(
        fs::read(root.join("tokens/mac-mini.token")).unwrap(),
        before_token
    );
    let saved = Identity::new(agent.id.clone(), agent.endpoint.clone()).unwrap();
    assert!(!format!("{agent:?}").contains(TOKEN));
    assert!(!String::from_utf8(before.clone()).unwrap().contains(TOKEN));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join("tokens/mac-mini.token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    // Both desktops decode the same DTO. Edits retain Iced's command fields.
    let mut iced = load_remote_agents(root).unwrap();
    iced.remote_agents[0].shell_command = Some("/bin/zsh -l".into());
    iced.remote_agents[0]
        .session_commands
        .insert("fixture".into(), "fixture-command".into());
    fs::write(
        root.join(REMOTE_AGENTS_FILE),
        serde_json::to_vec_pretty(&iced).unwrap(),
    )
    .unwrap();
    let gpui = save_remote_connect(
        root,
        &iced.remote_agents,
        draft("Renamed", &agent.endpoint, Some(&saved)),
    )
    .unwrap();
    let round_trip = load_remote_agents(root).unwrap();
    assert_eq!(round_trip.remote_agents[0].id, gpui.id);
    assert_eq!(round_trip.remote_agents[0].name, "Renamed");
    assert_eq!(
        round_trip.remote_agents[0].shell_command.as_deref(),
        Some("/bin/zsh -l")
    );
    assert_eq!(
        round_trip.remote_agents[0].session_commands["fixture"],
        "fixture-command"
    );
    let bytes = fs::read(root.join(REMOTE_AGENTS_FILE)).unwrap();
    let token_bytes = fs::read(root.join("tokens/mac-mini.token")).unwrap();
    assert_eq!(
        save_remote_connect(
            root,
            &[],
            draft("Mac mini", "other.invalid:7777", Some(&saved))
        )
        .unwrap_err(),
        IDENTITY_CHANGED
    );
    // A stale form cannot overwrite a disk edit either.
    let mut edited = load_remote_agents(root).unwrap();
    edited.remote_agents[0].endpoint = "http://other.invalid:7777".into();
    fs::write(
        root.join(REMOTE_AGENTS_FILE),
        serde_json::to_vec_pretty(&edited).unwrap(),
    )
    .unwrap();
    assert_eq!(
        save_remote_connect(root, &[], draft("Mac mini", &agent.endpoint, Some(&saved)))
            .unwrap_err(),
        IDENTITY_CHANGED
    );
    assert_eq!(
        load_remote_agents(root).unwrap().remote_agents[0].endpoint,
        "http://other.invalid:7777"
    );
    assert_eq!(
        fs::read(root.join("tokens/mac-mini.token")).unwrap(),
        token_bytes
    );
    fs::write(root.join(REMOTE_AGENTS_FILE), &bytes).unwrap();
    // Missing fields win over endpoint/file validation, exactly as Iced.
    assert_eq!(
        save_remote_connect(root, &[], draft("", "bad://", None)).unwrap_err(),
        REQUIRED
    );
    let error = save_remote_connect(
        root,
        &[],
        draft("Mac mini", &format!("http://{TOKEN}@fixture.invalid"), None),
    )
    .unwrap_err();
    assert_eq!(error, INVALID_ENDPOINT);
    assert!(!error.contains(TOKEN));
    for malformed in [
        b"{".as_slice(),
        format!("{{\"remote_agents\":\"{TOKEN}\"}}").as_bytes(),
    ] {
        fs::write(root.join(REMOTE_AGENTS_FILE), malformed).unwrap();
        let error =
            save_remote_connect(root, &[], draft("Mac mini", &agent.endpoint, None)).unwrap_err();
        assert_eq!(error, MALFORMED);
        assert_eq!(
            save_remote_agents(root, &RemoteAgentsFile::default()).unwrap_err(),
            MALFORMED
        );
        assert!(!error.contains(TOKEN));
        assert_eq!(fs::read(root.join(REMOTE_AGENTS_FILE)).unwrap(), malformed);
        assert_eq!(
            fs::read(root.join("tokens/mac-mini.token")).unwrap(),
            token_bytes
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
        // This test runs alone in a child process, so changing umask cannot
        // interfere with another test or write outside the synthetic root.
        let previous = unsafe { libc::umask(0o400) };
        let result = write_remote_agent_token(root, "private-mode", TOKEN);
        unsafe {
            libc::umask(previous);
        }
        result.unwrap();
        assert_eq!(
            fs::metadata(root.join("tokens/private-mode.token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::env::set_var(
            "GITTERM_SYNTHETIC_NON_UTF8_TOKEN",
            std::ffi::OsString::from_vec(b"synthetic-secret-\xff".to_vec()),
        );
        let error = super::super::token::resolve_token_ref("env:GITTERM_SYNTHETIC_NON_UTF8_TOKEN")
            .unwrap_err();
        assert!(!error.to_string().contains("synthetic-secret"));
        assert!(!format!("{error:?}").contains("synthetic-secret"));
    }
    assert!(!home.join(".config").exists());
}

#[test]
fn connect_contract_uses_absolute_override_and_temporary_home() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote::connect::tests::isolated_connect_contract_child",
            "--nocapture",
        ])
        .env("HOME", &home)
        .env("GITTERM_V6_CONFIG_DIR", home.join("v6"))
        .env("GITTERM_CONNECT_TEST_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

mod wire {
    pub use crate::remote::protocol::v6::*;
}
#[path = "../../../../../tests/support/remote_fixture.rs"]
mod remote_fixture;

// Tonic's interceptor API requires its unboxed Status as the error type.
#[allow(clippy::result_large_err)]
fn reject_echoed_token(_: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
    Err(tonic::Status::unauthenticated(format!(
        "Bearer {TOKEN} rejected"
    )))
}

#[tokio::test]
async fn handshake_never_displays_or_logs_a_token_echoed_by_a_peer() {
    use crate::remote::client::{RemoteAgentBackend, RemoteAgentClientConfig};
    use tokio_stream::wrappers::TcpListenerStream;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let service = wire::git_term_agent_server::GitTermAgentServer::with_interceptor(
            remote_fixture::Fixture::default(),
            reject_echoed_token,
        );
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let client = RemoteAgentBackend::new(
        RemoteAgentClientConfig {
            remote_id: "fixture".into(),
            name: "Fixture".into(),
            endpoint,
            token_ref: "fixture-only".into(),
        },
        |_| Ok(TOKEN.into()),
        "fixture",
    );
    let error = client.handshake().await.unwrap_err();
    assert!(!error.to_string().contains(TOKEN));
    assert!(!format!("{error:?}").contains(TOKEN));
    assert!(error.to_string().contains("[redacted]"));
    server.abort();
}
