//! Iced catalog hookups on the headless platform; no real config/harness reads.
use super::*;
use gpui::{TestAppContext, VisualTestContext};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}
fn fixture(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    fixture_catalog(cx, false)
}
fn fixture_catalog(
    cx: &mut TestAppContext,
    counts: bool,
) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("workspaces.json"), r#"{"active_workspace":0,"workspaces":[
        {"name":"Folder","abbrev":"F","dir":"/fixture/folder","color":"blue","tabs":[{"dir":"/fixture/folder","startup_command":"MUST NOT RUN"}]},
        {"name":"Named work","abbrev":"N","dir":"/fixture/work","color":"green","chats":["saved-chat"],"active_tab":1,"tabs":[{"dir":"/fixture/work","startup_command":"MUST NOT RUN"},{"dir":"/fixture/work","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"saved-chat"},{"dir":"/fixture/work","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"missing-chat"}]}
    ]}"#).unwrap();
    if counts {
        use gitterm_core::{merge_plan::*, prs::*, tasks::*};
        let path = source.join("workspaces.json");
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        file["remote_agents"] = serde_json::json!([{"id":"fixture-mac","name":"Fixture Mac","endpoint":"http://127.0.0.1:1","auth":{"type":"token","token_ref":"fixture"}}]);
        file["workspaces"][0]["tabs"] = serde_json::Value::Array(
            (0..7)
                .map(|n| {
                    serde_json::json!({
                        "dir":"/fixture/folder", "task_id":format!("attention-{n}")
                    })
                })
                .collect(),
        );
        for n in 0..16 {
            file["workspaces"].as_array_mut().unwrap().push(serde_json::json!({
                "name":format!("Parked {n}"),"abbrev":"P","dir":"/fixture/parked","color":"blue","tabs":[],"active":false,
                "chats": if n == 0 {vec!["parked-chat"]} else {vec![]}
            }));
        }
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let mut tasks = TaskStoreDocument::default();
        for n in 0..7 {
            let mut task =
                crate::tasks::fixture_task(&format!("attention-{n}"), "Needs input", "task/input");
            task.workspace.location = WorkspaceLocationIdentity::Local {
                directory: "/fixture/folder".into(),
            };
            task.attention = TaskAttention::raised(
                TaskAttentionReason::RequiresInput,
                &TaskAttention::default(),
                "2026-01-01T00:00:00Z",
            );
            tasks.tasks.push(task);
        }
        std::fs::write(
            source.join("tasks.json"),
            serde_json::to_vec(&tasks).unwrap(),
        )
        .unwrap();
        for (root, number) in [("saved-chat", 1), ("parked-chat", 2)] {
            let plan: MergePlan = serde_json::from_value(serde_json::json!({
                "root_chat_id":root, "entries":[{"repo":"fixture/repo","number":number,"step":1}]
            }))
            .unwrap();
            write_plan(&plans_dir(&source), &plan).unwrap();
        }
        let mut watch = WatchFile::default();
        for number in 1..=2 {
            let mut pr = WatchedPr::new(PrKey::new("fixture/repo", number));
            pr.snapshot.core = Some(PullCore {
                state: Some(PrState::Open),
                ..Default::default()
            });
            pr.news.push(NewsItem {
                key: format!("news-{number}"),
                kind: NewsKind::RequiredChecksPassed,
                text: "checks passed".into(),
                at: "2026-01-01T00:00:00Z".into(),
                dismissed: false,
                told: false,
                approvals_only: false,
            });
            watch.prs.insert(pr.key.id(), pr);
        }
        watch.save(&WatchFile::path_in(&source)).unwrap();
    }
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    store.chats.push(chat::SavedChat {
        uid: "read-only-source".into(),
        cwd: "/fixture/work".into(),
        provider: store.guard.as_ref().unwrap().preview().provider(),
        session_id: Some("saved-chat".into()),
        records: vec![gitterm_gpui_chat::events::Record {
            at_ms: None,
            entry: gitterm_gpui_chat::events::Entry::UserPrompt("Recorded conversation".into()),
        }],
    });
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            window,
            cx,
        )
        .unwrap()
    });
    draw(cx);
    (root, shell, cx)
}

#[gpui::test]
fn chrome_uses_catalog_order_and_workspace_selection_opens_its_active_history(
    cx: &mut TestAppContext,
) {
    let (root, shell, cx) = fixture(cx);
    let before = std::fs::read(root.path().join("iced/workspaces.json")).unwrap();
    cx.read(|cx| {
        let s = shell.read(cx);
        let input = s.bar_input(cx);
        assert_eq!(
            input.workspaces.iter().map(|w| w.id).collect::<Vec<_>>(),
            [WorkspaceId(1), WorkspaceId(2)]
        );
        assert_eq!(
            gitterm_core::workspace::visible_order(&input.layout()),
            [1, 0]
        );
        assert_eq!(s.visible_workspace_indices(cx), [1, 0]);
        assert_eq!(
            s.spine_input(cx)
                .dots(workspace_bar::Colors::new(true))
                .iter()
                .map(|d| d.workspace)
                .collect::<Vec<_>>(),
            [WorkspaceId(2), WorkspaceId(1)]
        );
        assert_eq!(s.strip_input(cx).workspace_name, "Folder");
    });
    for selector in [
        "pstrip-session-strip",
        "pstrip-workspace-bar",
        "pstrip-spine",
    ] {
        assert!(cx.debug_bounds(selector).is_some());
    }
    let work = cx.debug_bounds("pstrip-workspace-2").unwrap();
    let folder = cx.debug_bounds("pstrip-workspace-1").unwrap();
    assert!(work.origin.x < folder.origin.x);
    assert!(
        cx.debug_bounds("pstrip-dot-2").unwrap().origin.y
            < cx.debug_bounds("pstrip-dot-1").unwrap().origin.y
    );
    click(cx, "pstrip-workspace-2");
    cx.read(|cx| {
        let s = shell.read(cx);
        let input = s.strip_input(cx);
        assert_eq!(input.workspace, Some(WorkspaceId(2)));
        assert_eq!(input.sessions.iter().map(|t| t.display_title.as_str()).collect::<Vec<_>>(), ["Shell", "/fixture/work", "/fixture/work"]);
        assert!(input.sessions[1..].iter().all(|session| session.chat_title.is_none()));
        assert_eq!(input.active, Some(input.sessions[1].id));
        let state = s.active(cx);
        let pane = state.panes[&session_pane_id(state.sessions[1].id)].read(cx);
        assert!(matches!(&pane.content, pane::PaneContent::PreviewChat(view) if view.read(cx).matches_chat("saved-chat")));
        assert!(pane.session.is_none());
        assert!(pane_id_visible(state, &session_pane_id(state.sessions[1].id), cx));
        for w in &s.workspaces {
            assert!(w.store.terminals.as_ref().unwrap().is_empty());
            assert!(w.panes.values().all(|p| !matches!(p.read(cx).content, pane::PaneContent::Terminal(_))));
        }
        assert!(s.owner.read(cx).chats.is_empty());
    });
    let missing_id = cx.read(|cx| shell.read(cx).active(cx).sessions[2].id);
    let missing_selector = Box::leak(format!("pstrip-session-{}", missing_id.0).into_boxed_str());
    click(cx, missing_selector);
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(s.active(cx).active_session, Some(missing_id));
        assert!(matches!(&s.active(cx).panes[&session_pane_id(missing_id)].read(cx).content, pane::PaneContent::IcedSession(text) if text.contains("missing-chat") && text.contains("history unavailable in preview")));
    });
    // The spine and keyboard-index path select without adding any sessions.
    click(cx, "pstrip-dot-1");
    cx.update(|window, cx| shell.update(cx, |s, cx| s.select_workspace(0, window, cx)));
    draw(cx);
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(s.active_workspace(cx), Some(WorkspaceId(2)));
        assert_eq!(s.active(cx).sessions.len(), 3);
    });
    assert_eq!(
        std::fs::read(root.path().join("iced/workspaces.json")).unwrap(),
        before
    );
}

#[gpui::test]
fn strip_titles_resolve_inactive_facts_tabs_and_preserve_saved_titles(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let facts_dir = root.path().join("iced/chat-meta");
    std::fs::create_dir(&facts_dir).unwrap();
    let source = r#"{"children":[{"session_id":"missing-chat","kind":"worker","title":"Recorded child title"}]}"#;
    let facts_path = facts_dir.join("claude-saved-chat.json");
    std::fs::write(&facts_path, source).unwrap();
    shell.update_in(cx, |shell, window, cx| {
        let state = &mut shell.workspaces[1];
        let session = &mut state.sessions[1];
        session.title = "Previously saved title".into();
        state.panes[&session_pane_id(session.id)].update(cx, |pane, _| {
            pane.title = "Previously saved title".into();
        });
        shell.load_active_iced_history(window, cx);
    });
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        // Neither chat has been selected or imported into a history view.
        assert_eq!(shell.active_workspace(cx), Some(WorkspaceId(1)));
        let state = &shell.workspaces[1];
        assert_eq!(state.sessions[1].title, "Previously saved title");
        assert_eq!(state.sessions[2].title, "Recorded child title");
        assert!(
            state
                .panes
                .values()
                .all(|pane| !matches!(pane.read(cx).content, pane::PaneContent::PreviewChat(_)))
        );
    });
    assert_eq!(std::fs::read_to_string(facts_path).unwrap(), source);
}

#[gpui::test]
fn bottom_plus_opens_sheet_create_keeps_draft_and_existing_work_selects(cx: &mut TestAppContext) {
    let (_root, shell, cx) = fixture(cx);
    click(cx, "pstrip-new-work");
    assert!(cx.debug_bounds("workspace-sheet-card").is_some());
    cx.simulate_input("Cloud migration");
    draw(cx);
    click(cx, "workspace-sheet-create");
    cx.read(|cx| {
        let s = shell.read(cx);
        let sheet = s.new_work_sheet.as_ref().unwrap().read(cx);
        assert_eq!(sheet.draft().name, "Cloud migration");
        assert_eq!(
            sheet.draft().error.as_deref(),
            Some("Creating work isn't available in the preview")
        );
        assert!(s.content.read(cx).picker);
        assert_eq!(s.workspaces.len(), 2);
    });
    click(cx, "workspace-sheet-cancel");
    click(cx, "pstrip-new-work");
    click(cx, "workspace-sheet-existing-0");
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(s.active_workspace(cx), Some(WorkspaceId(2)));
        assert!(s.new_work_sheet.is_none());
        assert!(!s.content.read(cx).picker);
    });
}

#[gpui::test]
fn sheet_folder_picker_returns_selection_and_cancel_without_creating_work(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let repo = root.path().join("picked-repo");
    std::fs::create_dir(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    click(cx, "pstrip-new-work");
    cx.simulate_input("Draft work");
    draw(cx);
    click(cx, "workspace-sheet-add-repo");
    cx.simulate_path_prompt_response(|options| {
        assert!(options.directories && !options.files && !options.multiple);
        Some(vec![repo.clone()])
    });
    draw(cx);
    cx.read(|cx| {
        let s = shell.read(cx);
        let sheet = s.new_work_sheet.as_ref().unwrap().read(cx);
        assert_eq!(sheet.draft().name, "Draft work");
        assert_eq!(sheet.draft().repos.as_slice(), std::slice::from_ref(&repo));
        assert!(s.content.read(cx).picker);
        assert_eq!(s.workspaces.len(), 2);
    });
    click(cx, "workspace-sheet-add-repo");
    cx.simulate_path_prompt_response(|_| None);
    draw(cx);
    cx.read(|cx| {
        let s = shell.read(cx);
        let sheet = s.new_work_sheet.as_ref().unwrap().read(cx);
        assert_eq!(sheet.draft().name, "Draft work");
        assert_eq!(sheet.draft().repos, [repo]);
    });
    click(cx, "workspace-sheet-cancel");
    cx.read(|cx| assert!(!shell.read(cx).content.read(cx).picker));
}

#[gpui::test]
fn missing_host_controls_keep_iced_controls_and_report_unavailable(cx: &mut TestAppContext) {
    let (_root, shell, cx) = fixture(cx);
    // Iced labels; the preview reports them (I6, interaction map A02/A14).
    for selector in ["pstrip-browser", "pstrip-settings"] {
        click(cx, selector);
        cx.read(|cx| {
            let error = shell
                .read(cx)
                .owner
                .read(cx)
                .error
                .clone()
                .unwrap_or_default();
            assert!(
                error.ends_with(" · not available in preview"),
                "{selector}: {error}"
            )
        });
    }
    click(cx, "pstrip-usage");
    cx.read(|cx| assert!(shell.read(cx).usage_open));
    click(cx, "pstrip-usage");
    cx.read(|cx| assert!(!shell.read(cx).usage_open));
    assert!(cx.debug_bounds("pstrip-ready").is_none());
    assert!(cx.debug_bounds("pstrip-parked").is_none());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            for (outcome, label) in [
                (workspace_bar::Outcome::OpenReady, "Ready"),
                (workspace_bar::Outcome::OpenParked, "Parked"),
            ] {
                shell.bar_outcome(outcome, window, cx);
                assert_eq!(
                    shell.owner.read(cx).error,
                    Some(format!("{label} · unavailable in preview"))
                );
            }
        })
    });
}

#[gpui::test]
fn review_consult_shell_shortcuts_reject_in_preview_and_keep_the_draft(cx: &mut TestAppContext) {
    use gitterm_gpui_chat::review::{ReviewKind, ReviewOutcome};
    cx.update(install_app_menus);
    let (root, shell, cx) = fixture(cx);
    let source = root.path().join("iced");
    std::fs::write(source.join("settings.json"), r#"{"review":{"default_reviewer":"codex","subagent_model":"fable","codex_model":"gpt-6.1-sol"}}"#).unwrap();
    let before = ["workspaces.json", "settings.json", "tasks.json"]
        .map(|file| std::fs::read(source.join(file)).ok());
    shell.update_in(cx, |shell, window, cx| {
        shell.select_workspace_id(WorkspaceId(2), window, cx)
    });
    draw(cx);
    let chat = cx.read(|cx| {
        let shell = shell.read(cx);
        let state = shell.active(cx);
        let pane = state.panes[&session_pane_id(state.active_session.unwrap())].read(cx);
        let pane::PaneContent::PreviewChat(chat) = &pane.content else {
            panic!("preview chat");
        };
        chat.read(cx).test_view()
    });
    let outcomes = Rc::new(RefCell::new(vec![]));
    let captured = outcomes.clone();
    cx.update(|_, cx| {
        cx.subscribe(&chat, move |_, outcome: &ReviewOutcome, _| {
            if matches!(outcome, ReviewOutcome::Submit { .. }) {
                captured.borrow_mut().push(outcome.clone());
            }
        })
        .detach();
    });
    click(cx, "composer-review");
    // The context load is read-only and runs on the background executor.
    for _ in 0..100 {
        if cx.debug_bounds("review-submit").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        draw(cx);
    }
    assert!(cx.debug_bounds("review-submit").is_some());
    click(cx, "review-kind-consult");
    cx.simulate_input("Explain the lifecycle");
    draw(cx);
    cx.simulate_keystrokes("cmd-enter");
    draw(cx);
    assert!(
        cx.debug_bounds("review-error").is_some(),
        "preview rejection is visible"
    );
    assert!(
        cx.debug_bounds("review-composer").is_some(),
        "failed submission retains the form"
    );
    cx.simulate_keystrokes("ctrl-enter");
    draw(cx);
    click(cx, "review-submit");
    assert_eq!(
        outcomes.borrow().len(),
        3,
        "both shortcuts and the button submit once"
    );
    let first = outcomes.borrow()[0].clone();
    assert!(
        matches!(&first, ReviewOutcome::Submit { kind: ReviewKind::Consult, scope, note }
        if note == "Explain the lifecycle" && scope.target.is_none()
        && scope.reviewer == gitterm_core::settings::ReviewerKind::Codex && scope.model == "gpt-6.1-sol")
    );
    assert!(
        outcomes.borrow().iter().all(|outcome| outcome == &first),
        "preview rejection keeps the exact draft"
    );
    cx.update(|window, cx| chat.focus_handle(cx).focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("review-composer").is_none());
    let after = ["workspaces.json", "settings.json", "tasks.json"]
        .map(|file| std::fs::read(source.join(file)).ok());
    assert_eq!(before, after, "review preview never writes Iced's stores");
}

#[gpui::test]
fn catalog_bar_renders_repos_folder_badge_plus_and_real_counts_read_only(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture_catalog(cx, true);
    cx.simulate_resize(size(px(1600.), px(800.)));
    draw(cx);
    let source = root.path().join("iced");
    let before = ["workspaces.json", "tasks.json", "pr-watch.json"]
        .map(|name| std::fs::read(source.join(name)).unwrap());
    cx.read(|cx| {
        let input = shell.read(cx).bar_input(cx);
        assert_eq!(
            input.items(),
            [
                workspace_bar::Item::Workspace(1),
                workspace_bar::Item::Repos,
                workspace_bar::Item::Workspace(0),
                workspace_bar::Item::NewWork
            ]
        );
        assert_eq!(
            (
                input.ready_count,
                input.ready_news_count,
                input.parked_count
            ),
            (2, 2, 16)
        );
        assert_eq!(input.workspaces[0].attention().unwrap().count, 7);
        assert!(input.workspaces[1].pr_news.is_some());
        assert_eq!(input.machine_news.len(), 1);
        assert!(input.machine_news[0].label.contains("Fixture Mac"));
    });
    let selectors = [
        "pstrip-workspace-2",
        "pstrip-repos",
        "pstrip-workspace-1",
        "pstrip-new-work",
        "pstrip-ready",
        "pstrip-parked",
        "pstrip-terminal",
        "pstrip-browser",
    ];
    let bounds: Vec<_> = selectors
        .iter()
        .map(|selector| cx.debug_bounds(selector).unwrap())
        .collect();
    assert!(
        bounds
            .windows(2)
            .all(|pair| pair[0].right() <= pair[1].left())
    );
    assert!(cx.debug_bounds("pstrip-attention-1").is_some());
    // Iced's ⋯ more menu (A18) is rendered.
    assert!(cx.debug_bounds("pstrip-more").is_some());
    click(cx, "pstrip-repos");
    assert!(cx.debug_bounds("pstrip-repos-list").is_some());
    click(cx, "pstrip-repo-1");
    cx.read(|cx| assert_eq!(shell.read(cx).active_workspace(cx), Some(WorkspaceId(1))));
    for (name, bytes) in ["workspaces.json", "tasks.json", "pr-watch.json"]
        .into_iter()
        .zip(before)
    {
        assert_eq!(std::fs::read(source.join(name)).unwrap(), bytes);
    }
}
