//! Feature-free owner seam. The owner installs one receiver and supplies current
//! catalog facts at admission and execution; this module never starts a process.
use crate::intents::{ChatIntent, PermissionDecision, Question};
use gitterm_core::chat::types::DeliveryOutcome as CoreOutcome;
use gitterm_core::{
    chat::{
        state::{
            AgentEvent, Effect, EventEffects, HistoryTicket, Intent, PromptStatus, RuntimeStamp,
            State,
        },
        types::*,
    },
    session,
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttemptId(u64);

/// App-local outcomes, never serialized into a provider transcript or phone DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptState {
    Prepared,
    ChannelAccepted,
    Written,
    Rejected(String),
    Uncertain(String),
    /// Closed by the runtime, without claiming any particular human decision.
    Resolved,
}
impl ReceiptState {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }
    pub fn label(&self) -> String {
        match self {
            Self::Prepared => "Preparing command".into(),
            Self::ChannelAccepted => "Sending to harness".into(),
            Self::Written => "Written to harness · awaiting provider response".into(),
            Self::Rejected(error) => format!("Rejected · {error} · retry available"),
            Self::Uncertain(error) => format!("Uncertain · {error} · retry blocked"),
            Self::Resolved => "Resolved · decision not recorded".into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: AttemptId,
    pub state: ReceiptState,
}
impl Receipt {
    pub fn accepts_update(&self, next: &Self) -> bool {
        self.id == next.id
            && (self.state == next.state
                || match self.state {
                    ReceiptState::Prepared => !matches!(next.state, ReceiptState::Prepared),
                    ReceiptState::ChannelAccepted => !matches!(next.state, ReceiptState::Prepared),
                    ReceiptState::Written => matches!(
                        next.state,
                        ReceiptState::Rejected(_)
                            | ReceiptState::Uncertain(_)
                            | ReceiptState::Resolved
                    ),
                    ReceiptState::Uncertain(_) | ReceiptState::Rejected(_) => {
                        matches!(next.state, ReceiptState::Resolved)
                    }
                    ReceiptState::Resolved => false,
                })
    }
    pub fn rejected(error: impl Into<String>) -> Self {
        Self {
            id: AttemptId(NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed)),
            state: ReceiptState::Rejected(error.into()),
        }
    }
}

/// C7's executor port. Only a correlated actual I/O outcome may supply this.
/// Session::send's Ok is *only* ChannelAccepted; do not parse Error strings.
#[derive(Debug, Clone)]
pub enum DeliveryOutcome {
    Written,
    Rejected(String),
    Uncertain(String),
}

pub struct Admission {
    pub authority: session::State,
    pub config: AgentBackendConfig,
    pub process_running: bool,
    pub just_spawned: bool,
    pub message_id: String,
}

/// Opaque, single-use command: its admitted target cannot be substituted later.
pub struct Prepared {
    id: AttemptId,
    runtime: RuntimeStamp,
    command: HarnessCommand,
    echo: Option<AgentEvent>,
}
impl Prepared {
    /// Correlate the executor's eventual I/O outcome before handing off the command.
    pub fn id(&self) -> AttemptId {
        self.id
    }
}
struct Attempt {
    runtime: RuntimeStamp,
    request_id: Option<String>,
    /// Caller identity for retry/dedup; queued retries get a fresh wire ID.
    admission_id: Option<String>,
    message_id: Option<String>,
    withdraw_id: Option<String>,
    correlation: Option<CommandCorrelation>,
    receipt: Receipt,
}

/// Transient presentation instructions, derived from the sole conversation.
#[derive(Default)]
pub struct Update {
    pub presentation: Option<Presentation>,
    pub event: Option<EventEffects>,
    pub receipts: Vec<Receipt>,
}
pub enum Presentation {
    Append(Vec<AgentEvent>),
    Replay,
}

#[derive(Default)]
pub struct ChatAdapter {
    state: State,
    attempts: HashMap<AttemptId, Attempt>,
    exited: bool,
}
impl ChatAdapter {
    pub fn state(&self) -> &State {
        &self.state
    }
    pub fn bind_session(&mut self, id: Option<String>) {
        self.state.reduce(Intent::BindSession(id));
    }
    /// Exactly once per newly installed receiver, never on pane focus/show.
    pub fn begin_runtime(&mut self, target: session::Target) -> RuntimeStamp {
        self.state.begin_runtime();
        self.attempts.clear();
        self.exited = false;
        self.state.runtime_stamp(target)
    }
    pub fn receipt(&self, id: AttemptId) -> Option<&Receipt> {
        self.attempts.get(&id).map(|a| &a.receipt)
    }

    pub fn admit(&mut self, intent: ChatIntent, facts: Admission) -> Result<Prepared, Receipt> {
        let backend = facts.config.backend();
        if !backend.is_native() {
            return Err(Receipt::rejected(
                "This adapter requires a native harness backend",
            ));
        }
        let runtime = self.state.runtime_stamp(facts.authority.target.clone());
        let request_id = intent.request_id().map(str::to_owned);
        let withdraw_id = match &intent {
            ChatIntent::Withdraw { message_id } => Some(message_id.clone()),
            _ => None,
        };
        let admission_id =
            matches!(&intent, ChatIntent::Send { .. }).then(|| facts.message_id.clone());
        if let Some(attempt) = self.attempts.values().find(|a| {
            (request_id.is_some() && a.request_id == request_id
                || admission_id.is_some() && a.admission_id == admission_id)
                && self
                    .state
                    .accepts_stream(&a.runtime, &facts.authority.target)
                && !a.receipt.state.retryable()
        }) {
            return Err(attempt.receipt.clone());
        }
        let mut message_id = admission_id.clone();
        let effect = match intent {
            ChatIntent::Send { text, images } => {
                if !self.state.pending_requests.is_empty() {
                    return Err(Receipt::rejected(
                        "Answer the pending request before sending",
                    ));
                }
                if !facts.process_running || self.exited {
                    return Err(Receipt::rejected("No chat runtime is running"));
                }
                if text.trim().is_empty() && images.is_empty() {
                    return Err(Receipt::rejected("The prompt is empty"));
                }
                validate_images(&images).map_err(Receipt::rejected)?;
                let prompt = UserPrompt {
                    text,
                    images,
                    id: None,
                };
                // SubmissionAccepted optimistically marks an idle Submit as
                // streaming. An explicit retry after definitive non-delivery
                // must keep that Submit's identity instead of becoming a Queue.
                let retry_submit = self.attempts.values().any(|attempt| {
                    attempt.admission_id.as_ref() == Some(&facts.message_id)
                        && attempt.receipt.state.retryable()
                }) && !self.state.queued_prompts.contains_key(&facts.message_id);
                let retry_queue = self.attempts.values().any(|attempt| {
                    attempt.admission_id.as_ref() == Some(&facts.message_id)
                        && attempt.receipt.state.retryable()
                        && attempt.message_id.as_ref().is_some_and(|id| {
                            self.state
                                .queued_prompts
                                .get(id)
                                .is_some_and(|queued| queued.status == PromptStatus::Unsent)
                        })
                });
                let delivery_id = if retry_queue {
                    // The core intentionally retains Unsent IDs and refuses
                    // their reuse. Explicit retries are new deliveries; retain
                    // the old entry and let Queue track the new one normally.
                    loop {
                        let next = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
                        let id = format!("{}-retry-{next}", facts.message_id);
                        if !self.state.queued_prompts.contains_key(&id)
                            && !self.state.shows_prompt(&id)
                        {
                            break id;
                        }
                    }
                } else {
                    facts.message_id
                };
                message_id = Some(delivery_id.clone());
                if !retry_submit
                    && !facts.just_spawned
                    && self.state.submit_queues(backend, facts.process_running)
                {
                    self.state.reduce(Intent::Queue {
                        runtime: runtime.clone(),
                        current: facts.authority.target,
                        config: facts.config,
                        process_running: facts.process_running,
                        prompt,
                        message_id: delivery_id,
                    })
                } else {
                    self.state.reduce(Intent::Submit {
                        prompt,
                        backend,
                        process_running: facts.process_running,
                        just_spawned: facts.just_spawned,
                        message_id: delivery_id,
                    })
                }
            }
            ChatIntent::Withdraw { message_id } => self.state.reduce(Intent::Withdraw {
                runtime: runtime.clone(),
                current: facts.authority.target,
                process_running: facts.process_running,
                message_id,
            }),
            ChatIntent::ChangeMode { .. } => {
                return Err(Receipt::rejected(
                    "Mode changes are disabled until typed provider acknowledgement/rejection is available",
                ));
            }
            ChatIntent::Stop => {
                if facts.authority.control != session::TurnControl::Harness
                    || !facts.process_running
                {
                    return Err(Receipt::rejected("No native turn is running"));
                }
                self.state.reduce(Intent::Stop {
                    runtime: runtime.clone(),
                    target: facts.authority.target.clone(),
                    authority: facts.authority,
                })
            }
            intent => {
                let id = intent.request_id().expect("answer intent").to_owned();
                let kind = self
                    .current_request(&id)
                    .ok_or_else(|| Receipt::rejected("Request is no longer pending"))?;
                let decision = decision(&intent, kind).map_err(Receipt::rejected)?;
                self.state.reduce(Intent::Answer {
                    runtime: runtime.clone(),
                    current: facts.authority.target,
                    request_id: id,
                    decision,
                    process_running: facts.process_running,
                })
            }
        };
        let (command, echo, correlation) = match effect {
            Effect::Submit(submission) => (
                HarnessCommand::SendUserMessage(submission.prompt),
                Some(submission.echo),
                None,
            ),
            Effect::Dispatch {
                command,
                echo,
                correlation,
                ..
            } => (command, echo, Some(correlation)),
            Effect::Send { command, .. } => (command, None, None),
            Effect::Rejected(error) => return Err(Receipt::rejected(error)),
            // Native C7 never executes the legacy task-handle stop path.
            _ => return Err(Receipt::rejected("No native command was prepared")),
        };
        let id = AttemptId(NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed));
        self.attempts.insert(
            id,
            Attempt {
                runtime: runtime.clone(),
                request_id,
                admission_id,
                message_id,
                withdraw_id,
                correlation,
                receipt: Receipt {
                    id,
                    state: ReceiptState::Prepared,
                },
            },
        );
        Ok(Prepared {
            id,
            runtime,
            command,
            echo,
        })
    }

    /// Synchronous channel handoff. Recheck the full binding at the last possible
    /// point, including submissions and requests resolved since admission.
    pub fn execute(
        &mut self,
        prepared: Prepared,
        current: &session::Target,
        now_ms: i64,
        send: impl FnOnce(HarnessCommand) -> Result<(), String>,
    ) -> (Receipt, Update) {
        let Prepared {
            id,
            runtime,
            command,
            echo,
        } = prepared;
        let valid = self.attempts.get(&id).is_some_and(|a| {
            a.receipt.state == ReceiptState::Prepared
                && a.runtime == runtime
                && a.request_id
                    .as_ref()
                    .is_none_or(|id| self.current_request(id).is_some())
        }) && !self.exited
            && self.state.accepts_runtime(&runtime, current)
            && match &command {
                HarnessCommand::SendUserMessage(_) => self.state.pending_requests.is_empty(),
                HarnessCommand::WithdrawQueuedMessage(id) => {
                    self.state.queued_prompts.get(id).is_some_and(|p| {
                        matches!(p.status, PromptStatus::Pending | PromptStatus::Queued)
                            && p.withdraw_pending
                    })
                }
                _ => true,
            };
        if !valid {
            let receipt = self
                .attempts
                .get_mut(&id)
                .map(|attempt| {
                    if attempt.receipt.state == ReceiptState::Prepared {
                        attempt.receipt.state = ReceiptState::Rejected(
                            "Chat runtime or request changed before execution".into(),
                        );
                    }
                    attempt.receipt.clone()
                })
                .unwrap_or(Receipt {
                    id,
                    state: ReceiptState::Rejected("Chat runtime changed before execution".into()),
                });
            if let ReceiptState::Rejected(error) = &receipt.state {
                self.settle_core(id, current, CoreOutcome::Rejected(error.clone()));
            }
            return (receipt, Update::default());
        }
        let state = match send(command) {
            Ok(()) => ReceiptState::ChannelAccepted,
            Err(error) => ReceiptState::Rejected(error),
        };
        let receipt = Receipt { id, state };
        self.attempts
            .get_mut(&id)
            .expect("validated attempt")
            .receipt = receipt.clone();
        if let ReceiptState::Rejected(error) = &receipt.state {
            self.settle_core(id, current, CoreOutcome::Rejected(error.clone()));
        }
        let mut update = Update::default();
        if receipt.state == ReceiptState::ChannelAccepted
            && let Some(echo) = echo
        {
            // Queue must leave the active streaming turn intact.
            if self
                .attempts
                .get(&id)
                .is_some_and(|a| a.correlation.is_none())
            {
                self.state.reduce(Intent::SubmissionAccepted);
            }
            if !echo
                .prompt_id()
                .is_some_and(|id| self.state.shows_prompt(id))
            {
                update = self.record(echo, Some(now_ms));
            }
        }
        (receipt, update)
    }

    pub fn delivery(
        &mut self,
        id: AttemptId,
        current: &session::Target,
        outcome: DeliveryOutcome,
    ) -> Option<Receipt> {
        let core_outcome = match &outcome {
            DeliveryOutcome::Written => CoreOutcome::Written,
            DeliveryOutcome::Rejected(error) => CoreOutcome::Rejected(error.clone()),
            DeliveryOutcome::Uncertain(error) => CoreOutcome::Uncertain(error.clone()),
        };
        let attempt = self.attempts.get_mut(&id)?;
        if !self.state.accepts_stream(&attempt.runtime, current) {
            return None;
        }
        if !matches!(attempt.receipt.state, ReceiptState::ChannelAccepted) {
            if matches!(
                attempt.receipt.state,
                ReceiptState::Resolved | ReceiptState::Rejected(_)
            ) {
                // Lifecycle may beat the I/O receipt. Settle transport bookkeeping
                // without reopening the already authoritative display result.
                self.settle_core(id, current, core_outcome);
            }
            return None;
        }
        attempt.receipt.state = match outcome {
            DeliveryOutcome::Written => ReceiptState::Written,
            DeliveryOutcome::Rejected(error) => ReceiptState::Rejected(error),
            DeliveryOutcome::Uncertain(error) => ReceiptState::Uncertain(error),
        };
        let receipt = attempt.receipt.clone();
        self.settle_core(id, current, core_outcome);
        Some(receipt)
    }

    fn settle_core(&mut self, id: AttemptId, current: &session::Target, outcome: CoreOutcome) {
        if let Some(attempt) = self.attempts.get_mut(&id)
            && self.state.accepts_stream(&attempt.runtime, current)
            && let Some(correlation) = attempt.correlation.take()
        {
            // This settles an already admitted operation, never dispatches a
            // new command. A task rebind must not strand its old bookkeeping.
            self.state.reduce(Intent::CommandOutcome {
                runtime: attempt.runtime.clone(),
                current: attempt.runtime.target.clone(),
                correlation,
                outcome,
            });
        }
    }

    /// Gate the *entire* ingress before every observer, reducer effect and row.
    /// HistoryReplayed must instead go through complete_history with its source ticket.
    pub fn receive(
        &mut self,
        source: &RuntimeStamp,
        current: &session::Target,
        event: AgentEvent,
        now_ms: Option<i64>,
        observer: impl FnOnce(&AgentEvent, &EventEffects),
    ) -> Option<Update> {
        if !self.state.accepts_stream(source, current)
            || matches!(
                event,
                AgentEvent::Harness(HarnessEvent::HistoryReplayed { .. })
            )
        {
            return None;
        }
        // A receiver can relay our channel-accepted prompt back under the same ID.
        // Use the sole conversation's prompt lookup before any duplicate effects.
        if let AgentEvent::Harness(HarnessEvent::UserMessageEcho { id: Some(id), .. }) = &event
            && self.state.shows_prompt(id)
        {
            return Some(Update::default());
        }
        let Effect::Event(effects) = self.state.reduce(Intent::Event(event.clone())) else {
            unreachable!()
        };
        observer(&event, &effects);
        let mut receipts = Vec::new();
        match &event {
            AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved { request_id }) => {
                for attempt in self
                    .attempts
                    .values_mut()
                    .filter(|a| a.request_id.as_ref() == Some(request_id))
                {
                    attempt.receipt.state = ReceiptState::Resolved;
                    receipts.push(attempt.receipt.clone());
                }
            }
            AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                id,
                state: MessageState::Completed | MessageState::Cancelled,
            }) => {
                for attempt in self.attempts.values_mut().filter(|a| {
                    a.message_id.as_ref() == Some(id) || a.withdraw_id.as_ref() == Some(id)
                }) {
                    attempt.receipt.state = ReceiptState::Resolved;
                    receipts.push(attempt.receipt.clone());
                }
            }
            AgentEvent::Harness(HarnessEvent::MessageLifecycle {
                id,
                state: MessageState::Started,
            }) => {
                for attempt in self
                    .attempts
                    .values_mut()
                    .filter(|a| a.withdraw_id.as_ref() == Some(id))
                {
                    if !matches!(
                        attempt.receipt.state,
                        ReceiptState::Resolved | ReceiptState::Rejected(_)
                    ) {
                        attempt.receipt.state = ReceiptState::Rejected(format!(
                            "Not withdrawn ({id}): the message has already started"
                        ));
                        receipts.push(attempt.receipt.clone());
                    }
                }
            }
            AgentEvent::Harness(HarnessEvent::MessageWithdrawRefused { id, reason }) => {
                for attempt in self
                    .attempts
                    .values_mut()
                    .filter(|a| a.withdraw_id.as_ref() == Some(id))
                {
                    if !matches!(
                        attempt.receipt.state,
                        ReceiptState::Resolved | ReceiptState::Rejected(_)
                    ) {
                        attempt.receipt.state =
                            ReceiptState::Rejected(format!("Not withdrawn ({id}): {reason}"));
                        receipts.push(attempt.receipt.clone());
                    }
                }
            }
            AgentEvent::Harness(HarnessEvent::ProcessExited { .. }) => {
                self.exited = true;
                for attempt in self.attempts.values_mut() {
                    if matches!(
                        attempt.receipt.state,
                        ReceiptState::ChannelAccepted | ReceiptState::Written
                    ) {
                        attempt.receipt.state = if attempt.message_id.as_ref().is_some_and(|id| {
                            self.state
                                .queued_prompts
                                .get(id)
                                .is_some_and(|p| p.status == PromptStatus::Unsent)
                        }) {
                            ReceiptState::Rejected(
                                "Queued prompt was not sent before process exit".into(),
                            )
                        } else {
                            ReceiptState::Uncertain(
                                "Process exited before an authoritative outcome".into(),
                            )
                        };
                        receipts.push(attempt.receipt.clone());
                    }
                }
            }
            _ => {}
        }
        let mut update = self.record(event, now_ms);
        update.event = Some(effects);
        update.receipts = receipts;
        Some(update)
    }
    fn record(&mut self, event: AgentEvent, now_ms: Option<i64>) -> Update {
        let Effect::Recorded { mut before, after } = self.state.reduce(Intent::Record {
            event: event.clone(),
            now_ms,
        }) else {
            unreachable!()
        };
        before.push(event);
        before.extend(after);
        // C6 stores worker timing in conversation, outside Recorded::after.
        // Read that sole source so incremental and rebuilt projections agree.
        if let Some(AgentEvent::Other(marker)) = self.state.conversation.last()
            && marker["kind"] == "subagent_started_at"
        {
            before.push(AgentEvent::Other(marker.clone()));
        }
        Update {
            presentation: Some(Presentation::Append(before)),
            event: None,
            receipts: vec![],
        }
    }
    pub fn begin_history(
        &mut self,
        target: session::Target,
        config: AgentBackendConfig,
    ) -> Option<(HistoryTicket, String)> {
        let backend = config.backend();
        let Effect::LoadHistory(id) = self.state.reduce(Intent::BeginHistory(config)) else {
            return None;
        };
        Some((
            self.state
                .history_ticket(target, backend)
                .expect("loading source"),
            id,
        ))
    }
    /// Validate the original source on both success and failure. Replay is an
    /// explicit effect: history replacement does not increment core revision.
    pub fn complete_history(
        &mut self,
        ticket: &HistoryTicket,
        current: &session::Target,
        backend: AgentBackend,
        result: Result<Vec<AgentEvent>, String>,
    ) -> Result<Option<Update>, String> {
        if !self.state.accepts_history(ticket, current, backend) {
            return Ok(None);
        }
        let effect = self.state.reduce(match result {
            Ok(events) => Intent::HistoryLoaded(events),
            Err(error) => Intent::HistoryUnavailable(error),
        });
        match effect {
            Effect::ReplayHistory => Ok(Some(Update {
                presentation: Some(Presentation::Replay),
                event: None,
                receipts: vec![],
            })),
            Effect::Rejected(error) => Err(error),
            _ => Ok(Some(Update::default())),
        }
    }
    fn current_request(&self, id: &str) -> Option<&RuntimeRequestKind> {
        if !self
            .state
            .pending_requests
            .iter()
            .any(|pending| pending == id)
        {
            return None;
        }
        for event in self.state.conversation.iter().rev() {
            match event {
                AgentEvent::Harness(HarnessEvent::RuntimeRequest {
                    request_id, kind, ..
                }) if request_id == id => return Some(kind),
                AgentEvent::Harness(HarnessEvent::RuntimeRequestResolved { request_id })
                    if request_id == id =>
                {
                    return None;
                }
                AgentEvent::Harness(
                    HarnessEvent::ConversationReset { .. }
                    | HarnessEvent::TurnCompleted { .. }
                    | HarnessEvent::ProcessExited { .. },
                ) => return None,
                _ => {}
            }
        }
        None
    }
}

fn decision(intent: &ChatIntent, kind: &RuntimeRequestKind) -> Result<RuntimeDecision, String> {
    match (intent, kind) {
        (
            ChatIntent::Permission {
                decision: PermissionDecision::Allow,
                ..
            },
            RuntimeRequestKind::Permission { .. },
        ) => Ok(RuntimeDecision::Allow {
            updated_input: None,
            remember: None,
        }),
        (
            ChatIntent::Permission {
                decision: PermissionDecision::Deny,
                ..
            },
            RuntimeRequestKind::Permission { .. },
        )
        | (ChatIntent::DismissQuestion { .. }, RuntimeRequestKind::Question { .. }) => {
            Ok(RuntimeDecision::Deny {
                message: "Declined by the user".into(),
            })
        }
        (
            ChatIntent::Permission {
                decision: PermissionDecision::AllowAlways { .. },
                ..
            },
            _,
        ) => Err(
            "Remembering requires a verified preview-owned settings destination; unavailable in C7"
                .into(),
        ),
        (
            ChatIntent::AnswerQuestion { answers, .. },
            RuntimeRequestKind::Question { questions },
        ) => {
            let questions: Vec<Question> = serde_json::from_value(questions.clone())
                .map_err(|e| format!("Invalid questionnaire: {e}"))?;
            if questions.is_empty() || questions.len() != answers.len() {
                return Err("Answer every offered question exactly once".into());
            }
            let mut encoded = std::collections::BTreeMap::new();
            for (question, answer) in questions.iter().zip(answers) {
                if question.question != answer.question
                    || encoded.contains_key(&answer.question)
                    || (!question.multi_select
                        && answer.selected.len() + usize::from(answer.free_text.is_some()) != 1)
                    || answer
                        .selected
                        .iter()
                        .any(|label| !question.options.iter().any(|o| &o.label == label))
                    || answer
                        .selected
                        .iter()
                        .enumerate()
                        .any(|(i, label)| answer.selected[..i].contains(label))
                {
                    return Err("Answer does not match the offered questionnaire".into());
                }
                let mut labels = answer.selected.clone();
                if let Some(text) = &answer.free_text
                    && !text.trim().is_empty()
                {
                    labels.push(text.trim().into());
                }
                if labels.is_empty() {
                    return Err("An answer is required".into());
                }
                encoded.insert(answer.question.clone(), labels.join(", "));
            }
            Ok(RuntimeDecision::AnswerQuestion { answers: encoded })
        }
        _ => Err("Decision does not match this request's kind".into()),
    }
}

#[cfg(test)]
mod tests;
