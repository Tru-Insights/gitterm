use super::*;

fn target(id: &str, endpoint: &str) -> Target {
    Target {
        identity: Identity::new(id.into(), endpoint.into()).unwrap(),
        name: format!("Name {id}"),
    }
}
fn registered() -> State {
    let mut state = State::default();
    assert_eq!(
        state.reduce(Intent::Register(target("a", "http://a.invalid:1"))),
        vec![Effect::Render]
    );
    state
}
fn ensure(state: &mut State, id: &str, force: bool, now: Instant) -> Stamp {
    let effects = state.reduce(Intent::Ensure {
        remote_id: id.into(),
        force,
        now,
    });
    let [Effect::Render, Effect::Handshake(stamp)] = effects.as_slice() else {
        panic!("expected handshake, got {effects:?}");
    };
    stamp.clone()
}
fn reply(stamp: &Stamp, capabilities: &[&str]) -> RemoteAgentHandshake {
    RemoteAgentHandshake {
        remote_id: stamp.identity.remote_id().into(),
        endpoint: stamp.identity.endpoint().into(),
        agent_name: "agent".into(),
        agent_version: "v-next".into(),
        // The existing client does NOT reject response versions or normalize caps.
        protocol_version: 97,
        capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        home_dir: "Q:/opaque/../home\\folder".into(),
    }
}
fn finish(
    state: &mut State,
    stamp: &Stamp,
    result: Result<RemoteAgentHandshake, HandshakeFailed>,
    now: Instant,
) -> Vec<Effect> {
    state.reduce(Intent::HandshakeFinished {
        stamp: stamp.clone(),
        result,
        now,
    })
}

#[test]
fn defaults_ensure_force_and_fresh_reconnect_capabilities() {
    let mut state = registered();
    let now = Instant::now();
    let conn = state.connection("a").unwrap();
    assert_eq!(conn.status, Status::Disconnected);
    assert!(conn.last_attempt.is_none() && conn.last_connected.is_none());
    let first = ensure(&mut state, "a", false, now);
    assert!(state.accepts(&first));
    assert!(state
        .reduce(Intent::Ensure {
            remote_id: "a".into(),
            force: false,
            now
        })
        .is_empty());
    let h = reply(&first, &["claude_chat", "future", "future"]);
    assert_eq!(
        finish(&mut state, &first, Ok(h.clone()), now),
        vec![
            Effect::Render,
            Effect::Completed {
                stamp: first.clone(),
                connected: true
            }
        ]
    );
    let conn = state.connection("a").unwrap();
    assert_eq!(conn.handshake(), Some(&h));
    assert!(conn.has_capability("claude_chat"));
    assert!(!conn.has_capability("claude"));
    assert_eq!(conn.last_connected, Some(now));
    assert_eq!(conn.last_attempt, Some(now));
    assert!(!state.accepts(&first));
    assert!(state.is_current(&first));
    assert!(state
        .reduce(Intent::Ensure {
            remote_id: "a".into(),
            force: false,
            now
        })
        .is_empty());
    assert!(state
        .reduce(Intent::Register(target("a", "http://a.invalid:1")))
        .is_empty());
    let second = ensure(&mut state, "a", true, now);
    let conn = state.connection("a").unwrap();
    assert_eq!(conn.status, Status::Connecting);
    assert!(conn.home().is_none());
    assert!(conn.last_connected.is_none());
    assert!(!conn.has_capability("claude_chat"));
    assert!(!state.is_current(&first));
    let fresh = reply(&second, &["phone", "new"]);
    finish(&mut state, &second, Ok(fresh.clone()), now);
    let conn = state.connection("a").unwrap();
    assert_eq!(conn.handshake(), Some(&fresh));
    assert!(!conn.has_capability("claude_chat"));
    assert!(conn.has_capability("phone"));
}

#[test]
fn stale_success_failure_and_duplicates_have_no_effects() {
    let mut state = registered();
    let now = Instant::now();
    let first = ensure(&mut state, "a", false, now);
    let second = ensure(&mut state, "a", true, now);
    for result in [Ok(reply(&first, &["stale"])), Err(HandshakeFailed)] {
        assert!(finish(
            &mut state,
            &first,
            result,
            now + std::time::Duration::from_secs(9)
        )
        .is_empty());
        assert_eq!(state.connection("a").unwrap().last_attempt, Some(now));
        assert!(state.accepts(&second));
        assert_eq!(state.connection("a").unwrap().status, Status::Connecting);
    }
    finish(&mut state, &second, Ok(reply(&second, &["current"])), now);
    for result in [Ok(reply(&second, &["duplicate"])), Err(HandshakeFailed)] {
        assert!(finish(&mut state, &second, result, now).is_empty());
        assert!(state.connection("a").unwrap().has_capability("current"));
    }
}

#[test]
fn wrong_identity_and_endpoint_leave_legitimate_attempt_outstanding() {
    let mut state = registered();
    let now = Instant::now();
    let stamp = ensure(&mut state, "a", false, now);
    for (id, endpoint) in [("b", "http://a.invalid:1"), ("a", "http://b.invalid:1")] {
        let mut wrong = reply(&stamp, &["wrong"]);
        wrong.remote_id = id.into();
        wrong.endpoint = endpoint.into();
        assert!(finish(&mut state, &stamp, Ok(wrong), now).is_empty());
        assert!(state.accepts(&stamp));
    }
    let mut forged = stamp.clone();
    forged.identity = target("b", "http://b.invalid:1").identity;
    assert!(finish(&mut state, &forged, Err(HandshakeFailed), now).is_empty());
    assert!(state.accepts(&stamp));
    assert_eq!(
        finish(&mut state, &stamp, Ok(reply(&stamp, &[])), now),
        vec![
            Effect::Render,
            Effect::Completed {
                stamp,
                connected: true
            }
        ]
    );
}

#[test]
fn registration_conflict_is_atomic_and_remove_cannot_rebind() {
    let mut state = registered();
    let now = Instant::now();
    let stamp = ensure(&mut state, "a", false, now);
    let conflict = target("a", "https://other.invalid");
    assert_eq!(
        state.reduce(Intent::Register(conflict.clone())),
        vec![Effect::Rejected {
            remote_id: "a".into(),
            reason: Failure::IdentityChanged
        }]
    );
    assert!(state.accepts(&stamp));
    assert_eq!(
        state.connection("a").unwrap().target,
        target("a", "http://a.invalid:1")
    );
    let mut renamed = target("a", "http://a.invalid:1");
    renamed.name = "new display name".into();
    state.reduce(Intent::Register(renamed));
    assert!(state.accepts(&stamp));
    state.reduce(Intent::Remove {
        remote_id: "a".into(),
    });
    assert!(state.connection("a").is_none());
    assert!(!state.is_current(&stamp));
    assert!(matches!(
        state.reduce(Intent::Register(conflict)).as_slice(),
        [Effect::Rejected {
            reason: Failure::IdentityChanged,
            ..
        }]
    ));
    assert!(state.connection("a").is_none());
    state.reduce(Intent::Register(target("a", "http://a.invalid:1")));
    let fresh = ensure(&mut state, "a", false, now);
    assert_ne!(fresh, stamp);
    assert!(finish(&mut state, &stamp, Ok(reply(&stamp, &[])), now).is_empty());
    assert!(finish(&mut state, &stamp, Err(HandshakeFailed), now).is_empty());
    assert!(state.accepts(&fresh));
}

#[test]
fn disconnected_or_recreated_states_reject_old_completions() {
    let mut state = registered();
    let now = Instant::now();
    let stamp = ensure(&mut state, "a", false, now);
    state.reduce(Intent::Disconnect {
        remote_id: "a".into(),
    });
    assert!(!state.is_current(&stamp));
    assert!(finish(&mut state, &stamp, Err(HandshakeFailed), now).is_empty());
    assert!(finish(&mut state, &stamp, Ok(reply(&stamp, &[])), now).is_empty());
    assert_eq!(state.connection("a").unwrap().status, Status::Disconnected);
    let mut replacement = registered();
    let fresh = ensure(&mut replacement, "a", false, now);
    assert_ne!(stamp, fresh);
    assert!(finish(&mut replacement, &stamp, Ok(reply(&stamp, &[])), now).is_empty());
    assert!(finish(&mut replacement, &stamp, Err(HandshakeFailed), now).is_empty());
    assert!(replacement.accepts(&fresh));
}

#[test]
fn failure_notifies_completion_without_capabilities_and_can_retry() {
    let mut state = registered();
    let now = Instant::now();
    let later = now + std::time::Duration::from_secs(3);
    let stamp = ensure(&mut state, "a", false, now);
    assert_eq!(
        finish(&mut state, &stamp, Err(HandshakeFailed), later),
        vec![
            Effect::Render,
            Effect::Completed {
                stamp: stamp.clone(),
                connected: false
            }
        ]
    );
    let conn = state.connection("a").unwrap();
    assert_eq!(conn.status, Status::Error(Failure::HandshakeFailed));
    assert_eq!(conn.last_attempt, Some(later));
    assert!(conn.last_connected.is_none() && conn.home().is_none());
    assert!(!state.accepts(&stamp));
    let retry = ensure(&mut state, "a", false, later);
    assert_ne!(stamp, retry);
    assert_eq!(
        state.reduce(Intent::Ensure {
            remote_id: "missing".into(),
            force: false,
            now
        }),
        vec![Effect::Rejected {
            remote_id: "missing".into(),
            reason: Failure::NotConfigured
        }]
    );
}

#[test]
fn independent_remotes_cannot_retarget_each_others_results_or_paths() {
    let mut state = registered();
    state.reduce(Intent::Register(target("b", "http://a.invalid:1")));
    let now = Instant::now();
    let a = ensure(&mut state, "a", false, now);
    let b = ensure(&mut state, "b", false, now);
    finish(&mut state, &a, Ok(reply(&a, &["only-a"])), now);
    assert!(state.accepts(&b));
    assert!(!state.connection("b").unwrap().has_capability("only-a"));
    let home = state.connection("a").unwrap().home().unwrap();
    assert_eq!(home, SourcePath::Remote("Q:/opaque/../home\\folder".into()));
    assert!(home.as_local().is_none());
    let mut empty = reply(&b, &[]);
    empty.home_dir.clear();
    finish(&mut state, &b, Ok(empty), now);
    assert!(state.connection("b").unwrap().home().is_none());
}

#[test]
fn credential_bearing_or_invalid_endpoints_never_enter_state() {
    for endpoint in [
        "http://user:secret@host",
        "http://host?token=secret",
        "http://host#secret",
        "file:///private/tmp/test",
        "not a URL",
        "http://",
    ] {
        assert_eq!(
            Identity::new("remote".into(), endpoint.into()),
            Err(Failure::InvalidIdentity)
        );
    }
    assert_eq!(
        Identity::new(" ".into(), "http://host".into()),
        Err(Failure::InvalidIdentity)
    );
    let state = registered();
    let debug = format!("{state:?}");
    assert!(!debug.contains("token_ref"));
    assert!(!debug.contains("secret"));
    assert_eq!(
        Identity::new("remote".into(), "https://HOST:1234/".into())
            .unwrap()
            .endpoint(),
        "https://HOST:1234/"
    );
}
