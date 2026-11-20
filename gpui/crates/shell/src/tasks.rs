//! Fixture-backed task switcher and confirmation forms.
//!
//! These views deliberately stop at typed intents. The shared owner will
//! validate and dispatch them when task wiring lands.
use gitterm_core::tasks::{StoppingBoundary, TaskLifecycle, TaskRecord};
use gpui::{div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskFormIntent {
    Create {
        task_id: String,
        title: String,
        objective: String,
        issue_key: Option<String>,
        base_reference: String,
        branch: String,
        stopping_boundary: StoppingBoundary,
    },
    Close {
        task_id: String,
    },
    Delete {
        task_id: String,
    },
    Publish {
        task_id: String,
        action: PublishAction,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishAction {
    PushBranch,
    OpenDraftPr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateTaskForm {
    pub task_id: String,
    pub title: String,
    pub objective: String,
    pub issue_key: String,
    pub base_reference: String,
    pub branch: String,
    pub stopping_boundary: StoppingBoundary,
}

impl CreateTaskForm {
    pub fn intent(&self) -> TaskFormIntent {
        TaskFormIntent::Create {
            task_id: self.task_id.clone(),
            title: self.title.clone(),
            objective: self.objective.clone(),
            issue_key: nonempty(&self.issue_key),
            base_reference: self.base_reference.clone(),
            branch: self.branch.clone(),
            stopping_boundary: self.stopping_boundary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskConfirmationForm {
    pub task_id: String,
    pub title: String,
}

impl TaskConfirmationForm {
    pub fn close_intent(&self) -> TaskFormIntent {
        TaskFormIntent::Close {
            task_id: self.task_id.clone(),
        }
    }

    pub fn delete_intent(&self) -> TaskFormIntent {
        TaskFormIntent::Delete {
            task_id: self.task_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishTaskForm {
    pub task_id: String,
    pub title: String,
    pub action: PublishAction,
}

impl PublishTaskForm {
    pub fn intent(&self) -> TaskFormIntent {
        TaskFormIntent::Publish {
            task_id: self.task_id.clone(),
            action: self.action,
        }
    }
}

/// Cancel is a view dismissal and intentionally carries no domain intent.
pub fn cancel() -> Option<TaskFormIntent> {
    None
}

pub fn create_form(form: &CreateTaskForm) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w(px(620.))
        .p_4()
        .child("New task")
        .child(field("TITLE", &form.title))
        .child(field("OBJECTIVE", &form.objective))
        .child(field("LINEAR ISSUE", &form.issue_key))
        .child(field("BASE", &form.base_reference))
        .child(field("BRANCH / WORKTREE", &form.branch))
        .child(field(
            "STOPPING BOUNDARY",
            boundary_label(form.stopping_boundary),
        ))
        .child(
            div()
                .flex()
                .gap_2()
                .child(Button::new("cancel-create-task").label("Cancel"))
                .child(
                    Button::new("submit-create-task")
                        .primary()
                        .label("Create task"),
                ),
        )
}

pub fn close_form(form: &TaskConfirmationForm) -> impl IntoElement {
    confirmation_form("Close task?", form)
}

pub fn delete_form(form: &TaskConfirmationForm) -> impl IntoElement {
    confirmation_form("Delete task worktree?", form)
}

pub fn publish_form(form: &PublishTaskForm) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w(px(560.))
        .p_4()
        .child(match form.action {
            PublishAction::PushBranch => "Push task branch?",
            PublishAction::OpenDraftPr => "Open a draft PR?",
        })
        .child(form.title.clone())
        .child("Cancel to dismiss, or confirm to continue.")
        .child(
            div()
                .flex()
                .gap_2()
                .child(Button::new("cancel-publish-task").label("Cancel"))
                .child(
                    Button::new("submit-publish-task")
                        .primary()
                        .label(match form.action {
                            PublishAction::PushBranch => "Push branch",
                            PublishAction::OpenDraftPr => "Open draft PR",
                        }),
                ),
        )
}

fn confirmation_form(title: &'static str, form: &TaskConfirmationForm) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w(px(560.))
        .p_4()
        .child(title)
        .child(form.title.clone())
        .child(format!("Task {}", form.task_id))
        .child("Cancel to dismiss, or confirm to continue.")
        .child(
            div()
                .flex()
                .gap_2()
                .child(Button::new("cancel-confirm-task").label("Cancel"))
                .child(
                    Button::new("submit-confirm-task")
                        .primary()
                        .label(match title {
                            "Close task?" => "Close task",
                            _ => "Delete worktree",
                        }),
                ),
        )
}

fn field(label: &'static str, value: impl Into<gpui::SharedString>) -> impl IntoElement {
    let value = value.into();
    div().flex().flex_col().gap_1().child(label).child(value)
}

fn boundary_label(boundary: StoppingBoundary) -> &'static str {
    match boundary {
        StoppingBoundary::PlanOnly => "Plan only",
        StoppingBoundary::ImplementUntilTestsPass => "Tests pass",
        StoppingBoundary::PrepareDraftPr => "Draft PR",
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
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
