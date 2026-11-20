use super::*;
use gitterm_core::remote::config::RemoteAgentAuthConfig;

fn saved_remote() -> RemoteAgentConfig {
    RemoteAgentConfig {
        id: "remote-id-42".into(),
        name: "fixture-host".into(),
        endpoint: "http://fixture.invalid:7777".into(),
        auth: RemoteAgentAuthConfig::Token {
            token_ref: "env:FIXTURE_TOKEN".into(),
        },
        shell_command: None,
        session_commands: Default::default(),
    }
}

#[test]
fn token_is_masked_in_presentation_and_redacted_from_debug() {
    let mut form = RemoteForm::default();
    form.set_name("fixture-host");
    form.set_endpoint("http://fixture.invalid:7777");
    form.set_token("fixture-secret-token");

    let rendered = form.presentation_lines(&[], None).join("\n");
    let intent_debug = format!("{:?}", form.connect().unwrap());
    let token_debug = format!("{:?}", form.token);

    assert!(!rendered.contains("fixture-secret-token"));
    assert!(rendered.contains("Token: ••••••••••••••••"));
    assert!(!intent_debug.contains("fixture-secret-token"));
    assert!(!token_debug.contains("fixture-secret-token"));
    assert!(intent_debug.contains("[REDACTED]"));
}

#[test]
fn saved_remote_intents_use_core_identity() {
    let remote = saved_remote();
    assert_eq!(
        RemoteForm::connect_saved(&remote),
        RemoteIntent::ConnectSaved {
            remote_id: "remote-id-42".into(),
        }
    );
    assert_eq!(
        RemoteForm::remove(&remote),
        RemoteIntent::Remove {
            remote_id: "remote-id-42".into(),
        }
    );
}

#[test]
fn shown_errors_redact_the_entered_token() {
    let mut form = RemoteForm::default();
    form.set_token("fixture-secret-token");
    let rows = form.presentation_lines(
        &[],
        Some(&RemoteStatus::Failed(SafeRemoteError::new(
            "request rejected for token fixture-secret-token",
            "fixture-secret-token",
        ))),
    );
    let rendered = rows.join("\n");

    assert!(!rendered.contains("fixture-secret-token"));
    assert!(rendered.contains("request rejected for token [redacted]"));
}

#[test]
fn escape_cancels_without_an_intent_and_enter_submits() {
    let mut form = RemoteForm::default();
    form.set_name("fixture-host");
    form.set_endpoint("http://fixture.invalid:7777");
    form.set_token("fixture-secret-token");

    assert_eq!(form.key(RemoteKey::Escape), None);
    assert_eq!(form.key(RemoteKey::Enter), form.connect(),);
}

#[test]
fn saved_records_are_rendered_from_the_core_dto() {
    let lines = RemoteForm::default().presentation_lines(&[saved_remote()], None);
    assert!(lines.contains(&"remote-id-42 · fixture-host · http://fixture.invalid:7777".into()));
}
