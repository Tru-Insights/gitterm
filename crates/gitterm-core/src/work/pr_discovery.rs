//! Iced's recent delegated-task PR attribution, shared unchanged with Live.
use crate::{merge_plan, tasks::TaskStore};
use std::collections::HashMap;
const TASK_PR_MAX_AGE: chrono::Duration = chrono::Duration::days(14);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskBranch {
    pub repo: String,
    pub branch: String,
}

#[derive(Debug, Clone)]
pub struct TaskPrOrigin {
    pub chat: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub before: Option<chrono::DateTime<chrono::Utc>>,
}

/// Only recent task delegations can contribute branch PRs. The parent is
/// resolved to its current root when collecting the list, not when gh runs.
pub fn task_branches(
    store: &TaskStore,
    now: chrono::DateTime<chrono::Utc>,
    uid_to_chat: &HashMap<String, String>,
) -> Vec<(TaskPrOrigin, TaskBranch)> {
    store
        .delegations()
        .iter()
        .filter_map(|delegation| task_branch(store, delegation, now, uid_to_chat))
        .collect()
}

pub fn task_branch(
    store: &TaskStore,
    delegation: &crate::tasks::Delegation,
    now: chrono::DateTime<chrono::Utc>,
    uid_to_chat: &HashMap<String, String>,
) -> Option<(TaskPrOrigin, TaskBranch)> {
    let created = chrono::DateTime::parse_from_rfc3339(&delegation.created_at)
        .ok()?
        .with_timezone(&chrono::Utc);
    if created < now - TASK_PR_MAX_AGE || created > now {
        return None;
    }
    let crate::tasks::DelegationChild::TaskSession { task_id, .. } = &delegation.child else {
        return None;
    };
    let parent = delegation
        .parent
        .chat_session_id
        .as_ref()
        .or_else(|| uid_to_chat.get(&delegation.parent.session_uid))?;
    let task = store.get(task_id)?;
    let slug = crate::gh_identity::parse_repository_slug(task.repository.remote_url.as_deref()?)?;
    // PR identities and the watch currently support github.com only.
    if !slug.host.eq_ignore_ascii_case("github.com")
        || !merge_plan::valid_repo(&slug.full_name())
        || task.branch.is_empty()
    {
        return None;
    }
    // Reusing a task starts a new ownership window, even if the next parent
    // is no longer open. Later PRs must not leak back into the earlier chat.
    let before = store
        .delegations()
        .iter()
        .filter(|next| {
            matches!(
                &next.child,
                crate::tasks::DelegationChild::TaskSession { task_id: next_id, .. }
                    if next_id == task_id
            )
        })
        .filter_map(|next| chrono::DateTime::parse_from_rfc3339(&next.created_at).ok())
        .map(|at| at.with_timezone(&chrono::Utc))
        .filter(|at| *at > created && *at <= now)
        .min();
    Some((
        TaskPrOrigin {
            chat: parent.clone(),
            started_at: created,
            before,
        },
        TaskBranch {
            repo: slug.full_name(),
            branch: task.branch.clone(),
        },
    ))
}
