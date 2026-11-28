//! Per-launch ownership of a task terminal's agent and its descendants.
//! The terminal crate exposes no child PID; the launch command runs the agent
//! in a forked subshell and records that subshell in a private directory. The
//! terminal's interactive shell is never recorded, so once the agent exits the
//! identity is gone and nothing the user runs next can match it. Shutdown
//! never selects processes by name.
use std::path::PathBuf;
use std::time::Duration;

pub const PID_FILE_ENV: &str = "GITTERM_V6_WORKER_PID_FILE";
pub const RECORD_LAUNCH_ARG: &str = "--record-worker-launch";

pub use crate::tasks::model::LaunchIdentity;

pub struct WorkerProcess {
    dir: PathBuf,
    pub persisted_identity: Option<LaunchIdentity>,
}

impl WorkerProcess {
    pub fn new() -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("gitterm-v6-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)
            .map_err(|e| format!("create worker process directory {}: {e}", dir.display()))?;
        Ok(Self {
            dir,
            persisted_identity: None,
        })
    }

    pub fn pid_file(&self) -> PathBuf {
        self.dir.join("launch.json")
    }

    pub fn launch_identity(&self) -> Result<Option<LaunchIdentity>, String> {
        let path = self.pid_file();
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "read worker launch identity {}: {e}",
                    path.display()
                ))
            }
        };
        serde_json::from_str(&contents)
            .map(Some)
            .map_err(|e| format!("decode worker launch identity {}: {e}", path.display()))
    }

    /// Revoke an unpublished launch before its terminal is dropped. Moving
    /// the recorder directory closes the startup race: a late recorder cannot
    /// rename its record, so record_shell's `&&` never starts the harness.
    /// Capture descendants while ancestry is intact; shutdown then uses the
    /// same birth-checked TERM/grace/KILL path as a published worker.
    pub fn discard_unpublished(
        mut self,
    ) -> Result<Option<std::thread::JoinHandle<Result<(), String>>>, String> {
        let revoked = self.dir.with_extension("revoked");
        std::fs::rename(&self.dir, &revoked)
            .map_err(|e| format!("revoke worker recorder {}: {e}", self.dir.display()))?;
        self.dir = revoked;
        let Some(identity) = self.launch_identity()? else {
            return Ok(None);
        };
        let tree = owned_tree(&SystemProcesses, Some(&identity))?;
        let stop = std::thread::Builder::new()
            .name("unpublished-worker-stop".into())
            .spawn(move || {
                let _record = self;
                stop_captured_tree(&SystemProcesses, &tree)
            })
            .map_err(|e| format!("start unpublished worker shutdown: {e}"))?;
        Ok(Some(stop))
    }

    pub fn stop(self) -> Result<(), String> {
        stop_on_platform(
            &SystemProcesses,
            self.persisted_identity.as_ref(),
            cfg!(windows),
        )
    }
}

/// Runs before app initialization, from inside the subshell that will run
/// the harness. Records that subshell (the recorder's parent), never the
/// terminal's interactive shell: a parent equal to `terminal_shell` means no
/// subshell was forked, and the launch is refused before the agent starts.
pub fn record_launch(path: &std::path::Path, terminal_shell: u32) -> Result<(), String> {
    let launch = SystemProcesses
        .current(std::process::id())?
        .ok_or("worker launch recorder process is missing")?
        .parent;
    if launch == terminal_shell {
        return Err(format!(
            "refuse worker launch: the agent would run in the terminal shell {terminal_shell}, not a subshell of its own"
        ));
    }
    record_process(path, launch)
}

/// Persists `pid`'s native birth identity; the agent waits for this record.
pub fn record_process(path: &std::path::Path, pid: u32) -> Result<(), String> {
    let control = SystemProcesses;
    let process = control
        .current(pid)?
        .ok_or_else(|| format!("worker launch process {pid} is missing"))?;
    let identity = LaunchIdentity {
        pid,
        birth: process.birth,
    };
    let bytes =
        serde_json::to_vec(&identity).map_err(|e| format!("encode worker launch identity: {e}"))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|e| format!("persist worker launch identity {}: {e}", path.display()))
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "[tasks] remove worker process directory {}: {e}",
                self.dir.display()
            );
        }
    }
}

/// Called after provider-specific MCP configuration, so the wrapper never
/// changes harness detection or the persisted resume command.
pub fn record_shell(command: &str, path: &str) -> Result<String, String> {
    // shortcut: Windows records nothing and its finished workers are never
    // stopped automatically (see `stop_on_platform`); revisit with job objects.
    if cfg!(windows) {
        return Ok(command.to_string());
    }
    let executable =
        std::env::current_exe().map_err(|e| format!("locate worker launch recorder: {e}"))?;
    Ok(record_shell_with_executable(
        command,
        path,
        &executable.to_string_lossy(),
    ))
}

/// zsh and bash (the only shells that run a startup command) fork `( ... )`,
/// so the recorder's parent is the subshell that runs the agent and exits
/// with it. `$$` stays the interactive shell's PID inside the subshell.
fn record_shell_with_executable(command: &str, path: &str, executable: &str) -> String {
    let path = format!("'{}'", path.replace('\'', "'\\''"));
    let executable = format!("'{}'", executable.replace('\'', "'\\''"));
    format!("( {executable} {RECORD_LAUNCH_ARG} {path} \"$$\" && {{\n{command}\n}} )")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Process {
    pid: u32,
    parent: u32,
    birth: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    Terminate,
    Kill,
}

trait ProcessControl {
    fn snapshot(&self) -> Result<Vec<Process>, String>;
    fn current(&self, pid: u32) -> Result<Option<Process>, String>;
    fn signal(&self, pid: u32, signal: Signal) -> Result<(), String>;
    fn grace(&self) {
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Windows refuses before inspecting anything: `ParentProcessId` survives its
/// creator's exit, so a reused PID would adopt unrelated processes. The
/// caller keeps the tab open when this refuses.
fn stop_on_platform(
    control: &impl ProcessControl,
    identity: Option<&LaunchIdentity>,
    windows: bool,
) -> Result<(), String> {
    if windows {
        return Err(
            "refuse worker shutdown: automatic shutdown is not supported on Windows".into(),
        );
    }
    stop_tree(control, identity)
}

fn stop_tree(
    control: &impl ProcessControl,
    identity: Option<&LaunchIdentity>,
) -> Result<(), String> {
    let tree = owned_tree(control, identity)?;
    stop_captured_tree(control, &tree)
}

fn owned_tree(
    control: &impl ProcessControl,
    identity: Option<&LaunchIdentity>,
) -> Result<Vec<Process>, String> {
    let identity = identity.ok_or("refuse worker shutdown: missing persisted launch identity")?;
    let root = identity.pid;
    let snapshot = control.snapshot()?;
    if !snapshot
        .iter()
        .any(|p| p.pid == root && p.birth == identity.birth)
    {
        return Err(format!(
            "refuse worker shutdown: launch identity for PID {root} no longer matches"
        ));
    }
    // Check ancestry without granting authority to any sibling session.
    let mut owned = vec![std::process::id()];
    let mut tree = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = owned.pop() {
        if !seen.insert(parent) {
            continue;
        }
        for child in snapshot.iter().filter(|p| p.parent == parent) {
            if child.pid == root || tree.iter().any(|p: &Process| p.pid == parent) {
                tree.push(child.clone());
            }
            owned.push(child.pid);
        }
    }
    if !tree.iter().any(|p| p.pid == root) {
        return Err(format!(
            "refuse worker shutdown: PID {root} is no longer owned by this GitTerm"
        ));
    }
    if control
        .current(root)?
        .is_none_or(|p| p.birth != identity.birth)
    {
        return Err(format!(
            "refuse worker shutdown: launch identity for PID {root} changed during inspection"
        ));
    }
    Ok(tree)
}

fn stop_captured_tree(control: &impl ProcessControl, tree: &[Process]) -> Result<(), String> {
    let mut errors = Vec::new();
    for signal in [Signal::Terminate, Signal::Kill] {
        for process in tree.iter().rev() {
            match control.current(process.pid) {
                Ok(Some(current)) if current.birth == process.birth => {
                    if let Err(e) = control.signal(process.pid, signal) {
                        errors.push(e);
                    }
                }
                Ok(_) => {}
                Err(e) => errors.push(e),
            }
        }
        if signal == Signal::Terminate {
            control.grace();
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

struct SystemProcesses;

#[cfg(target_os = "macos")]
impl ProcessControl for SystemProcesses {
    fn snapshot(&self) -> Result<Vec<Process>, String> {
        macos::descendants(std::process::id()).map(|all| {
            all.into_iter()
                .map(|p| Process {
                    pid: p.pid,
                    parent: p.ppid,
                    birth: p.start_us.to_string(),
                })
                .collect()
        })
    }
    fn current(&self, pid: u32) -> Result<Option<Process>, String> {
        macos::info(pid).map(|p| {
            p.map(|p| Process {
                pid: p.pid,
                parent: p.ppid,
                birth: p.start_us.to_string(),
            })
        })
    }
    fn signal(&self, pid: u32, signal: Signal) -> Result<(), String> {
        signal_unix(pid, signal)
    }
}

#[cfg(unix)]
fn signal_unix(pid: u32, signal: Signal) -> Result<(), String> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Err(format!("refuse invalid worker PID {pid}"));
    }
    let number = match signal {
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: positive, owned PID, checked against its captured birth identity.
    if unsafe { libc::kill(pid as libc::pid_t, number) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("signal worker {pid} ({signal:?}): {error}"))
    }
}

#[cfg(not(target_os = "macos"))]
impl ProcessControl for SystemProcesses {
    fn snapshot(&self) -> Result<Vec<Process>, String> {
        #[cfg(target_os = "linux")]
        {
            let mut processes = Vec::new();
            for entry in std::fs::read_dir("/proc")
                .map_err(|e| format!("list worker processes in /proc: {e}"))?
            {
                let entry = entry.map_err(|e| format!("read worker process directory: {e}"))?;
                if let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                {
                    if let Some(process) = linux_process(pid)? {
                        processes.push(process);
                    }
                }
            }
            Ok(processes)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err("worker process inspection is unsupported on this platform".into())
        }
    }
    fn current(&self, pid: u32) -> Result<Option<Process>, String> {
        #[cfg(target_os = "linux")]
        {
            linux_process(pid)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(format!(
                "worker process inspection of PID {pid} is unsupported on this platform"
            ))
        }
    }
    fn signal(&self, pid: u32, signal: Signal) -> Result<(), String> {
        #[cfg(unix)]
        {
            signal_unix(pid, signal)
        }
        #[cfg(not(unix))]
        {
            Err(format!(
                "refuse to signal worker {pid} ({signal:?}): unsupported on this platform"
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_process(pid: u32) -> Result<Option<Process>, String> {
    let path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(&path) {
        Ok(stat) => parse_linux_process(pid, &stat).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read worker identity {path}: {e}")),
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_process(pid: u32, stat: &str) -> Result<Process, String> {
    // proc_pid_stat(5): comm can contain spaces and parentheses; fields 4
    // and 22 are ppid and native start ticks, never the rounded ps time.
    let fields = stat
        .rsplit_once(") ")
        .ok_or_else(|| format!("invalid worker stat for PID {pid}"))?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let parent = fields
        .get(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("missing worker parent for PID {pid}"))?;
    let birth = fields
        .get(19)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| format!("missing worker start ticks for PID {pid}"))?
        .to_string();
    Ok(Process { pid, parent, birth })
}

/// A process as discovery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    /// The kernel's short name (`p_comm`, 16 bytes).
    pub comm: String,
    pub start_us: i64,
}

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(test)]
#[path = "../../../src/worker_process/tests.rs"]
mod tests;

/// Persist only an identity captured by this launch's recorder.
pub fn persist_identity_record(
    store: &mut crate::tasks::TaskStore,
    task: &str,
    session: &str,
    identity: &LaunchIdentity,
) -> Result<(), String> {
    let mut record = store
        .get(task)
        .and_then(|task| task.sessions.iter().find(|s| s.task_session_id == session))
        .cloned()
        .ok_or_else(|| format!("worker session {session} is missing from task {task}"))?;
    record.launch_identity = Some(identity.clone());
    store
        .upsert_session(task, record, &chrono::Utc::now().to_rfc3339())
        .map_err(|error| error.to_string())
}

/// Early executable hook: the recorder must run before any app/state setup.
pub fn handle_recorder(args: &[std::ffi::OsString]) {
    if args.first().map(|arg| arg.as_os_str()) != Some(std::ffi::OsStr::new(RECORD_LAUNCH_ARG)) {
        return;
    }
    let result = (|| {
        let path = args.get(1).ok_or("missing worker launch record path")?;
        let terminal_shell = args
            .get(2)
            .ok_or("missing worker terminal shell PID")?
            .to_string_lossy()
            .parse::<u32>()
            .map_err(|error| format!("invalid worker terminal shell PID: {error}"))?;
        record_launch(std::path::Path::new(path), terminal_shell)
    })();
    if let Err(error) = result {
        eprintln!("[tasks] {error}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
