//! Iced's "Connect Remote Host" form. Typing is a draft held only in the
//! inputs; Connect would write remote-agents.json and a token, so the
//! preview shows a notice and saves nothing.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, rgb, rgba,
};
use gpui_component::{
    Sizable as _,
    button::Button,
    input::{Input, InputEvent, InputState},
};

use crate::theme::Palette;

pub const CONNECT_UNAVAILABLE: &str = "Connect · not available in preview";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectFormOutcome {
    Close,
}

pub struct ConnectForm {
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    token: Entity<InputState>,
    status: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConnectFormOutcome> for ConnectForm {}

impl ConnectForm {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            let input = InputState::new(window, cx).placeholder("Mac mini");
            input.focus(window, cx);
            input
        });
        let endpoint = cx
            .new(|cx| InputState::new(window, cx).placeholder("http://host-or-tailscale-ip:7777"));
        let token = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("paste the agent token");
            input.set_masked(true, window, cx);
            input
        });
        let subscriptions = [&name, &endpoint, &token]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit(cx);
                    }
                })
            })
            .collect();
        Self {
            name,
            endpoint,
            token,
            status: None,
            _subscriptions: subscriptions,
        }
    }

    /// Draft values (name, endpoint); the token is never exposed here.
    pub fn draft(&self, cx: &App) -> (String, String) {
        (
            self.name.read(cx).value().to_string(),
            self.endpoint.read(cx).value().to_string(),
        )
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        self.status = Some(CONNECT_UNAVAILABLE.into());
        cx.notify();
    }
}

impl Focusable for ConnectForm {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for ConnectForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let dark = p.base == 0x1e1e2e;
        let field = |label: &'static str, selector: &'static str, state: &Entity<InputState>| {
            div()
                .debug_selector(move || selector.into())
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(p.subtext0))
                        .child(label),
                )
                .child(Input::new(state).small().bg(rgb(p.base)))
        };
        let status: SharedString = self.status.clone().unwrap_or_default().into();
        let card = div()
            .id("connect-form-card")
            .debug_selector(|| "connect-form-card".into())
            .flex()
            .flex_col()
            .gap(px(4.))
            .w(px(420.))
            .p(px(20.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.mantle))
            .text_color(rgb(p.text))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(div().text_size(px(14.)).child("Connect Remote Host"))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child("Adds a gitterm-v6-agent host to remote-agents.json"),
            )
            .child(div().h(px(8.)))
            .child(field("Name", "connect-form-name", &self.name))
            .child(field("Endpoint", "connect-form-endpoint", &self.endpoint))
            .child(field("Token", "connect-form-token", &self.token))
            .child(div().text_size(px(10.)).text_color(rgb(p.muted)).child(
                "Stored in ~/.config/gitterm-v6/tokens/. Or paste a ref: \
                         env:VAR, file:~/path, keychain:service/account",
            ))
            .child(div().h(px(4.)))
            .child(
                div()
                    .debug_selector(|| "connect-form-status".into())
                    .text_size(px(11.))
                    .text_color(rgb(p.red))
                    .child(status),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1())
                    .child(
                        Button::new("connect-form-close")
                            .debug_selector(|| "connect-form-close".into())
                            .outline()
                            .small()
                            .label("Close")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(ConnectFormOutcome::Close)),
                            ),
                    )
                    .child(
                        Button::new("connect-form-connect")
                            .debug_selector(|| "connect-form-connect".into())
                            .outline()
                            .small()
                            .label("Connect")
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            );
        div()
            .id("connect-form")
            .debug_selector(|| "connect-form".into())
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(if dark { 0x11111bcc } else { 0xdce0e8cc }))
            .on_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(ConnectFormOutcome::Close);
                    cx.stop_propagation();
                }
            }))
            .child(card)
    }
}
