//! Compatibility facade for the shared delegations service.

pub use gitterm_core::delegations::*;

#[cfg(test)]
pub(crate) use gitterm_core::chat::history::DELIVERY_SOURCE_LINE;
#[cfg(test)]
#[path = "../crates/gitterm-core/src/delegation_test_support.rs"]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::test_task;

use crate::task_mcp;
use crate::tasks::{NewDelegation, TaskHandoff, TaskStore};
use gitterm_core::delegation_request as input;

fn review_input(request: &task_mcp::ReviewDelegationRequest) -> input::ReviewDelegationRequest {
    input::ReviewDelegationRequest {
        target: match request.target {
            task_mcp::ReviewTargetKind::Uncommitted => input::ReviewTargetKind::Uncommitted,
            task_mcp::ReviewTargetKind::Base => input::ReviewTargetKind::Base,
            task_mcp::ReviewTargetKind::Commit => input::ReviewTargetKind::Commit,
        },
        base_ref: request.base_ref.clone(),
        commit: request.commit.clone(),
        focus: request.focus.clone(),
        reviewer: request.reviewer.clone(),
        model: request.model.clone(),
        user_model_choice: request.user_model_choice,
        checkout: request.checkout.clone(),
        effort: request.effort,
        time_limit_minutes: request.time_limit_minutes,
    }
}

pub fn review_delegation(
    request: &task_mcp::ReviewDelegationRequest,
    caller: &CallerTab,
    configured_model: Option<&str>,
    previous: Option<String>,
) -> Result<NewDelegation, String> {
    gitterm_core::delegations::review_delegation(
        &review_input(request),
        caller,
        configured_model,
        previous,
    )
}

pub fn consult_delegation(
    request: &task_mcp::ConsultDelegationRequest,
    caller: &CallerTab,
    configured_model: Option<&str>,
    previous: Option<String>,
) -> Result<NewDelegation, String> {
    gitterm_core::delegations::consult_delegation(
        &input::ConsultDelegationRequest {
            brief: request.brief.clone(),
            model: request.model.clone(),
            user_model_choice: request.user_model_choice,
            checkout: request.checkout.clone(),
            effort: request.effort,
            time_limit_minutes: request.time_limit_minutes,
        },
        caller,
        configured_model,
        previous,
    )
}

pub fn review_request_from_popover(
    request: &crate::review::ReviewRequest,
) -> task_mcp::ReviewDelegationRequest {
    let request = gitterm_core::delegations::review_request_from_popover(request);
    task_mcp::ReviewDelegationRequest {
        target: match request.target {
            input::ReviewTargetKind::Uncommitted => task_mcp::ReviewTargetKind::Uncommitted,
            input::ReviewTargetKind::Base => task_mcp::ReviewTargetKind::Base,
            input::ReviewTargetKind::Commit => task_mcp::ReviewTargetKind::Commit,
        },
        base_ref: request.base_ref,
        commit: request.commit,
        focus: request.focus,
        reviewer: request.reviewer,
        model: request.model,
        user_model_choice: request.user_model_choice,
        checkout: request.checkout,
        effort: request.effort,
        time_limit_minutes: request.time_limit_minutes,
    }
}

pub fn worker_targets(
    request: &task_mcp::DelegateTaskRequest,
) -> Result<(WorkerPlace, crate::tasks::DelegationRunOptions), String> {
    gitterm_core::delegations::worker_targets(&input::WorkerTargetRequest {
        checkout: request.checkout.clone(),
        repo: request.repo.clone(),
        base_reference: request.base_reference.clone(),
        add_dirs: request.add_dirs.clone(),
        model: request.model.clone(),
        user_model_choice: request.user_model_choice,
        effort: request.effort,
        time_limit_minutes: request.time_limit_minutes,
    })
}

pub fn record_handoff(
    store: &mut TaskStore,
    task_id: &str,
    handoff: TaskHandoff,
    status: task_mcp::HandoffStatus,
    hosted_session: Option<&str>,
    timestamp: &str,
) -> Result<Option<String>, String> {
    let status = match status {
        task_mcp::HandoffStatus::Progress => input::HandoffStatus::Progress,
        task_mcp::HandoffStatus::Done => input::HandoffStatus::Done,
        task_mcp::HandoffStatus::Blocked => input::HandoffStatus::Blocked,
    };
    gitterm_core::delegations::record_handoff(
        store,
        task_id,
        handoff,
        status,
        hosted_session,
        timestamp,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_mcp::{
        ConsultDelegationRequest, DelegateTaskRequest, HandoffStatus, ReviewDelegationRequest,
        ReviewTargetKind,
    };
    use crate::tasks::{
        Delegation, DelegationChild, DelegationKind, DelegationRunOptions, DelegationStatus,
        ReviewTargetMode,
    };
    use crate::workers::{ReasoningEffort, WorkerChoice};
    use std::path::PathBuf;

    fn caller() -> CallerTab {
        CallerTab {
            session_uid: "tab-uid".into(),
            chat_session_id: Some("claude-1".into()),
            workspace: "GitTerm".into(),
            cwd: PathBuf::from("/repo"),
            remote: false,
            home: PathBuf::from("/repo"),
            repos: Vec::new(),
        }
    }

    fn review_request(target: ReviewTargetKind) -> ReviewDelegationRequest {
        ReviewDelegationRequest {
            target,
            base_ref: None,
            commit: None,
            focus: None,
            reviewer: None,
            model: None,
            user_model_choice: false,
            checkout: None,
            effort: None,
            time_limit_minutes: None,
        }
    }

    fn worker_choice() -> WorkerChoice {
        crate::workers::resolve_worker(
            &[crate::workers::PresetRef {
                name: "Codex",
                command: "codex",
            }],
            None,
            None,
            None,
            &crate::workers::ModelPolicy::default(),
        )
        .unwrap()
    }

    fn handoff(summary: &str, blockers: &[&str]) -> TaskHandoff {
        TaskHandoff {
            summary: summary.to_string(),
            decisions: vec!["Kept the PNG path".to_string()],
            next_steps: vec!["Review the branch".to_string()],
            blockers: blockers.iter().map(|item| item.to_string()).collect(),
            updated_by_session_id: Some("session-1".to_string()),
            updated_at: "2026-10-08T10:05:00Z".to_string(),
        }
    }

    fn session(id: &str) -> crate::tasks::TaskSessionRecord {
        crate::tasks::TaskSessionRecord {
            task_session_id: id.to_string(),
            label: "Codex".to_string(),
            harness: None,
            conversation: None,
            objective_delivery: crate::tasks::ObjectiveDeliveryState::Delivered,
            launch_identity: None,
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn review_requests_map_targets_and_refuse_bad_input() {
        let new = review_delegation(
            &review_request(ReviewTargetKind::Uncommitted),
            &caller(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(new.kind, DelegationKind::Review);
        assert_eq!(new.parent.session_uid, "tab-uid");
        assert_eq!(new.parent.cwd, PathBuf::from("/repo"));
        assert_eq!(new.target.unwrap().mode, ReviewTargetMode::Uncommitted);
        assert_eq!(
            new.child,
            DelegationChild::CodexReview {
                thread_id: None,
                model: None
            }
        );

        let mut base = review_request(ReviewTargetKind::Base);
        base.base_ref = Some(" v6 ".into());
        base.focus = Some("webview lifecycle".into());
        base.reviewer = Some("Codex".into());
        let new = review_delegation(&base, &caller(), Some("gpt-6-astra"), None).unwrap();
        let target = new.target.unwrap();
        assert_eq!(
            target.mode,
            ReviewTargetMode::Base {
                reference: "v6".into()
            }
        );
        assert_eq!(target.focus.as_deref(), Some("webview lifecycle"));
        assert!(new.brief.contains("against v6"), "{}", new.brief);
        // The configured Codex model applies when the request names none.
        assert_eq!(
            new.child,
            DelegationChild::CodexReview {
                thread_id: None,
                model: Some("gpt-6-astra".into())
            }
        );

        let mut commit = review_request(ReviewTargetKind::Commit);
        commit.commit = Some("4d018aa".into());
        commit.model = Some("gpt-6-sol".into());
        let new = review_delegation(&commit, &caller(), Some("ignored"), None).unwrap();
        assert_eq!(
            new.child,
            DelegationChild::CodexReview {
                thread_id: None,
                model: Some("gpt-6-sol".into())
            }
        );

        let errors = [
            review_request(ReviewTargetKind::Base),
            review_request(ReviewTargetKind::Commit),
            ReviewDelegationRequest {
                base_ref: Some("--upload-pack=x".into()),
                ..review_request(ReviewTargetKind::Base)
            },
            ReviewDelegationRequest {
                commit: Some("HEAD~1".into()),
                ..review_request(ReviewTargetKind::Commit)
            },
            ReviewDelegationRequest {
                commit: Some("abcdef1".into()),
                ..review_request(ReviewTargetKind::Uncommitted)
            },
            ReviewDelegationRequest {
                reviewer: Some("claude".into()),
                ..review_request(ReviewTargetKind::Uncommitted)
            },
            ReviewDelegationRequest {
                model: Some("x; rm -rf".into()),
                ..review_request(ReviewTargetKind::Uncommitted)
            },
        ];
        for request in errors {
            assert!(
                review_delegation(&request, &caller(), None, None).is_err(),
                "{request:?}"
            );
        }
        let remote = CallerTab {
            remote: true,
            ..caller()
        };
        let error = review_delegation(
            &review_request(ReviewTargetKind::Uncommitted),
            &remote,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("remote"), "{error}");
    }

    #[test]
    fn popover_default_model_keeps_configured_selection_without_an_override() {
        let mut page = crate::review::ReviewRequest {
            target: crate::review::ReviewTarget::Uncommitted,
            focus: None,
            reviewer: crate::review::PopoverReviewer::Codex,
            model: String::new(),
        };
        let request = review_request_from_popover(&page);
        let new = review_delegation(&request, &caller(), Some("gpt-6-astra"), None).unwrap();
        assert!(!new.options.user_model_choice);
        page.model = "gpt-6-astra".into();
        assert!(review_request_from_popover(&page).user_model_choice);
    }

    #[test]
    fn requests_carry_targets_effort_and_time_limits() {
        let mut review = review_request(ReviewTargetKind::Uncommitted);
        review.checkout = Some(PathBuf::from("/repos/api-wt"));
        review.effort = Some(ReasoningEffort::Xhigh);
        review.time_limit_minutes = Some(45);
        let new = review_delegation(&review, &caller(), None, None).unwrap();
        assert_eq!(
            new.options,
            DelegationRunOptions {
                checkout: Some(PathBuf::from("/repos/api-wt")),
                add_dirs: Vec::new(),
                effort: Some(ReasoningEffort::Xhigh),
                time_limit_minutes: Some(45),
                user_model_choice: false,
            }
        );
        for minutes in [0, MAX_TIME_LIMIT_MINUTES + 1] {
            review.time_limit_minutes = Some(minutes);
            let error = review_delegation(&review, &caller(), None, None).unwrap_err();
            assert!(error.contains("time_limit_minutes"), "{error}");
        }

        let consult = consult_delegation(
            &ConsultDelegationRequest {
                brief: "Which crate owns the parser?".into(),
                checkout: Some(PathBuf::from("/repos/api")),
                effort: Some(ReasoningEffort::Low),
                ..Default::default()
            },
            &caller(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(consult.options.checkout, Some(PathBuf::from("/repos/api")));

        let request = |checkout: Option<&str>, repo: Option<&str>| DelegateTaskRequest {
            title: "Fix".into(),
            objective: "Do it.".into(),
            checkout: checkout.map(PathBuf::from),
            repo: repo.map(PathBuf::from),
            add_dirs: vec![PathBuf::from("/work/out")],
            time_limit_minutes: Some(40),
            ..Default::default()
        };
        let (place, options) = worker_targets(&request(Some("/repos/api-wt"), None)).unwrap();
        assert_eq!(place, WorkerPlace::Checkout(PathBuf::from("/repos/api-wt")));
        assert_eq!(options.checkout, Some(PathBuf::from("/repos/api-wt")));
        assert_eq!(options.add_dirs, vec![PathBuf::from("/work/out")]);
        assert_eq!(options.time_limit_minutes, Some(40));
        let (place, options) = worker_targets(&request(None, Some("/repos/api"))).unwrap();
        assert_eq!(place, WorkerPlace::Repository(PathBuf::from("/repos/api")));
        assert_eq!(options.checkout, None);
        assert_eq!(
            worker_targets(&request(None, None)).unwrap().0,
            WorkerPlace::TabRepository
        );
        assert!(worker_targets(&request(Some("/a"), Some("/b")))
            .unwrap_err()
            .contains("not both"));
        let mut based = request(Some("/repos/api-wt"), None);
        based.base_reference = Some("main".into());
        assert!(worker_targets(&based)
            .unwrap_err()
            .contains("base_reference"));
    }

    #[test]
    fn only_the_workers_own_session_reports_done_or_blocked() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(crate::tasks::TASKS_FILE_NAME)).unwrap();
        store.insert(test_task("task-1", temp.path())).unwrap();
        // No delegation: done is just a task handoff.
        let none = record_handoff(
            &mut store,
            "task-1",
            handoff("Plain task", &[]),
            HandoffStatus::Done,
            None,
            "t1",
        )
        .unwrap();
        assert_eq!(none, None);

        let delegation = Delegation::new_requested(
            worker_delegation(
                &caller(),
                "task-1",
                "Fix",
                "Do it.",
                &worker_choice(),
                &DelegationRunOptions::default(),
            )
            .unwrap(),
            "t2",
        );
        let id = delegation.delegation_id.clone();
        store.insert_delegation(delegation).unwrap();
        store
            .upsert_session("task-1", session("session-1"), "t3")
            .unwrap();
        store
            .attach_worker_session(&id, "session-1", None, "t3")
            .unwrap();

        // The coordinator (or another task tab) cannot complete it, and the
        // refused call stores nothing.
        let error = record_handoff(
            &mut store,
            "task-1",
            handoff("I say it is done", &[]),
            HandoffStatus::Done,
            Some("session-2"),
            "t4",
        )
        .unwrap_err();
        assert!(
            error.contains("only the worker's own GitTerm tab"),
            "{error}"
        );
        assert_eq!(
            store
                .get("task-1")
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Plain task"
        );
        // Progress from anyone only updates the task handoff.
        assert_eq!(
            record_handoff(
                &mut store,
                "task-1",
                handoff("Halfway", &[]),
                HandoffStatus::Progress,
                None,
                "t5",
            )
            .unwrap(),
            None
        );
        assert_eq!(
            store.delegation(&id).unwrap().status,
            DelegationStatus::Running
        );

        let blocked = record_handoff(
            &mut store,
            "task-1",
            handoff("Need a key", &["No staging access"]),
            HandoffStatus::Blocked,
            Some("session-1"),
            "t6",
        )
        .unwrap();
        assert_eq!(blocked.as_deref(), Some(id.as_str()));
        assert_eq!(
            store.delegation(&id).unwrap().status,
            DelegationStatus::Blocked
        );
        let done = record_handoff(
            &mut store,
            "task-1",
            handoff("Export fixed, tests pass", &[]),
            HandoffStatus::Done,
            Some("session-1"),
            "t7",
        )
        .unwrap();
        assert_eq!(done.as_deref(), Some(id.as_str()));
        let completed = store.delegation(&id).unwrap();
        assert_eq!(completed.status, DelegationStatus::Completed);
        assert_eq!(
            completed
                .result
                .as_ref()
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Export fixed, tests pass"
        );
        // A later task handoff does not change the snapshot the parent got.
        record_handoff(
            &mut store,
            "task-1",
            handoff("Tidying up", &[]),
            HandoffStatus::Progress,
            Some("session-1"),
            "t8",
        )
        .unwrap();
        assert_eq!(
            store
                .delegation(&id)
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Export fixed, tests pass"
        );
    }

    #[test]
    fn adapters_keep_explicit_choice_and_all_consult_worker_options() {
        let mut review = review_request(ReviewTargetKind::Uncommitted);
        review.user_model_choice = true;
        assert!(review_delegation(&review, &caller(), Some("configured"), None).is_err());
        review.model = Some("gpt-6-astra".into());
        let new = review_delegation(&review, &caller(), None, Some("previous".into())).unwrap();
        assert!(new.options.user_model_choice);
        assert_eq!(new.previous.as_deref(), Some("previous"));

        let mut consult = ConsultDelegationRequest {
            brief: "  Advice  ".into(),
            model: Some("gpt-6-astra".into()),
            user_model_choice: true,
            checkout: Some(PathBuf::from("/target")),
            effort: Some(ReasoningEffort::High),
            time_limit_minutes: Some(15),
        };
        let new = consult_delegation(&consult, &caller(), None, Some("prior".into())).unwrap();
        assert_eq!(new.brief, "Advice");
        assert_eq!(new.previous.as_deref(), Some("prior"));
        assert_eq!(
            new.child,
            DelegationChild::CodexConsult {
                thread_id: None,
                model: consult.model.clone()
            }
        );
        assert_eq!(
            new.options,
            DelegationRunOptions {
                checkout: consult.checkout.clone(),
                add_dirs: vec![],
                effort: consult.effort,
                time_limit_minutes: consult.time_limit_minutes,
                user_model_choice: true,
            }
        );
        consult.model = None;
        assert!(consult_delegation(&consult, &caller(), Some("configured"), None).is_err());

        let mut worker = DelegateTaskRequest {
            checkout: Some(PathBuf::from("/target")),
            add_dirs: vec![PathBuf::from("/extra")],
            model: Some("gpt-6-astra".into()),
            user_model_choice: true,
            effort: Some(ReasoningEffort::Medium),
            time_limit_minutes: Some(30),
            ..Default::default()
        };
        let (place, options) = worker_targets(&worker).unwrap();
        assert_eq!(place, WorkerPlace::Checkout(PathBuf::from("/target")));
        assert_eq!(
            options,
            DelegationRunOptions {
                checkout: worker.checkout.clone(),
                add_dirs: worker.add_dirs.clone(),
                effort: worker.effort,
                time_limit_minutes: worker.time_limit_minutes,
                user_model_choice: true,
            }
        );
        worker.base_reference = Some("v6".into());
        assert!(worker_targets(&worker).is_err());
        worker.base_reference = None;
        worker.model = None;
        assert!(worker_targets(&worker).is_err());
    }

    #[test]
    fn popover_adapter_preserves_every_target_and_focus() {
        for target in [
            crate::review::ReviewTarget::Uncommitted,
            crate::review::ReviewTarget::Branch { base: "v6".into() },
            crate::review::ReviewTarget::Commit {
                sha: "abcdef1".into(),
            },
        ] {
            let page = crate::review::ReviewRequest {
                target,
                focus: Some("focus".into()),
                reviewer: crate::review::PopoverReviewer::Codex,
                model: "gpt-6-astra".into(),
            };
            let transport = review_request_from_popover(&page);
            let service = gitterm_core::delegations::review_request_from_popover(&page);
            assert_eq!(
                serde_json::to_value(&transport).unwrap(),
                serde_json::to_value(service).unwrap()
            );
            assert!(transport.user_model_choice);
            assert_eq!(transport.focus.as_deref(), Some("focus"));
        }
    }

    #[test]
    fn blocked_adapter_rejects_mismatched_host_before_any_write() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(crate::tasks::TASKS_FILE_NAME)).unwrap();
        store.insert(test_task("task-1", temp.path())).unwrap();
        let delegation = Delegation::new_requested(
            worker_delegation(
                &caller(),
                "task-1",
                "Fix",
                "Do it.",
                &worker_choice(),
                &DelegationRunOptions::default(),
            )
            .unwrap(),
            "t0",
        );
        let id = delegation.delegation_id.clone();
        store.insert_delegation(delegation).unwrap();
        store
            .upsert_session("task-1", session("session-1"), "t1")
            .unwrap();
        store
            .attach_worker_session(&id, "session-1", None, "t1")
            .unwrap();
        let disk = std::fs::read(store.path()).unwrap();
        for hosted in [None, Some("session-2")] {
            assert!(record_handoff(
                &mut store,
                "task-1",
                handoff("Refused", &[]),
                HandoffStatus::Blocked,
                hosted,
                "t2"
            )
            .is_err());
            assert!(store.get("task-1").unwrap().handoff.is_none());
            assert_eq!(
                store.delegation(&id).unwrap().status,
                DelegationStatus::Running
            );
            assert_eq!(std::fs::read(store.path()).unwrap(), disk);
        }
    }
}
