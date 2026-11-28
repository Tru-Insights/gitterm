use super::*;
use gitterm_core::desktop_store::DesktopStore;
use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use serde_json::json;

fn fixture(
    cx: &mut TestAppContext,
    mut workspaces: serde_json::Value,
) -> (
    tempfile::TempDir,
    DesktopStore,
    Entity<Shell>,
    &mut VisualTestContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    workspaces.as_array_mut().unwrap().push(json!({
        "name":"Support", "abbrev":"SP", "dir":root.path(), "color":"blue", "tabs":[]
    }));
    let desktop = root.path().join("desktop");
    std::fs::create_dir(&desktop).unwrap();
    std::fs::write(
        desktop.join("workspaces.json"),
        serde_json::to_vec(&json!({
            "active_workspace":0, "workspaces":workspaces
        }))
        .unwrap(),
    )
    .unwrap();
    let live = DesktopStore::open(&desktop).unwrap();
    cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(live.clone())));
    let layout = root.path().join("preview");
    let mut store = crate::preview::tests::guarded_store(&layout);
    store.iced = Some(crate::workspace_config::IcedCatalog::read(&desktop).unwrap());
    let model = crate::workspace_status::WorkspaceStatus::from_iced(store.iced.as_ref().unwrap());
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        let data = std::rc::Rc::new(crate::data::LocalData(owner.clone()));
        Shell::with_owner(data, owner, &layout, true, window, cx).unwrap()
    });
    (root, live, shell, cx)
}

#[gpui::test]
fn review_f1_filtered_resume_reconciles_roster_and_reparks_the_right_identity(
    cx: &mut TestAppContext,
) {
    let cwd = tempfile::tempdir_in("/private/tmp").unwrap();
    let (root, live, shell, cx) = fixture(
        cx,
        json!([{
            "name":"Synthetic", "abbrev":"S", "dir":cwd.path(), "color":"blue", "active_tab":1,
            "tabs":[
                {"dir":cwd.path(), "session_uid":"chat-caller", "tab_kind":"agent", "chat_session_id":"fixture-chat"},
                {"dir":cwd.path(), "session_uid":"terminal-caller", "future_tab":"keep"}
            ]
        }]),
    );
    let key = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = &shell.workspaces[0];
            let key = state.park_key(cx);
            shell.park_workspace(&state.park_target(cx), Mode::Stop, window, cx);
            key
        })
    });
    cx.run_until_parked();
    live.transact_workspaces(|file| {
        let mut other = file
            .workspaces
            .iter()
            .find(|w| w.name == "Synthetic")
            .unwrap()
            .clone();
        other.name = "Elsewhere".into();
        other.tabs.truncate(1);
        other.active = true;
        other.parked = None;
        file.workspaces.push(other);
        Ok(())
    })
    .wait()
    .unwrap();
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.resume_live_workspace(key, false, window, cx);
        })
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = &shell.workspaces[0];
            assert_eq!(
                state.sessions.len(),
                1,
                "filtered chat must have no runtime pane"
            );
            let pane = state.panes[&crate::session_pane_id(state.sessions[0].id)].read(cx);
            assert_eq!(
                pane.session.as_ref().unwrap().session_uid,
                "terminal-caller"
            );
            assert_eq!(
                shell.live_workspace_tabs(state.runtime_uid, cx)[0].extra["future_tab"],
                "keep"
            );
            shell.park_workspace(&state.park_target(cx), Mode::Stop, window, cx);
        })
    });
    cx.run_until_parked();
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("desktop/workspaces.json")).unwrap(),
    )
    .unwrap();
    let tabs = saved["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "Synthetic")
        .unwrap()["tabs"]
        .as_array()
        .unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["session_uid"], "terminal-caller");
    assert_eq!(tabs[0]["future_tab"], "keep");
}

#[gpui::test]
fn review_f2_owner_confirmation_refuses_ambiguous_workspace_without_writing(
    cx: &mut TestAppContext,
) {
    cx.background_executor.allow_parking();
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let path = root.path().join("workspaces.json");
    let workspace =
        json!({"name":"Synthetic", "abbrev":"S", "dir":"/fixture/repo", "color":"blue", "tabs":[]});
    std::fs::write(
        &path,
        serde_json::to_vec(
            &json!({"active_workspace":0,"workspaces":[workspace.clone(), workspace]}),
        )
        .unwrap(),
    )
    .unwrap();
    let live = DesktopStore::open(root.path()).unwrap();
    let mut store = crate::workspace_config::WorkspaceStore::new(root.path());
    store.iced = Some(crate::workspace_config::IcedCatalog::read(root.path()).unwrap());
    let model = crate::workspace_status::WorkspaceStatus::from_iced(store.iced.as_ref().unwrap());
    let owner = cx.update(|cx| {
        cx.set_global(crate::workspace_config::LiveWorkspaces(live.clone()));
        cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx))
    });
    let before = std::fs::read(&path).unwrap();
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner.confirm_live_tab(
                "Synthetic",
                "/fixture/repo",
                crate::chat::resume::tab(
                    std::path::Path::new("/fixture/repo"),
                    "fixture-caller".into(),
                    Some("fixture-confirmed".into()),
                ),
                cx,
            )
        });
    });
    cx.run_until_parked();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let gitterm_core::desktop_store::LoadState::Valid(document) =
        live.snapshot().wait().unwrap().workspaces.loaded
    else {
        panic!("valid document")
    };
    assert!(document.workspaces.iter().all(|w| w.tabs.is_empty()));
    cx.read(|cx| {
        let owner = owner.read(cx);
        assert!(
            owner
                .error
                .as_deref()
                .unwrap()
                .contains("Several saved workspaces")
        );
        assert!(
            owner
                .iced_catalog()
                .unwrap()
                .workspaces
                .iter()
                .all(|w| w.tabs.is_empty())
        );
    });
}

#[gpui::test]
fn review_f3_park_separates_bottom_cwds_and_excludes_run_panes(cx: &mut TestAppContext) {
    let cwd = tempfile::tempdir_in("/private/tmp").unwrap();
    let bottom = cwd.path().join("bottom");
    std::fs::create_dir(&bottom).unwrap();
    let (root, _live, shell, cx) = fixture(
        cx,
        json!([{
            "name":"Synthetic", "abbrev":"S", "dir":cwd.path(), "color":"blue",
            "tabs":[{"dir":cwd.path(), "session_uid":"main-caller"}],
            "bottom_terminals":[{"dir":"/fixture/stale"}]
        }]),
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let main = shell.workspaces[0].sessions[0].id;
            shell.new_terminal_at(
                gpui_component::dock::DockPlacement::Bottom,
                Some(cwd.path().into()),
                window,
                cx,
            );
            let id = crate::session_pane_id(shell.workspaces[0].sessions.last().unwrap().id);
            shell.workspaces[0].panes[&id]
                .update(cx, |pane, _| pane.terminal_cwd = Some(bottom.clone()));
            shell.new_terminal_at(
                gpui_component::dock::DockPlacement::Bottom,
                Some(cwd.path().into()),
                window,
                cx,
            );
            let run = crate::session_pane_id(shell.workspaces[0].sessions.last().unwrap().id);
            shell.run_tabs.insert(
                run,
                crate::bottom_panel::RunTab {
                    repo: gitterm_core::workspace_config::RepoKey::Local(cwd.path().into()),
                    command: "fixture-command".into(),
                    detected_url: None,
                    next_url_scan: std::time::Instant::now(),
                },
            );
            let state = &mut shell.workspaces[0];
            state.sessions.rotate_left(1);
            state.active_session = Some(main);
            let target = state.park_target(cx);
            shell.park_workspace(&target, Mode::Stop, window, cx);
        })
    });
    cx.run_until_parked();
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("desktop/workspaces.json")).unwrap(),
    )
    .unwrap();
    let workspace = saved["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "Synthetic")
        .unwrap();
    assert_eq!(workspace["tabs"].as_array().unwrap().len(), 1);
    assert_eq!(workspace["tabs"][0]["session_uid"], "main-caller");
    assert_eq!(workspace["active_tab"], 0);
    assert_eq!(workspace["bottom_terminals"], json!([{"dir":bottom}]));
}
