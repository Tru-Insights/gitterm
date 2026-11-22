//! H5b B11: fixture-mode delivery of finished results into the GPUI chat
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
//! Only delegations this host stored (`own_delegations`) are offered to
//! core, which gates each for the checkout its work ran in: imported
//! records are never delivered, and plain preview delivers nothing.
// shortcut: a chat's turn end is picked up by the next delivery tick, not
// flushed at once as Iced's turn-end arm does; subscribe to the panes'
// turn ends if the extra few seconds matter.
use super::*;
use gitterm_core::task_host::{
    DeliveryChat, DeliveryCompletion, DeliveryEffect, DeliveryReceipt, Submission,
};
use gitterm_gpui_chat::adapter::ReceiptState;

/// Iced's `Tick`, which runs its automatic delivery.
pub(crate) const DELIVERY_POLL: Duration = Duration::from_secs(5);

/// Stands in for writing a submission into its chat pane: `None` when no
/// receipt came back, else whether the chat took it.
#[cfg(test)]
pub(crate) type SubmitHook = Box<dyn FnMut(&Submission) -> Option<bool>>;

impl TaskHostOwner {
    /// Iced's `auto_deliver_delegations`, from the fixture delivery tick:
    /// collects every finished result at the same moment; idle chats flush
    /// now, busy ones keep their held results until a later tick.
    pub(crate) fn deliver_results(&mut self, cx: &mut Context<Self>) {
        if self.fixtures.is_empty() || self.is_shut_down() {
            return;
        }
        // A kept receipt may be saved by this tick: the store must still be
        // the copy under the preview root.
        if let Some(store) = self.host.store().as_ref()
            && let Err(error) = self.roots.scoped(store.path())
        {
            eprintln!(
                "[delegation] delivery skipped: task store {}: {error}",
                store.path().display()
            );
            return;
        }
        let auto_tell = self.auto_tell();
        let own = &self.own_delegations;
        let tick = self
            .host
            .deliver_finished_where(&delivery_chats(cx), auto_tell, |delegation| {
                own.contains_key(&delegation.delegation_id)
            });
        if let Some(error) = tick.receipt_error {
            eprintln!("[delegation] retry delivery acknowledgements: {error}");
        }
        for tab_id in tick.tabs {
            self.flush_chat(tab_id, auto_tell, cx);
        }
    }

    /// Iced's `delegations.auto_tell`, from the host's settings file: in
    /// fixture mode the fixture's, which `HostConfig::iced_config` names.
    fn auto_tell(&self) -> bool {
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
        let own = &self.own_delegations;
        let effects = self
            .host
            .flush_delivery_where(&chats, tab_id, auto_tell, |delegation| {
                own.contains_key(&delegation.delegation_id)
            });
        for effect in effects {
            match effect {
                // The cards are projected from the store on the next render.
                DeliveryEffect::Refresh(_) => cx.notify(),
                // Synchronous, so the chat is busy and the receipt recorded
                // before another tick can flush it again.
                DeliveryEffect::Submit(submission) => self.submit(submission, cx),
                DeliveryEffect::NotQueued(error) => {
                    eprintln!("[delegation] chat {tab_id}: cannot queue result: {error}")
                }
                // Only a manual send, which preview panes do not offer, gives these.
                DeliveryEffect::AlreadyDelivered | DeliveryEffect::NotSent(_) => {
                    unreachable!("manual send effect from a flush")
                }
            }
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

/// The GPUI chat panes, as core resolves a result's requesting chat. Every
/// preview pane runs its provider on this Mac, so none is remote.
fn delivery_chats(cx: &App) -> Vec<DeliveryChat> {
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
