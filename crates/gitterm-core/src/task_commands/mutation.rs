//! Mutation decisions are plans, not successful replies. The host executes one
//! effect, waits for its result, and only then acknowledges or advances. Store
//! effects use the existing TaskStore/delegations services (including their
//! validation, atomic saves and partial-save errors). No transport handles live
//! here. Host facts come from the sole live catalog, never caller-typed IDs.
use super::ReplyId;
use crate::session;
use crate::tasks::*;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskCreateOrigin {
    Form,
    Control,
}

#[derive(Debug, Clone)]
pub struct TaskCreate {
    pub title: String,
    pub objective: String,
    pub repository_path: PathBuf,
    pub workspace_name: Option<String>,
    pub issue_key: Option<String>,
    pub base_reference: Option<String>,
    pub stopping_boundary: StoppingBoundary,
    pub origin: TaskCreateOrigin,
}

#[derive(Debug, Clone)]
pub struct HandoffUpdate {
    pub task_id: String,
    pub summary: String,
    pub decisions: Vec<String>,
    pub next_steps: Vec<String>,
    pub blockers: Vec<String>,
    pub session_id: Option<String>,
    pub caller: Option<String>,
    pub status: HandoffStatus,
    pub timestamp: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPublish {
    PushBranch,
    OpenDraftPr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLaunchOrigin {
    User,
    Agent,
}

#[derive(Debug, Clone)]
pub struct SessionLaunch {
    pub task_id: String,
    pub preset_name: Option<String>,
    pub origin: TaskLaunchOrigin,
    pub user_model_choice: bool,
}

/// Process-local, one-shot preparation identity; never persisted or on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerPreparationId(pub(crate) u64);

#[derive(Debug)]
pub enum TaskMutation {
    Create(TaskCreate),
    InsertPreparing {
        record: Box<TaskRecord>,
    },
    PreparationCompleted {
        task_id: String,
        result: Result<CompletedWorktreePreparation, String>,
        cleanup_path: Option<PathBuf>,
    },
    WorktreeRemoved {
        task_id: String,
    },
    RecordDelivery {
        task_id: String,
        commit_sha: String,
        pr_url: Option<String>,
        pr_number: Option<u64>,
    },
    UpdateHandoff(HandoffUpdate),
    /// Close/archive the durable task, preserving its worktree and history.
    Close {
        task_id: String,
    },
    CloseSession {
        target: session::Target,
        confirmed: bool,
    },
    InspectDelete {
        task_id: String,
    },
    CheckWorkerDestination {
        expected: CompletedWorktreePreparation,
    },
    CheckWorkerCheckout {
        task_id: String,
        path: PathBuf,
    },
    DeleteWorktree {
        task_id: String,
        path: PathBuf,
        inspection: CleanupInspection,
    },
    Publish {
        task_id: String,
        action: TaskPublish,
        confirmed: bool,
    },
    LaunchSession(SessionLaunch),
    /// Capture before the first off-thread target check, not at its completion.
    BeginWorkerPreparation {
        caller: String,
    },
    /// Recheck after target resolution and before any checkout-adoption writes.
    CheckWorkerPreparation {
        id: WorkerPreparationId,
    },
    /// Bind after metadata insertion but before starting worktree preparation.
    BindWorkerPreparation {
        id: WorkerPreparationId,
        task_id: String,
        expected: CompletedWorktreePreparation,
    },
    /// Existing-checkout reuse preserves lifecycle, worktree and attention.
    ReuseWorkerCheckout {
        id: WorkerPreparationId,
        replacement: Box<TaskRecord>,
    },
    WorkerCheckoutReuseSaved {
        id: WorkerPreparationId,
        result: Result<(), String>,
    },
    WorkerPrepared {
        id: WorkerPreparationId,
        result: Result<CompletedWorktreePreparation, String>,
    },
    /// Feed the real result of complete_worktree_preparation, never a prediction.
    WorkerPreparationSaved {
        id: WorkerPreparationId,
        result: Result<(), String>,
    },
    CancelWorkerPreparation {
        id: WorkerPreparationId,
    },
}

#[derive(Debug)]
pub struct TaskMutationCommand {
    pub reply: Option<ReplyId>,
    pub mutation: TaskMutation,
}

#[derive(Debug)]
pub struct TaskMutationEffect {
    pub reply: Option<ReplyId>,
    pub action: Result<TaskMutationAction, String>,
}

#[derive(Debug)]
pub enum TaskMutationAction {
    None,
    PrepareTask(TaskCreate),
    PersistPreparing {
        record: Box<TaskRecord>,
    },
    PersistPreparation {
        task_id: String,
        prepared: CompletedWorktreePreparation,
    },
    PersistDelivery {
        task_id: String,
        delivery: DeliveryState,
    },
    /// Run record_handoff, which rechecks report authority before its first
    /// write. Only its successful result permits notification/UI completion.
    PersistHandoff {
        task_id: String,
        handoff: TaskHandoff,
        status: HandoffStatus,
        hosted_session: Option<String>,
    },
    ArchiveTask {
        task_id: String,
    },
    Session(session::Effect),
    InspectCleanup {
        task_id: String,
        repository: PathBuf,
        path: PathBuf,
        branch: String,
        base_commit: String,
        process_running: bool,
        pull_request_url: Option<String>,
    },
    /// Removal is always force=false. Feed success to clear_worktree; do not
    /// clear first. Record-only cleanup preserves session/conversation history.
    RemoveWorktree {
        task_id: String,
        repository: PathBuf,
        path: PathBuf,
    },
    ClearWorktree {
        task_id: String,
    },
    ConfirmPublish {
        task_id: String,
        action: TaskPublish,
    },
    Publish {
        task_id: String,
        action: TaskPublish,
        path: PathBuf,
        branch: String,
        base: String,
        title: String,
        body: String,
    },
    LaunchSession {
        request: SessionLaunch,
        preset_index: Option<usize>,
    },
    /// Persist harness/lifecycle before acknowledging queue admission.
    QueueSession {
        request: SessionLaunch,
        preset_index: usize,
        position: usize,
    },
    WorkerAuthorityChecked {
        id: WorkerPreparationId,
    },
    /// No start/reply accompanies this write. Feed its result back in
    /// WorkerPreparationSaved with a fresh live-facts projection.
    PersistWorkerPreparation {
        id: WorkerPreparationId,
        task_id: String,
        prepared: CompletedWorktreePreparation,
    },
    /// Run fail_worktree_preparation before returning the preparation error.
    /// An authority refusal emits no write; an ordinary failure records Failed.
    PersistWorkerPreparationFailure {
        task_id: String,
        error: String,
        cleanup_path: Option<PathBuf>,
    },
    PersistWorkerCheckoutReuse {
        id: WorkerPreparationId,
        replacement: Box<TaskRecord>,
    },
    StartWorker {
        task_id: String,
        caller: Box<WorkerCaller>,
    },
}

/// Full caller incarnation plus the workspace's checkout authorization roots.
/// Paths/identities must be canonical facts from existing Git/path services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCaller {
    pub target: session::Target,
    pub chat_session_id: Option<String>,
    pub workspace: WorkspaceIdentity,
    pub repository: Option<RepositoryIdentity>,
    pub checkout: PathBuf,
    pub home: PathBuf,
    pub repos: Vec<PathBuf>,
    pub remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCheckout {
    pub path: PathBuf,
    pub repository: RepositoryIdentity,
}

#[derive(Debug, Clone)]
pub struct TaskPreset {
    pub name: String,
    pub command: String,
}

/// Recomputed synchronously for this command. Directory facts must name the
/// actual tested path. Policy facts use current provider/parallelism evaluation
/// with origin and explicit-choice overrides; terminal launches ignore them.
#[derive(Debug, Default)]
pub struct TaskMutationFacts {
    pub sessions: Vec<session::State>,
    pub callers: Vec<WorkerCaller>,
    /// Independently resolved, canonical live destination identities.
    pub checkouts: Vec<WorkerCheckout>,
    /// Current normalized destinations, including not-yet-created paths.
    pub preparation_paths: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    /// Fresh Path::exists facts, including non-directory rollback leftovers.
    pub existing_paths: Vec<PathBuf>,
    /// Caller UIDs whose task sessions are currently Working (versus idle).
    pub working_sessions: Vec<String>,
    pub publish_busy: Vec<String>,
    pub presets: Vec<TaskPreset>,
    pub policy_blocked: bool,
    pub policy_reason: String,
    pub slot_holders: Vec<String>,
    pub queue: Vec<String>,
    pub concurrency_limit: usize,
}
