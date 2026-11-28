//! GPUI owns the shared task host on Preview copies or an admitted Live root.
//!
//! Live takes its already-locked TaskStore directly, shares one restricted
//! production service context, and never seeds or reseeds. Startup reconciles
//! saved execution state without adopting its processes; results use core's
//! delivery reducer and the same chat send path as the composer.
//! Live workers use core's Iced terminal command and process identity; headless
//! reviews/consults use core's production Codex environment. A separate worker
//! tick persists identities, enforces time limits, promotes queues and closes
//! final workers after identity-checked TERM/grace.
//! Live creates tasks and saves handoffs through core on the locked V6 store.
//! Live delivers saved and current results; general process restore stays closed.
//!
//! One [`TaskHostOwner`] per preview root: a lifetime host lock is taken
//! before anything is seeded or bound, the task store is a seeded copy of
//! Iced's `tasks.json` (read, never opened for writing), the task, browser
//! and log servers bind OS-assigned loopback ports, and the owner is the only
//! receiver of the task command ingress. Plain preview launches, attaches,
//! notifies and delivers nothing: imported running or queued records stay
//! inert.
//!
//! Reads answer in Iced's shapes from what the preview reads: the seeded
//! store, the shell's panes (pushed as [`TaskPane`]s), the GPUI chat panes,
//! and Iced's settings, plan-limits, merge-plan, PR-watch and chat-facts
//! files (only read). Every write, launch and run answers the preview
//! notice.
//!
//! Fixture mode (H5b B9, tests and acceptance only: [`TaskHostOwner::start_fixture`])
//! gives the host a fixture-launch service context. Then `task_create`,
//! `task_update_handoff`, `merge_plan_set` and turn-end notifications whose
//! target lies in a fixture write the preview's own store and state, and a
//! created task's worktree is prepared inside that fixture.
//!
//! In fixture mode (H5b B10) `review_request` and `consult_request` also run
//! Codex headlessly in a fixture checkout ([`runs`]), and `task_launch_session`
//! starts a task session's terminal for a task this host created
//! ([`launch`]), which the shell adopts into a pane. `delegate_task` creates
//! a task in a fixture repository and launches its worker there
//! ([`delegate`]). Plain preview still answers every launch, run and
//! delegation with the notice and starts nothing.
//!
//! In fixture mode (H5b B11) a finished result of a delegation this host
//! started is delivered into the GPUI chat that requested it, through core's
//! delivery reducer ([`delivery`]). Imported records are never delivered,
//! and plain preview delivers nothing.
use anyhow::{Context as _, Result};
use gitterm_browser::browser_mcp::BrowserMcpConnection;
use gitterm_core::{
    loopback::ListenerBind,
    preview::{
        Preview,
        service::{
            Access, AuthorizedTargets, Class, ServiceContext, classify_operation, preview_notice,
        },
    },
    session,
    task_commands::{
        HandoffUpdate, TaskCreate, TaskCreateOrigin, TaskMutation, TaskMutationAction,
        TaskMutationCommand, TaskMutationFacts,
    },
    task_host::{HostEffect, OpenSession, TaskHost},
    task_mcp::{
        CreateTaskRequest, HandoffStatus, MergePlanSetRequest, TaskControlEnvelope,
        TaskControlReceiver, TaskControlReply, TaskControlSender, TaskMcpConnection,
        UpdateTaskHandoffRequest,
    },
    task_worktree::{PrepareTaskWorktreeRequest, PreparedTaskWorktree, ResolvedTaskPreparation},
    tasks::{StoppingBoundary, TaskLifecycle, TaskRecord, TaskStore},
};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, Task};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

#[cfg(test)]
pub(crate) type RecheckHook = Box<dyn FnOnce(&Path)>;
/// Runs on the owner when a fixture run's continuation named by the
/// string is reached, before its shutdown guard.
#[cfg(test)]
pub(crate) type ContinuationHook = (&'static str, Box<dyn FnOnce(&mut TaskHostOwner)>);
/// Runs on the owner after a fixture launch's child started, before it is
/// published.
#[cfg(test)]
pub(crate) type PublishHook = Box<dyn FnOnce(&mut TaskHostOwner)>;
#[cfg(test)]
pub(crate) type TerminalSpecHook =
    Box<dyn Fn(&mut gitterm_core::task_host::command::TerminalCommand)>;

#[path = "task_host_delegate.rs"]
mod delegate;
#[path = "task_host_delivery.rs"]
mod delivery;
#[path = "task_host_launch.rs"]
mod launch;
#[path = "task_host_runs.rs"]
mod runs;
pub(crate) use delivery::forward_turn_ends;
#[cfg(test)]
pub(crate) use delivery::{DELIVERY_POLL, SubmitHook};
#[cfg(test)]
pub(crate) use launch::with_preassigned_session_id as launch_session_id;
pub(crate) use launch::{FinalWorkerClose, TerminalExited, TerminalLaunched};

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const SEED_STAGING: &str = ".tasks.seed.json";
const RESEED_POLL: Duration = Duration::from_secs(2);
/// Bounds quit: the servers were cancelled already; this only lets their
/// graceful shutdown finish before the runtime is dropped.
const SERVER_DRAIN: Duration = Duration::from_millis(500);
/// Bounds quit: a stopped fixture run kills its Codex process tree and
/// reaps it; this only waits for that.
const RUN_STOP: Duration = Duration::from_secs(3);
const SHUT_DOWN: &str = "GitTerm's task host has shut down";
const STORE_UNAVAILABLE: &str = "GitTerm's task store is unavailable";
/// A fixture create's worktrees go here, inside the repository's fixture.
const FIXTURE_WORKTREES: &str = "worktrees";

/// The app's task host, for B6 chat descriptors and B8 panes.
pub(crate) struct GpuiTaskHost(pub(crate) Entity<TaskHostOwner>);
impl Global for GpuiTaskHost {}

/// Session teardown must precede accepting another task write from this caller.
pub(crate) fn revoke_caller(caller: &str, cx: &gpui::App) {
    if let Some(host) = cx.try_global::<GpuiTaskHost>() {
        host.0.read(cx).host.revoke_caller(caller);
    }
}

/// Where commands come from. Production takes the process ingress; tests
/// pass their own channel, because the process receiver is take-once.
pub(crate) enum Ingress {
    Process,
    #[cfg(test)]
    Channel(TaskControlSender, TaskControlReceiver),
}

type ProcessStream = Pin<Box<dyn tokio_stream::Stream<Item = TaskControlEnvelope> + Send>>;
enum Commands {
    Process(ProcessStream),
    #[cfg_attr(not(test), allow(dead_code))]
    Channel(TaskControlReceiver),
}
impl Commands {
    async fn next(&mut self) -> Option<TaskControlEnvelope> {
        use tokio_stream::StreamExt as _;
        match self {
            Self::Process(stream) => stream.next().await,
            Self::Channel(receiver) => receiver.recv().await,
        }
    }
}
impl Ingress {
    fn open(self) -> (TaskControlSender, Commands) {
        match self {
            // The only caller of command_stream in the GPUI process.
            Self::Process => (
                gitterm_core::task_mcp::command_sender(),
                Commands::Process(Box::pin(gitterm_core::task_host::command_stream())),
            ),
            #[cfg(test)]
            Self::Channel(sender, receiver) => (sender, Commands::Channel(receiver)),
        }
    }
}

pub(crate) struct HostConfig {
    pub roots: Arc<Preview>,
    /// Iced's config dir, found the way `data.rs` reads it. Preview seeds
    /// from it; Live writes tasks under the service context's locked root.
    pub iced_config: PathBuf,
    pub ingress: Ingress,
}
impl HostConfig {
    pub(crate) fn production(roots: Arc<Preview>) -> Self {
        Self {
            roots,
            iced_config: crate::workspace_config::iced_config_dir(),
            ingress: Ingress::Process,
        }
    }
}

pub(crate) use gitterm_core::state_lock::StateLock as HostLock;

fn direct_store(lock: &HostLock) -> Result<TaskStore> {
    gitterm_core::task_host::load_task_store_for_startup(
        TaskStore::path_for_config_root(lock.root()),
        &chrono::Utc::now().to_rfc3339(),
    )
    .map(|(store, _)| store)
    .context("load and reconcile Live task store directly")
}

/// Iced's file identity for re-seeding: its mtime, `None` while missing.
type Stamp = Option<SystemTime>;
fn source_stamp(source: &Path) -> std::io::Result<Stamp> {
    match std::fs::metadata(source) {
        Ok(meta) => meta.modified().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Copies Iced's store into `<preview root>/tasks.json` and loads it from
/// there. The source is only read; the copy replaces the last good snapshot
/// only after it validates.
pub(crate) fn seed_copy(roots: &Preview, source: &Path) -> Result<(TaskStore, Stamp)> {
    let target = roots.scoped(&TaskStore::path_for_config_root(roots.root()))?;
    let stamp =
        source_stamp(source).with_context(|| format!("stat Iced tasks {}", source.display()))?;
    let bytes = match std::fs::read(source) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("read Iced tasks {}", source.display()));
        }
    };
    match bytes {
        // No Iced store yet: the preview's copy is empty too.
        None => match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clear {}", target.display()));
            }
        },
        Some(bytes) => {
            let staged = roots.scoped(&roots.root().join(SEED_STAGING))?;
            let validated = gitterm_core::atomic_file::write_atomically(&staged, &bytes)
                .map_err(anyhow::Error::msg)
                .and_then(|()| TaskStore::load(&staged).map_err(anyhow::Error::from))
                .and_then(|_| {
                    std::fs::rename(&staged, &target)
                        .with_context(|| format!("install {}", target.display()))
                });
            if let Err(error) = validated {
                let _ = std::fs::remove_file(&staged);
                return Err(error.context(format!("seed from {}", source.display())));
            }
        }
    }
    Ok((TaskStore::load(&target)?, stamp))
}

/// Anything `replace_store` would carry across a new store or drop with the
/// old one. Plain preview starts none of it; fixture mode does.
pub(crate) fn owns_in_flight_work(host: &TaskHost) -> bool {
    !host.launch_queue().is_empty()
        || host.delegations().busy()
        || host.delegations().held_tabs().next().is_some()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reseed {
    Unchanged,
    Reseeded,
    Deferred,
    Failed,
}

struct Servers {
    task_mcp: TaskMcpConnection,
    browser: BrowserMcpConnection,
    log: gitterm_core::log_server::ServerState,
    jobs: Vec<tokio::task::JoinHandle<()>>,
    limits_jobs: Vec<tokio::task::JoinHandle<()>>,
    runtime: tokio::runtime::Runtime,
}

/// One shell pane holding a task session, as Iced's `task_control_value`
/// lists a tab of the task (src/main.rs `task_control_value`). The shell
/// pushes every such pane in Iced's tab order ([`TaskHostOwner::set_panes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskPane {
    pub task_id: String,
    pub session: OpenSession,
    /// The folder the pane's session runs in. Iced's tab label becomes that
    /// folder's git root name once its status polls; see
    /// [`TaskHostOwner::set_panes`].
    pub folder: Option<PathBuf>,
}

pub(crate) struct TaskHostOwner {
    host: TaskHost,
    /// The shell's panes holding task sessions, as last pushed.
    panes: Vec<TaskPane>,
    pub(crate) terminal_callers: Vec<gitterm_core::delegation_state::Caller>,
    /// Iced's steady-state tab label for each pane folder: `None` while its
    /// background read runs. Held only while a pane has the folder.
    pub(crate) folder_labels: std::collections::HashMap<PathBuf, Option<String>>,
    roots: Arc<Preview>,
    /// Iced's config dir; never written.
    iced_config: PathBuf,
    /// Iced's `tasks.json` in it.
    source: PathBuf,
    stamp: Stamp,
    /// The latest seed problem, shown until a seed succeeds.
    pub(crate) notice: Option<String>,
    servers: Option<Servers>,
    /// Dropped after the servers and their runtime on quit.
    lock: Option<Arc<HostLock>>,
    reaped: Option<std::sync::mpsc::Receiver<Result<()>>>,
    reap_reply: Option<std::sync::mpsc::Sender<Result<()>>>,
    /// Canonical fixture roots; empty in plain preview.
    fixtures: Vec<PathBuf>,
    /// Creates between their first check and their last save.
    creating: usize,
    /// The preview store holds the host's own writes, which a re-seed from
    /// Iced's file would discard.
    wrote: bool,
    /// Fixture Codex runs in flight, by delegation id.
    runs: std::collections::HashMap<String, runs::RunHandle>,
    /// Delegations this host stored, with their authorized targets. Preview
    /// starts or settles only these; Live can explicitly relaunch saved workers.
    own_delegations: std::collections::HashMap<String, AuthorizedTargets>,
    /// Live observes and saves limits through Iced's shared state/jobs.
    plan_limits: Option<gitterm_core::limits::PlanLimitsState>,
    /// Tasks this host created, plus saved tasks admitted at Live startup.
    /// Admission never imports process ownership or starts a session.
    own_tasks: std::collections::HashSet<String>,
    /// Finished results waiting for a closed chat, by its session uid, as of
    /// the last delivery tick; their log line repeats only when this changes.
    pub(crate) delegation_waiting_counts: std::collections::HashMap<String, usize>,
    /// Launched terminals no pane has adopted yet, by task session.
    terminals: std::collections::HashMap<String, launch::HostTerminal>,
    /// Conversation ownership includes terminals already adopted by a pane.
    terminal_conversations:
        std::collections::HashMap<String, gpui::WeakEntity<gitterm_terminal::TerminalView>>,
    /// Every launched task session, adopted or not.
    launched: std::collections::HashMap<String, launch::Launched>,
    /// Runs on a prepared worktree path before its post-preparation recheck.
    #[cfg(test)]
    pub(crate) before_recheck: Option<RecheckHook>,
    /// Runs on a run's checkout before its post-check recheck.
    #[cfg(test)]
    pub(crate) before_run_recheck: std::cell::RefCell<Option<RecheckHook>>,
    /// Where a fixture run finds `codex`, in place of the child's PATH.
    #[cfg(test)]
    pub(crate) codex_search_path: Option<std::ffi::OsString>,
    #[cfg(test)]
    pub(crate) before_continuation: Option<ContinuationHook>,
    #[cfg(test)]
    pub(crate) before_publish: Option<PublishHook>,
    #[cfg(test)]
    pub(crate) terminal_spec_hook: Option<TerminalSpecHook>,
    /// Replaces writing a delivery into its chat pane.
    #[cfg(test)]
    pub(crate) submit_hook: Option<SubmitHook>,
    _pump: Task<()>,
    _reseed: Option<Task<()>>,
    /// Live/fixture delivery tick; fixture mode also promotes its queue.
    /// None in plain Preview.
    _deliver: Option<Task<()>>,
    _workers: Option<Task<()>>,
    /// Failed publications must finish their TERM/KILL grace even at quit.
    unpublished_stops: Vec<std::thread::JoinHandle<Result<(), String>>>,
    _quit: Subscription,
}

impl TaskHostOwner {
    /// Read the existing Live allowance state for the Usage rail projection.
    pub(crate) fn chrome_plan_limits(&self) -> Option<&gitterm_core::plan_limits::PlanLimitsFile> {
        self.plan_limits.as_ref().map(|state| state.file())
    }

    /// Lock, seed, bind, then create the entity. Startup errors fail closed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn start(config: HostConfig, cx: &mut App) -> Result<Entity<Self>> {
        Self::start_with(config, Vec::new(), None, None, cx)
    }

    pub(crate) fn start_locked(
        config: HostConfig,
        lock: Arc<HostLock>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        Self::start_with(config, Vec::new(), Some(lock), None, cx)
    }

    pub(crate) fn start_live(
        config: HostConfig,
        lock: Arc<HostLock>,
        services: Arc<ServiceContext>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        Self::start_with(config, Vec::new(), Some(lock), Some(services), cx)
    }

    pub(super) fn is_live(&self) -> bool {
        !self.host.services().is_preview()
    }

    fn launches(&self) -> bool {
        self.is_live() || !self.fixtures.is_empty()
    }

    pub(super) fn process_root(&self) -> &Path {
        if self.is_live() {
            self.host.services().state_root()
        } else {
            self.roots.root()
        }
    }

    /// [`Self::start`] with a fixture-launch context over `fixtures` (each a
    /// marked directory under the temp dir, as `ServiceContext::fixture_launch`
    /// checks). Tests and acceptance only: normal startup never calls it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn start_fixture(
        config: HostConfig,
        fixtures: Vec<PathBuf>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        anyhow::ensure!(!fixtures.is_empty(), "fixture mode needs a fixture root");
        Self::start_with(config, fixtures, None, None, cx)
    }

    fn start_with(
        config: HostConfig,
        fixtures: Vec<PathBuf>,
        lock: Option<Arc<HostLock>>,
        live_services: Option<Arc<ServiceContext>>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let HostConfig {
            roots,
            iced_config,
            ingress,
        } = config;
        let live = live_services.is_some();
        let iced_tasks = if live {
            live_services
                .as_ref()
                .unwrap()
                .state_root()
                .join("tasks.json")
        } else {
            TaskStore::path_for_config_root(&iced_config)
        };
        let (lock, store, stamp, services) = if let Some(services) = live_services {
            let lock = lock.context("Live task host requires the admitted V6 lease")?;
            anyhow::ensure!(
                lock.root() == services.state_root(),
                "Live task host root differs from locked root"
            );
            let store = direct_store(&lock)?;
            (lock, store, None, services)
        } else {
            roots.create_private_dir(roots.root())?;
            roots.scoped(&roots.root().join(gitterm_core::state_lock::LOCK_FILE))?;
            let lock = match lock {
                Some(lock) => {
                    anyhow::ensure!(
                        lock.root() == roots.root(),
                        "host lock root differs from preview root"
                    );
                    lock
                }
                None => Arc::new(HostLock::acquire(roots.root())?),
            };
            let (store, stamp) = seed_copy(&roots, &iced_tasks)?;
            let services = if fixtures.is_empty() {
                ServiceContext::preview(roots.clone())
            } else {
                ServiceContext::fixture_launch(roots.clone(), fixtures.iter().cloned())
            }
            .context("preview task services")?;
            (lock, store, stamp, Arc::new(services))
        };
        // fixture_launch accepted each root as it resolves.
        let fixtures = fixtures
            .iter()
            .map(std::fs::canonicalize)
            .collect::<std::io::Result<Vec<_>>>()
            .context("resolve fixture roots")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("create GPUI task host runtime")?;
        let (sender, commands) = ingress.open();
        let html = gitterm_browser::browser_control::html_preview_runner_in(&services);
        let (task_mcp, task_server) = if live {
            gitterm_core::task_mcp::prepare(
                services.task_mcp().instance_id(),
                sender.clone(),
                html,
                SERVER_VERSION,
            )
        } else {
            gitterm_core::task_mcp::prepare_on(
                ListenerBind::LoopbackEphemeral,
                services.task_mcp().token().to_string(),
                sender.clone(),
                html,
                SERVER_VERSION,
            )
        }
        .context("bind GPUI task MCP")?;
        let (browser, browser_server) = {
            let _entered = runtime.enter();
            if live {
                gitterm_browser::browser_mcp::prepare_production_scoped(
                    services.clone(),
                    SERVER_VERSION,
                )
            } else {
                gitterm_browser::browser_mcp::prepare_scoped(
                    services.clone(),
                    ListenerBind::LoopbackEphemeral,
                    SERVER_VERSION,
                )
            }
            .context("bind GPUI browser MCP")?
        };
        let log = gitterm_core::log_server::ServerState::new();
        // Establish the log listener before publishing the host or starting MCP
        // dispatch. Production retains its existing V6 port selection.
        let log_job = runtime.spawn({
            let log = log.clone();
            let instance = services.task_mcp().instance_id().to_owned();
            async move {
                if live {
                    gitterm_core::log_server::start_server(log, &instance).await;
                } else if let Err(error) =
                    gitterm_core::log_server::start_server_on(log, ListenerBind::LoopbackEphemeral)
                        .await
                {
                    eprintln!("preview log server: {error}");
                }
            }
        });
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if log.bound_port.lock().unwrap().is_some() {
                        return Ok(());
                    }
                    anyhow::ensure!(
                        !log_job.is_finished(),
                        "GPUI log listener failed before startup"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .context("bind GPUI log listener timed out")?
        })?;
        let jobs = vec![
            log_job,
            runtime.spawn(async move {
                if let Err(error) = task_server.run().await {
                    eprintln!("{error}");
                }
            }),
            runtime.spawn(async move {
                if let Err(error) = browser_server.run().await {
                    eprintln!("{error}");
                }
            }),
        ];
        let (reap_reply, reaped) = std::sync::mpsc::channel();
        let barrier = cx.try_global::<crate::lifetime::Barrier>().cloned();
        let reaped = if let Some(barrier) = barrier {
            barrier.register("task-host-reaper", move || {
                reaped
                    .recv_timeout(crate::lifetime::DRAIN_TIMEOUT)
                    .context("shutdown phase task-host-reaper: timeout or worker stopped")?
            });
            None
        } else {
            Some(reaped)
        };
        let own_tasks = if live {
            store
                .tasks()
                .iter()
                .map(|task| task.task_id.clone())
                .collect()
        } else {
            Default::default()
        };
        let host = TaskHost::new(Some(store), (*services).clone(), sender);
        Ok(cx.new(|cx| {
            let pump = cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                let mut commands = commands;
                while let Some(envelope) = commands.next().await {
                    if owner
                        .update(cx, |owner, cx| owner.execute(envelope, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let reseed = (!live).then(|| {
                cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                    loop {
                        cx.background_executor().timer(RESEED_POLL).await;
                        if owner
                            .update(cx, |owner, cx| {
                                owner.refresh_seed(cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
            });
            let deliver = (live || !fixtures.is_empty()).then(|| {
                cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                    loop {
                        cx.background_executor()
                            .timer(delivery::DELIVERY_POLL)
                            .await;
                        // Iced's Tick closes final workers after delivery,
                        // and starts queued launches.
                        if owner
                            .update(cx, |owner, cx| {
                                owner.deliver_results(cx);
                                if !owner.is_live() {
                                    owner.close_final_workers(cx);
                                    owner.promote_queued_tasks(cx);
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
            });
            let workers = live.then(|| {
                cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        if owner
                            .update(cx, |owner, cx| owner.live_worker_tick(cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
            });
            // Synchronous: GPUI's quit timeout starts after the callbacks
            // return. Registered before the workspace owner's chat drain.
            let quit = cx.on_app_quit(|owner: &mut Self, cx| {
                owner.quit(cx);
                std::future::ready(())
            });
            Self {
                host,
                panes: Vec::new(),
                terminal_callers: Vec::new(),
                folder_labels: std::collections::HashMap::new(),
                roots,
                plan_limits: live
                    .then(|| gitterm_core::limits::PlanLimitsState::load(&iced_config)),
                iced_config,
                source: iced_tasks,
                stamp,
                notice: None,
                servers: Some(Servers {
                    task_mcp,
                    browser,
                    log,
                    jobs,
                    limits_jobs: Vec::new(),
                    runtime,
                }),
                lock: Some(lock),
                reaped,
                reap_reply: Some(reap_reply),
                fixtures,
                creating: 0,
                wrote: false,
                runs: Default::default(),
                own_delegations: Default::default(),
                own_tasks,
                delegation_waiting_counts: Default::default(),
                terminals: Default::default(),
                terminal_conversations: Default::default(),
                launched: Default::default(),
                #[cfg(test)]
                before_recheck: None,
                #[cfg(test)]
                before_run_recheck: Default::default(),
                #[cfg(test)]
                codex_search_path: None,
                #[cfg(test)]
                before_continuation: None,
                #[cfg(test)]
                before_publish: None,
                #[cfg(test)]
                terminal_spec_hook: None,
                #[cfg(test)]
                submit_hook: None,
                _pump: pump,
                _reseed: reseed,
                _deliver: deliver,
                _workers: workers,
                unpublished_stops: Vec::new(),
                _quit: quit,
            }
        }))
    }

    /// The task and browser servers for one native Claude chat, attributed
    /// to `caller` (the chat's `session_uid`); none once the servers have
    /// shut down.
    pub(crate) fn claude_mcp_servers(
        &self,
        caller: &str,
    ) -> Vec<gitterm_core::harness::claude::ClaudeMcpServer> {
        self.servers.as_ref().map_or_else(Vec::new, |servers| {
            self.host.issue_caller(caller);
            vec![
                servers.task_mcp.claude_mcp_server(Some(caller)),
                servers.browser.claude_mcp_server(Some(caller)),
            ]
        })
    }

    pub(crate) fn host(&self) -> &TaskHost {
        &self.host
    }
    /// The shell's panes holding task sessions; the shell pushes them when
    /// they change. A pane keeps its title as `open_sessions` label until
    /// the read of its folder's label returns, as Iced's tab keeps the launch
    /// title until its first status poll. The read runs once per folder
    /// while a pane holds it (a `git init` in a held folder shows after the
    /// pane closes), off the UI thread.
    pub(crate) fn set_panes(&mut self, panes: Vec<TaskPane>, cx: &mut Context<Self>) {
        let held: std::collections::HashSet<&PathBuf> = panes
            .iter()
            .filter_map(|pane| pane.folder.as_ref())
            .collect();
        self.folder_labels.retain(|folder, _| held.contains(folder));
        for folder in held {
            if self.folder_labels.contains_key(folder) {
                continue;
            }
            self.folder_labels.insert(folder.clone(), None);
            let folder = folder.clone();
            cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                let read = folder.clone();
                let label = cx
                    .background_executor()
                    .spawn(async move { folder_label(&read) })
                    .await;
                let _ = owner.update(cx, |owner, _| {
                    // Gone if every pane in the folder closed meanwhile.
                    if let Some(slot) = owner.folder_labels.get_mut(&folder) {
                        *slot = Some(label);
                    }
                });
            })
            .detach();
        }
        self.panes = panes;
    }
    /// The panes as `open_sessions` lists them: each labelled with its
    /// folder's name once that is read, else with its title.
    pub(crate) fn labelled_panes(&self) -> Vec<TaskPane> {
        self.panes
            .iter()
            .map(|pane| {
                let mut pane = pane.clone();
                if let Some(Some(label)) =
                    pane.folder.as_ref().and_then(|f| self.folder_labels.get(f))
                {
                    pane.session.label = label.clone();
                }
                pane
            })
            .collect()
    }
    pub(crate) fn panes(&self) -> &[TaskPane] {
        &self.panes
    }
    pub(crate) fn host_mut(&mut self) -> &mut TaskHost {
        &mut self.host
    }
    #[cfg(test)]
    pub(crate) fn task_endpoint(&self) -> Option<String> {
        self.servers
            .as_ref()
            .map(|servers| servers.task_mcp.endpoint().to_string())
    }
    #[cfg(test)]
    pub(crate) fn server_tasks(&self) -> Vec<tokio::task::AbortHandle> {
        self.servers
            .as_ref()
            .unwrap()
            .jobs
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect()
    }
    #[cfg(test)]
    pub(crate) fn log_port(&self) -> Option<u16> {
        let servers = self.servers.as_ref()?;
        *servers.log.bound_port.lock().unwrap()
    }
    #[cfg(test)]
    pub(crate) fn browser_endpoint(&self) -> Option<String> {
        self.servers
            .as_ref()
            .map(|servers| servers.browser.endpoint().to_string())
    }

    /// The gate resolves targets from the store and the calling pane: its
    /// task, and its directory for `merge_plan_set`.
    fn execute(&mut self, envelope: TaskControlEnvelope, cx: &mut Context<Self>) {
        let label = classify_operation(&envelope.operation).label;
        let caller = envelope.caller.as_deref().and_then(|uid| {
            open_chats(cx)
                .into_iter()
                .find(|chat| chat.session_uid == uid)
        });
        let effect = self.host.dispatch_with_caller(
            envelope,
            caller.as_ref().and_then(|chat| chat.task_id.as_deref()),
            caller.as_ref().map(|chat| chat.cwd.as_path()),
        );
        if let Some(effect) = effect {
            self.execute_effect(effect, label, cx);
        }
    }

    /// The one executor for host effects; `label` is the operation's. Reads
    /// answer as Iced's arms do. The preview gate denies writes, launches and
    /// runs before dispatch. The store-write arms check their targets again
    /// themselves, so in plain preview they answer the same notice and do
    /// nothing else; in fixture mode they write the preview's own store and
    /// state. Review and consult run Codex in a fixture checkout in fixture
    /// mode (B10), a launch starts a task session's terminal (B10b), and
    /// `delegate_task` creates a task and launches its worker (B10c).
    pub(crate) fn execute_effect(
        &mut self,
        effect: HostEffect,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        match effect {
            // The plan-limits file is read on the background executor, which
            // sends the reply, so the UI thread never waits on it.
            HostEffect::PlanLimits { reply } if self.is_live() => {
                reply.send(
                    serde_json::to_value(self.policy().report)
                        .map_err(|error| format!("cannot encode plan_limits result: {error}")),
                );
            }
            HostEffect::PlanLimits { reply } => {
                let iced_config = self.iced_config.clone();
                cx.background_spawn(async move { reply.send(plan_limits(&iced_config)) })
                    .detach();
            }
            HostEffect::HtmlRender {
                reply,
                caller,
                request,
            } => reply.send(html_render(request, caller.as_deref(), &open_chats(cx))),
            // Iced's settings file is read on the background executor; the
            // panes are read back on the UI thread, which sends the reply.
            HostEffect::Read { reply, effect } => {
                let iced_config = self.iced_config.clone();
                cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
                    let presets = cx
                        .background_spawn(async move { preset_names(&iced_config) })
                        .await;
                    let _ = owner.update(cx, |owner, _| {
                        reply.send(read_projection(effect, &owner.labelled_panes(), &presets));
                    });
                })
                .detach();
            }
            // Iced adds the live worker card; the preview runs no workers.
            HostEffect::DelegationRecord { reply, record, .. } => reply.send(Ok(record)),
            HostEffect::MergePlanGet { reply, caller } => {
                self.merge_plan_get(reply, caller.as_deref(), cx)
            }
            HostEffect::Create {
                reply,
                caller,
                request,
            } => self.create(reply, caller, request, label, cx),
            HostEffect::UpdateHandoff {
                reply,
                caller,
                request,
            } => {
                reply.send(self.update_handoff(request, caller.as_deref(), label, cx));
                cx.notify();
            }
            HostEffect::TurnEnded {
                reply,
                task_id,
                session_id,
            } => {
                reply.send(self.turn_ended(&task_id, &session_id, label, cx));
                cx.notify();
            }
            HostEffect::MergePlanSet {
                reply,
                caller,
                request,
            } => reply.send(self.merge_plan_set(caller.as_deref(), request, label, cx)),
            HostEffect::RequestReview {
                reply,
                caller,
                request,
            } if self.launches() => self.request_codex_run(
                reply,
                caller,
                runs::CodexRequest::Review(request),
                label,
                cx,
            ),
            HostEffect::RequestConsult {
                reply,
                caller,
                request,
            } if self.launches() => self.request_codex_run(
                reply,
                caller,
                runs::CodexRequest::Consult(request),
                label,
                cx,
            ),
            HostEffect::LaunchSession { reply, launch } if self.launches() => {
                self.launch_session(reply, launch, label, cx)
            }
            HostEffect::DelegateTask {
                reply,
                caller,
                request,
            } if self.launches() => self.delegate_task(reply, caller, request, label, cx),
            HostEffect::LaunchSession { reply, .. }
            | HostEffect::DelegateTask { reply, .. }
            | HostEffect::RequestReview { reply, .. }
            | HostEffect::RequestConsult { reply, .. } => reply.send(Err(preview_notice(label))),
        }
    }

    /// The gate's write decision for `targets`; no target is denied in both
    /// modes. In plain preview every write is denied.
    fn authorize_write(
        &self,
        label: &'static str,
        targets: &[PathBuf],
    ) -> Result<AuthorizedTargets, String> {
        let class = Class {
            label,
            access: Access::Write,
        };
        self.host
            .services()
            .authorize_targets(class, targets)
            .map_err(|denied| denied.notice().to_string())
    }

    fn task_worktree(&self, task_id: &str) -> Vec<PathBuf> {
        self.host
            .store()
            .as_ref()
            .and_then(|store| store.get(task_id)?.worktree.path.clone())
            .into_iter()
            .collect()
    }

    /// Live writes only the locked V6 store. Preview re-scopes its copy so
    /// a swapped link cannot redirect the save, and marks it as the host's
    /// own so no re-seed discards a fixture write.
    fn writable_store(&mut self) -> Result<&mut TaskStore, String> {
        // The host lock is released at shutdown; nothing is saved after it.
        if self.is_shut_down() {
            return Err(SHUT_DOWN.to_string());
        }
        let preview = self.host.services().is_preview();
        let store = self.host.store_mut().as_mut().ok_or(STORE_UNAVAILABLE)?;
        if preview {
            self.roots
                .scoped(store.path())
                .map_err(|error| format!("task store {}: {error}", store.path().display()))?;
        } else {
            let lock = self.lock.as_ref().ok_or(SHUT_DOWN)?;
            if store.path() != TaskStore::path_for_config_root(lock.root()) {
                return Err("Live task store differs from locked root".into());
            }
        }
        self.wrote = true;
        Ok(store)
    }

    /// Iced's `update_handoff_requested`, on the preview's store: the core
    /// reducer and `record_handoff`, so done/blocked for an open worker
    /// delegation is accepted only from the pane (a chat or a launched
    /// terminal) hosting its task session, then the task's lifecycle
    /// mirrors it. Live also raises the parent's notification attention;
    /// PR-lookup remains separate. The report reaches the
    /// parent chat through the delivery tick ([`delivery`]), as in Iced.
    fn update_handoff(
        &mut self,
        request: UpdateTaskHandoffRequest,
        caller: Option<&str>,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        self.authorize_write(label, &self.task_worktree(&request.task_id))?;
        if !self.host.services().is_preview() {
            self.host.require_issued_caller(caller, label)?;
        }
        let chats = open_chats(cx);
        // A launched terminal's report is its task session's, as Iced's
        // facts list a task tab.
        let facts = TaskMutationFacts {
            sessions: chats
                .iter()
                .map(OpenChat::session_state)
                .chain(self.terminal_session_states())
                .collect(),
            ..Default::default()
        };
        let timestamp = chrono::Utc::now().to_rfc3339();
        let task_id = request.task_id.clone();
        let mutation = TaskMutation::UpdateHandoff(HandoffUpdate {
            task_id: request.task_id,
            summary: request.summary,
            decisions: request.decisions,
            next_steps: request.next_steps,
            blockers: request.blockers,
            session_id: request.session_id,
            caller: caller.map(str::to_string),
            status: request.status.unwrap_or_default(),
            timestamp: timestamp.clone(),
        });
        let TaskMutationAction::PersistHandoff {
            handoff,
            status,
            hosted_session,
            ..
        } = self.reduce(mutation, &facts)?
        else {
            return Err("task_update_handoff: the reducer did not persist a handoff".into());
        };
        let store = self.writable_store()?;
        let report_to = gitterm_core::delegations::record_handoff(
            store,
            &task_id,
            handoff,
            status,
            hosted_session.as_deref(),
            &timestamp,
        )?;
        let delegation_status = report_to.as_deref().and_then(|delegation_id| {
            store
                .delegation(delegation_id)
                .map(|delegation| delegation.status.label())
        });
        if let Some(delegation_id) = &report_to {
            // Iced's worker_reported: the task mirrors the worker's report.
            let lifecycle = if status == HandoffStatus::Done {
                TaskLifecycle::Completed
            } else {
                TaskLifecycle::WaitingForInput
            };
            if let Err(error) = store.record_lifecycle_signal(
                &task_id,
                lifecycle,
                None,
                &chrono::Utc::now().to_rfc3339(),
            ) {
                eprintln!("[delegation] {delegation_id}: task lifecycle not updated: {error}");
            }
        }
        let task = store
            .get(&task_id)
            .cloned()
            .ok_or_else(|| format!("Task {task_id} disappeared after updating its handoff"))?;
        if report_to.is_some() {
            // Iced's worker_reported, tab part: the reporting terminal waits
            // for input after `blocked` and claims nothing after `done`, so
            // its checkout can be delegated into again.
            if let Some(session) = hosted_session.as_deref() {
                self.worker_reported_live(session, status == HandoffStatus::Done);
            }
        }
        if self.is_live()
            && let Some(parent) = report_to.as_deref().and_then(|id| {
                self.host
                    .store()
                    .as_ref()?
                    .delegation(id)
                    .map(|d| d.parent.session_uid.clone())
            })
        {
            cx.defer(move |cx| crate::notifications::worker_reported(&parent, status, cx));
        }
        let mut value = self.task_value(&task)?;
        value["handoff_status"] = serde_json::Value::String(status.label().to_string());
        value["delegation_id"] = report_to
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        value["delegation_status"] = delegation_status
            .map(|label| serde_json::Value::String(label.to_string()))
            .unwrap_or(serde_json::Value::Null);
        if status != HandoffStatus::Progress && value["delegation_id"].is_null() {
            value["detail"] = serde_json::Value::String(
                "No worker delegation is open on this task, so nobody was notified; the handoff \
                 is saved on the task."
                    .to_string(),
            );
        }
        Ok(value)
    }

    /// Iced's `TurnEnded` arm, store part only: a chat pane or launched
    /// terminal hosting the session now waits for input, so the task does,
    /// unless the session is a worker's final one. A terminal carries live
    /// state, so the task's sessions decide the lifecycle together; chat
    /// panes carry none, so the ended session decides. Delivery waits for
    /// the next delivery tick ([`delivery`]).
    fn turn_ended(
        &mut self,
        task_id: &str,
        session_id: &str,
        label: &'static str,
        cx: &App,
    ) -> Result<serde_json::Value, String> {
        let label = if self.is_live() {
            if !self.owns_live_worker_session(task_id, session_id) {
                return Err("notify does not name an open Live worker session".into());
            }
            "Worker notify"
        } else {
            label
        };
        self.authorize_write(label, &self.task_worktree(task_id))?;
        // Iced checks every tab, terminals included: a launched terminal of
        // the task's session sets its live state and the task's sessions
        // decide the lifecycle (`apply_task_session_signal`).
        let terminal = self.terminal_turn_ended(task_id, session_id);
        if terminal {
            self.apply_task_session_signal(task_id, None, cx);
        }
        let hosted = terminal
            || open_chats(cx).iter().any(|chat| {
                chat.task_id.as_deref() == Some(task_id)
                    && chat.task_session_id.as_deref() == Some(session_id)
            });
        if hosted && !terminal {
            let store = self.writable_store()?;
            if !store.worker_session_is_final(task_id, Some(session_id))
                && let Err(error) = store.record_lifecycle_signal(
                    task_id,
                    TaskLifecycle::WaitingForInput,
                    None,
                    &chrono::Utc::now().to_rfc3339(),
                )
            {
                eprintln!("GitTerm GPUI dropped a task lifecycle signal: {error}");
            }
        }
        Ok(serde_json::json!({
            "task_id": task_id,
            "session_id": session_id,
            "waiting_for_input": hosted,
        }))
    }

    /// Iced's `merge_plan_set` for the caller's root chat, written to the
    /// preview's merge-plan dir (never Iced's). The target is the caller
    /// pane's directory. The write is synchronous, so no second set can
    /// interleave with it as Iced's `plan_writes` guards against.
    // shortcut: the plan file is written on the UI thread; move it to the
    // background executor if plans grow large.
    fn merge_plan_set(
        &mut self,
        caller: Option<&str>,
        request: MergePlanSetRequest,
        label: &'static str,
        cx: &App,
    ) -> Result<serde_json::Value, String> {
        use gitterm_core::merge_plan::{self, MergePlan};
        const TOOL: &str = "merge_plan_set";
        let target: Vec<PathBuf> = caller
            .and_then(|caller| {
                open_chats(cx)
                    .into_iter()
                    .find(|chat| chat.session_uid == caller)
            })
            .map(|chat| chat.cwd)
            .into_iter()
            .collect();
        self.authorize_write(label, &target)?;
        let (conversation, links) = self.caller_conversation(TOOL, caller, cx)?;
        let root = root_chat(&self.iced_config, &links, &conversation);
        let entries = merge_plan::validate_entries(request.entries)?;
        let dir = merge_plan::plans_dir(self.roots.root());
        self.roots
            .create_private_dir(&dir)
            .map_err(|error| format!("the merge plan was not saved: {error}"))?;
        let mut plan = merge_plan::load_plans(&dir)
            .remove(&root)
            .unwrap_or_else(|| MergePlan {
                root_chat_id: root.clone(),
                ..MergePlan::default()
            });
        let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let kept_holds = plan.replace_entries(entries, &stamp);
        let file = merge_plan::plan_path(&dir, &root)?;
        self.roots
            .scoped(&file)
            .map_err(|error| format!("the merge plan was not saved: {error}"))?;
        merge_plan::write_plan(&dir, &plan)
            .map_err(|error| format!("the merge plan was not saved: {error}"))?;
        eprintln!(
            "[merge-plan] {root}: plan set with {} PR(s)",
            plan.entries.len()
        );
        let mut value = merge_plan_in(&self.iced_config, &dir, &links, &conversation);
        value["kept_holds"] =
            serde_json::Value::from(kept_holds.iter().map(|key| key.label()).collect::<Vec<_>>());
        Ok(value)
    }

    /// `task_create` runs Iced's core reducer, resolution and preparation.
    /// Live uses V6's store and worktree root; fixtures use their own. Every
    /// target is authorized before resolution, again with the resolved
    /// repository and worktree before the first save, and rechecked after
    /// preparation; a refusal then fails the task with the notice.
    fn create(
        &mut self,
        reply: TaskControlReply,
        caller: Option<String>,
        request: CreateTaskRequest,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        match self.begin_create(reply.clone(), caller, request, label) {
            Ok(pending) => self.resolve_create(pending, cx),
            Err(error) => reply.send(Err(error)),
        }
    }

    /// A begun create's resolution, off the UI thread.
    fn resolve_create(&mut self, pending: PendingCreate, cx: &mut Context<Self>) {
        self.creating += 1;
        let request = pending.request.clone();
        cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move {
                    gitterm_core::task_worktree::resolve_task_preparation_blocking(&request)
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = owner.update(cx, |owner, cx| owner.create_resolved(pending, resolved, cx));
        })
        .detach();
    }

    /// Iced's `pending_task_from_control`, with the mode's worktree root.
    fn begin_create(
        &mut self,
        reply: TaskControlReply,
        caller: Option<String>,
        request: CreateTaskRequest,
        label: &'static str,
    ) -> Result<PendingCreate, String> {
        self.authorize_write(label, std::slice::from_ref(&request.repository_path))?;
        if !self.host.services().is_preview() {
            self.host.require_issued_caller(caller.as_deref(), label)?;
        }
        let mutation = TaskMutation::Create(TaskCreate {
            title: request.title,
            objective: request.objective,
            repository_path: request.repository_path,
            workspace_name: request.workspace_name,
            issue_key: request.issue_key,
            base_reference: request.base_reference,
            stopping_boundary: gitterm_core::task_host::stopping_boundary_from_control(
                request.stopping_boundary,
            ),
            origin: TaskCreateOrigin::Control,
        });
        let TaskMutationAction::PrepareTask(input) = self.reduce(mutation, &Default::default())?
        else {
            return Err("task_create: the reducer did not prepare a task".into());
        };
        let original_repository_path = input.repository_path.clone();
        let assembled = gitterm_core::task_host::assemble_task_create(
            self.host.store().as_ref(),
            input,
            |repository_path| {
                if self.host.services().is_preview() {
                    self.fixture_worktree_root(repository_path, label)
                } else {
                    gitterm_core::task_host::saved_task_worktree_root(
                        self.host.services().state_root(),
                    )
                }
            },
        )?;
        let targets = vec![
            original_repository_path,
            assembled.workspace_dir.clone(),
            assembled.request.worktree_root.clone(),
        ];
        let authorized = self.authorize_write(label, &targets)?;
        Ok(PendingCreate {
            reply,
            caller,
            label,
            task_id: assembled.task_id,
            workspace_name: assembled.workspace_name,
            title: assembled.title,
            objective: assembled.objective,
            issue_key: assembled.issue_key,
            stopping_boundary: assembled.stopping_boundary,
            request: assembled.request,
            targets,
            authorized,
            worktree: None,
            worker: None,
        })
    }

    /// `<fixture>/worktrees` for the fixture holding `repository`. Git
    /// refuses a worktree root inside the source checkout, so a fixture
    /// whose root is the repository itself fails preparation.
    fn fixture_worktree_root(
        &self,
        repository: &Path,
        label: &'static str,
    ) -> Result<PathBuf, String> {
        let actual = std::fs::canonicalize(repository)
            .map_err(|error| format!("{}: {error}", repository.display()))?;
        self.fixtures
            .iter()
            .find(|fixture| actual.starts_with(fixture))
            .map(|fixture| fixture.join(FIXTURE_WORKTREES))
            .ok_or_else(|| preview_notice(label))
    }

    /// Iced's `TaskMetadataResolved` arm: recheck, authorize the resolved
    /// repository and worktree, save the preparing record, then prepare.
    fn create_resolved(
        &mut self,
        mut pending: PendingCreate,
        resolved: Result<ResolvedTaskPreparation, String>,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.save_preparing(&mut pending, resolved, cx) {
            self.creating -= 1;
            if let Some(worker) = &pending.worker {
                self.cancel_worker_preparation(worker.authority());
            }
            pending.reply.send(Err(error));
            return;
        }
        let request = pending.request.clone();
        cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    gitterm_core::task_worktree::prepare_task_worktree_blocking(&request)
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = owner.update(cx, |owner, cx| owner.create_prepared(pending, prepared, cx));
        })
        .detach();
    }

    fn save_preparing(
        &mut self,
        pending: &mut PendingCreate,
        resolved: Result<ResolvedTaskPreparation, String>,
        cx: &App,
    ) -> Result<(), String> {
        use gitterm_core::tasks::{
            ExecutorTarget, IssueProvider, IssueReference, NewTaskRecord, TaskCreator,
            TaskCreatorKind, TaskWorktree, TaskWorktreeState, WorkspaceIdentity,
            WorkspaceLocationIdentity,
        };
        self.host
            .services()
            .recheck(&pending.authorized)
            .map_err(|denied| denied.notice().to_string())?;
        // A worker's authority holds before anything is saved.
        if let Some(worker) = pending.worker.take() {
            let checked = self.check_worker(&worker, cx);
            pending.worker = Some(worker);
            checked?;
        }
        let resolved = resolved?;
        pending.targets.push(resolved.repository.top_level.clone());
        pending.targets.push(resolved.proposal.path.clone());
        pending.authorized = self.authorize_write(pending.label, &pending.targets)?;
        let timestamp = chrono::Utc::now().to_rfc3339();
        let mut record = TaskRecord::new_draft(
            NewTaskRecord {
                task_id: pending.task_id.clone(),
                title: pending.title.clone(),
                objective: pending.objective.clone(),
                workspace: WorkspaceIdentity {
                    name: pending.workspace_name.clone(),
                    location: WorkspaceLocationIdentity::Local {
                        directory: resolved.repository.top_level.clone(),
                    },
                },
                repository: resolved.repository.identity.clone(),
                issue: pending.issue_key.as_ref().map(|key| IssueReference {
                    provider: IssueProvider::Linear,
                    key: key.clone(),
                    url: None,
                }),
                base: resolved.base.clone(),
                branch: resolved.proposal.branch.clone(),
                executor: ExecutorTarget::Local,
                harness: None,
                stopping_boundary: pending.stopping_boundary,
            },
            timestamp.clone(),
        );
        record.created_by = Some(TaskCreator {
            kind: TaskCreatorKind::Coordinator,
            session_id: pending.caller.clone(),
            harness_label: None,
            created_at: timestamp,
        });
        record.lifecycle = TaskLifecycle::Preparing;
        record.worktree = TaskWorktree {
            state: TaskWorktreeState::Preparing,
            path: None,
        };
        let mutation = TaskMutation::InsertPreparing {
            record: Box::new(record),
        };
        let TaskMutationAction::PersistPreparing { record } =
            self.reduce(mutation, &Default::default())?
        else {
            return Err("task_create: the reducer did not persist the task".into());
        };
        self.writable_store()?
            .insert(*record)
            .map_err(|error| error.to_string())?;
        self.own_tasks.insert(pending.task_id.clone());
        if let Some(worker) = pending.worker.take() {
            let bound = self.bind_worker(&worker, &pending.task_id, &resolved, cx);
            pending.worker = Some(worker);
            bound?;
        }
        pending.worktree = Some(resolved.proposal.path);
        Ok(())
    }

    /// Iced's `TaskWorktreePrepared` arm, after the targets' recheck: a
    /// target that no longer resolves as authorized fails the task with the
    /// notice, and its path is left alone.
    fn create_prepared(
        &mut self,
        mut pending: PendingCreate,
        prepared: Result<PreparedTaskWorktree, String>,
        cx: &mut Context<Self>,
    ) {
        self.creating -= 1;
        #[cfg(test)]
        if let (Some(hook), Some(path)) = (self.before_recheck.take(), &pending.worktree) {
            hook(path);
        }
        let prepared = match self.host.services().recheck(&pending.authorized) {
            Ok(()) => {
                if let Some(worker) = pending.worker.take() {
                    return self.worker_prepared(pending, worker, prepared, cx);
                }
                prepared
            }
            Err(denied) => {
                // A refused worker fails its task as a create does.
                if let Some(worker) = pending.worker.take() {
                    self.cancel_worker_preparation(worker.authority());
                }
                Err(denied.notice().to_string())
            }
        };
        let cleanup_path = pending.worktree.clone();
        let facts = TaskMutationFacts {
            existing_paths: cleanup_path
                .iter()
                .filter(|path| path.exists())
                .cloned()
                .collect(),
            ..Default::default()
        };
        let mutation = TaskMutation::PreparationCompleted {
            task_id: pending.task_id.clone(),
            result: prepared.map(Into::into),
            cleanup_path,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let result = match self.reduce(mutation, &facts) {
            Ok(TaskMutationAction::PersistPreparation { prepared, .. }) => self
                .writable_store()
                .and_then(|store| {
                    store
                        .complete_worktree_preparation(&pending.task_id, prepared, &now)
                        .map_err(|error| error.to_string())?;
                    store.get(&pending.task_id).cloned().ok_or_else(|| {
                        format!(
                            "Task {} disappeared after worktree preparation",
                            pending.task_id
                        )
                    })
                })
                .and_then(|task| self.task_value(&task)),
            Ok(TaskMutationAction::PersistWorkerPreparationFailure {
                error,
                cleanup_path,
                ..
            }) => {
                let saved = self.writable_store().and_then(|store| {
                    store
                        .fail_worktree_preparation(
                            &pending.task_id,
                            error.clone(),
                            cleanup_path,
                            &now,
                        )
                        .map_err(|error| error.to_string())
                });
                if let Err(store_error) = saved {
                    eprintln!("GitTerm GPUI could not save the failed task: {store_error}");
                }
                Err(error)
            }
            Ok(_) => Err("task_create: the reducer did not settle the preparation".into()),
            Err(error) => Err(error),
        };
        cx.notify();
        pending.reply.send(result);
    }

    fn reduce(
        &mut self,
        mutation: TaskMutation,
        facts: &TaskMutationFacts,
    ) -> Result<TaskMutationAction, String> {
        self.host
            .reduce_mutation(
                TaskMutationCommand {
                    reply: None,
                    mutation,
                },
                facts,
            )
            .action
    }

    /// A task in the tools' shape, with its panes as `open_sessions`.
    fn task_value(&self, task: &TaskRecord) -> Result<serde_json::Value, String> {
        gitterm_core::task_host::task_control_value(
            task,
            &open_sessions(task, &self.labelled_panes()),
        )
    }

    /// The caller's provider conversation and the store's chat links, as
    /// the merge-plan tools resolve a root (Iced's `merge_plan_root`).
    fn caller_conversation(
        &self,
        tool: &str,
        caller: Option<&str>,
        cx: &App,
    ) -> Result<(String, Vec<gitterm_core::chat_facts::ChildLink>), String> {
        let caller =
            caller.ok_or_else(|| gitterm_core::task_mcp::missing_plan_caller_error(tool))?;
        let chat = open_chats(cx)
            .into_iter()
            .find(|chat| chat.session_uid == caller)
            .ok_or_else(|| {
                format!("{tool}: no GitTerm tab on this Mac has session {caller}; merge plans are for this Mac's chats")
            })?;
        let conversation = chat.conversation.ok_or_else(|| {
            format!(
                "{tool}: this tab has no conversation id yet; call it again after your first reply"
            )
        })?;
        let workspaces = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .map_or(&[][..], |source| &source.workspaces[..]);
        let links = self.host.store().as_ref().map_or_else(Vec::new, |store| {
            crate::data::iced_chat_links(workspaces, store)
        });
        Ok((conversation, links))
    }

    /// Where merge plans are read: the preview's own dir in fixture mode,
    /// where `merge_plan_set` writes them, else Iced's (only read).
    fn plans_dir(&self) -> PathBuf {
        let config = if self.fixtures.is_empty() {
            &self.iced_config
        } else {
            self.roots.root()
        };
        gitterm_core::merge_plan::plans_dir(config)
    }

    /// Iced's `merge_plan_get`: the plan of the caller's root chat (caller
    /// pane, its conversation, then the conversation's root through saved
    /// facts and the store's links), with live PR state and the root's
    /// unplanned PRs. The caller and links are resolved here; the plan,
    /// watch and facts files are read on the background executor, which
    /// sends the reply, so the UI thread never waits on them.
    fn merge_plan_get(
        &self,
        reply: TaskControlReply,
        caller: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let (conversation, links) = match self.caller_conversation("merge_plan_get", caller, cx) {
            Ok(resolved) => resolved,
            Err(error) => return reply.send(Err(error)),
        };
        let (iced_config, plans_dir) = (self.iced_config.clone(), self.plans_dir());
        cx.background_spawn(async move {
            reply.send(Ok(merge_plan_in(
                &iced_config,
                &plans_dir,
                &links,
                &conversation,
            )));
        })
        .detach();
    }

    /// Re-seeds when Iced's file changed and the host owns no in-flight work
    /// and none of its own writes (fixture mode), which a re-seed would
    /// discard. A failed refresh keeps the last snapshot and raises a notice.
    // shortcut: the stat and copy run on the UI thread every RESEED_POLL;
    // move them to the background executor if tasks.json grows large.
    pub(crate) fn refresh_seed(&mut self, cx: &mut Context<Self>) -> Reseed {
        let stamp = match source_stamp(&self.source) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.raise(
                    format!("stat Iced tasks {}: {error}", self.source.display()),
                    cx,
                );
                return Reseed::Failed;
            }
        };
        if stamp == self.stamp {
            return Reseed::Unchanged;
        }
        if owns_in_flight_work(&self.host)
            || self.creating > 0
            || self.wrote
            || !self.runs.is_empty()
        {
            // The stamp stays old, so the next poll retries.
            return Reseed::Deferred;
        }
        match seed_copy(&self.roots, &self.source) {
            Ok((store, stamp)) => {
                self.host.replace_store(Some(store));
                self.stamp = stamp;
                self.notice = None;
                cx.notify();
                Reseed::Reseeded
            }
            Err(error) => {
                // Do not retry the same bad file every poll.
                self.stamp = stamp;
                self.raise(
                    format!("Task list refresh failed; keeping the last snapshot: {error:#}"),
                    cx,
                );
                Reseed::Failed
            }
        }
    }

    fn raise(&mut self, notice: String, cx: &mut Context<Self>) {
        eprintln!("{notice}");
        self.notice = Some(notice);
        cx.notify();
    }

    /// Close ingress and preserve existing run/terminal stop behavior. The
    /// reaper owns server, browser and child completion; the shell retains its
    /// lock through that completion and the later chat/job drains.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn shutdown(&mut self) {
        // Dropping each unadopted terminal kills its process group; GPUI
        // reaps the killed child when it releases the terminal.
        self.terminals.clear();
        self.stop(Vec::new());
    }

    /// [`Self::shutdown`] at quit, with the killed terminals' children
    /// reaped on the background thread too.
    pub(crate) fn quit(&mut self, cx: &mut App) {
        let terminals = self.stop_terminals(cx);
        self.stop(terminals);
    }

    fn stop(&mut self, terminals: Vec<gitterm_terminal::Backend>) {
        for session in self.launched.keys() {
            self.host.revoke_caller(session);
        }
        self.host.shutdown();
        let unpublished_stops = std::mem::take(&mut self.unpublished_stops);
        let mut runs: Vec<runs::RunHandle> = self.runs.drain().map(|(_, run)| run).collect();
        // On Unix this kills each spawned Codex's group before it returns,
        // and a run that has not spawned Codex yet never does.
        for run in &runs {
            run.stop.stop();
        }
        if let Some(Servers {
            task_mcp,
            browser,
            log,
            jobs,
            limits_jobs,
            runtime,
        }) = self.servers.take()
        {
            drop(task_mcp);
            log.shutdown.notify_one();
            let lock = self.lock.take();
            let reply = self.reap_reply.take();
            let reap = move || {
                // Retain standalone host ownership until every shutdown phase
                // completes. Production also has the shell's outer lock guard.
                let retained_lock = lock;
                let result = (|| -> Result<()> {
                    // Save jobs include spawn_blocking writes. Join them before
                    // releasing the lease, independent of UI completions.
                    runtime.block_on(async {
                        for job in limits_jobs {
                            job.await.context("join plan-limit job")?;
                        }
                        Ok::<_, anyhow::Error>(())
                    })?;
                    runtime
                        .block_on(browser.shutdown())
                        .map_err(anyhow::Error::msg)?;
                    drop(browser);
                    runtime.block_on(async {
                        tokio::time::timeout(SERVER_DRAIN, async {
                            for job in jobs {
                                job.await.context("join preview server")?;
                            }
                            Ok::<_, anyhow::Error>(())
                        })
                        .await
                        .context("shutdown phase task-host-servers: timeout")?
                    })?;
                    for stop in unpublished_stops {
                        stop.join()
                            .map_err(|_| anyhow::anyhow!("unpublished worker shutdown panicked"))?
                            .map_err(anyhow::Error::msg)?;
                    }
                    drop(terminals);
                    runtime.block_on(async {
                        tokio::time::timeout(RUN_STOP, async {
                            for run in &mut runs {
                                let _ = (&mut run.done).await;
                            }
                        })
                        .await
                        .context("shutdown phase task-host-runs: timeout")
                    })?;
                    Ok(())
                })();
                if let Err(error) = &result {
                    eprintln!("{error:#}");
                }
                for run in runs {
                    run.abort.abort();
                }
                runtime.shutdown_timeout(Duration::ZERO);
                drop(retained_lock);
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            };
            if let Err(error) = std::thread::Builder::new()
                .name("gitterm-run-reaper".into())
                .spawn(reap)
            {
                eprintln!("shutdown phase task-host-reaper: could not start: {error}");
            }
        }
        if let Some(reaped) = self.reaped.take()
            && let Err(error) = reaped
                .recv_timeout(crate::lifetime::DRAIN_TIMEOUT)
                .context("shutdown phase task-host-reaper: timeout or worker stopped")
                .and_then(|result| result)
        {
            eprintln!("{error:#}");
        }
    }

    /// After stop admission is closed; completion may still be reaping.
    fn is_shut_down(&self) -> bool {
        self.servers.is_none()
    }
}

/// One GPUI chat pane, as the read arms see it.
#[derive(Clone, Debug)]
pub(crate) struct OpenChat {
    pub tab_id: usize,
    /// The task MCP caller: the pane's target `session_uid`.
    pub session_uid: String,
    /// The provider conversation id, once the first reply named it.
    pub conversation: Option<String>,
    pub task_id: Option<String>,
    pub task_session_id: Option<String>,
    pub cwd: PathBuf,
}

impl OpenChat {
    /// The pane as the reducer's session facts: every GPUI chat is a native
    /// harness chat, and the preview tracks no task live state.
    fn session_state(&self) -> session::State {
        session::State {
            target: session::Target {
                tab_id: self.tab_id,
                session_uid: self.session_uid.clone(),
                task_id: self.task_id.clone(),
                task_session_id: self.task_session_id.clone(),
            },
            control: session::TurnControl::Harness,
            task_live: false,
            last_tab: false,
        }
    }
}

/// A create between its first check and its last save.
struct PendingCreate {
    reply: TaskControlReply,
    caller: Option<String>,
    label: &'static str,
    request: PrepareTaskWorktreeRequest,
    task_id: String,
    workspace_name: String,
    title: String,
    objective: String,
    issue_key: Option<String>,
    stopping_boundary: StoppingBoundary,
    /// Every path the create acts in, so far.
    targets: Vec<PathBuf>,
    authorized: AuthorizedTargets,
    /// The proposed worktree, once the preparing record is saved.
    worktree: Option<PathBuf>,
    /// Set by `delegate_task`: once the worktree is ready its worker is
    /// recorded and launched, instead of answering with the bare task.
    worker: Option<delegate::PendingWorker>,
}

/// The workspace owner's open chat panes; none before the window exists.
fn open_chats(cx: &App) -> Vec<OpenChat> {
    let Some(owner) = cx
        .try_global::<crate::work_sidebar::IcedWorkSource>()
        .and_then(|source| source.owner.upgrade())
    else {
        return Vec::new();
    };
    owner
        .read(cx)
        .chats
        .values()
        .map(|pane| {
            let pane = pane.read(cx);
            let target = pane.target();
            OpenChat {
                tab_id: target.tab_id,
                session_uid: target.session_uid,
                conversation: pane.chat_session_id(),
                task_id: target.task_id,
                task_session_id: target.task_session_id,
                cwd: pane.cwd().to_path_buf(),
            }
        })
        .collect()
}

/// Iced's `plan_limits` arm on Iced's plan-limits file, read as Usage reads it.
pub(crate) fn plan_limits(iced_config: &Path) -> Result<serde_json::Value, String> {
    use gitterm_core::plan_limits::{PlanLimitsFile, evaluate_policy};
    let file = PlanLimitsFile::load(&PlanLimitsFile::path_in(iced_config));
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    serde_json::to_value(evaluate_policy(&file, now).report)
        .map_err(|error| format!("cannot encode plan_limits result: {error}"))
}

/// Iced's `html_render` arm: a valid request from an open GPUI chat (every
/// GPUI chat is a native Claude chat).
pub(crate) fn html_render(
    request: gitterm_core::content::html_render::HtmlRenderRequest,
    caller: Option<&str>,
    chats: &[OpenChat],
) -> Result<serde_json::Value, String> {
    if !gitterm_core::content::html_render::NATIVE_HTML_SUPPORTED {
        return Err("html_render currently requires GitTerm for macOS; this platform lacks the required native subframe navigation guard".into());
    }
    request.validate()?;
    if !chats
        .iter()
        .any(|chat| Some(chat.session_uid.as_str()) == caller)
    {
        return Err("html_render requires an open local native Claude or ACP chat tab (Pi does not restore conversation history)".into());
    }
    serde_json::to_value(gitterm_core::chat::html_render::HtmlRenderResult {
        html_render: request,
    })
    .map_err(|error| format!("cannot encode visual reply: {error}"))
}

/// Iced's preset names, as the task list's `available_presets`.
pub(crate) fn preset_names(iced_config: &Path) -> Vec<String> {
    crate::workspace_config::load_iced_settings_from(iced_config)
        .agent_presets
        .iter()
        .map(|preset| preset.name.clone())
        .collect()
}

/// The task read in Iced's shape: each task with the shell's panes holding
/// it as `open_sessions`, and Iced's preset names on a list.
pub(crate) fn read_projection(
    effect: gitterm_core::task_commands::TaskReadEffect,
    panes: &[TaskPane],
    presets: &[String],
) -> Result<serde_json::Value, String> {
    use gitterm_core::task_commands::TaskProjection;
    let value = |task: &gitterm_core::tasks::TaskRecord| {
        gitterm_core::task_host::task_control_value(task, &open_sessions(task, panes))
    };
    match effect.projection? {
        TaskProjection::List(tasks) => Ok(serde_json::json!({
            "tasks": tasks.iter().map(value).collect::<Result<Vec<_>, _>>()?,
            "available_presets": presets,
        })),
        TaskProjection::Get(task) => value(&task),
    }
}

/// Iced's tab label once the tab's status polls (src/main.rs
/// `status.repo_name`, from core's `collect_repo_status`): the folder name of
/// the git root of `folder`, else the folder's own name, else `repo`.
/// Blocking; fsmonitor off so the read starts no daemon and writes nothing
/// under `.git`.
fn folder_label(folder: &Path) -> String {
    gitterm_core::git_status::collect_repo_status_with(
        folder,
        gitterm_core::git_status::StatusReadOptions { fsmonitor: false },
    )
    .repo_name
}

/// Iced lists the tabs whose task is `task` (src/main.rs
/// `task_control_value`); the preview's are the shell's panes.
fn open_sessions(task: &gitterm_core::tasks::TaskRecord, panes: &[TaskPane]) -> Vec<OpenSession> {
    panes
        .iter()
        .filter(|pane| pane.task_id == task.task_id)
        .map(|pane| pane.session.clone())
        .collect()
}

/// Iced's `merge_plan_value` for the root of `conversation`, from Iced's
/// files. Roots and PRs follow Iced's rules over a stand-in index built from
/// the saved facts files (the preview has no transcript index). Iced also
/// adds PRs its live tabs saw and its cached task-branch lookups; the
/// preview has neither (no PR scan of GPUI turns, no `gh`), so
/// `unplanned_prs` holds the root's saved chat facts PRs only.
#[cfg(test)]
pub(crate) fn merge_plan_for(
    iced_config: &Path,
    links: &[gitterm_core::chat_facts::ChildLink],
    conversation: &str,
) -> serde_json::Value {
    let plans = gitterm_core::merge_plan::plans_dir(iced_config);
    merge_plan_in(iced_config, &plans, links, conversation)
}

/// [`merge_plan_for`] with the plans read from `plans_dir`.
pub(crate) fn merge_plan_in(
    iced_config: &Path,
    plans_dir: &Path,
    links: &[gitterm_core::chat_facts::ChildLink],
    conversation: &str,
) -> serde_json::Value {
    use gitterm_core::{chat_facts, merge_plan, prs};
    let log = |message: &str| eprintln!("{message}");
    let dir = chat_facts::facts_dir(iced_config);
    let (index, root) = facts_root(&dir, links, conversation);
    let plans = merge_plan::load_plans(plans_dir);
    let watch = prs::WatchFile::load(&prs::WatchFile::path_in(iced_config));
    let saved = chat_facts::load_root_prs(&dir, &index, links, &root, log);
    let found = chat_facts::root_prs(&index, &root, &saved);
    prs::merge_plan_value(&root, plans.get(&root), &watch, found)
}

/// The root chat of `conversation`, as [`merge_plan_in`] resolves it.
fn root_chat(
    iced_config: &Path,
    links: &[gitterm_core::chat_facts::ChildLink],
    conversation: &str,
) -> String {
    let dir = gitterm_core::chat_facts::facts_dir(iced_config);
    facts_root(&dir, links, conversation).1
}

/// The saved facts index in `facts_dir` and the root of `conversation` in
/// it, by Iced's rules.
fn facts_root(
    facts_dir: &Path,
    links: &[gitterm_core::chat_facts::ChildLink],
    conversation: &str,
) -> (Vec<gitterm_core::chats::ChatIndexEntry>, String) {
    use gitterm_core::chat_facts;
    let log = |message: &str| eprintln!("{message}");
    let mut index = chat_facts::saved_facts_index(facts_dir, links, log);
    chat_facts::resolve_tree(&mut index, links);
    let root = chat_facts::root_chat_of(&index, conversation, || links.to_vec());
    (index, root)
}

#[cfg(test)]
mod live_tests {
    use super::*;
    #[gpui::test]
    fn live_uses_direct_store_shared_namespace_and_no_fixture_jobs(cx: &mut gpui::TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("v6");
        std::fs::create_dir(&root).unwrap();
        let roots = Arc::new(
            Preview::validate(
                &base.join("gpui"),
                gitterm_core::preview::ProtectedRoots::new(&base.join("home"), [root.clone()])
                    .unwrap(),
            )
            .unwrap(),
        );
        let source = root.join("tasks.json");
        let bytes = serde_json::to_vec(&gitterm_core::tasks::TaskStoreDocument::default()).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        // A prior seed must be left untouched, even when the source is missing.
        std::fs::create_dir(roots.root()).unwrap();
        let seed = roots.root().join("tasks.json");
        std::fs::write(&seed, b"keep old preview seed").unwrap();
        let lock = Arc::new(HostLock::acquire(&root).unwrap());
        let services = Arc::new(
            ServiceContext::production(&root, "274")
                .unwrap()
                .for_gpui_live(),
        );
        let token = services.task_mcp().token().to_owned();
        let (sender, receiver) = gitterm_core::task_commands::runtime::command_channel();
        let config = HostConfig {
            roots,
            iced_config: root.clone(),
            ingress: Ingress::Channel(sender.clone(), receiver),
        };
        let owner = cx
            .update(|cx| TaskHostOwner::start_live(config, lock, services, cx))
            .unwrap();
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.host.store().as_ref().unwrap().path(), source);
            assert_eq!(owner.host.services().state_root(), root);
            assert_eq!(owner.host.services().task_mcp().token(), token);
            assert!(!owner.host.services().is_preview());
            assert!(owner._reseed.is_none());
            assert!(owner._deliver.is_some());
            assert!(owner.fixtures.is_empty());
            assert!(
                owner
                    .host
                    .services()
                    .runner(
                        gitterm_core::preview::service::RunnerEffect::AutomaticDelivery,
                        None
                    )
                    .is_ok()
            );
            for effect in [
                gitterm_core::preview::service::RunnerEffect::HtmlPreview,
                gitterm_core::preview::service::RunnerEffect::RestoreSession,
            ] {
                assert!(
                    owner
                        .host
                        .services()
                        .runner(effect, None)
                        .unwrap_err()
                        .notice()
                        .ends_with("not available in preview")
                );
            }
        });
        owner.update(cx, |owner, _| owner.shutdown());
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(std::fs::read(&seed).unwrap(), b"keep old preview seed");
        std::fs::remove_file(&source).unwrap();
        let lock = HostLock::acquire(&root).unwrap();
        assert!(direct_store(&lock).unwrap().tasks().is_empty());
        assert!(!source.exists());
        assert_eq!(std::fs::read(&seed).unwrap(), b"keep old preview seed");
    }
}
