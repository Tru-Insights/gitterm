//! Actual Iced dispatch, with an in-memory ingress and disposable persistence.
use super::*;
use gitterm_core::task_commands::runtime::{command_channel, PendingReply};
use std::pin::Pin;

fn control(
    app: &mut App,
    operation: TaskControlOperation,
    caller: Option<&str>,
) -> Result<Value, String> {
    let (sender, mut receiver) = command_channel();
    let pending = sender
        .enqueue(operation, caller.map(str::to_string))
        .unwrap();
    let _effect = app.update_event(Event::TaskControlRequested(receiver.try_recv().unwrap()));
    iced::futures::executor::block_on(pending)
}

fn read(task_id: &str) -> TaskControlOperation {
    TaskControlOperation::Get(task_mcp::GetTaskRequest {
        task_id: task_id.into(),
    })
}

#[test]
fn iced_read_results_preserve_store_order_live_facts_errors_and_schema() {
    if isolated(
        "iced_read_results_preserve_store_order_live_facts_errors_and_schema",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    assert_eq!(
        control(&mut app, read("missing"), None).unwrap_err(),
        "Task missing is not available"
    );
    assert_eq!(
        control(
            &mut app,
            TaskControlOperation::List(task_mcp::ListTasksRequest {
                include_archived: false
            }),
            None
        )
        .unwrap_err(),
        "GitTerm's task store is unavailable"
    );
    app.task_authority = Some(TaskStore::load(dir.path().join("tasks.json")).unwrap()).into();
    assert_eq!(
        control(
            &mut app,
            TaskControlOperation::List(task_mcp::ListTasksRequest {
                include_archived: false
            }),
            None
        )
        .unwrap()["tasks"],
        json!([])
    );
    add_task(&mut app, dir.path(), dir.path());
    let mut archived = app
        .task_authority
        .store()
        .as_ref()
        .unwrap()
        .get("fixture-task")
        .unwrap()
        .clone();
    archived.task_id = "archived".into();
    archived.archived_at = Some("t3".into());
    archived.lifecycle = TaskLifecycle::Archived;
    app.task_authority
        .store_mut()
        .as_mut()
        .unwrap()
        .insert(archived)
        .unwrap();
    for (ws, label, session) in [(0, "front", "first"), (1, "background", "second")] {
        let tab = &mut app.workspaces[ws].tabs[0];
        tab.task_id = Some("fixture-task".into());
        tab.task_session_id = Some(session.into());
        tab.status.repo_name = label.into();
    }
    let mut expected = serde_json::to_value(
        app.task_authority
            .store()
            .as_ref()
            .unwrap()
            .get("fixture-task")
            .unwrap(),
    )
    .unwrap();
    expected["open_sessions"] = json!([
        {"session_id":"first","label":"front","workspace":"alpha","is_active":true},
        {"session_id":"second","label":"background","workspace":"beta","is_active":false}
    ]);
    let disk = std::fs::read(dir.path().join("tasks.json")).unwrap();
    assert_eq!(
        control(&mut app, read("fixture-task"), None).unwrap(),
        expected
    );
    let listed = control(
        &mut app,
        TaskControlOperation::List(task_mcp::ListTasksRequest {
            include_archived: false,
        }),
        Some("unrecognized-caller"),
    )
    .unwrap();
    assert_eq!(listed["tasks"], json!([expected]));
    assert_eq!(
        listed["available_presets"],
        json!(app
            .agent_presets
            .iter()
            .map(|p| &p.name)
            .collect::<Vec<_>>())
    );
    let listed = control(
        &mut app,
        TaskControlOperation::List(task_mcp::ListTasksRequest {
            include_archived: true,
        }),
        None,
    )
    .unwrap();
    assert_eq!(listed["tasks"][1]["task_id"], "archived");
    assert!(control(&mut app, read("archived"), None).is_ok());
    assert_eq!(std::fs::read(dir.path().join("tasks.json")).unwrap(), disk);
}

#[test]
fn iced_mutating_errors_notify_and_abandoned_waiter_keep_existing_behavior() {
    if isolated(
        "iced_mutating_errors_notify_and_abandoned_waiter_keep_existing_behavior",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let create = task_mcp::CreateTaskRequest {
        title: " ".into(),
        objective: "fixture".into(),
        repository_path: dir.path().into(),
        workspace_name: None,
        issue_key: None,
        base_reference: None,
        stopping_boundary: None,
    };
    assert_eq!(
        control(&mut app, TaskControlOperation::Create(create), None).unwrap_err(),
        "title is required"
    );
    assert_eq!(
        control(
            &mut app,
            TaskControlOperation::LaunchSession(task_mcp::LaunchTaskSessionRequest {
                task_id: "absent".into(),
                preset_name: None,
                user_model_choice: false,
            }),
            None
        )
        .unwrap_err(),
        "Task absent is not available"
    );
    let notify = task_mcp::TaskSessionEventRequest {
        task_id: "fixture-task".into(),
        session_id: "first".into(),
        event_type: "other".into(),
    };
    assert_eq!(
        control(&mut app, TaskControlOperation::SessionEvent(notify), None).unwrap(),
        json!({"ignored":"other"})
    );
    let (sender, mut receiver) = command_channel();
    let request: task_mcp::UpdateTaskHandoffRequest =
        serde_json::from_value(json!({"task_id":"fixture-task","summary":"Late accepted write"}))
            .unwrap();
    let pending = sender
        .enqueue(TaskControlOperation::UpdateHandoff(request), None)
        .unwrap();
    drop(pending);
    let _effect = app.update_event(Event::TaskControlRequested(receiver.try_recv().unwrap()));
    assert_eq!(
        TaskStore::load(dir.path().join("tasks.json"))
            .unwrap()
            .get("fixture-task")
            .unwrap()
            .handoff
            .as_ref()
            .unwrap()
            .summary,
        "Late accepted write"
    );
}

fn assert_pending(pending: &mut Pin<Box<PendingReply>>) {
    use iced::futures::FutureExt;
    assert!(pending.as_mut().now_or_never().is_none());
}

fn fixture_worker_source(path: &Path) -> Repository {
    let repo = Repository::init(path).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    {
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "Fixture", &tree, &[])
            .unwrap();
    }
    repo
}

fn fixture_worker_checkout(source: &Path, path: &Path, name: &str) {
    let repo = Repository::open(source).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    let reference = repo.branch(name, &commit, false).unwrap().into_reference();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&reference));
    repo.worktree(name, path, Some(&options)).unwrap();
}

fn pending_worker_fixture(
    app: &mut App,
    dir: &Path,
) -> (
    PendingTaskLaunch,
    PreparedTaskWorktree,
    PendingReply,
    gitterm_core::task_commands::runtime::CommandReceiver<()>,
) {
    pending_worker_fixture_at(app, dir, &dir.join("prepared"))
}

fn pending_worker_fixture_at(
    app: &mut App,
    dir: &Path,
    path: &Path,
) -> (
    PendingTaskLaunch,
    PreparedTaskWorktree,
    PendingReply,
    gitterm_core::task_commands::runtime::CommandReceiver<()>,
) {
    let source = dir.join("source");
    std::fs::create_dir_all(&source).unwrap();
    fixture_worker_source(&source);
    let source = std::fs::canonicalize(source).unwrap();
    app.workspaces[0].dir = source.clone();
    app.workspaces[0].location = WorkspaceLocation::Local {
        root: source.clone(),
    };
    app.workspaces[0].tabs[0].repo_path = source.clone();
    let uid = app.workspaces[0].tabs[0].session_uid.clone();
    let caller = app.delegation_caller(&uid).unwrap();
    let TaskMutationAction::WorkerAuthorityChecked { id } = app
        .reduce_task_mutation(TaskMutation::BeginWorkerPreparation {
            caller: uid.clone(),
        })
        .unwrap()
    else {
        panic!("capture")
    };
    let repository = gitterm::task_worktree::ResolvedRepository {
        top_level: source.clone(),
        identity: app
            .worker_authority_caller(&uid)
            .unwrap()
            .repository
            .unwrap(),
        current_branch: Some("main".into()),
    };
    let (sender, mut receiver) = command_channel();
    let response = sender.enqueue((), None).unwrap();
    let request = serde_json::from_value(
        json!({"title":"Prepared worker", "objective":"fixture", "repository_path":source}),
    )
    .unwrap();
    let mut pending = app
        .pending_task_from_control(request, receiver.try_recv().unwrap().reply, Some(uid))
        .unwrap();
    pending.worker = Some(PendingWorker {
        authority: id,
        caller,
        choice: gitterm::workers::WorkerChoice {
            preset_index: usize::MAX,
            preset_name: "Unavailable".into(),
            provider: gitterm::workers::WorkerProvider::Codex,
            role: gitterm::workers::WorkerRole::Judgment,
            model: None,
            model_source: gitterm::workers::ModelSource::CliDefault,
        },
        options: Default::default(),
    });
    let base = app
        .task_authority
        .store()
        .as_ref()
        .unwrap()
        .get("fixture-task")
        .unwrap()
        .base
        .clone();
    let path = path.to_path_buf();
    fixture_worker_checkout(&source, &path, "prepared");
    let prepared = PreparedTaskWorktree {
        repository: repository.clone(),
        base: base.clone(),
        branch: "prepared".into(),
        path: std::fs::canonicalize(&path).unwrap(),
    };
    let _effect = app.update_event(Event::TaskMetadataResolved(
        pending.clone(),
        Ok(ResolvedTaskPreparation {
            repository,
            base,
            proposal: gitterm::task_worktree::TaskWorktreeProposal {
                branch: "prepared".into(),
                path: path.clone(),
            },
        }),
    ));
    pending.resolved_worktree_path = Some(path);
    (pending, prepared, response, receiver)
}

#[test]
fn iced_stale_worker_completion_refuses_writes_and_launch_end_to_end() {
    if isolated(
        "iced_stale_worker_completion_refuses_writes_and_launch_end_to_end",
        "task_authority",
    ) {
        return;
    }
    for stale in [
        "caller",
        "incarnation",
        "session",
        "chat",
        "checkout",
        "workspace",
        "membership",
        "repository",
        "target",
    ] {
        for success in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut app = app_with_tabs(dir.path());
            add_task(&mut app, dir.path(), dir.path());
            let (pending, prepared, response, _receiver) =
                pending_worker_fixture(&mut app, dir.path());
            match stale {
                "caller" => app.workspaces[0].tabs.clear(),
                "incarnation" => app.workspaces[0].tabs[0].id += 1000,
                "session" => {
                    app.workspaces[0].tabs[0].task_session_id = Some("replacement-session".into())
                }
                "chat" => {
                    app.workspaces[0].tabs[0].chat_session_id = Some("replacement-chat".into())
                }
                "checkout" => {
                    app.workspaces[0].tabs[0].repo_path = dir.path().join("changed-checkout")
                }
                "workspace" => app.workspaces[0].name = "replacement-workspace".into(),
                "membership" => app.workspaces[0].pending_repos.push(dir.path().into()),
                "repository" => {
                    Repository::open(&app.workspaces[0].tabs[0].repo_path)
                        .unwrap()
                        .config()
                        .unwrap()
                        .set_str(
                            "remote.origin.url",
                            "https://example.invalid/replacement.git",
                        )
                        .unwrap();
                }
                "target" => {
                    let store = app.task_authority.store_mut().as_mut().unwrap();
                    let mut task = store.get(&pending.task_id).unwrap().clone();
                    task.branch = "replaced".into();
                    store.replace(task).unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
            let tabs = app.workspaces.iter().map(|ws| ws.tabs.len()).sum::<usize>();
            let result = if success {
                Ok(prepared)
            } else {
                Err("prepare failed; rollback requires attention".into())
            };
            let _effect = app.update_event(Event::TaskWorktreePrepared(pending.clone(), result));
            let error = iced::futures::executor::block_on(response).unwrap_err();
            assert!(error.contains("changed"), "{stale}/{success}: {error}");
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before,
                "{stale}/{success}"
            );
            assert!(app
                .task_authority
                .store()
                .as_ref()
                .unwrap()
                .delegations()
                .is_empty());
            assert!(app.task_launch_queue.is_empty());
            assert_eq!(
                app.workspaces.iter().map(|ws| ws.tabs.len()).sum::<usize>(),
                tabs
            );
            assert!(pending.resolved_worktree_path.unwrap().exists());
        }
    }
}

#[test]
fn iced_worker_completion_observes_actual_save_and_consumes_duplicates() {
    if isolated(
        "iced_worker_completion_observes_actual_save_and_consumes_duplicates",
        "task_authority",
    ) {
        return;
    }
    for outcome in ["saved", "save-error", "prepare-error", "rollback-file"] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let (pending, prepared, response, _receiver) = pending_worker_fixture(&mut app, dir.path());
        if outcome == "save-error" {
            std::fs::remove_file(dir.path().join("tasks.json")).unwrap();
            std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
        }
        if outcome == "rollback-file" {
            std::fs::remove_dir_all(&prepared.path).unwrap();
            std::fs::write(&prepared.path, "leftover").unwrap();
        }
        let result = match outcome {
            "prepare-error" => Err("ordinary preparation failure".into()),
            "rollback-file" => Err("prepare failed; rollback requires attention".into()),
            _ => Ok(prepared.clone()),
        };
        let _effect = app.update_event(Event::TaskWorktreePrepared(pending.clone(), result));
        let error = iced::futures::executor::block_on(response).unwrap_err();
        let store = app.task_authority.store().as_ref().unwrap();
        let task = store.get(&pending.task_id).unwrap();
        match outcome {
            "saved" => {
                assert!(
                    error.contains("selected agent preset is unavailable"),
                    "{error}"
                );
                assert_eq!(task.lifecycle, TaskLifecycle::Ready);
                assert_eq!(store.delegations().len(), 1);
            }
            "save-error" => {
                assert!(error.contains("atomically write"), "{error}");
                assert_eq!(task.lifecycle, TaskLifecycle::Preparing);
                assert!(store.delegations().is_empty());
            }
            "prepare-error" => {
                assert_eq!(error, "ordinary preparation failure");
                assert_eq!(task.lifecycle, TaskLifecycle::Failed);
                assert!(store.delegations().is_empty());
            }
            "rollback-file" => {
                assert!(error.contains("rollback requires attention"));
                assert_eq!(task.worktree.state, TaskWorktreeState::CleanupRequired);
                assert!(store.delegations().is_empty());
            }
            _ => unreachable!(),
        }
        if outcome != "save-error" {
            let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
            let _effect = app.update_event(Event::TaskWorktreePrepared(pending, Ok(prepared)));
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before
            );
        }
    }
}

#[test]
fn iced_owner_shutdown_releases_pending_replies() {
    if isolated(
        "iced_owner_shutdown_releases_pending_replies",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let sender = init_task_control_channel();
    let mut receiver = task_mcp::take_command_receiver().unwrap();
    let pending = sender.enqueue(read("fixture-task"), None).unwrap();
    let reply = receiver.try_recv().unwrap().reply;
    let _effect = app.update_event(Event::WindowCloseRequested);
    assert!(iced::futures::executor::block_on(pending).is_err());
    assert!(sender.enqueue(read("fixture-task"), None).is_err());
    reply.send(Ok(json!("late")));
}

#[test]
fn typed_handoff_session_does_not_grant_worker_completion_authority() {
    if isolated(
        "typed_handoff_session_does_not_grant_worker_completion_authority",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let session: TaskSessionRecord = serde_json::from_str(&fixture("tasks/session.json")).unwrap();
    let delegation: gitterm::tasks::Delegation =
        serde_json::from_str(&fixture("tasks/delegation.json")).unwrap();
    let store = app.task_authority.store_mut().as_mut().unwrap();
    store.upsert_session("fixture-task", session, "t1").unwrap();
    store.insert_delegation(delegation).unwrap();
    let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
    let coordinator = app.workspaces[0].tabs[0].session_uid.clone();
    for status in ["done", "blocked"] {
        let request = serde_json::from_value(json!({"task_id":"fixture-task", "summary":"Typed identity", "session_id":"fixture-session", "status":status})).unwrap();
        assert!(control(
            &mut app,
            TaskControlOperation::UpdateHandoff(request),
            Some(&coordinator)
        )
        .unwrap_err()
        .contains("only the worker's own GitTerm tab"));
        assert_eq!(
            std::fs::read(dir.path().join("tasks.json")).unwrap(),
            before
        );
    }
}

#[test]
fn iced_create_is_deferred_and_only_acknowledges_saved_preparation() {
    if isolated(
        "iced_create_is_deferred_and_only_acknowledges_saved_preparation",
        "task_authority",
    ) {
        return;
    }
    for fail_save in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let (sender, mut receiver) = command_channel();
        let request: task_mcp::CreateTaskRequest = serde_json::from_value(
            json!({"title":"Created","objective":"fixture","repository_path":dir.path()}),
        )
        .unwrap();
        let mut response = Box::pin(
            sender
                .enqueue(
                    TaskControlOperation::Create(request.clone()),
                    Some("original-caller".into()),
                )
                .unwrap(),
        );
        let envelope = receiver.try_recv().unwrap();
        let mut pending = app
            .pending_task_from_control(request, envelope.reply, envelope.caller)
            .unwrap();
        assert_pending(&mut response);
        let record = app
            .task_authority
            .store()
            .as_ref()
            .unwrap()
            .get("fixture-task")
            .unwrap()
            .clone();
        let repository = gitterm::task_worktree::ResolvedRepository {
            top_level: dir.path().into(),
            identity: record.repository.clone(),
            current_branch: Some("main".into()),
        };
        let resolved = ResolvedTaskPreparation {
            repository: repository.clone(),
            base: record.base.clone(),
            proposal: gitterm::task_worktree::TaskWorktreeProposal {
                branch: "created".into(),
                path: dir.path().join("created"),
            },
        };
        pending.resolved_worktree_path = Some(resolved.proposal.path.clone());
        let deferred = app.update_event(Event::TaskMetadataResolved(pending.clone(), Ok(resolved)));
        assert_pending(&mut response);
        assert_eq!(
            app.task_authority
                .store()
                .as_ref()
                .unwrap()
                .get(&pending.task_id)
                .unwrap()
                .lifecycle,
            TaskLifecycle::Preparing
        );
        let prepared = PreparedTaskWorktree {
            repository,
            base: record.base,
            branch: "created".into(),
            path: dir.path().join("created"),
        };
        if fail_save {
            std::fs::remove_file(dir.path().join("tasks.json")).unwrap();
            std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
        }
        let _effect = app.update_event(Event::TaskWorktreePrepared(pending.clone(), Ok(prepared)));
        let result = iced::futures::executor::block_on(response);
        if fail_save {
            assert!(result.unwrap_err().contains("atomically write"));
            assert_eq!(
                app.task_authority
                    .store()
                    .as_ref()
                    .unwrap()
                    .get(&pending.task_id)
                    .unwrap()
                    .lifecycle,
                TaskLifecycle::Preparing
            );
            drop(deferred);
            continue;
        }
        let value = result.unwrap();
        assert_eq!(value["lifecycle"], "ready");
        assert_eq!(value["created_by"]["session_id"], "original-caller");
        let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
        assert_eq!(
            saved.get(&pending.task_id).unwrap().lifecycle,
            TaskLifecycle::Ready
        );
        drop(deferred);
    }
}

#[test]
fn iced_mutation_input_fixture_preserves_control_normalization_and_errors() {
    if isolated(
        "iced_mutation_input_fixture_preserves_control_normalization_and_errors",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let cases: Vec<Value> = serde_json::from_str(&fixture("tasks/mutation-inputs.json")).unwrap();
    for case in cases {
        let mut input = case.clone();
        input["repository_path"] = json!(dir.path());
        let request = serde_json::from_value(input).unwrap();
        let (sender, mut receiver) = command_channel();
        let _pending = sender.enqueue((), None).unwrap();
        let result =
            app.pending_task_from_control(request, receiver.try_recv().unwrap().reply, None);
        if let Some(error) = case["error"].as_str() {
            assert_eq!(result.err().unwrap(), error);
        } else {
            let pending = result.unwrap();
            assert_eq!(pending.title, case["title_out"]);
            assert_eq!(pending.objective, case["objective_out"]);
            assert_eq!(json!(pending.issue_key), case["issue_out"]);
            assert_eq!(
                pending.request.base_reference,
                case["base_out"].as_str().unwrap_or(DEFAULT_TASK_BASE)
            );
        }
    }
}

#[test]
fn iced_mutation_refusals_do_not_change_the_store() {
    if isolated(
        "iced_mutation_refusals_do_not_change_the_store",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let store = app.task_authority.store_mut().as_mut().unwrap();
    let mut task = store.get("fixture-task").unwrap().clone();
    task.worktree = TaskWorktree {
        state: TaskWorktreeState::Unprepared,
        path: None,
    };
    store.replace(task).unwrap();
    let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
    for (summary, session, error) in [
        (" ", None, "handoff summary is required"),
        (
            "note",
            Some("not-a-session"),
            "Task fixture-task does not contain session not-a-session",
        ),
    ] {
        let request = serde_json::from_value(
            json!({"task_id":"fixture-task", "summary":summary,"session_id":session}),
        )
        .unwrap();
        assert_eq!(
            control(&mut app, TaskControlOperation::UpdateHandoff(request), None).unwrap_err(),
            error
        );
    }
    // Missing-worktree publishing is a silent no-op, even after issue confirmation.
    let _ = app.start_task_publish("absent", TaskPublishAction::PushBranch);
    let _ = app.start_task_publish("fixture-task", TaskPublishAction::OpenDraftPr);
    assert!(app.task_publish_busy.is_empty());
    let _ = app.update_event(Event::TaskWorktreeDeleteOpen("absent".into()));
    assert_eq!(
        app.task_ui_error.as_deref(),
        Some("Task is no longer in the task store")
    );
    assert_eq!(
        std::fs::read(dir.path().join("tasks.json")).unwrap(),
        before
    );
}

#[test]
fn iced_close_archive_states_match_the_shared_fixture() {
    if isolated(
        "iced_close_archive_states_match_the_shared_fixture",
        "task_authority",
    ) {
        return;
    }
    let cases: Vec<Value> = serde_json::from_str(&fixture("tasks/close-states.json")).unwrap();
    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let lifecycle: TaskLifecycle = serde_json::from_value(case["state"].clone()).unwrap();
        let store = app.task_authority.store_mut().as_mut().unwrap();
        if lifecycle != TaskLifecycle::Draft {
            store
                .begin_worktree_preparation("fixture-task", "t1")
                .unwrap();
            if lifecycle != TaskLifecycle::Preparing {
                let task = store.get("fixture-task").unwrap();
                let prepared = CompletedWorktreePreparation {
                    repository: task.repository.clone(),
                    base: task.base.clone(),
                    branch: task.branch.clone(),
                    path: dir.path().join("worktree"),
                };
                store
                    .complete_worktree_preparation("fixture-task", prepared, "t2")
                    .unwrap();
                if lifecycle == TaskLifecycle::Archived {
                    store.archive("fixture-task", "t3").unwrap();
                } else if lifecycle != TaskLifecycle::Ready {
                    store
                        .record_lifecycle_signal("fixture-task", lifecycle, None, "t3")
                        .unwrap();
                }
            }
        }
        let _ = app.update_event(Event::TaskArchive("fixture-task".into()));
        assert_eq!(
            app.task_ui_error.is_none(),
            case["allowed"].as_bool().unwrap(),
            "{case}"
        );
        let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
        assert_eq!(
            saved.get("fixture-task").unwrap().lifecycle == TaskLifecycle::Archived,
            case["allowed"].as_bool().unwrap()
        );
    }
}

#[test]
fn iced_cleanup_confirmation_blocks_risky_removal_and_preserves_history_when_record_only() {
    if isolated(
        "iced_cleanup_confirmation_blocks_risky_removal_and_preserves_history_when_record_only",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let session: TaskSessionRecord = serde_json::from_str(&fixture("tasks/session.json")).unwrap();
    app.task_authority
        .store_mut()
        .as_mut()
        .unwrap()
        .upsert_session("fixture-task", session, "t1")
        .unwrap();
    let inspection = CleanupInspection {
        path_exists: true,
        registered: true,
        dirty: false,
        unpushed_commits: 0,
        upstream: None,
        risks: vec![],
    };
    let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
    for risk in [
        CleanupRisk::ActiveProcess,
        CleanupRisk::UncommittedChanges,
        CleanupRisk::UnpushedCommits { count: 1 },
    ] {
        let mut blocked = inspection.clone();
        blocked.risks.push(risk);
        app.task_worktree_delete_prompt = Some(TaskWorktreeDeletePrompt {
            task_id: "fixture-task".into(),
            worktree_path: dir.path().into(),
            inspection: Some(Ok(blocked)),
            deleting: false,
            error: None,
        });
        let _ = app.update_event(Event::TaskWorktreeDeleteConfirm);
        assert!(!app.task_worktree_delete_prompt.as_ref().unwrap().deleting);
        assert_eq!(
            std::fs::read(dir.path().join("tasks.json")).unwrap(),
            before
        );
    }
    let mut record_only = inspection;
    record_only.path_exists = false;
    record_only.risks = vec![
        CleanupRisk::WorktreeMissing,
        CleanupRisk::ExistingPullRequest {
            url: "https://example.invalid/pr/1".into(),
        },
    ];
    app.task_worktree_delete_prompt.as_mut().unwrap().inspection = Some(Ok(record_only));
    let _ = app.update_event(Event::TaskWorktreeDeleteConfirm);
    let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    let task = saved.get("fixture-task").unwrap();
    assert!(task.worktree.path.is_none());
    assert_eq!(task.sessions.len(), 1);
    assert!(dir.path().is_dir());
    assert!(app.task_worktree_delete_prompt.is_none());
}

#[test]
fn iced_existing_checkout_reuse_preserves_outcomes_and_refuses_before_writes() {
    if isolated(
        "iced_existing_checkout_reuse_preserves_outcomes_and_refuses_before_writes",
        "task_authority",
    ) {
        return;
    }
    for scenario in [
        "completed",
        "failed",
        "stopped",
        "stale",
        "archived",
        "save-error",
        "removed-destination",
        "replaced-destination",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let (pending, _, _unused_reply, _unused_receiver) =
            pending_worker_fixture(&mut app, dir.path());
        let worker = pending.worker.unwrap();
        app.cancel_worker_preparation(worker.authority);
        let TaskMutationAction::WorkerAuthorityChecked { id: authority } = app
            .reduce_task_mutation(TaskMutation::BeginWorkerPreparation {
                caller: worker.caller.session_uid.clone(),
            })
            .unwrap()
        else {
            panic!("capture");
        };
        let checkout = dir.path().join("reuse");
        fixture_worker_checkout(&app.workspaces[0].tabs[0].repo_path, &checkout, "reuse");
        let checkout = std::fs::canonicalize(checkout).unwrap();
        let mut record = app
            .task_authority
            .store()
            .as_ref()
            .unwrap()
            .get("fixture-task")
            .unwrap()
            .clone();
        record.task_id = "reuse-task".into();
        record.worktree.path = Some(checkout.clone());
        record.repository = worker_repository_identity(&checkout).unwrap().unwrap();
        record.lifecycle = match scenario {
            "failed" => TaskLifecycle::Failed,
            "stopped" => TaskLifecycle::Stopped,
            "archived" => TaskLifecycle::Archived,
            _ => TaskLifecycle::Completed,
        };
        if scenario == "archived" {
            record.archived_at = Some("t2".into());
        }
        record.last_error = Some("retained diagnostic".into());
        record.attention.unread = true;
        app.task_authority
            .store_mut()
            .as_mut()
            .unwrap()
            .insert(record.clone())
            .unwrap();
        let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
        if scenario == "stale" {
            app.workspaces[0].tabs[0].chat_session_id = Some("replacement".into());
        }
        if scenario == "save-error" {
            std::fs::remove_file(dir.path().join("tasks.json")).unwrap();
            std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
        }
        if matches!(scenario, "removed-destination" | "replaced-destination") {
            std::fs::rename(&checkout, dir.path().join("old-checkout")).unwrap();
            if scenario == "replaced-destination" {
                Repository::init(&checkout).unwrap();
            }
        }
        let request = serde_json::from_value(
            json!({"title":"Reused title", "objective":"New objective", "checkout":checkout}),
        )
        .unwrap();
        let targets = DelegateTargets {
            authority,
            request,
            caller: Some(worker.caller.session_uid.clone()),
            tab: worker.caller,
            choice: worker.choice,
            place: gitterm::delegations::WorkerPlace::Checkout(checkout.clone()),
            options: worker.options,
            adoption: None,
        };
        let (sender, mut receiver) = command_channel();
        let response = sender.enqueue((), None).unwrap();
        let _effect =
            app.delegate_task_in_checkout(targets, checkout, receiver.try_recv().unwrap().reply);
        let error = iced::futures::executor::block_on(response).unwrap_err();
        let store = app.task_authority.store().as_ref().unwrap();
        let saved = store.get("reuse-task").unwrap();
        assert_eq!(saved.lifecycle, record.lifecycle);
        assert_eq!(saved.attention, record.attention);
        assert_eq!(saved.last_error, record.last_error);
        if matches!(
            scenario,
            "stale" | "archived" | "save-error" | "removed-destination" | "replaced-destination"
        ) {
            assert_eq!(saved, &record, "{scenario}: {error}");
            assert!(store.delegations().is_empty());
            if scenario != "save-error" {
                assert_eq!(
                    std::fs::read(dir.path().join("tasks.json")).unwrap(),
                    before
                );
            }
        } else {
            assert!(
                error.contains("selected agent preset is unavailable"),
                "{error}"
            );
            assert_eq!(saved.title, "Reused title");
            assert_eq!(saved.objective, "New objective");
            assert_eq!(store.delegations().len(), 1);
        }
        assert!(app.task_launch_queue.is_empty());
    }
}

#[test]
fn iced_prepared_destination_replacement_is_refused_before_save() {
    if isolated(
        "iced_prepared_destination_replacement_is_refused_before_save",
        "task_authority",
    ) {
        return;
    }
    for replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let (pending, prepared, response, _receiver) = pending_worker_fixture(&mut app, dir.path());
        std::fs::rename(&prepared.path, dir.path().join("old-prepared")).unwrap();
        if replace {
            Repository::init(&prepared.path).unwrap();
        }
        let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
        let _effect = app.update_event(Event::TaskWorktreePrepared(pending, Ok(prepared)));
        assert!(iced::futures::executor::block_on(response)
            .unwrap_err()
            .contains("destination checkout"));
        assert_eq!(
            std::fs::read(dir.path().join("tasks.json")).unwrap(),
            before
        );
        assert!(app
            .task_authority
            .store()
            .as_ref()
            .unwrap()
            .delegations()
            .is_empty());
        assert!(app.task_launch_queue.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn iced_symlink_destination_normalization_preserves_authority_and_detects_retargeting() {
    if isolated(
        "iced_symlink_destination_normalization_preserves_authority_and_detects_retargeting",
        "task_authority",
    ) {
        return;
    }
    for retarget in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("actual-root");
        std::fs::create_dir(&root).unwrap();
        let alias = dir.path().join("alias-root");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let (pending, prepared, response, _receiver) =
            pending_worker_fixture_at(&mut app, dir.path(), &alias.join("prepared"));
        let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
        if retarget {
            let other = dir.path().join("other-root");
            std::fs::create_dir(&other).unwrap();
            std::fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink(other, &alias).unwrap();
        }
        let path = prepared.path.clone();
        let _effect = app.update_event(Event::TaskWorktreePrepared(pending.clone(), Ok(prepared)));
        let error = iced::futures::executor::block_on(response).unwrap_err();
        if retarget {
            assert!(error.contains("destination checkout changed"), "{error}");
            assert_eq!(
                std::fs::read(dir.path().join("tasks.json")).unwrap(),
                before
            );
            assert!(app
                .task_authority
                .store()
                .as_ref()
                .unwrap()
                .delegations()
                .is_empty());
        } else {
            assert!(
                error.contains("selected agent preset is unavailable"),
                "{error}"
            );
            assert_eq!(
                app.task_authority
                    .store()
                    .as_ref()
                    .unwrap()
                    .get(&pending.task_id)
                    .unwrap()
                    .worktree
                    .path
                    .as_ref(),
                Some(&path)
            );
        }
    }
}

#[test]
fn iced_non_repository_caller_can_delegate_to_a_listed_checkout() {
    if isolated(
        "iced_non_repository_caller_can_delegate_to_a_listed_checkout",
        "task_authority",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let (pending, prepared, _unused_reply, _unused_receiver) =
        pending_worker_fixture(&mut app, dir.path());
    let worker = pending.worker.unwrap();
    app.cancel_worker_preparation(worker.authority);
    let home = dir.path().join("workspace-home");
    std::fs::create_dir(&home).unwrap();
    let listed_source = app.workspaces[0].tabs[0].repo_path.clone();
    app.workspaces[0].pending_repos.push(listed_source);
    app.workspaces[0].dir = home.clone();
    app.workspaces[0].location = WorkspaceLocation::Local { root: home.clone() };
    app.workspaces[0].tabs[0].repo_path = home;
    let uid = worker.caller.session_uid.clone();
    assert!(app
        .worker_authority_caller(&uid)
        .unwrap()
        .repository
        .is_none());
    let TaskMutationAction::WorkerAuthorityChecked { id: authority } = app
        .reduce_task_mutation(TaskMutation::BeginWorkerPreparation {
            caller: uid.clone(),
        })
        .unwrap()
    else {
        panic!("capture");
    };
    let caller = app.delegation_caller(&uid).unwrap();
    assert_eq!(
        gitterm::delegations::resolve_checkout("checkout", &prepared.path, &caller).unwrap(),
        prepared.path
    );
    let request = serde_json::from_value(
        json!({"title":"Explicit checkout", "objective":"Fixture", "checkout":prepared.path}),
    )
    .unwrap();
    let targets = DelegateTargets {
        authority,
        request,
        caller: Some(uid),
        tab: caller,
        choice: worker.choice,
        place: gitterm::delegations::WorkerPlace::Checkout(prepared.path.clone()),
        options: worker.options,
        adoption: Some(AdoptedWorktreeResolution {
            repository: prepared.repository.clone(),
            base: prepared.base.clone(),
            branch: prepared.branch.clone(),
        }),
    };
    let (sender, mut receiver) = command_channel();
    let response = sender.enqueue((), None).unwrap();
    let _effect =
        app.delegate_task_in_checkout(targets, prepared.path, receiver.try_recv().unwrap().reply);
    let error = iced::futures::executor::block_on(response).unwrap_err();
    assert!(
        error.contains("selected agent preset is unavailable"),
        "{error}"
    );
    assert_eq!(
        app.task_authority
            .store()
            .as_ref()
            .unwrap()
            .delegations()
            .len(),
        1
    );
}
