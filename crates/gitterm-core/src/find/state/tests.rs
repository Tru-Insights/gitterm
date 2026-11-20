use super::*;

#[test]
fn index_freshness_and_stale_source_completion() {
    let mut state = State::<ChatPreview>::default();
    let now = Instant::now();
    let load = |now| Intent::LoadIndex {
        machine: None,
        now,
        force: false,
    };
    let effects = state.reduce(load(now));
    let Effect::LoadIndex(old) = &effects[0] else {
        panic!()
    };
    assert!(state.reduce(load(now)).is_empty());
    state.reduce(Intent::RemoveMachine(None));
    let effects = state.reduce(load(now));
    let Effect::LoadIndex(current) = &effects[0] else {
        panic!()
    };
    assert!(state
        .reduce(Intent::IndexFinished {
            stamp: old.clone(),
            result: Ok(vec![]),
            now
        })
        .is_empty());
    assert!(state.loading_index(&None));
    state.reduce(Intent::IndexFinished {
        stamp: current.clone(),
        result: Ok(vec![]),
        now,
    });
    assert!(state
        .reduce(load(now + INDEX_FRESH - Duration::from_nanos(1)))
        .is_empty());
    assert!(matches!(
        &state.reduce(load(now + INDEX_FRESH))[0],
        Effect::LoadIndex(_)
    ));
}
#[test]
fn preview_identity_includes_machine_backend_path_and_generation() {
    let mut state = State::<ChatPreview>::default();
    let select = |machine| Intent::Select {
        owner: "tab-1".into(),
        chat: ChatRef::new(machine, "same-id"),
        path: PathBuf::from("/synthetic/chat.jsonl"),
        backend: ChatBackend::Claude,
    };
    let effects = state.reduce(select(None));
    let Effect::LoadPreview(old) = &effects[0] else {
        panic!()
    };
    let effects = state.reduce(select(Some("fixture-remote")));
    let Effect::LoadPreview(current) = &effects[0] else {
        panic!()
    };
    state.reduce(Intent::PreviewFinished {
        stamp: old.clone(),
        result: Err("old error".into()),
    });
    assert!(state.preview_error("tab-1").is_none());
    let mut wrong = current.clone();
    wrong.path = PathBuf::from("/other");
    state.reduce(Intent::PreviewFinished {
        stamp: wrong,
        result: Err("wrong path".into()),
    });
    let mut wrong = current.clone();
    wrong.backend = ChatBackend::Codex;
    state.reduce(Intent::PreviewFinished {
        stamp: wrong,
        result: Err("wrong backend".into()),
    });
    assert!(state.preview_error("tab-1").is_none());
    state.reduce(Intent::PreviewFinished {
        stamp: current.clone(),
        result: Ok(ChatPreview::default()),
    });
    assert!(state.preview("tab-1").is_some());
    state.reduce(Intent::ClosePreview("tab-1".into()));
    state.reduce(select(Some("fixture-remote")));
    state.reduce(Intent::PreviewFinished {
        stamp: current.clone(),
        result: Err("old generation".into()),
    });
    assert!(state.preview_error("tab-1").is_none());
    state.reduce(Intent::RemoveMachine(Some("fixture-remote".into())));
    assert!(state.selected("tab-1").is_none());
}

#[test]
fn tab_selections_survive_switching_and_source_replacement_invalidates_them() {
    let mut state = State::<ChatPreview>::default();
    let select = |owner: &str| Intent::Select {
        owner: owner.into(),
        chat: ChatRef::new(Some("fixture-remote"), "chat"),
        path: PathBuf::from("/fixture/chat.jsonl"),
        backend: ChatBackend::Claude,
    };
    let first = state.reduce(select("tab-1"));
    state.reduce(select("tab-2"));
    let Effect::LoadPreview(first) = &first[0] else {
        panic!()
    };
    state.reduce(Intent::PreviewFinished {
        stamp: first.clone(),
        result: Ok(ChatPreview::default()),
    });
    assert!(state.preview("tab-1").is_some());
    assert!(state.selected("tab-2").is_some());
    state.reduce(Intent::ClosePreview("tab-2".into()));
    assert!(state.selected("tab-1").is_some());
    state.reduce(Intent::SourceChanged(Some("fixture-remote".into())));
    state.reduce(select("tab-1"));
    state.reduce(Intent::PreviewFinished {
        stamp: first.clone(),
        result: Err("stale source".into()),
    });
    assert!(state.preview_error("tab-1").is_none());
}

#[test]
fn remote_index_uses_sixty_second_freshness_and_query_does_not_own_index() {
    let mut state = State::<ChatPreview>::default();
    let now = Instant::now();
    let load = |now| Intent::LoadIndex {
        machine: Some("fixture-remote".into()),
        now,
        force: false,
    };
    let effects = state.reduce(load(now));
    let Effect::LoadIndex(stamp) = &effects[0] else {
        panic!()
    };
    state.reduce(Intent::SetFilter(Filter {
        query: "different query".into(),
        ..Filter::default()
    }));
    state.reduce(Intent::IndexFinished {
        stamp: stamp.clone(),
        result: Ok(vec![]),
        now,
    });
    assert!(state
        .reduce(load(now + REMOTE_INDEX_FRESH - Duration::from_nanos(1)))
        .is_empty());
    assert!(matches!(
        &state.reduce(load(now + REMOTE_INDEX_FRESH))[0],
        Effect::LoadIndex(_)
    ));
}
