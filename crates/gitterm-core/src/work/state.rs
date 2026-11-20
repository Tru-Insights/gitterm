//! Work orchestration. The host supplies immutable ownership facts and runs
//! effects off-thread. No effect grants authority to write notes or chat facts.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use super::{documents, summary::*};
use crate::chats::ChatIndexEntry;

pub const VISIBLE_POLL: Duration = Duration::from_secs(15);
pub const VISIBLE_SLOW_POLL: Duration = Duration::from_secs(30);
pub const HIDDEN_POLL: Duration = Duration::from_secs(60);
pub const REOPEN_FRESH: Duration = Duration::from_secs(5);
pub const DOCUMENTS_POLL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Folder(PathBuf),
    Owned(Vec<String>),
}

/// Folder overviews share domain data; explicitly owned work uses its UID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Folder(PathBuf),
    Owner(u64),
}

/// Replacing a workspace's source invalidates all its outstanding jobs.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub workspace: u64,
    pub source: Source,
    pub tasks: Vec<TaskInfo>,
    pub focus: Focus,
    /// None on a remote workspace. Hosts retain today's local-only scan gate.
    pub documents: Option<documents::ScanRequest>,
}

impl Inputs {
    pub fn key(&self) -> Key {
        match &self.source {
            Source::Folder(dir) => Key::Folder(dir.clone()),
            Source::Owned(_) => Key::Owner(self.workspace),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub workspace: u64,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct SummaryPlan {
    pub source: Source,
    pub tasks: Vec<TaskInfo>,
    pub focus: Focus,
    pub now: SystemTime,
}
impl SummaryPlan {
    pub fn build(&self, entries: &[ChatIndexEntry]) -> WorkSummary {
        match &self.source {
            Source::Folder(dir) => {
                WorkSummary::build(entries, dir, &self.tasks, &self.focus, self.now)
            }
            Source::Owned(ids) => WorkSummary::build_owned(entries, ids, &self.tasks, self.now),
        }
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

#[derive(Debug, Default)]
pub struct Pane {
    pub summary: WorkSummary,
    pub summary_stale: bool,
    pub branches: Vec<BranchStatus>,
    pub errors: Vec<String>,
    pub last_refresh: Option<Instant>,
    pub last_full_refresh: Option<Instant>,
    pub needs_full: bool,
    pub unchanged_streak: u32,
    summary_job: Option<Stamp>,
    refresh_on_summary: bool,
    refresh_job: Option<Stamp>,
}
#[derive(Debug, Default)]
pub struct Documents {
    pub result: Option<documents::ScanResult>,
    pub error: Option<String>,
    pub scanned_at: Option<Instant>,
    job: Option<Stamp>,
}
#[derive(Debug)]
pub enum Intent {
    /// Also used after ownership/focus facts change. Invalidates outstanding work.
    SetInputs(Inputs),
    Open {
        workspace: u64,
        now: Instant,
        wall_time: SystemTime,
    },
    Poll {
        workspace: u64,
        visible: bool,
        now: Instant,
        wall_time: SystemTime,
    },
    Refresh {
        workspace: u64,
        now: Instant,
        wall_time: SystemTime,
    },
    /// Index reload invalidates hidden summaries; only the visible one rebuilds.
    IndexChanged {
        visible: Option<u64>,
        wall_time: SystemTime,
    },
    SummaryFinished {
        stamp: Stamp,
        result: Box<Result<WorkSummary, String>>,
        now: Instant,
        visible: bool,
    },
    RefreshFinished {
        stamp: Stamp,
        result: RefreshOutcome,
        now: Instant,
        visible: bool,
    },
    ScanDocuments(u64),
    DocumentsFinished {
        stamp: Stamp,
        result: Result<documents::ScanResult, String>,
        now: Instant,
    },
    Remove(u64),
}
#[derive(Debug)]
pub enum Effect {
    BuildSummary {
        stamp: Stamp,
        plan: SummaryPlan,
    },
    Refresh {
        stamp: Stamp,
        plan: RefreshPlan,
    },
    ScanDocuments {
        stamp: Stamp,
        request: documents::ScanRequest,
    },
    /// Route through Find's freshness gate. Catch-up writes require host authority.
    RefreshIndex {
        force: bool,
    },
}
#[derive(Debug, Default)]
pub struct State {
    inputs: HashMap<u64, Inputs>,
    panes: HashMap<Key, Pane>,
    documents: HashMap<u64, Documents>,
    generation: u64,
}
impl State {
    pub fn pane(&self, workspace: u64) -> Option<&Pane> {
        self.inputs
            .get(&workspace)
            .and_then(|i| self.panes.get(&i.key()))
    }
    pub fn documents(&self, workspace: u64) -> Option<&Documents> {
        self.documents.get(&workspace)
    }
    fn pane_mut(&mut self, workspace: u64) -> Option<&mut Pane> {
        let key = self.inputs.get(&workspace)?.key();
        self.panes.get_mut(&key)
    }
    fn invalidate_owner_jobs(&mut self, workspace: u64) {
        for pane in self.panes.values_mut() {
            if pane
                .summary_job
                .as_ref()
                .is_some_and(|stamp| stamp.workspace == workspace)
            {
                pane.summary_job = None;
                pane.summary_stale = true;
            }
            if pane
                .refresh_job
                .as_ref()
                .is_some_and(|stamp| stamp.workspace == workspace)
            {
                pane.refresh_job = None;
                pane.needs_full = true;
            }
        }
    }
    fn stamp(&mut self, workspace: u64) -> Stamp {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("Work generation exhausted");
        Stamp {
            workspace,
            generation: self.generation,
        }
    }
    fn summary(&mut self, workspace: u64, now: SystemTime) -> Vec<Effect> {
        let Some(input) = self.inputs.get(&workspace).cloned() else {
            return vec![];
        };
        let stamp = self.stamp(workspace);
        let pane = self.pane_mut(workspace).unwrap();
        pane.summary_stale = true;
        pane.summary_job = Some(stamp.clone());
        vec![Effect::BuildSummary {
            stamp,
            plan: SummaryPlan {
                source: input.source,
                tasks: input.tasks,
                focus: input.focus,
                now,
            },
        }]
    }
    fn refresh(&mut self, workspace: u64, mode: RefreshMode) -> Vec<Effect> {
        let Some(pane) = self.pane(workspace) else {
            return vec![];
        };
        if pane.refresh_job.is_some() || pane.summary_stale {
            return vec![];
        }
        let mode = if pane.last_full_refresh.is_none() || pane.needs_full {
            RefreshMode::Full
        } else {
            mode
        };
        let stamp = self.stamp(workspace);
        let pane = self.pane_mut(workspace).unwrap();
        if mode == RefreshMode::Full {
            pane.needs_full = false;
        }
        pane.refresh_job = Some(stamp.clone());
        vec![Effect::Refresh {
            stamp,
            plan: RefreshPlan {
                mode,
                candidates: pane.summary.candidates.clone(),
                issue_keys: pane.summary.issue_keys.clone(),
                key_repos: pane.summary.key_repos.clone(),
                previous: pane.branches.clone(),
            },
        }]
    }
    fn scan_documents(&mut self, workspace: u64) -> Vec<Effect> {
        let Some(request) = self
            .inputs
            .get(&workspace)
            .and_then(|i| i.documents.clone())
        else {
            return vec![];
        };
        if self
            .documents
            .get(&workspace)
            .is_some_and(|d| d.job.is_some())
        {
            return vec![];
        }
        let stamp = self.stamp(workspace);
        self.documents.entry(workspace).or_default().job = Some(stamp.clone());
        vec![Effect::ScanDocuments { stamp, request }]
    }
    pub fn reduce(&mut self, intent: Intent) -> Vec<Effect> {
        match intent {
            Intent::SetInputs(input) => {
                let id = input.workspace;
                self.invalidate_owner_jobs(id);
                let old = self.inputs.insert(id, input);
                if let Some(old) = old {
                    let key = old.key();
                    if !self.inputs.values().any(|i| i.key() == key) {
                        self.panes.remove(&key);
                    }
                }
                let key = self.inputs[&id].key();
                let pane = self.panes.entry(key).or_default();
                pane.summary_stale = true;
                pane.summary_job = None;
                pane.refresh_job = None;
                pane.needs_full = true;
                self.documents.remove(&id);
                vec![]
            }
            Intent::Remove(id) => {
                self.invalidate_owner_jobs(id);
                self.inputs.remove(&id);
                self.documents.remove(&id);
                self.panes
                    .retain(|key, _| self.inputs.values().any(|i| i.key() == *key));
                vec![]
            }
            Intent::Open {
                workspace,
                now,
                wall_time,
            } => {
                let mut effects = self.summary(workspace, wall_time);
                if let Some(pane) = self.pane_mut(workspace) {
                    pane.refresh_on_summary = true;
                }
                if !effects.is_empty() {
                    effects.push(Effect::RefreshIndex { force: false });
                }
                if self.documents.get(&workspace).is_none_or(|p| {
                    p.scanned_at
                        .is_none_or(|at| now.saturating_duration_since(at) >= DOCUMENTS_POLL)
                }) {
                    effects.extend(self.scan_documents(workspace));
                }
                effects
            }
            Intent::Refresh {
                workspace,
                now: _,
                wall_time,
            } => {
                if !self.inputs.contains_key(&workspace) {
                    return vec![];
                }
                self.pane_mut(workspace).unwrap().needs_full = true;
                let mut effects = self.summary(workspace, wall_time);
                effects.push(Effect::RefreshIndex { force: true });
                effects.extend(self.scan_documents(workspace));
                effects
            }
            Intent::IndexChanged { visible, wall_time } => {
                for pane in self.panes.values_mut() {
                    pane.summary_stale = true;
                    pane.summary_job = None;
                }
                visible
                    .map(|id| self.summary(id, wall_time))
                    .unwrap_or_default()
            }
            Intent::Poll {
                workspace,
                visible,
                now,
                wall_time,
            } => {
                let docs_due = visible
                    && self.documents.get(&workspace).is_none_or(|p| {
                        p.scanned_at
                            .is_none_or(|at| now.saturating_duration_since(at) >= DOCUMENTS_POLL)
                    });
                let mut effects = if docs_due {
                    self.scan_documents(workspace)
                } else {
                    vec![]
                };
                let Some(pane) = self.pane(workspace) else {
                    return effects;
                };
                if pane.summary_job.is_some() {
                    return effects;
                }
                if pane.summary_stale {
                    if visible {
                        effects.extend(self.reduce(Intent::Open {
                            workspace,
                            now,
                            wall_time,
                        }));
                    }
                    return effects;
                }
                let interval = if !visible {
                    HIDDEN_POLL
                } else if pane.unchanged_streak >= 2 {
                    VISIBLE_SLOW_POLL
                } else {
                    VISIBLE_POLL
                };
                let due = pane
                    .last_refresh
                    .is_none_or(|at| now.saturating_duration_since(at) >= interval);
                if due {
                    effects.extend(self.refresh(
                        workspace,
                        if visible {
                            RefreshMode::Full
                        } else {
                            RefreshMode::Gated
                        },
                    ));
                }
                effects
            }
            Intent::SummaryFinished {
                stamp,
                result,
                now,
                visible,
            } => {
                let Some(pane) = self
                    .pane_mut(stamp.workspace)
                    .filter(|p| p.summary_job.as_ref() == Some(&stamp))
                else {
                    return vec![];
                };
                pane.summary_job = None;
                match *result {
                    Err(error) => {
                        pane.errors.push(error);
                        return vec![];
                    }
                    Ok(summary) => {
                        if pane.summary.candidates != summary.candidates
                            || pane.summary.issue_keys != summary.issue_keys
                            || pane.summary.key_repos != summary.key_repos
                        {
                            pane.needs_full = true;
                        }
                        pane.summary = summary;
                        pane.summary_stale = false;
                    }
                }
                let open_refresh = std::mem::take(&mut pane.refresh_on_summary);
                let recent = !pane.needs_full
                    && pane
                        .last_full_refresh
                        .is_some_and(|at| now.saturating_duration_since(at) < REOPEN_FRESH);
                if visible && (pane.needs_full || (open_refresh && !recent)) {
                    self.refresh(stamp.workspace, RefreshMode::Full)
                } else {
                    vec![]
                }
            }
            Intent::RefreshFinished {
                stamp,
                result,
                now,
                visible,
            } => {
                let Some(pane) = self
                    .pane_mut(stamp.workspace)
                    .filter(|p| p.refresh_job.as_ref() == Some(&stamp))
                else {
                    return vec![];
                };
                pane.refresh_job = None;
                pane.last_refresh = Some(now);
                pane.unchanged_streak = if result.changed_from(&pane.branches) {
                    0
                } else {
                    pane.unchanged_streak.saturating_add(1)
                };
                if result.mode == RefreshMode::Full {
                    pane.last_full_refresh = Some(now);
                    pane.errors = result.errors;
                }
                pane.branches = result.branches;
                if pane.needs_full && visible {
                    self.refresh(stamp.workspace, RefreshMode::Full)
                } else {
                    vec![]
                }
            }
            Intent::ScanDocuments(id) => self.scan_documents(id),
            Intent::DocumentsFinished { stamp, result, now } => {
                let Some(pane) = self
                    .documents
                    .get_mut(&stamp.workspace)
                    .filter(|p| p.job.as_ref() == Some(&stamp))
                else {
                    return vec![];
                };
                pane.job = None;
                pane.scanned_at = Some(now);
                match result {
                    Ok(result) => {
                        pane.result = Some(result);
                        pane.error = None;
                    }
                    Err(error) => pane.error = Some(error),
                }
                vec![]
            }
        }
    }
}

#[cfg(test)]
mod tests;
