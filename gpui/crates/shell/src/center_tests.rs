//! Test-platform windows only: no native GUI or OS input.
use super::*;
use gitterm_terminal::{Command, SessionState, TerminalView};
use gpui::{TestAppContext, VisualTestContext};
use std::{path::PathBuf, time::Duration};

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
fn view(shell: &Entity<Shell>, index: usize, cx: &App) -> Entity<TerminalView> {
    let state = shell.read(cx).active(cx);
    let pane::PaneContent::Terminal(view) = &state.panes
        [&session_pane_id(state.sessions[index].id)]
        .read(cx)
        .content
    else {
        panic!("live center terminal required")
    };
    view.clone()
}
fn write(view: &Entity<TerminalView>, command: String, cx: &mut VisualTestContext) {
    view.update_in(cx, |view, _, _| {
        view.backend_mut()
            .handle(Command::Write(command.into_bytes()));
    });
}
fn wait(cx: &mut VisualTestContext, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        // Advance GPUI's test clock to deliver the terminal's notification timer.
        cx.executor().advance_clock(Duration::from_millis(20));
        if cx.read(&predicate) {
            return;
        }
        assert!(Instant::now() < deadline, "terminal outcome timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn process_exists(pid: &str) -> bool {
    !std::process::Command::new("/bin/ps")
        .args(["-p", pid, "-o", "stat="])
        .output()
        .unwrap()
        .stdout
        .is_empty()
}

#[gpui::test]
fn ordered_sessions_restore_fresh_shells_and_prune_stale_ids(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| local_shell(&root, config.path(), window, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_terminal(window, cx);
            shell.new_terminal(window, cx);
            shell.new_terminal(window, cx);
        })
    });
    cx.run_until_parked();
    let before = cx.read(|cx| {
        shell
            .read(cx)
            .active(cx)
            .sessions
            .iter()
            .map(|s| s.id.0)
            .collect::<Vec<_>>()
    });
    // A user reorder is persisted in the same order as the dock tabs.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = shell.active(cx);
            let tabs = state
                .sessions
                .iter()
                .rev()
                .fold(DockLayout::tabs(), |tabs, session| {
                    tabs.panel_view(
                        panel_handle(state.panes[&session_pane_id(session.id)].clone()),
                        cx,
                    )
                });
            state.dock.clone().update(cx, |dock, cx| {
                dock.set_center(DockLayout::h_split().child(tabs, None), window, cx)
            });
        })
    });
    cx.run_until_parked();
    let (path, expected) = cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        (
            state.store.path.clone(),
            state.store.terminals.clone().unwrap(),
        )
    });
    assert_eq!(
        expected.iter().map(|s| s.id).collect::<Vec<_>>(),
        before.into_iter().rev().collect::<Vec<_>>()
    );
    assert_eq!(expected.len(), 3);
    let first = cx.read(|cx| view(&shell, 0, cx));
    write(
        &first,
        "printf 'SCROLLBACK_only_before_restart\\n'\r".into(),
        cx,
    );
    wait(cx, |cx| {
        first
            .read(cx)
            .backend()
            .get_all_text()
            .is_some_and(|text| text.contains("SCROLLBACK_only_before_restart"))
    });
    // Distinct launch cwd, plus extra/duplicate dock IDs from stale state.
    let subfolder = root.join("subfolder");
    std::fs::create_dir(&subfolder).unwrap();
    let mut expected = expected;
    expected[0].cwd = subfolder.clone();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["terminal_sessions"] = serde_json::to_value(&expected).unwrap();
    let center = &mut value["center"]["children"][0]["children"];
    let first = center[0].clone();
    center.as_array_mut().unwrap().push(first.clone());
    let mut stale = first;
    stale["info"] = serde_json::json!({"panel":"session-stale"});
    center.as_array_mut().unwrap().push(stale);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    // Release old PTYs before constructing the restart. No layout writes here.
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            for pane in shell.active(cx).panes.values() {
                pane.update(cx, |pane, cx| pane.close_terminal(cx));
            }
        })
    });
    let restarted =
        cx.update(|window, cx| cx.new(|cx| local_shell(&root, config.path(), window, cx)));
    cx.read(|cx| {
        let state = restarted.read(cx).active(cx);
        assert_eq!(state.store.terminals.as_ref().unwrap(), &expected);
        assert!(state.error.is_none());
        assert!(!state.store.is_refused());
        assert_eq!(
            state.sessions.iter().map(|s| s.id.0).collect::<Vec<_>>(),
            expected.iter().map(|s| s.id).collect::<Vec<_>>()
        );
        let ids = layout::pane_order(&state.dock.read(cx).dump(cx));
        assert!(!ids.iter().any(|id| id == "session-stale"));
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("session-")).count(),
            3
        );
        for index in 0..3 {
            assert!(
                !view(&restarted, index, cx)
                    .read(cx)
                    .backend()
                    .get_all_text()
                    .unwrap_or_default()
                    .contains("SCROLLBACK_only_before_restart")
            );
            assert_eq!(
                view(&restarted, index, cx).read(cx).backend().state(),
                SessionState::Running
            );
        }
    });
    let cwd_file = config.path().join("restored-cwd");
    let restored_first = cx.read(|cx| view(&restarted, 0, cx));
    write(
        &restored_first,
        format!("pwd > '{}'\r", cwd_file.display()),
        cx,
    );
    wait(cx, |_| {
        std::fs::read_to_string(&cwd_file).is_ok_and(|s| !s.trim().is_empty())
    });
    assert_eq!(
        PathBuf::from(std::fs::read_to_string(cwd_file).unwrap().trim()),
        subfolder
    );
}

#[gpui::test]
fn fresh_local_workspace_creates_no_terminal(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| local_shell(&folder, config.path(), window, cx));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        assert!(state.sessions.is_empty());
        assert!(state.active_session.is_none());
        assert_eq!(state.store.terminals, Some(vec![]));
        assert!(state.panes.values().all(|pane| {
            pane.read(cx).session.is_none()
                && !matches!(pane.read(cx).content, pane::PaneContent::Terminal(_))
        }));
        assert_eq!(state.panes.len(), 2); // Git and Files only.
        assert!(state.dock.read(cx).layout(DockPlacement::Bottom).is_none());
    });
    cx.simulate_keystrokes("cmd-t");
    cx.read(|cx| assert!(shell.read(cx).active(cx).sessions.is_empty()));
}

#[gpui::test]
fn explicit_terminal_close_reaps_and_hidden_exit_updates_title(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (shell, cx) =
        cx.add_window_view(|window, cx| local_shell(&folder, config.path(), window, cx));
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.new_terminal(window, cx)));
    let first = cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        assert_eq!(state.sessions.len(), 1);
        let first = view(&shell, 0, cx);
        assert_eq!(first.read(cx).backend().state(), SessionState::Running);
        let pane = &state.panes[&session_pane_id(state.sessions[0].id)];
        assert!(
            state
                .dock
                .read(cx)
                .layout(DockPlacement::Center)
                .unwrap()
                .contains_panel(PanelId::from(pane.entity_id()))
        );
        assert!(state.dock.read(cx).layout(DockPlacement::Bottom).is_none());
        assert_eq!(
            pane.read(cx).tab_title(),
            folder.file_name().unwrap().to_str().unwrap()
        );
        first
    });
    let cwd_file = config.path().join("cwd");
    write(
        &first,
        format!(
            "pwd > '{}'; echo $$ > '{}'\r",
            cwd_file.display(),
            config.path().join("pid").display()
        ),
        cx,
    );
    wait(cx, |_| {
        cwd_file.exists() && config.path().join("pid").exists()
    });
    assert_eq!(
        std::fs::read_to_string(&cwd_file).unwrap().trim(),
        folder.to_str().unwrap()
    );
    let pid = std::fs::read_to_string(config.path().join("pid"))
        .unwrap()
        .trim()
        .to_string();
    assert!(process_exists(&pid));
    // Cmd+T is unbound even with terminal focus; explicit creation still works.
    cx.update(|window, cx| {
        first.read(cx).focus_handle().clone().focus(window, cx);
        window.draw(cx).clear(cx);
        let bindings = window.bindings_for_action_in(&NewTerminal, first.read(cx).focus_handle());
        assert!(bindings.is_empty());
        window.dispatch_action(Box::new(NewTerminal), cx);
    });
    cx.run_until_parked();
    cx.read(|cx| assert_eq!(shell.read(cx).active(cx).sessions.len(), 2));
    // First terminal is hidden; OSC title/reset/exit must still reach its tab.
    write(&first, "printf '\\033]2;hidden title\\007'\r".into(), cx);
    wait(cx, |cx| {
        shell.read(cx).active(cx).panes[&session_pane_id(shell.read(cx).active(cx).sessions[0].id)]
            .read(cx)
            .tab_title()
            == "hidden title"
    });
    write(&first, "printf '\\033]0;\\007'; exit 7\r".into(), cx);
    wait(cx, |cx| {
        let state = shell.read(cx).active(cx);
        state.panes[&session_pane_id(state.sessions[0].id)]
            .read(cx)
            .tab_title()
            .ends_with(" · Exited")
    });
    assert_eq!(
        cx.read(|cx| first.read(cx).backend().state()),
        SessionState::Exited(Some(7))
    );
    assert!(!process_exists(&pid), "exited shell was not reaped");
    let second = cx.read(|cx| view(&shell, 1, cx));
    let second_pid_file = config.path().join("second-pid");
    write(
        &second,
        format!("echo $$ > '{}'\r", second_pid_file.display()),
        cx,
    );
    wait(cx, |_| {
        std::fs::read_to_string(&second_pid_file).is_ok_and(|s| !s.trim().is_empty())
    });
    let second_pid = std::fs::read_to_string(&second_pid_file)
        .unwrap()
        .trim()
        .to_string();
    // Dock tab close must terminate, even while another handle owns the view.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = shell.active(cx);
            let pane = state.panes[&session_pane_id(state.sessions[1].id)].clone();
            state
                .dock
                .clone()
                .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        })
    });
    wait(cx, |cx| shell.read(cx).active(cx).sessions.len() == 1);
    assert!(
        !process_exists(&second_pid),
        "closed shell remains alive or zombie"
    );
    assert_eq!(
        cx.read(|cx| second.read(cx).backend().state()),
        SessionState::Cancelled
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let id = shell.active(cx).sessions[0].id;
            shell.close_session(id, window, cx);
        })
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = shell.active(cx);
            let pane = state.panes[&session_pane_id(state.sessions[0].id)].clone();
            state
                .dock
                .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        })
    });
    cx.run_until_parked();
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        assert_eq!(state.sessions.len(), 1);
        assert!(pane_id_visible(
            state,
            &session_pane_id(state.sessions[0].id),
            cx
        ));
    });
}
