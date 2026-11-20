//! Native preview composition. The shared workspace owner retains these entities;
//! docking/hiding never installs another runtime receiver.
mod provider;
#[cfg(test)]
mod tests;
use anyhow::Result;
#[cfg(test)]
use gitterm_core::chat::types::HarnessEvent;
use gitterm_core::{
    chat::{
        runtime::Session,
        state::{AgentEvent, RuntimeStamp},
        types::AgentBackend,
    },
    session,
};
use gitterm_gpui_chat::{
    ChatIntent, ChatView,
    adapter::{Admission, ChatAdapter, Receipt, Update},
    events::{Entry, Record},
};
use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled, Window,
    div,
};
use gpui_component::{Disableable, button::Button};
use provider::Provider;
use std::{path::PathBuf, rc::Rc, sync::Arc};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedChat {
    pub uid: String,
    pub cwd: PathBuf,
    pub provider: PathBuf,
    pub session_id: Option<String>,
    pub records: Vec<Record>,
}
impl SavedChat {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.uid.is_empty() && self.cwd.is_absolute() && self.provider.is_absolute(),
            "invalid preview chat metadata"
        );
        Ok(())
    }
}

struct Controller {
    adapter: ChatAdapter,
    target: session::Target,
    source: RuntimeStamp,
    runtime: Option<Session>,
    ready: bool,
    message: u64,
}
impl Controller {
    fn new() -> Self {
        let mut adapter = ChatAdapter::default();
        let target = crate::sessions::fresh_target();
        let source = adapter.begin_runtime(target.clone());
        Self {
            adapter,
            target,
            source,
            runtime: None,
            ready: false,
            message: 0,
        }
    }
    fn dispatch(&mut self, intent: ChatIntent) -> (Receipt, Update) {
        if matches!(&intent, ChatIntent::Send { images, .. } if !images.is_empty()) {
            return (
                Receipt::rejected("Images: not in preview yet"),
                Update::default(),
            );
        }
        if matches!(
            &intent,
            ChatIntent::Permission {
                decision: gitterm_gpui_chat::PermissionDecision::AllowAlways { .. },
                ..
            }
        ) {
            return (
                Receipt::rejected("Remember permission: not in preview yet"),
                Update::default(),
            );
        }
        self.message += 1;
        let facts = Admission {
            authority: session::State {
                target: self.target.clone(),
                control: session::TurnControl::Harness,
                task_live: false,
                last_tab: false,
            },
            backend: AgentBackend::Claude,
            process_running: self.ready && self.runtime.is_some(),
            just_spawned: false,
            message_id: format!("{}-{}", self.target.session_uid, self.message),
        };
        let prepared = match self.adapter.admit(intent, facts) {
            Ok(prepared) => prepared,
            Err(receipt) => return (receipt, Update::default()),
        };
        let runtime = self.runtime.as_ref();
        self.adapter
            .execute(prepared, &self.target, now_ms(), |command| {
                runtime
                    .ok_or_else(|| "No preview runtime".to_string())?
                    .send(command)
            })
    }
    /// C8a hook: channel handoff is not a correlated runner write outcome.
    /// Add the actual core outcome contract here when it lands; never infer one
    /// from errors, Ready, ProcessExited, or RuntimeRequestResolved.
    #[cfg(test)]
    fn apply_runtime_outcome(
        &mut self,
        source: &RuntimeStamp,
        event: HarnessEvent,
    ) -> Option<Update> {
        let ready = matches!(event, HarnessEvent::Ready { .. });
        let exited = matches!(event, HarnessEvent::ProcessExited { .. });
        let update = self.adapter.receive(
            source,
            &self.target,
            AgentEvent::Harness(event),
            Some(now_ms()),
            |_, _| {},
        )?;
        if ready {
            self.ready = true;
        }
        if exited {
            self.ready = false;
        }
        Some(update)
    }
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) struct ChatPane {
    controller: Controller,
    saved: SavedChat,
    view: Entity<ChatView>,
    status: String,
    pending: Vec<Update>,
    shutdown: Arc<RuntimeShutdown>,
}
impl EventEmitter<SavedChat> for ChatPane {}
impl ChatPane {
    pub fn new(
        shutdown: Arc<RuntimeShutdown>,
        preview: Arc<gitterm_core::preview::Preview>,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let controller = Controller::new();
        let saved = SavedChat {
            uid: controller.target.session_uid.clone(),
            cwd,
            provider: preview.provider(),
            session_id: None,
            records: vec![],
        };
        let view = cx.new(|cx| {
            let mut view = ChatView::new(vec![], window, cx);
            view.set_preview(cx);
            view.set_read_only(Some("Preview activation unavailable".into()), cx);
            view
        });
        let callback = Self::intent_callback(cx.weak_entity());
        view.update(cx, |view, cx| view.set_intent_callback(callback, cx));
        let status = (|| -> Result<String> {
            let program = claude_program()?;
            let provider = Provider::new(preview, &program)?;
            provider.validate_workspace(&saved.cwd)?;
            provider.prepare()?;
            let command = provider.sign_in_command();
            let gate = match provider.authorize() {
                Err(error) => error,
                Ok(()) => anyhow::bail!("Claude launch confinement has not been implemented"),
            };
            Ok(format!("Claude preview unavailable: {gate:#}\nSign in to Claude for the preview (after isolation review):\n{command}"))
        })().unwrap_or_else(|error| format!("Claude preview unavailable: {error:#}"));
        Self {
            controller,
            saved,
            view,
            status,
            pending: vec![],
            shutdown,
        }
    }
    fn intent_callback(weak: gpui::WeakEntity<Self>) -> gitterm_gpui_chat::IntentCallback {
        Rc::new(move |intent, cx| {
            weak.update(cx, |pane, cx| {
                let (receipt, update) = pane.controller.dispatch(intent);
                pane.pending.push(update);
                pane.changed(cx);
                receipt
            })
            .unwrap_or_else(|_| Receipt::rejected("Preview chat closed"))
        })
    }
    pub fn history(
        shutdown: Arc<RuntimeShutdown>,
        saved: SavedChat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let controller = Controller::new();
        let view = cx.new(|cx| {
            let mut view = ChatView::new(saved.records.clone(), window, cx);
            view.set_preview(cx);
            view.set_read_only(
                Some("Preview history · read only · resume not in preview yet".into()),
                cx,
            );
            view
        });
        Self {
            controller,
            saved,
            view,
            status: "Preview history · no process attached".into(),
            pending: vec![],
            shutdown,
        }
    }
    pub fn target(&self) -> session::Target {
        self.controller.target.clone()
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.controller.ready = false;
        self.shutdown_runtime();
        self.status = "Preview chat closed · history retained".into();
        self.view.update(cx, |view, cx| {
            view.set_read_only(Some("Chat closed".into()), cx)
        });
        cx.notify();
    }
    pub(crate) fn shutdown_runtime(&mut self) {
        self.controller.ready = false;
        if let Some(runtime) = self.controller.runtime.take()
            && let Err(runtime) = self.shutdown.enqueue(runtime)
        {
            self.controller.runtime = Some(runtime);
            eprintln!("Close preview Claude: shutdown queue stopped; retaining runtime ownership");
        }
    }
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.saved.session_id = self.controller.adapter.state().session_id.clone();
        self.saved.records = self
            .controller
            .adapter
            .state()
            .conversation
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Harness(event) => Some(Record {
                    at_ms: None,
                    entry: Entry::Harness(event.clone()),
                }),
                AgentEvent::Other(value) if value["type"] == "user_prompt" => {
                    value["text"].as_str().map(|text| Record {
                        at_ms: None,
                        entry: Entry::UserPrompt(text.into()),
                    })
                }
                _ => None,
            })
            .collect();
        cx.emit(self.saved.clone());
        cx.notify();
    }
    #[cfg(test)]
    fn install_fake(
        &mut self,
        runtime: Session,
        mut events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.runtime = Some(runtime);
        let source = self.controller.source.clone();
        cx.spawn_in(window, async move |pane, cx| {
            while let Some(event) = events.recv().await {
                if pane
                    .update_in(cx, |pane, _, cx| {
                        if let Some(update) = pane.controller.apply_runtime_outcome(&source, event)
                        {
                            pane.pending.push(update);
                            pane.changed(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        self.status = "Synthetic Claude runtime · delivery unconfirmed".into();
        self.view
            .update(cx, |view, cx| view.set_read_only(None, cx));
    }
}
impl Drop for ChatPane {
    fn drop(&mut self) {
        self.shutdown_runtime();
    }
}
impl gpui::Focusable for ChatPane {
    fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.view.focus_handle(cx)
    }
}

/// The application retains completion ownership after a pane is closed. Quit
/// drains synchronously before GPUI starts its 200 ms future timeout, as the
/// existing preview disk queue does. Ordinary close callbacks only enqueue.
pub(crate) struct RuntimeShutdown {
    sender: std::sync::mpsc::Sender<ShutdownCommand>,
}
enum ShutdownCommand {
    Session(Session),
    Drain(std::sync::mpsc::SyncSender<std::result::Result<(), String>>),
}
impl RuntimeShutdown {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("preview-chat-shutdown".into())
            .spawn(move || {
                let executor = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("create preview chat shutdown executor");
                let mut errors = Vec::new();
                while let Ok(command) = receiver.recv() {
                    match command {
                        ShutdownCommand::Session(mut session) => {
                            if let Err(error) = executor.block_on(session.completed()) {
                                eprintln!("Reap preview Claude: {error}");
                                errors.push(error);
                            }
                        }
                        ShutdownCommand::Drain(reply) => {
                            let result = if errors.is_empty() {
                                Ok(())
                            } else {
                                Err(std::mem::take(&mut errors).join("; "))
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .expect("create preview chat shutdown thread");
        Arc::new(Self { sender })
    }
    fn enqueue(&self, session: Session) -> std::result::Result<(), Session> {
        // A closed channel means the child has already exited. Completion still
        // owns snapshot cleanup and the thread join, so always queue the handle.
        let _ = session.send(gitterm_core::chat::types::HarnessCommand::Shutdown);
        self.sender
            .send(ShutdownCommand::Session(session))
            .map_err(|error| match error.0 {
                ShutdownCommand::Session(session) => session,
                ShutdownCommand::Drain(_) => unreachable!(),
            })
    }
    pub(crate) fn drain(&self) -> Result<()> {
        use anyhow::Context as _;
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.sender
            .send(ShutdownCommand::Drain(reply))
            .map_err(|_| anyhow::anyhow!("preview chat shutdown queue stopped"))?;
        result
            .recv()
            .context("preview chat shutdown drain stopped")?
            .map_err(anyhow::Error::msg)
    }
}
impl Drop for RuntimeShutdown {
    fn drop(&mut self) {
        if let Err(error) = self.drain() {
            eprintln!("Drain preview Claude runtimes: {error:#}");
        }
    }
}

impl Render for ChatPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for update in std::mem::take(&mut self.pending) {
            self.view.update(cx, |view, cx| {
                view.present(
                    &self.controller.source,
                    self.controller.adapter.state(),
                    update,
                    window,
                    cx,
                )
            });
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().p_2().child(self.status.clone()))
            .child(
                Button::new("preview-status")
                    .label("Isolation review required before sign-in or chat")
                    .disabled(true),
            )
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}
fn claude_program() -> Result<PathBuf> {
    // Resolve only; neither --version nor any real CLI subcommand runs here.
    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("PATH unavailable; Claude executable not found"))?;
    std::env::split_paths(&path)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("claude"))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow::anyhow!("Claude executable not found in PATH"))
}
