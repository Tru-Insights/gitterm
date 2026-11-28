use super::*;
use crate::{
    WorkspaceOwner,
    data::{LocalData, ShellData},
    pane::PaneContent,
    preview,
    workspace_status::WorkspaceStatus,
};
use gpui::{AppContext as _, Entity, TestAppContext};
use std::rc::Rc;

#[test]
fn parked_count_excludes_saved_entries_marked_done_like_iced() {
    let saved = [
        SavedPark {
            name: "still parked".into(),
            dir: PathBuf::from("/tmp/parked"),
            note: String::new(),
            parked_at: "2026-11-01T00:00:00Z".into(),
            done: false,
        },
        SavedPark {
            name: "done".into(),
            dir: PathBuf::from("/tmp/done"),
            note: String::new(),
            parked_at: "2026-11-02T00:00:00Z".into(),
            done: true,
        },
    ];

    assert_eq!(parked_count(3, &saved, 2, 1), 7);
}

struct Fixture {
    _root: tempfile::TempDir,
    config: PathBuf,
    folders: Vec<PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().canonicalize().unwrap().join("preview");
        let folders: Vec<_> = ["one", "two", "three"]
            .into_iter()
            .map(|name| {
                let path = root.path().join(name);
                std::fs::create_dir(&path).unwrap();
                path.canonicalize().unwrap()
            })
            .collect();
        Self {
            _root: root,
            config,
            folders,
        }
    }
    fn shell(&self, window: &mut Window, cx: &mut Context<Shell>) -> Shell {
        let fresh = !self.config.exists();
        let mut model = WorkspaceStatus::default();
        for folder in &self.folders {
            model.add(folder.clone(), Instant::now());
        }
        let mut store = preview::tests::guarded_store(&self.config);
        store.load().unwrap();
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        let data: Rc<dyn ShellData> = Rc::new(LocalData(owner.clone()));
        let mut shell = Shell::with_owner(data, owner, &self.config, true, window, cx).unwrap();
        if fresh {
            // Park scenarios explicitly open their terminals; startup creates none.
            for index in 0..self.folders.len() {
                shell.select_workspace(index, window, cx);
                shell.new_terminal(window, cx);
            }
            shell.select_workspace(0, window, cx);
        }
        shell
    }
}

fn terminal(shell: &Shell, workspace: usize, cx: &App) -> Entity<gitterm_terminal::TerminalView> {
    let state = &shell.workspaces[workspace];
    let id = state.sessions[0].id;
    let PaneContent::Terminal(view) = &state.panes[&crate::session_pane_id(id)].read(cx).content
    else {
        panic!("expected live fixture terminal")
    };
    view.clone()
}

#[gpui::test]
fn create_park_keep_resume_stop_close_and_stale_targets(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_terminal(window, cx);
            assert_eq!(shell.workspaces[0].sessions.len(), 2);
            let first = terminal(shell, 0, cx);
            let target = shell.workspaces[0].park_target(cx);
            shell.park_workspace(&target, Mode::KeepRunning, window, cx);
            assert!(!shell.workspaces[0].on_bar());
            assert_eq!(terminal(shell, 0, cx), first);
            assert_ne!(
                shell.active_workspace(cx),
                Some(shell.workspaces[0].workspace.id)
            );
            shell.select_workspace(0, window, cx);
            assert_eq!(
                shell.active_workspace(cx),
                Some(shell.workspaces[1].workspace.id)
            );
            let key = shell.workspaces[0].park_key(cx);
            shell.park_intent(None, Intent::Resume(key.clone()), window, cx);
            assert_eq!(terminal(shell, 0, cx), first);
            assert!(shell.workspaces[0].on_bar());

            let mut stale = target.clone();
            stale.tabs[0].session_uid.push('x');
            shell.park_workspace(&stale, Mode::Stop, window, cx);
            assert!(shell.workspaces[0].on_bar());
            shell.apply_park(
                Effect::Park {
                    target: stale,
                    mode: Mode::Stop,
                    info: Info::new("", "fixture".into()),
                },
                window,
                cx,
            );
            assert_eq!(terminal(shell, 0, cx), first);

            shell.park_workspace(&target, Mode::Stop, window, cx);
            assert_eq!(
                first.read(cx).backend().state(),
                gitterm_terminal::SessionState::Cancelled
            );
            assert!(
                shell.workspaces[0]
                    .panes
                    .values()
                    .all(|pane| pane.read(cx).session.is_none())
            );
            shell.park_intent(None, Intent::Resume(key), window, cx);
            let replacement = terminal(shell, 0, cx);
            assert_ne!(replacement, first);
            assert_ne!(shell.workspaces[0].park_target(cx), target);
            // Both a delayed workspace effect and a delayed close reject the replacement.
            shell.park_workspace(&target, Mode::Stop, window, cx);
            for tab in target.tabs {
                shell.close_target(&tab, window, cx);
            }
            assert_eq!(shell.workspaces[0].sessions.len(), 2);
            let tab = shell.workspaces[0].sessions[0].id;
            let close = shell.workspaces[0].panes[&crate::session_pane_id(tab)]
                .read(cx)
                .session
                .clone()
                .unwrap();
            // Changing selected workspace cannot redirect a captured close.
            shell.select_workspace(1, window, cx);
            shell.close_target(&close, window, cx);
            assert_eq!(shell.workspaces[0].sessions.len(), 1);
            assert_eq!(shell.workspaces[1].sessions.len(), 1);
            let current = shell.workspaces[0].park_target(cx);
            shell.park_intent(Some(&current), Intent::Close(current.uid), window, cx);
            assert!(shell.workspaces[0].parked.as_ref().unwrap().done);
            let second = shell.workspaces[1].park_target(cx);
            shell.park_workspace(&second, Mode::KeepRunning, window, cx);
            let last = shell.workspaces[2].park_target(cx);
            shell.park_workspace(&last, Mode::Stop, window, cx);
            shell.park_intent(Some(&last), Intent::Close(last.uid), window, cx);
            assert!(
                shell.workspaces[2].on_bar(),
                "last bar workspace is protected"
            );
            let key = shell.workspaces[1].park_key(cx);
            let running = terminal(shell, 1, cx);
            shell.park_intent(Some(&second), Intent::Done(key), window, cx);
            assert!(shell.workspaces[1].parked.as_ref().unwrap().done);
            assert_eq!(
                running.read(cx).backend().state(),
                gitterm_terminal::SessionState::Cancelled
            );
            shell.reopen_workspace(shell.workspaces[0].workspace.id, window, cx);
            assert!(shell.workspaces[0].on_bar());
            assert_ne!(terminal(shell, 0, cx), replacement);
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
fn restart_is_stopped_and_resume_never_replays_commands(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    let view = cx.read(|cx| terminal(shell.read(cx), 0, cx));
    let receipt = fixture.folders[0].join("once");
    view.update(cx, |view, _| {
        view.backend_mut().handle(gitterm_terminal::Command::Write(
            b"printf x >> once\r".to_vec(),
        ))
    });
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !receipt.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture shell did not execute explicit input"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let target = shell.workspaces[0].park_target(cx);
            shell.park_workspace(&target, Mode::KeepRunning, window, cx);
        })
    });
    // Drain the guarded queue before simulating restart. No GUI/real config used.
    let writer = cx.read(|cx| shell.read(cx).owner.read(cx).writer().unwrap());
    writer.close().unwrap().blocking_recv().unwrap().unwrap();
    let mut stored = preview::tests::guarded_store(&fixture.config);
    stored.load().unwrap();
    let saved = stored.parked;
    drop(stored.guard);
    assert_eq!(saved.len(), 1);
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            // Terminate fixture PTYs, as process exit would, before opening a new owner.
            for state in &shell.workspaces {
                for pane in state.panes.values() {
                    pane.update(cx, |pane, cx| pane.close_terminal(cx));
                }
            }
        })
    });
    // A second fixture window with the same validated root loads layouts and park metadata.
    // Construct in the same TestAppContext via its visual context's app update below.
    cx.update(|window, cx| {
        let restarted = cx.new(|cx| fixture.shell(window, cx));
        restarted.update(cx, |shell, cx| {
            assert!(
                shell.workspaces[0]
                    .parked
                    .as_ref()
                    .is_some_and(|parked| !parked.running)
            );
            assert!(
                shell.workspaces[0]
                    .panes
                    .values()
                    .all(|pane| pane.read(cx).session.is_none())
            );
            let key = shell.workspaces[0].park_key(cx);
            shell.park_intent(None, Intent::Resume(key), window, cx);
            assert!(shell.workspaces[0].on_bar());
            let fresh = terminal(shell, 0, cx);
            assert_ne!(fresh, view);
            fresh.update(cx, |view, _| {
                view.backend_mut().handle(gitterm_terminal::Command::Write(
                    b"printf ready > ready\r".to_vec(),
                ))
            });
        });
        // Hold the restarted owner until explicit input proves the shell is ready.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !fixture.folders[0].join("ready").exists() {
            assert!(Instant::now() < deadline, "resumed shell did not start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&receipt).unwrap(), b"x");
    });
}

#[test]
fn invalid_park_files_are_rejected_without_rewriting() {
    let fixture = Fixture::new();
    let mut store = preview::tests::guarded_store(&fixture.config);
    let writer = store.guard.clone().unwrap();
    let path = fixture.config.join("local-workspaces.json");
    for bytes in [b"{".as_slice(), br#"{"version":2,"folders":[]}"#, br#"{"version":1,"folders":[],"parked_workspaces":[{"name":"fixture","dir":"relative","note":"","parked_at":"","done":false}]}"#] {
        writer.replace_layout(&path, bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&[]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    for effect in [
        gitterm_core::preview::Effect::Agent,
        gitterm_core::preview::Effect::Worker,
        gitterm_core::preview::Effect::RestoreProcess,
    ] {
        assert!(preview::executable(Some(writer.preview()), effect, &fixture.folders[0]).is_err());
    }
}

#[gpui::test]
fn terminal_exit_wakes_parked_workspace_without_changing_selection(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    cx.run_until_parked();
    let view = cx.read(|cx| terminal(shell.read(cx), 0, cx));
    let selected = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let target = shell.workspaces[0].park_target(cx);
            shell.park_workspace(&target, Mode::KeepRunning, window, cx);
            shell.active_workspace(cx)
        })
    });
    view.update(cx, |view, _| {
        view.backend_mut()
            .handle(gitterm_terminal::Command::Write(b"exit 0\r".to_vec()))
    });
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
        if cx.read(|cx| shell.read(cx).workspaces[0].on_bar()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hidden terminal exit did not wake workspace"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cx.read(|cx| {
        assert_eq!(shell.read(cx).active_workspace(cx), selected);
        assert_eq!(terminal(shell.read(cx), 0, cx), view);
    });
}

#[gpui::test]
fn delayed_dock_close_never_stops_a_resumed_replacement(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    cx.run_until_parked();
    let (pane, replacement) = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_terminal(window, cx);
            let state = &shell.workspaces[0];
            let pane = state.panes[&crate::session_pane_id(state.sessions[0].id)].clone();
            let dock = state.dock.clone();
            let target = state.park_target(cx);
            let key = state.park_key(cx);
            dock.update(cx, |dock, cx| dock.remove_panel(pane.clone(), window, cx));
            // Do not deliver the removal event until the old PTY was stopped and
            // the same layout ID/pane entity contains a new runtime incarnation.
            shell.park_workspace(&target, Mode::Stop, window, cx);
            shell.park_intent(None, Intent::Resume(key), window, cx);
            let PaneContent::Terminal(view) = &pane.read(cx).content else {
                panic!("resumed terminal")
            };
            let replacement = view.clone();
            (pane, replacement)
        })
    });
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(shell.read(cx).workspaces[0].sessions.len(), 2);
        let PaneContent::Terminal(view) = &pane.read(cx).content else {
            panic!("resumed terminal")
        };
        assert_eq!(view, &replacement);
        assert_ne!(
            replacement.read(cx).backend().state(),
            gitterm_terminal::SessionState::Cancelled
        );
    });
    // A fresh, explicit dock close still stops that exact PTY with retained handles.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let state = &shell.workspaces[0];
            state
                .dock
                .clone()
                .update(cx, |dock, cx| dock.remove_panel(pane.clone(), window, cx));
        })
    });
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(shell.read(cx).workspaces[0].sessions.len(), 1);
        assert_eq!(
            replacement.read(cx).backend().state(),
            gitterm_terminal::SessionState::Cancelled
        );
    });
}
