//! The phone pairing panel on GPUI's test platform, opened through the
//! workspace bar like the app does. Nothing here has a network or an Iced
//! write to reach: the fake remote client records any call, and the Iced
//! fixture dir must stay byte-identical.
use super::*;
use crate::{
    remote_shell_tests::{Fixture, bar, click, draw, fixture, snapshot},
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

/// A phone-serving, connected machine, so the bar menu offers the entry.
fn serve_phones(f: &Fixture, cx: &mut VisualTestContext) {
    f.shell.update(cx, |shell, cx| {
        let mut facts = shell.saved_facts.snapshot();
        facts.repo_machines = vec![workspace_bar::Machine {
            key: "agent:mini".into(),
            name: "Mini".into(),
            status: None,
            connected: true,
            serves_phones: true,
        }];
        shell.saved_facts.replace(facts);
        cx.notify();
    });
    draw(cx);
}

fn assert_untouched(f: &Fixture, before: &[(std::path::PathBuf, Vec<u8>)]) {
    assert!(f.fake.calls().is_empty(), "{:?}", f.fake.calls());
    assert_eq!(snapshot(&f.iced()), before);
    assert!(!f.iced().join("tokens").exists());
}

#[gpui::test]
fn the_pair_a_phone_menu_entry_opens_the_panel(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    serve_phones(&f, cx);
    let before = snapshot(&f.iced());
    assert!(cx.debug_bounds("phone-panel").is_none());
    click("pstrip-repos", cx);
    // Mini follows this Mac (and any other section); its second action is the entry.
    let entry = (1..4)
        .map(|position| format!("pstrip-repo-action-{position}-1"))
        .find(|selector| {
            cx.debug_bounds(selector.as_str().to_owned().leak())
                .is_some()
        })
        .expect("a machine menu with a second action");
    click(&entry, cx);
    assert!(cx.debug_bounds("phone-panel").is_some());
    assert!(cx.debug_bounds("phone-title").is_some());
    // The entry opens the panel instead of reporting a preview notice.
    assert_eq!(shell_notice(&f, cx), None);
    let opened = panel(&f, cx);
    cx.read(|cx| assert_eq!(opened.read(cx).panel.machine_name, "Mini"));
    assert_untouched(&f, &before);
}

#[gpui::test]
fn labels_and_defaults_match_iced(cx: &mut TestAppContext) {
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
        // No agent is contacted, so there is no list; Iced's panel has one.
        "phone-list-unavailable",
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
    assert!(cx.debug_bounds("phone-list-unavailable").is_none());
}

#[gpui::test]
fn the_name_field_and_checkbox_are_local_only(cx: &mut TestAppContext) {
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
fn create_enter_and_revoke_only_show_the_preview_notice(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx);
    let before = snapshot(&f.iced());
    let panel = open(&f, cx);
    panel.update(cx, |panel, cx| {
        panel.phones_loaded("mini", Ok(fixture_phones()), cx)
    });
    draw(cx);
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
    click("phone-create", cx);
    both("New code", cx);

    click("phone-revoke-p1", cx);
    both("Revoke", cx);
    // Nothing was granted or revoked: the pairing and the list are as given.
    cx.read(|cx| {
        assert!(panel.read(cx).panel.pairing.is_some());
        assert_eq!(panel.read(cx).panel.phones, Some(fixture_phones()));
    });
    assert!(panel_is_open(&f, cx));
    assert_untouched(&f, &before);
}

#[gpui::test]
fn busy_disables_every_write_control(cx: &mut TestAppContext) {
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
    let (f, cx) = fixture(cx);
    open(&f, cx);
    click("phone-close", cx);
    assert!(!panel_is_open(&f, cx));
    assert!(cx.debug_bounds("phone-panel").is_none());
}
