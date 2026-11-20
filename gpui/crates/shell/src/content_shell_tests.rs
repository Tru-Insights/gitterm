//! FileOpen bridge on GPUI's test platform: no AppKit/WebKit or GUI launch.
use super::*;
use gpui::TestAppContext;

#[gpui::test]
fn files_open_content_reopen_focus_close_and_refuse_outside_root(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    std::fs::write(folder.join("guide.md"), "# Test document").unwrap();
    std::fs::write(folder.join("page.htm"), "<p>HTML</p>").unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| local_shell(&folder, config.path(), window, cx));
    let (owner, workspace) = cx.read(|cx| {
        let shell = shell.read(cx);
        (shell.owner.clone(), shell.active_workspace(cx).unwrap())
    });
    let open = |name: &str, cx: &mut gpui::VisualTestContext| {
        owner.update(cx, |owner, cx| {
            owner.open_file(workspace, folder.join(name), cx)
        });
        cx.run_until_parked();
    };
    open("guide.md", cx);
    let id = content_pane_id(workspace, &folder.join("guide.md"));
    let original = cx.read(|cx| {
        let shell = shell.read(cx);
        let state = shell.active(cx);
        let pane = state.panes[&id].clone();
        assert!(matches!(
            pane.read(cx).content,
            pane::PaneContent::Content(_)
        ));
        assert!(
            state
                .dock
                .read(cx)
                .layout(DockPlacement::Center)
                .unwrap()
                .contains_panel(PanelId::from(pane.entity_id()))
        );
        assert_eq!(shell.content.read(cx).selected.as_ref(), Some(&id));
        pane.entity_id()
    });
    open("page.htm", cx);
    open("guide.md", cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.active(cx).panes[&id].entity_id(), original);
        assert!(pane_id_visible(shell.active(cx), &id, cx));
        assert_eq!(shell.content.read(cx).selected.as_ref(), Some(&id));
    });
    let before = cx.read(|cx| shell.read(cx).active(cx).panes.len());
    open("unsupported.rs", cx);
    cx.read(|cx| assert_eq!(shell.read(cx).active(cx).panes.len(), before));
    owner.update(cx, |owner, cx| {
        owner.open_file(workspace, folder.parent().unwrap().join("outside.md"), cx)
    });
    cx.run_until_parked();
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.active(cx).panes.len(), before);
        assert!(
            shell
                .owner
                .read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("outside workspace root")
        );
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = shell.active(cx);
            let pane = state.panes[&id].clone();
            state
                .dock
                .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        });
    });
    cx.run_until_parked();
    cx.read(|cx| assert!(!shell.read(cx).active(cx).panes.contains_key(&id)));
    open("guide.md", cx);
    cx.read(|cx| assert_ne!(shell.read(cx).active(cx).panes[&id].entity_id(), original));
}

fn local_shell(
    folder: &Path,
    config: &Path,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Shell {
    let mut model = WorkspaceStatus::default();
    model.add(folder.to_path_buf(), Instant::now());
    let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
    Shell::with_owner(
        Rc::new(data::LocalData(owner.clone())),
        owner,
        config,
        true,
        window,
        cx,
    )
    .unwrap()
}

#[gpui::test]
fn zoomed_split_selects_its_content_and_non_content_zoom_hides_all_surfaces(
    cx: &mut TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    std::fs::write(root.join("guide.md"), "# Guide").unwrap();
    std::fs::write(root.join("page.htm"), "<p>Page</p>").unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| local_shell(&root, config.path(), window, cx));
    let (owner, workspace) = cx.read(|cx| {
        (
            shell.read(cx).owner.clone(),
            shell.read(cx).active_workspace(cx).unwrap(),
        )
    });
    for file in ["guide.md", "page.htm"] {
        owner.update(cx, |owner, cx| {
            owner.open_file(workspace, root.join(file), cx)
        });
        cx.run_until_parked();
    }
    let guide_id = content_pane_id(workspace, &root.join("guide.md"));
    let page_id = content_pane_id(workspace, &root.join("page.htm"));
    let (guide, page, git, dock) = cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        (
            state.panes[&guide_id].clone(),
            state.panes[&page_id].clone(),
            state.panes[&format!("workspace-{}-git", state.workspace.layout_key)].clone(),
            state.dock.clone(),
        )
    });
    cx.update(|window, cx| {
        dock.update(cx, |dock, cx| {
            dock.set_center(
                DockLayout::h_split()
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(guide.clone()), cx),
                        None,
                    )
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(page.clone()), cx),
                        None,
                    ),
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).content.read(cx).selected.as_ref(),
            Some(&page_id)
        )
    });
    let zoom = |pane: &Entity<Pane>, cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            dock.update(cx, |dock, cx| {
                let panel = PanelId::from(pane.entity_id());
                let node = [DockPlacement::Center, DockPlacement::Right]
                    .into_iter()
                    .find_map(|placement| {
                        dock.layout(placement)
                            .and_then(|tree| tree.find_panel_node(panel))
                    })
                    .unwrap();
                dock.set_zoomed_in(node, window, cx);
            })
        });
        cx.run_until_parked();
    };
    zoom(&guide, cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).content.read(cx).selected.as_ref(),
            Some(&guide_id)
        )
    });
    zoom(&page, cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).content.read(cx).selected.as_ref(),
            Some(&page_id)
        )
    });
    zoom(&git, cx);
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).selected.is_none()));
    cx.update(|window, cx| dock.update(cx, |dock, cx| dock.set_zoomed_out(window, cx)));
    cx.run_until_parked();
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).selected.is_some()));
}
