//! Native chat composition. The shared workspace owner retains these entities;
//! docking/hiding never installs another runtime receiver.
mod auth;
mod checkout;
mod fence;
mod lease;
mod managed;
mod navigation;
pub(crate) mod policy;
mod provider;
pub(crate) mod remote;
pub(crate) mod resume;
pub(crate) use navigation::{Navigation, navigation, worker_tab_target};
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod remote_tests;
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
    live: bool,
    remote: bool,
    message: u64,
    correlations: HashMap<CommandCorrelation, AttemptId>,
    config: gitterm_core::chat::types::AgentBackendConfig,
}
impl Controller {
    fn new() -> Self {
        let mut adapter = ChatAdapter::default();
        let target = crate::sessions::fresh_target();
        let source = adapter.begin_runtime(target.clone());
        // Rust confirms the launch configuration before the first provider
        // acknowledgement; Live replaces these defaults on first submission.
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
            live: false,
            remote: false,
            message: 0,
            correlations: HashMap::new(),
            config: resume::default_config(),
        }
    }
    fn seed_launch(&mut self, settings: &gitterm_gpui_chat::composer::LaunchSettings) {
        self.config = gitterm_core::chat::types::AgentBackendConfig::Claude {
            model: settings.model.clone(),
            permission_mode: Some(settings.permission_mode.clone()),
            effort: settings.effort.clone(),
        };
        for event in [
            HarnessEvent::ModelChanged(settings.model.clone()),
            HarnessEvent::PermissionModeChanged(settings.permission_mode.clone()),
            HarnessEvent::EffortChanged {
                effort: settings.effort.clone(),
                applied: None,
            },
        ] {
            self.adapter.receive(
                &self.source,
                &self.target,
                AgentEvent::Harness(event),
                None,
                |_, _| {},
            );
        }
    }
    fn dispatch(&mut self, intent: ChatIntent) -> (Receipt, Update) {
        let answer = match &intent {
            ChatIntent::Answer { answer, .. } => answer.as_ref(),
            intent => intent,
        };
        if !self.remote
            && matches!(
                answer,
                ChatIntent::Permission {
                    decision: gitterm_gpui_chat::PermissionDecision::AllowAlways { .. },
                    ..
                }
            )
        {
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
            config: self.config.clone(),
            process_running: (self.ready || self.live) && self.runtime.is_some(),
            just_spawned: self.live && !self.ready && self.message == 1,
            message_id: format!("{}-{}", self.target.session_uid, self.message),
        };
        let prepared = match self.adapter.admit(intent, facts) {
            Ok(prepared) => prepared,
            Err(receipt) => return (receipt, Update::default()),
        };
        let correlation = CommandCorrelation(self.message);
        if self
            .runtime
            .as_ref()
            .is_some_and(|r| r.supports_correlation())
        {
            self.correlations.insert(correlation, prepared.id());
        }
        let runtime = self.runtime.as_ref();
        let remote = self.remote;
        let result = self
            .adapter
            .execute(prepared, &self.target, now_ms(), |command| {
                let runtime = runtime.ok_or_else(|| "No preview runtime".to_string())?;
                if remote {
                    runtime.send(command)
                } else {
                    runtime.send_correlated(command, correlation)
                }
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
        let config = &mut self.config;
        let mut observe = |_: &AgentEvent, effects: &gitterm_core::chat::state::EventEffects| {
            use gitterm_core::chat::types::{AgentBackendConfig, SettingChange};
            if let AgentBackendConfig::Claude {
                model,
                permission_mode,
                effort,
            } = config
            {
                match &effects.settings_changed {
                    Some(SettingChange::Model(value)) => model.clone_from(value),
                    Some(SettingChange::Mode(value)) => *permission_mode = Some(value.clone()),
                    Some(SettingChange::Effort(value)) => effort.clone_from(value),
                    None => {}
                }
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
                &mut observe,
            );
        }
        let ready = matches!(event, HarnessEvent::Ready { .. });
        let exited = matches!(event, HarnessEvent::ProcessExited { .. });
        let update = self.adapter.receive(
            source,
            &self.target,
            AgentEvent::Harness(event),
            Some(now_ms()),
            observe,
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
                    checkout_head: None,
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

/// Each card's delegation id and record state, in roster order.
pub(crate) fn card_states(
    cards: &[gitterm_gpui_chat::delegation::DelegationCard],
) -> Vec<(String, String)> {
    cards
        .iter()
        .map(|card| {
            (
                card.delegation.delegation_id.clone(),
                card.delegation.status.label().to_owned(),
            )
        })
        .collect()
}

/// `cards` with `held` set on those whose delegation id is in `held`.
pub(crate) fn with_held(
    cards: &[gitterm_gpui_chat::delegation::DelegationCard],
    held: &std::collections::HashSet<String>,
) -> Vec<gitterm_gpui_chat::delegation::DelegationCard> {
    cards
        .iter()
        .cloned()
        .map(|mut card| {
            card.held = held.contains(&card.delegation.delegation_id);
            card
        })
        .collect()
}

struct DeferredLive {
    services: gitterm_core::preview::service::ServiceContext,
    preview_root: PathBuf,
    caller: String,
    workspace: Option<crate::data::WorkspaceId>,
    settings: Rc<std::cell::RefCell<gitterm_gpui_chat::composer::LaunchSettings>>,
}

pub(crate) struct ChatPane {
    controller: Controller,
    pub(crate) notification_attention: crate::notifications::ChatAttention,
    deferred_live: Option<DeferredLive>,
    remote: Option<remote::Binding>,
    guidelines_status: Option<std::sync::mpsc::Receiver<bool>>,
    live_claim: Option<Arc<provider::ConversationClaim>>,
    live_tab: Option<resume::LiveTab>,
    resume_error: Option<String>,
    awaiting_resume: bool,
    history_loading: bool,
    persisted_id: Option<String>,
    _history_load: Option<gpui::Task<()>>,
    saved: SavedChat,
    view: Entity<ChatView>,
    pending: Vec<Update>,
    shutdown: Arc<RuntimeShutdown>,
    /// The roster's cards as last projected, before `held` is applied.
    cards: Vec<gitterm_gpui_chat::delegation::DelegationCard>,
    /// Delegations whose result waits for this chat's turn end.
    held: std::collections::HashSet<String>,
    /// The cards' ids and states when the checkout's HEAD was last read.
    head_read_for: Vec<(String, String)>,
    _head_load: Option<gpui::Task<()>>,
    _release: gpui::Subscription,
    _roster_subscription: gpui::Subscription,
    _delegation_subscription: gpui::Subscription,
    _review_subscription: gpui::Subscription,
    _review_context_load: Option<gpui::Task<()>>,
    created_prs: gitterm_core::work::pr_created::CreatedPrs,
}
/// The provider finished a turn (Iced's `HarnessEvent::TurnCompleted`).
#[derive(Clone, Debug)]
pub(crate) struct TurnEnded(pub Option<gitterm_core::chat::types::RateLimitReading>);
impl EventEmitter<SavedChat> for ChatPane {}
impl EventEmitter<TurnEnded> for ChatPane {}
impl EventEmitter<gitterm_gpui_chat::model::roster::RosterOutcome> for ChatPane {}
impl ChatPane {
    #[cfg(feature = "stt")]
    pub(crate) fn dictation_view(&self) -> Entity<ChatView> {
        self.view.clone()
    }
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
        let mut controller = Controller::new();
        let policy = policy::current(preview.clone(), cx);
        controller.live = matches!(policy, policy::ChatLaunchPolicy::Live { .. });
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
            if controller.live {
                view.set_live_prelaunch(cx);
            } else {
                view.set_read_only(Some("Preview activation unavailable".into()), cx);
            }
            view
        });
        let roster_subscription = Self::forward_roster(&view, cx);
        let delegation_subscription = Self::handle_delegation(&view, cx);
        view.update(cx, |view, cx| {
            view.set_dismiss_gate(dismiss_gate(controller.live), cx);
            if controller.live {
                view.enable_delegation_actions(cx);
            }
        });
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
        let (deferred_live, launch) = match policy {
            policy::ChatLaunchPolicy::Live { services } => (
                Some(DeferredLive {
                    services,
                    preview_root: preview.root().to_path_buf(),
                    caller: controller.target.session_uid.clone(),
                    workspace: cx
                        .try_global::<crate::work_sidebar::IcedWorkSource>()
                        .and_then(|source| source.owner.upgrade())
                        .and_then(|owner| owner.read(cx).model.active()),
                    settings: view.read(cx).launch_settings_handle(),
                }),
                None,
            ),
            policy::ChatLaunchPolicy::Preview(preview) => {
                let mcp_servers = mcp_servers(&controller.target.session_uid, cx);
                let launch = (|| -> Result<_> {
                    let program = claude_program()?;
                    let provider = Arc::new(Provider::new(preview, &program)?);
                    provider.prepare()?;
                    start_runtime(&provider, saved.cwd.clone(), mcp_servers)
                })();
                (None, Some(launch))
            }
        };
        let mut pane = Self {
            controller,
            deferred_live,
            remote: None,
            notification_attention: Default::default(),
            guidelines_status: None,
            live_claim: None,
            live_tab: None,
            resume_error: None,
            awaiting_resume: false,
            history_loading: false,
            persisted_id: None,
            _history_load: None,
            saved,
            view,
            pending: vec![],
            shutdown,
            cards: vec![],
            held: Default::default(),
            head_read_for: vec![],
            _head_load: None,
            _release: cx.on_release(|pane, cx| pane.quit_runtime(cx)),
            _roster_subscription: roster_subscription,
            _delegation_subscription: delegation_subscription,
            _review_subscription: review_subscription,
            _review_context_load: None,
            created_prs: Default::default(),
        };
        if pane.controller.live {
            pane.init_live_tab(cx);
        }
        match launch {
            None => {}
            Some(Ok((runtime, events))) => pane.attach_runtime(runtime, events, cx),
            Some(Err(error)) => {
                pane.shutdown_runtime(cx);
                pane.view.update(cx, |view, cx| {
                    view.set_status_error(format!("Claude unavailable: {error:#}"), cx)
                });
            }
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
    fn handle_delegation(view: &Entity<ChatView>, cx: &mut Context<Self>) -> gpui::Subscription {
        cx.subscribe(
            view,
            |pane, _, action: &gitterm_gpui_chat::interactions::TranscriptAction, cx| {
                let tab_id = pane.target().tab_id;
                let action = action.clone();
                // The host may synchronously submit into this pane. Release both
                // the view and pane updates before asking it to execute the action.
                cx.defer(move |cx| {
                    if let Some(host) = cx
                        .try_global::<crate::task_host::GpuiTaskHost>()
                        .map(|h| h.0.clone())
                    {
                        host.update(cx, |host, cx| host.card_action(tab_id, action, cx));
                    }
                });
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
        if self.history_loading {
            return Receipt::rejected("Saved chat history is still loading");
        }
        if let Some(error) = &self.resume_error {
            return Receipt::rejected(error.clone());
        }
        if self.deferred_live.is_some()
            && self.controller.runtime.is_none()
            && matches!(&intent, ChatIntent::Send { .. })
        {
            if let ChatIntent::Send { text, images } = &intent {
                if text.trim().is_empty() && images.is_empty() {
                    return Receipt::rejected("The prompt is empty");
                }
                if let Err(error) = gitterm_core::chat::types::validate_images(images) {
                    return Receipt::rejected(error);
                }
            }
            if let Err(error) = self.launch_live(cx) {
                self.shutdown_runtime(cx);
                let error = format!("Claude unavailable: {error:#}");
                let view = self.view.clone();
                let status = error.clone();
                let missing_cwd = self.remote.is_none()
                    && self.saved.session_id.is_some()
                    && !self.saved.cwd.is_dir();
                if missing_cwd {
                    self.resume_error = Some(error.clone());
                    cx.notify();
                }
                cx.defer(move |cx| {
                    view.update(cx, |view, cx| {
                        if missing_cwd {
                            view.set_read_only(Some(status.clone()), cx);
                        }
                        view.set_status_error(status, cx)
                    })
                });
                return Receipt::rejected(error);
            }
        }
        let sending = matches!(&intent, ChatIntent::Send { .. });
        let (receipt, update) = self.controller.dispatch(intent);
        if sending && receipt.state == gitterm_gpui_chat::adapter::ReceiptState::ChannelAccepted {
            self.notification_attention.working();
        }
        self.pending.push(update);
        self.changed(cx);
        receipt
    }
    fn launch_live(&mut self, cx: &mut Context<Self>) -> Result<()> {
        if self.remote.is_some() {
            return self.launch_remote(None, cx);
        }
        let launch = self
            .deferred_live
            .as_ref()
            .context("Live chat already launched")?;
        // Resolve descriptors while the caller and matching host are still on
        // the UI thread, before the runtime does any asynchronous preparation.
        let servers = live_mcp_servers(&launch.services, &launch.caller, cx)?;
        let settings = launch.settings.borrow().clone();
        let workspace = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .and_then(|source| source.owner.upgrade())
            .and_then(|owner| {
                let owner = owner.read(cx);
                owner.iced_workspace(launch.workspace?).cloned()
            });
        let (status, receiver) = std::sync::mpsc::channel();
        let mut config = live_config(
            self.saved.cwd.clone(),
            &settings,
            servers,
            workspace
                .as_ref()
                .is_some_and(|workspace| workspace.claude_in_chrome),
        );
        config.resume = self.saved.session_id.clone();
        let claim = match &self.live_claim {
            Some(claim) => claim.clone(),
            None => Arc::new(provider::ConversationClaim::claim(
                &launch.services,
                &launch.caller,
            )?),
        };
        if let Some(id) = &config.resume {
            claim.provider_id(id)?;
        }
        let (runtime, events) = provider::start_live_claimed(
            &launch.services,
            config,
            launch.preview_root.clone(),
            claim.clone(),
            provider::LiveInstructions {
                workspace_off: workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.guidelines_off),
                places: workspace
                    .as_ref()
                    .and_then(gitterm_core::guidelines::config_document_places),
                status,
            },
        )?;
        if let Some(tab) = &mut self.live_tab {
            tab.tab.agent_config = Some(gitterm_core::chat::types::AgentBackendConfig::Claude {
                model: settings.model.clone(),
                permission_mode: Some(settings.permission_mode.clone()),
                effort: settings.effort.clone(),
            });
        }
        self.live_claim = Some(claim);
        self.controller.seed_launch(&settings);
        self.guidelines_status = Some(receiver);
        self.attach_runtime(runtime, events, cx);
        Ok(())
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
        let delegation_subscription = Self::handle_delegation(&view, cx);
        view.update(cx, |view, cx| {
            view.set_dismiss_gate(dismiss_gate(false), cx)
        });
        let review_subscription = Self::handle_review(&view, saved.cwd.clone(), window, cx);
        Self {
            controller,
            deferred_live: None,
            remote: None,
            notification_attention: Default::default(),
            guidelines_status: None,
            live_claim: None,
            live_tab: None,
            resume_error: None,
            awaiting_resume: false,
            history_loading: false,
            persisted_id: None,
            _history_load: None,
            saved,
            view,
            pending: vec![],
            shutdown,
            cards: vec![],
            held: Default::default(),
            head_read_for: vec![],
            _head_load: None,
            _release: cx.on_release(|pane, cx| pane.quit_runtime(cx)),
            _roster_subscription: roster_subscription,
            _delegation_subscription: delegation_subscription,
            _review_subscription: review_subscription,
            _review_context_load: None,
            created_prs: Default::default(),
        }
    }
    pub(crate) fn set_roster(&mut self, projection: RosterProjection, cx: &mut Context<Self>) {
        self.cards = projection.cards;
        self.show_cards(cx);
        // Iced re-reads HEAD when a delegation finishes and when the page
        // (re)loads with cards; here, when any card appears or changes state.
        let states = card_states(&self.cards);
        if states != self.head_read_for {
            self.head_read_for = states;
            self.refresh_checkout_head(cx);
        }
        self.view
            .update(cx, |view, cx| view.set_roster(projection.rows, cx));
    }

    /// The delegations whose result is held for this chat's turn end, as
    /// core's delivery state has them; their cards show it.
    pub(crate) fn set_delivery_held(
        &mut self,
        held: std::collections::HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        if self.held != held {
            self.held = held;
            self.show_cards(cx);
        }
    }

    /// Iced's `refresh_delegation_head`: read the checkout's HEAD off the
    /// main thread, for the cards' "branch has moved" banner. Read-only git;
    /// nothing runs without cards. A failed read is logged, as Iced does.
    fn refresh_checkout_head(&mut self, cx: &mut Context<Self>) {
        if self.remote.is_some() {
            return;
        }
        if self.cards.is_empty() {
            return;
        }
        let cwd = self.saved.cwd.clone();
        let read = cx
            .background_executor()
            .spawn(async move { gitterm_core::delegations::current_head(&cwd) });
        let view = self.view.downgrade();
        self._head_load = Some(cx.spawn(async move |_, cx| match read.await {
            Ok(head) => {
                let _ = view.update(cx, |view, cx| view.set_checkout_head(Some(head), cx));
            }
            Err(error) => eprintln!("[delegation] chat checkout: {error}"),
        }));
    }

    fn show_cards(&self, cx: &mut Context<Self>) {
        let cards = with_held(&self.cards, &self.held);
        self.view
            .update(cx, |view, cx| view.set_delegations(cards, cx));
    }

    #[cfg(test)]
    pub(crate) fn delivery_held(&self) -> &std::collections::HashSet<String> {
        &self.held
    }

    pub(crate) fn config(&self) -> &gitterm_core::chat::types::AgentBackendConfig {
        &self.controller.config
    }
    pub(crate) fn is_remote(&self) -> bool {
        self.remote.is_some()
    }
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.saved.cwd
    }
    /// Iced's `__reviewNote`: why an action was not done, as a warning note
    /// in the transcript.
    pub(crate) fn show_notice(&self, notice: String, cx: &mut Context<Self>) {
        self.view
            .update(cx, |view, cx| view.note_warning(notice, cx));
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
        if let Err(error) = self.stop_restoring_remote() {
            eprintln!("Stop restoring remote chat: {error:#}");
            self.show_notice(format!("Stop restoring remote chat: {error:#}"), cx);
            return;
        }
        self.deferred_live = None;
        self._history_load = None;
        self.history_loading = false;
        self.live_claim = None;
        self.controller.ready = false;
        self.shutdown_runtime(cx);
        self.view.update(cx, |view, cx| {
            view.set_read_only(Some("Chat closed".into()), cx)
        });
        cx.notify();
    }
    pub(crate) fn quit_runtime(&mut self, cx: &gpui::App) {
        self._history_load = None;
        self.history_loading = false;
        self.deferred_live = None;
        self.detach_remote_runtime();
        self.shutdown_runtime(cx);
    }
    fn detach_remote_runtime(&mut self) {
        if self.controller.remote
            && let Some(runtime) = self.controller.runtime.as_mut()
        {
            runtime.request_shutdown(); // Closing this port makes the queued Shutdown a no-op.
        }
    }
    pub(crate) fn shutdown_runtime(&mut self, cx: &gpui::App) {
        crate::task_host::revoke_caller(&self.controller.target.session_uid, cx);
        self.enqueue_shutdown();
    }
    fn enqueue_shutdown(&mut self) {
        self.controller.ready = false;
        if let Some(mut runtime) = self.controller.runtime.take() {
            if self.controller.remote {
                // Iced sends Stop then drops the handle. Closing the port lets
                // core bound delivery on a lost connection instead of reaping forever.
                let _ = runtime.send(gitterm_core::chat::types::HarnessCommand::Shutdown);
                runtime.request_shutdown();
            }
            if let Err(runtime) = self.shutdown.enqueue(runtime) {
                self.controller.runtime = Some(runtime);
                eprintln!(
                    "Close preview Claude: shutdown queue stopped; retaining runtime ownership"
                );
            }
        }
    }
    fn changed(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.controller.adapter.state().session_id.clone() {
            self.saved.session_id = Some(id);
        }
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
        if !self.controller.live {
            cx.emit(self.saved.clone());
        }
        cx.notify();
    }
    pub(crate) fn receive_runtime_event(
        &mut self,
        source: &RuntimeStamp,
        event: RuntimeEvent,
        cx: &mut Context<Self>,
    ) {
        if !self
            .controller
            .adapter
            .state()
            .accepts_stream(source, &self.controller.target)
        {
            return;
        }
        let history_gap = matches!(
            &event,
            RuntimeEvent::Harness(HarnessEvent::RemoteReconnected {
                history_gap: true,
                ..
            })
        );
        let exited = matches!(
            &event,
            RuntimeEvent::Harness(HarnessEvent::ProcessExited { .. })
        );
        let rate_limit = match &event {
            RuntimeEvent::Harness(HarnessEvent::TurnCompleted { rate_limit, .. }) => {
                rate_limit.clone()
            }
            _ => None,
        };
        let turn_ended = matches!(
            &event,
            RuntimeEvent::Harness(HarnessEvent::TurnCompleted { .. })
        );
        if let Some(applied) = self
            .guidelines_status
            .as_ref()
            .and_then(|status| status.try_recv().ok())
        {
            self.view
                .update(cx, |view, cx| view.set_guidelines_status(applied, cx));
        }
        if self.controller.live {
            if matches!(
                &event,
                RuntimeEvent::Harness(HarnessEvent::TurnStarted { .. })
            ) {
                self.awaiting_resume = false;
            }
            if let RuntimeEvent::Harness(HarnessEvent::TurnStarted {
                session_id: Some(id),
                ..
            }) = &event
                && let Err(error) = self.confirm_provider_id(id, cx)
            {
                self.shutdown_runtime(cx);
                self.resume_failed(format!("Cannot resume Claude: {error:#}"), cx);
                return;
            }
        }
        if matches!(
            &event,
            RuntimeEvent::Harness(HarnessEvent::ConversationReset { .. })
        ) {
            crate::task_host::revoke_caller(&self.controller.target.session_uid, cx);
        }
        if self.controller.live
            && let RuntimeEvent::Harness(event) = &event
        {
            let prs = self.created_prs.observe(event);
            if !prs.is_empty()
                && let Some(chat) = self
                    .live_tab
                    .as_ref()
                    .and_then(|t| t.tab.chat_session_id.clone())
                && let Some(watch) = cx.try_global::<crate::pr_live::LivePrs>().cloned()
            {
                let job = cx
                    .background_executor()
                    .spawn(async move { watch.note_live_prs(&chat, &prs) });
                cx.spawn(async move |pane, cx| {
                    if let Err(error) = job.await {
                        _ = pane.update(cx, |pane, cx| {
                            pane.view
                                .update(cx, |view, cx| view.set_status_error(error, cx))
                        });
                    }
                })
                .detach();
            }
        }
        let attention_event = match &event {
            RuntimeEvent::Harness(
                event @ (HarnessEvent::RuntimeRequest { .. }
                | HarnessEvent::RuntimeRequestResolved { .. }
                | HarnessEvent::TurnCompleted { .. }
                | HarnessEvent::Error(_)),
            ) => Some(event.clone()),
            _ => None,
        };
        let previous_settings = self.controller.adapter.state().acknowledged.clone();
        if let Some(update) = self.controller.apply_runtime_outcome(source, event) {
            self.pending.push(update);
            if let Some(event) = &attention_event {
                self.notification_attention.event(
                    event,
                    self.controller.adapter.state().has_pending_requests(),
                );
            }
            self.changed(cx);
        }
        if self.controller.live
            && self
                .live_tab
                .as_ref()
                .is_some_and(|tab| tab.tab.chat_session_id.is_some())
            && self.controller.adapter.state().acknowledged != previous_settings
            && let Err(error) = self.persist_live_tab(cx)
        {
            self.view.update(cx, |view, cx| {
                view.set_status_error(format!("Save Claude settings: {error:#}"), cx)
            });
        }
        if history_gap {
            self.reload_remote_history(false, cx);
        }
        if turn_ended {
            self.refresh_checkout_head(cx);
            cx.emit(TurnEnded(rate_limit));
        }
        if exited {
            self.shutdown_runtime(cx);
            if self.awaiting_resume && self.remote.is_none() {
                self.resume_failed(
                    "Cannot resume saved Claude session: process exited before the turn started"
                        .into(),
                    cx,
                );
                return;
            }
            let stopped = if self.controller.live {
                "Claude stopped"
            } else {
                "Claude preview stopped"
            };
            self.view.update(cx, |view, cx| {
                view.set_read_only(self.remote.is_none().then(|| stopped.into()), cx)
            });
        }
    }
    #[cfg(test)]
    fn install_runtime(
        &mut self,
        runtime: OwnedRuntime,
        events: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach_runtime(runtime, events, cx);
    }
    fn attach_runtime(
        &mut self,
        runtime: OwnedRuntime,
        mut events: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
        cx: &mut Context<Self>,
    ) {
        self.controller.runtime = Some(runtime);
        if self.controller.remote {
            self.controller.source = self
                .controller
                .adapter
                .begin_runtime(self.controller.target.clone());
            self.controller.correlations.clear();
        }
        let source = self.controller.source.clone();
        self.awaiting_resume = self.controller.live && self.saved.session_id.is_some();
        cx.spawn(async move |pane, cx| {
            while let Some(event) = events.recv().await {
                if pane
                    .update(cx, |pane, cx| {
                        pane.receive_runtime_event(&source, event, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
            let _ = pane.update(cx, |pane, cx| {
                if pane.controller.source == source {
                    pane.shutdown_runtime(cx);
                }
            });
        })
        .detach();
        let view = self.view.clone();
        cx.defer(move |cx| view.update(cx, |view, cx| view.set_read_only(None, cx)));
    }
}

#[cfg(test)]
impl ChatPane {
    pub(crate) fn runtime_source(&self) -> RuntimeStamp {
        self.controller.source.clone()
    }
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
        // GPUI releases the last window's panes before its deferred quit callback.
        // Only explicit close/Stop/Done sends a remote Shutdown.
        self.detach_remote_runtime();
        self.enqueue_shutdown();
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
    _conversation: Option<Arc<provider::ConversationClaim>>,
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
            _conversation: None,
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
    StopRemote(Box<remote::PendingStop>),
    Persist(
        Box<resume::LiveTab>,
        gitterm_core::desktop_store::DesktopStore,
        tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    ),
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
                        ShutdownCommand::StopRemote(stop) => {
                            if let Err(error) = stop.run(&executor) {
                                let error = format!("Stop restoring remote chat: {error:#}");
                                eprintln!("{error}");
                                errors.push(error);
                            }
                        }
                        ShutdownCommand::Session(mut session) => {
                            if let Err(error) = executor.block_on(session.completed()) {
                                eprintln!("Reap preview Claude: {error}");
                                errors.push(error);
                            }
                        }
                        ShutdownCommand::Persist(tab, store, reply) => {
                            let result = tab.persist(&store).map_err(|error| {
                                format!("Save confirmed Claude session: {error:#}")
                            });
                            if let Err(error) = &result {
                                errors.push(error.clone());
                            }
                            let _ = reply.send(result);
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
                ShutdownCommand::Drain(_)
                | ShutdownCommand::Persist(..)
                | ShutdownCommand::StopRemote(_) => unreachable!(),
            })
    }
    pub(crate) fn drain(&self) -> Result<()> {
        use anyhow::Context as _;
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.sender
            .send(ShutdownCommand::Drain(reply))
            .map_err(|_| anyhow::anyhow!("preview chat shutdown queue stopped"))?;
        result
            .recv_timeout(crate::lifetime::DRAIN_TIMEOUT)
            .context("shutdown phase chat-runtime: timeout or drain stopped")?
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
        if let Some(remote) = &self.remote {
            self.view.update(cx, |view, _| {
                view.set_remote_checkout(
                    remote.agent.name.clone(),
                    self.saved.cwd.display().to_string(),
                )
            });
        }
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
            .children(self.resume_error.as_ref().map(|_| {
                div()
                    .id("resume-failure-recovery")
                    .debug_selector(|| "resume-failure-recovery".into())
                    .p_2()
                    .child(
                        gpui_component::button::Button::new("start-fresh-chat")
                            .debug_selector(|| "start-fresh-chat".into())
                            .label("Start a fresh chat here")
                            .on_click(cx.listener(|pane, _, _, cx| pane.fresh_chat(cx))),
                    )
            }))
    }
}
fn dismiss_gate(live: bool) -> gitterm_gpui_chat::interactions::DismissGate {
    Rc::new(move |request, cx| {
        if live {
            return gitterm_core::preview::service::preview_notice(&request.label);
        }
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
fn live_config(
    cwd: PathBuf,
    settings: &gitterm_gpui_chat::composer::LaunchSettings,
    mcp_servers: Vec<gitterm_core::harness::claude::ClaudeMcpServer>,
    chrome: bool,
) -> gitterm_core::harness::claude::ClaudeSessionConfig {
    // Match Iced's AgentSession::claude_session_config default-model policy.
    gitterm_core::harness::claude::ClaudeSessionConfig {
        cwd,
        model: (!settings.model.trim().is_empty() && settings.model.trim() != "default")
            .then(|| settings.model.trim().to_owned()),
        permission_mode: settings.permission_mode.clone(),
        effort: settings.effort.clone(),
        resume: None,
        wire_log_dir: None,
        snapshot_facts_dir: None,
        mcp_servers,
        chrome,
        guidelines_file: None,
        guidelines_content: None,
    }
}

fn live_mcp_servers(
    services: &gitterm_core::preview::service::ServiceContext,
    caller: &str,
    cx: &gpui::App,
) -> Result<Vec<gitterm_core::harness::claude::ClaudeMcpServer>> {
    let host = cx
        .try_global::<crate::task_host::GpuiTaskHost>()
        .context("Live Claude requires the live task/browser host; it is unavailable")?;
    let host = host.0.read(cx);
    validate_live_host(services, host.host().services())?;
    let servers = host.claude_mcp_servers(caller);
    anyhow::ensure!(
        servers.len() == 2,
        "Live Claude task/browser host has shut down"
    );
    Ok(servers)
}
fn validate_live_host(
    services: &gitterm_core::preview::service::ServiceContext,
    actual: &gitterm_core::preview::service::ServiceContext,
) -> Result<()> {
    anyhow::ensure!(
        !services.is_preview()
            && !actual.is_preview()
            && services.state_root() == actual.state_root()
            && services.task_mcp().instance_id() == actual.task_mcp().instance_id()
            && services.task_mcp().token() == actual.task_mcp().token(),
        "Live Claude task/browser host does not match the installed production context"
    );
    Ok(())
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
        let _test_app = crate::test_app::Cleanup::new(cx);
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
