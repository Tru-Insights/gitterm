use super::super::viewed::ViewedState;
use super::*;
use crate::git_facts::{BaseChoice, BaseSource};

fn page(id: &str) -> PageIdentity {
    PageIdentity {
        token: id.into(),
        nonce: format!("nonce-{id}"),
    }
}
fn target(path: &str) -> ReviewTarget {
    ReviewTarget {
        checkout: path.into(),
        repo: "fixture".into(),
        branch: "feature".into(),
        owner: "synthetic".into(),
        base: Some(BaseChoice {
            reference: "main".into(),
            source: BaseSource::Local,
            considered: 1,
        }),
    }
}
fn open(state: &mut State, path: &str, token: &str) -> CollectStamp {
    let effects = state.reduce(Intent::Open {
        target: target(path),
        source: ReviewSource::Uncommitted,
        hide_noise: true,
        focus: Some("a.rs".into()),
        page: page(token),
    });
    assert!(matches!(
        effects.as_slice(),
        [Effect::Render, Effect::Collect(_)]
    ));
    state.pane().unwrap().stamp()
}
fn load(stamp: &CollectStamp) -> ReviewLoad {
    let files = vec![ReviewFile {
        path: "a.rs".into(),
        old_path: None,
        status: FileStatus::Modified,
        kind: FileKind::Code,
        additions: 0,
        deletions: 0,
        truncated: false,
        omitted: false,
        note: None,
        hunks: Vec::new(),
        content_id: Some("blob".into()),
    }];
    ReviewLoad {
        diff: ReviewDiff {
            checkout: stamp.target.checkout.clone(),
            branch: stamp.target.branch.clone(),
            base: stamp.target.base.clone(),
            source: DiffSource::Uncommitted,
            merge_base: None,
            totals: ReviewTotals::of(&files),
            files,
            unlisted: 0,
        },
        commits: ["newest", "oldest"]
            .into_iter()
            .map(|sha| ReviewCommit {
                sha: sha.into(),
                short_sha: sha.into(),
                subject: sha.into(),
                author: "Fixture".into(),
                authored_at: 0,
            })
            .collect(),
        turns: vec![
            ReviewTurn {
                number: 1,
                start: Some("start".into()),
                end: Some("end".into()),
                timestamp: 0,
            },
            ReviewTurn {
                number: 2,
                start: Some("start".into()),
                end: None,
                timestamp: 0,
            },
        ],
        commits_truncated: true,
        commits_error: Some("fixture metadata".into()),
        canonical_checkout: stamp.target.checkout.clone(),
        live_paths: Some(Default::default()),
    }
}
fn ipc(state: &mut State, message: ReviewIpc, chat: Option<&str>) -> Vec<Effect> {
    state.reduce(Intent::IpcRequest {
        message,
        visible: true,
        active_chat: chat.map(str::to_string),
        page: page("next"),
    })
}

#[test]
fn stale_target_source_generation_and_closed_completions_have_no_effects() {
    let mut state = State::default();
    let old = open(&mut state, "/fixture/a", "a");
    let current = open(&mut state, "/fixture/b", "b");
    let file = load(&current).diff.files.remove(0);
    let mut marks = ViewedMarks::default();
    marks.set(&current.target.checkout, &file, true);
    state.reduce(Intent::MarksLoaded(Ok(marks.clone())));
    // Even a forged matching generation cannot retarget a completion.
    let mut wrong_target = current.clone();
    wrong_target.target = old.target.clone();
    let mut wrong_source = current.clone();
    wrong_source.source = ReviewSource::VsBase;
    let mut wrong_generation = current.clone();
    wrong_generation.generation = old.generation;
    for stamp in [old.clone(), wrong_target, wrong_source, wrong_generation] {
        for result in [Ok(load(&stamp)), Err("stale failure".into())] {
            assert!(state
                .reduce(Intent::CollectFinished {
                    stamp: stamp.clone(),
                    result: Box::new(result),
                })
                .is_empty());
            assert!(state.pane().unwrap().loaded.is_none());
            assert!(state.pane().unwrap().error.is_none());
            assert_eq!(state.marks().unwrap().as_ref().unwrap(), &marks);
        }
    }
    assert!(matches!(
        state
            .reduce(Intent::CollectFinished {
                stamp: current.clone(),
                result: Box::new(Ok(load(&current))),
            })
            .as_slice(),
        [Effect::SaveMarks, Effect::Render]
    ));
    state.reduce(Intent::Close);
    assert!(state
        .reduce(Intent::CollectFinished {
            stamp: current.clone(),
            result: Box::new(Ok(load(&current))),
        })
        .is_empty());
    let reopened = open(&mut state, "/fixture/b", "reopened");
    assert_ne!(current.generation, reopened.generation);
    assert!(state
        .reduce(Intent::CollectFinished {
            stamp: current.clone(),
            result: Box::new(Err("old incarnation".into())),
        })
        .is_empty());
    state.reduce(Intent::Collect {
        page: page("refresh"),
    });
    assert!(!state.accepts(&reopened));
}

#[test]
fn ipc_token_and_visibility_guard_every_command_after_target_switch() {
    let mut state = State::default();
    open(&mut state, "/fixture/a", "old");
    let stamp = open(&mut state, "/fixture/b", "current");
    for message in [
        ReviewIpc::SetSource {
            token: "old".into(),
            source: DiffSource::VsBase,
            hide_noise: false,
        },
        ReviewIpc::ShowTurns {
            token: "old".into(),
        },
        ReviewIpc::SelectTurn {
            token: "old".into(),
            number: 1,
        },
        ReviewIpc::ShowCommits {
            token: "old".into(),
        },
        ReviewIpc::SelectCommits {
            token: "old".into(),
            first: "a".into(),
            last: "b".into(),
            pending: false,
        },
        ReviewIpc::SetViewed {
            token: "old".into(),
            path: "a.rs".into(),
            viewed: true,
        },
        ReviewIpc::OpenFile {
            token: "old".into(),
            path: "a.rs".into(),
        },
        ReviewIpc::Close {
            token: "old".into(),
        },
    ] {
        assert!(matches!(
            ipc(&mut state, message, Some("chat")).as_slice(),
            [Effect::Rejected(_)]
        ));
        assert_eq!(state.pane().unwrap().stamp(), stamp);
    }
    let effects = state.reduce(Intent::IpcRequest {
        message: ReviewIpc::Close {
            token: "current".into(),
        },
        visible: false,
        active_chat: None,
        page: page("unused"),
    });
    assert!(matches!(effects.as_slice(), [Effect::Rejected(_)]));
    assert_eq!(state.pane().unwrap().stamp(), stamp);
}

#[test]
fn selection_preserves_focus_noise_and_captured_chat_and_rejects_unknown_members() {
    let mut state = State::default();
    let stamp = open(&mut state, "/fixture/a", "page");
    state.reduce(Intent::CollectFinished {
        stamp: stamp.clone(),
        result: Box::new(Ok(load(&stamp))),
    });
    let rejected = ipc(
        &mut state,
        ReviewIpc::SelectCommits {
            token: "page".into(),
            first: "unknown".into(),
            last: "newest".into(),
            pending: false,
        },
        None,
    );
    assert!(matches!(rejected.as_slice(), [Effect::Rejected(_)]));
    ipc(
        &mut state,
        ReviewIpc::SelectCommits {
            token: "page".into(),
            first: "newest".into(),
            last: "oldest".into(),
            pending: false,
        },
        None,
    );
    let pane = state.pane().unwrap();
    assert_eq!(
        pane.source,
        ReviewSource::Commits(Some(CommitRange {
            first: "oldest".into(),
            last: "newest".into(),
            pending: false
        }))
    );
    assert_eq!(pane.focus.as_deref(), Some("a.rs"));
    assert!(pane.hide_noise);
    ipc(
        &mut state,
        ReviewIpc::ShowTurns {
            token: "next".into(),
        },
        Some("original-chat"),
    );
    let stamp = state.pane().unwrap().stamp();
    state.reduce(Intent::CollectFinished {
        stamp: stamp.clone(),
        result: Box::new(Ok(load(&stamp))),
    });
    assert!(ipc(
        &mut state,
        ReviewIpc::SelectTurn {
            token: "next".into(),
            number: 2
        },
        Some("replacement-chat")
    )
    .is_empty());
    ipc(
        &mut state,
        ReviewIpc::SelectTurn {
            token: "next".into(),
            number: 1,
        },
        Some("replacement-chat"),
    );
    assert_eq!(
        state.pane().unwrap().source,
        ReviewSource::Turns {
            chat: "original-chat".into(),
            selected: Some(1)
        }
    );
}

#[test]
fn viewed_pruning_and_errors_keep_the_single_marks_authority() {
    let mut state = State::default();
    let stamp = open(&mut state, "/fixture/a", "page");
    let load = load(&stamp);
    let file = load.diff.files[0].clone();
    let mut marks = ViewedMarks::default();
    marks.set(&stamp.target.checkout, &file, true);
    state.reduce(Intent::MarksLoaded(Ok(marks)));
    assert!(matches!(
        state
            .reduce(Intent::CollectFinished {
                stamp: stamp.clone(),
                result: Box::new(Ok(load)),
            })
            .as_slice(),
        [Effect::SaveMarks, Effect::Render]
    ));
    assert_eq!(
        state
            .marks()
            .unwrap()
            .as_ref()
            .unwrap()
            .state(&stamp.target.checkout, &file),
        ViewedState::Unviewed
    );
    assert!(matches!(
        ipc(
            &mut state,
            ReviewIpc::SetViewed {
                token: "page".into(),
                path: "a.rs".into(),
                viewed: true
            },
            None
        )
        .as_slice(),
        [Effect::SaveMarks]
    ));
    assert_eq!(
        state
            .marks()
            .unwrap()
            .as_ref()
            .unwrap()
            .state(&stamp.target.checkout, &file),
        ViewedState::Viewed
    );
    state.reduce(Intent::MarksLoaded(Err("malformed store".into())));
    assert!(matches!(
        ipc(
            &mut state,
            ReviewIpc::SetViewed {
                token: "page".into(),
                path: "a.rs".into(),
                viewed: false
            },
            None
        )
        .as_slice(),
        [Effect::Rejected(_)]
    ));
    assert!(matches!(
        ipc(
            &mut state,
            ReviewIpc::OpenFile {
                token: "page".into(),
                path: "absent.rs".into()
            },
            None
        )
        .as_slice(),
        [Effect::Rejected(_)]
    ));
    assert!(matches!(
        ipc(
            &mut state,
            ReviewIpc::OpenFile {
                token: "page".into(),
                path: "a.rs".into()
            },
            None
        )
        .as_slice(),
        [Effect::OpenFile(_)]
    ));
}

#[test]
fn noise_filter_is_presentation_only_and_preserves_the_loaded_generation() {
    let mut state = State::default();
    assert!(state.reduce(Intent::SetHideNoise(false)).is_empty());
    let stamp = open(&mut state, "/tmp/checkout", "page");
    state.reduce(Intent::CollectFinished {
        stamp: stamp.clone(),
        result: Box::new(Ok(load(&stamp))),
    });
    assert!(matches!(
        state.reduce(Intent::SetHideNoise(false)).as_slice(),
        [Effect::Render]
    ));
    let pane = state.pane().unwrap();
    assert!(!pane.hide_noise);
    assert_eq!(pane.generation, stamp.generation);
    assert!(pane.loaded.is_some());
    assert_eq!(pane.token, "page");
    assert_eq!(pane.focus.as_deref(), Some("a.rs"));
}
