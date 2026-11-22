//! H5b B10b: `task_launch_session` in fixture mode, with fake `claude` and
//! `codex` on a temp PATH.
use super::*;
use crate::task_host::TerminalLaunched;
use gitterm_core::tasks::TaskStore;
use std::{cell::RefCell, rc::Rc};

/// A fake agent, installed as `claude` and `codex`. Start `n` records
/// `pid-n`, `env-n`, `args-n` and `pwd-n`, starts a `sleep` descendant,
/// records its pid in `child-n` last, and waits for it.
struct FakeAgent {
    dir: tempfile::TempDir,
}
impl FakeAgent {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
echo run >> 'DIR/runs'
n=$(wc -l < 'DIR/runs' | tr -d ' ')
echo $$ > "DIR/pid-$n"
env > "DIR/env-$n"
printf '%s\n' "$@" > "DIR/args-$n"
pwd -P > "DIR/pwd-$n"
sleep 600 >/dev/null 2>&1 &
echo $! > "DIR/child-$n"
wait
"#
        .replace("DIR", &dir.path().display().to_string());
        for name in ["claude", "codex"] {
            let program = dir.path().join(name);
            std::fs::write(&program, &script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }
    fn search_path(&self) -> std::ffi::OsString {
        let git = std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let git = PathBuf::from(String::from_utf8(git.stdout).unwrap().trim());
        std::env::join_paths([
            self.dir.path(),
            Path::new("/bin"),
            Path::new("/usr/bin"),
            git.parent().unwrap(),
        ])
        .unwrap()
    }
    fn runs(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("runs")).map_or(0, |runs| runs.lines().count())
    }
    fn read(&self, name: &str, run: usize) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.path().join(format!("{name}-{run}"))).ok()?;
        text.ends_with('\n').then_some(text)
    }
    /// Start `run` recorded everything (its descendant's pid comes last).
    fn started(&self, run: usize) -> bool {
        self.read("child", run).is_some()
    }
    fn pid(&self, run: usize) -> String {
        self.read("pid", run).unwrap().trim().to_string()
    }
    fn descendant(&self, run: usize) -> String {
        self.read("child", run).unwrap().trim().to_string()
    }
    fn env(&self, run: usize) -> std::collections::BTreeMap<String, String> {
        self.read("env", run)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }
    fn args(&self, run: usize) -> Vec<String> {
        self.read("args", run)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// Whether `pid` is a process at all; a zombie still counts, so a child
/// counts as gone only once it was reaped.
fn exists(pid: &str) -> bool {
    !std::process::Command::new("/bin/ps")
        .args(["-p", pid, "-o", "stat="])
        .output()
        .unwrap()
        .stdout
        .is_empty()
}

/// Announced sessions, each with whether it was saved on disk by then.
type Seen = Rc<RefCell<Vec<(String, bool)>>>;

/// A fixture host whose Iced settings have Claude and Codex presets.
struct Launches {
    runs: Runs,
    agent: FakeAgent,
    created: std::cell::Cell<usize>,
}
impl Launches {
    fn start(cx: &mut TestAppContext) -> Self {
        Self::start_with(|_| {}, cx)
    }
    fn start_with(seed: impl FnOnce(&Runs0), cx: &mut TestAppContext) -> Self {
        let runs = Runs::start_with(
            2,
            |runs0| {
                std::fs::write(
                    runs0.fixture.iced.join("settings.json"),
                    serde_json::to_vec(&serde_json::json!({
                        "review": {"max_concurrent_reviews": 2},
                        "agent_presets": [
                            {"name": "Claude", "command": "claude"},
                            {"name": "Codex", "command": "codex"}
                        ]
                    }))
                    .unwrap(),
                )
                .unwrap();
                seed(runs0);
            },
            cx,
        );
        let agent = FakeAgent::new();
        runs.owner.update(cx, |owner, _| {
            owner.codex_search_path = Some(agent.search_path())
        });
        Self {
            runs,
            agent,
            created: Default::default(),
        }
    }
    /// A task this host creates, each with its own worktree.
    fn create(&self, cx: &mut TestAppContext) -> String {
        let n = self.created.get() + 1;
        self.created.set(n);
        let operation = TaskControlOperation::Create(req(serde_json::json!({
            "title": format!("Fixture task {n}"), "objective": "Launch a fixture session",
            "repository_path": self.runs.git.repo.display().to_string(), "issue_key": "tru-1"
        })));
        let created = dispatch(cx, &self.runs.sender, operation).unwrap();
        created["task_id"].as_str().unwrap().to_string()
    }
    fn launch(
        &self,
        task_id: &str,
        preset: Option<&str>,
        cx: &mut TestAppContext,
    ) -> Result<serde_json::Value, String> {
        dispatch_as(
            cx,
            &self.runs.sender,
            TaskControlOperation::LaunchSession(req(serde_json::json!({
                "task_id": task_id, "preset_name": preset
            }))),
            Some(&self.runs.caller),
        )
    }
    fn worktree(&self, task_id: &str, cx: &mut TestAppContext) -> PathBuf {
        stored_task(&self.runs.owner, task_id, cx)
            .worktree
            .path
            .unwrap()
    }
    fn saved(&self) -> TaskStore {
        TaskStore::load(self.runs.fixture.copy()).unwrap()
    }
    fn host_terminals(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.runs
            .owner
            .read_with(cx, |owner, _| owner.host_terminals())
    }
    /// Every TerminalLaunched the host emits, with whether the session was
    /// already saved on disk when it arrived.
    fn observe(&self, cx: &mut TestAppContext) -> (gpui::Subscription, Seen) {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let path = self.runs.fixture.copy();
        let record = seen.clone();
        let subscription = cx.update(|cx| {
            cx.subscribe(
                &self.runs.owner,
                move |_, launched: &TerminalLaunched, _| {
                    let saved = TaskStore::load(&path)
                        .unwrap()
                        .get(&launched.task_id)
                        .is_some_and(|task| {
                            task.sessions
                                .iter()
                                .any(|session| session.task_session_id == launched.session_id)
                        });
                    record
                        .borrow_mut()
                        .push((launched.session_id.clone(), saved));
                },
            )
        });
        (subscription, seen)
    }
    /// Kills a still-running fake start, so no test leaves a child.
    fn kill(&self, run: usize) {
        for pid in [self.agent.pid(run), self.agent.descendant(run)] {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid])
                .status();
        }
    }
}

fn set_mode(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Makes the preview root read-only until dropped, so no store save there
/// can succeed.
struct ReadOnly(PathBuf);
impl ReadOnly {
    fn new(dir: &Path) -> Self {
        set_mode(dir, 0o500);
        Self(dir.to_path_buf())
    }
}
impl Drop for ReadOnly {
    fn drop(&mut self) {
        set_mode(&self.0, 0o700);
    }
}

/// Blocks (inside a synchronous launch) until fake start `run` recorded
/// everything.
fn wait_for_start(dir: &Path, run: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !std::fs::read_to_string(dir.join(format!("child-{run}")))
        .is_ok_and(|pid| pid.ends_with('\n'))
    {
        assert!(Instant::now() < deadline, "the child did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui::test]
fn launch_publishes_only_after_save(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let (_subscription, seen) = launches.observe(cx);
    // A save that fails: no reply, no announced terminal, no child.
    let root = launches.runs.fixture.roots.root().to_path_buf();
    let dir = launches.agent.dir.path().to_path_buf();
    let locked = root.clone();
    launches.runs.owner.update(cx, |owner, _| {
        owner.before_publish = Some(Box::new(move |_| {
            wait_for_start(&dir, 1);
            set_mode(&locked, 0o500);
        }))
    });
    let reply = launches.launch(&task, Some("Codex"), cx);
    set_mode(&root, 0o700);
    let error = reply.unwrap_err();
    assert!(!error.is_empty());
    assert!(seen.borrow().is_empty());
    assert!(launches.host_terminals(cx).is_empty());
    let (pid, descendant) = (launches.agent.pid(1), launches.agent.descendant(1));
    wait_until(cx, |_| !exists(&pid) && !exists(&descendant));
    assert!(
        stored_task(&launches.runs.owner, &task, cx)
            .sessions
            .is_empty()
    );

    // A save that succeeds: the terminal is announced after it is on disk.
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    assert_eq!(*seen.borrow(), [(session.clone(), true)]);
    assert_eq!(launches.host_terminals(cx), [session]);
    wait_until(cx, |_| launches.agent.started(2));
    launches.kill(2);
}

#[gpui::test]
fn launch_origin_mcp_vs_ui_preserved(cx: &mut TestAppContext) {
    use gitterm_core::plan_limits::{LimitWindow, PlanLimits, PlanLimitsFile, Provider};
    let launches = Launches::start(cx);
    let held = launches.create(cx);
    let user = launches.create(cx);
    // Codex's weekly window is used up: an agent's launch waits, the
    // user's own does not.
    let now = chrono::Utc::now().timestamp();
    let mut limits = PlanLimitsFile::default();
    limits.absorb(PlanLimits {
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
    });
    limits
        .save(&PlanLimitsFile::path_in(&launches.runs.fixture.iced))
        .unwrap();
    let queued = launches.launch(&held, Some("Codex"), cx).unwrap();
    assert_eq!(queued["queued"], true, "{queued}");
    let queue = launches
        .runs
        .owner
        .read_with(cx, |owner, _| owner.host().launch_queue().to_vec());
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].task_id, held);
    assert_eq!(queue[0].origin, TaskLaunchOrigin::Agent);
    assert_eq!(
        stored_task(&launches.runs.owner, &held, cx).lifecycle,
        TaskLifecycle::Queued
    );
    assert_eq!(launches.agent.runs(), 0);

    let started = launches
        .runs
        .owner
        .update(cx, |owner, cx| {
            owner.launch_from_ui(&user, Some("Codex"), cx)
        })
        .unwrap();
    assert!(started["session_id"].is_string(), "{started}");
    wait_until(cx, |_| launches.agent.started(1));
    assert_eq!(
        stored_task(&launches.runs.owner, &user, cx).lifecycle,
        TaskLifecycle::Running
    );
    launches.kill(1);
}

#[gpui::test]
fn save_failure_on_queued_launch_gives_no_success_reply(cx: &mut TestAppContext) {
    use gitterm_core::plan_limits::{LimitWindow, PlanLimits, PlanLimitsFile, Provider};
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let now = chrono::Utc::now().timestamp();
    let mut limits = PlanLimitsFile::default();
    limits.absorb(PlanLimits {
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
    });
    limits
        .save(&PlanLimitsFile::path_in(&launches.runs.fixture.iced))
        .unwrap();
    let readonly = ReadOnly::new(launches.runs.fixture.roots.root());
    let reply = launches.launch(&task, Some("Codex"), cx);
    drop(readonly);
    let error = reply.unwrap_err();
    assert!(!error.contains("queued"), "{error}");
    let queue = launches
        .runs
        .owner
        .read_with(cx, |owner, _| owner.host().launch_queue().len());
    assert_eq!(queue, 0);
    assert_eq!(
        launches.saved().get(&task).unwrap().lifecycle,
        TaskLifecycle::Ready
    );
    assert_eq!(launches.agent.runs(), 0);
}

#[gpui::test]
fn launch_save_then_attach_failure_leaves_no_child(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let other = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, "gone"))
    });
    let (_subscription, seen) = launches.observe(cx);
    // After the child starts, the worker's delegation moves to another
    // task: the session saves, and the attach that follows fails.
    let dir = launches.agent.dir.path().to_path_buf();
    launches.runs.owner.update(cx, |owner, _| {
        owner.before_publish = Some(Box::new(move |owner| {
            wait_for_start(&dir, 1);
            let path = owner.host().store().as_ref().unwrap().path().to_path_buf();
            let mut document: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            for delegation in document["delegations"].as_array_mut().unwrap() {
                delegation["child"]["task_id"] = serde_json::json!(other);
            }
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            owner
                .host_mut()
                .replace_store(Some(TaskStore::load(&path).unwrap()));
        }))
    });
    let error = launches.launch(&task, Some("Codex"), cx).unwrap_err();
    assert!(error.contains("could not attach"), "{error}");
    // The model went on the command before it started.
    assert!(
        launches
            .agent
            .args(1)
            .windows(2)
            .any(|pair| pair[1] == "fixture-model"),
        "{:?}",
        launches.agent.args(1)
    );
    assert!(seen.borrow().is_empty());
    assert!(launches.host_terminals(cx).is_empty());
    let (pid, descendant) = (launches.agent.pid(1), launches.agent.descendant(1));
    wait_until(cx, |_| !exists(&pid) && !exists(&descendant));
    // The saved session was taken back out.
    assert!(launches.saved().get(&task).unwrap().sessions.is_empty());
}

#[gpui::test]
fn fixture_worker_checked_trust_equals_effective_config(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.launch(&task, Some("Codex"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    let env = launches.agent.env(1);
    let args = launches.agent.args(1);
    // The workspace repo's trust overlay, and the notify hook for the task.
    assert!(
        args.iter().any(|arg| arg.contains("trust_level")),
        "{args:?}"
    );
    assert!(args.iter().any(|arg| arg.contains("notify=")), "{args:?}");
    // The list the check reads is the child's own CODEX_HOME's.
    let config = PathBuf::from(&env["CODEX_HOME"]).join("config.toml");
    let main = launches.runs.git.repo.canonicalize().unwrap();
    std::fs::write(
        &config,
        format!(
            "[projects.\"{}\"]\ntrust_level = \"untrusted\"\n",
            main.display()
        ),
    )
    .unwrap();
    let error = launches.launch(&task, Some("Codex"), cx).unwrap_err();
    assert!(
        error.contains(&config.display().to_string()),
        "{error} / {}",
        config.display()
    );
    assert_eq!(launches.agent.runs(), 1);
}

#[gpui::test]
fn fixture_add_dir_outside_refused(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let mut worker = worker_delegation("worker", &task, "gone");
    worker.options.add_dirs = vec![launches.runs.git.outside.clone()];
    launches
        .runs
        .owner
        .update(cx, |owner, _| owner.own_worker(worker));
    let error = launches.launch(&task, Some("Codex"), cx).unwrap_err();
    assert_eq!(
        error,
        gitterm_core::preview::service::preview_notice("task_launch_session")
    );
    assert_eq!(launches.agent.runs(), 0);
    assert!(launches.saved().get(&task).unwrap().sessions.is_empty());
}

#[gpui::test]
fn launched_terminal_env_has_only_the_fixture_env(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    let worktree = launches.worktree(&task, cx);
    let mut expected: std::collections::BTreeMap<String, String> = launches
        .runs
        .owner
        .read_with(cx, |owner, _| owner.launch_env(&worktree, &session))
        .into_iter()
        .collect();
    let mut env = launches.agent.env(1);
    // sh sets these itself, and macOS strips DYLD_* when it execs the
    // protected /usr/bin/env and /bin/sh.
    for name in ["PWD", "SHLVL", "OLDPWD", "_"] {
        env.remove(name);
        expected.remove(name);
    }
    expected.retain(|name, _| !name.starts_with("DYLD_"));
    // Nothing the launcher or the PTY adds on its own survives.
    assert_eq!(env, expected);
    // The task MCP URL names the task session.
    use gitterm_core::task_commands::mcp_protocol::TASK_MCP_URL_ENV;
    assert!(env[TASK_MCP_URL_ENV].contains(&session), "{env:?}");
    assert_eq!(
        PathBuf::from(launches.agent.read("pwd", 1).unwrap().trim_end()),
        worktree.canonicalize().unwrap()
    );
}

#[gpui::test]
fn brief_written_under_preview_root(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let iced_before = tree(&launches.runs.fixture.iced);
    let task = launches.create(cx);
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    let root = launches.runs.fixture.roots.root();
    let brief = root.join("task-briefs").join(format!("{session}.md"));
    let text = std::fs::read_to_string(&brief).unwrap();
    assert!(
        text.contains(&format!("GitTerm task session: {session}")),
        "{text}"
    );
    // The harness got the brief as its prompt, the guidelines (written
    // under the preview root) and the notify hook for this session.
    let args = launches.agent.args(1);
    assert!(
        args.iter()
            .any(|arg| arg.contains(&format!("GitTerm task session: {session}"))),
        "{args:?}"
    );
    assert!(
        args.iter()
            .any(|arg| arg.starts_with("developer_instructions=")),
        "{args:?}"
    );
    assert!(root.join("agent-guidelines.md").is_file());
    assert!(
        args.iter()
            .any(|arg| arg.contains("notify=") && arg.contains(&session)),
        "{args:?}"
    );
    let saved = launches.saved();
    let record = &saved.get(&task).unwrap().sessions[0];
    assert_eq!(record.task_session_id, session);
    assert_eq!(
        record.objective_delivery,
        gitterm_core::tasks::ObjectiveDeliveryState::Pending
    );
    assert_eq!(tree(&launches.runs.fixture.iced), iced_before);
}

#[gpui::test]
fn imported_records_never_launched(cx: &mut TestAppContext) {
    let launches = Launches::start_with(
        |runs0| {
            runs0.fixture.write_iced(
                vec![task_at(
                    "imported",
                    TaskLifecycle::Ready,
                    &runs0.git.worktree,
                )],
                vec![],
            )
        },
        cx,
    );
    let before = launches.saved();
    let error = launches.launch("imported", Some("Codex"), cx).unwrap_err();
    assert_eq!(
        error,
        gitterm_core::preview::service::preview_notice("task_launch_session")
    );
    // An imported worker on a task of this host's is never attached.
    let task = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner
            .host_mut()
            .store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(worker_delegation("imported-worker", &task, "gone"))
            .unwrap()
    });
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    let worker = launches
        .saved()
        .delegation("imported-worker")
        .unwrap()
        .clone();
    let gitterm_core::tasks::DelegationChild::TaskSession {
        task_session_id, ..
    } = worker.child
    else {
        panic!("worker child")
    };
    assert_eq!(task_session_id.as_deref(), Some("gone"));
    assert!(
        !launches
            .agent
            .args(1)
            .contains(&"fixture-model".to_string())
    );
    assert!(reply["session_id"].is_string());
    assert_eq!(
        before.get("imported").unwrap().sessions,
        launches.saved().get("imported").unwrap().sessions
    );
}

#[gpui::test]
fn unadopted_terminal_killed_at_shutdown(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.launch(&task, Some("Codex"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    let (pid, descendant) = (launches.agent.pid(1), launches.agent.descendant(1));
    assert!(exists(&pid) && exists(&descendant));
    launches.runs.owner.update(cx, |owner, cx| owner.quit(cx));
    wait_until(cx, |_| !exists(&pid) && !exists(&descendant));
    assert!(launches.host_terminals(cx).is_empty());
}

#[gpui::test]
fn shell_adopts_launched_terminal_into_a_pane(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let owner = launches.runs.owner.clone();
    cx.update(|cx| cx.set_global(crate::task_host::GpuiTaskHost(owner)));
    let folder = launches.runs.git.root.clone();
    let config = tempfile::tempdir().unwrap();
    let config_dir = config.path().to_path_buf();
    let (shell, vcx) = cx.add_window_view(|window, cx| {
        let mut model = crate::workspace_status::WorkspaceStatus::default();
        model.add(folder.clone(), Instant::now());
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, None, cx));
        crate::Shell::with_owner(
            Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            &config_dir,
            true,
            window,
            cx,
        )
        .unwrap()
    });
    let reply = launches.launch(&task, Some("Codex"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    let adopted = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            shell.read(cx).active(cx).panes.values().any(|pane| {
                let pane = pane.read(cx);
                pane.session
                    .as_ref()
                    .is_some_and(|target| target.task_session_id.as_deref() == Some(&session))
                    && matches!(pane.content, crate::pane::PaneContent::Terminal(_))
            })
        })
    };
    wait_until(vcx, adopted);
    assert!(launches.host_terminals(vcx).is_empty());
    wait_until(vcx, |_| launches.agent.started(1));
    // The pane owns the child now; host shutdown leaves it running.
    let pid = launches.agent.pid(1);
    launches.runs.owner.update(vcx, |owner, cx| owner.quit(cx));
    vcx.run_until_parked();
    assert!(exists(&pid));
    launches.kill(1);
}

#[gpui::test]
fn preview_still_answers_notice_for_launch_and_delegate(cx: &mut TestAppContext) {
    use gitterm_core::preview::service::preview_notice;
    let fixture = Fixture::new();
    fixture.write_iced(
        vec![task_at(
            "queued",
            TaskLifecycle::Ready,
            &fixture.iced.parent().unwrap().join("wt"),
        )],
        vec![],
    );
    let (_owner, sender) = fixture.start(cx);
    let launch = TaskControlOperation::LaunchSession(req(serde_json::json!({"task_id": "queued"})));
    assert_eq!(
        dispatch(cx, &sender, launch),
        Err(preview_notice("task_launch_session"))
    );
    let delegate = |sender: &TaskControlSender, cx: &mut TestAppContext| {
        dispatch(
            cx,
            sender,
            TaskControlOperation::DelegateTask(req(
                serde_json::json!({"title": "t", "objective": "o"}),
            )),
        )
        .unwrap_err()
    };
    assert!(delegate(&sender, cx).contains("not available in preview"));
    // Fixture mode does not run delegate_task yet either.
    let launches = Launches::start(cx);
    assert!(delegate(&launches.runs.sender, cx).contains("not available in preview"));
    assert_eq!(launches.agent.runs(), 0);
}

/// The preview pins CLAUDE_CONFIG_DIR under its root, and core's Claude
/// trust check refuses any CLAUDE_CONFIG_DIR, so a fixture Claude launch is
/// refused before its child starts rather than checking a trust file Claude
/// would not read.
#[gpui::test]
fn claude_launch_refused_on_pinned_config_dir(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let error = launches.launch(&task, Some("Claude"), cx).unwrap_err();
    assert!(error.contains("CLAUDE_CONFIG_DIR"), "{error}");
    assert_eq!(launches.agent.runs(), 0);
    assert!(launches.host_terminals(cx).is_empty());
    assert!(launches.saved().get(&task).unwrap().sessions.is_empty());
}

#[test]
fn preassigned_claude_session_id_follows_iced() {
    use crate::task_host::launch_session_id as assign;
    let (command, id) = assign("claude");
    let id = id.unwrap();
    assert_eq!(command, format!("claude --session-id {id}"));
    assert_eq!(
        assign("claude --resume abc"),
        ("claude --resume abc".into(), Some("abc".into()))
    );
    assert_eq!(
        assign("claude --continue"),
        ("claude --continue".into(), None)
    );
    assert_eq!(assign("codex"), ("codex".into(), None));
}

/// B10a's headless runs get the change guidelines as Iced's
/// start_delegation_run gives them, from the preview root.
#[gpui::test]
fn fixture_codex_run_gets_guidelines(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    let id = runs.consult(cx);
    wait_until(cx, |cx| runs.status(&id, cx) == "completed");
    let args = runs.codex.args(1);
    assert!(
        args.iter()
            .any(|arg| arg.starts_with("developer_instructions=")),
        "{args:?}"
    );
    assert!(
        runs.fixture
            .roots
            .root()
            .join("agent-guidelines.md")
            .is_file()
    );
}
