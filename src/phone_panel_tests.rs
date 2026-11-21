//! TRU-274 PR 26a: the "Pair a phone" panel (TRU-151) as Iced drives it,
//! pinned before its state and rules move into core. Each test runs in a
//! child process with a temporary config dir, so `remote-agents.json` is
//! never read from real state. The fixture agent is the real gRPC service
//! with a temporary phone registry on 127.0.0.1.
use super::*;
use gitterm::agentd::phones::PhoneRegistry;
use gitterm::agentd::protocol::v6::git_term_agent_server::GitTermAgentServer;
use gitterm::agentd::server::{is_authorized_metadata, GitTermAgentService, PhoneAccess};
use tokio_stream::wrappers::TcpListenerStream;

const REMOTE: &str = "fixture-remote";
const TOKEN_ENV: &str = "GITTERM_V6_PHONE_PANEL_TEST_TOKEN";
const TOKEN: &str = "phone-panel-secret";

fn isolated(name: &str) -> bool {
    if std::env::var("GITTERM_V6_PHONE_PANEL_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let config = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("phone_panel_tests::{name}"),
            "--nocapture",
        ])
        .env(config::CONFIG_DIR_ENV, config.path())
        .env("GITTERM_V6_PHONE_PANEL_CHILD", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

/// A disposable agent: the real service, a temporary registry, the bearer
/// token check, on 127.0.0.1. `phones: false` is an agent run with
/// `--phone-addr off`.
struct FixtureAgent {
    runtime: tokio::runtime::Runtime,
    endpoint: String,
    phones: Option<Arc<PhoneRegistry>>,
    _dir: tempfile::TempDir,
}

impl FixtureAgent {
    // Tonic interceptors return `Status` directly.
    #[allow(clippy::result_large_err)]
    fn start(phones: bool) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let registry = phones.then(|| PhoneRegistry::load(dir.path()).unwrap());
        let mut service = GitTermAgentService::new("fixture-agent".to_string());
        if let Some(registry) = &registry {
            service = service.with_phone_access(PhoneAccess {
                phones: registry.clone(),
                base_url: "http://127.0.0.1:9".into(),
            });
        }
        let endpoint = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let service =
                GitTermAgentServer::with_interceptor(service, |request: tonic::Request<()>| {
                    if is_authorized_metadata(request.metadata(), TOKEN) {
                        Ok(request)
                    } else {
                        Err(tonic::Status::unauthenticated(
                            "missing or invalid bearer token",
                        ))
                    }
                });
            tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(service)
                    .serve_with_incoming(TcpListenerStream::new(listener)),
            );
            format!("http://{addr}")
        });
        std::env::set_var(TOKEN_ENV, TOKEN);
        Self {
            runtime,
            endpoint,
            phones: registry,
            _dir: dir,
        }
    }

    fn backend(&self, app: &App) -> RemoteAgentBackend {
        RemoteAgentBackend::new(app.remote_chat_client(REMOTE).unwrap())
    }
}

fn app_with_remote(endpoint: &str) -> (tempfile::TempDir, App) {
    let fixture = tempfile::tempdir().unwrap();
    let mut app = baseline_tests::app_with_tabs(fixture.path());
    app.remote_agent_defs.push(
        serde_json::from_value(serde_json::json!({
            "id": REMOTE,
            "name": "Fixture Mac",
            "endpoint": endpoint,
            "auth": {"type": "token", "token_ref": format!("env:{TOKEN_ENV}")},
        }))
        .unwrap(),
    );
    (fixture, app)
}

fn panel(app: &App) -> &PhonePanel {
    app.phone_panel.as_ref().expect("the phone panel is open")
}

fn units(task: Task<Event>) -> usize {
    task.units()
}

fn pairing(url: &str, scope: &str) -> PhonePairing {
    PhonePairing {
        pairing_url: url.into(),
        expires_unix_secs: 1_900_000_000,
        scope: scope.into(),
        short_code: "ABCD-EFGH".into(),
    }
}

fn phone(id: &str, scope: &str) -> PairedPhoneInfo {
    PairedPhoneInfo {
        id: id.into(),
        name: format!("phone {id}"),
        scope: scope.into(),
        created_unix_secs: 1_800_000_000,
        last_seen_unix_secs: 0,
    }
}

const NO_REMOTE: &str = "No remote agent named ghost is configured in remote-agents.json.";

#[test]
fn open_names_the_machine_starts_a_list_and_closes() {
    if isolated("open_names_the_machine_starts_a_list_and_closes") {
        return;
    }
    let (_dir, mut app) = app_with_remote("http://127.0.0.1:9");
    app.bar_menu = Some(BarMenu::Repos);
    let known = units(app.update_event(Event::PhonePanelOpen(REMOTE.into())));
    assert!(app.bar_menu.is_none());
    let p = panel(&app);
    assert_eq!(p.remote_id, REMOTE);
    assert_eq!(p.machine_name, "Fixture Mac");
    assert_eq!(p.device_name, "iPhone");
    assert!(!p.allow_act && !p.busy);
    assert!(p.pairing.is_none() && p.phones.is_none() && p.error.is_none());

    // An unconfigured remote: its id is the name and the list fails at once.
    let unknown = units(app.update_event(Event::PhonePanelOpen("ghost".into())));
    let p = panel(&app);
    assert_eq!(p.remote_id, "ghost");
    assert_eq!(p.machine_name, "ghost");
    assert_eq!(p.phones.as_deref(), Some(&[][..]));
    assert_eq!(p.error.as_deref(), Some(NO_REMOTE));
    assert!(!p.busy);
    // The list call is the one extra unit when the remote has a client.
    assert_eq!(known, unknown + 1);

    let _ = app.update_event(Event::PhonePanelClose);
    assert!(app.phone_panel.is_none());
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let _ = app.update_event(Event::KeyPressed(
        Key::Named(key::Named::Escape),
        Modifiers::empty(),
    ));
    assert!(app.phone_panel.is_none(), "Escape closes the panel");
}

#[test]
fn edits_change_the_panel_and_are_dropped_without_one() {
    if isolated("edits_change_the_panel_and_are_dropped_without_one") {
        return;
    }
    let (_dir, mut app) = app_with_remote("http://127.0.0.1:9");
    for event in [
        Event::PhoneDeviceNameChanged("x".into()),
        Event::PhoneAllowActToggled(true),
        Event::PhoneShortCodeEdited("x".into()),
        Event::PhonePairingCreate,
        Event::PhoneRevoke("p1".into()),
        Event::PhonePairingCreated(REMOTE.into(), Ok(pairing("http://x/pair#1", "view"))),
        Event::PhonesLoaded(REMOTE.into(), Ok(vec![phone("p1", "view")])),
        Event::PhoneRevoked(REMOTE.into(), Ok(true)),
    ] {
        assert_eq!(units(app.update_event(event)), 0);
        assert!(app.phone_panel.is_none());
    }

    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    assert_eq!(
        units(app.update_event(Event::PhoneDeviceNameChanged("  Work phone ".into()))),
        0
    );
    assert_eq!(panel(&app).device_name, "  Work phone ");
    let _ = app.update_event(Event::PhoneAllowActToggled(true));
    assert!(panel(&app).allow_act);
    let _ = app.update_event(Event::PhoneAllowActToggled(false));
    assert!(!panel(&app).allow_act);

    // The short-code field is read-only: an edit changes nothing.
    let _ = app.update_event(Event::PhonePairingCreated(
        REMOTE.into(),
        Ok(pairing("http://x/pair#1", "view")),
    ));
    assert_eq!(
        units(app.update_event(Event::PhoneShortCodeEdited("ZZZZ".into()))),
        0
    );
    assert_eq!(
        panel(&app).pairing.as_ref().unwrap().short_code,
        "ABCD-EFGH"
    );
}

#[test]
fn create_validates_then_resolves_then_goes_busy() {
    if isolated("create_validates_then_resolves_then_goes_busy") {
        return;
    }
    let (_dir, mut app) = app_with_remote("http://127.0.0.1:9");
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));

    // A blank name is refused before anything else.
    let _ = app.update_event(Event::PhoneDeviceNameChanged("   ".into()));
    assert_eq!(units(app.update_event(Event::PhonePairingCreate)), 0);
    assert_eq!(panel(&app).error.as_deref(), Some("Name the phone first."));
    assert!(!panel(&app).busy);

    // The name check wins over a missing client.
    let _ = app.update_event(Event::PhonePanelOpen("ghost".into()));
    let _ = app.update_event(Event::PhoneDeviceNameChanged("".into()));
    assert_eq!(units(app.update_event(Event::PhonePairingCreate)), 0);
    assert_eq!(panel(&app).error.as_deref(), Some("Name the phone first."));
    // A named phone on an unconfigured remote: the lookup error, not busy.
    let _ = app.update_event(Event::PhoneDeviceNameChanged("iPhone".into()));
    assert_eq!(units(app.update_event(Event::PhonePairingCreate)), 0);
    assert_eq!(panel(&app).error.as_deref(), Some(NO_REMOTE));
    assert!(!panel(&app).busy);

    // A configured remote: busy, the error cleared, one call started.
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    app.phone_panel.as_mut().unwrap().error = Some("old".into());
    assert_eq!(units(app.update_event(Event::PhonePairingCreate)), 1);
    assert!(panel(&app).busy);
    assert!(panel(&app).error.is_none());

    // Busy: a second create is dropped and leaves everything as it was.
    app.phone_panel.as_mut().unwrap().error = Some("kept".into());
    let _ = app.update_event(Event::PhoneDeviceNameChanged("".into()));
    assert_eq!(units(app.update_event(Event::PhonePairingCreate)), 0);
    assert!(panel(&app).busy);
    assert_eq!(panel(&app).error.as_deref(), Some("kept"));
}

#[test]
fn completions_for_another_machine_or_a_closed_panel_are_dropped() {
    if isolated("completions_for_another_machine_or_a_closed_panel_are_dropped") {
        return;
    }
    let (_dir, mut app) = app_with_remote("http://127.0.0.1:9");
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let _ = app.update_event(Event::PhonePairingCreate);
    assert!(panel(&app).busy);
    for event in [
        Event::PhonePairingCreated("other".into(), Ok(pairing("http://x/pair#1", "act"))),
        Event::PhonePairingCreated("other".into(), Err("boom".into())),
        Event::PhonesLoaded("other".into(), Ok(vec![phone("p1", "act")])),
        Event::PhonesLoaded("other".into(), Err("boom".into())),
        Event::PhoneRevoked("other".into(), Ok(false)),
        Event::PhoneRevoked("other".into(), Err("boom".into())),
    ] {
        assert_eq!(units(app.update_event(event)), 0);
        let p = panel(&app);
        assert!(p.busy && p.error.is_none() && p.pairing.is_none() && p.phones.is_none());
    }

    // The machine switched while a call was in flight.
    let _ = app.update_event(Event::PhonePanelOpen("ghost".into()));
    let _ = app.update_event(Event::PhonePairingCreated(
        REMOTE.into(),
        Ok(pairing("http://x/pair#1", "act")),
    ));
    assert!(panel(&app).pairing.is_none());
    assert_eq!(panel(&app).error.as_deref(), Some(NO_REMOTE));

    // Closed and reopened on the same machine: the id still matches, so a
    // completion from the earlier panel is applied (remote id is the only gate).
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let _ = app.update_event(Event::PhonePairingCreate);
    let _ = app.update_event(Event::PhonePanelClose);
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let _ = app.update_event(Event::PhonePairingCreated(
        REMOTE.into(),
        Ok(pairing("http://x/pair#1", "act")),
    ));
    assert_eq!(panel(&app).pairing.as_ref().unwrap().url, "http://x/pair#1");
}

#[test]
fn pairing_and_list_completions_set_the_view_or_the_error() {
    if isolated("pairing_and_list_completions_set_the_view_or_the_error") {
        return;
    }
    let (_dir, mut app) = app_with_remote("http://127.0.0.1:9");
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let _ = app.update_event(Event::PhonePairingCreate);
    assert_eq!(
        units(app.update_event(Event::PhonePairingCreated(
            REMOTE.into(),
            Ok(pairing("http://127.0.0.1:9/pair#code", "act")),
        ))),
        0
    );
    let p = panel(&app);
    assert!(!p.busy && p.error.is_none());
    let view = p.pairing.as_ref().unwrap();
    assert_eq!(view.url, "http://127.0.0.1:9/pair#code");
    assert_eq!(view.short_code, "ABCD-EFGH");
    assert_eq!(view.scope, "act");
    assert_eq!(view.expires_unix_secs, 1_900_000_000);

    // A failure keeps the pairing already shown.
    let _ = app.update_event(Event::PhonePairingCreate);
    let _ = app.update_event(Event::PhonePairingCreated(
        REMOTE.into(),
        Err("boom".into()),
    ));
    let p = panel(&app);
    assert!(!p.busy);
    assert_eq!(p.error.as_deref(), Some("Could not create a pairing: boom"));
    assert_eq!(
        p.pairing.as_ref().unwrap().url,
        "http://127.0.0.1:9/pair#code"
    );

    // A success clears that error.
    let _ = app.update_event(Event::PhonePairingCreated(
        REMOTE.into(),
        Ok(pairing("http://127.0.0.1:9/pair#two", "view")),
    ));
    assert!(panel(&app).error.is_none());
    assert_eq!(panel(&app).pairing.as_ref().unwrap().scope, "view");

    let phones = vec![phone("p1", "view"), phone("p2", "act")];
    assert_eq!(
        units(app.update_event(Event::PhonesLoaded(REMOTE.into(), Ok(phones.clone())))),
        0
    );
    assert_eq!(panel(&app).phones.as_ref(), Some(&phones));
    assert!(panel(&app).error.is_none());
    // A failed list empties it and says why; it does not touch busy.
    app.phone_panel.as_mut().unwrap().busy = true;
    let _ = app.update_event(Event::PhonesLoaded(REMOTE.into(), Err("down".into())));
    let p = panel(&app);
    assert_eq!(p.phones.as_deref(), Some(&[][..]));
    assert_eq!(
        p.error.as_deref(),
        Some("Could not list paired phones: down")
    );
    assert!(p.busy);
    // A later good list keeps the error.
    let _ = app.update_event(Event::PhonesLoaded(REMOTE.into(), Ok(phones.clone())));
    assert_eq!(
        panel(&app).error.as_deref(),
        Some("Could not list paired phones: down")
    );
}

#[test]
fn revoke_goes_busy_and_its_completion_refreshes_the_list() {
    if isolated("revoke_goes_busy_and_its_completion_refreshes_the_list") {
        return;
    }
    let (_dir, mut app) = app_with_remote("http://127.0.0.1:9");
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    app.phone_panel.as_mut().unwrap().error = Some("old".into());
    assert_eq!(units(app.update_event(Event::PhoneRevoke("p1".into()))), 1);
    assert!(panel(&app).busy && panel(&app).error.is_none());
    // Revoke has no busy guard of its own (the view disables the button).
    assert_eq!(units(app.update_event(Event::PhoneRevoke("p2".into()))), 1);

    // Revoked: not busy, the error untouched, and the list reloaded.
    assert_eq!(
        units(app.update_event(Event::PhoneRevoked(REMOTE.into(), Ok(true)))),
        1
    );
    assert!(!panel(&app).busy && panel(&app).error.is_none());

    let _ = app.update_event(Event::PhoneRevoke("p1".into()));
    assert_eq!(
        units(app.update_event(Event::PhoneRevoked(REMOTE.into(), Ok(false)))),
        1
    );
    assert_eq!(
        panel(&app).error.as_deref(),
        Some("That phone was no longer paired with the agent.")
    );
    let _ = app.update_event(Event::PhoneRevoke("p1".into()));
    assert_eq!(
        units(app.update_event(Event::PhoneRevoked(REMOTE.into(), Err("nope".into())))),
        1
    );
    assert_eq!(panel(&app).error.as_deref(), Some("Could not revoke: nope"));
    assert!(!panel(&app).busy);

    // The remote went away while the revoke ran: the refresh says so.
    let _ = app.update_event(Event::PhoneRevoke("p1".into()));
    app.remote_agent_defs.clear();
    assert_eq!(
        units(app.update_event(Event::PhoneRevoked(REMOTE.into(), Ok(true)))),
        0
    );
    let p = panel(&app);
    assert!(!p.busy);
    assert_eq!(
        p.error.as_deref(),
        Some("No remote agent named fixture-remote is configured in remote-agents.json.")
    );
    assert_eq!(p.phones.as_deref(), Some(&[][..]));

    // A revoke on an unconfigured remote: the lookup error, not busy.
    assert_eq!(units(app.update_event(Event::PhoneRevoke("p1".into()))), 0);
    assert!(!panel(&app).busy);
}

/// The panel against a fixture agent: the payloads the panel sends (the
/// trimmed name, "view" or "act"), the scope the agent grants, the list and
/// both revoke answers, fed back as the completions Iced receives.
#[test]
fn a_fixture_agent_pairs_lists_and_revokes_with_the_panel_scope() {
    if isolated("a_fixture_agent_pairs_lists_and_revokes_with_the_panel_scope") {
        return;
    }
    let agent = FixtureAgent::start(true);
    let registry = agent.phones.clone().unwrap();
    let (_dir, mut app) = app_with_remote(&agent.endpoint);
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let backend = agent.backend(&app);
    let listed = agent
        .runtime
        .block_on(backend.list_phones())
        .map_err(|e| e.to_string());
    let _ = app.update_event(Event::PhonesLoaded(REMOTE.into(), listed));
    assert_eq!(panel(&app).phones.as_deref(), Some(&[][..]));

    for (allow, name, scope) in [(false, " Work phone ", "view"), (true, "Spare", "act")] {
        let _ = app.update_event(Event::PhoneAllowActToggled(allow));
        let _ = app.update_event(Event::PhoneDeviceNameChanged(name.into()));
        assert_eq!(units(app.update_event(Event::PhonePairingCreate)), 1);
        // What the handler sends: the trimmed name and the checkbox scope.
        let created = agent
            .runtime
            .block_on(backend.create_phone_pairing(name.trim().into(), scope.into()))
            .map_err(|e| e.to_string());
        let _ = app.update_event(Event::PhonePairingCreated(REMOTE.into(), created));
        let p = panel(&app);
        assert!(!p.busy && p.error.is_none(), "{:?}", p.error);
        let view = p.pairing.as_ref().unwrap();
        assert_eq!(view.scope, scope);
        assert!(
            view.url.starts_with("http://127.0.0.1:9/pair#"),
            "{}",
            view.url
        );
        assert_eq!(view.short_code.len(), 9);
        let code = view.url.rsplit('#').next().unwrap();
        registry.redeem(code).unwrap();
    }

    let listed = agent
        .runtime
        .block_on(backend.list_phones())
        .map_err(|e| e.to_string());
    let _ = app.update_event(Event::PhonesLoaded(REMOTE.into(), listed));
    let phones = panel(&app).phones.clone().unwrap();
    let mut seen: Vec<_> = phones
        .iter()
        .map(|p| (p.name.as_str(), p.scope.as_str(), p.last_seen_unix_secs > 0))
        .collect();
    seen.sort();
    // Redeeming a code counts as the phone's first use.
    assert_eq!(seen, [("Spare", "act", true), ("Work phone", "view", true)]);

    let id = phones[0].id.clone();
    for (answer, error) in [
        (true, None),
        (
            false,
            Some("That phone was no longer paired with the agent."),
        ),
    ] {
        assert_eq!(units(app.update_event(Event::PhoneRevoke(id.clone()))), 1);
        let revoked = agent
            .runtime
            .block_on(backend.revoke_phone(id.clone()))
            .map_err(|e| e.to_string());
        assert_eq!(revoked, Ok(answer));
        assert_eq!(
            units(app.update_event(Event::PhoneRevoked(REMOTE.into(), revoked))),
            1
        );
        assert_eq!(panel(&app).error.as_deref(), error);
    }
    assert_eq!(registry.list().len(), 1);
}

#[test]
fn an_agent_without_phone_access_explains_each_failure() {
    if isolated("an_agent_without_phone_access_explains_each_failure") {
        return;
    }
    let agent = FixtureAgent::start(false);
    let (_dir, mut app) = app_with_remote(&agent.endpoint);
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let backend = agent.backend(&app);
    let listed = agent
        .runtime
        .block_on(backend.list_phones())
        .map_err(|e| e.to_string());
    let _ = app.update_event(Event::PhonesLoaded(REMOTE.into(), listed));
    assert_eq!(
        panel(&app).error.as_deref(),
        Some(
            "Could not list paired phones: list_phones failed: phone access is off on this \
             agent (--phone-addr off)"
        )
    );
    let _ = app.update_event(Event::PhonePairingCreate);
    let created = agent
        .runtime
        .block_on(backend.create_phone_pairing("iPhone".into(), "view".into()))
        .map_err(|e| e.to_string());
    let _ = app.update_event(Event::PhonePairingCreated(REMOTE.into(), created));
    assert_eq!(
        panel(&app).error.as_deref(),
        Some(
            "Could not create a pairing: create_phone_pairing failed: phone access is off on \
             this agent (--phone-addr off)"
        )
    );
    let _ = app.update_event(Event::PhoneRevoke("p1".into()));
    let revoked = agent
        .runtime
        .block_on(backend.revoke_phone("p1".into()))
        .map_err(|e| e.to_string());
    let _ = app.update_event(Event::PhoneRevoked(REMOTE.into(), revoked));
    assert_eq!(
        panel(&app).error.as_deref(),
        Some("Could not revoke: revoke_phone failed: phone access is off on this agent (--phone-addr off)")
    );

    // A wrong token is refused by the agent and shown the same way.
    std::env::set_var(TOKEN_ENV, "wrong");
    let listed = agent
        .runtime
        .block_on(backend.list_phones())
        .map_err(|e| e.to_string());
    let _ = app.update_event(Event::PhonesLoaded(REMOTE.into(), listed));
    assert_eq!(
        panel(&app).error.as_deref(),
        Some("Could not list paired phones: list_phones failed: missing or invalid bearer token")
    );
}

/// Iced's executor for the core's calls, against the fixture agent: the
/// payload each call carries reaches the agent, and the answer comes back
/// as the event the panel applies.
#[test]
fn iced_runs_the_core_calls_against_the_fixture_agent() {
    if isolated("iced_runs_the_core_calls_against_the_fixture_agent") {
        return;
    }
    let agent = FixtureAgent::start(true);
    let registry = agent.phones.clone().unwrap();
    let (_dir, mut app) = app_with_remote(&agent.endpoint);
    let client = app.remote_chat_client(REMOTE).unwrap();
    let resolve = |_: &str| Ok::<_, String>(client.clone());
    let _ = app.update_event(Event::PhonePanelOpen(REMOTE.into()));
    let _ = app.update_event(Event::PhoneDeviceNameChanged(" Work phone ".into()));
    let _ = app.update_event(Event::PhoneAllowActToggled(true));

    let call = app.phone_panel.as_mut().unwrap().create(resolve).unwrap();
    assert_eq!(
        call.request,
        PhoneRequest::Create {
            device_name: "Work phone".into(),
            scope: "act",
        }
    );
    let event = agent.runtime.block_on(run_phone_call(call));
    assert!(
        matches!(&event, Event::PhonePairingCreated(id, Ok(p)) if id == REMOTE && p.scope == "act")
    );
    let _ = app.update_event(event);
    let code = panel(&app)
        .pairing
        .as_ref()
        .unwrap()
        .url
        .rsplit('#')
        .next()
        .unwrap()
        .to_string();
    registry.redeem(&code).unwrap();

    let call = app.phone_panel.as_mut().unwrap().list(resolve).unwrap();
    let event = agent.runtime.block_on(run_phone_call(call));
    let _ = app.update_event(event);
    let phones = panel(&app).phones.clone().unwrap();
    assert_eq!(phones.len(), 1);
    assert_eq!(
        (phones[0].name.as_str(), phones[0].scope.as_str()),
        ("Work phone", "act")
    );

    let call = app
        .phone_panel
        .as_mut()
        .unwrap()
        .revoke(phones[0].id.clone(), resolve)
        .unwrap();
    let event = agent.runtime.block_on(run_phone_call(call));
    assert!(matches!(&event, Event::PhoneRevoked(id, Ok(true)) if id == REMOTE));
    let _ = app.update_event(event);
    assert!(registry.list().is_empty());
    assert!(!panel(&app).busy && panel(&app).error.is_none());
}
