pub(crate) use gitterm_core::tasks::store::replace_file_atomically;
pub use gitterm_core::tasks::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn activity_snapshot_cannot_precede_session_start() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("activity")).unwrap();
        let start = "2026-11-05T10:00:45.123456+00:00";
        store
            .upsert_session(
                "activity",
                TaskSessionRecord {
                    task_session_id: "session".into(),
                    label: "Codex".into(),
                    harness: None,
                    conversation: None,
                    objective_delivery: ObjectiveDeliveryState::Pending,
                    launch_identity: None,
                    created_at: start.into(),
                    updated_at: start.into(),
                },
                start,
            )
            .unwrap();
        assert_eq!(
            store
                .get("activity")
                .unwrap()
                .progress
                .as_ref()
                .unwrap()
                .last_activity_at
                .as_deref(),
            Some(start)
        );
        store
            .update_progress(
                "activity",
                TaskProgress {
                    last_activity_at: Some("2026-11-05T10:00:00Z".into()),
                    ..Default::default()
                },
                None,
                start,
            )
            .unwrap();
        assert_eq!(
            store
                .get("activity")
                .unwrap()
                .progress
                .as_ref()
                .unwrap()
                .last_activity_at
                .as_deref(),
            Some(start)
        );
        let later = "2026-11-05T10:01:00+00:00";
        assert_eq!(
            clamp_activity_to_session_start(Some(later), [start].into_iter()).as_deref(),
            Some(later)
        );
    }

    #[test]
    fn raised_attention_keeps_since_while_reason_is_unchanged() {
        let first = TaskAttention::raised(
            TaskAttentionReason::ExecutionFailed,
            &TaskAttention::default(),
            "2026-08-29T10:00:00Z",
        );
        assert_eq!(first.since.as_deref(), Some("2026-08-29T10:00:00Z"));
        assert!(first.unread);

        let again = TaskAttention::raised(
            TaskAttentionReason::ExecutionFailed,
            &first,
            "2026-08-29T10:05:00Z",
        );
        assert_eq!(again.since.as_deref(), Some("2026-08-29T10:00:00Z"));

        let changed = TaskAttention::raised(
            TaskAttentionReason::CompletedUnread,
            &again,
            "2026-08-29T10:10:00Z",
        );
        assert_eq!(changed.since.as_deref(), Some("2026-08-29T10:10:00Z"));
    }

    include!("tasks/test_support.rs");

    fn sample_delegation(id: &str, session_uid: &str, timestamp: &str) -> Delegation {
        let mut delegation = Delegation::new_requested(
            NewDelegation {
                kind: DelegationKind::Review,
                parent: DelegationParent {
                    session_uid: session_uid.to_string(),
                    chat_session_id: Some("claude-session-1".to_string()),
                    workspace: "GitTerm V6".to_string(),
                    cwd: PathBuf::from("/repo with spaces/gitterm-v6"),
                },
                child: DelegationChild::CodexReview {
                    thread_id: None,
                    model: None,
                },
                brief: "Review the uncommitted changes".to_string(),
                target: Some(ReviewTarget {
                    mode: ReviewTargetMode::Base {
                        reference: "v6".to_string(),
                    },
                    focus: Some("error handling".to_string()),
                }),
                previous: None,
                options: Default::default(),
            },
            timestamp,
        );
        delegation.delegation_id = id.to_string();
        delegation
    }

    fn sample_review_result() -> DelegationResult {
        DelegationResult {
            handoff: None,
            findings: Some(ReviewFindings {
                verdict: ReviewVerdict::NeedsChanges,
                summary: "One regression".to_string(),
                findings: vec![ReviewFinding {
                    id: "F1".to_string(),
                    severity: ReviewSeverity::P1,
                    title: "Return None when no user matches".to_string(),
                    body: "matches[0] raises IndexError".to_string(),
                    file: Some(PathBuf::from("stats.py")),
                    line_start: Some(18),
                    line_end: Some(18),
                    confidence: Some(0.75),
                }],
                reviewer: Reviewer {
                    harness: "codex".to_string(),
                    model: None,
                    conversation_id: Some("thread-1".to_string()),
                },
                structured: true,
            }),
            reviewed: Some(ReviewedState {
                head: "0123456789abcdef".to_string(),
                dirty: true,
                target_description: "changes against 'v6'".to_string(),
            }),
        }
    }

    #[test]
    fn failed_delivery_round_trips_without_an_incompatible_result_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        for id in ["failed", "limited"] {
            store
                .insert_delegation(sample_delegation(id, "parent", "t1"))
                .unwrap();
        }
        store
            .set_delegation_status(
                "failed",
                DelegationStatus::Failed {
                    message: "runner exited".into(),
                },
                "t2",
            )
            .unwrap();
        store
            .stop_delegation_at_time_limit("limited", "stopped at the time limit".into(), "t2")
            .unwrap();
        store
            .mark_delegations_delivered(&["failed".into(), "limited".into()], "t3")
            .unwrap();
        let mut store = TaskStore::load(&path).unwrap();
        for id in ["failed", "limited"] {
            let d = store.delegation(id).unwrap();
            assert!(d.delivered);
            assert!(d.delivered_at.is_none()); // Older builds accept this record.
            assert!(store.mark_delegation_delivered(id, "t4").is_err());
        }
    }

    #[test]
    fn a_delivery_batch_is_not_partially_stamped_if_one_record_is_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        for id in ["finished", "running"] {
            store
                .insert_delegation(sample_delegation(id, "parent", "t1"))
                .unwrap();
        }
        store
            .complete_delegation("finished", sample_review_result(), "t2")
            .unwrap();
        assert!(store
            .mark_delegations_delivered(&["finished".into(), "running".into()], "t3")
            .is_err());
        assert!(!store.delegation("finished").unwrap().is_delivered());
    }

    #[test]
    fn document_without_delegations_loads_and_is_written_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let old = serde_json::json!({
            "schema_version": TASK_STORE_SCHEMA_VERSION,
            "tasks": [sample_task("task-1")],
        });
        fs::write(&path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();

        let mut store = TaskStore::load(&path).unwrap();
        assert_eq!(store.tasks().len(), 1);
        assert!(store.delegations().is_empty());

        store.archive("task-1", "2026-10-08T10:00:00Z").unwrap();
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(written.get("delegations").is_none());
    }

    #[test]
    fn delegations_round_trip_through_the_store_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let review = sample_delegation("review-1", "tab-a", "2026-10-08T10:00:00Z");
        store.insert_delegation(review).unwrap();
        store
            .set_delegation_status(
                "review-1",
                DelegationStatus::Running,
                "2026-10-08T10:00:01Z",
            )
            .unwrap();
        store
            .complete_delegation("review-1", sample_review_result(), "2026-10-08T10:01:00Z")
            .unwrap();
        store
            .mark_delegation_delivered("review-1", "2026-10-08T10:02:00Z")
            .unwrap();

        let mut worker = sample_delegation("implement-1", "tab-a", "2026-10-08T10:03:00Z");
        worker.kind = DelegationKind::Implement;
        worker.target = None;
        worker.previous = Some("review-1".to_string());
        worker.child = DelegationChild::TaskSession {
            task_id: "task-1".to_string(),
            task_session_id: Some("session-1".to_string()),
            conversation: Some(HarnessConversationRef {
                backend: HarnessConversationBackend::Claude,
                session_id: "conversation-1".to_string(),
            }),
            preset_name: None,
            model: None,
            role: None,
        };
        store.insert_delegation(worker).unwrap();
        store
            .set_delegation_status(
                "implement-1",
                DelegationStatus::Failed {
                    message: "worker exited".to_string(),
                },
                "2026-10-08T10:04:00Z",
            )
            .unwrap();

        let reloaded = TaskStore::load(&path).unwrap();
        assert_eq!(reloaded.delegations(), store.delegations());
        let review = reloaded.delegation("review-1").unwrap();
        assert_eq!(review.status, DelegationStatus::Completed);
        assert_eq!(review.result, Some(sample_review_result()));
        assert_eq!(review.delivered_at.as_deref(), Some("2026-10-08T10:02:00Z"));
        assert_eq!(review.updated_at, "2026-10-08T10:02:00Z");
        assert_eq!(review.created_at, "2026-10-08T10:00:00Z");

        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let raw_review = &raw["delegations"][0];
        assert_eq!(raw_review["kind"], "review");
        assert_eq!(raw_review["child"]["runner"], "codex_review");
        assert_eq!(raw_review["status"]["state"], "completed");
        assert_eq!(raw_review["target"]["mode"]["type"], "base");
        assert_eq!(raw_review["target"]["mode"]["reference"], "v6");
        assert_eq!(raw_review["result"]["findings"]["verdict"], "needs_changes");
        assert_eq!(
            raw_review["result"]["findings"]["findings"][0]["severity"],
            "P1"
        );
        let raw_worker = &raw["delegations"][1];
        assert_eq!(raw_worker["child"]["runner"], "task_session");
        assert_eq!(raw_worker["status"]["state"], "failed");
        assert_eq!(raw_worker["status"]["message"], "worker exited");
    }

    #[test]
    fn new_delegations_get_uuid_ids_and_start_requested() {
        let mut first = sample_delegation("unused", "tab-a", "2026-10-08T10:00:00Z");
        first.delegation_id = Delegation::new_requested(
            NewDelegation {
                kind: first.kind,
                parent: first.parent.clone(),
                child: first.child.clone(),
                brief: first.brief.clone(),
                target: first.target.clone(),
                previous: None,
                options: Default::default(),
            },
            "2026-10-08T10:00:00Z",
        )
        .delegation_id;
        assert!(uuid::Uuid::parse_str(&first.delegation_id).is_ok());
        assert_eq!(first.status, DelegationStatus::Requested);
        assert_eq!(first.created_at, first.updated_at);
    }

    #[test]
    fn insert_delegation_rejects_duplicates_and_invalid_records() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t1"))
            .unwrap();
        assert!(store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t2"))
            .is_err());

        let mut completed = sample_delegation("review-2", "tab-a", "t2");
        completed.status = DelegationStatus::Completed;
        completed.result = Some(sample_review_result());
        assert!(store.insert_delegation(completed).is_err());

        let mut unknown_previous = sample_delegation("review-3", "tab-a", "t3");
        unknown_previous.previous = Some("missing".to_string());
        assert!(store.insert_delegation(unknown_previous).is_err());

        let mut unknown_task = sample_delegation("implement-1", "tab-a", "t4");
        unknown_task.kind = DelegationKind::Implement;
        unknown_task.target = None;
        unknown_task.child = DelegationChild::TaskSession {
            task_id: "task-9".to_string(),
            task_session_id: None,
            conversation: None,
            preset_name: None,
            model: None,
            role: None,
        };
        assert!(store.insert_delegation(unknown_task).is_err());

        let mut mismatched = sample_delegation("review-4", "tab-a", "t5");
        mismatched.kind = DelegationKind::Implement;
        let error = store.insert_delegation(mismatched).unwrap_err();
        assert_eq!(error.operation(), "validate");

        let mut empty_parent = sample_delegation("review-5", "tab-a", "t6");
        empty_parent.parent.session_uid = " ".to_string();
        assert!(store.insert_delegation(empty_parent).is_err());

        assert_eq!(store.delegations().len(), 1);
    }

    #[test]
    fn completing_a_delegation_twice_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t1"))
            .unwrap();
        store
            .complete_delegation("review-1", sample_review_result(), "t2")
            .unwrap();

        let mut other = sample_review_result();
        other.findings.as_mut().unwrap().summary = "Different".to_string();
        let error = store
            .complete_delegation("review-1", other, "t3")
            .unwrap_err();
        assert!(error.to_string().contains("already completed"));
        let reloaded = TaskStore::load(&path).unwrap();
        let review = reloaded.delegation("review-1").unwrap();
        assert_eq!(review.result, Some(sample_review_result()));
        assert_eq!(review.updated_at, "t2");

        store
            .insert_delegation(sample_delegation("review-2", "tab-a", "t4"))
            .unwrap();
        store.cancel_delegation("review-2", "t5").unwrap();
        assert!(store
            .complete_delegation("review-2", sample_review_result(), "t6")
            .is_err());

        store
            .insert_delegation(sample_delegation("review-3", "tab-a", "t7"))
            .unwrap();
        store
            .set_delegation_status(
                "review-3",
                DelegationStatus::Failed {
                    message: "codex is not logged in".to_string(),
                },
                "t8",
            )
            .unwrap();
        assert!(store
            .complete_delegation("review-3", sample_review_result(), "t9")
            .is_err());
        assert!(store
            .complete_delegation("missing", sample_review_result(), "t9")
            .is_err());
    }

    #[test]
    fn completion_requires_the_result_the_kind_promises() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t1"))
            .unwrap();
        let error = store
            .complete_delegation("review-1", DelegationResult::default(), "t2")
            .unwrap_err();
        assert!(error.to_string().contains("without findings"));
        assert_eq!(
            store.delegation("review-1").unwrap().status,
            DelegationStatus::Requested
        );
    }

    #[test]
    fn delegation_status_transitions_are_checked() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t1"))
            .unwrap();
        assert!(store
            .set_delegation_status("review-1", DelegationStatus::Completed, "t2")
            .is_err());
        assert!(store
            .set_delegation_status("review-1", DelegationStatus::Requested, "t2")
            .is_err());
        assert!(store
            .set_delegation_status(
                "review-1",
                DelegationStatus::Failed {
                    message: " ".to_string()
                },
                "t2"
            )
            .is_err());
        store
            .set_delegation_status("review-1", DelegationStatus::Running, "t2")
            .unwrap();
        store.cancel_delegation("review-1", "t3").unwrap();
        assert!(store.cancel_delegation("review-1", "t4").is_err());
        assert!(store
            .set_delegation_status("review-1", DelegationStatus::Running, "t4")
            .is_err());
        assert!(store.mark_delegation_delivered("review-1", "t4").is_err());
    }

    #[test]
    fn delivering_twice_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t1"))
            .unwrap();
        assert!(store.mark_delegation_delivered("review-1", "t2").is_err());
        store
            .complete_delegation("review-1", sample_review_result(), "t2")
            .unwrap();
        store.mark_delegation_delivered("review-1", "t3").unwrap();
        let error = store
            .mark_delegation_delivered("review-1", "t4")
            .unwrap_err();
        assert!(error.to_string().contains("already delivered at t3"));
        assert_eq!(
            store
                .delegation("review-1")
                .unwrap()
                .delivered_at
                .as_deref(),
            Some("t3")
        );
    }

    #[test]
    fn delegations_for_parent_lists_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        for (id, parent, timestamp) in [
            ("d1", "tab-a", "2026-10-08T10:00:00Z"),
            ("d2", "tab-b", "2026-10-08T10:01:00Z"),
            ("d3", "tab-a", "2026-10-08T10:02:00Z"),
            ("d4", "tab-a", "2026-10-08T10:03:00Z"),
        ] {
            store
                .insert_delegation(sample_delegation(id, parent, timestamp))
                .unwrap();
        }
        // Updating an older delegation does not reorder the list.
        store
            .set_delegation_status("d1", DelegationStatus::Running, "2026-10-08T10:04:00Z")
            .unwrap();
        let ids = |uid: &str| {
            store
                .delegations_for_parent(uid)
                .into_iter()
                .map(|delegation| delegation.delegation_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("tab-a"), ["d4", "d3", "d1"]);
        assert_eq!(ids("tab-b"), ["d2"]);
        assert!(ids("tab-c").is_empty());
    }

    #[test]
    fn startup_reconciles_requested_and_running_delegations_to_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        for id in ["requested", "running", "completed", "cancelled"] {
            store
                .insert_delegation(sample_delegation(id, "tab-a", "t1"))
                .unwrap();
        }
        store
            .set_delegation_status("running", DelegationStatus::Running, "t2")
            .unwrap();
        store
            .complete_delegation("completed", sample_review_result(), "t2")
            .unwrap();
        store.cancel_delegation("cancelled", "t2").unwrap();

        // No task was active, so the task count is zero, but the
        // delegations are still reconciled and written.
        assert_eq!(store.reconcile_after_restart("t9").unwrap(), 0);

        let mut reloaded = TaskStore::load(&path).unwrap();
        let state = |store: &TaskStore, id: &str| {
            let delegation = store.delegation(id).unwrap();
            (delegation.status.clone(), delegation.updated_at.clone())
        };
        assert_eq!(
            state(&reloaded, "requested"),
            (DelegationStatus::Interrupted, "t9".to_string())
        );
        assert_eq!(
            state(&reloaded, "running"),
            (DelegationStatus::Interrupted, "t9".to_string())
        );
        assert_eq!(
            state(&reloaded, "completed"),
            (DelegationStatus::Completed, "t2".to_string())
        );
        assert_eq!(
            state(&reloaded, "cancelled"),
            (DelegationStatus::Cancelled, "t2".to_string())
        );
        let interrupted = reloaded
            .interrupted_delegations()
            .into_iter()
            .map(|delegation| delegation.delegation_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(interrupted, ["running", "requested"]);

        // A resumed worker may still report back after an interruption.
        reloaded
            .complete_delegation("running", sample_review_result(), "t10")
            .unwrap();
        assert_eq!(reloaded.reconcile_after_restart("t11").unwrap(), 0);
        assert_eq!(
            state(&reloaded, "requested"),
            (DelegationStatus::Interrupted, "t9".to_string())
        );
    }

    fn worker_delegation(id: &str, task_id: &str) -> Delegation {
        let mut worker = sample_delegation(id, "tab-a", "t1");
        worker.kind = DelegationKind::Implement;
        worker.target = None;
        worker.child = DelegationChild::TaskSession {
            task_id: task_id.to_string(),
            task_session_id: None,
            conversation: None,
            preset_name: Some("Codex".to_string()),
            model: Some("gpt-6-luna".to_string()),
            role: Some(crate::workers::WorkerRole::Scoped),
        };
        worker
    }

    fn worker_result(summary: &str, blockers: &[&str]) -> DelegationResult {
        DelegationResult {
            handoff: Some(TaskHandoff {
                summary: summary.to_string(),
                decisions: Vec::new(),
                next_steps: Vec::new(),
                blockers: blockers.iter().map(|item| item.to_string()).collect(),
                updated_by_session_id: Some("session-1".to_string()),
                updated_at: "t5".to_string(),
            }),
            findings: None,
            reviewed: None,
        }
    }

    fn add_session(store: &mut TaskStore, task_id: &str, session_id: &str) {
        store
            .upsert_session(
                task_id,
                TaskSessionRecord {
                    task_session_id: session_id.to_string(),
                    label: "Codex".to_string(),
                    harness: None,
                    conversation: None,
                    objective_delivery: ObjectiveDeliveryState::Delivered,
                    launch_identity: None,
                    created_at: "t2".to_string(),
                    updated_at: "t2".to_string(),
                },
                "t2",
            )
            .unwrap();
    }

    #[test]
    fn a_new_worker_supersedes_a_blocked_worker_atomically_and_keeps_its_report() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        store
            .insert_delegation(worker_delegation("old", "task-1"))
            .unwrap();
        add_session(&mut store, "task-1", "old-session");
        store
            .attach_worker_session("old", "old-session", None, "t2")
            .unwrap();
        let result = worker_result("Needs a decision", &["Human decision"]);
        store.block_delegation("old", result.clone(), "t3").unwrap();
        store.mark_delegation_delivered("old", "t4").unwrap();
        assert_eq!(store.open_worker_delegations("task-1").len(), 1);
        store
            .insert_delegation(worker_delegation("replacement", "task-1"))
            .unwrap();
        let loaded = TaskStore::load(&path).unwrap();
        let old = loaded.delegation("old").unwrap();
        assert!(old.status.is_final());
        assert_eq!(old.superseded_by.as_deref(), Some("replacement"));
        assert_eq!(old.result.as_ref(), Some(&result));
        assert!(old.is_delivered());
        assert!(loaded.worker_session_is_final("task-1", Some("old-session")));
        assert_eq!(
            loaded
                .open_worker_delegations("task-1")
                .iter()
                .map(|d| d.delegation_id.as_str())
                .collect::<Vec<_>>(),
            ["replacement"]
        );
        assert!(store.complete_delegation("old", result, "t5").is_err());
    }

    #[test]
    fn worker_deadlines_survive_restore_and_expire_at_the_original_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let mut worker = worker_delegation("timed", "task-1");
        worker.options.time_limit_minutes = Some(1);
        store.insert_delegation(worker).unwrap();
        // A queued delegation has no timer until it actually launches.
        assert_eq!(store.worker_time_limit("timed").unwrap(), None);
        add_session(&mut store, "task-1", "session-1");
        let start = "2026-10-20T10:00:00Z";
        let deadline = chrono::DateTime::parse_from_rfc3339("2026-10-20T10:01:00Z")
            .unwrap()
            .timestamp_millis();
        store
            .attach_worker_session("timed", "session-1", None, start)
            .unwrap();
        store
            .attach_worker_session("timed", "session-1", None, "2026-10-20T10:00:30Z")
            .unwrap();

        let mut restored = TaskStore::load(&path).unwrap();
        restored
            .reconcile_after_restart("2026-10-20T10:00:40Z")
            .unwrap();
        assert_eq!(
            restored
                .delegation("timed")
                .unwrap()
                .time_limit_deadline_unix_ms,
            Some(deadline)
        );
        let timer = restored.worker_time_limit("timed").unwrap().unwrap();
        assert_eq!(timer.session_id, "session-1");
        assert_eq!(
            timer.remaining(deadline - 20_000),
            std::time::Duration::from_secs(20)
        );
        assert_eq!(timer.remaining(deadline), std::time::Duration::ZERO);
        assert_eq!(
            timer.remaining(deadline + 20_000),
            std::time::Duration::ZERO
        );
        restored
            .stop_delegation_at_time_limit(
                "timed",
                "Stopped at the time limit".into(),
                "2026-10-20T10:01:20Z",
            )
            .unwrap();
        let stopped = TaskStore::load(&path).unwrap();
        assert_eq!(
            stopped
                .delegation("timed")
                .unwrap()
                .time_limit_reached_at
                .as_deref(),
            Some("2026-10-20T10:01:20Z")
        );
        assert_eq!(stopped.worker_time_limit("timed").unwrap(), None);

        // A different session gets a fresh limit; its identity invalidates
        // any pending event from the original session.
        add_session(&mut store, "task-1", "session-2");
        store
            .attach_worker_session("timed", "session-2", None, "2026-10-20T11:00:00Z")
            .unwrap();
        let relaunched = store.worker_time_limit("timed").unwrap().unwrap();
        assert_eq!(relaunched.session_id, "session-2");
        assert_eq!(relaunched.deadline_unix_ms, deadline + 3_600_000);
        assert_ne!(relaunched, timer);
    }

    #[test]
    fn legacy_worker_deadlines_use_the_linked_session_start_and_refuse_bad_starts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        let mut task = sample_task("task-1");
        task.sessions.push(TaskSessionRecord {
            task_session_id: "session-1".into(),
            label: "Codex".into(),
            harness: None,
            conversation: None,
            objective_delivery: ObjectiveDeliveryState::Delivered,
            launch_identity: None,
            created_at: "2026-10-20T10:00:00Z".into(),
            updated_at: "2026-10-20T10:00:30Z".into(),
        });
        store.insert(task).unwrap();
        let mut worker = worker_delegation("legacy", "task-1");
        worker.options.time_limit_minutes = Some(1);
        if let DelegationChild::TaskSession {
            task_session_id, ..
        } = &mut worker.child
        {
            *task_session_id = Some("session-1".into());
        }
        store.insert_delegation(worker).unwrap();
        let restored = TaskStore::load(&path).unwrap();
        assert_eq!(
            restored
                .delegation("legacy")
                .unwrap()
                .time_limit_deadline_unix_ms,
            None
        );
        let timer = restored.worker_time_limit("legacy").unwrap().unwrap();
        assert_eq!(
            timer.deadline_unix_ms,
            chrono::DateTime::parse_from_rfc3339("2026-10-20T10:01:00Z")
                .unwrap()
                .timestamp_millis()
        );
        store
            .attach_worker_session("legacy", "session-1", None, "2026-10-20T11:00:00Z")
            .unwrap();
        assert_eq!(store.worker_time_limit("legacy").unwrap().unwrap(), timer);

        let mut bad = worker_delegation("bad", "task-1");
        bad.options.time_limit_minutes = Some(1);
        store.insert_delegation(bad).unwrap();
        let error = store
            .attach_worker_session("bad", "session-1", None, "invalid")
            .unwrap_err();
        assert!(error.to_string().contains("invalid worker start timestamp"));
        let bad = store.delegation("bad").unwrap();
        assert_eq!(bad.status, DelegationStatus::Requested);
        assert_eq!(bad.time_limit_deadline_unix_ms, None);
        assert!(matches!(
            &bad.child,
            DelegationChild::TaskSession {
                task_session_id: None,
                ..
            }
        ));

        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["delegations"][0]
            .as_object_mut()
            .unwrap()
            .remove("time_limit_deadline_unix_ms");
        document["tasks"][0]["sessions"][0]["created_at"] = serde_json::json!("invalid");
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let corrupted = TaskStore::load(&path).unwrap();
        assert!(corrupted
            .worker_time_limit("legacy")
            .unwrap_err()
            .contains("invalid worker start timestamp"));
    }

    #[test]
    fn a_new_worker_report_supersedes_an_unpersisted_delivery_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        store
            .insert_delegation(worker_delegation("w1", "task-1"))
            .unwrap();
        store
            .block_delegation("w1", worker_result("Stuck", &["Need access"]), "t2")
            .unwrap();
        let saved = temp.path().join("saved.json");
        fs::rename(&path, &saved).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.mark_delegation_delivered("w1", "t3").is_err());
        assert!(store.delegation("w1").unwrap().is_delivered());
        fs::remove_dir(&path).unwrap();
        fs::rename(&saved, &path).unwrap();

        // The ordinary commit for a later report supersedes the pending
        // receipt; retries cannot stamp it with the earlier acknowledgement.
        store
            .block_delegation(
                "w1",
                worker_result("Still stuck", &["Need another input"]),
                "t5",
            )
            .unwrap();
        store.retry_delivery_acknowledgements().unwrap();
        let reloaded = TaskStore::load(&path).unwrap();
        assert!(!reloaded.delegation("w1").unwrap().is_delivered());
        assert_eq!(reloaded.delegation("w1").unwrap().delivered_at, None);
        store
            .complete_delegation("w1", worker_result("Done", &[]), "t6")
            .unwrap();
        store.mark_delegation_delivered("w1", "t7").unwrap();
        assert!(TaskStore::load(&path)
            .unwrap()
            .delegation("w1")
            .unwrap()
            .is_delivered());
    }

    #[test]
    fn a_worker_attaches_its_session_blocks_and_then_completes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        store
            .insert_delegation(worker_delegation("w1", "task-1"))
            .unwrap();
        assert_eq!(store.open_worker_delegations("task-1").len(), 1);
        assert!(store.open_worker_delegations("task-2").is_empty());

        // Only a session the task knows can be attached.
        assert!(store
            .attach_worker_session("w1", "session-1", None, "t2")
            .is_err());
        add_session(&mut store, "task-1", "session-1");
        store
            .attach_worker_session("w1", "session-1", None, "t3")
            .unwrap();
        let worker = store.delegation("w1").unwrap();
        assert_eq!(worker.status, DelegationStatus::Running);
        assert!(matches!(
            &worker.child,
            DelegationChild::TaskSession { task_session_id: Some(id), model: Some(model), .. }
                if id == "session-1" && model == "gpt-6-luna"
        ));

        store
            .block_delegation("w1", worker_result("Stuck", &["No staging access"]), "t4")
            .unwrap();
        store.mark_delegation_delivered("w1", "t5").unwrap();
        store.dismiss_delegation("w1", "t6").unwrap();
        // Blocked is still open: the worker may report again.
        assert_eq!(store.open_worker_delegations("task-1").len(), 1);
        store
            .complete_delegation("w1", worker_result("Done, tests pass", &[]), "t7")
            .unwrap();
        let reloaded = TaskStore::load(&path).unwrap();
        let worker = reloaded.delegation("w1").unwrap();
        assert_eq!(worker.status, DelegationStatus::Completed);
        assert_eq!(
            worker
                .result
                .as_ref()
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Done, tests pass"
        );
        // The done report needs its own delivery in automatic or manual mode.
        assert_eq!(worker.delivered_at, None);
        assert!(!worker.is_delivered());
        assert_eq!(worker.dismissed_at, None);
        assert!(reloaded.open_worker_delegations("task-1").is_empty());

        // A finished worker cannot report again, and reviews never block.
        let mut store = reloaded;
        assert!(store
            .complete_delegation("w1", worker_result("again", &[]), "t8")
            .is_err());
        store
            .insert_delegation(sample_delegation("r1", "tab-a", "t8"))
            .unwrap();
        let error = store
            .block_delegation("r1", worker_result("x", &[]), "t9")
            .unwrap_err();
        assert!(error.to_string().contains("only workers"), "{error}");
        // A blocked or completed worker needs a handoff.
        let error = store
            .block_delegation("w1", DelegationResult::default(), "t9")
            .unwrap_err();
        assert!(error.to_string().contains("already completed"), "{error}");
    }

    #[test]
    fn a_blocked_worker_without_a_handoff_is_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        store
            .insert_delegation(worker_delegation("w1", "task-1"))
            .unwrap();
        let error = store
            .block_delegation("w1", DelegationResult::default(), "t2")
            .unwrap_err();
        assert_eq!(error.operation(), "validate");
        assert_eq!(
            store.delegation("w1").unwrap().status,
            DelegationStatus::Requested
        );
    }

    #[test]
    fn restart_keeps_workers_whose_task_exists_running() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        add_session(&mut store, "task-1", "session-1");
        store
            .insert_delegation(worker_delegation("running", "task-1"))
            .unwrap();
        store
            .attach_worker_session("running", "session-1", None, "t3")
            .unwrap();
        // Still queued behind the concurrency limit: no session yet.
        store
            .insert_delegation(worker_delegation("queued", "task-1"))
            .unwrap();
        store
            .insert_delegation(sample_delegation("review", "tab-a", "t3"))
            .unwrap();
        store.reconcile_after_restart("t9").unwrap();

        let reloaded = TaskStore::load(&path).unwrap();
        let status = |id: &str| reloaded.delegation(id).unwrap().status.clone();
        assert_eq!(status("running"), DelegationStatus::Running);
        assert_eq!(status("queued"), DelegationStatus::Requested);
        assert_eq!(status("review"), DelegationStatus::Interrupted);
    }

    fn active_attempt(attempt_id: &str, executor: ExecutorTarget) -> TaskExecutionAttempt {
        TaskExecutionAttempt {
            attempt_id: attempt_id.to_string(),
            executor,
            harness: Some(HarnessSelection {
                kind: HarnessKind::TerminalPreset {
                    preset_name: "Codex".to_string(),
                },
                model: Some("gpt-5.6".to_string()),
            }),
            state: AttemptState::Running,
            started_at: "2026-08-19T08:01:00Z".to_string(),
            ended_at: None,
            session_ref: Some("session-1".to_string()),
            failure: None,
        }
    }

    fn make_ready(store: &mut TaskStore, task_id: &str) {
        let mut task = store.get(task_id).unwrap().clone();
        task.lifecycle = TaskLifecycle::Preparing;
        task.updated_at = "2026-08-19T08:00:30Z".to_string();
        store.replace(task).unwrap();

        let mut task = store.get(task_id).unwrap().clone();
        task.lifecycle = TaskLifecycle::Ready;
        task.updated_at = "2026-08-19T08:00:45Z".to_string();
        store.replace(task).unwrap();
    }

    #[test]
    fn marking_a_worktree_missing_keeps_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(&temp.path().join(TASKS_FILE_NAME)).unwrap();
        let mut task = sample_task("task-1");
        task.worktree = TaskWorktree {
            state: TaskWorktreeState::Ready,
            path: Some(PathBuf::from("/tmp/worktrees/task-1")),
        };
        store.insert(task).unwrap();

        store
            .mark_worktree_missing("task-1", "2026-08-28T00:00:00Z")
            .unwrap();

        let task = store.get("task-1").unwrap();
        assert_eq!(task.worktree.state, TaskWorktreeState::Missing);
        assert_eq!(
            task.worktree.path.as_deref(),
            Some(std::path::Path::new("/tmp/worktrees/task-1"))
        );
        assert_eq!(task.updated_at, "2026-08-28T00:00:00Z");
        assert!(store.mark_worktree_missing("task-9", "later").is_err());
    }

    #[test]
    fn missing_store_loads_empty_without_creating_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let store = TaskStore::load(&path).unwrap();
        assert!(store.tasks().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn older_task_records_default_to_empty_session_history() {
        let mut value = serde_json::to_value(sample_task("task-1")).unwrap();
        value.as_object_mut().unwrap().remove("sessions");

        let decoded: TaskRecord = serde_json::from_value(value).unwrap();

        assert!(decoded.sessions.is_empty());
    }

    #[test]
    fn older_records_default_creator_and_objective_delivery_metadata() {
        let mut task = sample_task("task-1");
        task.created_by = Some(TaskCreator {
            kind: TaskCreatorKind::Coordinator,
            session_id: Some("coordinator-1".to_string()),
            harness_label: Some("Codex".to_string()),
            created_at: "2026-08-19T08:00:00Z".to_string(),
        });
        task.sessions.push(TaskSessionRecord {
            task_session_id: "task-session-1".to_string(),
            label: "Claude implementation".to_string(),
            harness: None,
            conversation: None,
            objective_delivery: ObjectiveDeliveryState::Delivered,
            launch_identity: None,
            created_at: "2026-08-19T08:01:00Z".to_string(),
            updated_at: "2026-08-19T08:01:00Z".to_string(),
        });
        let mut value = serde_json::to_value(task).unwrap();
        let task = value.as_object_mut().unwrap();
        task.remove("created_by");
        task["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("objective_delivery");

        let decoded: TaskRecord = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.created_by, None);
        assert_eq!(
            decoded.sessions[0].objective_delivery,
            ObjectiveDeliveryState::Unknown
        );
    }

    #[test]
    fn task_session_history_round_trips_native_conversation_references() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let session = TaskSessionRecord {
            task_session_id: "task-session-1".to_string(),
            label: "Codex implementation".to_string(),
            harness: Some(HarnessSelection {
                kind: HarnessKind::TerminalPreset {
                    preset_name: "Codex".to_string(),
                },
                model: Some("gpt-5.6".to_string()),
            }),
            conversation: Some(HarnessConversationRef {
                backend: HarnessConversationBackend::Codex,
                session_id: "codex-chat-1".to_string(),
            }),
            objective_delivery: ObjectiveDeliveryState::Delivered,
            launch_identity: None,
            created_at: "2026-08-19T08:01:00Z".to_string(),
            updated_at: "2026-08-19T08:01:00Z".to_string(),
        };

        store
            .upsert_session("task-1", session.clone(), "2026-08-19T08:01:00Z")
            .unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        assert_eq!(reloaded.get("task-1").unwrap().sessions, vec![session]);
        assert_eq!(
            reloaded
                .get("task-1")
                .unwrap()
                .harness
                .as_ref()
                .map(|harness| &harness.kind),
            Some(&HarnessKind::TerminalPreset {
                preset_name: "Codex".to_string()
            })
        );
    }

    #[test]
    fn recorded_delivery_survives_reload_and_unchanged_writes_are_no_ops() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let delivery = DeliveryState {
            commit_sha: Some("bafc5f5757b0265cb89a50a64fd539089b4265ab".to_string()),
            pr_url: Some("https://github.com/o/r/pull/30".to_string()),
            pr_number: Some(30),
        };

        store
            .record_delivery("task-1", delivery.clone(), "2026-08-22T10:00:00Z")
            .unwrap();
        // Re-recording identical state must not churn updated_at — discovery
        // runs on every overview visit and updated_at drives rail ordering.
        store
            .record_delivery("task-1", delivery.clone(), "2026-08-22T11:00:00Z")
            .unwrap();

        let reloaded = TaskStore::load(&path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.delivery, delivery);
        assert_eq!(task.updated_at, "2026-08-22T10:00:00Z");

        let error = store
            .record_delivery("task-missing", delivery, "2026-08-22T12:00:00Z")
            .unwrap_err();
        assert!(error.to_string().contains("task-missing"));
    }

    #[test]
    fn clearing_the_worktree_preserves_conversation_references() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        store
            .begin_worktree_preparation("task-1", "2026-08-22T08:59:00Z")
            .unwrap();
        store
            .complete_worktree_preparation(
                "task-1",
                CompletedWorktreePreparation {
                    repository: RepositoryIdentity {
                        common_dir: PathBuf::from("/repo with spaces/gitterm-v6/.git"),
                        remote_url: None,
                    },
                    base: GitBase {
                        reference: "v6".to_string(),
                        commit: "0123456789abcdef".to_string(),
                    },
                    branch: "task/task-1-durable-tasks".to_string(),
                    path: temp.path().join("worktrees").join("task-1"),
                },
                "2026-08-22T09:00:00Z",
            )
            .unwrap();
        let session = TaskSessionRecord {
            task_session_id: "task-session-1".to_string(),
            label: "Codex implementation".to_string(),
            harness: None,
            conversation: Some(HarnessConversationRef {
                backend: HarnessConversationBackend::Codex,
                session_id: "codex-chat-1".to_string(),
            }),
            objective_delivery: ObjectiveDeliveryState::Delivered,
            launch_identity: None,
            created_at: "2026-08-22T09:01:00Z".to_string(),
            updated_at: "2026-08-22T09:01:00Z".to_string(),
        };
        store
            .upsert_session("task-1", session.clone(), "2026-08-22T09:01:00Z")
            .unwrap();

        store
            .clear_worktree("task-1", "2026-08-22T10:00:00Z")
            .unwrap();

        let reloaded = TaskStore::load(&path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.worktree, TaskWorktree::default());
        assert_eq!(task.sessions, vec![session]);
        assert_eq!(task.updated_at, "2026-08-22T10:00:00Z");
    }

    #[test]
    fn draft_pr_copy_prefixes_issue_key_and_links_the_issue() {
        let task = sample_task("task-1");

        let (title, body) = task.draft_pr_copy();

        assert_eq!(title, "TRU-104: Add durable tasks");
        assert_eq!(
            body,
            "Persist task state without opening a workspace\n\n\
             Linked issue: [TRU-104](https://linear.app/example/TRU-104)"
        );
    }

    #[test]
    fn draft_pr_copy_without_issue_uses_title_and_objective_as_is() {
        let mut task = sample_task("task-1");
        task.issue = None;
        task.title = "TRU-104: Already prefixed".to_string();

        let (title, body) = task.draft_pr_copy();

        assert_eq!(title, "TRU-104: Already prefixed");
        assert_eq!(body, "Persist task state without opening a workspace");
    }

    #[test]
    fn requested_harness_recorded_at_queue_time_survives_reload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        let mut task = sample_task("task-1");
        // A task deferred to the queue before its first launch has no
        // recorded harness yet — that is exactly the case being fixed.
        task.harness = None;
        store.insert(task).unwrap();
        make_ready(&mut store, "task-1");
        let requested = HarnessSelection {
            kind: HarnessKind::TerminalPreset {
                preset_name: "Sleeper".to_string(),
            },
            model: None,
        };

        store
            .record_requested_harness("task-1", requested.clone(), "2026-08-19T08:01:00Z")
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Queued,
                None,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Queued);
        assert_eq!(task.harness.as_ref(), Some(&requested));
        // The queued attempt carries the request too — signal-built attempts
        // clone the task's harness.
        assert_eq!(
            task.attempts
                .last()
                .and_then(|attempt| attempt.harness.as_ref()),
            Some(&requested)
        );
    }

    #[test]
    fn startup_migrates_queued_gemini_references_and_preserves_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        let mut task = sample_task("queued");
        task.harness = Some(HarnessSelection {
            kind: HarnessKind::TerminalPreset {
                preset_name: "Gemini".into(),
            },
            model: None,
        });
        store.insert(task.clone()).unwrap();
        task.task_id = "history".into();
        store.insert(task.clone()).unwrap();
        make_ready(&mut store, "queued");
        store
            .record_lifecycle_signal("queued", TaskLifecycle::Queued, None, "t1")
            .unwrap();
        let mut worker = sample_delegation("worker", "parent", "t1");
        worker.kind = DelegationKind::Implement;
        worker.target = None;
        worker.child = DelegationChild::TaskSession {
            task_id: "queued".into(),
            task_session_id: None,
            conversation: None,
            preset_name: Some("Gemini".into()),
            model: None,
            role: None,
        };
        store.insert_delegation(worker).unwrap();

        let mut store = TaskStore::load(&path).unwrap();
        store.migrate_queued_gemini_presets().unwrap();
        let reloaded = TaskStore::load(&path).unwrap();
        let queued = reloaded.get("queued").unwrap();
        let expected = Some(HarnessSelection {
            kind: HarnessKind::TerminalPreset {
                preset_name: "Antigravity".into(),
            },
            model: None,
        });
        assert_eq!(queued.harness, expected);
        assert_eq!(queued.attempts.last().unwrap().harness, expected);
        assert_eq!(reloaded.get("history").unwrap(), &task);
        assert!(matches!(
            &reloaded.delegation("worker").unwrap().child,
            DelegationChild::TaskSession { preset_name: Some(name), .. } if name == "Antigravity"
        ));
        let before = std::fs::read(&path).unwrap();
        store.migrate_queued_gemini_presets().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let error = "Antigravity workers can't reach GitTerm's task tools yet";
        store
            .record_lifecycle_signal("queued", TaskLifecycle::Failed, Some(error.into()), "t4")
            .unwrap();
        let store = TaskStore::load(&path).unwrap();
        let task = store.get("queued").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Failed);
        assert_eq!(task.last_error.as_deref(), Some(error));
        assert_eq!(task.attempts.last().unwrap().state, AttemptState::Failed);
        let worker = store.delegation("worker").unwrap();
        assert_eq!(
            worker.status,
            DelegationStatus::Failed {
                message: error.into()
            }
        );
        assert!(store.open_worker_delegations("queued").is_empty());
        assert_eq!(
            crate::delegations::worker_session_state(
                &worker.status,
                Some(task.lifecycle),
                crate::delegations::WorkerTab::Closed,
            ),
            "failed"
        );
    }

    #[test]
    fn task_handoff_round_trips_compact_cross_harness_context() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let handoff = TaskHandoff {
            summary: "Implementation is complete; review remains".to_string(),
            decisions: vec!["Keep the task store as the only writer".to_string()],
            next_steps: vec!["Run the all-features gate".to_string()],
            blockers: Vec::new(),
            updated_by_session_id: None,
            updated_at: "2026-08-19T09:00:00Z".to_string(),
        };

        store
            .update_handoff("task-1", handoff.clone(), "2026-08-19T09:00:00Z")
            .unwrap();

        assert_eq!(
            TaskStore::load(path)
                .unwrap()
                .get("task-1")
                .unwrap()
                .handoff,
            Some(handoff)
        );
    }

    #[test]
    fn round_trips_task_records_and_every_enum_variant() {
        let lifecycle = vec![
            TaskLifecycle::Draft,
            TaskLifecycle::Preparing,
            TaskLifecycle::Ready,
            TaskLifecycle::Queued,
            TaskLifecycle::Running,
            TaskLifecycle::WaitingForInput,
            TaskLifecycle::Completed,
            TaskLifecycle::Failed,
            TaskLifecycle::Stopped,
            TaskLifecycle::Interrupted,
            TaskLifecycle::Archived,
        ];
        let attempts = vec![
            AttemptState::Preparing,
            AttemptState::Queued,
            AttemptState::Running,
            AttemptState::WaitingForInput,
            AttemptState::Completed,
            AttemptState::Failed,
            AttemptState::Stopped,
            AttemptState::Interrupted,
        ];
        let executors = vec![
            ExecutorTarget::Local,
            ExecutorTarget::RemoteAgent {
                remote_id: "mini".to_string(),
            },
        ];
        let harnesses = vec![
            HarnessKind::TerminalPreset {
                preset_name: "Codex".to_string(),
            },
            HarnessKind::NativeClaude,
            HarnessKind::NativePi,
        ];
        let stopping_boundaries = vec![
            StoppingBoundary::PlanOnly,
            StoppingBoundary::ImplementUntilTestsPass,
            StoppingBoundary::PrepareDraftPr,
        ];
        let worktree_states = vec![
            TaskWorktreeState::Unprepared,
            TaskWorktreeState::Preparing,
            TaskWorktreeState::Ready,
            TaskWorktreeState::Missing,
            TaskWorktreeState::CleanupRequired,
        ];
        let attention_reasons = vec![
            TaskAttentionReason::RequiresInput,
            TaskAttentionReason::ExecutionFailed,
            TaskAttentionReason::Interrupted,
            TaskAttentionReason::CompletedUnread,
            TaskAttentionReason::ReadyForReview,
            TaskAttentionReason::RemoteUnavailable,
        ];
        let verification_states = vec![
            VerificationState::Unknown,
            VerificationState::NotRun,
            VerificationState::Running,
            VerificationState::Passed,
            VerificationState::Failed,
        ];
        let issue_providers = vec![IssueProvider::Linear, IssueProvider::GitHub];
        let conversation_backends = vec![
            HarnessConversationBackend::Claude,
            HarnessConversationBackend::Codex,
            HarnessConversationBackend::Pi,
        ];
        let workspace_locations = vec![
            WorkspaceLocationIdentity::Local {
                directory: PathBuf::from("/local/repo"),
            },
            WorkspaceLocationIdentity::RemoteAgent {
                remote_id: "mini".to_string(),
                root: "/remote/repo".to_string(),
            },
        ];
        let encoded = serde_json::to_vec(&(
            lifecycle.clone(),
            attempts.clone(),
            executors.clone(),
            harnesses.clone(),
            stopping_boundaries.clone(),
            worktree_states.clone(),
            attention_reasons.clone(),
            verification_states.clone(),
            issue_providers.clone(),
            workspace_locations.clone(),
            conversation_backends.clone(),
        ))
        .unwrap();
        let decoded: (
            Vec<TaskLifecycle>,
            Vec<AttemptState>,
            Vec<ExecutorTarget>,
            Vec<HarnessKind>,
            Vec<StoppingBoundary>,
            Vec<TaskWorktreeState>,
            Vec<TaskAttentionReason>,
            Vec<VerificationState>,
            Vec<IssueProvider>,
            Vec<WorkspaceLocationIdentity>,
            Vec<HarnessConversationBackend>,
        ) = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.0, lifecycle);
        assert_eq!(decoded.1, attempts);
        assert_eq!(decoded.2, executors);
        assert_eq!(decoded.3, harnesses);
        assert_eq!(decoded.4, stopping_boundaries);
        assert_eq!(decoded.5, worktree_states);
        assert_eq!(decoded.6, attention_reasons);
        assert_eq!(decoded.7, verification_states);
        assert_eq!(decoded.8, issue_providers);
        assert_eq!(decoded.9, workspace_locations);
        assert_eq!(decoded.10, conversation_backends);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        let task = sample_task("task-1");
        store.insert(task.clone()).unwrap();
        let reloaded = TaskStore::load(path).unwrap();
        assert_eq!(reloaded.tasks(), &[task]);
    }

    #[test]
    fn task_harness_is_optional_without_breaking_existing_records() {
        let legacy_task = sample_task("legacy-task");
        let legacy_json = serde_json::to_value(&legacy_task).unwrap();
        let decoded_legacy: TaskRecord = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(decoded_legacy.harness, legacy_task.harness);

        let mut task_without_session = serde_json::to_value(sample_task("new-task")).unwrap();
        task_without_session
            .as_object_mut()
            .unwrap()
            .remove("harness");
        let decoded_without_session: TaskRecord =
            serde_json::from_value(task_without_session).unwrap();
        assert_eq!(decoded_without_session.harness, None);
        assert!(
            serde_json::to_value(decoded_without_session).unwrap()["harness"].is_null(),
            "a task with no child session should not persist a task-level harness"
        );
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        fs::write(&path, r#"{"schema_version":99,"tasks":[]}"#).unwrap();
        let error = TaskStore::load(&path).unwrap_err();
        assert_eq!(error.operation(), "validate");
        assert!(error.to_string().contains("unsupported schema version 99"));
    }

    #[test]
    fn corrupt_store_is_reported_without_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let original = b"{ definitely not valid json";
        fs::write(&path, original).unwrap();
        let error = TaskStore::load(&path).unwrap_err();
        assert_eq!(error.operation(), "decode");
        assert_eq!(error.path(), path);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn startup_reconciles_active_local_attempts_to_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();
        assert_eq!(
            store
                .reconcile_after_restart("2026-08-19T09:00:00Z")
                .unwrap(),
            1
        );
        let reloaded = TaskStore::load(path).unwrap();
        let task = &reloaded.tasks()[0];
        assert_eq!(task.lifecycle, TaskLifecycle::Interrupted);
        assert_eq!(task.active_attempt_id, None);
        assert_eq!(task.attempts[0].state, AttemptState::Interrupted);
        assert_eq!(
            task.attempts[0].ended_at.as_deref(),
            Some("2026-08-19T09:00:00Z")
        );
        assert!(task.attempts[0]
            .failure
            .as_deref()
            .unwrap()
            .contains("GitTerm restarted"));
        // A routine restart is an interruption, not a failure — it must not
        // wear failure-red attention.
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::Interrupted)
        );
        assert!(task.attention.unread);
    }

    #[test]
    fn startup_leaves_active_remote_attempts_for_remote_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt(
                    "attempt-1",
                    ExecutorTarget::RemoteAgent {
                        remote_id: "mini".to_string(),
                    },
                ),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();
        assert_eq!(
            store
                .reconcile_after_restart("2026-08-19T09:00:00Z")
                .unwrap(),
            0
        );
        assert_eq!(store.tasks()[0].lifecycle, TaskLifecycle::Running);
    }

    #[test]
    fn enforces_one_active_attempt_per_task() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();
        let error = store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-2", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:02:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("already has an active attempt"));
    }

    #[test]
    fn rejects_invalid_lifecycle_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let mut task = store.get("task-1").unwrap().clone();
        task.lifecycle = TaskLifecycle::Completed;
        let error = store.replace(task).unwrap_err();
        assert!(error.to_string().contains("cannot transition"));
    }

    #[test]
    fn rejects_active_attempt_state_that_does_not_match_task_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");

        let error = store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::WaitingForInput,
                "2026-08-19T08:01:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn archives_inactive_tasks_without_deleting_their_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        store.archive("task-1", "2026-08-19T10:00:00Z").unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Archived);
        assert_eq!(task.archived_at.as_deref(), Some("2026-08-19T10:00:00Z"));
    }

    #[test]
    fn refuses_to_archive_a_task_with_an_active_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();

        let error = store.archive("task-1", "2026-08-19T10:00:00Z").unwrap_err();
        assert!(error.to_string().contains("cannot transition"));
        assert_eq!(store.tasks()[0].lifecycle, TaskLifecycle::Running);
    }

    #[test]
    fn persists_worktree_preparation_success_and_failure_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let ready_worktree = temp.path().join("worktrees").join("ready-task");
        let failed_worktree = temp.path().join("worktrees").join("failed-task");
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("ready-task")).unwrap();
        store.insert(sample_task("failed-task")).unwrap();

        store
            .begin_worktree_preparation("ready-task", "2026-08-19T08:01:00Z")
            .unwrap();
        store
            .complete_worktree_preparation(
                "ready-task",
                CompletedWorktreePreparation {
                    repository: RepositoryIdentity {
                        common_dir: PathBuf::from("/repo/.git"),
                        remote_url: None,
                    },
                    base: GitBase {
                        reference: "main".to_string(),
                        commit: "fedcba9876543210".to_string(),
                    },
                    branch: "task/ready-task-prepared".to_string(),
                    path: ready_worktree.clone(),
                },
                "2026-08-19T08:02:00Z",
            )
            .unwrap();

        store
            .begin_worktree_preparation("failed-task", "2026-08-19T08:03:00Z")
            .unwrap();
        store
            .fail_worktree_preparation(
                "failed-task",
                "git worktree add failed".to_string(),
                Some(failed_worktree),
                "2026-08-19T08:04:00Z",
            )
            .unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        let ready = reloaded.get("ready-task").unwrap();
        assert_eq!(ready.lifecycle, TaskLifecycle::Ready);
        assert_eq!(ready.worktree.state, TaskWorktreeState::Ready);
        assert_eq!(
            ready.worktree.path.as_deref(),
            Some(ready_worktree.as_path())
        );
        let failed = reloaded.get("failed-task").unwrap();
        assert_eq!(failed.lifecycle, TaskLifecycle::Failed);
        assert_eq!(failed.worktree.state, TaskWorktreeState::CleanupRequired);
        assert_eq!(
            failed.last_error.as_deref(),
            Some("git worktree add failed")
        );
        assert_eq!(
            failed.attention.reason,
            Some(TaskAttentionReason::ExecutionFailed)
        );
    }

    #[test]
    fn lifecycle_signal_hops_through_running_into_active_states() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");

        // A waiting signal from a Ready task means the session ran first even
        // though the start itself was never observed.
        let changed = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::WaitingForInput,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        assert!(changed);
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::WaitingForInput);
        assert_eq!(task.updated_at, "2026-08-19T08:05:00Z");
        // The signal opened an attempt record for the run it implies.
        assert_eq!(task.attempts.len(), 1);
        assert_eq!(task.attempts[0].state, AttemptState::WaitingForInput);
        assert_eq!(
            task.active_attempt_id.as_deref(),
            Some(task.attempts[0].attempt_id.as_str())
        );
    }

    #[test]
    fn lifecycle_signal_never_hops_between_terminal_states() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Failed,
                Some("agent reported an error".to_string()),
                "2026-08-19T08:05:00Z",
            )
            .unwrap();

        // A later completion from a sibling session must not launder the
        // failure into success.
        let error = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Completed,
                None,
                "2026-08-19T08:06:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("Failed"), "{error}");
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Failed);
        assert_eq!(task.last_error.as_deref(), Some("agent reported an error"));
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::ExecutionFailed)
        );
        assert!(task.attention.unread);
    }

    #[test]
    fn lifecycle_signal_same_state_is_a_noop_without_detail() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();

        let changed = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:06:00Z",
            )
            .unwrap();
        assert!(!changed);
        assert_eq!(
            store.get("task-1").unwrap().updated_at,
            "2026-08-19T08:05:00Z"
        );
    }

    #[test]
    fn lifecycle_signal_rejects_states_that_never_ran() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        // Draft tasks have no worktree and no session; every session signal
        // against one is a wiring bug worth surfacing.
        let error = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("Draft"), "{error}");
        assert_eq!(store.get("task-1").unwrap().lifecycle, TaskLifecycle::Draft);
    }

    #[test]
    fn lifecycle_signal_resumes_interrupted_tasks_into_waiting() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .reconcile_after_restart("2026-08-19T08:06:00Z")
            .unwrap();
        assert_eq!(
            store.get("task-1").unwrap().lifecycle,
            TaskLifecycle::Interrupted
        );

        // A restored session (claude --resume) that shows its prompt again is
        // genuinely alive and waiting.
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::WaitingForInput,
                None,
                "2026-08-19T08:07:00Z",
            )
            .unwrap();
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::WaitingForInput);
        // The interrupted attempt stays closed as history; the resume opened
        // a fresh one.
        assert_eq!(task.attempts.len(), 2);
        assert_eq!(task.attempts[0].state, AttemptState::Interrupted);
        assert!(task.attempts[0].ended_at.is_some());
        assert_eq!(task.attempts[1].state, AttemptState::WaitingForInput);
        assert_eq!(
            task.active_attempt_id.as_deref(),
            Some(task.attempts[1].attempt_id.as_str())
        );
        // The interruption's attention and error are superseded by the live
        // resume — the failure detail survives on the closed attempt.
        assert_eq!(task.attention.reason, None);
        assert!(!task.attention.unread);
        assert_eq!(task.last_error, None);
        assert_eq!(
            task.attempts[0].failure.as_deref(),
            Some("GitTerm restarted while this local execution was active")
        );
    }

    #[test]
    fn completion_signal_marks_the_task_completed_unread() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Completed,
                None,
                "2026-08-19T08:10:00Z",
            )
            .unwrap();
        let task = store.get("task-1").unwrap();
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::CompletedUnread)
        );
        assert!(task.attention.unread);
    }

    #[test]
    fn acknowledging_attention_clears_completion_but_keeps_state_backed_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Completed,
                None,
                "2026-08-19T08:10:00Z",
            )
            .unwrap();
        assert!(store
            .acknowledge_attention("task-1", "2026-08-19T08:11:00Z")
            .unwrap());
        let task = store.get("task-1").unwrap();
        assert_eq!(task.attention.reason, None);
        assert!(!task.attention.unread);
        // Acknowledging an already-read task is a no-op that leaves the file
        // untouched — callers fire it on every visit and periodic tick.
        let before = std::fs::read(&path).unwrap();
        assert!(!store
            .acknowledge_attention("task-1", "2026-08-19T08:12:00Z")
            .unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // A failure reason survives the visit — only its unread badge clears.
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:13:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Failed,
                Some("agent reported an error".to_string()),
                "2026-08-19T08:14:00Z",
            )
            .unwrap();
        assert!(store
            .acknowledge_attention("task-1", "2026-08-19T08:15:00Z")
            .unwrap());
        let task = store.get("task-1").unwrap();
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::ExecutionFailed)
        );
        assert!(!task.attention.unread);
    }

    #[test]
    fn dismissing_attention_drops_even_state_backed_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Failed,
                Some("agent reported an error".to_string()),
                "2026-08-19T08:06:00Z",
            )
            .unwrap();
        // A visit keeps a failure reason; an explicit dismissal drops it —
        // the lifecycle still says Failed, so the rail stays truthful.
        assert!(store
            .dismiss_attention("task-1", "2026-08-19T08:07:00Z")
            .unwrap());
        let task = store.get("task-1").unwrap();
        assert_eq!(task.attention, TaskAttention::default());
        assert_eq!(task.lifecycle, TaskLifecycle::Failed);
        // Dismissing again is a no-op that leaves the file untouched.
        let before = std::fs::read(&path).unwrap();
        assert!(!store
            .dismiss_attention("task-1", "2026-08-19T08:08:00Z")
            .unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn queued_task_can_cancel_back_to_ready() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Queued,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        assert_eq!(
            store.get("task-1").unwrap().lifecycle,
            TaskLifecycle::Queued
        );
        // Cancelling the queue steps back to Ready without pretending it ran.
        store
            .record_lifecycle_signal("task-1", TaskLifecycle::Ready, None, "2026-08-19T08:06:00Z")
            .unwrap();
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Ready);
        assert_eq!(task.attention, TaskAttention::default());
    }

    #[test]
    fn update_progress_persists_snapshot_and_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        let progress = TaskProgress {
            phase: Some("Running tests".to_string()),
            last_update_line: Some("cargo test --features excalidraw".to_string()),
            last_activity_at: Some("2026-08-19T08:10:00Z".to_string()),
        };
        let changes = ChangedFilesSummary {
            changed: 3,
            staged: 1,
            checked_at: Some("2026-08-19T08:10:00Z".to_string()),
        };
        let wrote = store
            .update_progress(
                "task-1",
                progress.clone(),
                Some(changes.clone()),
                "2026-08-19T08:10:05Z",
            )
            .unwrap();
        assert!(wrote);

        let reloaded = TaskStore::load(&path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.progress.as_ref(), Some(&progress));
        assert_eq!(task.changes, changes);
        assert_eq!(task.updated_at, "2026-08-19T08:10:05Z");
    }

    #[test]
    fn update_progress_is_a_no_op_when_nothing_differs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        let progress = TaskProgress {
            phase: Some("Editing".to_string()),
            last_update_line: None,
            last_activity_at: Some("2026-08-19T08:10:00Z".to_string()),
        };
        assert!(store
            .update_progress("task-1", progress.clone(), None, "2026-08-19T08:10:05Z")
            .unwrap());
        let bytes_after_first = fs::read(&path).unwrap();

        // Same snapshot again — the flush timer will do this constantly, and it
        // must not rewrite the file or bump updated_at.
        assert!(!store
            .update_progress("task-1", progress.clone(), None, "2026-08-19T08:15:00Z")
            .unwrap());
        assert_eq!(fs::read(&path).unwrap(), bytes_after_first);

        // Unchanged progress but fresh change counts still writes.
        let changes = ChangedFilesSummary {
            changed: 2,
            staged: 0,
            checked_at: Some("2026-08-19T08:16:00Z".to_string()),
        };
        assert!(store
            .update_progress("task-1", progress, Some(changes), "2026-08-19T08:16:00Z")
            .unwrap());
    }

    #[test]
    fn update_progress_rejects_unknown_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        let error = store
            .update_progress(
                "missing",
                TaskProgress::default(),
                None,
                "2026-08-19T08:10:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn task_store_writes_do_not_modify_workspace_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_path = temp.path().join("workspaces.json");
        let workspace_bytes = br#"{"workspaces":[{"name":"existing"}]}"#;
        fs::write(&workspace_path, workspace_bytes).unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        assert_eq!(fs::read(workspace_path).unwrap(), workspace_bytes);
    }

    #[test]
    fn a_started_codex_run_records_its_thread_and_a_finished_card_can_be_dismissed() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store
            .insert_delegation(sample_delegation("review-1", "tab-a", "t1"))
            .unwrap();
        // Active delegations cannot be dismissed.
        assert!(store.dismiss_delegation("review-1", "t2").is_err());
        store
            .mark_delegation_started("review-1", "thread-9", "t2")
            .unwrap();
        let delegation = store.delegation("review-1").unwrap();
        assert_eq!(delegation.status, DelegationStatus::Running);
        assert_eq!(delegation.codex_child(), Some((Some("thread-9"), None)));
        // Starting twice is an error.
        assert!(store
            .mark_delegation_started("review-1", "thread-10", "t3")
            .is_err());
        store
            .complete_delegation("review-1", sample_review_result(), "t4")
            .unwrap();
        store.dismiss_delegation("review-1", "t5").unwrap();
        store.dismiss_delegation("review-1", "t6").unwrap();
        assert_eq!(
            store
                .delegation("review-1")
                .unwrap()
                .dismissed_at
                .as_deref(),
            Some("t5")
        );
        let reloaded = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        assert_eq!(
            reloaded
                .delegation("review-1")
                .unwrap()
                .dismissed_at
                .as_deref(),
            Some("t5")
        );
    }

    #[test]
    fn consult_delegations_need_a_codex_consult_child_and_complete_with_a_handoff() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        let mut consult = sample_delegation("consult-1", "tab-a", "t1");
        consult.kind = DelegationKind::Consult;
        consult.target = None;
        // A review child does not fit a consult.
        assert!(store.insert_delegation(consult.clone()).is_err());
        consult.child = DelegationChild::CodexConsult {
            thread_id: None,
            model: Some("gpt-6-astra".to_string()),
        };
        store.insert_delegation(consult).unwrap();
        // Findings alone are not a consult result.
        assert!(store
            .complete_delegation("consult-1", sample_review_result(), "t2")
            .is_err());
        store
            .complete_delegation(
                "consult-1",
                DelegationResult {
                    handoff: Some(TaskHandoff {
                        summary: "Split the module".to_string(),
                        decisions: Vec::new(),
                        next_steps: Vec::new(),
                        blockers: Vec::new(),
                        updated_by_session_id: None,
                        updated_at: "t2".to_string(),
                    }),
                    findings: None,
                    reviewed: None,
                },
                "t2",
            )
            .unwrap();
        assert_eq!(
            store.delegation("consult-1").unwrap().status,
            DelegationStatus::Completed
        );
    }
}
