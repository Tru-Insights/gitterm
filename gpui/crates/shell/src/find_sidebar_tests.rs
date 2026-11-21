use super::*;
use gitterm_core::{
    chats::{ChatPreview, ChatPreviewMessage},
    find::state::{Effect, Filter},
};
use gpui::{TestAppContext, VisualTestContext};
use std::time::{Duration, Instant, SystemTime};

fn chat(id: &str, repo: &str, backend: ChatBackend) -> ChatIndexEntry {
    ChatIndexEntry {
        id: id.into(),
        backend,
        path: format!("/fixture/{id}.jsonl").into(),
        cwd: repo.into(),
        repo_root: None,
        is_worktree: false,
        branch: Some("main".into()),
        title: format!("Title {id}"),
        latest: Some(format!("Latest {id}")),
        mtime: SystemTime::now() - Duration::from_secs(3 * 3600),
        size: 2048,
        dead_cwd: false,
        facts: None,
        root_id: None,
    }
}

fn load(state: &mut State, machine: Machine, entries: Vec<ChatIndexEntry>) {
    let now = Instant::now();
    let Effect::LoadIndex(stamp) = state
        .reduce(Intent::LoadIndex {
            machine,
            now,
            force: false,
        })
        .remove(0)
    else {
        panic!("index effect")
    };
    state.reduce(Intent::IndexFinished {
        stamp,
        result: Ok(entries),
        now,
    });
}

fn fixture() -> (State, FindFacts) {
    let mut state = State::default();
    load(
        &mut state,
        Some("remote".into()),
        vec![chat("same", "/other/repo", ChatBackend::Pi)],
    );
    load(
        &mut state,
        None,
        vec![
            chat("first", "/work/api", ChatBackend::Claude),
            chat("second", "/other/web", ChatBackend::Codex),
            chat("same", "/work/api", ChatBackend::Pi),
        ],
    );
    state.reduce(Intent::SetFilter(Filter {
        workspace_root: Some("/work".into()),
        ..Filter::default()
    }));
    (
        state,
        FindFacts {
            machines: vec![
                MachineFacts {
                    id: None,
                    name: "This Mac".into(),
                    status: String::new(),
                    reachable: true,
                    claude_chat: true,
                },
                MachineFacts {
                    id: Some("remote".into()),
                    name: "Build machine".into(),
                    status: "connected".into(),
                    reachable: true,
                    claude_chat: true,
                },
                MachineFacts {
                    id: Some("offline".into()),
                    name: "Offline".into(),
                    status: "disconnected".into(),
                    reachable: false,
                    claude_chat: false,
                },
            ],
            local_files_openable: true,
            ..FindFacts::default()
        },
    )
}

fn chats(rows: Vec<Row>) -> Vec<ChatRow> {
    rows.into_iter()
        .filter_map(|row| {
            if let Row::Chat(row) = row {
                Some(row)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn rows_use_core_order_iced_repo_groups_and_labels() {
    let (state, facts) = fixture();
    let rows = rows(&state, "tab", &facts, &Disclosures::default());
    let groups: Vec<_> = rows
        .iter()
        .filter_map(|r| {
            if let Row::Group { label, count } = r {
                Some((label.as_str(), *count))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(groups, [("API", 2), ("WEB", 1)]);
    let rows = chats(rows);
    assert_eq!(
        rows.iter().map(|r| r.id.id.as_str()).collect::<Vec<_>>(),
        ["first", "same", "second"]
    );
    assert_eq!(
        (
            rows[0].title.as_str(),
            rows[0].age.as_str(),
            rows[0].backend.label()
        ),
        ("Title first", "3h", "claude")
    );
    assert_eq!(rows[0].details, ["↳ Latest first"]);
    assert_eq!(rows[0].branch.as_deref(), Some("main"));
    assert_eq!(rows[2].backend.label(), "codex");
}

#[test]
fn scopes_and_backend_chips_produce_filter_outcomes_preserving_host_axes() {
    let (mut state, facts) = fixture();
    assert_eq!(
        scopes(&state, &facts)
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        ["This workspace", "This Mac", "Everywhere"]
    );
    for control in scopes(&state, &facts).into_iter().chain(backends()) {
        let old = state.filter().clone();
        let Intent::SetFilter(filter) = control.outcome.intent(&state, "tab").unwrap() else {
            panic!("filter")
        };
        assert_eq!(filter.machine, old.machine);
        assert_eq!(filter.workspace_root, old.workspace_root);
        assert_eq!(filter.query, old.query);
        match control.outcome {
            FindOutcome::SetScope(scope) => {
                assert_eq!(filter.scope, scope);
                assert_eq!(filter.backend, old.backend);
            }
            FindOutcome::SetBackend(backend) => {
                assert_eq!(filter.backend, backend);
                assert_eq!(filter.scope, old.scope);
            }
            _ => panic!("filter control"),
        }
        state.reduce(Intent::SetFilter(filter));
    }
    let intent = FindOutcome::SetQuery("TITLE FIRST".into())
        .intent(&state, "tab")
        .unwrap();
    state.reduce(intent);
    assert_eq!(state.filter().query, "TITLE FIRST");
    assert_eq!(
        backends()
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        ["All", "claude", "codex", "pi"]
    );
}

#[test]
fn reducer_filters_workspace_backend_query_and_machine_without_view_policy() {
    let (mut state, facts) = fixture();
    for outcome in [
        FindOutcome::SetScope(ChatScope::Workspace),
        FindOutcome::SetBackend(Some(ChatBackend::Claude)),
        FindOutcome::SetQuery("FIRST".into()),
    ] {
        let intent = outcome.intent(&state, "tab").unwrap();
        state.reduce(intent);
    }
    let chat_rows = chats(rows(&state, "tab", &facts, &Disclosures::default()));
    assert_eq!(chat_rows.len(), 1);
    assert_eq!(chat_rows[0].id, ChatRef::local("first"));
    let mut filter = state.filter().clone();
    filter.scope = ChatScope::Machine;
    filter.machine = Some("remote".into());
    filter.query.clear();
    filter.backend = None;
    state.reduce(Intent::SetFilter(filter));
    assert_eq!(scopes(&state, &facts)[1].label, "Build machine");
    assert_eq!(
        chats(rows(&state, "tab", &facts, &Disclosures::default()))[0].id,
        ChatRef::new(Some("remote"), "same")
    );
}

#[test]
fn everywhere_is_configured_machine_order_and_search_expands_sections() {
    let (mut state, facts) = fixture();
    state.reduce(Intent::SetFilter(Filter {
        scope: ChatScope::Everywhere,
        query: "title".into(),
        ..Filter::default()
    }));
    let rows = rows(&state, "tab", &facts, &Disclosures::default());
    let machines: Vec<_> = rows
        .iter()
        .filter_map(|r| {
            if let Row::Machine {
                label,
                count,
                expanded,
                ..
            } = r
            {
                Some((label.as_str(), *count, *expanded))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        machines,
        [
            ("THIS MAC", 3, true),
            ("BUILD MACHINE", 1, true),
            ("OFFLINE", 0, true)
        ]
    );
    let rows = chats(rows);
    assert_eq!(
        rows.last().unwrap().id,
        ChatRef::new(Some("remote"), "same")
    );
    assert!(
        rows.last()
            .unwrap()
            .meta
            .contains(&"@ Build machine".into())
    );
}

#[test]
fn core_folding_keeps_parent_and_expands_matching_child() {
    let mut state = State::default();
    let root = chat("root", "/work", ChatBackend::Claude);
    let mut child = chat("worker", "/work", ChatBackend::Claude);
    child.root_id = Some(root.id.clone());
    load(&mut state, None, vec![child, root]);
    state.reduce(Intent::SetFilter(Filter {
        query: "worker".into(),
        ..Filter::default()
    }));
    let rows = chats(rows(
        &state,
        "tab",
        &FindFacts::default(),
        &Disclosures::default(),
    ));
    assert_eq!(
        rows.iter().map(|r| r.id.id.as_str()).collect::<Vec<_>>(),
        ["root", "worker"]
    );
    assert_eq!(rows[0].fold, Some(true));
    assert!(rows[1].child);
}

#[test]
fn select_is_machine_qualified_preview_is_stamp_guarded_and_close_is_owner_scoped() {
    let (mut state, facts) = fixture();
    let id = ChatRef::new(Some("remote"), "same");
    let select = FindOutcome::Select { id: id.clone() };
    let intent = select.intent(&state, "tab").unwrap();
    let Effect::LoadPreview(stamp) = state.reduce(intent).remove(0) else {
        panic!("preview")
    };
    assert_eq!(stamp.backend, ChatBackend::Pi);
    assert_eq!(stamp.chat, id);
    assert_eq!(stamp.path.to_str(), Some("/fixture/same.jsonl"));
    let mut stale = stamp.clone();
    stale.generation += 1;
    state.reduce(Intent::PreviewFinished {
        stamp: stale,
        result: Ok(ChatPreview::default().into()),
    });
    assert!(state.preview("tab").is_none());
    state.reduce(Intent::PreviewFinished {
        stamp,
        result: Ok(ChatPreview {
            messages: vec![ChatPreviewMessage {
                is_user: true,
                text: "Hello".into(),
            }],
            message_count: Some(9),
        }
        .into()),
    });
    assert_eq!(state.preview("tab").unwrap().messages[0].text, "Hello");
    let intent = FindOutcome::ClosePreview
        .intent(&state, "other-tab")
        .unwrap();
    state.reduce(intent);
    assert!(state.selected("tab").is_some());
    let intent = select.intent(&state, "tab").unwrap();
    state.reduce(intent);
    assert!(
        state.selected("tab").is_none(),
        "core toggles repeated selection"
    );
    assert!(
        FindOutcome::Select {
            id: ChatRef::local("missing")
        }
        .intent(&state, "tab")
        .is_none()
    );
    assert!(
        chats(rows(&state, "tab", &facts, &Disclosures::default()))
            .iter()
            .all(|r| !r.selected)
    );
}

#[test]
fn each_open_and_resume_action_has_the_iced_outcome_and_precedence() {
    let (_, mut facts) = fixture();
    let mut entry = chat("first", "/work", ChatBackend::Claude);
    let id = ChatRef::local("first");
    let (actions, _) = preview_actions(&entry, &id, &facts);
    assert_eq!(
        actions,
        [
            Control {
                label: "Resume as Session".into(),
                outcome: FindOutcome::Resume {
                    id: id.clone(),
                    mode: ResumeMode::Session
                }
            },
            Control {
                label: "Resume as Chat".into(),
                outcome: FindOutcome::Resume {
                    id: id.clone(),
                    mode: ResumeMode::Chat
                }
            }
        ]
    );
    entry.dead_cwd = true;
    assert_eq!(
        preview_actions(&entry, &id, &facts).0,
        [Control {
            label: "Recreate directory & resume".into(),
            outcome: FindOutcome::Resume {
                id: id.clone(),
                mode: ResumeMode::RecreateDirectory
            }
        }]
    );
    facts.open_sessions.push(OpenSession {
        id: id.clone(),
        title: "First".into(),
        workspace: "Work".into(),
        marker: "●".into(),
        detail: String::new(),
        selected: true,
    });
    assert_eq!(
        preview_actions(&entry, &id, &facts).0,
        [Control {
            label: "Go to Session".into(),
            outcome: FindOutcome::Open { id: id.clone() }
        }]
    );
    let remote = ChatRef::new(Some("remote"), "first");
    assert!(
        preview_actions(&entry, &remote, &facts).0.is_empty(),
        "dead remote cwd has no rescue"
    );
    facts.running_remote.push(remote.clone());
    assert_eq!(
        preview_actions(&entry, &remote, &facts).0,
        [Control {
            label: "Open Chat".into(),
            outcome: FindOutcome::Resume {
                id: remote.clone(),
                mode: ResumeMode::Chat
            }
        }]
    );
    facts.machines[1].reachable = false;
    assert!(
        preview_actions(&entry, &remote, &facts).0.is_empty(),
        "unreachable precedes running"
    );
    entry.dead_cwd = false;
    facts.running_remote.clear();
    facts.machines[1].reachable = true;
    facts.machines[1].claude_chat = false;
    assert_eq!(preview_actions(&entry, &remote, &facts).0.len(), 1);
    entry.backend = ChatBackend::Codex;
    assert_eq!(
        preview_actions(&entry, &id, &FindFacts::default()).0.len(),
        1
    );
    for outcome in [
        FindOutcome::Open { id: id.clone() },
        FindOutcome::Resume {
            id,
            mode: ResumeMode::Session,
        },
    ] {
        assert!(outcome.intent(&State::default(), "tab").is_none());
    }
}

// Mock GPUI test-platform windows only: no native GUI or production state.
fn click(selector: &'static str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
    let bounds = cx.debug_bounds(selector).expect("visible Find control");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn native_query_filters_row_and_open_marker_emit_typed_outcomes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (state, mut facts) = fixture();
    facts.open_sessions.push(OpenSession {
        id: ChatRef::local("first"),
        title: "First".into(),
        workspace: "Work".into(),
        marker: "●".into(),
        detail: String::new(),
        selected: false,
    });
    let state = Rc::new(RefCell::new(state));
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| FindSidebar::new(state, "tab".into(), facts, window, cx));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindSidebar>().unwrap()
    });
    let events = Rc::new(RefCell::new(vec![]));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&view, move |_, outcome: &FindOutcome, _| {
            events.borrow_mut().push(outcome.clone())
        })
        .detach();
    });
    view.update_in(cx, |view, window, cx| {
        view.query.update(cx, |input, cx| input.focus(window, cx))
    });
    cx.simulate_input("First");
    cx.run_until_parked();
    assert_eq!(
        events.borrow().last(),
        Some(&FindOutcome::SetQuery("First".into()))
    );
    click("find-scope-0", cx);
    click("find-backend-2", cx);
    click("find-chat-2", cx);
    click("find-focus-2", cx);
    assert_eq!(
        &events.borrow()[events.borrow().len() - 4..],
        &[
            FindOutcome::SetScope(ChatScope::Workspace),
            FindOutcome::SetBackend(Some(ChatBackend::Codex)),
            FindOutcome::Select {
                id: ChatRef::local("first")
            },
            FindOutcome::Open {
                id: ChatRef::local("first")
            }
        ]
    );
}

#[gpui::test]
fn native_preview_buttons_emit_resume_and_close_and_raw_toggle_is_local(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (mut state, facts) = fixture();
    let select = FindOutcome::Select {
        id: ChatRef::local("first"),
    }
    .intent(&state, "tab")
    .unwrap();
    state.reduce(select);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| FindPreview::new(Rc::new(RefCell::new(state)), "tab".into(), facts));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindPreview>().unwrap()
    });
    let events = Rc::new(RefCell::new(vec![]));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&view, move |_, outcome: &FindOutcome, _| {
            events.borrow_mut().push(outcome.clone())
        })
        .detach();
    });
    click("find-preview-action-0", cx);
    click("find-preview-action-1", cx);
    click("find-preview-conversation", cx);
    assert!(view.read_with(cx, |view, _| view.raw_expanded));
    click("find-close-preview", cx);
    assert_eq!(
        *events.borrow(),
        [
            FindOutcome::Unavailable("Resume as Session".into()),
            FindOutcome::Unavailable("Resume as Chat".into()),
            FindOutcome::ClosePreview
        ]
    );
}

#[test]
fn open_lines_match_iced_title_and_place_limits() {
    let mut session = OpenSession {
        id: ChatRef::local("first"),
        title: "Revisiting GPUI discussion".into(),
        workspace: "Work".into(),
        marker: "✳ ".into(),
        detail: "main — Latest prompt".into(),
        selected: true,
    };
    assert_eq!(
        open_lines(&session),
        (
            "Revisiting GPUI discussion".into(),
            "Work · main — Latest prompt".into(),
        )
    );
    session.title = "é".repeat(49);
    session.detail = "界".repeat(80);
    assert_eq!(
        open_lines(&session),
        (
            format!("{}…", "é".repeat(47)),
            format!("Work · {}…", "界".repeat(64)),
        )
    );
}

#[gpui::test]
fn measured_rows_are_left_aligned_and_never_overlap(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (state, mut facts) = fixture();
    facts.open_sessions = [
        ("first", "Short", "main"),
        (
            "same",
            "A longer open conversation title that wraps",
            "feature/long-branch-name",
        ),
        ("second", "Third", "main — Last user prompt"),
    ]
    .into_iter()
    .map(|(id, title, detail)| OpenSession {
        id: ChatRef::local(id),
        title: title.into(),
        workspace: "Work".into(),
        marker: "✳".into(),
        detail: detail.into(),
        selected: id == "first",
    })
    .collect();
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            FindSidebar::new(
                Rc::new(RefCell::new(state)),
                "tab".into(),
                facts,
                window,
                cx,
            )
        });
        gpui_component::Root::new(view, window, cx)
    });
    for width in [280., 480.] {
        cx.simulate_resize(gpui::size(px(width), px(1200.)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = |cx: &mut VisualTestContext, selector| cx.debug_bounds(selector).unwrap();
        let mut previous = bounds(cx, "find-open");
        let disclosure = bounds(cx, "find-open-part-0");
        assert_eq!(disclosure.left(), previous.left() + px(6.));
        assert_eq!(disclosure.top(), previous.top() + px(6.));
        for i in 0..3 {
            let row = bounds(cx, Box::leak(format!("find-open-{i}").into_boxed_str()));
            let icon = bounds(
                cx,
                Box::leak(format!("find-open-icon-{i}").into_boxed_str()),
            );
            assert_eq!(icon.left(), row.left() + px(8.));
        }
        let backfill = bounds(cx, "find-backfill");
        assert_eq!(
            bounds(cx, "find-backfill-label").left(),
            backfill.left() + px(8.)
        );
        for (row, title, detail) in [
            ("find-open-0", "find-open-title-0", "find-open-detail-0"),
            ("find-open-1", "find-open-title-1", "find-open-detail-1"),
            ("find-open-2", "find-open-title-2", "find-open-detail-2"),
        ] {
            let row = bounds(cx, row);
            let title = bounds(cx, title);
            let detail = bounds(cx, detail);
            assert!(
                previous.bottom() <= row.top(),
                "open rows overlap at {width}px"
            );
            assert!(title.bottom() <= detail.top(), "title overlaps detail");
            assert!(
                detail.bottom() <= row.bottom(),
                "two-line content escapes button"
            );
            assert_eq!(title.left(), detail.left());
            assert!(title.left() < row.left() + px(40.), "open text is centred");
            previous = row;
        }
        for selector in [
            "find-machine-0",
            "find-group-1",
            "find-chat-2",
            "find-chat-3",
            "find-group-4",
            "find-chat-5",
        ] {
            let row = bounds(cx, selector);
            assert!(
                previous.bottom() <= row.top(),
                "{selector} overlaps preceding row"
            );
            assert!(row.left() < px(20.), "{selector} is centred");
            previous = row;
        }
        for (row, title, latest, meta) in [
            (
                "find-chat-2",
                "find-title-2",
                "find-detail-2-0",
                "find-meta-2",
            ),
            (
                "find-chat-3",
                "find-title-3",
                "find-detail-3-0",
                "find-meta-3",
            ),
        ] {
            let row = bounds(cx, row);
            let title = bounds(cx, title);
            let latest = bounds(cx, latest);
            let meta = bounds(cx, meta);
            assert!(title.bottom() <= latest.top());
            assert!(latest.bottom() <= meta.top());
            assert!(meta.bottom() <= row.bottom());
            assert_eq!(title.left(), latest.left());
            assert_eq!(latest.left(), meta.left());
            assert_eq!(title.left(), row.left() + px(8.));
        }
    }
}

#[test]
fn fixture_text_and_order_match_iced_find_sidebar() {
    let (state, mut facts) = fixture();
    facts.open_sessions.push(OpenSession {
        id: ChatRef::local("first"),
        title: "Title first".into(),
        workspace: "Work".into(),
        marker: "✳".into(),
        detail: "main".into(),
        selected: false,
    });
    // src/main.rs view_chat_row: dot + title, latest, then age, branch,
    // open-in (when present), and remaining status indicators. No backend
    // name or workspace/age line is inserted into an indexed chat row.
    let text: Vec<Vec<String>> = rows(&state, "tab", &facts, &Disclosures::default())
        .into_iter()
        .map(|row| match row {
            Row::Machine { label, .. } => vec![label],
            Row::Group { label, count } => vec![format!("{label} {count}")],
            Row::Note(note) => vec![note],
            Row::Chat(row) => {
                let mut text = vec![format!("● {}", row.title)];
                text.extend(row.details);
                let mut meta = vec![row.age];
                meta.extend(row.branch);
                meta.extend(row.open.map(|control| control.label));
                meta.extend(row.meta);
                text.push(meta.join(" | "));
                text
            }
        })
        .collect();
    assert_eq!(
        text,
        [
            vec!["THIS MAC"],
            vec!["API 2"],
            vec![
                "● Title first",
                "↳ Latest first",
                "3h | main | ● open in Work"
            ],
            vec!["● Title same", "↳ Latest same", "3h | main"],
            vec!["WEB 1"],
            vec!["● Title second", "↳ Latest second", "3h | main"],
        ]
    );
}

fn preview_state(state: &mut State, id: ChatRef, data: PreviewData) {
    let intent = FindOutcome::Select { id }.intent(state, "tab").unwrap();
    let Effect::LoadPreview(stamp) = state.reduce(intent).remove(0) else {
        panic!("preview load");
    };
    state.reduce(Intent::PreviewFinished {
        stamp,
        result: Ok(data),
    });
}

fn observe_preview(
    view: &Entity<FindPreview>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<Vec<FindOutcome>>> {
    let events = Rc::new(RefCell::new(vec![]));
    cx.update(|_, cx| {
        let captured = events.clone();
        cx.subscribe(view, move |_, outcome: &FindOutcome, _| {
            captured.borrow_mut().push(outcome.clone())
        })
        .detach();
    });
    events
}

#[test]
fn overview_facts_follow_iced_machine_repo_branch_harness_order() {
    let (state, facts) = fixture();
    let mut session = OpenSession {
        id: ChatRef::local("first"),
        title: "Title".into(),
        workspace: "Work".into(),
        marker: "✦".into(),
        detail: "main".into(),
        selected: false,
    };
    assert_eq!(
        overview_place(&facts, &session, entry(&state, &session.id)),
        "This Mac · api · main · claude"
    );
    session.id = ChatRef::new(Some("remote"), "same");
    assert_eq!(
        overview_place(&facts, &session, entry(&state, &session.id)),
        "Build machine · repo · main · pi"
    );
    assert_eq!(
        overview_place(&facts, &session, None),
        "Build machine · Work · main"
    );
}

#[gpui::test]
fn n13_overview_groups_idle_fold_and_live_navigation(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (state, mut facts) = fixture();
    for (id, marker) in [("first", "●"), ("second", "▶"), ("same", "✦")] {
        facts.open_sessions.push(OpenSession {
            id: ChatRef::local(id),
            title: format!("{id} title"),
            workspace: "Work".into(),
            marker: marker.into(),
            detail: "main".into(),
            selected: false,
        });
    }
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| FindPreview::new(Rc::new(RefCell::new(state)), "tab".into(), facts));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindPreview>().unwrap()
    });
    let events = observe_preview(&view, cx);
    for width in [480., 1000.] {
        cx.simulate_resize(gpui::size(px(width), px(1200.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = |cx: &mut VisualTestContext, selector| cx.debug_bounds(selector).unwrap();
        let overview = bounds(cx, "find-overview");
        let fold = bounds(cx, "find-overview-idle");
        let arrow = bounds(cx, "find-overview-idle-part-0");
        let label = bounds(cx, "find-overview-idle-part-1");
        let count = bounds(cx, "find-overview-idle-part-2");
        assert_eq!(fold.left(), overview.left() + px(18.));
        assert_eq!(fold.right(), overview.right() - px(18.));
        assert_eq!(arrow.left(), fold.left() + px(6.));
        assert_eq!(arrow.top(), fold.top() + px(10.));
        assert_eq!(label.left(), arrow.right() + px(8.));
        assert_eq!(count.left(), label.right() + px(8.));
        assert_eq!(fold.bottom(), arrow.bottom() + px(4.));
        assert!(cx.debug_bounds("find-overview-row-2").is_none());
        click("find-overview-idle", cx);
        assert!(view.read_with(cx, |v, _| v.idle_expanded));
        for i in 0..3 {
            let row = bounds(
                cx,
                Box::leak(format!("find-overview-row-{i}").into_boxed_str()),
            );
            let icon = bounds(
                cx,
                Box::leak(format!("find-overview-icon-{i}").into_boxed_str()),
            );
            let title = bounds(
                cx,
                Box::leak(format!("find-overview-title-{i}").into_boxed_str()),
            );
            let meta = bounds(
                cx,
                Box::leak(format!("find-overview-meta-{i}").into_boxed_str()),
            );
            let detail = bounds(
                cx,
                Box::leak(format!("find-overview-detail-{i}").into_boxed_str()),
            );
            let right = bounds(
                cx,
                Box::leak(format!("find-overview-right-{i}").into_boxed_str()),
            );
            assert_eq!(row.left(), overview.left() + px(18.));
            assert_eq!(icon.left(), row.left() + px(10.));
            assert_eq!(title.left(), icon.right() + px(8.));
            assert_eq!(right.right(), row.right() - px(10.));
            assert_eq!(right.top(), title.top());
            assert_eq!(title.top(), row.top() + px(5.));
            assert_eq!(meta.top(), title.bottom() + px(1.));
            assert_eq!(detail.top(), meta.bottom() + px(1.));
            assert_eq!(meta.left(), title.left());
            assert_eq!(detail.left(), title.left());
            assert_eq!(row.bottom(), detail.bottom() + px(5.));
            assert!(meta.right() + px(8.) <= right.left());
        }
        click("find-overview-idle", cx);
        assert!(!view.read_with(cx, |v, _| v.idle_expanded));
    }
    click("find-overview-row-0", cx);
    click("find-overview-row-1", cx);
    click("find-overview-idle", cx);
    click("find-overview-row-2", cx);
    click("find-overview-idle", cx);
    assert_eq!(
        *events.borrow(),
        ["first", "second", "same"].map(|id| FindOutcome::Open {
            id: ChatRef::local(id)
        })
    );
}

#[gpui::test]
fn n13b_summary_folds_match_iced_limits_and_files_emit_local_open(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (mut state, facts) = fixture();
    preview_state(
        &mut state,
        ChatRef::local("first"),
        PreviewData {
            tail: ChatPreview::default(),
            summary: Summary {
                ask: Some(
                    (0..6)
                        .map(|i| format!("ask {i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                reply: Some(
                    (0..18)
                        .map(|i| format!("reply {i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                files: vec!["src/main.rs".into()],
            },
        },
    );
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| FindPreview::new(Rc::new(RefCell::new(state)), "tab".into(), facts));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindPreview>().unwrap()
    });
    let events = observe_preview(&view, cx);
    cx.simulate_resize(gpui::size(px(1000.), px(1800.)));
    click("find-summary-ask", cx);
    assert!(view.read_with(cx, |v, _| v.ask_expanded));
    click("find-summary-ask", cx);
    assert!(!view.read_with(cx, |v, _| v.ask_expanded));
    click("find-summary-reply", cx);
    assert!(view.read_with(cx, |v, _| v.reply_expanded));
    click("find-summary-reply", cx);
    assert!(!view.read_with(cx, |v, _| v.reply_expanded));
    for (row, label) in [
        ("find-summary-ask", "find-summary-ask-label"),
        ("find-summary-reply", "find-summary-reply-label"),
    ] {
        let row = cx.debug_bounds(row).unwrap();
        let label = cx.debug_bounds(label).unwrap();
        assert_eq!(label.left(), row.left() + px(6.));
    }
    click("find-summary-file-0", cx);
    let file = cx.debug_bounds("find-summary-file-0").unwrap();
    let label = cx.debug_bounds("find-summary-file-label-0").unwrap();
    assert_eq!(label.left(), file.left() + px(6.));
    let fold = cx.debug_bounds("find-preview-conversation").unwrap();
    let arrow = cx.debug_bounds("find-preview-conversation-part-0").unwrap();
    assert_eq!(arrow.left(), fold.left() + px(6.));
    assert_eq!(
        *events.borrow(),
        [FindOutcome::OpenFile("/work/api/src/main.rs".into())]
    );
    assert_eq!(clip_lines("one\ntwo\nthree", 2), ("one\ntwo".into(), true));
    assert_eq!(
        clip_lines(&"é".repeat(201), 4),
        (format!("{}…", "é".repeat(199)), true)
    );
}

#[gpui::test]
fn n12_and_n13d_workspace_and_waiting_actions_are_notice_only(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (mut state, mut facts) = fixture();
    facts.workspaces = vec![
        WorkspaceFacts {
            name: "Owner".into(),
            chats: vec!["first".into()],
            open: true,
        },
        WorkspaceFacts {
            name: "Other".into(),
            chats: vec![],
            open: true,
        },
    ];
    facts.open_sessions.push(OpenSession {
        id: ChatRef::local("first"),
        title: "Waiting".into(),
        workspace: "Owner".into(),
        marker: "●".into(),
        detail: "main".into(),
        selected: false,
    });
    preview_state(
        &mut state,
        ChatRef::local("first"),
        ChatPreview::default().into(),
    );
    let state = Rc::new(RefCell::new(state));
    let retained = state.clone();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| FindPreview::new(state, "tab".into(), facts));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindPreview>().unwrap()
    });
    let events = observe_preview(&view, cx);
    cx.simulate_resize(gpui::size(px(1000.), px(1200.)));
    for selector in [
        "find-workspace-owner",
        "find-workspace-add",
        "find-summary-prompt",
    ] {
        click(selector, cx);
    }
    assert_eq!(
        *events.borrow(),
        [
            FindOutcome::Unavailable("Go to Owner".into()),
            FindOutcome::Unavailable("Add to workspace…".into()),
            // N13d: Iced navigates to the waiting session.
            FindOutcome::Open {
                id: ChatRef::local("first")
            },
        ]
    );
    assert_eq!(
        retained.borrow().selected("tab").unwrap().chat,
        ChatRef::local("first")
    );
    assert!(!view.read_with(cx, |v, _| v.raw_expanded));
    view.update(cx, |v, cx| {
        v.facts.workspaces[0].chats.clear();
        v.facts.open_sessions.clear();
        cx.notify();
    });
    click("find-workspace-make", cx);
    assert_eq!(
        events.borrow().last(),
        Some(&FindOutcome::Unavailable("Make workspace".into()))
    );
}

#[gpui::test]
fn n14_relevant_guard_and_n14a_widen_clicks_preserve_filters(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (mut state, mut facts) = fixture();
    state.reduce(Intent::SetFilter(Filter {
        query: "Title".into(),
        scope: ChatScope::Workspace,
        workspace_root: Some("/work".into()),
        ..Filter::default()
    }));
    let suggestions = widen_suggestions(&state, &facts);
    assert_eq!(
        suggestions
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        [
            "1 match elsewhere on this machine",
            "1 match on other machines"
        ]
    );
    facts.relevance_available = true;
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            FindSidebar::new(
                Rc::new(RefCell::new(state)),
                "tab".into(),
                facts,
                window,
                cx,
            )
        });
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindSidebar>().unwrap()
    });
    let events = Rc::new(RefCell::new(vec![]));
    cx.update(|_, cx| {
        let captured = events.clone();
        cx.subscribe(&view, move |_, outcome: &FindOutcome, _| {
            captured.borrow_mut().push(outcome.clone())
        })
        .detach();
    });
    cx.simulate_resize(gpui::size(px(600.), px(1800.)));
    click("find-relevant", cx);
    assert!(view.read_with(cx, |v, _| v.relevant));
    click("find-relevant", cx);
    assert!(!view.read_with(cx, |v, _| v.relevant));
    view.update(cx, |v, cx| {
        v.facts.relevance_down = true;
        cx.notify();
    });
    click("find-relevant", cx);
    assert!(!view.read_with(cx, |v, _| v.relevant));
    for selector in ["find-widen-0", "find-widen-1", "find-backfill"] {
        click(selector, cx);
    }
    assert_eq!(
        *events.borrow(),
        [
            FindOutcome::SetScope(ChatScope::Machine),
            FindOutcome::SetScope(ChatScope::Everywhere),
            FindOutcome::Unavailable("Index older chats".into())
        ]
    );
    view.update(cx, |v, cx| {
        v.facts.relevance_available = false;
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("find-relevant").is_none());
}

fn draw_host(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

#[gpui::test]
fn n16_escape_dismisses_file_before_preview_then_returns_to_overview(cx: &mut TestAppContext) {
    let (root, shell, cx) = crate::h7_tests::fixture(cx);
    cx.update(|window, cx| {
        window.dispatch_action(Box::new(crate::app_menus::ChatsOverviewOpen), cx)
    });
    draw_host(cx);
    let mut item = chat(
        "preview",
        root.path().join("work").to_str().unwrap(),
        ChatBackend::Claude,
    );
    item.dead_cwd = false;
    let owner = cx.read(|cx| shell.read(cx).find_owner(cx));
    shell.update(cx, |shell, cx| {
        shell
            .find_state
            .borrow_mut()
            .reduce(Intent::SourceChanged(None));
        load(&mut shell.find_state.borrow_mut(), None, vec![item]);
        let mut state = shell.find_state.borrow_mut();
        let intent = FindOutcome::Select {
            id: ChatRef::local("preview"),
        }
        .intent(&state, &owner)
        .unwrap();
        let Effect::LoadPreview(stamp) = state.reduce(intent).remove(0) else {
            panic!("load");
        };
        state.reduce(Intent::PreviewFinished {
            stamp,
            result: Ok(ChatPreview::default().into()),
        });
        cx.notify();
    });
    draw_host(cx);
    assert!(cx.debug_bounds("find-centre").is_some());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.find_outcome(
                &owner,
                &FindOutcome::OpenFile(root.path().join("work/file.txt")),
                window,
                cx,
            )
        })
    });
    draw_host(cx);
    assert!(cx.debug_bounds("close-file-viewer").is_some());
    cx.simulate_keystrokes("escape");
    draw_host(cx);
    assert!(cx.debug_bounds("close-file-viewer").is_none());
    assert!(cx.read(|cx| {
        shell
            .read(cx)
            .find_state
            .borrow()
            .selected(&owner)
            .is_some()
    }));
    cx.simulate_keystrokes("escape");
    draw_host(cx);
    assert!(cx.read(|cx| {
        shell
            .read(cx)
            .find_state
            .borrow()
            .selected(&owner)
            .is_none()
    }));
    assert!(cx.debug_bounds("find-overview").is_some());
}

fn tree_snapshot(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = vec![];
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            entries.extend(tree_snapshot(&path));
        } else {
            entries.push((path.clone(), std::fs::read(path).unwrap()));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[gpui::test]
fn pw_notices_do_not_write_or_create_a_process_or_session(cx: &mut TestAppContext) {
    let (root, shell, cx) = crate::h7_tests::fixture(cx);
    cx.update(|window, cx| {
        window.dispatch_action(Box::new(crate::app_menus::ChatsOverviewOpen), cx)
    });
    draw_host(cx);
    let before = tree_snapshot(root.path());
    let sessions = cx.read(|cx| {
        shell
            .read(cx)
            .workspaces
            .iter()
            .map(|w| w.sessions.len())
            .sum::<usize>()
    });
    let owner = cx.read(|cx| shell.read(cx).find_owner(cx));
    for label in [
        "Resume as Session",
        "Resume as Chat",
        "Open Chat",
        "Recreate directory & resume",
        "Go to Owner",
        "Make workspace",
        "Add to workspace…",
        "Add its coordinator to workspace…",
        "Other",
        "Go to prompt",
        "Index older chats",
    ] {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.find_outcome(&owner, &FindOutcome::Unavailable(label.into()), window, cx)
            })
        });
        draw_host(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(
                shell.owner.read(cx).error.as_deref(),
                Some(format!("{label} · not available in preview").as_str())
            );
            assert_eq!(
                shell
                    .workspaces
                    .iter()
                    .map(|w| w.sessions.len())
                    .sum::<usize>(),
                sessions
            );
            assert!(
                shell
                    .workspaces
                    .iter()
                    .all(|w| w.panes.values().all(|p| p.read(cx).session.is_none()))
            );
        });
    }
    assert_eq!(tree_snapshot(root.path()), before);
}

#[test]
fn summary_reads_whole_claude_transcript_and_excludes_failed_edits() {
    let dir = tempfile::tempdir_in("/private/tmp").unwrap();
    let path = dir.path().join("chat.jsonl");
    let lines = [
        serde_json::json!({"type":"user","message":{"content":"original request"}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Write","input":{"file_path":"old.rs"}}]}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"b","name":"Edit","input":{"file_path":"failed.rs"}}]}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b","is_error":true}]}}),
        serde_json::json!({"type":"user","isMeta":true,"message":{"content":"ignore me"}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
    ].map(|v| v.to_string()).join("\n");
    std::fs::write(&path, &lines).unwrap();
    let data = load_preview_data(&path, ChatBackend::Claude).unwrap();
    assert_eq!(data.summary.ask.as_deref(), Some("original request"));
    assert_eq!(data.summary.reply.as_deref(), Some("done"));
    assert_eq!(data.summary.files, ["old.rs"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), lines);
}

#[test]
fn n14_confidence_choice_and_source_guard_match_iced() {
    let answer = |confidence, choice| serde_json::json!({"answers":{"relevant":{"type":"choice","choice":choice,"confidence":confidence,"probabilities":{"c0":0.1,"c1":0.8,"none":0.1}}}});
    assert_eq!(
        ranked_probabilities(&answer(0.9, "c1"), 2).unwrap(),
        (vec![0.1, 0.8], 0.9, false)
    );
    assert!(ranked_probabilities(&answer(0.9, "c7"), 2).is_err());
    assert!(ranked_probabilities(&answer(0.9, "c1"), 1).is_err());
    let (mut state, _) = fixture();
    let old = rank_key(&state).unwrap();
    let mut filter = state.filter().clone();
    filter.query = "first".into();
    state.reduce(Intent::SetFilter(filter));
    assert_ne!(rank_key(&state).as_ref(), Some(&old));
}

#[gpui::test]
fn n14_toggle_restores_recency_and_confident_order_keeps_adjacent_repo_groups(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_component::init);
    let (state, mut facts) = fixture();
    facts.relevance_available = true;
    let key = rank_key(&state).unwrap();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            let mut sidebar = FindSidebar::new(
                Rc::new(RefCell::new(state)),
                "tab".into(),
                facts,
                window,
                cx,
            );
            sidebar.relevance.result = Some((
                key,
                vec!["same".into(), "second".into(), "first".into()],
                true,
            ));
            sidebar
        });
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindSidebar>().unwrap()
    });
    let order = |view: &FindSidebar| {
        let state = view.state.borrow();
        let rank = view
            .relevance
            .result
            .as_ref()
            .filter(|(_, _, confident)| view.relevant && *confident)
            .map(|(_, order, _)| order.as_slice());
        chats(rows_ordered(&state, "tab", &view.facts, &view.folds, rank))
            .into_iter()
            .map(|r| r.id.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        view.read_with(cx, |v, _| order(v)),
        ["first", "same", "second"]
    );
    click("find-relevant", cx);
    assert_eq!(
        view.read_with(cx, |v, _| order(v)),
        ["same", "second", "first"]
    );
    click("find-relevant", cx);
    assert_eq!(
        view.read_with(cx, |v, _| order(v)),
        ["first", "same", "second"]
    );
    view.update(cx, |v, cx| {
        v.relevance.result.as_mut().unwrap().2 = false;
        cx.notify();
    });
    click("find-relevant", cx);
    assert_eq!(
        view.read_with(cx, |v, _| order(v)),
        ["first", "same", "second"]
    );
}

#[gpui::test]
fn n13_non_targets_are_disabled_and_remote_changed_files_are_text(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (state, mut facts) = fixture();
    facts
        .running_remote
        .push(ChatRef::new(Some("remote"), "missing"));
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| FindPreview::new(Rc::new(RefCell::new(state)), "tab".into(), facts));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindPreview>().unwrap()
    });
    let events = observe_preview(&view, cx);
    click("find-overview-remote-0", cx);
    let remote = cx.debug_bounds("find-overview-remote-0").unwrap();
    let icon = cx.debug_bounds("find-overview-remote-icon-0").unwrap();
    let title = cx.debug_bounds("find-overview-remote-title-0").unwrap();
    assert_eq!(icon.left(), remote.left() + px(10.));
    assert_eq!(title.left(), icon.right() + px(8.));
    assert!(events.borrow().is_empty());
    view.update(cx, |v, cx| {
        preview_state(
            &mut v.state.borrow_mut(),
            ChatRef::new(Some("remote"), "same"),
            PreviewData {
                tail: ChatPreview::default(),
                summary: Summary {
                    files: vec!["src/lib.rs".into()],
                    ..Summary::default()
                },
            },
        );
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("find-summary-file-0").is_none());
}

#[test]
fn n14_fake_transport_uses_iced_contract_and_confidence_fallback() {
    use gpui::http_client::{FakeHttpClient, Inner, Response};
    for (confidence, choice, expected) in
        [(0.9, "c1", true), (0.2, "c1", false), (0.9, "none", false)]
    {
        let client = FakeHttpClient::create(move |request| async move {
            assert_eq!(
                request.uri().to_string(),
                "https://api.typesafe.ai/v1/systemone"
            );
            assert_eq!(request.method(), "POST");
            assert!(request.headers()["Authorization"].is_sensitive());
            let Inner::Bytes(body) = request.into_body().0 else {
                panic!("request bytes");
            };
            let value: serde_json::Value = serde_json::from_slice(&body.into_inner()).unwrap();
            assert_eq!(value["model"], "jev-latest");
            assert_eq!(value["questions"]["relevant"]["type"], "choice");
            assert_eq!(
                value["questions"]["relevant"]["criteria"]["none"],
                "None of these conversations is about the current work"
            );
            Ok(Response::builder().status(200).body(serde_json::json!({"answers":{"relevant":{"type":"choice","confidence":confidence,"choice":choice,"probabilities":{"c0":0.1,"c1":0.8,"none":0.1}}}}).to_string().into()).unwrap())
        });
        let result = gpui::block_on(rank_chats(
            client,
            "fixture-key".into(),
            Filter::default(),
            vec![
                chat("first", "/fixture", ChatBackend::Claude),
                chat("second", "/fixture", ChatBackend::Codex),
            ],
        ))
        .unwrap();
        assert_eq!(result, (vec!["second".into(), "first".into()], expected));
    }
}

#[gpui::test]
fn n10_n11_and_n12b_child_controls_emit_exact_notice_labels(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (state, facts) = fixture();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| FindPreview::new(Rc::new(RefCell::new(state)), "tab".into(), facts));
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<FindPreview>().unwrap()
    });
    let events = observe_preview(&view, cx);
    cx.simulate_resize(gpui::size(px(1000.), px(1400.)));
    for (remote, dead, child, label) in [
        (false, true, false, "Recreate directory & resume"),
        (true, false, false, "Open Chat"),
        (false, false, true, "Resume as Session"),
    ] {
        view.update(cx, |v, cx| {
            let machine = remote.then(|| "remote".to_string());
            let mut item = chat("action", "/fixture", ChatBackend::Claude);
            item.dead_cwd = dead;
            item.root_id = child.then(|| "coordinator".to_string());
            let id = ChatRef::new(machine.as_deref(), "action");
            let mut state = v.state.borrow_mut();
            state.reduce(Intent::SourceChanged(machine.clone()));
            load(&mut state, machine, vec![item]);
            preview_state(&mut state, id.clone(), ChatPreview::default().into());
            v.facts.running_remote = if remote { vec![id] } else { vec![] };
            v.facts.workspaces = vec![WorkspaceFacts {
                name: "Target".into(),
                chats: vec![],
                open: true,
            }];
            cx.notify();
        });
        click("find-preview-action-0", cx);
        assert_eq!(
            events.borrow().last(),
            Some(&FindOutcome::Unavailable(label.into()))
        );
        if child {
            click("find-workspace-add", cx);
            assert_eq!(
                events.borrow().last(),
                Some(&FindOutcome::Unavailable(
                    "Add its coordinator to workspace…".into()
                ))
            );
        }
    }
}
