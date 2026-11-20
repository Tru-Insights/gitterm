//! Fixture-backed remote connection presentation. This module collects typed
//! intents only; it does not persist remotes or attempt a connection.

use std::{cell::RefCell, fmt, rc::Rc};

use gitterm_core::remote::{client::RemoteAgentHandshake, config::RemoteAgentConfig};
use gpui::{
    AppContext, Entity, InteractiveElement as _, IntoElement, ParentElement, Styled, div, px,
};
use gpui_component::{
    button::Button,
    input::{Input, InputState},
};

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;

/// Token payload is intentionally omitted from its Debug representation.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RemoteToken(String);

impl RemoteToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for RemoteToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteToken([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteIntent {
    Connect {
        name: String,
        endpoint: String,
        token: RemoteToken,
    },
    ConnectSaved {
        remote_id: String,
    },
    Remove {
        remote_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteKey {
    Enter,
    Escape,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteForm {
    name: String,
    endpoint: String,
    token: RemoteToken,
}

impl RemoteForm {
    pub fn set_name(&mut self, value: impl Into<String>) {
        self.name = value.into();
    }

    pub fn set_endpoint(&mut self, value: impl Into<String>) {
        self.endpoint = value.into();
    }

    pub fn set_token(&mut self, value: impl Into<String>) {
        self.token = RemoteToken::new(value);
    }

    pub fn connect(&self) -> Option<RemoteIntent> {
        Some(RemoteIntent::Connect {
            name: self.name.clone(),
            endpoint: self.endpoint.clone(),
            token: self.token.clone(),
        })
    }

    pub fn key(&self, key: RemoteKey) -> Option<RemoteIntent> {
        match key {
            RemoteKey::Enter => self.connect(),
            RemoteKey::Escape => None,
        }
    }

    pub fn connect_saved(remote: &RemoteAgentConfig) -> RemoteIntent {
        RemoteIntent::ConnectSaved {
            remote_id: remote.id.clone(),
        }
    }

    pub fn remove(remote: &RemoteAgentConfig) -> RemoteIntent {
        RemoteIntent::Remove {
            remote_id: remote.id.clone(),
        }
    }

    /// Strings suitable for the fixture presentation. The token field shows
    /// only bullets and saved records expose only their core identity/name/URL.
    pub fn presentation_lines(
        &self,
        remotes: &[RemoteAgentConfig],
        status: Option<&RemoteStatus>,
    ) -> Vec<String> {
        let mut lines = vec![
            format!("Name: {}", self.name),
            format!("Endpoint: {}", self.endpoint),
            format!("Token: {}", mask_token(&self.token.0)),
        ];
        lines.extend(
            remotes
                .iter()
                .map(|remote| format!("{} · {} · {}", remote.id, remote.name, remote.endpoint)),
        );
        if let Some(status) = status {
            match status {
                RemoteStatus::Connected(handshake) => lines.extend([
                    format!(
                        "Connected — {} (protocol {})",
                        handshake.agent_name, handshake.protocol_version
                    ),
                    format!("Capabilities: {}", handshake.capabilities.join(", ")),
                ]),
                RemoteStatus::Failed(error) => {
                    lines.push(format!("Connection failed: {}", error.as_str()))
                }
            }
        }
        lines
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteStatus {
    Connected(RemoteAgentHandshake),
    Failed(SafeRemoteError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeRemoteError(String);

impl SafeRemoteError {
    pub fn new(error: &str, token: &str) -> Self {
        Self(safe_error(error, token))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Replace an echoed token before any remote error reaches a presentation.
pub fn safe_error(error: &str, token: &str) -> String {
    if token.is_empty() {
        return error.to_owned();
    }
    error.replace(token, "[redacted]")
}

fn mask_token(token: &str) -> String {
    "•".repeat(token.chars().count().min(16))
}

/// GPUI input states are created together so the token starts masked and cannot
/// accidentally be passed to the view as a plain input.
pub struct RemoteInputs {
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    token: Entity<InputState>,
}

#[derive(Clone, Default)]
pub struct RemoteIntentSink(Rc<RefCell<Vec<RemoteIntent>>>);

impl RemoteIntentSink {
    pub fn drain(&self) -> Vec<RemoteIntent> {
        std::mem::take(&mut *self.0.borrow_mut())
    }

    fn push(&self, intent: RemoteIntent) {
        self.0.borrow_mut().push(intent);
    }
}

pub fn new_inputs(window: &mut gpui::Window, cx: &mut gpui::App) -> RemoteInputs {
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Mac mini"));
    let endpoint =
        cx.new(|cx| InputState::new(window, cx).placeholder("http://host-or-tailscale-ip:7777"));
    let token = cx.new(|cx| InputState::new(window, cx).placeholder("Agent token"));
    token.update(cx, |state, cx| state.set_masked(true, window, cx));
    RemoteInputs {
        name,
        endpoint,
        token,
    }
}

/// Native fixture entry point. The host may collect `RemoteIntent` values
/// later; this view has no registration or connection path.
pub fn fixture_view(
    remotes: &[RemoteAgentConfig],
    inputs: &RemoteInputs,
    status: Option<&RemoteStatus>,
    intents: RemoteIntentSink,
    on_cancel: impl Fn() + 'static,
) -> impl IntoElement {
    let field = |label: &'static str, state: &Entity<InputState>| {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(label)
            .child(Input::new(state))
    };
    let saved = remotes.iter().map(|remote| {
        let connect_sink = intents.clone();
        let connect_id = remote.id.clone();
        let remove_sink = intents.clone();
        let remove_id = remote.id.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(format!(
                "{} · {} · {}",
                remote.id, remote.name, remote.endpoint
            ))
            .child(
                Button::new(format!("connect-{}", remote.id))
                    .label("Connect")
                    .on_click(move |_, _, _| {
                        connect_sink.push(RemoteIntent::ConnectSaved {
                            remote_id: connect_id.clone(),
                        });
                    }),
            )
            .child(
                Button::new(format!("remove-{}", remote.id))
                    .label("Remove")
                    .on_click(move |_, _, _| {
                        remove_sink.push(RemoteIntent::Remove {
                            remote_id: remove_id.clone(),
                        });
                    }),
            )
    });
    let connect_sink = intents.clone();
    let name = inputs.name.clone();
    let endpoint = inputs.endpoint.clone();
    let token = inputs.token.clone();
    let cancel = Rc::new(on_cancel);
    let key_intents = intents.clone();
    let key_name = inputs.name.clone();
    let key_endpoint = inputs.endpoint.clone();
    let key_token = inputs.token.clone();
    let key_cancel = cancel.clone();
    let status_lines = status.map_or_else(Vec::new, |status| match status {
        RemoteStatus::Connected(handshake) => vec![
            format!(
                "Connected — {} (protocol {})",
                handshake.agent_name, handshake.protocol_version
            ),
            format!("Capabilities: {}", handshake.capabilities.join(", ")),
        ],
        RemoteStatus::Failed(error) => vec![format!("Connection failed: {}", error.as_str())],
    });
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w(px(560.))
        .p_3()
        .child(field("Name", &inputs.name))
        .child(field("Endpoint", &inputs.endpoint))
        .child(field("Token", &inputs.token))
        .child(
            div()
                .flex()
                .gap_2()
                .child(Button::new("connect-new-remote").label("Connect").on_click(
                    move |_, _, cx| {
                        connect_sink.push(RemoteIntent::Connect {
                            name: name.read(cx).value().to_string(),
                            endpoint: endpoint.read(cx).value().to_string(),
                            token: RemoteToken::new(token.read(cx).value().to_string()),
                        });
                    },
                ))
                .child(
                    Button::new("cancel-remote")
                        .label("Cancel")
                        .on_click(move |_, _, _| cancel()),
                ),
        )
        .child(div().child("Saved remotes"))
        .children(saved)
        .children(status_lines.into_iter().map(|line| div().child(line)))
        .on_key_down(
            move |event, _, cx| match event.keystroke.key.to_lowercase().as_str() {
                "enter" => {
                    key_intents.push(RemoteIntent::Connect {
                        name: key_name.read(cx).value().to_string(),
                        endpoint: key_endpoint.read(cx).value().to_string(),
                        token: RemoteToken::new(key_token.read(cx).value().to_string()),
                    });
                    cx.stop_propagation();
                }
                "escape" => {
                    key_cancel();
                    cx.stop_propagation();
                }
                _ => {}
            },
        )
}
