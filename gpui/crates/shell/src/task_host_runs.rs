//! H5b B10: `review_request` and `consult_request` in fixture mode. The
//! order is Iced's (`request_delegation`, `delegation_checked`,
//! `start_queued_delegations`, `delegation_start_checked`,
//! `start_delegation_run`, `finish_delegation`), with the fixture guard
//! around it: every target is authorized before the checkout check, again
//! after it (with the resolved checkout, its work tree and Git common dir),
//! and again after a queued start's check. Codex runs with a cleared
//! environment whose homes lie under the preview root.
//!
//! The full fixture guard runs once more on Codex's cwd and extra folders
//! after the runner's git preflight, immediately before the spawn, with Git
//! itself (in Codex's environment) naming each one's work tree, Git dir and
//! common dir inside the fixture; that preflight runs no fsmonitor hook.
//! Every continuation stops at shutdown, which kills each run's Codex
//! process group at once and reaps it on a background thread.
//!
//! Only delegations this host stored are started or settled; an imported
//! record is never touched. Results are stored; the delivery tick
//! ([`super::delivery`]) sends them into the requesting chat.
use super::*;
use gitterm_core::{
    codex_runner::{BeforeSpawn, CodexEnvironment, CodexRunEvent, CodexRunOutcome, CodexStop},
    delegation_state::{Authority, Caller, Effect, Intent, RequestId},
    delegations::{CallerTab, RunFailure},
    task_host::RunnerCapacity,
    task_mcp::{ConsultDelegationRequest, ReviewDelegationRequest},
    tasks::{Delegation, DelegationStatus, NewDelegation},
};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
};

/// The provider homes a fixture child gets, under the preview root.
pub(crate) const FIXTURE_HOME: &str = "provider-homes/home";
pub(super) const COST_NOTE: &str = "Explicit user model choice overrides plan limits; this may consume remaining allowance or credits.";
/// The label a queued start's refusal carries.
const START_LABEL: &str = "Start delegation";

pub(crate) enum CodexRequest {
    Review(ReviewDelegationRequest),
    Consult(ConsultDelegationRequest),
}

type Outcome = Result<Box<CodexRunOutcome>, RunFailure>;

/// A fixture run in flight.
pub(super) struct RunHandle {
    pub stop: CodexStop,
    /// Closes when the run's task ends, after Codex has been reaped.
    pub done: tokio::sync::oneshot::Receiver<()>,
    pub abort: tokio::task::AbortHandle,
}

impl TaskHostOwner {
    /// Iced's `request_delegation`, after the fixture guard has authorized
    /// the request's checkout and extra folders.
    pub(super) fn request_codex_run(
        &mut self,
        reply: TaskControlReply,
        caller: Option<String>,
        request: CodexRequest,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let prepared = (|| {
            let uid = caller
                .as_deref()
                .ok_or_else(|| gitterm_core::task_mcp::missing_caller_error(label))?;
            let identity = self.delegation_identity(uid, cx)?;
            let model = crate::workspace_config::load_iced_settings_from(&self.iced_config)
                .review
                .codex_model;
            let new = match request {
                CodexRequest::Review(request) => gitterm_core::delegations::review_delegation(
                    &(&request).into(),
                    &identity.tab,
                    model.as_deref(),
                    None,
                ),
                CodexRequest::Consult(request) => gitterm_core::delegations::consult_delegation(
                    &(&request).into(),
                    &identity.tab,
                    model.as_deref(),
                    None,
                ),
            }?;
            let authorized = self.authorize_process(label, &new_targets(&new))?;
            let ceiling = self.ceiling_for(run_target(&new), label)?;
            if self.host.store().is_none() {
                return Err(STORE_UNAVAILABLE.to_string());
            }
            Ok((new, identity, authorized, ceiling))
        })();
        let (new, identity, authorized, ceiling) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return reply.send(Err(error)),
        };
        let effect = self
            .host
            .delegations_mut()
            .reduce(Intent::Request {
                new: Box::new(new),
                caller: identity,
            })
            .pop();
        let (request, new, caller) = match effect {
            Some(Effect::Check {
                request,
                new,
                caller,
            }) => (request, *new, caller),
            Some(Effect::Rejected(error)) => return reply.send(Err(error)),
            _ => unreachable!("request effect"),
        };
        cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
            let (new, result) = cx
                .background_executor()
                .spawn(async move {
                    let mut new = new;
                    let result = check_delegation_run(&mut new, &caller, &ceiling);
                    (new, result)
                })
                .await;
            let _ = owner.update(cx, |owner, cx| {
                owner.codex_run_checked(request, new, authorized, result, reply, label, cx)
            });
        })
        .detach();
    }

    /// Iced's `delegation_checked`: the guard again, then store and admit.
    #[allow(clippy::too_many_arguments)]
    fn codex_run_checked(
        &mut self,
        request: RequestId,
        new: NewDelegation,
        authorized: AuthorizedTargets,
        result: Result<Authority, String>,
        reply: TaskControlReply,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        if !self.continues("checked") {
            return reply.send(Err(SHUT_DOWN.to_string()));
        }
        let result = result.and_then(|authority| {
            self.recheck_run(label, &authorized, &new_targets(&new), &authority)?;
            Ok(authority)
        });
        let caller = self.delegation_identity(&new.parent.session_uid, cx).ok();
        let effect = self
            .host
            .delegations_mut()
            .reduce(Intent::Checked {
                request,
                new: Box::new(new),
                caller: caller.as_ref(),
                result,
            })
            .pop();
        let (new, caller, authority) = match effect {
            Some(Effect::Insert {
                new,
                caller,
                authority,
            }) => (*new, *caller, *authority),
            Some(Effect::Rejected(error)) => return reply.send(Err(error)),
            None => return,
            _ => unreachable!("checked effect"),
        };
        let delegation = Delegation::new_requested(new, chrono::Utc::now().to_rfc3339());
        let id = delegation.delegation_id.clone();
        let user_model_choice = delegation.options.user_model_choice;
        if let Err(error) = self.writable_store().and_then(|store| {
            store
                .insert_delegation(delegation)
                .map_err(|error| error.to_string())
        }) {
            return reply.send(Err(error));
        }
        self.own_delegations.insert(id.clone(), authorized);
        self.host.delegations_mut().reduce(Intent::Stored {
            id: id.clone(),
            caller,
            authority,
        });
        self.start_queued_runs(cx);
        let queued = !self.host.delegations().admitted(&id);
        let routing = self.policy().report.routing;
        reply.send(Ok(serde_json::json!({
            "delegation_id": id,
            "status": "requested",
            "routing": routing,
            "provider": "codex",
            "cost_note": user_model_choice.then_some(COST_NOTE),
            "queued": queued,
            "detail": if queued {
                "Queued until plan limits and concurrency permit a Codex run. Do not wait or poll: the result appears as a card in the requesting chat tab, and delegation_get reads it."
            } else {
                "Codex is working in the background. Do not wait or poll: the result appears as a card in the requesting chat tab, and delegation_get reads it."
            },
        })));
        cx.notify();
    }

    /// Iced's `start_queued_delegations`. Admission authorizes each
    /// delegation's own targets (`TaskHost::admit_delegations`).
    pub(crate) fn start_queued_runs(&mut self, cx: &mut Context<Self>) {
        let running = self.running_runs();
        let policy = self.policy().evaluation;
        let max_concurrent = self.max_concurrent_runs();
        let callers: HashMap<String, Caller> = self
            .host
            .store()
            .as_ref()
            .map(|store| {
                store
                    .delegations()
                    .iter()
                    .filter(|d| {
                        self.own_delegations.contains_key(&d.delegation_id)
                            && d.codex_child().is_some()
                            && d.status == DelegationStatus::Requested
                            && !running.contains(&d.delegation_id)
                    })
                    .filter_map(|d| {
                        self.delegation_identity(&d.parent.session_uid, cx)
                            .ok()
                            .map(|caller| (d.parent.session_uid.clone(), caller))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let capacity = RunnerCapacity {
            running: &running,
            max_concurrent,
            policy: &policy,
            running_workers: self.running_codex_workers(cx),
        };
        for effect in self.host.admit_delegations(capacity, &callers) {
            match effect {
                Effect::CheckStart {
                    id,
                    check,
                    new,
                    caller,
                } => {
                    let ceiling = self.ceiling_for(run_target(&new), START_LABEL);
                    cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                let mut new = *new;
                                check_delegation_run(&mut new, &caller, &ceiling?)
                            })
                            .await;
                        let _ = owner.update(cx, |owner, cx| {
                            owner.queued_start_checked(id, check, result, cx)
                        });
                    })
                    .detach();
                }
                Effect::StartRejected { id, error } => {
                    self.record_outcome(&id, Err(RunFailure::Failed(error)), cx)
                }
                _ => unreachable!("admission effect"),
            }
        }
    }

    /// Iced's `delegation_start_checked`, with the guard's recheck first: a
    /// target swapped since the request is refused.
    fn queued_start_checked(
        &mut self,
        id: String,
        check: RequestId,
        result: Result<Authority, String>,
        cx: &mut Context<Self>,
    ) {
        if !self.continues("queued start") {
            return;
        }
        let Some(delegation) = self
            .host
            .store()
            .as_ref()
            .and_then(|store| store.delegation(&id))
            .cloned()
        else {
            return;
        };
        let result = result.and_then(|authority| {
            let authorized = self
                .own_delegations
                .get(&id)
                .cloned()
                .ok_or_else(|| preview_notice(START_LABEL))?;
            self.recheck_run(
                START_LABEL,
                &authorized,
                &delegation_targets(&delegation),
                &authority,
            )?;
            Ok(authority)
        });
        let running = self.running_runs();
        let policy = self.policy().evaluation;
        let capacity = RunnerCapacity {
            running: &running,
            max_concurrent: self.max_concurrent_runs(),
            policy: &policy,
            running_workers: self.running_codex_workers(cx),
        };
        let caller = self
            .delegation_identity(&delegation.parent.session_uid, cx)
            .ok();
        let effects =
            self.host
                .delegation_start_checked(&id, check, result, capacity, caller.as_ref());
        for effect in effects {
            match effect {
                Effect::Start(id) => self.start_run(&id, cx),
                Effect::StartRejected { id, error } => {
                    self.record_outcome(&id, Err(RunFailure::Failed(error)), cx)
                }
                Effect::Refresh(_) => cx.notify(),
                _ => unreachable!("checked start effect"),
            }
        }
    }

    /// Iced's `start_delegation_run`, in the fixture's scoped environment.
    fn start_run(&mut self, id: &str, cx: &mut Context<Self>) {
        let started = (|| {
            let delegation = self
                .host
                .store()
                .as_ref()
                .and_then(|store| store.delegation(id))
                .ok_or_else(|| format!("delegation {id} is not stored"))?;
            let mut run = gitterm_core::delegations::codex_run(delegation, self.process_root())?;
            // Iced's start_delegation_run: the guidelines for an AGENTS.md
            // reader, without the task tools note.
            run.guidelines = self.launch_instructions(
                &run.cwd,
                gitterm_core::guidelines::InstructionFile::AgentsMd,
                &delegation.parent.workspace,
                false,
                cx,
            );
            let stop = CodexStop::new();
            let environment = if self.is_live() {
                let mut environment =
                    CodexEnvironment::detect().map_err(|error| error.to_string())?;
                environment.stop = Some(stop.clone());
                environment
            } else {
                let fixture = self.fixture_of(&run.cwd, START_LABEL)?;
                let targets: Vec<PathBuf> = std::iter::once(run.cwd.clone())
                    .chain(delegation.options.add_dirs.iter().cloned())
                    .collect();
                let authorized = self.authorize_process(START_LABEL, &targets)?;
                let env = self.fixture_child_env(&fixture, None)?;
                let codex_home = value_of(&env, "CODEX_HOME")
                    .map(PathBuf::from)
                    .ok_or("the fixture child has no CODEX_HOME")?;
                // The whole fixture guard once more, after the runner's git
                // preflight and immediately before Codex starts: the cwd and
                // each extra folder still resolve where they did, and their Git
                // dir and common dir still resolve inside the fixture with no
                // link under them. So whatever a config-driven program in the
                // preflight changed (a clean filter, a hook) cannot redirect the
                // run. Then Git itself, in Codex's own environment, names the
                // work tree, Git dir and common dir of each, which must lie in
                // the fixture too: config the preflight rewrote (`core.worktree`,
                // a `config.worktree`) can point the work tree elsewhere while
                // the admin dirs stay put. The remaining gap is the instant
                // between this check and the exec.
                let services = self.host.services().clone();
                let child_env = env.clone();
                let before_spawn = BeforeSpawn::new(move |_cwd: &Path| {
                    // The cwd is the first authorized target.
                    services
                        .recheck(&authorized)
                        .and_then(|()| services.recheck_git_work_trees(&authorized, &child_env))
                        .map_err(|denied| denied.notice().to_string())
                });

                CodexEnvironment {
                    search_path: self.codex_search_path(&env),
                    sessions_root: codex_home.join("sessions"),
                    child_env: Some(env),
                    before_spawn: Some(before_spawn),
                    stop: Some(stop.clone()),
                }
            };
            let runtime = &self
                .servers
                .as_ref()
                .ok_or("GitTerm's task host has shut down")?
                .runtime;
            let (progress, events) = tokio::sync::mpsc::unbounded_channel();
            eprintln!(
                "[delegation] starting {id} in {} (log {})",
                run.cwd.display(),
                run.log_path.display()
            );
            let (finished, done) = tokio::sync::oneshot::channel::<()>();
            let handle = runtime.spawn(async move {
                let result =
                    gitterm_core::codex_runner::run_with(run, progress, &environment).await;
                drop(finished);
                result
            });
            Ok::<_, String>((handle, events, stop, done))
        })();
        let (handle, mut events, stop, done) = match started {
            Ok(started) => started,
            Err(error) => return self.finish_run(id, Err(RunFailure::Failed(error)), cx),
        };
        self.runs.insert(
            id.to_string(),
            RunHandle {
                stop,
                done,
                abort: handle.abort_handle(),
            },
        );
        let id = id.to_string();
        cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
            while let Some(event) = events.recv().await {
                if owner
                    .update(cx, |owner, cx| owner.run_progress(&id, event, cx))
                    .is_err()
                {
                    return;
                }
            }
            let outcome = match handle.await {
                Ok(result) => result.map(Box::new).map_err(|error| {
                    if error.is_time_limit() {
                        RunFailure::TimeLimit
                    } else {
                        RunFailure::Failed(error.to_string())
                    }
                }),
                Err(error) => Err(RunFailure::Failed(format!(
                    "the Codex run stopped: {error}"
                ))),
            };
            let _ = owner.update(cx, |owner, cx| owner.finish_run(&id, outcome, cx));
        })
        .detach();
    }

    fn run_progress(&mut self, id: &str, event: CodexRunEvent, cx: &mut Context<Self>) {
        if !self.continues("progress") {
            return;
        }
        let Some(effects) = self
            .host
            .reduce_delegation(id, |delegation| Intent::Progress { delegation, event })
        else {
            return;
        };
        for effect in effects {
            match effect {
                Effect::RecordStarted { id, thread_id } => {
                    let now = chrono::Utc::now().to_rfc3339();
                    if let Err(error) = self.writable_store().and_then(|store| {
                        store
                            .mark_delegation_started(&id, &thread_id, &now)
                            .map_err(|error| error.to_string())
                    }) {
                        eprintln!("[delegation] {id}: {error}");
                    }
                }
                Effect::Refresh(_) => cx.notify(),
                _ => unreachable!("progress effect"),
            }
        }
    }

    /// Iced's `finish_delegation`. A run stopped by shutdown records nothing.
    fn finish_run(&mut self, id: &str, outcome: Outcome, cx: &mut Context<Self>) {
        self.runs.remove(id);
        if !self.continues("finish") {
            return;
        }
        let Some(mut effects) = self
            .host
            .reduce_delegation(id, |delegation| Intent::Finished { delegation })
        else {
            eprintln!("[delegation] {id} finished but is not in the store");
            return;
        };
        match effects.pop() {
            Some(Effect::RecordCompletion(id)) => self.record_outcome(&id, outcome, cx),
            None => self.start_queued_runs(cx),
            _ => unreachable!("completion effect"),
        }
    }

    /// Iced's `record_delegation_outcome`, store part only: attention, the
    /// parent's card and PR lookups are UI or delivery (B11).
    fn record_outcome(&mut self, id: &str, outcome: Outcome, cx: &mut Context<Self>) {
        if !self.own_delegations.contains_key(id) {
            // An imported record stays as Iced wrote it.
            eprintln!("[delegation] {id} is not this host's; its outcome is not recorded");
            return;
        }
        let now = chrono::Utc::now().to_rfc3339();
        let saved = self.writable_store().and_then(|store| {
            let delegation = store
                .delegation(id)
                .ok_or_else(|| format!("delegation {id} is not in the store"))?;
            if !delegation.status.is_active() {
                eprintln!(
                    "[delegation] {id} finished after it was {}; the result is dropped",
                    delegation.status.label()
                );
                return Ok(());
            }
            let minutes = delegation.options.time_limit_minutes.unwrap_or_default();
            let checkout = delegation.run_checkout().to_path_buf();
            let failed = |store: &mut TaskStore, message: String| {
                eprintln!("[delegation] {id} failed: {message}");
                store
                    .set_delegation_status(id, DelegationStatus::Failed { message }, &now)
                    .map_err(|error| error.to_string())
            };
            match outcome {
                Ok(outcome) => match store.complete_delegation(id, outcome.result, &now) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        failed(store, format!("could not store the Codex result: {error}"))
                    }
                },
                Err(RunFailure::TimeLimit) => {
                    let message = gitterm_core::delegations::time_limit_message(
                        minutes,
                        Some(&checkout),
                        false,
                    );
                    store
                        .stop_delegation_at_time_limit(id, message, &now)
                        .map_err(|error| error.to_string())
                }
                Err(RunFailure::Failed(message)) => failed(store, message),
            }
        });
        if let Err(error) = saved {
            eprintln!("[delegation] {id}: {error}");
        }
        cx.notify();
        self.start_queued_runs(cx);
    }

    /// Whether a fixture run's continuation at `stage` goes on: not once the
    /// host has shut down, when it writes nothing to the store.
    pub(super) fn continues(&mut self, stage: &'static str) -> bool {
        #[cfg(test)]
        if let Some((at, hook)) = self.before_continuation.take() {
            if at == stage {
                hook(self);
            } else {
                self.before_continuation = Some((at, hook));
            }
        }
        #[cfg(not(test))]
        let _ = stage;
        !self.is_shut_down()
    }

    /// After a checkout check: the targets authorized before it still
    /// resolve as they did, and the resolved targets, work tree and Git
    /// common dir pass the guard too.
    fn recheck_run(
        &self,
        label: &'static str,
        authorized: &AuthorizedTargets,
        targets: &[PathBuf],
        authority: &Authority,
    ) -> Result<(), String> {
        #[cfg(test)]
        if let Some(hook) = self.before_run_recheck.borrow_mut().take() {
            hook(&authority.checkout);
        }
        self.host
            .services()
            .recheck(authorized)
            .map_err(|denied| denied.notice().to_string())?;
        let mut targets = targets.to_vec();
        targets.extend([
            authority.checkout.clone(),
            authority.workdir.clone(),
            authority.repository.common_dir.clone(),
        ]);
        self.authorize_process(label, &targets).map(drop)
    }

    /// The gate's process decision for `targets` (denied with none).
    pub(super) fn authorize_process(
        &self,
        label: &'static str,
        targets: &[PathBuf],
    ) -> Result<AuthorizedTargets, String> {
        let class = Class {
            label,
            access: Access::Process,
        };
        self.host
            .services()
            .authorize_targets(class, targets)
            .map_err(|denied| denied.notice().to_string())
    }

    /// The fixture root holding `path`, resolved.
    pub(super) fn fixture_of(&self, path: &Path, label: &'static str) -> Result<PathBuf, String> {
        let actual = std::fs::canonicalize(path).map_err(|_| preview_notice(label))?;
        self.fixtures
            .iter()
            .find(|fixture| actual.starts_with(fixture))
            .cloned()
            .ok_or_else(|| preview_notice(label))
    }

    /// Git discovery from `path` stops below the fixture's parent.
    pub(super) fn ceiling_for(&self, path: &Path, label: &'static str) -> Result<PathBuf, String> {
        if self.is_live() {
            return Ok(PathBuf::from("/"));
        }
        let fixture = self.fixture_of(path, label)?;
        Ok(fixture.parent().unwrap_or(&fixture).to_path_buf())
    }

    /// A fixture child's whole environment, for `env_clear()`: the
    /// context's child environment (no `GITTERM_*`, the V6 config dir and
    /// provider homes pinned to the preview root) without any inherited
    /// `GIT_*` variable, the task MCP variables for `mcp_caller`, HOME and
    /// CODEX_HOME under the preview root, and a Git ceiling at the
    /// fixture's parent. The Codex trust check reads this same list.
    pub(super) fn fixture_child_env(
        &self,
        fixture: &Path,
        mcp_caller: Option<&str>,
    ) -> Result<Vec<(OsString, OsString)>, String> {
        let inherited = std::env::vars_os()
            .filter(|(name, _)| !name.to_str().is_none_or(|name| name.starts_with("GIT_")));
        let mut env = self
            .host
            .services()
            .child_env(inherited)
            .map_err(|error| format!("the fixture child environment: {error}"))?;
        let home = self.roots.root().join(FIXTURE_HOME);
        let codex_home = home.join(".codex");
        for dir in [home.parent().unwrap_or(&home), &home, &codex_home] {
            self.roots
                .create_private_dir(dir)
                .map_err(|error| format!("prepare {}: {error}", dir.display()))?;
        }
        let ceiling = fixture.parent().unwrap_or(fixture);
        let mut pinned: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("CODEX_HOME".into(), codex_home.into()),
            ("GIT_CEILING_DIRECTORIES".into(), ceiling.into()),
        ];
        if let Some(servers) = &self.servers {
            pinned.extend(
                servers
                    .task_mcp
                    .terminal_environment(mcp_caller)
                    .map(|(name, value)| (name.into(), value.into())),
            );
        }
        env.retain(|(name, _)| !pinned.iter().any(|(pin, _)| pin == name));
        env.extend(pinned);
        let readable: Vec<(String, String)> = env
            .iter()
            .filter_map(|(name, value)| Some((name.to_str()?.into(), value.to_str()?.into())))
            .collect();
        gitterm_core::workers::validate_codex_trust_home(&home, &readable, |_| None)?;
        Ok(env)
    }

    /// Where `codex` is found: the child's PATH.
    fn codex_search_path(&self, env: &[(OsString, OsString)]) -> OsString {
        #[cfg(test)]
        if let Some(path) = &self.codex_search_path {
            return path.clone();
        }
        value_of(env, "PATH").cloned().unwrap_or_default()
    }

    fn running_runs(&self) -> HashSet<String> {
        self.runs.keys().cloned().collect()
    }

    /// Live Codex worker sessions, for the plan-limit policy.
    fn running_codex_workers(&self, cx: &App) -> usize {
        self.running_workers(gitterm_core::plan_limits::Provider::Codex, cx)
    }

    fn max_concurrent_runs(&self) -> usize {
        crate::workspace_config::load_iced_settings_from(&self.iced_config)
            .review
            .max_concurrent_reviews
    }

    /// Iced's plan-limit reading, from its file (only read).
    pub(super) fn policy(&self) -> PolicyNow {
        use gitterm_core::plan_limits::{PlanLimitsFile, evaluate_policy};
        let file = PlanLimitsFile::load(&PlanLimitsFile::path_in(&self.iced_config));
        let evaluation = evaluate_policy(&file, chrono::Utc::now().timestamp());
        PolicyNow {
            report: evaluation.report.clone(),
            evaluation,
        }
    }

    /// Iced's `delegation_identity` for a GPUI chat pane: the pane, and the
    /// Iced workspace whose folder holds its checkout.
    pub(super) fn delegation_identity(&self, uid: &str, cx: &App) -> Result<Caller, String> {
        if let Some(caller) = self.live_terminal_caller(uid, cx) {
            return Ok(caller);
        }
        let chat = open_chats(cx)
            .into_iter()
            .find(|chat| chat.session_uid == uid)
            .ok_or_else(|| {
                format!(
                    "no open GitTerm tab has session id {uid}; a delegation reports back to the open tab that asked"
                )
            })?;
        let workspaces = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .map_or(&[][..], |source| &source.workspaces[..]);
        let workspace = workspaces
            .iter()
            .filter(|workspace| chat.cwd.starts_with(&workspace.dir))
            .max_by_key(|workspace| workspace.dir.len())
            .ok_or_else(|| {
                format!(
                    "the calling GitTerm tab's folder {} is in no workspace",
                    chat.cwd.display()
                )
            })?;
        let home = PathBuf::from(&workspace.dir);
        let remote = matches!(
            workspace.location,
            Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
        );
        let tab = CallerTab {
            session_uid: uid.to_string(),
            chat_session_id: chat.conversation.clone(),
            workspace: workspace.name.clone(),
            cwd: chat.cwd.clone(),
            remote,
            home: home.clone(),
            repos: workspace.repos.iter().map(PathBuf::from).collect(),
        };
        Ok(Caller {
            tab,
            target: chat.session_state().target,
            workspace_uid: crate::layout::stable_path_hash(&home),
            local_root: (!remote).then_some(home),
        })
    }
}

/// Iced's plan-limit evaluation and its report.
pub(super) struct PolicyNow {
    pub evaluation: gitterm_core::plan_limits::PolicyEvaluation,
    pub report: gitterm_core::plan_limits::PolicyReport,
}

pub(super) fn value_of<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsString> {
    env.iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

/// Where a run acts: its checkout (else the caller's), then extra folders.
fn new_targets(new: &NewDelegation) -> Vec<PathBuf> {
    std::iter::once(run_target(new))
        .chain(&new.options.add_dirs)
        .cloned()
        .collect()
}

fn run_target(new: &NewDelegation) -> &PathBuf {
    new.options.checkout.as_ref().unwrap_or(&new.parent.cwd)
}

fn delegation_targets(delegation: &Delegation) -> Vec<PathBuf> {
    std::iter::once(delegation.run_checkout().to_path_buf())
        .chain(delegation.options.add_dirs.iter().cloned())
        .collect()
}

/// Iced's `check_delegation_run` (src/main.rs), in the same order: the
/// core target checks, then the checkout's Git identity. Discovery stops
/// below `ceiling`, so it cannot leave the fixture.
fn check_delegation_run(
    new: &mut NewDelegation,
    caller: &CallerTab,
    ceiling: &Path,
) -> Result<Authority, String> {
    if ceiling == Path::new("/") {
        return gitterm_core::task_host::command::check_delegation_run(new, caller);
    }
    gitterm_core::delegations::check_run(new, caller)?;
    let path = new.options.checkout.as_deref().unwrap_or(&new.parent.cwd);
    let checkout = std::fs::canonicalize(path)
        .map_err(|error| format!("resolve delegation checkout {}: {error}", path.display()))?;
    let repository = gitterm_core::delegations::repository_identity(&checkout)?;
    let workdir = work_tree(&checkout, ceiling)?;
    let workdir = std::fs::canonicalize(&workdir).map_err(|error| {
        format!(
            "resolve delegation working directory {}: {error}",
            workdir.display()
        )
    })?;
    let roots = std::iter::once(&caller.home)
        .chain(caller.repos.iter())
        .map(|root| std::fs::canonicalize(root).ok())
        .collect();
    Ok(Authority {
        checkout,
        repository,
        workdir,
        roots,
    })
}

/// The work tree Git discovers from `checkout` (Iced opens it with git2),
/// with the caller's `GIT_*` variables cleared and discovery stopped below
/// `ceiling`. `rev-parse` only reads.
pub(super) fn work_tree(checkout: &Path, ceiling: &Path) -> Result<PathBuf, String> {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_none_or(|name| name.starts_with("GIT_")) {
            command.env_remove(name);
        }
    }
    let output = command
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("open delegation checkout {}: {error}", checkout.display()))?;
    if !output.status.success() {
        return Err(format!(
            "delegation checkout {} has no working directory: {}",
            checkout.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let path = String::from_utf8(output.stdout)
        .map_err(|_| format!("delegation checkout {} is not UTF-8", checkout.display()))?;
    Ok(PathBuf::from(path.trim_end_matches('\n')))
}
