use super::TaskAuthority;
use crate::session;
use crate::task_commands::*;
use crate::task_worktree::{pr_base_branch, DEFAULT_TASK_BASE};
use crate::tasks::*;

#[derive(Debug)]
pub(super) struct PendingWorker {
    caller: WorkerCaller,
    stage: PreparationStage,
}

#[derive(Debug)]
enum PreparationStage {
    Captured,
    Reusing {
        expected: Box<TaskRecord>,
    },
    Bound {
        before: Box<TaskRecord>,
        expected: CompletedWorktreePreparation,
    },
    Saving {
        before: Box<TaskRecord>,
        expected: CompletedWorktreePreparation,
    },
}

impl TaskAuthority {
    /// No IO: effects are instructions, never acknowledgements of saved work.
    /// Worker completions are single-use and tied to this authority instance.
    pub fn reduce_mutation(
        &mut self,
        command: TaskMutationCommand,
        facts: &TaskMutationFacts,
    ) -> TaskMutationEffect {
        TaskMutationEffect {
            reply: command.reply,
            action: self.mutate(command.mutation, facts),
        }
    }

    /// What a pending worker's next mutation reads from the host's facts:
    /// the session UID of the caller it captured and, while a save is
    /// pending, the checkout that save re-checks. The reducer looks up no
    /// other caller or checkout, so a host gathers only these (TRU-274).
    pub fn pending_worker_subject(
        &self,
        id: WorkerPreparationId,
    ) -> Option<(&str, Option<&std::path::Path>)> {
        let pending = self.pending_workers.get(&id)?;
        let checkout = match &pending.stage {
            PreparationStage::Saving { expected, .. } => Some(expected.path.as_path()),
            PreparationStage::Reusing { expected } => expected.worktree.path.as_deref(),
            PreparationStage::Captured | PreparationStage::Bound { .. } => None,
        };
        Some((pending.caller.target.session_uid.as_str(), checkout))
    }

    fn task(&self, id: &str) -> Result<&TaskRecord, String> {
        self.store
            .as_ref()
            .and_then(|store| store.get(id))
            .ok_or_else(|| format!("Task {id} is not available"))
    }

    fn mutate(
        &mut self,
        mutation: TaskMutation,
        facts: &TaskMutationFacts,
    ) -> Result<TaskMutationAction, String> {
        use TaskMutationAction as Effect;
        match mutation {
            TaskMutation::Create(mut input) => {
                input.title = input.title.trim().to_string();
                input.objective = input.objective.trim().to_string();
                let form = input.origin == TaskCreateOrigin::Form;
                validate_task_title(&input.title).map_err(|error| {
                    if form {
                        "Title is required".into()
                    } else {
                        error
                    }
                })?;
                validate_task_objective(&input.objective).map_err(|error| {
                    if form {
                        "Objective is required".into()
                    } else {
                        error
                    }
                })?;
                if !form && !input.repository_path.is_absolute() {
                    return Err(format!(
                        "repository_path must be absolute: {}",
                        input.repository_path.display()
                    ));
                }
                input.issue_key = input
                    .issue_key
                    .map(|key| {
                        let key = key.trim();
                        if form {
                            key.to_string()
                        } else {
                            key.to_uppercase()
                        }
                    })
                    .filter(|key| !key.is_empty());
                input.base_reference = input
                    .base_reference
                    .map(|base| base.trim().to_string())
                    .filter(|base| !base.is_empty());
                if form {
                    validate_base_reference(input.base_reference.as_deref().unwrap_or_default())?;
                }
                if input
                    .issue_key
                    .as_deref()
                    .is_some_and(|key| !is_linear_issue_key(key))
                {
                    return Err(if form {
                        "Linear issue must look like TRU-105"
                    } else {
                        "Linear issue must look like TRU-106"
                    }
                    .into());
                }
                if input.base_reference.is_none() {
                    input.base_reference = Some(DEFAULT_TASK_BASE.into());
                }
                Ok(Effect::PrepareTask(input))
            }
            TaskMutation::InsertPreparing { record } => {
                validate_task(&record)?;
                Ok(Effect::PersistPreparing { record })
            }
            TaskMutation::PreparationCompleted {
                task_id,
                result,
                cleanup_path,
            } => match result {
                Ok(prepared) => Ok(Effect::PersistPreparation { task_id, prepared }),
                Err(error) => Ok(Effect::PersistWorkerPreparationFailure {
                    task_id,
                    cleanup_path: cleanup_path.filter(|path| {
                        error.contains("rollback requires attention")
                            && facts.existing_paths.contains(path)
                    }),
                    error,
                }),
            },
            TaskMutation::WorktreeRemoved { task_id } => Ok(Effect::ClearWorktree { task_id }),
            TaskMutation::RecordDelivery {
                task_id,
                commit_sha,
                pr_url,
                pr_number,
            } => {
                let Some(task) = self.store.as_ref().and_then(|store| store.get(&task_id)) else {
                    return Ok(Effect::None);
                };
                Ok(Effect::PersistDelivery {
                    task_id,
                    delivery: DeliveryState {
                        commit_sha: Some(commit_sha),
                        pr_url: pr_url.or_else(|| task.delivery.pr_url.clone()),
                        pr_number: pr_number.or(task.delivery.pr_number),
                    },
                })
            }
            TaskMutation::UpdateHandoff(input) => self.handoff(input, facts),
            TaskMutation::Close { task_id } => {
                let task = self.task(&task_id)?;
                let mut archived = task.clone();
                archived.lifecycle = TaskLifecycle::Archived;
                validate_task_transition(task, &archived)?;
                Ok(Effect::ArchiveTask { task_id })
            }
            TaskMutation::Stop { task_id } => {
                self.task(&task_id)?;
                let mut targets = Vec::new();
                for state in &facts.sessions {
                    if state.target.task_id.as_deref() == Some(&task_id)
                        && !targets.contains(&state.target)
                    {
                        targets.push(state.target.clone());
                    }
                }
                Ok(Effect::StopTask { task_id, targets })
            }
            TaskMutation::CancelQueue { task_id } => {
                self.task(&task_id)?;
                Ok(Effect::CancelQueue { task_id })
            }
            TaskMutation::ResumeConversation {
                task_id,
                chat_id,
                timestamp,
            } => {
                let task = self.task(&task_id)?;
                let mut session = task
                    .sessions
                    .iter()
                    .rev()
                    .find(|s| {
                        s.conversation
                            .as_ref()
                            .is_some_and(|c| c.session_id == chat_id)
                    })
                    .cloned()
                    .ok_or_else(|| {
                        format!("Task {} no longer links conversation {chat_id}", task.title)
                    })?;
                if facts.sessions.iter().any(|s| {
                    s.target.task_id.as_deref() == Some(&task_id)
                        && self.store.as_ref().is_some_and(|store| {
                            store.worker_session_is_final(
                                &task_id,
                                s.target.task_session_id.as_deref(),
                            )
                        })
                        && s.target.task_session_id.as_deref() == Some(&session.task_session_id)
                }) {
                    return Err("This finished worker is closing. Reopen its conversation after its tab closes.".into());
                }
                // Iced unlinks a preassigned Claude ID that never got a message.
                if facts.phantom_conversations.contains(&chat_id) {
                    session.conversation = None;
                } else {
                    let path =
                        task.worktree.path.as_ref().ok_or_else(|| {
                            format!("Task {} has no prepared worktree", task.title)
                        })?;
                    if !facts.directories.contains(path) {
                        return Err(format!(
                            "Task {} worktree is gone at {}",
                            task.title,
                            path.display()
                        ));
                    }
                    if let Some(entry) = facts.conversations.iter().find(|c| c.chat_id == chat_id) {
                        if entry.remote {
                            return Err("Remote task conversation resume requires the remote-executor slice".into());
                        }
                        if entry.cwd != *path {
                            return Err(format!("Refusing to resume conversation {chat_id}: its recorded directory {} does not match task worktree {}", entry.cwd.display(), path.display()));
                        }
                    }
                    if !matches!(
                        task.workspace.location,
                        WorkspaceLocationIdentity::Local { .. }
                    ) {
                        return Err(
                            "Remote task conversation resume requires the remote-executor slice"
                                .into(),
                        );
                    }
                    if self.store.as_ref().is_some_and(|store| {
                        store.worker_session_is_final(&task_id, Some(&session.task_session_id))
                    }) {
                        session.task_session_id = uuid::Uuid::new_v4().to_string();
                        session.launch_identity = None;
                        session.created_at = timestamp.clone();
                    }
                }
                session.updated_at = timestamp;
                Ok(Effect::ResumeConversation {
                    task_id,
                    session: Box::new(session),
                })
            }
            TaskMutation::CloseSession { target, confirmed } => Ok(facts
                .sessions
                .iter()
                .find(|state| state.target == target)
                .map(|state| {
                    Effect::Session(state.reduce(session::Intent::Close { target, confirmed }))
                })
                .unwrap_or(Effect::None)),
            TaskMutation::InspectDelete { task_id } => {
                let task = self
                    .store
                    .as_ref()
                    .and_then(|store| store.get(&task_id))
                    .ok_or("Task is no longer in the task store")?;
                let path = task
                    .worktree
                    .path
                    .clone()
                    .ok_or("This task has no prepared worktree to delete")?;
                let WorkspaceLocationIdentity::Local { directory } = &task.workspace.location
                else {
                    return Err("Remote task worktrees are managed on their remote Mac".into());
                };
                Ok(Effect::InspectCleanup {
                    task_id: task_id.clone(),
                    repository: directory.clone(),
                    path,
                    branch: task.branch.clone(),
                    base_commit: task.base.commit.clone(),
                    process_running: task.lifecycle.is_active()
                        || facts
                            .sessions
                            .iter()
                            .any(|state| state.target.task_id.as_deref() == Some(&task_id)),
                    pull_request_url: task.delivery.pr_url.clone(),
                })
            }
            TaskMutation::DeleteWorktree {
                task_id,
                path,
                inspection,
            } => {
                let task = self.task(&task_id)?;
                if inspection.risks.iter().any(|risk| {
                    matches!(
                        risk,
                        CleanupRisk::ActiveProcess
                            | CleanupRisk::UncommittedChanges
                            | CleanupRisk::UnpushedCommits { .. }
                    )
                }) {
                    return Ok(Effect::None);
                }
                if !inspection.path_exists || !inspection.registered {
                    return Ok(Effect::ClearWorktree { task_id });
                }
                let WorkspaceLocationIdentity::Local { directory } = &task.workspace.location
                else {
                    return Err("Could not resolve the task's repository directory".into());
                };
                Ok(Effect::RemoveWorktree {
                    task_id,
                    repository: directory.clone(),
                    path,
                })
            }
            TaskMutation::Publish {
                task_id,
                action,
                confirmed,
            } => {
                let Some(task) = self.store.as_ref().and_then(|store| store.get(&task_id)) else {
                    return Ok(Effect::None);
                };
                if task.issue.is_none() && !confirmed {
                    return Ok(Effect::ConfirmPublish { task_id, action });
                }
                let Some(path) = task.worktree.path.clone() else {
                    return Ok(Effect::None);
                };
                if facts.publish_busy.contains(&task_id) {
                    return Ok(Effect::None);
                }
                let (title, body) = task.draft_pr_copy();
                Ok(Effect::Publish {
                    task_id,
                    action,
                    path,
                    branch: task.branch.clone(),
                    base: pr_base_branch(&task.base.reference).to_string(),
                    title,
                    body,
                })
            }
            TaskMutation::LaunchSession(request) => self.launch(request, facts),
            TaskMutation::CheckWorkerDestination { expected } => {
                check_destination(&expected.path, &expected.repository, facts)?;
                Ok(Effect::None)
            }
            TaskMutation::CheckWorkerCheckout { task_id, path } => {
                self.worker_checkout(&task_id, &path, facts)?;
                Ok(Effect::None)
            }
            TaskMutation::BeginWorkerPreparation { caller } => {
                let caller = facts
                    .callers
                    .iter()
                    .find(|tab| tab.target.session_uid == caller)
                    .ok_or("The calling GitTerm tab is no longer available")?
                    .clone();
                if caller.remote {
                    return Err("Remote workers are not supported yet".into());
                }
                self.next_preparation = self
                    .next_preparation
                    .checked_add(1)
                    .ok_or("Worker preparation identity exhausted")?;
                let id = WorkerPreparationId(self.next_preparation);
                self.pending_workers.insert(
                    id,
                    PendingWorker {
                        caller,
                        stage: PreparationStage::Captured,
                    },
                );
                Ok(Effect::WorkerAuthorityChecked { id })
            }
            TaskMutation::CheckWorkerPreparation { id } => {
                if let Err(error) = self.current_worker(id, facts) {
                    self.pending_workers.remove(&id);
                    return Err(error);
                }
                Ok(Effect::WorkerAuthorityChecked { id })
            }
            TaskMutation::BindWorkerPreparation {
                id,
                task_id,
                expected,
            } => {
                let result = (|| {
                    let pending = self.current_worker(id, facts)?;
                    if !matches!(pending.stage, PreparationStage::Captured) {
                        return Err("Worker preparation is already bound".into());
                    }
                    let task = self.task(&task_id)?;
                    if task.repository != expected.repository
                        || task.base != expected.base
                        || task.branch != expected.branch
                        || !expected.path.is_absolute()
                        || task
                            .worktree
                            .path
                            .as_ref()
                            .is_some_and(|path| path != &expected.path)
                    {
                        return Err(
                            "Worker preparation does not match the target task checkout".into()
                        );
                    }
                    Ok(Box::new(task.clone()))
                })();
                let before = match result {
                    Ok(task) => task,
                    Err(error) => {
                        self.pending_workers.remove(&id);
                        return Err(error);
                    }
                };
                self.pending_workers.get_mut(&id).unwrap().stage =
                    PreparationStage::Bound { before, expected };
                Ok(Effect::WorkerAuthorityChecked { id })
            }
            TaskMutation::ReuseWorkerCheckout { id, replacement } => {
                let result = (|| {
                    let pending = self.current_worker(id, facts)?;
                    if !matches!(pending.stage, PreparationStage::Captured) {
                        return Err("Worker preparation is already bound".into());
                    }
                    let current = self.task(&replacement.task_id)?;
                    // Only requested work metadata may change on reuse.
                    let mut allowed = current.clone();
                    allowed.title = replacement.title.clone();
                    allowed.objective = replacement.objective.clone();
                    allowed.issue = replacement.issue.clone();
                    allowed.stopping_boundary = replacement.stopping_boundary;
                    allowed.updated_at = replacement.updated_at.clone();
                    if &allowed != replacement.as_ref() {
                        return Err("Worker checkout reuse changed task authority".into());
                    }
                    let path = current
                        .worktree
                        .path
                        .as_ref()
                        .ok_or("Worker checkout is missing")?;
                    self.worker_checkout(&current.task_id, path, facts)?;
                    validate_task(&replacement)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    self.pending_workers.remove(&id);
                    return Err(error);
                }
                self.pending_workers.get_mut(&id).unwrap().stage = PreparationStage::Reusing {
                    expected: replacement.clone(),
                };
                Ok(Effect::PersistWorkerCheckoutReuse { id, replacement })
            }
            TaskMutation::WorkerCheckoutReuseSaved { id, result } => {
                let pending = self
                    .pending_workers
                    .remove(&id)
                    .ok_or("Worker preparation is no longer pending")?;
                result?;
                check_caller(&pending.caller, facts)?;
                let PreparationStage::Reusing { expected } = pending.stage else {
                    return Err("Worker preparation is not awaiting a reuse save".into());
                };
                if self.task(&expected.task_id)? != expected.as_ref() {
                    return Err(
                        "Saved worker checkout reuse no longer matches the target task".into(),
                    );
                }
                self.worker_checkout(
                    &expected.task_id,
                    expected
                        .worktree
                        .path
                        .as_ref()
                        .ok_or("Worker checkout is missing")?,
                    facts,
                )?;
                Ok(Effect::StartWorker {
                    task_id: expected.task_id,
                    caller: Box::new(pending.caller),
                })
            }
            TaskMutation::WorkerPrepared { id, result } => {
                let mut pending = self
                    .pending_workers
                    .remove(&id)
                    .ok_or("Worker preparation is no longer pending")?;
                check_caller(&pending.caller, facts)?;
                let PreparationStage::Bound { before, expected } = pending.stage else {
                    return Err("Worker preparation is not awaiting a completion".into());
                };
                if !facts.preparation_paths.contains(&expected.path) {
                    return Err("Worker preparation destination checkout changed; any prepared worktree remains for explicit cleanup".into());
                }
                let current = self.task(&before.task_id)?;
                if !same_target(current, &before) {
                    return Err(format!(
                        "Worker preparation target changed; prepared worktree {} was not applied",
                        expected.path.display()
                    ));
                }
                let prepared = match result {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let cleanup_path = (error.contains("rollback requires attention")
                            && facts.existing_paths.contains(&expected.path))
                        .then_some(expected.path);
                        return Ok(Effect::PersistWorkerPreparationFailure {
                            task_id: before.task_id,
                            error,
                            cleanup_path,
                        });
                    }
                };
                if prepared != expected {
                    return Err(format!(
                        "Worker preparation result does not match the target; worktree {} was not applied",
                        prepared.path.display()
                    ));
                }
                check_destination(&expected.path, &expected.repository, facts)?;
                // Preserve this operation only while its store result is pending.
                let task_id = before.task_id.clone();
                pending.stage = PreparationStage::Saving { before, expected };
                self.pending_workers.insert(id, pending);
                Ok(Effect::PersistWorkerPreparation {
                    id,
                    task_id,
                    prepared,
                })
            }
            TaskMutation::WorkerPreparationSaved { id, result } => {
                let pending = self
                    .pending_workers
                    .remove(&id)
                    .ok_or("Worker preparation is no longer pending")?;
                result?;
                check_caller(&pending.caller, facts)?;
                let PreparationStage::Saving { before, expected } = pending.stage else {
                    return Err("Worker preparation is not awaiting a save".into());
                };
                let current = self.task(&before.task_id)?;
                if current.created_at != before.created_at
                    || current.created_by != before.created_by
                    || current.workspace != before.workspace
                    || current.repository != expected.repository
                    || current.base != expected.base
                    || current.branch != expected.branch
                    || current.worktree.path.as_ref() != Some(&expected.path)
                    || current.worktree.state != TaskWorktreeState::Ready
                    || current.lifecycle != TaskLifecycle::Ready
                    || current.archived_at.is_some()
                {
                    return Err(
                        "Saved worker preparation no longer matches the target task checkout"
                            .into(),
                    );
                }
                self.worker_checkout(&before.task_id, &expected.path, facts)?;
                Ok(Effect::StartWorker {
                    task_id: before.task_id,
                    caller: Box::new(pending.caller),
                })
            }
            TaskMutation::CancelWorkerPreparation { id } => {
                self.pending_workers.remove(&id);
                Ok(Effect::None)
            }
        }
    }

    fn current_worker(
        &self,
        id: WorkerPreparationId,
        facts: &TaskMutationFacts,
    ) -> Result<&PendingWorker, String> {
        let pending = self
            .pending_workers
            .get(&id)
            .ok_or("Worker preparation is no longer pending")?;
        check_caller(&pending.caller, facts)?;
        Ok(pending)
    }

    fn handoff(
        &self,
        input: HandoffUpdate,
        facts: &TaskMutationFacts,
    ) -> Result<TaskMutationAction, String> {
        let summary = input.summary.trim().to_string();
        if summary.is_empty() {
            return Err("handoff summary is required".into());
        }
        let store = self
            .store
            .as_ref()
            .ok_or("GitTerm's task store is unavailable")?;
        let task = store.get(&input.task_id);
        let hosted_session = facts
            .sessions
            .iter()
            .find(|state| {
                Some(&state.target.session_uid) == input.caller.as_ref()
                    && state.target.task_id.as_ref() == Some(&input.task_id)
            })
            .and_then(|state| state.target.task_session_id.clone());
        let known = |id: &str| {
            task.is_some_and(|task| {
                task.sessions
                    .iter()
                    .any(|session| session.task_session_id == id)
            })
        };
        let updated_by_session_id = if let Some(id) = input.session_id.as_deref().map(str::trim) {
            if !known(id) {
                return Err(format!(
                    "Task {} does not contain session {id}",
                    input.task_id
                ));
            }
            Some(id.to_string())
        } else {
            hosted_session
                .clone()
                .filter(|id| known(id))
                .or(input.caller)
        };
        if input.status != HandoffStatus::Progress {
            crate::delegations::worker_report_target(
                &store.open_worker_delegations(&input.task_id),
                &input.task_id,
                hosted_session.as_deref(),
            )?;
        }
        let clean = |items: Vec<String>| {
            items
                .into_iter()
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        };
        Ok(TaskMutationAction::PersistHandoff {
            task_id: input.task_id,
            status: input.status,
            hosted_session,
            handoff: TaskHandoff {
                summary,
                decisions: clean(input.decisions),
                next_steps: clean(input.next_steps),
                blockers: clean(input.blockers),
                updated_by_session_id,
                updated_at: input.timestamp,
            },
        })
    }

    fn launch(
        &self,
        request: SessionLaunch,
        facts: &TaskMutationFacts,
    ) -> Result<TaskMutationAction, String> {
        // MCP resolves the name before looking up the task.
        let preset_index = request
            .preset_name
            .as_deref()
            .map(|name| {
                facts
                    .presets
                    .iter()
                    .position(|preset| preset.name.eq_ignore_ascii_case(name.trim()))
                    .ok_or_else(|| {
                        format!(
                            "Agent preset {name:?} is unavailable. Configured presets: {}",
                            facts
                                .presets
                                .iter()
                                .map(|preset| preset.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
            })
            .transpose()?;
        let task = self.task(&request.task_id)?;
        let path = task
            .worktree
            .path
            .as_ref()
            .ok_or_else(|| format!("Task {} has no prepared worktree to resume", task.title))?;
        if task.worktree.state != TaskWorktreeState::Ready || !facts.directories.contains(path) {
            return Err(format!(
                "Task {} worktree is not ready at {}",
                task.title,
                path.display()
            ));
        }
        if let Some(index) = preset_index {
            crate::workers::require_task_tools(&facts.presets[index].command)?;
        }
        if !matches!(
            task.workspace.location,
            WorkspaceLocationIdentity::Local { .. }
        ) {
            return Err("Remote task sessions require the later remote-executor slice".into());
        }
        if let Some(index) = preset_index {
            let blocked = request.origin == TaskLaunchOrigin::Agent
                && !request.user_model_choice
                && facts.policy_blocked;
            let holder = facts.slot_holders.contains(&request.task_id);
            if blocked && holder {
                return Err(facts.policy_reason.clone());
            }
            if blocked || (!holder && facts.slot_holders.len() >= facts.concurrency_limit.max(1)) {
                let position = facts
                    .queue
                    .iter()
                    .position(|id| id == &request.task_id)
                    .map(|position| position + 1)
                    .unwrap_or(facts.queue.len() + 1);
                return Ok(TaskMutationAction::QueueSession {
                    request,
                    preset_index: index,
                    position,
                });
            }
        }
        Ok(TaskMutationAction::LaunchSession {
            request,
            preset_index,
        })
    }

    fn worker_checkout(
        &self,
        task_id: &str,
        checkout: &std::path::Path,
        facts: &TaskMutationFacts,
    ) -> Result<(), String> {
        let task = self.task(task_id)?;
        let store = self.store.as_ref().unwrap();
        if task.worktree.path.as_deref() != Some(checkout) {
            return Err("The worker checkout no longer belongs to the target task".into());
        }
        if task.archived_at.is_some() {
            return Err(format!("checkout {} is the worktree of archived task \"{}\" ({}); delegate to another checkout", checkout.display(), task.title, task.task_id));
        }
        check_destination(checkout, &task.repository, facts)?;
        let blocked = store.open_worker_delegations(task_id);
        if let Some(open) = blocked
            .iter()
            .find(|d| d.status != DelegationStatus::Blocked)
        {
            return Err(format!("checkout {} is task \"{}\"'s worktree, and worker delegation {} is still open there; wait for its report, or stop tracking it, before handing that checkout more work", checkout.display(), task.title, open.delegation_id));
        }
        let live = facts.sessions.iter().any(|state| {
            state.target.task_id.as_deref() == Some(task_id) && state.task_live
                && !store.worker_session_is_final(task_id, state.target.task_session_id.as_deref())
                && !blocked.iter().any(|d| d.status == DelegationStatus::Blocked
                    && !facts.working_sessions.contains(&state.target.session_uid)
                    && matches!(&d.child, DelegationChild::TaskSession { task_session_id: Some(id), .. } if Some(id) == state.target.task_session_id.as_ref()))
        });
        if live {
            return Err(format!("checkout {} is task \"{}\"'s worktree, and a session is still Working in it; wait until it is idle or exited before handing that checkout more work", checkout.display(), task.title));
        }
        Ok(())
    }
}

fn check_destination(
    path: &std::path::Path,
    repository: &RepositoryIdentity,
    facts: &TaskMutationFacts,
) -> Result<(), String> {
    if !facts
        .checkouts
        .iter()
        .any(|checkout| checkout.path == path && &checkout.repository == repository)
    {
        return Err(format!("Worker destination checkout {} changed or is unavailable; no task write or worker launch was authorized", path.display()));
    }
    Ok(())
}

fn check_caller(expected: &WorkerCaller, facts: &TaskMutationFacts) -> Result<(), String> {
    let current = facts
        .callers
        .iter()
        .find(|caller| caller.target.session_uid == expected.target.session_uid);
    if current != Some(expected) {
        return Err("Worker preparation refused: caller, session, workspace authority or checkout changed; any prepared worktree remains for explicit cleanup".into());
    }
    Ok(())
}

fn same_target(current: &TaskRecord, before: &TaskRecord) -> bool {
    current.task_id == before.task_id
        && current.created_at == before.created_at
        && current.created_by == before.created_by
        && current.repository == before.repository
        && current.workspace == before.workspace
        && current.base == before.base
        && current.branch == before.branch
        && current.worktree == before.worktree
        && current.lifecycle == before.lifecycle
        && current.archived_at == before.archived_at
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn decide(
        state: &mut TaskAuthority,
        mutation: TaskMutation,
        facts: &TaskMutationFacts,
    ) -> Result<TaskMutationAction, String> {
        state
            .reduce_mutation(
                TaskMutationCommand {
                    reply: None,
                    mutation,
                },
                facts,
            )
            .action
    }

    fn fixture() -> (tempfile::TempDir, TaskAuthority, TaskMutationFacts) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(
            &path,
            include_bytes!("../../../../tests/fixtures/stage45/tasks/old.tasks.json"),
        )
        .unwrap();
        let mut store = TaskStore::load(path).unwrap();
        store
            .begin_worktree_preparation("fixture-task", "t1")
            .unwrap();
        let task = store.get("fixture-task").unwrap();
        let prepared = CompletedWorktreePreparation {
            repository: task.repository.clone(),
            base: task.base.clone(),
            branch: task.branch.clone(),
            path: dir.path().join("worktree"),
        };
        store
            .complete_worktree_preparation("fixture-task", prepared, "t2")
            .unwrap();
        let caller = WorkerCaller {
            target: session::Target {
                tab_id: 7,
                session_uid: "caller".into(),
                task_id: Some("source-task".into()),
                task_session_id: Some("source-session".into()),
            },
            chat_session_id: Some("source-chat".into()),
            workspace: store.get("fixture-task").unwrap().workspace.clone(),
            repository: Some(store.get("fixture-task").unwrap().repository.clone()),
            repository_workdir: Some(dir.path().join("source")),
            checkout: dir.path().join("source"),
            home: dir.path().into(),
            repos: vec![dir.path().join("source")],
            remote: false,
        };
        let facts = TaskMutationFacts {
            checkouts: vec![WorkerCheckout {
                path: dir.path().join("worktree"),
                repository: store.get("fixture-task").unwrap().repository.clone(),
            }],
            preparation_paths: vec![dir.path().join("worktree")],
            callers: vec![caller],
            directories: vec![dir.path().join("worktree")],
            presets: vec![TaskPreset {
                name: "Codex".into(),
                command: "codex".into(),
            }],
            concurrency_limit: 1,
            ..Default::default()
        };
        (dir, Some(store).into(), facts)
    }

    #[test]
    fn panel_stop_and_cancel_use_authority_and_capture_every_session_once() {
        let (_dir, mut state, mut facts) = fixture();
        let target = session::Target {
            tab_id: 1,
            session_uid: "placeholder".into(),
            task_id: Some("fixture-task".into()),
            task_session_id: Some("session".into()),
        };
        let own = session::State {
            target: target.clone(),
            control: session::TurnControl::Terminal,
            task_live: true,
            last_tab: false,
        };
        facts.sessions = vec![own.clone(), own];
        let TaskMutationAction::StopTask { targets, .. } = decide(
            &mut state,
            TaskMutation::Stop {
                task_id: "fixture-task".into(),
            },
            &facts,
        )
        .unwrap() else {
            panic!("stop effect")
        };
        assert_eq!(targets, [target]);
        assert!(matches!(
            decide(
                &mut state,
                TaskMutation::CancelQueue {
                    task_id: "fixture-task".into()
                },
                &facts
            )
            .unwrap(),
            TaskMutationAction::CancelQueue { .. }
        ));
        assert!(decide(
            &mut state,
            TaskMutation::Stop {
                task_id: "gone".into()
            },
            &facts
        )
        .unwrap_err()
        .contains("not available"));
    }

    #[test]
    fn panel_resume_preserves_link_and_worktree_guards_and_phantom_unlink() {
        let (_dir, mut state, mut facts) = fixture();
        let mut task = state.task("fixture-task").unwrap().clone();
        let session: TaskSessionRecord = serde_json::from_value(serde_json::json!({
            "task_session_id":"placeholder-session", "label":"Placeholder", "created_at":"t1", "updated_at":"t1",
            "conversation":{"backend":"codex", "session_id":"placeholder-chat"}
        })).unwrap();
        task.sessions.push(session);
        state.store_mut().as_mut().unwrap().replace(task).unwrap();
        let resume = || TaskMutation::ResumeConversation {
            task_id: "fixture-task".into(),
            chat_id: "placeholder-chat".into(),
            timestamp: "t3".into(),
        };
        let TaskMutationAction::ResumeConversation { session, .. } =
            decide(&mut state, resume(), &facts).unwrap()
        else {
            panic!("resume effect")
        };
        assert_eq!(session.task_session_id, "placeholder-session");
        assert_eq!(session.updated_at, "t3");
        facts.conversations.push(TaskConversationFact {
            chat_id: "placeholder-chat".into(),
            cwd: "/tmp/wrong".into(),
            remote: true,
        });
        assert!(decide(&mut state, resume(), &facts)
            .unwrap_err()
            .contains("Remote task conversation"));
        facts.conversations[0].remote = false;
        assert!(decide(&mut state, resume(), &facts)
            .unwrap_err()
            .contains("does not match task worktree"));
        facts.conversations.clear();
        facts.directories.clear();
        assert!(decide(&mut state, resume(), &facts)
            .unwrap_err()
            .contains("worktree is gone"));
        facts.phantom_conversations.push("placeholder-chat".into());
        let TaskMutationAction::ResumeConversation { session, .. } =
            decide(&mut state, resume(), &facts).unwrap()
        else {
            panic!("unlink effect")
        };
        assert!(session.conversation.is_none());
        assert!(decide(
            &mut state,
            TaskMutation::ResumeConversation {
                task_id: "fixture-task".into(),
                chat_id: "unlinked".into(),
                timestamp: "t3".into()
            },
            &facts
        )
        .unwrap_err()
        .contains("no longer links"));
    }

    fn expected(state: &TaskAuthority) -> CompletedWorktreePreparation {
        let task = state.task("fixture-task").unwrap();
        CompletedWorktreePreparation {
            repository: task.repository.clone(),
            base: task.base.clone(),
            branch: task.branch.clone(),
            path: task.worktree.path.clone().unwrap(),
        }
    }

    fn begin(state: &mut TaskAuthority, facts: &TaskMutationFacts) -> WorkerPreparationId {
        let TaskMutationAction::WorkerAuthorityChecked { id } = decide(
            state,
            TaskMutation::BeginWorkerPreparation {
                caller: "caller".into(),
            },
            facts,
        )
        .unwrap() else {
            panic!("expected authority capture")
        };
        id
    }

    fn bound(state: &mut TaskAuthority, facts: &TaskMutationFacts) -> WorkerPreparationId {
        let id = begin(state, facts);
        let expected = expected(state);
        decide(
            state,
            TaskMutation::BindWorkerPreparation {
                id,
                task_id: "fixture-task".into(),
                expected,
            },
            facts,
        )
        .unwrap();
        id
    }

    #[test]
    fn stale_destination_refuses_reuse_and_preparation_before_persistence() {
        for reuse in [false, true] {
            for missing in [false, true] {
                let (dir, mut state, mut facts) = fixture();
                let id = if reuse {
                    begin(&mut state, &facts)
                } else {
                    bound(&mut state, &facts)
                };
                if missing {
                    facts.checkouts.clear();
                } else {
                    facts.checkouts[0].repository.common_dir = dir.path().join("replacement.git");
                }
                let mutation = if reuse {
                    TaskMutation::ReuseWorkerCheckout {
                        id,
                        replacement: Box::new(state.task("fixture-task").unwrap().clone()),
                    }
                } else {
                    TaskMutation::WorkerPrepared {
                        id,
                        result: Ok(expected(&state)),
                    }
                };
                let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
                assert!(decide(&mut state, mutation, &facts)
                    .unwrap_err()
                    .contains("destination checkout"));
                assert_eq!(
                    std::fs::read(dir.path().join("tasks.json")).unwrap(),
                    before
                );
                assert!(decide(
                    &mut state,
                    TaskMutation::CheckWorkerPreparation { id },
                    &facts
                )
                .is_err());
            }
        }
    }

    #[test]
    fn checkout_reuse_requires_actual_save_fresh_authority_and_one_shot_completion() {
        for outcome in ["saved", "save-error", "caller", "target"] {
            let (dir, mut state, mut facts) = fixture();
            let id = begin(&mut state, &facts);
            let mut replacement = state.task("fixture-task").unwrap().clone();
            replacement.title = "Replacement work".into();
            let TaskMutationAction::PersistWorkerCheckoutReuse { replacement, .. } = decide(
                &mut state,
                TaskMutation::ReuseWorkerCheckout {
                    id,
                    replacement: Box::new(replacement),
                },
                &facts,
            )
            .unwrap() else {
                panic!("reuse save");
            };
            let result = if outcome == "save-error" {
                Err("actual store failure".into())
            } else {
                state
                    .store_mut()
                    .as_mut()
                    .unwrap()
                    .replace(*replacement)
                    .map_err(|error| error.to_string())
            };
            match outcome {
                "caller" => facts.callers[0].target.tab_id += 1,
                "target" => {
                    let store = state.store_mut().as_mut().unwrap();
                    let mut task = store.get("fixture-task").unwrap().clone();
                    task.objective = "Changed after save".into();
                    store.replace(task).unwrap();
                }
                _ => {}
            }
            let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
            let effect = decide(
                &mut state,
                TaskMutation::WorkerCheckoutReuseSaved { id, result },
                &facts,
            );
            assert_eq!(
                matches!(effect, Ok(TaskMutationAction::StartWorker { .. })),
                outcome == "saved"
            );
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before
            );
            assert!(decide(
                &mut state,
                TaskMutation::WorkerCheckoutReuseSaved { id, result: Ok(()) },
                &facts
            )
            .is_err());
        }
    }

    #[test]
    fn authority_fix_rechecks_caller_session_and_checkout_before_preparation_save() {
        for change in 0..13 {
            let (dir, mut state, mut facts) = fixture();
            let id = bound(&mut state, &facts);
            let prepared = expected(&state);
            match change {
                0 => facts.callers.clear(),
                1 => facts.callers[0].target.session_uid = "other".into(),
                2 => facts.callers[0].target.tab_id += 1, // same persisted UID restored
                3 => facts.callers[0].target.task_session_id = Some("other-session".into()),
                4 => facts.callers[0].target.task_id = Some("other-task".into()),
                5 => facts.callers[0].chat_session_id = Some("other-chat".into()),
                6 => facts.callers[0].checkout = dir.path().join("other"),
                7 => {
                    facts.callers[0].repository.as_mut().unwrap().common_dir =
                        dir.path().join("replacement.git")
                }
                8 => facts.callers[0].home = dir.path().join("other-home"),
                9 => facts.callers[0].repos.clear(),
                10 => facts.callers[0].remote = true,
                12 => facts.callers[0].repository_workdir = Some(dir.path().join("redirected")),
                _ => facts.callers[0].workspace.name = "changed workspace".into(),
            }
            let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
            let error = decide(
                &mut state,
                TaskMutation::WorkerPrepared {
                    id,
                    result: Ok(prepared.clone()),
                },
                &facts,
            )
            .unwrap_err();
            assert!(
                error.contains("caller, session, workspace authority or checkout changed"),
                "{change}: {error}"
            );
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before
            );
            assert!(decide(
                &mut state,
                TaskMutation::WorkerPrepared {
                    id,
                    result: Ok(prepared)
                },
                &facts
            )
            .is_err());
        }
    }

    #[test]
    fn target_and_result_are_checked_before_save_and_completions_are_single_use() {
        for mismatch in 0..4 {
            let (dir, mut state, facts) = fixture();
            let id = bound(&mut state, &facts);
            let mut prepared = expected(&state);
            if mismatch == 0 {
                prepared.path = dir.path().join("wrong");
            } else if mismatch == 1 {
                prepared.branch = "task/wrong".into();
            } else if mismatch == 2 {
                let mut task = state.task("fixture-task").unwrap().clone();
                task.repository.common_dir = dir.path().join("replaced.git");
                state.store.as_mut().unwrap().replace(task).unwrap();
            } else {
                state.store = None;
            }
            assert!(decide(
                &mut state,
                TaskMutation::WorkerPrepared {
                    id,
                    result: Ok(prepared)
                },
                &facts
            )
            .is_err());
            assert!(state.pending_workers.is_empty());
        }
    }

    #[test]
    fn preparation_save_is_a_gate_and_rechecks_authority_again_before_start() {
        for failure in ["none", "save", "caller", "target", "fake-success"] {
            let (dir, mut state, mut facts) = fixture();
            let id = bound(&mut state, &facts);
            let prepared = expected(&state);
            // No active/selected workspace fact is used: focus changes are valid.
            assert!(matches!(
                decide(
                    &mut state,
                    TaskMutation::CheckWorkerPreparation { id },
                    &facts
                )
                .unwrap(),
                TaskMutationAction::WorkerAuthorityChecked { .. }
            ));
            let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
            let TaskMutationAction::PersistWorkerPreparation {
                prepared: to_save, ..
            } = decide(
                &mut state,
                TaskMutation::WorkerPrepared {
                    id,
                    result: Ok(prepared),
                },
                &facts,
            )
            .unwrap()
            else {
                panic!("expected only a store effect")
            };
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before
            );
            let result = if failure == "save" {
                Err("injected save failure".into())
            } else {
                state
                    .store
                    .as_mut()
                    .unwrap()
                    .complete_worktree_preparation("fixture-task", to_save, "t3")
                    .map_err(|error| error.to_string())
            };
            if failure == "caller" {
                facts.callers.clear();
            }
            if failure == "target" || failure == "fake-success" {
                let mut task = state.task("fixture-task").unwrap().clone();
                task.worktree.path = Some(dir.path().join("changed"));
                state.store.as_mut().unwrap().replace(task).unwrap();
            }
            let action = decide(
                &mut state,
                TaskMutation::WorkerPreparationSaved { id, result },
                &facts,
            );
            if failure == "none" {
                assert!(matches!(
                    action.unwrap(),
                    TaskMutationAction::StartWorker { .. }
                ));
            } else {
                assert!(action.is_err(), "{failure}");
            }
            assert!(decide(
                &mut state,
                TaskMutation::WorkerPreparationSaved { id, result: Ok(()) },
                &facts
            )
            .is_err());
        }
    }

    #[test]
    fn preparation_failure_records_failed_and_preserves_existing_rollback_leftovers() {
        for (error, leftover) in [
            ("Git preparation failed", true),
            ("rollback requires attention", false),
            ("rollback requires attention", true),
        ] {
            for fail_save in [false, true] {
                let (dir, mut state, mut facts) = fixture();
                let expected = expected(&state);
                state
                    .store
                    .as_mut()
                    .unwrap()
                    .begin_worktree_preparation("fixture-task", "t3")
                    .unwrap();
                let id = begin(&mut state, &facts);
                decide(
                    &mut state,
                    TaskMutation::BindWorkerPreparation {
                        id,
                        task_id: "fixture-task".into(),
                        expected: expected.clone(),
                    },
                    &facts,
                )
                .unwrap();
                if leftover {
                    // Path::exists also preserves a partial file, not just a directory.
                    std::fs::write(&expected.path, "partial checkout").unwrap();
                    facts.existing_paths.push(expected.path.clone());
                }
                let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
                let TaskMutationAction::PersistWorkerPreparationFailure {
                    task_id,
                    error: detail,
                    cleanup_path,
                } = decide(
                    &mut state,
                    TaskMutation::WorkerPrepared {
                        id,
                        result: Err(error.into()),
                    },
                    &facts,
                )
                .unwrap()
                else {
                    panic!("expected failure save only")
                };
                assert_eq!(detail, error);
                let needs_cleanup = leftover && error.contains("rollback requires attention");
                assert_eq!(cleanup_path.is_some(), needs_cleanup);
                assert_eq!(
                    std::fs::read(dir.path().join("tasks.json")).unwrap(),
                    before
                );
                assert!(state.pending_workers.is_empty());
                if fail_save {
                    std::fs::remove_file(dir.path().join("tasks.json")).unwrap();
                    std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
                }
                let saved = state.store.as_mut().unwrap().fail_worktree_preparation(
                    &task_id,
                    detail,
                    cleanup_path,
                    "t4",
                );
                if fail_save {
                    assert!(saved.is_err());
                    assert_eq!(
                        state.task(&task_id).unwrap().lifecycle,
                        TaskLifecycle::Preparing
                    );
                } else {
                    saved.unwrap();
                    let task = state.task(&task_id).unwrap();
                    assert_eq!(task.lifecycle, TaskLifecycle::Failed);
                    assert_eq!(task.last_error.as_deref(), Some(error));
                    assert_eq!(
                        task.worktree.state,
                        if needs_cleanup {
                            TaskWorktreeState::CleanupRequired
                        } else {
                            TaskWorktreeState::Unprepared
                        }
                    );
                    assert_eq!(
                        task.worktree.path.as_ref(),
                        needs_cleanup.then_some(&expected.path)
                    );
                    assert!(matches!(
                        decide(&mut state, TaskMutation::Close { task_id }, &facts).unwrap(),
                        TaskMutationAction::ArchiveTask { .. }
                    ));
                }
                assert!(decide(
                    &mut state,
                    TaskMutation::WorkerPreparationSaved { id, result: Ok(()) },
                    &facts
                )
                .is_err());
            }
        }
    }

    #[test]
    fn preparation_errors_cannot_write_through_stale_caller_or_target_authority() {
        for stale_caller in [false, true] {
            let (dir, mut state, mut facts) = fixture();
            let expected = expected(&state);
            state
                .store
                .as_mut()
                .unwrap()
                .begin_worktree_preparation("fixture-task", "t3")
                .unwrap();
            let id = begin(&mut state, &facts);
            decide(
                &mut state,
                TaskMutation::BindWorkerPreparation {
                    id,
                    task_id: "fixture-task".into(),
                    expected,
                },
                &facts,
            )
            .unwrap();
            if stale_caller {
                facts.callers.clear();
            } else {
                let mut task = state.task("fixture-task").unwrap().clone();
                task.branch = "task/replaced".into();
                state.store.as_mut().unwrap().replace(task).unwrap();
            }
            let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
            assert!(decide(
                &mut state,
                TaskMutation::WorkerPrepared {
                    id,
                    result: Err("rollback requires attention".into())
                },
                &facts
            )
            .is_err());
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before
            );
            assert!(state.pending_workers.is_empty());
        }
    }

    #[test]
    fn create_uses_the_same_fixture_as_iced_and_preserves_form_differences() {
        let (_, mut state, facts) = fixture();
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/mutation-inputs.json"
        ))
        .unwrap();
        for case in cases {
            let input = TaskCreate {
                title: case["title"].as_str().unwrap().into(),
                objective: case["objective"].as_str().unwrap().into(),
                repository_path: PathBuf::from("/fixture/repo"),
                workspace_name: None,
                issue_key: case["issue_key"].as_str().map(str::to_string),
                base_reference: case["base_reference"].as_str().map(str::to_string),
                stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
                origin: TaskCreateOrigin::Control,
            };
            let result = decide(&mut state, TaskMutation::Create(input.clone()), &facts);
            if let Some(error) = case["error"].as_str() {
                assert_eq!(result.unwrap_err(), error);
            } else {
                let TaskMutationAction::PrepareTask(normalized) = result.unwrap() else {
                    panic!("expected preparation")
                };
                assert_eq!(normalized.title, case["title_out"]);
                assert_eq!(normalized.objective, case["objective_out"]);
                assert_eq!(serde_json::json!(normalized.issue_key), case["issue_out"]);
                assert_eq!(
                    normalized.base_reference.as_deref(),
                    Some(case["base_out"].as_str().unwrap_or(DEFAULT_TASK_BASE))
                );
            }
        }
        let mut form = TaskCreate {
            title: "work".into(),
            objective: "work".into(),
            repository_path: PathBuf::from("/fixture/repo"),
            workspace_name: None,
            issue_key: Some("tru-274".into()),
            base_reference: Some(" ".into()),
            stopping_boundary: StoppingBoundary::PlanOnly,
            origin: TaskCreateOrigin::Form,
        };
        assert_eq!(
            decide(&mut state, TaskMutation::Create(form.clone()), &facts).unwrap_err(),
            "Base reference is required"
        );
        form.base_reference = Some("main".into());
        assert_eq!(
            decide(&mut state, TaskMutation::Create(form), &facts).unwrap_err(),
            "Linear issue must look like TRU-105"
        );
    }

    fn handoff(caller: Option<&str>, session: Option<&str>, status: HandoffStatus) -> TaskMutation {
        TaskMutation::UpdateHandoff(HandoffUpdate {
            task_id: "fixture-task".into(),
            summary: " note ".into(),
            decisions: vec![" ".into(), " decision ".into()],
            next_steps: vec![],
            blockers: vec![],
            session_id: session.map(str::to_string),
            caller: caller.map(str::to_string),
            status,
            timestamp: "t3".into(),
        })
    }

    #[test]
    fn handoff_typed_session_is_attribution_never_worker_authority() {
        let (dir, mut state, mut facts) = fixture();
        let session: TaskSessionRecord = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/session.json"
        ))
        .unwrap();
        let delegation: Delegation = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/delegation.json"
        ))
        .unwrap();
        state
            .store
            .as_mut()
            .unwrap()
            .upsert_session("fixture-task", session, "t3")
            .unwrap();
        state
            .store
            .as_mut()
            .unwrap()
            .insert_delegation(delegation)
            .unwrap();
        let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
        for caller in [None, Some("coordinator")] {
            for status in [HandoffStatus::Done, HandoffStatus::Blocked] {
                assert!(decide(
                    &mut state,
                    handoff(caller, Some("fixture-session"), status),
                    &facts
                )
                .unwrap_err()
                .contains("only the worker's own GitTerm tab"));
            }
            let TaskMutationAction::PersistHandoff { handoff, .. } = decide(
                &mut state,
                handoff(caller, Some("fixture-session"), HandoffStatus::Progress),
                &facts,
            )
            .unwrap() else {
                panic!("expected save")
            };
            assert_eq!(
                handoff.updated_by_session_id.as_deref(),
                Some("fixture-session")
            );
            assert_eq!(handoff.summary, "note");
            assert_eq!(handoff.decisions, vec!["decision"]);
        }
        let target = session::Target {
            tab_id: 9,
            session_uid: "worker".into(),
            task_id: Some("fixture-task".into()),
            task_session_id: Some("fixture-session".into()),
        };
        facts.sessions.push(session::State {
            target,
            control: session::TurnControl::Harness,
            task_live: true,
            last_tab: false,
        });
        assert!(matches!(
            decide(
                &mut state,
                handoff(Some("worker"), None, HandoffStatus::Done),
                &facts
            )
            .unwrap(),
            TaskMutationAction::PersistHandoff {
                hosted_session: Some(_),
                ..
            }
        ));
        assert!(decide(
            &mut state,
            handoff(
                Some("worker"),
                Some("sibling-unknown"),
                HandoffStatus::Progress
            ),
            &facts
        )
        .is_err());
        assert_eq!(
            std::fs::read(dir.path().join("tasks.json")).unwrap(),
            before
        );
        let (_, mut no_worker, _) = fixture();
        assert!(matches!(
            decide(
                &mut no_worker,
                handoff(None, None, HandoffStatus::Done),
                &facts
            )
            .unwrap(),
            TaskMutationAction::PersistHandoff { .. }
        ));
    }

    fn launch(
        preset: Option<&str>,
        origin: TaskLaunchOrigin,
        override_choice: bool,
    ) -> TaskMutation {
        TaskMutation::LaunchSession(SessionLaunch {
            task_id: "fixture-task".into(),
            preset_name: preset.map(str::to_string),
            origin,
            user_model_choice: override_choice,
        })
    }

    #[test]
    fn launch_policy_capacity_queue_and_terminal_rules_are_preserved() {
        let (_, mut state, mut facts) = fixture();
        facts.policy_blocked = true;
        facts.policy_reason = "held".into();
        for (origin, override_choice, queued) in [
            (TaskLaunchOrigin::Agent, false, true),
            (TaskLaunchOrigin::Agent, true, false),
            (TaskLaunchOrigin::User, false, false),
        ] {
            let action = decide(
                &mut state,
                launch(Some(" codex "), origin, override_choice),
                &facts,
            )
            .unwrap();
            assert_eq!(
                matches!(action, TaskMutationAction::QueueSession { .. }),
                queued
            );
        }
        facts.slot_holders.push("other".into());
        facts.queue = vec!["earlier".into(), "fixture-task".into()];
        assert!(matches!(
            decide(
                &mut state,
                launch(Some("Codex"), TaskLaunchOrigin::User, true),
                &facts
            )
            .unwrap(),
            TaskMutationAction::QueueSession { position: 2, .. }
        ));
        assert!(matches!(
            decide(
                &mut state,
                launch(None, TaskLaunchOrigin::Agent, false),
                &facts
            )
            .unwrap(),
            TaskMutationAction::LaunchSession {
                preset_index: None,
                ..
            }
        ));
        facts.slot_holders = vec!["fixture-task".into()];
        assert_eq!(
            decide(
                &mut state,
                launch(Some("Codex"), TaskLaunchOrigin::Agent, false),
                &facts
            )
            .unwrap_err(),
            "held"
        );
        facts.policy_blocked = false;
        assert!(matches!(
            decide(
                &mut state,
                launch(Some("Codex"), TaskLaunchOrigin::Agent, false),
                &facts
            )
            .unwrap(),
            TaskMutationAction::LaunchSession { .. }
        ));
        facts.directories.clear();
        assert!(decide(
            &mut state,
            launch(None, TaskLaunchOrigin::User, false),
            &facts
        )
        .unwrap_err()
        .contains("worktree is not ready"));
        assert!(decide(
            &mut state,
            launch(Some("absent"), TaskLaunchOrigin::User, false),
            &facts
        )
        .unwrap_err()
        .contains("Agent preset"));
    }

    #[test]
    fn publish_delete_and_close_keep_their_distinct_gates() {
        let (_, mut state, mut facts) = fixture();
        let publish = |confirmed| TaskMutation::Publish {
            task_id: "fixture-task".into(),
            action: TaskPublish::OpenDraftPr,
            confirmed,
        };
        assert!(matches!(
            decide(&mut state, publish(false), &facts).unwrap(),
            TaskMutationAction::ConfirmPublish { .. }
        ));
        assert!(matches!(
            decide(&mut state, publish(true), &facts).unwrap(),
            TaskMutationAction::Publish { .. }
        ));
        facts.publish_busy.push("fixture-task".into());
        assert!(matches!(
            decide(&mut state, publish(true), &facts).unwrap(),
            TaskMutationAction::None
        ));
        let path = expected(&state).path;
        let mut inspection = CleanupInspection {
            path_exists: true,
            registered: true,
            dirty: false,
            unpushed_commits: 0,
            upstream: None,
            risks: vec![],
        };
        for risk in [
            CleanupRisk::ActiveProcess,
            CleanupRisk::UncommittedChanges,
            CleanupRisk::UnpushedCommits { count: 1 },
        ] {
            inspection.risks = vec![risk];
            assert!(matches!(
                decide(
                    &mut state,
                    TaskMutation::DeleteWorktree {
                        task_id: "fixture-task".into(),
                        path: path.clone(),
                        inspection: inspection.clone()
                    },
                    &facts
                )
                .unwrap(),
                TaskMutationAction::None
            ));
        }
        inspection.risks = vec![CleanupRisk::ExistingPullRequest {
            url: "https://example.invalid/pr/1".into(),
        }];
        assert!(matches!(
            decide(
                &mut state,
                TaskMutation::DeleteWorktree {
                    task_id: "fixture-task".into(),
                    path: path.clone(),
                    inspection: inspection.clone()
                },
                &facts
            )
            .unwrap(),
            TaskMutationAction::RemoveWorktree { .. }
        ));
        inspection.path_exists = false;
        assert!(matches!(
            decide(
                &mut state,
                TaskMutation::DeleteWorktree {
                    task_id: "fixture-task".into(),
                    path,
                    inspection
                },
                &facts
            )
            .unwrap(),
            TaskMutationAction::ClearWorktree { .. }
        ));
        let target = session::Target {
            tab_id: 4,
            session_uid: "worker".into(),
            task_id: Some("fixture-task".into()),
            task_session_id: Some("session".into()),
        };
        facts.sessions.push(session::State {
            target: target.clone(),
            control: session::TurnControl::Harness,
            task_live: true,
            last_tab: true,
        });
        assert!(matches!(
            decide(
                &mut state,
                TaskMutation::CloseSession {
                    target: target.clone(),
                    confirmed: false
                },
                &facts
            )
            .unwrap(),
            TaskMutationAction::Session(session::Effect::ConfirmClose(_))
        ));
        assert!(matches!(
            decide(
                &mut state,
                TaskMutation::CloseSession {
                    target: target.clone(),
                    confirmed: true
                },
                &facts
            )
            .unwrap(),
            TaskMutationAction::Session(session::Effect::Close(_))
        ));
        let mut stale = target;
        stale.tab_id += 1;
        assert!(matches!(
            decide(
                &mut state,
                TaskMutation::CloseSession {
                    target: stale,
                    confirmed: true
                },
                &facts
            )
            .unwrap(),
            TaskMutationAction::None
        ));
    }

    #[test]
    fn public_branch_validator_keeps_git_rules_separate_from_record_rules() {
        let dir = tempfile::tempdir().unwrap();
        validate_branch(dir.path(), "task/fixture").unwrap();
        for branch in [
            "adopted",
            "task/nested/branch",
            "task/has space",
            "task/a..b",
        ] {
            assert!(validate_branch(dir.path(), branch).is_err(), "{branch}");
        }
    }
}
