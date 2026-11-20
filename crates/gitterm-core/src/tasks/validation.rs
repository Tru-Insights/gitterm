//! Existing task input and persisted-record rules, without filesystem access.
use super::model::*;
use std::collections::HashSet;

pub fn validate_task_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("title is required".to_string());
    }
    Ok(())
}

pub fn validate_task_objective(objective: &str) -> Result<(), String> {
    if objective.trim().is_empty() {
        return Err("objective is required".to_string());
    }
    Ok(())
}

/// The manual form requires a base; MCP instead selects DEFAULT_TASK_BASE.
pub fn validate_base_reference(base: &str) -> Result<(), String> {
    if base.trim().is_empty() {
        return Err("Base reference is required".to_string());
    }
    Ok(())
}

pub fn is_linear_issue_key(value: &str) -> bool {
    let Some((team, number)) = value.split_once('-') else {
        return false;
    };
    (2..=10).contains(&team.len())
        && team
            .bytes()
            .next()
            .is_some_and(|character| character.is_ascii_uppercase())
        && team
            .bytes()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        && (1..=7).contains(&number.len())
        && number.bytes().all(|character| character.is_ascii_digit())
}

/// The store's existing lifecycle replacement rule.
pub fn validate_task_transition(previous: &TaskRecord, task: &TaskRecord) -> Result<(), String> {
    if !previous.lifecycle.can_transition_to(task.lifecycle) {
        return Err(format!(
            "task {} cannot transition from {:?} to {:?}",
            task.task_id, previous.lifecycle, task.lifecycle
        ));
    }
    Ok(())
}

/// The store contract, deliberately weaker than Git branch/issue validation.
pub fn validate_task(task: &TaskRecord) -> Result<(), String> {
    if task.schema_version != TASK_STORE_SCHEMA_VERSION {
        return Err(format!(
            "task {} has unsupported schema version {}; expected {}",
            task.task_id, task.schema_version, TASK_STORE_SCHEMA_VERSION
        ));
    }
    for (label, value) in [
        ("task id", task.task_id.as_str()),
        ("title", task.title.as_str()),
        ("objective", task.objective.as_str()),
        ("created_at timestamp", task.created_at.as_str()),
        ("updated_at timestamp", task.updated_at.as_str()),
        ("workspace name", task.workspace.name.as_str()),
        ("base reference", task.base.reference.as_str()),
        ("base commit", task.base.commit.as_str()),
        ("branch", task.branch.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("task {} has an empty {label}", task.task_id));
        }
    }
    if task.lifecycle == TaskLifecycle::Archived && task.archived_at.is_none() {
        return Err(format!(
            "task {} is archived without an archived_at timestamp",
            task.task_id
        ));
    }
    if task.lifecycle != TaskLifecycle::Archived && task.archived_at.is_some() {
        return Err(format!(
            "task {} has archived_at but is not archived",
            task.task_id
        ));
    }
    match (&task.worktree.state, &task.worktree.path) {
        (TaskWorktreeState::Ready, None) => {
            return Err(format!(
                "task {} has a ready worktree without a path",
                task.task_id
            ));
        }
        (TaskWorktreeState::Unprepared, Some(_)) => {
            return Err(format!(
                "task {} has an unprepared worktree with a path",
                task.task_id
            ));
        }
        _ => {}
    }
    let mut attempt_ids = HashSet::new();
    let mut active_attempts = Vec::new();
    for attempt in &task.attempts {
        if attempt.attempt_id.trim().is_empty() {
            return Err(format!("task {} has an empty attempt id", task.task_id));
        }
        if attempt.started_at.trim().is_empty() {
            return Err(format!(
                "task {} attempt {} has an empty started_at timestamp",
                task.task_id, attempt.attempt_id
            ));
        }
        if !attempt_ids.insert(attempt.attempt_id.as_str()) {
            return Err(format!(
                "task {} has duplicate attempt id {}",
                task.task_id, attempt.attempt_id
            ));
        }
        if attempt.state.is_active() {
            if attempt.ended_at.is_some() {
                return Err(format!(
                    "task {} active attempt {} has an ended_at timestamp",
                    task.task_id, attempt.attempt_id
                ));
            }
            active_attempts.push(attempt);
        } else if attempt.ended_at.is_none() {
            return Err(format!(
                "task {} inactive attempt {} has no ended_at timestamp",
                task.task_id, attempt.attempt_id
            ));
        }
    }
    if active_attempts.len() > 1 {
        return Err(format!(
            "task {} has {} active attempts",
            task.task_id,
            active_attempts.len()
        ));
    }
    if matches!(
        task.lifecycle,
        TaskLifecycle::Queued | TaskLifecycle::Running | TaskLifecycle::WaitingForInput
    ) && active_attempts.is_empty()
    {
        return Err(format!(
            "task {} is {:?} without an active attempt",
            task.task_id, task.lifecycle
        ));
    }
    match (&task.active_attempt_id, active_attempts.as_slice()) {
        (None, []) => {}
        (Some(active_id), [attempt]) if active_id == &attempt.attempt_id => {
            if attempt.executor != task.executor {
                return Err(format!(
                    "task {} active attempt executor does not match the task executor",
                    task.task_id
                ));
            }
            if task.lifecycle != attempt.state.lifecycle() {
                return Err(format!(
                    "task {} lifecycle {:?} does not match active attempt state {:?}",
                    task.task_id, task.lifecycle, attempt.state
                ));
            }
        }
        (Some(active_id), _) => {
            return Err(format!(
                "task {} active attempt id {} does not identify its one active attempt",
                task.task_id, active_id
            ));
        }
        (None, [_]) => {
            return Err(format!(
                "task {} has an active attempt without active_attempt_id",
                task.task_id
            ));
        }
        _ => unreachable!("more than one active attempt was rejected above"),
    }
    let mut task_session_ids = HashSet::new();
    let mut conversation_ids = HashSet::new();
    for session in &task.sessions {
        if session.task_session_id.trim().is_empty() {
            return Err(format!("task {} has an empty session id", task.task_id));
        }
        if session.label.trim().is_empty() {
            return Err(format!(
                "task {} session {} has an empty label",
                task.task_id, session.task_session_id
            ));
        }
        if session.created_at.trim().is_empty() || session.updated_at.trim().is_empty() {
            return Err(format!(
                "task {} session {} has an empty timestamp",
                task.task_id, session.task_session_id
            ));
        }
        if !task_session_ids.insert(session.task_session_id.as_str()) {
            return Err(format!(
                "task {} has duplicate session id {}",
                task.task_id, session.task_session_id
            ));
        }
        if let Some(conversation) = &session.conversation {
            if conversation.session_id.trim().is_empty() {
                return Err(format!(
                    "task {} session {} has an empty harness conversation id",
                    task.task_id, session.task_session_id
                ));
            }
            if !conversation_ids.insert((conversation.backend, conversation.session_id.as_str())) {
                return Err(format!(
                    "task {} links the same {:?} conversation {} more than once",
                    task.task_id, conversation.backend, conversation.session_id
                ));
            }
        }
    }
    if task
        .handoff
        .as_ref()
        .is_some_and(|handoff| handoff.summary.trim().is_empty())
    {
        return Err(format!(
            "task {} has an empty handoff summary",
            task.task_id
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_validation_remains_available_without_runtime_and_allows_adopted_branches() {
        let document: TaskStoreDocument = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/old.tasks.json"
        ))
        .unwrap();
        let mut task = document.tasks[0].clone();
        task.branch = "an-adopted-branch".into();
        validate_task(&task).unwrap();
        for field in ["title", "objective", "branch", "base"] {
            let mut invalid = task.clone();
            match field {
                "title" => invalid.title = " \n".into(),
                "objective" => invalid.objective = " ".into(),
                "branch" => invalid.branch.clear(),
                _ => invalid.base.reference.clear(),
            }
            assert!(validate_task(&invalid).unwrap_err().contains("empty"));
        }
    }

    #[test]
    fn form_predicates_preserve_existing_issue_and_required_field_rules() {
        assert!(validate_task_title(" \n").is_err());
        assert!(validate_task_objective("\t").is_err());
        assert!(validate_base_reference("").is_err());
        assert!(validate_task_title(" task ").is_ok());
        for valid in ["TRU-274", "A2-0", "ABCDEFGHIJ-1234567"] {
            assert!(is_linear_issue_key(valid), "{valid}");
        }
        for invalid in [
            "tru-274",
            "A-1",
            "2A-1",
            "ABCDEFGHIJK-1",
            "AB-12345678",
            "AB-",
            "AB-1-2",
            " AB-1",
        ] {
            assert!(!is_linear_issue_key(invalid), "{invalid}");
        }
    }
}

#[cfg(test)]
mod transition_fixture {
    use super::*;

    #[test]
    fn close_states_match_the_iced_store_contract() {
        let document: TaskStoreDocument = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/old.tasks.json"
        ))
        .unwrap();
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/close-states.json"
        ))
        .unwrap();
        for case in cases {
            let mut previous = document.tasks[0].clone();
            previous.lifecycle = serde_json::from_value(case["state"].clone()).unwrap();
            let mut next = previous.clone();
            next.lifecycle = TaskLifecycle::Archived;
            assert_eq!(
                validate_task_transition(&previous, &next).is_ok(),
                case["allowed"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
