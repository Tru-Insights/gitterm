use super::*;

fn input(id: u64) -> Inputs {
    Inputs {
        workspace: id,
        source: Source::Owned(vec![format!("chat-{id}")]),
        tasks: vec![],
        focus: Focus::Latest,
        documents: Some(documents::ScanRequest::default()),
    }
}
fn summary_stamp(effects: &[Effect]) -> Stamp {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::BuildSummary { stamp, .. } => Some(stamp.clone()),
            _ => None,
        })
        .unwrap()
}
fn refresh_stamp(effects: &[Effect]) -> Stamp {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::Refresh { stamp, .. } => Some(stamp.clone()),
            _ => None,
        })
        .unwrap()
}
fn finish_summary(state: &mut State, stamp: Stamp, now: Instant) -> Vec<Effect> {
    state.reduce(Intent::SummaryFinished {
        stamp,
        result: Box::new(Ok(WorkSummary::default())),
        now,
        visible: true,
    })
}
fn finish_refresh(state: &mut State, stamp: Stamp, now: Instant) {
    state.reduce(Intent::RefreshFinished {
        stamp,
        result: RefreshOutcome {
            mode: RefreshMode::Full,
            branches: vec![],
            errors: vec![],
            git_runs: 0,
            skipped_unchanged: 0,
            took: Duration::ZERO,
        },
        now,
        visible: true,
    });
}
fn loaded() -> (State, Instant) {
    let now = Instant::now();
    let mut state = State::default();
    state.reduce(Intent::SetInputs(input(1)));
    let effects = state.reduce(Intent::Open {
        workspace: 1,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    });
    let effects = finish_summary(&mut state, summary_stamp(&effects), now);
    finish_refresh(&mut state, refresh_stamp(&effects), now);
    (state, now)
}
#[test]
fn freshness_matches_iced_boundaries() {
    assert_eq!(
        (
            VISIBLE_POLL.as_secs(),
            VISIBLE_SLOW_POLL.as_secs(),
            HIDDEN_POLL.as_secs(),
            REOPEN_FRESH.as_secs(),
            DOCUMENTS_POLL.as_secs()
        ),
        (15, 30, 60, 5, 60)
    );
    let (mut state, now) = loaded();
    let poll = |now, visible| Intent::Poll {
        workspace: 1,
        visible,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    };
    assert!(state
        .reduce(poll(now + VISIBLE_POLL - Duration::from_nanos(1), true))
        .is_empty());
    let effects = state.reduce(poll(now + VISIBLE_POLL, true));
    finish_refresh(&mut state, refresh_stamp(&effects), now + VISIBLE_POLL);
    assert_eq!(state.pane(1).unwrap().unchanged_streak, 2);
    assert!(state.reduce(poll(now + VISIBLE_POLL * 2, true)).is_empty());
    let effects = state.reduce(poll(now + VISIBLE_POLL + VISIBLE_SLOW_POLL, true));
    finish_refresh(&mut state, refresh_stamp(&effects), now);
    assert!(state
        .reduce(poll(now + HIDDEN_POLL - Duration::from_nanos(1), false))
        .is_empty());
    let effects = state.reduce(poll(now + HIDDEN_POLL, false));
    assert!(matches!(
        &effects[0],
        Effect::Refresh {
            plan: RefreshPlan {
                mode: RefreshMode::Gated,
                ..
            },
            ..
        }
    ));
}
#[test]
fn reopening_and_index_reload_preserve_refresh_gates() {
    let (mut state, now) = loaded();
    let open = |now| Intent::Open {
        workspace: 1,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    };
    let effects = state.reduce(open(now + REOPEN_FRESH - Duration::from_nanos(1)));
    assert!(finish_summary(
        &mut state,
        summary_stamp(&effects),
        now + REOPEN_FRESH - Duration::from_nanos(1)
    )
    .is_empty());
    let effects = state.reduce(open(now + REOPEN_FRESH));
    let effects = finish_summary(&mut state, summary_stamp(&effects), now + REOPEN_FRESH);
    finish_refresh(&mut state, refresh_stamp(&effects), now + REOPEN_FRESH);
    let effects = state.reduce(Intent::IndexChanged {
        visible: Some(1),
        wall_time: SystemTime::UNIX_EPOCH,
    });
    assert!(finish_summary(&mut state, summary_stamp(&effects), now + HIDDEN_POLL).is_empty());
    state.reduce(Intent::IndexChanged {
        visible: None,
        wall_time: SystemTime::UNIX_EPOCH,
    });
    assert!(state.pane(1).unwrap().summary_stale);
    assert!(state
        .reduce(Intent::Poll {
            workspace: 1,
            visible: false,
            now,
            wall_time: SystemTime::UNIX_EPOCH
        })
        .is_empty());
}
#[test]
fn stale_summary_refresh_documents_and_removed_workspace_are_ignored() {
    let mut state = State::default();
    let now = Instant::now();
    state.reduce(Intent::SetInputs(input(1)));
    let old = state.reduce(Intent::Open {
        workspace: 1,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    });
    let old_summary = summary_stamp(&old);
    let old_docs = old
        .iter()
        .find_map(|e| match e {
            Effect::ScanDocuments { stamp, .. } => Some(stamp.clone()),
            _ => None,
        })
        .unwrap();
    let newer = state.reduce(Intent::IndexChanged {
        visible: Some(1),
        wall_time: SystemTime::UNIX_EPOCH,
    });
    assert!(finish_summary(&mut state, old_summary, now).is_empty());
    assert!(state.pane(1).unwrap().summary_stale);
    let refresh = finish_summary(&mut state, summary_stamp(&newer), now);
    let old_refresh = refresh_stamp(&refresh);
    state.reduce(Intent::Remove(1));
    state.reduce(Intent::SetInputs(input(1)));
    finish_refresh(&mut state, old_refresh, now);
    state.reduce(Intent::DocumentsFinished {
        stamp: old_docs,
        result: Ok(documents::ScanResult::default()),
        now,
    });
    assert!(state.pane(1).unwrap().last_refresh.is_none());
    assert!(state.documents(1).is_none());
}
#[test]
fn documents_complete_for_background_owner_and_keep_data_on_failure() {
    let now = Instant::now();
    let mut state = State::default();
    state.reduce(Intent::SetInputs(input(1)));
    state.reduce(Intent::SetInputs(input(2)));
    let scan = state.reduce(Intent::ScanDocuments(1));
    assert!(state.reduce(Intent::ScanDocuments(1)).is_empty());
    let Effect::ScanDocuments { stamp, .. } = &scan[0] else {
        panic!()
    };
    state.reduce(Intent::DocumentsFinished {
        stamp: stamp.clone(),
        result: Ok(documents::ScanResult::default()),
        now,
    });
    assert!(state.documents(1).unwrap().result.is_some());
    assert!(state.documents(2).is_none());
    let scan = state.reduce(Intent::ScanDocuments(1));
    let Effect::ScanDocuments { stamp, .. } = &scan[0] else {
        panic!()
    };
    state.reduce(Intent::DocumentsFinished {
        stamp: stamp.clone(),
        result: Err("synthetic failure".into()),
        now,
    });
    assert!(state.documents(1).unwrap().result.is_some());
    assert_eq!(
        state.documents(1).unwrap().error.as_deref(),
        Some("synthetic failure")
    );
    let mut remote = input(3);
    remote.documents = None;
    state.reduce(Intent::SetInputs(remote));
    assert!(state.reduce(Intent::ScanDocuments(3)).is_empty());
}

#[test]
fn folder_summaries_share_data_but_documents_keep_workspace_identity() {
    let now = Instant::now();
    let mut state = State::default();
    for id in [1, 2] {
        let mut input = input(id);
        input.source = Source::Folder(PathBuf::from("/synthetic/repo"));
        state.reduce(Intent::SetInputs(input));
    }
    let effects = state.reduce(Intent::Open {
        workspace: 1,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    });
    finish_summary(&mut state, summary_stamp(&effects), now);
    assert!(!state.pane(2).unwrap().summary_stale);
    assert!(std::ptr::eq(state.pane(1).unwrap(), state.pane(2).unwrap()));
    state.reduce(Intent::Remove(1));
    assert!(state.pane(2).is_some());
    assert!(state.documents(2).is_none());
}

#[test]
fn removing_a_shared_folder_job_owner_releases_loading_for_the_remaining_owner() {
    let mut state = State::default();
    let now = Instant::now();
    for id in [1, 2] {
        let mut input = input(id);
        input.source = Source::Folder(PathBuf::from("/fixture/shared"));
        state.reduce(Intent::SetInputs(input));
    }
    let old = state.reduce(Intent::Open {
        workspace: 1,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    });
    state.reduce(Intent::Remove(1));
    assert!(finish_summary(&mut state, summary_stamp(&old), now).is_empty());
    let effects = state.reduce(Intent::Poll {
        workspace: 2,
        visible: true,
        now,
        wall_time: SystemTime::UNIX_EPOCH,
    });
    let refresh = finish_summary(&mut state, summary_stamp(&effects), now);
    assert!(matches!(&refresh[0], Effect::Refresh { .. }));
}
