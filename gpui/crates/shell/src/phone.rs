//! Iced's "Pair a phone" panel (TRU-151). The preview is read-only on Iced's
//! data: typing and the checkbox are local, while every control that would
//! grant a pairing or revoke a phone shows a notice and calls nothing.
//!
//! Paired phones and an issued pairing only ever come from a live agent, which
//! this preview never contacts, so the shell passes neither; `set_phones` and
//! `set_pairing` take a read-only source if one appears. The pairing QR image
//! is not drawn (no QR dependency, and no pairing to draw it for).

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
/// Iced's initial `device_name`; the placeholder is the same word.
pub const DEFAULT_PHONE_NAME: &str = "iPhone";
pub const CREATE_LABEL: &str = "Show pairing code";
pub const RECREATE_LABEL: &str = "New code";
pub const REVOKE_LABEL: &str = "Revoke";
pub const ALLOW_ACT_LABEL: &str = "Allow sending and approving";
pub const EXPLANATION: &str = "Scan the code with the phone's camera; the phone must be on the \
     same tailnet. It can view this machine's chats; allowed to send, \
     it can also message them, answer their prompts and stop a turn. \
     To change what a phone may do, revoke it and pair it again.";
pub const SHORT_CODE_HINT: &str = "Using the home-screen app? Open it and type this code.";
pub const PAIRED_PHONES_UNAVAILABLE: &str = "Paired phones are not listed in preview.";

/// A phone already paired with the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedPhone {
    pub id: String,
    pub name: String,
    /// "view only" / "can send and approve", as Iced's `phone_scope_label`.
    pub scope: String,
    /// "never used" or "seen 5m ago".
    pub seen: String,
}

/// An issued pairing, as the panel shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub url: String,
    /// `XXXX-XXXX`; empty from an agent that predates short codes.
    pub short_code: String,
    pub scope: String,
    pub expires: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhonePanelOutcome {
    Close,
    /// A write control was used; the shell reports it as unavailable.
    Unavailable(String),
}

pub fn unavailable_notice(label: &str) -> String {
    format!("{label} · not available in preview")
}

pub struct PhonePanel {
    machine_name: String,
    name: Entity<InputState>,
    allow_act: bool,
    pairing: Option<Pairing>,
    phones: Option<Vec<PairedPhone>>,
    busy: bool,
    notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PhonePanelOutcome> for PhonePanel {}

impl PhonePanel {
    pub fn new(machine_name: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder(DEFAULT_PHONE_NAME);
            input.set_value(DEFAULT_PHONE_NAME, window, cx);
            input.focus(window, cx);
            input
        });
        let subscription = cx.subscribe(&name, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.create(cx);
            }
        });
        Self {
            machine_name,
            name,
            allow_act: false,
            pairing: None,
            phones: None,
            busy: false,
            notice: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn device_name(&self, cx: &App) -> String {
        self.name.read(cx).value().to_string()
    }

    pub fn allow_act(&self) -> bool {
        self.allow_act
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_phones(&mut self, phones: Option<Vec<PairedPhone>>, cx: &mut Context<Self>) {
        self.phones = phones;
        cx.notify();
    }

    pub fn set_pairing(&mut self, pairing: Option<Pairing>, cx: &mut Context<Self>) {
        self.pairing = pairing;
        cx.notify();
    }

    /// A pairing or revoke in flight disables the write controls.
    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    fn create_label(&self) -> &'static str {
        if self.pairing.is_some() {
            RECREATE_LABEL
        } else {
            CREATE_LABEL
        }
    }

    /// Show pairing code / New code, and Enter in the name field.
    pub fn create(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            self.unavailable(self.create_label(), cx);
        }
    }

    pub fn revoke(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            self.unavailable(REVOKE_LABEL, cx);
        }
    }

    fn unavailable(&mut self, label: &str, cx: &mut Context<Self>) {
        self.notice = Some(unavailable_notice(label));
        cx.emit(PhonePanelOutcome::Unavailable(label.to_string()));
        cx.notify();
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
        let busy = self.busy;
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

        let create = outline("phone-create".into(), self.create_label(), p.blue)
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
                .checked(self.allow_act)
                .disabled(busy)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    this.allow_act = *checked;
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
                    format!("Pair a phone with {}", self.machine_name).into(),
                    14.,
                    p.text,
                )
                .debug_selector(|| "phone-title".into()),
            )
            .child(text(EXPLANATION.into(), 11., p.muted))
            .child(name_row)
            .child(allow);

        if let Some(pairing) = &self.pairing {
            let mut block = div().flex().flex_col().items_center().gap(px(6.));
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
                            pairing.scope, pairing.expires
                        )
                        .into(),
                        10.,
                        p.muted,
                    )),
            );
        }

        if let Some(notice) = &self.notice {
            card = card.child(
                text(notice.clone().into(), 11., p.red).debug_selector(|| "phone-notice".into()),
            );
        }

        card = card.child(text("Paired phones".into(), 12., p.subtext0));
        match &self.phones {
            None => {
                card = card.child(
                    text(PAIRED_PHONES_UNAVAILABLE.into(), 11., p.muted)
                        .debug_selector(|| "phone-list-unavailable".into()),
                )
            }
            Some(phones) if phones.is_empty() => {
                card = card.child(text("None yet.".into(), 11., p.muted));
            }
            Some(phones) => {
                for phone in phones {
                    let selector = format!("phone-revoke-{}", phone.id);
                    let id = selector.clone();
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
                                        format!("{} · {}", phone.scope, phone.seen).into(),
                                        10.,
                                        p.muted,
                                    )),
                            )
                            .child(
                                outline(selector.clone().into(), REVOKE_LABEL, p.red)
                                    .debug_selector(move || id.clone())
                                    .on_click(cx.listener(|this, _, _, cx| this.revoke(cx))),
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
            .on_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(PhonePanelOutcome::Close);
                    cx.stop_propagation();
                }
            }))
            .child(card)
    }
}
