//! Closing a worker's final session once its result went out: the decisions
//! Iced's `close_final_workers` makes, moved here so GPUI's fixture
//! launches decide them the same way. The frontend tracks its sessions,
//! stops the ones this names and records the lifecycles it returns.
use super::{DeliveryChat, TaskHost, TaskSessionLiveState};
use crate::tasks::{Delegation, DelegationChild, DelegationStatus, TaskLifecycle, TaskStore};
use std::collections::HashSet;

/// Whether a finished worker's result has gone far enough for its final
/// session to close: its handoff is stored, and the result was delivered,
/// is held for its chat's turn end, or will never be sent automatically
/// (automatic delivery is off, or its chat is not open).
pub fn worker_delivery_allows_close(
    d: &Delegation,
    auto_tell: bool,
    parent_present: bool,
    held: bool,
) -> bool {
    let handoff_stored = d
        .result
        .as_ref()
        .and_then(|result| result.handoff.as_ref())
        .is_some();
    handoff_stored && (d.is_delivered() || held || !auto_tell || !parent_present)
}

/// One open task session, as the frontend tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSession<'a> {
    pub task_id: Option<&'a str>,
    pub task_session_id: Option<&'a str>,
    /// Its close is already under way.
    pub closing: bool,
    /// A stop was refused once; it is never retried.
    pub shutdown_refused: bool,
    pub live: Option<TaskSessionLiveState>,
}

/// The working-session predicate used by Iced's local slots and provider
/// budget counts. A final, closing or refused worker spends no active slot.
pub fn worker_session_is_working(store: Option<&TaskStore>, session: &WorkerSession) -> bool {
    session.live == Some(TaskSessionLiveState::Working)
        && !session.shutdown_refused
        && !session.closing
        && !session.task_id.is_some_and(|task| {
            store.is_some_and(|store| store.worker_session_is_final(task, session.task_session_id))
        })
}

/// Iced's `close_final_worker` decision: the session is its worker's final
/// one (completed or failed, never cancelled), the result is `ready`
/// ([`worker_delivery_allows_close`]), no close is under way or was
/// refused, and a superseded worker is not still working.
pub fn final_worker_closes(
    store: &TaskStore,
    session: &WorkerSession,
    ready: &HashSet<String>,
) -> bool {
    let (Some(task), Some(id)) = (session.task_id, session.task_session_id) else {
        return false;
    };
    let Some(delegation) = store.delegations().iter().rev().find(|d| {
        matches!(&d.child, DelegationChild::TaskSession { task_id, task_session_id: Some(child), .. }
            if task_id == task && child == id)
    }) else {
        return false;
    };
    !(session.closing
        || session.shutdown_refused
        || !store.worker_session_is_final(task, Some(id))
        || !ready.contains(&delegation.delegation_id)
        || (delegation.superseded_by.is_some()
            && session.live == Some(TaskSessionLiveState::Working)))
}

/// Iced's `close_final_workers` ending: each task whose final worker
/// session is in `closing` and that has no open worker delegation left
/// ends as its newest final delegation did (a worker that ran out of time
/// stopped). One entry per task. The frontend records it unless another of
/// the task's sessions is still live.
pub fn closed_worker_task_outcomes(
    store: &TaskStore,
    closing: &[String],
) -> Vec<(String, TaskLifecycle)> {
    let mut ending_tasks = HashSet::new();
    store
        .delegations()
        .iter()
        .rev()
        .filter(|d| d.status.is_final())
        .filter_map(|d| {
            let DelegationChild::TaskSession {
                task_id,
                task_session_id: Some(session),
                ..
            } = &d.child
            else {
                return None;
            };
            if !closing.iter().any(|id| id == session)
                || !store.open_worker_delegations(task_id).is_empty()
                || !ending_tasks.insert(task_id.clone())
            {
                return None;
            }
            let state = match d.status {
                DelegationStatus::Completed => TaskLifecycle::Completed,
                DelegationStatus::Failed { .. } if d.time_limit_reached_at.is_none() => {
                    TaskLifecycle::Failed
                }
                _ => TaskLifecycle::Stopped,
            };
            Some((task_id.clone(), state))
        })
        .collect()
}

impl TaskHost {
    /// Iced's `close_final_workers` ready set over `chats`: the stored
    /// delegations whose result lets their worker's final session close.
    pub fn final_workers_ready(&self, chats: &[DeliveryChat], auto_tell: bool) -> HashSet<String> {
        let Some(store) = self.store().as_ref() else {
            return HashSet::new();
        };
        store
            .delegations()
            .iter()
            .filter(|d| {
                let parent = super::delivery::chat_for(chats, &d.parent);
                let held = parent.is_some_and(|id| self.delivery_held(id, &d.delegation_id));
                worker_delivery_allows_close(d, auto_tell, parent.is_some(), held)
            })
            .map(|d| d.delegation_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{DelegationKind, DelegationParent, NewDelegation, TaskHandoff};

    fn store_with_worker(dir: &std::path::Path) -> (TaskStore, String) {
        let mut store = TaskStore::load(dir.join("tasks.json")).unwrap();
        store
            .insert(crate::delegation_test_support::test_task("task-slot", dir))
            .unwrap();
        let delegation = Delegation::new_requested(
            NewDelegation {
                kind: DelegationKind::Implement,
                parent: DelegationParent {
                    session_uid: "parent".into(),
                    chat_session_id: None,
                    workspace: "work".into(),
                    cwd: dir.into(),
                },
                child: DelegationChild::TaskSession {
                    task_id: "task-slot".into(),
                    task_session_id: Some("worker".into()),
                    conversation: None,
                    preset_name: Some("Codex".into()),
                    model: None,
                    role: None,
                },
                brief: "Fix it".into(),
                target: None,
                previous: None,
                options: Default::default(),
            },
            "2026-11-05T10:00:00Z",
        );
        let id = delegation.delegation_id.clone();
        store.insert_delegation(delegation).unwrap();
        (store, id)
    }

    fn complete(store: &mut TaskStore, id: &str) {
        let handoff = TaskHandoff {
            summary: "Finished".into(),
            decisions: vec![],
            next_steps: vec![],
            blockers: vec![],
            updated_by_session_id: Some("worker".into()),
            updated_at: "2026-11-05T10:01:00Z".into(),
        };
        store
            .complete_delegation(id, crate::delegations::worker_result(handoff), "t2")
            .unwrap();
    }

    fn worker(live: Option<TaskSessionLiveState>) -> WorkerSession<'static> {
        WorkerSession {
            task_id: Some("task-slot"),
            task_session_id: Some("worker"),
            closing: false,
            shutdown_refused: false,
            live,
        }
    }

    #[test]
    fn only_a_working_nonfinal_unrefused_session_spends_a_slot() {
        let temp = tempfile::tempdir().unwrap();
        let (mut store, id) = store_with_worker(temp.path());
        let working = worker(Some(TaskSessionLiveState::Working));
        assert!(worker_session_is_working(Some(&store), &working));
        for session in [
            worker(None),
            worker(Some(TaskSessionLiveState::AwaitingInput)),
            WorkerSession {
                closing: true,
                ..working
            },
            WorkerSession {
                shutdown_refused: true,
                ..working
            },
        ] {
            assert!(!worker_session_is_working(Some(&store), &session));
        }
        complete(&mut store, &id);
        assert!(!worker_session_is_working(Some(&store), &working));
    }

    /// Moved from Iced: a rejected delivery waits for the next tick's
    /// receipt; held, automatic delivery off or no open chat lets it close.
    #[test]
    fn rejected_worker_delivery_waits_for_next_tick_receipt_before_close() {
        let temp = tempfile::tempdir().unwrap();
        let (mut store, id) = store_with_worker(temp.path());
        complete(&mut store, &id);
        let rejected = store.delegation(&id).unwrap();
        assert!(!worker_delivery_allows_close(rejected, true, true, false));
        let ready = HashSet::new(); // submission rejected, nothing held
        assert!(!final_worker_closes(&store, &worker(None), &ready));
        assert!(worker_delivery_allows_close(rejected, true, true, true));
        assert!(worker_delivery_allows_close(rejected, false, true, false));
        assert!(worker_delivery_allows_close(rejected, true, false, false));
        store.mark_delegation_delivered(&id, "t3").unwrap();
        let persisted = TaskStore::load(temp.path().join("tasks.json")).unwrap();
        let ready = persisted
            .delegations()
            .iter()
            .filter(|d| worker_delivery_allows_close(d, true, true, false))
            .map(|d| d.delegation_id.clone())
            .collect();
        assert!(final_worker_closes(&persisted, &worker(None), &ready));
    }

    /// No stored handoff never closes, whatever the delivery.
    #[test]
    fn a_result_without_a_handoff_never_allows_close() {
        let temp = tempfile::tempdir().unwrap();
        let (store, id) = store_with_worker(temp.path());
        let open = store.delegation(&id).unwrap();
        assert!(!worker_delivery_allows_close(open, false, false, true));
    }

    /// Moved from Iced: Cancelled is not final; a close under way, a refused
    /// stop or another session never closes; a superseded worker closes
    /// only once it stopped working.
    #[test]
    fn only_an_idle_final_session_closes() {
        let temp = tempfile::tempdir().unwrap();
        let (mut store, id) = store_with_worker(temp.path());
        let ready = HashSet::from([id.clone()]);
        assert!(!final_worker_closes(&store, &worker(None), &ready));
        store.cancel_delegation(&id, "t2").unwrap();
        let working = worker(Some(TaskSessionLiveState::Working));
        assert!(!final_worker_closes(&store, &working, &ready));

        let temp = tempfile::tempdir().unwrap();
        let (mut store, id) = store_with_worker(temp.path());
        complete(&mut store, &id);
        let ready = HashSet::from([id.clone()]);
        assert!(final_worker_closes(&store, &working, &ready));
        for session in [
            WorkerSession {
                closing: true,
                ..worker(None)
            },
            WorkerSession {
                shutdown_refused: true,
                ..worker(None)
            },
            WorkerSession {
                task_session_id: Some("sibling"),
                ..worker(None)
            },
            WorkerSession {
                task_id: None,
                ..worker(None)
            },
        ] {
            assert!(
                !final_worker_closes(&store, &session, &ready),
                "{session:?}"
            );
        }
    }

    /// Iced's ending: the task ends as its final delegation did, once per
    /// task, and only for a session being closed.
    #[test]
    fn closing_a_final_worker_ends_its_task_as_the_delegation_did() {
        let temp = tempfile::tempdir().unwrap();
        let (mut store, id) = store_with_worker(temp.path());
        complete(&mut store, &id);
        assert!(closed_worker_task_outcomes(&store, &[]).is_empty());
        assert_eq!(
            closed_worker_task_outcomes(&store, &["worker".into()]),
            vec![("task-slot".to_string(), TaskLifecycle::Completed)]
        );
        // A worker still open on the task keeps it going.
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = store_with_worker(temp.path());
        assert!(closed_worker_task_outcomes(&store, &["worker".into()]).is_empty());
    }
}
