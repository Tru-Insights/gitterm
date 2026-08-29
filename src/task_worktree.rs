use crate::agentd::git::git_command;
use crate::gh_identity::{self, GhIdentity};
use crate::tasks::{CompletedWorktreePreparation, GitBase, RepositoryIdentity};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;

/// Internal request marker for the product default: prefer `develop`, then
/// `main`, and fall back to the current branch only when neither exists.
pub const DEFAULT_TASK_BASE: &str = "__gitterm_default_task_base__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepository {
    pub top_level: PathBuf,
    pub identity: RepositoryIdentity,
    pub current_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWorktreeProposal {
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareTaskWorktreeRequest {
    pub repository_path: PathBuf,
    pub worktree_root: PathBuf,
    pub task_id: String,
    pub title: String,
    pub issue_key: Option<String>,
    pub base_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTaskWorktree {
    pub repository: ResolvedRepository,
    pub base: GitBase,
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTaskPreparation {
    pub repository: ResolvedRepository,
    pub base: GitBase,
    pub proposal: TaskWorktreeProposal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptWorktreeRequest {
    pub repository_path: PathBuf,
    pub worktree_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedWorktreeResolution {
    pub repository: ResolvedRepository,
    pub base: GitBase,
    pub branch: String,
}

impl From<PreparedTaskWorktree> for CompletedWorktreePreparation {
    fn from(prepared: PreparedTaskWorktree) -> Self {
        Self {
            repository: prepared.repository.identity,
            base: prepared.base,
            branch: prepared.branch,
            path: prepared.path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupContext {
    pub process_running: bool,
    pub pull_request_url: Option<String>,
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

/// A pull request found for a task branch during delivery discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPullRequest {
    pub url: String,
    pub number: u64,
    pub head_sha: String,
    pub is_draft: bool,
}

/// Snapshot of what a task worktree has delivered: its current HEAD and any
/// open pull request for its branch. Discovery only reads state — it never
/// pushes or publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryDiscovery {
    pub local_head: String,
    pub pull_request: Option<DiscoveredPullRequest>,
}

#[derive(Debug, Clone)]
pub struct TaskWorktreeError {
    operation: &'static str,
    path: PathBuf,
    detail: String,
}

impl TaskWorktreeError {
    fn new(operation: &'static str, path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            operation,
            path: path.into(),
            detail: detail.into(),
        }
    }

    pub fn operation(&self) -> &str {
        self.operation
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for TaskWorktreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to {} task worktree for {}: {}",
            self.operation,
            self.path.display(),
            self.detail
        )
    }
}

impl std::error::Error for TaskWorktreeError {}

pub fn resolve_repository(path: &Path) -> Result<ResolvedRepository, TaskWorktreeError> {
    let output = run_git(
        path,
        "resolve repository",
        &[
            "--no-optional-locks",
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )?;
    let text = stdout_text(&output, path, "resolve repository")?;
    let mut lines = text.lines();
    let top_level = required_path(lines.next(), path, "repository top-level")?;
    let common_dir = required_path(lines.next(), path, "repository common directory")?;
    if lines.next().is_some() {
        return Err(TaskWorktreeError::new(
            "resolve repository",
            path,
            "git returned unexpected extra repository identity lines",
        ));
    }
    let top_level = fs::canonicalize(&top_level).map_err(|error| {
        TaskWorktreeError::new(
            "canonicalize repository top-level",
            &top_level,
            error.to_string(),
        )
    })?;
    let common_dir = fs::canonicalize(&common_dir).map_err(|error| {
        TaskWorktreeError::new(
            "canonicalize repository common directory",
            &common_dir,
            error.to_string(),
        )
    })?;

    let current_branch_output = git_command()
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(&top_level)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new("read current branch", &top_level, error.to_string())
        })?;
    let current_branch = if current_branch_output.status.success() {
        Some(
            stdout_text(&current_branch_output, &top_level, "read current branch")?
                .trim()
                .to_string(),
        )
    } else if current_branch_output.status.code() == Some(1) {
        None
    } else {
        return Err(command_failure(
            "read current branch",
            &top_level,
            &current_branch_output,
        ));
    };

    let remote_output = git_command()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(&top_level)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new("read origin URL", &top_level, error.to_string())
        })?;
    let remote_url = if remote_output.status.success() {
        Some(
            stdout_text(&remote_output, &top_level, "read origin URL")?
                .trim()
                .to_string(),
        )
    } else if remote_output.status.code() == Some(1) {
        None
    } else {
        return Err(command_failure(
            "read origin URL",
            &top_level,
            &remote_output,
        ));
    };

    Ok(ResolvedRepository {
        top_level,
        identity: RepositoryIdentity {
            common_dir,
            remote_url,
        },
        current_branch,
    })
}

pub fn resolve_base(
    repository: &ResolvedRepository,
    reference: &str,
) -> Result<GitBase, TaskWorktreeError> {
    if reference.trim().is_empty() {
        return Err(TaskWorktreeError::new(
            "resolve base",
            &repository.top_level,
            "base reference is empty",
        ));
    }
    let commit_expression = format!("{reference}^{{commit}}");
    let output = run_git(
        &repository.top_level,
        "resolve base",
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &commit_expression,
        ],
    )?;
    let commit = stdout_text(&output, &repository.top_level, "resolve base")?
        .trim()
        .to_string();
    if commit.is_empty() {
        return Err(TaskWorktreeError::new(
            "resolve base",
            &repository.top_level,
            format!("base reference {reference:?} resolved to an empty commit"),
        ));
    }
    Ok(GitBase {
        reference: reference.to_string(),
        commit,
    })
}

pub fn propose_task_worktree(
    repository: &ResolvedRepository,
    worktree_root: &Path,
    task_id: &str,
    issue_key: Option<&str>,
    title: &str,
) -> Result<TaskWorktreeProposal, TaskWorktreeError> {
    if !worktree_root.is_absolute() {
        return Err(TaskWorktreeError::new(
            "propose",
            worktree_root,
            "configured worktree root must be an absolute path",
        ));
    }
    let branch = suggested_task_branch(task_id, issue_key, title).ok_or_else(|| {
        TaskWorktreeError::new(
            "propose",
            worktree_root,
            "issue key, task id, or title does not contain a usable branch value",
        )
    })?;
    let branch_suffix = branch
        .strip_prefix("task/")
        .ok_or_else(|| {
            TaskWorktreeError::new(
                "propose",
                worktree_root,
                "generated task branch is missing its task/ prefix",
            )
        })?
        .to_string();
    validate_branch(&repository.top_level, &branch)?;

    let repository_name = repository
        .top_level
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| slug(name, false, 48))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            TaskWorktreeError::new(
                "propose",
                &repository.top_level,
                "repository top-level has no usable directory name",
            )
        })?;
    Ok(TaskWorktreeProposal {
        branch,
        path: worktree_root.join(repository_name).join(branch_suffix),
    })
}

pub fn suggested_task_branch(
    task_id: &str,
    issue_key: Option<&str>,
    title: &str,
) -> Option<String> {
    let identifier = slug(issue_key.unwrap_or(task_id), true, 48);
    let title = slug(title, false, 48);
    (!identifier.is_empty() && !title.is_empty()).then(|| format!("task/{identifier}-{title}"))
}

pub fn suggested_task_worktree_path(
    repository_path: &Path,
    worktree_root: &Path,
    branch: &str,
) -> Option<PathBuf> {
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| slug(name, false, 48))
        .filter(|name| !name.is_empty())?;
    let suffix = branch.strip_prefix("task/")?;
    Some(worktree_root.join(repository_name).join(suffix))
}

pub async fn prepare_task_worktree(
    request: PrepareTaskWorktreeRequest,
) -> Result<PreparedTaskWorktree, TaskWorktreeError> {
    let error_path = request.repository_path.clone();
    tokio::task::spawn_blocking(move || prepare_task_worktree_blocking(&request))
        .await
        .map_err(|error| {
            TaskWorktreeError::new("join preparation worker", error_path, error.to_string())
        })?
}

pub async fn resolve_task_preparation(
    request: PrepareTaskWorktreeRequest,
) -> Result<ResolvedTaskPreparation, TaskWorktreeError> {
    let error_path = request.repository_path.clone();
    tokio::task::spawn_blocking(move || resolve_task_preparation_blocking(&request))
        .await
        .map_err(|error| {
            TaskWorktreeError::new("join resolution worker", error_path, error.to_string())
        })?
}

pub fn resolve_task_preparation_blocking(
    request: &PrepareTaskWorktreeRequest,
) -> Result<ResolvedTaskPreparation, TaskWorktreeError> {
    let repository = resolve_repository(&request.repository_path)?;
    let base = if request.base_reference == DEFAULT_TASK_BASE {
        let preferred = ["develop", "main"]
            .into_iter()
            .find_map(|reference| resolve_base(&repository, reference).ok());
        if let Some(base) = preferred {
            base
        } else {
            resolve_base(
                &repository,
                repository.current_branch.as_deref().unwrap_or("HEAD"),
            )?
        }
    } else {
        resolve_base(&repository, &request.base_reference)?
    };
    let proposal = propose_task_worktree(
        &repository,
        &request.worktree_root,
        &request.task_id,
        request.issue_key.as_deref(),
        &request.title,
    )?;
    reject_collisions(&repository, &proposal)?;
    Ok(ResolvedTaskPreparation {
        repository,
        base,
        proposal,
    })
}

pub fn resolve_worktree_adoption_blocking(
    request: &AdoptWorktreeRequest,
) -> Result<AdoptedWorktreeResolution, TaskWorktreeError> {
    let repository = resolve_repository(&request.repository_path)?;
    let registered = registered_worktrees(&repository.top_level)?
        .iter()
        .any(|path| paths_equal(path, &request.worktree_path));
    if !registered {
        return Err(TaskWorktreeError::new(
            "adopt worktree",
            &request.worktree_path,
            "path is not a registered worktree of this repository",
        ));
    }
    let branch_output = git_command()
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(&request.worktree_path)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new(
                "read worktree branch",
                &request.worktree_path,
                error.to_string(),
            )
        })?;
    if !branch_output.status.success() {
        return Err(TaskWorktreeError::new(
            "adopt worktree",
            &request.worktree_path,
            "worktree is on a detached HEAD; check out a branch before adopting it",
        ));
    }
    let branch = stdout_text(
        &branch_output,
        &request.worktree_path,
        "read worktree branch",
    )?
    .trim()
    .to_string();
    if branch.is_empty() {
        return Err(TaskWorktreeError::new(
            "adopt worktree",
            &request.worktree_path,
            "worktree branch name is empty",
        ));
    }
    let base = infer_adoption_base(&repository, &branch)?;
    Ok(AdoptedWorktreeResolution {
        repository,
        base,
        branch,
    })
}

/// Off-thread wrapper over [`resolve_worktree_adoption_blocking`] for UI callers.
pub async fn resolve_worktree_adoption(
    request: AdoptWorktreeRequest,
) -> Result<AdoptedWorktreeResolution, TaskWorktreeError> {
    let error_path = request.repository_path.clone();
    tokio::task::spawn_blocking(move || resolve_worktree_adoption_blocking(&request))
        .await
        .map_err(|error| {
            TaskWorktreeError::new("join adoption worker", error_path, error.to_string())
        })?
}

/// Prefer the remote default branch, then the product defaults, then the main
/// checkout's branch; a branch can never be its own base. The recorded base
/// commit is the fork point when one exists — for a pre-existing branch that
/// is the honest anchor for "what changed" comparisons.
fn infer_adoption_base(
    repository: &ResolvedRepository,
    branch: &str,
) -> Result<GitBase, TaskWorktreeError> {
    let origin_head = git_command()
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .current_dir(&repository.top_level)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut candidates: Vec<String> = Vec::new();
    candidates.extend(origin_head);
    candidates.push("develop".to_string());
    candidates.push("main".to_string());
    if let Some(current) = &repository.current_branch {
        candidates.push(current.clone());
    }
    let mut base = None;
    for reference in candidates {
        if reference == branch || reference.strip_prefix("origin/") == Some(branch) {
            continue;
        }
        if let Ok(resolved) = resolve_base(repository, &reference) {
            base = Some(resolved);
            break;
        }
    }
    let mut base = base.ok_or_else(|| {
        TaskWorktreeError::new(
            "infer adoption base",
            &repository.top_level,
            format!("no base branch could be resolved for {branch}"),
        )
    })?;
    let merge_base_output = git_command()
        .args(["merge-base", &base.commit, branch])
        .current_dir(&repository.top_level)
        .output();
    if let Ok(output) = merge_base_output {
        if output.status.success() {
            let fork_point = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !fork_point.is_empty() {
                base.commit = fork_point;
            }
        }
    }
    Ok(base)
}

pub fn prepare_task_worktree_blocking(
    request: &PrepareTaskWorktreeRequest,
) -> Result<PreparedTaskWorktree, TaskWorktreeError> {
    prepare_task_worktree_with(request, || Ok(()))
}

fn prepare_task_worktree_with<F>(
    request: &PrepareTaskWorktreeRequest,
    after_add: F,
) -> Result<PreparedTaskWorktree, TaskWorktreeError>
where
    F: FnOnce() -> Result<(), String>,
{
    let resolved = resolve_task_preparation_blocking(request)?;
    let repository = resolved.repository;
    let base = resolved.base;
    let proposal = resolved.proposal;
    if request.worktree_root.starts_with(&repository.top_level) {
        return Err(TaskWorktreeError::new(
            "validate worktree root",
            &request.worktree_root,
            "configured worktree root must not be inside the source checkout",
        ));
    }

    let parent = proposal.path.parent().ok_or_else(|| {
        TaskWorktreeError::new(
            "prepare",
            &proposal.path,
            "worktree path has no parent directory",
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        TaskWorktreeError::new("create worktree parent", parent, error.to_string())
    })?;
    ensure_safe_worktree_path(&repository, &request.worktree_root, &proposal.path)?;

    let output = git_command()
        .args(["worktree", "add", "-b"])
        .arg(&proposal.branch)
        .arg(&proposal.path)
        .arg(&base.commit)
        .current_dir(&repository.top_level)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new("run git worktree add", &proposal.path, error.to_string())
        })?;
    if !output.status.success() {
        let primary = command_failure("run git worktree add", &proposal.path, &output);
        return Err(rollback_failed_preparation(
            &repository,
            &proposal,
            &base.commit,
            primary,
        ));
    }

    if let Err(detail) = after_add() {
        let primary = TaskWorktreeError::new("verify prepared worktree", &proposal.path, detail);
        return Err(rollback_failed_preparation(
            &repository,
            &proposal,
            &base.commit,
            primary,
        ));
    }

    let verification = verify_prepared_worktree(&repository, &proposal, &base.commit);
    if let Err(primary) = verification {
        return Err(rollback_failed_preparation(
            &repository,
            &proposal,
            &base.commit,
            primary,
        ));
    }
    let path = fs::canonicalize(&proposal.path).map_err(|error| {
        TaskWorktreeError::new(
            "canonicalize prepared path",
            &proposal.path,
            error.to_string(),
        )
    })?;

    Ok(PreparedTaskWorktree {
        repository,
        base,
        branch: proposal.branch,
        path,
    })
}

/// Off-thread wrapper over [`inspect_cleanup`] for UI callers.
pub async fn inspect_task_cleanup(
    repository_path: PathBuf,
    worktree_path: PathBuf,
    branch: String,
    base_commit: String,
    context: CleanupContext,
) -> Result<CleanupInspection, TaskWorktreeError> {
    let error_path = repository_path.clone();
    tokio::task::spawn_blocking(move || {
        inspect_cleanup(
            &repository_path,
            &worktree_path,
            &branch,
            &base_commit,
            &context,
        )
    })
    .await
    .map_err(|error| {
        TaskWorktreeError::new(
            "join cleanup inspection worker",
            error_path,
            error.to_string(),
        )
    })?
}

/// Off-thread wrapper over [`discover_delivery_blocking`] for UI callers.
pub async fn discover_task_delivery(
    worktree_path: PathBuf,
    branch: String,
    gh_account: Option<String>,
) -> Result<DeliveryDiscovery, TaskWorktreeError> {
    let error_path = worktree_path.clone();
    tokio::task::spawn_blocking(move || {
        let identity = resolve_gh_identity(&worktree_path, gh_account.as_deref())?;
        discover_delivery_blocking(&worktree_path, &branch, &identity)
    })
    .await
    .map_err(|error| {
        TaskWorktreeError::new(
            "join delivery discovery worker",
            error_path,
            error.to_string(),
        )
    })?
}

fn discover_delivery_blocking(
    worktree_path: &Path,
    branch: &str,
    identity: &GhIdentity,
) -> Result<DeliveryDiscovery, TaskWorktreeError> {
    let operation = "read task worktree head";
    let output = run_git(worktree_path, operation, &["rev-parse", "HEAD"])?;
    let local_head = stdout_text(&output, worktree_path, operation)?
        .trim()
        .to_string();
    let pull_request = lookup_branch_pull_request(worktree_path, branch, identity)?;
    Ok(DeliveryDiscovery {
        local_head,
        pull_request,
    })
}

/// Pushes a task branch to origin (setting upstream on first push) and
/// re-reads delivery state so the caller gets a fresh snapshot in one trip.
/// Explicit invocation only — nothing in discovery calls this.
pub async fn push_task_branch(
    worktree_path: PathBuf,
    branch: String,
    gh_account: Option<String>,
) -> Result<DeliveryDiscovery, TaskWorktreeError> {
    let error_path = worktree_path.clone();
    tokio::task::spawn_blocking(move || {
        let identity = resolve_gh_identity(&worktree_path, gh_account.as_deref())?;
        push_branch_as(&worktree_path, &branch, &identity)?;
        discover_delivery_blocking(&worktree_path, &branch, &identity)
    })
    .await
    .map_err(|error| TaskWorktreeError::new("join push worker", error_path, error.to_string()))?
}

/// Pushes the branch, opens a draft pull request via `gh pr create`, and
/// re-reads delivery state so the new PR lands in the same snapshot shape as
/// discovery. Draft is not optional — review-readiness belongs to /pr-ready,
/// never to local intent.
pub async fn open_task_draft_pr(
    worktree_path: PathBuf,
    branch: String,
    base: String,
    title: String,
    body: String,
    gh_account: Option<String>,
) -> Result<DeliveryDiscovery, TaskWorktreeError> {
    let error_path = worktree_path.clone();
    tokio::task::spawn_blocking(move || {
        let identity = resolve_gh_identity(&worktree_path, gh_account.as_deref())?;
        push_branch_as(&worktree_path, &branch, &identity)?;
        run_gh(
            &worktree_path,
            &identity,
            "open draft pull request",
            &[
                "pr", "create", "--draft", "--head", &branch, "--base", &base, "--title", &title,
                "--body", &body,
            ],
        )?;
        discover_delivery_blocking(&worktree_path, &branch, &identity)
    })
    .await
    .map_err(|error| {
        TaskWorktreeError::new("join draft PR worker", error_path, error.to_string())
    })?
}

/// The branch name `gh pr create --base` expects: a stored base reference may
/// carry a remote or full-ref prefix, but the GitHub API wants the bare name.
pub fn pr_base_branch(reference: &str) -> &str {
    reference
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| reference.strip_prefix("refs/heads/"))
        .or_else(|| reference.strip_prefix("origin/"))
        .unwrap_or(reference)
}

/// Asks the `gh` CLI for an open pull request whose head is `branch`, run from
/// the worktree so gh resolves the repository from its origin. Returns Ok(None)
/// when no open PR exists; a merged or closed PR is deliberately not reported —
/// callers keep their last-known record instead.
fn lookup_branch_pull_request(
    worktree_path: &Path,
    branch: &str,
    identity: &GhIdentity,
) -> Result<Option<DiscoveredPullRequest>, TaskWorktreeError> {
    let operation = "look up task pull request";
    let output = run_gh(
        worktree_path,
        identity,
        operation,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--json",
            "url,number,headRefOid,isDraft",
            "--limit",
            "1",
        ],
    )?;
    let stdout = stdout_text(&output, worktree_path, operation)?;
    parse_pull_request_rows(stdout)
        .map_err(|detail| TaskWorktreeError::new(operation, worktree_path, detail))
}

/// Resolves which gh account acts for the worktree's repository — the
/// workspace-pinned login when set, otherwise the logged-in account that can
/// push there. Task delivery never depends on the globally active gh account.
fn resolve_gh_identity(
    worktree_path: &Path,
    gh_account: Option<&str>,
) -> Result<GhIdentity, TaskWorktreeError> {
    gh_identity::resolve(worktree_path, gh_account).map_err(|error| {
        TaskWorktreeError::new("resolve gh account for", worktree_path, error.to_string())
    })
}

/// `git push --set-upstream origin <branch>` as `identity`: the gh credential
/// helper honors `GH_TOKEN`, so HTTPS remotes push under the same login gh
/// will use for the pull request. SSH remotes ignore it and use the key.
fn push_branch_as(
    worktree_path: &Path,
    branch: &str,
    identity: &GhIdentity,
) -> Result<Output, TaskWorktreeError> {
    let operation = "push task branch";
    let mut command = git_command();
    identity.apply(&mut command);
    let output = command
        .args(["push", "--set-upstream", "origin", branch])
        .current_dir(worktree_path)
        .output()
        .map_err(|error| TaskWorktreeError::new(operation, worktree_path, error.to_string()))?;
    if output.status.success() {
        Ok(output)
    } else {
        gh_identity::forget(worktree_path);
        Err(command_failure(operation, worktree_path, &output))
    }
}

/// Runs the `gh` CLI in a worktree as `identity`, surfacing nonzero exits
/// with stderr and the account that was used. A failure drops the cached
/// identity so the next attempt re-resolves (revoked token, changed login).
fn run_gh(
    worktree_path: &Path,
    identity: &GhIdentity,
    operation: &'static str,
    arguments: &[&str],
) -> Result<Output, TaskWorktreeError> {
    let mut command = gh_identity::gh_command()
        .map_err(|error| TaskWorktreeError::new(operation, worktree_path, error.to_string()))?;
    identity.apply(&mut command);
    let output = command
        .args(arguments)
        .current_dir(worktree_path)
        .output()
        .map_err(|error| TaskWorktreeError::new(operation, worktree_path, error.to_string()))?;
    if !output.status.success() {
        gh_identity::forget(worktree_path);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(TaskWorktreeError::new(
            operation,
            worktree_path,
            format!(
                "gh (as {}) exited with status {}: {stderr}",
                identity.account(),
                output.status
            ),
        ));
    }
    Ok(output)
}

fn parse_pull_request_rows(json: &str) -> Result<Option<DiscoveredPullRequest>, String> {
    #[derive(serde::Deserialize)]
    struct Row {
        url: String,
        number: u64,
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
        #[serde(rename = "isDraft")]
        is_draft: bool,
    }
    let rows: Vec<Row> =
        serde_json::from_str(json).map_err(|error| format!("unexpected gh output: {error}"))?;
    Ok(rows.into_iter().next().map(|row| DiscoveredPullRequest {
        url: row.url,
        number: row.number,
        head_sha: row.head_ref_oid,
        is_draft: row.is_draft,
    }))
}

/// Removes a task's managed worktree. Callers must run [`inspect_cleanup`]
/// first and only proceed when `safe_to_remove()`; this function additionally
/// refuses (git rejects removal of dirty trees without `--force`) as a second
/// line of defense. The task branch is deliberately left in place — removal
/// deletes the working copy, never history.
pub fn remove_task_worktree_blocking(
    repository_path: &Path,
    worktree_path: &Path,
    force: bool,
) -> Result<(), TaskWorktreeError> {
    let repository = resolve_repository(repository_path)?;
    let registered = registered_worktrees(&repository.top_level)?
        .iter()
        .any(|path| paths_equal(path, worktree_path));
    if !registered {
        return Err(TaskWorktreeError::new(
            "remove worktree",
            worktree_path,
            "path is not a registered worktree of this repository",
        ));
    }
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    let output = git_command()
        .args(args)
        .arg(worktree_path)
        .current_dir(&repository.top_level)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new("remove worktree", worktree_path, error.to_string())
        })?;
    if !output.status.success() {
        return Err(command_failure("remove worktree", worktree_path, &output));
    }
    Ok(())
}

/// Off-thread wrapper over [`remove_task_worktree_blocking`] for UI callers.
pub async fn remove_task_worktree(
    repository_path: PathBuf,
    worktree_path: PathBuf,
    force: bool,
) -> Result<(), TaskWorktreeError> {
    let error_path = repository_path.clone();
    tokio::task::spawn_blocking(move || {
        remove_task_worktree_blocking(&repository_path, &worktree_path, force)
    })
    .await
    .map_err(|error| {
        TaskWorktreeError::new(
            "join worktree removal worker",
            error_path,
            error.to_string(),
        )
    })?
}

pub fn prune_worktrees_blocking(repository_path: &Path) -> Result<(), TaskWorktreeError> {
    let repository = resolve_repository(repository_path)?;
    let output = git_command()
        .args(["worktree", "prune"])
        .current_dir(&repository.top_level)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new("prune worktrees", &repository.top_level, error.to_string())
        })?;
    if !output.status.success() {
        return Err(command_failure(
            "prune worktrees",
            &repository.top_level,
            &output,
        ));
    }
    Ok(())
}

/// Off-thread wrapper over [`prune_worktrees_blocking`] for UI callers.
pub async fn prune_worktrees(repository_path: PathBuf) -> Result<(), TaskWorktreeError> {
    let error_path = repository_path.clone();
    tokio::task::spawn_blocking(move || prune_worktrees_blocking(&repository_path))
        .await
        .map_err(|error| {
            TaskWorktreeError::new("join worktree prune worker", error_path, error.to_string())
        })?
}

pub fn inspect_cleanup(
    repository_path: &Path,
    worktree_path: &Path,
    branch: &str,
    base_commit: &str,
    context: &CleanupContext,
) -> Result<CleanupInspection, TaskWorktreeError> {
    let repository = resolve_repository(repository_path)?;
    validate_branch(&repository.top_level, branch)?;
    let path_exists = worktree_path.exists();
    let registered = registered_worktrees(&repository.top_level)?
        .iter()
        .any(|path| paths_equal(path, worktree_path));
    let mut risks = Vec::new();
    if context.process_running {
        risks.push(CleanupRisk::ActiveProcess);
    }
    if !path_exists {
        risks.push(CleanupRisk::WorktreeMissing);
    }
    if !registered {
        risks.push(CleanupRisk::WorktreeNotRegistered);
    }

    let dirty = if path_exists {
        !run_git(
            worktree_path,
            "inspect changes",
            &["status", "--porcelain=v1"],
        )?
        .stdout
        .is_empty()
    } else {
        false
    };
    if dirty {
        risks.push(CleanupRisk::UncommittedChanges);
    }

    let upstream = branch_upstream(&repository.top_level, branch)?;
    let comparison = upstream.as_deref().unwrap_or(base_commit);
    let unpushed_commits = rev_count(
        &repository.top_level,
        &format!("{comparison}..{branch}"),
        "inspect unpushed commits",
    )?;
    if unpushed_commits > 0 {
        risks.push(CleanupRisk::UnpushedCommits {
            count: unpushed_commits,
        });
    }
    if let Some(url) = context.pull_request_url.as_ref() {
        risks.push(CleanupRisk::ExistingPullRequest { url: url.clone() });
    }

    Ok(CleanupInspection {
        path_exists,
        registered,
        dirty,
        unpushed_commits,
        upstream,
        risks,
    })
}

fn reject_collisions(
    repository: &ResolvedRepository,
    proposal: &TaskWorktreeProposal,
) -> Result<(), TaskWorktreeError> {
    if proposal.path.exists() {
        return Err(TaskWorktreeError::new(
            "check collisions",
            &proposal.path,
            "proposed worktree path already exists",
        ));
    }
    if registered_worktrees(&repository.top_level)?
        .iter()
        .any(|path| paths_equal(path, &proposal.path))
    {
        return Err(TaskWorktreeError::new(
            "check collisions",
            &proposal.path,
            "proposed worktree path is already registered",
        ));
    }
    let branch_ref = format!("refs/heads/{}", proposal.branch);
    let output = git_command()
        .args(["show-ref", "--verify", "--quiet", &branch_ref])
        .current_dir(&repository.top_level)
        .output()
        .map_err(|error| {
            TaskWorktreeError::new(
                "check branch collision",
                &repository.top_level,
                error.to_string(),
            )
        })?;
    if output.status.success() {
        return Err(TaskWorktreeError::new(
            "check collisions",
            &repository.top_level,
            format!("proposed branch {} already exists", proposal.branch),
        ));
    }
    if output.status.code() != Some(1) {
        return Err(command_failure(
            "check branch collision",
            &repository.top_level,
            &output,
        ));
    }
    Ok(())
}

fn verify_prepared_worktree(
    repository: &ResolvedRepository,
    proposal: &TaskWorktreeProposal,
    base_commit: &str,
) -> Result<(), TaskWorktreeError> {
    let head = stdout_text(
        &run_git(
            &proposal.path,
            "verify prepared HEAD",
            &["rev-parse", "HEAD"],
        )?,
        &proposal.path,
        "verify prepared HEAD",
    )?
    .trim()
    .to_string();
    if head != base_commit {
        return Err(TaskWorktreeError::new(
            "verify prepared HEAD",
            &proposal.path,
            format!("expected {base_commit}, found {head}"),
        ));
    }
    let branch = stdout_text(
        &run_git(
            &proposal.path,
            "verify prepared branch",
            &["symbolic-ref", "--short", "HEAD"],
        )?,
        &proposal.path,
        "verify prepared branch",
    )?
    .trim()
    .to_string();
    if branch != proposal.branch {
        return Err(TaskWorktreeError::new(
            "verify prepared branch",
            &proposal.path,
            format!("expected {}, found {branch}", proposal.branch),
        ));
    }
    let prepared_repository = resolve_repository(&proposal.path)?;
    if prepared_repository.identity.common_dir != repository.identity.common_dir {
        return Err(TaskWorktreeError::new(
            "verify repository identity",
            &proposal.path,
            "prepared worktree belongs to a different repository common directory",
        ));
    }
    Ok(())
}

fn rollback_failed_preparation(
    repository: &ResolvedRepository,
    proposal: &TaskWorktreeProposal,
    base_commit: &str,
    primary: TaskWorktreeError,
) -> TaskWorktreeError {
    let mut rollback_failures = Vec::new();
    let mut preserve_created_artifacts = false;
    let dirty = if proposal.path.exists() {
        match git_command()
            .args(["status", "--porcelain=v1"])
            .current_dir(&proposal.path)
            .output()
        {
            Ok(output) if output.status.success() => !output.stdout.is_empty(),
            Ok(output) => {
                preserve_created_artifacts = true;
                rollback_failures.push(
                    command_failure("inspect rollback changes", &proposal.path, &output)
                        .to_string(),
                );
                false
            }
            Err(error) => {
                preserve_created_artifacts = true;
                rollback_failures.push(format!(
                    "failed to inspect rollback changes in {}: {error}",
                    proposal.path.display()
                ));
                false
            }
        }
    } else {
        false
    };
    if dirty {
        preserve_created_artifacts = true;
        rollback_failures
            .push("prepared path contains changes; preserved it for manual cleanup".to_string());
    }
    if !preserve_created_artifacts {
        let registered = match registered_worktrees(&repository.top_level) {
            Ok(paths) => paths.iter().any(|path| paths_equal(path, &proposal.path)),
            Err(error) => {
                preserve_created_artifacts = true;
                rollback_failures.push(error.to_string());
                false
            }
        };
        if registered && !preserve_created_artifacts {
            let output = git_command()
                .args(["worktree", "remove", "--force"])
                .arg(&proposal.path)
                .current_dir(&repository.top_level)
                .output();
            match output {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    preserve_created_artifacts = true;
                    rollback_failures.push(
                        command_failure("roll back worktree", &proposal.path, &output).to_string(),
                    );
                }
                Err(error) => {
                    preserve_created_artifacts = true;
                    rollback_failures.push(format!(
                        "failed to run worktree rollback for {}: {error}",
                        proposal.path.display()
                    ));
                }
            }
        }
        if !preserve_created_artifacts && proposal.path.exists() {
            if let Err(error) = remove_created_path(&proposal.path) {
                preserve_created_artifacts = true;
                rollback_failures.push(format!(
                    "failed to remove created path {}: {error}",
                    proposal.path.display()
                ));
            }
        }
    }

    let branch_ref = format!("refs/heads/{}", proposal.branch);
    let branch_exists = git_command()
        .args(["show-ref", "--verify", "--quiet", &branch_ref])
        .current_dir(&repository.top_level)
        .output();
    match branch_exists {
        Ok(output) if output.status.success() => {
            let commit = git_command()
                .args(["rev-parse", "--verify", "--end-of-options", &branch_ref])
                .current_dir(&repository.top_level)
                .output();
            let actual = match commit {
                Ok(output) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                }
                Ok(output) => {
                    rollback_failures.push(
                        command_failure(
                            "inspect rollback branch commit",
                            &repository.top_level,
                            &output,
                        )
                        .to_string(),
                    );
                    String::new()
                }
                Err(error) => {
                    rollback_failures.push(format!(
                        "failed to inspect rollback branch commit {}: {error}",
                        proposal.branch
                    ));
                    String::new()
                }
            };
            if actual == base_commit && !preserve_created_artifacts {
                let delete = git_command()
                    .args(["branch", "-D", "--", &proposal.branch])
                    .current_dir(&repository.top_level)
                    .output();
                match delete {
                    Ok(output) if output.status.success() => {}
                    Ok(output) => rollback_failures.push(
                        command_failure("roll back branch", &repository.top_level, &output)
                            .to_string(),
                    ),
                    Err(error) => rollback_failures.push(format!(
                        "failed to run branch rollback for {}: {error}",
                        proposal.branch
                    )),
                }
            } else {
                rollback_failures.push(format!(
                    "branch {} moved away from the prepared base; preserved it for manual cleanup",
                    proposal.branch
                ));
            }
        }
        Ok(output) if output.status.code() == Some(1) => {}
        Ok(output) => rollback_failures.push(
            command_failure("inspect rollback branch", &repository.top_level, &output).to_string(),
        ),
        Err(error) => rollback_failures.push(format!(
            "failed to inspect rollback branch {}: {error}",
            proposal.branch
        )),
    }

    if rollback_failures.is_empty() {
        primary
    } else {
        TaskWorktreeError::new(
            primary.operation,
            primary.path,
            format!(
                "{}; rollback requires attention: {}",
                primary.detail,
                rollback_failures.join("; ")
            ),
        )
    }
}

fn remove_created_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

fn registered_worktrees(repository_path: &Path) -> Result<Vec<PathBuf>, TaskWorktreeError> {
    let output = run_git(
        repository_path,
        "list registered worktrees",
        &["--no-optional-locks", "worktree", "list", "--porcelain"],
    )?;
    let text = stdout_text(&output, repository_path, "list registered worktrees")?;
    Ok(text
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn branch_upstream(
    repository_path: &Path,
    branch: &str,
) -> Result<Option<String>, TaskWorktreeError> {
    let branch_ref = format!("refs/heads/{branch}");
    let output = run_git(
        repository_path,
        "inspect branch upstream",
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(upstream:short)",
            &branch_ref,
        ],
    )?;
    let upstream = stdout_text(&output, repository_path, "inspect branch upstream")?
        .trim()
        .to_string();
    Ok((!upstream.is_empty()).then_some(upstream))
}

fn rev_count(
    repository_path: &Path,
    range: &str,
    operation: &'static str,
) -> Result<u64, TaskWorktreeError> {
    let output = run_git(
        repository_path,
        operation,
        &["rev-list", "--count", "--end-of-options", range],
    )?;
    let value = stdout_text(&output, repository_path, operation)?;
    value.trim().parse::<u64>().map_err(|error| {
        TaskWorktreeError::new(
            operation,
            repository_path,
            format!("git returned invalid commit count {value:?}: {error}"),
        )
    })
}

fn validate_branch(repository_path: &Path, branch: &str) -> Result<(), TaskWorktreeError> {
    if !branch.starts_with("task/") || branch["task/".len()..].contains('/') {
        return Err(TaskWorktreeError::new(
            "validate branch",
            repository_path,
            format!("task branch {branch:?} must match task/<identifier>-<slug>"),
        ));
    }
    run_git(
        repository_path,
        "validate branch",
        &["check-ref-format", "--branch", branch],
    )?;
    Ok(())
}

fn ensure_safe_worktree_path(
    repository: &ResolvedRepository,
    root: &Path,
    candidate: &Path,
) -> Result<(), TaskWorktreeError> {
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        TaskWorktreeError::new("canonicalize worktree root", root, error.to_string())
    })?;
    let candidate_parent = candidate.parent().ok_or_else(|| {
        TaskWorktreeError::new("validate worktree path", candidate, "path has no parent")
    })?;
    let canonical_parent = fs::canonicalize(candidate_parent).map_err(|error| {
        TaskWorktreeError::new(
            "canonicalize worktree parent",
            candidate_parent,
            error.to_string(),
        )
    })?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(TaskWorktreeError::new(
            "validate worktree path",
            candidate,
            format!(
                "resolved parent {} is outside configured root {}",
                canonical_parent.display(),
                canonical_root.display()
            ),
        ));
    }
    if canonical_parent.starts_with(&repository.top_level)
        || canonical_parent.starts_with(&repository.identity.common_dir)
    {
        return Err(TaskWorktreeError::new(
            "validate worktree path",
            candidate,
            "proposed worktree path resolves inside the source repository",
        ));
    }
    Ok(())
}

fn slug(value: &str, preserve_case: bool, max_len: usize) -> String {
    let mut output = String::new();
    let mut last_was_separator = false;
    for character in value.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if output.len() >= max_len {
                break;
            }
            output.push(if preserve_case {
                character
            } else {
                character.to_ascii_lowercase()
            });
            last_was_separator = false;
        } else if !output.is_empty() && !last_was_separator && output.len() < max_len {
            output.push('-');
            last_was_separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    output
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn run_git(
    repository_path: &Path,
    operation: &'static str,
    arguments: &[&str],
) -> Result<Output, TaskWorktreeError> {
    let output = git_command()
        .args(arguments)
        .current_dir(repository_path)
        .output()
        .map_err(|error| TaskWorktreeError::new(operation, repository_path, error.to_string()))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failure(operation, repository_path, &output))
    }
}

fn command_failure(operation: &'static str, path: &Path, output: &Output) -> TaskWorktreeError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        format!("git exited with status {}", output.status)
    } else {
        format!("git exited with status {}: {stderr}", output.status)
    };
    TaskWorktreeError::new(operation, path, detail)
}

fn stdout_text<'a>(
    output: &'a Output,
    path: &Path,
    operation: &'static str,
) -> Result<&'a str, TaskWorktreeError> {
    std::str::from_utf8(&output.stdout).map_err(|error| {
        TaskWorktreeError::new(
            operation,
            path,
            format!("git output was not UTF-8: {error}"),
        )
    })
}

fn required_path(
    value: Option<&str>,
    source: &Path,
    label: &str,
) -> Result<PathBuf, TaskWorktreeError> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    value.map(PathBuf::from).ok_or_else(|| {
        TaskWorktreeError::new(
            "resolve repository",
            source,
            format!("git did not return {label}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRepository {
        _temp: tempfile::TempDir,
        path: PathBuf,
        worktree_root: PathBuf,
        initial_commit: String,
    }

    impl TestRepository {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("repository with spaces");
            let worktree_root = temp.path().join("managed worktrees with spaces");
            fs::create_dir_all(&path).unwrap();
            git_ok(&path, &["init"]);
            git_ok(&path, &["config", "user.name", "GitTerm Test"]);
            git_ok(&path, &["config", "user.email", "gitterm@example.invalid"]);
            git_ok(&path, &["config", "commit.gpgsign", "false"]);
            git_ok(
                &path,
                &["config", "core.hooksPath", ".git/gitterm-test-no-hooks"],
            );
            fs::write(path.join("README.md"), "initial\n").unwrap();
            git_ok(&path, &["add", "README.md"]);
            git_ok(&path, &["commit", "-m", "Initial commit"]);
            git_ok(&path, &["branch", "-M", "main"]);
            let initial_commit = git_stdout(&path, &["rev-parse", "HEAD"]);
            Self {
                _temp: temp,
                path,
                worktree_root,
                initial_commit,
            }
        }

        fn request(
            &self,
            task_id: &str,
            issue_key: Option<&str>,
            title: &str,
            base_reference: &str,
        ) -> PrepareTaskWorktreeRequest {
            PrepareTaskWorktreeRequest {
                repository_path: self.path.clone(),
                worktree_root: self.worktree_root.clone(),
                task_id: task_id.to_string(),
                title: title.to_string(),
                issue_key: issue_key.map(str::to_string),
                base_reference: base_reference.to_string(),
            }
        }

        fn commit_file(&self, name: &str, contents: &str, message: &str) -> String {
            fs::write(self.path.join(name), contents).unwrap();
            git_ok(&self.path, &["add", name]);
            git_ok(&self.path, &["commit", "-m", message]);
            git_stdout(&self.path, &["rev-parse", "HEAD"])
        }
    }

    fn git_output(path: &Path, arguments: &[&str]) -> Output {
        git_command()
            .args(arguments)
            .current_dir(path)
            .output()
            .unwrap()
    }

    fn git_ok(path: &Path, arguments: &[&str]) {
        let output = git_output(path, arguments);
        assert!(
            output.status.success(),
            "git {arguments:?} failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(path: &Path, arguments: &[&str]) -> String {
        let output = git_output(path, arguments);
        assert!(
            output.status.success(),
            "git {arguments:?} failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn resolves_repository_identity_and_optional_origin() {
        let repository = TestRepository::new();
        git_ok(
            &repository.path,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repository.git",
            ],
        );

        let resolved = resolve_repository(&repository.path).unwrap();

        assert_eq!(
            resolved.top_level,
            fs::canonicalize(&repository.path).unwrap()
        );
        assert_eq!(resolved.current_branch.as_deref(), Some("main"));
        assert_eq!(
            resolved.identity.remote_url.as_deref(),
            Some("https://example.invalid/repository.git")
        );
        assert_eq!(
            resolved.identity.common_dir,
            fs::canonicalize(repository.path.join(".git")).unwrap()
        );
    }

    #[test]
    fn creates_task_branch_from_named_base_without_touching_main_checkout() {
        let repository = TestRepository::new();
        let request =
            repository.request("task-1", Some("TRU-97"), "Provision safe worktrees", "main");
        let main_head_before = git_stdout(&repository.path, &["rev-parse", "HEAD"]);

        let prepared = prepare_task_worktree_blocking(&request).unwrap();

        assert_eq!(prepared.branch, "task/TRU-97-provision-safe-worktrees");
        assert!(prepared
            .path
            .to_string_lossy()
            .contains("managed worktrees with spaces"));
        assert_eq!(
            git_stdout(&prepared.path, &["rev-parse", "HEAD"]),
            main_head_before
        );
        assert_eq!(
            git_stdout(&prepared.path, &["branch", "--show-current"]),
            prepared.branch
        );
        assert_eq!(
            git_stdout(&repository.path, &["rev-parse", "HEAD"]),
            main_head_before
        );
        assert!(git_stdout(&repository.path, &["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn default_base_prefers_develop_over_main_and_the_current_branch() {
        let repository = TestRepository::new();
        git_ok(
            &repository.path,
            &["branch", "develop", &repository.initial_commit],
        );
        let main_head = repository.commit_file("main-only.txt", "main\n", "Advance main");
        assert_ne!(main_head, repository.initial_commit);
        let request =
            repository.request("default-develop", None, "Prefer develop", DEFAULT_TASK_BASE);

        let resolved = resolve_task_preparation_blocking(&request).unwrap();

        assert_eq!(resolved.base.reference, "develop");
        assert_eq!(resolved.base.commit, repository.initial_commit);
    }

    #[test]
    fn default_base_uses_main_when_develop_does_not_exist() {
        let repository = TestRepository::new();
        let request =
            repository.request("default-main", None, "Fall back to main", DEFAULT_TASK_BASE);

        let resolved = resolve_task_preparation_blocking(&request).unwrap();

        assert_eq!(resolved.base.reference, "main");
        assert_eq!(resolved.base.commit, repository.initial_commit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_preparation_runs_through_the_blocking_worker_boundary() {
        let repository = TestRepository::new();
        let request = repository.request("async-task", None, "Async preparation", "main");

        let prepared = prepare_task_worktree(request).await.unwrap();

        assert!(prepared.path.is_dir());
        assert_eq!(prepared.branch, "task/async-task-async-preparation");
    }

    #[test]
    fn creates_from_an_exact_detached_commit() {
        let repository = TestRepository::new();
        let current_main = repository.commit_file("second.txt", "second\n", "Second commit");
        assert_ne!(repository.initial_commit, current_main);
        let request = repository.request(
            "task-detached",
            None,
            "Use exact detached base",
            &repository.initial_commit,
        );

        let prepared = prepare_task_worktree_blocking(&request).unwrap();

        assert_eq!(prepared.base.commit, repository.initial_commit);
        assert_eq!(
            git_stdout(&prepared.path, &["rev-parse", "HEAD"]),
            repository.initial_commit
        );
        assert_eq!(
            git_stdout(&repository.path, &["rev-parse", "main"]),
            current_main
        );
    }

    #[test]
    fn prepares_distinct_tasks_from_one_repository_concurrently() {
        let repository = TestRepository::new();
        let first = repository.request("task-one", None, "First task", "main");
        let second = repository.request("task-two", None, "Second task", "main");

        let first_thread = std::thread::spawn(move || prepare_task_worktree_blocking(&first));
        let second_thread = std::thread::spawn(move || prepare_task_worktree_blocking(&second));
        let first = first_thread.join().unwrap().unwrap();
        let second = second_thread.join().unwrap().unwrap();

        assert_ne!(first.branch, second.branch);
        assert_ne!(first.path, second.path);
        assert!(first.path.is_dir());
        assert!(second.path.is_dir());
    }

    #[test]
    fn rejects_existing_branch_and_path_collisions() {
        let repository = TestRepository::new();
        let request = repository.request("task-1", Some("TRU-97"), "Collision", "main");
        let first = prepare_task_worktree_blocking(&request).unwrap();

        let error = prepare_task_worktree_blocking(&request).unwrap_err();

        assert_eq!(error.operation(), "check collisions");
        assert!(error.to_string().contains("already exists"));
        assert!(paths_equal(error.path(), &first.path));
    }

    #[test]
    fn rejects_an_existing_branch_when_the_proposed_path_is_free() {
        let repository = TestRepository::new();
        let request = repository.request("branch-only", None, "Branch collision", "main");
        let resolved = resolve_repository(&repository.path).unwrap();
        let proposal = propose_task_worktree(
            &resolved,
            &request.worktree_root,
            &request.task_id,
            None,
            &request.title,
        )
        .unwrap();
        git_ok(
            &repository.path,
            &["branch", &proposal.branch, &repository.initial_commit],
        );
        assert!(!proposal.path.exists());

        let error = prepare_task_worktree_blocking(&request).unwrap_err();

        assert_eq!(error.operation(), "check collisions");
        assert!(error.to_string().contains("branch"));
        assert!(error.to_string().contains("already exists"));
        assert!(!proposal.path.exists());
    }

    #[test]
    fn failed_post_add_verification_rolls_back_only_created_artifacts() {
        let repository = TestRepository::new();
        let request = repository.request("task-rollback", None, "Rollback fixture", "main");
        let resolved = resolve_repository(&repository.path).unwrap();
        let proposal = propose_task_worktree(
            &resolved,
            &request.worktree_root,
            &request.task_id,
            None,
            &request.title,
        )
        .unwrap();

        let error = prepare_task_worktree_with(&request, || {
            Err("injected verification failure".to_string())
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected verification failure"));
        assert!(!proposal.path.exists());
        assert!(!registered_worktrees(&repository.path)
            .unwrap()
            .iter()
            .any(|path| paths_equal(path, &proposal.path)));
        let branch_ref = format!("refs/heads/{}", proposal.branch);
        assert!(!git_output(
            &repository.path,
            &["show-ref", "--verify", "--quiet", &branch_ref]
        )
        .status
        .success());
        assert_eq!(
            git_stdout(&repository.path, &["rev-parse", "HEAD"]),
            repository.initial_commit
        );
    }

    #[test]
    fn cleanup_inspection_refuses_dirty_unpushed_active_or_published_work() {
        let repository = TestRepository::new();
        let request = repository.request("task-cleanup", None, "Inspect cleanup", "main");
        let prepared = prepare_task_worktree_blocking(&request).unwrap();
        let quiet_context = CleanupContext {
            process_running: false,
            pull_request_url: None,
        };

        let clean = inspect_cleanup(
            &repository.path,
            &prepared.path,
            &prepared.branch,
            &prepared.base.commit,
            &quiet_context,
        )
        .unwrap();
        assert!(clean.safe_to_remove());

        fs::write(prepared.path.join("uncommitted.txt"), "change\n").unwrap();
        let dirty = inspect_cleanup(
            &repository.path,
            &prepared.path,
            &prepared.branch,
            &prepared.base.commit,
            &quiet_context,
        )
        .unwrap();
        assert!(dirty.risks.contains(&CleanupRisk::UncommittedChanges));

        git_ok(&prepared.path, &["add", "uncommitted.txt"]);
        git_ok(&prepared.path, &["commit", "-m", "Task change"]);
        let guarded_context = CleanupContext {
            process_running: true,
            pull_request_url: Some("https://example.invalid/pull/1".to_string()),
        };
        let guarded = inspect_cleanup(
            &repository.path,
            &prepared.path,
            &prepared.branch,
            &prepared.base.commit,
            &guarded_context,
        )
        .unwrap();
        assert!(!guarded.dirty);
        assert_eq!(guarded.unpushed_commits, 1);
        assert!(guarded.risks.contains(&CleanupRisk::ActiveProcess));
        assert!(guarded
            .risks
            .contains(&CleanupRisk::UnpushedCommits { count: 1 }));
        assert!(guarded.risks.contains(&CleanupRisk::ExistingPullRequest {
            url: "https://example.invalid/pull/1".to_string()
        }));
        assert!(!guarded.safe_to_remove());
    }

    #[test]
    fn rejects_non_repository_missing_base_and_relative_root() {
        let temp = tempfile::tempdir().unwrap();
        let error = resolve_repository(temp.path()).unwrap_err();
        assert_eq!(error.operation(), "resolve repository");

        let repository = TestRepository::new();
        let resolved = resolve_repository(&repository.path).unwrap();
        let error = resolve_base(&resolved, "refs/heads/does-not-exist").unwrap_err();
        assert_eq!(error.operation(), "resolve base");

        let error = propose_task_worktree(
            &resolved,
            Path::new("relative/worktrees"),
            "task-1",
            None,
            "Relative root",
        )
        .unwrap_err();
        assert!(error.to_string().contains("absolute path"));
    }

    #[test]
    fn rejects_a_managed_root_inside_the_source_checkout() {
        let repository = TestRepository::new();
        let mut request = repository.request("nested-root", None, "Nested root", "main");
        request.worktree_root = repository.path.join("managed worktrees");

        let error = prepare_task_worktree_blocking(&request).unwrap_err();

        assert!(error.operation().starts_with("validate worktree"));
        assert!(error.to_string().contains("inside the source"));
        assert_eq!(
            git_stdout(&repository.path, &["rev-parse", "HEAD"]),
            repository.initial_commit
        );
        assert!(git_stdout(&repository.path, &["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn parse_pull_request_rows_reads_gh_fields() {
        let json = r#"[{"url":"https://github.com/o/r/pull/30","number":30,
            "headRefOid":"bafc5f5757b0265cb89a50a64fd539089b4265ab","isDraft":true}]"#;

        let parsed = parse_pull_request_rows(json).unwrap().unwrap();

        assert_eq!(parsed.url, "https://github.com/o/r/pull/30");
        assert_eq!(parsed.number, 30);
        assert_eq!(parsed.head_sha, "bafc5f5757b0265cb89a50a64fd539089b4265ab");
        assert!(parsed.is_draft);
    }

    #[test]
    fn parse_pull_request_rows_empty_means_no_open_pull_request() {
        assert_eq!(parse_pull_request_rows("[]").unwrap(), None);
    }

    #[test]
    fn parse_pull_request_rows_rejects_malformed_output() {
        let error = parse_pull_request_rows("not json").unwrap_err();
        assert!(error.contains("unexpected gh output"));
    }

    #[test]
    fn pr_base_branch_strips_remote_and_ref_prefixes() {
        assert_eq!(pr_base_branch("v5"), "v5");
        assert_eq!(pr_base_branch("origin/v5"), "v5");
        assert_eq!(pr_base_branch("refs/heads/v5"), "v5");
        assert_eq!(pr_base_branch("refs/remotes/origin/v5"), "v5");
    }
}
