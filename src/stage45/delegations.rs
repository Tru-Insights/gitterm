//! Pins through the pre-reducer Iced owner. Tasks are inspected, never polled.
use super::*;

#[test]
fn review_consult_admission_cancel_and_late_completion() {
    if isolated(
        "review_consult_admission_cancel_and_late_completion",
        "delegations",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = delegation_app(dir.path());
    let caller = app.delegation_caller_for_tab(10).unwrap();
    let identity = app.delegation_identity(&caller.session_uid).unwrap();
    for id in ["first", "second"] {
        app.delegation_state.reduce(DelegationIntent::Stored {
            id: id.into(),
            caller: identity.clone(),
        });
        let new = gitterm::delegations::consult_delegation(
            &task_mcp::ConsultDelegationRequest {
                brief: "Inspect the parser".into(),
                ..Default::default()
            },
            &caller,
            None,
            None,
        )
        .unwrap();
        let mut delegation = gitterm::tasks::Delegation::new_requested(new, "t0");
        delegation.delegation_id = id.into();
        app.task_authority
            .store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(delegation)
            .unwrap();
    }
    let _unpolled = app.start_queued_delegations();
    assert!(app.delegation_runs.contains_key("first"));
    assert!(!app.delegation_runs.contains_key("second"));
    let _unpolled = app.start_queued_delegations();
    assert_eq!(app.delegation_runs.len(), 1);
    let _unpolled = app.update_event(Event::DelegationDismissRequested(10, "first".into()));
    assert!(!app.delegation_runs.contains_key("first"));
    assert!(app.delegation_runs.contains_key("second"));
    let _unpolled = app.update_event(Event::DelegationDismissRequested(10, "first".into()));
    let _unpolled = app.finish_delegation(
        "first",
        Err(gitterm::delegations::RunFailure::Failed("late".into())),
    );
    assert_eq!(app.delegation_runs.len(), 1);
    let _unpolled = app.finish_delegation(
        "second",
        Err(gitterm::delegations::RunFailure::Failed(
            "fixture failure".into(),
        )),
    );
    let store = app.task_authority.store().as_ref().unwrap();
    let first = store.delegation("first").unwrap();
    let second = store.delegation("second").unwrap();
    golden(
        "tasks/delegation-lifecycle.trace.json",
        json!({
            "first":first.status, "second":second.status,
            "first_dismissed":first.dismissed_at.is_some(),
            "running":app.delegation_runs.len(),
            "rerun_previous":gitterm::delegations::rerun_delegation(second).unwrap().previous,
        }),
    );
}

fn delegation_app(dir: &Path) -> App {
    let mut app = app_with_tabs(dir);
    app.task_authority = Some(TaskStore::load(dir.join("tasks.json")).unwrap()).into();
    app.review_config.max_concurrent_reviews = 1;
    app.workspaces[0].dir = dir.into();
    app.workspaces[0].location = WorkspaceLocation::Local { root: dir.into() };
    app.workspaces[0].tabs[0].repo_path = dir.into();
    app.workspaces[0].tabs[0].kind =
        TabKind::Agent(AgentSession::new(tab::AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        }));
    app
}

#[test]
fn checked_request_replies_only_after_insert_and_refuses_a_restored_caller() {
    if isolated(
        "checked_request_replies_only_after_insert_and_refuses_a_restored_caller",
        "delegations",
    ) {
        return;
    }
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let _trace = Trace::start();
        let mut app = delegation_app(dir.path());
        let caller = app
            .delegation_identity(&app.workspaces[0].tabs[0].session_uid)
            .unwrap();
        let new = gitterm::delegations::consult_delegation(
            &task_mcp::ConsultDelegationRequest {
                brief: "Inspect parser".into(),
                ..Default::default()
            },
            &caller.tab,
            None,
            None,
        )
        .unwrap();
        let DelegationEffect::Check { request, new, .. } = app
            .delegation_state
            .reduce(DelegationIntent::Request {
                new: Box::new(new),
                caller,
            })
            .pop()
            .unwrap()
        else {
            panic!()
        };
        if stale {
            app.workspaces[0].tabs[0].id = 11;
        }
        let (sender, mut receiver) = gitterm_core::task_commands::runtime::command_channel();
        let reply = sender.enqueue((), None).unwrap();
        let _unpolled = app.delegation_checked(
            request,
            *new,
            DelegationOrigin::Mcp(receiver.try_recv().unwrap().reply),
            Ok(()),
        );
        let result = iced::futures::executor::block_on(reply);
        if stale {
            assert!(result.unwrap_err().contains("calling GitTerm tab changed"));
            assert!(app
                .task_authority
                .store()
                .as_ref()
                .unwrap()
                .delegations()
                .is_empty());
            assert!(app.delegation_runs.is_empty());
        } else {
            let result = result.unwrap();
            assert_eq!(result["status"], "requested");
            assert_eq!(result["queued"], false);
            let id = result["delegation_id"].as_str().unwrap();
            let loaded = TaskStore::load(dir.path().join("tasks.json")).unwrap();
            assert!(loaded.delegation(id).is_some());
            assert!(app.delegation_runs.contains_key(id));
        }
    }
}

#[test]
fn failed_started_terminal_and_cancellation_saves_never_restart_a_paid_attempt() {
    if isolated(
        "failed_started_terminal_and_cancellation_saves_never_restart_a_paid_attempt",
        "delegations",
    ) {
        return;
    }
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let _trace = Trace::start();
        let mut app = delegation_app(dir.path());
        let caller = app
            .delegation_identity(&app.workspaces[0].tabs[0].session_uid)
            .unwrap();
        let new = gitterm::delegations::consult_delegation(
            &task_mcp::ConsultDelegationRequest {
                brief: "Inspect parser".into(),
                ..Default::default()
            },
            &caller.tab,
            None,
            None,
        )
        .unwrap();
        let mut d = gitterm::tasks::Delegation::new_requested(new, "t0");
        d.delegation_id = "attempt".into();
        app.task_authority
            .store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(d)
            .unwrap();
        app.delegation_state.reduce(DelegationIntent::Stored {
            id: "attempt".into(),
            caller,
        });
        let _unpolled = app.start_queued_delegations();
        assert!(app.delegation_runs.contains_key("attempt"));
        // Replace only this fixture's destination with a directory to fail
        // atomic saves while preserving the in-memory Requested record.
        std::fs::rename(
            dir.path().join("tasks.json"),
            dir.path().join("before.json"),
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
        app.delegation_progress(
            "attempt",
            gitterm::codex_runner::CodexRunEvent::Started {
                thread_id: "fake-thread".into(),
            },
        );
        if cancel {
            let _unpolled =
                app.update_event(Event::DelegationDismissRequested(10, "attempt".into()));
        } else {
            let _unpolled = app.finish_delegation(
                "attempt",
                Err(gitterm::delegations::RunFailure::Failed(
                    "fixture failure".into(),
                )),
            );
        }
        assert!(app.delegation_runs.is_empty());
        assert!(!app.delegation_state.busy());
        assert_eq!(
            app.task_authority
                .store()
                .as_ref()
                .unwrap()
                .delegation("attempt")
                .unwrap()
                .status,
            gitterm::tasks::DelegationStatus::Requested
        );
        let _unpolled = app.start_queued_delegations();
        assert!(app.delegation_runs.is_empty());
        app.delegation_progress(
            "attempt",
            gitterm::codex_runner::CodexRunEvent::Activity {
                description: "late".into(),
            },
        );
        assert!(app.delegation_state.activity("attempt").is_none());
    }
}
