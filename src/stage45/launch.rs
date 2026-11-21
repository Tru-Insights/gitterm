//! TRU-274 H4b: worker launch publication, pinned through the Iced paths.
//! Each test runs in its own child process with a temporary config dir,
//! HOME and a fake login shell, so the PTY child is a script that records
//! its pid and sleeps; no harness binary ever runs.
use super::*;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
struct LaunchFixture {
    dir: tempfile::TempDir,
    app: App,
    preset: usize,
}

/// A Ready task (worktree = the fixture dir) and a fake preset. `delegation`
/// adds an open worker delegation that this preset carries out.
#[cfg(unix)]
fn launch_fixture(preset_command: &str, delegation: bool) -> LaunchFixture {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let shell = dir.path().join("fake-shell");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 60\n",
            dir.path().join("child.pid").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Only this test runs in the child process (see `isolated`).
    std::env::set_var("HOME", &home);
    std::env::set_var("SHELL", &shell);
    let mut app = app_with_tabs(dir.path());
    app.task_host.replace_store(Some(
        TaskStore::load(dir.path().join("tasks.json")).unwrap(),
    ));
    add_task(&mut app, dir.path(), dir.path());
    let store = app.task_host.store_mut().as_mut().unwrap();
    let task = store.get("fixture-task").unwrap().clone();
    store
        .begin_worktree_preparation("fixture-task", "t1")
        .unwrap();
    store
        .complete_worktree_preparation(
            "fixture-task",
            gitterm::tasks::CompletedWorktreePreparation {
                repository: task.repository,
                base: task.base,
                branch: task.branch,
                path: dir.path().into(),
            },
            "t2",
        )
        .unwrap();
    app.agent_presets.push(AgentPreset {
        name: "Fake".into(),
        command: preset_command.into(),
        resume_command: None,
        icon: String::new(),
        color: WorkspaceColor::Blue,
    });
    let preset = app.agent_presets.len() - 1;
    if delegation {
        use gitterm::tasks::{
            Delegation, DelegationChild, DelegationKind, DelegationParent, NewDelegation,
        };
        let mut worker = Delegation::new_requested(
            NewDelegation {
                kind: DelegationKind::Implement,
                parent: DelegationParent {
                    session_uid: "parent".into(),
                    chat_session_id: None,
                    workspace: "alpha".into(),
                    cwd: dir.path().into(),
                },
                child: DelegationChild::TaskSession {
                    task_id: "fixture-task".into(),
                    task_session_id: None,
                    conversation: None,
                    preset_name: Some("Fake".into()),
                    model: None,
                    role: None,
                },
                brief: "Fix it".into(),
                target: None,
                previous: None,
                options: Default::default(),
            },
            "2000-01-01T00:00:00Z",
        );
        worker.delegation_id = "worker".into();
        app.task_host
            .store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(worker)
            .unwrap();
    }
    LaunchFixture { dir, app, preset }
}

#[cfg(unix)]
fn task_tabs(app: &App) -> usize {
    app.workspaces
        .iter()
        .flat_map(|ws| &ws.tabs)
        .filter(|tab| tab.task_id.as_deref() == Some("fixture-task"))
        .count()
}

#[cfg(unix)]
fn child_pid(dir: &Path) -> Option<u32> {
    let path = dir.join("child.pid");
    for _ in 0..100 {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Child processes of this (isolated, single-test) process: the PTY shells
/// of its tabs. `pgrep` leaves itself out.
#[cfg(unix)]
fn live_children() -> usize {
    let out = std::process::Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

/// The PTY child of a dropped tab gets SIGHUP from the terminal's event loop
/// thread, so it stops shortly after, not synchronously. A child dropped
/// before its script ran never wrote a pid, so the check is on this
/// process's children, not only the recorded pid.
#[cfg(unix)]
fn assert_child_stopped(dir: &Path) {
    let pid = std::fs::read_to_string(dir.join("child.pid"))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    for _ in 0..200 {
        if live_children() == 0 && !pid.is_some_and(alive) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("an unpublished child is still running (recorded pid {pid:?})");
}

#[cfg(unix)]
fn mcp_launch(app: &mut App, user_model_choice: bool) -> Result<Value, String> {
    let (sender, mut receiver) = gitterm_core::task_commands::runtime::command_channel();
    let pending = sender.enqueue((), None).unwrap();
    let _unpolled = app.execute_host_effect(HostEffect::LaunchSession {
        reply: receiver.try_recv().unwrap().reply,
        launch: SessionLaunch {
            task_id: "fixture-task".into(),
            preset_name: Some("Fake".into()),
            origin: TaskLaunchOrigin::Agent,
            user_model_choice,
        },
    });
    iced::futures::executor::block_on(pending)
}

#[cfg(unix)]
fn queued_launches(app: &App) -> Vec<QueuedTaskLaunch> {
    app.task_host.launch_queue().to_vec()
}

#[cfg(unix)]
fn worker_record(app: &App) -> gitterm::tasks::Delegation {
    app.task_host
        .store()
        .as_ref()
        .unwrap()
        .delegation("worker")
        .unwrap()
        .clone()
}

#[cfg(unix)]
fn worker_session(delegation: &gitterm::tasks::Delegation) -> Option<String> {
    match &delegation.child {
        gitterm::tasks::DelegationChild::TaskSession {
            task_session_id, ..
        } => task_session_id.clone(),
        _ => None,
    }
}

#[test]
fn launch_save_or_attach_failure_stops_unpublished_child() {
    if isolated(
        "launch_save_or_attach_failure_stops_unpublished_child",
        "launch",
    ) {
        return;
    }
    #[cfg(unix)]
    for case in ["save", "attach", "mcp save"] {
        let LaunchFixture {
            dir,
            mut app,
            preset,
        } = launch_fixture("fake-harness", true);
        if case == "attach" {
            // A dismissed Interrupted worker is still open for a relaunch,
            // but attaching it would make it Running while dismissed, which
            // the store refuses after the session record was written.
            let store = app.task_host.store_mut().as_mut().unwrap();
            store
                .set_delegation_status("worker", gitterm::tasks::DelegationStatus::Running, "t1")
                .unwrap();
            store
                .set_delegation_status(
                    "worker",
                    gitterm::tasks::DelegationStatus::Interrupted,
                    "t2",
                )
                .unwrap();
            store.dismiss_delegation("worker", "t3").unwrap();
        } else {
            std::fs::rename(
                dir.path().join("tasks.json"),
                dir.path().join("before.json"),
            )
            .unwrap();
            std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
        }
        let before = worker_record(&app);
        let tabs = app.workspaces.iter().map(|ws| ws.tabs.len()).sum::<usize>();
        let error = if case == "mcp save" {
            mcp_launch(&mut app, false).unwrap_err()
        } else {
            match app.try_launch_task_child(
                "fixture-task",
                Some(preset),
                false,
                TaskLaunchOrigin::Agent,
                false,
            ) {
                Ok(_) => panic!("{case}: launch published"),
                Err(error) => error,
            }
        };
        // The child was never published and is stopped.
        assert_eq!(task_tabs(&app), 0, "{case}");
        assert_eq!(
            app.workspaces.iter().map(|ws| ws.tabs.len()).sum::<usize>(),
            tabs,
            "{case}"
        );
        assert_child_stopped(dir.path());
        // The delegation is not attached to the dropped session.
        assert_eq!(worker_record(&app), before, "{case}");
        let task = app
            .task_host
            .store()
            .as_ref()
            .unwrap()
            .get("fixture-task")
            .unwrap()
            .clone();
        if case == "attach" {
            assert!(
                error.contains("could not attach to delegation worker")
                    && error.ends_with("; stopping worker"),
                "{error}"
            );
            // Pinned as today: the session record written before the attach
            // stays in the store (in memory and on disk) without a tab.
            assert_eq!(task.sessions.len(), 1);
            let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
            assert_eq!(saved.get("fixture-task").unwrap().sessions.len(), 1);
            assert_eq!(worker_session(saved.delegation("worker").unwrap()), None);
        } else {
            assert!(
                error.contains("atomically write") && error.contains("tasks.json"),
                "{error}"
            );
            assert!(task.sessions.is_empty(), "{case}");
            assert_eq!(worker_session(&before), None);
        }
    }
}

#[test]
fn launch_publishes_only_after_session_and_attach_are_saved() {
    if isolated(
        "launch_publishes_only_after_session_and_attach_are_saved",
        "launch",
    ) {
        return;
    }
    #[cfg(unix)]
    {
        let LaunchFixture { dir, mut app, .. } = launch_fixture("fake-harness", true);
        let reply = mcp_launch(&mut app, false).unwrap();
        let session_id = reply["session_id"].as_str().unwrap().to_string();
        assert_eq!(reply["task_id"], "fixture-task");
        assert_eq!(reply["label"], "Fake");
        assert_eq!(reply["preset_name"], "Fake");
        assert_eq!(reply["workspace"], "alpha");
        assert_eq!(reply["objective_submitted"], false);
        assert!(reply.get("cost_note").is_none());
        // Published: the tab, the saved session and the attached worker.
        assert_eq!(task_tabs(&app), 1);
        let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
        let task = saved.get("fixture-task").unwrap();
        assert_eq!(task.sessions.len(), 1);
        assert_eq!(task.sessions[0].task_session_id, session_id);
        assert_eq!(task.sessions[0].label, "Fake");
        let worker = saved.delegation("worker").unwrap();
        assert_eq!(worker_session(worker).as_deref(), Some(session_id.as_str()));
        assert_eq!(worker.status, gitterm::tasks::DelegationStatus::Running);
        // A background launch leaves the user's tab focused.
        assert_eq!(app.active_workspace_index(), 0);
        assert_eq!(app.workspaces[0].active_tab, 0);
        // The published child runs until its tab goes.
        let pid = child_pid(dir.path()).expect("the published child started");
        assert!(alive(pid));
        assert_eq!(live_children(), 1);
        for workspace in &mut app.workspaces {
            workspace
                .tabs
                .retain(|tab| tab.task_id.as_deref() != Some("fixture-task"));
        }
        assert_child_stopped(dir.path());
    }
}

#[test]
fn launch_origin_and_explicit_choice_decide_policy_at_admission() {
    if isolated(
        "launch_origin_and_explicit_choice_decide_policy_at_admission",
        "launch",
    ) {
        return;
    }
    #[cfg(unix)]
    for case in ["mcp", "mcp override", "ui"] {
        use gitterm::plan_limits::{LimitWindow, PlanLimits, Provider};
        let codex = tempfile::tempdir().unwrap();
        let command = codex.path().join("codex");
        std::fs::write(&command, "#!/bin/sh\nexit 0\n").unwrap();
        let LaunchFixture {
            dir,
            mut app,
            preset,
        } = launch_fixture(&command.display().to_string(), false);
        // Codex launches pass the worker trust check, which needs a checkout.
        git(dir.path(), &["init", "-q", "-b", "main"]);
        let now = chrono::Utc::now().timestamp();
        assert!(app.plan_limits.finish_codex_read(Some(Box::new(PlanLimits {
            provider: Provider::Codex,
            observed_at: now,
            local: Some(true),
            windows: vec![LimitWindow {
                label: "7d".into(),
                used_percent: Some(99.0),
                resets_at: Some(now + 86_400),
            }],
            alerts: vec![],
            plan_type: None,
            credits: None,
        }))));
        let queued = match case {
            "ui" => {
                let _unpolled = app.launch_task_child("fixture-task", Some(preset));
                assert_eq!(app.task_ui_error, None);
                assert_eq!(
                    app.task_launch_cost_note,
                    Some("Explicit user preset choice overrides plan limits; this may consume remaining allowance or credits.")
                );
                task_tabs(&app) == 0
            }
            _ => {
                let reply = mcp_launch(&mut app, case == "mcp override").unwrap();
                assert_eq!(reply["cost_note"].is_string(), case == "mcp override");
                reply["queued"] == true
            }
        };
        // Only the agent's own launch without an explicit choice is held.
        assert_eq!(queued, case == "mcp", "{case}");
        assert_eq!(task_tabs(&app), usize::from(!queued), "{case}");
        let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
        let task = saved.get("fixture-task").unwrap();
        if queued {
            assert_eq!(task.lifecycle, TaskLifecycle::Queued);
            let queue = queued_launches(&app);
            assert_eq!(queue.len(), 1);
            assert!(matches!(queue[0].origin, TaskLaunchOrigin::Agent));
            assert!(task.sessions.is_empty());
        } else {
            assert!(queued_launches(&app).is_empty());
            assert_eq!(task.sessions.len(), 1);
        }
        for workspace in &mut app.workspaces {
            workspace
                .tabs
                .retain(|tab| tab.task_id.as_deref() != Some("fixture-task"));
        }
        assert_child_stopped(dir.path());
    }
}
