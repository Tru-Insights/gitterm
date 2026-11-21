use super::model::*;
use super::{validate_task, validate_task_transition};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct TaskStoreError {
    operation: &'static str,
    path: PathBuf,
    detail: String,
}

impl TaskStoreError {
    fn new(operation: &'static str, path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            operation,
            path: path.into(),
            detail: detail.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn operation(&self) -> &str {
        self.operation
    }
}

impl fmt::Display for TaskStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to {} task store {}: {}",
            self.operation,
            self.path.display(),
            self.detail
        )
    }
}

impl std::error::Error for TaskStoreError {}

#[derive(Debug)]
pub struct TaskStore {
    path: PathBuf,
    document: TaskStoreDocument,
    // An accepted prompt must not be resent just because its receipt could not
    // be written. Any successful store commit also persists these receipts.
    delivery_ack_pending: bool,
    /// See [`TaskStore::revision`].
    revision: u64,
    #[cfg(test)]
    write_count: std::cell::Cell<usize>,
}

/// Process-wide, so a reloaded store never repeats an earlier store's value.
static NEXT_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl TaskStore {
    pub fn path_for_config_root(config_root: &Path) -> PathBuf {
        config_root.join(TASKS_FILE_NAME)
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self, TaskStoreError> {
        let path = path.into();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    document: TaskStoreDocument::default(),
                    delivery_ack_pending: false,
                    revision: next_revision(),
                    #[cfg(test)]
                    write_count: std::cell::Cell::new(0),
                });
            }
            Err(error) => {
                return Err(TaskStoreError::new("read", &path, error.to_string()));
            }
        };
        let document: TaskStoreDocument = serde_json::from_slice(&bytes)
            .map_err(|error| TaskStoreError::new("decode", &path, error.to_string()))?;
        validate_document(&document)
            .map_err(|detail| TaskStoreError::new("validate", &path, detail))?;
        Ok(Self {
            path,
            document,
            delivery_ack_pending: false,
            revision: next_revision(),
            #[cfg(test)]
            write_count: std::cell::Cell::new(0),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Changes whenever the document does (load or commit), and differs
    /// between stores, so a reader can key what it derives from the tasks
    /// on it instead of rescanning them.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn tasks(&self) -> &[TaskRecord] {
        &self.document.tasks
    }

    pub fn get(&self, task_id: &str) -> Option<&TaskRecord> {
        self.document
            .tasks
            .iter()
            .find(|task| task.task_id == task_id)
    }

    /// Called after preset migration, when no configured Gemini preset remains.
    /// Historical executions and conversations keep their original provider.
    pub fn migrate_queued_gemini_presets(&mut self) -> Result<(), TaskStoreError> {
        let mut candidate = self.document.clone();
        let mut migrated = HashSet::new();
        let migrate = |harness: &mut Option<HarnessSelection>| {
            if let Some(HarnessSelection {
                kind: HarnessKind::TerminalPreset { preset_name },
                ..
            }) = harness
            {
                if preset_name == "Gemini" {
                    *preset_name = "Antigravity".into();
                    return true;
                }
            }
            false
        };
        for task in &mut candidate.tasks {
            if task.lifecycle == TaskLifecycle::Queued && migrate(&mut task.harness) {
                migrated.insert(task.task_id.clone());
                for attempt in &mut task.attempts {
                    if attempt.state == AttemptState::Queued {
                        migrate(&mut attempt.harness);
                    }
                }
            }
        }
        if migrated.is_empty() {
            return Ok(());
        }
        for delegation in &mut candidate.delegations {
            if delegation.status.accepts_completion() {
                if let DelegationChild::TaskSession {
                    task_id,
                    task_session_id: None,
                    preset_name: Some(name),
                    ..
                } = &mut delegation.child
                {
                    if migrated.contains(task_id) && name == "Gemini" {
                        *name = "Antigravity".into();
                    }
                }
            }
        }
        self.commit_candidate(candidate)
    }

    pub fn insert(&mut self, task: TaskRecord) -> Result<(), TaskStoreError> {
        if self.get(&task.task_id).is_some() {
            return Err(TaskStoreError::new(
                "insert",
                &self.path,
                format!("task {} already exists", task.task_id),
            ));
        }
        let mut candidate = self.document.clone();
        candidate.tasks.push(task);
        self.commit_candidate(candidate)
    }

    pub fn replace(&mut self, task: TaskRecord) -> Result<(), TaskStoreError> {
        self.replace_many(vec![task])
    }

    /// Validate and persist related replacements in one atomic store write.
    /// On failure neither the in-memory document nor the file is replaced.
    pub fn replace_many(&mut self, tasks: Vec<TaskRecord>) -> Result<(), TaskStoreError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let mut candidate = self.document.clone();
        for task in tasks {
            let Some(index) = candidate
                .tasks
                .iter()
                .position(|existing| existing.task_id == task.task_id)
            else {
                return Err(TaskStoreError::new(
                    "replace",
                    &self.path,
                    format!("task {} does not exist", task.task_id),
                ));
            };
            let previous = &candidate.tasks[index];
            validate_task_transition(previous, &task)
                .map_err(|detail| TaskStoreError::new("replace", &self.path, detail))?;
            candidate.tasks[index] = task;
        }
        self.commit_candidate(candidate)
    }

    pub fn archive(&mut self, task_id: &str, timestamp: &str) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "archive",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.lifecycle = TaskLifecycle::Archived;
        task.archived_at = Some(timestamp.to_string());
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn begin_worktree_preparation(
        &mut self,
        task_id: &str,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "begin worktree preparation in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.lifecycle = TaskLifecycle::Preparing;
        task.worktree = TaskWorktree {
            state: TaskWorktreeState::Preparing,
            path: None,
        };
        task.last_error = None;
        task.attention = TaskAttention::default();
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn complete_worktree_preparation(
        &mut self,
        task_id: &str,
        prepared: CompletedWorktreePreparation,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        if !prepared.path.is_absolute() {
            return Err(TaskStoreError::new(
                "complete worktree preparation in",
                &self.path,
                format!(
                    "task {task_id} prepared worktree path {} is not absolute",
                    prepared.path.display()
                ),
            ));
        }
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "complete worktree preparation in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.repository = prepared.repository;
        task.base = prepared.base;
        task.branch = prepared.branch;
        task.worktree = TaskWorktree {
            state: TaskWorktreeState::Ready,
            path: Some(prepared.path),
        };
        task.lifecycle = TaskLifecycle::Ready;
        task.last_error = None;
        task.attention = TaskAttention::default();
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    /// Clear a task's worktree record after cleanup removed the working copy.
    /// Sessions and their conversation references are deliberately untouched —
    /// transcripts must stay auditable after the working copy is gone, and a
    /// re-prepared worktree makes them resumable again.
    pub fn clear_worktree(&mut self, task_id: &str, timestamp: &str) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "clear worktree in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.worktree = TaskWorktree::default();
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    /// Records that a prepared worktree no longer exists on disk — removed
    /// outside GitTerm (an agent ran `git worktree remove`, a manual
    /// cleanup). The path is kept so the overview can still name it.
    pub fn mark_worktree_missing(
        &mut self,
        task_id: &str,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "mark worktree missing in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        if task.worktree.state == TaskWorktreeState::Missing {
            return Ok(());
        }
        task.worktree.state = TaskWorktreeState::Missing;
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn fail_worktree_preparation(
        &mut self,
        task_id: &str,
        detail: String,
        cleanup_path: Option<PathBuf>,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "fail worktree preparation in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.worktree = TaskWorktree {
            state: if cleanup_path.is_some() {
                TaskWorktreeState::CleanupRequired
            } else {
                TaskWorktreeState::Unprepared
            },
            path: cleanup_path,
        };
        task.lifecycle = TaskLifecycle::Failed;
        task.last_error = Some(detail);
        task.attention = TaskAttention::raised(
            TaskAttentionReason::ExecutionFailed,
            &task.attention,
            timestamp,
        );
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn record_resumable_error(
        &mut self,
        task_id: &str,
        detail: String,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "record resumable error in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.last_error = Some(detail);
        task.attention = TaskAttention::raised(
            TaskAttentionReason::ExecutionFailed,
            &task.attention,
            timestamp,
        );
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn upsert_session(
        &mut self,
        task_id: &str,
        session: TaskSessionRecord,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "record session in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        if session.harness.is_some() {
            task.harness = session.harness.clone();
        }
        if let Some(existing) = task
            .sessions
            .iter_mut()
            .find(|existing| existing.task_session_id == session.task_session_id)
        {
            *existing = session;
        } else {
            task.sessions.push(session);
        }
        let starts: Vec<String> = task
            .sessions
            .iter()
            .map(|session| session.created_at.clone())
            .collect();
        let progress = task.progress.get_or_insert_with(TaskProgress::default);
        progress.last_activity_at = clamp_activity_to_session_start(
            progress.last_activity_at.as_deref(),
            starts.iter().map(String::as_str),
        );
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    /// Remember the harness a deferred launch asked for, so a queue entry
    /// that survives a restart relaunches with the same preset instead of
    /// falling back to the first configured one. No lifecycle change — the
    /// Queued signal follows separately.
    pub fn record_requested_harness(
        &mut self,
        task_id: &str,
        harness: HarnessSelection,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "record requested harness in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        task.harness = Some(harness);
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    /// Record what a task has delivered (latest commit, discovered PR).
    /// Unchanged state is a no-op — discovery runs on every overview visit,
    /// and rewriting an identical record would churn `updated_at` (which
    /// drives rail ordering) and the on-disk file for nothing.
    pub fn record_delivery(
        &mut self,
        task_id: &str,
        delivery: DeliveryState,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "record delivery in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        if task.delivery == delivery {
            return Ok(());
        }
        task.delivery = delivery;
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn update_handoff(
        &mut self,
        task_id: &str,
        handoff: TaskHandoff,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "update handoff in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        if handoff.summary.trim().is_empty() {
            return Err(TaskStoreError::new(
                "update handoff in",
                &self.path,
                format!("task {task_id} handoff summary is empty"),
            ));
        }
        task.handoff = Some(handoff);
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    pub fn begin_attempt(
        &mut self,
        task_id: &str,
        attempt: TaskExecutionAttempt,
        lifecycle: TaskLifecycle,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        if !attempt.state.is_active() {
            return Err(TaskStoreError::new(
                "begin attempt in",
                &self.path,
                format!("attempt {} is not active", attempt.attempt_id),
            ));
        }
        if !lifecycle.is_active() || attempt.state.lifecycle() != lifecycle {
            return Err(TaskStoreError::new(
                "begin attempt in",
                &self.path,
                format!(
                    "attempt {} state {:?} does not match active task lifecycle {:?}",
                    attempt.attempt_id, attempt.state, lifecycle
                ),
            ));
        }
        let mut task = self.get(task_id).cloned().ok_or_else(|| {
            TaskStoreError::new(
                "begin attempt in",
                &self.path,
                format!("task {task_id} does not exist"),
            )
        })?;
        if task.active_attempt_id.is_some()
            || task.attempts.iter().any(|item| item.state.is_active())
        {
            return Err(TaskStoreError::new(
                "begin attempt in",
                &self.path,
                format!("task {task_id} already has an active attempt"),
            ));
        }
        task.active_attempt_id = Some(attempt.attempt_id.clone());
        task.executor = attempt.executor.clone();
        task.harness = attempt.harness.clone();
        task.attempts.push(attempt);
        task.lifecycle = lifecycle;
        task.last_error = None;
        task.updated_at = timestamp.to_string();
        self.replace(task)
    }

    /// Apply a lifecycle observation derived from a live session (harness
    /// stream state, terminal-title heuristics, session end). Unlike
    /// `replace`, a signal may hop through an unobserved `Running`: an agent
    /// asking for input — or erroring — from a `Ready` task necessarily ran
    /// first, even if the start itself was never observed. From a state that
    /// already records an outcome (`Failed`, `Stopped`, ...), only a *live*
    /// signal may hop (the session demonstrably resumed); an outcome signal
    /// gets no hop there, so a worse outcome (`Failed`) is never overwritten
    /// by a later `Completed` from a sibling session. Attempt records are
    /// maintained alongside: entering an active state opens (or updates) the
    /// active attempt, an outcome closes it. Invalid signals are errors for
    /// the caller to log — never silently applied.
    ///
    /// Returns whether the record changed.
    pub fn record_lifecycle_signal(
        &mut self,
        task_id: &str,
        target: TaskLifecycle,
        last_error: Option<String>,
        timestamp: &str,
    ) -> Result<bool, TaskStoreError> {
        let Some(index) = self
            .document
            .tasks
            .iter()
            .position(|task| task.task_id == task_id)
        else {
            return Err(TaskStoreError::new(
                "record lifecycle signal in",
                &self.path,
                format!("task {task_id} does not exist"),
            ));
        };
        let current = self.document.tasks[index].lifecycle;
        if current == target && last_error.is_none() {
            return Ok(false);
        }
        let direct = current.can_transition_to(target);
        let current_is_outcome = matches!(
            current,
            TaskLifecycle::Completed
                | TaskLifecycle::Failed
                | TaskLifecycle::Stopped
                | TaskLifecycle::Interrupted
        );
        let via_running = (target.is_active() || !current_is_outcome)
            && current.can_transition_to(TaskLifecycle::Running)
            && TaskLifecycle::Running.can_transition_to(target);
        if !(direct || via_running) {
            return Err(TaskStoreError::new(
                "record lifecycle signal in",
                &self.path,
                format!(
                    "task {task_id} session signal cannot transition {current:?} to {target:?}"
                ),
            ));
        }
        let mut candidate = self.document.clone();
        let task = &mut candidate.tasks[index];
        task.lifecycle = target;
        if let Some(state) = AttemptState::for_lifecycle(target) {
            let active_index = task.attempts.iter().position(|item| item.state.is_active());
            match (state.is_active(), active_index) {
                (true, Some(existing)) => {
                    task.attempts[existing].state = state;
                }
                (true, None) => {
                    let attempt_id = uuid::Uuid::new_v4().simple().to_string();
                    task.attempts.push(TaskExecutionAttempt {
                        attempt_id: attempt_id.clone(),
                        executor: task.executor.clone(),
                        harness: task.harness.clone(),
                        state,
                        started_at: timestamp.to_string(),
                        ended_at: None,
                        session_ref: None,
                        failure: None,
                    });
                    task.active_attempt_id = Some(attempt_id);
                }
                (false, Some(existing)) => {
                    let attempt = &mut task.attempts[existing];
                    attempt.state = state;
                    attempt.ended_at = Some(timestamp.to_string());
                    if state == AttemptState::Failed {
                        attempt.failure = last_error.clone();
                    }
                    task.active_attempt_id = None;
                }
                (false, None) if current != target => {
                    // The hop case: the run happened unobserved and is already
                    // over. Record it as an attempt that started and ended at
                    // the moment we learned about it.
                    task.attempts.push(TaskExecutionAttempt {
                        attempt_id: uuid::Uuid::new_v4().simple().to_string(),
                        executor: task.executor.clone(),
                        harness: task.harness.clone(),
                        state,
                        started_at: timestamp.to_string(),
                        ended_at: Some(timestamp.to_string()),
                        session_ref: None,
                        failure: (state == AttemptState::Failed)
                            .then(|| last_error.clone())
                            .flatten(),
                    });
                }
                (false, None) => {}
            }
        } else if let Some(existing) = task.attempts.iter().position(|item| item.state.is_active())
        {
            // A between-runs target (Ready via queue cancel) closes the
            // attempt that never got to run.
            let attempt = &mut task.attempts[existing];
            attempt.state = AttemptState::Stopped;
            attempt.ended_at = Some(timestamp.to_string());
            task.active_attempt_id = None;
        }
        let has_new_detail = last_error.is_some();
        if let Some(detail) = last_error {
            task.last_error = Some(detail);
        }
        if target == TaskLifecycle::Failed {
            task.attention = TaskAttention::raised(
                TaskAttentionReason::ExecutionFailed,
                &task.attention,
                timestamp,
            );
        } else if target == TaskLifecycle::Completed && current != target {
            // Completion is attention until the task is visited — the visit
            // (not a glance at the rail) acknowledges it. The current==target
            // guard keeps a repeated completion signal from re-marking a task
            // the user already read.
            task.attention = TaskAttention::raised(
                TaskAttentionReason::CompletedUnread,
                &task.attention,
                timestamp,
            );
        } else if target.is_active() {
            // The task is observably running again, so any failure/outcome
            // attention from a previous attempt is superseded — otherwise the
            // rail keeps shouting "Failed" over a healthy resumed session.
            // The old detail stays recorded on the closed attempt's `failure`.
            task.attention = TaskAttention::default();
            if !has_new_detail {
                task.last_error = None;
            }
        }
        task.updated_at = timestamp.to_string();
        if current == TaskLifecycle::Queued && target == TaskLifecycle::Failed {
            let message = task
                .last_error
                .clone()
                .unwrap_or_else(|| "Queued worker could not start".into());
            for delegation in &mut candidate.delegations {
                if delegation.status.accepts_completion()
                    && matches!(&delegation.child, DelegationChild::TaskSession {
                        task_id: child, ..
                    } if child == task_id)
                {
                    delegation.status = DelegationStatus::Failed {
                        message: message.clone(),
                    };
                    delegation.updated_at = timestamp.to_string();
                    delegation.record_worker_failure(timestamp);
                }
            }
        }
        self.commit_candidate(candidate)?;
        Ok(true)
    }

    /// Persist a progress snapshot (and optionally refreshed changed-file
    /// counts). Returns `Ok(false)` without touching the file when nothing
    /// differs — callers flush on a timer and must be able to fire this
    /// repeatedly without churning `tasks.json`.
    pub fn update_progress(
        &mut self,
        task_id: &str,
        mut progress: TaskProgress,
        changes: Option<ChangedFilesSummary>,
        timestamp: &str,
    ) -> Result<bool, TaskStoreError> {
        let Some(index) = self
            .document
            .tasks
            .iter()
            .position(|task| task.task_id == task_id)
        else {
            return Err(TaskStoreError::new(
                "update progress in",
                &self.path,
                format!("task {task_id} does not exist"),
            ));
        };
        let current = &self.document.tasks[index];
        progress.last_activity_at = clamp_activity_to_session_start(
            progress.last_activity_at.as_deref(),
            current
                .sessions
                .iter()
                .map(|session| session.created_at.as_str()),
        );
        let progress_unchanged = current.progress.as_ref() == Some(&progress);
        let changes_unchanged = changes
            .as_ref()
            .is_none_or(|summary| *summary == current.changes);
        if progress_unchanged && changes_unchanged {
            return Ok(false);
        }
        let mut candidate = self.document.clone();
        let task = &mut candidate.tasks[index];
        task.progress = Some(progress);
        if let Some(summary) = changes {
            task.changes = summary;
        }
        task.updated_at = timestamp.to_string();
        self.commit_candidate(candidate)?;
        Ok(true)
    }

    /// The user visited the task: clear the unread badge, and drop a
    /// `CompletedUnread` reason entirely — completion attention exists only
    /// until it is seen. State-backed reasons (a failure, an interruption)
    /// survive the visit; they clear when the underlying state resolves.
    /// Returns `Ok(false)` without touching the file when nothing changes —
    /// callers fire this on every visit and periodic tick.
    pub fn acknowledge_attention(
        &mut self,
        task_id: &str,
        timestamp: &str,
    ) -> Result<bool, TaskStoreError> {
        let Some(index) = self
            .document
            .tasks
            .iter()
            .position(|task| task.task_id == task_id)
        else {
            return Err(TaskStoreError::new(
                "acknowledge attention in",
                &self.path,
                format!("task {task_id} does not exist"),
            ));
        };
        let current = &self.document.tasks[index];
        let clear_reason = current.attention.reason == Some(TaskAttentionReason::CompletedUnread);
        if !current.attention.unread && !clear_reason {
            return Ok(false);
        }
        let mut candidate = self.document.clone();
        let task = &mut candidate.tasks[index];
        task.attention.unread = false;
        if clear_reason {
            task.attention.reason = None;
        }
        task.updated_at = timestamp.to_string();
        self.commit_candidate(candidate)?;
        Ok(true)
    }

    /// The user explicitly dismissed the task's attention from the inbox:
    /// drop the reason and the unread badge outright, state-backed or not.
    /// Stronger than [`Self::acknowledge_attention`] — a dismissal is a
    /// deliberate "stop flagging this"; the underlying lifecycle still tells
    /// the truth in the task rail. Returns `Ok(false)` without touching the
    /// file when there is nothing to dismiss.
    pub fn dismiss_attention(
        &mut self,
        task_id: &str,
        timestamp: &str,
    ) -> Result<bool, TaskStoreError> {
        let Some(index) = self
            .document
            .tasks
            .iter()
            .position(|task| task.task_id == task_id)
        else {
            return Err(TaskStoreError::new(
                "dismiss attention in",
                &self.path,
                format!("task {task_id} does not exist"),
            ));
        };
        let current = &self.document.tasks[index];
        if current.attention.reason.is_none() && !current.attention.unread {
            return Ok(false);
        }
        let mut candidate = self.document.clone();
        let task = &mut candidate.tasks[index];
        task.attention = TaskAttention::default();
        task.updated_at = timestamp.to_string();
        self.commit_candidate(candidate)?;
        Ok(true)
    }

    /// Marks local executions that were active when GitTerm stopped as
    /// interrupted, and in the same write turns requested or running Codex
    /// delegations into `Interrupted` (no runner survives a restart). A
    /// worker delegation whose task still exists is left as it was: the
    /// worker's tab comes back and may still report (TRU-142 S6). The
    /// returned count is tasks only; see `interrupted_delegations` for the
    /// delegations this touched.
    pub fn reconcile_after_restart(&mut self, timestamp: &str) -> Result<usize, TaskStoreError> {
        let mut candidate = self.document.clone();
        let interrupted_delegations = reconcile_delegations_in(&mut candidate, timestamp);
        let mut reconciled = 0;
        for task in &mut candidate.tasks {
            if task.executor != ExecutorTarget::Local || !task.lifecycle.is_active() {
                continue;
            }
            // A queued task had nothing running to interrupt — it stays
            // queued and rejoins the launch queue on startup.
            if task.lifecycle == TaskLifecycle::Queued {
                continue;
            }
            for attempt in &mut task.attempts {
                if attempt.executor == ExecutorTarget::Local && attempt.state.is_active() {
                    attempt.state = AttemptState::Interrupted;
                    attempt.ended_at = Some(timestamp.to_string());
                    attempt.failure =
                        Some("GitTerm restarted while this local execution was active".to_string());
                }
            }
            task.active_attempt_id = None;
            task.lifecycle = TaskLifecycle::Interrupted;
            task.updated_at = timestamp.to_string();
            task.attention =
                TaskAttention::raised(TaskAttentionReason::Interrupted, &task.attention, timestamp);
            task.last_error =
                Some("GitTerm restarted while this local execution was active".to_string());
            reconciled += 1;
        }
        if reconciled > 0 || interrupted_delegations > 0 {
            self.commit_candidate(candidate)?;
        }
        Ok(reconciled)
    }

    /// Delegations interrupted by a restart, newest first, for the parent
    /// tab or inbox to offer Re-run.
    pub fn interrupted_delegations(&self) -> Vec<&Delegation> {
        self.document
            .delegations
            .iter()
            .rev()
            .filter(|delegation| delegation.status == DelegationStatus::Interrupted)
            .collect()
    }

    pub fn delegations(&self) -> &[Delegation] {
        &self.document.delegations
    }

    pub fn delegation(&self, delegation_id: &str) -> Option<&Delegation> {
        self.document
            .delegations
            .iter()
            .find(|delegation| delegation.delegation_id == delegation_id)
    }

    /// Restores the original timer, including legacy records written before
    /// deadlines were persisted. A missing/malformed legacy start is an error.
    pub fn worker_time_limit(
        &self,
        delegation_id: &str,
    ) -> Result<Option<WorkerTimeLimit>, String> {
        let delegation = self
            .delegation(delegation_id)
            .ok_or_else(|| format!("delegation {delegation_id} does not exist"))?;
        let DelegationChild::TaskSession {
            task_id,
            task_session_id: Some(session_id),
            ..
        } = &delegation.child
        else {
            return Ok(None);
        };
        let Some(minutes) = delegation.options.time_limit_minutes else {
            return Ok(None);
        };
        if !delegation.status.is_active() {
            return Ok(None);
        }
        let deadline_unix_ms = match delegation.time_limit_deadline_unix_ms {
            Some(deadline) => deadline,
            None => {
                let session = self
                    .get(task_id)
                    .and_then(|task| {
                        task.sessions.iter().find(|session| &session.task_session_id == session_id)
                    })
                    .ok_or_else(|| {
                        format!("delegation {delegation_id}: task {task_id} has no worker session {session_id} to restore its time limit")
                    })?;
                worker_deadline(&session.created_at, minutes)
                    .map_err(|error| format!("delegation {delegation_id}: {error}"))?
            }
        };
        Ok(Some(WorkerTimeLimit {
            delegation_id: delegation_id.to_string(),
            session_id: session_id.clone(),
            deadline_unix_ms,
        }))
    }

    /// Delegations requested by one parent session, newest first. The store
    /// appends on insert, so newest means most recently inserted.
    pub fn delegations_for_parent(&self, session_uid: &str) -> Vec<&Delegation> {
        self.document
            .delegations
            .iter()
            .rev()
            .filter(|delegation| delegation.parent.session_uid == session_uid)
            .collect()
    }

    pub fn insert_delegation(&mut self, delegation: Delegation) -> Result<(), TaskStoreError> {
        let id = delegation.delegation_id.as_str();
        if self.delegation(id).is_some() {
            return Err(TaskStoreError::new(
                "insert delegation into",
                &self.path,
                format!("delegation {id} already exists"),
            ));
        }
        if !delegation.status.is_active()
            || delegation.result.is_some()
            || delegation.is_delivered()
            || delegation.dismissed_at.is_some()
        {
            return Err(TaskStoreError::new(
                "insert delegation into",
                &self.path,
                format!(
                    "delegation {id} must be inserted requested or running, without a result or delivery; it is {}",
                    delegation.status.label()
                ),
            ));
        }
        if let Some(previous) = delegation.previous.as_deref() {
            if self.delegation(previous).is_none() {
                return Err(TaskStoreError::new(
                    "insert delegation into",
                    &self.path,
                    format!("delegation {id} follows unknown delegation {previous}"),
                ));
            }
        }
        if let DelegationChild::TaskSession { task_id, .. } = &delegation.child {
            if self.get(task_id).is_none() {
                return Err(TaskStoreError::new(
                    "insert delegation into",
                    &self.path,
                    format!("delegation {id} names unknown task {task_id}"),
                ));
            }
        }
        let mut candidate = self.document.clone();
        if let DelegationChild::TaskSession { task_id, .. } = &delegation.child {
            for previous in &mut candidate.delegations {
                if previous.status == DelegationStatus::Blocked
                    && matches!(&previous.child, DelegationChild::TaskSession { task_id: child, .. } if child == task_id)
                {
                    previous.status = DelegationStatus::Completed;
                    previous.superseded_by = Some(delegation.delegation_id.clone());
                    previous.updated_at = delegation.created_at.clone();
                }
            }
        }
        candidate.delegations.push(delegation);
        self.commit_candidate(candidate)
    }

    /// Moves a delegation through its non-terminal lifecycle. `Completed`
    /// is rejected here: use `complete_delegation`, which carries the result.
    pub fn set_delegation_status(
        &mut self,
        delegation_id: &str,
        status: DelegationStatus,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "set delegation status in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        if !delegation.status.can_transition_to(&status) {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} cannot move from {} to {}",
                    delegation.status.label(),
                    status.label()
                ),
            ));
        }
        delegation.status = status;
        delegation.record_worker_failure(timestamp);
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    /// GitTerm stopped the job at its time limit (TRU-212): the delegation
    /// fails with `message` and remembers when the limit was reached.
    pub fn stop_delegation_at_time_limit(
        &mut self,
        delegation_id: &str,
        message: String,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "stop a delegation at its time limit in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        let status = DelegationStatus::Failed { message };
        if !delegation.status.can_transition_to(&status) {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} is {} and cannot be stopped",
                    delegation.status.label()
                ),
            ));
        }
        delegation.status = status;
        delegation.record_worker_failure(timestamp);
        delegation.time_limit_reached_at = Some(timestamp.to_string());
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    /// Records the child's result. A delegation that already completed,
    /// failed or was cancelled is an error, never a silent overwrite.
    pub fn complete_delegation(
        &mut self,
        delegation_id: &str,
        result: DelegationResult,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "complete delegation in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        if !delegation.status.accepts_completion() {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} is already {}",
                    delegation.status.label()
                ),
            ));
        }
        if delegation.status == DelegationStatus::Blocked {
            delegation.dismissed_at = None;
        }
        // Delivery belongs to the report, not the worker's whole lifetime.
        delegation.delivered = false;
        delegation.delivered_at = None;
        delegation.status = DelegationStatus::Completed;
        delegation.result = Some(result);
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    /// A worker reported itself blocked (S6): the delegation becomes
    /// `Blocked` with the worker's handoff as its result. Reporting blocked
    /// again replaces the card's result and makes the new report sendable.
    /// Only implementation delegations block.
    pub fn block_delegation(
        &mut self,
        delegation_id: &str,
        result: DelegationResult,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "block delegation in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        if delegation.kind != DelegationKind::Implement {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} is a {}, and only workers report blocked",
                    delegation.kind.label()
                ),
            ));
        }
        if !delegation.status.accepts_completion() {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} is already {}",
                    delegation.status.label()
                ),
            ));
        }
        delegation.status = DelegationStatus::Blocked;
        delegation.result = Some(result);
        delegation.delivered = false;
        delegation.delivered_at = None;
        delegation.dismissed_at = None;
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    /// The task session that carries out an implementation delegation
    /// started (S6): it is recorded on the child, and a requested or
    /// interrupted delegation becomes `Running`. A worker relaunched after
    /// its tab was closed attaches its new session the same way.
    pub fn attach_worker_session(
        &mut self,
        delegation_id: &str,
        task_session_id: &str,
        conversation: Option<HarnessConversationRef>,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "attach a worker session in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        let DelegationChild::TaskSession {
            task_id,
            task_session_id: session_slot,
            conversation: conversation_slot,
            ..
        } = &mut delegation.child
        else {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!("delegation {delegation_id} has no task-session worker"),
            ));
        };
        let session = self
            .get(task_id)
            .and_then(|task| {
                task.sessions
                    .iter()
                    .find(|session| session.task_session_id == task_session_id)
            })
            .ok_or_else(|| {
                TaskStoreError::new(
                    OPERATION,
                    &self.path,
                    format!("task {task_id} has no session {task_session_id}"),
                )
            })?;
        if session_slot.as_deref() != Some(task_session_id)
            || delegation.time_limit_deadline_unix_ms.is_none()
        {
            let start = if session_slot.as_deref() == Some(task_session_id) {
                session.created_at.as_str()
            } else {
                timestamp
            };
            delegation.time_limit_deadline_unix_ms = delegation
                .options
                .time_limit_minutes
                .map(|minutes| worker_deadline(start, minutes))
                .transpose()
                .map_err(|detail| TaskStoreError::new(OPERATION, &self.path, detail))?;
        }
        *session_slot = Some(task_session_id.to_string());
        *conversation_slot = conversation;
        if matches!(
            delegation.status,
            DelegationStatus::Requested | DelegationStatus::Interrupted
        ) {
            delegation.status = DelegationStatus::Running;
        }
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    /// Implementation delegations still open on `task_id` (a worker may
    /// still report: requested, running, interrupted or blocked), newest
    /// first.
    pub fn open_worker_delegations(&self, task_id: &str) -> Vec<&Delegation> {
        self.document
            .delegations
            .iter()
            .rev()
            .filter(|delegation| delegation.status.accepts_completion())
            .filter(|delegation| {
                matches!(
                    &delegation.child,
                    DelegationChild::TaskSession { task_id: child, .. } if child == task_id
                )
            })
            .collect()
    }

    /// Match the session, not the whole task: user-opened siblings restore.
    /// Cancelled means Stop tracking, so its terminal stays user-owned.
    pub fn worker_session_is_final(&self, task_id: &str, session_id: Option<&str>) -> bool {
        if session_id.is_none() {
            return false;
        }
        self.document.delegations.iter().rev().find(|delegation| {
            matches!(&delegation.child, DelegationChild::TaskSession { task_id: child, task_session_id, .. }
                if child == task_id && task_session_id.as_deref() == session_id)
        }).is_some_and(|delegation| matches!(delegation.status, DelegationStatus::Completed | DelegationStatus::Failed { .. }))
    }

    pub fn task_worktree_missing(&self, task_id: &str) -> bool {
        self.get(task_id).is_some_and(|task| {
            task.executor == ExecutorTarget::Local
                && task
                    .worktree
                    .path
                    .as_ref()
                    .is_some_and(|path| !path.is_dir())
        })
    }

    /// Marks an accepted batch in memory, then persists it atomically. A write
    /// failure leaves the receipt in memory for retry without another prompt.
    pub fn mark_delegations_delivered(
        &mut self,
        delegation_ids: &[String],
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "mark delegations delivered in";
        let mut candidate = self.document.clone();
        for id in delegation_ids {
            let delegation = candidate
                .delegations
                .iter_mut()
                .find(|delegation| delegation.delegation_id == *id)
                .ok_or_else(|| {
                    TaskStoreError::new(
                        OPERATION,
                        &self.path,
                        format!("delegation {id} does not exist"),
                    )
                })?;
            if delegation.is_delivered() {
                return Err(TaskStoreError::new(
                    OPERATION,
                    &self.path,
                    format!(
                        "delegation {id} was already delivered at {}",
                        delegation
                            .delivered_at
                            .as_deref()
                            .unwrap_or("an earlier delivery")
                    ),
                ));
            }
            if !delegation.status.is_sendable() {
                return Err(TaskStoreError::new(
                    OPERATION,
                    &self.path,
                    format!(
                        "delegation {id} is {}, not a final outcome",
                        delegation.status.label()
                    ),
                ));
            }
            delegation.delivered = true;
            if delegation.status.has_result() {
                delegation.delivered_at = Some(timestamp.to_string());
            }
            delegation.updated_at = timestamp.to_string();
        }
        validate_document(&candidate)
            .map_err(|detail| TaskStoreError::new("validate", &self.path, detail))?;
        self.document = candidate;
        self.revision = next_revision();
        self.delivery_ack_pending = true;
        self.retry_delivery_acknowledgements()
    }

    pub fn retry_delivery_acknowledgements(&mut self) -> Result<(), TaskStoreError> {
        if self.delivery_ack_pending {
            self.write_document_with(&self.document, |temporary, destination| {
                replace_file_atomically(temporary, destination)
            })?;
            self.delivery_ack_pending = false;
        }
        Ok(())
    }

    pub fn mark_delegation_delivered(
        &mut self,
        delegation_id: &str,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        self.mark_delegations_delivered(&[delegation_id.to_string()], timestamp)
    }

    pub fn cancel_delegation(
        &mut self,
        delegation_id: &str,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        self.set_delegation_status(delegation_id, DelegationStatus::Cancelled, timestamp)
    }

    /// A Codex run started: the delegation becomes `Running` and records the
    /// run's thread id. Only `Requested` delegations with a Codex child start.
    pub fn mark_delegation_started(
        &mut self,
        delegation_id: &str,
        thread_id: &str,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "mark delegation started in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        if delegation.status != DelegationStatus::Requested {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} is {}, not requested",
                    delegation.status.label()
                ),
            ));
        }
        match &mut delegation.child {
            DelegationChild::CodexReview {
                thread_id: slot, ..
            }
            | DelegationChild::CodexConsult {
                thread_id: slot, ..
            } => {
                *slot = Some(thread_id.to_string());
            }
            other => {
                return Err(TaskStoreError::new(
                    OPERATION,
                    &self.path,
                    format!("delegation {delegation_id} has no Codex run ({other:?})"),
                ));
            }
        }
        delegation.status = DelegationStatus::Running;
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    /// The human dismissed a finished delegation's card. Dismissing twice is
    /// a no-op that keeps the first time; an active delegation must be
    /// cancelled first.
    pub fn dismiss_delegation(
        &mut self,
        delegation_id: &str,
        timestamp: &str,
    ) -> Result<(), TaskStoreError> {
        const OPERATION: &str = "dismiss delegation in";
        let (index, mut delegation) = self.delegation_for_update(OPERATION, delegation_id)?;
        if delegation.status.is_active() {
            return Err(TaskStoreError::new(
                OPERATION,
                &self.path,
                format!(
                    "delegation {delegation_id} is still {}; cancel it first",
                    delegation.status.label()
                ),
            ));
        }
        if delegation.dismissed_at.is_some() {
            return Ok(());
        }
        delegation.dismissed_at = Some(timestamp.to_string());
        delegation.updated_at = timestamp.to_string();
        self.commit_delegation(index, delegation)
    }

    fn delegation_for_update(
        &self,
        operation: &'static str,
        delegation_id: &str,
    ) -> Result<(usize, Delegation), TaskStoreError> {
        self.document
            .delegations
            .iter()
            .position(|delegation| delegation.delegation_id == delegation_id)
            .map(|index| (index, self.document.delegations[index].clone()))
            .ok_or_else(|| {
                TaskStoreError::new(
                    operation,
                    &self.path,
                    format!("delegation {delegation_id} does not exist"),
                )
            })
    }

    fn commit_delegation(
        &mut self,
        index: usize,
        delegation: Delegation,
    ) -> Result<(), TaskStoreError> {
        let mut candidate = self.document.clone();
        candidate.delegations[index] = delegation;
        self.commit_candidate(candidate)
    }

    fn commit_candidate(&mut self, candidate: TaskStoreDocument) -> Result<(), TaskStoreError> {
        validate_document(&candidate)
            .map_err(|detail| TaskStoreError::new("validate", &self.path, detail))?;
        self.write_document_with(&candidate, |temporary, destination| {
            replace_file_atomically(temporary, destination)
        })?;
        self.document = candidate;
        self.revision = next_revision();
        self.delivery_ack_pending = false;
        Ok(())
    }

    fn write_document_with<F>(
        &self,
        document: &TaskStoreDocument,
        replace: F,
    ) -> Result<(), TaskStoreError>
    where
        F: FnOnce(&Path, &Path) -> io::Result<()>,
    {
        #[cfg(test)]
        self.write_count.set(self.write_count.get() + 1);
        let mut encoded = serde_json::to_vec_pretty(document)
            .map_err(|error| TaskStoreError::new("serialize", &self.path, error.to_string()))?;
        encoded.push(b'\n');
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| {
            TaskStoreError::new("create parent directory for", &self.path, error.to_string())
        })?;
        let destination_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(TASKS_FILE_NAME);
        let temporary_path = parent.join(format!(
            ".{destination_name}.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            replace(&temporary_path, &self.path)
        })();
        if let Err(error) = write_result {
            let detail = cleanup_temporary_file(&temporary_path, error.to_string());
            return Err(TaskStoreError::new("atomically write", &self.path, detail));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
pub fn replace_file_atomically(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(target_os = "windows")]
pub fn replace_file_atomically(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    if !destination.exists() {
        return fs::rename(temporary, destination);
    }

    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let temporary_wide = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    #[link(name = "Kernel32")]
    extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
    }

    // SAFETY: Both path buffers are NUL-terminated and remain alive for the
    // duration of the call. Optional pointers are null as required by the API.
    let replaced = unsafe {
        ReplaceFileW(
            destination_wide.as_ptr(),
            temporary_wide.as_ptr(),
            ptr::null(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn cleanup_temporary_file(path: &Path, primary_error: String) -> String {
    match fs::remove_file(path) {
        Ok(()) => primary_error,
        Err(error) if error.kind() == io::ErrorKind::NotFound => primary_error,
        Err(error) => format!(
            "{primary_error}; also failed to remove temporary file {}: {error}",
            path.display()
        ),
    }
}

fn reconcile_delegations_in(document: &mut TaskStoreDocument, timestamp: &str) -> usize {
    let tasks: HashSet<&str> = document
        .tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect();
    let mut interrupted = 0;
    for delegation in &mut document.delegations {
        // A worker lives in a task tab, which a restart restores (or the
        // launch queue starts later): it may still be going and report
        // `done`, so its delegation stays as it was. Only GitTerm-owned
        // runs (Codex reviews and consults) die with the app.
        let worker_survives = matches!(
            &delegation.child,
            DelegationChild::TaskSession { task_id, .. } if tasks.contains(task_id.as_str())
        );
        if delegation.status.is_active() && !worker_survives {
            delegation.status = DelegationStatus::Interrupted;
            delegation.updated_at = timestamp.to_string();
            interrupted += 1;
        }
    }
    interrupted
}

fn validate_delegation(delegation: &Delegation) -> Result<(), String> {
    let id = delegation.delegation_id.as_str();
    if id.trim().is_empty() {
        return Err("delegation id is empty".to_string());
    }
    for (label, value) in [
        ("parent session uid", delegation.parent.session_uid.as_str()),
        ("parent workspace", delegation.parent.workspace.as_str()),
        ("brief", delegation.brief.as_str()),
        ("created_at timestamp", delegation.created_at.as_str()),
        ("updated_at timestamp", delegation.updated_at.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("delegation {id} has an empty {label}"));
        }
    }
    if delegation.parent.cwd.as_os_str().is_empty() {
        return Err(format!("delegation {id} has an empty parent cwd"));
    }
    let child_matches_kind = match (&delegation.kind, &delegation.child) {
        (
            DelegationKind::Review,
            DelegationChild::CodexReview { .. } | DelegationChild::ClaudeSubagent { .. },
        ) => true,
        (
            DelegationKind::Consult,
            DelegationChild::CodexConsult { .. } | DelegationChild::ClaudeSubagent { .. },
        ) => true,
        (DelegationKind::Implement, DelegationChild::TaskSession { task_id, .. }) => {
            !task_id.trim().is_empty()
        }
        _ => false,
    };
    if !child_matches_kind {
        return Err(format!(
            "delegation {id} has a {:?} child that does not fit a {:?} delegation",
            delegation.child, delegation.kind
        ));
    }
    if delegation.target.is_some() && delegation.kind != DelegationKind::Review {
        return Err(format!(
            "delegation {id} has a review target but is not a review"
        ));
    }
    if delegation.previous.as_deref() == Some(id) {
        return Err(format!("delegation {id} names itself as previous"));
    }
    if let Some(next) = &delegation.superseded_by {
        if next.trim().is_empty()
            || next == id
            || delegation.status != DelegationStatus::Completed
            || delegation.kind != DelegationKind::Implement
        {
            return Err(format!("delegation {id} has invalid supersession {next:?}"));
        }
    }
    if let DelegationStatus::Failed { message } = &delegation.status {
        if message.trim().is_empty() {
            return Err(format!("delegation {id} failed without a message"));
        }
    }
    match (&delegation.status, &delegation.result) {
        (DelegationStatus::Completed | DelegationStatus::Blocked, None) => {
            return Err(format!(
                "delegation {id} is {} without a result",
                delegation.status.label()
            ));
        }
        (DelegationStatus::Blocked, Some(_)) if delegation.kind != DelegationKind::Implement => {
            return Err(format!("delegation {id} is blocked but only workers block"));
        }
        (DelegationStatus::Completed | DelegationStatus::Blocked, Some(result)) => {
            validate_delegation_result(delegation, result)?
        }
        (DelegationStatus::Failed { .. }, Some(result))
            if matches!(delegation.child, DelegationChild::TaskSession { .. }) =>
        {
            validate_delegation_result(delegation, result)?
        }
        (_, Some(_)) => {
            return Err(format!(
                "delegation {id} has a result but is {}",
                delegation.status.label()
            ));
        }
        (_, None) => {}
    }
    if delegation.dismissed_at.is_some() && delegation.status.is_active() {
        return Err(format!(
            "delegation {id} is dismissed but still {}",
            delegation.status.label()
        ));
    }
    if delegation.delivered && !delegation.status.is_sendable() {
        return Err(format!(
            "delegation {id} is marked delivered but is {}",
            delegation.status.label()
        ));
    }
    if delegation.delivered_at.is_some() && !delegation.status.has_result() {
        return Err(format!(
            "delegation {id} is marked delivered but is {}",
            delegation.status.label()
        ));
    }
    Ok(())
}

fn validate_delegation_result(
    delegation: &Delegation,
    result: &DelegationResult,
) -> Result<(), String> {
    let id = delegation.delegation_id.as_str();
    match delegation.kind {
        DelegationKind::Review if result.findings.is_none() => {
            return Err(format!("review delegation {id} completed without findings"));
        }
        DelegationKind::Implement if result.handoff.is_none() => {
            return Err(format!(
                "implement delegation {id} completed without a handoff"
            ));
        }
        DelegationKind::Consult if result.handoff.is_none() => {
            return Err(format!(
                "consult delegation {id} completed without a handoff"
            ));
        }
        _ => {}
    }
    if let Some(handoff) = &result.handoff {
        if handoff.summary.trim().is_empty() {
            return Err(format!("delegation {id} handoff summary is empty"));
        }
    }
    if let Some(findings) = &result.findings {
        let mut finding_ids = HashSet::new();
        for finding in &findings.findings {
            if !finding_ids.insert(finding.id.as_str()) {
                return Err(format!(
                    "delegation {id} has duplicate finding id {}",
                    finding.id
                ));
            }
        }
    }
    Ok(())
}

fn validate_document(document: &TaskStoreDocument) -> Result<(), String> {
    if document.schema_version != TASK_STORE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema version {}; expected {}",
            document.schema_version, TASK_STORE_SCHEMA_VERSION
        ));
    }
    let mut task_ids = HashSet::new();
    for task in &document.tasks {
        validate_task(task)?;
        if !task_ids.insert(task.task_id.as_str()) {
            return Err(format!("duplicate task id {}", task.task_id));
        }
    }
    let mut delegation_ids = HashSet::new();
    for delegation in &document.delegations {
        validate_delegation(delegation)?;
        if !delegation_ids.insert(delegation.delegation_id.as_str()) {
            return Err(format!(
                "duplicate delegation id {}",
                delegation.delegation_id
            ));
        }
    }
    Ok(())
}

fn worker_deadline(timestamp: &str, minutes: u32) -> Result<i64, String> {
    let start = chrono::DateTime::parse_from_rfc3339(timestamp)
        .map_err(|error| format!("invalid worker start timestamp {timestamp:?}: {error}"))?;
    start
        .timestamp_millis()
        .checked_add(i64::from(minutes) * 60_000)
        .ok_or_else(|| {
            format!("worker deadline overflows for start {timestamp:?} and limit {minutes}")
        })
}

#[cfg(test)]
#[path = "../../../../src/tasks/store_tests.rs"]
mod tests;
