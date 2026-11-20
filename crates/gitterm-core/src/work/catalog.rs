//! Named-work sheet rules from the Iced host. No filesystem or UI effects.
//!
//! Persisted workspace DTOs remain host-owned. The projections below preserve
//! catalog indices and runtime-only claims without creating a second catalog.
//!
//! Sources at 1454685: main.rs NewWorkSheet/WorkStart, existing_work_matches,
//! existing_work_named, new_work_recent_repos, parked_order, existing_work,
//! submit_new_work, Workspace::{owns_chats,is_work}, open_local_workspace_index,
//! ensure_local_workspace_for_chat and AgentTabEvent's conversation join rule.
//! Config's home_dir/is_parked delegate here; main.rs adoption is a separate task.

use std::path::{Path, PathBuf};

/// "New workspace" (TRU-194, `+`): a sheet for a piece of work. Its
/// name (its home is always a new `~/Work/<slug>`), optional starting repos (browsed with the folder picker, or one click
/// from `recent`) and a decisions repo among them (TRU-205).
#[derive(Debug, Clone, Default)]
pub struct NewWorkSheet {
    pub name: String,
    pub error: Option<String>,
    /// This Mac's recently used repos, read when the sheet opens (see
    /// [`new_work_recent_repos`]).
    pub recent: Vec<PathBuf>,
    /// The starting repos, in the order picked.
    pub repos: Vec<PathBuf>,
    pub decisions_repo: Option<PathBuf>,
}

/// How many recent repos the New workspace sheet offers as one-click adds.
pub const NEW_WORK_RECENT_REPOS: usize = 5;

impl NewWorkSheet {
    /// Create's validation order: name first, then the decisions-repo choice.
    /// Optional repos need not exist and keep their picked order.
    pub fn validate(&self) -> Result<(String, WorkStart), String> {
        let name = validate_work_name(&self.name)?;
        let start = WorkStart::new(self.repos.clone(), self.decisions_repo.clone())?;
        Ok((name, start))
    }

    /// "Use this folder" for a picker the sheet opened: `dir` joins the
    /// starting repos (once).
    pub fn use_picked_folder(&mut self, dir: PathBuf) {
        if !self.repos.contains(&dir) {
            self.repos.push(dir);
        }
        self.error = None;
    }

    /// Drops `dir` from the starting repos, and as decisions repo.
    pub fn remove_repo(&mut self, dir: &Path) {
        self.repos.retain(|repo| repo != dir);
        if self.decisions_repo.as_deref() == Some(dir) {
            self.decisions_repo = None;
        }
        self.error = None;
    }

    /// The Recent row: up to [`NEW_WORK_RECENT_REPOS`] of `recent` not
    /// picked yet.
    pub fn recent_shortcuts(&self) -> Vec<&PathBuf> {
        self.recent
            .iter()
            .filter(|dir| !self.repos.contains(dir))
            .take(NEW_WORK_RECENT_REPOS)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExistingWorkStatus {
    Open,
    Parked,
    /// Done or closed (Close parks then marks done): kept in the file;
    /// Reopen clears the done state and resumes it.
    Done,
}

impl ExistingWorkStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Parked => "parked",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingWorkAction {
    /// Switch to the open workspace with this uid (`WorkspaceSelect`).
    Switch(u64),
    /// Resume the parked entry (`ParkedResume`).
    Resume(crate::park::Key),
    /// Reopen done work: clear its done state, then resume it
    /// (`reopen_done`, `ParkedResume`).
    Reopen(crate::park::Key),
}

/// A piece of work that already exists, as the sheet lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingWork {
    pub name: String,
    pub status: ExistingWorkStatus,
    pub action: Option<ExistingWorkAction>,
}

/// How many existing pieces of work the sheet lists.
pub const NEW_WORK_EXISTING_SHOWN: usize = 5;

/// The sheet's existing work rows for the typed `name` (TRU-205), from
/// `work` in recency order within each status. A blank name lists the
/// open and parked work, most recent first; otherwise the work whose
/// name holds the typed one ignoring case, open before parked before
/// done. At most [`NEW_WORK_EXISTING_SHOWN`].
pub fn existing_work_matches<'a>(work: &'a [ExistingWork], name: &str) -> Vec<&'a ExistingWork> {
    let query = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let query = query.to_lowercase();
    let mut matches: Vec<&ExistingWork> = work
        .iter()
        .filter(|item| {
            if query.is_empty() {
                item.status != ExistingWorkStatus::Done
            } else {
                item.name.to_lowercase().contains(&query)
            }
        })
        .collect();
    // Stable: recency order holds within a status.
    matches.sort_by_key(|item| item.status);
    matches.truncate(NEW_WORK_EXISTING_SHOWN);
    matches
}

/// Whether some existing work is called `name` (ignoring case and extra
/// spaces): Create then makes another, with a numbered folder.
pub fn existing_work_named(work: &[ExistingWork], name: &str) -> bool {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    !name.is_empty()
        && work.iter().any(|item| {
            item.name
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
                == name.to_lowercase()
        })
}

/// The repos among `candidates` (most recently used first) the New
/// workspace sheet offers: main checkouts only (`is_main_checkout`: the
/// folder holds a `.git` directory, so a linked worktree, whose `.git` is
/// a file, and a plain folder are left out), never a folder under
/// `.worktrees/` or `.claude/worktrees/`, each once, in order.
pub fn new_work_recent_repos(
    candidates: impl IntoIterator<Item = PathBuf>,
    is_main_checkout: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let in_worktrees = |dir: &Path| {
        let path = dir.to_string_lossy();
        path.contains("/.worktrees/") || path.contains("/.claude/worktrees/")
    };
    let mut repos: Vec<PathBuf> = Vec::new();
    for dir in candidates {
        if !in_worktrees(&dir) && !repos.contains(&dir) && is_main_checkout(&dir) {
            repos.push(dir);
        }
    }
    repos
}

/// What a new piece of work starts with (TRU-194): repos added by hand,
/// and a decisions repo among them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkStart {
    pub repos: Vec<PathBuf>,
    pub decisions_repo: Option<PathBuf>,
}

impl WorkStart {
    /// Rejects a decisions repo that is not one of `repos`.
    pub fn new(repos: Vec<PathBuf>, decisions_repo: Option<PathBuf>) -> Result<Self, String> {
        if let Some(dir) = &decisions_repo {
            if !repos.contains(dir) {
                return Err(format!(
                    "The decisions repo {} is not one of the starting repos.",
                    dir.display()
                ));
            }
        }
        Ok(Self {
            repos,
            decisions_repo,
        })
    }
}

/// The sheet's Create validation, before validating the optional starting repos.
pub fn validate_work_name(name: &str) -> Result<String, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Name the work, for example \"Cloud migration\".".to_string());
    }
    Ok(name)
}

/// A workspace owns conversations once its root-chat list is nonempty.
pub fn owns_chats(chats: &[String]) -> bool {
    !chats.is_empty()
}

/// Runtime-only claims keep freshly created named work in the work zone.
pub fn is_work(chats: &[String], claims_new_chats: bool) -> bool {
    owns_chats(chats) || claims_new_chats
}

/// New task sessions remain task-owned; existing conversation ownership wins.
/// `already_owned` is `summary::owning_workspace` over open then closed chat
/// lists with this session id and `root_id: None`, including this workspace.
pub fn should_claim_chat(
    chats: &[String],
    claims_new_chats: bool,
    task_id: Option<&str>,
    already_owned: bool,
) -> bool {
    is_work(chats, claims_new_chats) && task_id.is_none() && !already_owned
}

/// Persisted home selection (an explicitly empty home is still the home).
pub fn home_dir<'a>(home: Option<&'a str>, dir: &'a str) -> &'a str {
    home.unwrap_or(dir)
}

pub fn is_parked(active: bool, done_at: Option<&str>) -> bool {
    !active && done_at.is_none()
}

/// Facts needed to resolve an unowned local conversation by folder.
#[derive(Debug, Clone, Copy)]
pub struct FolderWorkspace<'a> {
    pub dir: &'a Path,
    /// Open: runtime location is Local. Closed: persisted location is absent
    /// or Local (including legacy entries with a remote_session).
    pub local: bool,
    pub chats: &'a [String],
}

/// The deepest matching local non-owning folder; equal depths choose the last.
/// The host resolves task-worktree aliases in cwd/repo_root before calling.
/// Parked/running and claims_new_chats deliberately do not affect this rule.
pub fn local_workspace_index<'a>(
    workspaces: impl IntoIterator<Item = FolderWorkspace<'a>>,
    cwd: &Path,
    repo_root: &Path,
) -> Option<usize> {
    workspaces
        .into_iter()
        .enumerate()
        .filter(|(_, ws)| {
            ws.local
                && !owns_chats(ws.chats)
                && (cwd.starts_with(ws.dir) || repo_root.starts_with(ws.dir))
        })
        .max_by_key(|(_, ws)| ws.dir.components().count())
        .map(|(idx, _)| idx)
}

/// An open catalog entry, in host Vec order; running parked entries stay here.
#[derive(Debug, Clone, Copy)]
pub struct OpenWork<'a> {
    pub uid: u64,
    pub name: &'a str,
    pub dir: &'a str,
    pub chats: &'a [String],
    pub claims_new_chats: bool,
    pub running_park: bool,
}

/// A saved closed entry; claims_new_chats is intentionally not persisted.
#[derive(Debug, Clone, Copy)]
pub struct ClosedWork<'a> {
    pub name: &'a str,
    pub dir: &'a str,
    pub chats: &'a [String],
    pub active: bool,
    pub parked_at: Option<&'a str>,
    pub done_at: Option<&'a str>,
}

/// Closed, not done, newest parsed parked stamp first; stable for equal/invalid
/// stamps. Indices still address the original host catalog.
pub fn parked_order(configs: &[ClosedWork<'_>]) -> Vec<usize> {
    let mut order: Vec<_> = configs
        .iter()
        .enumerate()
        .filter(|(_, cfg)| is_parked(cfg.active, cfg.done_at))
        .map(|(idx, cfg)| (idx, cfg.parked_at.and_then(super::summary::parse_stamp)))
        .collect();
    order.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    order.into_iter().map(|(idx, _)| idx).collect()
}

/// Open work in host Vec order, running parked before closed parked (newest first),
/// then done (newest first). Filtering by query/status happens in matches.
pub fn existing_work(open: &[OpenWork<'_>], closed: &[ClosedWork<'_>]) -> Vec<ExistingWork> {
    let key = |name: &str, dir: &str| crate::park::Key {
        name: name.into(),
        dir: dir.into(),
    };
    let mut work: Vec<_> = open
        .iter()
        .filter(|ws| is_work(ws.chats, ws.claims_new_chats))
        .map(|ws| ExistingWork {
            name: ws.name.into(),
            status: if ws.running_park {
                ExistingWorkStatus::Parked
            } else {
                ExistingWorkStatus::Open
            },
            action: Some(if ws.running_park {
                ExistingWorkAction::Resume(key(ws.name, ws.dir))
            } else {
                ExistingWorkAction::Switch(ws.uid)
            }),
        })
        .collect();
    work.extend(
        parked_order(closed)
            .into_iter()
            .map(|idx| &closed[idx])
            .filter(|cfg| owns_chats(cfg.chats))
            .map(|cfg| ExistingWork {
                name: cfg.name.into(),
                status: ExistingWorkStatus::Parked,
                action: Some(ExistingWorkAction::Resume(key(cfg.name, cfg.dir))),
            }),
    );
    let mut done: Vec<_> = closed
        .iter()
        .filter(|cfg| owns_chats(cfg.chats) && cfg.done_at.is_some())
        .collect();
    done.sort_by_key(|cfg| std::cmp::Reverse(cfg.done_at.and_then(super::summary::parse_stamp)));
    work.extend(done.into_iter().map(|cfg| ExistingWork {
        name: cfg.name.into(),
        status: ExistingWorkStatus::Done,
        action: Some(ExistingWorkAction::Reopen(key(cfg.name, cfg.dir))),
    }));
    work
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
