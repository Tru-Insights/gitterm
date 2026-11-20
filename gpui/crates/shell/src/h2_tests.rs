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
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("workspaces.json"), r#"{"active_workspace":0,"workspaces":[
        {"name":"Folder","abbrev":"F","dir":"/fixture/folder","color":"blue","tabs":[{"dir":"/fixture/folder","startup_command":"MUST NOT RUN"}]},
        {"name":"Named work","abbrev":"N","dir":"/fixture/work","color":"green","chats":["saved-chat"],"active_tab":1,"tabs":[{"dir":"/fixture/work","startup_command":"MUST NOT RUN"},{"dir":"/fixture/work","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"saved-chat"},{"dir":"/fixture/work","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"missing-chat"}]}
    ]}"#).unwrap();
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
        assert_eq!(input.sessions.iter().map(|t| t.display_title.as_str()).collect::<Vec<_>>(), ["Shell", "saved-chat", "missing-chat"]);
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
    for (selector, label) in [
        ("pstrip-repos", "Repos ▾"),
        ("pstrip-browser", "Browser"),
        ("pstrip-settings", "Settings"),
    ] {
        click(cx, selector);
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).owner.read(cx).error,
                Some(format!("{label} · unavailable in preview"))
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
