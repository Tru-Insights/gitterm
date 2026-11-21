//! Task rows and Iced's attention scale. The mounted rail, Overview verbs
//! and task modals live in `task_panel.rs`.
use gitterm_core::tasks::{TaskLifecycle, TaskRecord};
use gpui::{div, prelude::*, px};

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSwitcherRow {
    pub task_id: String,
    pub title: String,
    pub issue: Option<String>,
    pub branch: String,
    pub state: &'static str,
    pub attention: bool,
    pub unread: bool,
}

pub fn switcher_rows(tasks: &[TaskRecord]) -> Vec<TaskSwitcherRow> {
    tasks
        .iter()
        .filter(|task| task.lifecycle != TaskLifecycle::Archived)
        .map(|task| TaskSwitcherRow {
            task_id: task.task_id.clone(),
            title: task.title.clone(),
            issue: task.issue.as_ref().map(|issue| issue.key.clone()),
            branch: task.branch.clone(),
            state: lifecycle_label(task.lifecycle),
            attention: task.attention.reason.is_some(),
            unread: task.attention.unread,
        })
        .collect()
}

fn lifecycle_label(state: TaskLifecycle) -> &'static str {
    match state {
        TaskLifecycle::Draft => "Draft",
        TaskLifecycle::Preparing => "Preparing",
        TaskLifecycle::Ready => "Ready",
        TaskLifecycle::Queued => "Queued",
        TaskLifecycle::Running => "Running",
        TaskLifecycle::WaitingForInput => "Waiting for input",
        TaskLifecycle::Completed => "Completed",
        TaskLifecycle::Failed => "Failed",
        TaskLifecycle::Stopped => "Stopped",
        TaskLifecycle::Interrupted => "Interrupted",
        TaskLifecycle::Archived => "Archived",
    }
}

/// Native GPUI switcher content. Ordering and attention facts come directly
/// from the supplied core records; the view does not reorder the fixture.
pub fn switcher(tasks: &[TaskRecord]) -> impl IntoElement {
    let rows = switcher_rows(tasks);
    div()
        .flex()
        .flex_col()
        .gap_2()
        .w(px(480.))
        .p_3()
        .child(div().text_xs().child("SWITCH TASK"))
        .children(rows.into_iter().map(|row| {
            let attention = if row.attention { "  !" } else { "" };
            let unread = if row.unread { "  •" } else { "" };
            let issue = row.issue.map(|key| format!("{key} · ")).unwrap_or_default();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .child(format!("{issue}{}{}{}", row.title, attention, unread))
                .child(format!("{} · {} · {}", row.branch, row.state, row.task_id))
        }))
}

#[cfg(test)]
pub(crate) fn fixture_task(task_id: &str, title: &str, branch: &str) -> TaskRecord {
    use gitterm_core::tasks::{
        ExecutorTarget, GitBase, NewTaskRecord, RepositoryIdentity, StoppingBoundary,
        WorkspaceIdentity, WorkspaceLocationIdentity,
    };
    TaskRecord::new_draft(
        NewTaskRecord {
            task_id: task_id.into(),
            title: title.into(),
            objective: format!("Implement {title}"),
            workspace: WorkspaceIdentity {
                name: "fixture".into(),
                location: WorkspaceLocationIdentity::Local {
                    directory: "/tmp/fixture".into(),
                },
            },
            repository: RepositoryIdentity {
                common_dir: "/tmp/fixture/.git".into(),
                remote_url: None,
            },
            issue: None,
            base: GitBase {
                reference: "main".into(),
                commit: "0123456789abcdef".into(),
            },
            branch: branch.into(),
            executor: ExecutorTarget::Local,
            harness: None,
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
        },
        "2026-01-01T00:00:00Z",
    )
}

/// Iced's task attention scale, retained independently of persisted lifecycle.
pub(crate) fn attention_presentation(
    reason: gitterm_core::tasks::TaskAttentionReason,
) -> (u8, &'static str, &'static str) {
    use gitterm_core::tasks::TaskAttentionReason::*;
    match reason {
        RequiresInput => (0, "●", "Input or approval needed"),
        RemoteUnavailable => (0, "●", "Remote unavailable"),
        ExecutionFailed => (1, "!", "Task failed"),
        Interrupted => (2, "○", "Interrupted · resume"),
        CompletedUnread | ReadyForReview => (3, "✓", "Ready to review"),
    }
}

pub(crate) fn attention_tasks(tasks: &[TaskRecord]) -> Vec<&TaskRecord> {
    let mut tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.lifecycle != TaskLifecycle::Archived && t.attention.reason.is_some())
        .collect();
    let now = std::time::SystemTime::now();
    let raised = |t: &TaskRecord| {
        gitterm_core::work::summary::parse_stamp(
            t.attention.since.as_deref().unwrap_or(&t.updated_at),
        )
        .map_or(now, |at| at.min(now))
    };
    tasks.sort_by(|a, b| {
        let priority = |t: &TaskRecord| {
            t.attention
                .reason
                .map_or(3, |r| attention_presentation(r).0)
        };
        priority(a)
            .cmp(&priority(b))
            .then_with(|| raised(a).cmp(&raised(b)))
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    tasks
}

/// Iced task rail: attention first, then active, ready, and settled tasks.
pub(crate) fn sort_bucket(task: &TaskRecord) -> u8 {
    if task.attention.reason.is_some() || task.attention.unread {
        0
    } else if task.lifecycle.is_active() {
        1
    } else if task.lifecycle == TaskLifecycle::Ready {
        2
    } else {
        3
    }
}
