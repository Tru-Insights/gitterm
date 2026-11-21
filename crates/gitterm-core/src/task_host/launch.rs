//! Starting a task session (H4b): the host admits the launch, owns the
//! launch queue, and publishes the session only after it is saved.
//!
//! The frontend resolves its preset and builds the command, then asks
//! [`TaskHost::admit_launch`]. Admission runs the plan-limit policy for the
//! launch origin, the explicit-choice override and the local concurrency
//! limit, and either queues the launch (written to the store first) or hands
//! back a [`LaunchTicket`]. The frontend starts the unpublished child (a
//! terminal tab) and gives it to [`TaskHost::publish_launch`], which saves
//! the session and attaches the worker delegation. The child comes back only
//! when both writes succeeded; on failure the host drops it, which stops it.
use super::TaskHost;
use crate::plan_limits::{PolicyEvaluation, Provider};
use crate::preview::service::RunnerEffect;
use crate::task_commands::{
    SessionLaunch, TaskLaunchOrigin, TaskMutation, TaskMutationAction, TaskMutationCommand,
    TaskMutationFacts,
};
use crate::tasks::{
    DelegationChild, DelegationRunOptions, HarnessConversationRef, HarnessSelection,
    ObjectiveDeliveryState, TaskLifecycle, TaskSessionRecord, TaskStore, TaskStoreError,
};
use crate::workers::{provider_for_command, WorkerProvider};
use std::path::PathBuf;

const STORE_GONE: &str = "GitTerm's task store became unavailable";

/// A launch the local concurrency limit or plan policy deferred: the task
/// sits in the store as `Queued`; this remembers which preset to start when
/// a slot frees.
#[derive(Debug, Clone)]
pub struct QueuedTaskLaunch {
    pub task_id: String,
    pub preset_idx: Option<usize>,
    pub origin: TaskLaunchOrigin,
    pub user_model_choice: bool,
}

/// A re-request for a queued task keeps its place. A user request makes the
/// entry the user's; an explicit model choice is never taken back.
pub fn update_queued_task_request(
    queue: &mut [QueuedTaskLaunch],
    task_id: &str,
    origin: TaskLaunchOrigin,
    user_model_choice: bool,
) -> Option<usize> {
    let position = queue.iter().position(|entry| entry.task_id == task_id)?;
    let entry = &mut queue[position];
    if matches!(origin, TaskLaunchOrigin::User) {
        entry.origin = TaskLaunchOrigin::User;
    }
    entry.user_model_choice |= user_model_choice;
    Some(position)
}

/// A queued worker whose delegation was cancelled before its session ever
/// started leaves the queue, and its task stops.
pub fn cancel_queued_worker_launch(
    store: &mut TaskStore,
    queue: &mut Vec<QueuedTaskLaunch>,
    delegation_id: &str,
    timestamp: &str,
) -> Result<(), TaskStoreError> {
    let task_id = store
        .delegation(delegation_id)
        .filter(|d| d.status == crate::tasks::DelegationStatus::Cancelled)
        .and_then(|d| match &d.child {
            DelegationChild::TaskSession {
                task_id,
                task_session_id: None,
                ..
            } => Some(task_id.clone()),
            _ => None,
        });
    if let Some(task_id) = task_id {
        let before = queue.len();
        queue.retain(|launch| launch.task_id != task_id);
        if queue.len() != before {
            store.record_lifecycle_signal(&task_id, TaskLifecycle::Stopped, None, timestamp)?;
        }
    }
    Ok(())
}

/// The plan-limit provider a launch command spends, if any.
pub fn launch_provider(command: &str) -> Option<Provider> {
    match provider_for_command(command) {
        WorkerProvider::Claude => Some(Provider::Claude),
        WorkerProvider::Codex => Some(Provider::Codex),
        WorkerProvider::Other | WorkerProvider::Antigravity => None,
    }
}

/// An open worker delegation this launch carries out.
#[derive(Debug, Clone)]
pub struct AwaitingWorker {
    pub delegation_id: String,
    pub model: Option<String>,
    pub options: DelegationRunOptions,
}

/// Plan-limit facts the frontend counts from its live sessions.
pub struct LaunchPolicy {
    pub evaluation: PolicyEvaluation,
    pub reason: String,
    /// Live worker sessions of the launch's provider.
    pub running_workers: usize,
    /// Codex runs the frontend has started (headless delegations).
    pub codex_runs: usize,
}

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub task_id: String,
    pub preset_idx: Option<usize>,
    pub preset_name: Option<String>,
    pub origin: TaskLaunchOrigin,
    /// The requester's own explicit choice. The worker's choice counts for
    /// policy too, but only the requester's is kept on a queue entry.
    pub user_model_choice: bool,
    pub worker: Option<AwaitingWorker>,
    /// The provider the final command spends (see [`launch_provider`]).
    pub provider: Option<Provider>,
    /// Recorded on a queued task so a restart relaunches the same preset.
    pub harness: Option<HarnessSelection>,
    /// A preset child starts its harness at once and is live from launch.
    pub launched_agent: bool,
    pub worktree: PathBuf,
}

#[derive(Debug)]
pub enum LaunchAdmission {
    /// Saved as Queued; 1-based place in the queue.
    Queued {
        position: usize,
    },
    Open(LaunchTicket),
}

/// An admitted launch, not yet published. Hand it, with the started child,
/// to [`TaskHost::publish_launch`], or to [`TaskHost::abandon_launch`] when
/// the child could not start.
#[derive(Debug)]
#[must_use = "an admitted launch must be published or abandoned"]
pub struct LaunchTicket {
    task_id: String,
    session_id: String,
    worker: Option<String>,
}

impl LaunchTicket {
    #[cfg(test)]
    pub(super) fn for_test(task_id: &str, worker: Option<&str>) -> Self {
        Self {
            task_id: task_id.into(),
            session_id: "session".into(),
            worker: worker.map(str::to_string),
        }
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    /// The GitTerm task session the child is started as.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// The worker delegation the session will be attached to.
    pub fn worker(&self) -> Option<&str> {
        self.worker.as_deref()
    }
}

/// What the frontend started, for the saved session record.
#[derive(Debug, Clone)]
pub struct LaunchedSession {
    pub label: String,
    pub harness: Option<HarnessSelection>,
    pub conversation: Option<HarnessConversationRef>,
    pub objective_delivery: ObjectiveDeliveryState,
    pub timestamp: String,
}

/// One step of queue promotion.
#[derive(Debug)]
pub enum Promotion {
    /// No capacity (or promotion is not allowed): leave the queue as it is.
    Stop,
    /// The head entry was dropped: its task is no longer queued.
    Skip,
    /// Relaunch this entry; it has left the queue.
    Start(QueuedTaskLaunch),
}

impl TaskHost {
    pub fn launch_queue(&self) -> &[QueuedTaskLaunch] {
        &self.launch_queue
    }

    /// Transitional frontend seam, like [`Self::store_mut`].
    pub fn launch_queue_mut(&mut self) -> &mut Vec<QueuedTaskLaunch> {
        &mut self.launch_queue
    }

    pub fn store_and_launch_queue_mut(
        &mut self,
    ) -> (&mut Option<TaskStore>, &mut Vec<QueuedTaskLaunch>) {
        (self.authority.store_mut(), &mut self.launch_queue)
    }

    /// The queue rebuilt from the store at startup.
    pub fn restore_launch_queue(&mut self, queue: Vec<QueuedTaskLaunch>) {
        self.launch_queue = queue;
    }

    /// The worker delegation open on `task_id` that a launch of
    /// `preset_name` carries out: its session never started, or its tab is
    /// gone (`live` says whether a session still has one).
    pub fn worker_awaiting_launch(
        &self,
        task_id: &str,
        preset_name: &str,
        live: impl Fn(&str) -> bool,
    ) -> Option<AwaitingWorker> {
        self.store()
            .as_ref()?
            .open_worker_delegations(task_id)
            .into_iter()
            .find_map(|delegation| match &delegation.child {
                DelegationChild::TaskSession {
                    preset_name: Some(preset),
                    task_session_id,
                    model,
                    ..
                } if preset == preset_name
                    && task_session_id
                        .as_deref()
                        .is_none_or(|session| !live(session)) =>
                {
                    Some(AwaitingWorker {
                        delegation_id: delegation.delegation_id.clone(),
                        model: model.clone(),
                        options: delegation.options.clone(),
                    })
                }
                _ => None,
            })
    }

    /// Decides whether `request` starts now or waits. `facts` are the
    /// frontend's current mutation facts with its slot holders; the host adds
    /// the policy decision. A queued launch is acknowledged only after the
    /// store has recorded it.
    pub fn admit_launch(
        &mut self,
        request: LaunchRequest,
        policy: LaunchPolicy,
        mut facts: TaskMutationFacts,
    ) -> Result<LaunchAdmission, String> {
        let worker_choice = request
            .worker
            .as_ref()
            .is_some_and(|worker| worker.options.user_model_choice);
        let user_model_choice = worker_choice || request.user_model_choice;
        // A delegation may have queued before its provider crossed a limit.
        // UI launches are the user's own choice. MCP launches, including
        // tasks without delegations, must pass policy unless explicitly
        // overridden.
        facts.policy_blocked = request.provider.is_some_and(|provider| {
            request.origin.policy_blocked(
                &policy.evaluation,
                provider,
                policy.running_workers > 0
                    || (provider == Provider::Codex
                        && (self.delegations.busy() || policy.codex_runs > 0)),
                user_model_choice,
            )
        });
        facts.policy_reason = policy.reason;
        let decision = self
            .reduce_mutation(
                TaskMutationCommand {
                    reply: None,
                    mutation: TaskMutation::LaunchSession(SessionLaunch {
                        task_id: request.task_id.clone(),
                        preset_name: request.preset_name.clone(),
                        origin: request.origin,
                        user_model_choice,
                    }),
                },
                &facts,
            )
            .action?;
        self.services
            .task_action(&decision, Some(&request.worktree))
            .map_err(|denied| denied.notice().to_string())?;
        let task_id = request.task_id;
        match decision {
            TaskMutationAction::QueueSession { position, .. } => {
                if let Some(position) = update_queued_task_request(
                    &mut self.launch_queue,
                    &task_id,
                    request.origin,
                    request.user_model_choice,
                ) {
                    return Ok(LaunchAdmission::Queued {
                        position: position + 1,
                    });
                }
                let store = self.store_mut().as_mut().ok_or(STORE_GONE)?;
                let timestamp = chrono::Utc::now().to_rfc3339();
                if let Some(selection) = request.harness {
                    store
                        .record_requested_harness(&task_id, selection, &timestamp)
                        .map_err(|error| error.to_string())?;
                }
                store
                    .record_lifecycle_signal(&task_id, TaskLifecycle::Queued, None, &timestamp)
                    .map_err(|error| error.to_string())?;
                self.launch_queue.push(QueuedTaskLaunch {
                    task_id,
                    preset_idx: request.preset_idx,
                    origin: request.origin,
                    user_model_choice: request.user_model_choice,
                });
                Ok(LaunchAdmission::Queued { position })
            }
            TaskMutationAction::LaunchSession { .. } => {
                if request.launched_agent && !facts.slot_holders.contains(&task_id) {
                    self.launch_queue.retain(|entry| entry.task_id != task_id);
                }
                Ok(LaunchAdmission::Open(LaunchTicket {
                    task_id,
                    session_id: uuid::Uuid::new_v4().to_string(),
                    worker: request.worker.map(|worker| worker.delegation_id),
                }))
            }
            _ => unreachable!("launch effect"),
        }
    }

    /// Saves the session, then attaches the worker delegation to it, and
    /// only then returns `child` for the frontend to publish. On either
    /// failure the host drops `child`, stopping the unpublished process.
    /// shortcut: a failed attach keeps the session record it follows (as
    /// before H4b); removing it would change saved state.
    pub fn publish_launch<C>(
        &mut self,
        ticket: LaunchTicket,
        session: LaunchedSession,
        child: C,
    ) -> Result<C, String> {
        let LaunchTicket {
            task_id,
            session_id,
            worker,
        } = ticket;
        let saved = (|| {
            let store = self.store_mut().as_mut().ok_or(STORE_GONE)?;
            let conversation = session.conversation.clone();
            store
                .upsert_session(
                    &task_id,
                    TaskSessionRecord {
                        task_session_id: session_id.clone(),
                        label: session.label,
                        harness: session.harness,
                        conversation: session.conversation,
                        objective_delivery: session.objective_delivery,
                        launch_identity: None,
                        created_at: session.timestamp.clone(),
                        updated_at: session.timestamp.clone(),
                    },
                    &session.timestamp,
                )
                .map_err(|error| error.to_string())?;
            if let Some(delegation_id) = &worker {
                // The worker is running; its report reaches the delegation.
                store
                    .attach_worker_session(
                        delegation_id,
                        &session_id,
                        conversation,
                        &session.timestamp,
                    )
                    .map_err(|error| {
                        format!(
                            "worker session {session_id} could not attach to delegation {delegation_id}: {error}; stopping worker"
                        )
                    })?;
            }
            Ok::<(), String>(())
        })();
        match saved {
            Ok(()) => Ok(child),
            Err(error) => {
                drop(child);
                Err(error)
            }
        }
    }

    /// The child never started; nothing was saved for this ticket.
    pub fn abandon_launch(&mut self, ticket: LaunchTicket) {
        drop(ticket);
    }

    /// The next queued launch to start, oldest first. `running` is the
    /// frontend's count of tasks holding a slot now.
    pub fn next_queued_launch(&mut self, running: usize, limit: usize) -> Promotion {
        if running >= limit.max(1) {
            return Promotion::Stop;
        }
        let Some(head) = self.launch_queue.first() else {
            return Promotion::Stop;
        };
        let worktree = self
            .store()
            .as_ref()
            .and_then(|store| store.get(&head.task_id))
            .and_then(|task| task.worktree.path.clone());
        if self
            .services
            .runner(RunnerEffect::QueuePromotion, worktree.as_deref())
            .is_err()
        {
            return Promotion::Stop;
        }
        let entry = self.launch_queue.remove(0);
        // The queue only holds launches for tasks still marked Queued;
        // anything cancelled, archived, or started by hand just drops out.
        let still_queued = self
            .store()
            .as_ref()
            .and_then(|store| store.get(&entry.task_id))
            .is_some_and(|task| task.lifecycle == TaskLifecycle::Queued);
        if still_queued {
            Promotion::Start(entry)
        } else {
            Promotion::Skip
        }
    }

    /// A queued launch that can no longer start (worktree gone, preset
    /// removed) must not sit Queued forever.
    pub fn fail_queued_launch(&mut self, task_id: &str, error: &str) {
        let timestamp = chrono::Utc::now().to_rfc3339();
        if let Some(store) = self.store_mut().as_mut() {
            if let Err(store_error) = store.record_lifecycle_signal(
                task_id,
                TaskLifecycle::Failed,
                Some(error.to_string()),
                &timestamp,
            ) {
                eprintln!("GitTerm V6 dropped a queued-launch failure signal: {store_error}");
            }
        }
    }

    pub fn cancel_queued_worker_launch(
        &mut self,
        delegation_id: &str,
        timestamp: &str,
    ) -> Result<(), String> {
        let (store, queue) = self.store_and_launch_queue_mut();
        let store = store.as_mut().ok_or(STORE_GONE)?;
        cancel_queued_worker_launch(store, queue, delegation_id, timestamp)
            .map_err(|error| error.to_string())
    }
}
