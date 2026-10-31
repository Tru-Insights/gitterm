//! Workspace overview (TRU-167, step 2 of `.plans/work-first-ux/design.md`).
//!
//! Reads a workspace as one piece of work across repos: its coordinator
//! chats with their fan-out, the repos the work edited, the branches and
//! worktrees it owns with their live git state, and the issue keys it
//! carries. Read-only: nothing here writes to git or to `workspaces.json`.
//!
//! Which chats: the index entries in the workspace's scope
//! (`ChatIndexEntry::in_workspace`) plus the descendants of in-scope roots
//! (a worker in a task worktree folds under the coordinator that started
//! it). Which branches (design decision 13, minus manual Add/Ignore):
//!
//! - (a) facts checkouts with edits, or any linked worktree a chat worked
//!   in; a main checkout only visited by `cd` never counts;
//! - (b) task worktrees the chats created (facts children with a task id,
//!   which `chat_facts::resolve_tree` fills from delegations and tasks);
//! - (c) worktrees in the work's repos whose branch names one of its issue
//!   keys.
//!
//! Nothing else in a shared repo appears. Polling (decisions 16 and 18):
//! only owned checkouts, one `git status --porcelain=v2 --branch` and one
//! `git rev-list --left-right --count base...HEAD` each, in a background
//! task with timeouts; a gated refresh first compares a stat of the
//! checkout's gitdir `index`, `HEAD` and `logs/HEAD` and runs no git when
//! nothing moved. Status output is counted, never stored, and capped.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::chat_facts::{self, ChildKind};
use crate::chats::{self, ChatIndexEntry};
use crate::tasks::TaskRecord;
use crate::worktree_follow::{self, WorktreeInfo};

/// One git call gives up after this long; the process is killed.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// A whole refresh stops starting new git calls after this long.
pub const REFRESH_DEADLINE: Duration = Duration::from_secs(45);
/// Status entries counted per checkout before counting stops ("5000+").
pub const STATUS_COUNT_CAP: usize = 5000;
/// Diff lines read for one branch diff before reading stops.
pub const DIFF_LINE_CAP: usize = 20_000;
/// Untracked file names listed in an Uncommitted diff.
const UNTRACKED_LIST_CAP: usize = 200;
/// Local branch names tried, in order, for the repo default base.
const DEFAULT_BRANCHES: [&str; 4] = ["main", "master", "develop", "trunk"];

// ---------------------------------------------------------------------------
// Chats, repos and issue keys.

/// The chats of the workspace at `ws_dir`, in `entries` order (the index
/// keeps newest first): entries in its scope, plus the descendants of
/// in-scope roots.
pub fn work_chats<'a>(entries: &'a [ChatIndexEntry], ws_dir: &Path) -> Vec<&'a ChatIndexEntry> {
    let in_scope: HashSet<&str> = entries
        .iter()
        .filter(|entry| entry.in_workspace(ws_dir))
        .map(|entry| entry.id.as_str())
        .collect();
    entries
        .iter()
        .filter(|entry| {
            in_scope.contains(entry.id.as_str())
                || entry
                    .root_id
                    .as_deref()
                    .is_some_and(|root| in_scope.contains(root))
        })
        .collect()
}

/// A root chat of the work and what folds under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Coordinator {
    pub id: String,
    pub title: String,
    pub latest: Option<String>,
    pub mtime: SystemTime,
    /// "35 subagents · 2 workers"; `None` without children.
    pub fan_out: Option<String>,
    /// Children with an index entry of their own (workers, reviews), newest
    /// first. Subagents usually have none and show only in `fan_out`.
    pub children: Vec<CoordinatorChild>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoordinatorChild {
    pub id: String,
    pub title: String,
    pub kind: Option<ChildKind>,
    pub mtime: SystemTime,
}

/// A repo the work edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTouched {
    pub dir: PathBuf,
    pub edits: u64,
}

impl RepoTouched {
    pub fn name(&self) -> String {
        folder_name(&self.dir)
    }
}

pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Coordinators of `work` (roots, children folded under them as the Chats
/// panel folds them), newest first.
pub fn coordinators(work: &[&ChatIndexEntry], all: &[ChatIndexEntry]) -> Vec<Coordinator> {
    chats::fold_under_roots(work, all)
        .into_iter()
        .map(|folded| {
            let root = folded.root;
            let kind_of = |id: &str| {
                root.facts.as_ref().and_then(|facts| {
                    facts
                        .children
                        .iter()
                        .find(|child| child.session_id == id)
                        .map(|child| child.kind)
                })
            };
            let children = folded
                .children
                .iter()
                .map(|child| CoordinatorChild {
                    id: child.id.clone(),
                    title: child.title.clone(),
                    kind: kind_of(&child.id),
                    mtime: child.mtime,
                })
                .collect();
            Coordinator {
                id: root.id.clone(),
                title: root.title.clone(),
                latest: root.latest.clone(),
                mtime: root.mtime,
                fan_out: root
                    .facts
                    .as_ref()
                    .and_then(chat_facts::ChatFacts::fan_out_line),
                children,
            }
        })
        .collect()
}

/// The union of the chats' repos with edits, most edited first.
pub fn repos_touched(work: &[&ChatIndexEntry]) -> Vec<RepoTouched> {
    let mut by_dir: HashMap<&Path, u64> = HashMap::new();
    for facts in work.iter().filter_map(|entry| entry.facts.as_ref()) {
        for repo in facts.repos_touched.iter().filter(|repo| repo.edits > 0) {
            *by_dir.entry(repo.dir.as_path()).or_default() += repo.edits;
        }
    }
    let mut repos: Vec<RepoTouched> = by_dir
        .into_iter()
        .map(|(dir, edits)| RepoTouched {
            dir: dir.to_path_buf(),
            edits,
        })
        .collect();
    repos.sort_by(|a, b| b.edits.cmp(&a.edits).then_with(|| a.dir.cmp(&b.dir)));
    repos
}

/// The work's issue keys, most mentioned first, with obvious false
/// positives dropped: a key needs 2+ digits, or a prefix that also names
/// one of `branches` or that occurs more than once across `per_chat`
/// (`GPT-5` or `SHA-1` mentioned once is prose, not an issue).
pub fn work_issue_keys(per_chat: &[&[String]], branches: &[&str]) -> Vec<String> {
    let branch_prefixes: HashSet<String> = branches
        .iter()
        .flat_map(|branch| chat_facts::issue_keys_in_name(branch))
        .filter_map(|key| key_prefix(&key).map(str::to_string))
        .collect();
    let mut key_count: HashMap<&str, usize> = HashMap::new();
    let mut prefix_count: HashMap<&str, usize> = HashMap::new();
    for keys in per_chat {
        for key in keys.iter() {
            *key_count.entry(key.as_str()).or_default() += 1;
            if let Some(prefix) = key_prefix(key) {
                *prefix_count.entry(prefix).or_default() += 1;
            }
        }
    }
    let mut kept: Vec<(&str, usize)> = key_count
        .into_iter()
        .filter(|(key, _)| {
            let Some((prefix, digits)) = key.split_once('-') else {
                return false;
            };
            digits.len() >= 2
                || branch_prefixes.contains(prefix)
                || prefix_count.get(prefix).copied().unwrap_or(0) > 1
        })
        .collect();
    kept.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    kept.into_iter().map(|(key, _)| key.to_string()).collect()
}

fn key_prefix(key: &str) -> Option<&str> {
    key.split_once('-').map(|(prefix, _)| prefix)
}

// ---------------------------------------------------------------------------
// Branch ownership.

/// Why a checkout belongs to the work. Ordered weakest first; a checkout
/// claimed several ways keeps the strongest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ownership {
    /// (c) Its branch names one of the work's issue keys.
    IssueKey(String),
    /// (a) A linked worktree a chat ran commands in.
    WorkedIn,
    /// (a) A checkout a chat edited.
    Edited,
    /// (b) A task worktree a chat created.
    Task,
}

/// The task fields the overview reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    pub task_id: String,
    pub title: String,
    pub issue_key: Option<String>,
    pub branch: String,
    pub base: String,
    pub worktree: Option<PathBuf>,
    pub pr_number: Option<u64>,
}

impl From<&TaskRecord> for TaskInfo {
    fn from(task: &TaskRecord) -> Self {
        Self {
            task_id: task.task_id.clone(),
            title: task.title.clone(),
            issue_key: task.issue.as_ref().map(|issue| issue.key.clone()),
            branch: task.branch.clone(),
            base: task.base.reference.clone(),
            worktree: task.worktree.path.clone(),
            pr_number: task.delivery.pr_number,
        }
    }
}

impl TaskInfo {
    /// "PRJ-101", else the title cut short.
    fn short_label(&self) -> String {
        self.issue_key
            .clone()
            .unwrap_or_else(|| shorten(&self.title, 24))
    }
}

/// A checkout claimed by (a) or (b), before git confirms it still exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCandidate {
    /// Main checkout of the repo; `None` when only git can tell (a task
    /// worktree, whose record holds the common dir).
    pub repo: Option<PathBuf>,
    pub checkout: PathBuf,
    pub ownership: Ownership,
    pub owner: String,
    /// RFC 3339 time of the latest touch the facts recorded.
    pub last: Option<String>,
    pub edits: u64,
    pub task: Option<TaskLink>,
}

/// The task a checkout belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLink {
    pub task_id: String,
    pub base: String,
    pub pr_number: Option<u64>,
}

/// How a chat relates to the work, for owner copy.
fn chat_owner(
    entry: &ChatIndexEntry,
    kinds: &HashMap<&str, ChildKind>,
    coordinators: usize,
) -> String {
    if entry.root_id.is_none() {
        return if coordinators > 1 {
            format!("coordinator \u{201C}{}\u{201D}", shorten(&entry.title, 20))
        } else {
            "coordinator".to_string()
        };
    }
    match kinds.get(entry.id.as_str()) {
        Some(ChildKind::Worker) => "worker",
        Some(ChildKind::Review) => "review",
        Some(ChildKind::Consult) => "consult",
        Some(ChildKind::Subagent) => "subagent",
        None => "child chat",
    }
    .to_string()
}

/// Branch candidates (a) and (b) from the work's chats and tasks, merged
/// per checkout.
pub fn branch_candidates(work: &[&ChatIndexEntry], tasks: &[TaskInfo]) -> Vec<BranchCandidate> {
    let roots = work.iter().filter(|entry| entry.root_id.is_none()).count();
    let mut kinds: HashMap<&str, ChildKind> = HashMap::new();
    let mut task_ids: Vec<&str> = Vec::new();
    for facts in work.iter().filter_map(|entry| entry.facts.as_ref()) {
        for child in &facts.children {
            kinds.entry(child.session_id.as_str()).or_insert(child.kind);
            if let Some(task_id) = child.task_id.as_deref() {
                if !task_ids.contains(&task_id) {
                    task_ids.push(task_id);
                }
            }
        }
    }
    let mut out: Vec<BranchCandidate> = Vec::new();
    for entry in work {
        let Some(facts) = &entry.facts else {
            continue;
        };
        for checkout in &facts.checkouts {
            let linked = checkout.dir != checkout.repo;
            let ownership = if checkout.edits > 0 {
                Ownership::Edited
            } else if linked {
                Ownership::WorkedIn
            } else {
                // A main checkout visited by `cd` names nothing.
                continue;
            };
            add_candidate(
                &mut out,
                BranchCandidate {
                    repo: Some(checkout.repo.clone()),
                    checkout: checkout.dir.clone(),
                    ownership,
                    owner: chat_owner(entry, &kinds, roots),
                    last: Some(checkout.last.clone()),
                    edits: checkout.edits,
                    task: None,
                },
            );
        }
    }
    for task_id in task_ids {
        let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
            continue;
        };
        let Some(worktree) = &task.worktree else {
            continue;
        };
        add_candidate(
            &mut out,
            BranchCandidate {
                repo: None,
                checkout: worktree.clone(),
                ownership: Ownership::Task,
                owner: format!("worker {}", task.short_label()),
                last: None,
                edits: 0,
                task: Some(TaskLink {
                    task_id: task.task_id.clone(),
                    base: task.base.clone(),
                    pr_number: task.pr_number,
                }),
            },
        );
    }
    out
}

/// Adds `candidate`, or merges it into the one already claiming its
/// checkout: the strongest ownership and its owner win, edits add up, the
/// latest touch and any repo or task are kept.
fn add_candidate(out: &mut Vec<BranchCandidate>, candidate: BranchCandidate) {
    let Some(known) = out
        .iter_mut()
        .find(|known| known.checkout == candidate.checkout)
    else {
        out.push(candidate);
        return;
    };
    if candidate.ownership > known.ownership {
        known.ownership = candidate.ownership.clone();
        known.owner = candidate.owner.clone();
    }
    known.edits += candidate.edits;
    if let Some(last) = candidate.last {
        if known.last.as_deref().is_none_or(|old| later(&last, old)) {
            known.last = Some(last);
        }
    }
    if known.repo.is_none() {
        known.repo = candidate.repo;
    }
    if known.task.is_none() {
        known.task = candidate.task;
    }
}

fn later(a: &str, b: &str) -> bool {
    match (parse_stamp(a), parse_stamp(b)) {
        (Some(a), Some(b)) => a > b,
        _ => a > b,
    }
}

pub fn parse_stamp(stamp: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(SystemTime::from)
}

/// A checkout the work owns, confirmed against its repo's worktree list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedBranch {
    pub repo: PathBuf,
    pub checkout: PathBuf,
    /// Live branch from `git worktree list`; `None` when detached.
    pub branch: Option<String>,
    pub is_main: bool,
    pub ownership: Ownership,
    pub owner: String,
    pub last: Option<String>,
    pub edits: u64,
    pub task: Option<TaskLink>,
}

/// Confirms candidates against `worktrees` (main checkout -> its list,
/// paths canonical) and adds rule (c): worktrees of `key_repos` whose
/// branch names one of `issue_keys`. A candidate whose checkout is no
/// longer a worktree is dropped. Rows come out grouped by `key_repos`
/// order (then other repos by path), main checkout first, then by branch.
pub fn resolve_owned(
    candidates: &[BranchCandidate],
    issue_keys: &[String],
    key_repos: &[PathBuf],
    worktrees: &HashMap<PathBuf, Vec<WorktreeInfo>>,
) -> Vec<OwnedBranch> {
    let mut out: Vec<OwnedBranch> = Vec::new();
    for candidate in candidates {
        let found = match &candidate.repo {
            Some(repo) => worktrees
                .get(repo)
                .and_then(|list| locate(list, &candidate.checkout).map(|i| (repo, list, i))),
            None => worktrees.iter().find_map(|(repo, list)| {
                locate(list, &candidate.checkout).map(|i| (repo, list, i))
            }),
        };
        let Some((repo, list, index)) = found else {
            continue;
        };
        out.push(OwnedBranch {
            repo: repo.clone(),
            checkout: list[index].path.clone(),
            branch: list[index].branch.clone(),
            is_main: index == 0,
            ownership: candidate.ownership.clone(),
            owner: candidate.owner.clone(),
            last: candidate.last.clone(),
            edits: candidate.edits,
            task: candidate.task.clone(),
        });
    }
    let wanted: HashSet<&str> = issue_keys.iter().map(String::as_str).collect();
    for repo in key_repos {
        let Some(list) = worktrees.get(repo) else {
            continue;
        };
        for (index, worktree) in list.iter().enumerate() {
            if out.iter().any(|owned| owned.checkout == worktree.path) {
                continue;
            }
            let Some(branch) = &worktree.branch else {
                continue;
            };
            let Some(key) = chat_facts::issue_keys_in_name(branch)
                .into_iter()
                .find(|key| wanted.contains(key.as_str()))
            else {
                continue;
            };
            out.push(OwnedBranch {
                repo: repo.clone(),
                checkout: worktree.path.clone(),
                branch: Some(branch.clone()),
                is_main: index == 0,
                ownership: Ownership::IssueKey(key.clone()),
                owner: format!("matches {key}"),
                last: None,
                edits: 0,
                task: None,
            });
        }
    }
    let repo_rank = |repo: &Path| {
        key_repos
            .iter()
            .position(|known| known == repo)
            .unwrap_or(usize::MAX)
    };
    out.sort_by(|a, b| {
        repo_rank(&a.repo)
            .cmp(&repo_rank(&b.repo))
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| b.is_main.cmp(&a.is_main))
            .then_with(|| a.branch.cmp(&b.branch))
            .then_with(|| a.checkout.cmp(&b.checkout))
    });
    out
}

fn locate(list: &[WorktreeInfo], checkout: &Path) -> Option<usize> {
    list.iter().position(|worktree| worktree.path == checkout)
}

// ---------------------------------------------------------------------------
// The overview model.

/// Everything the panel shows that comes from the index and tasks, built
/// on the UI thread when the index changes or the panel opens (a pass over
/// the index, no I/O).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkSummary {
    pub coordinators: Vec<Coordinator>,
    pub repos: Vec<RepoTouched>,
    pub issue_keys: Vec<String>,
    pub candidates: Vec<BranchCandidate>,
    /// Repos whose worktrees rule (c) scans: repos touched, then repos of
    /// candidates that name one.
    pub key_repos: Vec<PathBuf>,
    pub chat_count: usize,
    /// Any of the work's chats has a facts file.
    pub has_facts: bool,
}

impl WorkSummary {
    pub fn build(entries: &[ChatIndexEntry], ws_dir: &Path, tasks: &[TaskInfo]) -> Self {
        let work = work_chats(entries, ws_dir);
        let repos = repos_touched(&work);
        let candidates = branch_candidates(&work, tasks);
        let mut branches: Vec<&str> = work
            .iter()
            .filter_map(|entry| entry.facts.as_ref())
            .flat_map(|facts| facts.checkouts.iter())
            .filter_map(|checkout| checkout.branch.as_deref())
            .collect();
        let work_tasks: HashSet<&str> = candidates
            .iter()
            .filter_map(|candidate| candidate.task.as_ref())
            .map(|task| task.task_id.as_str())
            .collect();
        branches.extend(
            tasks
                .iter()
                .filter(|task| work_tasks.contains(task.task_id.as_str()))
                .map(|task| task.branch.as_str()),
        );
        let per_chat: Vec<&[String]> = work
            .iter()
            .filter_map(|entry| entry.facts.as_ref())
            .map(|facts| facts.issue_keys.as_slice())
            .collect();
        let issue_keys = work_issue_keys(&per_chat, &branches);
        let mut key_repos: Vec<PathBuf> = repos.iter().map(|repo| repo.dir.clone()).collect();
        for repo in candidates.iter().filter_map(|c| c.repo.as_ref()) {
            if !key_repos.contains(repo) {
                key_repos.push(repo.clone());
            }
        }
        Self {
            coordinators: coordinators(&work, entries),
            repos,
            issue_keys,
            candidates,
            key_repos,
            chat_count: work.len(),
            has_facts: work.iter().any(|entry| entry.facts.is_some()),
        }
    }
}

// ---------------------------------------------------------------------------
// Base selection.

/// Where a row's base came from (decision 15); shown on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    /// The task's recorded base branch.
    Task,
    /// The remote's default branch (`origin/HEAD`, else `origin/main` or
    /// `origin/master`); the diff is against its merge-base.
    Upstream,
    /// A local default branch (`main`, `master`, `develop`, `trunk`).
    RepoDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseChoice {
    pub reference: String,
    pub source: BaseSource,
}

impl BaseChoice {
    /// "task base", "upstream default", "repo default".
    pub fn source_label(&self) -> &'static str {
        match self.source {
            BaseSource::Task => "task base",
            BaseSource::Upstream => "upstream default",
            BaseSource::RepoDefault => "repo default",
        }
    }
}

/// The candidate base refs one repo has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoRefs {
    /// Short name of the remote default, e.g. `origin/main`.
    pub upstream: Option<String>,
    /// Which of `DEFAULT_BRANCHES` exist locally, in that order.
    pub local: Vec<String>,
}

/// The `for-each-ref` arguments [`parse_repo_refs`] reads.
pub fn repo_refs_args() -> Vec<String> {
    let mut args: Vec<String> = [
        "--no-optional-locks",
        "for-each-ref",
        "--format=%(refname)%09%(symref)",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    args.extend(
        DEFAULT_BRANCHES
            .iter()
            .map(|name| format!("refs/heads/{name}")),
    );
    args
}

/// Parses `git for-each-ref --format=%(refname)%09%(symref)` over the refs
/// of [`repo_refs_args`]. `for-each-ref` also matches refs below a pattern
/// (`refs/heads/main/x`), so only exact names count.
pub fn parse_repo_refs(output: &str) -> RepoRefs {
    let mut origin_head: Option<String> = None;
    let mut origin_named: Vec<&str> = Vec::new();
    let mut local: HashSet<&str> = HashSet::new();
    for line in output.lines() {
        let (name, symref) = line.split_once('\t').unwrap_or((line, ""));
        if name == "refs/remotes/origin/HEAD" {
            origin_head = symref
                .strip_prefix("refs/remotes/")
                .filter(|target| !target.is_empty())
                .map(str::to_string);
        } else if let Some(short) = name.strip_prefix("refs/remotes/") {
            if short == "origin/main" || short == "origin/master" {
                origin_named.push(short);
            }
        } else if let Some(short) = name.strip_prefix("refs/heads/") {
            if DEFAULT_BRANCHES.contains(&short) {
                local.insert(short);
            }
        }
    }
    let upstream = origin_head.or_else(|| {
        ["origin/main", "origin/master"]
            .into_iter()
            .find(|name| origin_named.contains(name))
            .map(str::to_string)
    });
    RepoRefs {
        upstream,
        local: DEFAULT_BRANCHES
            .iter()
            .filter(|name| local.contains(*name))
            .map(|name| name.to_string())
            .collect(),
    }
}

/// The task's base, else the upstream default, else the repo default.
pub fn choose_base(task_base: Option<&str>, refs: &RepoRefs) -> Option<BaseChoice> {
    if let Some(base) = task_base.filter(|base| !base.trim().is_empty()) {
        return Some(BaseChoice {
            reference: base.trim().to_string(),
            source: BaseSource::Task,
        });
    }
    if let Some(upstream) = &refs.upstream {
        return Some(BaseChoice {
            reference: upstream.clone(),
            source: BaseSource::Upstream,
        });
    }
    refs.local.first().map(|local| BaseChoice {
        reference: local.clone(),
        source: BaseSource::RepoDefault,
    })
}

// ---------------------------------------------------------------------------
// Status and rev-list parsing.

/// Counts from `git status --porcelain=v2 --branch`. File paths are never
/// kept, only counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusCounts {
    /// Checked-out branch; `None` when detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    /// `(ahead, behind)` against the upstream; `None` without one or when
    /// it is gone.
    pub upstream_ab: Option<(u32, u32)>,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
    /// Changed paths (each entry line once).
    pub files: usize,
    /// Counting stopped at [`STATUS_COUNT_CAP`].
    pub capped: bool,
}

/// Whether a branch's commits are on its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushState {
    NoUpstream,
    /// Configured upstream no longer exists on the remote.
    UpstreamGone,
    Pushed,
    Unpushed(u32),
}

impl StatusCounts {
    /// Feeds one output line; false once the cap is reached and reading
    /// should stop. Headers come first in porcelain v2, so stopping early
    /// never loses them.
    pub fn observe(&mut self, line: &str, cap: usize) -> bool {
        if let Some(header) = line.strip_prefix("# ") {
            if let Some(head) = header.strip_prefix("branch.head ") {
                let head = head.trim();
                self.head = (head != "(detached)" && !head.is_empty()).then(|| head.to_string());
            } else if let Some(upstream) = header.strip_prefix("branch.upstream ") {
                self.upstream = Some(upstream.trim().to_string());
            } else if let Some(ab) = header.strip_prefix("branch.ab ") {
                let mut parts = ab.split_whitespace();
                let ahead = parts
                    .next()
                    .and_then(|part| part.strip_prefix('+'))
                    .and_then(|n| n.parse().ok());
                let behind = parts
                    .next()
                    .and_then(|part| part.strip_prefix('-'))
                    .and_then(|n| n.parse().ok());
                if let (Some(ahead), Some(behind)) = (ahead, behind) {
                    self.upstream_ab = Some((ahead, behind));
                }
            }
            return true;
        }
        let bytes = line.as_bytes();
        match bytes.first() {
            Some(b'1' | b'2') if bytes.get(1) == Some(&b' ') && bytes.len() >= 4 => {
                if bytes[2] != b'.' {
                    self.staged += 1;
                }
                if bytes[3] != b'.' {
                    self.unstaged += 1;
                }
            }
            Some(b'u') if bytes.get(1) == Some(&b' ') => self.conflicted += 1,
            Some(b'?') if bytes.get(1) == Some(&b' ') => self.untracked += 1,
            _ => return true,
        }
        self.files += 1;
        if self.files >= cap {
            self.capped = true;
            return false;
        }
        true
    }

    pub fn push_state(&self) -> PushState {
        match (&self.upstream, self.upstream_ab) {
            (None, _) => PushState::NoUpstream,
            (Some(_), None) => PushState::UpstreamGone,
            (Some(_), Some((0, _))) => PushState::Pushed,
            (Some(_), Some((ahead, _))) => PushState::Unpushed(ahead),
        }
    }

    /// "clean", "12 dirty", "5000+ dirty".
    pub fn dirty_label(&self) -> String {
        match (self.files, self.capped) {
            (0, _) => "clean".to_string(),
            (n, true) => format!("{n}+ dirty"),
            (n, false) => format!("{n} dirty"),
        }
    }
}

/// Parses a whole porcelain v2 output (tests and small inputs).
pub fn parse_status_v2(output: &str, cap: usize) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for line in output.lines() {
        if !counts.observe(line, cap) {
            break;
        }
    }
    counts
}

/// Parses `git rev-list --left-right --count base...HEAD`: `(behind,
/// ahead)`, the commits only on the base and only on HEAD.
pub fn parse_left_right(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((behind, ahead))
}

// ---------------------------------------------------------------------------
// Gitdir stat gate.

/// The checkout's own gitdir: `.git` itself, or for a linked worktree the
/// directory its `.git` file points at.
pub fn gitdir_of(checkout: &Path) -> Option<PathBuf> {
    let dot_git = checkout.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?;
    let target = PathBuf::from(target.trim());
    Some(if target.is_absolute() {
        target
    } else {
        checkout.join(target)
    })
}

/// mtime and length of the gitdir's `index`, `HEAD` and `logs/HEAD`.
/// Equal stamps mean no commit, checkout, stage or reset happened between
/// them (edits to files alone are not seen; the visible tier covers them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitdirStamp {
    entries: [Option<(SystemTime, u64)>; 3],
}

impl GitdirStamp {
    pub fn read(gitdir: &Path) -> Self {
        let stat = |rel: &str| {
            let meta = std::fs::metadata(gitdir.join(rel)).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        };
        Self {
            entries: [stat("index"), stat("HEAD"), stat("logs/HEAD")],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    /// The latest of the three mtimes: when git last moved here.
    pub fn latest(&self) -> Option<SystemTime> {
        self.entries.iter().flatten().map(|(time, _)| *time).max()
    }
}

// ---------------------------------------------------------------------------
// Git, off the UI thread.

/// `git` (tokio) with repo-discovery variables scrubbed, as
/// `agentd::git::git_command` does, killed when dropped (a timeout).
fn git_async(dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Runs git in `dir`, feeding stdout lines to `on_line` until it returns
/// false (the process is then killed). Errors name the command and dir.
async fn run_git_lines(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
    mut on_line: impl FnMut(&str) -> bool,
) -> Result<(), String> {
    let what = || format!("git {} in {}", args.join(" "), dir.display());
    let run = async {
        let mut child = git_async(dir)
            .args(args)
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", what()))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        // Stdout first: these commands write little to stderr, so its pipe
        // never fills while stdout is read. Stopping early must kill git
        // before anything waits on it, or git blocks on a full stdout pipe.
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if !on_line(&line) {
                        // A cap: the exit status says nothing.
                        let _ = child.start_kill();
                        return Ok(());
                    }
                }
                Ok(None) => break,
                Err(error) => return Err(format!("reading {}: {error}", what())),
            }
        }
        let mut err = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut err).await;
        let status = child
            .wait()
            .await
            .map_err(|error| format!("waiting for {}: {error}", what()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} failed: {}",
                what(),
                String::from_utf8_lossy(&err).trim()
            ))
        }
    };
    tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| format!("{} timed out after {}s", what(), timeout.as_secs()))?
}

async fn git_text(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut out = String::new();
    run_git_lines(dir, args, timeout, |line| {
        out.push_str(line);
        out.push('\n');
        true
    })
    .await?;
    Ok(out)
}

async fn list_worktrees_async(dir: &Path) -> Result<Vec<WorktreeInfo>, String> {
    let text = git_text(
        dir,
        &["--no-optional-locks", "worktree", "list", "--porcelain"],
        GIT_TIMEOUT,
    )
    .await?;
    Ok(worktree_follow::parse_worktree_porcelain(&text)
        .into_iter()
        .map(|entry| WorktreeInfo {
            path: worktree_follow::resolve_for_match(&entry.path),
            branch: entry.branch,
        })
        .collect())
}

/// One owned checkout with its live state.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchStatus {
    pub owned: OwnedBranch,
    pub base: Option<BaseChoice>,
    pub status: Option<StatusCounts>,
    /// `(behind, ahead)` against `base`.
    pub vs_base: Option<(u32, u32)>,
    /// What went wrong reading this checkout, if anything.
    pub error: Option<String>,
    pub stamp: GitdirStamp,
    /// When the numbers were read.
    pub checked_at: SystemTime,
}

impl BranchStatus {
    /// The latest of the facts' last touch and git's last move.
    pub fn last_activity(&self) -> Option<SystemTime> {
        let facts = self.owned.last.as_deref().and_then(parse_stamp);
        facts.into_iter().chain(self.stamp.latest()).max()
    }

    /// "coordinator", "worker PRJ-101", "matches PRJ-101".
    pub fn owner_label(&self) -> String {
        self.owned.owner.clone()
    }

    /// "PR #12" when the task store already recorded one (never fetched).
    pub fn pr_label(&self) -> Option<String> {
        self.owned
            .task
            .as_ref()
            .and_then(|task| task.pr_number)
            .map(|number| format!("PR #{number}"))
    }
}

/// What one refresh does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Rediscover owned branches (worktree lists, refs), read every one.
    Full,
    /// Keep the previous rows; read only those whose gitdir stamp moved.
    Gated,
}

#[derive(Debug, Clone)]
pub struct RefreshPlan {
    pub mode: RefreshMode,
    pub candidates: Vec<BranchCandidate>,
    pub issue_keys: Vec<String>,
    pub key_repos: Vec<PathBuf>,
    pub previous: Vec<BranchStatus>,
}

#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub mode: RefreshMode,
    pub branches: Vec<BranchStatus>,
    /// Repo-level failures (a worktree list that could not be read).
    pub errors: Vec<String>,
    pub git_runs: usize,
    pub skipped_unchanged: usize,
    pub took: Duration,
}

impl RefreshOutcome {
    /// Whether any row's numbers differ from `previous` (same rows, same
    /// counts, same state), ignoring when they were read.
    pub fn changed_from(&self, previous: &[BranchStatus]) -> bool {
        let key = |row: &BranchStatus| {
            (
                row.owned.clone(),
                row.base.clone(),
                row.status.clone(),
                row.vs_base,
                row.error.clone(),
                row.stamp.clone(),
            )
        };
        self.branches.len() != previous.len()
            || self
                .branches
                .iter()
                .zip(previous)
                .any(|(a, b)| key(a) != key(b))
    }
}

/// "12s ago", "5m ago", "now" for a reading's age.
pub fn age_label(at: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(at)
        .map(|age| age.as_secs())
        .unwrap_or(0);
    match secs {
        0..=2 => "now".to_string(),
        3..=59 => format!("{secs}s ago"),
        _ => format!("{} ago", chats::format_age(at)),
    }
}

/// Runs one refresh: sequential git calls, each with [`GIT_TIMEOUT`], no
/// new call after [`REFRESH_DEADLINE`]. Never call on the UI thread.
pub async fn refresh(plan: RefreshPlan) -> RefreshOutcome {
    let started = Instant::now();
    let mut outcome = RefreshOutcome {
        mode: plan.mode,
        branches: Vec::new(),
        errors: Vec::new(),
        git_runs: 0,
        skipped_unchanged: 0,
        took: Duration::ZERO,
    };
    let owned_with_base: Vec<(OwnedBranch, Option<BaseChoice>)> = match plan.mode {
        RefreshMode::Gated => plan
            .previous
            .iter()
            .map(|row| (row.owned.clone(), row.base.clone()))
            .collect(),
        RefreshMode::Full => discover(&plan, &mut outcome, started).await,
    };
    for (owned, base) in owned_with_base {
        let previous = plan
            .previous
            .iter()
            .find(|row| row.owned.checkout == owned.checkout && row.base == base);
        let stamp = gitdir_of(&owned.checkout)
            .map(|gitdir| GitdirStamp::read(&gitdir))
            .unwrap_or_default();
        if let Some(previous) = previous {
            let unchanged = !stamp.is_empty() && previous.stamp == stamp;
            if (plan.mode == RefreshMode::Gated && unchanged)
                || started.elapsed() > REFRESH_DEADLINE
            {
                let mut row = previous.clone();
                row.owned = owned;
                outcome.skipped_unchanged += usize::from(unchanged);
                outcome.branches.push(row);
                continue;
            }
        } else if started.elapsed() > REFRESH_DEADLINE {
            outcome.branches.push(BranchStatus {
                owned,
                base,
                status: None,
                vs_base: None,
                error: Some("not read: refresh time limit reached".to_string()),
                stamp,
                checked_at: SystemTime::now(),
            });
            continue;
        }
        let row = read_checkout(owned, base, stamp, &mut outcome.git_runs).await;
        outcome.branches.push(row);
    }
    outcome.took = started.elapsed();
    outcome
}

/// Worktree lists per repo, rule (a)/(b)/(c) resolution, bases per row.
async fn discover(
    plan: &RefreshPlan,
    outcome: &mut RefreshOutcome,
    started: Instant,
) -> Vec<(OwnedBranch, Option<BaseChoice>)> {
    let mut lists: HashMap<PathBuf, Vec<WorktreeInfo>> = HashMap::new();
    // Worktree lists come back canonical; compare like with like.
    let key_repos: Vec<PathBuf> = plan
        .key_repos
        .iter()
        .map(|repo| worktree_follow::resolve_for_match(repo))
        .collect();
    let mut candidates = plan.candidates.clone();
    for candidate in &mut candidates {
        candidate.checkout = worktree_follow::resolve_for_match(&candidate.checkout);
        candidate.repo = candidate
            .repo
            .as_deref()
            .map(worktree_follow::resolve_for_match);
    }
    let mut probes: Vec<PathBuf> = key_repos.clone();
    for candidate in &candidates {
        if let Some(repo) = &candidate.repo {
            if !probes.contains(repo) {
                probes.push(repo.clone());
            }
        }
    }
    for probe in probes {
        if started.elapsed() > REFRESH_DEADLINE {
            outcome.errors.push(format!(
                "{}: not read, refresh time limit reached",
                probe.display()
            ));
            continue;
        }
        if !probe.is_dir() {
            continue;
        }
        outcome.git_runs += 1;
        match list_worktrees_async(&probe).await {
            Ok(list) if !list.is_empty() => {
                lists.insert(list[0].path.clone(), list);
            }
            Ok(_) => {}
            Err(error) => outcome.errors.push(error),
        }
    }
    // Task worktrees name only their checkout: list from there, unless a
    // known repo already lists it.
    for candidate in candidates.iter().filter(|c| c.repo.is_none()) {
        let listed = lists
            .values()
            .any(|list| locate(list, &candidate.checkout).is_some());
        if listed || !candidate.checkout.is_dir() || started.elapsed() > REFRESH_DEADLINE {
            continue;
        }
        outcome.git_runs += 1;
        match list_worktrees_async(&candidate.checkout).await {
            Ok(list) if !list.is_empty() => {
                lists.insert(list[0].path.clone(), list);
            }
            Ok(_) => {}
            Err(error) => outcome.errors.push(error),
        }
    }
    let owned = resolve_owned(&candidates, &plan.issue_keys, &key_repos, &lists);
    let mut refs: HashMap<PathBuf, RepoRefs> = HashMap::new();
    let mut out = Vec::with_capacity(owned.len());
    for row in owned {
        let task_base = row.task.as_ref().map(|task| task.base.as_str());
        if task_base.is_none() && !refs.contains_key(&row.repo) {
            let args = repo_refs_args();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            outcome.git_runs += 1;
            let parsed = match git_text(&row.repo, &args, GIT_TIMEOUT).await {
                Ok(text) => parse_repo_refs(&text),
                Err(error) => {
                    outcome.errors.push(error);
                    RepoRefs::default()
                }
            };
            refs.insert(row.repo.clone(), parsed);
        }
        let empty = RepoRefs::default();
        let base = choose_base(task_base, refs.get(&row.repo).unwrap_or(&empty));
        out.push((row, base));
    }
    out
}

/// Status plus ahead/behind for one checkout.
async fn read_checkout(
    owned: OwnedBranch,
    base: Option<BaseChoice>,
    stamp: GitdirStamp,
    git_runs: &mut usize,
) -> BranchStatus {
    let mut counts = StatusCounts::default();
    *git_runs += 1;
    let status = run_git_lines(
        &owned.checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ],
        GIT_TIMEOUT,
        |line| counts.observe(line, STATUS_COUNT_CAP),
    )
    .await;
    let mut errors: Vec<String> = Vec::new();
    let status = match status {
        Ok(()) => Some(counts),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    let mut vs_base = None;
    if let Some(base) = &base {
        let range = format!("{}...HEAD", base.reference);
        *git_runs += 1;
        match git_text(
            &owned.checkout,
            &[
                "--no-optional-locks",
                "rev-list",
                "--left-right",
                "--count",
                &range,
                "--",
            ],
            GIT_TIMEOUT,
        )
        .await
        {
            Ok(text) => match parse_left_right(&text) {
                Some(counts) => vs_base = Some(counts),
                None => errors.push(format!(
                    "unexpected rev-list output in {}: {:?}",
                    owned.checkout.display(),
                    text.trim()
                )),
            },
            Err(error) => errors.push(error),
        }
    }
    BranchStatus {
        owned,
        base,
        status,
        vs_base,
        error: (!errors.is_empty()).then(|| errors.join("; ")),
        stamp,
        checked_at: SystemTime::now(),
    }
}

// ---------------------------------------------------------------------------
// Branch diffs.

/// Which changes a branch diff shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    /// Working tree and index against HEAD, plus untracked file names.
    Uncommitted,
    /// HEAD against its merge-base with the base.
    VsBase,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchDiff {
    pub lines: Vec<crate::agentd::git::FileDiffLine>,
    pub files: usize,
    /// Reading stopped at [`DIFF_LINE_CAP`].
    pub truncated: bool,
    /// Short merge-base commit for a vs-base diff.
    pub merge_base: Option<String>,
}

/// Parses `git diff` unified output into the diff panel's lines: a header
/// per file and per hunk, numbered context, additions and deletions.
#[derive(Debug, Default)]
pub struct UnifiedDiffParser {
    old_line: u32,
    new_line: u32,
    pub out: BranchDiff,
}

impl UnifiedDiffParser {
    pub fn feed(&mut self, line: &str) {
        use crate::agentd::git::{DiffLineKind, FileDiffLine};
        let header = |content: String| FileDiffLine {
            content,
            kind: DiffLineKind::Header,
            old_line: None,
            new_line: None,
        };
        if let Some(rest) = line.strip_prefix("diff --git ") {
            self.out.files += 1;
            let name = rest
                .split_once(" b/")
                .map(|(_, b)| b)
                .unwrap_or(rest)
                .to_string();
            self.out
                .lines
                .push(header(format!("\u{2500}\u{2500} {name}")));
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            let (old, new) = parse_hunk_starts(hunk).unwrap_or((0, 0));
            self.old_line = old;
            self.new_line = new;
            self.out.lines.push(header(line.to_string()));
        } else if line.starts_with("Binary files ") {
            self.out.lines.push(header(line.to_string()));
        } else if line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("index ")
            || line.starts_with('\\')
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
        {
        } else if let Some(text) = line.strip_prefix('+') {
            self.out.lines.push(FileDiffLine {
                content: text.to_string(),
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(self.new_line),
            });
            self.new_line += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            self.out.lines.push(FileDiffLine {
                content: text.to_string(),
                kind: DiffLineKind::Deletion,
                old_line: Some(self.old_line),
                new_line: None,
            });
            self.old_line += 1;
        } else if let Some(text) = line.strip_prefix(' ') {
            self.out.lines.push(FileDiffLine {
                content: text.to_string(),
                kind: DiffLineKind::Context,
                old_line: Some(self.old_line),
                new_line: Some(self.new_line),
            });
            self.old_line += 1;
            self.new_line += 1;
        }
    }
}

/// `-12,5 +14,7 @@ ...` -> (12, 14).
fn parse_hunk_starts(hunk: &str) -> Option<(u32, u32)> {
    let mut parts = hunk.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let start = |range: &str| range.split(',').next()?.parse().ok();
    Some((start(old)?, start(new)?))
}

/// Loads a whole-checkout diff. Never call on the UI thread.
pub async fn load_branch_diff(
    checkout: PathBuf,
    source: DiffSource,
    base: Option<String>,
) -> Result<BranchDiff, String> {
    let mut parser = UnifiedDiffParser::default();
    let mut count = 0usize;
    let mut truncated = false;
    let base = match source {
        DiffSource::Uncommitted => None,
        DiffSource::VsBase => Some(base.ok_or_else(|| {
            format!(
                "no base branch found for {}: no task base, upstream default or local main/master",
                checkout.display()
            )
        })?),
    };
    let range = base.as_deref().map(|base| format!("{base}...HEAD"));
    let args: Vec<&str> = match &range {
        None => vec![
            "--no-optional-locks",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "HEAD",
            "--",
        ],
        Some(range) => vec![
            "--no-optional-locks",
            "diff",
            "--no-color",
            "--no-ext-diff",
            range,
            "--",
        ],
    };
    run_git_lines(&checkout, &args, GIT_TIMEOUT, |line| {
        count += 1;
        if count > DIFF_LINE_CAP {
            truncated = true;
            return false;
        }
        parser.feed(line);
        true
    })
    .await?;
    let mut diff = parser.out;
    diff.truncated = truncated;
    match base {
        None => {
            let mut names: Vec<String> = Vec::new();
            let mut more = false;
            run_git_lines(
                &checkout,
                &[
                    "--no-optional-locks",
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                ],
                GIT_TIMEOUT,
                |line| {
                    if names.len() >= UNTRACKED_LIST_CAP {
                        more = true;
                        return false;
                    }
                    names.push(line.to_string());
                    true
                },
            )
            .await?;
            if !names.is_empty() {
                use crate::agentd::git::{DiffLineKind, FileDiffLine};
                let more = if more { " (first 200)" } else { "" };
                diff.lines.push(FileDiffLine {
                    content: format!("\u{2500}\u{2500} untracked{more}"),
                    kind: DiffLineKind::Header,
                    old_line: None,
                    new_line: None,
                });
                diff.files += names.len();
                for name in names {
                    diff.lines.push(FileDiffLine {
                        content: name,
                        kind: DiffLineKind::Addition,
                        old_line: None,
                        new_line: None,
                    });
                }
            }
        }
        Some(base) => {
            let text = git_text(
                &checkout,
                &["--no-optional-locks", "merge-base", &base, "HEAD"],
                GIT_TIMEOUT,
            )
            .await?;
            diff.merge_base = text.trim().get(..7).map(str::to_string);
        }
    }
    Ok(diff)
}

// ---------------------------------------------------------------------------
// Copy helpers.

/// `text` cut to `max` characters with an ellipsis.
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// A checkout path for a row: "main checkout", else relative to the main
/// checkout's parent (`.worktrees/api-server-prj-1`), else `~/…`.
pub fn short_checkout(owned: &OwnedBranch, home: Option<&Path>) -> String {
    if owned.is_main {
        return "main checkout".to_string();
    }
    if let Some(rel) = owned
        .repo
        .parent()
        .and_then(|parent| owned.checkout.strip_prefix(parent).ok())
    {
        return rel.display().to_string();
    }
    if let Some(rel) = home.and_then(|home| owned.checkout.strip_prefix(home).ok()) {
        return format!("~/{}", rel.display());
    }
    owned.checkout.display().to_string()
}

/// "↑3 ↓1 vs main", "even with main".
pub fn vs_base_label(row: &BranchStatus) -> Option<String> {
    let base = row.base.as_ref()?;
    let (behind, ahead) = row.vs_base?;
    Some(match (ahead, behind) {
        (0, 0) => format!("even with {}", base.reference),
        (ahead, 0) => format!("\u{2191}{ahead} vs {}", base.reference),
        (0, behind) => format!("\u{2193}{behind} vs {}", base.reference),
        (ahead, behind) => format!("\u{2191}{ahead} \u{2193}{behind} vs {}", base.reference),
    })
}

pub fn push_label(state: PushState) -> String {
    match state {
        PushState::NoUpstream => "not pushed".to_string(),
        PushState::UpstreamGone => "upstream gone".to_string(),
        PushState::Pushed => "pushed".to_string(),
        PushState::Unpushed(n) => format!("{n} unpushed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_facts::{ChatChild, ChatFacts, CheckoutTouch, RepoTouch};
    use crate::chats::ChatBackend;

    fn entry(id: &str, cwd: &str) -> ChatIndexEntry {
        ChatIndexEntry {
            id: id.into(),
            backend: ChatBackend::Claude,
            path: PathBuf::from(format!("/t/{id}.jsonl")),
            cwd: PathBuf::from(cwd),
            repo_root: None,
            is_worktree: false,
            branch: None,
            title: format!("chat {id}"),
            latest: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 1,
            dead_cwd: false,
            facts: None,
            root_id: None,
        }
    }

    fn touch(repo: &str, dir: &str, branch: &str, edits: u64, last: &str) -> CheckoutTouch {
        CheckoutTouch {
            repo: PathBuf::from(repo),
            dir: PathBuf::from(dir),
            branch: Some(branch.into()),
            edits,
            last: last.into(),
        }
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn work_chats_take_scope_and_descendants_of_scoped_roots() {
        let root = entry("root", "/w/acme");
        let mut worker = entry("worker", "/elsewhere/wt");
        worker.root_id = Some("root".into());
        let other = entry("other", "/w/unrelated");
        let mut stray = entry("stray", "/elsewhere/x");
        stray.root_id = Some("other".into());
        let entries = vec![root, worker, other, stray];
        let ids: Vec<&str> = work_chats(&entries, Path::new("/w/acme"))
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["root", "worker"]);
    }

    #[test]
    fn repos_touched_need_edits_and_sort_by_edits() {
        let mut a = entry("a", "/w");
        a.facts = Some(ChatFacts {
            repos_touched: vec![
                RepoTouch {
                    dir: "/w/infra".into(),
                    edits: 5,
                },
                RepoTouch {
                    dir: "/w/docs".into(),
                    edits: 0,
                },
            ],
            ..ChatFacts::default()
        });
        let mut b = entry("b", "/w");
        b.facts = Some(ChatFacts {
            repos_touched: vec![
                RepoTouch {
                    dir: "/w/api".into(),
                    edits: 3,
                },
                RepoTouch {
                    dir: "/w/infra".into(),
                    edits: 1,
                },
            ],
            ..ChatFacts::default()
        });
        let work = vec![&a, &b];
        let repos = repos_touched(&work);
        assert_eq!(
            repos,
            vec![
                RepoTouched {
                    dir: "/w/infra".into(),
                    edits: 6
                },
                RepoTouched {
                    dir: "/w/api".into(),
                    edits: 3
                },
            ]
        );
    }

    #[test]
    fn issue_keys_drop_single_digit_prose_unless_backed() {
        let a = keys(&["PRJ-101", "GPT-5", "SHA-1", "OPS-7"]);
        let b = keys(&["PRJ-101", "OPS-8"]);
        let c = keys(&["ABC-3"]);
        let per_chat: Vec<&[String]> = vec![&a, &b, &c];
        // OPS twice in the work; ABC on a branch; GPT and SHA once each.
        let kept = work_issue_keys(&per_chat, &["feat/abc-3-thing", "main"]);
        assert_eq!(kept, ["PRJ-101", "ABC-3", "OPS-7", "OPS-8"]);
    }

    #[test]
    fn candidates_follow_ownership_rules() {
        let mut root = entry("root", "/w");
        root.facts = Some(ChatFacts {
            checkouts: vec![
                // Main checkout edited: owned.
                touch("/r/api", "/r/api", "main", 2, "2026-10-01T00:00:00Z"),
                // Main checkout only visited: not owned.
                touch("/r/infra", "/r/infra", "main", 0, "2026-10-01T00:00:00Z"),
                // Linked worktree visited: owned.
                touch(
                    "/r/infra",
                    "/r/infra-prj-1",
                    "prj-1",
                    0,
                    "2026-10-02T00:00:00Z",
                ),
            ],
            children: vec![ChatChild {
                session_id: "worker".into(),
                kind: ChildKind::Worker,
                delegation_id: None,
                task_id: Some("t1".into()),
                title: None,
            }],
            ..ChatFacts::default()
        });
        let mut worker = entry("worker", "/w/task");
        worker.root_id = Some("root".into());
        worker.facts = Some(ChatFacts {
            checkouts: vec![touch(
                "/r/api",
                "/cfg/worktrees/api/t1",
                "prj-2-x",
                4,
                "2026-10-03T00:00:00Z",
            )],
            ..ChatFacts::default()
        });
        let tasks = vec![TaskInfo {
            task_id: "t1".into(),
            title: "Do the thing".into(),
            issue_key: Some("PRJ-2".into()),
            branch: "prj-2-x".into(),
            base: "develop".into(),
            worktree: Some("/cfg/worktrees/api/t1".into()),
            pr_number: Some(12),
        }];
        let work = vec![&root, &worker];
        let found = branch_candidates(&work, &tasks);
        let dirs: Vec<(&str, &Ownership, &str)> = found
            .iter()
            .map(|c| (c.checkout.to_str().unwrap(), &c.ownership, c.owner.as_str()))
            .collect();
        assert_eq!(
            dirs,
            [
                ("/r/api", &Ownership::Edited, "coordinator"),
                ("/r/infra-prj-1", &Ownership::WorkedIn, "coordinator"),
                ("/cfg/worktrees/api/t1", &Ownership::Task, "worker PRJ-2"),
            ]
        );
        let task_row = &found[2];
        assert_eq!(task_row.edits, 4);
        assert_eq!(task_row.repo.as_deref(), Some(Path::new("/r/api")));
        assert_eq!(task_row.task.as_ref().unwrap().base, "develop");
        assert_eq!(task_row.last.as_deref(), Some("2026-10-03T00:00:00Z"));
    }

    #[test]
    fn resolve_drops_gone_checkouts_and_adds_key_matches_only() {
        let wt = |path: &str, branch: &str| WorktreeInfo {
            path: PathBuf::from(path),
            branch: Some(branch.into()),
        };
        let mut lists = HashMap::new();
        lists.insert(
            PathBuf::from("/r/api"),
            vec![
                wt("/r/api", "main"),
                wt("/r/api-prj-101", "work/prj-101-standby"),
                wt("/r/api-prj-1010", "prj-1010-other"),
                wt("/r/api-ops-9", "ops-9-unrelated"),
                wt("/r/api-mine", "renamed-branch"),
            ],
        );
        let candidates = vec![
            BranchCandidate {
                repo: Some("/r/api".into()),
                checkout: "/r/api-mine".into(),
                ownership: Ownership::Edited,
                owner: "coordinator".into(),
                last: None,
                edits: 3,
                task: None,
            },
            BranchCandidate {
                repo: Some("/r/api".into()),
                checkout: "/r/api-removed".into(),
                ownership: Ownership::Edited,
                owner: "coordinator".into(),
                last: None,
                edits: 1,
                task: None,
            },
        ];
        let owned = resolve_owned(
            &candidates,
            &keys(&["PRJ-101"]),
            &[PathBuf::from("/r/api")],
            &lists,
        );
        let rows: Vec<(&str, Option<&str>, &Ownership)> = owned
            .iter()
            .map(|o| {
                (
                    o.checkout.to_str().unwrap(),
                    o.branch.as_deref(),
                    &o.ownership,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("/r/api-mine", Some("renamed-branch"), &Ownership::Edited),
                (
                    "/r/api-prj-101",
                    Some("work/prj-101-standby"),
                    &Ownership::IssueKey("PRJ-101".into())
                ),
            ]
        );
    }

    #[test]
    fn base_prefers_task_then_upstream_then_local_default() {
        let refs = parse_repo_refs(
            "refs/remotes/origin/HEAD\trefs/remotes/origin/develop\n\
             refs/remotes/origin/main\t\n\
             refs/heads/main\t\n\
             refs/heads/main/old\t\n\
             refs/heads/develop\t\n",
        );
        assert_eq!(refs.upstream.as_deref(), Some("origin/develop"));
        assert_eq!(refs.local, ["main", "develop"]);
        let task = choose_base(Some("release"), &refs).unwrap();
        assert_eq!(
            (task.reference.as_str(), task.source),
            ("release", BaseSource::Task)
        );
        let upstream = choose_base(None, &refs).unwrap();
        assert_eq!(upstream.source, BaseSource::Upstream);
        let no_head = parse_repo_refs("refs/remotes/origin/master\t\nrefs/heads/master\t\n");
        assert_eq!(no_head.upstream.as_deref(), Some("origin/master"));
        let local_only = parse_repo_refs("refs/heads/develop\t\nrefs/heads/trunk\t\n");
        let base = choose_base(Some("  "), &local_only).unwrap();
        assert_eq!(
            (base.reference.as_str(), base.source),
            ("develop", BaseSource::RepoDefault)
        );
        assert_eq!(choose_base(None, &RepoRefs::default()), None);
    }

    #[test]
    fn status_counts_without_storing_and_caps() {
        let out = "# branch.oid 0123\n\
                   # branch.head prj-1-x\n\
                   # branch.upstream origin/prj-1-x\n\
                   # branch.ab +2 -1\n\
                   1 M. N... 100644 100644 100644 a b src/a.rs\n\
                   1 .M N... 100644 100644 100644 a b src/b.rs\n\
                   1 MM N... 100644 100644 100644 a b src/c.rs\n\
                   2 R. N... 100644 100644 100644 a b R100 new.rs\told.rs\n\
                   u UU N... 1 2 3 4 a b c conflict.rs\n\
                   ? notes.txt\n\
                   ! ignored.log\n";
        let counts = parse_status_v2(out, 100);
        assert_eq!(counts.head.as_deref(), Some("prj-1-x"));
        assert_eq!(counts.upstream_ab, Some((2, 1)));
        assert_eq!(
            (
                counts.staged,
                counts.unstaged,
                counts.untracked,
                counts.conflicted
            ),
            (3, 2, 1, 1)
        );
        assert_eq!(counts.files, 6);
        assert_eq!(counts.push_state(), PushState::Unpushed(2));
        assert_eq!(counts.dirty_label(), "6 dirty");
        let capped = parse_status_v2(out, 2);
        assert!(capped.capped);
        assert_eq!(capped.dirty_label(), "2+ dirty");
        assert_eq!(capped.head.as_deref(), Some("prj-1-x"));
        let gone = parse_status_v2("# branch.head x\n# branch.upstream origin/x\n", 10);
        assert_eq!(gone.push_state(), PushState::UpstreamGone);
        let detached = parse_status_v2("# branch.head (detached)\n", 10);
        assert_eq!(detached.head, None);
        assert_eq!(detached.push_state(), PushState::NoUpstream);
    }

    #[test]
    fn left_right_counts_parse() {
        assert_eq!(parse_left_right("3\t5\n"), Some((3, 5)));
        assert_eq!(parse_left_right("0 0"), Some((0, 0)));
        assert_eq!(parse_left_right("x\t1"), None);
        assert_eq!(parse_left_right("1"), None);
        assert_eq!(parse_left_right("1 2 3"), None);
    }

    #[test]
    fn unified_diff_parses_files_hunks_and_numbers() {
        let mut parser = UnifiedDiffParser::default();
        for line in [
            "diff --git a/src/a.rs b/src/a.rs",
            "index 1..2 100644",
            "--- a/src/a.rs",
            "+++ b/src/a.rs",
            "@@ -10,3 +10,4 @@ fn x() {",
            " keep",
            "-old",
            "+new",
            "+more",
            "\\ No newline at end of file",
        ] {
            parser.feed(line);
        }
        let out = parser.out;
        assert_eq!(out.files, 1);
        use crate::agentd::git::DiffLineKind;
        let kinds: Vec<(DiffLineKind, Option<u32>, Option<u32>)> = out
            .lines
            .iter()
            .map(|l| (l.kind, l.old_line, l.new_line))
            .collect();
        assert_eq!(
            kinds,
            [
                (DiffLineKind::Header, None, None),
                (DiffLineKind::Header, None, None),
                (DiffLineKind::Context, Some(10), Some(10)),
                (DiffLineKind::Deletion, Some(11), None),
                (DiffLineKind::Addition, None, Some(11)),
                (DiffLineKind::Addition, None, Some(12)),
            ]
        );
        assert!(out.lines[0].content.ends_with("src/a.rs"));
    }

    // -----------------------------------------------------------------------
    // Real git.

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    #[tokio::test]
    async fn real_git_owned_unrelated_and_key_matched_worktrees() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = worktree_follow::resolve_for_match(temp.path());
        let repo = root.join("api-server");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let owned_wt = root.join("api-server-edit");
        let unrelated_wt = root.join("api-server-ops-9");
        let keyed_wt = root.join("api-server-prj-101");
        for (path, branch) in [
            (&owned_wt, "feature-standby"),
            (&unrelated_wt, "ops-9-unrelated"),
            (&keyed_wt, "work/prj-101-cloud"),
        ] {
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        }
        // The owned worktree: one commit ahead of main and one dirty file.
        std::fs::write(owned_wt.join("b.txt"), "two\n").unwrap();
        git(&owned_wt, &["add", "b.txt"]);
        git(&owned_wt, &["commit", "-q", "-m", "work"]);
        std::fs::write(owned_wt.join("a.txt"), "changed\n").unwrap();
        std::fs::write(owned_wt.join("scratch.txt"), "x\n").unwrap();

        let mut chat = entry("c1", repo.to_str().unwrap());
        chat.facts = Some(ChatFacts {
            issue_keys: keys(&["PRJ-101"]),
            repos_touched: vec![RepoTouch {
                dir: repo.clone(),
                edits: 2,
            }],
            checkouts: vec![CheckoutTouch {
                repo: repo.clone(),
                dir: owned_wt.clone(),
                branch: Some("feature-standby".into()),
                edits: 2,
                last: "2026-10-01T00:00:00Z".into(),
            }],
            ..ChatFacts::default()
        });
        let entries = vec![chat];
        let summary = WorkSummary::build(&entries, &root, &[]);
        assert_eq!(summary.issue_keys, ["PRJ-101"]);
        let plan = RefreshPlan {
            mode: RefreshMode::Full,
            candidates: summary.candidates.clone(),
            issue_keys: summary.issue_keys.clone(),
            key_repos: summary.key_repos.clone(),
            previous: Vec::new(),
        };
        let outcome = refresh(plan).await;
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let rows: Vec<(&Path, Option<&str>)> = outcome
            .branches
            .iter()
            .map(|row| (row.owned.checkout.as_path(), row.owned.branch.as_deref()))
            .collect();
        assert_eq!(
            rows,
            [
                (owned_wt.as_path(), Some("feature-standby")),
                (keyed_wt.as_path(), Some("work/prj-101-cloud")),
            ],
            "the unrelated worktree must not appear"
        );
        let owned = &outcome.branches[0];
        assert_eq!(owned.error, None);
        let base = owned.base.as_ref().unwrap();
        assert_eq!(
            (base.reference.as_str(), base.source),
            ("main", BaseSource::RepoDefault)
        );
        assert_eq!(owned.vs_base, Some((0, 1)));
        let status = owned.status.as_ref().unwrap();
        assert_eq!((status.unstaged, status.untracked, status.files), (1, 1, 2));
        assert_eq!(status.push_state(), PushState::NoUpstream);
        assert!(!owned.stamp.is_empty());
        let keyed = &outcome.branches[1];
        assert_eq!(keyed.owner_label(), "matches PRJ-101");
        assert_eq!(keyed.vs_base, Some((0, 0)));
        assert_eq!(keyed.status.as_ref().unwrap().files, 0);

        // A gated refresh with nothing moved runs no git.
        let gated = refresh(RefreshPlan {
            mode: RefreshMode::Gated,
            candidates: Vec::new(),
            issue_keys: Vec::new(),
            key_repos: Vec::new(),
            previous: outcome.branches.clone(),
        })
        .await;
        assert_eq!(gated.git_runs, 0);
        assert_eq!(gated.skipped_unchanged, 2);
        // A commit moves the stamp; the gated refresh reads that row again.
        git(&keyed_wt, &["commit", "-q", "--allow-empty", "-m", "more"]);
        let gated = refresh(RefreshPlan {
            mode: RefreshMode::Gated,
            candidates: Vec::new(),
            issue_keys: Vec::new(),
            key_repos: Vec::new(),
            previous: outcome.branches.clone(),
        })
        .await;
        assert_eq!(gated.skipped_unchanged, 1);
        assert_eq!(gated.branches[1].vs_base, Some((0, 1)));

        // Both diff sources.
        let uncommitted = load_branch_diff(owned_wt.clone(), DiffSource::Uncommitted, None)
            .await
            .unwrap();
        assert_eq!(uncommitted.files, 2, "{:?}", uncommitted.lines);
        assert!(uncommitted.lines.iter().any(|l| l.content == "changed"));
        assert!(uncommitted.lines.iter().any(|l| l.content == "scratch.txt"));
        let vs_base = load_branch_diff(
            owned_wt.clone(),
            DiffSource::VsBase,
            Some("main".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(vs_base.files, 1);
        assert!(vs_base.lines.iter().any(|l| l.content == "two"));
        assert_eq!(vs_base.merge_base.as_ref().map(String::len), Some(7));
        let missing = load_branch_diff(owned_wt, DiffSource::VsBase, None).await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn real_git_capped_status_stops_without_waiting_for_git() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("big");
        std::fs::create_dir_all(repo.join("generated")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        // Far more than a pipe buffer of status output.
        for i in 0..3000 {
            let name = format!("generated/untracked-file-with-a-long-name-{i:05}.txt");
            std::fs::write(repo.join(name), "x").unwrap();
        }
        let started = Instant::now();
        let mut counts = StatusCounts::default();
        run_git_lines(
            &repo,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
            ],
            Duration::from_secs(5),
            |line| counts.observe(line, 10),
        )
        .await
        .expect("a capped read is not an error");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(counts.capped);
        assert_eq!(
            (counts.untracked, counts.dirty_label().as_str()),
            (10, "10+ dirty")
        );
    }

    /// Prints the overview model for real data. Read-only. Run with
    /// `GITTERM_REAL_FACTS_DIR=<config>/chat-meta GITTERM_REAL_WORKSPACE_DIR=<dir>
    /// cargo test --lib workspace_overview::tests::real_data -- --ignored --nocapture`.
    /// Tasks are read from `tasks.json` beside the facts dir when present.
    #[tokio::test]
    #[ignore]
    async fn real_data_overview_model() {
        let (Ok(facts_dir), Ok(ws_dir)) = (
            std::env::var("GITTERM_REAL_FACTS_DIR"),
            std::env::var("GITTERM_REAL_WORKSPACE_DIR"),
        ) else {
            panic!("set GITTERM_REAL_FACTS_DIR and GITTERM_REAL_WORKSPACE_DIR");
        };
        let facts_dir = PathBuf::from(facts_dir);
        let ws_dir = PathBuf::from(ws_dir);
        let store = facts_dir
            .parent()
            .map(crate::tasks::TaskStore::path_for_config_root)
            .filter(|path| path.exists())
            .map(|path| crate::tasks::TaskStore::load(path).expect("read tasks.json"));
        let started = Instant::now();
        let mut entries = chats::build_local_index();
        chat_facts::attach_facts(&mut entries, &facts_dir);
        let links = store
            .as_ref()
            .map(|store| {
                let uid_to_chat: HashMap<String, String> = store
                    .delegations()
                    .iter()
                    .filter_map(|d| {
                        Some((
                            d.parent.session_uid.clone(),
                            d.parent.chat_session_id.clone()?,
                        ))
                    })
                    .collect();
                chat_facts::store_links(store.delegations(), store.tasks(), &uid_to_chat)
            })
            .unwrap_or_default();
        chat_facts::resolve_tree(&mut entries, &links);
        let tasks: Vec<TaskInfo> = store
            .as_ref()
            .map(|store| store.tasks().iter().map(TaskInfo::from).collect())
            .unwrap_or_default();
        println!(
            "index: {} chats, built in {}ms",
            entries.len(),
            started.elapsed().as_millis()
        );
        let built = Instant::now();
        let summary = WorkSummary::build(&entries, &ws_dir, &tasks);
        println!(
            "summary in {}ms: {} chats, facts: {}",
            built.elapsed().as_millis(),
            summary.chat_count,
            summary.has_facts
        );
        println!("\nCOORDINATORS · {}", summary.coordinators.len());
        for coordinator in summary.coordinators.iter().take(15) {
            println!(
                "  {} · {}",
                shorten(&coordinator.title, 70),
                chats::format_age(coordinator.mtime)
            );
            if let Some(fan_out) = &coordinator.fan_out {
                println!("      fan-out: {fan_out}");
            }
            for child in coordinator.children.iter().take(5) {
                println!(
                    "      \u{21b3} {:?} {}",
                    child.kind,
                    shorten(&child.title, 60)
                );
            }
        }
        println!("\nREPOS TOUCHED · {}", summary.repos.len());
        for repo in &summary.repos {
            println!("  {} · {} edits", repo.dir.display(), repo.edits);
        }
        println!("\nISSUES · {}", summary.issue_keys.join(" · "));
        println!("\nCANDIDATES · {}", summary.candidates.len());
        let outcome = refresh(RefreshPlan {
            mode: RefreshMode::Full,
            candidates: summary.candidates.clone(),
            issue_keys: summary.issue_keys.clone(),
            key_repos: summary.key_repos.clone(),
            previous: Vec::new(),
        })
        .await;
        println!(
            "\nOWNED BRANCHES · {} ({} git runs, {}ms)",
            outcome.branches.len(),
            outcome.git_runs,
            outcome.took.as_millis()
        );
        let home = dirs::home_dir();
        let mut repo: Option<&Path> = None;
        for row in &outcome.branches {
            if repo != Some(row.owned.repo.as_path()) {
                repo = Some(row.owned.repo.as_path());
                println!("  {}", folder_name(&row.owned.repo));
            }
            let mut parts = vec![short_checkout(&row.owned, home.as_deref())];
            parts.extend(vs_base_label(row));
            if let Some(status) = &row.status {
                parts.push(status.dirty_label());
                parts.push(push_label(status.push_state()));
            }
            parts.push(row.owner_label());
            if let Some(base) = &row.base {
                parts.push(format!("base {} ({})", base.reference, base.source_label()));
            }
            println!(
                "    {} · {}",
                row.owned.branch.as_deref().unwrap_or("(detached)"),
                parts.join(" · ")
            );
            if let Some(error) = &row.error {
                println!("      error: {error}");
            }
        }
        for error in &outcome.errors {
            println!("  repo error: {error}");
        }
    }
}
