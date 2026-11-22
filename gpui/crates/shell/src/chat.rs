//! Native preview composition. The shared workspace owner retains these entities;
//! docking/hiding never installs another runtime receiver.
mod auth;
mod checkout;
mod fence;
mod lease;
mod managed;
mod navigation;
mod provider;
pub(crate) use navigation::{Navigation, navigation, worker_tab_target};
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
    },
    session,
};
use gitterm_gpui_chat::{
    ChatIntent, ChatView,
    adapter::{Admission, AttemptId, ChatAdapter, Receipt, Update},
    events::{Entry, Record},
};
use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Window, div,
};
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
        // As in Iced, Rust confirms the launch configuration before the first
        // provider acknowledgement. No picker is enabled until Ready.
        for event in [
            HarnessEvent::ModelChanged("default".into()),
            HarnessEvent::PermissionModeChanged("default".into()),
            HarnessEvent::EffortChanged {
                effort: None,
                applied: None,
            },
        ] {
            adapter.receive(
                &source,
                &target,
                AgentEvent::Harness(event),
                None,
                |_, _| {},
            );
        }
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
        let answer = match &intent {
            ChatIntent::Answer { answer, .. } => answer.as_ref(),
            intent => intent,
        };
        if matches!(
            answer,
            ChatIntent::Permission {
                decision: gitterm_gpui_chat::PermissionDecision::AllowAlways { .. },
                ..
            }
        ) {
            return (
                Receipt::rejected("Allow and remember · not saved in preview"),
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
            config: gitterm_core::chat::types::AgentBackendConfig::Claude {
                model: String::new(),
                permission_mode: Some("default".into()),
                effort: None,
            },
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
        // The initialize reply is an authoritative mode acknowledgement too.
        if let HarnessEvent::Ready {
            permission_mode: Some(mode),
            ..
        } = &event
        {
            self.adapter.receive(
                source,
                &self.target,
                AgentEvent::Harness(HarnessEvent::PermissionModeChanged(mode.clone())),
                Some(now_ms()),
                |_, _| {},
            );
        }
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

#[derive(Default)]
pub(crate) struct RosterProjection {
    pub rows: Vec<gitterm_gpui_chat::model::roster::RosterEntry>,
    pub cards: Vec<gitterm_gpui_chat::delegation::DelegationCard>,
}
impl RosterProjection {
    pub fn new(
        tasks: &[gitterm_core::tasks::TaskRecord],
        workers: &[gitterm_core::tasks::Delegation],
        tab: impl Fn(&gitterm_core::tasks::Delegation) -> gitterm_core::delegations::WorkerTab,
    ) -> Self {
        let rows = crate::roster_entries(tasks, workers, &tab);
        let mut workers: Vec<_> = workers.iter().collect();
        workers.sort_by_key(|worker| {
            std::cmp::Reverse(gitterm_core::work::summary::parse_stamp(&worker.created_at))
        });
        let cards = workers
            .into_iter()
            .map(|worker| {
                let task = match &worker.child {
                    gitterm_core::tasks::DelegationChild::TaskSession { task_id, .. } => {
                        tasks.iter().find(|task| &task.task_id == task_id)
                    }
                    _ => None,
                };
                gitterm_gpui_chat::delegation::DelegationCard {
                    delegation: worker.clone(),
                    activity: None,
                    held: false,
                    worker: matches!(
                        worker.child,
                        gitterm_core::tasks::DelegationChild::TaskSession { .. }
                    )
                    .then(|| {
                        gitterm_core::delegations::worker_card_info(worker, task, tab(worker))
                    }),
                }
            })
            .collect();
        Self { rows, cards }
    }
}

pub(crate) struct ChatPane {
    controller: Controller,
    saved: SavedChat,
    view: Entity<ChatView>,
    pending: Vec<Update>,
    shutdown: Arc<RuntimeShutdown>,
    _roster_subscription: gpui::Subscription,
    _review_subscription: gpui::Subscription,
    _review_context_load: Option<gpui::Task<()>>,
}
impl EventEmitter<SavedChat> for ChatPane {}
impl EventEmitter<gitterm_gpui_chat::model::roster::RosterOutcome> for ChatPane {}
impl ChatPane {
    #[cfg(test)]
    pub(crate) fn test_view(&self) -> Entity<ChatView> {
        self.view.clone()
    }
    pub(crate) fn set_width(
        &mut self,
        width: gitterm_core::settings::ChatWidth,
        cx: &mut Context<Self>,
    ) {
        self.view.update(cx, |view, cx| view.set_width(width, cx));
        cx.notify();
    }
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
            view.set_appearance(crate::theme::chat_appearance(cx), cx);
            view.set_width(crate::theme::chat_width(cx), cx);
            view.set_preview(cx);
            view.set_backend(gitterm_core::chat::types::AgentBackend::Claude, cx);
            view.set_read_only(Some("Preview activation unavailable".into()), cx);
            view
        });
        let roster_subscription = Self::forward_roster(&view, cx);
        view.update(cx, |view, cx| view.set_dismiss_gate(dismiss_gate(), cx));
        let review_subscription = Self::handle_review(&view, saved.cwd.clone(), window, cx);
        let callback = Self::intent_callback(cx.weak_entity());
        view.update(cx, |view, cx| view.set_intent_callback(callback, cx));
        let source = checkout::source(
            saved.cwd.clone(),
            cx.try_global::<crate::work_sidebar::IcedWorkSource>()
                .map(|source| source.config.clone()),
        );
        view.update(cx, |view, cx| view.set_checkout_source(source, cx));
        // The caller is this pane's target, fixed here before the gate and
        // launch run; nothing later can re-attribute the chat's task or
        // browser calls.
        let mcp_servers = mcp_servers(&controller.target.session_uid, cx);
        let launch = (|| -> Result<_> {
            let program = claude_program()?;
            let provider = Arc::new(Provider::new(preview, &program)?);
            provider.prepare()?;
            let (runtime, events) = start_runtime(&provider, saved.cwd.clone(), mcp_servers)?;
            Ok((runtime, events))
        })();
        let mut pane = Self {
            controller,
            saved,
            view,
            pending: vec![],
            shutdown,
            _roster_subscription: roster_subscription,
            _review_subscription: review_subscription,
            _review_context_load: None,
        };
        match launch {
            Ok((runtime, events)) => pane.install_runtime(runtime, events, window, cx),
            Err(error) => pane.view.update(cx, |view, cx| {
                view.set_status_error(format!("Claude unavailable: {error:#}"), cx)
            }),
        }
        pane
    }
    /// Shell navigation subscribes to ChatPane's typed roster events. The view
    /// has already hidden Dismiss locally; Open retains the host's worker id.
    fn forward_roster(view: &Entity<ChatView>, cx: &mut Context<Self>) -> gpui::Subscription {
        cx.subscribe(
            view,
            |_, _, outcome: &gitterm_gpui_chat::model::roster::RosterOutcome, cx| {
                cx.emit(outcome.clone());
            },
        )
    }
    /// The popover emits requests only. Preview cannot start a runner or mutate
    /// Iced's task store, so rejection leaves its inputs and chat draft intact.
    fn handle_review(
        view: &Entity<ChatView>,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        let config = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .map(|source| source.config.clone());
        cx.subscribe_in(
            view,
            window,
            move |pane, view, outcome: &gitterm_gpui_chat::review::ReviewOutcome, window, cx| {
                use gitterm_gpui_chat::review::{ReviewContext, ReviewOutcome};
                match outcome {
                    ReviewOutcome::Submit { .. } => {
                        view.update(cx, |view, cx| {
                            view.show_review_error(
                                "Review / Consult is not available in preview".into(),
                                cx,
                            )
                        });
                    }
                    ReviewOutcome::ContextRequested => {
                        let cwd = cwd.clone();
                        let config = config.clone();
                        let load = cx.background_executor().spawn(async move {
                            let context = gitterm_core::review_request::review_context(&cwd, None);
                            ReviewContext {
                                base: context.base,
                                base_source: context.base_source,
                                ahead: context.ahead,
                                head: context.head,
                                dirty: context.dirty,
                                error: context.error,
                                config: config
                                    .map(|config| {
                                        crate::workspace_config::load_iced_settings_from(&config)
                                            .review
                                    })
                                    .unwrap_or_default(),
                            }
                        });
                        let view = view.downgrade();
                        // Reopening replaces the task, so an earlier context
                        // cannot reset a newer opening's draft.
                        pane._review_context_load =
                            Some(cx.spawn_in(window, async move |_, cx| {
                                let context = load.await;
                                let _ = view.update_in(cx, |view, window, cx| {
                                    view.set_review_context(context, window, cx)
                                });
                            }));
                    }
                }
            },
        )
    }
    fn intent_callback(weak: gpui::WeakEntity<Self>) -> gitterm_gpui_chat::IntentCallback {
        Rc::new(move |intent, cx| {
            weak.update(cx, |pane, cx| pane.dispatch(intent, cx))
                .unwrap_or_else(|_| Receipt::rejected("Preview chat closed"))
        })
    }
    fn dispatch(&mut self, intent: ChatIntent, cx: &mut Context<Self>) -> Receipt {
        let (receipt, update) = self.controller.dispatch(intent);
        self.pending.push(update);
        self.changed(cx);
        receipt
    }
    /// Sends `text` as a user turn the way the composer does, for the task
    /// host's result delivery. The receipt is the composer's own:
    /// `ChannelAccepted` means the runtime took the prompt, as Iced's
    /// `submit_prompt` Ok does; any other state means nothing was sent.
    pub(crate) fn submit_prompt(&mut self, text: String, cx: &mut Context<Self>) -> Receipt {
        self.dispatch(
            ChatIntent::Send {
                text,
                images: vec![],
            },
            cx,
        )
    }
    pub fn history(
        shutdown: Arc<RuntimeShutdown>,
        saved: SavedChat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let controller = Controller::new();
        let config = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .and_then(|source| source.owner.upgrade())
            .and_then(|owner| {
                let owner = owner.read(cx);
                let catalog = owner.iced_catalog()?;
                saved_history_config(&catalog.workspaces, saved.session_id.as_deref()).cloned()
            });
        let view = cx.new(|cx| {
            let mut view = ChatView::new(saved.records.clone(), window, cx);
            if let Some(config) = &config {
                view.set_history_config(config, cx);
            }
            view.set_appearance(crate::theme::chat_appearance(cx), cx);
            view.set_width(crate::theme::chat_width(cx), cx);
            view.set_preview(cx);
            view.set_backend(
                config
                    .as_ref()
                    .map_or(gitterm_core::chat::types::AgentBackend::Claude, |config| {
                        config.backend()
                    }),
                cx,
            );
            view.set_read_only(
                Some("Preview history · read only · resume not in preview yet".into()),
                cx,
            );
            view
        });
        let roster_subscription = Self::forward_roster(&view, cx);
        view.update(cx, |view, cx| view.set_dismiss_gate(dismiss_gate(), cx));
        let review_subscription = Self::handle_review(&view, saved.cwd.clone(), window, cx);
        Self {
            controller,
            saved,
            view,
            pending: vec![],
            shutdown,
            _roster_subscription: roster_subscription,
            _review_subscription: review_subscription,
            _review_context_load: None,
        }
    }
    pub(crate) fn set_roster(&self, projection: RosterProjection, cx: &mut Context<Self>) {
        self.view.update(cx, |view, cx| {
            view.set_delegations(projection.cards, cx);
            view.set_roster(projection.rows, cx);
        });
    }

    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.saved.cwd
    }
    pub(crate) fn show_notice(&self, notice: String, cx: &mut Context<Self>) {
        self.view
            .update(cx, |view, cx| view.set_status_error(notice, cx));
    }
    pub(crate) fn is_working(&self) -> bool {
        matches!(
            self.controller.adapter.state().state,
            gitterm_core::chat::types::AgentSessionState::Streaming
        )
    }
    /// The provider chat id Iced keys its turn snapshots by.
    pub(crate) fn chat_session_id(&self) -> Option<String> {
        self.saved.session_id.clone()
    }
    pub(crate) fn matches_chat(&self, id: &str) -> bool {
        self.saved.uid == id || self.saved.session_id.as_deref() == Some(id)
    }
    pub fn target(&self) -> session::Target {
        self.controller.target.clone()
    }
    /// Preview panes start without a task; host tests bind one to a task
    /// session, as a launched task session pane will be.
    #[cfg(test)]
    pub(crate) fn bind_task_session(&mut self, task_id: &str, task_session_id: &str) {
        self.controller.target.task_id = Some(task_id.into());
        self.controller.target.task_session_id = Some(task_session_id.into());
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.controller.ready = false;
        self.shutdown_runtime();
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
                AgentEvent::Other(value)
                    if value["type"] == "user_prompt" && value.get("images").is_some() =>
                {
                    Some(Record {
                        at_ms: None,
                        entry: match serde_json::from_value(value.clone()) {
                            Ok(prompt) => Entry::SubmittedPrompt(prompt),
                            Err(_) => Entry::Harness(HarnessEvent::Error(
                                "Invalid saved image prompt".into(),
                            )),
                        },
                    })
                }
                AgentEvent::Other(value) if value["type"] == "user_prompt" => {
                    value["text"].as_str().map(|text| Record {
                        at_ms: None,
                        entry: Entry::UserPrompt(text.into()),
                    })
                }
                AgentEvent::Other(value) if value["kind"] == "turn_timing" => Some(Record {
                    at_ms: None,
                    entry: Entry::TurnTiming {
                        start_ms: value["start_ms"].as_u64(),
                        end_ms: value["end_ms"].as_u64(),
                        split_ms: value["split_ms"].as_u64(),
                    },
                }),
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
        self.view
            .update(cx, |view, cx| view.set_read_only(None, cx));
    }
}

#[cfg(test)]
impl ChatPane {
    /// Starts this pane's runtime on the fake Claude `program` in the pane's
    /// folder, through the fixture provider's probe and launch.
    pub(crate) fn start_fake_runtime(
        &mut self,
        preview: Arc<gitterm_core::preview::Preview>,
        program: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let provider = Arc::new(Provider::fixture(preview, program)?);
        let started = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(provider.fake_start(self.saved.cwd.clone(), vec![]))?;
        let (runtime, events) = started.context("the fake Claude reported no sign-in")?;
        self.install_runtime(runtime, events, window, cx);
        Ok(())
    }
    /// The provider acknowledged the launch.
    pub(crate) fn runtime_ready(&self) -> bool {
        self.controller.ready
    }
    /// The user turns this chat recorded, in order.
    pub(crate) fn user_prompts(&self) -> Vec<String> {
        self.saved
            .records
            .iter()
            .filter_map(|record| match &record.entry {
                Entry::UserPrompt(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

fn saved_history_config<'a>(
    workspaces: &'a [gitterm_core::workspace_config::WorkspaceConfig],
    session_id: Option<&str>,
) -> Option<&'a gitterm_core::chat::types::AgentBackendConfig> {
    let session_id = session_id?;
    workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find(|tab| tab.chat_session_id.as_deref() == Some(session_id))?
        .agent_config
        .as_ref()
}

#[cfg(test)]
mod history_projection_tests {
    use super::*;

    #[test]
    fn saved_chips_match_conversation_identity_instead_of_checkout() {
        let workspaces = serde_json::from_value::<Vec<gitterm_core::workspace_config::WorkspaceConfig>>(serde_json::json!([
            {"name":"Work", "abbrev":"W", "dir":"/fixture", "color":"green", "tabs":[
                {"dir":"/fixture", "chat_session_id":"other", "agent_config":{"backend":"claude", "model":"sonnet"}},
                {"dir":"/fixture", "chat_session_id":"saved", "agent_config":{"backend":"claude", "model":"opus", "permission_mode":"auto", "effort":"high"}}
            ]}
        ])).unwrap();
        let config = saved_history_config(&workspaces, Some("saved")).unwrap();
        assert_eq!(config.chat_model(), Some("opus"));
        assert_eq!(config.chat_effort(), Some("high"));
        assert!(
            matches!(config, gitterm_core::chat::types::AgentBackendConfig::Claude { permission_mode: Some(mode), .. } if mode == "auto")
        );
        assert!(saved_history_config(&workspaces, Some("missing")).is_none());
        assert!(saved_history_config(&workspaces, None).is_none());
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
        self.view.update(cx, |view, cx| {
            view.set_appearance(crate::theme::chat_appearance(cx), cx);
            view.set_width(crate::theme::chat_width(cx), cx);
        });
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
            .on_action(
                cx.listener(|pane, _: &crate::app_menus::Escape, window, cx| {
                    if !pane
                        .view
                        .update(cx, |view, cx| view.dismiss_review(window, cx))
                    {
                        cx.propagate();
                    }
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}
fn dismiss_gate() -> gitterm_gpui_chat::interactions::DismissGate {
    Rc::new(|request, cx| {
        dismiss_notice(
            crate::task_panel::host(cx).map(|host| host.services()),
            request,
        )
    })
}

/// Iced `delegation_dismiss` runs `Intent::Dismiss`: abort a live run, then
/// persist the dismissal. The host's `ServiceContext` decides each effect;
/// the preview denies both, so nothing is stopped or written. The notice
/// keeps the button's label, as before. Without a host it is the same notice.
// shortcut: an allowed effect is reported, not run; B9–B11 add the executor.
pub(crate) fn dismiss_notice(
    services: Option<&gitterm_core::preview::service::ServiceContext>,
    request: &gitterm_gpui_chat::interactions::DismissRequest,
) -> String {
    use gitterm_core::delegation_state::Effect;
    use gitterm_gpui_chat::interactions::preview_notice;
    let Some(services) = services else {
        return preview_notice(&request.label);
    };
    let abort = request.active.then(|| Effect::Abort(request.id.clone()));
    let persist = Effect::PersistDismiss {
        id: request.id.clone(),
        cancel: request.active,
    };
    if abort
        .iter()
        .chain([&persist])
        .any(|effect| services.delegation_effect(effect, None).is_err())
    {
        preview_notice(&request.label)
    } else {
        format!("{} · not wired in the GPUI preview", request.label)
    }
}

/// The running preview task host's task and browser servers for a chat
/// attributed to `caller`. Both gate every call server-side. Without a host
/// the chat launches with no MCP server, and the first such launch says why.
fn mcp_servers(
    caller: &str,
    cx: &gpui::App,
) -> Vec<gitterm_core::harness::claude::ClaudeMcpServer> {
    let servers = cx
        .try_global::<crate::task_host::GpuiTaskHost>()
        .map_or_else(Vec::new, |host| host.0.read(cx).claude_mcp_servers(caller));
    if servers.is_empty() {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            eprintln!(
                "Claude preview chats launch without task or browser tools: the preview task host is not running (see its startup error)"
            )
        });
    }
    servers
}
fn claude_program() -> Result<PathBuf> {
    find_claude(
        std::env::var_os("PATH"),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}
/// Resolve only; neither --version nor any real CLI subcommand runs here.
/// A Finder/Dock launch gets a minimal PATH, so also search the install
/// folders Iced adds for terminal tabs (`src/harness/claude.rs` `extra_bin_dirs`).
fn find_claude(path: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Result<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = home.filter(|home| home.is_absolute()) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("claude"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!("Claude executable not found in PATH or the usual install folders")
        })
}

#[cfg(test)]
mod appearance_tests {

    #[gpui::test]
    fn loaded_chat_font_is_independent_and_theme_refresh_retains_it(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let settings = crate::settings::FixtureSettings {
                theme: "light".into(),
                chat_font_size: 19.,
                ui_font_size: 12.,
                terminal_font_size: 17.,
                ..Default::default()
            };
            crate::theme::apply_settings(&settings, None, cx);
            assert_eq!(
                crate::theme::chat_appearance(cx),
                gitterm_gpui_chat::ChatAppearance {
                    light: true,
                    font_size: 19.
                }
            );
            crate::theme::apply(true, None, cx);
            assert_eq!(
                crate::theme::chat_appearance(cx),
                gitterm_gpui_chat::ChatAppearance {
                    light: false,
                    font_size: 19.
                }
            );
            let updated = crate::settings::FixtureSettings {
                chat_font_size: 22.,
                ..settings
            };
            crate::theme::apply_settings(&updated, None, cx);
            assert_eq!(crate::theme::chat_appearance(cx).font_size, 22.);
        });
    }
}

/// Starts the runtime. A folder refusal is reported alone: the sign-in hint
/// is only attached to failures where sign-in can be the cause.
fn start_runtime(
    provider: &Arc<Provider>,
    cwd: PathBuf,
    mcp_servers: Vec<gitterm_core::harness::claude::ClaudeMcpServer>,
) -> Result<(
    OwnedRuntime,
    tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
)> {
    provider.validate_workspace(&cwd)?;
    let command = provider.sign_in_command();
    provider.clone().start(cwd, mcp_servers).with_context(|| {
        format!("Sign in once with a personal Pro/Max account in the isolated root:\n{command}")
    })
}
