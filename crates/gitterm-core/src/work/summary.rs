//! Pure Work ownership, focus and summary rules. Git jobs and persistence stay in the host.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::chat_facts::{self, ChildKind};
use crate::chats::{self, ChatIndexEntry};
use crate::git_facts::BaseChoice;
use crate::tasks::TaskRecord;

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Short branch name; `None` for a detached HEAD.
    pub branch: Option<String>,
}

/// Status entries counted per checkout before counting stops ("5000+").
pub const STATUS_COUNT_CAP: usize = 5000;
/// Coordinators active within this are recent: listed by default and the
/// only ones a union view reads.
pub const RECENT_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

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
    /// Latest activity of the root or any indexed child.
    pub active: SystemTime,
    /// Active within [`RECENT_WINDOW`].
    pub recent: bool,
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

/// One repo header of the Repos section (TRU-192): its edits by the work
/// (`None` when the work has not edited it) and the owned checkouts there.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoGroup<'a> {
    pub dir: PathBuf,
    pub edits: Option<u64>,
    pub rows: Vec<&'a BranchStatus>,
}

/// One list from the repos the workspace lists (in its order), the repos the
/// work edited, and the owned checkouts: listed repos come first even when
/// untouched, then edited or branch-only repos; a repo appears once. A repo
/// the user removed by hand (`removed`) stays out unless it is listed.
pub fn merge_repo_groups<'a>(
    listed: &[PathBuf],
    removed: &[PathBuf],
    touched: &[RepoTouched],
    branches: &'a [BranchStatus],
) -> Vec<RepoGroup<'a>> {
    let hidden = |dir: &Path| {
        removed.iter().any(|gone| gone == dir) && !listed.iter().any(|known| known == dir)
    };
    fn slot(dir: &Path, groups: &mut Vec<RepoGroup<'_>>) -> usize {
        if let Some(idx) = groups.iter().position(|group| group.dir == dir) {
            return idx;
        }
        groups.push(RepoGroup {
            dir: dir.to_path_buf(),
            edits: None,
            rows: Vec::new(),
        });
        groups.len() - 1
    }
    let mut groups: Vec<RepoGroup<'a>> = Vec::new();
    for dir in listed {
        slot(dir, &mut groups);
    }
    for repo in touched.iter().filter(|repo| !hidden(&repo.dir)) {
        let idx = slot(&repo.dir, &mut groups);
        groups[idx].edits = Some(repo.edits);
    }
    for row in branches.iter().filter(|row| !hidden(&row.owned.repo)) {
        let idx = slot(&row.owned.repo, &mut groups);
        groups[idx].rows.push(row);
    }
    groups
}

pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Coordinators of `work` (roots, children folded under them as the Chats
/// panel folds them), most recently active first; `recent` when the root or
/// a child moved within [`RECENT_WINDOW`] of `now`.
pub fn coordinators(
    work: &[&ChatIndexEntry],
    all: &[ChatIndexEntry],
    now: SystemTime,
) -> Vec<Coordinator> {
    let cutoff = now
        .checked_sub(RECENT_WINDOW)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut out: Vec<Coordinator> = chats::fold_under_roots(work, all)
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
                .collect::<Vec<CoordinatorChild>>();
            let active = children
                .iter()
                .map(|child| child.mtime)
                .chain([root.mtime])
                .max()
                .unwrap_or(root.mtime);
            Coordinator {
                id: root.id.clone(),
                title: root.title.clone(),
                latest: root.latest.clone(),
                mtime: root.mtime,
                active,
                recent: active >= cutoff,
                fan_out: root
                    .facts
                    .as_ref()
                    .and_then(chat_facts::ChatFacts::fan_out_line),
                children,
            }
        })
        .collect();
    out.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.id.cmp(&b.id)));
    out
}

/// A recent coordinator that lives in another workspace but edited repos
/// inside this one.
#[derive(Debug, Clone, PartialEq)]
pub struct AlsoWorked {
    pub coordinator: Coordinator,
    /// Edits it made in repos inside this workspace's dir.
    pub edits_here: u64,
}

/// Recent coordinators outside the workspace at `ws_dir` (their own cwd and
/// repo are not in it) whose facts record edits in a repo inside it, most
/// recently active first.
pub fn also_worked_here(
    entries: &[ChatIndexEntry],
    ws_dir: &Path,
    now: SystemTime,
) -> Vec<AlsoWorked> {
    let here: HashSet<&str> = work_chats(entries, ws_dir)
        .into_iter()
        .map(|entry| entry.id.as_str())
        .collect();
    also_worked_outside(entries, &here, |dir| dir.starts_with(ws_dir), now)
}

/// Recent coordinators whose chats are not in `here` and whose facts
/// record edits in a repo `inside` accepts, most recently active first.
fn also_worked_outside(
    entries: &[ChatIndexEntry],
    here: &HashSet<&str>,
    inside: impl Fn(&Path) -> bool,
    now: SystemTime,
) -> Vec<AlsoWorked> {
    let elsewhere: Vec<&ChatIndexEntry> = entries
        .iter()
        .filter(|entry| !here.contains(entry.id.as_str()))
        .collect();
    coordinators(&elsewhere, entries, now)
        .into_iter()
        .filter(|coordinator| coordinator.recent)
        .filter_map(|coordinator| {
            let under = chats_under(entries, std::slice::from_ref(&coordinator.id));
            let edits_here = repos_touched(&under)
                .iter()
                .filter(|repo| inside(&repo.dir))
                .map(|repo| repo.edits)
                .sum::<u64>();
            (edits_here > 0).then_some(AlsoWorked {
                coordinator,
                edits_here,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Owned chats (TRU-169): a workspace that owns its chats, not a folder.

/// Which workspace owns a chat: the first of `owned` (each workspace's
/// root chat ids) that lists the chat itself or its root. A chat belongs
/// to at most one workspace, so the first match is the only one.
pub fn owning_workspace<'a>(
    owned: impl IntoIterator<Item = &'a [String]>,
    id: &str,
    root_id: Option<&str>,
) -> Option<usize> {
    owned.into_iter().position(|chats| {
        chats
            .iter()
            .any(|owned| owned == id || Some(owned.as_str()) == root_id)
    })
}

/// Puts root chat `root` in exactly one workspace's list: the one marked
/// `true` gains it (once), every other list loses it. Returns whether any
/// list changed.
pub fn assign_chat<'a>(
    lists: impl IntoIterator<Item = (bool, &'a mut Vec<String>)>,
    root: &str,
) -> bool {
    let mut changed = false;
    for (is_target, chats) in lists {
        if is_target {
            if !chats.iter().any(|id| id == root) {
                chats.push(root.to_string());
                changed = true;
            }
        } else {
            let before = chats.len();
            chats.retain(|id| id != root);
            changed |= chats.len() != before;
        }
    }
    changed
}

/// Where resuming a local chat lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeTarget {
    /// The open workspace at this index owns the chat.
    OwnerOpen(usize),
    /// The closed workspace config at this index owns the chat: reopen it.
    OwnerClosed(usize),
    /// The caller named a workspace (the overview opening in place).
    Into(usize),
    /// Unowned: the deepest open folder, else reopen or create one.
    ByFolder,
}

/// The resume target for a chat: its owning workspace wins (open, then
/// closed), then the caller's `into`, then the folder rule. Ownership
/// overrides the folder rule for owned chats only.
pub fn resume_target<'a>(
    open_owned: impl IntoIterator<Item = &'a [String]>,
    closed_owned: impl IntoIterator<Item = &'a [String]>,
    id: &str,
    root_id: Option<&str>,
    into: Option<usize>,
) -> ResumeTarget {
    if let Some(idx) = owning_workspace(open_owned, id, root_id) {
        return ResumeTarget::OwnerOpen(idx);
    }
    if let Some(idx) = owning_workspace(closed_owned, id, root_id) {
        return ResumeTarget::OwnerClosed(idx);
    }
    match into {
        Some(idx) => ResumeTarget::Into(idx),
        None => ResumeTarget::ByFolder,
    }
}

/// A workspace name from a chat title: whitespace trimmed and collapsed,
/// cut at [`WORKSPACE_NAME_MAX`] characters on a word boundary when one is
/// near; "Workspace" for an empty title.
pub fn workspace_name_from_title(title: &str) -> String {
    let words: Vec<&str> = title.split_whitespace().collect();
    let joined = words.join(" ");
    if joined.chars().count() <= WORKSPACE_NAME_MAX {
        return if joined.is_empty() {
            "Workspace".to_string()
        } else {
            joined
        };
    }
    let cut: String = joined.chars().take(WORKSPACE_NAME_MAX).collect();
    let at_word = cut
        .rfind(' ')
        .filter(|at| *at >= WORKSPACE_NAME_MAX / 2)
        .map(|at| cut[..at].to_string())
        .unwrap_or(cut);
    at_word.trim_end().to_string()
}

/// The longest workspace name made from a chat title; rename to taste.
pub const WORKSPACE_NAME_MAX: usize = 40;

/// The folder name for a new workspace called `name`: lowercase ASCII
/// letters and digits, every other run of characters one `-`, no leading
/// or trailing `-`; "workspace" when nothing is left.
pub fn work_folder_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "workspace".to_string()
    } else {
        slug.to_string()
    }
}

/// Which coordinators the repos, branches and issues are read for: the
/// interim answer until a workspace owns its chats (design step 4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Focus {
    /// The most recently active coordinator (the default).
    #[default]
    Latest,
    /// One coordinator, picked by hand.
    Coordinator(String),
    /// The union over the recent coordinators.
    AllRecent,
}

/// The index of the tab hosting chat `id` among a workspace's tabs, given
/// each tab's chat session id.
pub fn hosting_tab<'a>(
    tab_chat_ids: impl IntoIterator<Item = Option<&'a str>>,
    id: &str,
) -> Option<usize> {
    tab_chat_ids
        .into_iter()
        .position(|tab_id| tab_id == Some(id))
}

/// The root ids `focus` reads, from `coordinators` (sorted, most recently
/// active first). `Latest` takes the newest recent coordinator, else the
/// newest of all; a picked coordinator that left the list reads as
/// `Latest`.
pub fn focused_roots(focus: &Focus, coordinators: &[Coordinator]) -> Vec<String> {
    let latest = || {
        coordinators
            .iter()
            .find(|c| c.recent)
            .or_else(|| coordinators.first())
            .map(|c| vec![c.id.clone()])
            .unwrap_or_default()
    };
    match focus {
        Focus::Latest => latest(),
        Focus::Coordinator(id) => {
            if coordinators.iter().any(|c| &c.id == id) {
                vec![id.clone()]
            } else {
                latest()
            }
        }
        Focus::AllRecent => coordinators
            .iter()
            .filter(|c| c.recent)
            .map(|c| c.id.clone())
            .collect(),
    }
}

/// The chats under `roots`: the roots and every chat folded under them.
pub fn chats_under<'a>(entries: &'a [ChatIndexEntry], roots: &[String]) -> Vec<&'a ChatIndexEntry> {
    let roots: HashSet<&str> = roots.iter().map(String::as_str).collect();
    entries
        .iter()
        .filter(|entry| {
            roots.contains(entry.id.as_str())
                || entry
                    .root_id
                    .as_deref()
                    .is_some_and(|root| roots.contains(root))
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

/// The work's issue keys, most mentioned first. A key is kept only when
/// its prefix also appears in one of `names` (the work's owned branch and
/// worktree folder names): a prefix seen only in chat text never
/// qualifies. Zero-padded (`ARTIST-00`) numbers are dropped everywhere,
/// including as the source of a prefix; year-like ones (`ABC-2024`) are kept.
pub fn work_issue_keys(per_chat: &[&[String]], names: &[&str]) -> Vec<String> {
    let name_prefixes: HashSet<String> = names
        .iter()
        .flat_map(|name| chat_facts::issue_keys_in_name(name))
        .filter(|key| plausible_key(key))
        .filter_map(|key| key_prefix(&key).map(str::to_string))
        .collect();
    let mut key_count: HashMap<&str, usize> = HashMap::new();
    for keys in per_chat {
        for key in keys.iter() {
            *key_count.entry(key.as_str()).or_default() += 1;
        }
    }
    let mut kept: Vec<(&str, usize)> = key_count
        .into_iter()
        .filter(|(key, _)| {
            plausible_key(key) && key_prefix(key).is_some_and(|p| name_prefixes.contains(p))
        })
        .collect();
    kept.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    kept.into_iter().map(|(key, _)| key.to_string()).collect()
}

fn key_prefix(key: &str) -> Option<&str> {
    key.split_once('-').map(|(prefix, _)| prefix)
}

/// Not zero-padded (`-00`, `-05`). Year-like numbers (`ABC-2024`) are real.
fn plausible_key(key: &str) -> bool {
    key.split_once('-')
        .is_some_and(|(_, number)| !number.starts_with('0'))
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
    /// Branch as last seen by the facts or recorded by the task; its name
    /// (and the checkout folder's) qualify issue-key prefixes.
    pub branch: Option<String>,
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
            } else if linked && checkout.branch.is_some() {
                Ownership::WorkedIn
            } else {
                // A main checkout, or a detached worktree, only visited by
                // `cd` names nothing; a detached one needs edits or a task.
                continue;
            };
            add_candidate(
                &mut out,
                BranchCandidate {
                    repo: Some(checkout.repo.clone()),
                    checkout: checkout.dir.clone(),
                    branch: checkout.branch.clone(),
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
                branch: Some(task.branch.clone()),
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
    if known.branch.is_none() {
        known.branch = candidate.branch;
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
    /// Still listed by git but its folder is gone (a removed `/tmp`
    /// worktree not yet pruned): shown folded, never read.
    pub gone: bool,
}

/// Confirms candidates against `worktrees` (main checkout -> its list,
/// paths canonical) and adds rule (c): worktrees of `key_repos` whose
/// branch names one of `issue_keys`. A candidate whose checkout is no
/// longer a worktree is dropped, and so is a worktree only visited that is
/// detached now. Rows come out grouped by `key_repos` order (then other
/// repos by path), main checkout first, then by branch.
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
        if list[index].branch.is_none() && candidate.ownership == Ownership::WorkedIn {
            continue;
        }
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
            gone: false,
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
                gone: false,
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

#[doc(hidden)]
pub fn locate(list: &[WorktreeInfo], checkout: &Path) -> Option<usize> {
    list.iter().position(|worktree| worktree.path == checkout)
}

// ---------------------------------------------------------------------------
// The overview model.

/// Everything the panel shows that comes from the index and tasks, built
/// on the UI thread when the index changes or the panel opens (a pass over
/// the index, no I/O).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkSummary {
    /// Every coordinator of the workspace, most recently active first;
    /// `recent` ones are listed, the rest fold under "older".
    pub coordinators: Vec<Coordinator>,
    /// Recent coordinators of other workspaces that edited here.
    pub also_worked: Vec<AlsoWorked>,
    /// The focus asked for, and the roots it resolved to. Repos, branches
    /// and issues below are those of these roots and their children only.
    pub focus: Focus,
    pub focused: Vec<String>,
    pub repos: Vec<RepoTouched>,
    pub issue_keys: Vec<String>,
    pub candidates: Vec<BranchCandidate>,
    /// Repos whose worktrees rule (c) scans: repos touched, then repos of
    /// candidates that name one.
    pub key_repos: Vec<PathBuf>,
    pub chat_count: usize,
    /// Any of the work's chats has a facts file.
    pub has_facts: bool,
    /// Built for a workspace that owns its chats ([`WorkSummary::build_owned`]):
    /// every coordinator is one of its roots, all of them are read, and
    /// there is no focus to pick.
    pub owned: bool,
}

impl WorkSummary {
    pub fn build(
        entries: &[ChatIndexEntry],
        ws_dir: &Path,
        tasks: &[TaskInfo],
        focus: &Focus,
        now: SystemTime,
    ) -> Self {
        let all_work = work_chats(entries, ws_dir);
        let coordinators = coordinators(&all_work, entries, now);
        let also_worked = also_worked_here(entries, ws_dir, now);
        let focused = match focus {
            Focus::Coordinator(id)
                if !coordinators.iter().any(|c| &c.id == id)
                    && also_worked.iter().any(|a| &a.coordinator.id == id) =>
            {
                vec![id.clone()]
            }
            _ => focused_roots(focus, &coordinators),
        };
        let mut summary = Self::for_roots(entries, focused, tasks);
        summary.coordinators = coordinators;
        summary.also_worked = also_worked;
        summary.focus = focus.clone();
        summary.chat_count = all_work.len();
        summary.has_facts = all_work.iter().any(|entry| entry.facts.is_some());
        summary
    }

    /// The overview of a workspace that owns its chats: the owned roots
    /// and their children wherever they ran, every root listed (newest
    /// first) and read; repos, branches and issues from those chats only.
    /// "Also worked here": other recent coordinators that edited a repo
    /// the owned work touched.
    pub fn build_owned(
        entries: &[ChatIndexEntry],
        owned: &[String],
        tasks: &[TaskInfo],
        now: SystemTime,
    ) -> Self {
        let work = chats_under(entries, owned);
        let coordinators = coordinators(&work, entries, now);
        let here: HashSet<&str> = work.iter().map(|entry| entry.id.as_str()).collect();
        let focused: Vec<String> = coordinators.iter().map(|c| c.id.clone()).collect();
        let mut summary = Self::for_roots(entries, focused, tasks);
        let touched: Vec<PathBuf> = summary.repos.iter().map(|r| r.dir.clone()).collect();
        summary.also_worked = also_worked_outside(
            entries,
            &here,
            |dir| touched.iter().any(|repo| dir.starts_with(repo)),
            now,
        );
        summary.coordinators = coordinators;
        summary.chat_count = work.len();
        summary.has_facts = work.iter().any(|entry| entry.facts.is_some());
        summary.owned = true;
        summary
    }

    /// Repos, branch candidates, issue keys and key repos of `focused`
    /// and their children; the caller fills the coordinator lists.
    fn for_roots(entries: &[ChatIndexEntry], focused: Vec<String>, tasks: &[TaskInfo]) -> Self {
        let work = chats_under(entries, &focused);
        let repos = repos_touched(&work);
        let candidates = branch_candidates(&work, tasks);
        // Owned branch and worktree folder names qualify key prefixes.
        let folders: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.repo.as_ref() != Some(&candidate.checkout))
            .map(|candidate| folder_name(&candidate.checkout))
            .collect();
        let names: Vec<&str> = candidates
            .iter()
            .filter_map(|candidate| candidate.branch.as_deref())
            .chain(folders.iter().map(String::as_str))
            .collect();
        let per_chat: Vec<&[String]> = work
            .iter()
            .filter_map(|entry| entry.facts.as_ref())
            .map(|facts| facts.issue_keys.as_slice())
            .collect();
        let issue_keys = work_issue_keys(&per_chat, &names);
        let mut key_repos: Vec<PathBuf> = repos.iter().map(|repo| repo.dir.clone()).collect();
        for repo in candidates.iter().filter_map(|c| c.repo.as_ref()) {
            if !key_repos.contains(repo) {
                key_repos.push(repo.clone());
            }
        }
        Self {
            focused,
            repos,
            issue_keys,
            candidates,
            key_repos,
            ..Self::default()
        }
    }

    /// "Focus: <title>" copy for the header.
    pub fn focus_label(&self) -> String {
        match &self.focus {
            Focus::AllRecent => format!(
                "All recent work \u{00B7} {} coordinator{}",
                self.focused.len(),
                if self.focused.len() == 1 { "" } else { "s" }
            ),
            _ => self
                .focused
                .first()
                .and_then(|id| {
                    self.coordinators
                        .iter()
                        .chain(self.also_worked.iter().map(|a| &a.coordinator))
                        .find(|c| &c.id == id)
                })
                .map(|c| c.title.clone())
                .unwrap_or_else(|| "no chats".to_string()),
        }
    }

    pub fn recent_count(&self) -> usize {
        self.coordinators.iter().filter(|c| c.recent).count()
    }
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

/// mtime and length of the gitdir's `index`, `HEAD` and `logs/HEAD`.
/// Equal stamps mean no commit, checkout, stage or reset happened between
/// them (edits to files alone are not seen; the visible tier covers them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitdirStamp {
    entries: [Option<(SystemTime, u64)>; 3],
}

impl GitdirStamp {
    /// Stats collected by the host, in index, HEAD, logs/HEAD order.
    pub fn from_entries(entries: [Option<(SystemTime, u64)>; 3]) -> Self {
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    /// The latest of the three mtimes: when git last moved here.
    pub fn latest(&self) -> Option<SystemTime> {
        self.entries.iter().flatten().map(|(time, _)| *time).max()
    }
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

// ---------------------------------------------------------------------------
// Work sidebar lines (TRU-208): one line per thing, showing only what needs
// you; the rest waits for hover or a fold.

/// The one fact a branch's line shows. Clean and pushed shows nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchFact {
    /// Uncommitted changes: "3 dirty", "5000+ dirty".
    Dirty(String),
    /// Commits the upstream lacks: "↑2".
    Unpushed(u32),
}

impl BranchFact {
    pub fn label(&self) -> String {
        match self {
            BranchFact::Dirty(label) => label.clone(),
            BranchFact::Unpushed(n) => format!("\u{2191}{n}"),
        }
    }
}

/// What a branch's line says: dirty files first, else commits not pushed
/// to its upstream, else nothing (also while it was never read). The
/// upstream's absence and the rest wait for hover.
pub fn branch_fact(status: Option<&StatusCounts>) -> Option<BranchFact> {
    let status = status?;
    if status.files > 0 {
        return Some(BranchFact::Dirty(status.dirty_label()));
    }
    match status.push_state() {
        PushState::Unpushed(n) if n > 0 => Some(BranchFact::Unpushed(n)),
        _ => None,
    }
}

/// The right side of a repo's line: "home · 348 edits", or for a repo
/// with no branch rows the note that would have been a second line
/// ("home · no edits", "12 edits · no live checkout of this work").
pub fn repo_facts(home: bool, edits: Option<u64>, empty_note: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if home {
        parts.push("home".to_string());
    }
    match (edits, empty_note) {
        (Some(edits), note) => {
            parts.push(format!("{edits} edits"));
            parts.extend(note.map(str::to_string));
        }
        (None, Some(_)) => parts.push("no edits".to_string()),
        (None, None) => {}
    }
    parts.join(" \u{00B7} ")
}

/// "skills 2 · MCPs 1 · hooks 0", or `None` when all three are 0.
pub fn agent_config_line(skills: usize, mcps: usize, hooks: usize) -> Option<String> {
    (skills + mcps + hooks > 0)
        .then(|| format!("skills {skills} \u{00B7} MCPs {mcps} \u{00B7} hooks {hooks}"))
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
    fn issue_keys_need_a_prefix_on_an_owned_name() {
        let a = keys(&[
            "PRJ-101",
            "GPT-5",
            "SHA-1",
            "OPS-7",
            "BETA-2026",
            "ARTIST-00",
            "ABC-2024",
            "C8-10014",
            "RELEASE-7",
        ]);
        let b = keys(&["PRJ-101", "PRJ-102", "OPS-8", "LOCAL-2576"]);
        let per_chat: Vec<&[String]> = vec![&a, &b];
        // PRJ, OPS and ABC (a year-like number is a real key) name owned
        // branches or worktree folders; the rest are text only.
        let kept = work_issue_keys(
            &per_chat,
            &[
                "work/prj-101-standby",
                "api-server-ops-9",
                "abc-1-year",
                "main",
            ],
        );
        assert_eq!(kept, ["PRJ-101", "ABC-2024", "OPS-7", "OPS-8", "PRJ-102"]);
        assert!(work_issue_keys(&per_chat, &[]).is_empty());
        let padded = keys(&["PRJ-05"]);
        assert!(work_issue_keys(&[&padded], &["prj-101-x"]).is_empty());
        // Words after a name's key give no prefixes: chat text naming
        // STAGE-5 or PR-17 stays out (TRU-274).
        let text = keys(&["STAGE-5", "PR-17", "PRJ-9"]);
        assert_eq!(
            work_issue_keys(&[&text], &["PRJ-274-prj-274-stage-5-pr-17-delegation"]),
            ["PRJ-9"]
        );
    }

    #[test]
    fn focus_picks_the_latest_recent_coordinator_and_scopes_the_work() {
        let now = SystemTime::now();
        let ago = |hours: u64| now - Duration::from_secs(hours * 3600);
        let repo_facts = |dir: &str| ChatFacts {
            repos_touched: vec![RepoTouch {
                dir: dir.into(),
                edits: 1,
            }],
            ..ChatFacts::default()
        };
        let mut a = entry("a", "/w/x");
        a.mtime = ago(24);
        a.facts = Some(repo_facts("/w/infra"));
        let mut b = entry("b", "/w/y");
        b.mtime = ago(240);
        let mut b1 = entry("b1", "/w/y");
        b1.mtime = ago(2);
        b1.root_id = Some("b".into());
        b1.facts = Some(repo_facts("/w/api"));
        let mut c = entry("c", "/w/z");
        c.mtime = ago(480);
        c.facts = Some(repo_facts("/w/old"));
        let entries = vec![b1, a, b, c];

        let latest = WorkSummary::build(&entries, Path::new("/w"), &[], &Focus::Latest, now);
        let order: Vec<(&str, bool)> = latest
            .coordinators
            .iter()
            .map(|c| (c.id.as_str(), c.recent))
            .collect();
        assert_eq!(order, [("b", true), ("a", true), ("c", false)]);
        assert_eq!(latest.focused, ["b"]);
        let dirs =
            |s: &WorkSummary| -> Vec<PathBuf> { s.repos.iter().map(|r| r.dir.clone()).collect() };
        assert_eq!(dirs(&latest), [PathBuf::from("/w/api")]);

        let all = WorkSummary::build(&entries, Path::new("/w"), &[], &Focus::AllRecent, now);
        assert_eq!(all.focused, ["b", "a"]);
        assert_eq!(
            dirs(&all),
            [PathBuf::from("/w/api"), PathBuf::from("/w/infra")],
            "older coordinators stay out of the union"
        );
        let picked = WorkSummary::build(
            &entries,
            Path::new("/w"),
            &[],
            &Focus::Coordinator("c".into()),
            now,
        );
        assert_eq!(dirs(&picked), [PathBuf::from("/w/old")]);
        let gone = focused_roots(&Focus::Coordinator("nope".into()), &latest.coordinators);
        assert_eq!(gone, ["b"]);
        assert_eq!(latest.recent_count(), 2);
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
                // Detached worktree only visited: not owned.
                CheckoutTouch {
                    branch: None,
                    ..touch(
                        "/r/infra",
                        "/tmp/infra-review",
                        "x",
                        0,
                        "2026-10-02T00:00:00Z",
                    )
                },
                // Detached worktree edited: owned.
                CheckoutTouch {
                    branch: None,
                    ..touch(
                        "/r/infra",
                        "/tmp/infra-hotfix",
                        "x",
                        1,
                        "2026-10-02T00:00:00Z",
                    )
                },
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
                ("/tmp/infra-hotfix", &Ownership::Edited, "coordinator"),
                ("/cfg/worktrees/api/t1", &Ownership::Task, "worker PRJ-2"),
            ]
        );
        let task_row = &found[3];
        assert_eq!(task_row.edits, 4);
        assert_eq!(task_row.repo.as_deref(), Some(Path::new("/r/api")));
        assert_eq!(task_row.branch.as_deref(), Some("prj-2-x"));
        assert_eq!(task_row.task.as_ref().unwrap().base, "develop");
        assert_eq!(task_row.last.as_deref(), Some("2026-10-03T00:00:00Z"));
    }

    #[test]
    fn resolve_drops_gone_checkouts_and_adds_key_matches_only() {
        let wt = |path: &str, branch: Option<&str>| WorktreeInfo {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
        };
        let mut lists = HashMap::new();
        lists.insert(
            PathBuf::from("/r/api"),
            vec![
                wt("/r/api", Some("main")),
                wt("/r/api-prj-101", Some("work/prj-101-standby")),
                wt("/r/api-prj-1010", Some("prj-1010-other")),
                wt("/r/api-ops-9", Some("ops-9-unrelated")),
                wt("/r/api-mine", Some("renamed-branch")),
                wt("/r/api-detached", None),
            ],
        );
        let candidate = |checkout: &str, ownership: Ownership, edits: u64| BranchCandidate {
            repo: Some("/r/api".into()),
            checkout: checkout.into(),
            branch: Some("old-name".into()),
            ownership,
            owner: "coordinator".into(),
            last: None,
            edits,
            task: None,
        };
        let candidates = vec![
            candidate("/r/api-mine", Ownership::Edited, 3),
            candidate("/r/api-removed", Ownership::Edited, 1),
            // Visited while on a branch, detached now: dropped.
            candidate("/r/api-detached", Ownership::WorkedIn, 0),
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
    fn also_worked_here_lists_recent_outside_coordinators_with_edits_inside() {
        let now = SystemTime::now();
        let edit = |dir: &str, edits: u64| ChatFacts {
            repos_touched: vec![RepoTouch {
                dir: dir.into(),
                edits,
            }],
            ..ChatFacts::default()
        };
        // Home is another workspace, edited a repo here: listed.
        let mut away = entry("away", "/other/app");
        away.mtime = now;
        away.facts = Some(edit("/w/api", 3));
        // Same, but older than the recent window: not listed.
        let mut stale = entry("stale", "/other/app");
        stale.mtime = now - RECENT_WINDOW - Duration::from_secs(60);
        stale.facts = Some(edit("/w/api", 3));
        // Outside, edits only elsewhere or zero here: not listed.
        let mut elsewhere = entry("elsewhere", "/other/app");
        elsewhere.mtime = now;
        elsewhere.facts = Some(edit("/other/lib", 9));
        let mut zero = entry("zero", "/other/app");
        zero.mtime = now;
        zero.facts = Some(edit("/w/api", 0));
        // Lives here: it is a coordinator of this workspace, not "also".
        let mut home = entry("home", "/w/api");
        home.mtime = now;
        home.facts = Some(edit("/w/api", 4));
        let entries = vec![away, stale, elsewhere, zero, home];
        let also = also_worked_here(&entries, Path::new("/w"), now);
        assert_eq!(also.len(), 1);
        assert_eq!(also[0].coordinator.id, "away");
        assert_eq!(also[0].edits_here, 3);
        // A pick of such a coordinator is honoured, not replaced by Latest.
        let summary = WorkSummary::build(
            &entries,
            Path::new("/w"),
            &[],
            &Focus::Coordinator("away".into()),
            now,
        );
        assert_eq!(summary.focused, vec!["away".to_string()]);
    }

    #[test]
    fn owning_workspace_overview_reads_owned_roots_and_children_only() {
        let now = SystemTime::now();
        let ago = |hours: u64| now - Duration::from_secs(hours * 3600);
        let edit = |dir: &str, edits: u64| ChatFacts {
            repos_touched: vec![RepoTouch {
                dir: dir.into(),
                edits,
            }],
            ..ChatFacts::default()
        };
        // The coordinator started in a folder of repos; its worker ran in a
        // task worktree elsewhere.
        let mut root = entry("root", "/w/acme");
        root.title = "Cloud migration".into();
        root.mtime = ago(30);
        root.facts = Some(edit("/w/acme/infra", 4));
        let mut worker = entry("worker", "/tasks/api-server-prj-101");
        worker.root_id = Some("root".into());
        worker.mtime = ago(1);
        worker.facts = Some(edit("/w/acme/api-server", 2));
        // A second owned root, older than the recent window: still listed.
        let mut second = entry("second", "/elsewhere/notes");
        second.mtime = ago(24 * 20);
        // Same folder, not owned: never part of this workspace.
        let mut unrelated = entry("unrelated", "/w/acme");
        unrelated.mtime = ago(2);
        unrelated.facts = Some(edit("/w/acme/billing", 9));
        // Another workspace's recent coordinator that edited a touched repo.
        let mut visitor = entry("visitor", "/other/app");
        visitor.mtime = ago(3);
        visitor.facts = Some(edit("/w/acme/infra", 1));
        let entries = vec![worker, unrelated, visitor, root, second];
        let owned = keys(&["second", "root"]);

        let summary = WorkSummary::build_owned(&entries, &owned, &[], now);
        assert!(summary.owned);
        let ids: Vec<&str> = summary.coordinators.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["root", "second"], "every owned root, newest first");
        assert_eq!(summary.coordinators[0].children.len(), 1);
        assert_eq!(summary.coordinators[0].children[0].id, "worker");
        assert_eq!(summary.focused, ["root", "second"]);
        assert_eq!(summary.chat_count, 3);
        let dirs: Vec<&Path> = summary.repos.iter().map(|r| r.dir.as_path()).collect();
        assert_eq!(
            dirs,
            [Path::new("/w/acme/infra"), Path::new("/w/acme/api-server")],
            "repos from the owned chats only, the folder's other chat excluded"
        );
        let also: Vec<&str> = summary
            .also_worked
            .iter()
            .map(|a| a.coordinator.id.as_str())
            .collect();
        assert_eq!(
            also,
            ["visitor"],
            "only coordinators that edited a touched repo"
        );
        assert_eq!(summary.also_worked[0].edits_here, 1);

        // The folder view of the same dir is unchanged by ownership.
        let folder =
            WorkSummary::build(&entries, Path::new("/w/acme"), &[], &Focus::AllRecent, now);
        assert!(!folder.owned);
        assert!(folder.coordinators.iter().any(|c| c.id == "unrelated"));
    }

    #[test]
    fn a_chat_belongs_to_exactly_one_workspace() {
        let mut a = keys(&["root-1", "root-2"]);
        let mut b = keys(&["root-3"]);
        let mut closed = keys(&["root-2"]);
        // Adding root-2 to B takes it from A and from a closed workspace.
        let changed = assign_chat(
            [(false, &mut a), (true, &mut b), (false, &mut closed)],
            "root-2",
        );
        assert!(changed);
        assert_eq!(a, ["root-1"]);
        assert_eq!(b, ["root-3", "root-2"]);
        assert!(closed.is_empty());
        // Adding it again changes nothing and never duplicates.
        assert!(!assign_chat([(false, &mut a), (true, &mut b)], "root-2"));
        assert_eq!(b, ["root-3", "root-2"]);
        let lists: [&[String]; 2] = [&a, &b];
        assert_eq!(owning_workspace(lists, "root-2", None), Some(1));
        // A child is owned through its root.
        assert_eq!(owning_workspace(lists, "worker", Some("root-1")), Some(0));
        assert_eq!(owning_workspace(lists, "stray", None), None);
    }

    #[test]
    fn owned_chats_resume_in_their_owner_and_others_by_folder() {
        let open_a = keys(&[]);
        let open_b = keys(&["root-1"]);
        let closed_c = keys(&["root-9"]);
        let open: [&[String]; 2] = [&open_a, &open_b];
        let closed: [&[String]; 1] = [&closed_c];
        let target = |id: &str, root: Option<&str>, into: Option<usize>| {
            resume_target(open, closed, id, root, into)
        };
        assert_eq!(target("root-1", None, None), ResumeTarget::OwnerOpen(1));
        // The owner wins over the caller's workspace and the folder rule.
        assert_eq!(target("root-1", None, Some(0)), ResumeTarget::OwnerOpen(1));
        assert_eq!(
            target("worker", Some("root-1"), None),
            ResumeTarget::OwnerOpen(1)
        );
        assert_eq!(
            target("root-9", None, Some(0)),
            ResumeTarget::OwnerClosed(0)
        );
        // Unowned: the caller's workspace, else the folder rule.
        assert_eq!(target("free", None, Some(0)), ResumeTarget::Into(0));
        assert_eq!(target("free", None, None), ResumeTarget::ByFolder);
    }
}
