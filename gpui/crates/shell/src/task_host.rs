//! GPUI owns the shared task host (H5b B4) on the preview root only.
//!
//! One [`TaskHostOwner`] per preview root: a lifetime host lock is taken
//! before anything is seeded or bound, the task store is a seeded copy of
//! Iced's `tasks.json` (read, never opened for writing), the task, browser
//! and log servers bind OS-assigned loopback ports, and the owner is the only
//! receiver of the task command ingress. Nothing here launches, attaches,
//! notifies or delivers: imported running or queued records stay inert.
//!
//! Reads answer in Iced's shapes from what the preview reads: the seeded
//! store, the GPUI chat panes, and Iced's settings, plan-limits, merge-plan,
//! PR-watch and chat-facts files (only read). Every write, launch and run
//! answers the preview notice.
//!
//! Fixture mode (H5b B9, tests and acceptance only: [`TaskHostOwner::start_fixture`])
//! gives the host a fixture-launch service context. Then `task_create`,
//! `task_update_handoff`, `merge_plan_set` and turn-end notifications whose
//! target lies in a fixture write the preview's own store and state, and a
//! created task's worktree is prepared inside that fixture. Launches and runs
//! still answer the notice.
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

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const HOST_LOCK: &str = ".task-host.lock";
const SEED_STAGING: &str = ".tasks.seed.json";
const RESEED_POLL: Duration = Duration::from_secs(2);
/// Bounds quit: the servers were cancelled already; this only lets their
/// graceful shutdown finish before the runtime is dropped.
const SERVER_DRAIN: Duration = Duration::from_millis(500);
const STORE_UNAVAILABLE: &str = "GitTerm's task store is unavailable";
/// A fixture create's worktrees go here, inside the repository's fixture.
const FIXTURE_WORKTREES: &str = "worktrees";

/// The app's task host, for B6 chat descriptors and B8 panes.
pub(crate) struct GpuiTaskHost(pub(crate) Entity<TaskHostOwner>);
impl Global for GpuiTaskHost {}

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
    /// Iced's config dir, found the way `data.rs` reads it. Its `tasks.json`
    /// is seeded from; it and the files the reads use are only read.
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

/// Cross-process lifetime lock on one preview root. It is separate from the
/// layout writer lease, which locks only around each save. The file is never
/// unlinked: the kernel lock (flock, released by close or process death),
/// not its contents, owns the host.
pub(crate) struct HostLock {
    _file: std::fs::File,
}
impl HostLock {
    pub(crate) fn acquire(roots: &Preview) -> Result<Self> {
        roots.create_private_dir(roots.root())?;
        let path = roots.scoped(&roots.root().join(HOST_LOCK))?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("open task host lock {}", path.display()))?;
        // scoped() refused a symlinked path; this catches a swap since, and
        // the handle must be the file at the path, as the writer lease checks.
        let meta = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            meta.is_file(),
            "task host lock is not a regular file: {}",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                meta.nlink() == 1,
                "task host lock is hard-linked: {}",
                path.display()
            );
            let opened = file.metadata()?;
            anyhow::ensure!(
                opened.dev() == meta.dev() && opened.ino() == meta.ino(),
                "task host lock was replaced: {}",
                path.display()
            );
        }
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "another GitTerm GPUI already hosts tasks for {}",
                roots.root().display()
            ),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("lock task host {}", path.display()))
            }
        }
    }
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
/// old one. B4 starts none of it; B9–B11 do.
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
    runtime: tokio::runtime::Runtime,
}

pub(crate) struct TaskHostOwner {
    host: TaskHost,
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
    lock: Option<HostLock>,
    /// Canonical fixture roots; empty in plain preview.
    fixtures: Vec<PathBuf>,
    /// Fixture creates between their first check and their last save.
    creating: usize,
    /// The preview store holds the host's own writes, which a re-seed from
    /// Iced's file would discard.
    wrote: bool,
    /// Runs on a prepared worktree path before its post-preparation recheck.
    #[cfg(test)]
    pub(crate) before_recheck: Option<RecheckHook>,
    _pump: Task<()>,
    _reseed: Task<()>,
    _quit: Subscription,
}

impl TaskHostOwner {
    /// Lock, seed, bind, then create the entity. On any error nothing is
    /// bound and the caller runs without a host.
    pub(crate) fn start(config: HostConfig, cx: &mut App) -> Result<Entity<Self>> {
        Self::start_with(config, Vec::new(), cx)
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
        Self::start_with(config, fixtures, cx)
    }

    fn start_with(
        config: HostConfig,
        fixtures: Vec<PathBuf>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let HostConfig {
            roots,
            iced_config,
            ingress,
        } = config;
        let iced_tasks = TaskStore::path_for_config_root(&iced_config);
        let lock = HostLock::acquire(&roots)?;
        let (store, stamp) = seed_copy(&roots, &iced_tasks)?;
        let services = if fixtures.is_empty() {
            ServiceContext::preview(roots.clone())
        } else {
            ServiceContext::fixture_launch(roots.clone(), fixtures.iter().cloned())
        }
        .context("preview task services")?;
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
        let (task_mcp, task_server) = gitterm_core::task_mcp::prepare_on(
            ListenerBind::LoopbackEphemeral,
            services.task_mcp().token().to_string(),
            sender.clone(),
            // The preview context denies RunnerEffect::HtmlPreview: no Chrome.
            gitterm_browser::browser_control::html_preview_runner_in(&services),
            SERVER_VERSION,
        )
        .context("bind preview task MCP")?;
        // The browser server gates every call on its own preview context
        // over the same roots, so its state root and profile are the host's.
        let browser_services =
            Arc::new(ServiceContext::preview(roots.clone()).context("preview browser services")?);
        let (browser, browser_server) = {
            let _entered = runtime.enter();
            gitterm_browser::browser_mcp::prepare_scoped(
                browser_services,
                ListenerBind::LoopbackEphemeral,
                SERVER_VERSION,
            )
            .context("bind preview browser MCP")?
        };
        let log = gitterm_core::log_server::ServerState::new();
        let jobs = vec![
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
            runtime.spawn({
                let log = log.clone();
                async move {
                    if let Err(error) = gitterm_core::log_server::start_server_on(
                        log,
                        ListenerBind::LoopbackEphemeral,
                    )
                    .await
                    {
                        eprintln!("preview log server: {error}");
                    }
                }
            }),
        ];
        let host = TaskHost::new(Some(store), services, sender);
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
            let reseed = cx.spawn(async move |owner: gpui::WeakEntity<Self>, cx| {
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
            });
            // Synchronous: GPUI's quit timeout starts after the callbacks
            // return. Registered before the workspace owner's chat drain.
            let quit = cx.on_app_quit(|owner: &mut Self, _| {
                owner.shutdown();
                std::future::ready(())
            });
            Self {
                host,
                roots,
                iced_config,
                source: iced_tasks,
                stamp,
                notice: None,
                servers: Some(Servers {
                    task_mcp,
                    browser,
                    log,
                    jobs,
                    runtime,
                }),
                lock: Some(lock),
                fixtures,
                creating: 0,
                wrote: false,
                #[cfg(test)]
                before_recheck: None,
                _pump: pump,
                _reseed: reseed,
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
            vec![
                servers.task_mcp.claude_mcp_server(Some(caller)),
                servers.browser.claude_mcp_server(Some(caller)),
            ]
        })
    }

    pub(crate) fn host(&self) -> &TaskHost {
        &self.host
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
    /// state. Launch and run arms answer the notice in both modes: no
    /// process, no network (B10–B11).
    pub(crate) fn execute_effect(
        &mut self,
        effect: HostEffect,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        match effect {
            HostEffect::PlanLimits { reply } => reply.send(plan_limits(&self.iced_config)),
            HostEffect::HtmlRender {
                reply,
                caller,
                request,
            } => reply.send(html_render(request, caller.as_deref(), &open_chats(cx))),
            HostEffect::Read { reply, effect } => {
                let source = cx.try_global::<crate::work_sidebar::IcedWorkSource>();
                let workspaces = source.map_or(&[][..], |source| &source.workspaces[..]);
                let presets: Vec<String> =
                    crate::workspace_config::load_iced_settings_from(&self.iced_config)
                        .agent_presets
                        .iter()
                        .map(|preset| preset.name.clone())
                        .collect();
                reply.send(read_projection(
                    effect,
                    &open_chats(cx),
                    workspaces,
                    &presets,
                ));
            }
            // Iced adds the live worker card; the preview runs no workers.
            HostEffect::DelegationRecord { reply, record, .. } => reply.send(Ok(record)),
            HostEffect::MergePlanGet { reply, caller } => {
                reply.send(self.merge_plan_get(caller.as_deref(), cx));
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

    /// The preview's store for a write: the copy under the preview root,
    /// re-scoped first so a swapped link never redirects the save. Marks
    /// the store as the host's own, so no re-seed discards the write.
    fn writable_store(&mut self) -> Result<&mut TaskStore, String> {
        let store = self.host.store_mut().as_mut().ok_or(STORE_UNAVAILABLE)?;
        self.roots
            .scoped(store.path())
            .map_err(|error| format!("task store {}: {error}", store.path().display()))?;
        self.wrote = true;
        Ok(store)
    }

    /// Iced's `update_handoff_requested`, on the preview's store: the core
    /// reducer and `record_handoff`, so done/blocked for an open worker
    /// delegation is accepted only from the pane hosting its task session,
    /// then the task's lifecycle mirrors it. Iced's tab, attention, parent
    /// and PR-lookup steps are UI or delivery (B11) and are not run.
    fn update_handoff(
        &mut self,
        request: UpdateTaskHandoffRequest,
        caller: Option<&str>,
        label: &'static str,
        cx: &App,
    ) -> Result<serde_json::Value, String> {
        self.authorize_write(label, &self.task_worktree(&request.task_id))?;
        let chats = open_chats(cx);
        let facts = TaskMutationFacts {
            sessions: chats.iter().map(OpenChat::session_state).collect(),
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
        let mut value = self.task_value(&task, &chats, cx)?;
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

    /// Iced's `TurnEnded` arm, store part only: a pane hosting the session
    /// now waits for input, so the task does, unless the session is a
    /// worker's final one. Preview panes carry no other live state, so the
    /// ended session decides Iced's aggregate. Nothing is delivered (B11).
    fn turn_ended(
        &mut self,
        task_id: &str,
        session_id: &str,
        label: &'static str,
        cx: &App,
    ) -> Result<serde_json::Value, String> {
        self.authorize_write(label, &self.task_worktree(task_id))?;
        let hosted = open_chats(cx).iter().any(|chat| {
            chat.task_id.as_deref() == Some(task_id)
                && chat.task_session_id.as_deref() == Some(session_id)
        });
        if hosted {
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

    /// `task_create` (Iced's control create path) in fixture mode: the core
    /// reducer, resolution and preparation Iced runs, into the preview's
    /// store, with the worktree under the repository's fixture. Every
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
        let pending = match self.begin_create(reply.clone(), caller, request, label) {
            Ok(pending) => pending,
            Err(error) => {
                reply.send(Err(error));
                return;
            }
        };
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

    /// Iced's `pending_task_from_control`, with the fixture's worktree root.
    fn begin_create(
        &mut self,
        reply: TaskControlReply,
        caller: Option<String>,
        request: CreateTaskRequest,
        label: &'static str,
    ) -> Result<PendingCreate, String> {
        self.authorize_write(label, std::slice::from_ref(&request.repository_path))?;
        let mutation = TaskMutation::Create(TaskCreate {
            title: request.title,
            objective: request.objective,
            repository_path: request.repository_path,
            workspace_name: request.workspace_name,
            issue_key: request.issue_key,
            base_reference: request.base_reference,
            stopping_boundary: stopping_boundary_from_control(request.stopping_boundary),
            origin: TaskCreateOrigin::Control,
        });
        let TaskMutationAction::PrepareTask(input) = self.reduce(mutation, &Default::default())?
        else {
            return Err("task_create: the reducer did not prepare a task".into());
        };
        let base_reference = input
            .base_reference
            .ok_or("task_create: the reducer left no base reference")?;
        let repository_path = self.host.store().as_ref().map_or_else(
            || input.repository_path.clone(),
            |store| owning_workspace_directory(store, &input.repository_path),
        );
        let came_from_managed_worktree =
            !gitterm_core::chat::checkout::paths_equal(&repository_path, &input.repository_path);
        let workspace_name = (!came_from_managed_worktree)
            .then_some(input.workspace_name)
            .flatten()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                repository_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Task workspace".to_string());
        let worktree_root = self.fixture_worktree_root(&repository_path, label)?;
        let targets = vec![
            input.repository_path,
            repository_path.clone(),
            worktree_root.clone(),
        ];
        let authorized = self.authorize_write(label, &targets)?;
        let task_id = TaskRecord::fresh_id();
        Ok(PendingCreate {
            reply,
            caller,
            label,
            request: PrepareTaskWorktreeRequest {
                repository_path,
                worktree_root,
                task_id: task_id.clone(),
                title: input.title.clone(),
                issue_key: input.issue_key.clone(),
                base_reference,
            },
            task_id,
            workspace_name,
            title: input.title,
            objective: input.objective,
            issue_key: input.issue_key,
            stopping_boundary: input.stopping_boundary,
            targets,
            authorized,
            worktree: None,
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
        if let Err(error) = self.save_preparing(&mut pending, resolved) {
            self.creating -= 1;
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
        pending.worktree = Some(resolved.proposal.path);
        Ok(())
    }

    /// Iced's `TaskWorktreePrepared` arm, after the targets' recheck: a
    /// target that no longer resolves as authorized fails the task with the
    /// notice, and its path is left alone.
    fn create_prepared(
        &mut self,
        pending: PendingCreate,
        prepared: Result<PreparedTaskWorktree, String>,
        cx: &mut Context<Self>,
    ) {
        self.creating -= 1;
        #[cfg(test)]
        if let (Some(hook), Some(path)) = (self.before_recheck.take(), &pending.worktree) {
            hook(path);
        }
        let prepared = match self.host.services().recheck(&pending.authorized) {
            Ok(()) => prepared,
            Err(denied) => Err(denied.notice().to_string()),
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
                .and_then(|task| self.task_value(&task, &open_chats(cx), cx)),
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
    fn task_value(
        &self,
        task: &TaskRecord,
        chats: &[OpenChat],
        cx: &App,
    ) -> Result<serde_json::Value, String> {
        let workspaces = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .map_or(&[][..], |source| &source.workspaces[..]);
        gitterm_core::task_host::task_control_value(task, &open_sessions(task, chats, workspaces))
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
    /// unplanned PRs.
    // shortcut: the plan, watch and facts files are read on the UI thread per
    // call; move them to the background executor if a call shows up in frames.
    fn merge_plan_get(&self, caller: Option<&str>, cx: &App) -> Result<serde_json::Value, String> {
        let (conversation, links) = self.caller_conversation("merge_plan_get", caller, cx)?;
        Ok(merge_plan_in(
            &self.iced_config,
            &self.plans_dir(),
            &links,
            &conversation,
        ))
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
        if owns_in_flight_work(&self.host) || self.creating > 0 || self.wrote {
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

    /// Quit order: stop ingress (resolving pending replies), drop both MCP
    /// connections, signal the log server, drain the servers, then release
    /// the host lock. Chat drains run in the workspace owner's later callback.
    pub(crate) fn shutdown(&mut self) {
        self.host.shutdown();
        if let Some(Servers {
            task_mcp,
            browser,
            log,
            jobs,
            runtime,
        }) = self.servers.take()
        {
            drop(task_mcp);
            drop(browser);
            log.shutdown.notify_one();
            runtime.block_on(async {
                let _ = tokio::time::timeout(SERVER_DRAIN, async {
                    for job in jobs {
                        let _ = job.await;
                    }
                })
                .await;
            });
            runtime.shutdown_timeout(Duration::ZERO);
        }
        self.lock = None;
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

/// A fixture create between its first check and its last save.
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
}

/// Iced's `stopping_boundary_from_control`.
fn stopping_boundary_from_control(
    boundary: Option<gitterm_core::task_mcp::TaskStoppingBoundary>,
) -> StoppingBoundary {
    use gitterm_core::task_mcp::TaskStoppingBoundary as B;
    match boundary.unwrap_or(B::ImplementUntilTestsPass) {
        B::PlanOnly => StoppingBoundary::PlanOnly,
        B::ImplementUntilTestsPass => StoppingBoundary::ImplementUntilTestsPass,
        B::PrepareDraftPr => StoppingBoundary::PrepareDraftPr,
    }
}

/// Iced's `canonical_task_workspace_directory`: a managed task worktree
/// belongs to its owning repository workspace, followed back through any
/// worktree ancestry.
fn owning_workspace_directory(store: &TaskStore, directory: &Path) -> PathBuf {
    use gitterm_core::{chat::checkout::paths_equal, tasks::WorkspaceLocationIdentity};
    let mut canonical = directory.to_path_buf();
    let mut visited = std::collections::HashSet::new();
    while visited.insert(canonical.clone()) {
        let Some(owner) = store.tasks().iter().find(|task| {
            task.worktree
                .path
                .as_deref()
                .is_some_and(|worktree| canonical.starts_with(worktree))
        }) else {
            break;
        };
        let WorkspaceLocationIdentity::Local { directory } = &owner.workspace.location else {
            break;
        };
        if paths_equal(&canonical, directory) {
            break;
        }
        canonical = directory.clone();
    }
    canonical
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

/// The task read in Iced's shape: each task with the GPUI chats bound to it
/// as `open_sessions`, and Iced's preset names on a list.
pub(crate) fn read_projection(
    effect: gitterm_core::task_commands::TaskReadEffect,
    chats: &[OpenChat],
    workspaces: &[gitterm_core::workspace_config::WorkspaceConfig],
    presets: &[String],
) -> Result<serde_json::Value, String> {
    use gitterm_core::task_commands::TaskProjection;
    let value = |task: &gitterm_core::tasks::TaskRecord| {
        gitterm_core::task_host::task_control_value(task, &open_sessions(task, chats, workspaces))
    };
    match effect.projection? {
        TaskProjection::List(tasks) => Ok(serde_json::json!({
            "tasks": tasks.iter().map(value).collect::<Result<Vec<_>, _>>()?,
            "available_presets": presets,
        })),
        TaskProjection::Get(task) => value(&task),
    }
}

/// Iced lists the task's tabs; the preview's are its chat panes. Preview
/// panes start without a task (`sessions::fresh_target`), so this is empty
/// until GPUI opens task sessions.
fn open_sessions(
    task: &gitterm_core::tasks::TaskRecord,
    chats: &[OpenChat],
    workspaces: &[gitterm_core::workspace_config::WorkspaceConfig],
) -> Vec<OpenSession> {
    chats
        .iter()
        .filter(|chat| chat.task_id.as_deref() == Some(task.task_id.as_str()))
        .map(|chat| OpenSession {
            session_id: chat.task_session_id.clone(),
            label: chat.cwd.file_name().map_or_else(
                || chat.cwd.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            workspace: workspaces
                .iter()
                .find(|workspace| chat.cwd.starts_with(&workspace.dir))
                .map_or_else(
                    || chat.cwd.display().to_string(),
                    |workspace| workspace.name.clone(),
                ),
            // shortcut: the active pane lives on the Shell, which the host
            // cannot read; report it when B8 gives the host pane state.
            is_active: false,
        })
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
