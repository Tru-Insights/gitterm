//! H5b B10c: `delegate_task` in fixture mode, in the order of Iced's
//! `delegate_task_requested`, `delegate_task_targets_checked`, the
//! `TaskMetadataResolved` / `TaskWorktreePrepared` arms with a worker,
//! `complete_worker_preparation` and `start_delegated_worker`: the worker is
//! resolved and its authority captured, the request's paths are checked off
//! the UI thread, the task is created through the fixture `task_create`
//! path (the worker rides along on its [`PendingCreate`]), and once the
//! worktree is prepared and saved the worker delegation is stored and its
//! session launched through B10b's launch path.
//!
//! A `checkout` (an existing worktree) follows Iced's
//! `delegate_task_in_checkout`: the task this host created there carries
//! the new objective, or a new task is set up around the worktree (as
//! Adopt does), and the worker launches there.
//!
//! The fixture guard authorizes every target before preparation (the
//! repository, the requested checkout or the calling tab's checkout, and
//! each extra folder), again with the checked paths, and rechecks them
//! after the asynchronous preparation together with the prepared worktree;
//! the launch rechecks the worktree and extra folders right before the
//! child starts.
//!
//! Only delegations this host stored are started, and only tasks it
//! created are reused. The worker's report reaches the requesting chat
//! through the delivery tick ([`super::delivery`]).
use super::*;
use gitterm_core::{
    delegations::{CallerTab, WorkerPlace},
    task_commands::{
        SessionLaunch, TaskLaunchOrigin, WorkerCaller, WorkerCheckout, WorkerPreparationId,
    },
    task_mcp::DelegateTaskRequest,
    task_worktree::{AdoptWorktreeRequest, AdoptedWorktreeResolution},
    tasks::{
        CompletedWorktreePreparation, Delegation, DelegationRunOptions, DelegationStatus,
        WorkspaceIdentity, WorkspaceLocationIdentity,
    },
    workers::WorkerChoice,
};

/// The worker a `delegate_task` call asked for, carried through task
/// creation (Iced's `PendingWorker`).
pub(super) struct PendingWorker {
    authority: WorkerPreparationId,
    caller: CallerTab,
    choice: WorkerChoice,
    /// The request's run options, checked.
    options: DelegationRunOptions,
    /// Where the worker acts: the repository and each extra folder, then
    /// the prepared worktree.
    targets: Vec<PathBuf>,
    authorized: AuthorizedTargets,
}

/// A request on its way through the off-thread target check (Iced's
/// `DelegateTargets`).
struct Targets {
    authority: WorkerPreparationId,
    request: DelegateTaskRequest,
    caller: Option<String>,
    tab: CallerTab,
    choice: WorkerChoice,
    place: WorkerPlace,
    options: DelegationRunOptions,
    authorized: AuthorizedTargets,
    /// How to set a task up around the requested checkout.
    adoption: Option<AdoptedWorktreeResolution>,
}

impl PendingWorker {
    pub(super) fn authority(&self) -> WorkerPreparationId {
        self.authority
    }
}

impl TaskHostOwner {
    /// Iced's `delegate_task_requested`, after the gate authorized the
    /// request's targets.
    pub(super) fn delegate_task(
        &mut self,
        reply: TaskControlReply,
        caller: Option<String>,
        request: DelegateTaskRequest,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let prepared = (|| {
            let uid = caller
                .as_deref()
                .ok_or_else(|| gitterm_core::task_mcp::missing_caller_error(label))?;
            let tab = self.delegation_identity(uid, cx)?.tab;
            if tab.remote {
                return Err(format!(
                    "delegate_task creates a local task worktree from the calling tab's \
                     repository, and the tab's workspace {:?} is remote; remote workers are not \
                     supported yet",
                    tab.workspace
                ));
            }
            let settings = crate::workspace_config::load_iced_settings_from(&self.iced_config);
            let presets: Vec<gitterm_core::workers::PresetRef<'_>> = settings
                .agent_presets
                .iter()
                .map(|preset| gitterm_core::workers::PresetRef {
                    name: &preset.name,
                    command: &preset.command,
                })
                .collect();
            let choice = gitterm_core::workers::resolve_worker(
                &presets,
                request.preset_name.as_deref(),
                request.role,
                request.model.as_deref(),
                &settings.policy,
            )?;
            let (place, options) = gitterm_core::delegations::worker_targets(&(&request).into())?;
            // Refuse options the preset's CLI cannot take before any
            // worktree or task exists.
            let preset_command = settings
                .agent_presets
                .get(choice.preset_index)
                .map(|preset| preset.command.as_str())
                .ok_or_else(|| format!("preset {} is unavailable", choice.preset_name))?;
            gitterm_core::workers::with_launch_options(
                preset_command,
                options.effort,
                &options.add_dirs,
            )
            .map_err(|error| format!("preset {}: {error}", choice.preset_name))?;
            let authorized =
                self.authorize_process(label, &place_targets(&place, &tab, &options))?;
            let facts = self.worker_facts(uid, true, cx)?;
            let TaskMutationAction::WorkerAuthorityChecked { id: authority } = self.reduce(
                TaskMutation::BeginWorkerPreparation {
                    caller: uid.to_string(),
                },
                &facts,
            )?
            else {
                unreachable!("capture worker authority")
            };
            Ok(Targets {
                authority,
                request,
                caller: caller.clone(),
                tab,
                choice,
                place,
                options,
                authorized,
                adoption: None,
            })
        })();
        let targets = match prepared {
            Ok(targets) => targets,
            Err(error) => return reply.send(Err(error)),
        };
        cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
            let (targets, result) = cx
                .background_executor()
                .spawn(async move {
                    let mut targets = targets;
                    let result = check_targets(&mut targets);
                    (targets, result)
                })
                .await;
            let _ = owner.update(cx, |owner, cx| {
                owner.delegate_targets_checked(targets, result, reply, label, cx)
            });
        })
        .detach();
    }

    /// Iced's `delegate_task_targets_checked`, with the guard: the targets
    /// authorized before the check still resolve as they did, and the
    /// checked ones pass it too. Then the task is created as `task_create`
    /// creates it, with the worker on it.
    fn delegate_targets_checked(
        &mut self,
        targets: Targets,
        result: Result<(), String>,
        reply: TaskControlReply,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let authority = targets.authority;
        if !self.continues("delegate checked") {
            self.cancel_worker_preparation(authority);
            return reply.send(Err(SHUT_DOWN.to_string()));
        }
        let checked = self
            .worker_facts(&targets.tab.session_uid, false, cx)
            .and_then(|facts| {
                self.reduce(
                    TaskMutation::CheckWorkerPreparation { id: authority },
                    &facts,
                )
            });
        if let Err(error) = checked {
            return reply.send(Err(error));
        }
        let pending = (|| {
            result?;
            self.host
                .services()
                .recheck(&targets.authorized)
                .map_err(|denied| denied.notice().to_string())?;
            let worker_targets = place_targets(&targets.place, &targets.tab, &targets.options);
            let authorized = self.authorize_process(label, &worker_targets)?;
            let repository = match &targets.place {
                WorkerPlace::Repository(repo) => repo.clone(),
                WorkerPlace::Checkout(checkout) => {
                    return Ok(Checked::Checkout {
                        checkout: checkout.clone(),
                        worker_targets,
                        authorized,
                    });
                }
                WorkerPlace::TabRepository => targets.tab.cwd.clone(),
            };
            let request = targets.request.clone();
            let create = CreateTaskRequest {
                title: request.title,
                objective: request.objective,
                repository_path: repository,
                workspace_name: request.workspace_name,
                issue_key: request.issue_key,
                base_reference: request.base_reference,
                stopping_boundary: request.stopping_boundary,
            };
            let mut pending =
                self.begin_create(reply.clone(), targets.caller.clone(), create, label)?;
            // Check the delegation's own rules before any worktree exists.
            gitterm_core::delegations::worker_delegation(
                &targets.tab,
                &pending.task_id,
                &pending.title,
                &pending.objective,
                &targets.choice,
                &targets.options,
            )?;
            pending.worker = Some(PendingWorker {
                authority,
                caller: targets.tab.clone(),
                choice: targets.choice.clone(),
                options: targets.options.clone(),
                targets: worker_targets,
                authorized,
            });
            Ok(Checked::Create(Box::new(pending)))
        })();
        match pending {
            Ok(Checked::Create(pending)) => self.resolve_create(*pending, cx),
            Ok(Checked::Checkout {
                checkout,
                worker_targets,
                authorized,
            }) => self.delegate_in_checkout(
                targets,
                checkout,
                worker_targets,
                authorized,
                reply,
                label,
                cx,
            ),
            Err(error) => {
                self.cancel_worker_preparation(authority);
                reply.send(Err(error));
            }
        }
    }

    /// Iced's `delegate_task_in_checkout`: the task that owns the checkout
    /// carries the new objective, or a new task is set up around it (as
    /// Adopt does), and the worker launches there. Nothing is written when
    /// the request is refused: an archived task, one with a worker or a
    /// live session still in it, or (fixture only) a task this host did
    /// not create. The checkout passes the guard before each write, and
    /// the launch rechecks it right before the child starts.
    #[allow(clippy::too_many_arguments)]
    fn delegate_in_checkout(
        &mut self,
        targets: Targets,
        checkout: PathBuf,
        worker_targets: Vec<PathBuf>,
        authorized: AuthorizedTargets,
        reply: TaskControlReply,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        use gitterm_core::tasks::{
            ExecutorTarget, IssueProvider, IssueReference, NewTaskRecord, TaskCreator,
            TaskCreatorKind, TaskWorktree, TaskWorktreeState,
        };
        let Targets {
            authority,
            request,
            caller,
            tab,
            choice,
            options,
            adoption,
            ..
        } = targets;
        let uid = tab.session_uid.clone();
        let prepared = (|| {
            let facts = self.worker_facts(&uid, false, cx)?;
            self.reduce(
                TaskMutation::CheckWorkerPreparation { id: authority },
                &facts,
            )?;
            let title = request.title.trim().to_string();
            let objective = request.objective.trim().to_string();
            let issue_key = request
                .issue_key
                .as_deref()
                .map(|key| key.trim().to_uppercase())
                .filter(|key| !key.is_empty());
            if issue_key
                .as_deref()
                .is_some_and(|key| !gitterm_core::tasks::is_linear_issue_key(key))
            {
                return Err("Linear issue must look like TRU-106".to_string());
            }
            let stopping_boundary =
                gitterm_core::task_host::stopping_boundary_from_control(request.stopping_boundary);
            if let Some(resolution) = &adoption {
                let facts = self.checkout_facts(&uid, Some(&checkout), label, cx)?;
                self.reduce(
                    TaskMutation::CheckWorkerDestination {
                        expected: CompletedWorktreePreparation {
                            repository: resolution.repository.identity.clone(),
                            base: resolution.base.clone(),
                            branch: resolution.branch.clone(),
                            path: checkout.clone(),
                        },
                    },
                    &facts,
                )?;
            }
            let owner = self.worktree_task(&checkout).cloned();
            let task_id = match &owner {
                Some(task) => task.task_id.clone(),
                None => TaskRecord::fresh_id(),
            };
            gitterm_core::delegations::worker_delegation(
                &tab, &task_id, &title, &objective, &choice, &options,
            )?;
            let timestamp = chrono::Utc::now().to_rfc3339();
            let issue = issue_key.map(|key| IssueReference {
                provider: IssueProvider::Linear,
                key,
                url: None,
            });
            match owner {
                Some(mut task) => {
                    // An imported task stays as Iced wrote it.
                    if !self.own_tasks.contains(&task_id) {
                        return Err(preview_notice(label));
                    }
                    let written = self.authorize_write(label, std::slice::from_ref(&checkout))?;
                    let facts = self.checkout_facts(&uid, Some(&checkout), label, cx)?;
                    self.reduce(
                        TaskMutation::CheckWorkerCheckout {
                            task_id: task_id.clone(),
                            path: checkout.clone(),
                        },
                        &facts,
                    )?;
                    // The task now carries this work; its last handoff
                    // stays as the worker's context.
                    task.title = title;
                    task.objective = objective;
                    if issue.is_some() {
                        task.issue = issue;
                    }
                    task.stopping_boundary = stopping_boundary;
                    task.updated_at = timestamp;
                    let TaskMutationAction::PersistWorkerCheckoutReuse { replacement, .. } = self
                        .reduce(
                        TaskMutation::ReuseWorkerCheckout {
                            id: authority,
                            replacement: Box::new(task),
                        },
                        &facts,
                    )?
                    else {
                        unreachable!("reuse effect")
                    };
                    let saved = self
                        .host
                        .services()
                        .recheck(&written)
                        .map_err(|denied| denied.notice().to_string())
                        .and_then(|()| self.writable_store())
                        .and_then(|store| {
                            store
                                .replace(*replacement)
                                .map_err(|error| error.to_string())
                        });
                    let facts = self.saved_facts(&uid, authority, label, cx)?;
                    self.reduce(
                        TaskMutation::WorkerCheckoutReuseSaved {
                            id: authority,
                            result: saved,
                        },
                        &facts,
                    )
                }
                None => {
                    let resolution = adoption.ok_or_else(|| {
                        format!(
                            "checkout {} was not resolved as a worktree; try again",
                            checkout.display()
                        )
                    })?;
                    let top_level = resolution.repository.top_level.clone();
                    let written = self.authorize_write(label, &[checkout.clone(), top_level])?;
                    let workspace_name = request
                        .workspace_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            gitterm_core::checkout_name::repo_name(&resolution.repository.top_level)
                        });
                    let mut record = TaskRecord::new_draft(
                        NewTaskRecord {
                            task_id: task_id.clone(),
                            title,
                            objective,
                            workspace: WorkspaceIdentity {
                                name: workspace_name,
                                location: WorkspaceLocationIdentity::Local {
                                    directory: resolution.repository.top_level.clone(),
                                },
                            },
                            repository: resolution.repository.identity.clone(),
                            issue,
                            base: resolution.base.clone(),
                            branch: resolution.branch.clone(),
                            executor: ExecutorTarget::Local,
                            harness: None,
                            stopping_boundary,
                        },
                        timestamp.clone(),
                    );
                    record.created_by = Some(TaskCreator {
                        kind: TaskCreatorKind::Coordinator,
                        session_id: caller.clone(),
                        harness_label: None,
                        created_at: timestamp.clone(),
                    });
                    record.lifecycle = TaskLifecycle::Preparing;
                    record.worktree = TaskWorktree {
                        state: TaskWorktreeState::Preparing,
                        path: None,
                    };
                    let TaskMutationAction::PersistPreparing { record } = self.reduce(
                        TaskMutation::InsertPreparing {
                            record: Box::new(record),
                        },
                        &Default::default(),
                    )?
                    else {
                        unreachable!("metadata effect")
                    };
                    self.host
                        .services()
                        .recheck(&written)
                        .map_err(|denied| denied.notice().to_string())?;
                    self.writable_store()?
                        .insert(*record)
                        .map_err(|error| error.to_string())?;
                    self.own_tasks.insert(task_id.clone());
                    let expected = CompletedWorktreePreparation {
                        repository: resolution.repository.identity.clone(),
                        base: resolution.base.clone(),
                        branch: resolution.branch.clone(),
                        path: checkout.clone(),
                    };
                    let facts = self.worker_facts(&uid, false, cx)?;
                    self.reduce(
                        TaskMutation::BindWorkerPreparation {
                            id: authority,
                            task_id: task_id.clone(),
                            expected: expected.clone(),
                        },
                        &facts,
                    )?;
                    let facts = self.checkout_facts(&uid, Some(&checkout), label, cx)?;
                    let TaskMutationAction::PersistWorkerPreparation { prepared, .. } = self
                        .reduce(
                            TaskMutation::WorkerPrepared {
                                id: authority,
                                result: Ok(expected),
                            },
                            &facts,
                        )?
                    else {
                        unreachable!("adoption effect")
                    };
                    let saved = self
                        .host
                        .services()
                        .recheck(&written)
                        .map_err(|denied| denied.notice().to_string())
                        .and_then(|()| self.writable_store())
                        .and_then(|store| {
                            store
                                .complete_worktree_preparation(&task_id, prepared, &timestamp)
                                .map_err(|error| error.to_string())
                        });
                    let facts = self.saved_facts(&uid, authority, label, cx)?;
                    self.reduce(
                        TaskMutation::WorkerPreparationSaved {
                            id: authority,
                            result: saved,
                        },
                        &facts,
                    )
                }
            }
        })();
        cx.notify();
        match prepared {
            Ok(TaskMutationAction::StartWorker { task_id, .. }) => {
                let worker = PendingWorker {
                    authority,
                    caller: tab,
                    choice,
                    options,
                    targets: worker_targets,
                    authorized,
                };
                self.start_delegated_worker(&task_id, worker, reply, label, cx)
            }
            Ok(_) => unreachable!("worker start effect"),
            Err(error) => {
                self.cancel_worker_preparation(authority);
                reply.send(Err(error));
            }
        }
    }

    /// Iced's `worktree_task`: the task whose worktree is `path`.
    fn worktree_task(&self, path: &Path) -> Option<&TaskRecord> {
        self.host.store().as_ref()?.tasks().iter().find(|task| {
            task.worktree
                .path
                .as_deref()
                .is_some_and(|worktree| gitterm_core::chat::checkout::paths_equal(worktree, path))
        })
    }

    /// Iced's `mutation_facts` for a worker mutation naming a destination
    /// checkout: the worker facts, with that destination as a preparation
    /// path and, when Git resolves it below the fixture, its checkout.
    fn checkout_facts(
        &self,
        uid: &str,
        destination: Option<&Path>,
        label: &'static str,
        cx: &App,
    ) -> Result<TaskMutationFacts, String> {
        let mut facts = self.worker_facts(uid, false, cx)?;
        if let Some(path) = destination {
            if let Ok(normalized) = normalized_worker_path(path) {
                facts.preparation_paths.push(normalized);
            }
            if let Ok(checkout) = self
                .ceiling_for(path, label)
                .and_then(|ceiling| worker_checkout_fact(path, &ceiling))
            {
                facts.checkouts.push(checkout);
            }
        }
        Ok(facts)
    }

    /// The facts a worker save is reduced with: a save rechecks only its own
    /// task's checkout (Iced's `mutation_facts` for the saved arms).
    fn saved_facts(
        &self,
        uid: &str,
        id: WorkerPreparationId,
        label: &'static str,
        cx: &App,
    ) -> Result<TaskMutationFacts, String> {
        let mut facts = self.worker_facts(uid, false, cx)?;
        if let Some((_, Some(path))) = self.host.pending_worker_subject(id) {
            let path = path.to_path_buf();
            facts.checkouts = self
                .ceiling_for(&path, label)
                .and_then(|ceiling| worker_checkout_fact(&path, &ceiling))
                .ok()
                .into_iter()
                .collect();
        }
        Ok(facts)
    }

    /// The `TaskMetadataResolved` arm's worker steps, after the preparing
    /// record is saved: the worker's authority again, then bound to the
    /// task and its proposed worktree.
    pub(super) fn bind_worker(
        &mut self,
        worker: &PendingWorker,
        task_id: &str,
        resolved: &ResolvedTaskPreparation,
        cx: &App,
    ) -> Result<(), String> {
        let facts = self.worker_facts(&worker.caller.session_uid, false, cx)?;
        let expected = CompletedWorktreePreparation {
            repository: resolved.repository.identity.clone(),
            base: resolved.base.clone(),
            branch: resolved.proposal.branch.clone(),
            path: normalized_worker_path(&resolved.proposal.path)?,
        };
        self.reduce(
            TaskMutation::BindWorkerPreparation {
                id: worker.authority,
                task_id: task_id.to_string(),
                expected,
            },
            &facts,
        )
        .map(drop)
    }

    /// Before the worktree is prepared: whether the worker's authority still
    /// holds (the `TaskMetadataResolved` arm's first check).
    pub(super) fn check_worker(&mut self, worker: &PendingWorker, cx: &App) -> Result<(), String> {
        let facts = self.worker_facts(&worker.caller.session_uid, false, cx)?;
        self.reduce(
            TaskMutation::CheckWorkerPreparation {
                id: worker.authority,
            },
            &facts,
        )
        .map(drop)
    }

    /// Iced's `complete_worker_preparation`, after the create's own recheck:
    /// the worker's targets recheck and the prepared worktree passes the
    /// guard, the preparation is saved, and the worker starts.
    pub(super) fn worker_prepared(
        &mut self,
        pending: PendingCreate,
        mut worker: PendingWorker,
        prepared: Result<PreparedTaskWorktree, String>,
        cx: &mut Context<Self>,
    ) {
        let reply = pending.reply.clone();
        if !self.continues("worker prepared") {
            self.cancel_worker_preparation(worker.authority);
            return reply.send(Err(SHUT_DOWN.to_string()));
        }
        let label = pending.label;
        let prepared = prepared.and_then(|prepared| {
            self.host
                .services()
                .recheck(&worker.authorized)
                .map_err(|denied| denied.notice().to_string())?;
            worker.targets.push(prepared.path.clone());
            worker.authorized = self.authorize_process(label, &worker.targets)?;
            Ok(prepared)
        });
        let id = worker.authority;
        let uid = worker.caller.session_uid.clone();
        let mutation = TaskMutation::WorkerPrepared {
            id,
            result: prepared.map(Into::into),
        };
        let result = (|| {
            let mut facts = self.worker_facts(&uid, false, cx)?;
            facts.existing_paths = pending
                .worktree
                .iter()
                .filter(|path| path.exists())
                .filter_map(|path| std::fs::canonicalize(path).ok())
                .collect();
            if let Some(path) = &pending.worktree
                && let Ok(path) = normalized_worker_path(path)
            {
                facts.preparation_paths.push(path);
            }
            if let TaskMutation::WorkerPrepared {
                result: Ok(prepared),
                ..
            } = &mutation
                && let Ok(checkout) = self
                    .ceiling_for(&prepared.path, label)
                    .and_then(|ceiling| worker_checkout_fact(&prepared.path, &ceiling))
            {
                facts.checkouts.push(checkout);
            }
            let now = chrono::Utc::now().to_rfc3339();
            match self.reduce(mutation, &facts)? {
                TaskMutationAction::PersistWorkerPreparation {
                    task_id, prepared, ..
                } => {
                    let saved = self.writable_store().and_then(|store| {
                        store
                            .complete_worktree_preparation(&task_id, prepared, &now)
                            .map_err(|error| error.to_string())
                    });
                    let mut facts = self.worker_facts(&uid, false, cx)?;
                    // A save rechecks only its own task's checkout.
                    if let Some((_, Some(path))) = self.host.pending_worker_subject(id) {
                        let path = path.to_path_buf();
                        facts.checkouts = self
                            .ceiling_for(&path, label)
                            .and_then(|ceiling| worker_checkout_fact(&path, &ceiling))
                            .ok()
                            .into_iter()
                            .collect();
                    }
                    self.reduce(
                        TaskMutation::WorkerPreparationSaved { id, result: saved },
                        &facts,
                    )
                }
                TaskMutationAction::PersistWorkerPreparationFailure {
                    task_id,
                    error,
                    cleanup_path,
                } => {
                    let saved = self.writable_store().and_then(|store| {
                        store
                            .fail_worktree_preparation(&task_id, error.clone(), cleanup_path, &now)
                            .map_err(|error| error.to_string())
                    });
                    if let Err(store_error) = saved {
                        eprintln!("GitTerm GPUI could not save the failed task: {store_error}");
                    }
                    Err(error)
                }
                _ => unreachable!("worker completion effect"),
            }
        })();
        cx.notify();
        match result {
            Ok(TaskMutationAction::StartWorker { task_id, .. }) => {
                self.start_delegated_worker(&task_id, worker, reply, label, cx)
            }
            Ok(_) => unreachable!("worker start effect"),
            Err(error) => {
                self.cancel_worker_preparation(id);
                reply.send(Err(error));
            }
        }
    }

    /// Iced's `start_delegated_worker`: the worker delegation is stored as
    /// this host's own, then its session launches through the fixture
    /// launch path, and `delegate_task` is answered. The card and parent
    /// attention are UI and are not run.
    fn start_delegated_worker(
        &mut self,
        task_id: &str,
        worker: PendingWorker,
        reply: TaskControlReply,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self
            .host
            .store()
            .as_ref()
            .and_then(|store| store.get(task_id))
            .cloned()
        else {
            return reply.send(Err(format!(
                "Task {task_id} disappeared after worktree preparation"
            )));
        };
        let new = match gitterm_core::delegations::worker_delegation(
            &worker.caller,
            task_id,
            &task.title,
            &task.objective,
            &worker.choice,
            &worker.options,
        ) {
            Ok(new) => new,
            Err(error) => return reply.send(Err(error)),
        };
        let delegation = Delegation::new_requested(new, chrono::Utc::now().to_rfc3339());
        let delegation_id = delegation.delegation_id.clone();
        if let Err(error) = self.writable_store().and_then(|store| {
            store
                .insert_delegation(delegation)
                .map_err(|error| error.to_string())
        }) {
            return reply.send(Err(format!(
                "Created task {task_id}, but could not record the worker delegation: {error}"
            )));
        }
        self.own_delegations
            .insert(delegation_id.clone(), worker.authorized.clone());
        let settings = crate::workspace_config::load_iced_settings_from(&self.iced_config);
        let facts = self.launch_facts(&settings, cx);
        let launch = SessionLaunch {
            task_id: task_id.to_string(),
            preset_name: Some(worker.choice.preset_name.clone()),
            origin: TaskLaunchOrigin::Agent,
            user_model_choice: false,
        };
        let launched = self.try_launch(
            &launch,
            Some(worker.choice.preset_index),
            &settings,
            facts,
            label,
            cx,
        );
        let (task_session_id, queue_position) = match launched {
            Ok(launch::Started::Session(session)) => {
                (session["session_id"].as_str().map(str::to_string), None)
            }
            Ok(launch::Started::Queued(position)) => (None, Some(position)),
            Err(error) => {
                let message = format!("the worker could not start: {error}");
                eprintln!("[delegation] {delegation_id}: {message}");
                if let Err(store_error) = self.writable_store().and_then(|store| {
                    store
                        .set_delegation_status(
                            &delegation_id,
                            DelegationStatus::Failed {
                                message: message.clone(),
                            },
                            &chrono::Utc::now().to_rfc3339(),
                        )
                        .map_err(|error| error.to_string())
                }) {
                    eprintln!("[delegation] {delegation_id}: {store_error}");
                }
                cx.notify();
                return reply.send(Err(format!(
                    "Created task {task_id} at {}, but {message}",
                    task.worktree
                        .path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                )));
            }
        };
        let queued = queue_position.is_some();
        let routing = self.policy().report.routing;
        cx.notify();
        reply.send(Ok(serde_json::json!({
            "delegation_id": delegation_id,
            "task_id": task_id,
            "routing": routing,
            "provider": worker.choice.provider.label(),
            "cost_note": worker.options.user_model_choice.then_some(runs::COST_NOTE),
            "task_session_id": task_session_id,
            "worktree_path": task.worktree.path,
            "branch": task.branch,
            "preset_name": worker.choice.preset_name,
            "model": worker.choice.model,
            "role": worker.choice.role.label(),
            "queued": queued,
            "queue_position": queue_position,
            "detail": if queued {
                "The worker is queued until plan limits and concurrency permit it. Do not wait or poll: its report appears as a card in the requesting chat tab, and delegation_get reads it."
            } else {
                "The worker is running in a background tab. Do not wait or poll: its report appears as a card in the requesting chat tab, and delegation_get reads it."
            },
        })));
    }

    pub(super) fn cancel_worker_preparation(&mut self, id: WorkerPreparationId) {
        let _ = self.reduce(
            TaskMutation::CancelWorkerPreparation { id },
            &Default::default(),
        );
    }

    /// The facts a worker mutation is reduced with: the open panes, and the
    /// calling pane's worker authority. `required` surfaces why the caller
    /// cannot be resolved (Iced checks it before capturing authority);
    /// otherwise an unresolved caller is absent, which the reducer refuses.
    fn worker_facts(
        &self,
        uid: &str,
        required: bool,
        cx: &App,
    ) -> Result<TaskMutationFacts, String> {
        let callers = match self.worker_caller(uid, cx) {
            Ok(caller) => vec![caller],
            Err(error) if required => return Err(error),
            Err(_) => Vec::new(),
        };
        Ok(TaskMutationFacts {
            sessions: open_chats(cx)
                .iter()
                .map(OpenChat::session_state)
                .chain(self.terminal_session_states())
                .collect(),
            working_sessions: self.terminal_working_sessions(),
            callers,
            ..Default::default()
        })
    }

    /// Iced's `worker_authority_caller` for a GPUI chat pane.
    fn worker_caller(&self, uid: &str, cx: &App) -> Result<WorkerCaller, String> {
        let caller = self.delegation_identity(uid, cx)?;
        let tab = &caller.tab;
        if tab.remote {
            return Err("Remote workers are not supported yet".into());
        }
        let canonical = |path: &Path| {
            std::fs::canonicalize(path)
                .map_err(|error| format!("resolve worker authority {}: {error}", path.display()))
        };
        let checkout = canonical(&tab.cwd)?;
        // Discovery stops below the caller's fixture, so it cannot leave it.
        let ceiling = self.ceiling_for(&checkout, "delegate_task")?;
        // A non-repository workspace home can delegate to listed repos.
        let repository = worker_repository_identity(&checkout)?;
        let repository_workdir = if repository.is_some() {
            Some(canonical(&runs::work_tree(&checkout, &ceiling)?)?)
        } else {
            None
        };
        let home = canonical(&tab.home)?;
        Ok(WorkerCaller {
            target: caller.target,
            chat_session_id: tab.chat_session_id.clone(),
            workspace: WorkspaceIdentity {
                name: tab.workspace.clone(),
                location: WorkspaceLocationIdentity::Local {
                    directory: home.clone(),
                },
            },
            repository,
            repository_workdir,
            checkout,
            home,
            repos: tab
                .repos
                .iter()
                .map(|path| canonical(path))
                .collect::<Result<_, _>>()?,
            remote: false,
        })
    }
}

/// A checked request's next step: a new task worktree, or the requested
/// existing checkout.
enum Checked {
    Create(Box<PendingCreate>),
    Checkout {
        checkout: PathBuf,
        worker_targets: Vec<PathBuf>,
        authorized: AuthorizedTargets,
    },
}

/// Where a worker acts before its worktree exists: the repository its
/// worktree comes from (the calling tab's, unless it names one) and each
/// extra folder.
fn place_targets(
    place: &WorkerPlace,
    tab: &CallerTab,
    options: &DelegationRunOptions,
) -> Vec<PathBuf> {
    let repository = match place {
        WorkerPlace::Repository(repo) | WorkerPlace::Checkout(repo) => repo,
        WorkerPlace::TabRepository => &tab.cwd,
    };
    std::iter::once(repository)
        .chain(&options.add_dirs)
        .cloned()
        .collect()
}

/// Iced's `check_delegate_targets`: the extra folders and the named
/// repository or checkout checked against the caller's workspace, a
/// repository folded to its main checkout, and for a checkout how a task
/// is set up around it. Blocking.
fn check_targets(targets: &mut Targets) -> Result<(), String> {
    gitterm_core::delegations::resolve_targets(&mut targets.options, &targets.tab)?;
    match &targets.place {
        WorkerPlace::TabRepository => {}
        WorkerPlace::Repository(repo) => {
            let top = gitterm_core::delegations::resolve_checkout("repo", repo, &targets.tab)?;
            targets.place = WorkerPlace::Repository(gitterm_core::workers::main_checkout(&top));
        }
        WorkerPlace::Checkout(_) => {
            let checkout = targets.options.checkout.clone().ok_or_else(|| {
                "delegate_task lost its checkout during the target check".to_string()
            })?;
            let request = AdoptWorktreeRequest {
                repository_path: gitterm_core::workers::main_checkout(&checkout),
                worktree_path: checkout.clone(),
            };
            let resolution = gitterm_core::task_worktree::resolve_worktree_adoption_blocking(
                &request,
            )
            .map_err(|error| {
                format!(
                    "checkout {} cannot carry a worker: {error}",
                    checkout.display()
                )
            })?;
            targets.adoption = Some(resolution);
            targets.place = WorkerPlace::Checkout(checkout);
        }
    }
    Ok(())
}

/// Iced's `normalized_worker_path`: canonical, with the missing tail kept.
fn normalized_worker_path(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let resolve = || format!("resolve worker destination {}: {error}", path.display());
                missing.push(ancestor.file_name().ok_or_else(resolve)?.to_os_string());
                ancestor = ancestor.parent().ok_or_else(resolve)?;
            }
            Err(error) => {
                return Err(format!(
                    "resolve worker destination {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

/// Iced's `worker_repository_identity`: none outside Git, else the strict
/// main repository's identity.
fn worker_repository_identity(
    checkout: &Path,
) -> Result<Option<gitterm_core::tasks::RepositoryIdentity>, String> {
    if gitterm_core::workers::checkout_root(checkout).is_none() {
        return Ok(None);
    }
    gitterm_core::workers::trust_main_repository(checkout)?;
    gitterm_core::delegations::repository_identity(checkout).map(Some)
}

/// Iced's `worker_checkout_fact`: `path` is Git's current checkout root,
/// with its repository identity. Discovery stops below `ceiling` (Iced
/// discovers with git2).
fn worker_checkout_fact(path: &Path, ceiling: &Path) -> Result<WorkerCheckout, String> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("resolve worker destination {}: {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!(
            "worker destination {} is not a directory",
            path.display()
        ));
    }
    // core.worktree can redirect Git while leaving the old .git entry intact.
    let root = runs::work_tree(&path, ceiling)?;
    if std::fs::canonicalize(&root)
        .map_err(|error| format!("resolve worker checkout root {}: {error}", path.display()))?
        != path
    {
        return Err(format!(
            "worker destination {} is not Git's current checkout root",
            path.display()
        ));
    }
    let repository = worker_repository_identity(&path)?.ok_or_else(|| {
        format!(
            "worker destination {} is not a Git checkout",
            path.display()
        )
    })?;
    Ok(WorkerCheckout { path, repository })
}
