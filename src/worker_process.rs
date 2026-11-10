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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchIdentity {
    pub pid: u32,
    pub birth: String,
}

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
        use crate::codex_exec::Processes;
        crate::codex_exec::Libproc
            .descendants(std::process::id())
            .map(|all| {
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
        use crate::codex_exec::Processes;
        crate::codex_exec::Libproc.info(pid).map(|p| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn linux_identity_uses_native_start_ticks_and_handles_process_names() {
        let stat = "10 (a name ) with (parens)) S 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1234567 0";
        let process = parse_linux_process(10, stat).unwrap();
        assert_eq!(process.parent, 7);
        assert_eq!(process.birth, "1234567");
        assert!(parse_linux_process(10, "10 (truncated) S 7").is_err());
    }

    struct ReusedPid;
    impl ProcessControl for ReusedPid {
        fn snapshot(&self) -> Result<Vec<Process>, String> {
            Ok(vec![
                Process {
                    pid: 10,
                    parent: std::process::id(),
                    birth: "another-session".into(),
                },
                Process {
                    pid: 11,
                    parent: 10,
                    birth: "another-session-child".into(),
                },
            ])
        }
        fn current(&self, pid: u32) -> Result<Option<Process>, String> {
            Ok(self.snapshot()?.into_iter().find(|p| p.pid == pid))
        }
        fn signal(&self, _: u32, _: Signal) -> Result<(), String> {
            panic!("must not signal the reused PID or any of its children")
        }
        fn grace(&self) {
            panic!("no signals means no grace wait")
        }
    }

    #[test]
    fn reused_launch_pid_cannot_authorize_shutdown_of_another_session() {
        let error = stop_tree(
            &ReusedPid,
            Some(&LaunchIdentity {
                pid: 10,
                birth: "original-worker".into(),
            }),
        )
        .unwrap_err();
        assert!(error.contains("no longer matches"));
    }

    #[test]
    fn missing_launch_identity_sends_no_signals() {
        let error = stop_tree(&ReusedPid, None).unwrap_err();
        assert!(error.contains("missing persisted launch identity"));
    }

    /// The terminal shell (10) outlived its agent: the recorded launch
    /// subshell (15) is gone and the user now runs another program (20, and a
    /// reuse of PID 15 with a new birth) in the same terminal.
    struct AgentExited;
    impl ProcessControl for AgentExited {
        fn snapshot(&self) -> Result<Vec<Process>, String> {
            Ok(vec![
                Process {
                    pid: 10,
                    parent: std::process::id(),
                    birth: "terminal-shell".into(),
                },
                Process {
                    pid: 20,
                    parent: 10,
                    birth: "user-program".into(),
                },
                Process {
                    pid: 15,
                    parent: 10,
                    birth: "user-program-on-reused-pid".into(),
                },
            ])
        }
        fn current(&self, pid: u32) -> Result<Option<Process>, String> {
            Ok(self.snapshot()?.into_iter().find(|p| p.pid == pid))
        }
        fn signal(&self, pid: u32, _: Signal) -> Result<(), String> {
            panic!("must not signal {pid}: the launched agent has exited")
        }
        fn grace(&self) {
            panic!("no signals means no grace wait")
        }
    }

    #[test]
    fn exited_agent_leaves_the_programs_now_in_its_terminal_alone() {
        let error = stop_on_platform(
            &AgentExited,
            Some(&LaunchIdentity {
                pid: 15,
                birth: "agent-launch".into(),
            }),
            false,
        )
        .unwrap_err();
        assert!(error.contains("no longer matches"), "{error}");
    }

    struct Untouchable;
    impl ProcessControl for Untouchable {
        fn snapshot(&self) -> Result<Vec<Process>, String> {
            panic!("Windows must not walk parent links")
        }
        fn current(&self, _: u32) -> Result<Option<Process>, String> {
            panic!("Windows must not inspect processes")
        }
        fn signal(&self, _: u32, _: Signal) -> Result<(), String> {
            panic!("Windows must not signal")
        }
    }

    #[test]
    fn windows_refuses_before_any_process_inspection() {
        let identity = LaunchIdentity {
            pid: 10,
            birth: "a".into(),
        };
        let error = stop_on_platform(&Untouchable, Some(&identity), true).unwrap_err();
        assert!(error.contains("not supported on Windows"), "{error}");
        assert!(stop_on_platform(&Untouchable, None, true).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn launch_wrapper_records_the_agent_subshell_not_the_terminal_shell() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("worker's pid");
        use std::os::unix::fs::PermissionsExt;
        let recorder = temp.path().join("recorder's script");
        // Stands in for `record_launch`: the recorder's parent and `$$`.
        std::fs::write(
            &recorder,
            "#!/bin/sh\nprintf '%s %s' \"$PPID\" \"$3\" > \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = record_shell_with_executable(
            "printf '%s' 'a quoted command'",
            path.to_str().unwrap(),
            recorder.to_str().unwrap(),
        );
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !std::path::Path::new(shell).exists() {
                continue;
            }
            let _ = std::fs::remove_file(&path);
            let output = std::process::Command::new(shell)
                .args(["-c", &format!("{command}; printf ' %s' \"$$\"")])
                .output()
                .unwrap();
            assert!(output.status.success(), "{shell}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let terminal = stdout.rsplit_once(' ').unwrap().1;
            assert_eq!(stdout, format!("a quoted command {terminal}"), "{shell}");
            let recorded = std::fs::read_to_string(&path).unwrap();
            let (launch, shell_pid) = recorded.split_once(' ').unwrap();
            assert_eq!(shell_pid, terminal, "{shell}: `$$` is the terminal shell");
            assert_ne!(
                launch, terminal,
                "{shell}: the agent needs its own subshell"
            );
        }
        let failed =
            record_shell_with_executable("printf 'must not start'", "unused", "/missing-recorder");
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &failed])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn launch_without_its_own_subshell_is_refused() {
        let process = WorkerProcess::new().unwrap();
        let parent = SystemProcesses
            .current(std::process::id())
            .unwrap()
            .unwrap()
            .parent;
        let error = record_launch(&process.pid_file(), parent).unwrap_err();
        assert!(error.contains("not a subshell"), "{error}");
        assert_eq!(process.launch_identity().unwrap(), None);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn launch_recorder_persists_native_birth_before_the_agent_starts() {
        let process = WorkerProcess::new().unwrap();
        record_process(&process.pid_file(), std::process::id()).unwrap();
        let identity = process.launch_identity().unwrap().unwrap();
        let current = SystemProcesses
            .current(std::process::id())
            .unwrap()
            .unwrap();
        assert_eq!(
            identity,
            LaunchIdentity {
                pid: current.pid,
                birth: current.birth
            }
        );
    }

    #[test]
    fn graceful_shutdown_only_escalates_owned_survivors_with_the_same_birth() {
        struct Mock {
            signals: RefCell<Vec<(u32, Signal)>>,
            waited: RefCell<bool>,
        }
        impl ProcessControl for Mock {
            fn snapshot(&self) -> Result<Vec<Process>, String> {
                Ok(vec![
                    Process {
                        pid: 10,
                        parent: std::process::id(),
                        birth: "a".into(),
                    },
                    Process {
                        pid: 11,
                        parent: 10,
                        birth: "b".into(),
                    },
                    Process {
                        pid: 12,
                        parent: 11,
                        birth: "c".into(),
                    },
                    Process {
                        pid: 99,
                        parent: std::process::id(),
                        birth: "unrelated".into(),
                    },
                ])
            }
            fn current(&self, pid: u32) -> Result<Option<Process>, String> {
                let mut p = self.snapshot()?.into_iter().find(|p| p.pid == pid);
                if *self.waited.borrow() {
                    if pid == 11 {
                        p = None;
                    }
                    if pid == 12 {
                        p.as_mut().unwrap().birth = "reused".into();
                    }
                }
                Ok(p)
            }
            fn signal(&self, pid: u32, signal: Signal) -> Result<(), String> {
                self.signals.borrow_mut().push((pid, signal));
                Ok(())
            }
            fn grace(&self) {
                *self.waited.borrow_mut() = true;
            }
        }
        let mock = Mock {
            signals: RefCell::new(vec![]),
            waited: RefCell::new(false),
        };
        stop_tree(
            &mock,
            Some(&LaunchIdentity {
                pid: 10,
                birth: "a".into(),
            }),
        )
        .unwrap();
        assert_eq!(
            *mock.signals.borrow(),
            vec![
                (12, Signal::Terminate),
                (11, Signal::Terminate),
                (10, Signal::Terminate),
                (10, Signal::Kill)
            ]
        );
        mock.signals.borrow_mut().clear();
        assert!(stop_tree(
            &mock,
            Some(&LaunchIdentity {
                pid: 1234,
                birth: "a".into()
            })
        )
        .is_err());
        assert!(stop_tree(
            &mock,
            Some(&LaunchIdentity {
                pid: 10,
                birth: "original-before-PID-reuse".into()
            })
        )
        .is_err());
        assert!(stop_tree(&mock, None).is_err());
        assert!(mock.signals.borrow().is_empty());
    }
}
