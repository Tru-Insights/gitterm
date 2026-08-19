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
    pub workspace: WorkspaceIdentity,
    pub repository: RepositoryIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueReference>,
    pub base: GitBase,
    pub branch: String,
    pub worktree: TaskWorktree,
    pub executor: ExecutorTarget,
    pub harness: HarnessSelection,
    pub stopping_boundary: StoppingBoundary,
    pub lifecycle: TaskLifecycle,
    pub attention: TaskAttention,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub attempts: Vec<TaskExecutionAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_attempt_id: Option<String>,
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
            last_error: None,
            attempts: Vec::new(),
            active_attempt_id: None,
            changes: ChangedFilesSummary::default(),
            verification: VerificationSummary::default(),
            delivery: DeliveryState::default(),
            archived_at: None,
        }
    }
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
    pub harness: HarnessSelection,
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
        task.attempts.push(attempt);
        task.lifecycle = lifecycle;
        task.last_error = None;
        task.updated_at = timestamp.to_string();
        self.replace(task)
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
                reason: Some(TaskAttentionReason::ExecutionFailed),
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
                harness: HarnessSelection {
                    kind: HarnessKind::TerminalPreset {
                        preset_name: "Codex".to_string(),
                    },
                    model: Some("gpt-5.6".to_string()),
                },
                stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            },
            "2026-08-19T08:00:00Z",
        )
    }

    fn active_attempt(attempt_id: &str, executor: ExecutorTarget) -> TaskExecutionAttempt {
        TaskExecutionAttempt {
            attempt_id: attempt_id.to_string(),
            executor,
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

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(TASKS_FILE_NAME);
        let mut store = TaskStore::load(&path).unwrap();
        let task = sample_task("task-1");
        store.insert(task.clone()).unwrap();
        let reloaded = TaskStore::load(path).unwrap();
        assert_eq!(reloaded.tasks(), &[task]);
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
