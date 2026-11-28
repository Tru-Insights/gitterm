// Runs inside live_writes' subprocess harness: HOME and the absolute V6 root
// are synthetic before any GPUI/core service is constructed.
struct LiveFake {
    trace: tempfile::TempDir,
    spec:
        std::rc::Rc<std::cell::RefCell<Option<gitterm_core::task_host::command::TerminalCommand>>>,
}
impl LiveFake {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let trace = tempfile::tempdir().unwrap();
        let bin = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = r#"#!/bin/sh
n=$(/usr/bin/find 'DIR' -name 'pid-*' | /usr/bin/wc -l | /usr/bin/tr -d ' ')
n=$((n + 1))
echo $$ > "DIR/pid-$n"
/usr/bin/env | /usr/bin/sort > "DIR/env-$n"
printf '%s\n' "$@" > "DIR/args-$n"
pwd -P > "DIR/pwd-$n"
printf ready > "DIR/ready-$n"
if [ "$1" = exec ]; then
  cat > "DIR/stdin-$n"
  echo '{"type":"thread.started","thread_id":"placeholder-thread"}'
  echo '{"type":"turn.started"}'
  echo '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Placeholder result"}}'
  echo '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'
  exit 0
fi
trap 'echo term >> "DIR/term-'"$n"'"; exec /bin/sleep 60' TERM
read -r placeholder
exec /bin/sleep 60
"#.replace("DIR", &trace.path().display().to_string());
        for name in ["claude", "codex"] {
            let path = bin.join(name);
            std::fs::write(&path, &script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Only this already isolated test subprocess changes PATH. Codex's
        // detected production environment now resolves the fake before all else.
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin",
                    bin.display()
                ),
            );
            std::env::set_var("SHELL", "/bin/zsh");
            std::env::remove_var("CODEX_HOME");
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        Self {
            trace,
            spec: Default::default(),
        }
    }
    fn ignore_hup(&self) {
        let bin = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/bin");
        for name in ["claude", "codex"] {
            let path = bin.join(name);
            let script = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                path,
                script.replacen("#!/bin/sh\n", "#!/bin/sh\ntrap '' HUP\n", 1),
            )
            .unwrap();
        }
    }
    fn file(&self, name: &str, run: usize) -> PathBuf {
        self.trace.path().join(format!("{name}-{run}"))
    }
    fn text(&self, name: &str, run: usize) -> String {
        std::fs::read_to_string(self.file(name, run)).unwrap()
    }
    fn prepare_recorder(&self, owner: &Entity<TaskHostOwner>, cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::env::current_exe().unwrap();
        let recorder = self.trace.path().join("recorder");
        let quote = gitterm_core::workers::shell_word;
        std::fs::write(&recorder, format!(
            "#!/bin/sh\nexport GITTERM_TEST_RECORD_PATH=\"$2\" GITTERM_TEST_RECORD_SHELL=\"$3\"\nexec {} --exact task_host_tests::live_writes::live_recorder --nocapture\n", quote(exe.to_str().unwrap())
        )).unwrap();
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = format!("'{}'", exe.display().to_string().replace('\'', "'\\''"));
        let new = format!("'{}'", recorder.display());
        let captured = self.spec.clone();
        owner.update(cx, |owner, _| {
            owner.pause_live_worker_tick();
            owner.terminal_spec_hook = Some(Box::new(move |spec| {
                let command = spec.env.get_mut("GITTERM_STARTUP_CMD").unwrap();
                assert!(command.contains(&old));
                *command = command.replace(&old, &new);
                *captured.borrow_mut() = Some(spec.clone());
            }));
        });
    }
}

struct LiveProbe {
    child: std::process::Child,
    process: Option<gitterm_core::worker_process::WorkerProcess>,
}
impl Drop for LiveProbe {
    fn drop(&mut self) {
        if let Some(process) = self.process.take()
            && let Err(error) = process.stop()
        {
            eprintln!("stop fake probe: {error}");
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn live_recorder() {
    let Some(path) = std::env::var_os("GITTERM_TEST_RECORD_PATH") else {
        return;
    };
    let shell = std::env::var("GITTERM_TEST_RECORD_SHELL")
        .unwrap()
        .parse()
        .unwrap();
    gitterm_core::worker_process::record_launch(Path::new(&path), shell).unwrap();
}

fn worker_settings(fixture: &Fixture) {
    std::fs::write(
        fixture.iced.join("settings.json"),
        serde_json::to_vec(&json!({
            "agent_presets": [
                {"name": "Claude", "command": "claude --model placeholder"},
                {"name": "Codex", "command": "codex -m placeholder"}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn fresh_limits(fixture: &Fixture, held: bool) {
    use gitterm_core::plan_limits::{LimitWindow, PlanLimits, PlanLimitsFile, Provider};
    let now = chrono::Utc::now().timestamp();
    let mut file = PlanLimitsFile::default();
    for provider in [Provider::Codex, Provider::Claude] {
        file.absorb(PlanLimits {
            provider,
            observed_at: now,
            local: Some(true),
            windows: vec![LimitWindow {
                label: "7d".into(),
                used_percent: Some(if held { 99.0 } else { 0.0 }),
                resets_at: Some(now + 86400),
            }],
            alerts: vec![],
            plan_type: None,
            credits: None,
        });
    }
    file.save(&PlanLimitsFile::path_in(&fixture.iced)).unwrap();
}

fn launch_live(
    cx: &mut TestAppContext,
    sender: &TaskControlSender,
    id: &str,
    preset: &str,
) -> Result<serde_json::Value, String> {
    dispatch(
        cx,
        sender,
        TaskControlOperation::LaunchSession(req(json!({"task_id": id, "preset_name": preset}))),
    )
}

#[gpui::test]
fn live_workers_launch_record_identity_and_restart_without_adoption(cx: &mut TestAppContext) {
    if child("live_workers_launch_record_identity_and_restart_without_adoption") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    for (n, preset) in [(1, "Claude"), (3, "Codex")] {
        let created = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
            "title": format!("Placeholder {preset}"), "objective": "Placeholder objective", "repository_path": git.repo, "issue_key": "TRU-274"
        })))).unwrap();
        let id = created["task_id"].as_str().unwrap();
        let result = launch_live(cx, &sender, id, preset).unwrap();
        let session = result["session_id"].as_str().unwrap();
        wait_until(cx, |_| fake.file("ready", n).exists());
        wait_until(cx, |cx| {
            owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
            TaskStore::load(fixture.source())
                .unwrap()
                .get(id)
                .unwrap()
                .sessions[0]
                .launch_identity
                .is_some()
        });
        assert_eq!(
            PathBuf::from(fake.text("pwd", n).trim()),
            PathBuf::from(created["worktree"]["path"].as_str().unwrap())
                .canonicalize()
                .unwrap()
        );
        let env = fake.text("env", n);
        assert!(env.contains(&format!("HOME={}\n", std::env::var("HOME").unwrap())));
        assert!(env.contains(&format!(
            "GITTERM_V6_CONFIG_DIR={}\n",
            fixture.iced.display()
        )));
        assert!(env.contains("GITTERM_V6_TASK_MCP_URL="));
        assert!(fake.text("args", n).contains("placeholder"));
        // The core terminal spec is pinned against pre-extraction Iced by
        // command_tests. Execute its startup in the same empty synthetic
        // home, and compare what the provider actually receives from GPUI.
        let mut iced = fake.spec.borrow().clone().unwrap();
        let startup = iced.env.remove("GITTERM_STARTUP_CMD").unwrap().replace(
            &format!("'{}'", fake.trace.path().join("recorder").display()),
            "/usr/bin/true",
        );
        let probe = std::process::Command::new(&iced.program)
            .args(["-l", "-c", &startup])
            .envs(&iced.env)
            // Iced's PTY receives its tab id; GPUI's backend currently uses
            // 278. Give the Iced probe the same synthetic terminal id.
            .env("ALACRITTY_WINDOW_ID", "278")
            .env("WINDOWID", "278")
            .current_dir(&iced.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut process = gitterm_core::worker_process::WorkerProcess::new().unwrap();
        gitterm_core::worker_process::record_process(&process.pid_file(), probe.id()).unwrap();
        process.persisted_identity = process.launch_identity().unwrap();
        let probe = LiveProbe {
            child: probe,
            process: Some(process),
        };
        wait_until(cx, |_| fake.file("ready", n + 1).exists());
        for file in ["args", "pwd"] {
            assert_eq!(
                fake.text(file, n),
                fake.text(file, n + 1),
                "{preset}: {file}"
            );
        }
        let environment = |run| -> std::collections::BTreeMap<String, String> {
            fake.text("env", run)
                .lines()
                .filter_map(|line| line.split_once('='))
                .filter(|(key, _)| !["PWD", "OLDPWD", "SHLVL", "_"].contains(key))
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        };
        let actual = environment(n);
        let expected = environment(n + 1);
        let differing: Vec<_> = actual
            .keys()
            .chain(expected.keys())
            .filter(|key| actual.get(*key) != expected.get(*key))
            .collect();
        assert!(
            differing.is_empty(),
            "{preset} environment names differ: {differing:?}"
        );
        drop(probe);

        assert!(owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .get(id)
                .unwrap()
                .sessions
                .iter()
                .any(|record| record.task_session_id == session)
        }));
    }
    owner.update(cx, |owner, cx| {
        let callers: Vec<_> = owner.host_terminals();
        assert_eq!(callers.len(), 2);
        owner.quit(cx);
        for caller in callers {
            assert!(owner.host().require_issued_caller(Some(&caller), "consult_request").is_err());
        }
    });
    let (restarted, _) = start(&fixture, cx);
    restarted.read_with(cx, |owner, _| {
        assert!(owner.host_terminals().is_empty());
        assert!(owner.terminal_working_sessions().is_empty());
    });
    restarted.update(cx, |owner, _| owner.shutdown());
}

#[gpui::test]
fn live_plan_hold_and_queue_promotion(cx: &mut TestAppContext) {
    if child("live_plan_hold_and_queue_promotion") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, true);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let created = create(cx, &sender, &git.repo);
    let id = created["task_id"].as_str().unwrap();
    assert_eq!(
        launch_live(cx, &sender, id, "Codex").unwrap()["queued"],
        true
    );
    owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
    assert!(!fake.file("ready", 1).exists());
    fresh_limits(&fixture, false);
    owner.update(cx, |owner, cx| {
        owner.test_reset_plan_limits(&fixture.iced);
        owner.live_worker_tick(cx);
    });
    wait_until(cx, |_| fake.file("ready", 1).exists());
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_save_failure_stops_unpublished_fake(cx: &mut TestAppContext) {
    if child("live_save_failure_stops_unpublished_fake") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    fake.ignore_hup();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let created = create(cx, &sender, &git.repo);
    let id = created["task_id"].as_str().unwrap();
    let trace = fake.trace.path().to_path_buf();
    let root = fixture.iced.clone();
    owner.update(cx, |owner, _| {
        owner.before_publish = Some(Box::new(move |_| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !trace.join("ready-1").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            live_set_mode(&root, 0o500);
        }))
    });
    let began = Instant::now();
    let failed = launch_live(cx, &sender, id, "Claude");
    live_set_mode(&fixture.iced, 0o700);
    assert!(failed.unwrap_err().contains("atomically write"));
    assert!(owner.read_with(cx, |owner, _| owner.host_terminals().is_empty()));
    let spec = fake.spec.borrow();
    let url = &spec.as_ref().unwrap().env["GITTERM_V6_TASK_MCP_URL"];
    let caller = url.split("caller=").nth(1).unwrap();
    assert!(owner.read_with(cx, |owner, _| {
        owner.host().require_issued_caller(Some(caller), "consult_request").is_err()
    }));
    let pid: u32 = fake.text("pid", 1).trim().parse().unwrap();
    wait_until(cx, |_| {
        gitterm_core::worker_process::macos::info(pid)
            .unwrap()
            .is_none()
    });
    assert!(
        fake.file("term", 1).exists(),
        "worker must receive TERM despite ignoring HUP"
    );
    assert!(
        began.elapsed() >= Duration::from_secs(2),
        "KILL must wait for grace"
    );
    owner.update(cx, |owner, cx| owner.quit(cx));
}

fn live_set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn open_live_caller(
    fixture: &Fixture,
    git: &GitFixture,
    cx: &mut TestAppContext,
) -> (Chats, String) {
    let chats = Chats::new(fixture, cx);
    let workspace = req(
        json!({"name": "FX", "abbrev": "FX", "color": "blue", "dir": git.root, "repos": [git.repo], "tabs": []}),
    );
    cx.update(|cx| {
        cx.global_mut::<crate::work_sidebar::IcedWorkSource>()
            .workspaces = vec![workspace]
    });
    let caller = chats.open_in(fixture, Some("placeholder-chat"), &git.repo, cx);
    (chats, caller)
}

#[gpui::test]
fn live_review_consult_match_iced_runner_argv_env_cwd(cx: &mut TestAppContext) {
    if child("live_review_consult_match_iced_runner_argv_env_cwd") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    fresh_limits(&fixture, false);
    let (_chats, caller) = open_live_caller(&fixture, &git, cx);
    let (owner, sender) = start(&fixture, cx);
    owner.read_with(cx, |owner, _| owner.claude_mcp_servers(&caller));
    for (n, review) in [(1, true), (3, false)] {
        let operation = if review {
            TaskControlOperation::RequestReview(req(
                json!({"target": "uncommitted", "checkout": git.repo, "model": "placeholder"}),
            ))
        } else {
            TaskControlOperation::RequestConsult(req(
                json!({"brief": "Placeholder advice", "checkout": git.repo, "model": "placeholder"}),
            ))
        };
        let result = dispatch_as(cx, &sender, operation, Some(&caller)).unwrap();
        assert_eq!(result["queued"], false, "{result}");
        let id = result["delegation_id"].as_str().unwrap();
        wait_until(cx, |cx| {
            owner.read_with(cx, |owner, _| {
                owner
                    .host()
                    .store()
                    .as_ref()
                    .unwrap()
                    .delegation(id)
                    .unwrap()
                    .status
                    .is_final()
            })
        });
        let record = TaskStore::load(fixture.source())
            .unwrap()
            .delegation(id)
            .unwrap()
            .clone();
        assert!(record.result.is_some());
        assert!(!record.is_delivered());
        let mut iced_run = gitterm_core::delegations::codex_run(&record, &fixture.iced).unwrap();
        iced_run.guidelines = owner.read_with(cx, |owner, cx| {
            owner.launch_instructions(
                &iced_run.cwd,
                gitterm_core::guidelines::InstructionFile::AgentsMd,
                &record.parent.workspace,
                false,
                cx,
            )
        });
        // This is Iced's exact runner entry, with the same detected environment.
        // Only the scripts installed by LiveFake can be resolved on PATH.
        let outcome = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let (progress, _events) = tokio::sync::mpsc::unbounded_channel();
            runtime.block_on(gitterm_core::codex_runner::run(iced_run, progress))
        })
        .join()
        .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        for file in ["args", "env", "pwd", "stdin"] {
            assert_eq!(
                fake.text(file, n),
                fake.text(file, n + 1),
                "{file}, review={review}"
            );
        }
    }
    let delivered = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = delivered.clone();
    owner.update(cx, |owner, cx| {
        owner.submit_hook = Some(Box::new(move |submission| { captured.borrow_mut().push(submission.clone()); Some(true) }));
        owner.deliver_results(cx);
        owner.deliver_results(cx);
    });
    assert_eq!(delivered.borrow().len(), 1);
    assert_eq!(delivered.borrow()[0].batch.delegation_ids.len(), 2);
    assert!(TaskStore::load(fixture.source()).unwrap().delegations().iter().all(|d| d.is_delivered()));
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_delegate_final_stop_grace_and_reused_identity_refusal(cx: &mut TestAppContext) {
    if child("live_delegate_final_stop_grace_and_reused_identity_refusal") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (_chats, caller) = open_live_caller(&fixture, &git, cx);
    let (owner, sender) = start(&fixture, cx);
    owner.read_with(cx, |owner, _| owner.claude_mcp_servers(&caller));
    fake.prepare_recorder(&owner, cx);
    for (n, refused) in [(1, false), (2, true)] {
        let result = dispatch_as(cx, &sender, TaskControlOperation::DelegateTask(req(json!({
            "title": format!("Placeholder work {n}"), "objective": "Placeholder objective", "repo": git.repo,
            "preset_name": "Codex", "issue_key": "TRU-274", "time_limit_minutes": 5
        }))), Some(&caller)).unwrap();
        assert_eq!(result["queued"], false, "{result}");
        let session = result["task_session_id"].as_str().unwrap();
        let id = result["task_id"].as_str().unwrap();
        wait_until(cx, |_| fake.file("ready", n).exists());
        wait_until(cx, |cx| {
            owner.update(cx, |owner, cx| {
                owner.live_worker_tick(cx);
                owner.test_worker_identity(session, None)
            })
        });
        if refused {
            assert!(owner.update(cx, |owner, _| {
                owner.test_worker_identity(session, Some("reused-pid-birth"))
            }));
        }
        dispatch_as(
            cx,
            &sender,
            TaskControlOperation::UpdateHandoff(req(json!({
                "task_id": id, "status": "done", "summary": "Placeholder result"
            }))),
            Some(session),
        )
        .unwrap();
        owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
        assert!(!fake.file("term", n).exists(), "open parent must receive its result before final close");
        let began = Instant::now();
        owner.update(cx, |owner, cx| {
            owner.submit_hook = Some(Box::new(|_| Some(true)));
            owner.deliver_results(cx);
            owner.live_worker_tick(cx);
        });
        assert!(owner.read_with(cx, |owner, _| {
            owner.host_terminals().iter().any(|id| id == session)
        }));
        if refused {
            wait_until(cx, |cx| {
                owner.read_with(cx, |owner, _| owner.test_worker_stop_refused(session))
            });
            assert!(!fake.file("term", n).exists());
            assert!(owner.read_with(cx, |owner, _| {
                owner.host_terminals().iter().any(|id| id == session)
            }));
            owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
            assert!(owner.read_with(cx, |owner, _| owner.test_worker_stop_refused(session)));
        } else {
            wait_until(cx, |_| fake.file("term", n).exists());
            wait_until(cx, |cx| {
                owner.read_with(cx, |owner, _| {
                    !owner.host_terminals().iter().any(|id| id == session)
                })
            });
            assert!(began.elapsed() >= Duration::from_secs(2));
        }
    }
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_attach_failure_stops_unpublished_fake(cx: &mut TestAppContext) {
    if child("live_attach_failure_stops_unpublished_fake") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    fake.ignore_hup();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let created = create(cx, &sender, &git.repo);
    let other = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
        "title": "Other placeholder", "objective": "Placeholder objective", "repository_path": git.repo, "issue_key": "TRU-274"
    })))).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let id = created["task_id"].as_str().unwrap();
    owner.update(cx, |owner, _| {
        let mut worker = worker_delegation("placeholder-worker", id, "gone");
        if let gitterm_core::tasks::DelegationChild::TaskSession { model, .. } = &mut worker.child {
            *model = None;
        }
        owner.own_worker(worker);
    });
    let trace = fake.trace.path().to_path_buf();
    owner.update(cx, |owner, _| {
        owner.before_publish = Some(Box::new(move |owner| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !trace.join("ready-1").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            let path = owner.host().store().as_ref().unwrap().path().to_path_buf();
            let mut document: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            document["delegations"][0]["child"]["task_id"] = json!(other);
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            owner
                .host_mut()
                .replace_store(Some(TaskStore::load(&path).unwrap()));
        }))
    });
    let began = Instant::now();
    let failed = launch_live(cx, &sender, id, "Codex").unwrap_err();
    assert!(failed.contains("could not attach"), "{failed}");
    assert!(owner.read_with(cx, |owner, _| owner.host_terminals().is_empty()));
    let spec = fake.spec.borrow();
    let url = &spec.as_ref().unwrap().env["GITTERM_V6_TASK_MCP_URL"];
    let caller = url.split("caller=").nth(1).unwrap();
    assert!(owner.read_with(cx, |owner, _| {
        owner.host().require_issued_caller(Some(caller), "consult_request").is_err()
    }));
    assert!(
        TaskStore::load(fixture.source())
            .unwrap()
            .get(id)
            .unwrap()
            .sessions
            .is_empty()
    );
    let pid: u32 = fake.text("pid", 1).trim().parse().unwrap();
    wait_until(cx, |_| {
        gitterm_core::worker_process::macos::info(pid)
            .unwrap()
            .is_none()
    });
    assert!(
        fake.file("term", 1).exists(),
        "worker must receive TERM despite ignoring HUP"
    );
    assert!(
        began.elapsed() >= Duration::from_secs(2),
        "KILL must wait for grace"
    );
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_worker_deadline_uses_iced_time_limit_result_and_stop(cx: &mut TestAppContext) {
    if child("live_worker_deadline_uses_iced_time_limit_result_and_stop") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (_chats, caller) = open_live_caller(&fixture, &git, cx);
    let (owner, sender) = start(&fixture, cx);
    owner.read_with(cx, |owner, _| owner.claude_mcp_servers(&caller));
    fake.prepare_recorder(&owner, cx);
    let result = dispatch_as(
        cx,
        &sender,
        TaskControlOperation::DelegateTask(req(json!({
            "title": "Placeholder deadline", "objective": "Placeholder objective", "repo": git.repo,
            "preset_name": "Codex", "issue_key": "TRU-274", "time_limit_minutes": 5
        }))),
        Some(&caller),
    )
    .unwrap();
    let session = result["task_session_id"].as_str().unwrap();
    let id = result["delegation_id"].as_str().unwrap();
    wait_until(cx, |_| fake.file("ready", 1).exists());
    wait_until(cx, |cx| {
        owner.update(cx, |owner, cx| {
            owner.live_worker_tick(cx);
            owner.test_worker_identity(session, None)
        })
    });
    owner.update(cx, |owner, cx| {
        let path = owner.host().store().as_ref().unwrap().path().to_path_buf();
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let record = document["delegations"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|record| record["delegation_id"] == id)
            .unwrap();
        record["time_limit_deadline_unix_ms"] = json!(1);
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        owner
            .host_mut()
            .replace_store(Some(TaskStore::load(&path).unwrap()));
        owner.live_worker_tick(cx);
    });
    let saved = TaskStore::load(fixture.source()).unwrap();
    let record = saved.delegation(id).unwrap();
    assert!(record.time_limit_reached_at.is_some());
    assert!(record.status.is_final());
    assert!(!record.is_delivered());
    owner.update(cx, |owner, cx| {
        owner.submit_hook = Some(Box::new(|_| Some(true)));
        owner.deliver_results(cx);
        owner.live_worker_tick(cx);
    });
    wait_until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            !owner.host_terminals().iter().any(|id| id == session)
        })
    });
    assert!(fake.file("term", 1).exists());
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_worker_terminal_can_request_consult_and_delegate(cx: &mut TestAppContext) {
    if child("live_worker_terminal_can_request_consult_and_delegate") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (_chats, caller) = open_live_caller(&fixture, &git, cx);
    let (owner, sender) = start(&fixture, cx);
    owner.read_with(cx, |owner, _| owner.claude_mcp_servers(&caller));
    fake.prepare_recorder(&owner, cx);
    let first = dispatch_as(
        cx,
        &sender,
        TaskControlOperation::DelegateTask(req(json!({
            "title": "Placeholder parent", "objective": "Placeholder objective", "repo": git.repo,
            "preset_name": "Codex", "issue_key": "TRU-274"
        }))),
        Some(&caller),
    )
    .unwrap();
    let worker = first["task_session_id"].as_str().unwrap();
    wait_until(cx, |_| fake.file("ready", 1).exists());
    let consult = dispatch_as(
        cx,
        &sender,
        TaskControlOperation::RequestConsult(req(json!({
            "brief": "Placeholder advice"
        }))),
        Some(worker),
    )
    .unwrap();
    let id = consult["delegation_id"].as_str().unwrap();
    wait_until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .delegation(id)
                .unwrap()
                .status
                .is_final()
        })
    });
    assert_eq!(
        TaskStore::load(fixture.source())
            .unwrap()
            .delegation(id)
            .unwrap()
            .parent
            .session_uid,
        worker
    );
    assert_eq!(
        PathBuf::from(fake.text("pwd", 2).trim()),
        PathBuf::from(fake.text("pwd", 1).trim())
    );
    let second = dispatch_as(
        cx,
        &sender,
        TaskControlOperation::DelegateTask(req(json!({
            "title": "Placeholder child", "objective": "Placeholder objective", "repo": git.repo,
            "preset_name": "Codex", "issue_key": "TRU-274"
        }))),
        Some(worker),
    )
    .unwrap();
    assert_eq!(second["queued"], false);
    wait_until(cx, |_| fake.file("ready", 3).exists());
    let child_session = second["task_session_id"].as_str().unwrap();
    owner.update(cx, |owner, cx| {
        // Both callers were usable after publication; teardown must revoke
        // only the ended session, even while another worker stays alive.
        assert!(owner.host().require_issued_caller(Some(child_session), "consult_request").is_ok());
        owner.test_terminal_exited(worker, cx);
        assert!(owner.host().require_issued_caller(Some(worker), "consult_request").is_err());
        assert!(owner.host().require_issued_caller(Some(child_session), "consult_request").is_ok());
        owner.session_closed(child_session, cx);
        assert!(owner.host().require_issued_caller(Some(child_session), "consult_request").is_err());
    });
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_worker_notify_releases_slot_but_imported_notify_stays_closed(cx: &mut TestAppContext) {
    if child("live_worker_notify_releases_slot_but_imported_notify_stays_closed") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    let settings = fixture.iced.join("settings.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    value["max_concurrent_local_tasks"] = json!(1);
    std::fs::write(settings, serde_json::to_vec(&value).unwrap()).unwrap();
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let first = create(cx, &sender, &git.repo);
    let id = first["task_id"].as_str().unwrap();
    let session = launch_live(cx, &sender, id, "Codex").unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until(cx, |_| fake.file("ready", 1).exists());
    let next = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
        "title": "Next placeholder", "objective": "Placeholder objective", "repository_path": git.repo, "issue_key": "TRU-274"
    })))).unwrap();
    assert_eq!(
        launch_live(cx, &sender, next["task_id"].as_str().unwrap(), "Codex").unwrap()["queued"],
        true
    );
    let event = |session: &str, event_type: &str| {
        TaskControlOperation::SessionEvent(req(
            json!({"task_id": id, "session_id": session, "event_type": event_type}),
        ))
    };
    assert_eq!(
        dispatch(cx, &sender, event("imported-worker", "agent-turn-complete")),
        Err(gitterm_core::preview::service::preview_notice("notify"))
    );
    assert_eq!(
        dispatch(cx, &sender, event(&session, "other-event")),
        Err(gitterm_core::preview::service::preview_notice("notify"))
    );
    assert_eq!(
        dispatch(cx, &sender, event(&session, "agent-turn-complete")).unwrap()["waiting_for_input"],
        true
    );
    owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
    wait_until(cx, |_| fake.file("ready", 2).exists());
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_observed_limits_hold_later_launches(cx: &mut TestAppContext) {
    if child("live_observed_limits_hold_later_launches") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let now = chrono::Utc::now().timestamp() + 1;
    let sessions = gitterm_core::limits::local_codex_sessions().unwrap();
    assert!(sessions.starts_with(std::env::var_os("HOME").unwrap()));
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("rollout-placeholder.jsonl"), serde_json::to_string(&json!({
        "timestamp": chrono::DateTime::from_timestamp(now, 0).unwrap().to_rfc3339(),
        "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {
            "limit_id": "codex", "primary": {"used_percent": 100, "window_minutes": 300, "resets_at": now + 3600},
            "rate_limit_reached_type": "primary"
        }}
    })).unwrap() + "\n").unwrap();
    let polled_at = Instant::now();
    owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
    wait_until(cx, |cx| {
        owner.update(cx, |owner, _| {
            owner
                .test_plan_limits()
                .file()
                .codex
                .as_ref()
                .is_some_and(|limits| limits.observed_at == now)
        })
    });
    let first = create(cx, &sender, &git.repo);
    assert_eq!(
        launch_live(cx, &sender, first["task_id"].as_str().unwrap(), "Codex").unwrap()["queued"],
        true
    );
    owner.update(cx, |owner, cx| {
        owner.test_claude_limit(
            &gitterm_core::chat::types::RateLimitReading {
                observed_at: now,
                info: json!({
                    "status": "rejected", "rateLimitType": "five_hour", "resetsAt": now + 3600
                }),
            },
            cx,
        )
    });
    let second = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
        "title": "Claude held", "objective": "Placeholder", "repository_path": git.repo, "issue_key": "TRU-274"
    })))).unwrap();
    assert_eq!(
        launch_live(cx, &sender, second["task_id"].as_str().unwrap(), "Claude").unwrap()["queued"],
        true
    );
    assert!(!fake.file("ready", 1).exists());
    owner.update(cx, |owner, _| {
        let state = owner.test_plan_limits();
        assert!(!state.codex_read_due(polled_at + Duration::from_secs(59)));
        assert!(state.codex_read_due(
            polled_at + gitterm_core::limits::CODEX_READ_EVERY + Duration::from_secs(1)
        ));
    });
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_notify_idle_input_reclaims_worker_capacity(cx: &mut TestAppContext) {
    if child("live_notify_idle_input_reclaims_worker_capacity") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    worker_settings(&fixture);
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let created = create(cx, &sender, &git.repo);
    let id = created["task_id"].as_str().unwrap();
    let launched = launch_live(cx, &sender, id, "Codex").unwrap();
    let session = launched["session_id"].as_str().unwrap();
    wait_until(cx, |_| fake.file("ready", 1).exists());
    dispatch(
        cx,
        &sender,
        TaskControlOperation::SessionEvent(req(json!({
            "task_id": id, "session_id": session, "event_type": "agent-turn-complete"
        }))),
    )
    .unwrap();
    owner.update(cx, |owner, cx| {
        assert_eq!(owner.test_worker_counts(cx), (0, 0));
        owner.test_terminal_input(session, cx);
        assert_eq!(owner.test_worker_counts(cx), (1, 1));
    });
    assert_eq!(
        stored_task(&owner, id, cx).lifecycle,
        TaskLifecycle::Running
    );
    owner.update(cx, |owner, cx| owner.quit(cx));
}

#[gpui::test]
fn live_explicit_relaunch_attaches_saved_worker_constraints(cx: &mut TestAppContext) {
    if child("live_explicit_relaunch_attaches_saved_worker_constraints") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    std::fs::write(
        fixture.iced.join("settings.json"),
        serde_json::to_vec(&json!({
            "agent_presets": [{"name": "Codex", "command": "codex"}]
        }))
        .unwrap(),
    )
    .unwrap();
    fresh_limits(&fixture, false);
    let (owner, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &git.repo);
    let id = created["task_id"].as_str().unwrap();
    owner.update(cx, |owner, _| {
        let mut worker = worker_delegation("saved-worker", id, "old-session");
        worker.options =
            req(json!({"effort": "high", "add_dirs": [git.repo], "time_limit_minutes": 5}));
        owner.own_worker(worker);
        owner.shutdown();
    });
    let (owner, sender) = start(&fixture, cx);
    fake.prepare_recorder(&owner, cx);
    let launched = launch_live(cx, &sender, id, "Codex").unwrap();
    let session = launched["session_id"].as_str().unwrap();
    wait_until(cx, |_| fake.file("ready", 1).exists());
    let args = fake.text("args", 1);
    assert!(args.contains("fixture-model"), "{args}");
    assert!(args.contains("high"), "{args}");
    assert!(args.contains(git.repo.to_str().unwrap()), "{args}");
    let saved = TaskStore::load(fixture.source()).unwrap();
    let worker = saved.delegation("saved-worker").unwrap();
    assert!(
        matches!(&worker.child, gitterm_core::tasks::DelegationChild::TaskSession {
        task_session_id: Some(attached), ..
    } if attached == session)
    );
    assert!(saved.worker_time_limit("saved-worker").unwrap().is_some());
    wait_until(cx, |cx| {
        owner.update(cx, |owner, cx| owner.live_worker_tick(cx));
        TaskStore::load(fixture.source())
            .unwrap()
            .get(id)
            .unwrap()
            .sessions
            .iter()
            .any(|record| record.task_session_id == session && record.launch_identity.is_some())
    });
    owner.update(cx, |owner, cx| {
        let path = owner.host().store().as_ref().unwrap().path().to_path_buf();
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["delegations"][0]["time_limit_deadline_unix_ms"] = json!(1);
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        owner
            .host_mut()
            .replace_store(Some(TaskStore::load(&path).unwrap()));
        owner.live_worker_tick(cx);
    });
    assert!(
        TaskStore::load(fixture.source())
            .unwrap()
            .delegation("saved-worker")
            .unwrap()
            .time_limit_reached_at
            .is_some()
    );
    wait_until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            !owner.host_terminals().iter().any(|id| id == session)
        })
    });
    assert!(fake.file("term", 1).exists());
    owner.update(cx, |owner, cx| owner.quit(cx));
}
