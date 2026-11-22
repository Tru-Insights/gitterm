//! Delivering finished results into the chat that requested them (H4c).
//!
//! The host decides what goes where and keeps the delivery state: results
//! held while a chat is busy, automatic sends, and the one submission per
//! chat that is out with the frontend. The frontend passes its open chat
//! tabs ([`DeliveryChat`]) and executes what comes back: card refreshes,
//! notices, and each [`Submission`], which it writes into the chat and
//! reports with [`TaskHost::delivery_completed`].
//!
//! A submission is reserved before it is handed out, so a flush that runs
//! before its completion never submits those results again. The completion
//! carries the submission's identity and is applied only while that
//! reservation is still the chat's; a stale one is ignored. An accepted
//! submission's receipt is written here: kept in memory when the save
//! fails, and saved again by the next [`TaskHost::deliver_finished`]. A
//! rejected one leaves its results undelivered for the next flush.
use super::TaskHost;
use crate::delegation_state::{Effect, Intent, RequestId};
use crate::delegations::{self, SendBatch};
use crate::preview::service::RunnerEffect;
use crate::tasks::{Delegation, DelegationChild, DelegationParent};

const STORE_UNAVAILABLE: &str = "GitTerm's task store is unavailable";

/// One open local or remote chat tab the frontend can write a prompt into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryChat {
    pub tab_id: usize,
    pub session_uid: String,
    pub chat_session_id: Option<String>,
    /// The tab's workspace has a remote identity: results stay on this
    /// machine, so they are never delivered there.
    pub remote: bool,
    /// A turn is running: results wait for it to end.
    pub streaming: bool,
}

/// Results to write into chat `tab_id` as one prompt, reserved until the
/// frontend reports it with [`TaskHost::delivery_completed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub delivery: RequestId,
    pub tab_id: usize,
    pub batch: SendBatch,
}

/// What the frontend's write into the chat did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryCompletion {
    /// The chat took the prompt.
    Accepted,
    /// Nothing was submitted.
    Rejected,
}

/// The host's answer to a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryReceipt {
    /// The receipt is saved.
    Saved,
    /// Accepted, and the receipt is kept in memory, but saving it failed;
    /// the next [`TaskHost::deliver_finished`] saves it again.
    SaveFailed(String),
    /// Rejected: the results stay undelivered for the next flush.
    NotSubmitted,
    /// Not the chat's reservation (any more): ignored.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryEffect {
    /// Redraw this delegation's card.
    Refresh(String),
    Submit(Submission),
    /// An automatic result could not be queued (logged).
    NotQueued(String),
    /// A manual send of a result that was already sent.
    AlreadyDelivered,
    /// A manual send refused, for the user.
    NotSent(String),
}

/// One tick of automatic delivery.
#[derive(Debug, Default)]
pub struct DeliveryTick {
    /// Saving a kept receipt failed again.
    pub receipt_error: Option<String>,
    /// The requesting chat session of each finished result whose chat is
    /// not open, one entry per result.
    pub waiting: Vec<String>,
    /// Chats to flush with [`TaskHost::flush_delivery`], one at a time, so
    /// each flush sees the chat state the previous submission left.
    pub tabs: Vec<usize>,
}

/// A dismissed card: the runs to stop, and whether the store took it.
#[derive(Debug)]
pub struct Dismissal {
    pub abort: Vec<String>,
    pub saved: Result<(), String>,
}

fn chat_for(chats: &[DeliveryChat], parent: &DelegationParent) -> Option<usize> {
    delegations::parent_tab_id(
        parent,
        chats.iter().map(|chat| {
            (
                chat.tab_id,
                chat.session_uid.as_str(),
                chat.chat_session_id.as_deref(),
                chat.remote,
            )
        }),
    )
}

impl TaskHost {
    fn automatic_delivery(&self, auto_tell: bool) -> bool {
        auto_tell
            && self
                .services
                .runner(RunnerEffect::AutomaticDelivery, None)
                .is_ok()
    }

    /// The automatic-delivery gate for one result, checked for the checkout
    /// its work ran in (a worker's task worktree): a fixture host sends only
    /// results of work done inside its fixtures, and preview none.
    fn automatic_delivery_of(&self, auto_tell: bool, delegation: &Delegation) -> bool {
        let target = match &delegation.child {
            DelegationChild::TaskSession { task_id, .. } => self
                .store()
                .as_ref()
                .and_then(|store| store.get(task_id)?.worktree.path.clone()),
            _ => Some(delegation.run_checkout().to_path_buf()),
        };
        auto_tell
            && target.is_some_and(|target| {
                self.services
                    .runner(RunnerEffect::AutomaticDelivery, Some(&target))
                    .is_ok()
            })
    }

    /// The stored delegation behind a card on `tab_id`, checked to belong
    /// to that chat.
    pub fn chat_delegation(
        &self,
        chats: &[DeliveryChat],
        tab_id: usize,
        delegation_id: &str,
    ) -> Result<&Delegation, String> {
        let delegation = self
            .store()
            .as_ref()
            .ok_or_else(|| STORE_UNAVAILABLE.to_string())?
            .delegation(delegation_id)
            .ok_or_else(|| format!("delegation {delegation_id} does not exist"))?;
        if chat_for(chats, &delegation.parent) != Some(tab_id) {
            return Err(format!("delegation {delegation_id} belongs to another tab"));
        }
        Ok(delegation)
    }

    /// Whether a card on `tab_id` shows its result waiting for the turn end.
    pub fn delivery_held(&self, tab_id: usize, delegation_id: &str) -> bool {
        self.delegations
            .held(tab_id)
            .is_some_and(|held| held.iter().any(|send| send.delegation_id == delegation_id))
    }

    /// Whether `delivery` is still out with the frontend for `tab_id`.
    pub fn delivery_pending(&self, tab_id: usize, delivery: RequestId) -> bool {
        self.delegations.submitting(tab_id) == Some(delivery)
    }

    /// From the tick: saves receipts kept from a failed save, then names
    /// every chat with held results or finished ones to send automatically.
    pub fn deliver_finished(&mut self, chats: &[DeliveryChat], auto_tell: bool) -> DeliveryTick {
        let automatic = self.automatic_delivery(auto_tell);
        self.deliver_finished_with(chats, |_, _| automatic)
    }

    /// [`Self::deliver_finished`] for a host that sends only the results
    /// `eligible` accepts, each gated for the checkout its work ran in.
    pub fn deliver_finished_where(
        &mut self,
        chats: &[DeliveryChat],
        auto_tell: bool,
        eligible: impl Fn(&Delegation) -> bool,
    ) -> DeliveryTick {
        self.deliver_finished_with(chats, |host, delegation| {
            eligible(delegation) && host.automatic_delivery_of(auto_tell, delegation)
        })
    }

    fn deliver_finished_with(
        &mut self,
        chats: &[DeliveryChat],
        automatic: impl Fn(&Self, &Delegation) -> bool,
    ) -> DeliveryTick {
        let mut tick = DeliveryTick::default();
        if let Some(store) = self.store_mut().as_mut() {
            if let Err(error) = store.retry_delivery_acknowledgements() {
                tick.receipt_error = Some(error.to_string());
            }
        }
        let mut tabs: std::collections::HashSet<usize> = self.delegations.held_tabs().collect();
        if let Some(store) = self.store().as_ref() {
            for delegation in store.delegations() {
                if !(delegation.status.is_sendable()
                    && !delegation.is_delivered()
                    && automatic(self, delegation))
                {
                    continue;
                }
                match chat_for(chats, &delegation.parent) {
                    Some(tab_id) => {
                        tabs.insert(tab_id);
                    }
                    None => tick.waiting.push(delegation.parent.session_uid.clone()),
                }
            }
        }
        tick.tabs = tabs.into_iter().collect();
        tick
    }

    /// From the tick and turn end: queues `tab_id`'s finished results and,
    /// when the chat is idle and has no submission out, reserves the next
    /// one. At most one submitted prompt per chat.
    pub fn flush_delivery(
        &mut self,
        chats: &[DeliveryChat],
        tab_id: usize,
        auto_tell: bool,
    ) -> Vec<DeliveryEffect> {
        let automatic = self.automatic_delivery(auto_tell);
        self.flush_delivery_with(chats, tab_id, automatic, |_, _| true)
    }

    /// [`Self::flush_delivery`] over only the results `eligible` accepts,
    /// each gated as [`Self::deliver_finished_where`] gates it. Any other
    /// record of the chat is left alone, held or not.
    pub fn flush_delivery_where(
        &mut self,
        chats: &[DeliveryChat],
        tab_id: usize,
        auto_tell: bool,
        eligible: impl Fn(&Delegation) -> bool,
    ) -> Vec<DeliveryEffect> {
        self.flush_delivery_with(chats, tab_id, auto_tell, |host, delegation| {
            eligible(delegation) && host.automatic_delivery_of(auto_tell, delegation)
        })
    }

    fn flush_delivery_with(
        &mut self,
        chats: &[DeliveryChat],
        tab_id: usize,
        automatic: bool,
        include: impl Fn(&Self, &Delegation) -> bool,
    ) -> Vec<DeliveryEffect> {
        let chat = chats.iter().find(|chat| chat.tab_id == tab_id);
        // Oldest first, including results requested before Resume as Chat.
        let records: Vec<&Delegation> = self
            .authority
            .store()
            .as_ref()
            .map(|store| {
                store
                    .delegations()
                    .iter()
                    .filter(|d| include(self, d) && chat_for(chats, &d.parent) == Some(tab_id))
                    .collect()
            })
            .unwrap_or_default();
        self.delegations
            .reduce(Intent::Flush {
                tab_id,
                recipient_exists: chat.is_some(),
                delegations: &records,
                streaming: chat.is_some_and(|chat| chat.streaming),
                automatic,
            })
            .into_iter()
            .map(|effect| match effect {
                Effect::Refresh(id) => DeliveryEffect::Refresh(id),
                Effect::Deliver {
                    tab_id,
                    delivery,
                    batch,
                } => DeliveryEffect::Submit(Submission {
                    delivery,
                    tab_id,
                    batch,
                }),
                Effect::Rejected(error) => DeliveryEffect::NotQueued(error),
                _ => unreachable!("delivery effect"),
            })
            .collect()
    }

    /// "Send to Claude" on a card: holds the result for the turn end, or
    /// flushes it now.
    pub fn send_delegation(
        &mut self,
        chats: &[DeliveryChat],
        tab_id: usize,
        delegation_id: &str,
        finding_ids: &[String],
        auto_tell: bool,
    ) -> Vec<DeliveryEffect> {
        let streaming = chats
            .iter()
            .any(|chat| chat.tab_id == tab_id && chat.streaming);
        let delegation = match self.chat_delegation(chats, tab_id, delegation_id) {
            Ok(delegation) => delegation.clone(),
            Err(error) => return vec![DeliveryEffect::NotSent(error)],
        };
        let effects = self.delegations.reduce(Intent::ManualSend {
            tab_id,
            delegation: &delegation,
            finding_ids,
            streaming,
        });
        let mut out = Vec::new();
        for effect in effects {
            match effect {
                Effect::Flush(tab_id) => return self.flush_delivery(chats, tab_id, auto_tell),
                Effect::Refresh(id) => out.push(DeliveryEffect::Refresh(id)),
                Effect::AlreadyDelivered => out.push(DeliveryEffect::AlreadyDelivered),
                Effect::Rejected(error) => out.push(DeliveryEffect::NotSent(error)),
                _ => unreachable!("manual send effect"),
            }
        }
        out
    }

    /// Applies the frontend's report of `delivery` to chat `tab_id`.
    pub fn delivery_completed(
        &mut self,
        tab_id: usize,
        delivery: RequestId,
        completion: DeliveryCompletion,
    ) -> DeliveryReceipt {
        let effects = self.delegations.reduce(Intent::Submitted {
            tab_id,
            delivery,
            accepted: completion == DeliveryCompletion::Accepted,
        });
        let Some(effect) = effects.into_iter().next() else {
            return DeliveryReceipt::Stale;
        };
        let Effect::RecordDelivery(ids) = effect else {
            return DeliveryReceipt::NotSubmitted;
        };
        let timestamp = chrono::Utc::now().to_rfc3339();
        let saved = self
            .store_mut()
            .as_mut()
            .ok_or_else(|| STORE_UNAVAILABLE.to_string())
            .and_then(|store| {
                store
                    .mark_delegations_delivered(&ids, &timestamp)
                    .map_err(|error| error.to_string())
            });
        match saved {
            Ok(()) => DeliveryReceipt::Saved,
            Err(error) => DeliveryReceipt::SaveFailed(error),
        }
    }

    /// Dismisses the card `delegation_id` on `tab_id`: an active run is
    /// stopped and cancelled, a queued worker launch for it leaves the
    /// queue, and the card is hidden.
    pub fn dismiss_delegation(
        &mut self,
        chats: &[DeliveryChat],
        tab_id: usize,
        delegation_id: &str,
        timestamp: &str,
    ) -> Result<Dismissal, String> {
        let delegation = self.chat_delegation(chats, tab_id, delegation_id)?.clone();
        let mut abort = Vec::new();
        let mut active = false;
        for effect in self.delegations.reduce(Intent::Dismiss {
            delegation: &delegation,
        }) {
            match effect {
                Effect::Abort(id) => abort.push(id),
                Effect::PersistDismiss { cancel, .. } => active = cancel,
                _ => unreachable!("dismiss effect"),
            }
        }
        let (store, queue) = self.store_and_launch_queue_mut();
        let saved = match store.as_mut() {
            Some(store) => {
                let cancelled = if active {
                    store.cancel_delegation(delegation_id, timestamp)
                } else {
                    Ok(())
                };
                cancelled
                    .and_then(|()| {
                        super::cancel_queued_worker_launch(store, queue, delegation_id, timestamp)
                    })
                    .and_then(|()| store.dismiss_delegation(delegation_id, timestamp))
                    .map_err(|error| error.to_string())
            }
            None => Err(STORE_UNAVAILABLE.to_string()),
        };
        Ok(Dismissal { abort, saved })
    }
}
