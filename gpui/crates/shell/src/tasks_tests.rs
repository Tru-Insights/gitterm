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
