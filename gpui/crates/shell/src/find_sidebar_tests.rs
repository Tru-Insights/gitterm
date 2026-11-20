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
        result: Ok(ChatPreview::default()),
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
        }),
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
            FindOutcome::Resume {
                id: ChatRef::local("first"),
                mode: ResumeMode::Session
            },
            FindOutcome::Resume {
                id: ChatRef::local("first"),
                mode: ResumeMode::Chat
            },
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
            assert!(title.left() < row.left() + px(12.));
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
