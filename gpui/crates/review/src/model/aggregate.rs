//! Native aggregate review is a read-only projection of the shared authority.
use gitterm_core::{
    git_facts::{BaseChoice, DiffSource},
    review::{
        budget::COMMIT_LIST_CAP,
        model::{ReviewFile, ReviewSource},
        state::{Effect, Intent, PageIdentity, Pane, ReviewIpc, State},
        viewed::ViewedState,
    },
};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct AggregateModel {
    state: State,
}

pub struct FileProjection<'a> {
    pub file: &'a ReviewFile,
    pub viewed: ViewedState,
}

pub struct AggregateProjection<'a> {
    pub pane: Option<&'a Pane>,
    pub files: Vec<FileProjection<'a>>,
    pub notices: Vec<String>,
    pub marks_error: Option<&'a str>,
}

impl AggregateModel {
    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn dispatch(&mut self, intent: Intent) -> Vec<Effect> {
        self.state.reduce(intent)
    }

    /// Native pages do not expose IPC, but the shared reducer still owns their identity.
    pub fn page() -> PageIdentity {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        PageIdentity {
            token: format!("native-review-{id}"),
            nonce: format!("native-review-{id}"),
        }
    }

    pub fn source_intent(&self, source: DiffSource) -> Intent {
        Intent::SelectBase {
            source,
            hide_noise: self.state.pane().is_some_and(|pane| pane.hide_noise),
            page: Self::page(),
        }
    }

    /// SelectBase changes the diff source, not the target's branch reference.
    /// Reopen through the authority so a branch change also invalidates old jobs.
    pub fn base_intent(&self, base: BaseChoice) -> Option<Intent> {
        let pane = self.state.pane()?;
        let mut target = pane.target.clone();
        target.base = Some(base);
        Some(Intent::Open {
            target,
            source: ReviewSource::VsBase,
            hide_noise: pane.hide_noise,
            focus: pane.focus.clone(),
            page: Self::page(),
        })
    }

    pub fn commits_intent(&self) -> Option<Intent> {
        let pane = self.state.pane()?;
        Some(Intent::IpcRequest {
            message: ReviewIpc::ShowCommits {
                token: pane.token.clone(),
            },
            visible: true,
            active_chat: None,
            page: Self::page(),
        })
    }

    /// Same request Iced's Turns button sends; the reducer rejects it without a chat.
    pub fn turns_intent(&self, active_chat: Option<String>) -> Option<Intent> {
        let pane = self.state.pane()?;
        Some(Intent::IpcRequest {
            message: ReviewIpc::ShowTurns {
                token: pane.token.clone(),
            },
            visible: true,
            active_chat,
            page: Self::page(),
        })
    }

    pub fn turn_intent(&self, number: u64) -> Option<Intent> {
        let pane = self.state.pane()?;
        Some(Intent::IpcRequest {
            message: ReviewIpc::SelectTurn {
                token: pane.token.clone(),
                number,
            },
            visible: true,
            active_chat: None,
            page: Self::page(),
        })
    }

    /// Iced's per-file Open request: the reducer resolves the path and refuses deleted files.
    pub fn open_file_intent(&self, path: &str) -> Option<Intent> {
        let pane = self.state.pane()?;
        Some(Intent::IpcRequest {
            message: ReviewIpc::OpenFile {
                token: pane.token.clone(),
                path: path.into(),
            },
            visible: true,
            active_chat: None,
            page: Self::page(),
        })
    }

    pub fn commit_intent(&self, sha: String) -> Intent {
        let first = match self.state.pane().map(|pane| &pane.source) {
            Some(ReviewSource::Commits(Some(range))) if range.pending => Some(range.first.clone()),
            _ => None,
        };
        Intent::SelectCommit {
            pending: first.is_none(),
            first: first.unwrap_or_else(|| sha.clone()),
            last: sha,
            page: Self::page(),
        }
    }

    pub fn viewed_intent(&self, path: &str) -> Option<Intent> {
        let file = self
            .projection()
            .files
            .into_iter()
            .find(|row| row.file.path == path)?;
        Some(Intent::ToggleViewed {
            path: path.into(),
            viewed: file.viewed != ViewedState::Viewed,
        })
    }

    pub fn projection(&self) -> AggregateProjection<'_> {
        let pane = self.state.pane();
        let marks = self.state.marks().and_then(|marks| marks.as_ref().ok());
        let mut notices = Vec::new();
        let files =
            pane.and_then(|pane| pane.loaded.as_ref())
                .map_or_else(Vec::new, |(diff, checkout)| {
                    if diff.unlisted > 0 {
                        notices.push(format!(
                            "{} changed files are not listed (file-list limit).",
                            diff.unlisted
                        ));
                    }
                    let omitted = diff.files.iter().filter(|file| file.omitted).count();
                    let truncated = diff.files.iter().filter(|file| file.truncated).count();
                    if omitted > 0 {
                        notices.push(format!(
                            "{omitted} files omitted from display by the review budget."
                        ));
                    }
                    if truncated > 0 {
                        notices.push(format!(
                            "{truncated} file diffs truncated at the review text limits."
                        ));
                    }
                    diff.files
                        .iter()
                        .filter(|file| {
                            !pane.is_some_and(|pane| pane.hide_noise && file.kind.is_noise())
                        })
                        .map(|file| FileProjection {
                            file,
                            viewed: marks
                                .map_or(ViewedState::Unviewed, |marks| marks.state(checkout, file)),
                        })
                        .collect()
                });
        if let Some(pane) = pane {
            if pane.commits_truncated {
                notices.push(format!(
                    "Showing the newest {COMMIT_LIST_CAP} commits; older commits are omitted."
                ));
            }
            if let Some(error) = &pane.commits_error {
                notices.push(format!("Cannot load commits: {error}"));
            }
        }
        AggregateProjection {
            pane,
            files,
            notices,
            marks_error: self
                .state
                .marks()
                .and_then(|marks| marks.as_ref().err())
                .map(String::as_str),
        }
    }
}

#[cfg(test)]
mod tests;
