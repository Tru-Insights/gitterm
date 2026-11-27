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

#[test]
fn unpublished_rollback_revokes_a_late_recorder_before_harness_start() {
    let process = WorkerProcess::new().unwrap();
    let path = process.pid_file();
    process.discard_unpublished().unwrap();
    assert!(!path.parent().unwrap().exists());
    assert!(record_process(&path, std::process::id()).is_err());
}
