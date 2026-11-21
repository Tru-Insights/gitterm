//! Review/consult lifecycle authority. The host owns the sole task store and
//! executes effects synchronously; process handles and IO never enter this state.
//! Worker preparation and worker deadlines remain with TaskAuthority/the host.
use std::collections::{HashMap, HashSet};

use crate::codex_runner::CodexRunEvent;
use crate::delegations::{self, CallerTab, HeldSend, SendBatch, SendDecision};
use crate::plan_limits::PolicyEvaluation;
use crate::session::Target;
use crate::tasks::RepositoryIdentity;
use crate::tasks::{Delegation, NewDelegation};
use std::path::PathBuf;

/// Catalog facts and runtime incarnation, captured without filesystem IO.
/// A persisted caller UID alone cannot authorize an async continuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub tab: CallerTab,
    pub target: Target,
    pub workspace_uid: u64,
    pub local_root: Option<PathBuf>,
}

/// Filesystem facts collected off-thread after the existing checkout checks.
/// Missing optional authorization roots are represented, not made mandatory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub checkout: PathBuf,
    pub repository: RepositoryIdentity,
    pub workdir: PathBuf,
    pub roots: Vec<Option<PathBuf>>,
}

/// A new user-authorized rerun belongs to the current caller. Its historical
/// default checkout remains an explicit target if the caller moved meanwhile.
pub fn rerun(delegation: &Delegation, caller: &CallerTab) -> Result<NewDelegation, String> {
    let mut new = delegations::rerun_delegation(delegation)?;
    if new.options.checkout.is_none() && new.parent.cwd != caller.cwd {
        new.options.checkout = Some(new.parent.cwd.clone());
    }
    new.parent.session_uid = caller.session_uid.clone();
    new.parent.chat_session_id = caller.chat_session_id.clone();
    new.parent.workspace = caller.workspace.clone();
    new.parent.cwd = caller.cwd.clone();
    Ok(new)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

pub enum Intent<'a> {
    Request {
        new: Box<NewDelegation>,
        caller: Caller,
    },
    Checked {
        request: RequestId,
        new: Box<NewDelegation>,
        caller: Option<&'a Caller>,
        result: Result<Authority, String>,
    },
    /// Sent only after the host successfully inserts the requested record.
    Stored {
        id: String,
        caller: Caller,
        authority: Authority,
    },
    Admit {
        delegations: &'a [Delegation],
        running: &'a HashSet<String>,
        callers: &'a HashMap<String, Caller>,
        max_concurrent: usize,
        policy: &'a PolicyEvaluation,
        running_workers: usize,
    },
    StartChecked {
        delegation: &'a Delegation,
        check: RequestId,
        caller: Option<&'a Caller>,
        result: Result<Authority, String>,
        running: &'a HashSet<String>,
        max_concurrent: usize,
        policy: &'a PolicyEvaluation,
        running_workers: usize,
    },
    Progress {
        delegation: &'a Delegation,
        event: CodexRunEvent,
    },
    Finished {
        delegation: &'a Delegation,
    },
    Dismiss {
        delegation: &'a Delegation,
    },
    ManualSend {
        tab_id: usize,
        delegation: &'a Delegation,
        finding_ids: &'a [String],
        streaming: bool,
    },
    Flush {
        tab_id: usize,
        recipient_exists: bool,
        delegations: &'a [&'a Delegation],
        streaming: bool,
        automatic: bool,
    },
    /// The frontend's report of a `Deliver`. Applied only while `delivery`
    /// is still the chat's reservation.
    Submitted {
        tab_id: usize,
        delivery: RequestId,
        accepted: bool,
    },
}

#[derive(Debug)]
pub enum Effect {
    Check {
        request: RequestId,
        new: Box<NewDelegation>,
        caller: CallerTab,
    },
    Insert {
        new: Box<NewDelegation>,
        caller: Box<Caller>,
        authority: Box<Authority>,
    },
    CheckStart {
        id: String,
        check: RequestId,
        new: Box<NewDelegation>,
        caller: CallerTab,
    },
    Start(String),
    StartRejected {
        id: String,
        error: String,
    },
    RecordStarted {
        id: String,
        thread_id: String,
    },
    RecordCompletion(String),
    Abort(String),
    PersistDismiss {
        id: String,
        cancel: bool,
    },
    Refresh(String),
    Flush(usize),
    /// Reserved: no flush submits these results again until `Submitted`
    /// reports `delivery`.
    Deliver {
        tab_id: usize,
        delivery: RequestId,
        batch: SendBatch,
    },
    /// An accepted submission: write these results' receipts.
    RecordDelivery(Vec<String>),
    Rejected(String),
    AlreadyDelivered,
}

#[derive(Debug)]
struct PendingRequest {
    caller: Caller,
    previous: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    next_request: u64,
    pending: HashMap<RequestId, PendingRequest>,
    callers: HashMap<String, (Caller, Authority)>,
    checks: HashMap<String, RequestId>,
    reserved: HashSet<String>,
    // Terminal/cancelled attempts stay settled even if their store write failed.
    // A rerun gets a fresh durable delegation ID and new policy admission.
    settled: HashSet<String>,
    activity: HashMap<String, String>,
    held: HashMap<usize, Vec<HeldSend>>,
    // The one submission per chat that is out with the frontend.
    submitting: HashMap<usize, (RequestId, Vec<String>)>,
}

impl State {
    pub fn pending_start(&self, id: &str) -> Option<RequestId> {
        self.checks.get(id).copied()
    }
    pub fn admitted(&self, id: &str) -> bool {
        self.reserved.contains(id)
    }
    pub fn busy(&self) -> bool {
        !self.reserved.is_empty()
    }
    pub fn activity(&self, id: &str) -> Option<&str> {
        self.activity.get(id).map(String::as_str)
    }
    pub fn held(&self, tab_id: usize) -> Option<&Vec<HeldSend>> {
        self.held.get(&tab_id)
    }
    pub fn held_tabs(&self) -> impl Iterator<Item = usize> + '_ {
        self.held.keys().copied()
    }
    /// The submission chat `tab_id` has out, if any.
    pub fn submitting(&self, tab_id: usize) -> Option<RequestId> {
        self.submitting.get(&tab_id).map(|(delivery, _)| *delivery)
    }
    fn reserved_for_delivery(&self, id: &str) -> bool {
        self.submitting
            .values()
            .any(|(_, ids)| ids.iter().any(|reserved| reserved == id))
    }

    pub fn reduce(&mut self, intent: Intent<'_>) -> Vec<Effect> {
        match intent {
            Intent::Request { new, caller } => {
                if caller.tab.remote
                    || new.parent.session_uid != caller.tab.session_uid
                    || new.parent.chat_session_id != caller.tab.chat_session_id
                    || new.parent.workspace != caller.tab.workspace
                    || new.parent.cwd != caller.tab.cwd
                {
                    return vec![Effect::Rejected(
                        "The calling GitTerm tab changed; request the delegation again".into(),
                    )];
                }
                if new.previous.as_ref().is_some_and(|previous| {
                    self.pending
                        .values()
                        .any(|p| p.previous.as_ref() == Some(previous))
                }) {
                    return vec![Effect::Rejected(
                        "That delegation rerun is already being checked".into(),
                    )];
                }
                self.next_request += 1;
                let request = RequestId(self.next_request);
                self.pending.insert(
                    request,
                    PendingRequest {
                        caller: caller.clone(),
                        previous: new.previous.clone(),
                    },
                );
                vec![Effect::Check {
                    request,
                    new,
                    caller: caller.tab,
                }]
            }
            Intent::Checked {
                request,
                new,
                caller,
                result,
            } => {
                let Some(pending) = self.pending.remove(&request) else {
                    return Vec::new();
                };
                let authority = match result {
                    Ok(authority) => authority,
                    Err(error) => return vec![Effect::Rejected(error)],
                };
                if caller != Some(&pending.caller) {
                    return vec![Effect::Rejected("The calling GitTerm tab changed while the checkout was checked; request the delegation again".into())];
                }
                vec![Effect::Insert {
                    new,
                    caller: Box::new(pending.caller),
                    authority: Box::new(authority),
                }]
            }
            Intent::Stored {
                id,
                caller,
                authority,
            } => {
                self.callers.insert(id, (caller, authority));
                Vec::new()
            }
            Intent::Admit {
                delegations,
                running,
                callers,
                max_concurrent,
                policy,
                running_workers,
            } => {
                let occupied: HashSet<_> = self.reserved.union(running).cloned().collect();
                let mut effects = Vec::new();
                let mut eligible = Vec::new();
                for d in delegations {
                    if d.codex_child().is_none()
                        || !d.status.is_active()
                        || occupied.contains(&d.delegation_id)
                        || self.settled.contains(&d.delegation_id)
                    {
                        continue;
                    }
                    if self
                        .callers
                        .get(&d.delegation_id)
                        .is_none_or(|(expected, _)| {
                            callers.get(&d.parent.session_uid) != Some(expected)
                        })
                    {
                        self.settled.insert(d.delegation_id.clone());
                        effects.push(Effect::StartRejected { id: d.delegation_id.clone(), error: "The calling GitTerm tab changed before the queued delegation could start; request it again".into() });
                    } else {
                        eligible.push(d.clone());
                    }
                }
                for id in delegations::runs_to_start(
                    &eligible,
                    &occupied,
                    max_concurrent,
                    policy,
                    running_workers,
                ) {
                    let d = delegations.iter().find(|d| d.delegation_id == id).unwrap();
                    let new = match delegations::rerun_delegation(d) {
                        Ok(new) => new,
                        Err(error) => {
                            self.settled.insert(id.clone());
                            effects.push(Effect::StartRejected { id, error });
                            continue;
                        }
                    };
                    self.next_request += 1;
                    let check = RequestId(self.next_request);
                    self.checks.insert(id.clone(), check);
                    self.reserved.insert(id.clone());
                    effects.push(Effect::CheckStart {
                        id: id.clone(),
                        check,
                        new: Box::new(new),
                        caller: self.callers[&id].0.tab.clone(),
                    });
                }
                effects
            }
            Intent::StartChecked {
                delegation,
                check,
                caller,
                result,
                running,
                max_concurrent,
                policy,
                running_workers,
            } => {
                let id = &delegation.delegation_id;
                if self.checks.get(id) != Some(&check) {
                    return Vec::new();
                }
                self.checks.remove(id);
                if !delegation.status.is_active() {
                    self.reserved.remove(id);
                    self.settled.insert(id.clone());
                    return Vec::new();
                }
                let (expected, authority) = &self.callers[id];
                let error = match result {
                    Err(error) => Some(error),
                    Ok(current) if caller != Some(expected) || current != *authority => Some("The calling GitTerm tab or checkout changed before the delegation could start; request it again".into()),
                    Ok(_) => None,
                };
                if let Some(error) = error {
                    self.reserved.remove(id);
                    self.settled.insert(id.clone());
                    return vec![Effect::StartRejected {
                        id: id.clone(),
                        error,
                    }];
                }
                let occupied: HashSet<_> = self.reserved.union(running).collect();
                if occupied.len() > max_concurrent.max(1)
                    || !policy.allows_start(
                        crate::plan_limits::Provider::Codex,
                        running_workers > 0 || occupied.iter().any(|other| *other != id),
                        delegation.options.user_model_choice,
                    )
                {
                    self.reserved.remove(id);
                    return vec![Effect::Refresh(id.clone())];
                }
                vec![Effect::Start(id.clone())]
            }
            Intent::Progress { delegation, event } => {
                let id = &delegation.delegation_id;
                if !self.reserved.contains(id)
                    || self.checks.contains_key(id)
                    || !delegation.status.is_active()
                {
                    return Vec::new();
                }
                let mut effects = Vec::new();
                match event {
                    CodexRunEvent::Started { thread_id } => effects.push(Effect::RecordStarted {
                        id: id.clone(),
                        thread_id,
                    }),
                    CodexRunEvent::Activity { description } => {
                        self.activity.insert(id.clone(), description);
                    }
                    CodexRunEvent::Completed | CodexRunEvent::Failed { .. } => return Vec::new(),
                }
                effects.push(Effect::Refresh(id.clone()));
                effects
            }
            Intent::Finished { delegation } => {
                let id = &delegation.delegation_id;
                self.activity.remove(id);
                if self.checks.contains_key(id)
                    || !self.reserved.remove(id)
                    || !self.settled.insert(id.clone())
                    || !delegation.status.is_active()
                {
                    return Vec::new();
                }
                vec![Effect::RecordCompletion(id.clone())]
            }
            Intent::Dismiss { delegation } => {
                let id = &delegation.delegation_id;
                let mut effects = Vec::new();
                if self.settled.insert(id.clone()) {
                    effects.push(Effect::Abort(id.clone()));
                }
                self.reserved.remove(id);
                self.checks.remove(id);
                self.activity.remove(id);
                for held in self.held.values_mut() {
                    held.retain(|s| s.delegation_id != *id);
                }
                effects.push(Effect::PersistDismiss {
                    id: id.clone(),
                    cancel: delegation.status.is_active(),
                });
                effects
            }
            Intent::ManualSend {
                tab_id,
                delegation,
                finding_ids,
                streaming,
            } => {
                // Already out with the frontend: like a held send, wait.
                if self.reserved_for_delivery(&delegation.delegation_id) {
                    return Vec::new();
                }
                let message = match delegations::compose_send_message(delegation, finding_ids) {
                    Ok(message) => message,
                    Err(error) => return vec![Effect::Rejected(error)],
                };
                let held = self.held.entry(tab_id).or_default();
                match delegations::decide_send(
                    streaming,
                    delegation.is_delivered(),
                    held,
                    &delegation.delegation_id,
                ) {
                    SendDecision::SendNow | SendDecision::Hold => {
                        held.push(HeldSend {
                            delegation_id: delegation.delegation_id.clone(),
                            message,
                            automatic: false,
                        });
                        if streaming {
                            vec![Effect::Refresh(delegation.delegation_id.clone())]
                        } else {
                            vec![Effect::Flush(tab_id)]
                        }
                    }
                    SendDecision::AlreadyDelivered => vec![Effect::AlreadyDelivered],
                    SendDecision::AlreadyHeld => Vec::new(),
                }
            }
            Intent::Flush {
                tab_id,
                recipient_exists,
                delegations,
                streaming,
                automatic,
            } => {
                if !recipient_exists {
                    self.held.remove(&tab_id);
                    return Vec::new();
                }
                // Results out with the frontend are neither queued again
                // nor resubmitted before their completion.
                let delegations: Vec<&Delegation> = delegations
                    .iter()
                    .copied()
                    .filter(|d| !self.reserved_for_delivery(&d.delegation_id))
                    .collect();
                let delegations = delegations.as_slice();
                let reserved = self.submitting.contains_key(&tab_id);
                let held = self.held.entry(tab_id).or_default();
                let mut ids: HashSet<_> =
                    held.iter().map(|send| send.delegation_id.clone()).collect();
                // The host supplies only records whose parent resolves to this
                // local chat, including Resume as Chat's persisted fallback.
                held.retain(|send| {
                    delegations.iter().any(|d| {
                        d.delegation_id == send.delegation_id
                            && d.status.is_sendable()
                            && !d.is_delivered()
                    })
                });
                let mut effects = Vec::new();
                if let Err(error) = delegations::queue_auto_sends(delegations, automatic, held) {
                    effects.push(Effect::Rejected(error));
                }
                ids.extend(held.iter().map(|send| send.delegation_id.clone()));
                let next = delegations::next_held_send(held, streaming || reserved);
                effects.extend(ids.into_iter().map(Effect::Refresh));
                if let Some(batch) = next {
                    self.next_request += 1;
                    let delivery = RequestId(self.next_request);
                    self.submitting
                        .insert(tab_id, (delivery, batch.delegation_ids.clone()));
                    effects.push(Effect::Deliver {
                        tab_id,
                        delivery,
                        batch,
                    });
                }
                effects
            }
            Intent::Submitted {
                tab_id,
                delivery,
                accepted,
            } => {
                if self.submitting(tab_id) != Some(delivery) {
                    return Vec::new();
                }
                let (_, ids) = self.submitting.remove(&tab_id).unwrap();
                if accepted {
                    vec![Effect::RecordDelivery(ids)]
                } else {
                    ids.into_iter().map(Effect::Refresh).collect()
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "delegation_state_tests.rs"]
mod tests;
