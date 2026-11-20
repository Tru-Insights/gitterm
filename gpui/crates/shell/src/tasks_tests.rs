use super::*;
use gitterm_core::tasks::{TaskAttention, TaskAttentionReason};

#[test]
fn switcher_preserves_fixture_order_and_marks_attention_and_unread() {
    let mut first = fixture_task("task-a", "First", "task/a-first");
    first.attention = TaskAttention::raised(
        TaskAttentionReason::RequiresInput,
        &TaskAttention::default(),
        "2026-01-01T00:00:00Z",
    );
    let second = fixture_task("task-b", "Second", "task/b-second");
    let rows = switcher_rows(&[first, second]);

    assert_eq!(
        rows.iter()
            .map(|row| row.task_id.as_str())
            .collect::<Vec<_>>(),
        ["task-a", "task-b"]
    );
    assert!(rows[0].attention);
    assert!(rows[0].unread);
    assert!(!rows[1].attention);
    assert!(!rows[1].unread);
    assert_eq!(rows[0].branch, "task/a-first");
    assert_eq!(rows[0].state, "Draft");
}

#[test]
fn create_submit_keeps_task_identity_and_form_values() {
    let form = CreateTaskForm {
        task_id: "task-new".into(),
        title: "Outcome".into(),
        objective: "Do the work".into(),
        issue_key: "TRU-274".into(),
        base_reference: "main".into(),
        branch: "task/274-outcome".into(),
        stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
    };
    assert_eq!(
        form.intent(),
        TaskFormIntent::Create {
            task_id: "task-new".into(),
            title: "Outcome".into(),
            objective: "Do the work".into(),
            issue_key: Some("TRU-274".into()),
            base_reference: "main".into(),
            branch: "task/274-outcome".into(),
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
        }
    );
}

#[test]
fn close_delete_and_publish_submit_with_task_ids() {
    let confirmation = TaskConfirmationForm {
        task_id: "task-stable-id".into(),
        title: "Task title".into(),
    };
    assert_eq!(
        confirmation.close_intent(),
        TaskFormIntent::Close {
            task_id: "task-stable-id".into()
        }
    );
    assert_eq!(
        confirmation.delete_intent(),
        TaskFormIntent::Delete {
            task_id: "task-stable-id".into()
        }
    );
    let publish = PublishTaskForm {
        task_id: "task-stable-id".into(),
        title: "Task title".into(),
        action: PublishAction::OpenDraftPr,
    };
    assert_eq!(
        publish.intent(),
        TaskFormIntent::Publish {
            task_id: "task-stable-id".into(),
            action: PublishAction::OpenDraftPr,
        }
    );
}

#[test]
fn every_cancel_path_returns_no_intent() {
    assert_eq!(cancel(), None);
}

#[test]
fn optional_issue_is_trimmed_and_empty_is_absent() {
    let mut form = CreateTaskForm {
        task_id: "task-new".into(),
        title: "Outcome".into(),
        objective: "Do the work".into(),
        issue_key: "  TRU-274  ".into(),
        base_reference: "main".into(),
        branch: "task/274-outcome".into(),
        stopping_boundary: StoppingBoundary::PlanOnly,
    };
    let TaskFormIntent::Create { issue_key, .. } = form.intent() else {
        panic!("create form should produce a create intent");
    };
    assert_eq!(issue_key.as_deref(), Some("TRU-274"));
    form.issue_key = "  ".into();
    let TaskFormIntent::Create { issue_key, .. } = form.intent() else {
        panic!("create form should produce a create intent");
    };
    assert_eq!(issue_key, None);
}

#[test]
fn durable_attention_uses_iced_priority_and_raised_time_with_legacy_fallback() {
    use gitterm_core::tasks::TaskAttentionReason;
    let mut old = fixture_task("old", "Old input", "old");
    old.attention.reason = Some(TaskAttentionReason::RequiresInput);
    old.updated_at = "2026-01-01T00:00:00Z".into();
    let mut recent = old.clone();
    recent.task_id = "recent".into();
    recent.attention.since = Some("2026-01-02T00:00:00Z".into());
    let mut failed = old.clone();
    failed.task_id = "failed".into();
    failed.attention.reason = Some(TaskAttentionReason::ExecutionFailed);
    let mut unread = old.clone();
    unread.task_id = "unread".into();
    unread.attention.reason = None;
    unread.attention.unread = true;
    let tasks = [failed, recent, unread, old];
    assert_eq!(
        attention_tasks(&tasks)
            .iter()
            .map(|t| t.task_id.as_str())
            .collect::<Vec<_>>(),
        ["old", "recent", "failed"]
    );
}
