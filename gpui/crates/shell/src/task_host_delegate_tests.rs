//! H5b B10c: `delegate_task` in fixture mode prepares a task worktree and
//! launches its worker with fake `codex` and `claude`.
use super::*;
use gitterm_core::preview::service::preview_notice;

const NOTICE: &str = "delegate_task";

impl Launches {
    fn delegate(
        &self,
        request: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> Result<serde_json::Value, String> {
        let mut body = serde_json::json!({
            "title": "Fixture worker", "objective": "Change the fixture", "issue_key": "tru-1"
        });
        body.as_object_mut()
            .unwrap()
            .extend(request.as_object().unwrap().clone());
        dispatch_as(
            cx,
            &self.runs.sender,
            TaskControlOperation::DelegateTask(req(body)),
            Some(&self.runs.caller),
        )
    }
    fn tasks(&self, cx: &mut TestAppContext) -> Vec<TaskRecord> {
        self.runs.owner.read_with(cx, |owner, _| {
            owner.host().store().as_ref().unwrap().tasks().to_vec()
        })
    }
}

/// A started worker: its task's worktree in the fixture, its session
/// launched there and attached to the stored delegation.
fn assert_worker_started(
    launches: &Launches,
    reply: &serde_json::Value,
    run: usize,
    cx: &mut TestAppContext,
) {
    let task_id = reply["task_id"].as_str().unwrap();
    let delegation_id = reply["delegation_id"].as_str().unwrap();
    let session = reply["task_session_id"].as_str().unwrap();
    assert_eq!(reply["queued"], false, "{reply}");
    let worktree = launches.worktree(task_id, cx);
    let fixture = launches.runs.git.root.canonicalize().unwrap();
    assert!(
        worktree.canonicalize().unwrap().starts_with(&fixture),
        "{}",
        worktree.display()
    );
    wait_until(cx, |_| launches.agent.started(run));
    assert_eq!(
        PathBuf::from(launches.agent.read("pwd", run).unwrap().trim_end()),
        worktree.canonicalize().unwrap()
    );
    assert!(
        launches
            .host_terminals(cx)
            .iter()
            .any(|held| held == session)
    );
    let saved = launches.saved();
    let delegation = saved.delegation(delegation_id).unwrap();
    match &delegation.child {
        gitterm_core::tasks::DelegationChild::TaskSession {
            task_id: child,
            task_session_id,
            ..
        } => {
            assert_eq!(child, task_id);
            assert_eq!(task_session_id.as_deref(), Some(session));
        }
        other => panic!("not a worker: {other:?}"),
    }
    assert!(
        saved
            .get(task_id)
            .unwrap()
            .sessions
            .iter()
            .any(|item| item.task_session_id == session)
    );
}

#[gpui::test]
fn delegate_task_prepares_and_starts_worker_in_fixture(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    // Codex from the calling tab's repository.
    let codex = launches
        .delegate(serde_json::json!({"preset_name": "Codex"}), cx)
        .unwrap();
    assert_eq!(codex["provider"], "codex", "{codex}");
    assert_worker_started(&launches, &codex, 1, cx);
    launches.kill(1);

    // Claude from a named repository, trusted in the pinned config.
    let repo = launches.runs.git.repo.clone();
    pin_claude_trust(&launches, &repo, true);
    let claude = launches
        .delegate(
            serde_json::json!({
                "title": "Second fixture worker", "preset_name": "Claude",
                "repo": repo, "add_dirs": [repo],
            }),
            cx,
        )
        .unwrap();
    assert_eq!(claude["provider"], "claude", "{claude}");
    assert_worker_started(&launches, &claude, 2, cx);
    let args = launches.agent.args(2);
    launches.kill(2);
    assert!(args.iter().any(|arg| arg == "--add-dir"), "{args:?}");
    assert_ne!(codex["task_id"], claude["task_id"]);
}

#[gpui::test]
fn fixture_add_dir_outside_refused(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let git = &launches.runs.git;
    // Outside the fixture, named directly or through a link inside it.
    let link = git.root.join("outside-link");
    std::os::unix::fs::symlink(&git.outside, &link).unwrap();
    for dir in [git.outside.clone(), link] {
        assert_eq!(
            launches.delegate(serde_json::json!({"add_dirs": [dir]}), cx),
            Err(preview_notice(NOTICE))
        );
    }
    assert!(launches.tasks(cx).is_empty());
    assert!(launches.saved().delegations().is_empty());
    assert_eq!(launches.agent.runs(), 0);
}

#[gpui::test]
fn delegate_path_swapped_after_prep_refused(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let base = launches.runs.fixture.iced.parent().unwrap().to_path_buf();
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let elsewhere_before = tree(&elsewhere);
    let moved = launches.runs.git.root.join("moved");
    launches.runs.owner.update(cx, |owner, _| {
        let (elsewhere, moved) = (elsewhere.clone(), moved.clone());
        owner.before_recheck = Some(Box::new(move |path: &Path| {
            std::fs::rename(path, &moved).unwrap();
            std::os::unix::fs::symlink(&elsewhere, path).unwrap();
        }));
    });
    assert_eq!(
        launches.delegate(serde_json::json!({"preset_name": "Codex"}), cx),
        Err(preview_notice(NOTICE))
    );
    let tasks = launches.tasks(cx);
    assert_eq!(tasks.len(), 1);
    assert_ne!(tasks[0].lifecycle, TaskLifecycle::Ready, "{:?}", tasks[0]);
    assert_eq!(tasks[0].worktree.path, None, "{:?}", tasks[0]);
    assert!(launches.saved().delegations().is_empty());
    assert_eq!(launches.agent.runs(), 0);
    assert!(launches.host_terminals(cx).is_empty());
    assert_eq!(tree(&elsewhere), elsewhere_before);
}

/// An open worker delegation imported from Iced, awaiting its launch, is
/// never started or attached; this host's own worker is.
#[gpui::test]
fn imported_delegation_never_started(cx: &mut TestAppContext) {
    let launches = Launches::start_with(
        |runs0| {
            let mut imported = worker_delegation("imported-worker", "imported", "unused");
            if let gitterm_core::tasks::DelegationChild::TaskSession {
                task_session_id, ..
            } = &mut imported.child
            {
                *task_session_id = None;
            }
            imported.status = gitterm_core::tasks::DelegationStatus::Requested;
            runs0.fixture.write_iced(
                vec![task_at(
                    "imported",
                    TaskLifecycle::Ready,
                    &runs0.git.worktree,
                )],
                vec![imported],
            );
        },
        cx,
    );
    let before = launches.saved();
    let imported = before.delegation("imported-worker").unwrap().clone();
    assert_eq!(
        launches.launch("imported", Some("Codex"), cx),
        Err(preview_notice("task_launch_session"))
    );
    let reply = launches
        .delegate(serde_json::json!({"preset_name": "Codex"}), cx)
        .unwrap();
    assert_worker_started(&launches, &reply, 1, cx);
    launches.kill(1);
    settle(cx);
    let saved = launches.saved();
    assert_eq!(saved.delegation("imported-worker"), Some(&imported));
    assert!(saved.get("imported").unwrap().sessions.is_empty());
    assert_eq!(launches.agent.runs(), 1);
}

/// Plain preview answers `delegate_task` with the notice and creates,
/// stores and starts nothing.
#[gpui::test]
fn preview_delegate_task_answers_notice(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git = GitFixture::new(&fixture);
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open_in(&fixture, Some("root-chat"), &git.repo, cx);
    let (owner, sender) = fixture.start(cx);
    let request = TaskControlOperation::DelegateTask(req(serde_json::json!({
        "title": "t", "objective": "o", "repo": git.repo,
    })));
    assert_eq!(
        dispatch_as(cx, &sender, request, Some(&caller)),
        Err(preview_notice(NOTICE))
    );
    owner.read_with(cx, |owner, _| {
        let store = owner.host().store().as_ref().unwrap();
        assert!(store.tasks().is_empty());
        assert!(store.delegations().is_empty());
    });
}

/// Iced's `delegate_task_in_checkout`, reuse: a checkout that is a task's
/// worktree gives that task the new work, and the worker starts there.
/// While its worker delegation is open the checkout takes no more work.
#[gpui::test]
fn delegate_into_task_checkout_reuses_the_task(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let reply = launches
        .delegate(
            serde_json::json!({
                "title": "Reused worker", "objective": "More work here",
                "preset_name": "Codex", "checkout": worktree,
            }),
            cx,
        )
        .unwrap();
    assert_eq!(reply["task_id"], task.as_str(), "{reply}");
    assert_worker_started(&launches, &reply, 1, cx);
    let saved = launches.saved();
    let record = saved.get(&task).unwrap();
    assert_eq!(record.title, "Reused worker");
    assert_eq!(record.objective, "More work here");
    assert_eq!(launches.tasks(cx).len(), 1);

    let error = launches
        .delegate(
            serde_json::json!({"preset_name": "Codex", "checkout": worktree}),
            cx,
        )
        .unwrap_err();
    assert!(error.contains("is still open there"), "{error}");
    launches.kill(1);
    assert_eq!(launches.agent.runs(), 1);
    assert_eq!(launches.saved().delegations().len(), 1);
}

/// Iced's `worker_reported` (main.rs:19890): a worker that reports
/// `blocked` from its still-open terminal waits for input, so it no longer
/// counts as working and its checkout can be delegated into again.
#[gpui::test]
fn blocked_worker_checkout_can_be_delegated_again(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let worktree = launches.worktree(&task, cx);
    let first = launches
        .delegate(
            serde_json::json!({"preset_name": "Codex", "checkout": worktree}),
            cx,
        )
        .unwrap();
    assert_worker_started(&launches, &first, 1, cx);
    let session = first["task_session_id"].as_str().unwrap().to_string();
    let blocked = dispatch_as(
        cx,
        &launches.runs.sender,
        TaskControlOperation::UpdateHandoff(req(serde_json::json!({
            "task_id": task, "summary": "Stuck", "status": "blocked",
            "blockers": ["needs a decision"]
        }))),
        Some(&session),
    )
    .unwrap();
    assert_eq!(blocked["delegation_status"], "blocked", "{blocked}");
    let again = launches.delegate(
        serde_json::json!({
            "objective": "Carry on", "preset_name": "Codex", "checkout": worktree
        }),
        cx,
    );
    launches.kill(1);
    let again = again.unwrap();
    assert_eq!(again["task_id"], task.as_str(), "{again}");
    wait_until(cx, |_| launches.agent.started(2));
    launches.kill(2);
}

/// Iced's `delegate_task_in_checkout`, adoption: a registered worktree no
/// task owns gets a new task set up around it, and the worker starts
/// there. A checkout outside the fixture is refused before anything is
/// written.
#[gpui::test]
fn delegate_into_unowned_worktree_adopts_it(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let git = &launches.runs.git;
    assert_eq!(
        launches.delegate(
            serde_json::json!({"preset_name": "Codex", "checkout": git.outside}),
            cx
        ),
        Err(preview_notice(NOTICE))
    );
    assert!(launches.tasks(cx).is_empty());

    let adopt = git.root.join("adopt");
    super::git(
        &git.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "adopted",
            adopt.to_str().unwrap(),
        ],
    );
    let reply = launches
        .delegate(
            serde_json::json!({"preset_name": "Codex", "checkout": adopt}),
            cx,
        )
        .unwrap();
    assert_worker_started(&launches, &reply, 1, cx);
    launches.kill(1);
    let task_id = reply["task_id"].as_str().unwrap();
    let saved = launches.saved();
    let record = saved.get(task_id).unwrap();
    assert_eq!(record.worktree.path, Some(adopt.canonicalize().unwrap()));
    assert_eq!(
        record.worktree.state,
        gitterm_core::tasks::TaskWorktreeState::Ready
    );
    assert_eq!(record.branch, "adopted");
    assert_eq!(launches.tasks(cx).len(), 1);
}

/// Iced folds a named `repo` with `workspace_repos::main_checkout`, which
/// keeps a worktree of a bare repository as it is (the strict trust fold
/// refuses it). The task is made from that worktree; the worker's own
/// checkout check then refuses it, as Iced's does.
#[gpui::test]
fn delegate_repo_folds_as_iced_main_checkout(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let git = &launches.runs.git;
    let bare = git.root.join("bare.git");
    let linked = git.root.join("bare-wt");
    super::git(
        &git.root,
        &[
            "clone",
            "-q",
            "--bare",
            git.repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    super::git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let linked = linked.canonicalize().unwrap();
    cx.update(|cx| {
        cx.global_mut::<crate::work_sidebar::IcedWorkSource>()
            .workspaces[0]
            .repos
            .push(linked.display().to_string())
    });
    let error = launches
        .delegate(
            serde_json::json!({"preset_name": "Codex", "repo": linked}),
            cx,
        )
        .unwrap_err();
    assert!(!error.contains("for trust"), "{error}");
    let tasks = launches.tasks(cx);
    assert_eq!(tasks.len(), 1, "{error}");
    assert_eq!(
        tasks[0].workspace.location,
        gitterm_core::tasks::WorkspaceLocationIdentity::Local { directory: linked }
    );
    assert_eq!(launches.agent.runs(), 0);
}
