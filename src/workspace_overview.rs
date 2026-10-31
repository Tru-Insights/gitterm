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
//! - (a) facts checkouts with edits, or any linked worktree on a branch a
//!   chat worked in; a main checkout or a detached worktree only visited by
//!   `cd` never counts;
//! - (b) task worktrees the chats created (facts children with a task id,
//!   which `chat_facts::resolve_tree` fills from delegations and tasks);
//! - (c) worktrees in the work's repos whose branch names one of its issue
//!   keys (keys whose prefix an owned branch or worktree name carries).
//!
//! Nothing else in a shared repo appears. Repos, branches and keys are read
//! for a [`Focus`]: the most recently active coordinator by default, or the
//! union over the last 7 days. Polling (decisions 16, 18): only owned
//! checkouts, one `git status --porcelain=v2 --branch` and one
//! `for-each-ref` measuring every base candidate each, in a background task
//! with timeouts; a gated refresh first compares a stat of the checkout's
//! gitdir `index`, `HEAD` and `logs/HEAD` and runs no git when nothing
//! moved. A checkout whose folder is gone gets no git. Status output is
//! counted, never stored, and capped.

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
/// Integration branch names tried as a base, remote then local, in this
/// order on a tie.
const INTEGRATION_BRANCHES: [&str; 4] = ["develop", "main", "master", "trunk"];
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

/// Creates the home folder of a new workspace called `name` under `base`
/// (made when missing): `<base>/<slug>`, else `<slug>-2`, `<slug>-3`, … for
/// the first name nothing holds yet. Returns the created folder.
pub fn create_work_folder(base: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(base)?;
    let slug = work_folder_slug(name);
    for n in 1u32.. {
        let dir = if n == 1 {
            base.join(&slug)
        } else {
            base.join(format!("{slug}-{n}"))
        };
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("create {}: {error}", dir.display()),
                ))
            }
        }
    }
    unreachable!("u32 suffixes exhausted under {}", base.display())
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

/// A focus the user picked by hand. `Latest` is the automatic default and
/// is never stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedFocus {
    All,
    Coordinator(String),
}

/// The focus picks per workspace directory, kept across restarts in
/// `overview-focus.json` in the config dir. A missing or unreadable file
/// reads as no picks.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FocusPicks(std::collections::BTreeMap<String, PersistedFocus>);

impl FocusPicks {
    /// The file inside the config directory `dir`.
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("overview-focus.json")
    }

    pub fn parse(json: &str) -> Self {
        match serde_json::from_str(json) {
            Ok(picks) => picks,
            Err(error) => {
                eprintln!("[workspace-overview] ignoring unreadable focus picks: {error}");
                Self::default()
            }
        }
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(json) => Self::parse(&json),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!(
                    "[workspace-overview] cannot read focus picks {}: {error}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// The stored pick for `dir`, or the default.
    pub fn focus_for(&self, dir: &Path) -> Focus {
        match self.0.get(&dir.to_string_lossy().into_owned()) {
            Some(PersistedFocus::All) => Focus::AllRecent,
            Some(PersistedFocus::Coordinator(id)) => Focus::Coordinator(id.clone()),
            None => Focus::Latest,
        }
    }

    /// Records `focus` for `dir`; `Latest` clears the pick. Returns whether
    /// anything changed.
    pub fn set(&mut self, dir: &Path, focus: &Focus) -> bool {
        let before = self.stored(dir);
        let key = dir.to_string_lossy().into_owned();
        match focus {
            Focus::Latest => {
                self.0.remove(&key);
            }
            Focus::AllRecent => {
                self.0.insert(key, PersistedFocus::All);
            }
            Focus::Coordinator(id) => {
                self.0.insert(key, PersistedFocus::Coordinator(id.clone()));
            }
        }
        before != self.stored(dir)
    }

    fn stored(&self, dir: &Path) -> Option<PersistedFocus> {
        self.0.get(&dir.to_string_lossy().into_owned()).cloned()
    }

    /// Writes the picks to `path` through a temporary file and an atomic
    /// replace. Blocking: run it off the UI thread.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let context = |what: &str, error: &dyn std::fmt::Display| {
            format!("{what} {}: {error}", path.display())
        };
        let json = serde_json::to_vec_pretty(self).map_err(|e| context("encode", &e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| context("create the directory of", &e))?;
        }
        let temporary = path.with_extension(format!(
            "json.tmp-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&json)?;
            file.sync_all()?;
            crate::tasks::replace_file_atomically(&temporary, path)
        })();
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(context("atomically write", &error));
        }
        Ok(())
    }
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
/// qualifies. Year-like (`BETA-2026`) and zero-padded (`ARTIST-00`) numbers
/// are dropped everywhere, including as the source of a prefix.
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

/// Not a year (`-1990`..`-2099`) and not zero-padded (`-00`, `-05`).
fn plausible_key(key: &str) -> bool {
    let Some((_, number)) = key.split_once('-') else {
        return false;
    };
    let year_like = number.len() == 4 && (number.starts_with("19") || number.starts_with("20"));
    !number.starts_with('0') && !year_like
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
// Base selection.
//
// A repo's integration branch is not always `origin/HEAD` (one works off
// `develop` while `main` sits far behind), so every plausible base is
// measured and the one with the nearest merge-base wins: the fewest
// commits on HEAD that it lacks. The row names the winner and where it
// came from, so the comparison is never a guess.

/// Where a row's base came from; shown on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    /// The task's recorded base branch.
    Task,
    /// The branch's tracking ref, when it is another branch.
    Tracking,
    /// `origin/develop`, `origin/main`, `origin/master`, `origin/trunk`.
    Remote,
    /// The local forms of those.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseChoice {
    /// What git is given: `origin/develop`, `develop`, a task's base.
    pub reference: String,
    pub source: BaseSource,
    /// Candidates that exist here; the base is the nearest of them.
    pub considered: usize,
}

impl BaseChoice {
    pub fn source_label(&self) -> &'static str {
        match self.source {
            BaseSource::Task => "task base",
            BaseSource::Tracking => "tracked branch",
            BaseSource::Remote => "remote",
            BaseSource::Local => "local",
        }
    }

    /// "task base, nearest of 4".
    pub fn describe(&self) -> String {
        if self.considered > 1 {
            format!("{}, nearest of {}", self.source_label(), self.considered)
        } else {
            self.source_label().to_string()
        }
    }
}

/// A ref that may be a row's base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseCandidate {
    /// Full ref name measured (`refs/remotes/origin/develop`), or a raw
    /// revision for a task base that is not a ref.
    pub refname: String,
    pub reference: String,
    pub source: BaseSource,
}

/// The candidates, in tie-break order: the task base, the tracking ref when
/// it names another branch than `branch`, then `origin/<integration>` and
/// the local `<integration>` branches. The checked-out branch itself is
/// never its own base.
pub fn base_candidates(
    task_base: Option<&str>,
    branch: Option<&str>,
    upstream: Option<&str>,
) -> Vec<BaseCandidate> {
    fn push(out: &mut Vec<BaseCandidate>, refname: String, reference: &str, source: BaseSource) {
        if !out.iter().any(|known| known.refname == refname) {
            out.push(BaseCandidate {
                refname,
                reference: reference.to_string(),
                source,
            });
        }
    }
    let mut out = Vec::new();
    if let Some(base) = task_base.map(str::trim).filter(|base| !base.is_empty()) {
        if base.starts_with("refs/") {
            push(&mut out, base.to_string(), base, BaseSource::Task);
        } else {
            push(
                &mut out,
                format!("refs/heads/{base}"),
                base,
                BaseSource::Task,
            );
            push(
                &mut out,
                format!("refs/remotes/{base}"),
                base,
                BaseSource::Task,
            );
        }
    }
    if let Some(upstream) = upstream.map(str::trim).filter(|u| !u.is_empty()) {
        let tail = upstream.split_once('/').map(|(_, rest)| rest);
        let same = Some(upstream) == branch || (tail.is_some() && tail == branch);
        if !same {
            push(
                &mut out,
                format!("refs/remotes/{upstream}"),
                upstream,
                BaseSource::Tracking,
            );
            push(
                &mut out,
                format!("refs/heads/{upstream}"),
                upstream,
                BaseSource::Tracking,
            );
        }
    }
    for name in INTEGRATION_BRANCHES {
        push(
            &mut out,
            format!("refs/remotes/origin/{name}"),
            &format!("origin/{name}"),
            BaseSource::Remote,
        );
    }
    for name in INTEGRATION_BRANCHES {
        push(
            &mut out,
            format!("refs/heads/{name}"),
            name,
            BaseSource::Local,
        );
    }
    if let Some(branch) = branch {
        let own = format!("refs/heads/{branch}");
        out.retain(|candidate| candidate.refname != own);
    }
    out
}

/// One `for-each-ref` measuring every candidate ref against HEAD.
pub fn ahead_behind_args(candidates: &[BaseCandidate]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--no-optional-locks",
        "for-each-ref",
        "--format=%(refname)%09%(ahead-behind:HEAD)",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    args.extend(
        candidates
            .iter()
            .filter(|candidate| candidate.refname.starts_with("refs/"))
            .map(|candidate| candidate.refname.clone()),
    );
    args
}

/// Parses `%(refname)%09%(ahead-behind:HEAD)` lines into refname ->
/// `(behind, ahead)` of HEAD against that ref (the atom prints commits only
/// in the ref, then commits only in HEAD).
pub fn parse_ahead_behind(output: &str) -> HashMap<String, (u32, u32)> {
    output
        .lines()
        .filter_map(|line| {
            let (name, counts) = line.split_once('\t')?;
            Some((name.to_string(), parse_left_right(counts)?))
        })
        .collect()
}

/// The measured candidate with the nearest merge-base (fewest commits
/// ahead of it); ties go to candidate order. With its `(behind, ahead)`.
pub fn choose_nearest(
    candidates: &[BaseCandidate],
    measured: &HashMap<String, (u32, u32)>,
) -> Option<(BaseChoice, (u32, u32))> {
    let present: Vec<(&BaseCandidate, (u32, u32))> = candidates
        .iter()
        .filter_map(|candidate| measured.get(&candidate.refname).map(|c| (candidate, *c)))
        .collect();
    let considered = present
        .iter()
        .map(|(candidate, _)| candidate.reference.as_str())
        .collect::<HashSet<_>>()
        .len();
    let mut best: Option<(&BaseCandidate, (u32, u32))> = None;
    for (candidate, counts) in present {
        if best.is_none_or(|(_, (_, ahead))| counts.1 < ahead) {
            best = Some((candidate, counts));
        }
    }
    best.map(|(candidate, counts)| {
        (
            BaseChoice {
                reference: candidate.reference.clone(),
                source: candidate.source,
                considered,
            },
            counts,
        )
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
/// new call after [`REFRESH_DEADLINE`]. A checkout whose folder is gone
/// gets no git call at all. Never call on the UI thread.
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
    let owned: Vec<OwnedBranch> = match plan.mode {
        RefreshMode::Gated => plan.previous.iter().map(|row| row.owned.clone()).collect(),
        RefreshMode::Full => discover(&plan, &mut outcome, started).await,
    };
    for mut owned in owned {
        owned.gone = !owned.checkout.is_dir();
        if owned.gone {
            outcome.branches.push(BranchStatus {
                owned,
                base: None,
                status: None,
                vs_base: None,
                error: None,
                stamp: GitdirStamp::default(),
                checked_at: SystemTime::now(),
            });
            continue;
        }
        let previous = plan
            .previous
            .iter()
            .find(|row| row.owned.checkout == owned.checkout && !row.owned.gone);
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
                base: None,
                status: None,
                vs_base: None,
                error: Some("not read: refresh time limit reached".to_string()),
                stamp,
                checked_at: SystemTime::now(),
            });
            continue;
        }
        let row = read_checkout(owned, stamp, &mut outcome.git_runs).await;
        outcome.branches.push(row);
    }
    outcome.took = started.elapsed();
    outcome
}

/// Worktree lists per repo, then rule (a)/(b)/(c) resolution.
async fn discover(
    plan: &RefreshPlan,
    outcome: &mut RefreshOutcome,
    started: Instant,
) -> Vec<OwnedBranch> {
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
    resolve_owned(&candidates, &plan.issue_keys, &key_repos, &lists)
}

/// Status, then every base candidate measured against HEAD in one
/// `for-each-ref` (git 2.41+ `%(ahead-behind)`), the nearest kept.
async fn read_checkout(
    owned: OwnedBranch,
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
    let task_base = owned.task.as_ref().map(|task| task.base.as_str());
    let branch = status
        .as_ref()
        .and_then(|status| status.head.as_deref())
        .or(owned.branch.as_deref());
    let upstream = status
        .as_ref()
        .and_then(|status| status.upstream.as_deref());
    let mut candidates = base_candidates(task_base, branch, upstream);
    let measured = match measure_bases(&owned.checkout, &mut candidates, git_runs).await {
        Ok(measured) => measured,
        Err(error) => {
            errors.push(error);
            HashMap::new()
        }
    };
    let (base, vs_base) = match choose_nearest(&candidates, &measured) {
        Some((base, counts)) => (Some(base), Some(counts)),
        None => (None, None),
    };
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

/// `(behind, ahead)` per candidate ref that exists. One `for-each-ref`;
/// on a git without `%(ahead-behind)`, one listing plus a `rev-list` per
/// existing candidate. A task base that is no ref (a commit) is measured
/// as a revision and added to `candidates`.
async fn measure_bases(
    checkout: &Path,
    candidates: &mut Vec<BaseCandidate>,
    git_runs: &mut usize,
) -> Result<HashMap<String, (u32, u32)>, String> {
    let args = ahead_behind_args(candidates);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    *git_runs += 1;
    let mut measured = match git_text(checkout, &args, GIT_TIMEOUT).await {
        Ok(text) => parse_ahead_behind(&text),
        Err(_) => {
            // Older git: find which candidates exist, measure each.
            let mut list_args: Vec<&str> =
                vec!["--no-optional-locks", "for-each-ref", "--format=%(refname)"];
            list_args.extend(args[3..].iter().copied());
            *git_runs += 1;
            let existing = git_text(checkout, &list_args, GIT_TIMEOUT).await?;
            let mut measured = HashMap::new();
            for refname in existing.lines().filter(|name| args[3..].contains(name)) {
                *git_runs += 1;
                if let Some(counts) = rev_list_counts(checkout, refname).await? {
                    measured.insert(refname.to_string(), counts);
                }
            }
            measured
        }
    };
    let task_measured = candidates
        .iter()
        .any(|c| c.source == BaseSource::Task && measured.contains_key(&c.refname));
    let raw_task = candidates
        .iter()
        .find(|c| c.source == BaseSource::Task)
        .map(|c| c.reference.clone());
    if let (false, Some(base)) = (task_measured, raw_task) {
        *git_runs += 1;
        if let Ok(Some(counts)) = rev_list_counts(checkout, &base).await {
            candidates.insert(
                0,
                BaseCandidate {
                    refname: base.clone(),
                    reference: base.clone(),
                    source: BaseSource::Task,
                },
            );
            measured.insert(base, counts);
        }
    }
    Ok(measured)
}

/// `git rev-list --left-right --count <rev>...HEAD`; `Ok(None)` when the
/// output is not two counts.
async fn rev_list_counts(checkout: &Path, rev: &str) -> Result<Option<(u32, u32)>, String> {
    let range = format!("{rev}...HEAD");
    let text = git_text(
        checkout,
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
    .await?;
    Ok(parse_left_right(&text))
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
                "no base branch found for {}: no task base, tracked branch, or develop/main/master/trunk",
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
    fn issue_keys_need_a_prefix_on_an_owned_name() {
        let a = keys(&[
            "PRJ-101",
            "GPT-5",
            "SHA-1",
            "OPS-7",
            "BETA-2026",
            "ARTIST-00",
            "C8-10014",
            "RELEASE-7",
        ]);
        let b = keys(&["PRJ-101", "PRJ-102", "OPS-8", "LOCAL-2576"]);
        let per_chat: Vec<&[String]> = vec![&a, &b];
        // PRJ and OPS name owned branches or worktree folders; RELEASE only
        // through a year, which never counts; the rest are text only.
        let kept = work_issue_keys(
            &per_chat,
            &[
                "work/prj-101-standby",
                "api-server-ops-9",
                "release-2026",
                "main",
            ],
        );
        assert_eq!(kept, ["PRJ-101", "OPS-7", "OPS-8", "PRJ-102"]);
        assert!(work_issue_keys(&per_chat, &[]).is_empty());
        let padded = keys(&["PRJ-05"]);
        assert!(work_issue_keys(&[&padded], &["prj-101-x"]).is_empty());
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
    fn base_is_the_nearest_candidate() {
        let refnames = |list: &[BaseCandidate]| -> Vec<String> {
            list.iter().map(|c| c.refname.clone()).collect()
        };
        // A feature branch pushed to its own name: no tracking candidate.
        let feature = base_candidates(Some("release"), Some("feature-x"), Some("origin/feature-x"));
        assert_eq!(
            refnames(&feature),
            [
                "refs/heads/release",
                "refs/remotes/release",
                "refs/remotes/origin/develop",
                "refs/remotes/origin/main",
                "refs/remotes/origin/master",
                "refs/remotes/origin/trunk",
                "refs/heads/develop",
                "refs/heads/main",
                "refs/heads/master",
                "refs/heads/trunk",
            ]
        );
        // The branch is never its own base; its remote form still counts.
        let develop = base_candidates(None, Some("develop"), Some("origin/develop"));
        assert!(!refnames(&develop).contains(&"refs/heads/develop".to_string()));
        assert!(refnames(&develop).contains(&"refs/remotes/origin/develop".to_string()));
        // Tracking another branch puts it first.
        let tracking = base_candidates(None, Some("feature"), Some("origin/develop"));
        assert_eq!(tracking[0].source, BaseSource::Tracking);
        assert_eq!(tracking[0].reference, "origin/develop");

        let measured = parse_ahead_behind(
            "refs/remotes/origin/main\t0 762\n\
             refs/heads/develop\t3 2\n\
             refs/heads/main\t0 762\n\
             junk line\n",
        );
        let (base, counts) = choose_nearest(&feature, &measured).unwrap();
        assert_eq!(
            (
                base.reference.as_str(),
                base.source,
                base.considered,
                counts
            ),
            ("develop", BaseSource::Local, 3, (3, 2))
        );
        assert_eq!(base.describe(), "local, nearest of 3");
        // A tie goes to candidate order (the remote form first).
        let tie = HashMap::from([
            ("refs/heads/develop".to_string(), (0, 2)),
            ("refs/remotes/origin/develop".to_string(), (0, 2)),
        ]);
        let (base, _) = choose_nearest(&feature, &tie).unwrap();
        assert_eq!(base.reference, "origin/develop");
        assert_eq!(choose_nearest(&feature, &HashMap::new()), None);
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
        // The owned worktree's folder names PRJ, which qualifies the
        // chat's PRJ-101 mention for rule (c).
        let owned_wt = root.join("api-server-prj-100-edit");
        let unrelated_wt = root.join("api-server-ops-9");
        let keyed_wt = root.join("api-server-prj-101");
        let gone_wt = root.join("api-server-prj-101-old");
        for (path, branch) in [
            (&owned_wt, "feature-standby"),
            (&unrelated_wt, "ops-9-unrelated"),
            (&keyed_wt, "work/prj-101-cloud"),
            (&gone_wt, "work/prj-101-old"),
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
        // Removed by hand, never pruned: git still lists it.
        std::fs::remove_dir_all(&gone_wt).unwrap();

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
        let summary = WorkSummary::build(&entries, &root, &[], &Focus::Latest, SystemTime::now());
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
                (gone_wt.as_path(), Some("work/prj-101-old")),
            ],
            "the unrelated worktree must not appear"
        );
        let gone = &outcome.branches[2];
        assert!(gone.owned.gone);
        assert_eq!((&gone.status, &gone.error), (&None, &None));
        let owned = &outcome.branches[0];
        assert_eq!(owned.error, None);
        let base = owned.base.as_ref().unwrap();
        assert_eq!(
            (base.reference.as_str(), base.source),
            ("main", BaseSource::Local)
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
    async fn real_git_develop_is_the_base_when_main_is_far_behind() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = worktree_follow::resolve_for_match(temp.path());
        let repo = root.join("portal");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["checkout", "-q", "-b", "develop"]);
        for i in 0..30 {
            git(
                &repo,
                &["commit", "-q", "--allow-empty", "-m", &format!("d{i}")],
            );
        }
        let feature = root.join("portal-prj-7");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "prj-7-upload",
                feature.to_str().unwrap(),
                "develop",
            ],
        );
        git(&feature, &["commit", "-q", "--allow-empty", "-m", "f1"]);
        git(&feature, &["commit", "-q", "--allow-empty", "-m", "f2"]);
        let owned = OwnedBranch {
            repo: repo.clone(),
            checkout: feature.clone(),
            branch: Some("prj-7-upload".into()),
            is_main: false,
            ownership: Ownership::Edited,
            owner: "coordinator".into(),
            last: None,
            edits: 1,
            task: None,
            gone: false,
        };
        let mut runs = 0;
        let row = read_checkout(owned.clone(), GitdirStamp::default(), &mut runs).await;
        assert_eq!(row.error, None);
        let base = row.base.as_ref().unwrap();
        assert_eq!(
            (base.reference.as_str(), base.source, base.considered),
            ("develop", BaseSource::Local, 2)
        );
        assert_eq!(row.vs_base, Some((0, 2)), "not 32 ahead of main");
        assert_eq!(runs, 2, "status plus one for-each-ref");

        // A task base that is a commit, not a ref, is measured as one.
        let main_commit = git_text(&repo, &["rev-parse", "main"], GIT_TIMEOUT)
            .await
            .unwrap();
        let task_owned = OwnedBranch {
            task: Some(TaskLink {
                task_id: "t1".into(),
                base: main_commit.trim().to_string(),
                pr_number: None,
            }),
            ..owned
        };
        let row = read_checkout(task_owned, GitdirStamp::default(), &mut runs).await;
        let base = row.base.as_ref().unwrap();
        assert_eq!(base.source, BaseSource::Local, "develop is still nearer");
        assert_eq!(base.considered, 3);
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
        // Optional: GITTERM_REAL_FOCUS=all, or a coordinator's session id.
        let focus = match std::env::var("GITTERM_REAL_FOCUS").ok().as_deref() {
            None | Some("") | Some("latest") => Focus::Latest,
            Some("all") => Focus::AllRecent,
            Some(id) => Focus::Coordinator(id.to_string()),
        };
        let now = SystemTime::now();
        let built = Instant::now();
        let summary = WorkSummary::build(&entries, &ws_dir, &tasks, &focus, now);
        println!(
            "summary in {}ms: {} chats, facts: {}",
            built.elapsed().as_millis(),
            summary.chat_count,
            summary.has_facts
        );
        let union = WorkSummary::build(&entries, &ws_dir, &tasks, &Focus::AllRecent, now);
        println!(
            "all recent work: {} coordinators, {} repos, {} candidates, {} keys",
            union.focused.len(),
            union.repos.len(),
            union.candidates.len(),
            union.issue_keys.len()
        );
        println!(
            "\nCOORDINATORS · {} recent · {} older",
            summary.recent_count(),
            summary.coordinators.len() - summary.recent_count()
        );
        println!("FOCUS · {:?} · {}", summary.focus, summary.focus_label());
        for coordinator in summary.coordinators.iter().filter(|c| c.recent).take(15) {
            let mark = if summary.focused.contains(&coordinator.id) {
                "\u{25B8}"
            } else {
                " "
            };
            println!(
                "{mark} {} · {} · {}",
                shorten(&coordinator.title, 70),
                chats::format_age(coordinator.active),
                coordinator.id
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
            if row.owned.gone {
                println!(
                    "    (folder gone) {}",
                    row.owned.branch.as_deref().unwrap_or("(detached)")
                );
                continue;
            }
            let mut parts = vec![short_checkout(&row.owned, home.as_deref())];
            parts.extend(vs_base_label(row));
            if let Some(status) = &row.status {
                parts.push(status.dirty_label());
                parts.push(push_label(status.push_state()));
            }
            parts.push(row.owner_label());
            if let Some(base) = &row.base {
                parts.push(format!("base {} ({})", base.reference, base.describe()));
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

    #[test]
    fn focus_picks_round_trip_through_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("overview-focus.json");
        let (a, b, c) = (Path::new("/w/a"), Path::new("/w/b"), Path::new("/w/c"));
        let mut picks = FocusPicks::default();
        assert!(picks.set(a, &Focus::Coordinator("one".into())));
        assert!(picks.set(b, &Focus::AllRecent));
        assert!(!picks.set(b, &Focus::AllRecent));
        picks.save(&path).unwrap();
        let loaded = FocusPicks::load(&path);
        assert_eq!(loaded, picks);
        assert_eq!(loaded.focus_for(a), Focus::Coordinator("one".into()));
        assert_eq!(loaded.focus_for(b), Focus::AllRecent);
        assert_eq!(loaded.focus_for(c), Focus::Latest);
        // Back to the automatic default clears the pick.
        let mut cleared = loaded;
        assert!(cleared.set(a, &Focus::Latest));
        assert_eq!(cleared.focus_for(a), Focus::Latest);
        cleared.save(&path).unwrap();
        assert_eq!(FocusPicks::load(&path).focus_for(a), Focus::Latest);
        // No temporary files are left behind.
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn focus_picks_missing_or_corrupt_file_reads_as_default() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("none.json");
        assert_eq!(FocusPicks::load(&missing), FocusPicks::default());
        let corrupt = temp.path().join("bad.json");
        std::fs::write(&corrupt, "{ not json").unwrap();
        assert_eq!(FocusPicks::load(&corrupt), FocusPicks::default());
    }

    #[test]
    fn stored_unknown_coordinator_falls_back_to_latest() {
        let picks = FocusPicks::parse(r#"{"/w":{"coordinator":"gone"}}"#);
        let focus = picks.focus_for(Path::new("/w"));
        assert_eq!(focus, Focus::Coordinator("gone".into()));
        let roots = focused_roots(&focus, &[]);
        assert!(roots.is_empty());
    }

    #[test]
    fn hosting_tab_finds_the_tab_with_the_chat() {
        let tabs = [None, Some("a"), Some("b")];
        assert_eq!(hosting_tab(tabs, "b"), Some(2));
        assert_eq!(hosting_tab(tabs, "a"), Some(1));
        assert_eq!(hosting_tab(tabs, "c"), None);
        assert_eq!(hosting_tab([], "a"), None);
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

    #[test]
    fn workspace_names_come_from_trimmed_titles() {
        assert_eq!(
            workspace_name_from_title("  Cloud   migration \n"),
            "Cloud migration"
        );
        assert_eq!(workspace_name_from_title("   "), "Workspace");
        let long = "Move the api-server and billing stacks to the new cloud account";
        let name = workspace_name_from_title(long);
        assert!(name.chars().count() <= WORKSPACE_NAME_MAX, "{name}");
        assert_eq!(name, "Move the api-server and billing stacks");
    }

    #[test]
    fn work_folder_slug_is_filesystem_safe() {
        assert_eq!(work_folder_slug("Cloud migration"), "cloud-migration");
        assert_eq!(
            work_folder_slug("  PRJ-101: fix / ship!  "),
            "prj-101-fix-ship"
        );
        assert_eq!(work_folder_slug("../etc"), "etc");
        assert_eq!(work_folder_slug("Café déjà"), "caf-d-j");
        assert_eq!(work_folder_slug("***"), "workspace");
        assert_eq!(work_folder_slug(""), "workspace");
    }

    #[test]
    fn work_folders_get_a_suffix_on_collision() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("Work");
        let first = create_work_folder(&base, "Cloud migration").unwrap();
        assert_eq!(first, base.join("cloud-migration"));
        assert!(first.is_dir());
        let second = create_work_folder(&base, "cloud  MIGRATION").unwrap();
        assert_eq!(second, base.join("cloud-migration-2"));
        // A file in the way counts as taken too.
        std::fs::write(base.join("cloud-migration-3"), "").unwrap();
        let fourth = create_work_folder(&base, "Cloud migration").unwrap();
        assert_eq!(fourth, base.join("cloud-migration-4"));
        assert!(fourth.is_dir());
    }

    /// Prints the overview of a workspace owning one real chat. Read-only.
    /// Run with `GITTERM_REAL_FACTS_DIR=<config>/chat-meta
    /// GITTERM_REAL_CHAT_ID=<root session id> cargo test --lib
    /// workspace_overview::tests::real_owning_workspace -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads GITTERM_REAL_FACTS_DIR and GITTERM_REAL_CHAT_ID"]
    fn real_owning_workspace_overview() {
        let (Ok(facts_dir), Ok(chat_id)) = (
            std::env::var("GITTERM_REAL_FACTS_DIR"),
            std::env::var("GITTERM_REAL_CHAT_ID"),
        ) else {
            panic!("set GITTERM_REAL_FACTS_DIR and GITTERM_REAL_CHAT_ID");
        };
        let facts_dir = PathBuf::from(facts_dir);
        let store = facts_dir
            .parent()
            .map(crate::tasks::TaskStore::path_for_config_root)
            .filter(|path| path.exists())
            .map(|path| crate::tasks::TaskStore::load(path).expect("read tasks.json"));
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
        let Some(chat) = entries.iter().find(|entry| entry.id == chat_id) else {
            panic!("chat {chat_id} is not in this Mac's index");
        };
        let root = chat.root_id.clone().unwrap_or_else(|| chat.id.clone());
        let name = workspace_name_from_title(&chat.title);
        println!(
            "WORKSPACE · {name} · home ~/Work/{} · owns {root}",
            work_folder_slug(&name)
        );
        println!("  chat resumes from {}", chat.cwd.display());
        let started = Instant::now();
        let summary = WorkSummary::build_owned(&entries, &[root], &tasks, SystemTime::now());
        println!(
            "summary in {}ms: {} chats, facts: {}",
            started.elapsed().as_millis(),
            summary.chat_count,
            summary.has_facts
        );
        println!("\nCHATS · {}", summary.coordinators.len());
        for coordinator in &summary.coordinators {
            println!(
                "  {} · {} · {}",
                shorten(&coordinator.title, 70),
                chats::format_age(coordinator.active),
                coordinator.id
            );
            if let Some(fan_out) = &coordinator.fan_out {
                println!("      fan-out: {fan_out}");
            }
            for child in coordinator.children.iter().take(8) {
                println!(
                    "      \u{21b3} {:?} {}",
                    child.kind,
                    shorten(&child.title, 60)
                );
            }
        }
        println!("\nALSO WORKED HERE · {}", summary.also_worked.len());
        for item in &summary.also_worked {
            println!(
                "  {} · {} edits here",
                shorten(&item.coordinator.title, 70),
                item.edits_here
            );
        }
        println!("\nREPOS TOUCHED · {}", summary.repos.len());
        for repo in &summary.repos {
            println!("  {} · {} edits", repo.dir.display(), repo.edits);
        }
        println!("\nISSUES · {}", summary.issue_keys.join(" · "));
        println!("\nBRANCH CANDIDATES · {}", summary.candidates.len());
        for candidate in &summary.candidates {
            println!(
                "  {} · {}",
                candidate.branch.as_deref().unwrap_or("(detached)"),
                candidate.checkout.display()
            );
        }
    }
}
