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
                        .get(launched.target.task_id.as_deref().unwrap())
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

/// Git in `dir` with no inherited `GIT_*` and no user or system config.
fn fixture_git(dir: &Path, args: &[&str]) -> String {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_none_or(|name| name.starts_with("GIT_")) {
            command.env_remove(name);
        }
    }
    let output = command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap()
}

#[gpui::test]
fn launch_refused_when_git_names_a_work_tree_outside(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let outside = launches.runs.git.outside.clone();
    // Only this worktree's own config.worktree names the outside folder,
    // which the admin-dir recheck cannot see.
    fixture_git(&worktree, &["config", "extensions.worktreeConfig", "true"]);
    fixture_git(
        &worktree,
        &[
            "config",
            "--worktree",
            "core.worktree",
            outside.to_str().unwrap(),
        ],
    );
    assert_eq!(
        PathBuf::from(fixture_git(&worktree, &["rev-parse", "--show-toplevel"]).trim_end())
            .canonicalize()
            .unwrap(),
        outside.canonicalize().unwrap()
    );
    let error = launches.launch(&task, Some("Codex"), cx).unwrap_err();
    assert_eq!(
        error,
        gitterm_core::preview::service::preview_notice("task_launch_session")
    );
    assert_eq!(launches.agent.runs(), 0);
    assert!(launches.saved().get(&task).unwrap().sessions.is_empty());
    assert!(launches.host_terminals(cx).is_empty());
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
    // The terminal's own TERM/COLORTERM, whatever the parent has.
    for (name, value) in [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")] {
        expected.insert(name.into(), value.into());
    }
    // Nothing else the launcher or the PTY adds survives. Only names are
    // printed: the values hold tokens.
    let differing: Vec<&String> = env
        .keys()
        .chain(expected.keys())
        .filter(|name| env.get(*name) != expected.get(*name))
        .collect();
    assert!(differing.is_empty(), "env differs in: {differing:?}");
    // The task MCP URL names the task session.
    use gitterm_core::task_commands::mcp_protocol::TASK_MCP_URL_ENV;
    assert!(
        env[TASK_MCP_URL_ENV].contains(&session),
        "{TASK_MCP_URL_ENV} does not name the session"
    );
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

/// Iced stores the launch's command on the tab (`set_startup_command`,
/// main.rs): Claude's pre-assigned session as its resume command, any other
/// agent's brief-free command, a plain terminal none. The pane's layout
/// entry holds the same, in memory and in the saved file.
#[gpui::test]
fn adopted_terminal_saves_its_startup_command_on_its_layout_pane(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let repo = launches.runs.git.repo.clone();
    pin_claude_trust(&launches, &repo, true);
    let claude = launches.create(cx);
    let codex = launches.create(cx);
    let plain = launches.create(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = task_shell(&launches, config.path(), cx);
    let saved_command = |session: &str, cx: &mut TestAppContext| -> Option<Option<String>> {
        cx.read(|cx| {
            let state = shell.read(cx).active(cx);
            let pane = state.panes.values().find(|pane| {
                pane.read(cx)
                    .session
                    .as_ref()
                    .is_some_and(|target| target.task_session_id.as_deref() == Some(session))
            })?;
            let id = state
                .sessions
                .iter()
                .find(|s| crate::session_pane_id(s.id) == pane.read(cx).id)?
                .id
                .0;
            let terminals = state.store.terminals.as_ref()?;
            Some(
                terminals
                    .iter()
                    .find(|t| t.id == id)?
                    .startup_command
                    .clone(),
            )
        })
    };
    let reply = launches.launch(&claude, Some("Claude"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    let conversation = reply["conversation_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| saved_command(&session, cx).is_some());
    assert_eq!(
        saved_command(&session, vcx),
        Some(Some(format!("claude --resume {conversation}")))
    );
    wait_until(vcx, |_| launches.agent.started(1));
    launches.kill(1);

    let reply = launches.launch(&codex, Some("Codex"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| saved_command(&session, cx).is_some());
    let command = saved_command(&session, vcx).unwrap().unwrap();
    assert!(command.starts_with("codex"), "{command}");
    assert!(
        !command.contains("task-briefs"),
        "a restart must not re-submit the brief: {command}"
    );
    wait_until(vcx, |_| launches.agent.started(2));
    launches.kill(2);

    let reply = launches.launch(&plain, None, vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| saved_command(&session, cx).is_some());
    assert_eq!(saved_command(&session, vcx), Some(None));

    // The layout file carries them, and only for the panes that have one.
    let path = vcx.read(|cx| shell.read(cx).active(cx).store.path.clone());
    wait_until(vcx, |_| {
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                let sessions = value["terminal_sessions"].as_array()?.clone();
                let commands = sessions
                    .iter()
                    .filter(|session| session.get("startup_command").is_some())
                    .count();
                Some(sessions.len() == 3 && commands == 2)
            })
            .unwrap_or(false)
    });
}

/// A local shell over the fixture repo; the host's launched terminals
/// become its panes.
fn task_shell<'a>(
    launches: &Launches,
    config_dir: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
    shell_over(
        &launches.runs.owner,
        &launches.runs.git.root,
        config_dir,
        cx,
    )
}

/// A local shell over `folder` whose task host is `owner`.
fn shell_over<'a>(
    owner: &Entity<TaskHostOwner>,
    folder: &Path,
    config_dir: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
    let owner = owner.clone();
    cx.update(|cx| cx.set_global(crate::task_host::GpuiTaskHost(owner)));
    let folder = folder.to_path_buf();
    cx.add_window_view(|window, cx| {
        let mut model = crate::workspace_status::WorkspaceStatus::default();
        model.add(folder.clone(), Instant::now());
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, None, cx));
        crate::Shell::with_owner(
            Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            config_dir,
            true,
            window,
            cx,
        )
        .unwrap()
    })
}

/// `task_get`'s `open_sessions` for `task`.
fn open_sessions_of(launches: &Launches, task: &str, cx: &mut TestAppContext) -> serde_json::Value {
    let got = dispatch(
        cx,
        &launches.runs.sender,
        TaskControlOperation::Get(req(serde_json::json!({"task_id": task}))),
    )
    .unwrap();
    got["open_sessions"].clone()
}

/// The shell's session holding task session `session`.
fn session_holding(
    shell: &Entity<crate::Shell>,
    session: &str,
    cx: &mut TestAppContext,
) -> Option<crate::data::SessionId> {
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        state.sessions.iter().map(|s| s.id).find(|id| {
            state.panes[&crate::session_pane_id(*id)]
                .read(cx)
                .task_session(cx)
                .is_some_and(|target| target.task_session_id.as_deref() == Some(session))
        })
    })
}

/// Iced lists a task's tabs as `open_sessions` (src/main.rs
/// `task_control_value`): the pane holding the launched session, in Iced's
/// shape, with the tab's label and workspace. Another task lists none.
#[gpui::test]
fn open_sessions_lists_panes_holding_the_task(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let other = launches.create(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = task_shell(&launches, config.path(), cx);
    assert_eq!(
        open_sessions_of(&launches, &task, vcx),
        serde_json::json!([])
    );
    let reply = launches.launch(&task, Some("Codex"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| session_holding(&shell, &session, cx).is_some());
    let workspace = vcx.read(|cx| shell.read(cx).active(cx).workspace.name.clone());
    let expected = serde_json::json!([{
        "session_id": session,
        "label": "Codex · Fixture task 1",
        "workspace": workspace,
        "is_active": true,
    }]);
    wait_until(vcx, |cx| open_sessions_of(&launches, &task, cx) == expected);
    // The whole reply is core's task_control_value over that one session.
    let got = dispatch(
        vcx,
        &launches.runs.sender,
        TaskControlOperation::Get(req(serde_json::json!({"task_id": task}))),
    )
    .unwrap();
    let sessions: Vec<gitterm_core::task_host::OpenSession> =
        vec![gitterm_core::task_host::OpenSession {
            session_id: Some(session.clone()),
            label: "Codex · Fixture task 1".into(),
            workspace,
            is_active: true,
        }];
    assert_eq!(
        got,
        gitterm_core::task_host::task_control_value(
            &stored_task(&launches.runs.owner, &task, vcx),
            &sessions
        )
        .unwrap()
    );
    assert_eq!(
        open_sessions_of(&launches, &other, vcx),
        serde_json::json!([])
    );
    // task_list carries the same open_sessions per task.
    let listed = dispatch(
        vcx,
        &launches.runs.sender,
        TaskControlOperation::List(req(serde_json::json!({}))),
    )
    .unwrap();
    let listed = |id: &str| {
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["task_id"] == id)
            .unwrap()["open_sessions"]
            .clone()
    };
    assert_eq!(
        (listed(&task), listed(&other)),
        (expected, serde_json::json!([]))
    );
    wait_until(vcx, |_| launches.agent.started(1));
    launches.kill(1);
}

/// Iced's `is_active`: the tab is the active workspace's active tab. A new
/// terminal takes the active pane from the task's session; selecting the
/// session's pane again gives it back.
#[gpui::test]
fn is_active_follows_the_active_pane(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = task_shell(&launches, config.path(), cx);
    let reply = launches.launch(&task, Some("Codex"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    let active = |cx: &mut TestAppContext| {
        open_sessions_of(&launches, &task, cx)
            .as_array()
            .and_then(|sessions| sessions.first().map(|s| s["is_active"].clone()))
    };
    wait_until(vcx, |cx| active(cx) == Some(serde_json::json!(true)));
    shell.update_in(vcx, |shell, window, cx| shell.new_terminal(window, cx));
    wait_until(vcx, |cx| active(cx) == Some(serde_json::json!(false)));
    let id = session_holding(&shell, &session, vcx).unwrap();
    // The strip lists one task context at a time; select the pane as its
    // session tab does.
    shell.update_in(vcx, |shell, window, cx| {
        let active = shell.active_workspace(cx);
        shell
            .workspaces
            .iter_mut()
            .find(|w| Some(w.workspace.id) == active)
            .unwrap()
            .active_session = Some(id);
        shell.show_pane(&crate::session_pane_id(id), window, cx);
    });
    wait_until(vcx, |cx| active(cx) == Some(serde_json::json!(true)));
    wait_until(vcx, |_| launches.agent.started(1));
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
    // Fixture mode refuses a delegate_task with no caller, and so no
    // target it can check.
    let launches = Launches::start(cx);
    assert!(delegate(&launches.runs.sender, cx).contains("not available in preview"));
    assert_eq!(launches.agent.runs(), 0);
}

/// Writes the pinned Claude config with `repo`'s trust bit.
fn pin_claude_trust(launches: &Launches, repo: &Path, trusted: bool) -> PathBuf {
    let roots = &launches.runs.fixture.roots;
    let dir = roots.root().join("provider-homes/claude");
    for dir in [dir.parent().unwrap(), &dir] {
        roots.create_private_dir(dir).unwrap();
    }
    let key = repo.canonicalize().unwrap().display().to_string();
    std::fs::write(
        dir.join(".claude.json"),
        serde_json::json!({"projects": {key: {"hasTrustDialogAccepted": trusted}}}).to_string(),
    )
    .unwrap();
    dir
}

/// Iced's Claude trust write (`claude_task_trust` before every task
/// process start), aimed at the preview's pinned CLAUDE_CONFIG_DIR: with no
/// trust file there nothing is written and the launch goes ahead, as Iced
/// skips an absent `~/.claude.json`; with one, the workspace repository is
/// trusted in it before the child starts. Neither the host HOME nor the
/// child's HOME gets a trust file.
#[gpui::test]
fn fixture_claude_trust_written_to_pinned_config(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let repo = launches.runs.git.repo.clone();
    let root = launches.runs.fixture.roots.root().to_path_buf();
    let pinned = root.join("provider-homes/claude/.claude.json");
    let child_home = root.join("provider-homes/home");
    // The fixture's stand-in for the real HOME, with Iced's trust file.
    let host_home = launches
        .runs
        .fixture
        .iced
        .parent()
        .unwrap()
        .join("fixture-home");
    std::fs::create_dir(&host_home).unwrap();
    std::fs::write(host_home.join(".claude.json"), "{}").unwrap();
    let host_before = tree(&host_home);

    // No pinned trust file: nothing written, the child starts.
    let first = launches.create(cx);
    launches.launch(&first, Some("Claude"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    assert!(!pinned.exists());

    // An untrusted entry: trusted before the child starts, in the file
    // the child's own CLAUDE_CONFIG_DIR names.
    let dir = pin_claude_trust(&launches, &repo, false);
    let task = launches.create(cx);
    let reply = launches.launch(&task, Some("Claude"), cx).unwrap();
    assert!(reply["session_id"].is_string(), "{reply}");
    wait_until(cx, |_| launches.agent.started(2));
    let env = launches.agent.env(2);
    launches.kill(2);
    assert!(
        env.get("CLAUDE_CONFIG_DIR").map(PathBuf::from) == Some(dir.clone()),
        "the child's CLAUDE_CONFIG_DIR is not the dir trust was written to"
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join(".claude.json")).unwrap()).unwrap();
    let key = repo.canonicalize().unwrap().display().to_string();
    assert_eq!(
        saved["projects"][key.as_str()]["hasTrustDialogAccepted"],
        true
    );
    assert_eq!(launches.saved().get(&task).unwrap().sessions.len(), 1);
    assert_eq!(tree(&host_home), host_before);
    assert!(!child_home.join(".claude.json").exists());
    assert!(!child_home.join(".claude").exists());
}

/// Advances the fixture tick (delivery and queue promotion) until `done`.
fn tick_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    wait_until(cx, |cx| {
        cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
        cx.run_until_parked();
        done(cx)
    });
}

/// Iced's `promote_queued_tasks` on the Tick: a launch the concurrency
/// limit queued waits while the slot is held and starts once the running
/// session's child exits, its task then running.
#[gpui::test]
fn queued_launch_promoted_when_a_slot_frees(cx: &mut TestAppContext) {
    let launches = Launches::start_with(
        |runs0| {
            std::fs::write(
                runs0.fixture.iced.join("settings.json"),
                serde_json::to_vec(&serde_json::json!({
                    "max_concurrent_local_tasks": 1,
                    "agent_presets": [{"name": "Codex", "command": "codex"}]
                }))
                .unwrap(),
            )
            .unwrap();
        },
        cx,
    );
    let running = launches.create(cx);
    let waiting = launches.create(cx);
    launches.launch(&running, Some("Codex"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    let queued = launches.launch(&waiting, Some("Codex"), cx).unwrap();
    assert_eq!(queued["queued"], true, "{queued}");
    // The slot is held: a tick starts nothing.
    cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
    cx.run_until_parked();
    assert_eq!(launches.agent.runs(), 1);
    assert_eq!(
        stored_task(&launches.runs.owner, &waiting, cx).lifecycle,
        TaskLifecycle::Queued
    );
    // The running session's child exits, freeing the slot.
    launches.kill(1);
    tick_until(cx, |_| launches.agent.started(2));
    let task = stored_task(&launches.runs.owner, &waiting, cx);
    assert_eq!(task.lifecycle, TaskLifecycle::Running);
    assert_eq!(task.sessions.len(), 1);
    assert_eq!(launches.saved().get(&waiting).unwrap().sessions.len(), 1);
    assert!(
        launches
            .runs
            .owner
            .read_with(cx, |owner, _| owner.host().launch_queue().is_empty())
    );
    launches.kill(2);
}

/// Iced routes a task tab's `task_update_handoff` to its task session: the
/// launched worker's own terminal (its MCP caller) reports done, which
/// completes its delegation; another caller cannot.
#[gpui::test]
fn launched_terminal_handoff_routes_to_its_task_session(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, "gone"))
    });
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |_| launches.agent.started(1));
    let done = |caller: &str, cx: &mut TestAppContext| {
        dispatch_as(
            cx,
            &launches.runs.sender,
            TaskControlOperation::UpdateHandoff(req(serde_json::json!({
                "task_id": task, "summary": "Finished", "status": "done"
            }))),
            Some(caller),
        )
    };
    // Some other pane is not the worker's.
    assert!(done(&launches.runs.caller, cx).is_err());
    let reply = done(&session, cx).unwrap();
    assert_eq!(reply["delegation_id"], "worker", "{reply}");
    assert_eq!(reply["delegation_status"], "completed", "{reply}");
    let saved = launches.saved();
    let handoff = saved.get(&task).unwrap().handoff.clone().unwrap();
    assert_eq!(
        handoff.updated_by_session_id.as_deref(),
        Some(session.as_str())
    );
    assert_eq!(
        saved.get(&task).unwrap().lifecycle,
        TaskLifecycle::Completed
    );
    launches.kill(1);
}

/// Iced's exit path for a task tab (`Action::Shutdown`): an adopted agent
/// terminal whose child exits stops its task and shows exited; a worker
/// that already reported done keeps its task's Completed.
#[gpui::test]
fn adopted_terminal_exit_updates_its_task(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let plain = launches.create(cx);
    let worker = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &worker, "gone"))
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = task_shell(&launches, config.path(), cx);
    let pane_exited = |session: &str, cx: &mut TestAppContext| {
        cx.read(|cx| {
            shell.read(cx).active(cx).panes.values().any(|pane| {
                let pane = pane.read(cx);
                pane.session
                    .as_ref()
                    .is_some_and(|target| target.task_session_id.as_deref() == Some(session))
                    && pane.tab_title().ends_with("· Exited")
            })
        })
    };
    let reply = launches.launch(&plain, Some("Codex"), vcx).unwrap();
    let first = reply["session_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| session_holding(&shell, &first, cx).is_some());
    wait_until(vcx, |_| launches.agent.started(1));
    assert_eq!(
        stored_task(&launches.runs.owner, &plain, vcx).lifecycle,
        TaskLifecycle::Running
    );
    launches.kill(1);
    // The terminal view polls its backend on a timer.
    tick_until(vcx, |cx| pane_exited(&first, cx));
    wait_until(vcx, |_| {
        launches.saved().get(&plain).unwrap().lifecycle == TaskLifecycle::Stopped
    });

    let reply = launches.launch(&worker, Some("Codex"), vcx).unwrap();
    let second = reply["session_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| session_holding(&shell, &second, cx).is_some());
    wait_until(vcx, |_| launches.agent.started(2));
    dispatch_as(
        vcx,
        &launches.runs.sender,
        TaskControlOperation::UpdateHandoff(req(serde_json::json!({
            "task_id": worker, "summary": "Finished", "status": "done"
        }))),
        Some(&second),
    )
    .unwrap();
    launches.kill(2);
    tick_until(vcx, |cx| pane_exited(&second, cx));
    vcx.run_until_parked();
    assert_eq!(
        launches.saved().get(&worker).unwrap().lifecycle,
        TaskLifecycle::Completed
    );
}

/// Iced's launch adds the work's documents note to the instructions and
/// passes Claude in Chrome as the task's saved workspace sets it, read from
/// the workspaces the fixture host was given.
#[gpui::test]
fn launch_adds_documents_note_and_workspace_chrome(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let name = stored_task(&launches.runs.owner, &task, cx).workspace.name;
    cx.update(|cx| {
        let source = cx.global_mut::<crate::work_sidebar::IcedWorkSource>();
        // The fixture's workspace, under the name the task was given.
        let ws = &mut source.workspaces[0];
        ws.name = name;
        ws.chats = vec!["root-chat".into()];
        ws.claude_in_chrome = true;
    });
    let docs = launches.runs.git.root.join("docs");
    launches.launch(&task, Some("Codex"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    let instructions = launches
        .agent
        .args(1)
        .into_iter()
        .find(|arg| arg.starts_with("developer_instructions="))
        .unwrap();
    assert!(
        instructions.contains("# Documents for this work (added by GitTerm)")
            && instructions.contains(&docs.display().to_string()),
        "{instructions}"
    );

    let repo = launches.runs.git.repo.clone();
    pin_claude_trust(&launches, &repo, true);
    launches.launch(&task, Some("Claude"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(2));
    launches.kill(2);
    let args = launches.agent.args(2);
    assert!(args.contains(&"--chrome".to_string()), "{args:?}");
    assert!(!args.contains(&"--no-chrome".to_string()), "{args:?}");
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

#[path = "task_host_delegate_tests.rs"]
mod delegate_tests;

/// A shell restarted over a layout in `config` whose one terminal pane
/// saved `saved`'s fields (cwd, startup_command, task_id, task_session_id):
/// a first shell opens a plain terminal and saves its layout, its window
/// closes, the entry is edited, and a second shell restores it.
fn restored_shell<'a>(
    owner: &Entity<TaskHostOwner>,
    folder: &Path,
    config: &Path,
    saved: serde_json::Value,
    cx: &'a mut TestAppContext,
) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
    let path = {
        let (shell, vcx) = shell_over(owner, folder, config, cx);
        vcx.update(|window, cx| shell.update(cx, |shell, cx| shell.new_terminal(window, cx)));
        let path = vcx.read(|cx| shell.read(cx).active(cx).store.path.clone());
        wait_until(vcx, |_| saved_sessions(&path).is_some_and(|s| s.len() == 1));
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();
        path
    };
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let entry = &mut value["terminal_sessions"][0];
    for (key, field) in saved.as_object().unwrap() {
        entry[key] = field.clone();
    }
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    shell_over(owner, folder, config, cx)
}

/// The terminal sessions in the layout file at `path`.
fn saved_sessions(path: &Path) -> Option<Vec<serde_json::Value>> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value["terminal_sessions"].as_array().cloned()
}

/// The restored shell's one terminal pane.
fn only_pane(shell: &Entity<crate::Shell>, cx: &gpui::App) -> Entity<crate::pane::Pane> {
    let state = shell.read(cx).active(cx);
    assert_eq!(state.sessions.len(), 1);
    state.panes[&crate::session_pane_id(state.sessions[0].id)].clone()
}

/// Iced's relaunch restore (src/main.rs, the saved tab loop, then
/// `create_tab_with_session_uid`): a fixture task pane with a saved command
/// runs it once, in its saved folder, as its task session, in a terminal
/// the host started and the pane adopted; the layout keeps its fields.
#[gpui::test]
fn fixture_restore_runs_the_saved_command_in_the_saved_folder(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let folder = worktree.join("sub");
    std::fs::create_dir(&folder).unwrap();
    let config = tempfile::tempdir().unwrap();
    let saved = serde_json::json!({
        "cwd": folder, "startup_command": "codex",
        "task_id": task, "task_session_id": "restored-session",
    });
    let (shell, vcx) = restored_shell(
        &launches.runs.owner,
        &launches.runs.git.root,
        config.path(),
        saved,
        cx,
    );
    wait_until(vcx, |_| launches.agent.started(1));
    settle(vcx);
    assert_eq!(launches.agent.runs(), 1);
    assert_eq!(
        launches.agent.read("pwd", 1).unwrap().trim(),
        folder.canonicalize().unwrap().display().to_string()
    );
    let target = vcx.read(|cx| only_pane(&shell, cx).read(cx).session.clone().unwrap());
    assert_eq!(target.task_id.as_deref(), Some(task.as_str()));
    assert_eq!(target.task_session_id.as_deref(), Some("restored-session"));
    assert_eq!(target.session_uid, "restored-session");
    // The pane owns the terminal; the host kept none.
    assert!(launches.host_terminals(vcx).is_empty());
    assert!(vcx.read(|cx| only_pane(&shell, cx).read(cx).terminal_alive()));
    let terminals = vcx.read(|cx| shell.read(cx).active(cx).store.terminals.clone().unwrap());
    assert_eq!(terminals[0].startup_command.as_deref(), Some("codex"));
    assert_eq!(terminals[0].task_id.as_deref(), Some(task.as_str()));
    assert_eq!(
        terminals[0].task_session_id.as_deref(),
        Some("restored-session")
    );
    launches.kill(1);
}

/// Iced's folder rule: a saved folder that is gone restores in the task's
/// worktree; with that gone too, a plain shell that keeps the saved fields.
#[gpui::test]
fn restore_falls_back_to_the_worktree_then_a_plain_shell(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let saved = serde_json::json!({
        "cwd": worktree.join("gone"), "startup_command": "codex",
        "task_id": task, "task_session_id": "restored-session",
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = restored_shell(
        &launches.runs.owner,
        &launches.runs.git.root,
        config.path(),
        saved.clone(),
        cx,
    );
    wait_until(vcx, |_| launches.agent.started(1));
    assert_eq!(
        launches.agent.read("pwd", 1).unwrap().trim(),
        worktree.canonicalize().unwrap().display().to_string()
    );
    let restored = vcx.read(|cx| shell.read(cx).active(cx).store.terminals.clone().unwrap());
    assert_eq!(restored[0].cwd, worktree);
    launches.kill(1);
    vcx.update(|window, _| window.remove_window());

    let moved = worktree.with_file_name("wt-moved");
    std::fs::rename(&worktree, &moved).unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = restored_shell(
        &launches.runs.owner,
        &launches.runs.git.root,
        config.path(),
        saved,
        cx,
    );
    settle(vcx);
    assert_eq!(launches.agent.runs(), 1);
    let pane = vcx.read(|cx| only_pane(&shell, cx));
    assert!(vcx.read(|cx| pane.read(cx).task_session(cx).is_none()));
    let terminals = vcx.read(|cx| shell.read(cx).active(cx).store.terminals.clone().unwrap());
    assert_eq!(terminals[0].startup_command.as_deref(), Some("codex"));
    assert_eq!(terminals[0].task_id.as_deref(), Some(task.as_str()));
    assert_eq!(
        terminals[0].task_session_id.as_deref(),
        Some("restored-session")
    );
    std::fs::rename(&moved, &worktree).unwrap();
}

/// Plain preview never starts a saved command: the restored pane is a
/// plain shell (it echoes a marker, so it ran) and the fake agent never
/// starts; the layout file still holds the saved fields after a save.
#[gpui::test]
fn preview_restore_starts_a_plain_shell_and_keeps_the_saved_fields(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git = GitFixture::new(&fixture);
    fixture.write_iced(
        vec![task_at("worker", TaskLifecycle::Running, &git.worktree)],
        vec![],
    );
    let _chats = Chats::new(&fixture, cx);
    let (owner, _sender) = fixture.start(cx);
    let agent = FakeAgent::new();
    // A path to the fake, so a wrong start could never run a real agent.
    let command = agent.dir.path().join("codex").display().to_string();
    let config = tempfile::tempdir().unwrap();
    let saved = serde_json::json!({
        "cwd": git.worktree, "startup_command": command,
        "task_id": "worker", "task_session_id": "child-1",
    });
    let (shell, vcx) = restored_shell(&owner, &git.root, config.path(), saved, cx);
    plain_shell_answers(&shell, vcx);
    assert_eq!(agent.runs(), 0);
    // A layout save keeps the saved fields.
    let path = vcx.read(|cx| shell.read(cx).active(cx).store.path.clone());
    std::fs::remove_file(&path).unwrap();
    vcx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            crate::save_layout(&mut shell.workspaces[0], cx)
        })
    });
    wait_until(vcx, |_| saved_sessions(&path).is_some());
    let sessions = saved_sessions(&path).unwrap();
    assert_eq!(sessions[0]["startup_command"], command);
    assert_eq!(sessions[0]["task_id"], "worker");
    assert_eq!(sessions[0]["task_session_id"], "child-1");
    assert_eq!(agent.runs(), 0);
}

/// Iced's restore rewrite with the fixture child's Claude transcripts
/// (under its pinned CLAUDE_CONFIG_DIR, never ~/.claude): a resume whose
/// transcript was never written starts a fresh `claude`, and a pre-assigned
/// `--session-id` whose transcript exists is resumed.
#[gpui::test]
fn fixture_restore_rewrites_claude_against_the_fixture_transcripts(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let restore = |command: &str, cx: &mut TestAppContext| {
        let config = tempfile::tempdir().unwrap();
        let saved = serde_json::json!({
            "cwd": worktree, "startup_command": command,
            "task_id": task, "task_session_id": "restored-session",
        });
        let (shell, vcx) = restored_shell(
            &launches.runs.owner,
            &launches.runs.git.root,
            config.path(),
            saved,
            cx,
        );
        let saved = vcx.read(|cx| {
            shell.read(cx).active(cx).store.terminals.clone().unwrap()[0]
                .startup_command
                .clone()
        });
        vcx.update(|window, _| window.remove_window());
        saved
    };
    assert_eq!(
        restore("claude --resume abc", cx).as_deref(),
        Some("claude")
    );
    wait_until(cx, |_| launches.agent.started(1));
    assert!(!launches.agent.args(1).contains(&"--resume".to_string()));
    launches.kill(1);

    let roots = &launches.runs.fixture.roots;
    let projects = roots.root().join("provider-homes/claude/projects");
    for dir in [
        projects.parent().unwrap().parent().unwrap(),
        projects.parent().unwrap(),
        &projects,
        &projects.join("fixture-slug"),
    ] {
        roots.create_private_dir(dir).unwrap();
    }
    std::fs::write(projects.join("fixture-slug/abc.jsonl"), "{}\n").unwrap();
    assert_eq!(
        restore("claude --session-id abc", cx).as_deref(),
        Some("claude --resume abc")
    );
    wait_until(cx, |_| launches.agent.started(2));
    let args = launches.agent.args(2);
    assert!(
        args.windows(2).any(|pair| pair == ["--resume", "abc"]),
        "{args:?}"
    );
    launches.kill(2);
}

/// The restored pane is a plain shell, not a task session, and it runs
/// what it is sent (it echoes a marker): the restore has finished.
fn plain_shell_answers(shell: &Entity<crate::Shell>, vcx: &mut gpui::VisualTestContext) {
    let pane = vcx.read(|cx| only_pane(shell, cx));
    let view = vcx.read(|cx| match &pane.read(cx).content {
        crate::pane::PaneContent::Terminal(view) => view.clone(),
        _ => panic!("a plain shell is restored"),
    });
    view.update_in(vcx, |view, _, _| {
        view.backend_mut().handle(gitterm_terminal::Command::Write(
            b"echo RESTORED_PLAIN_$((40+2))\r".to_vec(),
        ));
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    while !vcx.read(|cx| {
        view.read(cx)
            .backend()
            .get_all_text()
            .is_some_and(|text| text.contains("RESTORED_PLAIN_42"))
    }) {
        assert!(Instant::now() < deadline, "the plain shell did not answer");
        vcx.run_until_parked();
        vcx.executor().advance_clock(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(vcx.read(|cx| pane.read(cx).task_session(cx).is_none()));
}

/// Issue 86: a saved command's `--add-dir` folders are under the fixture
/// guard too. One outside the fixture restores a plain shell, starts
/// nothing and keeps the saved fields.
#[gpui::test]
fn restore_refuses_a_saved_add_dir_outside_the_fixture(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let outside = tempfile::tempdir().unwrap();
    let command = format!(
        "codex --add-dir {}",
        gitterm_core::workers::shell_word(&outside.path().display().to_string())
    );
    let saved = serde_json::json!({
        "cwd": worktree, "startup_command": command,
        "task_id": task, "task_session_id": "restored-session",
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = restored_shell(
        &launches.runs.owner,
        &launches.runs.git.root,
        config.path(),
        saved,
        cx,
    );
    plain_shell_answers(&shell, vcx);
    assert_eq!(launches.agent.runs(), 0);
    assert!(launches.host_terminals(vcx).is_empty());
    let terminals = vcx.read(|cx| shell.read(cx).active(cx).store.terminals.clone().unwrap());
    assert_eq!(
        terminals[0].startup_command.as_deref(),
        Some(command.as_str())
    );
    assert_eq!(terminals[0].task_id.as_deref(), Some(task.as_str()));
    assert_eq!(
        terminals[0].task_session_id.as_deref(),
        Some("restored-session")
    );
}

/// A saved `--add-dir` folder inside the fixture passes the guard: the
/// command runs with it.
#[gpui::test]
fn restore_runs_a_saved_add_dir_inside_the_fixture(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let notes = launches.runs.git.repo.clone();
    let command = format!(
        "codex --add-dir {}",
        gitterm_core::workers::shell_word(&notes.display().to_string())
    );
    let saved = serde_json::json!({
        "cwd": worktree, "startup_command": command,
        "task_id": task, "task_session_id": "restored-session",
    });
    let config = tempfile::tempdir().unwrap();
    let (_shell, vcx) = restored_shell(
        &launches.runs.owner,
        &launches.runs.git.root,
        config.path(),
        saved,
        cx,
    );
    wait_until(vcx, |_| launches.agent.started(1));
    let args = launches.agent.args(1);
    let notes = notes.display().to_string();
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--add-dir" && pair[1] == notes),
        "{args:?}"
    );
    launches.kill(1);
}

/// Iced prunes a launched Claude session's pre-assigned conversation ref
/// when no transcript was written (src/main.rs, before tabs restore): the
/// restore starts a fresh `claude`, never `--resume` of a missing id.
#[gpui::test]
fn restore_of_an_unmessaged_claude_launch_starts_fresh(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    pin_claude_trust(&launches, &launches.runs.git.repo, true);
    launches.launch(&task, Some("Claude"), cx).unwrap();
    wait_until(cx, |_| launches.agent.started(1));
    launches.kill(1);
    let (session_id, chat_id) = stored_task(&launches.runs.owner, &task, cx)
        .sessions
        .iter()
        .find_map(|session| {
            let conversation = session.conversation.as_ref()?;
            Some((
                session.task_session_id.clone(),
                conversation.session_id.clone(),
            ))
        })
        .expect("the Claude launch records its pre-assigned conversation");
    let saved = serde_json::json!({
        "cwd": worktree, "startup_command": format!("claude --resume {chat_id}"),
        "task_id": task, "task_session_id": session_id,
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = restored_shell(
        &launches.runs.owner,
        &launches.runs.git.root,
        config.path(),
        saved,
        cx,
    );
    wait_until(vcx, |_| launches.agent.started(2));
    let restored = vcx.read(|cx| shell.read(cx).active(cx).store.terminals.clone().unwrap());
    assert_eq!(restored[0].startup_command.as_deref(), Some("claude"));
    let args = launches.agent.args(2);
    assert!(!args.contains(&"--resume".to_string()), "{args:?}");
    launches.kill(2);
}

#[path = "task_host_launch_turn_tests.rs"]
mod turn_tests;

/// Issue 86 F1: a saved command not in the form GPUI's launch saves (a
/// wrapper around a harness, a quoted executable, an option GPUI never
/// writes) restores a plain shell, starts nothing and keeps the saved
/// fields.
#[gpui::test]
fn restore_runs_only_a_command_in_gpuis_saved_form(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let outside = tempfile::tempdir().unwrap();
    let word = |path: &Path| gitterm_core::workers::shell_word(&path.display().to_string());
    for command in [
        format!("env codex --cd {}", word(outside.path())),
        "co'dex'".to_string(),
        "codex --sandbox danger-full-access".to_string(),
    ] {
        let saved = serde_json::json!({
            "cwd": worktree, "startup_command": command,
            "task_id": task, "task_session_id": "restored-session",
        });
        let config = tempfile::tempdir().unwrap();
        let (shell, vcx) = restored_shell(
            &launches.runs.owner,
            &launches.runs.git.root,
            config.path(),
            saved,
            cx,
        );
        plain_shell_answers(&shell, vcx);
        assert_eq!(launches.agent.runs(), 0, "{command}");
        assert!(launches.host_terminals(vcx).is_empty(), "{command}");
        let terminals = vcx.read(|cx| shell.read(cx).active(cx).store.terminals.clone().unwrap());
        assert_eq!(
            terminals[0].startup_command.as_deref(),
            Some(command.as_str())
        );
        assert_eq!(terminals[0].task_id.as_deref(), Some(task.as_str()));
        vcx.update(|window, _| window.remove_window());
    }
}

/// GPUI's own saved forms still run on restore: a Codex launch's saved
/// command (its notify hook included), and a Claude worker's resume with
/// options whose transcript was never written, which starts a fresh
/// `claude` with its options (issue 88).
#[gpui::test]
fn restore_runs_gpuis_own_saved_forms(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx) = task_shell(&launches, config.path(), cx);
    let reply = launches.launch(&task, Some("Codex"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    let saved_command = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            shell
                .read(cx)
                .active(cx)
                .store
                .terminals
                .as_ref()?
                .iter()
                .find(|t| t.task_session_id.as_deref() == Some(session.as_str()))?
                .startup_command
                .clone()
        })
    };
    wait_until(vcx, |cx| saved_command(cx).is_some());
    let command = saved_command(vcx).unwrap();
    assert!(command.contains("notify=["), "{command}");
    wait_until(vcx, |_| launches.agent.started(1));
    launches.kill(1);
    vcx.update(|window, _| window.remove_window());

    let restore = |command: &str, session: &str, cx: &mut TestAppContext| {
        let saved = serde_json::json!({
            "cwd": worktree, "startup_command": command,
            "task_id": task, "task_session_id": session,
        });
        let config = tempfile::tempdir().unwrap();
        let (shell, vcx) = restored_shell(
            &launches.runs.owner,
            &launches.runs.git.root,
            config.path(),
            saved,
            cx,
        );
        let saved = vcx.read(|cx| {
            shell.read(cx).active(cx).store.terminals.clone().unwrap()[0]
                .startup_command
                .clone()
        });
        vcx.update(|window, _| window.remove_window());
        saved
    };
    assert_eq!(
        restore(&command, &session, cx).as_deref(),
        Some(command.as_str())
    );
    wait_until(cx, |_| launches.agent.started(2));
    assert!(launches.agent.args(2).contains(&"--config".to_string()));
    launches.kill(2);

    let notes = gitterm_core::workers::shell_word(&launches.runs.git.repo.display().to_string());
    let fresh = format!("claude --model opus --effort high --add-dir {notes}");
    assert_eq!(
        restore(
            &format!("claude --model opus --effort high --add-dir {notes} --resume abc"),
            "restored-session",
            cx
        ),
        Some(fresh)
    );
    wait_until(cx, |_| launches.agent.started(3));
    let args = launches.agent.args(3);
    assert!(!args.contains(&"--resume".to_string()), "{args:?}");
    assert!(
        args.windows(2).any(|pair| pair == ["--model", "opus"]),
        "{args:?}"
    );
    launches.kill(3);
}
