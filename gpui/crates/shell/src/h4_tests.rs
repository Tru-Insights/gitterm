//! H4 host wiring on the headless test platform; only isolated transcript fixtures.
use super::*;
use find_sidebar::{FindOutcome, ResumeMode};
use gitterm_core::chats::ChatRef;
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
pub(super) fn fixture(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    std::fs::create_dir(&source).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("file.txt"), "covered file").unwrap();
    std::fs::write(source.join("workspaces.json"), serde_json::json!({
        "active_workspace": 0,
        "workspaces": [
            {"name": "First", "abbrev": "F", "color": "blue", "dir": &work, "tabs": [
                {"dir": &work, "session_uid": "owner-first", "tab_kind": "agent", "agent_config": {"backend":"claude", "model":"default"}, "chat_session_id":"first"},
                {"dir": &work, "session_uid": "owner-second", "tab_kind": "agent", "agent_config": {"backend":"claude", "model":"default"}, "chat_session_id":"second"}
            ]},
            {"name": "Other", "abbrev": "O", "color": "green", "dir": root.path().join("other"), "tabs": []}
        ]
    }).to_string()).unwrap();
    let home = root.path().join("home");
    let transcripts = home.join(".claude/projects/fixture");
    std::fs::create_dir_all(&transcripts).unwrap();
    for id in ["first", "second", "closed"] {
        std::fs::write(
            transcripts.join(format!("{id}.jsonl")),
            serde_json::json!({
                "type": "user", "cwd": &work, "message": {"content": format!("Transcript {id}")}
            })
            .to_string(),
        )
        .unwrap();
    }
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    for id in ["first", "second"] {
        store.chats.push(chat::SavedChat {
            uid: format!("saved-{id}"),
            cwd: work.clone(),
            provider: store.guard.as_ref().unwrap().preview().provider(),
            session_id: Some(id.into()),
            records: vec![],
        });
    }
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        let mut shell = Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            window,
            cx,
        )
        .unwrap();
        shell.find_home = Some(home);
        shell
    });
    draw(cx);
    (root, shell, cx)
}

#[gpui::test]
fn find_mount_select_close_filters_and_tab_owners(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (root, shell, cx) = fixture(cx);
    let before = std::fs::read(root.path().join("iced/workspaces.json")).unwrap();
    click(cx, "Find");
    assert!(cx.debug_bounds("find-sidebar").is_some());
    let (owner, sidebar, session_entity) = cx.read(|cx| {
        let shell = shell.read(cx);
        let owner = shell.find_owner(cx);
        let views = &shell.find_views[&owner];
        let state = shell.active(cx);
        assert_eq!(
            shell
                .find_state
                .borrow()
                .index(&None)
                .unwrap()
                .entries
                .len(),
            3
        );
        (
            owner,
            views.sidebar.clone(),
            state.panes[&session_pane_id(state.active_session.unwrap())].entity_id(),
        )
    });
    sidebar.update(cx, |_, cx| {
        cx.emit(FindOutcome::Select {
            id: ChatRef::new(None, "closed"),
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("find-centre").is_some());
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(
            shell.find_state.borrow().preview(&owner).unwrap().messages[0].text,
            "Transcript closed"
        );
        assert_eq!(
            shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())]
                .entity_id(),
            session_entity
        );
    });
    // A different tab gets a different retained owner and no inherited selection.
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.select_session(1, window, cx)));
    draw(cx);
    assert!(cx.debug_bounds("find-centre").is_none());
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.select_session(0, window, cx)));
    draw(cx);
    assert!(cx.debug_bounds("find-centre").is_some());
    cx.read(|cx| assert_eq!(shell.read(cx).find_views[&owner].sidebar, sidebar));
    click(cx, "find-close-preview");
    assert!(cx.debug_bounds("find-centre").is_none());
    assert!(cx.debug_bounds("session-content").is_some());
    sidebar.update(cx, |_, cx| {
        cx.emit(FindOutcome::SetScope(
            gitterm_core::chats::ChatScope::Workspace,
        ));
        cx.emit(FindOutcome::SetQuery("closed".into()));
    });
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).find_state.borrow().filtered_entries().len(),
            1
        )
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.select_workspace_id(WorkspaceId(2), window, cx)
        })
    });
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        let state = shell.find_state.borrow();
        assert_eq!(state.filter().machine, None);
        assert_eq!(
            state.filter().workspace_root,
            Some(root.path().join("other"))
        );
        assert!(state.filtered_entries().is_empty());
    });
    assert_eq!(
        std::fs::read(root.path().join("iced/workspaces.json")).unwrap(),
        before
    );
}

#[gpui::test]
fn find_open_resume_select_existing_history_and_refuse_new_process(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (root, shell, cx) = fixture(cx);
    let before = std::fs::read(root.path().join("iced/workspaces.json")).unwrap();
    click(cx, "Find");
    let (owner, sidebar) = cx.read(|cx| {
        let shell = shell.read(cx);
        let owner = shell.find_owner(cx);
        (owner.clone(), shell.find_views[&owner].sidebar.clone())
    });
    for outcome in [
        FindOutcome::Open {
            id: ChatRef::new(None, "second"),
        },
        FindOutcome::Resume {
            id: ChatRef::new(None, "first"),
            mode: ResumeMode::Chat,
        },
    ] {
        sidebar.update(cx, |_, cx| cx.emit(outcome));
        draw(cx);
    }
    cx.read(|cx| {
        assert!(
            shell
                .read(cx)
                .owner
                .read(cx)
                .error
                .as_deref()
                .unwrap()
                .contains("not available in preview")
        );
    });
    sidebar.update(cx, |_, cx| {
        cx.emit(FindOutcome::Open {
            id: ChatRef::local("first"),
        })
    });
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.find_owner(cx), owner);
        assert_eq!(shell.active(cx).sessions.len(), 2);
        assert!(shell.owner.read(cx).chats.is_empty());
        assert!(matches!(
            shell.active(cx).panes[&session_pane_id(shell.active(cx).active_session.unwrap())]
                .read(cx)
                .content,
            pane::PaneContent::PreviewChat(_)
        ));
    });
    sidebar.update(cx, |_, cx| {
        cx.emit(FindOutcome::Resume {
            id: ChatRef::new(None, "closed"),
            mode: ResumeMode::RecreateDirectory,
        })
    });
    draw(cx);
    cx.read(|cx| {
        assert!(
            shell
                .read(cx)
                .owner
                .read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("not available in preview")
        )
    });
    assert_eq!(
        std::fs::read(root.path().join("iced/workspaces.json")).unwrap(),
        before
    );
}

#[gpui::test]
fn file_open_event_close_button_and_escape_return_to_same_session(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (root, shell, cx) = fixture(cx);
    let (owner, workspace, session, pane) = cx.read(|cx| {
        let shell = shell.read(cx);
        let state = shell.active(cx);
        let session = state.active_session.unwrap();
        (
            shell.owner.clone(),
            state.workspace.id,
            session,
            state.panes[&session_pane_id(session)].entity_id(),
        )
    });
    for escape in [false, true] {
        owner.update(cx, |owner, cx| {
            owner.open_file(workspace, root.path().join("work/file.txt"), cx)
        });
        draw(cx);
        assert!(cx.debug_bounds("close-file-viewer").is_some());
        if escape {
            cx.simulate_keystrokes("escape");
            draw(cx);
        } else {
            click(cx, "close-file-viewer");
        }
        assert!(cx.debug_bounds("close-file-viewer").is_none());
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.active(cx).active_session, Some(session));
            assert_eq!(
                shell.active(cx).panes[&session_pane_id(session)].entity_id(),
                pane
            );
            assert!(shell.content.read(cx).selected.is_none());
        });
    }
}

#[gpui::test]
fn find_open_rows_refresh_and_keep_needs_you_before_working_before_idle(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_root, shell, cx) = fixture(cx);
    let sessions = cx.read(|cx| shell.read(cx).active(cx).sessions.clone());
    for (first_title, second_title, expected) in [
        ("Idle", "◐ Working", 1),
        ("✳ Needs you", "◐ Working", 0),
        ("Idle", "Idle", 0),
    ] {
        // Selecting a row restores that tab's own sidebar mode.
        if cx.read(|cx| shell.read(cx).active(cx).store.regions.sidebar_mode) != SidebarMode::Find {
            click(cx, "Find");
        }
        shell.update(cx, |shell, cx| {
            for (session, title) in sessions.iter().zip([first_title, second_title]) {
                shell.active(cx).panes[&session_pane_id(session.id)].update(cx, |pane, cx| {
                    pane.title = title.into();
                    pane.content = pane::PaneContent::IcedSession(title.into());
                    cx.notify();
                });
            }
            cx.notify();
        });
        draw(cx);
        click(cx, "find-open-0");
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).active(cx).active_session,
                Some(sessions[expected].id)
            )
        });
    }
}
