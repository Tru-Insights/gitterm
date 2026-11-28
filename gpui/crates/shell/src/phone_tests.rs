//! The phone pairing panel on GPUI's test platform, opened through the
//! workspace bar like the app does. Nothing here has a network or an Iced
//! write to reach: the fake remote client records any call, and the Iced
//! fixture dir must stay byte-identical.
use super::*;
use crate::{
    remote_shell_tests::{FakeRemote, Fixture, bar, click, draw, fixture, fixture_with, snapshot},
    workspace_bar,
};
use gitterm_core::remote::phone_panel::{CREATE_LABEL, RECREATE_LABEL};
use gpui::{TestAppContext, VisualTestContext};

fn fixture_phones() -> Vec<PairedPhoneInfo> {
    vec![PairedPhoneInfo {
        id: "p1".into(),
        name: "Fixture phone".into(),
        scope: "view".into(),
        created_unix_secs: 1,
        last_seen_unix_secs: 0,
    }]
}

fn fixture_pairing(short_code: &str) -> PhonePairing {
    PhonePairing {
        pairing_url: "http://fixture.invalid/pair?t=abc".into(),
        short_code: short_code.into(),
        scope: "view".into(),
        expires_unix_secs: 1_000,
    }
}

/// A fake agent that serves phones and lists `phones`.
fn serving(phones: Result<Vec<PairedPhoneInfo>, String>) -> FakeRemote {
    let mut fake = FakeRemote::default();
    fake.capabilities = vec!["handshake".into(), "phone".into()];
    fake.phones = Some(phones);
    fake
}

fn open(f: &Fixture, cx: &mut VisualTestContext) -> Entity<PhonePanel> {
    bar(
        f,
        workspace_bar::Outcome::PairPhone("agent:mini".into()),
        cx,
    );
    panel(f, cx)
}

fn panel(f: &Fixture, cx: &mut VisualTestContext) -> Entity<PhonePanel> {
    cx.read(|cx| f.shell.read(cx).phone_panel().cloned())
        .expect("phone panel open")
}

fn shell_notice(f: &Fixture, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| f.shell.read(cx).owner.read(cx).error.clone())
}

/// The only agent calls: the bar's handshake with Mini, then one list as
/// the panel opened.
fn assert_untouched(f: &Fixture, before: &[(std::path::PathBuf, Vec<u8>)]) {
    assert_eq!(f.fake.calls(), ["handshake mini", "list_phones mini"]);
    assert_eq!(snapshot(&f.iced()), before);
    assert!(!f.iced().join("tokens").exists());
}

/// The machine menus' second actions: Mini's is "Pair a phone…" only when
/// its agent serves phones ("Show Mini chats" is first).
fn second_actions(cx: &mut VisualTestContext) -> Vec<String> {
    (1..4)
        .map(|position| format!("pstrip-repo-action-{position}-1"))
        .filter(|selector| {
            cx.debug_bounds(selector.as_str().to_owned().leak())
                .is_some()
        })
        .collect()
}

fn machine(f: &Fixture, cx: &mut VisualTestContext) -> workspace_bar::Machine {
    cx.update(|_, cx| {
        f.shell
            .read(cx)
            .bar_input(cx)
            .repo_machines
            .into_iter()
            .find(|machine| machine.key == "agent:mini")
            .expect("Mini on the bar")
    })
}

#[gpui::test]
fn phone_panel_opens_when_machine_serves_phones(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture_with(serving(Ok(fixture_phones())), cx);
    let before = snapshot(&f.iced());
    // The bar handshook Mini as Iced connects it, and nothing else ran.
    assert_eq!(f.fake.calls(), ["handshake mini"]);
    let mini = machine(&f, cx);
    assert!(mini.connected && mini.serves_phones);
    assert_eq!(mini.status.map(|s| s.label).as_deref(), Some("connected"));
    assert!(cx.debug_bounds("phone-panel").is_none());
    click("pstrip-repos", cx);
    let entries = second_actions(cx);
    assert_eq!(entries.len(), 1, "{entries:?}");
    click(&entries[0], cx);
    assert!(cx.debug_bounds("phone-panel").is_some());
    assert!(cx.debug_bounds("phone-title").is_some());
    // The entry opens the panel instead of reporting a preview notice.
    assert_eq!(shell_notice(&f, cx), None);
    let opened = panel(&f, cx);
    cx.read(|cx| assert_eq!(opened.read(cx).panel.machine_name, "Mini"));
    assert_untouched(&f, &before);
}

#[gpui::test]
fn no_pair_a_phone_entry_when_the_agent_does_not_serve_phones(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    let mini = machine(&f, cx);
    assert!(mini.connected && !mini.serves_phones);
    click("pstrip-repos", cx);
    assert_eq!(second_actions(cx), Vec::<String>::new());
    assert_eq!(f.fake.calls(), ["handshake mini"]);
}

#[gpui::test]
fn an_unreachable_agent_shows_iced_word_and_no_entry(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    // The default test client fails every call, as an agent that is down.
    let (f, cx) = fixture(cx);
    f.shell.update(cx, |shell, cx| {
        shell.set_remote_client(std::sync::Arc::new(crate::remote_client::Offline));
        shell.remote.connections.clear();
        shell.connect_remotes(cx);
    });
    draw(cx);
    let mini = machine(&f, cx);
    assert!(!mini.connected && !mini.serves_phones);
    assert_eq!(mini.status.map(|s| s.label).as_deref(), Some("unreachable"));
}

#[gpui::test]
fn phone_list_runs_once_on_open_and_shows_rows(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture_with(serving(Ok(fixture_phones())), cx);
    let before = snapshot(&f.iced());
    let panel = open(&f, cx);
    cx.read(|cx| {
        let panel = panel.read(cx);
        assert_eq!(panel.panel.phones, Some(fixture_phones()));
        assert_eq!(panel.notice(), None);
    });
    assert!(cx.debug_bounds("phone-revoke-p1").is_some());
    assert!(cx.debug_bounds("phone-list-loading").is_none());
    assert!(cx.debug_bounds("phone-list-empty").is_none());
    // Redrawing and local edits list nothing more.
    click("phone-allow-act", cx);
    draw(cx);
    assert_eq!(shell_notice(&f, cx), None);
    assert_untouched(&f, &before);
}

#[gpui::test]
fn phone_list_error_shows_iced_text(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture_with(serving(Err("list_phones failed: agent said no".into())), cx);
    let before = snapshot(&f.iced());
    let panel = open(&f, cx);
    cx.read(|cx| {
        assert_eq!(
            panel.read(cx).notice(),
            Some("Could not list paired phones: list_phones failed: agent said no")
        )
    });
    assert!(cx.debug_bounds("phone-notice").is_some());
    // Iced shows the empty list under the error.
    assert!(cx.debug_bounds("phone-list-empty").is_some());
    assert!(cx.debug_bounds("phone-list-loading").is_none());
    // A failed read is the panel's error, not a shell notice.
    assert_eq!(shell_notice(&f, cx), None);
    assert_untouched(&f, &before);
}

#[gpui::test]
fn the_list_shows_iced_loading_text_until_it_answers(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    let panel = open(&f, cx);
    assert_eq!(LOADING, "Loading…");
    assert_eq!(NO_PHONES, "None yet.");
    panel.update(cx, |panel, cx| {
        panel.panel.phones = None;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("phone-list-loading").is_some());
    panel.update(cx, |panel, cx| panel.phones_loaded("mini", Ok(vec![]), cx));
    draw(cx);
    assert!(cx.debug_bounds("phone-list-loading").is_none());
    assert!(cx.debug_bounds("phone-list-empty").is_some());

    // A list the gate refuses does not stay "Loading…".
    panel.update(cx, |panel, cx| {
        panel.panel.phones = None;
        panel.refused("Paired phones · not available in preview".into(), cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("phone-list-loading").is_none());
    assert!(cx.debug_bounds("phone-list-empty").is_some());
    assert!(cx.debug_bounds("phone-notice").is_some());
}

#[gpui::test]
fn labels_and_defaults_match_iced(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    let panel = open(&f, cx);
    assert_eq!(CREATE_LABEL, "Show pairing code");
    assert_eq!(RECREATE_LABEL, "New code");
    assert_eq!(ALLOW_ACT_LABEL, "Allow sending and approving");
    assert_eq!(REVOKE_LABEL, "Revoke");
    assert_eq!(DEFAULT_DEVICE_NAME, "iPhone");
    cx.read(|cx| {
        let panel = panel.read(cx);
        assert_eq!(panel.panel.machine_name, "Mini");
        assert_eq!(panel.device_name(cx), "iPhone");
        assert!(!panel.allow_act());
        assert_eq!(panel.create_label(), "Show pairing code");
        assert_eq!(panel.notice(), None);
    });
    for selector in [
        "phone-panel-card",
        "phone-name",
        "phone-create",
        "phone-allow-act",
        "phone-close",
        // The fake agent lists no phones.
        "phone-list-empty",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    // No pairing yet: no short-code row, no URL.
    assert!(cx.debug_bounds("phone-short-code").is_none());
    assert!(cx.debug_bounds("phone-pairing-url").is_none());

    panel.update(cx, |panel, cx| {
        panel.pairing_created("mini", Ok(fixture_pairing("ABCD-1234")), cx)
    });
    draw(cx);
    cx.read(|cx| assert_eq!(panel.read(cx).create_label(), "New code"));
    assert!(cx.debug_bounds("phone-short-code").is_some());
    assert!(cx.debug_bounds("phone-pairing-url").is_some());

    // An agent that predates short codes: the row is absent, the rest stays.
    panel.update(cx, |panel, cx| {
        panel.pairing_created("mini", Ok(fixture_pairing("")), cx)
    });
    draw(cx);
    assert!(cx.debug_bounds("phone-short-code").is_none());
    assert!(cx.debug_bounds("phone-pairing-url").is_some());

    panel.update(cx, |panel, cx| {
        panel.phones_loaded("mini", Ok(fixture_phones()), cx)
    });
    draw(cx);
    assert!(cx.debug_bounds("phone-revoke-p1").is_some());
    assert!(cx.debug_bounds("phone-list-empty").is_none());
}

#[gpui::test]
fn the_name_field_and_checkbox_are_local_only(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    let before = snapshot(&f.iced());
    let panel = open(&f, cx);
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("Pixel");
    draw(cx);
    cx.read(|cx| assert_eq!(panel.read(cx).device_name(cx), "Pixel"));
    click("phone-allow-act", cx);
    cx.read(|cx| assert!(panel.read(cx).allow_act()));
    click("phone-allow-act", cx);
    cx.read(|cx| assert!(!panel.read(cx).allow_act()));
    click("phone-allow-act", cx);
    cx.read(|cx| {
        assert!(panel.read(cx).allow_act());
        assert_eq!(panel.read(cx).notice(), None);
    });
    assert_eq!(shell_notice(&f, cx), None);
    assert_untouched(&f, &before);
}

#[gpui::test]
fn phone_create_and_revoke_refused_without_calls(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture_with(serving(Ok(fixture_phones())), cx);
    let before = snapshot(&f.iced());
    let panel = open(&f, cx);
    let both = |label: &str, cx: &mut VisualTestContext| {
        let expected = format!("{label} · not available in preview");
        cx.read(|cx| assert_eq!(panel.read(cx).notice(), Some(expected.as_str())));
        assert_eq!(shell_notice(&f, cx), Some(expected));
    };

    click("phone-create", cx);
    both("Show pairing code", cx);
    f.shell.update(cx, |shell, cx| {
        shell.owner.update(cx, |o, cx| {
            o.error = None;
            cx.notify()
        })
    });

    // Enter in the name field is the same control as the button.
    cx.simulate_keystrokes("enter");
    draw(cx);
    both("Show pairing code", cx);
    f.shell.update(cx, |shell, cx| {
        shell.owner.update(cx, |o, cx| {
            o.error = None;
            cx.notify()
        })
    });

    panel.update(cx, |panel, cx| {
        panel.pairing_created("mini", Ok(fixture_pairing("ABCD-1234")), cx)
    });
    draw(cx);
    // The gate names a second code as it names the first.
    click("phone-create", cx);
    both("Show pairing code", cx);

    click("phone-revoke-p1", cx);
    both("Revoke", cx);
    // Nothing was granted or revoked, and nothing is left in flight.
    cx.read(|cx| {
        assert!(panel.read(cx).panel.pairing.is_some());
        assert_eq!(panel.read(cx).panel.phones, Some(fixture_phones()));
        assert!(!panel.read(cx).panel.busy);
    });
    assert!(panel_is_open(&f, cx));
    assert_untouched(&f, &before);
}

#[gpui::test]
fn busy_disables_every_write_control(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    let panel = open(&f, cx);
    panel.update(cx, |panel, cx| {
        panel.phones_loaded("mini", Ok(fixture_phones()), cx);
        panel.set_busy(true, cx);
    });
    draw(cx);
    click("phone-create", cx);
    click("phone-revoke-p1", cx);
    click("phone-allow-act", cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    cx.read(|cx| {
        assert_eq!(panel.read(cx).notice(), None);
        assert!(!panel.read(cx).allow_act());
    });
    assert_eq!(shell_notice(&f, cx), None);
    // Close still works while busy, as in Iced.
    click("phone-close", cx);
    assert!(!panel_is_open(&f, cx));
}

fn panel_is_open(f: &Fixture, cx: &mut VisualTestContext) -> bool {
    cx.read(|cx| f.shell.read(cx).phone_panel().is_some())
}

#[gpui::test]
fn escape_closes_the_panel(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    let before = snapshot(&f.iced());
    open(&f, cx);
    assert!(panel_is_open(&f, cx));
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(!panel_is_open(&f, cx));
    assert!(cx.debug_bounds("phone-panel").is_none());
    assert_eq!(shell_notice(&f, cx), None);
    assert_untouched(&f, &before);
}

#[gpui::test]
fn close_closes_the_panel(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = fixture(cx);
    open(&f, cx);
    click("phone-close", cx);
    assert!(!panel_is_open(&f, cx));
    assert!(cx.debug_bounds("phone-panel").is_none());
}
