//! Pin the actual pre-wiring Iced handlers without starting the application.
//! Including the binary here keeps private App state accessible without editing
//! main.rs. Tasks are inspected and dropped, never polled (no credential IO).
include!("../src/main.rs");

#[cfg(test)]
mod remote_connection_baseline {
    use super::*;

    fn agent() -> RemoteAgentConfig {
        RemoteAgentConfig {
            id: "fixture".into(),
            name: "Fixture".into(),
            endpoint: "http://fixture.invalid:1234".into(),
            auth: config::RemoteAgentAuthConfig::Token {
                token_ref: "env:GITTERM_V6_UNUSED_FIXTURE".into(),
            },
            shell_command: None,
            session_commands: Default::default(),
        }
    }

    fn reply(capabilities: &[&str]) -> RemoteAgentHandshake {
        RemoteAgentHandshake {
            remote_id: "fixture".into(),
            endpoint: agent().endpoint,
            agent_name: "fixture-agent".into(),
            agent_version: "fixture-version".into(),
            protocol_version: 1,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            home_dir: "Q:/opaque/../home".into(),
        }
    }

    #[test]
    fn defaults_dedup_forced_retry_and_missing_config() {
        let fixture = tempfile::tempdir().unwrap();
        let mut app = baseline_tests::app_with_tabs(fixture.path());
        app.remote_agent_defs = vec![agent()];
        app.register_remote_agent_connection_defaults();
        let conn = &app.remote_agent_connections["fixture"];
        assert!(matches!(
            conn.status,
            RemoteAgentConnectionStatus::Disconnected
        ));
        assert!(conn.last_attempt.is_none() && conn.last_connected.is_none());
        assert_eq!(
            app.ensure_remote_agent_connection("fixture", false).units(),
            1
        );
        assert_eq!(
            app.ensure_remote_agent_connection("fixture", false).units(),
            0
        );
        app.remote_agent_connections
            .get_mut("fixture")
            .unwrap()
            .status = RemoteAgentConnectionStatus::Connected(reply(&["unknown", "unknown"]));
        assert_eq!(
            app.ensure_remote_agent_connection("fixture", false).units(),
            0
        );
        app.register_remote_agent_connection_defaults();
        assert!(matches!(
            app.remote_agent_connections["fixture"].status,
            RemoteAgentConnectionStatus::Connected(_)
        ));
        assert_eq!(
            app.ensure_remote_agent_connection("fixture", true).units(),
            1
        );
        let conn = &app.remote_agent_connections["fixture"];
        assert!(matches!(
            conn.status,
            RemoteAgentConnectionStatus::Connecting
        ));
        assert!(conn.last_attempt.is_some() && conn.last_connected.is_none());
        assert_eq!(
            app.ensure_remote_agent_connection("missing", false).units(),
            0
        );
        assert!(matches!(&app.remote_agent_connections["missing"].status,
            RemoteAgentConnectionStatus::Error(error) if error == "Remote agent config not found for missing"));
    }

    #[test]
    fn completed_handshake_capabilities_home_failure_and_form_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let mut app = baseline_tests::app_with_tabs(fixture.path());
        app.remote_agent_defs = vec![agent()];
        app.register_remote_agent_connection_defaults();
        app.remote_connect_form = Some(RemoteConnectForm {
            saved_remote_id: Some("another-remote".into()),
            ..Default::default()
        });
        // No active remote workspace, picker or persistent-chat capability:
        // the real handler emits no IO tasks. Unknown names/order are retained.
        let task = app.update_event(Event::RemoteAgentHandshakeLoaded(
            RemoteAgentHandshakeSnapshot {
                remote_id: "fixture".into(),
                endpoint: agent().endpoint,
                result: Ok(reply(&["unknown", "unknown"])),
            },
        ));
        drop(task);
        assert!(app.remote_connect_form.is_some());
        assert_eq!(
            app.folder_picker_home(Some("fixture")).as_deref(),
            Some("Q:/opaque/../home")
        );
        let conn = &app.remote_agent_connections["fixture"];
        assert_eq!(conn.name, "Fixture");
        assert_eq!(conn.endpoint, agent().endpoint);
        assert!(
            matches!(&conn.status, RemoteAgentConnectionStatus::Connected(h) if h.capabilities == ["unknown", "unknown"])
        );
        let last_connected = conn.last_connected;
        assert!(last_connected.is_some() && conn.last_attempt.is_some());
        drop(app.update_event(Event::RemoteAgentHandshakeLoaded(
            RemoteAgentHandshakeSnapshot {
                remote_id: "fixture".into(),
                endpoint: agent().endpoint,
                result: Err("fixture failure".into()),
            },
        )));
        let conn = &app.remote_agent_connections["fixture"];
        assert!(
            matches!(&conn.status, RemoteAgentConnectionStatus::Error(e) if e == "fixture failure")
        );
        assert_eq!(conn.last_connected, last_connected);
        assert!(app.folder_picker_home(Some("fixture")).is_none());
        assert!(!app.remote_runs_claude_chat("fixture"));
        assert!(!app.remote_serves_phones("fixture"));
        assert!(!app.remote_owns_claude_chats("fixture"));
        assert!(app.remote_connect_form.is_some());
        assert!(!remote_connect_form_done(
            app.remote_connect_form.as_ref().unwrap(),
            "fixture",
            true
        ));
    }
    #[test]
    fn matching_picker_is_notified_on_success_and_failure() {
        let fixture = tempfile::tempdir().unwrap();
        let mut app = baseline_tests::app_with_tabs(fixture.path());
        app.remote_agent_defs = vec![agent()];
        app.folder_picker = Some(FolderPicker {
            purpose: FolderPickerPurpose::NewWorkspace,
            remote_id: Some("fixture".into()),
            files: FilesState::at(SourcePath::Remote("/".into())),
            path_input: "/".into(),
            path_error: None,
            typed_seq: 0,
            awaiting_home: true,
            start_hint: None,
        });
        let task = app.update_event(Event::RemoteAgentHandshakeLoaded(
            RemoteAgentHandshakeSnapshot {
                remote_id: "other".into(),
                endpoint: agent().endpoint,
                result: Err("fixture failure".into()),
            },
        ));
        assert_eq!(task.units(), 0);
        assert!(app.folder_picker.as_ref().unwrap().awaiting_home);
        let task = app.update_event(Event::RemoteAgentHandshakeLoaded(
            RemoteAgentHandshakeSnapshot {
                remote_id: "fixture".into(),
                endpoint: agent().endpoint,
                result: Err("fixture failure".into()),
            },
        ));
        assert_eq!(task.units(), 0);
        let picker = app.folder_picker.as_ref().unwrap();
        assert!(!picker.awaiting_home);
        assert_eq!(picker.files.dir, SourcePath::Remote("/".into()));
        app.folder_picker.as_mut().unwrap().awaiting_home = true;
        let task = app.update_event(Event::RemoteAgentHandshakeLoaded(
            RemoteAgentHandshakeSnapshot {
                remote_id: "fixture".into(),
                endpoint: agent().endpoint,
                result: Ok(reply(&[])),
            },
        ));
        // This task is only a future; dropping it never resolves credentials.
        assert_eq!(task.units(), 1);
        drop(task);
        let picker = app.folder_picker.as_ref().unwrap();
        assert!(!picker.awaiting_home);
        assert_eq!(
            picker.files.dir,
            SourcePath::Remote("Q:/opaque/../home".into())
        );
        assert!(picker.files.dir.as_local().is_none());
    }
}
