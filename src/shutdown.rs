//! Process lifetime accounting for Iced saves and session joins.
//! Tickets are acquired before dispatch, and survive until completion is
//! reduced on the UI thread (or a blocking worker actually returns).
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Write,
    Session,
}

#[derive(Debug, Default)]
struct Pending {
    next: u64,
    operations: BTreeMap<u64, (Kind, String)>,
    deadline: Option<Instant>,
}

#[derive(Debug, Default, Clone)]
pub struct Operations(Arc<(Mutex<Pending>, Condvar)>);

#[derive(Debug)]
pub struct Ticket {
    operations: Operations,
    id: u64,
    failed: std::sync::atomic::AtomicBool,
}

impl Operations {
    pub fn begin(&self, timeout: Duration) -> bool {
        let mut state = self.0 .0.lock().unwrap();
        if state.deadline.is_some() {
            return false;
        }
        state.deadline = Some(Instant::now() + timeout);
        true
    }

    pub fn started(&self) -> bool {
        self.0 .0.lock().unwrap().deadline.is_some()
    }

    pub fn expired(&self) -> bool {
        self.0
             .0
            .lock()
            .unwrap()
            .deadline
            .is_some_and(|at| Instant::now() >= at)
    }

    pub fn ticket(&self, kind: Kind, name: impl Into<String>) -> Arc<Ticket> {
        let mut state = self.0 .0.lock().unwrap();
        let id = state.next;
        state.next += 1;
        state.operations.insert(id, (kind, name.into()));
        Arc::new(Ticket {
            operations: self.clone(),
            id,
            failed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn pending(&self, kind: Kind) -> bool {
        self.0
             .0
            .lock()
            .unwrap()
            .operations
            .values()
            .any(|(k, _)| *k == kind)
    }

    pub fn unfinished(&self) -> Vec<String> {
        self.0
             .0
            .lock()
            .unwrap()
            .operations
            .values()
            .map(|(_, name)| name.clone())
            .collect()
    }

    pub fn log_timeout(&self, mut log: impl FnMut(&str)) {
        for name in self.unfinished() {
            log(&format!("[shutdown] timed out waiting for {name}"));
        }
    }

    /// The final main() boundary, also used if the window runtime exits with
    /// an error. A single deadline includes all earlier UI shutdown phases.
    pub fn wait(&self) {
        self.begin(TIMEOUT);
        let mut state = self.0 .0.lock().unwrap();
        while !state.operations.is_empty() {
            let left = state
                .deadline
                .unwrap()
                .saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(state);
                self.log_timeout(|line| eprintln!("{line}"));
                return;
            }
            state = self.0 .1.wait_timeout(state, left).unwrap().0;
        }
    }
}

impl Ticket {
    pub fn complete(&self) {
        if self.failed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.operations
            .0
             .0
            .lock()
            .unwrap()
            .operations
            .remove(&self.id);
        self.operations.0 .1.notify_all();
    }

    pub fn fail(&self, error: impl std::fmt::Display) {
        self.failed.store(true, std::sync::atomic::Ordering::SeqCst);
        eprintln!("[shutdown] {error}");
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.failed.load(std::sync::atomic::Ordering::SeqCst) || std::thread::panicking() {
            return;
        }
        self.operations
            .0
             .0
            .lock()
            .unwrap()
            .operations
            .remove(&self.id);
        self.operations.0 .1.notify_all();
    }
}

pub fn operations() -> Operations {
    static OPERATIONS: OnceLock<Operations> = OnceLock::new();
    OPERATIONS.get_or_init(Operations::default).clone()
}

/// Keep joins off the UI thread. Accounting includes the closure's teardown.
pub fn spawn(
    kind: Kind,
    name: impl Into<String>,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) {
    let name = name.into();
    let ticket = operations().ticket(kind, name.clone());
    let running = ticket.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            if let Err(error) = work() {
                running.fail(error);
            }
        })
    {
        ticket.fail(format!("start {name}: {error}"));
    }
}

/// Kept until main's final barrier. The handler only sets an atomic flag.
static SIGNAL_QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DESKTOP_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn activate_desktop() {
    DESKTOP_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, quit_signal as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGTERM,
            quit_signal as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(unix)]
extern "C" fn quit_signal(_: libc::c_int) {
    SIGNAL_QUIT.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn signal_requested() -> bool {
    SIGNAL_QUIT.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn desktop_active() -> bool {
    DESKTOP_ACTIVE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Unlike a future's ticket, this one cannot disappear when Iced cancels the
/// awaiting task while spawn_blocking continues to write in the background.
pub async fn blocking<T: Send + 'static>(
    name: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let ticket = operations().ticket(Kind::Write, name);
    tokio::task::spawn_blocking(move || {
        let result = work();
        drop(ticket);
        result
    })
    .await
}

/// External children retain the desktop's existing drop policy. They never
/// get tickets in the bounded in-process wait.
#[derive(Debug, Clone)]
pub struct OwnedChild {
    pub kind: String,
    pub task_id: Option<String>,
    pub delegation_id: Option<String>,
    pub identity: Option<gitterm_core::tasks::LaunchIdentity>,
}

impl OwnedChild {
    pub fn log_if_running(&self, mut log: impl FnMut(&str)) {
        #[cfg(target_os = "macos")]
        if let Some(identity) = &self.identity {
            match gitterm_core::worker_process::macos::info(identity.pid) {
                Ok(Some(process)) if process.start_us.to_string() == identity.birth => {}
                Ok(_) => return,
                Err(error) => log(&format!("[shutdown] inspect owned child: {error}")),
            }
        }
        log(&format!(
            "[shutdown] owned child still running: kind={} task={} delegation={} launch={:?}",
            self.kind,
            self.task_id.as_deref().unwrap_or("unknown"),
            self.delegation_id.as_deref().unwrap_or("unknown"),
            self.identity,
        ));
    }
}

fn children() -> &'static Mutex<Vec<OwnedChild>> {
    static CHILDREN: OnceLock<Mutex<Vec<OwnedChild>>> = OnceLock::new();
    CHILDREN.get_or_init(Mutex::default)
}

pub fn record_children(owned: Vec<OwnedChild>) {
    children().lock().unwrap().extend(owned);
}

pub fn log_children() {
    for child in children().lock().unwrap().drain(..) {
        child.log_if_running(|line| eprintln!("{line}"));
    }
}

/// Tokio's default Runtime destructor waits forever for blocking jobs. Iced
/// must not enter that destructor on a timed-out quit. This runtime lives to
/// process exit; main's barrier supplies its bounded write/join boundary.
pub struct Executor(&'static tokio::runtime::Runtime);
impl Executor {
    pub fn runtime() -> Result<&'static tokio::runtime::Runtime, std::io::Error> {
        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        if let Some(runtime) = RUNTIME.get() {
            return Ok(runtime);
        }
        let runtime = tokio::runtime::Runtime::new()?;
        // Only the desktop main thread initializes this runtime.
        let _ = RUNTIME.set(runtime);
        Ok(RUNTIME.get().unwrap())
    }
}
impl iced::Executor for Executor {
    fn new() -> Result<Self, std::io::Error> {
        Self::runtime().map(Self)
    }
    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        drop(self.0.spawn(future));
    }
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.0.block_on(future)
    }
    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _entered = self.0.enter();
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::state_lock::{LockError, StateLock};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    fn until(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !path.exists() {
            assert!(Instant::now() < deadline, "waiting for {}", path.display());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn helper(root: &Path, mode: &str) -> Child {
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "shutdown::tests::lock_helper", "--nocapture"])
            .env("GITTERM_V6_CONFIG_DIR", root)
            .env("HOME", home)
            .env("GITTERM_V6_ICED_LOCK_HELPER", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn lock_helper() {
        let Ok(mode) = std::env::var("GITTERM_V6_ICED_LOCK_HELPER") else {
            return;
        };
        let root = gitterm_core::config_paths::desktop_config_dir().unwrap();
        assert!(root.is_absolute());
        if mode == "contender" {
            assert!(matches!(
                StateLock::acquire(root),
                Err(LockError::Busy { .. })
            ));
            std::fs::write(root.join("blocked"), "yes").unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                match StateLock::acquire(root) {
                    Ok(_lock) => {
                        if root.join("expect-save").exists() {
                            assert_eq!(
                                std::fs::read_to_string(root.join("saved")).unwrap(),
                                "complete"
                            );
                        }
                        std::fs::write(root.join("acquired"), "yes").unwrap();
                        break;
                    }
                    Err(LockError::Busy { .. }) => assert!(Instant::now() < deadline),
                    Err(error) => panic!("{error}"),
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        let lock = StateLock::acquire(root).unwrap();
        let operations = Operations::default();
        let ticket = operations.ticket(Kind::Write, "delayed facts-index save");
        if mode == "timeout" {
            std::fs::write(root.join("ready"), "yes").unwrap();
            until(&root.join("blocked"));
            operations.begin(Duration::from_millis(150));
            operations.wait();
            assert!(!operations.unfinished().is_empty());
            drop(lock);
            until(&root.join("acquired"));
            drop(ticket);
        } else {
            let root = root.to_owned();
            let writer = std::thread::spawn(move || {
                until(&root.join("allow-save"));
                std::fs::write(root.join("saved"), "complete").unwrap();
                drop(ticket);
            });
            std::fs::write(lock.root().join("ready"), "yes").unwrap();
            operations.wait();
            writer.join().unwrap();
            drop(lock);
        }
    }

    #[test]
    fn delayed_save_holds_state_lock_until_writer_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("expect-save"), "yes").unwrap();
        let owner = helper(root, "save");
        until(&root.join("ready"));
        let contender = helper(root, "contender");
        until(&root.join("blocked"));
        assert!(!root.join("acquired").exists());
        assert!(!root.join("saved").exists());
        std::fs::write(root.join("allow-save"), "yes").unwrap();
        for child in [owner, contender] {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(root.join("acquired").exists());
    }

    #[test]
    fn timeout_releases_state_lock_and_names_unfinished_save() {
        let dir = tempfile::tempdir().unwrap();
        let owner = helper(dir.path(), "timeout");
        until(&dir.path().join("ready"));
        let contender = helper(dir.path(), "contender");
        let output = owner.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("[shutdown] timed out waiting for delayed facts-index save"),
            "{stderr}"
        );
        let output = contender.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn failed_step_remains_unfinished_until_the_deadline() {
        let jobs = Operations::default();
        jobs.begin(Duration::from_millis(30));
        let ticket = jobs.ticket(Kind::Session, "failed session join");
        ticket.fail("synthetic join failure");
        ticket.complete();
        drop(ticket);
        assert_eq!(jobs.unfinished(), ["failed session join"]);
        jobs.wait();
        assert!(jobs.expired());
        let mut logs = Vec::new();
        jobs.log_timeout(|line| logs.push(line.to_owned()));
        assert!(logs[0].contains("failed session join"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_child_at_quit_is_logged_without_being_killed_or_waited_on() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let info = gitterm_core::worker_process::macos::info(child.id())
            .unwrap()
            .unwrap();
        let owned = OwnedChild {
            kind: "worker PTY".into(),
            task_id: Some("task-fixture".into()),
            delegation_id: Some("delegation-fixture".into()),
            identity: Some(gitterm_core::tasks::LaunchIdentity {
                pid: child.id(),
                birth: info.start_us.to_string(),
            }),
        };
        let jobs = Operations::default();
        jobs.wait();
        let mut logs = Vec::new();
        owned.log_if_running(|line| logs.push(line.to_owned()));
        assert!(logs[0].contains("kind=worker PTY task=task-fixture delegation=delegation-fixture"));
        assert!(logs[0].contains(&child.id().to_string()));
        assert!(
            child.try_wait().unwrap().is_none(),
            "logging must leave the child running"
        );
        // Only this test owns and cleans up its synthetic child.
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn one_deadline_and_completion_ticket_includes_result_handling() {
        let jobs = Operations::default();
        assert!(jobs.begin(Duration::from_secs(1)));
        assert!(!jobs.begin(Duration::from_secs(30)));
        let running = jobs.ticket(Kind::Write, "PR watch save");
        let result = running.clone();
        drop(running);
        assert!(jobs.pending(Kind::Write));
        let follow_up = jobs.ticket(Kind::Write, "PR watch save-again");
        drop(result);
        assert!(jobs.pending(Kind::Write));
        drop(follow_up);
        assert!(jobs.unfinished().is_empty());
    }
}
