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
use std::collections::{HashMap, HashSet};

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

    /// Transitional seam for intake and delivery (H4c), like
    /// [`Self::store_mut`].
    pub fn delegations_mut(&mut self) -> &mut State {
        &mut self.delegations
    }

    /// Reserves queued Codex runs while slots are free. Each `CheckStart`
    /// needs the frontend's checkout check and then
    /// [`Self::delegation_start_checked`]; a `StartRejected` is final.
    /// `callers` maps a parent tab's session uid to its current identity,
    /// for the requested runs not yet running.
    pub fn admit_delegations(
        &mut self,
        capacity: RunnerCapacity<'_>,
        callers: &HashMap<String, Caller>,
    ) -> Vec<Effect> {
        if self
            .services
            .runner(RunnerEffect::QueuePromotion, None)
            .is_err()
        {
            return Vec::new();
        }
        let Some(store) = self.authority.store().as_ref() else {
            return Vec::new();
        };
        self.delegations.reduce(Intent::Admit {
            delegations: store.delegations(),
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
