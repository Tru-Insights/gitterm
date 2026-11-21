//! GPUI owns the shared task host (H5b B4) on the preview root only.
//!
//! One [`TaskHostOwner`] per preview root: a lifetime host lock is taken
//! before anything is seeded or bound, the task store is a seeded copy of
//! Iced's `tasks.json` (read, never opened for writing), the task, browser
//! and log servers bind OS-assigned loopback ports, and the owner is the only
//! receiver of the task command ingress. Nothing here launches, attaches,
//! notifies or delivers: imported running or queued records stay inert.
use anyhow::{Context as _, Result};
use gitterm_browser::browser_mcp::BrowserMcpConnection;
use gitterm_core::{
    content::html_render::HtmlPreviewRequest,
    loopback::ListenerBind,
    preview::{
        Preview,
        service::{ServiceContext, classify_operation, preview_notice},
    },
    task_host::{HostEffect, TaskHost},
    task_mcp::{
        HtmlPreviewFuture, HtmlPreviewRunner, TaskControlEnvelope, TaskControlReceiver,
        TaskControlSender, TaskMcpConnection,
    },
    tasks::TaskStore,
};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, Task};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const HOST_LOCK: &str = ".task-host.lock";
const SEED_STAGING: &str = ".tasks.seed.json";
const RESEED_POLL: Duration = Duration::from_secs(2);
/// Bounds quit: the servers were cancelled already; this only lets their
/// graceful shutdown finish before the runtime is dropped.
const SERVER_DRAIN: Duration = Duration::from_millis(500);

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
    /// Iced's `tasks.json`, found the way `data.rs` reads it. Only read.
    pub iced_tasks: PathBuf,
    pub ingress: Ingress,
}
impl HostConfig {
    pub(crate) fn production(roots: Arc<Preview>) -> Self {
        Self {
            roots,
            iced_tasks: TaskStore::path_for_config_root(&crate::workspace_config::iced_config_dir()),
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
    source: PathBuf,
    stamp: Stamp,
    /// The latest seed problem, shown until a seed succeeds.
    pub(crate) notice: Option<String>,
    servers: Option<Servers>,
    /// Dropped after the servers and their runtime on quit.
    lock: Option<HostLock>,
    _pump: Task<()>,
    _reseed: Task<()>,
    _quit: Subscription,
}

impl TaskHostOwner {
    /// Lock, seed, bind, then create the entity. On any error nothing is
    /// bound and the caller runs without a host.
    pub(crate) fn start(config: HostConfig, cx: &mut App) -> Result<Entity<Self>> {
        let HostConfig {
            roots,
            iced_tasks,
            ingress,
        } = config;
        let lock = HostLock::acquire(&roots)?;
        let (store, stamp) = seed_copy(&roots, &iced_tasks)?;
        let services = ServiceContext::preview(roots.clone()).context("preview task services")?;
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
            refused_html_preview(),
            SERVER_VERSION,
        )
        .context("bind preview task MCP")?;
        let (browser, browser_server) = {
            let _entered = runtime.enter();
            gitterm_browser::browser_mcp::prepare_on(
                services.state_root(),
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
                _pump: pump,
                _reseed: reseed,
                _quit: quit,
            }
        }))
    }

    /// The task server for one native Claude chat, attributed to `caller`
    /// (the chat's `session_uid`); `None` once the servers have shut down.
    pub(crate) fn claude_mcp_server(
        &self,
        caller: &str,
    ) -> Option<gitterm_core::harness::claude::ClaudeMcpServer> {
        self.servers
            .as_ref()
            .map(|servers| servers.task_mcp.claude_mcp_server(Some(caller)))
    }

    pub(crate) fn host(&self) -> &TaskHost {
        &self.host
    }
    #[cfg(test)]
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

    /// The one executor for host effects. B4 answers every effect with the
    /// preview notice except the task read, which returns the authority's
    /// projection. B5 fills in the read arms (open sessions, presets, plan
    /// limits, html_render, delegation_get, merge_plan_get); B9–B11 the
    /// fixture-mode write and process arms.
    fn execute(&mut self, envelope: TaskControlEnvelope, _cx: &mut Context<Self>) {
        let label = classify_operation(&envelope.operation).label;
        let notice = || Err(preview_notice(label));
        let Some(effect) = self.host.dispatch(envelope) else {
            return;
        };
        match effect {
            HostEffect::Read { reply, effect } => reply.send(read_projection(effect)),
            HostEffect::PlanLimits { reply }
            | HostEffect::HtmlRender { reply, .. }
            | HostEffect::Create { reply, .. }
            | HostEffect::LaunchSession { reply, .. }
            | HostEffect::UpdateHandoff { reply, .. }
            | HostEffect::DelegateTask { reply, .. }
            | HostEffect::TurnEnded { reply, .. }
            | HostEffect::RequestReview { reply, .. }
            | HostEffect::RequestConsult { reply, .. }
            | HostEffect::DelegationRecord { reply, .. }
            | HostEffect::MergePlanSet { reply, .. }
            | HostEffect::MergePlanGet { reply, .. } => reply.send(notice()),
        }
    }

    /// Re-seeds when Iced's file changed and the host owns no in-flight work.
    /// A failed refresh keeps the last snapshot and raises a notice.
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
        if owns_in_flight_work(&self.host) {
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

/// B4's task read: the authority's records as stored. B5 adds open sessions
/// and presets (Iced's `task_control_value` shape).
fn read_projection(
    effect: gitterm_core::task_commands::TaskReadEffect,
) -> Result<serde_json::Value, String> {
    use gitterm_core::task_commands::TaskProjection;
    let value = |task: &gitterm_core::tasks::TaskRecord| {
        serde_json::to_value(task)
            .map_err(|error| format!("could not serialize task {}: {error}", task.task_id))
    };
    match effect.projection? {
        TaskProjection::List(tasks) => Ok(serde_json::json!({
            "tasks": tasks.iter().map(value).collect::<Result<Vec<_>, _>>()?,
            "available_presets": [],
        })),
        TaskProjection::Get(task) => value(&task),
    }
}

/// B7 gates html_preview; until then the preview runner starts nothing.
fn refused_html_preview() -> HtmlPreviewRunner {
    Arc::new(|_: HtmlPreviewRequest| -> HtmlPreviewFuture {
        Box::pin(std::future::ready(Err(preview_notice("html_preview"))))
    })
}
