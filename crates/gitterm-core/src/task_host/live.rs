//! A task's lifecycle from its sessions' live states, the decision Iced's
//! `apply_task_session_signal` makes (moved here so GPUI's fixture launches
//! decide it the same way). The frontend tracks each session's live state
//! and records the outcome this returns.
use crate::tasks::TaskLifecycle;

/// The outcome a finished task session implies: the lifecycle it ends in,
/// plus failure detail for the task's `last_error` when there is any.
pub type TaskSessionOutcome = (TaskLifecycle, Option<String>);

/// What a task-linked session is observably doing right now, from harness
/// signals only (agent stream state, terminal-title heuristics, launch and
/// exit of the harness process). Ephemeral view state — the durable record
/// is the task's persisted lifecycle. `None` means no live execution signal:
/// an open shell with nothing observably running claims nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSessionLiveState {
    Working,
    AwaitingInput,
}

/// Input answers notify-driven idle. A title-driven waiting state remains
/// owned by the harness title adapter, as in Iced's terminal-write arm.
pub fn input_resumes_waiting(live: Option<TaskSessionLiveState>, title: Option<&str>) -> bool {
    live == Some(TaskSessionLiveState::AwaitingInput)
        && title.is_none_or(|title| !title.starts_with('✳'))
}

/// Aggregate the live states of every session linked to one task into a
/// single lifecycle observation. A session awaiting input outranks work in
/// progress — the human is the blocker; work elsewhere doesn't unblock it.
/// No live signal at all yields None: the caller falls back to the ending
/// session's own outcome, or leaves the durable lifecycle untouched.
pub fn aggregate_task_live_states(
    states: impl Iterator<Item = TaskSessionLiveState>,
) -> Option<TaskLifecycle> {
    let mut working = false;
    for state in states {
        match state {
            TaskSessionLiveState::AwaitingInput => return Some(TaskLifecycle::WaitingForInput),
            TaskSessionLiveState::Working => working = true,
        }
    }
    working.then_some(TaskLifecycle::Running)
}

/// One session (tab or pane) linked to the task.
#[derive(Debug, Clone, Copy)]
pub struct LinkedSession {
    pub live: Option<TaskSessionLiveState>,
    /// The session is a worker's whose report is final.
    pub is_final: bool,
}

/// The lifecycle to record for a task after a session observation, over
/// every session linked to it (the signalling one already updated).
/// `ended` carries the outcome of a session that just finished: it sets the
/// lifecycle only when no other live session keeps the task active, but its
/// failure detail is recorded either way. A late observation when every
/// linked session is a final worker's records nothing, so it cannot replace
/// the persisted handoff outcome.
pub fn task_session_signal(
    sessions: &[LinkedSession],
    ended: Option<TaskSessionOutcome>,
) -> Option<TaskSessionOutcome> {
    if !sessions.is_empty() && sessions.iter().all(|session| session.is_final) {
        return None;
    }
    let live = aggregate_task_live_states(
        sessions
            .iter()
            .filter(|session| !session.is_final)
            .filter_map(|session| session.live),
    );
    match (live, ended) {
        (Some(state), ended) => Some((state, ended.and_then(|(_, detail)| detail))),
        (None, Some(ended)) => Some(ended),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(live: Option<TaskSessionLiveState>, is_final: bool) -> LinkedSession {
        LinkedSession { live, is_final }
    }

    #[test]
    fn input_resumes_notify_idle_but_preserves_title_owned_waiting() {
        assert!(input_resumes_waiting(
            Some(TaskSessionLiveState::AwaitingInput),
            None
        ));
        assert!(input_resumes_waiting(
            Some(TaskSessionLiveState::AwaitingInput),
            Some("shell")
        ));
        assert!(!input_resumes_waiting(
            Some(TaskSessionLiveState::AwaitingInput),
            Some("✳ Claude Code")
        ));
        assert!(!input_resumes_waiting(None, None));
    }

    #[test]
    fn task_live_state_aggregation_lets_waiting_outrank_working() {
        assert_eq!(aggregate_task_live_states(std::iter::empty()), None);
        assert_eq!(
            aggregate_task_live_states([TaskSessionLiveState::Working].into_iter()),
            Some(TaskLifecycle::Running)
        );
        assert_eq!(
            aggregate_task_live_states(
                [
                    TaskSessionLiveState::Working,
                    TaskSessionLiveState::AwaitingInput,
                ]
                .into_iter()
            ),
            Some(TaskLifecycle::WaitingForInput)
        );
    }

    #[test]
    fn ended_session_stops_the_task_only_when_nothing_else_is_live() {
        let stopped = Some((TaskLifecycle::Stopped, None));
        // The exited session was the only live one.
        assert_eq!(
            task_session_signal(&[session(None, false)], stopped.clone()),
            stopped
        );
        // Another session still works: the task stays running, and the
        // ending session's failure detail is kept.
        assert_eq!(
            task_session_signal(
                &[
                    session(None, false),
                    session(Some(TaskSessionLiveState::Working), false)
                ],
                Some((TaskLifecycle::Failed, Some("boom".into()))),
            ),
            Some((TaskLifecycle::Running, Some("boom".into())))
        );
        // A final worker's live state does not count.
        assert_eq!(
            task_session_signal(
                &[
                    session(None, false),
                    session(Some(TaskSessionLiveState::Working), true)
                ],
                stopped.clone(),
            ),
            stopped
        );
        // Every linked session is final: nothing is recorded.
        assert_eq!(
            task_session_signal(&[session(None, true)], stopped.clone()),
            None
        );
        // No live signal and nothing ended: nothing is recorded.
        assert_eq!(task_session_signal(&[session(None, false)], None), None);
        // No linked session left (the tab closed): the outcome stands.
        assert_eq!(task_session_signal(&[], stopped.clone()), stopped);
    }
}
