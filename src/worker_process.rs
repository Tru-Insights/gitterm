//! Per-launch ownership of a task terminal's shell and agent descendants.
//! The terminal crate exposes no child PID; the launch command records its
//! shell in a private directory. Shutdown never selects processes by name.
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
        stop_tree(&SystemProcesses, self.persisted_identity.as_ref())
    }
}

/// Runs before app initialization and before the harness command. The shell
/// waits for this durable launch record; shutdown never discovers authority.
pub fn record_launch(path: &std::path::Path, pid: Option<u32>) -> Result<(), String> {
    let control = SystemProcesses;
    let pid = match pid {
        Some(pid) => pid,
        None => {
            control
                .current(std::process::id())?
                .ok_or("worker launch recorder process is missing")?
                .parent
        }
    };
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
pub fn record_shell(command: &str, shell: &str, path: &str) -> Result<String, String> {
    let executable =
        std::env::current_exe().map_err(|e| format!("locate worker launch recorder: {e}"))?;
    Ok(record_shell_with_executable(
        command,
        shell,
        path,
        &executable.to_string_lossy(),
    ))
}

fn record_shell_with_executable(
    command: &str,
    shell: &str,
    path: &str,
    executable: &str,
) -> String {
    if cfg!(windows) {
        if shell.to_lowercase().contains("powershell") || shell.to_lowercase().contains("pwsh") {
            let path = path.replace('\'', "''");
            let executable = executable.replace('\'', "''");
            format!("& '{executable}' {RECORD_LAUNCH_ARG} '{path}' $PID; if ($LASTEXITCODE -eq 0) {{ {command} }}")
        } else {
            format!("\"{executable}\" {RECORD_LAUNCH_ARG} \"{path}\" && {command}")
        }
    } else {
        let _ = shell;
        let path = format!("'{}'", path.replace('\'', "'\\''"));
        let executable = format!("'{}'", executable.replace('\'', "'\\''"));
        format!("{executable} {RECORD_LAUNCH_ARG} {path} \"$$\" && {{\n{command}\n}}")
    }
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
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            Err("worker launch identity is unsupported on this Unix platform".into())
        }
        #[cfg(windows)]
        {
            let output = std::process::Command::new("powershell").args(["-NoProfile", "-Command",
                "ConvertTo-Json -Compress -InputObject @(Get-CimInstance Win32_Process | ForEach-Object { @{pid=$_.ProcessId;parent=$_.ParentProcessId;birth=$_.CreationDate.ToString('o')} })"])
                .output().map_err(|e| format!("list worker processes: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "list worker processes: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            #[derive(serde::Deserialize)]
            struct Row {
                pid: u32,
                parent: u32,
                birth: String,
            }
            let rows: Vec<Row> = serde_json::from_slice(&output.stdout)
                .map_err(|e| format!("decode worker processes: {e}"))?;
            Ok(rows
                .into_iter()
                .map(|p| Process {
                    pid: p.pid,
                    parent: p.parent,
                    birth: p.birth,
                })
                .collect())
        }
    }
    fn current(&self, pid: u32) -> Result<Option<Process>, String> {
        #[cfg(target_os = "linux")]
        {
            linux_process(pid)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(self.snapshot()?.into_iter().find(|p| p.pid == pid))
        }
    }
    fn signal(&self, pid: u32, signal: Signal) -> Result<(), String> {
        #[cfg(unix)]
        {
            signal_unix(pid, signal)
        }
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new("taskkill");
            command.args(["/PID", &pid.to_string()]);
            if signal == Signal::Kill {
                command.arg("/F");
            }
            let output = command
                .output()
                .map_err(|e| format!("stop worker {pid}: {e}"))?;
            if output.status.success() || self.current(pid)?.is_none() {
                Ok(())
            } else {
                Err(format!(
                    "stop worker {pid}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ))
            }
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

    #[test]
    #[cfg(unix)]
    fn launch_wrapper_records_its_shell_and_preserves_quoted_commands() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("worker's pid");
        use std::os::unix::fs::PermissionsExt;
        let recorder = temp.path().join("recorder's script");
        std::fs::write(&recorder, "#!/bin/sh\nprintf '%s' \"$3\" > \"$2\"\n").unwrap();
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = record_shell_with_executable(
            "printf '%s' 'a quoted command'",
            "/bin/sh",
            path.to_str().unwrap(),
            recorder.to_str().unwrap(),
        );
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"a quoted command");
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .parse::<u32>()
                .unwrap()
                > 1
        );
        let failed = record_shell_with_executable(
            "printf 'must not start'",
            "/bin/sh",
            "unused",
            "/missing-recorder",
        );
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &failed])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn launch_recorder_persists_native_birth_before_the_agent_starts() {
        let process = WorkerProcess::new().unwrap();
        record_launch(&process.pid_file(), Some(std::process::id())).unwrap();
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
