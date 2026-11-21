//! The headless runner's admission (H4b): which queued Codex delegations
//! start, re-checked when their off-thread checkout check returns.
//!
//! The host owns the delegation lifecycle state and reads the store itself.
//! The frontend passes what only it knows (the runs it has spawned, the
//! live caller identity of a tab, plan-limit counts), runs the checkout
//! checks and spawns the run for each `Start` it gets back. Every call
//! recounts the frontend's runs, so the starts of one batch of returning
//! checks share one running count.
use super::TaskHost;
use crate::delegation_state::{Authority, Caller, Effect, Intent, RequestId, State};
use crate::plan_limits::PolicyEvaluation;
use crate::preview::service::RunnerEffect;
use crate::tasks::Delegation;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Capacity and policy facts for one admission or checked start.
#[derive(Clone, Copy)]
pub struct RunnerCapacity<'a> {
    /// Delegations the frontend has a running process for.
    pub running: &'a HashSet<String>,
    pub max_concurrent: usize,
    pub policy: &'a PolicyEvaluation,
    /// Live Codex worker sessions.
    pub running_workers: usize,
}

impl TaskHost {
    pub fn delegations(&self) -> &State {
        &self.delegations
    }

    /// Transitional seam for intake and run progress, like
    /// [`Self::store_mut`]. Delivery goes through [`super::delivery`].
    pub fn delegations_mut(&mut self) -> &mut State {
        &mut self.delegations
    }

    /// Reserves queued Codex runs while slots are free. Each `CheckStart`
    /// needs the frontend's checkout check and then
    /// [`Self::delegation_start_checked`]; a `StartRejected` is final.
    /// `callers` maps a parent tab's session uid to its current identity,
    /// for the requested runs not yet running.
    ///
    /// Each delegation's own targets ([`delegation_targets`]) must pass the
    /// gate; one that does not stays queued, untouched, and the others are
    /// admitted as before.
    pub fn admit_delegations(
        &mut self,
        capacity: RunnerCapacity<'_>,
        callers: &HashMap<String, Caller>,
    ) -> Vec<Effect> {
        let Some(store) = self.authority.store().as_ref() else {
            return Vec::new();
        };
        let all = store.delegations();
        let allowed = |delegation: &Delegation| {
            delegation_targets(delegation).iter().all(|target| {
                self.services
                    .runner(RunnerEffect::QueuePromotion, Some(target))
                    .is_ok()
            })
        };
        // Production allows every target, so it keeps reducing the stored
        // slice itself.
        let filtered: Vec<Delegation>;
        let delegations = if all.iter().all(allowed) {
            all
        } else {
            filtered = all.iter().filter(|d| allowed(d)).cloned().collect();
            &filtered
        };
        if delegations.is_empty() {
            return Vec::new();
        }
        self.delegations.reduce(Intent::Admit {
            delegations,
            running: capacity.running,
            callers,
            max_concurrent: capacity.max_concurrent,
            policy: capacity.policy,
            running_workers: capacity.running_workers,
        })
    }

    /// The checkout check for an admitted run returned: policy, capacity,
    /// the caller and the checkout are checked again before `Start`.
    /// `caller` is the parent tab's identity now. Nothing happens when the
    /// delegation is no longer stored.
    pub fn delegation_start_checked(
        &mut self,
        id: &str,
        check: RequestId,
        result: Result<Authority, String>,
        capacity: RunnerCapacity<'_>,
        caller: Option<&Caller>,
    ) -> Vec<Effect> {
        let Some(delegation) = self
            .authority
            .store()
            .as_ref()
            .and_then(|store| store.delegation(id))
        else {
            return Vec::new();
        };
        self.delegations.reduce(Intent::StartChecked {
            delegation,
            check,
            caller,
            result,
            running: capacity.running,
            max_concurrent: capacity.max_concurrent,
            policy: capacity.policy,
            running_workers: capacity.running_workers,
        })
    }

    /// Reduces `intent_for` against the stored delegation `id`, borrowing
    /// the store and the lifecycle state together. `None` when it is not
    /// stored.
    pub fn reduce_delegation(
        &mut self,
        id: &str,
        intent_for: impl for<'d> FnOnce(&'d crate::tasks::Delegation) -> Intent<'d>,
    ) -> Option<Vec<Effect>> {
        let delegation = self.authority.store().as_ref()?.delegation(id)?;
        Some(self.delegations.reduce(intent_for(delegation)))
    }
}

/// Where a queued headless run would act: its explicit checkout, else the
/// requesting tab's checkout, plus every extra writable folder.
fn delegation_targets(delegation: &Delegation) -> Vec<PathBuf> {
    let checkout = delegation
        .options
        .checkout
        .as_ref()
        .unwrap_or(&delegation.parent.cwd);
    std::iter::once(checkout)
        .chain(&delegation.options.add_dirs)
        .cloned()
        .collect()
}
