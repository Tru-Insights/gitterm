use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const TASK_STORE_SCHEMA_VERSION: u32 = 1;
pub const TASKS_FILE_NAME: &str = "tasks.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStoreDocument {
    pub schema_version: u32,
    pub tasks: Vec<TaskRecord>,
}

impl Default for TaskStoreDocument {
    fn default() -> Self {
        Self {
            schema_version: TASK_STORE_SCHEMA_VERSION,
            tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub schema_version: u32,
    pub task_id: String,
    pub title: String,
    pub objective: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<TaskCreator>,
    pub workspace: WorkspaceIdentity,
    pub repository: RepositoryIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueReference>,
    pub base: GitBase,
    pub branch: String,
    pub worktree: TaskWorktree,
    pub executor: ExecutorTarget,
    /// Last/default launch choice. The actual harness belongs to each execution
    /// attempt or child tab; tasks created without a session leave this empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessSelection>,
    pub stopping_boundary: StoppingBoundary,
    pub lifecycle: TaskLifecycle,
    pub attention: TaskAttention,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<TaskHandoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub attempts: Vec<TaskExecutionAttempt>,
    /// Durable child-session history for this task. Unlike execution attempts,
    /// several sessions may belong to one task (for example an implementation
    /// agent, a review agent, and a terminal).
    #[serde(default)]
    pub sessions: Vec<TaskSessionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_attempt_id: Option<String>,
    /// Coarse progress snapshot for the rail/overview, flushed on a debounce —
    /// in-memory UI state is always fresher than this while the app runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
    pub changes: ChangedFilesSummary,
    pub verification: VerificationSummary,
    pub delivery: DeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

impl TaskRecord {
    pub fn new_draft(input: NewTaskRecord, timestamp: impl Into<String>) -> Self {
        let timestamp = timestamp.into();
        Self {
            schema_version: TASK_STORE_SCHEMA_VERSION,
            task_id: input.task_id,
            title: input.title,
            objective: input.objective,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            created_by: None,
            workspace: input.workspace,
            repository: input.repository,
            issue: input.issue,
            base: input.base,
            branch: input.branch,
            worktree: TaskWorktree::default(),
            executor: input.executor,
            harness: input.harness,
            stopping_boundary: input.stopping_boundary,
            lifecycle: TaskLifecycle::Draft,
            attention: TaskAttention::default(),
            handoff: None,
            last_error: None,
            attempts: Vec::new(),
            sessions: Vec::new(),
            active_attempt_id: None,
            progress: None,
            changes: ChangedFilesSummary::default(),
            verification: VerificationSummary::default(),
            delivery: DeliveryState::default(),
            archived_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCreatorKind {
    Manual,
    Coordinator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCreator {
    pub kind: TaskCreatorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_label: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTaskRecord {
    pub task_id: String,
    pub title: String,
    pub objective: String,
    pub workspace: WorkspaceIdentity,
    pub repository: RepositoryIdentity,
    pub issue: Option<IssueReference>,
    pub base: GitBase,
    pub branch: String,
    pub executor: ExecutorTarget,
    pub harness: Option<HarnessSelection>,
    pub stopping_boundary: StoppingBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedWorktreePreparation {
    pub repository: RepositoryIdentity,
    pub base: GitBase,
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub name: String,
    pub location: WorkspaceLocationIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceLocationIdentity {
    Local { directory: PathBuf },
    RemoteAgent { remote_id: String, root: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryIdentity {
    pub common_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueProvider {
    Linear,
    GitHub,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueReference {
    pub provider: IssueProvider,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBase {
    pub reference: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWorktree {
    pub state: TaskWorktreeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl Default for TaskWorktree {
    fn default() -> Self {
        Self {
            state: TaskWorktreeState::Unprepared,
            path: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorktreeState {
    Unprepared,
    Preparing,
    Ready,
    Missing,
    CleanupRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutorTarget {
    Local,
    RemoteAgent { remote_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessKind {
    TerminalPreset { preset_name: String },
    NativeClaude,
    NativePi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSelection {
    pub kind: HarnessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessConversationBackend {
    Claude,
    Codex,
    Pi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessConversationRef {
    pub backend: HarnessConversationBackend,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSessionRecord {
    /// GitTerm-owned identity for this child view. It remains stable when the
    /// underlying harness conversation is closed and later resumed.
    pub task_session_id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<HarnessConversationRef>,
    #[serde(default)]
    pub objective_delivery: ObjectiveDeliveryState,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveDeliveryState {
    #[default]
    Unknown,
    NotDelivered,
    Delivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskHandoff {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by_session_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppingBoundary {
    PlanOnly,
    ImplementUntilTestsPass,
    PrepareDraftPr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycle {
    Draft,
    Preparing,
    Ready,
    Queued,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Stopped,
    Interrupted,
    Archived,
}

impl TaskLifecycle {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Queued | Self::Running | Self::WaitingForInput
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        match self {
            Self::Draft => matches!(next, Self::Preparing | Self::Archived),
            Self::Preparing => matches!(next, Self::Ready | Self::Failed | Self::Interrupted),
            Self::Ready => matches!(
                next,
                Self::Preparing | Self::Queued | Self::Running | Self::Archived
            ),
            Self::Queued => matches!(
                next,
                Self::Running | Self::Stopped | Self::Failed | Self::Interrupted
            ),
            Self::Running | Self::WaitingForInput => matches!(
                next,
                Self::Running
                    | Self::WaitingForInput
                    | Self::Completed
                    | Self::Failed
                    | Self::Stopped
                    | Self::Interrupted
            ),
            Self::Completed | Self::Failed | Self::Stopped | Self::Interrupted => matches!(
                next,
                Self::Preparing | Self::Queued | Self::Running | Self::Archived
            ),
            Self::Archived => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAttentionReason {
    RequiresInput,
    ExecutionFailed,
    /// A GitTerm restart cut the local execution short. Calmer than a real
    /// failure: the worktree is intact and the session can simply resume.
    Interrupted,
    CompletedUnread,
    ReadyForReview,
    RemoteUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TaskAttention {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TaskAttentionReason>,
    #[serde(default)]
    pub unread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskExecutionAttempt {
    pub attempt_id: String,
    pub executor: ExecutorTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessSelection>,
    pub state: AttemptState,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Preparing,
    Queued,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Stopped,
    Interrupted,
}

impl AttemptState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Queued | Self::Running | Self::WaitingForInput
        )
    }

    fn lifecycle(self) -> TaskLifecycle {
        match self {
            Self::Preparing => TaskLifecycle::Preparing,
            Self::Queued => TaskLifecycle::Queued,
            Self::Running => TaskLifecycle::Running,
            Self::WaitingForInput => TaskLifecycle::WaitingForInput,
            Self::Completed => TaskLifecycle::Completed,
            Self::Failed => TaskLifecycle::Failed,
            Self::Stopped => TaskLifecycle::Stopped,
            Self::Interrupted => TaskLifecycle::Interrupted,
        }
    }

    /// The attempt state a task lifecycle maps back onto. `Draft`, `Ready`,
    /// and `Archived` describe the task between runs, not a run itself.
    fn for_lifecycle(lifecycle: TaskLifecycle) -> Option<Self> {
        match lifecycle {
            TaskLifecycle::Preparing => Some(Self::Preparing),
            TaskLifecycle::Queued => Some(Self::Queued),
            TaskLifecycle::Running => Some(Self::Running),
            TaskLifecycle::WaitingForInput => Some(Self::WaitingForInput),
            TaskLifecycle::Completed => Some(Self::Completed),
            TaskLifecycle::Failed => Some(Self::Failed),
            TaskLifecycle::Stopped => Some(Self::Stopped),
            TaskLifecycle::Interrupted => Some(Self::Interrupted),
            TaskLifecycle::Draft | TaskLifecycle::Ready | TaskLifecycle::Archived => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChangedFilesSummary {
    #[serde(default)]
    pub changed: u32,
    #[serde(default)]
    pub staged: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

/// Coarse, honest progress for a task's current run: what the session last
/// said it was doing and when it was last heard from. Elapsed time derives
/// from the active attempt's `started_at`, so it is not stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TaskProgress {
    /// Short label for what the session is doing right now (a tool name,
    /// "Responding", …). `None` means nothing richer than the lifecycle is
    /// known — render the lifecycle-derived phase instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Last meaningful line the session produced (single line, pre-truncated
    /// by the writer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_line: Option<String>,
    /// RFC3339 wall-clock time of the last observed session activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    #[default]
    Unknown,
    NotRun,
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VerificationSummary {
    pub state: VerificationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeliveryState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
}

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
        Ok(Self { path, document })
    }

    pub fn path(&self) -> &Path {
        &self.path
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
        let Some(index) = self
            .document
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
        let previous = &self.document.tasks[index];
        if !previous.lifecycle.can_transition_to(task.lifecycle) {
            return Err(TaskStoreError::new(
                "replace",
                &self.path,
                format!(
                    "task {} cannot transition from {:?} to {:?}",
                    task.task_id, previous.lifecycle, task.lifecycle
                ),
            ));
        }
        let mut candidate = self.document.clone();
        candidate.tasks[index] = task;
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
        task.attention = TaskAttention {
            reason: Some(TaskAttentionReason::ExecutionFailed),
            unread: true,
        };
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
        task.attention = TaskAttention {
            reason: Some(TaskAttentionReason::ExecutionFailed),
            unread: true,
        };
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
        }
        let has_new_detail = last_error.is_some();
        if let Some(detail) = last_error {
            task.last_error = Some(detail);
        }
        if target == TaskLifecycle::Failed {
            task.attention = TaskAttention {
                reason: Some(TaskAttentionReason::ExecutionFailed),
                unread: true,
            };
        } else if target == TaskLifecycle::Completed && current != target {
            // Completion is attention until the task is visited — the visit
            // (not a glance at the rail) acknowledges it. The current==target
            // guard keeps a repeated completion signal from re-marking a task
            // the user already read.
            task.attention = TaskAttention {
                reason: Some(TaskAttentionReason::CompletedUnread),
                unread: true,
            };
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
        progress: TaskProgress,
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

    pub fn reconcile_after_restart(&mut self, timestamp: &str) -> Result<usize, TaskStoreError> {
        let mut candidate = self.document.clone();
        let mut reconciled = 0;
        for task in &mut candidate.tasks {
            if task.executor != ExecutorTarget::Local || !task.lifecycle.is_active() {
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
            task.attention = TaskAttention {
                reason: Some(TaskAttentionReason::Interrupted),
                unread: true,
            };
            task.last_error =
                Some("GitTerm restarted while this local execution was active".to_string());
            reconciled += 1;
        }
        if reconciled > 0 {
            self.commit_candidate(candidate)?;
        }
        Ok(reconciled)
    }

    fn commit_candidate(&mut self, candidate: TaskStoreDocument) -> Result<(), TaskStoreError> {
        validate_document(&candidate)
            .map_err(|detail| TaskStoreError::new("validate", &self.path, detail))?;
        self.write_document_with(&candidate, |temporary, destination| {
            replace_file_atomically(temporary, destination)
        })?;
        self.document = candidate;
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
fn replace_file_atomically(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(target_os = "windows")]
fn replace_file_atomically(temporary: &Path, destination: &Path) -> io::Result<()> {
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
    Ok(())
}

fn validate_task(task: &TaskRecord) -> Result<(), String> {
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

    fn sample_task(task_id: &str) -> TaskRecord {
        TaskRecord::new_draft(
            NewTaskRecord {
                task_id: task_id.to_string(),
                title: "Add durable tasks".to_string(),
                objective: "Persist task state without opening a workspace".to_string(),
                workspace: WorkspaceIdentity {
                    name: "GitTerm V5".to_string(),
                    location: WorkspaceLocationIdentity::Local {
                        directory: PathBuf::from("/repo with spaces/gitterm-v5"),
                    },
                },
                repository: RepositoryIdentity {
                    common_dir: PathBuf::from("/repo with spaces/gitterm-v5/.git"),
                    remote_url: Some("https://github.com/Tru-Insights/gitterm.git".to_string()),
                },
                issue: Some(IssueReference {
                    provider: IssueProvider::Linear,
                    key: "TRU-104".to_string(),
                    url: Some("https://linear.app/example/TRU-104".to_string()),
                }),
                base: GitBase {
                    reference: "v5".to_string(),
                    commit: "0123456789abcdef".to_string(),
                },
                branch: format!("task/{task_id}-durable-tasks"),
                executor: ExecutorTarget::Local,
                harness: Some(HarnessSelection {
                    kind: HarnessKind::TerminalPreset {
                        preset_name: "Codex".to_string(),
                    },
                    model: Some("gpt-5.6".to_string()),
                }),
                stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            },
            "2026-08-19T08:00:00Z",
        )
    }

    fn active_attempt(attempt_id: &str, executor: ExecutorTarget) -> TaskExecutionAttempt {
        TaskExecutionAttempt {
            attempt_id: attempt_id.to_string(),
            executor,
            harness: Some(HarnessSelection {
                kind: HarnessKind::TerminalPreset {
                    preset_name: "Codex".to_string(),
                },
                model: Some("gpt-5.6".to_string()),
            }),
            state: AttemptState::Running,
            started_at: "2026-08-19T08:01:00Z".to_string(),
            ended_at: None,
            session_ref: Some("session-1".to_string()),
            failure: None,
        }
    }

    fn make_ready(store: &mut TaskStore, task_id: &str) {
        let mut task = store.get(task_id).unwrap().clone();
        task.lifecycle = TaskLifecycle::Preparing;
        task.updated_at = "2026-08-19T08:00:30Z".to_string();
        store.replace(task).unwrap();

        let mut task = store.get(task_id).unwrap().clone();
        task.lifecycle = TaskLifecycle::Ready;
        task.updated_at = "2026-08-19T08:00:45Z".to_string();
        store.replace(task).unwrap();
    }

    #[test]
    fn missing_store_loads_empty_without_creating_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let store = TaskStore::load(&path).unwrap();
        assert!(store.tasks().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn older_task_records_default_to_empty_session_history() {
        let mut value = serde_json::to_value(sample_task("task-1")).unwrap();
        value.as_object_mut().unwrap().remove("sessions");

        let decoded: TaskRecord = serde_json::from_value(value).unwrap();

        assert!(decoded.sessions.is_empty());
    }

    #[test]
    fn older_records_default_creator_and_objective_delivery_metadata() {
        let mut task = sample_task("task-1");
        task.created_by = Some(TaskCreator {
            kind: TaskCreatorKind::Coordinator,
            session_id: Some("coordinator-1".to_string()),
            harness_label: Some("Codex".to_string()),
            created_at: "2026-08-19T08:00:00Z".to_string(),
        });
        task.sessions.push(TaskSessionRecord {
            task_session_id: "task-session-1".to_string(),
            label: "Claude implementation".to_string(),
            harness: None,
            conversation: None,
            objective_delivery: ObjectiveDeliveryState::Delivered,
            created_at: "2026-08-19T08:01:00Z".to_string(),
            updated_at: "2026-08-19T08:01:00Z".to_string(),
        });
        let mut value = serde_json::to_value(task).unwrap();
        let task = value.as_object_mut().unwrap();
        task.remove("created_by");
        task["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("objective_delivery");

        let decoded: TaskRecord = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.created_by, None);
        assert_eq!(
            decoded.sessions[0].objective_delivery,
            ObjectiveDeliveryState::Unknown
        );
    }

    #[test]
    fn task_session_history_round_trips_native_conversation_references() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let session = TaskSessionRecord {
            task_session_id: "task-session-1".to_string(),
            label: "Codex implementation".to_string(),
            harness: Some(HarnessSelection {
                kind: HarnessKind::TerminalPreset {
                    preset_name: "Codex".to_string(),
                },
                model: Some("gpt-5.6".to_string()),
            }),
            conversation: Some(HarnessConversationRef {
                backend: HarnessConversationBackend::Codex,
                session_id: "codex-chat-1".to_string(),
            }),
            objective_delivery: ObjectiveDeliveryState::Delivered,
            created_at: "2026-08-19T08:01:00Z".to_string(),
            updated_at: "2026-08-19T08:01:00Z".to_string(),
        };

        store
            .upsert_session("task-1", session.clone(), "2026-08-19T08:01:00Z")
            .unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        assert_eq!(reloaded.get("task-1").unwrap().sessions, vec![session]);
        assert_eq!(
            reloaded
                .get("task-1")
                .unwrap()
                .harness
                .as_ref()
                .map(|harness| &harness.kind),
            Some(&HarnessKind::TerminalPreset {
                preset_name: "Codex".to_string()
            })
        );
    }

    #[test]
    fn task_handoff_round_trips_compact_cross_harness_context() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let handoff = TaskHandoff {
            summary: "Implementation is complete; review remains".to_string(),
            decisions: vec!["Keep the task store as the only writer".to_string()],
            next_steps: vec!["Run the all-features gate".to_string()],
            blockers: Vec::new(),
            updated_by_session_id: None,
            updated_at: "2026-08-19T09:00:00Z".to_string(),
        };

        store
            .update_handoff("task-1", handoff.clone(), "2026-08-19T09:00:00Z")
            .unwrap();

        assert_eq!(
            TaskStore::load(path)
                .unwrap()
                .get("task-1")
                .unwrap()
                .handoff,
            Some(handoff)
        );
    }

    #[test]
    fn round_trips_task_records_and_every_enum_variant() {
        let lifecycle = vec![
            TaskLifecycle::Draft,
            TaskLifecycle::Preparing,
            TaskLifecycle::Ready,
            TaskLifecycle::Queued,
            TaskLifecycle::Running,
            TaskLifecycle::WaitingForInput,
            TaskLifecycle::Completed,
            TaskLifecycle::Failed,
            TaskLifecycle::Stopped,
            TaskLifecycle::Interrupted,
            TaskLifecycle::Archived,
        ];
        let attempts = vec![
            AttemptState::Preparing,
            AttemptState::Queued,
            AttemptState::Running,
            AttemptState::WaitingForInput,
            AttemptState::Completed,
            AttemptState::Failed,
            AttemptState::Stopped,
            AttemptState::Interrupted,
        ];
        let executors = vec![
            ExecutorTarget::Local,
            ExecutorTarget::RemoteAgent {
                remote_id: "mini".to_string(),
            },
        ];
        let harnesses = vec![
            HarnessKind::TerminalPreset {
                preset_name: "Codex".to_string(),
            },
            HarnessKind::NativeClaude,
            HarnessKind::NativePi,
        ];
        let stopping_boundaries = vec![
            StoppingBoundary::PlanOnly,
            StoppingBoundary::ImplementUntilTestsPass,
            StoppingBoundary::PrepareDraftPr,
        ];
        let worktree_states = vec![
            TaskWorktreeState::Unprepared,
            TaskWorktreeState::Preparing,
            TaskWorktreeState::Ready,
            TaskWorktreeState::Missing,
            TaskWorktreeState::CleanupRequired,
        ];
        let attention_reasons = vec![
            TaskAttentionReason::RequiresInput,
            TaskAttentionReason::ExecutionFailed,
            TaskAttentionReason::Interrupted,
            TaskAttentionReason::CompletedUnread,
            TaskAttentionReason::ReadyForReview,
            TaskAttentionReason::RemoteUnavailable,
        ];
        let verification_states = vec![
            VerificationState::Unknown,
            VerificationState::NotRun,
            VerificationState::Running,
            VerificationState::Passed,
            VerificationState::Failed,
        ];
        let issue_providers = vec![IssueProvider::Linear, IssueProvider::GitHub];
        let conversation_backends = vec![
            HarnessConversationBackend::Claude,
            HarnessConversationBackend::Codex,
            HarnessConversationBackend::Pi,
        ];
        let workspace_locations = vec![
            WorkspaceLocationIdentity::Local {
                directory: PathBuf::from("/local/repo"),
            },
            WorkspaceLocationIdentity::RemoteAgent {
                remote_id: "mini".to_string(),
                root: "/remote/repo".to_string(),
            },
        ];
        let encoded = serde_json::to_vec(&(
            lifecycle.clone(),
            attempts.clone(),
            executors.clone(),
            harnesses.clone(),
            stopping_boundaries.clone(),
            worktree_states.clone(),
            attention_reasons.clone(),
            verification_states.clone(),
            issue_providers.clone(),
            workspace_locations.clone(),
            conversation_backends.clone(),
        ))
        .unwrap();
        let decoded: (
            Vec<TaskLifecycle>,
            Vec<AttemptState>,
            Vec<ExecutorTarget>,
            Vec<HarnessKind>,
            Vec<StoppingBoundary>,
            Vec<TaskWorktreeState>,
            Vec<TaskAttentionReason>,
            Vec<VerificationState>,
            Vec<IssueProvider>,
            Vec<WorkspaceLocationIdentity>,
            Vec<HarnessConversationBackend>,
        ) = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.0, lifecycle);
        assert_eq!(decoded.1, attempts);
        assert_eq!(decoded.2, executors);
        assert_eq!(decoded.3, harnesses);
        assert_eq!(decoded.4, stopping_boundaries);
        assert_eq!(decoded.5, worktree_states);
        assert_eq!(decoded.6, attention_reasons);
        assert_eq!(decoded.7, verification_states);
        assert_eq!(decoded.8, issue_providers);
        assert_eq!(decoded.9, workspace_locations);
        assert_eq!(decoded.10, conversation_backends);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        let task = sample_task("task-1");
        store.insert(task.clone()).unwrap();
        let reloaded = TaskStore::load(path).unwrap();
        assert_eq!(reloaded.tasks(), &[task]);
    }

    #[test]
    fn task_harness_is_optional_without_breaking_existing_records() {
        let legacy_task = sample_task("legacy-task");
        let legacy_json = serde_json::to_value(&legacy_task).unwrap();
        let decoded_legacy: TaskRecord = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(decoded_legacy.harness, legacy_task.harness);

        let mut task_without_session = serde_json::to_value(sample_task("new-task")).unwrap();
        task_without_session
            .as_object_mut()
            .unwrap()
            .remove("harness");
        let decoded_without_session: TaskRecord =
            serde_json::from_value(task_without_session).unwrap();
        assert_eq!(decoded_without_session.harness, None);
        assert!(
            serde_json::to_value(decoded_without_session).unwrap()["harness"].is_null(),
            "a task with no child session should not persist a task-level harness"
        );
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        fs::write(&path, r#"{"schema_version":99,"tasks":[]}"#).unwrap();
        let error = TaskStore::load(&path).unwrap_err();
        assert_eq!(error.operation(), "validate");
        assert!(error.to_string().contains("unsupported schema version 99"));
    }

    #[test]
    fn corrupt_store_is_reported_without_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let original = b"{ definitely not valid json";
        fs::write(&path, original).unwrap();
        let error = TaskStore::load(&path).unwrap_err();
        assert_eq!(error.operation(), "decode");
        assert_eq!(error.path(), path);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn failed_atomic_replace_keeps_the_prior_valid_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let before = fs::read(&path).unwrap();
        let mut candidate = store.document.clone();
        candidate.tasks[0].title = "Replacement title".to_string();
        let error = store
            .write_document_with(&candidate, |_temporary, _destination| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected replace failure",
                ))
            })
            .unwrap_err();
        assert!(error.to_string().contains("injected replace failure"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(store.tasks()[0].title, "Add durable tasks");
        assert_eq!(
            TaskStore::load(path).unwrap().tasks()[0].title,
            "Add durable tasks"
        );
    }

    #[test]
    fn startup_reconciles_active_local_attempts_to_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();
        assert_eq!(
            store
                .reconcile_after_restart("2026-08-19T09:00:00Z")
                .unwrap(),
            1
        );
        let reloaded = TaskStore::load(path).unwrap();
        let task = &reloaded.tasks()[0];
        assert_eq!(task.lifecycle, TaskLifecycle::Interrupted);
        assert_eq!(task.active_attempt_id, None);
        assert_eq!(task.attempts[0].state, AttemptState::Interrupted);
        assert_eq!(
            task.attempts[0].ended_at.as_deref(),
            Some("2026-08-19T09:00:00Z")
        );
        assert!(task.attempts[0]
            .failure
            .as_deref()
            .unwrap()
            .contains("GitTerm restarted"));
        // A routine restart is an interruption, not a failure — it must not
        // wear failure-red attention.
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::Interrupted)
        );
        assert!(task.attention.unread);
    }

    #[test]
    fn startup_leaves_active_remote_attempts_for_remote_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt(
                    "attempt-1",
                    ExecutorTarget::RemoteAgent {
                        remote_id: "mini".to_string(),
                    },
                ),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();
        assert_eq!(
            store
                .reconcile_after_restart("2026-08-19T09:00:00Z")
                .unwrap(),
            0
        );
        assert_eq!(store.tasks()[0].lifecycle, TaskLifecycle::Running);
    }

    #[test]
    fn enforces_one_active_attempt_per_task() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();
        let error = store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-2", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:02:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("already has an active attempt"));
    }

    #[test]
    fn rejects_invalid_lifecycle_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        let mut task = store.get("task-1").unwrap().clone();
        task.lifecycle = TaskLifecycle::Completed;
        let error = store.replace(task).unwrap_err();
        assert!(error.to_string().contains("cannot transition"));
    }

    #[test]
    fn rejects_active_attempt_state_that_does_not_match_task_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");

        let error = store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::WaitingForInput,
                "2026-08-19T08:01:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_running_task_without_an_active_attempt() {
        let mut task = sample_task("task-1");
        task.lifecycle = TaskLifecycle::Running;
        let error = validate_task(&task).unwrap_err();
        assert!(error.contains("without an active attempt"));
    }

    #[test]
    fn archives_inactive_tasks_without_deleting_their_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        store.archive("task-1", "2026-08-19T10:00:00Z").unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Archived);
        assert_eq!(task.archived_at.as_deref(), Some("2026-08-19T10:00:00Z"));
    }

    #[test]
    fn refuses_to_archive_a_task_with_an_active_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .begin_attempt(
                "task-1",
                active_attempt("attempt-1", ExecutorTarget::Local),
                TaskLifecycle::Running,
                "2026-08-19T08:01:00Z",
            )
            .unwrap();

        let error = store.archive("task-1", "2026-08-19T10:00:00Z").unwrap_err();
        assert!(error.to_string().contains("cannot transition"));
        assert_eq!(store.tasks()[0].lifecycle, TaskLifecycle::Running);
    }

    #[test]
    fn persists_worktree_preparation_success_and_failure_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let ready_worktree = temp.path().join("worktrees").join("ready-task");
        let failed_worktree = temp.path().join("worktrees").join("failed-task");
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("ready-task")).unwrap();
        store.insert(sample_task("failed-task")).unwrap();

        store
            .begin_worktree_preparation("ready-task", "2026-08-19T08:01:00Z")
            .unwrap();
        store
            .complete_worktree_preparation(
                "ready-task",
                CompletedWorktreePreparation {
                    repository: RepositoryIdentity {
                        common_dir: PathBuf::from("/repo/.git"),
                        remote_url: None,
                    },
                    base: GitBase {
                        reference: "main".to_string(),
                        commit: "fedcba9876543210".to_string(),
                    },
                    branch: "task/ready-task-prepared".to_string(),
                    path: ready_worktree.clone(),
                },
                "2026-08-19T08:02:00Z",
            )
            .unwrap();

        store
            .begin_worktree_preparation("failed-task", "2026-08-19T08:03:00Z")
            .unwrap();
        store
            .fail_worktree_preparation(
                "failed-task",
                "git worktree add failed".to_string(),
                Some(failed_worktree),
                "2026-08-19T08:04:00Z",
            )
            .unwrap();

        let reloaded = TaskStore::load(path).unwrap();
        let ready = reloaded.get("ready-task").unwrap();
        assert_eq!(ready.lifecycle, TaskLifecycle::Ready);
        assert_eq!(ready.worktree.state, TaskWorktreeState::Ready);
        assert_eq!(
            ready.worktree.path.as_deref(),
            Some(ready_worktree.as_path())
        );
        let failed = reloaded.get("failed-task").unwrap();
        assert_eq!(failed.lifecycle, TaskLifecycle::Failed);
        assert_eq!(failed.worktree.state, TaskWorktreeState::CleanupRequired);
        assert_eq!(
            failed.last_error.as_deref(),
            Some("git worktree add failed")
        );
        assert_eq!(
            failed.attention.reason,
            Some(TaskAttentionReason::ExecutionFailed)
        );
    }

    #[test]
    fn lifecycle_signal_hops_through_running_into_active_states() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");

        // A waiting signal from a Ready task means the session ran first even
        // though the start itself was never observed.
        let changed = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::WaitingForInput,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        assert!(changed);
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::WaitingForInput);
        assert_eq!(task.updated_at, "2026-08-19T08:05:00Z");
        // The signal opened an attempt record for the run it implies.
        assert_eq!(task.attempts.len(), 1);
        assert_eq!(task.attempts[0].state, AttemptState::WaitingForInput);
        assert_eq!(
            task.active_attempt_id.as_deref(),
            Some(task.attempts[0].attempt_id.as_str())
        );
    }

    #[test]
    fn lifecycle_signal_never_hops_between_terminal_states() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Failed,
                Some("agent reported an error".to_string()),
                "2026-08-19T08:05:00Z",
            )
            .unwrap();

        // A later completion from a sibling session must not launder the
        // failure into success.
        let error = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Completed,
                None,
                "2026-08-19T08:06:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("Failed"), "{error}");
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::Failed);
        assert_eq!(task.last_error.as_deref(), Some("agent reported an error"));
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::ExecutionFailed)
        );
        assert!(task.attention.unread);
    }

    #[test]
    fn lifecycle_signal_same_state_is_a_noop_without_detail() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();

        let changed = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:06:00Z",
            )
            .unwrap();
        assert!(!changed);
        assert_eq!(
            store.get("task-1").unwrap().updated_at,
            "2026-08-19T08:05:00Z"
        );
    }

    #[test]
    fn lifecycle_signal_rejects_states_that_never_ran() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        // Draft tasks have no worktree and no session; every session signal
        // against one is a wiring bug worth surfacing.
        let error = store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("Draft"), "{error}");
        assert_eq!(store.get("task-1").unwrap().lifecycle, TaskLifecycle::Draft);
    }

    #[test]
    fn lifecycle_signal_resumes_interrupted_tasks_into_waiting() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .reconcile_after_restart("2026-08-19T08:06:00Z")
            .unwrap();
        assert_eq!(
            store.get("task-1").unwrap().lifecycle,
            TaskLifecycle::Interrupted
        );

        // A restored session (claude --resume) that shows its prompt again is
        // genuinely alive and waiting.
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::WaitingForInput,
                None,
                "2026-08-19T08:07:00Z",
            )
            .unwrap();
        let task = store.get("task-1").unwrap();
        assert_eq!(task.lifecycle, TaskLifecycle::WaitingForInput);
        // The interrupted attempt stays closed as history; the resume opened
        // a fresh one.
        assert_eq!(task.attempts.len(), 2);
        assert_eq!(task.attempts[0].state, AttemptState::Interrupted);
        assert!(task.attempts[0].ended_at.is_some());
        assert_eq!(task.attempts[1].state, AttemptState::WaitingForInput);
        assert_eq!(
            task.active_attempt_id.as_deref(),
            Some(task.attempts[1].attempt_id.as_str())
        );
        // The interruption's attention and error are superseded by the live
        // resume — the failure detail survives on the closed attempt.
        assert_eq!(task.attention.reason, None);
        assert!(!task.attention.unread);
        assert_eq!(task.last_error, None);
        assert_eq!(
            task.attempts[0].failure.as_deref(),
            Some("GitTerm restarted while this local execution was active")
        );
    }

    #[test]
    fn completion_signal_marks_the_task_completed_unread() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Completed,
                None,
                "2026-08-19T08:10:00Z",
            )
            .unwrap();
        let task = store.get("task-1").unwrap();
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::CompletedUnread)
        );
        assert!(task.attention.unread);
    }

    #[test]
    fn acknowledging_attention_clears_completion_but_keeps_state_backed_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Completed,
                None,
                "2026-08-19T08:10:00Z",
            )
            .unwrap();
        assert!(store
            .acknowledge_attention("task-1", "2026-08-19T08:11:00Z")
            .unwrap());
        let task = store.get("task-1").unwrap();
        assert_eq!(task.attention.reason, None);
        assert!(!task.attention.unread);
        // Acknowledging an already-read task is a no-op that leaves the file
        // untouched — callers fire it on every visit and periodic tick.
        let before = std::fs::read(&path).unwrap();
        assert!(!store
            .acknowledge_attention("task-1", "2026-08-19T08:12:00Z")
            .unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // A failure reason survives the visit — only its unread badge clears.
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:13:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Failed,
                Some("agent reported an error".to_string()),
                "2026-08-19T08:14:00Z",
            )
            .unwrap();
        assert!(store
            .acknowledge_attention("task-1", "2026-08-19T08:15:00Z")
            .unwrap());
        let task = store.get("task-1").unwrap();
        assert_eq!(
            task.attention.reason,
            Some(TaskAttentionReason::ExecutionFailed)
        );
        assert!(!task.attention.unread);
    }

    #[test]
    fn dismissing_attention_drops_even_state_backed_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        make_ready(&mut store, "task-1");
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Running,
                None,
                "2026-08-19T08:05:00Z",
            )
            .unwrap();
        store
            .record_lifecycle_signal(
                "task-1",
                TaskLifecycle::Failed,
                Some("agent reported an error".to_string()),
                "2026-08-19T08:06:00Z",
            )
            .unwrap();
        // A visit keeps a failure reason; an explicit dismissal drops it —
        // the lifecycle still says Failed, so the rail stays truthful.
        assert!(store
            .dismiss_attention("task-1", "2026-08-19T08:07:00Z")
            .unwrap());
        let task = store.get("task-1").unwrap();
        assert_eq!(task.attention, TaskAttention::default());
        assert_eq!(task.lifecycle, TaskLifecycle::Failed);
        // Dismissing again is a no-op that leaves the file untouched.
        let before = std::fs::read(&path).unwrap();
        assert!(!store
            .dismiss_attention("task-1", "2026-08-19T08:08:00Z")
            .unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn update_progress_persists_snapshot_and_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        let progress = TaskProgress {
            phase: Some("Running tests".to_string()),
            last_update_line: Some("cargo test --features excalidraw".to_string()),
            last_activity_at: Some("2026-08-19T08:10:00Z".to_string()),
        };
        let changes = ChangedFilesSummary {
            changed: 3,
            staged: 1,
            checked_at: Some("2026-08-19T08:10:00Z".to_string()),
        };
        let wrote = store
            .update_progress(
                "task-1",
                progress.clone(),
                Some(changes.clone()),
                "2026-08-19T08:10:05Z",
            )
            .unwrap();
        assert!(wrote);

        let reloaded = TaskStore::load(&path).unwrap();
        let task = reloaded.get("task-1").unwrap();
        assert_eq!(task.progress.as_ref(), Some(&progress));
        assert_eq!(task.changes, changes);
        assert_eq!(task.updated_at, "2026-08-19T08:10:05Z");
    }

    #[test]
    fn update_progress_is_a_no_op_when_nothing_differs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        store.insert(sample_task("task-1")).unwrap();

        let progress = TaskProgress {
            phase: Some("Editing".to_string()),
            last_update_line: None,
            last_activity_at: Some("2026-08-19T08:10:00Z".to_string()),
        };
        assert!(store
            .update_progress("task-1", progress.clone(), None, "2026-08-19T08:10:05Z")
            .unwrap());
        let bytes_after_first = fs::read(&path).unwrap();

        // Same snapshot again — the flush timer will do this constantly, and it
        // must not rewrite the file or bump updated_at.
        assert!(!store
            .update_progress("task-1", progress.clone(), None, "2026-08-19T08:15:00Z")
            .unwrap());
        assert_eq!(fs::read(&path).unwrap(), bytes_after_first);

        // Unchanged progress but fresh change counts still writes.
        let changes = ChangedFilesSummary {
            changed: 2,
            staged: 0,
            checked_at: Some("2026-08-19T08:16:00Z".to_string()),
        };
        assert!(store
            .update_progress("task-1", progress, Some(changes), "2026-08-19T08:16:00Z")
            .unwrap());
    }

    #[test]
    fn update_progress_rejects_unknown_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        let error = store
            .update_progress(
                "missing",
                TaskProgress::default(),
                None,
                "2026-08-19T08:10:00Z",
            )
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn task_store_writes_do_not_modify_workspace_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_path = temp.path().join("workspaces.json");
        let workspace_bytes = br#"{"workspaces":[{"name":"existing"}]}"#;
        fs::write(&workspace_path, workspace_bytes).unwrap();
        let mut store = TaskStore::load(temp.path().join(TASKS_FILE_NAME)).unwrap();
        store.insert(sample_task("task-1")).unwrap();
        assert_eq!(fs::read(workspace_path).unwrap(), workspace_bytes);
    }
}
