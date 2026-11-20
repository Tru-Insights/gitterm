//! Native preview composition. The shared workspace owner retains these entities;
//! docking/hiding never installs another runtime receiver.
mod auth;
mod fence;
mod lease;
mod managed;
mod provider;
#[cfg(test)]
mod tests;
use anyhow::{Context as _, Result};
use gitterm_core::chat::{
    runtime::{DeliveryOutcome, RuntimeEvent},
    types::HarnessEvent,
};
use gitterm_core::{
    chat::{
        runtime::{CommandCorrelation, Session},
        state::{AgentEvent, RuntimeStamp},
        types::AgentBackend,
    },
    session,
};
use gitterm_gpui_chat::{
    ChatIntent, ChatView,
    adapter::{Admission, AttemptId, ChatAdapter, Receipt, Update},
    events::{Entry, Record},
};
use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled, Window,
    div,
};
use gpui_component::{Disableable, button::Button};
use provider::Provider;
use std::{collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

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
    runtime: Option<OwnedRuntime>,
    ready: bool,
    message: u64,
    correlations: HashMap<CommandCorrelation, AttemptId>,
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
            correlations: HashMap::new(),
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
        let correlation = CommandCorrelation(self.message);
        self.correlations.insert(correlation, prepared.id());
        let runtime = self.runtime.as_ref();
        let result = self
            .adapter
            .execute(prepared, &self.target, now_ms(), |command| {
                runtime
                    .ok_or_else(|| "No preview runtime".to_string())?
                    .send_correlated(command, correlation)
            });
        if !matches!(
            result.0.state,
            gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted
        ) {
            self.correlations.remove(&correlation);
        }
        result
    }
    fn apply_runtime_outcome(
        &mut self,
        source: &RuntimeStamp,
        event: RuntimeEvent,
    ) -> Option<Update> {
        if !self.adapter.state().accepts_stream(source, &self.target) {
            return None;
        }
        let event = match event {
            RuntimeEvent::Harness(event) => event,
            RuntimeEvent::CommandOutcome {
                correlation,
                outcome,
            } => {
                let id = self.correlations.remove(&correlation)?;
                let outcome = match outcome {
                    DeliveryOutcome::Written => {
                        gitterm_gpui_chat::adapter::DeliveryOutcome::Written
                    }
                    DeliveryOutcome::Rejected(error) => {
                        gitterm_gpui_chat::adapter::DeliveryOutcome::Rejected(error)
                    }
                    DeliveryOutcome::Uncertain(error) => {
                        gitterm_gpui_chat::adapter::DeliveryOutcome::Uncertain(error)
                    }
                };
                let receipt = self.adapter.delivery(id, &self.target, outcome)?;
                return Some(Update {
                    receipts: vec![receipt],
                    ..Update::default()
                });
            }
        };
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
        let launch = (|| -> Result<_> {
            let program = claude_program()?;
            let provider = Arc::new(Provider::new(preview, &program)?);
            provider.prepare()?;
            let command = provider.sign_in_command();
            let (runtime, events) = provider.start(saved.cwd.clone()).with_context(|| {
                format!(
                    "Sign in once with a personal Pro/Max account in the isolated root:\n{command}"
                )
            })?;
            Ok((runtime, events))
        })();
        let mut pane = Self {
            controller,
            saved,
            view,
            status: String::new(),
            pending: vec![],
            shutdown,
        };
        match launch {
            Ok((runtime, events)) => pane.install_runtime(runtime, events, window, cx),
            Err(error) => pane.status = format!("Claude preview unavailable: {error:#}"),
        }
        pane
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
    fn install_runtime(
        &mut self,
        runtime: OwnedRuntime,
        mut events: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.runtime = Some(runtime);
        let source = self.controller.source.clone();
        cx.spawn_in(window, async move |pane, cx| {
            while let Some(event) = events.recv().await {
                if pane
                    .update_in(cx, |pane, _, cx| {
                        if !pane
                            .controller
                            .adapter
                            .state()
                            .accepts_stream(&source, &pane.controller.target)
                        {
                            return;
                        }
                        if let RuntimeEvent::Harness(HarnessEvent::Error(error)) = &event {
                            pane.status = format!("Claude preview unavailable: {error}");
                        }
                        let exited = matches!(
                            &event,
                            RuntimeEvent::Harness(HarnessEvent::ProcessExited { .. })
                        );
                        if let Some(update) = pane.controller.apply_runtime_outcome(&source, event)
                        {
                            pane.pending.push(update);
                            pane.changed(cx);
                        }
                        if exited {
                            pane.shutdown_runtime();
                            pane.view.update(cx, |view, cx| {
                                view.set_read_only(Some("Claude preview stopped".into()), cx)
                            });
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        self.status = "Fresh isolated Claude preview · personal Pro/Max gate".into();
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

/// The lease travels with the runtime into the completion queue. It is released
/// only after completed() has joined the session thread, including cleanup.
struct OwnedRuntime {
    session: Session,
    _lease: Option<Arc<lease::ProviderLease>>,
}
impl std::ops::Deref for OwnedRuntime {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}
impl std::ops::DerefMut for OwnedRuntime {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}
#[cfg(test)]
impl From<Session> for OwnedRuntime {
    fn from(session: Session) -> Self {
        Self {
            session,
            _lease: None,
        }
    }
}

/// The application retains completion ownership after a pane is closed. Quit
/// drains synchronously before GPUI starts its 200 ms future timeout, as the
/// existing preview disk queue does. Ordinary close callbacks only enqueue.
pub(crate) struct RuntimeShutdown {
    sender: std::sync::mpsc::Sender<ShutdownCommand>,
}
enum ShutdownCommand {
    Session(OwnedRuntime),
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
    fn enqueue(&self, session: OwnedRuntime) -> std::result::Result<(), OwnedRuntime> {
        // A closed channel means the child has already exited. Completion still
        // owns snapshot cleanup and the thread join, so always queue the handle.
        session.close_turn_snapshots();
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
                    .label("Fresh local chat · resume, workers and remote unavailable")
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
