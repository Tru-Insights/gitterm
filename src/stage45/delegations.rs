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
            authority: fixture_authority(&caller),
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
        app.task_host
            .store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(delegation)
            .unwrap();
    }
    let _unpolled = app.start_queued_delegations();
    complete_checks(&mut app);
    assert!(app.delegation_runs.contains_key("first"));
    assert!(!app.delegation_runs.contains_key("second"));
    let _unpolled = app.start_queued_delegations();
    complete_checks(&mut app);
    assert_eq!(app.delegation_runs.len(), 1);
    let _unpolled = app.update_event(Event::DelegationDismissRequested(10, "first".into()));
    complete_checks(&mut app);
    assert!(!app.delegation_runs.contains_key("first"));
    assert!(app.delegation_runs.contains_key("second"));
    let _unpolled = app.update_event(Event::DelegationDismissRequested(10, "first".into()));
    complete_checks(&mut app);
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
    let store = app.task_host.store().as_ref().unwrap();
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
    app.task_host
        .replace_store(Some(TaskStore::load(dir.join("tasks.json")).unwrap()));
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
        let authority = fixture_authority(
            &app.delegation_caller_for_tab(if stale { 11 } else { 10 })
                .unwrap(),
        );
        let _unpolled = app.delegation_checked(
            request,
            *new,
            DelegationOrigin::Mcp(receiver.try_recv().unwrap().reply),
            Ok(authority),
        );
        let result = iced::futures::executor::block_on(reply);
        complete_checks(&mut app);
        if stale {
            assert!(result.unwrap_err().contains("calling GitTerm tab changed"));
            assert!(app
                .task_host
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
        app.task_host
            .store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(d)
            .unwrap();
        app.delegation_state.reduce(DelegationIntent::Stored {
            id: "attempt".into(),
            authority: fixture_authority(&caller.tab),
            caller,
        });
        let _unpolled = app.start_queued_delegations();
        complete_checks(&mut app);
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
            app.task_host
                .store()
                .as_ref()
                .unwrap()
                .delegation("attempt")
                .unwrap()
                .status,
            gitterm::tasks::DelegationStatus::Requested
        );
        let _unpolled = app.start_queued_delegations();
        complete_checks(&mut app);
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

// A fake preflight result drives the real Iced continuation without polling a
// Codex stream. Filesystem validation itself has separate disposable Git tests.
fn fixture_authority(caller: &gitterm::delegations::CallerTab) -> delegation_state::Authority {
    delegation_state::Authority {
        checkout: caller.cwd.clone(),
        repository: gitterm::tasks::RepositoryIdentity {
            common_dir: caller.cwd.join(".git"),
            remote_url: None,
        },
        workdir: caller.cwd.clone(),
        roots: vec![Some(caller.home.clone())],
    }
}
fn complete_checks(app: &mut App) {
    let ids: Vec<_> = app
        .task_host
        .store()
        .as_ref()
        .into_iter()
        .flat_map(|s| s.delegations())
        .map(|d| d.delegation_id.clone())
        .collect();
    for id in ids {
        if let Some(check) = app.delegation_state.pending_start(&id) {
            let d = app
                .task_host
                .store()
                .as_ref()
                .unwrap()
                .delegation(&id)
                .unwrap();
            let caller = app.delegation_caller(&d.parent.session_uid).unwrap();
            let _unpolled = app.delegation_start_checked(id, check, Ok(fixture_authority(&caller)));
        }
    }
}

#[test]
fn missing_unrelated_repo_is_optional_and_queued_checks_do_not_read_the_filesystem() {
    if isolated(
        "missing_unrelated_repo_is_optional_and_queued_checks_do_not_read_the_filesystem",
        "delegations",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("file"), "fixture").unwrap();
    git(dir.path(), &["add", "file"]);
    git(dir.path(), &["commit", "-q", "-m", "fixture"]);
    let mut app = delegation_app(dir.path());
    app.workspaces[0]
        .pending_repos
        .push(dir.path().join("unavailable"));
    let uid = app.workspaces[0].tabs[0].session_uid.clone();
    let caller = app.delegation_identity(&uid).unwrap();
    assert!(caller.tab.repos.iter().any(|p| p.ends_with("unavailable")));
    let mut new = gitterm::delegations::consult_delegation(
        &task_mcp::ConsultDelegationRequest {
            brief: "Inspect parser".into(),
            checkout: Some(dir.path().into()),
            ..Default::default()
        },
        &caller.tab,
        None,
        None,
    )
    .unwrap();
    let authority = check_delegation_run(&mut new, &caller.tab).unwrap();
    assert!(authority.roots.iter().any(Option::is_none));
    let mut d = gitterm::tasks::Delegation::new_requested(new, "t0");
    d.delegation_id = "preflight".into();
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .insert_delegation(d)
        .unwrap();
    app.delegation_state.reduce(DelegationIntent::Stored {
        id: "preflight".into(),
        caller,
        authority: authority.clone(),
    });
    let _unpolled = app.start_queued_delegations();
    let check = app.delegation_state.pending_start("preflight").unwrap();
    assert!(app.delegation_runs.is_empty());
    let _unpolled = app.start_queued_delegations();
    assert_eq!(app.delegation_state.pending_start("preflight"), Some(check));
    // The catalog-only identity remains readable with unavailable paths. A
    // changed incarnation while the worker checks paths is refused at return.
    app.workspaces[0].tabs[0].id = 11;
    let _unpolled = app.delegation_start_checked("preflight".into(), check, Ok(authority));
    assert!(app.delegation_runs.is_empty());
    assert!(matches!(
        app.task_host
            .store()
            .as_ref()
            .unwrap()
            .delegation("preflight")
            .unwrap()
            .status,
        gitterm::tasks::DelegationStatus::Failed { .. }
    ));
}

fn pending_start(app: &App, id: &str) -> Option<delegation_state::RequestId> {
    app.delegation_state.pending_start(id)
}

fn store_consult(app: &mut App, id: &str) {
    let caller = app.delegation_caller_for_tab(10).unwrap();
    let identity = app.delegation_identity(&caller.session_uid).unwrap();
    app.delegation_state.reduce(DelegationIntent::Stored {
        id: id.into(),
        caller: identity,
        authority: fixture_authority(&caller),
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
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .insert_delegation(delegation)
        .unwrap();
}

/// A fresh Codex reading `step` seconds after the previous one.
fn codex_reading(app: &mut App, step: i64, weekly: f64, five_hour: f64) {
    use gitterm::plan_limits::{LimitWindow, PlanLimits, Provider};
    let now = chrono::Utc::now().timestamp() + step;
    assert!(app.plan_limits.finish_codex_read(Some(Box::new(PlanLimits {
        provider: Provider::Codex,
        observed_at: now,
        local: Some(true),
        windows: vec![
            LimitWindow {
                label: "5h".into(),
                used_percent: Some(five_hour),
                resets_at: Some(now + 3 * 3600),
            },
            LimitWindow {
                label: "7d".into(),
                used_percent: Some(weekly),
                resets_at: Some(now + 86_400),
            },
        ],
        alerts: vec![],
        plan_type: None,
        credits: None,
    }))));
}

fn delegation_status(app: &App, id: &str) -> gitterm::tasks::DelegationStatus {
    app.task_host
        .store()
        .as_ref()
        .unwrap()
        .delegation(id)
        .unwrap()
        .status
        .clone()
}

#[test]
fn queued_start_rechecks_policy_trust_and_batch_capacity() {
    use gitterm::tasks::DelegationStatus;
    if isolated(
        "queued_start_rechecks_policy_trust_and_batch_capacity",
        "delegations",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = delegation_app(dir.path());
    app.review_config.max_concurrent_reviews = 2;
    for id in ["a", "b", "c", "d"] {
        store_consult(&mut app, id);
    }
    // Batch capacity: two slots, so admission reserves a and b only, and
    // both checks returning in one batch start both.
    let _unpolled = app.start_queued_delegations();
    assert!(pending_start(&app, "a").is_some() && pending_start(&app, "b").is_some());
    assert!(pending_start(&app, "c").is_none() && pending_start(&app, "d").is_none());
    complete_checks(&mut app);
    assert!(app.delegation_runs.contains_key("a") && app.delegation_runs.contains_key("b"));
    let _unpolled = app.start_queued_delegations();
    assert!(pending_start(&app, "c").is_none() && pending_start(&app, "d").is_none());

    // Policy: a and b finish and free both slots; c and d are admitted, then
    // a burst hold arrives before their checks return. The batch shares one
    // running count: c sees d reserved (parallel) and is refused, d then runs
    // alone (serial).
    for id in ["a", "b"] {
        let _unpolled = app.finish_delegation(
            id,
            Err(gitterm::delegations::RunFailure::Failed("fixture".into())),
        );
    }
    assert!(pending_start(&app, "c").is_some() && pending_start(&app, "d").is_some());
    codex_reading(&mut app, 0, 10.0, 85.0);
    complete_checks(&mut app);
    assert!(!app.delegation_runs.contains_key("c"));
    assert!(app.delegation_runs.contains_key("d"));
    assert_eq!(delegation_status(&app, "c"), DelegationStatus::Requested);
    assert!(pending_start(&app, "c").is_none());

    // A weekly hold refuses a re-admitted c at its check, without failing it.
    codex_reading(&mut app, 1, 10.0, 10.0);
    let _unpolled = app.start_queued_delegations();
    assert!(pending_start(&app, "c").is_some());
    codex_reading(&mut app, 2, 99.0, 10.0);
    complete_checks(&mut app);
    assert!(!app.delegation_runs.contains_key("c"));
    assert_eq!(delegation_status(&app, "c"), DelegationStatus::Requested);

    // Trust: the hold lifts, c is admitted again, but its checkout changed
    // while it was checked. The paid attempt fails and is never retried.
    codex_reading(&mut app, 3, 10.0, 10.0);
    let _unpolled = app.start_queued_delegations();
    let check = pending_start(&app, "c").expect("c is re-admitted");
    let caller = app.delegation_caller_for_tab(10).unwrap();
    let mut moved = fixture_authority(&caller);
    moved.checkout = dir.path().join("elsewhere");
    let _unpolled = app.delegation_start_checked("c".into(), check, Ok(moved));
    assert!(!app.delegation_runs.contains_key("c"));
    match delegation_status(&app, "c") {
        DelegationStatus::Failed { message } => {
            assert!(message.contains("checkout changed"), "{message}")
        }
        other => panic!("c is {other:?}"),
    }
    let _unpolled = app.start_queued_delegations();
    assert!(pending_start(&app, "c").is_none());
    assert!(!app.delegation_runs.contains_key("c"));
}
