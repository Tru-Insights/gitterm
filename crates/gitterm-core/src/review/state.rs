//! Review authority. Hosts execute effects, then return the original collection
//! stamp; neither focus nor a replacement target may retarget a completion.
use std::path::PathBuf;

use super::{model::*, viewed::ViewedMarks};
use crate::git_facts::DiffSource;

/// Rotated by the host on each new page. Tokens bind IPC to this exact review;
/// nonces are only a rendering capability, never a completion identity.
#[derive(Debug, Clone)]
pub struct PageIdentity {
    pub token: String,
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectStamp {
    pub target: ReviewTarget,
    pub source: ReviewSource,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct Pane {
    pub target: ReviewTarget,
    pub source: ReviewSource,
    pub commits: Vec<ReviewCommit>,
    pub turns: Vec<ReviewTurn>,
    pub commits_truncated: bool,
    pub commits_error: Option<String>,
    pub generation: u64,
    pub token: String,
    pub nonce: String,
    pub hide_noise: bool,
    pub focus: Option<String>,
    pub loaded: Option<(ReviewDiff, PathBuf)>,
    pub error: Option<String>,
}
impl Pane {
    pub fn stamp(&self) -> CollectStamp {
        CollectStamp {
            target: self.target.clone(),
            source: self.source.clone(),
            generation: self.generation,
        }
    }
}

/// Parsed page commands. Wire parsing remains in the frontend adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewIpc {
    SetSource {
        token: String,
        source: DiffSource,
        hide_noise: bool,
    },
    ShowTurns {
        token: String,
    },
    SelectTurn {
        token: String,
        number: u64,
    },
    ShowCommits {
        token: String,
    },
    SelectCommits {
        token: String,
        first: String,
        last: String,
        pending: bool,
    },
    SetViewed {
        token: String,
        path: String,
        viewed: bool,
    },
    Close {
        token: String,
    },
    OpenFile {
        token: String,
        path: String,
    },
}
impl ReviewIpc {
    pub fn token(&self) -> &str {
        match self {
            Self::SetSource { token, .. }
            | Self::ShowTurns { token }
            | Self::SelectTurn { token, .. }
            | Self::ShowCommits { token }
            | Self::SelectCommits { token, .. }
            | Self::SetViewed { token, .. }
            | Self::Close { token }
            | Self::OpenFile { token, .. } => token,
        }
    }
}

#[derive(Debug)]
pub enum Intent {
    Open {
        target: ReviewTarget,
        source: ReviewSource,
        hide_noise: bool,
        focus: Option<String>,
        page: PageIdentity,
    },
    SelectBase {
        source: DiffSource,
        hide_noise: bool,
        page: PageIdentity,
    },
    SelectCommit {
        first: String,
        last: String,
        pending: bool,
        page: PageIdentity,
    },
    Collect {
        page: PageIdentity,
    },
    CollectFinished {
        stamp: CollectStamp,
        result: Box<Result<ReviewLoad, String>>,
    },
    ToggleViewed {
        path: String,
        viewed: bool,
    },
    MarksLoaded(Result<ViewedMarks, String>),
    IpcRequest {
        message: ReviewIpc,
        visible: bool,
        active_chat: Option<String>,
        page: PageIdentity,
    },
    Close,
}

#[derive(Debug)]
pub enum Effect {
    Render,
    Collect(CollectStamp),
    SaveMarks,
    Close,
    OpenFile(PathBuf),
    Rejected(String),
}

#[derive(Debug, Default)]
pub struct State {
    pane: Option<Pane>,
    generation: u64,
    marks: Option<Result<ViewedMarks, String>>,
}
impl State {
    pub fn pane(&self) -> Option<&Pane> {
        self.pane.as_ref()
    }
    pub fn marks(&self) -> Option<&Result<ViewedMarks, String>> {
        self.marks.as_ref()
    }
    pub fn accepts_ipc(&self, message: &ReviewIpc, visible: bool) -> bool {
        visible
            && self
                .pane
                .as_ref()
                .is_some_and(|pane| pane.token == message.token())
    }
    pub fn accepts(&self, stamp: &CollectStamp) -> bool {
        self.pane
            .as_ref()
            .is_some_and(|pane| pane.stamp() == *stamp)
    }
    fn reopen(
        &mut self,
        source: ReviewSource,
        hide_noise: bool,
        page: PageIdentity,
    ) -> Vec<Effect> {
        let Some(pane) = self.pane.as_ref() else {
            return Vec::new();
        };
        self.reduce(Intent::Open {
            target: pane.target.clone(),
            source,
            hide_noise,
            focus: pane.focus.clone(),
            page,
        })
    }
    pub fn reduce(&mut self, intent: Intent) -> Vec<Effect> {
        match intent {
            Intent::Open {
                target,
                source,
                hide_noise,
                focus,
                page,
            } => {
                self.generation += 1;
                self.pane = Some(Pane {
                    target,
                    source,
                    commits: Vec::new(),
                    turns: Vec::new(),
                    commits_truncated: false,
                    commits_error: None,
                    generation: self.generation,
                    token: page.token,
                    nonce: page.nonce,
                    hide_noise,
                    focus,
                    loaded: None,
                    error: None,
                });
                vec![
                    Effect::Render,
                    Effect::Collect(self.pane.as_ref().unwrap().stamp()),
                ]
            }
            Intent::Collect { page } => {
                let Some(pane) = self.pane.as_ref() else {
                    return Vec::new();
                };
                self.reopen(pane.source.clone(), pane.hide_noise, page)
            }
            Intent::SelectBase {
                source,
                hide_noise,
                page,
            } => {
                let Some(pane) = self.pane.as_ref() else {
                    return Vec::new();
                };
                if source == DiffSource::VsBase && pane.target.base.is_none() {
                    return vec![Effect::Rejected(format!(
                        "{} has no base to compare with",
                        pane.target.checkout.display()
                    ))];
                }
                self.reopen(source.into(), hide_noise, page)
            }
            Intent::SelectCommit {
                first,
                last,
                pending,
                page,
            } => {
                let Some(pane) = self.pane.as_ref() else {
                    return Vec::new();
                };
                let first = pane.commits.iter().position(|commit| commit.sha == first);
                let last = pane.commits.iter().position(|commit| commit.sha == last);
                let (Some(first), Some(last)) = (first, last) else {
                    return vec![Effect::Rejected(
                        "selected commit is not in the loaded branch commit list".into(),
                    )];
                };
                let range = CommitRange {
                    first: pane.commits[first.max(last)].sha.clone(),
                    last: pane.commits[first.min(last)].sha.clone(),
                    pending,
                };
                self.reopen(ReviewSource::Commits(Some(range)), pane.hide_noise, page)
            }
            Intent::CollectFinished { stamp, result } => {
                if !self.accepts(&stamp) {
                    return Vec::new();
                }
                let pane = self.pane.as_mut().unwrap();
                let mut effects = Vec::new();
                match *result {
                    Ok(load) => {
                        if let (Some(live), Some(Ok(marks))) = (&load.live_paths, &mut self.marks) {
                            if marks.prune(&load.canonical_checkout, live) {
                                effects.push(Effect::SaveMarks);
                            }
                        }
                        pane.commits = load.commits;
                        pane.turns = load.turns;
                        pane.commits_truncated = load.commits_truncated;
                        pane.commits_error = load.commits_error;
                        pane.loaded = Some((load.diff, load.canonical_checkout));
                        pane.error = None;
                    }
                    Err(error) => {
                        pane.loaded = None;
                        pane.error = Some(error);
                    }
                }
                effects.push(Effect::Render);
                effects
            }
            Intent::MarksLoaded(marks) => {
                self.marks = Some(marks);
                Vec::new()
            }
            Intent::ToggleViewed { path, viewed } => {
                let Some((file, canonical)) = self
                    .pane
                    .as_ref()
                    .and_then(|pane| pane.loaded.as_ref())
                    .and_then(|(diff, canonical)| diff.file(&path).map(|file| (file, canonical)))
                else {
                    return vec![Effect::Rejected(format!(
                        "viewed: {path} is not in the review"
                    ))];
                };
                match self.marks.as_mut() {
                    Some(Ok(marks)) => {
                        if marks.set(canonical, file, viewed) {
                            vec![Effect::SaveMarks]
                        } else {
                            vec![Effect::Rejected(format!(
                                "{path} has no content hash; not marked viewed"
                            ))]
                        }
                    }
                    Some(Err(error)) => vec![Effect::Rejected(format!(
                        "viewed marks unavailable: {error}"
                    ))],
                    None => vec![Effect::Rejected(
                        "viewed marks unavailable: not loaded".into(),
                    )],
                }
            }
            Intent::Close => {
                self.pane = None;
                Vec::new()
            }
            Intent::IpcRequest {
                message,
                visible,
                active_chat,
                page,
            } => {
                if !self.accepts_ipc(&message, visible) {
                    return vec![Effect::Rejected(
                        "dropping a message for a review pane that is not open".into(),
                    )];
                }
                let pane = self.pane.as_ref().unwrap();
                match message {
                    ReviewIpc::SetSource {
                        source, hide_noise, ..
                    } => self.reduce(Intent::SelectBase {
                        source,
                        hide_noise,
                        page,
                    }),
                    ReviewIpc::SelectCommits {
                        first,
                        last,
                        pending,
                        ..
                    } => self.reduce(Intent::SelectCommit {
                        first,
                        last,
                        pending,
                        page,
                    }),
                    ReviewIpc::SetViewed { path, viewed, .. } => {
                        self.reduce(Intent::ToggleViewed { path, viewed })
                    }
                    ReviewIpc::Close { .. } => {
                        self.pane = None;
                        vec![Effect::Close]
                    }
                    ReviewIpc::ShowCommits { .. } => {
                        if pane.target.base.is_none() {
                            return vec![Effect::Rejected(
                                "commits unavailable without a branch base".into(),
                            )];
                        }
                        self.reopen(ReviewSource::Commits(None), pane.hide_noise, page)
                    }
                    ReviewIpc::ShowTurns { .. } => {
                        let Some(chat) = active_chat else {
                            return vec![Effect::Rejected("Turns needs an active chat".into())];
                        };
                        self.reopen(
                            ReviewSource::Turns {
                                chat,
                                selected: None,
                            },
                            pane.hide_noise,
                            page,
                        )
                    }
                    ReviewIpc::SelectTurn { number, .. } => {
                        let ReviewSource::Turns { chat, .. } = &pane.source else {
                            return Vec::new();
                        };
                        if !pane.turns.iter().any(|turn| {
                            turn.number == number && turn.start.is_some() && turn.end.is_some()
                        }) {
                            return Vec::new();
                        }
                        self.reopen(
                            ReviewSource::Turns {
                                chat: chat.clone(),
                                selected: Some(number),
                            },
                            pane.hide_noise,
                            page,
                        )
                    }
                    ReviewIpc::OpenFile { path, .. } => {
                        let Some(file) =
                            pane.loaded.as_ref().and_then(|(diff, _)| diff.file(&path))
                        else {
                            return vec![Effect::Rejected(format!(
                                "open: {path} is not in the review"
                            ))];
                        };
                        if file.status == FileStatus::Deleted {
                            return vec![Effect::Rejected(format!("open: {path} was deleted"))];
                        }
                        vec![Effect::OpenFile(pane.target.checkout.join(path))]
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
