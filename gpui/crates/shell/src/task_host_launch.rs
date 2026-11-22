//! H5b B10b: `task_launch_session` in fixture mode, in the order of Iced's
//! `try_launch_task_child`: the preset from Iced's settings, the worker
//! delegation the launch carries out, its model and options on the command,
//! a preassigned Claude session id, then admission (`TaskHost::admit_launch`,
//! which may queue), the Codex notify hook, the brief, the terminal child and
//! last `TaskHost::publish_launch`. The session is announced only after the
//! store saved it and attached its worker; a failed save drops the child,
//! which kills its process group.
//!
//! The child runs `/bin/sh -c <command>` in a terminal whose environment is
//! exactly the fixture child environment (every other inherited variable is
//! removed), with the task MCP variables naming the task session. Briefs and
//! guidelines are written under the preview root, never Iced's config dir.
//!
//! The host keeps each launched terminal until the shell adopts it into a
//! pane ([`TerminalLaunched`]); one never adopted is killed at shutdown.
//! Only tasks and worker delegations this host stored are launched or
//! attached: a record imported from Iced answers the preview notice.
//!
//! As in Iced, the fixture tick starts queued launches when a slot frees
//! ([`TaskHostOwner::promote_queued_tasks`]), a launched agent's exit folds
//! into its task's lifecycle ([`TaskHostOwner::terminal_exited`]), and the
//! terminal's `task_update_handoff` is attributed to its task session.
use super::*;
use gitterm_core::{
    plan_limits::Provider,
    session,
    task_commands::{SessionLaunch, TaskLaunchOrigin, TaskPreset},
    task_host::{
        LaunchAdmission, LaunchPolicy, LaunchRequest, LaunchedSession, LinkedSession, Promotion,
        TaskSessionLiveState, TaskSessionOutcome, launch_provider,
    },
    tasks::{
        ExecutorTarget, HarnessConversationBackend, HarnessConversationRef, HarnessKind,
        HarnessSelection, ObjectiveDeliveryState, TaskWorktreeState, WorkspaceLocationIdentity,
    },
    workers::{WorkerProvider, provider_for_command},
};
use gitterm_terminal::{SessionState, TerminalLaunch, TerminalView};
use std::{collections::HashMap, ffi::OsString};

/// Composed task briefs, under the preview root (Iced's `task-briefs`).
pub(crate) const BRIEFS: &str = "task-briefs";
/// Variables the PTY launcher sets itself when the overlay does not.
/// The terminal's own variables (gitterm_terminal's `BackendSettings`
/// defaults), added on top of the fixture env.
const TERMINAL_SET: [(&str, &str); 2] = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];

const LAUNCHER_SET: [&str; 7] = [
    "LANG",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "ALACRITTY_WINDOW_ID",
    "WINDOWID",
];

/// A fixture launch published task session `session_id`; the shell adopts
/// its terminal with [`TaskHostOwner::take_terminal`].
#[derive(Clone, Debug)]
pub(crate) struct TerminalLaunched {
    pub session_id: String,
    pub title: String,
    pub cwd: PathBuf,
    /// Iced's persisted `startup_command` for the tab (see
    /// [`persisted_startup_command`]).
    pub startup_command: Option<String>,
    /// The pane's session target. Its `session_uid` is the task MCP caller
    /// the child's environment names (the task session id).
    pub target: session::Target,
}
impl gpui::EventEmitter<TerminalLaunched> for TaskHostOwner {}

/// A launched terminal's child exited; the shell marks its pane exited.
#[derive(Clone, Debug)]
pub(crate) struct TerminalExited {
    pub session_id: String,
}
impl gpui::EventEmitter<TerminalExited> for TaskHostOwner {}

/// A launched terminal the host still owns. Dropping it kills the child's
/// process group (the PTY child leads one); the entity's release then
/// reaps the killed child.
pub(crate) struct HostTerminal {
    view: Entity<TerminalView>,
    group: Option<u32>,
}
impl HostTerminal {
    /// The terminal, now someone else's to stop.
    fn release(mut self) -> Entity<TerminalView> {
        self.group = None;
        self.view.clone()
    }
    fn kill(&mut self) {
        if let Some(pid) = self.group.take()
            && let Err(error) = gitterm_terminal::kill_process_group(pid)
        {
            eprintln!("[tasks] could not stop task terminal group {pid}: {error}");
        }
    }
}
impl Drop for HostTerminal {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A launched session, adopted or not: liveness and the provider it
/// spends, for worker lookup and plan limits.
pub(super) struct Launched {
    task_id: String,
    provider: Option<Provider>,
    view: gpui::WeakEntity<TerminalView>,
    /// The pane's target, for the reducer's session facts.
    target: session::Target,
    /// Iced's `task_live_state` for the tab: working from an agent's
    /// launch, none once its child exited.
    live: Option<TaskSessionLiveState>,
}

pub(super) enum Started {
    Session(serde_json::Value),
    Queued(usize),
}

impl TaskHostOwner {
    /// The `task_launch_session` arm in fixture mode (an agent's launch).
    pub(super) fn launch_session(
        &mut self,
        reply: TaskControlReply,
        launch: SessionLaunch,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let result = self.launch_task_session(launch, label, cx);
        reply.send(result);
    }

    /// A launch from GPUI's own task UI: the user's choice, which plan
    /// limits never hold.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn launch_from_ui(
        &mut self,
        task_id: &str,
        preset_name: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        let launch = SessionLaunch {
            task_id: task_id.to_string(),
            preset_name: preset_name.map(str::to_string),
            origin: TaskLaunchOrigin::User,
            user_model_choice: false,
        };
        self.launch_task_session(launch, "task_launch_session", cx)
    }

    /// The launched terminal of `session_id`, for the shell's new pane; the
    /// host no longer stops it.
    pub(crate) fn take_terminal(&mut self, session_id: &str) -> Option<Entity<TerminalView>> {
        self.terminals.remove(session_id).map(HostTerminal::release)
    }

    /// Iced's relaunch restore of a task tab (src/main.rs, the saved tab
    /// loop of workspace restore, then `create_tab_with_session_uid`), in
    /// fixture mode: a restored layout pane with a saved command and task
    /// runs the command rebuilt as a launch builds it, as the same task
    /// session, in a terminal this host owns until the shell adopts it
    /// ([`Self::take_terminal`]). None restores a plain shell and keeps the
    /// saved fields: no command or task, a task this host did not create
    /// (always so in plain preview), neither folder there, or a refusal.
    pub(crate) fn restore_terminal(
        &mut self,
        saved: &crate::layout::TerminalSession,
        cx: &mut Context<Self>,
    ) -> Option<TerminalLaunched> {
        let (Some(_), Some(task_id)) = (&saved.startup_command, &saved.task_id) else {
            return None;
        };
        self.try_restore(saved, task_id, cx)
            .inspect_err(|error| {
                eprintln!("[tasks] {task_id}: restored a plain shell, not its command: {error}");
            })
            .ok()
    }

    fn try_restore(
        &mut self,
        saved: &crate::layout::TerminalSession,
        task_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<TerminalLaunched, String> {
        const LABEL: &str = "task_launch_session";
        if !self.own_tasks.contains(task_id) {
            return Err(preview_notice(LABEL));
        }
        let task = self
            .host
            .store()
            .as_ref()
            .and_then(|store| store.get(task_id))
            .cloned()
            .ok_or_else(|| format!("Task {task_id} is not available"))?;
        let WorkspaceLocationIdentity::Local { .. } = &task.workspace.location else {
            return Err("Remote task sessions require the later remote-executor slice".into());
        };
        let worktree = task.worktree.path.clone().filter(|path| path.is_dir());
        // Iced's folder: the saved one while it is a directory, else the
        // task's worktree.
        let folder = if saved.cwd.is_dir() {
            saved.cwd.clone()
        } else {
            worktree.clone().ok_or_else(|| {
                format!(
                    "neither {} nor task {task_id}'s worktree is a folder",
                    saved.cwd.display()
                )
            })?
        };
        let fixture = self.fixture_of(&folder, LABEL)?;
        // Trust and guidelines read the task's worktree, as its launch did.
        let worktree = worktree.unwrap_or_else(|| folder.clone());
        let mut targets = vec![folder.clone()];
        if worktree != folder {
            targets.push(worktree.clone());
        }
        let authorized = self.authorize_process(LABEL, &targets)?;
        // Iced keeps the saved task session, or fixes a new one before the
        // terminal starts.
        let session_id = saved
            .task_session_id
            .clone()
            .unwrap_or_else(TaskRecord::fresh_id);
        let env = self.terminal_child_env(&fixture, &session_id)?;
        let settings = crate::workspace_config::load_iced_settings_from(&self.iced_config);
        let notify = self
            .servers
            .as_ref()
            .ok_or(SHUT_DOWN)?
            .task_mcp
            .notify_endpoint();
        // A transcript lives where the fixture child's Claude writes it.
        let projects = runs::value_of(&env, "CLAUDE_CONFIG_DIR")
            .map(|dir| PathBuf::from(dir).join("projects"));
        let restored = gitterm_core::task_host::restored_startup(
            saved.startup_command.clone(),
            None,
            Some(&task),
            saved.task_session_id.as_deref(),
            |name| {
                settings
                    .agent_presets
                    .iter()
                    .find(|preset| preset.name == name)
                    .and_then(|preset| preset_backend(&preset.name, &preset.command))
            },
            |id| {
                projects
                    .as_deref()
                    .is_some_and(|projects| claude_transcript_exists(projects, id))
            },
            |command| {
                gitterm_core::task_mcp::configure_codex_notify(
                    &command,
                    &notify,
                    task_id,
                    &session_id,
                )
            },
        );
        let command = restored
            .startup_command
            .clone()
            .ok_or("the saved command was dropped")?;
        let runtime = self.runtime_command(&command, &worktree, &env, &task, cx)?;
        // The fixture guard once more, right before the child starts, with
        // the work tree Git itself would use (issue 84).
        let services = self.host.services();
        services
            .recheck(&authorized)
            .and_then(|()| services.recheck_git_work_trees(&authorized, &env))
            .map_err(|denied| denied.notice().to_string())?;
        let terminal = spawn_terminal(
            &folder,
            Some(runtime),
            env,
            cx.weak_entity(),
            &session_id,
            cx,
        )?;
        let target = session::Target {
            session_uid: session_id.clone(),
            task_id: Some(task_id.to_string()),
            task_session_id: Some(session_id.clone()),
            ..crate::sessions::fresh_target()
        };
        // Iced's restored tab tracks no live state until its agent signals.
        self.launched.insert(
            session_id.clone(),
            Launched {
                task_id: task_id.to_string(),
                provider: launch_provider(&command),
                view: terminal.view.downgrade(),
                target: target.clone(),
                live: None,
            },
        );
        self.terminals.insert(session_id.clone(), terminal);
        let label = task
            .sessions
            .iter()
            .find(|session| session.task_session_id == session_id)
            .map_or("Terminal", |session| session.label.as_str());
        Ok(TerminalLaunched {
            title: format!("{label} · {}", task.title),
            session_id,
            cwd: folder,
            startup_command: restored.startup_command,
            target,
        })
    }

    /// Iced's `LaunchSession` arm, after the fixture guard authorized the
    /// task's worktree.
    fn launch_task_session(
        &mut self,
        launch: SessionLaunch,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        if !self.own_tasks.contains(&launch.task_id) {
            // An imported task stays as Iced wrote it.
            return Err(preview_notice(label));
        }
        self.authorize_process(label, &self.task_worktree(&launch.task_id))?;
        let settings = crate::workspace_config::load_iced_settings_from(&self.iced_config);
        let facts = self.launch_facts(&settings, cx);
        let preset_idx = match self.reduce(TaskMutation::LaunchSession(launch.clone()), &facts)? {
            TaskMutationAction::LaunchSession { preset_index, .. } => preset_index,
            TaskMutationAction::QueueSession { preset_index, .. } => Some(preset_index),
            _ => unreachable!("launch effect"),
        };
        let cost_note = launch.user_model_choice.then_some(runs::COST_NOTE);
        match self.try_launch(&launch, preset_idx, &settings, facts, label, cx)? {
            Started::Session(mut session) => {
                if let Some(note) = cost_note {
                    session["cost_note"] = serde_json::json!(note);
                }
                Ok(session)
            }
            Started::Queued(position) => Ok(serde_json::json!({
                "task_id": launch.task_id,
                "queued": true,
                "queue_position": position,
                "cost_note": cost_note,
                "detail": "Concurrency or plan limits deferred this launch; it starts automatically when capacity permits.",
            })),
        }
    }

    /// Iced's `try_launch_task_child`, with the fixture's terminal.
    pub(super) fn try_launch(
        &mut self,
        launch: &SessionLaunch,
        preset_idx: Option<usize>,
        settings: &crate::workspace_config::IcedSettings,
        mut facts: TaskMutationFacts,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Result<Started, String> {
        let task_id = launch.task_id.as_str();
        let task = self
            .host
            .store()
            .as_ref()
            .and_then(|store| store.get(task_id))
            .cloned()
            .ok_or_else(|| format!("Task {task_id} is not available"))?;
        let worktree = task
            .worktree
            .path
            .clone()
            .ok_or_else(|| format!("Task {} has no prepared worktree to resume", task.title))?;
        if task.worktree.state != TaskWorktreeState::Ready || !worktree.is_dir() {
            return Err(format!(
                "Task {} worktree is not ready at {}",
                task.title,
                worktree.display()
            ));
        }
        let (command, child_label, harness, backend) = match preset_idx {
            Some(index) => {
                let preset = settings
                    .agent_presets
                    .get(index)
                    .ok_or("The selected agent preset is unavailable")?;
                gitterm_core::workers::require_task_tools(&preset.command)?;
                (
                    Some(preset.command.clone()),
                    preset.name.clone(),
                    Some(HarnessSelection {
                        kind: HarnessKind::TerminalPreset {
                            preset_name: preset.name.clone(),
                        },
                        model: None,
                    }),
                    preset_backend(&preset.name, &preset.command),
                )
            }
            None => (None, "Terminal".to_string(), None, None),
        };
        // The open worker delegation this launch carries out, when this
        // host stored it.
        let worker = command
            .as_ref()
            .and_then(|_| {
                self.host
                    .worker_awaiting_launch(task_id, &child_label, |session| {
                        self.session_live(session, cx)
                    })
            })
            .filter(|worker| self.own_delegations.contains_key(&worker.delegation_id));
        let worker_model = worker.as_ref().and_then(|worker| worker.model.clone());
        let options = worker
            .as_ref()
            .map(|worker| worker.options.clone())
            .unwrap_or_default();
        // The worker's extra folders go on the command: the guard covers
        // them with the worktree, now and right before the child starts.
        let targets: Vec<PathBuf> = std::iter::once(worktree.clone())
            .chain(options.add_dirs.iter().cloned())
            .collect();
        let authorized = self.authorize_process(label, &targets)?;
        let (command, harness) = match (command, worker_model.as_deref()) {
            (Some(command), Some(model)) => (
                Some(
                    gitterm_core::workers::with_model_flag(&command, model).map_err(|error| {
                        format!("The worker model {model} cannot be passed to preset {child_label}: {error}")
                    })?,
                ),
                harness.map(|harness| HarnessSelection {
                    model: Some(model.to_string()),
                    ..harness
                }),
            ),
            (command, _) => (command, harness),
        };
        let command = command
            .map(|command| {
                gitterm_core::workers::with_launch_options(
                    &command,
                    options.effort,
                    &options.add_dirs,
                )
                .map_err(|error| {
                    format!(
                        "The worker's options cannot be passed to preset {child_label}: {error}"
                    )
                })
            })
            .transpose()?;
        let (command, chat_session_id) = match command {
            Some(command) => {
                let (command, id) = with_preassigned_session_id(&command);
                (Some(command), id)
            }
            None => (None, None),
        };
        let launched_agent = command.is_some();
        let WorkspaceLocationIdentity::Local { .. } = &task.workspace.location else {
            return Err("Remote task sessions require the later remote-executor slice".into());
        };
        let provider = command.as_deref().and_then(launch_provider);
        let policy = self.policy();
        let policy = LaunchPolicy {
            reason: policy.report.routing.reason.clone(),
            evaluation: policy.evaluation,
            running_workers: provider.map_or(0, |provider| self.running_workers(provider, cx)),
            codex_runs: self.runs.len(),
        };
        facts.slot_holders = self.slot_holders(cx);
        let request = LaunchRequest {
            task_id: task_id.into(),
            preset_idx,
            preset_name: preset_idx.map(|_| child_label.clone()),
            origin: launch.origin,
            user_model_choice: launch.user_model_choice,
            worker,
            provider,
            harness: harness.clone(),
            launched_agent,
            worktree: worktree.clone(),
        };
        // A queued launch is written to the store: the preview's own copy.
        self.writable_store()?;
        let ticket = match self.host.admit_launch(request, policy, facts)? {
            LaunchAdmission::Queued { position } => return Ok(Started::Queued(position)),
            LaunchAdmission::Open(ticket) => ticket,
        };
        let session_id = ticket.session_id().to_string();
        // Iced's tab target. The caller uid is the task session id, which
        // the child's task MCP URL already names (Iced's is the tab's own).
        let target = session::Target {
            session_uid: session_id.clone(),
            task_id: Some(task_id.to_string()),
            task_session_id: Some(session_id.clone()),
            ..crate::sessions::fresh_target()
        };
        let owner = cx.weak_entity();
        let prepared = (|| {
            let notify = self
                .servers
                .as_ref()
                .ok_or(SHUT_DOWN)?
                .task_mcp
                .notify_endpoint();
            let command = match command {
                Some(command) if backend == Some(HarnessConversationBackend::Codex) => {
                    Some(gitterm_core::task_mcp::configure_codex_notify(
                        &command,
                        &notify,
                        task_id,
                        &session_id,
                    ))
                }
                other => other,
            };
            // Kept for the persisted command: a restart must never re-submit
            // the objective (Iced's `pre_injection_command`).
            let pre_injection = command.clone();
            // Iced's TRU-128 brief delivery, written under the preview root.
            let (command, objective_delivery) = match command {
                Some(command) if accepts_initial_prompt(&command, backend) => {
                    match self.write_brief(&session_id, &task, ticket.worker()) {
                        Ok(brief) => (
                            Some(command_with_initial_prompt(&command, &brief)),
                            if matches!(
                                backend,
                                Some(
                                    HarnessConversationBackend::Codex
                                        | HarnessConversationBackend::Claude
                                )
                            ) {
                                ObjectiveDeliveryState::Pending
                            } else {
                                ObjectiveDeliveryState::Delivered
                            },
                        ),
                        Err(error) => {
                            eprintln!(
                                "[tasks] Brief for task {task_id} session {session_id} could not be embedded ({error}); the session starts unbriefed"
                            );
                            (Some(command), ObjectiveDeliveryState::NotDelivered)
                        }
                    }
                }
                other => (other, ObjectiveDeliveryState::NotDelivered),
            };
            let fixture = self.fixture_of(&worktree, label)?;
            let env = self.terminal_child_env(&fixture, &session_id)?;
            let command = command
                .map(|command| self.runtime_command(&command, &worktree, &env, &task, cx))
                .transpose()?;
            // The fixture guard once more, right before the child starts,
            // with the work tree Git itself would use (issue 84).
            let services = self.host.services();
            services
                .recheck(&authorized)
                .and_then(|()| services.recheck_git_work_trees(&authorized, &env))
                .map_err(|denied| denied.notice().to_string())?;
            let terminal = spawn_terminal(&worktree, command, env, owner, &session_id, cx)?;
            Ok::<_, String>((terminal, objective_delivery, pre_injection))
        })();
        let (terminal, objective_delivery, pre_injection) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.host.abandon_launch(ticket);
                return Err(error);
            }
        };
        #[cfg(test)]
        if let Some(hook) = self.before_publish.take() {
            hook(self);
        }
        let startup_command = persisted_startup_command(
            chat_session_id.as_deref(),
            worker_model.as_deref(),
            &options,
            pre_injection,
        );
        let conversation = chat_session_id.map(|session_id| HarnessConversationRef {
            backend: backend.unwrap_or(HarnessConversationBackend::Claude),
            session_id,
        });
        let timestamp = chrono::Utc::now().to_rfc3339();
        // A shut-down host saves nothing; dropping the child stops it.
        if let Err(error) = self.writable_store() {
            drop(terminal);
            self.host.abandon_launch(ticket);
            return Err(error);
        }
        let terminal = self.host.publish_launch(
            ticket,
            LaunchedSession {
                label: child_label.clone(),
                harness,
                conversation,
                objective_delivery,
                timestamp,
            },
            terminal,
        )?;
        if launched_agent {
            // Iced's apply_task_session_signal: a started agent is working.
            if let Err(error) = self.writable_store().and_then(|store| {
                store
                    .record_lifecycle_signal(
                        task_id,
                        TaskLifecycle::Running,
                        None,
                        &chrono::Utc::now().to_rfc3339(),
                    )
                    .map_err(|error| error.to_string())
            }) {
                eprintln!("[tasks] {task_id}: lifecycle not updated after launch: {error}");
            }
        }
        self.launched.insert(
            session_id.clone(),
            Launched {
                task_id: task_id.to_string(),
                provider,
                view: terminal.view.downgrade(),
                target: target.clone(),
                live: launched_agent.then_some(TaskSessionLiveState::Working),
            },
        );
        self.terminals.insert(session_id.clone(), terminal);
        let conversation_id = self
            .host
            .store()
            .as_ref()
            .and_then(|store| store.get(task_id))
            .and_then(|task| {
                task.sessions
                    .iter()
                    .find(|item| item.task_session_id == session_id)
            })
            .and_then(|item| item.conversation.as_ref())
            .map(|conversation| conversation.session_id.clone());
        cx.emit(TerminalLaunched {
            session_id: session_id.clone(),
            title: format!("{child_label} · {}", task.title),
            cwd: worktree.clone(),
            startup_command,
            target,
        });
        cx.notify();
        Ok(Started::Session(serde_json::json!({
            "task_id": task_id,
            "session_id": session_id,
            "label": child_label,
            "workspace": task.workspace.name,
            "worktree_path": task.worktree.path,
            "preset_name": preset_idx.map(|_| child_label.clone()),
            "conversation_id": conversation_id,
            "objective_submitted": false,
        })))
    }

    /// Iced's `promote_queued_tasks`, on the fixture tick: start queued
    /// launches while capacity allows, oldest first. A launch that can no
    /// longer start fails its task rather than sitting Queued.
    pub(super) fn promote_queued_tasks(&mut self, cx: &mut Context<Self>) {
        const LABEL: &str = "task_launch_session";
        if self.host.launch_queue().is_empty() || self.is_shut_down() {
            return;
        }
        let settings = crate::workspace_config::load_iced_settings_from(&self.iced_config);
        for _ in 0..self.host.launch_queue().len() {
            let running = self.slot_holders(cx).len();
            let entry = match self
                .host
                .next_queued_launch(running, settings.max_concurrent_local_tasks)
            {
                Promotion::Stop => break,
                Promotion::Skip => continue,
                Promotion::Start(entry) => entry,
            };
            // Only this host's own tasks are queued; anything else drops out.
            if !self.own_tasks.contains(&entry.task_id) {
                continue;
            }
            let launch = SessionLaunch {
                task_id: entry.task_id.clone(),
                preset_name: entry
                    .preset_idx
                    .and_then(|index| settings.agent_presets.get(index))
                    .map(|preset| preset.name.clone()),
                origin: entry.origin,
                user_model_choice: entry.user_model_choice,
            };
            let facts = self.launch_facts(&settings, cx);
            match self.try_launch(&launch, entry.preset_idx, &settings, facts, LABEL, cx) {
                Ok(_) => {}
                Err(error) => {
                    eprintln!("[tasks] {}: queued launch failed: {error}", entry.task_id);
                    match self.writable_store() {
                        Ok(_) => self.host.fail_queued_launch(&entry.task_id, &error),
                        Err(store) => eprintln!(
                            "[tasks] {}: queued-launch failure not saved: {store}",
                            entry.task_id
                        ),
                    }
                }
            }
        }
    }

    /// Iced's `Action::Shutdown` arm for a task tab: a session that was
    /// observably live ended without reporting an outcome, so it stopped;
    /// the task's other sessions decide its lifecycle with it.
    pub(super) fn terminal_exited(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(launched) = self.launched.get_mut(session_id) else {
            return;
        };
        let task_id = launched.task_id.clone();
        let was_live = launched.live.take().is_some();
        cx.emit(TerminalExited {
            session_id: session_id.to_string(),
        });
        if was_live {
            self.apply_task_session_signal(&task_id, Some((TaskLifecycle::Stopped, None)), cx);
        }
        cx.notify();
    }

    /// Iced's `apply_task_session_signal` over the task's launched
    /// terminals still open and its chat panes, which track no live state.
    pub(super) fn apply_task_session_signal(
        &mut self,
        task_id: &str,
        ended: Option<TaskSessionOutcome>,
        cx: &App,
    ) {
        let Some(store) = self.host.store().as_ref() else {
            return;
        };
        let is_final = |session: Option<&str>| store.worker_session_is_final(task_id, session);
        let terminals = self
            .launched
            .iter()
            .filter(|(_, launched)| {
                launched.task_id == task_id && launched.view.upgrade().is_some()
            })
            .map(|(session, launched)| LinkedSession {
                live: launched.live,
                is_final: is_final(Some(session)),
            });
        let chats = open_chats(cx)
            .into_iter()
            .filter(|chat| chat.task_id.as_deref() == Some(task_id))
            .map(|chat| LinkedSession {
                live: None,
                is_final: is_final(chat.task_session_id.as_deref()),
            });
        let sessions: Vec<LinkedSession> = terminals.chain(chats).collect();
        let Some((target, detail)) = gitterm_core::task_host::task_session_signal(&sessions, ended)
        else {
            return;
        };
        if let Err(error) = self.writable_store().and_then(|store| {
            store
                .record_lifecycle_signal(task_id, target, detail, &chrono::Utc::now().to_rfc3339())
                .map_err(|error| error.to_string())
        }) {
            eprintln!("GitTerm GPUI dropped a task lifecycle signal: {error}");
        }
    }

    /// The reducer's session facts for launched terminals still open, as
    /// Iced's facts list a task tab: its target, terminal turn control and
    /// whether it is observably live.
    pub(super) fn terminal_session_states(&self) -> Vec<session::State> {
        self.launched
            .values()
            .filter(|launched| launched.view.upgrade().is_some())
            .map(|launched| session::State {
                target: launched.target.clone(),
                control: session::TurnControl::Terminal,
                task_live: launched.live.is_some(),
                last_tab: false,
            })
            .collect()
    }

    /// Iced's `worker_reported` for a launched terminal (main.rs:19876):
    /// done clears its live state, blocked leaves it awaiting input.
    pub(super) fn worker_reported_live(&mut self, session_id: &str, done: bool) {
        if let Some(launched) = self.launched.get_mut(session_id) {
            launched.live = (!done).then_some(TaskSessionLiveState::AwaitingInput);
        }
    }

    /// Iced's `TurnEnded` arm for a launched terminal: a still-open terminal
    /// of `task_id` running `session_id` now awaits input. Whether one did.
    pub(super) fn terminal_turn_ended(&mut self, task_id: &str, session_id: &str) -> bool {
        match self.launched.get_mut(session_id) {
            Some(launched) if launched.task_id == task_id && launched.view.upgrade().is_some() => {
                launched.live = Some(TaskSessionLiveState::AwaitingInput);
                true
            }
            _ => false,
        }
    }

    /// Iced's `working_sessions` for launched terminals still open: those
    /// whose agent is working.
    pub(super) fn terminal_working_sessions(&self) -> Vec<String> {
        self.launched
            .values()
            .filter(|launched| launched.view.upgrade().is_some())
            .filter(|launched| matches!(launched.live, Some(TaskSessionLiveState::Working)))
            .map(|launched| launched.target.session_uid.clone())
            .collect()
    }

    /// The whole environment a task terminal's child gets: the fixture
    /// child environment naming `session_id`, with the terminal's own
    /// TERM/COLORTERM. The trust checks read this same list.
    fn terminal_child_env(
        &self,
        fixture: &Path,
        session_id: &str,
    ) -> Result<Vec<(OsString, OsString)>, String> {
        let mut env = self.fixture_child_env(fixture, Some(session_id))?;
        #[cfg(test)]
        if let Some(path) = &self.codex_search_path {
            env.retain(|(name, _)| name != "PATH");
            env.push(("PATH".into(), path.clone()));
        }
        // Every GPUI terminal gets the backend's TERM/COLORTERM (an agent
        // CLI needs TERM), so the child env is the same whatever the parent
        // has.
        env.retain(|(name, _)| !TERMINAL_SET.iter().any(|(set, _)| name == *set));
        env.extend(
            TERMINAL_SET
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into())),
        );
        Ok(env)
    }

    /// Iced's `create_tab_with_session_uid` command steps for a task launch:
    /// the provider's trust check and write against the environment the
    /// child gets (for Claude, the trust file its pinned CLAUDE_CONFIG_DIR
    /// names), then
    /// the guidelines and Claude in Chrome as the task's saved workspace
    /// sets it (Iced's `saved_ws` fallback; off unless switched on).
    fn runtime_command(
        &self,
        command: &str,
        worktree: &Path,
        env: &[(OsString, OsString)],
        task: &TaskRecord,
        cx: &App,
    ) -> Result<String, String> {
        let readable: Vec<(String, String)> = env
            .iter()
            .filter_map(|(name, value)| Some((name.to_str()?.into(), value.to_str()?.into())))
            .collect();
        let home = runs::value_of(env, "HOME")
            .map(PathBuf::from)
            .ok_or("the fixture child has no HOME")?;
        let roots = self.trust_roots(cx);
        let command = command.trim();
        let command = match provider_for_command(command) {
            // Iced's validate_claude_trust_launch refuses any
            // CLAUDE_CONFIG_DIR, and the preview pins one so a fixture child
            // never reads the real ~/.claude: Iced's checks against the file
            // it pins, then Iced's trust write into that file, never outside
            // the preview root.
            WorkerProvider::Claude => {
                let config = gitterm_core::workers::claude_pinned_trust_file(command, &readable)?;
                let config = self.roots.scoped(&config).map_err(|error| {
                    format!("the fixture's Claude trust file is outside the preview: {error}")
                })?;
                if let Some(main) =
                    gitterm_core::workers::claude_task_trust(worktree, &roots, &config)?
                {
                    eprintln!(
                        "[tasks] accepted Claude workspace trust for {} in $CLAUDE_CONFIG_DIR/.claude.json",
                        main.display()
                    );
                }
                command.to_string()
            }
            WorkerProvider::Codex => {
                gitterm_core::workers::validate_codex_trust_home(&home, &readable, |_| None)?;
                match gitterm_core::workers::codex_task_trust(
                    command,
                    worktree,
                    &roots,
                    &home.join(".codex/config.toml"),
                )? {
                    Some(value) => gitterm_core::workers::with_codex_trust(command, &value)?,
                    None => command.to_string(),
                }
            }
            WorkerProvider::Other | WorkerProvider::Antigravity => command.to_string(),
        };
        let command = if gitterm_core::guidelines::starts_a_harness(&command) {
            match self.launch_instructions(
                worktree,
                gitterm_core::guidelines::instruction_file_for_command(&command),
                &task.workspace.name,
                true,
                cx,
            ) {
                Some(guidelines) => {
                    gitterm_core::guidelines::configure_terminal_command(&command, &guidelines)
                }
                None => command,
            }
        } else {
            command
        };
        let chrome =
            saved_workspace(&task.workspace.name, cx).is_some_and(|ws| ws.claude_in_chrome);
        Ok(gitterm_core::harness::claude::configure_terminal_chrome(
            &command, chrome,
        ))
    }

    /// Iced's `launch_instructions`: the change guidelines, the documents
    /// note of a work (TRU-191, from its saved workspace entry, as Iced
    /// reads a workspace that is not open) and, for an agent with the task
    /// tools, the Codex tools note. The note only names the work's folders;
    /// nothing is written there. The guidelines settings and file, and the
    /// combined file, live under the preview root (a missing guidelines
    /// file is written there from the default), so Iced's config dir is
    /// never written.
    pub(super) fn launch_instructions(
        &self,
        cwd: &Path,
        instruction_file: gitterm_core::guidelines::InstructionFile,
        workspace: &str,
        task_tools: bool,
        cx: &App,
    ) -> Option<gitterm_core::guidelines::Guidelines> {
        let saved = saved_workspace(workspace, cx);
        let workspace_off = saved.is_some_and(|ws| ws.guidelines_off);
        let places = saved.and_then(gitterm_core::guidelines::config_document_places);
        let root = self.roots.root();
        gitterm_core::guidelines::launch_instructions(
            gitterm_core::guidelines::for_launch(root, cwd, workspace_off, instruction_file),
            places.as_ref(),
            task_tools,
            root,
        )
    }

    /// Iced's `write_task_brief`, under `<preview root>/task-briefs`.
    fn write_brief(
        &self,
        session_id: &str,
        task: &TaskRecord,
        report_back: Option<&str>,
    ) -> Result<PathBuf, String> {
        let dir = self.roots.root().join(BRIEFS);
        let path = dir.join(format!("{session_id}.md"));
        if path.display().to_string().contains('\'') {
            return Err(format!(
                "brief path {} contains a single quote and cannot be shell-quoted",
                path.display()
            ));
        }
        self.roots
            .create_private_dir(&dir)
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
        let path = self
            .roots
            .scoped(&path)
            .map_err(|error| format!("brief {}: {error}", path.display()))?;
        std::fs::write(
            &path,
            format!(
                "{}\n\nGitTerm task session: {session_id}\n",
                gitterm_core::tasks::task_handoff_prompt(task, report_back),
            ),
        )
        .map_err(|error| format!("write {}: {error}", path.display()))?;
        Ok(path)
    }

    /// Iced's task facts for a launch, from Iced's settings and this host's
    /// sessions.
    pub(super) fn launch_facts(
        &self,
        settings: &crate::workspace_config::IcedSettings,
        cx: &App,
    ) -> TaskMutationFacts {
        TaskMutationFacts {
            sessions: open_chats(cx).iter().map(OpenChat::session_state).collect(),
            presets: settings
                .agent_presets
                .iter()
                .map(|preset| TaskPreset {
                    name: preset.name.clone(),
                    command: preset.command.clone(),
                })
                .collect(),
            slot_holders: self.slot_holders(cx),
            queue: self
                .host
                .launch_queue()
                .iter()
                .map(|entry| entry.task_id.clone())
                .collect(),
            concurrency_limit: settings.max_concurrent_local_tasks,
            directories: self.host.store().as_ref().map_or_else(Vec::new, |store| {
                store
                    .tasks()
                    .iter()
                    .filter_map(|task| task.worktree.path.as_ref())
                    .filter(|path| path.is_dir())
                    .cloned()
                    .collect()
            }),
            ..Default::default()
        }
    }

    /// Iced's `local_slot_holders`: running local tasks with a live session.
    fn slot_holders(&self, cx: &App) -> Vec<String> {
        let Some(store) = self.host.store().as_ref() else {
            return Vec::new();
        };
        let mut holders: Vec<String> = self
            .launched
            .iter()
            .filter(|(session, _)| self.session_live(session, cx))
            .filter_map(|(_, launched)| store.get(&launched.task_id))
            .filter(|task| {
                task.executor == ExecutorTarget::Local && task.lifecycle == TaskLifecycle::Running
            })
            .map(|task| task.task_id.clone())
            .collect();
        holders.sort();
        holders.dedup();
        holders
    }

    /// Whether launched session `session_id` still has a running terminal.
    pub(super) fn session_live(&self, session_id: &str, cx: &App) -> bool {
        self.launched
            .get(session_id)
            .and_then(|launched| launched.view.upgrade())
            .is_some_and(|view| view.read(cx).backend().state() == SessionState::Running)
    }

    /// Live launched sessions spending `provider`, for the plan limits.
    pub(super) fn running_workers(&self, provider: Provider, cx: &App) -> usize {
        self.launched
            .iter()
            .filter(|(session, launched)| {
                launched.provider == Some(provider) && self.session_live(session, cx)
            })
            .count()
    }

    /// The local folders of Iced's workspaces, whose repositories a worker
    /// may be trusted in.
    fn trust_roots(&self, cx: &App) -> Vec<PathBuf> {
        cx.try_global::<crate::work_sidebar::IcedWorkSource>()
            .map_or(&[][..], |source| &source.workspaces[..])
            .iter()
            .filter(|ws| {
                matches!(
                    ws.location,
                    None | Some(
                        gitterm_core::remote::config::WorkspaceLocationConfig::Local { .. }
                    )
                )
            })
            .flat_map(|ws| std::iter::once(&ws.dir).chain(&ws.repos))
            .map(PathBuf::from)
            .collect()
    }

    /// Quit: kill each unadopted terminal's process group, then reap the
    /// killed children on `reap`'s thread with the other runs.
    pub(super) fn stop_terminals(&mut self, cx: &mut App) -> Vec<gitterm_terminal::Backend> {
        self.terminals
            .drain()
            .map(|(_, mut terminal)| {
                terminal.kill();
                let view = terminal.release();
                view.update(cx, |view, _| {
                    std::mem::replace(
                        view.backend_mut(),
                        gitterm_terminal::Backend::new_deferred(Default::default()),
                    )
                })
            })
            .collect()
    }
}

/// Starts the child: `/bin/sh -c <command>` (a shell for a plain
/// terminal) with exactly `env` ([`TaskHostOwner::terminal_child_env`]).
/// Every inherited variable not in `env` is removed, and so are the ones
/// the PTY launcher would add.
fn spawn_terminal(
    cwd: &Path,
    command: Option<String>,
    env: Vec<(OsString, OsString)>,
    owner: gpui::WeakEntity<TaskHostOwner>,
    session_id: &str,
    cx: &mut App,
) -> Result<HostTerminal, String> {
    let mut overlay: HashMap<String, Option<String>> = HashMap::new();
    for (name, value) in env {
        let name = name
            .into_string()
            .map_err(|_| "the fixture child environment has a non-UTF-8 name".to_string())?;
        let value = value
            .into_string()
            .map_err(|_| format!("the fixture child variable {name} is not UTF-8"))?;
        overlay.insert(name, Some(value));
    }
    for (name, _) in std::env::vars_os() {
        let name = name
            .into_string()
            .map_err(|_| "an inherited variable has a non-UTF-8 name".to_string())?;
        overlay.entry(name).or_insert(None);
    }
    for name in LAUNCHER_SET {
        overlay.entry(name.into()).or_insert(None);
    }
    let launch = TerminalLaunch {
        cwd: Some(cwd.to_path_buf()),
        env: overlay,
        program: "/bin/sh".into(),
        args: command.map_or_else(Vec::new, |command| vec!["-c".into(), command]),
        ..Default::default()
    };
    // The pane that adopts the terminal delivers its events, so the exit
    // reaches the host once adopted. Deferred: the host reads the view.
    // shortcut: an unadopted terminal's exit is not seen (nothing delivers
    // its events); deliver them from the host if a launch can stay unadopted.
    let session_id = session_id.to_string();
    let view = cx.new(|cx| {
        TerminalView::launch(launch, Default::default(), cx).with_exit_callback(move |_, cx| {
            let owner = owner.clone();
            let session_id = session_id.clone();
            cx.defer(move |cx| {
                _ = owner.update(cx, |owner, cx| owner.terminal_exited(&session_id, cx));
            });
        })
    });
    let backend = view.read(cx).backend();
    let Some(pid) = backend.child_pid() else {
        let error = backend
            .status_message()
            .unwrap_or_else(|| "the task terminal did not start".into());
        return Err(format!(
            "Prepared {}, but the agent terminal could not start: {error}",
            cwd.display()
        ));
    };
    Ok(HostTerminal {
        view,
        group: Some(pid),
    })
}

/// The saved entry of workspace `name` in Iced's workspaces file.
fn saved_workspace<'a>(
    name: &str,
    cx: &'a App,
) -> Option<&'a gitterm_core::workspace_config::WorkspaceConfig> {
    cx.try_global::<crate::work_sidebar::IcedWorkSource>()?
        .workspaces
        .iter()
        .find(|ws| ws.name == name)
}

/// Iced's stored `startup_command` for a launched task tab (main.rs, after
/// `create_tab_with_session_uid`): a pre-assigned Claude session persists
/// as its resume command, so a restart reconnects instead of creating the
/// session again, and a worker keeps its model, effort and folders; any
/// other launch persists its brief-free command, so a restart never
/// re-submits the objective. A plain terminal has none.
fn persisted_startup_command(
    chat_session_id: Option<&str>,
    worker_model: Option<&str>,
    options: &gitterm_core::tasks::DelegationRunOptions,
    pre_injection: Option<String>,
) -> Option<String> {
    let Some(chat_session_id) = chat_session_id else {
        return pre_injection;
    };
    let model = worker_model
        .map(|model| format!("--model {model} "))
        .unwrap_or_default();
    let resume = format!("claude {model}--resume {chat_session_id}");
    Some(
        gitterm_core::workers::with_launch_options(&resume, options.effort, &options.add_dirs)
            .unwrap_or_else(|error| {
                // The launch command took the same options, so this cannot
                // refuse; Iced keeps the plain resume if it ever does.
                eprintln!("[tasks] worker resume command for {chat_session_id}: {error}");
                resume
            }),
    )
}

/// Iced's `claude_session_exists`, under the fixture child's Claude
/// `projects` dir instead of ~/.claude: Claude writes
/// `<projects>/<cwd-slug>/<id>.jsonl` on the first message. An id that is
/// not a plain name never names a file.
fn claude_transcript_exists(projects: &Path, session_id: &str) -> bool {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(projects).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().join(&file).is_file())
    })
}

/// Iced's `preset_conversation_backend`.
fn preset_backend(name: &str, command: &str) -> Option<HarnessConversationBackend> {
    let name = name.to_lowercase();
    let executable = command
        .split_whitespace()
        .next()
        .and_then(|word| Path::new(word.trim_matches(['\'', '"'])).file_name())
        .map(|word| word.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("claude") || executable == "claude" {
        Some(HarnessConversationBackend::Claude)
    } else if name.contains("codex") || executable == "codex" {
        Some(HarnessConversationBackend::Codex)
    } else if name == "pi" || executable == "pi" {
        Some(HarnessConversationBackend::Pi)
    } else {
        None
    }
}

/// Iced's `command_accepts_initial_prompt`.
fn accepts_initial_prompt(command: &str, backend: Option<HarnessConversationBackend>) -> bool {
    backend.is_some() || provider_for_command(command) == WorkerProvider::Antigravity
}

/// Iced's `command_with_initial_prompt`: only the brief's quote-checked
/// path is embedded.
fn command_with_initial_prompt(command: &str, brief: &Path) -> String {
    let flag = if provider_for_command(command) == WorkerProvider::Antigravity {
        " --prompt-interactive"
    } else {
        ""
    };
    format!("{command}{flag} \"$(cat '{}')\"", brief.display())
}

/// Iced's `with_preassigned_session_id`: a new Claude launch gets a session
/// id, and a preset naming one keeps it.
pub(crate) fn with_preassigned_session_id(command: &str) -> (String, Option<String>) {
    if provider_for_command(command) != WorkerProvider::Claude {
        return (command.to_string(), None);
    }
    match claude_preset_session_id(command) {
        Ok(Some(id)) => (command.to_string(), Some(id)),
        Err(()) => (command.to_string(), None),
        Ok(None) => {
            let id = TaskRecord::fresh_id();
            (format!("{command} --session-id {id}"), Some(id))
        }
    }
}

/// Iced's `claude_preset_session_id`. `Err` means the preset selects a
/// session without a safely extractable id.
fn claude_preset_session_id(command: &str) -> Result<Option<String>, ()> {
    let words = shell_words::split(command).map_err(|_| ())?;
    let mut session_id: Option<String> = None;
    let mut saw_identity_flag = false;
    for (index, word) in words.iter().enumerate().skip(1) {
        if word == "--continue" || word == "-c" {
            return Err(());
        }
        let value = if let Some(value) = word
            .strip_prefix("--resume=")
            .or_else(|| word.strip_prefix("--session-id="))
        {
            saw_identity_flag = true;
            Some(value)
        } else if word == "--resume" || word == "-r" || word == "--session-id" {
            saw_identity_flag = true;
            Some(
                words
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .map(String::as_str)
                    .unwrap_or(""),
            )
        } else {
            None
        };
        if let Some(value) = value {
            if value.is_empty()
                || session_id
                    .as_deref()
                    .is_some_and(|existing| existing != value)
            {
                return Err(());
            }
            session_id = Some(value.to_string());
        }
    }
    if saw_identity_flag {
        session_id.map(Some).ok_or(())
    } else {
        Ok(None)
    }
}

#[cfg(test)]
impl TaskHostOwner {
    /// Stores `delegation` as this host's own worker, as DelegateTask will.
    pub(crate) fn own_worker(&mut self, delegation: gitterm_core::tasks::Delegation) {
        let targets: Vec<PathBuf> = match &delegation.child {
            gitterm_core::tasks::DelegationChild::TaskSession { task_id, .. } => {
                self.task_worktree(task_id)
            }
            _ => Vec::new(),
        };
        let authorized = self
            .authorize_process("task_launch_session", &targets)
            .expect("the worker's task is in a fixture");
        let id = delegation.delegation_id.clone();
        self.writable_store()
            .unwrap()
            .insert_delegation(delegation)
            .unwrap();
        self.own_delegations.insert(id, authorized);
    }
    /// Sessions whose terminal the host still owns.
    pub(crate) fn host_terminals(&self) -> Vec<String> {
        self.terminals.keys().cloned().collect()
    }
    /// The environment a launch of `session` in `worktree` gives its child.
    pub(crate) fn launch_env(&self, worktree: &Path, session: &str) -> Vec<(String, String)> {
        let fixture = self.fixture_of(worktree, "test").unwrap();
        let env = self.terminal_child_env(&fixture, session).unwrap();
        env.into_iter()
            .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
            .collect()
    }
}
