//! The brief a task session starts with, shared by Iced and GPUI.

use super::{HarnessConversationBackend, StoppingBoundary, TaskLifecycle, TaskRecord};

/// The brief a task session starts with. `report_back` is the worker
/// delegation this session carries out (TRU-142 S6): the brief then ends with
/// the Report back section naming it.
pub fn task_handoff_prompt(task: &TaskRecord, report_back: Option<&str>) -> String {
    let bullet_list = |items: &[String]| {
        if items.is_empty() {
            "- none recorded".to_string()
        } else {
            items
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let issue = task
        .issue
        .as_ref()
        .map(|issue| issue.key.as_str())
        .unwrap_or("none");
    let worktree = task
        .worktree
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not prepared".to_string());
    let prior_sessions = task
        .sessions
        .iter()
        .filter_map(|session| {
            session.conversation.as_ref().map(|conversation| {
                format!(
                    "- {}: {} ({})",
                    backend_label(conversation.backend),
                    session.label,
                    conversation.session_id
                )
            })
        })
        .collect::<Vec<_>>();
    let prior_sessions = if prior_sessions.is_empty() {
        "- none recorded".to_string()
    } else {
        prior_sessions.join("\n")
    };
    let last_error = task.last_error.as_deref().unwrap_or("none");
    let durable_handoff = task.handoff.as_ref().map_or_else(
        || "No prior handoff has been recorded.".to_string(),
        |handoff| {
            format!(
                "Summary:\n{}\n\nDecisions:\n{}\n\nNext steps:\n{}\n\nBlockers:\n{}",
                handoff.summary,
                bullet_list(&handoff.decisions),
                bullet_list(&handoff.next_steps),
                bullet_list(&handoff.blockers),
            )
        },
    );
    let brief = format!(
        "Continue GitTerm task {task_id}: {title}\n\n\
Objective:\n{objective}\n\n\
Task context:\n\
- Issue: {issue}\n\
- State: {state}\n\
- Worktree: {worktree}\n\
- Branch: {branch}\n\
- Base: {base_ref} @ {base_commit}\n\
- Stop when: {stopping_boundary}\n\
- Last error: {last_error}\n\n\
Latest durable handoff:\n{durable_handoff}\n\n\
Prior harness conversations:\n{prior_sessions}\n\n\
Work from the existing worktree. Inspect git status and the current diff before changing files. \
Use GitTerm's task_get tool for task {task_id} when it is available. Preserve valid prior work, \
surface conflicts or missing context explicitly, and use task_update_handoff to leave a concise durable \
summary before stopping when that tool is available.",
        task_id = task.task_id,
        title = task.title,
        objective = task.objective,
        state = lifecycle_label(task.lifecycle),
        branch = task.branch,
        base_ref = task.base.reference,
        base_commit = task.base.commit,
        stopping_boundary = boundary_label(task.stopping_boundary),
    );
    match report_back {
        Some(delegation_id) => format!(
            "{brief}\n\n{}",
            crate::delegations::report_back_section(delegation_id, &task.task_id)
        ),
        None => brief,
    }
}

// The three labels below are Iced's `task_lifecycle_label`,
// `stopping_boundary_label` and `conversation_backend_label` (src/main.rs).
fn lifecycle_label(lifecycle: TaskLifecycle) -> &'static str {
    match lifecycle {
        TaskLifecycle::Draft => "draft",
        TaskLifecycle::Preparing => "preparing",
        TaskLifecycle::Ready => "ready",
        TaskLifecycle::Queued => "queued",
        TaskLifecycle::Running => "running",
        TaskLifecycle::WaitingForInput => "waiting",
        TaskLifecycle::Completed => "completed",
        TaskLifecycle::Failed => "failed",
        TaskLifecycle::Stopped => "stopped",
        TaskLifecycle::Interrupted => "interrupted",
        TaskLifecycle::Archived => "archived",
    }
}

fn boundary_label(boundary: StoppingBoundary) -> &'static str {
    match boundary {
        StoppingBoundary::PlanOnly => "Plan only",
        StoppingBoundary::ImplementUntilTestsPass => "Tests pass",
        StoppingBoundary::PrepareDraftPr => "Draft PR",
    }
}

fn backend_label(backend: HarnessConversationBackend) -> &'static str {
    match backend {
        HarnessConversationBackend::Claude => "Claude",
        HarnessConversationBackend::Codex => "Codex",
        HarnessConversationBackend::Pi => "Pi",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{
        HarnessConversationRef, ObjectiveDeliveryState, TaskHandoff, TaskSessionRecord,
    };
    use std::path::Path;

    /// A task touching every branch of the brief: issue, last error, a
    /// handoff with an empty list, sessions with and without a conversation.
    pub(crate) fn rich_task() -> TaskRecord {
        let mut task = crate::delegation_test_support::test_task("task-7", Path::new("/wt/task-7"));
        task.branch = "task/task-7-export".to_string();
        task.lifecycle = TaskLifecycle::WaitingForInput;
        task.stopping_boundary = StoppingBoundary::PrepareDraftPr;
        task.last_error = Some("push rejected".to_string());
        task.handoff = Some(TaskHandoff {
            summary: "Export keeps images.".to_string(),
            decisions: vec!["Use data URIs".to_string(), "Keep the old path".to_string()],
            next_steps: vec![],
            blockers: vec!["Needs a fixture".to_string()],
            updated_by_session_id: None,
            updated_at: "2026-10-08T11:00:00Z".to_string(),
        });
        let session = |id: &str, label: &str, conversation: Option<HarnessConversationRef>| {
            TaskSessionRecord {
                task_session_id: id.to_string(),
                label: label.to_string(),
                harness: None,
                conversation,
                objective_delivery: ObjectiveDeliveryState::Unknown,
                launch_identity: None,
                created_at: "2026-10-08T10:00:00Z".to_string(),
                updated_at: "2026-10-08T10:00:00Z".to_string(),
            }
        };
        task.sessions = vec![
            session(
                "s1",
                "Claude",
                Some(HarnessConversationRef {
                    backend: HarnessConversationBackend::Claude,
                    session_id: "c-1".to_string(),
                }),
            ),
            session("s2", "Terminal", None),
            session(
                "s3",
                "Codex review",
                Some(HarnessConversationRef {
                    backend: HarnessConversationBackend::Codex,
                    session_id: "x-2".to_string(),
                }),
            ),
        ];
        task
    }

    /// Iced's `task_handoff_prompt` output for `rich_task`, captured from
    /// src/main.rs before the function moved here.
    const ICED_RICH: &str = r#"Continue GitTerm task task-7: Fix the Excalidraw export

Objective:
Make the export keep embedded images; stop when tests pass.

Task context:
- Issue: TRU-150
- State: waiting
- Worktree: /wt/task-7
- Branch: task/task-7-export
- Base: v6 @ 0123456789abcdef
- Stop when: Draft PR
- Last error: push rejected

Latest durable handoff:
Summary:
Export keeps images.

Decisions:
- Use data URIs
- Keep the old path

Next steps:
- none recorded

Blockers:
- Needs a fixture

Prior harness conversations:
- Claude: Claude (c-1)
- Codex: Codex review (x-2)

Work from the existing worktree. Inspect git status and the current diff before changing files. Use GitTerm's task_get tool for task task-7 when it is available. Preserve valid prior work, surface conflicts or missing context explicitly, and use task_update_handoff to leave a concise durable summary before stopping when that tool is available."#;

    #[test]
    fn handoff_prompt_matches_iced() {
        let task = rich_task();
        assert_eq!(task_handoff_prompt(&task, None), ICED_RICH);
        let worker = task_handoff_prompt(&task, Some("d-9"));
        assert_eq!(
            worker,
            format!(
                "{ICED_RICH}\n\n{}",
                crate::delegations::report_back_section("d-9", "task-7")
            )
        );
    }
}
