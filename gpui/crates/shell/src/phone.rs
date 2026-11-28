//! Iced's phone panel, using core's state, agent calls and QR pixels.

use std::{
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use gitterm_core::{
    chats,
    remote::phone_panel::{self, DEFAULT_DEVICE_NAME, PairedPhoneInfo, PhonePairing, scope_label},
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, rgb, rgba,
};
use gpui_component::{
    Disableable as _, Sizable as _,
    button::Button,
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};

use crate::theme::Palette;

#[cfg(test)]
#[path = "phone_tests.rs"]
mod tests;

const MONO: &str = "Menlo";
pub const REVOKE_LABEL: &str = "Revoke";
pub const ALLOW_ACT_LABEL: &str = "Allow sending and approving";
pub const EXPLANATION: &str = "Scan the code with the phone's camera; the phone must be on the \
     same tailnet. It can view this machine's chats; allowed to send, \
     it can also message them, answer their prompts and stop a turn. \
     To change what a phone may do, revoke it and pair it again.";
pub const SHORT_CODE_HINT: &str = "Using the home-screen app? Open it and type this code.";
/// Iced's texts while the list loads and when it is empty.
pub const LOADING: &str = "Loading…";
pub const NO_PHONES: &str = "None yet.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhonePanelOutcome {
    Close,
    /// Show pairing code / New code, or Enter in the name field; the device
    /// name is already copied into the rules.
    Create,
    Revoke(String),
}

/// `HH:MM` on this Mac's clock, as Iced's `local_clock_time`.
fn local_clock_time(unix_secs: u64) -> String {
    chrono::DateTime::from_timestamp(unix_secs as i64, 0)
        .map(|at| at.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| unix_secs.to_string())
}

fn seen_text(last_seen_unix_secs: u64) -> String {
    if last_seen_unix_secs == 0 {
        "never used".to_string()
    } else {
        format!(
            "seen {} ago",
            chats::format_age(UNIX_EPOCH + Duration::from_secs(last_seen_unix_secs))
        )
    }
}

pub(crate) type PairingImage = Arc<gpui::RenderImage>;

pub struct PhonePanel {
    pub(crate) list_generation: u64,
    /// Iced's rules and state; the name field's text lives in `name` and is
    /// copied over when a pairing is asked for.
    pub panel: phone_panel::PhonePanel<PairingImage>,
    name: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PhonePanelOutcome> for PhonePanel {}

impl PhonePanel {
    /// Escape, also while a terminal behind the panel holds focus.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(PhonePanelOutcome::Close);
    }
    pub fn new(
        remote_id: String,
        machine_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder(DEFAULT_DEVICE_NAME);
            input.set_value(DEFAULT_DEVICE_NAME, window, cx);
            input.focus(window, cx);
            input
        });
        let subscription = cx.subscribe(&name, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.create(cx);
            }
        });
        Self {
            panel: phone_panel::PhonePanel::open(remote_id, machine_name),
            list_generation: 0,
            name,
            _subscriptions: vec![subscription],
        }
    }

    pub fn device_name(&self, cx: &App) -> String {
        self.name.read(cx).value().to_string()
    }

    pub fn allow_act(&self) -> bool {
        self.panel.allow_act
    }

    pub fn notice(&self) -> Option<&str> {
        self.panel.error.as_deref()
    }

    pub fn create_label(&self) -> &'static str {
        self.panel.create_label()
    }

    /// The answer to a list call.
    pub fn phones_loaded(
        &mut self,
        remote_id: &str,
        result: Result<Vec<PairedPhoneInfo>, String>,
        cx: &mut Context<Self>,
    ) {
        self.panel.phones_loaded(remote_id, result);
        cx.notify();
    }

    pub(crate) fn phones_loaded_at(
        &mut self,
        generation: u64,
        remote_id: &str,
        result: Result<Vec<PairedPhoneInfo>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.list_generation == generation {
            self.phones_loaded(remote_id, result, cx);
        }
    }

    /// The answer to a create call, drawn from the same pixels as Iced.
    pub fn pairing_created(
        &mut self,
        remote_id: &str,
        result: Result<PhonePairing, String>,
        cx: &mut Context<Self>,
    ) {
        self.panel.pairing_created(remote_id, result, |url| {
            let qr = phone_panel::pairing_qr(url)?;
            let image = image::RgbaImage::from_raw(qr.side, qr.side, qr.pixels)
                .ok_or("Invalid pairing QR pixels")?;
            Ok(Arc::new(gpui::RenderImage::new([image::Frame::new(image)])))
        });
        cx.notify();
    }

    /// A pairing or revoke in flight disables the write controls.
    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.panel.busy = busy;
        cx.notify();
    }

    /// A call the preview gate refused: nothing is in flight, and the
    /// notice shows where Iced shows a call's error. A refused list shows
    /// the empty list, as Iced's failed list does, not "Loading…".
    pub fn refused(&mut self, notice: String, cx: &mut Context<Self>) {
        self.panel.busy = false;
        self.panel.error = Some(notice);
        self.panel.phones.get_or_insert_with(Vec::new);
        cx.notify();
    }

    /// Show pairing code / New code, and Enter in the name field.
    pub fn create(&mut self, cx: &mut Context<Self>) {
        self.panel.set_device_name(self.device_name(cx));
        cx.emit(PhonePanelOutcome::Create);
    }

    pub fn revoke(&mut self, id: String, cx: &mut Context<Self>) {
        // Iced disables Revoke while busy; the rule itself does not check.
        if self.panel.busy {
            return;
        }
        cx.emit(PhonePanelOutcome::Revoke(id));
    }
}

impl Focusable for PhonePanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for PhonePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let dark = p.base == 0x1e1e2e;
        let busy = self.panel.busy;
        let text = |value: SharedString, size: f32, color: u32| {
            div()
                .text_size(px(size))
                .text_color(rgb(color))
                .child(value)
        };
        let mono =
            |value: SharedString, size: f32, color: u32| text(value, size, color).font_family(MONO);
        let outline = |id: SharedString, label: &'static str, color: u32| {
            Button::new(id)
                .outline()
                .small()
                .font_family(MONO)
                .text_color(rgb(color))
                .label(label)
                .disabled(busy)
        };

        let create = outline("phone-create".into(), self.panel.create_label(), p.blue)
            .debug_selector(|| "phone-create".into())
            .on_click(cx.listener(|this, _, _, cx| this.create(cx)));
        let name_row = div()
            .flex()
            .items_end()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(text("Phone name".into(), 11., p.subtext0))
                    .child(
                        div()
                            .debug_selector(|| "phone-name".into())
                            .child(Input::new(&self.name).small().bg(rgb(p.base))),
                    ),
            )
            .child(create);
        let allow = div().debug_selector(|| "phone-allow-act".into()).child(
            Checkbox::new("phone-allow-act-box")
                .label(ALLOW_ACT_LABEL)
                .checked(self.panel.allow_act)
                .disabled(busy)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    this.panel.set_allow_act(*checked);
                    cx.notify();
                })),
        );

        let mut card = div()
            .id("phone-panel-card")
            .debug_selector(|| "phone-panel-card".into())
            .flex()
            .flex_col()
            .gap(px(10.))
            .w(px(460.))
            .px(px(28.))
            .py(px(24.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.mantle))
            .text_color(rgb(p.text))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                text(
                    format!("Pair a phone with {}", self.panel.machine_name).into(),
                    14.,
                    p.text,
                )
                .debug_selector(|| "phone-title".into()),
            )
            .child(text(EXPLANATION.into(), 11., p.muted))
            .child(name_row)
            .child(allow);

        if let Some(pairing) = &self.panel.pairing {
            let mut block = div().flex().flex_col().items_center().gap(px(6.)).child(
                div()
                    .debug_selector(|| "phone-pairing-qr".into())
                    .child(gpui::img(pairing.qr.clone()).w(px(240.)).h(px(240.))),
            );
            // An older agent sends no short code; the pairing still works.
            if !pairing.short_code.is_empty() {
                block = block
                    .child(
                        mono(pairing.short_code.clone().into(), 30., p.text)
                            .debug_selector(|| "phone-short-code".into()),
                    )
                    .child(text(SHORT_CODE_HINT.into(), 11., p.subtext0));
            }
            card = card.child(
                block
                    .child(
                        mono(pairing.url.clone().into(), 11., p.subtext0)
                            .debug_selector(|| "phone-pairing-url".into()),
                    )
                    .child(text(
                        format!(
                            "{} · works once · expires at {}",
                            scope_label(&pairing.scope),
                            local_clock_time(pairing.expires_unix_secs)
                        )
                        .into(),
                        10.,
                        p.muted,
                    )),
            );
        }

        if let Some(notice) = &self.panel.error {
            card = card.child(
                text(notice.clone().into(), 11., p.red).debug_selector(|| "phone-notice".into()),
            );
        }

        card = card.child(text("Paired phones".into(), 12., p.subtext0));
        match &self.panel.phones {
            None => {
                card = card.child(
                    text(LOADING.into(), 11., p.muted)
                        .debug_selector(|| "phone-list-loading".into()),
                )
            }
            Some(phones) if phones.is_empty() => {
                card = card.child(
                    text(NO_PHONES.into(), 11., p.muted)
                        .debug_selector(|| "phone-list-empty".into()),
                );
            }
            Some(phones) => {
                for phone in phones {
                    let selector = format!("phone-revoke-{}", phone.id);
                    let id = selector.clone();
                    let phone_id = phone.id.clone();
                    card = card.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(text(phone.name.clone().into(), 12., p.text))
                                    .child(text(
                                        format!(
                                            "{} · {}",
                                            scope_label(&phone.scope),
                                            seen_text(phone.last_seen_unix_secs)
                                        )
                                        .into(),
                                        10.,
                                        p.muted,
                                    )),
                            )
                            .child(
                                outline(selector.clone().into(), REVOKE_LABEL, p.red)
                                    .debug_selector(move || id.clone())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.revoke(phone_id.clone(), cx)
                                    })),
                            ),
                    );
                }
            }
        }

        card = card.child(
            div().flex().child(div().flex_1()).child(
                outline("phone-close".into(), "Close", p.subtext0)
                    .disabled(false)
                    .debug_selector(|| "phone-close".into())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PhonePanelOutcome::Close))),
            ),
        );

        div()
            .id("phone-panel")
            .debug_selector(|| "phone-panel".into())
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(if dark { 0x11111bcc } else { 0xdce0e8cc }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close(cx);
                    cx.stop_propagation();
                }
            }))
            .child(card)
    }
}
