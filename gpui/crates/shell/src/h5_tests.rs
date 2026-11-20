//! H5 host hookups: headless windows and isolated Iced/preview fixture roots.
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
    cx.update(bind_workspace_keys);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    let work = root.path().join("work");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("file.txt"), "covered file").unwrap();
    std::fs::write(source.join("settings.json"), r#"{"console_height":240,"console_expanded":false,"quick_commands":[{"name":"test","command":"MUST NOT RUN"}]}"#).unwrap();
    std::fs::write(source.join("workspaces.json"), serde_json::json!({
        "active_workspace":0,"workspaces":[{"name":"H5","abbrev":"H5","color":"blue","dir":work,"repos":[work],"tabs":[]}]
    }).to_string()).unwrap();
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    let settings = workspace_config::load_iced_settings_from(&source);
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        Shell::with_owner_settings(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            &settings,
            window,
            cx,
        )
        .unwrap()
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    (root, shell, cx)
}
fn emit(shell: &Entity<Shell>, outcome: bottom_panel::Outcome, cx: &mut VisualTestContext) {
    let panel = cx.read(|cx| shell.read(cx).bottom.clone());
    panel.update(cx, |_, cx| cx.emit(outcome));
    draw(cx);
}

#[gpui::test]
fn explicit_bottom_toggle_add_select_last_close_and_restore(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let source = root.path().join("iced");
    let before =
        ["settings.json", "workspaces.json"].map(|f| std::fs::read(source.join(f)).unwrap());
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(
            shell.active(cx).sessions.len(),
            1,
            "read-only Iced main shell retained"
        );
        assert!(!shell.bottom.read(cx).input.visible());
        assert_eq!(shell.bottom.read(cx).input.quick_commands.len(), 1);
        assert_eq!(shell.bottom.read(cx).input.repos.len(), 1);
    });
    assert!(cx.debug_bounds("standalone-bottom-panel").is_none());
    // Cmd+J is the same explicit empty-panel toggle as the workspace bar.
    cx.update(|w, cx| shell.read(cx).focus_handle(cx).focus(w, cx));
    cx.simulate_keystrokes("cmd-j");
    draw(cx);
    assert!(cx.debug_bounds("standalone-bottom-panel").is_some());
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(
            shell.active(cx).sessions.len(),
            1,
            "read-only Iced main shell retained"
        );
        assert_eq!(shell.bottom.read(cx).input.height, 240.);
    });
    emit(
        &shell,
        bottom_panel::Outcome::AddTerminal { repo: None },
        cx,
    );
    let (id, target) = cx.read(|cx| {
        let shell = shell.read(cx);
        let input = &shell.bottom.read(cx).input;
        assert_eq!(input.tabs.len(), 1);
        let id = input.tabs[0].id;
        (
            id,
            shell.active(cx).panes[&session_pane_id(SessionId(id as u64))].downgrade(),
        )
    });
    let main = cx.read(|cx| shell.read(cx).active(cx).active_session);
    emit(&shell, bottom_panel::Outcome::SelectTab { id }, cx);
    cx.update(|window, cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.active(cx).active_session, main);
        assert_eq!(
            shell.strip_input(cx).sessions.len(),
            1,
            "bottom tabs stay out of the main session strip"
        );
        assert!(
            shell.active(cx).panes[&session_pane_id(SessionId(id as u64))]
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
    });
    let config = cx.read(|cx| shell.read(cx).config_dir.clone());
    // Restore explicit preview tabs without allocating an automatic terminal.
    let owner = cx.read(|cx| shell.read(cx).owner.clone());
    let restored = cx.update(|window, cx| {
        cx.new(|cx| {
            let mut result = Shell::with_owner(
                Rc::new(data::LocalData(owner.clone())),
                owner,
                &config,
                true,
                window,
                cx,
            )
            .unwrap();
            result.refresh_bottom_panel(window, cx);
            result
        })
    });
    cx.read(|cx| {
        assert!(restored.read(cx).bottom.read(cx).input.opened);
        assert_eq!(restored.read(cx).bottom.read(cx).input.tabs.len(), 1);
    });
    drop(restored);
    emit(&shell, bottom_panel::Outcome::CloseTab { id }, cx);
    cx.read(|cx| {
        assert!(target.upgrade().is_none());
        let shell = shell.read(cx);
        assert_eq!(
            shell.active(cx).sessions.len(),
            1,
            "read-only Iced main shell retained"
        );
        let input = &shell.bottom.read(cx).input;
        assert!(input.opened && input.visible());
        assert!(input.tabs.is_empty());
    });
    emit(&shell, bottom_panel::Outcome::Resize { height: 900. }, cx);
    cx.read(|cx| assert_eq!(shell.read(cx).bottom.read(cx).input.height, 660.));
    cx.simulate_resize(size(px(1400.), px(500.)));
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.bottom.read(cx).input.window_height, 500.);
        assert_eq!(shell.bottom.read(cx).input.height, 360.);
    });
    click(cx, "pstrip-terminal");
    assert!(cx.debug_bounds("standalone-bottom-panel").is_none());
    for (file, bytes) in ["settings.json", "workspaces.json"].into_iter().zip(before) {
        assert_eq!(std::fs::read(source.join(file)).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_dir(source).unwrap().count(), 2);
}

#[gpui::test]
fn git_mount_actions_file_cover_return_and_unavailable_worktrees(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let source = root.path().join("iced");
    let before = std::fs::read(source.join("workspaces.json")).unwrap();
    click(cx, "open-git-sidebar");
    assert!(cx.debug_bounds("git-changes").is_some());
    let git = cx.read(|cx| {
        let shell = shell.read(cx);
        let state = shell.active(cx);
        assert_eq!(state.store.regions.sidebar_mode, SidebarMode::Git);
        state.panes[&format!("workspace-{}-git", state.workspace.layout_key)]
            .read(cx)
            .git
            .clone()
            .unwrap()
    });
    git.update(cx, |_, cx| {
        cx.emit(pane::git_view::GitAction::EditFile(
            root.path().join("work/file.txt"),
        ))
    });
    draw(cx);
    assert!(cx.debug_bounds("close-file-viewer").is_some());
    click(cx, "close-file-viewer");
    assert!(cx.debug_bounds("git-changes").is_some());
    for action in [
        pane::git_view::GitAction::OpenWorktree(root.path().join("MUST NOT CREATE")),
        pane::git_view::GitAction::AdoptWorktree(root.path().join("MUST NOT CREATE")),
        pane::git_view::GitAction::RemoveWorktree(root.path().join("work")),
    ] {
        git.update(cx, |_, cx| cx.emit(action));
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
    }
    click(cx, "git-back-work");
    assert!(cx.debug_bounds("work-sidebar").is_some());
    assert!(cx.debug_bounds("git-changes").is_none());
    assert!(!root.path().join("MUST NOT CREATE").exists());
    assert!(root.path().join("work/file.txt").exists());
    assert_eq!(
        std::fs::read(source.join("workspaces.json")).unwrap(),
        before
    );
}
