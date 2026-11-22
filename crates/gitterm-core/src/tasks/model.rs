use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const TASK_STORE_SCHEMA_VERSION: u32 = 1;
pub const TASKS_FILE_NAME: &str = "tasks.json";

// Not `Eq`: review findings carry an `f32` confidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStoreDocument {
    pub schema_version: u32,
    pub tasks: Vec<TaskRecord>,
    /// Additive (TRU-142): documents written before delegations existed
    /// load with an empty list, and an empty list is not written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegations: Vec<Delegation>,
}

impl Default for TaskStoreDocument {
    fn default() -> Self {
        Self {
            schema_version: TASK_STORE_SCHEMA_VERSION,
            tasks: Vec::new(),
            delegations: Vec::new(),
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
    /// A fresh task id: a hyphenated UUID, as Iced's create path makes.
    pub fn fresh_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

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

    /// Title and body for this task's draft pull request. The linked issue
    /// key prefixes the title unless the title already carries it (repo
    /// convention: commits and PRs carry an issue key), and the body links
    /// back to the issue so review lands with its context.
    pub fn draft_pr_copy(&self) -> (String, String) {
        let title = match &self.issue {
            Some(issue) if !self.title.starts_with(&issue.key) => {
                format!("{}: {}", issue.key, self.title)
            }
            _ => self.title.clone(),
        };
        let mut body = self.objective.trim().to_string();
        if let Some(issue) = &self.issue {
            let line = match &issue.url {
                Some(url) => format!("Linked issue: [{}]({url})", issue.key),
                None => format!("Linked issue: {}", issue.key),
            };
            if body.is_empty() {
                body = line;
            } else {
                body = format!("{body}\n\n{line}");
            }
        }
        (title, body)
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
    /// Captured by the launch wrapper, never inferred from a PID at shutdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_identity: Option<LaunchIdentity>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveDeliveryState {
    #[default]
    Unknown,
    NotDelivered,
    /// Initial prompt submitted; awaiting an on-disk harness session.
    Pending,
    Delivered,
}

/// Activity snapshots may be rounded to a minute, but never before launch.
pub fn clamp_activity_to_session_start<'a>(
    activity: Option<&str>,
    starts: impl Iterator<Item = &'a str>,
) -> Option<String> {
    let newest = starts
        .filter_map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .ok()
                .map(|time| (raw, time))
        })
        .max_by_key(|(_, time)| *time);
    match newest {
        Some((raw, started))
            if activity
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .is_none_or(|time| time < started) =>
        {
            Some(raw.to_string())
        }
        _ => activity.map(str::to_string),
    }
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
            // Ready is the cancel path: a queued task that never started can
            // step back out of the queue without pretending it ran.
            Self::Queued => matches!(
                next,
                Self::Ready | Self::Running | Self::Stopped | Self::Failed | Self::Interrupted
            ),
            Self::Running | Self::WaitingForInput => matches!(
                next,
                Self::Queued
                    | Self::Running
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
    /// When the current `reason` was first raised (RFC 3339). Unlike
    /// `updated_at`, this survives unrelated record writes, so the inbox can
    /// order rows by how long they have been waiting without reshuffling
    /// every time progress is captured (TRU-133).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

impl TaskAttention {
    /// Raise `reason` as unread attention. The `since` stamp is kept from
    /// `previous` when it already carries the same reason, so re-raising an
    /// unchanged reason does not make the task look newly waiting.
    pub fn raised(reason: TaskAttentionReason, previous: &Self, timestamp: &str) -> Self {
        let since = match (&previous.reason, &previous.since) {
            (Some(prior), Some(since)) if *prior == reason => Some(since.clone()),
            _ => Some(timestamp.to_string()),
        };
        Self {
            reason: Some(reason),
            unread: true,
            since,
        }
    }
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

    pub(super) fn lifecycle(self) -> TaskLifecycle {
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
    #[cfg(feature = "runtime")]
    pub(super) fn for_lifecycle(lifecycle: TaskLifecycle) -> Option<Self> {
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

/// What a delegation asks the child to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationKind {
    Review,
    Implement,
    /// Free-form advice (brainstorming, architecture, investigation). The
    /// result is a handoff, not findings (TRU-144).
    Consult,
}

impl DelegationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Implement => "implement",
            Self::Consult => "consult",
        }
    }
}

/// The session that asked for the delegation and is told when it lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationParent {
    /// Persisted GitTerm tab identity (S1 caller identity).
    pub session_uid: String,
    /// Harness conversation of the parent chat, so a result can reappear
    /// when that conversation is reopened after its tab was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
    pub workspace: String,
    pub cwd: PathBuf,
}

/// Who carries the delegation out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "runner", rename_all = "snake_case")]
pub enum DelegationChild {
    /// A background `codex exec review` run owned by GitTerm.
    CodexReview {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        /// `-m <model>`; `None` keeps Codex's configured default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// A background `codex exec` consult owned by GitTerm.
    CodexConsult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// A worker session inside a GitTerm task.
    TaskSession {
        task_id: String,
        /// The task session carrying the work out; `None` until the launch
        /// (which the concurrency queue may defer) starts it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<HarnessConversationRef>,
        /// The agent preset the worker runs on (S6, additive).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preset_name: Option<String>,
        /// The model GitTerm passed to the preset's CLI; `None` keeps the
        /// preset's or the CLI's own choice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<WorkerRole>,
    },
    /// Claude's own Agent tool inside the parent chat.
    ClaudeSubagent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReviewTargetMode {
    /// Staged, unstaged and untracked changes against HEAD.
    Uncommitted,
    /// The merge base with `reference` against the working tree (Codex
    /// `--base`, which includes uncommitted changes).
    Base { reference: String },
    /// One commit.
    Commit { sha: String },
}

/// What a review delegation looks at. Distinct from `review::ReviewTarget`,
/// which is the R1 chat-page payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTarget {
    pub mode: ReviewTargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DelegationStatus {
    Requested,
    Running,
    Completed,
    Failed {
        message: String,
    },
    /// GitTerm restarted while the delegation was requested or running.
    Interrupted,
    Cancelled,
    /// A worker reported that it cannot go on without a human (S6). It
    /// carries the worker's handoff as its result, like `Completed`, and a
    /// later `done` from the same worker still completes it.
    Blocked,
}

impl DelegationStatus {
    /// Requested or running: a result may still arrive from the child.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Requested | Self::Running)
    }

    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed { .. } | Self::Cancelled
        )
    }

    /// Transitions allowed through `TaskStore::set_delegation_status`.
    /// `Completed` is reached only through `complete_delegation`, which
    /// carries the result. `Interrupted` may resume to `Running` because a
    /// task-session worker can be resumed after a restart.
    #[cfg(feature = "runtime")]
    pub(super) fn can_transition_to(&self, next: &Self) -> bool {
        matches!(
            (self, next),
            (Self::Requested | Self::Interrupted, Self::Running)
                | (
                    Self::Requested | Self::Running,
                    Self::Failed { .. } | Self::Interrupted | Self::Cancelled,
                )
                | (Self::Interrupted, Self::Failed { .. } | Self::Cancelled)
        )
    }

    /// States `complete_delegation` accepts. A late result for an
    /// interrupted delegation (a resumed worker) is still accepted, and so
    /// is a worker's `done` after it reported itself blocked.
    #[cfg(feature = "runtime")]
    pub(super) fn accepts_completion(&self) -> bool {
        matches!(
            self,
            Self::Requested | Self::Running | Self::Interrupted | Self::Blocked
        )
    }

    /// Whether the delegation carries a result the parent can read and be
    /// sent: a completed one, or a worker's blocked report.
    pub fn has_result(&self) -> bool {
        matches!(self, Self::Completed | Self::Blocked)
    }

    /// Final outcomes eligible for delivery, including failures and time limits.
    pub fn is_sendable(&self) -> bool {
        self.has_result() || matches!(self, Self::Failed { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed { .. } => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Correct,
    NeedsChanges,
    Unknown,
}

/// Codex priority 0-3, rendered as `[P0]`..`[P3]`. P0 is the most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReviewSeverity {
    P0,
    P1,
    P2,
    P3,
}

impl ReviewSeverity {
    pub fn from_priority(priority: u8) -> Option<Self> {
        match priority {
            0 => Some(Self::P0),
            1 => Some(Self::P1),
            2 => Some(Self::P2),
            3 => Some(Self::P3),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// Stable within one result: `F1`, `F2`, ... in reviewer order.
    pub id: String,
    pub severity: ReviewSeverity,
    pub title: String,
    pub body: String,
    /// Repo-relative when the reviewer's path is under the repo root,
    /// otherwise the reviewer's absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewer {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFindings {
    pub verdict: ReviewVerdict,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    pub reviewer: Reviewer,
    /// False when the findings could not be read structurally and
    /// `summary` holds the reviewer's whole text instead.
    pub structured: bool,
}

/// The checkout a review actually looked at, for the stale banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedState {
    pub head: String,
    pub dirty: bool,
    pub target_description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DelegationResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<TaskHandoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<ReviewFindings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<ReviewedState>,
}

/// A parent-addressed request with a typed result, stored next to tasks
/// (TRU-142, plan option B).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delegation {
    pub delegation_id: String,
    pub kind: DelegationKind,
    pub parent: DelegationParent,
    pub child: DelegationChild,
    pub brief: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ReviewTarget>,
    /// The delegation this one re-runs or follows up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The replacement delegation when this blocked worker was superseded.
    /// Its report and delivery receipt stay intact; Completed keeps the
    /// document readable by older builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub status: DelegationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<DelegationResult>,
    pub created_at: String,
    pub updated_at: String,
    /// When the result was sent to the parent, so a replay or restart never
    /// sends it twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    /// Additive delivery flag (TRU-218). Failures leave `delivered_at`
    /// absent because older builds only accept that timestamp on results.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delivered: bool,
    /// When the human dismissed the result card; it then stays collapsed
    /// and leaves the roster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,
    /// Where and how the job runs when the request said so (TRU-212).
    #[serde(default, skip_serializing_if = "DelegationRunOptions::is_default")]
    pub options: DelegationRunOptions,
    /// When GitTerm stopped the job at its time limit (TRU-212). The status
    /// is then `failed` with a message saying so: older GitTerm builds share
    /// `tasks.json` and know no other state for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_reached_at: Option<String>,
    /// Worker launch time + limit, in Unix milliseconds. Additive so older
    /// builds can still read the same task document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_deadline_unix_ms: Option<i64>,
}

/// A session-bound timer identity, stable across app restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerTimeLimit {
    pub delegation_id: String,
    pub session_id: String,
    pub deadline_unix_ms: i64,
}

impl WorkerTimeLimit {
    pub fn remaining(&self, now_unix_ms: i64) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.deadline_unix_ms.saturating_sub(now_unix_ms).max(0) as u64
        )
    }
}

/// A delegation's run options from `review_request`, `consult_request` or
/// `delegate_task` (TRU-212). Paths are stored after GitTerm checked them
/// against the requesting chat's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DelegationRunOptions {
    /// The user explicitly chose the model, rather than the coordinator or
    /// configuration choosing it. Kept through the queue and restarts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_model_choice: bool,
    /// The checkout the job runs in, in place of the calling tab's (a Codex
    /// run) or of a new task worktree (a worker): its git top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<PathBuf>,
    /// Extra writable folders (`--add-dir`), for workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_dirs: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// GitTerm stops the job after this many minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_minutes: Option<u32>,
}

impl DelegationRunOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDelegation {
    pub kind: DelegationKind,
    pub parent: DelegationParent,
    pub child: DelegationChild,
    pub brief: String,
    pub target: Option<ReviewTarget>,
    pub previous: Option<String>,
    pub options: DelegationRunOptions,
}

impl Delegation {
    pub fn is_delivered(&self) -> bool {
        self.delivered || self.delivered_at.is_some()
    }

    /// A new `requested` delegation with a fresh UUID id, the same id
    /// convention as `TaskRecord.task_id`.
    pub fn new_requested(input: NewDelegation, timestamp: impl Into<String>) -> Self {
        let timestamp = timestamp.into();
        Self {
            delegation_id: uuid::Uuid::new_v4().to_string(),
            kind: input.kind,
            parent: input.parent,
            child: input.child,
            brief: input.brief,
            target: input.target,
            previous: input.previous,
            superseded_by: None,
            status: DelegationStatus::Requested,
            result: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            delivered_at: None,
            delivered: false,
            dismissed_at: None,
            options: input.options,
            time_limit_reached_at: None,
            time_limit_deadline_unix_ms: None,
        }
    }

    /// The checkout the job runs in: the requested one, else the calling
    /// tab's (TRU-212).
    pub fn run_checkout(&self) -> &Path {
        self.options.checkout.as_deref().unwrap_or(&self.parent.cwd)
    }

    /// The Codex run's thread id and model, for a delegation GitTerm runs
    /// through `codex_runner`. `None` for any other child.
    pub fn codex_child(&self) -> Option<(Option<&str>, Option<&str>)> {
        match &self.child {
            DelegationChild::CodexReview { thread_id, model }
            | DelegationChild::CodexConsult { thread_id, model } => {
                Some((thread_id.as_deref(), model.as_deref()))
            }
            _ => None,
        }
    }

    #[cfg(feature = "runtime")]
    pub(super) fn record_worker_failure(&mut self, timestamp: &str) {
        if let (
            DelegationChild::TaskSession {
                task_session_id, ..
            },
            DelegationStatus::Failed { message },
        ) = (&self.child, &self.status)
        {
            // A timeout/launch failure may have no worker-authored report.
            // Persist its outcome before delivery and automatic shutdown.
            self.result.get_or_insert_with(|| DelegationResult {
                handoff: Some(TaskHandoff {
                    summary: message.clone(),
                    decisions: vec![],
                    next_steps: vec![],
                    blockers: vec![],
                    updated_by_session_id: task_session_id.clone(),
                    updated_at: timestamp.to_string(),
                }),
                ..Default::default()
            });
            self.delivered = false;
            self.delivered_at = None;
        }
    }
}

/// What kind of work a worker is trusted with. The coordinator (the chat
/// tab's own Claude) is not a worker role: Tracey picks that model herself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    /// Routine coding with resolved decisions and established patterns.
    #[default]
    Scoped,
    /// Harder coding that needs technical judgment or new design.
    Judgment,
    /// Review, investigation, brainstorming.
    Specialist,
}

impl WorkerRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Judgment => "judgment",
            Self::Specialist => "specialist",
        }
    }
}

/// A worker's or Codex run's reasoning effort (TRU-212).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Codex's `-c` value: `model_reasoning_effort` is the config key
    /// (`~/.codex/config.toml` uses it; codex-cli 0.164.0 knows low,
    /// medium, high and xhigh), the value a TOML string.
    pub fn codex_config_value(self) -> String {
        format!("model_reasoning_effort=\"{}\"", self.label())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchIdentity {
    pub pid: u32,
    pub birth: String,
}

/// What a `task_update_handoff` says about the work (TRU-142 S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    /// Still working; the delegating chat's card shows the summary line.
    #[default]
    Progress,
    /// The objective is met: the delegation completes with this handoff.
    Done,
    /// Cannot go on without a human: blockers say why.
    Blocked,
}

impl HandoffStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupRisk {
    ActiveProcess,
    WorktreeMissing,
    WorktreeNotRegistered,
    UncommittedChanges,
    UnpushedCommits { count: u64 },
    ExistingPullRequest { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupInspection {
    pub path_exists: bool,
    pub registered: bool,
    pub dirty: bool,
    pub unpushed_commits: u64,
    pub upstream: Option<String>,
    pub risks: Vec<CleanupRisk>,
}

impl CleanupInspection {
    pub fn safe_to_remove(&self) -> bool {
        self.risks.is_empty()
    }
}
