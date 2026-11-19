//! Git status state and pure transitions. The host resolves sources and runs jobs.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileStatus {
    pub path: String,
    pub status: String,
    pub is_staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    /// The repo root actually used; self-healed to `--show-toplevel` when
    /// the requested path was inside (not at) the repository root.
    pub root: PathBuf,
    pub repo_name: String,
    pub branch_name: String,
    pub is_git_repo: bool,
    pub staged: Vec<GitFileStatus>,
    pub unstaged: Vec<GitFileStatus>,
    pub untracked: Vec<GitFileStatus>,
}

#[derive(Debug, Clone)]
pub struct GitStatusSnapshot {
    pub tab_id: usize,
    pub repo_path: PathBuf,
    pub repo_name: String,
    pub branch_name: String,
    pub is_git_repo: bool,
    pub staged: Vec<GitFileStatus>,
    pub unstaged: Vec<GitFileStatus>,
    pub untracked: Vec<GitFileStatus>,
    /// Source-level failure (e.g. remote agent unreachable); shown in the
    /// Git sidebar instead of silently reporting a clean tree.
    pub error: Option<String>,
}

const FAST_MS: u64 = 5_000;
const MEDIUM_MS: u64 = 10_000;
const SLOW_MS: u64 = 15_000;
const IDLE_MS: u64 = 30_000;
const NON_REPO_MS: u64 = 20_000;

/// The sole status owner for a tab; session roots and selection stay in the host.
#[derive(Debug, Clone)]
pub struct GitStatusState {
    pub repo_name: String,
    pub branch_name: String,
    pub is_git_repo: bool,
    pub staged: Vec<GitFileStatus>,
    pub unstaged: Vec<GitFileStatus>,
    pub untracked: Vec<GitFileStatus>,
    pub last_poll: Instant,
    pub git_poll_interval_ms: u64,
    pub git_unchanged_streak: u32,
    pub last_git_status_hash: Option<u64>,
    pub git_status_loading: bool,
    pub git_status_error: Option<String>,
}

/// Preserve each caller's historical request prelude, including resume's omission.
#[derive(Debug, Clone, Copy)]
pub enum RequestPrelude {
    Resume,
    TabOpened,
    Refresh,
}

#[derive(Debug)]
pub enum StatusEvent {
    Request {
        root: PathBuf,
        prelude: RequestPrelude,
    },
    /// Reset cadence/hash after context changes or discovered-root adoption.
    Reset,
    Input,
    Wake,
    /// Supplied from the current sidebar/selected-diff facts, without mirroring them.
    VisibilityChanged {
        focused: bool,
        root: PathBuf,
    },
    Completed(GitStatusSnapshot),
    /// A vanished followed tree's result must not replace the last good data.
    DiscardCompletion {
        clear_hash: bool,
    },
    /// UI/source failures outside a status job retain their existing loading behavior.
    SetError(String),
    ClearLoading,
    SetLabel(String),
    InitializeRemote,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StatusCommand {
    CollectStatus {
        root: PathBuf,
    },
    /// The host's existing maintenance tick checks this deadline; no new timer.
    SchedulePoll {
        after: Duration,
    },
}

impl GitStatusState {
    pub fn new(repo_name: String, is_git_repo: bool, now: Instant) -> Self {
        Self {
            repo_name,
            branch_name: "main".into(),
            is_git_repo,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            last_poll: now - Duration::from_millis(FAST_MS),
            git_poll_interval_ms: FAST_MS,
            git_unchanged_streak: 0,
            last_git_status_hash: None,
            git_status_loading: false,
            git_status_error: None,
        }
    }

    /// No clock reads, I/O, runtime or freshness policy: completions win by arrival.
    pub fn reduce(&mut self, event: StatusEvent, now: Instant) -> Vec<StatusCommand> {
        match event {
            StatusEvent::Request { root, prelude } => {
                match prelude {
                    RequestPrelude::Resume => {}
                    RequestPrelude::TabOpened => self.git_status_loading = true,
                    RequestPrelude::Refresh => {
                        self.git_status_loading = true;
                        self.last_poll = now;
                    }
                }
                return vec![StatusCommand::CollectStatus { root }];
            }
            StatusEvent::Reset => {
                self.git_poll_interval_ms = FAST_MS;
                self.git_unchanged_streak = 0;
                self.last_git_status_hash = None;
            }
            StatusEvent::Input => {
                if self.git_poll_interval_ms > FAST_MS {
                    self.git_poll_interval_ms = FAST_MS;
                    self.git_unchanged_streak = 0;
                }
            }
            StatusEvent::Wake => {
                self.git_poll_interval_ms = FAST_MS;
                self.git_unchanged_streak = 0;
                self.last_poll = now - Duration::from_secs(60);
            }
            StatusEvent::VisibilityChanged { focused, root } => {
                if self.git_status_loading {
                    return Vec::new();
                }
                let interval = if focused {
                    self.git_poll_interval_ms
                } else {
                    self.git_poll_interval_ms.max(SLOW_MS)
                };
                let remaining = Duration::from_millis(interval)
                    .saturating_sub(now.saturating_duration_since(self.last_poll));
                if remaining.is_zero() {
                    return self.reduce(
                        StatusEvent::Request {
                            root,
                            prelude: RequestPrelude::Refresh,
                        },
                        now,
                    );
                }
                return vec![StatusCommand::SchedulePoll { after: remaining }];
            }
            StatusEvent::Completed(snapshot) => {
                self.git_status_loading = false;
                self.git_status_error = snapshot.error;
                if self.git_status_error.is_some() {
                    return Vec::new();
                }
                self.repo_name = snapshot.repo_name;
                self.branch_name = snapshot.branch_name;
                self.is_git_repo = snapshot.is_git_repo;
                self.staged = snapshot.staged;
                self.unstaged = snapshot.unstaged;
                self.untracked = snapshot.untracked;
                let hash = self.hash();
                self.git_unchanged_streak = if self.last_git_status_hash == Some(hash) {
                    self.git_unchanged_streak.saturating_add(1)
                } else {
                    0
                };
                let has_changes = !self.staged.is_empty()
                    || !self.unstaged.is_empty()
                    || !self.untracked.is_empty();
                self.git_poll_interval_ms =
                    next_interval(self.is_git_repo, has_changes, self.git_unchanged_streak);
                self.last_git_status_hash = Some(hash);
                self.last_poll = now;
            }
            StatusEvent::DiscardCompletion { clear_hash } => {
                self.git_status_loading = false;
                self.git_status_error = None;
                if clear_hash {
                    self.last_git_status_hash = None;
                }
            }
            StatusEvent::SetError(error) => self.git_status_error = Some(error),
            StatusEvent::ClearLoading => self.git_status_loading = false,
            StatusEvent::SetLabel(label) => self.repo_name = label,
            StatusEvent::InitializeRemote => {
                self.branch_name = "remote".into();
                self.is_git_repo = false;
            }
        }
        Vec::new()
    }

    fn hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.is_git_repo.hash(&mut hasher);
        self.branch_name.hash(&mut hasher);
        // Repository name/root are deliberately excluded. List order is significant.
        for entries in [&self.staged, &self.unstaged, &self.untracked] {
            for entry in entries {
                entry.path.hash(&mut hasher);
                entry.status.hash(&mut hasher);
                entry.is_staged.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

fn next_interval(is_git_repo: bool, has_changes: bool, streak: u32) -> u64 {
    if !is_git_repo {
        return NON_REPO_MS;
    }
    if has_changes {
        return match streak {
            0..=2 => FAST_MS,
            3..=8 => MEDIUM_MS,
            _ => SLOW_MS,
        };
    }
    match streak {
        0..=1 => FAST_MS,
        2..=4 => MEDIUM_MS,
        5..=8 => SLOW_MS,
        _ => IDLE_MS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(branch: &str, dirty: bool, repo: bool) -> GitStatusSnapshot {
        GitStatusSnapshot {
            tab_id: 7,
            repo_path: "/fixture/repo".into(),
            repo_name: "fixture".into(),
            branch_name: branch.into(),
            is_git_repo: repo,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: if dirty {
                vec![GitFileStatus {
                    path: "new.txt".into(),
                    status: "?".into(),
                    is_staged: false,
                }]
            } else {
                Vec::new()
            },
            error: None,
        }
    }

    fn request(
        state: &mut GitStatusState,
        prelude: RequestPrelude,
        now: Instant,
    ) -> Vec<StatusCommand> {
        state.reduce(
            StatusEvent::Request {
                root: "/fixture/requested".into(),
                prelude,
            },
            now,
        )
    }

    #[test]
    fn overlapping_jobs_apply_in_both_arrival_orders() {
        let now = Instant::now();
        for order in [["old", "new"], ["new", "old"]] {
            let mut state = GitStatusState::new("fixture".into(), true, now);
            for _ in 0..2 {
                assert_eq!(
                    request(&mut state, RequestPrelude::Refresh, now),
                    [StatusCommand::CollectStatus {
                        root: "/fixture/requested".into()
                    }]
                );
                assert!(state.git_status_loading);
            }
            for branch in order {
                state.reduce(StatusEvent::Completed(snapshot(branch, false, true)), now);
                assert!(!state.git_status_loading);
                assert_eq!(state.branch_name, branch);
            }
            assert_eq!(state.branch_name, order[1]);
        }
    }

    #[test]
    fn errors_clear_loading_and_retain_last_good_status_in_both_orders() {
        let now = Instant::now();
        for error_first in [true, false] {
            let mut state = GitStatusState::new("fixture".into(), true, now);
            state.reduce(StatusEvent::Completed(snapshot("good", true, true)), now);
            let hash = state.last_git_status_hash;
            request(&mut state, RequestPrelude::Refresh, now);
            request(&mut state, RequestPrelude::Refresh, now);
            let mut error = snapshot("discard", false, false);
            error.error = Some("synthetic source failure".into());
            let good = snapshot("good", true, true);
            let arrivals = if error_first {
                [error, good]
            } else {
                [good, error]
            };
            for value in arrivals {
                let failed = value.error.is_some();
                let before = state.last_poll;
                state.reduce(StatusEvent::Completed(value), now + Duration::from_secs(1));
                assert!(!state.git_status_loading);
                assert_eq!(state.branch_name, "good");
                assert_eq!(state.last_git_status_hash, hash);
                assert_eq!(state.untracked.len(), 1);
                assert_eq!(
                    state.git_status_error.as_deref(),
                    failed.then_some("synthetic source failure")
                );
                if failed {
                    assert_eq!(state.last_poll, before);
                }
            }
        }
    }

    #[test]
    fn cadence_boundaries_match_characterized_clean_dirty_and_non_repo_traces() {
        let now = Instant::now();
        for (repo, dirty, expected) in [
            (
                true,
                true,
                [
                    5000, 5000, 5000, 10000, 10000, 10000, 10000, 10000, 10000, 15000,
                ],
            ),
            (
                true,
                false,
                [
                    5000, 5000, 10000, 10000, 10000, 15000, 15000, 15000, 15000, 30000,
                ],
            ),
            (false, false, [20000; 10]),
        ] {
            let mut state = GitStatusState::new("fixture".into(), repo, now);
            for (streak, interval) in expected.into_iter().enumerate() {
                let clock = now + Duration::from_secs(streak as u64);
                state.reduce(
                    StatusEvent::Completed(snapshot("branch", dirty, repo)),
                    clock,
                );
                assert_eq!(state.git_unchanged_streak, streak as u32);
                assert_eq!(state.git_poll_interval_ms, interval);
                assert_eq!(state.last_poll, clock);
            }
        }
    }

    #[test]
    fn hash_ignores_repo_identity_but_preserves_file_order_and_flags() {
        let now = Instant::now();
        let mut state = GitStatusState::new("fixture".into(), true, now);
        let mut value = snapshot("branch", true, true);
        value.untracked.push(GitFileStatus {
            path: "other".into(),
            status: "?".into(),
            is_staged: false,
        });
        state.reduce(StatusEvent::Completed(value.clone()), now);
        let hash = state.last_git_status_hash;
        value.repo_name = "renamed".into();
        value.repo_path = "/fixture/another-root".into();
        state.reduce(StatusEvent::Completed(value.clone()), now);
        assert_eq!(state.last_git_status_hash, hash);
        assert_eq!(state.git_unchanged_streak, 1);
        assert_eq!(state.repo_name, "renamed");
        value.untracked.reverse();
        state.reduce(StatusEvent::Completed(value.clone()), now);
        assert_ne!(state.last_git_status_hash, hash);
        assert_eq!(state.git_unchanged_streak, 0);
        let hash = state.last_git_status_hash;
        value.untracked[0].is_staged = true;
        state.reduce(StatusEvent::Completed(value), now);
        assert_ne!(state.last_git_status_hash, hash);
    }

    #[test]
    fn request_preludes_preserve_resume_and_tab_open_clock_behavior() {
        let now = Instant::now();
        for prelude in [
            RequestPrelude::Resume,
            RequestPrelude::TabOpened,
            RequestPrelude::Refresh,
        ] {
            let mut state = GitStatusState::new("fixture".into(), true, now);
            let before = state.last_poll;
            request(&mut state, prelude, now);
            assert_eq!(
                state.git_status_loading,
                !matches!(prelude, RequestPrelude::Resume)
            );
            assert_eq!(
                state.last_poll,
                if matches!(prelude, RequestPrelude::Refresh) {
                    now
                } else {
                    before
                }
            );
        }
    }

    #[test]
    fn visibility_deadlines_and_loading_guard_use_passed_clock() {
        let now = Instant::now();
        for (focused, elapsed, due) in [
            (true, 4999, false),
            (true, 5000, true),
            (false, 5000, false),
            (false, 15000, true),
        ] {
            let mut state = GitStatusState::new("fixture".into(), true, now);
            state.reduce(StatusEvent::Completed(snapshot("branch", true, true)), now);
            let commands = state.reduce(
                StatusEvent::VisibilityChanged {
                    focused,
                    root: "/fixture/root".into(),
                },
                now + Duration::from_millis(elapsed),
            );
            assert_eq!(state.git_status_loading, due);
            assert_eq!(
                commands,
                if due {
                    vec![StatusCommand::CollectStatus {
                        root: "/fixture/root".into(),
                    }]
                } else {
                    vec![StatusCommand::SchedulePoll {
                        after: Duration::from_millis(if focused { 5000 } else { 15000 } - elapsed),
                    }]
                }
            );
            request(&mut state, RequestPrelude::Refresh, now);
            assert!(state
                .reduce(
                    StatusEvent::VisibilityChanged {
                        focused,
                        root: "/fixture/root".into()
                    },
                    now + Duration::from_secs(60)
                )
                .is_empty());
        }
    }

    #[test]
    fn input_wake_reset_and_discard_keep_distinct_state_changes() {
        let now = Instant::now();
        let mut state = GitStatusState::new("fixture".into(), true, now);
        for _ in 0..10 {
            state.reduce(StatusEvent::Completed(snapshot("branch", false, true)), now);
        }
        let hash = state.last_git_status_hash;
        state.reduce(StatusEvent::Input, now);
        assert_eq!(
            (
                state.git_poll_interval_ms,
                state.git_unchanged_streak,
                state.last_git_status_hash,
                state.last_poll
            ),
            (5000, 0, hash, now)
        );
        state.reduce(StatusEvent::Completed(snapshot("branch", false, true)), now);
        state.reduce(StatusEvent::Input, now);
        assert_eq!(
            state.git_unchanged_streak, 1,
            "already-fast input keeps streak"
        );
        state.reduce(StatusEvent::Wake, now);
        assert_eq!(
            (
                state.git_poll_interval_ms,
                state.git_unchanged_streak,
                state.last_poll,
                state.last_git_status_hash
            ),
            (5000, 0, now - Duration::from_secs(60), hash)
        );
        state.reduce(StatusEvent::Reset, now);
        assert_eq!(state.last_git_status_hash, None);
        assert_eq!(state.branch_name, "branch");
        for clear_hash in [false, true] {
            state.reduce(StatusEvent::Completed(snapshot("branch", false, true)), now);
            request(&mut state, RequestPrelude::TabOpened, now);
            state.reduce(StatusEvent::SetError("old error".into()), now);
            state.reduce(StatusEvent::DiscardCompletion { clear_hash }, now);
            assert!(!state.git_status_loading);
            assert!(state.git_status_error.is_none());
            assert_eq!(state.last_git_status_hash.is_none(), clear_hash);
            assert_eq!(state.branch_name, "branch");
        }
    }
}
