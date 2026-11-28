//! Delivery of Live and fixture results into the GPUI chat
//! that requested them, in the order of Iced's `auto_deliver_delegations`,
//! `flush_held_delegation`, `run_delivery_effects`, `deliver_delegations`
//! and the `AgentSubmitDelegations` completion.
//!
//! Core's reducer decides what goes where ([`TaskHost::deliver_finished_where`],
//! [`TaskHost::flush_delivery_where`]); this module hands it the open chat
//! panes and executes its effects. Each submission is reserved by core
//! before it is handed out, written into the pane synchronously through the
//! composer's own send path, and reported with the identity core stamped
//! on it. A rejection, or a submission that never reached a pane, is
//! reported rejected, so its results stay undelivered for the next flush.
//!
//! Live delivers current and saved reports as Iced does. Fixture delivery
//! offers only delegations this host stored (`own_delegations`), gated for
//! the checkout its work ran in; imported fixture records remain inert and
//! plain Preview delivers nothing.
//!
//! As in Iced, delivery runs from two places: the tick
//! ([`TaskHostOwner::deliver_results`], Iced's `Tick` at src/main.rs
//! `auto_deliver_delegations`) and a chat's turn end
//! ([`TaskHostOwner::chat_turn_ended`], Iced's `TurnCompleted` arm calling
//! `flush_held_delegation`), which the shell reports for its chat panes.
use super::*;
use gitterm_core::task_host::{
    DeliveryChat, DeliveryCompletion, DeliveryEffect, DeliveryReceipt, Submission,
    waiting_delegation_log_lines,
};
use gitterm_gpui_chat::adapter::ReceiptState;

/// Iced's `Tick`, which runs its automatic delivery.
pub(crate) const DELIVERY_POLL: Duration = Duration::from_secs(5);

/// Stands in for writing a submission into its chat pane: `None` when no
/// receipt came back, else whether the chat took it.
#[cfg(test)]
pub(crate) type SubmitHook = Box<dyn FnMut(&Submission) -> Option<bool>>;

impl TaskHostOwner {
    /// Iced's `auto_deliver_delegations`, from the delivery tick:
    /// collects every finished result at the same moment; idle chats flush
    /// now, busy ones keep their held results until their turn ends
    /// ([`Self::chat_turn_ended`]) or a later tick.
    pub(crate) fn deliver_results(&mut self, cx: &mut Context<Self>) {
        if !self.delivers() {
            return;
        }
        let auto_tell = self.auto_tell();
        let chats = delivery_chats(cx);
        let tick = if self.is_live() {
            self.host.deliver_finished(&chats, auto_tell)
        } else {
            let own = &self.own_delegations;
            self.host
                .deliver_finished_where(&chats, auto_tell, |d| own.contains_key(&d.delegation_id))
        };
        if let Some(error) = tick.receipt_error {
            eprintln!("[delegation] retry delivery acknowledgements: {error}");
        }
        // Iced's `auto_deliver_delegations`: a finished result whose chat is
        // not open is logged once per change of its chat's count.
        for line in waiting_delegation_log_lines(&mut self.delegation_waiting_counts, tick.waiting)
        {
            eprintln!("{line}");
        }
        for tab_id in tick.tabs {
            self.flush_chat(tab_id, auto_tell, cx);
        }
    }

    /// Iced's `TurnCompleted` arm: the chat's held results go out as soon
    /// as its turn ends, including those finished since the last tick.
    pub(crate) fn chat_turn_ended(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        if self.delivers() {
            let auto_tell = self.auto_tell();
            self.flush_chat(tab_id, auto_tell, cx);
        }
    }

    /// Iced's `close_final_workers`, on the tick after delivery: a worker's
    /// final session closes once its result went out (core decides which,
    /// [`gitterm_core::task_host::final_worker_closes`]); a task left with
    /// no open worker ends as its final delegation did, unless another of
    /// its sessions is live. The shell closes each pane
    /// ([`super::FinalWorkerClose`]), which ends its terminal, and the
    /// close folds the session into its task ([`Self::session_closed`]).
    /// Iced stops the worker's process with a grace period first; GPUI's
    /// close drops the PTY, which hangs up the child, then reaps it.
    pub(crate) fn close_final_workers(&mut self, cx: &mut Context<Self>) {
        if !self.delivers() {
            return;
        }
        let ready = self
            .host
            .final_workers_ready(&delivery_chats(cx), self.auto_tell());
        let closing = self.start_final_worker_closes(&ready);
        if closing.is_empty() {
            return;
        }
        let sessions: Vec<String> = closing
            .iter()
            .filter_map(|target| target.task_session_id.clone())
            .collect();
        let ended = self.host.store().as_ref().map_or_else(Vec::new, |store| {
            gitterm_core::task_host::closed_worker_task_outcomes(store, &sessions)
        });
        for (task_id, state) in ended {
            if self.task_live_besides_closing(&task_id) {
                continue;
            }
            if let Err(error) = self.writable_store().and_then(|store| {
                store
                    .record_lifecycle_signal(
                        &task_id,
                        state,
                        None,
                        &chrono::Utc::now().to_rfc3339(),
                    )
                    .map_err(|error| error.to_string())
            }) {
                eprintln!("[tasks] finalize worker task {task_id}: {error}");
            }
        }
        for target in closing {
            match target.task_session_id.as_deref() {
                // Never adopted: the host holds the terminal and ends it.
                Some(session) if self.terminals.contains_key(session) => {
                    self.session_closed(session, cx)
                }
                _ => cx.emit(super::FinalWorkerClose { target }),
            }
        }
        cx.notify();
    }

    /// Live uses the locked durable store; fixture delivery stays scoped to
    /// its preview copy. Plain Preview never reaches core's receipt retry.
    fn delivers(&mut self) -> bool {
        if (!self.is_live() && self.fixtures.is_empty()) || self.is_shut_down() {
            return false;
        }
        if self.is_live() {
            if let Err(error) = self.writable_store() {
                eprintln!("[delegation] delivery skipped: {error}");
                return false;
            }
        } else if let Some(store) = self.host.store().as_ref()
            && let Err(error) = self.roots.scoped(store.path())
        {
            eprintln!(
                "[delegation] delivery skipped: task store {}: {error}",
                store.path().display()
            );
            return false;
        }
        true
    }

    /// Iced's `delegations.auto_tell`, from the host's settings file: in
    /// fixture mode the fixture's, which `HostConfig::iced_config` names.
    pub(super) fn auto_tell(&self) -> bool {
        crate::workspace_config::load_iced_settings_from(&self.iced_config)
            .delegations
            .auto_tell
    }

    /// Iced's `flush_held_delegation`: at most one submitted prompt per chat.
    pub(super) fn flush_chat(&mut self, tab_id: usize, auto_tell: bool, cx: &mut Context<Self>) {
        // Read again for each chat: the previous submission changed its state.
        let chats = delivery_chats(cx);
        if !chats.iter().any(|chat| chat.tab_id == tab_id) {
            eprintln!("[delegation] not sent: requesting chat tab {tab_id} is gone");
        }
        let effects = if self.is_live() {
            self.host.flush_delivery(&chats, tab_id, auto_tell)
        } else {
            let own = &self.own_delegations;
            self.host
                .flush_delivery_where(&chats, tab_id, auto_tell, |d| {
                    own.contains_key(&d.delegation_id)
                })
        };
        self.run_delivery_effects(tab_id, effects, cx);
    }

    fn run_delivery_effects(
        &mut self,
        tab_id: usize,
        effects: Vec<DeliveryEffect>,
        cx: &mut Context<Self>,
    ) {
        for effect in effects {
            match effect {
                DeliveryEffect::Refresh(_) => cx.notify(),
                DeliveryEffect::Submit(submission) => self.submit(submission, cx),
                DeliveryEffect::NotQueued(error) => {
                    eprintln!("[delegation] chat {tab_id}: cannot queue result: {error}")
                }
                DeliveryEffect::AlreadyDelivered => {
                    self.card_notice(tab_id, "That result was already sent to Claude.".into(), cx)
                }
                DeliveryEffect::NotSent(error) => {
                    self.card_notice(tab_id, format!("Not sent: {error}"), cx)
                }
            }
        }
        self.show_held(tab_id, cx);
    }

    fn card_notice(&self, tab_id: usize, notice: String, cx: &mut Context<Self>) {
        eprintln!("[delegation] chat {tab_id}: {notice}");
        if let Some(pane) = chat_pane(tab_id, cx) {
            pane.update(cx, |pane, cx| pane.show_notice(notice, cx));
        }
    }

    /// The same card intents as Iced, all checked against the requesting
    /// chat before reaching core. Only Live exposes these actions.
    pub(crate) fn card_action(
        &mut self,
        tab_id: usize,
        action: gitterm_gpui_chat::interactions::TranscriptAction,
        cx: &mut Context<Self>,
    ) {
        use gitterm_gpui_chat::interactions::TranscriptAction as A;
        if !self.is_live() || !self.delivers() {
            return;
        }
        let chats = delivery_chats(cx);
        match action {
            A::SendDelegation { id, finding_ids } => {
                let effects =
                    self.host
                        .send_delegation(&chats, tab_id, &id, &finding_ids, self.auto_tell());
                self.run_delivery_effects(tab_id, effects, cx);
            }
            A::Dismiss(request) => {
                match self.host.dismiss_delegation(
                    &chats,
                    tab_id,
                    &request.id,
                    &chrono::Utc::now().to_rfc3339(),
                ) {
                    Ok(dismissal) => {
                        for id in dismissal.abort {
                            if let Some(run) = self.runs.get(&id) {
                                // Keep the handle until the child is reaped; a
                                // cancelled completion cannot overwrite the card.
                                run.stop.stop();
                            }
                        }
                        if let Err(error) = dismissal.saved {
                            self.card_notice(tab_id, format!("Not dismissed: {error}"), cx);
                        }
                        self.start_queued_runs(cx);
                        self.show_held(tab_id, cx);
                        cx.notify();
                    }
                    Err(error) => self.card_notice(tab_id, format!("Not dismissed: {error}"), cx),
                }
            }
            A::RerunDelegation { id } => {
                let Some(chat) = chats.iter().find(|chat| chat.tab_id == tab_id) else {
                    return;
                };
                let label = match self.host.chat_delegation(&chats, tab_id, &id) {
                    Ok(d) if d.kind == gitterm_core::tasks::DelegationKind::Review => {
                        "review_request"
                    }
                    Ok(_) => "consult_request",
                    Err(error) => {
                        self.card_notice(tab_id, error, cx);
                        return;
                    }
                };
                // Card requests use the same asynchronous admission/reply path
                // as MCP. Retain its waiter until the check reports its result.
                let (sender, mut receiver) =
                    gitterm_core::task_commands::runtime::command_channel();
                let pending = sender.enqueue((), None).expect("new card request channel");
                let reply = receiver.try_recv().expect("enqueued card request").reply;
                self.request_codex_run(
                    reply,
                    Some(chat.session_uid.clone()),
                    super::runs::CodexRequest::Rerun(id),
                    label,
                    cx,
                );
                cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                    if let Err(error) = pending.await {
                        let _ = owner.update(cx, |owner, cx| owner.card_notice(tab_id, error, cx));
                    }
                    drop((receiver, sender));
                })
                .detach();
            }
            _ => {}
        }
    }

    /// Iced's `delegation_card` `held`: a card whose result waits for its
    /// chat's turn end, read from core's delivery state after each flush
    /// (the only place it changes). The pane is told, not asked to read
    /// the owner, which is being updated here.
    pub(crate) fn show_held(&self, tab_id: usize, cx: &mut Context<Self>) {
        let held: std::collections::HashSet<String> = self
            .host
            .delegations()
            .held(tab_id)
            .into_iter()
            .flatten()
            .map(|send| send.delegation_id.clone())
            .collect();
        if let Some(pane) = chat_pane(tab_id, cx) {
            pane.update(cx, |pane, cx| pane.set_delivery_held(held, cx));
        }
    }

    /// Iced's `deliver_delegations` and the `AgentSubmitDelegations`
    /// completion: the results must still belong to the chat, the store
    /// must still be the preview's, then the prompt goes into the pane and
    /// its receipt is reported under the submission's identity. A
    /// submission nothing reported is rejected afterwards.
    fn submit(&mut self, submission: Submission, cx: &mut Context<Self>) {
        let (tab_id, delivery) = (submission.tab_id, submission.delivery);
        let ids = &submission.batch.delegation_ids;
        let chats = delivery_chats(cx);
        let completion = if ids
            .iter()
            .any(|id| self.host.chat_delegation(&chats, tab_id, id).is_err())
        {
            eprintln!("[delegation] {ids:?}: requesting chat identity changed before delivery");
            None
        } else if let Err(error) = self.writable_store() {
            eprintln!("[delegation] {ids:?}: not sent: {error}");
            None
        } else if !self.continues("submit") {
            None
        } else {
            self.write_into_chat(&submission, cx)
        };
        if let Some(completion) = completion
            && let DeliveryReceipt::SaveFailed(error) =
                self.host.delivery_completed(tab_id, delivery, completion)
        {
            eprintln!("[delegation] {ids:?}: delivery acknowledgement failed: {error}");
            if let Some(pane) = chat_pane(tab_id, cx) {
                pane.update(cx, |pane, cx| {
                    pane.show_notice(
                        format!(
                            "Could not save the delivery acknowledgement; GitTerm will \
                             retry saving it: {error}"
                        ),
                        cx,
                    )
                });
            }
        }
        if self.host.delivery_pending(tab_id, delivery) {
            self.host
                .delivery_completed(tab_id, delivery, DeliveryCompletion::Rejected);
        }
        cx.notify();
    }

    /// The prompt into chat `tab_id`; `None` when no pane gave a receipt.
    fn write_into_chat(
        &mut self,
        submission: &Submission,
        cx: &mut Context<Self>,
    ) -> Option<DeliveryCompletion> {
        #[cfg(test)]
        if let Some(hook) = self.submit_hook.as_mut() {
            return hook(submission).map(|accepted| {
                if accepted {
                    DeliveryCompletion::Accepted
                } else {
                    DeliveryCompletion::Rejected
                }
            });
        }
        let Some(pane) = chat_pane(submission.tab_id, cx) else {
            eprintln!(
                "[delegation] {:?}: not sent: requesting chat tab {} is gone",
                submission.batch.delegation_ids, submission.tab_id
            );
            return None;
        };
        let receipt = pane.update(cx, |pane, cx| {
            pane.submit_prompt(submission.batch.message.clone(), cx)
        });
        Some(match receipt.state {
            ReceiptState::ChannelAccepted => DeliveryCompletion::Accepted,
            state => {
                eprintln!(
                    "[delegation] {:?}: chat tab {} did not take the result: {}",
                    submission.batch.delegation_ids,
                    submission.tab_id,
                    state.label()
                );
                DeliveryCompletion::Rejected
            }
        })
    }
}

/// Flushes `view`'s delivery whenever its turn ends; the shell holds one
/// per live chat pane.
pub(crate) fn forward_turn_ends(
    view: &Entity<crate::chat::ChatPane>,
    cx: &mut App,
) -> Subscription {
    cx.subscribe(view, |view, event: &crate::chat::TurnEnded, cx| {
        let tab_id = view.read(cx).target().tab_id;
        if let Some(host) = cx.try_global::<GpuiTaskHost>().map(|host| host.0.clone()) {
            host.update(cx, |host, cx| {
                if let Some(reading) = &event.0 {
                    host.observe_claude_rate_limit(reading, cx);
                }
                host.chat_turn_ended(tab_id, cx);
            });
        }
    })
}

/// The GPUI chat panes, as core resolves a result's requesting chat. Every
/// preview pane runs its provider on this Mac, so none is remote.
pub(super) fn delivery_chats(cx: &App) -> Vec<DeliveryChat> {
    chat_panes(cx)
        .into_iter()
        .map(|pane| {
            let pane = pane.read(cx);
            let target = pane.target();
            DeliveryChat {
                tab_id: target.tab_id,
                session_uid: target.session_uid,
                chat_session_id: pane.chat_session_id(),
                remote: false,
                streaming: pane.is_working(),
            }
        })
        .collect()
}

fn chat_pane(tab_id: usize, cx: &App) -> Option<Entity<crate::chat::ChatPane>> {
    chat_panes(cx)
        .into_iter()
        .find(|pane| pane.read(cx).target().tab_id == tab_id)
}

/// The workspace owner's chat panes; none before the window exists.
fn chat_panes(cx: &App) -> Vec<Entity<crate::chat::ChatPane>> {
    cx.try_global::<crate::work_sidebar::IcedWorkSource>()
        .and_then(|source| source.owner.upgrade())
        .map(|owner| owner.read(cx).chats.values().cloned().collect())
        .unwrap_or_default()
}
