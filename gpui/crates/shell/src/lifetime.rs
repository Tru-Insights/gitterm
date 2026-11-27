//! Complete entity preparation and bounded drains before platform termination.
use anyhow::{Context as _, Result, ensure};
use gitterm_core::{preview::Preview, state_lock::StateLock};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const RECOVERY: &str = ".gpui-shutdown-pending";
type Drain = Box<dyn FnOnce() -> Result<()> + Send>;

#[derive(Clone, Default)]
pub(crate) struct Barrier(Arc<Mutex<Vec<(&'static str, Drain)>>>);
impl gpui::Global for Barrier {}
impl Barrier {
    pub(crate) fn register(
        &self,
        label: &'static str,
        drain: impl FnOnce() -> Result<()> + Send + 'static,
    ) {
        self.0.lock().unwrap().push((label, Box::new(drain)));
    }
    fn finish_with_timeout(&self, timeout: Duration) -> Result<()> {
        let mut errors = Vec::new();
        let drains = std::mem::take(&mut *self.0.lock().unwrap());
        for (label, drain) in drains {
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name(format!("gpui-{label}"))
                .spawn(move || {
                    let _ = send.send(drain());
                });
            let result = match spawned {
                Ok(_) => receive
                    .recv_timeout(timeout)
                    .with_context(|| {
                        format!(
                            "shutdown phase {label}: timeout or worker stopped after {timeout:?}"
                        )
                    })
                    .and_then(|result| result.with_context(|| format!("shutdown phase {label}"))),
                Err(error) => Err(error).with_context(|| format!("start shutdown phase {label}")),
            };
            if let Err(error) = result {
                eprintln!("{error:#}");
                errors.push(format!("{error:#}"));
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}

pub(crate) struct Lifetime {
    pub(crate) lock: Arc<StateLock>,
    pub(crate) barrier: Barrier,
    marker: PathBuf,
    roots: Arc<Preview>,
    active: bool,
}
impl Lifetime {
    pub(crate) fn acquire(roots: &Arc<Preview>) -> Result<Self> {
        roots.create_private_dir(roots.root())?;
        roots.scoped(&roots.root().join(gitterm_core::state_lock::LOCK_FILE))?;
        let lock = Arc::new(StateLock::acquire(roots.root())?);
        let marker = roots.scoped(&roots.root().join(RECOVERY))?;
        let recovering = marker.try_exists()?;
        if recovering {
            let prior = gitterm_core::tasks::TaskStore::load(roots.scoped(&roots.root().join("tasks.json"))?)
                .with_context(|| format!("refuse GPUI startup: unresolved prior shutdown {}; inspect prior launch identities", marker.display()))?;
            for task in prior.tasks() {
                for session in &task.sessions {
                    let Some(identity) = &session.launch_identity else {
                        continue;
                    };
                    let process = gitterm_core::worker_process::macos::info(identity.pid)
                        .map_err(anyhow::Error::msg)
                        .context("inspect prior GPUI launch identity")?;
                    ensure!(
                        process
                            .is_none_or(|process| process.start_us.to_string() != identity.birth),
                        "refuse GPUI startup: unresolved prior shutdown {}; recorded launch PID {} is still alive (task '{}', session '{}')",
                        marker.display(),
                        identity.pid,
                        task.title,
                        session.label
                    );
                }
            }
        }
        let profile = roots.scoped(&roots.root().join("browser-profile"))?;
        gitterm_browser::browser_control::verify_profile_released(&profile)?;
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if recovering {
            options.truncate(true);
        } else {
            options.create_new(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(&marker).and_then(|mut file| file.write_all(b"GPUI owner active; startup checks recorded launch identities and Chrome ownership\n"))
            .with_context(|| format!("record GPUI lifetime {}", marker.display()))?;
        if recovering {
            eprintln!(
                "GitTerm GPUI recovered prior unclean shutdown: {}",
                roots.root().display()
            );
        }
        Ok(Self {
            lock,
            barrier: Barrier::default(),
            marker,
            roots: roots.clone(),
            active: true,
        })
    }
    pub(crate) fn finish(self) -> Result<()> {
        self.finish_with_timeout(DRAIN_TIMEOUT)
    }
    fn finish_with_timeout(mut self, timeout: Duration) -> Result<()> {
        self.finish_inner(timeout)
    }
    fn finish_inner(&mut self, timeout: Duration) -> Result<()> {
        // Do not retry failed drains in Drop: retain the marker on failure.
        self.active = false;
        self.barrier.finish_with_timeout(timeout)?;
        self.roots.scoped(&self.marker)?;
        gitterm_browser::browser_control::verify_profile_released(
            &self
                .roots
                .scoped(&self.lock.root().join("browser-profile"))?,
        )?;
        std::fs::remove_file(&self.marker)
            .with_context(|| format!("clear GPUI recovery marker {}", self.marker.display()))?;
        // The guard drops after all completion phases.
        Ok(())
    }
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        // Covers every early return during startup, before GPUI owns the guard.
        if self.active
            && let Err(error) = self.finish_inner(DRAIN_TIMEOUT)
        {
            eprintln!(
                "release GPUI lifetime {}: {error:#}",
                self.roots.root().display()
            );
        }
    }
}

#[derive(Clone)]
pub(crate) struct Shutdown {
    owner: std::rc::Rc<std::cell::RefCell<Option<Lifetime>>>,
    stopping: std::rc::Rc<std::cell::Cell<bool>>,
}
impl gpui::Global for Shutdown {}
impl Shutdown {
    pub(crate) fn new(lifetime: Lifetime) -> Self {
        Self {
            owner: std::rc::Rc::new(std::cell::RefCell::new(Some(lifetime))),
            stopping: Default::default(),
        }
    }
    pub(crate) fn finish(&self) -> Result<()> {
        if let Some(lifetime) = self.owner.borrow_mut().take() {
            lifetime.finish()?;
        }
        Ok(())
    }
}

pub(crate) fn shutdown(cx: &mut gpui::App) -> bool {
    let shutdown = cx.try_global::<Shutdown>().cloned();
    if shutdown
        .as_ref()
        .is_some_and(|shutdown| shutdown.stopping.replace(true))
    {
        return false;
    }
    // Callbacks still have main-thread entity access here. They stop admission,
    // detach PTYs and start reapers. The barrier waits only on independent work.
    cx.shutdown();
    if let Some(shutdown) = shutdown
        && let Err(error) = shutdown.finish()
    {
        eprintln!("GitTerm GPUI shutdown: {error:#}");
    }
    true
}

pub(crate) fn quit(cx: &mut gpui::App) {
    // Action dispatch may be borrowing a window or entity. Defer until that
    // borrow ends so shutdown callbacks can update every owner and PTY.
    cx.defer(|cx| {
        if shutdown(cx) {
            cx.quit();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::preview::ProtectedRoots;
    fn roots(dir: &std::path::Path) -> Arc<Preview> {
        Arc::new(
            Preview::validate(
                &dir.join("preview"),
                ProtectedRoots::new(&dir.join("home"), []).unwrap(),
            )
            .unwrap(),
        )
    }
    #[test]
    fn startup_child() {
        let Some(dir) = std::env::var_os("GITTERM_GPUI_LIFETIME_CHILD") else {
            return;
        };
        let roots = roots(&PathBuf::from(dir));
        if std::env::var_os("GITTERM_GPUI_SURVIVOR").is_some() {
            use std::io::Write as _;
            gitterm_core::worker_process::record_process(
                &roots.root().join("survivor-launch.json"),
                std::process::id(),
            )
            .unwrap();
            println!("SURVIVOR-READY");
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            return;
        }
        let error = Lifetime::acquire(&roots)
            .err()
            .expect("replacement started")
            .to_string();
        assert!(
            error.contains("already in use") || error.contains("unresolved prior shutdown"),
            "{error}"
        );
        assert!(!roots.root().join("writer-started").exists());
    }
    fn replacement(dir: &std::path::Path) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lifetime::tests::startup_child", "--nocapture"])
            .env("GITTERM_GPUI_LIFETIME_CHILD", dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn contention_refuses_before_writer_or_seed_and_other_root_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let lifetime = Lifetime::acquire(&roots).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(roots.root())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
        let tasks = roots.root().join("tasks.json");
        std::fs::write(&tasks, b"unchanged").unwrap();
        let before = std::fs::read(&tasks).unwrap();
        replacement(dir.path());
        assert_eq!(std::fs::read(&tasks).unwrap(), before);
        assert!(!roots.root().join(".writer").exists());
        let other = tempfile::tempdir().unwrap();
        Lifetime::acquire(&self::roots(other.path()))
            .unwrap()
            .finish()
            .unwrap();
        lifetime.finish().unwrap();
    }
    #[test]
    fn lock_outlives_delayed_chat_and_reaper_completion() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let lifetime = Lifetime::acquire(&roots).unwrap();
        let (begin, begun) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        lifetime.barrier.register("chat-runtimes", move || {
            begin.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        });
        let (reap, reaping) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        lifetime.barrier.register("task-host-reaper", move || {
            reap.send(()).unwrap();
            completed.recv().unwrap();
            Ok(())
        });
        let finish = std::thread::spawn(move || lifetime.finish());
        begun.recv_timeout(Duration::from_secs(5)).unwrap();
        replacement(dir.path());
        release.send(()).unwrap();
        reaping.recv_timeout(Duration::from_secs(5)).unwrap();
        replacement(dir.path());
        done.send(()).unwrap();
        finish.join().unwrap().unwrap();
        StateLock::acquire(roots.root()).unwrap();
        assert!(!roots.root().join(RECOVERY).exists());
    }
    #[test]
    fn timeout_is_labelled_retains_marker_and_allows_recovery_without_live_launches() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let lifetime = Lifetime::acquire(&roots).unwrap();
        let (release, released) = std::sync::mpsc::channel();
        lifetime.barrier.register("delayed-reaper", move || {
            let _ = released.recv();
            Ok(())
        });
        let began = std::time::Instant::now();
        let error = lifetime
            .finish_with_timeout(Duration::from_millis(30))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("delayed-reaper") && error.contains("timeout"),
            "{error}"
        );
        assert!(began.elapsed() < Duration::from_secs(5));
        StateLock::acquire(roots.root()).unwrap();
        assert!(roots.root().join(RECOVERY).exists());
        release.send(()).unwrap();
        Lifetime::acquire(&roots).unwrap().finish().unwrap();
    }
    #[test]
    fn crash_refuses_live_identity_recovers_dead_or_reused_pid_and_preserves_stale_profile() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        // Simulate loss of the app guard, with a recorded child surviving it.
        let lifetime = Lifetime::acquire(&roots).unwrap();
        let tasks = roots.root().join("tasks.json");
        let identities = br#"{"schema_version":1,"tasks":[],"delegations":[]}"#;
        std::fs::write(&tasks, identities).unwrap();
        let mut survivor = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lifetime::tests::startup_child", "--nocapture"])
            .env("GITTERM_GPUI_LIFETIME_CHILD", dir.path())
            .env("GITTERM_GPUI_SURVIVOR", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::BufRead as _;
        let mut output = std::io::BufReader::new(survivor.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(output.read_line(&mut line).unwrap(), 0, "survivor exited");
            if line.contains("SURVIVOR-READY") {
                break;
            }
        }
        let identity = serde_json::from_slice(
            &std::fs::read(roots.root().join("survivor-launch.json")).unwrap(),
        )
        .unwrap();
        let mut task = crate::tasks::fixture_task("orphan", "Orphan fixture", "task/orphan");
        task.sessions.push(gitterm_core::tasks::TaskSessionRecord {
            task_session_id: "survivor".into(),
            label: "fixture".into(),
            harness: None,
            conversation: None,
            objective_delivery: Default::default(),
            launch_identity: Some(identity),
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
        });
        let identities = serde_json::to_vec(&gitterm_core::tasks::TaskStoreDocument {
            tasks: vec![task],
            ..Default::default()
        })
        .unwrap();
        std::fs::write(&tasks, &identities).unwrap();
        let mut lifetime = lifetime;
        lifetime.active = false; // Simulate process death, bypassing clean Drop.
        drop(lifetime);
        replacement(dir.path());
        let error = Lifetime::acquire(&roots).err().unwrap().to_string();
        assert!(
            error.contains(&format!("PID {} is still alive", survivor.id())),
            "{error}"
        );
        // A live PID with a different birth time is not the recorded launch.
        let mut reused: gitterm_core::tasks::TaskStoreDocument =
            serde_json::from_slice(&identities).unwrap();
        reused.tasks[0].sessions[0]
            .launch_identity
            .as_mut()
            .unwrap()
            .birth = "0".into();
        std::fs::write(&tasks, serde_json::to_vec(&reused).unwrap()).unwrap();
        Lifetime::acquire(&roots).unwrap().finish().unwrap();
        std::fs::write(&tasks, &identities).unwrap();
        std::fs::write(roots.root().join(RECOVERY), b"simulated crash").unwrap();
        assert!(
            survivor.try_wait().unwrap().is_none(),
            "refusal must not signal the survivor"
        );
        drop(survivor.stdin.take());
        assert!(survivor.wait().unwrap().success());
        assert_eq!(std::fs::read(&tasks).unwrap(), identities);
        // A dead recorded launch no longer blocks replacement; stale singleton
        // files belong to Chrome and must survive both startup and shutdown.
        let profile = roots.root().join("browser-profile");
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(profile.join("SingletonLock"), b"broken fixture").unwrap();
        Lifetime::acquire(&roots).unwrap().finish().unwrap();
        assert!(profile.join("SingletonLock").exists());
        assert_eq!(std::fs::read(&tasks).unwrap(), identities);
    }

    #[test]
    fn startup_error_releases_marker_and_drains_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let drained = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = (|| -> Result<()> {
            let lifetime = Lifetime::acquire(&roots)?;
            let drained = drained.clone();
            lifetime.barrier.register("startup-writer", move || {
                drained.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            });
            anyhow::bail!("startup rejected folders")
        })();
        assert!(result.is_err());
        assert!(drained.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!roots.root().join(RECOVERY).exists());
        Lifetime::acquire(&roots).unwrap().finish().unwrap();
    }

    #[gpui::test]
    fn quit_action_prepares_entities_and_drains_before_platform_quit(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let lifetime = Lifetime::acquire(&roots).unwrap();
        let barrier = lifetime.barrier.clone();
        let app = cx.new_app();
        let shutdown = Shutdown::new(lifetime);
        let surviving = shutdown.clone();
        let prepared = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = prepared.clone();
        app.update(|cx| {
            cx.set_global(barrier.clone());
            cx.set_global(shutdown);
            cx.on_app_quit(move |_| {
                observed.set(true);
                barrier.register("entity-prepared", || Ok(()));
                std::future::ready(())
            })
            .detach();
            crate::install_app_menus(cx);
            cx.dispatch_action(&crate::app_menus::Quit);
        });
        assert!(prepared.get());
        assert!(surviving.owner.borrow().is_none());
        assert!(!roots.root().join(RECOVERY).exists());
        StateLock::acquire(roots.root()).unwrap();
    }
}
