//! The shared task host: one owner per process for the task command ingress,
//! the task authority (store and pending worker preparations), the service
//! gate and the reply lifecycle.
//!
//! The frontend polls [`command_stream`], the only consumer of the process
//! ingress, and hands every envelope to [`TaskHost::dispatch`]. The host asks
//! the [`ServiceContext`] first, answers what it can decide from the store
//! alone, and returns a typed [`HostEffect`] for the rest. Each effect carries
//! its reply and plain-data facts only. The frontend executes it with its own
//! live state (tabs, presets, launches) and answers the reply once its store
//! write has succeeded.
//!
//! Replies stay at-most-once runtime tickets: dropping every clone without an
//! answer releases the waiter, and a transport timeout never rolls back
//! accepted work. [`TaskHost::shutdown`] stops ingress and resolves pending
//! replies.
//!
//! Starting work is decided here too (H4b): the host admits task-session
//! launches, owns the launch queue and publishes a session only after it is
//! saved ([`launch`]); it owns the delegation lifecycle state and admits
//! headless Codex runs ([`runner`]). The frontend still builds commands,
//! starts terminals and spawns runs. Delivering finished results into the
//! requesting chat is decided here as well (H4c, [`delivery`]): the host
//! holds, reserves and records each submission, and the frontend writes the
//! prompt into its chat and reports the outcome.
use crate::content::html_render::HtmlRenderRequest;
use crate::delegations;
use crate::preview::service::ServiceContext;
use crate::task_commands::{
    SessionLaunch, TaskLaunchOrigin, TaskMutationCommand, TaskMutationEffect, TaskMutationFacts,
    TaskRead, TaskReadCommand, TaskReadEffect,
};
use crate::task_mcp::{
    missing_caller_error, ConsultDelegationRequest, CreateTaskRequest, DelegateTaskRequest,
    GetDelegationRequest, ListDelegationsRequest, MergePlanSetRequest, ReviewDelegationRequest,
    TaskControlEnvelope, TaskControlOperation, TaskControlReceiver, TaskControlReply,
    TaskControlSender, TaskSessionEventRequest, UpdateTaskHandoffRequest,
};
use crate::tasks::state::TaskAuthority;
use crate::tasks::{Delegation, DelegationChild, DelegationStatus, TaskStore};
use serde_json::{json, Value};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::Stream;

mod delivery;
mod launch;
mod runner;
pub use delivery::{
    DeliveryChat, DeliveryCompletion, DeliveryEffect, DeliveryReceipt, DeliveryTick, Dismissal,
    Submission,
};
pub use launch::{
    cancel_queued_worker_launch, launch_provider, update_queued_task_request, AwaitingWorker,
    LaunchAdmission, LaunchPolicy, LaunchRequest, LaunchTicket, LaunchedSession, Promotion,
    QueuedTaskLaunch,
};
pub use runner::RunnerCapacity;

const STORE_UNAVAILABLE: &str = "GitTerm's task store is unavailable";

/// Work the frontend executes for one command, with the reply it must answer.
#[derive(Debug)]
pub enum HostEffect {
    /// `plan_limits`: the snapshot needs live sessions and settings.
    PlanLimits { reply: TaskControlReply },
    /// `html_render`: needs the caller's open native chat tab.
    HtmlRender {
        reply: TaskControlReply,
        caller: Option<String>,
        request: HtmlRenderRequest,
    },
    /// `task_list` / `task_get`: the authority's projection. The frontend adds
    /// open sessions and presets before replying.
    Read {
        reply: TaskControlReply,
        effect: TaskReadEffect,
    },
    /// `task_create`: deferred; replies after the preparation is saved.
    Create {
        reply: TaskControlReply,
        caller: Option<String>,
        request: CreateTaskRequest,
    },
    /// `task_launch_session`. The host fixes the MCP origin; the frontend
    /// runs admission and launch with it.
    LaunchSession {
        reply: TaskControlReply,
        launch: SessionLaunch,
    },
    UpdateHandoff {
        reply: TaskControlReply,
        caller: Option<String>,
        request: UpdateTaskHandoffRequest,
    },
    DelegateTask {
        reply: TaskControlReply,
        caller: Option<String>,
        request: DelegateTaskRequest,
    },
    /// A task session's turn ended and no worker report explains it: mark
    /// that session's tabs as waiting for input.
    TurnEnded {
        reply: TaskControlReply,
        task_id: String,
        session_id: String,
    },
    RequestReview {
        reply: TaskControlReply,
        caller: Option<String>,
        request: ReviewDelegationRequest,
    },
    RequestConsult {
        reply: TaskControlReply,
        caller: Option<String>,
        request: ConsultDelegationRequest,
    },
    /// `delegation_get`: the stored record; the frontend adds the live
    /// worker card.
    DelegationRecord {
        reply: TaskControlReply,
        delegation: Box<Delegation>,
        record: Value,
    },
    MergePlanSet {
        reply: TaskControlReply,
        caller: Option<String>,
        request: MergePlanSetRequest,
    },
    MergePlanGet {
        reply: TaskControlReply,
        caller: Option<String>,
    },
}

pub struct TaskHost {
    authority: TaskAuthority,
    services: ServiceContext,
    commands: TaskControlSender,
    launch_queue: Vec<QueuedTaskLaunch>,
    delegations: crate::delegation_state::State,
}

impl TaskHost {
    /// `commands` is the sender of the ingress whose receiver
    /// [`command_stream`] (or a fixture) feeds to [`Self::dispatch`].
    pub fn new(
        store: Option<TaskStore>,
        services: ServiceContext,
        commands: TaskControlSender,
    ) -> Self {
        Self {
            authority: store.into(),
            services,
            commands,
            launch_queue: Vec::new(),
            delegations: Default::default(),
        }
    }

    pub fn services(&self) -> &ServiceContext {
        &self.services
    }

    /// Transitional frontend seam (see [`TaskAuthority::store`]).
    pub fn store(&self) -> &Option<TaskStore> {
        self.authority.store()
    }

    pub fn store_mut(&mut self) -> &mut Option<TaskStore> {
        self.authority.store_mut()
    }

    /// A new authority for this store: pending worker preparations belonged
    /// to the store being replaced and are dropped with it. The launch queue
    /// and delegation state are kept, as before they moved here.
    pub fn replace_store(&mut self, store: Option<TaskStore>) {
        self.authority = store.into();
    }

    pub fn reduce(&self, command: TaskReadCommand) -> TaskReadEffect {
        self.authority.reduce(command)
    }

    pub fn reduce_mutation(
        &mut self,
        command: TaskMutationCommand,
        facts: &TaskMutationFacts,
    ) -> TaskMutationEffect {
        self.authority.reduce_mutation(command, facts)
    }

    /// See [`TaskAuthority::pending_worker_subject`].
    pub fn pending_worker_subject(
        &self,
        id: crate::task_commands::WorkerPreparationId,
    ) -> Option<(&str, Option<&std::path::Path>)> {
        self.authority.pending_worker_subject(id)
    }

    /// Stops ingress and resolves every pending reply, even ones whose
    /// tickets are still held by deferred work.
    pub fn shutdown(&self) {
        self.commands.close();
    }

    /// `None` when the host has answered the command itself. The caller's
    /// own task is not known here, so an operation that would default to
    /// the caller's task worktree has no target (see
    /// [`Self::dispatch_with_caller_task`]).
    pub fn dispatch(&self, envelope: TaskControlEnvelope) -> Option<HostEffect> {
        self.dispatch_with_caller_task(envelope, None)
    }

    /// [`Self::dispatch`], with `caller_task` the task the calling tab
    /// belongs to: the frontend knows its tabs; the store knows the task's
    /// worktree. Every write target the operation resolves to must pass the
    /// gate, and an unresolved one is checked as no target, which a
    /// fixture-launch context denies.
    pub fn dispatch_with_caller_task(
        &self,
        envelope: TaskControlEnvelope,
        caller_task: Option<&str>,
    ) -> Option<HostEffect> {
        let TaskControlEnvelope {
            operation,
            caller,
            reply,
        } = envelope;
        let authorized = match self.write_targets(&operation, caller_task) {
            Some(targets) if !targets.is_empty() => targets
                .iter()
                .try_for_each(|target| self.services.operation(&operation, Some(target))),
            _ => self.services.operation(&operation, None),
        };
        if let Err(denied) = authorized {
            reply.send(Err(denied.notice().to_string()));
            return None;
        }
        use TaskControlOperation as Op;
        Some(match operation {
            Op::PlanLimits => HostEffect::PlanLimits { reply },
            Op::HtmlRender(request) => HostEffect::HtmlRender {
                reply,
                caller,
                request,
            },
            Op::List(request) => self.read(
                reply,
                TaskRead::List {
                    include_archived: request.include_archived,
                },
            ),
            Op::Get(request) => self.read(
                reply,
                TaskRead::Get {
                    task_id: request.task_id,
                },
            ),
            Op::Create(request) => HostEffect::Create {
                reply,
                caller,
                request,
            },
            Op::LaunchSession(request) => HostEffect::LaunchSession {
                reply,
                launch: SessionLaunch {
                    task_id: request.task_id,
                    preset_name: request.preset_name,
                    origin: TaskLaunchOrigin::Agent,
                    user_model_choice: request.user_model_choice,
                },
            },
            Op::UpdateHandoff(request) => HostEffect::UpdateHandoff {
                reply,
                caller,
                request,
            },
            Op::DelegateTask(request) => HostEffect::DelegateTask {
                reply,
                caller,
                request,
            },
            Op::SessionEvent(request) => return self.session_event(reply, request),
            Op::RequestReview(request) => HostEffect::RequestReview {
                reply,
                caller,
                request,
            },
            Op::RequestConsult(request) => HostEffect::RequestConsult {
                reply,
                caller,
                request,
            },
            Op::GetDelegation(request) => {
                return self.get_delegation(reply, caller.as_deref(), &request)
            }
            Op::ListDelegations(request) => {
                reply.send(self.list_delegations(caller.as_deref(), &request));
                return None;
            }
            Op::MergePlanSet(request) => HostEffect::MergePlanSet {
                reply,
                caller,
                request,
            },
            Op::MergePlanGet => HostEffect::MergePlanGet { reply, caller },
        })
    }

    /// Every path a write or launch would act in, from the request and the
    /// store. `None` when any one of them cannot be resolved, and for
    /// operations with no checkout (reads, `merge_plan_set`). Git admin dirs
    /// of each target are checked by the gate itself.
    fn write_targets(
        &self,
        operation: &TaskControlOperation,
        caller_task: Option<&str>,
    ) -> Option<Vec<PathBuf>> {
        use TaskControlOperation as Op;
        let worktree = |task_id: &str| self.store().as_ref()?.get(task_id)?.worktree.path.clone();
        let checkout_or_caller =
            |checkout: Option<&PathBuf>| checkout.cloned().or_else(|| worktree(caller_task?));
        match operation {
            Op::Create(request) => Some(vec![request.repository_path.clone()]),
            Op::LaunchSession(request) => Some(vec![worktree(&request.task_id)?]),
            Op::UpdateHandoff(request) => Some(vec![worktree(&request.task_id)?]),
            Op::SessionEvent(request) => Some(vec![worktree(&request.task_id)?]),
            Op::RequestReview(request) => {
                Some(vec![checkout_or_caller(request.checkout.as_ref())?])
            }
            Op::RequestConsult(request) => {
                Some(vec![checkout_or_caller(request.checkout.as_ref())?])
            }
            Op::DelegateTask(request) => {
                let mut targets: Vec<_> = request
                    .checkout
                    .iter()
                    .chain(&request.repo)
                    .cloned()
                    .collect();
                if targets.is_empty() {
                    targets.push(worktree(caller_task?)?);
                }
                targets.extend(request.add_dirs.iter().cloned());
                Some(targets)
            }
            Op::MergePlanSet(_)
            | Op::PlanLimits
            | Op::HtmlRender(_)
            | Op::List(_)
            | Op::Get(_)
            | Op::GetDelegation(_)
            | Op::ListDelegations(_)
            | Op::MergePlanGet => None,
        }
    }

    fn read(&self, reply: TaskControlReply, query: TaskRead) -> HostEffect {
        let effect = self.authority.reduce(TaskReadCommand {
            reply: reply.id(),
            query,
        });
        HostEffect::Read { reply, effect }
    }

    fn session_event(
        &self,
        reply: TaskControlReply,
        request: TaskSessionEventRequest,
    ) -> Option<HostEffect> {
        // Only turn completion carries a lifecycle edge today; other notify
        // types are acknowledged and dropped.
        if request.event_type != "agent-turn-complete" {
            reply.send(Ok(json!({ "ignored": request.event_type })));
            return None;
        }
        // A worker that reported done goes idle at its prompt; that turn end
        // is not a request for input (S6).
        let reported_done = self.store().as_ref().is_some_and(|store| {
            store.delegations().iter().any(|delegation| {
                delegation.status == DelegationStatus::Completed
                    && matches!(
                        &delegation.child,
                        DelegationChild::TaskSession {
                            task_session_id: Some(session),
                            ..
                        } if *session == request.session_id
                    )
            })
        });
        if reported_done {
            reply.send(Ok(json!({
                "task_id": request.task_id,
                "session_id": request.session_id,
                "waiting_for_input": false,
                "ignored": "the worker reported done",
            })));
            return None;
        }
        Some(HostEffect::TurnEnded {
            reply,
            task_id: request.task_id,
            session_id: request.session_id,
        })
    }

    fn get_delegation(
        &self,
        reply: TaskControlReply,
        caller: Option<&str>,
        request: &GetDelegationRequest,
    ) -> Option<HostEffect> {
        let id = request.delegation_id.trim();
        let found = caller
            .ok_or_else(|| missing_caller_error("delegation_get"))
            .and_then(|_| {
                self.store()
                    .as_ref()
                    .ok_or_else(|| STORE_UNAVAILABLE.into())
            })
            .and_then(|store| {
                store
                    .delegation(id)
                    .ok_or_else(|| format!("delegation {id} does not exist"))
            })
            .and_then(|delegation| {
                let record =
                    delegations::delegation_record(delegation, self.services.state_root())?;
                Ok((Box::new(delegation.clone()), record))
            });
        match found {
            Ok((delegation, record)) => Some(HostEffect::DelegationRecord {
                reply,
                delegation,
                record,
            }),
            Err(error) => {
                reply.send(Err(error));
                None
            }
        }
    }

    fn list_delegations(
        &self,
        caller: Option<&str>,
        request: &ListDelegationsRequest,
    ) -> Result<Value, String> {
        let caller = caller.ok_or_else(|| missing_caller_error("delegation_list"))?;
        let store = self
            .store()
            .as_ref()
            .ok_or_else(|| STORE_UNAVAILABLE.to_string())?;
        delegations::delegation_list(
            &store.delegations_for_parent(caller),
            request.status.as_deref(),
        )
    }
}

/// The process's task commands in admission (FIFO) order. The receiver is
/// taken once per process; a later call yields an empty stream.
pub fn command_stream() -> impl Stream<Item = TaskControlEnvelope> + Send + 'static {
    commands(crate::task_mcp::take_command_receiver())
}

type NextCommand =
    Pin<Box<dyn Future<Output = Option<(TaskControlEnvelope, TaskControlReceiver)>> + Send>>;

struct Commands(Option<NextCommand>);

fn next_command(mut receiver: TaskControlReceiver) -> NextCommand {
    Box::pin(async move {
        let envelope = receiver.recv().await?;
        Some((envelope, receiver))
    })
}

fn commands(receiver: Option<TaskControlReceiver>) -> Commands {
    Commands(receiver.map(next_command))
}

impl Stream for Commands {
    type Item = TaskControlEnvelope;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(next) = self.0.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = next.as_mut().poll(cx);
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some((envelope, receiver))) => {
                self.0 = Some(next_command(receiver));
                Poll::Ready(Some(envelope))
            }
            Poll::Ready(None) => {
                self.0 = None;
                Poll::Ready(None)
            }
        }
    }
}

#[cfg(test)]
#[path = "task_host_tests.rs"]
mod tests;
