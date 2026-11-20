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

#[test]
fn legacy_worker_preparation_retains_stale_caller_snapshot() {
    if isolated(
        "legacy_worker_preparation_retains_stale_caller_snapshot",
        "task_authority",
    ) {
        return;
    }
    // Existing gap, deferred to PR15b's R2 gate: post-preparation dispatch
    // consumes the old caller snapshot. An invalid preset prevents any launch.
    for remove_tab in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_tabs(dir.path());
        add_task(&mut app, dir.path(), dir.path());
        let caller_id = app.workspaces[0].tabs[0].session_uid.clone();
        let caller = app
            .delegation_caller_for_tool(Some(&caller_id), "delegate_task")
            .unwrap();
        if remove_tab {
            app.workspaces[0].tabs.clear();
        } else {
            app.workspaces[0].tabs[0].repo_path = dir.path().join("changed-checkout");
        }
        let (sender, mut receiver) = command_channel();
        let pending = sender.enqueue((), None).unwrap();
        let worker = PendingWorker {
            caller: caller.clone(),
            choice: gitterm::workers::WorkerChoice {
                preset_index: usize::MAX,
                preset_name: "Unavailable".into(),
                provider: gitterm::workers::WorkerProvider::Codex,
                role: gitterm::workers::WorkerRole::Judgment,
                model: None,
                model_source: gitterm::workers::ModelSource::CliDefault,
            },
            options: Default::default(),
        };
        let _effect =
            app.start_delegated_worker("fixture-task", worker, receiver.try_recv().unwrap().reply);
        assert!(iced::futures::executor::block_on(pending)
            .unwrap_err()
            .contains("selected agent preset is unavailable"));
        let delegation = &app.task_authority.store().as_ref().unwrap().delegations()[0];
        assert_eq!(delegation.parent.session_uid, caller_id);
        assert_eq!(delegation.parent.cwd, caller.cwd);
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
