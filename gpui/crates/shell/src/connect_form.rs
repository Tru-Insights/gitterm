//! Iced's "Connect Remote Host" form. Preview keeps drafts local; Live uses
//! the same core persistence and validation as Iced, off the UI thread.

use gpui::prelude::FluentBuilder as _;
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
use gitterm_core::remote::{
    config::RemoteAgentConfig,
    connect::{ConnectDraft, save_remote_connect},
    state::Identity,
};
use std::sync::{Arc, Condvar, Mutex};

/// Installed only after Live admission, while the host retains its root lease.
#[derive(Clone)]
pub(crate) struct LiveConnectRoot {
    lease: Arc<gitterm_core::state_lock::StateLock>,
    queue: Arc<(Mutex<SaveQueue>, Condvar)>,
    serial: Arc<Mutex<()>>,
}
#[derive(Default)]
struct SaveQueue {
    closed: bool,
    pending: usize,
}

/// Admission happens on submit, before the background executor can queue it.
struct ConnectSave(LiveConnectRoot);
impl Drop for ConnectSave {
    fn drop(&mut self) {
        let (state, drained) = &*self.0.queue;
        state.lock().unwrap().pending -= 1;
        drained.notify_all();
    }
}
impl ConnectSave {
    fn save(
        self,
        known: &[RemoteAgentConfig],
        draft: ConnectDraft<'_>,
    ) -> Result<RemoteAgentConfig, String> {
        let _serial = self.0.serial.lock().unwrap();
        save_remote_connect(self.0.root(), known, draft)
    }
}
impl gpui::Global for LiveConnectRoot {}
impl LiveConnectRoot {
    pub(crate) fn new(lease: Arc<gitterm_core::state_lock::StateLock>) -> Self {
        Self {
            lease,
            queue: Arc::new((Mutex::new(SaveQueue::default()), Condvar::new())),
            serial: Arc::new(Mutex::new(())),
        }
    }
    pub(crate) fn root(&self) -> &std::path::Path {
        self.lease.root()
    }
    pub(crate) fn close(&self) {
        let (state, drained) = &*self.queue;
        let mut state = state.lock().unwrap();
        state.closed = true;
        // Disk writes finish without needing a UI completion to run.
        while state.pending != 0 {
            state = drained.wait(state).unwrap();
        }
    }
    fn admit(&self) -> Result<ConnectSave, String> {
        let mut state = self.queue.0.lock().unwrap();
        if state.closed {
            return Err("Remote connection save refused: GitTerm is shutting down".into());
        }
        state.pending += 1;
        Ok(ConnectSave(self.clone()))
    }
}

pub const CONNECT_UNAVAILABLE: &str = "Connect · not available in preview";

#[derive(Clone, Debug)]
pub enum ConnectFormOutcome {
    Close,
    SaveFailed(String),
    Saved {
        agent: RemoteAgentConfig,
        attempt: u64,
    },
}

pub struct ConnectForm {
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    token: Entity<InputState>,
    status: Option<String>,
    live: Option<(LiveConnectRoot, Vec<RemoteAgentConfig>)>,
    saved: Option<Identity>,
    saving: bool,
    attempt: u64,
    saved_token: String,
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
            live: None,
            saved: None,
            saving: false,
            attempt: 0,
            saved_token: String::new(),
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

    pub(crate) fn enable_live(&mut self, root: LiveConnectRoot, known: Vec<RemoteAgentConfig>) {
        self.live = Some((root, known));
    }

    pub(crate) fn is_saving(&self) -> bool {
        self.saving
    }

    pub(crate) fn saved_matches(&self, remote: &RemoteAgentConfig, attempt: u64) -> bool {
        self.attempt == attempt
            && self.saved.as_ref().is_some_and(|saved| {
                saved.remote_id() == remote.id && saved.endpoint() == remote.endpoint
            })
    }

    pub(crate) fn connection_failed(&mut self, error: &str, cx: &mut Context<Self>) {
        let token = self.saved_token.trim();
        let safe = if token.is_empty() {
            error.to_string()
        } else {
            error.replace(token, "[redacted]")
        };
        self.status = Some(format!("Connection failed: {safe}"));
        cx.notify();
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let Some((root, known)) = self.live.clone() else {
            self.status = Some(CONNECT_UNAVAILABLE.into());
            cx.notify();
            return;
        };
        if self.saving {
            return;
        }
        let save = match root.admit() {
            Ok(save) => save,
            Err(error) => {
                self.status = Some(error);
                cx.notify();
                return;
            }
        };
        let name = self.name.read(cx).value().to_string();
        let endpoint = self.endpoint.read(cx).value().to_string();
        let token = self.token.read(cx).value().to_string();
        let saved = self.saved.clone();
        self.saving = true;
        self.attempt += 1;
        self.saved_token = token.clone();
        // Schedule disk work before returning to the UI: quit may drain before
        // the foreground completion task gets its first poll.
        let write = cx.background_executor().spawn(async move {
            save.save(
                &known,
                ConnectDraft {
                    name: &name,
                    endpoint: &endpoint,
                    token: &token,
                    saved: saved.as_ref(),
                },
            )
        });
        cx.spawn(async move |form, cx| {
            let result = write.await;
            _ = form.update(cx, |form, cx| {
                form.saving = false;
                match result {
                    Ok(agent) => {
                        form.saved = Some(
                            Identity::new(agent.id.clone(), agent.endpoint.clone())
                                .expect("shared connect writer validated the saved identity"),
                        );
                        form.status = Some("Connecting…".into());
                        cx.emit(ConnectFormOutcome::Saved {
                            agent,
                            attempt: form.attempt,
                        });
                    }
                    Err(error) => {
                        form.status = Some(error.clone());
                        cx.emit(ConnectFormOutcome::SaveFailed(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
        let live = self.live.is_some();
        let field = |label: &'static str, selector: &'static str, state: &Entity<InputState>| {
            div()
                .debug_selector(move || selector.into())
                .when(live, |field| {
                    let state = state.clone();
                    // Iced focuses the whole field, including its padded area.
                    field.on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        state.focus_handle(cx).focus(window, cx);
                    })
                })
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(p.subtext0))
                        .child(label),
                )
                .child(
                    Input::new(state)
                        .small()
                        .bg(rgb(p.base))
                        .when(live, |input| {
                            input
                                .focus_bordered(false)
                                .border_color(rgb(p.border))
                                .font_family("Menlo")
                                .text_size(px(12.))
                                .px(px(8.))
                                .py(px(6.))
                                .rounded(px(3.))
                        }),
                )
        };
        let status: SharedString = self.status.clone().unwrap_or_default().into();
        let card = div()
            .id("connect-form-card")
            .debug_selector(|| "connect-form-card".into())
            .flex()
            .flex_col()
            .gap(px(if live { 10. } else { 4. }))
            .w(px(if live { 460. } else { 420. }))
            .py(px(if live { 24. } else { 20. }))
            .px(px(if live { 28. } else { 20. }))
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
                    .text_color(rgb(if self.status.as_deref() == Some("Connecting…") {
                        p.subtext0
                    } else {
                        p.red
                    }))
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
                            .when(live, |button| {
                                button
                                    .bg(rgba(0x00000000))
                                    .font_family("Menlo")
                                    .text_size(px(12.))
                                    .text_color(rgb(p.subtext0))
                                    .border_color(rgb(p.border))
                                    .rounded(px(3.))
                                    .px(px(14.))
                                    .py(px(4.))
                            })
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
                            .when(live, |button| {
                                button
                                    .bg(rgba(0x00000000))
                                    .font_family("Menlo")
                                    .text_size(px(12.))
                                    .text_color(rgb(p.blue))
                                    .border_color(rgb(p.blue))
                                    .rounded(px(3.))
                                    .px(px(14.))
                                    .py(px(4.))
                            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::remote::{
        config::REMOTE_AGENTS_FILE,
        connect::{IDENTITY_CHANGED, MALFORMED, REQUIRED, load_remote_agents},
    };
    use gpui::TestAppContext;

    #[test]
    fn live_writer_keeps_the_lease_and_refuses_writes_after_shutdown_drain() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let lease = Arc::new(gitterm_core::state_lock::StateLock::acquire(&root).unwrap());
        let writer = LiveConnectRoot::new(lease.clone());
        drop(lease);
        assert!(gitterm_core::state_lock::StateLock::acquire(&root).is_err());
        let queued = writer.clone();
        writer.close();
        let error = queued.admit().err().unwrap();
        assert!(error.contains("shutting down"));
        assert!(!root.join(REMOTE_AGENTS_FILE).exists());
        assert!(!root.join("tokens").exists());
        drop(writer);
        drop(queued);
        assert!(gitterm_core::state_lock::StateLock::acquire(&root).is_ok());
    }

    #[test]
    fn shutdown_drains_submitted_saves_waiting_for_the_writer() {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = temp.path().join("v6");
        let writer = LiveConnectRoot::new(Arc::new(
            gitterm_core::state_lock::StateLock::acquire(&root).unwrap(),
        ));
        let first = writer.admit().unwrap();
        let second = writer.admit().unwrap();
        let serial = writer.serial.lock().unwrap();
        let run = |save: ConnectSave, name: &'static str| {
            std::thread::spawn(move || {
                save.save(
                    &[],
                    ConnectDraft {
                        name,
                        endpoint: "127.0.0.1:9",
                        token: "synthetic-token",
                        saved: None,
                    },
                )
            })
        };
        let first = run(first, "First");
        let second = run(second, "Second");
        let closing = writer.clone();
        let (done, drained) = std::sync::mpsc::channel();
        let close = std::thread::spawn(move || {
            closing.close();
            done.send(()).unwrap();
        });
        while !writer.queue.0.lock().unwrap().closed {
            std::thread::yield_now();
        }
        assert!(writer.admit().is_err());
        assert!(
            drained.try_recv().is_err(),
            "drain returned before queued writes"
        );
        drop(serial);
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        close.join().unwrap();
        drained.recv().unwrap();
        let agents = load_remote_agents(&root).unwrap();
        assert_eq!(agents.remote_agents.len(), 2);
        for id in ["first", "second"] {
            assert!(agents.remote_agents.iter().any(|agent| agent.id == id));
            assert_eq!(
                std::fs::read(root.join(format!("tokens/{id}.token"))).unwrap(),
                b"synthetic-token\n"
            );
        }
    }

    // No native window or network: GPUI's in-memory test platform.
    #[gpui::test]
    fn live_connect_round_trip_validation_redaction_and_preview_gate(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = temp.path().join("v6");
        let (form, cx) = cx.add_window_view(ConnectForm::new);
        // The same submit, even with complete credentials, cannot write in Preview.
        cx.update(|window, cx| {
            form.update(cx, |form, cx| {
                form.name
                    .update(cx, |input, cx| input.set_value("Mac mini", window, cx));
                form.endpoint.update(cx, |input, cx| {
                    input.set_value("fixture.invalid:7777", window, cx)
                });
                form.token.update(cx, |input, cx| {
                    input.set_value("synthetic-connect-secret", window, cx)
                });
                form.submit(cx);
                assert_eq!(form.status(), Some(CONNECT_UNAVAILABLE));
            });
        });
        cx.run_until_parked();
        assert!(!root.exists());
        cx.update(|_, cx| {
            form.update(cx, |form, cx| {
                form.enable_live(
                    LiveConnectRoot::new(Arc::new(
                        gitterm_core::state_lock::StateLock::acquire(&root).unwrap(),
                    )),
                    vec![],
                );
                form.submit(cx);
            })
        });
        cx.run_until_parked();
        let agents = load_remote_agents(&root).unwrap();
        assert_eq!(agents.remote_agents.len(), 1);
        assert_eq!(agents.remote_agents[0].id, "mac-mini");
        assert_eq!(
            agents.remote_agents[0].endpoint,
            "http://fixture.invalid:7777"
        );
        cx.read(|cx| assert_eq!(form.read(cx).status(), Some("Connecting…")));
        let before = std::fs::read(root.join(REMOTE_AGENTS_FILE)).unwrap();
        // Iced saves a second entry, and the GPUI form preserves it on retry.
        save_remote_connect(
            &root,
            &[],
            ConnectDraft {
                name: "Other",
                endpoint: "other.invalid:7777",
                token: "env:SYNTHETIC_TOKEN",
                saved: None,
            },
        )
        .unwrap();
        cx.update(|_, cx| form.update(cx, |form, cx| form.submit(cx)));
        cx.run_until_parked();
        assert_eq!(load_remote_agents(&root).unwrap().remote_agents.len(), 2);
        cx.update(|window, cx| {
            form.update(cx, |form, cx| {
                form.connection_failed("synthetic-connect-secret rejected", cx);
                assert_eq!(
                    form.status(),
                    Some("Connection failed: [redacted] rejected")
                );
                form.endpoint.update(cx, |input, cx| {
                    input.set_value("changed.invalid:7777", window, cx)
                });
                form.submit(cx);
            })
        });
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(form.read(cx).status(), Some(IDENTITY_CHANGED)));
        std::fs::write(root.join(REMOTE_AGENTS_FILE), "{").unwrap();
        cx.update(|window, cx| {
            form.update(cx, |form, cx| {
                form.name
                    .update(cx, |input, cx| input.set_value("", window, cx));
                form.submit(cx);
            })
        });
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(form.read(cx).status(), Some(REQUIRED)));
        cx.update(|window, cx| {
            form.update(cx, |form, cx| {
                form.name
                    .update(cx, |input, cx| input.set_value("Mac mini", window, cx));
                form.submit(cx);
            })
        });
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(form.read(cx).status(), Some(MALFORMED)));
        assert_eq!(std::fs::read(root.join(REMOTE_AGENTS_FILE)).unwrap(), b"{");
        assert!(
            !String::from_utf8(before)
                .unwrap()
                .contains("synthetic-connect-secret")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(root.join("tokens/mac-mini.token"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
