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
use std::time::{Duration, Instant, SystemTime};

pub use gitterm_core::git_facts::{
    ahead_behind_args, base_candidates, choose_nearest, parse_ahead_behind, parse_left_right,
    BaseCandidate, BaseChoice, BaseSource, DiffSource, GIT_TIMEOUT,
};
pub(crate) use gitterm_core::git_facts::{
    git_text, measure_bases, resolve_commit, run_git_lines, run_git_records,
    run_git_records_bounded,
};

#[cfg(test)]
use crate::chat_facts;
#[cfg(test)]
use crate::chats::{self, ChatIndexEntry};
use crate::worktree_follow;

pub use gitterm_core::work::summary::*;

/// A whole refresh stops starting new git calls after this long.
pub const REFRESH_DEADLINE: Duration = Duration::from_secs(45);

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

// ---------------------------------------------------------------------------
// Git, off the UI thread.

fn read_gitdir_stamp(gitdir: &Path) -> GitdirStamp {
    let stat = |rel: &str| {
        let meta = std::fs::metadata(gitdir.join(rel)).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    };
    GitdirStamp::from_entries([stat("index"), stat("HEAD"), stat("logs/HEAD")])
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
            .map(|gitdir| read_gitdir_stamp(&gitdir))
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

/// A checkout no overview row names, read as a row reads it: its main
/// repo, branch and nearest base (TRU-179, a chat's edit card opening the
/// review pane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutFacts {
    /// The main checkout (a linked worktree's repo), for its name.
    pub repo: PathBuf,
    /// `None` when detached.
    pub branch: Option<String>,
    pub base: Option<BaseChoice>,
}

/// Reads [`CheckoutFacts`] for `checkout` (its top level): the common git
/// dir, `status --branch` headers, then the base candidates measured as
/// [`read_checkout`] measures them.
pub async fn checkout_facts(checkout: &Path) -> Result<CheckoutFacts, String> {
    let common = git_text(
        checkout,
        &[
            "--no-optional-locks",
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let common = PathBuf::from(common.trim());
    let repo = match common.file_name() {
        Some(name) if name == ".git" => common
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| checkout.to_path_buf()),
        // A bare repo's worktree: the repo is the git dir itself.
        _ => common,
    };
    let mut counts = StatusCounts::default();
    // Headers come first; the first entry line ends the read.
    run_git_lines(
        checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ],
        GIT_TIMEOUT,
        |line| counts.observe(line, 1),
    )
    .await?;
    let mut candidates = base_candidates(None, counts.head.as_deref(), counts.upstream.as_deref());
    let mut git_runs = 0;
    let measured = measure_bases(checkout, &mut candidates, &mut git_runs).await?;
    Ok(CheckoutFacts {
        repo,
        branch: counts.head,
        base: choose_nearest(&candidates, &measured).map(|(base, _)| base),
    })
}

// ---------------------------------------------------------------------------
// Review sources.

// ---------------------------------------------------------------------------
// Copy helpers.

/// The rows a fold shows: all of them open, none folded.
pub fn fold_rows<T>(items: &[T], open: bool) -> &[T] {
    if open {
        items
    } else {
        &[]
    }
}

/// The marker after a fold's count: "▾" open, "▸" folded.
pub fn fold_marker(open: bool) -> &'static str {
    if open {
        "\u{25BE}"
    } else {
        "\u{25B8}"
    }
}

/// A Work sidebar row whose detail and actions can be opened with its
/// ▸ / ▾ disclosure (TRU-208). Hover never opens anything.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// A PR row, by PR id.
    Pr(String),
    /// A repo's line, by its folder.
    Repo(PathBuf),
    /// A branch row, by its checkout.
    Branch(PathBuf),
    /// A chat row, by chat id.
    Chat(String),
    /// A document row, by its key.
    Document(String),
}

/// Opens `key` in one workspace's set of open rows, or closes it when it
/// was open; returns whether it is open now. Other rows stay as they are.
pub fn toggle_row(open: &mut HashSet<RowKey>, key: RowKey) -> bool {
    if open.remove(&key) {
        false
    } else {
        open.insert(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_facts::{ChatFacts, CheckoutTouch, RepoTouch};
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

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn issues_group_by_state_then_mentions_without_changing_source_keys() {
        use crate::issue_titles::{CachedIssue, IssueGroup, IssueTitlesFile};
        // Done is the most mentioned overall; active work still goes first.
        let a = keys(&["PRJ-1", "PRJ-2", "PRJ-3", "PRJ-4", "PRJ-5", "PRJ-6"]);
        let b = keys(&["PRJ-2", "PRJ-5", "PRJ-6"]);
        let c = keys(&["PRJ-6"]);
        let ordered = work_issue_keys(&[&a, &b, &c], &["work/prj-1-groups"]);
        assert_eq!(
            ordered,
            ["PRJ-6", "PRJ-2", "PRJ-5", "PRJ-1", "PRJ-3", "PRJ-4"]
        );
        let mut cache = IssueTitlesFile::default();
        for (key, state) in a.iter().zip([
            "In Progress",
            "Waiting for deploy to DEV",
            "In DEV for Testing",
            "Backlog",
            "An unknown state",
            "Done",
        ]) {
            let mut entry = CachedIssue {
                state: Some(state.to_string()),
                ..CachedIssue::default()
            };
            entry.observe_done("2026-11-11T10:00:00Z");
            cache.issues.insert(key.clone(), entry);
        }
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-11T11:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let groups: Vec<_> = cache
            .grouped_keys(&ordered, now)
            .into_iter()
            .map(|(group, keys)| {
                (
                    group,
                    keys.into_iter().map(String::as_str).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            groups,
            vec![
                (IssueGroup::InProgress, vec!["PRJ-2", "PRJ-1"]),
                (IssueGroup::InDevForTesting, vec!["PRJ-3"]),
                (IssueGroup::Backlog, vec!["PRJ-5", "PRJ-4"]),
                (IssueGroup::Done, vec!["PRJ-6"]),
            ]
        );
        // Missing cache entries join Backlog, empty groups never get headers.
        assert_eq!(
            cache.grouped_keys(&["PRJ-99".to_string()], now)[0].0,
            IssueGroup::Backlog
        );
        assert_eq!(cache.grouped_keys(&a[..1], now).len(), 1);
        assert_eq!(a.len(), 6);
    }

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

        // Both review sources (TRU-178), against the row's own base.
        use crate::review_pane::{load_review, ReviewTarget};
        let review = ReviewTarget {
            checkout: owned_wt.clone(),
            repo: folder_name(&repo),
            branch: "feature-standby".into(),
            owner: owned.owner_label(),
            base: owned.base.clone(),
        };
        let uncommitted = load_review(review.clone(), DiffSource::Uncommitted)
            .await
            .unwrap()
            .diff;
        let paths: Vec<&str> = uncommitted.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "scratch.txt"]);
        let texts = |file: &crate::review_pane::ReviewFile| -> Vec<String> {
            file.hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter().map(|line| line.text.clone()))
                .collect()
        };
        assert!(texts(&uncommitted.files[0]).contains(&"changed".to_string()));
        assert_eq!(texts(&uncommitted.files[1]), ["x"]);
        let vs_base = load_review(review.clone(), DiffSource::VsBase)
            .await
            .unwrap()
            .diff;
        let paths: Vec<&str> = vs_base.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["b.txt"]);
        assert_eq!(texts(&vs_base.files[0]), ["two"]);
        assert_eq!(vs_base.merge_base.as_ref().map(String::len), Some(7));
        let missing = load_review(
            ReviewTarget {
                base: None,
                ..review
            },
            DiffSource::VsBase,
        )
        .await;
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

    fn branch_in(repo: &str, checkout: &str) -> BranchStatus {
        BranchStatus {
            owned: OwnedBranch {
                repo: PathBuf::from(repo),
                checkout: PathBuf::from(checkout),
                branch: Some("topic".into()),
                is_main: false,
                ownership: Ownership::Edited,
                owner: String::new(),
                last: None,
                edits: 1,
                task: None,
                gone: false,
            },
            base: None,
            status: None,
            vs_base: None,
            error: None,
            stamp: GitdirStamp::default(),
            checked_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn merged_repo_groups_keep_listed_untouched_repos_in_order_once() {
        let listed = vec![
            PathBuf::from("/r/a"),
            PathBuf::from("/r/b"),
            PathBuf::from("/r/c"),
        ];
        let touched = vec![
            RepoTouched {
                dir: PathBuf::from("/r/c"),
                edits: 4,
            },
            RepoTouched {
                dir: PathBuf::from("/r/d"),
                edits: 2,
            },
        ];
        let branches = vec![
            branch_in("/r/c", "/r/c"),
            branch_in("/r/c", "/r/c-wt"),
            branch_in("/r/e", "/r/e"),
        ];
        let groups = merge_repo_groups(&listed, &[], &touched, &branches);
        let dirs: Vec<&str> = groups.iter().map(|g| g.dir.to_str().unwrap()).collect();
        assert_eq!(dirs, ["/r/a", "/r/b", "/r/c", "/r/d", "/r/e"]);
        // Removed by hand (TRU-238): an edited or branch-only repo drops out,
        // a listed one stays.
        let removed = vec![
            PathBuf::from("/r/d"),
            PathBuf::from("/r/e"),
            PathBuf::from("/r/a"),
        ];
        let kept = merge_repo_groups(&listed, &removed, &touched, &branches);
        let kept: Vec<&str> = kept.iter().map(|g| g.dir.to_str().unwrap()).collect();
        assert_eq!(kept, ["/r/a", "/r/b", "/r/c"]);
        assert_eq!(groups[0].edits, None);
        assert!(groups[0].rows.is_empty());
        assert_eq!(groups[2].edits, Some(4));
        assert_eq!(groups[2].rows.len(), 2);
        assert_eq!(groups[3].edits, Some(2));
        assert_eq!(groups[4].edits, None);
        assert_eq!(groups[4].rows.len(), 1);
    }

    fn counts(upstream: Option<(u32, u32)>, files: usize) -> StatusCounts {
        StatusCounts {
            head: Some("feature".into()),
            upstream: upstream.map(|_| "origin/feature".to_string()),
            upstream_ab: upstream,
            files,
            ..StatusCounts::default()
        }
    }

    #[test]
    fn a_branch_line_shows_dirty_first_then_unpushed_and_nothing_when_clean() {
        let dirty_and_ahead = counts(Some((2, 0)), 3);
        assert_eq!(
            branch_fact(Some(&dirty_and_ahead)),
            Some(BranchFact::Dirty("3 dirty".into()))
        );
        let ahead = counts(Some((2, 1)), 0);
        assert_eq!(branch_fact(Some(&ahead)), Some(BranchFact::Unpushed(2)));
        assert_eq!(BranchFact::Unpushed(2).label(), "\u{2191}2");
        let clean = counts(Some((0, 4)), 0);
        assert_eq!(branch_fact(Some(&clean)), None);
        // No upstream, a gone upstream or no read yet: nothing on the line.
        assert_eq!(branch_fact(Some(&counts(None, 0))), None);
        let mut gone = counts(None, 0);
        gone.upstream = Some("origin/feature".into());
        assert_eq!(branch_fact(Some(&gone)), None);
        assert_eq!(branch_fact(None), None);
    }

    #[test]
    fn a_repo_with_nothing_going_on_says_so_on_its_one_line() {
        assert_eq!(
            repo_facts(true, None, Some("no edits yet")),
            "home \u{00B7} no edits"
        );
        assert_eq!(repo_facts(false, Some(348), None), "348 edits");
        assert_eq!(
            repo_facts(false, Some(12), Some("no live checkout of this work")),
            "12 edits \u{00B7} no live checkout of this work"
        );
        assert_eq!(repo_facts(false, None, None), "");
    }

    #[test]
    fn the_agent_config_line_hides_when_all_are_zero() {
        assert_eq!(agent_config_line(0, 0, 0), None);
        assert_eq!(
            agent_config_line(0, 1, 0).as_deref(),
            Some("skills 0 \u{00B7} MCPs 1 \u{00B7} hooks 0")
        );
    }

    #[test]
    fn a_folded_section_hides_its_rows() {
        let docs = ["a", "b", "c"];
        assert!(fold_rows(&docs, false).is_empty());
        assert_eq!(fold_rows(&docs, true), &docs);
        assert_eq!(fold_marker(false), "\u{25B8}");
        assert_eq!(fold_marker(true), "\u{25BE}");
    }

    #[test]
    fn a_row_opens_and_closes_on_its_toggle() {
        let mut open = HashSet::new();
        let branch = RowKey::Branch(PathBuf::from("/r/a-wt"));
        assert!(toggle_row(&mut open, branch.clone()));
        assert!(open.contains(&branch));
        assert!(!toggle_row(&mut open, branch.clone()));
        assert!(open.is_empty());
    }

    #[test]
    fn several_rows_stay_open_together() {
        let mut open = HashSet::new();
        toggle_row(&mut open, RowKey::Pr("o/r#1".into()));
        toggle_row(&mut open, RowKey::Chat("c1".into()));
        toggle_row(&mut open, RowKey::Repo(PathBuf::from("/r/a")));
        assert_eq!(open.len(), 3);
        // Closing one leaves the others open.
        toggle_row(&mut open, RowKey::Chat("c1".into()));
        assert!(open.contains(&RowKey::Pr("o/r#1".into())));
        assert!(open.contains(&RowKey::Repo(PathBuf::from("/r/a"))));
        assert!(!open.contains(&RowKey::Chat("c1".into())));
    }

    #[test]
    fn open_rows_are_kept_per_workspace() {
        let mut by_workspace: HashMap<&str, HashSet<RowKey>> = HashMap::new();
        let doc = RowKey::Document("plan.md".into());
        toggle_row(by_workspace.entry("one").or_default(), doc.clone());
        assert!(by_workspace["one"].contains(&doc));
        assert!(by_workspace
            .get("two")
            .is_none_or(|open| !open.contains(&doc)));
        toggle_row(by_workspace.entry("two").or_default(), doc.clone());
        toggle_row(by_workspace.entry("one").or_default(), doc.clone());
        assert!(!by_workspace["one"].contains(&doc));
        assert!(by_workspace["two"].contains(&doc));
    }
}
