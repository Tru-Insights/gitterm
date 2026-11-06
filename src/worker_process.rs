//! Per-launch ownership of a task terminal's shell and agent descendants.
//! The terminal crate exposes no child PID; the launch command records its
//! shell in a private directory. Shutdown never selects processes by name.
use std::path::PathBuf;
use std::time::Duration;

pub const PID_FILE_ENV: &str = "GITTERM_V6_WORKER_PID_FILE";

pub struct WorkerProcess {
    dir: PathBuf,
}

impl WorkerProcess {
    pub fn new() -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("gitterm-v6-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)
            .map_err(|e| format!("create worker process directory {}: {e}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn pid_file(&self) -> PathBuf {
        self.dir.join("shell.pid")
    }

    pub fn stop(self) -> Result<(), String> {
        let path = self.pid_file();
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            // The shell has not executed the launch command. Dropping its
            // terminal prevents it from starting an untracked agent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("read worker PID {}: {e}", path.display())),
        };
        let pid = contents
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("invalid worker PID in {}: {e}", path.display()))?;
        stop_tree(&SystemProcesses, pid)
    }
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
pub fn record_shell(command: &str, shell: &str, path: &str) -> String {
    if cfg!(windows) {
        let path = path.replace('\'', "''");
        if shell.to_lowercase().contains("powershell") || shell.to_lowercase().contains("pwsh") {
            format!("[System.IO.File]::WriteAllText('{path}', [string]$PID); {command}")
        } else {
            format!("powershell -NoProfile -Command \"(Get-CimInstance Win32_Process -Filter ('ProcessId=' + $PID)).ParentProcessId | Set-Content -LiteralPath '{path}'\" && {command}")
        }
    } else {
        let _ = shell;
        let path = format!("'{}'", path.replace('\'', "'\\''"));
        format!("printf '%s' \"$$\" > {path} && {{\n{command}\n}}")
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

fn stop_tree(control: &impl ProcessControl, root: u32) -> Result<(), String> {
    let snapshot = control.snapshot()?;
    // A PID file alone is insufficient authority: the root must still belong
    // to this GitTerm, and escalation checks birth identities against reuse.
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
        return Ok(());
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
        #[cfg(unix)]
        {
            // shortcut: ps birth identity has second precision on non-macOS Unix;
            // use native start ticks if task workers are supported there.
            let output = std::process::Command::new("ps")
                .args(["-eo", "pid=,ppid=,lstart="])
                .output()
                .map_err(|e| format!("list worker processes: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "list worker processes: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| {
                    let mut words = line.split_whitespace();
                    let pid = words
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("invalid ps PID: {line}"))?;
                    let parent = words
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("invalid ps parent: {line}"))?;
                    let birth = words.collect::<Vec<_>>().join(" ");
                    if birth.is_empty() {
                        return Err(format!("missing ps birth: {line}"));
                    }
                    Ok(Process { pid, parent, birth })
                })
                .collect()
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
        Ok(self.snapshot()?.into_iter().find(|p| p.pid == pid))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    #[cfg(unix)]
    fn launch_wrapper_records_its_shell_and_preserves_quoted_commands() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("worker's pid");
        let command = record_shell(
            "printf '%s' 'a quoted command'",
            "/bin/sh",
            path.to_str().unwrap(),
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
        stop_tree(&mock, 10).unwrap();
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
        stop_tree(&mock, 1234).unwrap();
        assert!(mock.signals.borrow().is_empty());
    }
}
