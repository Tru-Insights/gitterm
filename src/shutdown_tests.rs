//! Headless process regressions for the desktop's actual job wrapper.
use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

fn until(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn launch(root: &Path, mode: &str) -> Child {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "shutdown_periodic_tests::helper", "--nocapture"])
        .env("GITTERM_V6_CONFIG_DIR", root)
        .env("HOME", home)
        .env("GITTERM_V6_PERIODIC_TEST", mode)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(root.join("stdout")).unwrap())
        .stderr(std::fs::File::create(root.join("stderr")).unwrap())
        .spawn()
        .unwrap()
}

fn exited_within(child: &mut Child, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "fixture exited {status}");
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn barrier_finished(child: &mut Child, root: &Path, drain: bool) -> bool {
    if !drain {
        return exited_within(child, Duration::from_secs(2));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !root.join("barrier-done").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    root.join("barrier-done").exists()
}

#[test]
fn helper() {
    let Ok(mode) = std::env::var("GITTERM_V6_PERIODIC_TEST") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("GITTERM_V6_CONFIG_DIR").unwrap());
    let operations = gitterm::shutdown::operations();
    let task = if mode.starts_with("network") {
        periodic_task(
            "poll_pr_watch",
            gitterm::pr_watch::poll(gitterm::pr_watch::PollRequest {
                items: vec![gitterm::pr_watch::PollItem {
                    key: gitterm::merge_plan::PrKey::new("fixture/repo", 1),
                    account: None,
                    snapshot: Default::default(),
                    etags: Default::default(),
                }],
            }),
            |outcome| Event::Pr(PrEvent::Polled(Box::new(outcome))),
        )
    } else {
        let roots = gitterm_core::turn_snapshots::SnapshotRoots::new(
            root.join("turn-snapshots"),
            root.clone(),
        )
        .unwrap();
        let facts = root.join("facts");
        let cancelled = operations.clone();
        periodic_task(
            "maintain_turn_snapshots",
            async move {
                gitterm_core::turn_snapshots::maintain_cancellable(
                    &roots,
                    facts,
                    Default::default(),
                    None,
                    gitterm_core::turn_snapshots::MaintenanceControl {
                        cancelled: Arc::new(move || cancelled.started()),
                    },
                )
                .await
            },
            Event::TurnSnapshotsCleaned,
        )
    };
    // Start exactly the registered job, without a window or the Iced event loop.
    let job = shutdown_jobs().lock().unwrap().last().unwrap().clone();
    job.start();
    drop(task);
    until(&root.join("quit"));
    operations.begin(gitterm::shutdown::TIMEOUT);
    operations.wait();
    assert!(operations.unfinished().is_empty());
    assert!(
        job.take().is_none(),
        "cancelled poll must not reach its save reducer"
    );
    if mode.ends_with("drain") {
        std::fs::write(root.join("barrier-done"), b"").unwrap();
        gitterm::shutdown::Executor::runtime()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), job.done.cancelled())
                    .await
                    .unwrap();
            });
    }
    if mode.starts_with("network") {
        // Even a late completion cannot admit the watch save after cancellation.
        let result = gitterm::shutdown::Executor::runtime().unwrap().block_on(
            gitterm::shutdown::periodic_scope(operations, async {
                gitterm::pr_watch::save(&Default::default(), &root.join("pr-watch.json"))
            }),
        );
        assert!(result.is_err());
    }
}

#[test]
fn pr_watch_blocked_mid_network_quits_under_two_seconds_without_saving() {
    for mode in ["network", "network-drain"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("bin")).unwrap();
        let watch = root.join("pr-watch.json");
        std::fs::write(&watch, b"original watch bytes").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        script(
        &root.join("bin/gh"),
        &format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho call >> \"$GITTERM_V6_CONFIG_DIR/network-calls\"\nexec /usr/bin/nc 127.0.0.1 {}\n",
            listener.local_addr().unwrap().port(),
        ),
    );
        let mut child = launch(root, mode);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut connection = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "PR poll never reached network");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        std::fs::write(root.join("quit"), b"").unwrap();
        let exited = barrier_finished(&mut child, root, mode.ends_with("drain"));
        // Unblock the fixture even when testing a deliberately regressed barrier.
        let _ = connection.write_all(b"HTTP/2.0 200 OK\n\n{\"login\":\"fixture\"}");
        drop(connection);
        if !exited {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(
            exited,
            "quit waited for PR-watch network; {}",
            std::fs::read_to_string(root.join("stderr")).unwrap()
        );
        assert!(exited_within(&mut child, Duration::from_secs(2)));
        assert_eq!(std::fs::read(watch).unwrap(), b"original watch bytes");
        assert_eq!(
            std::fs::read_to_string(root.join("network-calls"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

#[test]
fn snapshot_git_quits_under_two_seconds_and_child_finishes_without_being_killed() {
    for mode in ["snapshot", "snapshot-drain"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = root.join("turn-snapshots").join("0".repeat(64));
        std::fs::create_dir_all(&store).unwrap();
        assert!(Command::new("/usr/bin/git")
            .args(["init", "--bare", "--quiet"])
            .arg(&store)
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.join("bin")).unwrap();
        script(
            &root.join("bin/git"),
            r#"#!/bin/sh
case " $* " in
  *" for-each-ref "*)
    echo $$ > "$GITTERM_V6_CONFIG_DIR/git-pid"
    while [ ! -f "$GITTERM_V6_CONFIG_DIR/release-git" ]; do /bin/sleep 0.01; done
    /usr/bin/git "$@"
    result=$?
    echo "$result" > "$GITTERM_V6_CONFIG_DIR/git-result"
    /bin/mv "$GITTERM_V6_CONFIG_DIR/git-result" "$GITTERM_V6_CONFIG_DIR/git-finished"
    exit "$result"
    ;;
  *) echo "$*" >> "$GITTERM_V6_CONFIG_DIR/unexpected-next-command"; exit 1 ;;
esac
"#,
        );
        let mut child = launch(root, mode);
        until(&root.join("git-pid"));
        std::fs::write(root.join("quit"), b"").unwrap();
        let exited = barrier_finished(&mut child, root, mode.ends_with("drain"));
        let pid: i32 = std::fs::read_to_string(root.join("git-pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal zero only checks the private fixture child's existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        std::fs::write(root.join("release-git"), b"").unwrap();
        if !exited {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(exited, "quit waited for snapshot git");
        assert!(alive, "shutdown killed the in-flight git child");
        until(&root.join("git-finished"));
        assert_eq!(
            std::fs::read_to_string(root.join("git-finished"))
                .unwrap()
                .trim(),
            "0"
        );
        assert!(exited_within(&mut child, Duration::from_secs(2)));
        assert!(!root.join("unexpected-next-command").exists());
    }
}
