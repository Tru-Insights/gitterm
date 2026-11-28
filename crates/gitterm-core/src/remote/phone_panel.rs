//! The "Pair a phone" panel (TRU-151) for one remote agent: its state and
//! rules, shared by the desktop frontends.
//!
//! Frontends draw the panel and execute each [`PhoneCall`] against the
//! remote's agent (`list_phones`, `create_phone_pairing`, `revoke_phone`),
//! then report the answer to [`PhonePanel::phones_loaded`],
//! [`PhonePanel::pairing_created`] or [`PhonePanel::revoked`]. The agent
//! keeps the registry, token and scope authority; the panel only asks.
//!
//! A frontend resolves the agent client with its own lookup, passed to the
//! rules that start a call, so the core never sees credentials. `Q` is the
//! frontend's drawn QR code for an issued pairing (`()` without one).

/// A one-time phone pairing from `CreatePhonePairing`.
#[derive(Clone, PartialEq, Eq)]
pub struct PhonePairing {
    /// Open on the phone; the code is in its fragment.
    pub pairing_url: String,
    pub expires_unix_secs: u64,
    /// The scope the phone gets ("view" or "act").
    pub scope: String,
    /// The same pairing, typed into the phone's home-screen app
    /// (`XXXX-XXXX`). Empty from an agent that predates short codes.
    pub short_code: String,
}

/// A phone paired with an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPhoneInfo {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub created_unix_secs: u64,
    /// 0: not used since pairing.
    pub last_seen_unix_secs: u64,
}

/// The name field's starting value and placeholder.
pub const DEFAULT_DEVICE_NAME: &str = "iPhone";
pub const NAME_REQUIRED: &str = "Name the phone first.";
pub const NO_LONGER_PAIRED: &str = "That phone was no longer paired with the agent.";
pub const CREATE_LABEL: &str = "Show pairing code";
pub const RECREATE_LABEL: &str = "New code";

/// The scope a new pairing asks for: "act" when "Allow sending and
/// approving" is ticked, else "view".
pub fn requested_scope(allow_act: bool) -> &'static str {
    if allow_act {
        "act"
    } else {
        "view"
    }
}

/// How the panel names a phone scope from the agent.
pub fn scope_label(scope: &str) -> String {
    match scope {
        "view" => "view only".to_string(),
        "act" => "can send and approve".to_string(),
        other => other.to_string(),
    }
}

/// What a call asks the remote agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneRequest {
    /// `list_phones`.
    List,
    /// `create_phone_pairing(device_name, scope)`.
    Create {
        device_name: String,
        scope: &'static str,
    },
    /// `revoke_phone(id)`.
    Revoke { id: String },
}

/// One agent call for the frontend to run, with the client its lookup
/// resolved. The answer goes back with `remote_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneCall<C> {
    pub remote_id: String,
    pub request: PhoneRequest,
    pub client: C,
}

/// An issued pairing as the panel shows it.
#[derive(Clone)]
pub struct IssuedPairing<Q> {
    pub url: String,
    /// `XXXX-XXXX`; empty from an agent that predates short codes.
    pub short_code: String,
    /// The scope the agent granted ("view" or "act").
    pub scope: String,
    pub expires_unix_secs: u64,
    pub qr: Q,
}

/// The panel for one remote agent.
#[derive(Debug, Clone)]
pub struct PhonePanel<Q> {
    pub remote_id: String,
    pub machine_name: String,
    /// Shown in the paired-phones list.
    pub device_name: String,
    /// "Allow sending and approving": the next pairing gets act scope.
    /// Off by default; a phone already paired keeps its scope.
    pub allow_act: bool,
    pub pairing: Option<IssuedPairing<Q>>,
    /// `None` while loading.
    pub phones: Option<Vec<PairedPhoneInfo>>,
    /// A pairing or revoke is in flight.
    pub busy: bool,
    pub error: Option<String>,
}

impl<Q> PhonePanel<Q> {
    /// A fresh panel; follow it with [`Self::list`].
    pub fn open(remote_id: String, machine_name: String) -> Self {
        Self {
            remote_id,
            machine_name,
            device_name: DEFAULT_DEVICE_NAME.to_string(),
            allow_act: false,
            pairing: None,
            phones: None,
            busy: false,
            error: None,
        }
    }

    pub fn set_device_name(&mut self, value: String) {
        self.device_name = value;
    }

    pub fn set_allow_act(&mut self, allow: bool) {
        self.allow_act = allow;
    }

    /// The create button: a second code replaces the one shown.
    pub fn create_label(&self) -> &'static str {
        if self.pairing.is_some() {
            RECREATE_LABEL
        } else {
            CREATE_LABEL
        }
    }

    /// Load (or reload) the paired phones. Without a client the list is
    /// empty and the error says why.
    pub fn list<C>(
        &mut self,
        resolve: impl FnOnce(&str) -> Result<C, String>,
    ) -> Option<PhoneCall<C>> {
        match resolve(&self.remote_id) {
            Ok(client) => Some(self.call(PhoneRequest::List, client)),
            Err(error) => {
                self.error = Some(error);
                self.phones = Some(Vec::new());
                None
            }
        }
    }

    /// Ask for a pairing with the trimmed name and the ticked scope. Busy
    /// drops it; a blank name is refused before the client is looked up.
    pub fn create<C>(
        &mut self,
        resolve: impl FnOnce(&str) -> Result<C, String>,
    ) -> Option<PhoneCall<C>> {
        if self.busy {
            return None;
        }
        let device_name = self.device_name.trim().to_string();
        if device_name.is_empty() {
            self.error = Some(NAME_REQUIRED.to_string());
            return None;
        }
        let scope = requested_scope(self.allow_act);
        let client = self.start(resolve)?;
        Some(self.call(PhoneRequest::Create { device_name, scope }, client))
    }

    /// Revoke the phone `id`. The view disables Revoke while busy; this
    /// rule does not check it.
    pub fn revoke<C>(
        &mut self,
        id: String,
        resolve: impl FnOnce(&str) -> Result<C, String>,
    ) -> Option<PhoneCall<C>> {
        let client = self.start(resolve)?;
        Some(self.call(PhoneRequest::Revoke { id }, client))
    }

    /// The answer to a [`PhoneRequest::Create`]. `draw` turns the pairing
    /// URL into the frontend's QR code; a drawing failure is a failed
    /// pairing. Dropped unless the panel is still on `remote_id`.
    pub fn pairing_created(
        &mut self,
        remote_id: &str,
        result: Result<PhonePairing, String>,
        draw: impl FnOnce(&str) -> Result<Q, String>,
    ) {
        if !self.is_for(remote_id) {
            return;
        }
        self.busy = false;
        let issued = result.and_then(|pairing| {
            draw(&pairing.pairing_url).map(|qr| IssuedPairing {
                url: pairing.pairing_url,
                short_code: pairing.short_code,
                scope: pairing.scope,
                expires_unix_secs: pairing.expires_unix_secs,
                qr,
            })
        });
        match issued {
            Ok(issued) => {
                self.pairing = Some(issued);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("Could not create a pairing: {error}"));
            }
        }
    }

    /// The answer to a [`PhoneRequest::List`]. Dropped unless the panel is
    /// still on `remote_id`.
    pub fn phones_loaded(&mut self, remote_id: &str, result: Result<Vec<PairedPhoneInfo>, String>) {
        if !self.is_for(remote_id) {
            return;
        }
        match result {
            Ok(phones) => self.phones = Some(phones),
            Err(error) => {
                self.phones = Some(Vec::new());
                self.error = Some(format!("Could not list paired phones: {error}"));
            }
        }
    }

    /// The answer to a [`PhoneRequest::Revoke`]; every answer reloads the
    /// list. Dropped (no reload) unless the panel is still on `remote_id`.
    pub fn revoked<C>(
        &mut self,
        remote_id: &str,
        result: Result<bool, String>,
        resolve: impl FnOnce(&str) -> Result<C, String>,
    ) -> Option<PhoneCall<C>> {
        if !self.is_for(remote_id) {
            return None;
        }
        self.busy = false;
        match result {
            Ok(true) => {}
            Ok(false) => self.error = Some(NO_LONGER_PAIRED.to_string()),
            Err(error) => {
                self.error = Some(format!("Could not revoke: {error}"));
            }
        }
        self.list(resolve)
    }

    /// shortcut: the remote id is the only stale gate, so an answer from a
    /// panel closed and reopened on the same machine still applies (Iced's
    /// behaviour); add a panel generation if that ever matters.
    fn is_for(&self, remote_id: &str) -> bool {
        self.remote_id == remote_id
    }

    /// The client for a write: busy with the error cleared, or the lookup
    /// error shown and nothing started.
    fn start<C>(&mut self, resolve: impl FnOnce(&str) -> Result<C, String>) -> Option<C> {
        match resolve(&self.remote_id) {
            Ok(client) => {
                self.busy = true;
                self.error = None;
                Some(client)
            }
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }

    fn call<C>(&self, request: PhoneRequest, client: C) -> PhoneCall<C> {
        PhoneCall {
            remote_id: self.remote_id.clone(),
            request,
            client,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Panel = PhonePanel<()>;

    fn client(id: &str) -> Result<&'static str, String> {
        if id == "mac" {
            Ok("client")
        } else {
            Err(format!("no {id}"))
        }
    }

    fn pairing(scope: &str) -> PhonePairing {
        PhonePairing {
            pairing_url: "http://127.0.0.1:9/pair#c".into(),
            expires_unix_secs: 5,
            scope: scope.into(),
            short_code: "ABCD-EFGH".into(),
        }
    }

    #[test]
    fn create_sends_the_trimmed_name_and_the_ticked_scope() {
        let mut panel = Panel::open("mac".into(), "Mac".into());
        assert_eq!(panel.create_label(), CREATE_LABEL);
        panel.set_device_name("  Work phone ".into());
        for (allow, scope) in [(false, "view"), (true, "act")] {
            panel.busy = false;
            panel.set_allow_act(allow);
            let call = panel.create(client).unwrap();
            assert_eq!(
                call,
                PhoneCall {
                    remote_id: "mac".into(),
                    request: PhoneRequest::Create {
                        device_name: "Work phone".into(),
                        scope,
                    },
                    client: "client",
                }
            );
            assert!(panel.busy);
        }
        // Busy drops a second create without looking up a client.
        assert!(panel
            .create(|_| -> Result<(), String> { panic!("no lookup while busy") })
            .is_none());
    }

    #[test]
    fn a_blank_name_is_refused_before_the_lookup() {
        let mut panel = Panel::open("ghost".into(), "ghost".into());
        panel.set_device_name(" ".into());
        assert!(panel
            .create(|_| -> Result<(), String> { panic!("no lookup for a blank name") })
            .is_none());
        assert_eq!(panel.error.as_deref(), Some(NAME_REQUIRED));
        panel.set_device_name("iPhone".into());
        assert!(panel.create(client).is_none());
        assert_eq!(panel.error.as_deref(), Some("no ghost"));
        assert!(!panel.busy);
    }

    #[test]
    fn answers_for_another_machine_change_nothing() {
        let mut panel = Panel::open("mac".into(), "Mac".into());
        panel.create(client).unwrap();
        panel.pairing_created("other", Ok(pairing("act")), |_| Ok(()));
        panel.phones_loaded("other", Err("down".into()));
        assert!(panel.revoked("other", Ok(false), client).is_none());
        assert!(panel.busy && panel.pairing.is_none() && panel.phones.is_none());
        assert!(panel.error.is_none());
    }

    #[test]
    fn a_pairing_shows_and_a_drawing_failure_is_an_error() {
        let mut panel = Panel::open("mac".into(), "Mac".into());
        panel.create(client).unwrap();
        panel.pairing_created("mac", Ok(pairing("act")), |url| {
            assert_eq!(url, "http://127.0.0.1:9/pair#c");
            Ok(())
        });
        let shown = panel.pairing.as_ref().unwrap();
        assert_eq!(
            (shown.scope.as_str(), shown.short_code.as_str()),
            ("act", "ABCD-EFGH")
        );
        assert_eq!(panel.create_label(), RECREATE_LABEL);
        panel.create(client).unwrap();
        panel.pairing_created("mac", Ok(pairing("view")), |_| Err("no QR".into()));
        assert!(!panel.busy);
        assert_eq!(
            panel.error.as_deref(),
            Some("Could not create a pairing: no QR")
        );
        assert_eq!(panel.pairing.as_ref().unwrap().scope, "act");
    }

    #[test]
    fn every_revoke_answer_reloads_the_list() {
        let mut panel = Panel::open("mac".into(), "Mac".into());
        let call = panel.revoke("p1".into(), client).unwrap();
        assert_eq!(call.request, PhoneRequest::Revoke { id: "p1".into() });
        for (answer, error) in [
            (Ok(true), None),
            (Ok(false), Some(NO_LONGER_PAIRED.to_string())),
            (Err("x".into()), Some("Could not revoke: x".to_string())),
        ] {
            panel.error = None;
            let reload = panel.revoked("mac", answer, client).unwrap();
            assert_eq!(reload.request, PhoneRequest::List);
            assert_eq!(panel.error, error);
            assert!(!panel.busy);
        }
    }

    #[test]
    fn scopes_are_named_for_people() {
        assert_eq!(scope_label("view"), "view only");
        assert_eq!(scope_label("act"), "can send and approve");
        assert_eq!(scope_label("admin"), "admin");
        assert_eq!(requested_scope(false), "view");
        assert_eq!(requested_scope(true), "act");
    }
}

impl std::fmt::Debug for PhonePairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhonePairing")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}
impl<Q> std::fmt::Debug for IssuedPairing<Q> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedPairing")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

pub const PHONE_QR_SCALE: usize = 6;
pub const PHONE_QR_QUIET: usize = 4;
/// RGBA pixels from Iced's phone_pairing_qr, including the quiet zone.
pub struct PairingQr {
    pub side: u32,
    pub pixels: Vec<u8>,
}
pub fn pairing_qr(url: &str) -> Result<PairingQr, String> {
    let code = qrcode::QrCode::new(url.as_bytes())
        .map_err(|err| format!("could not draw the pairing QR code: {err}"))?;
    let modules = code.width();
    let side = (modules + 2 * PHONE_QR_QUIET) * PHONE_QR_SCALE;
    let mut pixels = vec![255u8; side * side * 4];
    for (index, color) in code.to_colors().iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (mx, my) = (index % modules, index / modules);
        for dy in 0..PHONE_QR_SCALE {
            let y = (my + PHONE_QR_QUIET) * PHONE_QR_SCALE + dy;
            for dx in 0..PHONE_QR_SCALE {
                let x = (mx + PHONE_QR_QUIET) * PHONE_QR_SCALE + dx;
                let at = (y * side + x) * 4;
                pixels[at..at + 3].fill(0);
            }
        }
    }
    Ok(PairingQr {
        side: side as u32,
        pixels,
    })
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    #[test]
    fn parity_phone_qr_pixels_match_iced_and_debug_redacts_codes() {
        let url = "http://fixture.invalid/pair#placeholder-pairing";
        let code = pairing_qr(url).unwrap();
        let side = code.side as usize;
        let quiet = PHONE_QR_QUIET * PHONE_QR_SCALE;
        assert_eq!(code.pixels.len(), side * side * 4);
        assert_eq!(&code.pixels[..4], &[255; 4]);
        assert_eq!(
            &code.pixels[(quiet * side + quiet) * 4..(quiet * side + quiet) * 4 + 4],
            &[0, 0, 0, 255]
        );
        let pairing = PhonePairing {
            pairing_url: url.into(),
            expires_unix_secs: 1000,
            scope: "view".into(),
            short_code: "PLACEHOLDER-CODE".into(),
        };
        let debug = format!("{pairing:?}");
        assert!(!debug.contains("placeholder-pairing") && !debug.contains("PLACEHOLDER-CODE"));
        let mut panel = PhonePanel::<()>::open("placeholder".into(), "Example".into());
        panel.pairing_created("placeholder", Ok(pairing), |_| Ok(()));
        let debug = format!("{:?}", panel.pairing);
        assert!(!debug.contains("placeholder-pairing") && !debug.contains("PLACEHOLDER-CODE"));
    }
}
