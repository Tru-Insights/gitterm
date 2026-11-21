//! H7 host routing over isolated Iced and preview fixtures.
use super::*;
use gitterm_core::workspace_config::RepoKey;
use gpui::{TestAppContext, VisualTestContext};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn dispatch(action: impl gpui::Action, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(action), cx));
    draw(cx);
}
fn emit(shell: &Entity<Shell>, outcome: bottom_panel::Outcome, cx: &mut VisualTestContext) {
    let panel = cx.read(|cx| shell.read(cx).bottom.clone());
    panel.update(cx, |_, cx| cx.emit(outcome));
    draw(cx);
}
fn wait_file(path: &Path, expected: &str, cx: &mut VisualTestContext) {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        draw(cx);
        if std::fs::read_to_string(path).ok().as_deref() == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "command did not produce {}",
            path.display()
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
pub(super) fn fixture(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(install_app_menus);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    let work = root.path().join("work");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&work).unwrap();
    let second = root.path().join("second");
    std::fs::create_dir(&second).unwrap();
    std::fs::write(work.join("file.txt"), "retained file").unwrap();
    std::fs::write(
        source.join("settings.json"),
        r#"{"quick_commands":[{"name":"test","command":"printf q >> h7-quick.txt"}]}"#,
    )
    .unwrap();
    std::fs::write(source.join("workspaces.json"), serde_json::json!({
        "active_workspace":0,"repos":[{"dir":work,"run_command":"printf x >> h7-run.txt"}],
        "workspaces":[{"name":"H7","abbrev":"H7","color":"blue","dir":work,"repos":[work],"tabs":[]},
        {"name":"second","abbrev":"S","color":"green","dir":second,"repos":[second],"tabs":[]}]
    }).to_string()).unwrap();
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    let settings = workspace_config::load_iced_settings_from(&source);
    cx.update(|cx| theme::apply_settings(&settings, None, cx));
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
    cx.update(|window, cx| shell.focus_handle(cx).focus(window, cx));
    draw(cx);
    (root, shell, cx)
}

#[gpui::test]
fn usage_toggle_preserves_previous_content_and_escape_returns(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let workspace = shell.active_workspace(cx).unwrap();
            shell.open_file_viewer(
                &FileOpen {
                    workspace,
                    path: root.path().join("work/file.txt"),
                    changed: None,
                    from_files: true,
                },
                window,
                cx,
            );
        })
    });
    draw(cx);
    let before = cx.read(|cx| shell.read(cx).content.read(cx).selected.clone());
    assert!(before.is_some());
    dispatch(app_menus::UsageViewToggle, cx);
    cx.read(|cx| assert!(shell.read(cx).usage_open));
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| {
        assert!(!shell.read(cx).usage_open);
        assert_eq!(shell.read(cx).content.read(cx).selected, before);
    });
    dispatch(app_menus::UsageViewToggle, cx);
    dispatch(app_menus::UsageViewToggle, cx);
    cx.read(|cx| assert!(!shell.read(cx).usage_open));
}

#[gpui::test]
fn run_spawns_focuses_restarts_and_quick_command_uses_plain_terminal(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let repo = RepoKey::local(&root.path().join("work"));
    emit(
        &shell,
        bottom_panel::Outcome::Run { repo: repo.clone() },
        cx,
    );
    wait_file(&root.path().join("work/h7-run.txt"), "x", cx);
    let (id, original) = cx.read(|cx| {
        let shell = shell.read(cx);
        let tabs = &shell.bottom.read(cx).input;
        assert_eq!(tabs.tabs.len(), 1);
        assert_eq!(tabs.repos[0].run_state, bottom_panel::RunState::Focus);
        let id = tabs.tabs[0].id;
        let pane::PaneContent::Terminal(view) = &shell.active(cx).panes
            [&session_pane_id(SessionId(id as u64))]
            .read(cx)
            .content
        else {
            panic!("terminal");
        };
        (id, view.clone())
    });
    emit(&shell, bottom_panel::Outcome::Toggle, cx);
    cx.read(|cx| assert!(!shell.read(cx).bottom.read(cx).input.visible()));
    emit(
        &shell,
        bottom_panel::Outcome::Focus { repo: repo.clone() },
        cx,
    );
    cx.read(|cx| assert!(shell.read(cx).bottom.read(cx).input.visible()));
    cx.read(|cx| assert_eq!(shell.read(cx).bottom.read(cx).input.tabs.len(), 1));
    dispatch(app_menus::RunRepoCommand, cx);
    assert_eq!(
        std::fs::read_to_string(root.path().join("work/h7-run.txt")).unwrap(),
        "x"
    );
    original.update(cx, |view, _| view.backend_mut().cancel());
    draw(cx);
    emit(&shell, bottom_panel::Outcome::Restart { repo }, cx);
    wait_file(&root.path().join("work/h7-run.txt"), "xx", cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.bottom.read(cx).input.tabs.len(), 1);
        let pane::PaneContent::Terminal(view) = &shell.active(cx).panes
            [&session_pane_id(SessionId(id as u64))]
            .read(cx)
            .content
        else {
            panic!("terminal");
        };
        assert_ne!(view.entity_id(), original.entity_id());
    });
    emit(&shell, bottom_panel::Outcome::QuickCommand { id: 0 }, cx);
    wait_file(&root.path().join("work/h7-quick.txt"), "q", cx);
    cx.read(|cx| assert_eq!(shell.read(cx).bottom.read(cx).input.tabs.len(), 2));
    emit(&shell, bottom_panel::Outcome::QuickCommand { id: 0 }, cx);
    wait_file(&root.path().join("work/h7-quick.txt"), "qq", cx);
    cx.read(|cx| assert_eq!(shell.read(cx).bottom.read(cx).input.tabs.len(), 2));
    emit(
        &shell,
        bottom_panel::Outcome::AddTerminal { repo: None },
        cx,
    );
    let (middle, selected) = cx.read(|cx| {
        let input = &shell.read(cx).bottom.read(cx).input;
        (input.tabs[1].id, input.active)
    });
    emit(&shell, bottom_panel::Outcome::CloseTab { id: middle }, cx);
    cx.read(|cx| assert_eq!(shell.read(cx).bottom.read(cx).input.active, selected));
    emit(
        &shell,
        bottom_panel::Outcome::CloseTab {
            id: selected.unwrap(),
        },
        cx,
    );
    cx.update(|window, cx| {
        let shell = shell.read(cx);
        let main = shell.active(cx).active_session.unwrap();
        assert!(
            shell.active(cx).panes[&session_pane_id(main)]
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
        assert_eq!(shell.bottom.read(cx).input.tabs.len(), 1);
    });
}

#[gpui::test]
fn chat_font_clamps_and_writes_only_preview(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let source = root.path().join("iced");
    let before =
        ["settings.json", "workspaces.json"].map(|name| std::fs::read(source.join(name)).unwrap());
    for _ in 0..30 {
        dispatch(app_menus::IncreaseChatFont, cx);
    }
    cx.read(|cx| assert_eq!(theme::chat_appearance(cx).font_size, 22.));
    for _ in 0..30 {
        dispatch(app_menus::DecreaseChatFont, cx);
    }
    cx.read(|cx| {
        assert_eq!(theme::chat_appearance(cx).font_size, 11.);
        assert_eq!(
            shell.read(cx).active(cx).store.regions.chat_font_size,
            Some(11.)
        );
    });
    assert_eq!(
        ["settings.json", "workspaces.json"].map(|name| std::fs::read(source.join(name)).unwrap()),
        before
    );
    assert_eq!(std::fs::read_dir(source).unwrap().count(), 2);
    let (path, config, owner) = cx.read(|cx| {
        let shell = shell.read(cx);
        (
            shell.active(cx).store.path.clone(),
            shell.config_dir.clone(),
            shell.owner.clone(),
        )
    });
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        draw(cx);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if value["regions"]["chat_font_size"] == 11. {
            break;
        }
        assert!(Instant::now() < deadline, "chat font was not persisted");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cx.update(|window, cx| {
        theme::set_chat_font_size(15., cx);
        let _restored = cx.new(|cx| {
            Shell::with_owner(
                Rc::new(data::LocalData(owner.clone())),
                owner,
                &config,
                true,
                window,
                cx,
            )
            .unwrap()
        });
        assert_eq!(theme::chat_appearance(cx).font_size, 11.);
    });
}

#[test]
fn run_output_url_matches_iced_patterns() {
    assert_eq!(
        bottom_panel::output_url("Server at http://localhost:3000)"),
        Some("http://localhost:3000".into())
    );
    assert_eq!(
        bottom_panel::output_url("listening on :4000"),
        Some("http://localhost:4000".into())
    );
    assert_eq!(bottom_panel::output_url("port 0"), None);
}

#[gpui::test]
fn usage_limits_filters_and_pin_are_read_only(cx: &mut TestAppContext) {
    use gitterm_core::plan_limits::{PlanLimitsFile, Provider, RoutingPin};
    let (root, shell, cx) = fixture(cx);
    let path = root.path().join("iced/plan-limits.json");
    let pin = RoutingPin {
        provider: Provider::Codex,
        until: 2_000_000_000,
    };
    let limits = PlanLimitsFile {
        pin: Some(pin.clone()),
        ..Default::default()
    };
    let before = serde_json::to_vec(&limits).unwrap();
    std::fs::write(&path, &before).unwrap();
    dispatch(app_menus::UsageViewToggle, cx);
    let page = cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.usage_input.plan_limits.pin, Some(pin));
        shell.usage_page.clone()
    });
    for intent in [
        usage::UsageIntent::ChartTokens(true),
        usage::UsageIntent::ToggleHeadless,
        usage::UsageIntent::SelectRepo(Some((Some(root.path().join("work")), "work".into()))),
        usage::UsageIntent::PinChanged(None),
    ] {
        page.update(cx, |_, cx| cx.emit(intent));
        draw(cx);
    }
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.usage_input.chart_tokens);
        assert!(!shell.usage_input.show_headless);
        assert!(shell.usage_input.repo.is_some());
        assert!(
            shell
                .owner
                .read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("not available in preview")
        );
    });
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[gpui::test]
fn saved_run_inventory_and_workspace_switches_never_start_a_terminal(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture(cx);
    let cwd = root.path().join("work");
    let id = SessionId(42);
    let (owner, config) = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            // Model a saved run tab without ever creating a real PTY in the fixture.
            let workspace = shell.active(cx).workspace.id;
            let pane_id = session_pane_id(id);
            let pane = cx.new(|cx| {
                Pane::new(
                    pane_id.clone(),
                    "printf x >> h7-run.txt".into(),
                    PaneKind::StoppedTerminal(cwd.clone(), true),
                    workspace,
                    shell.data.clone(),
                    window,
                    cx,
                )
            });
            let state = shell
                .workspaces
                .iter_mut()
                .find(|w| w.workspace.id == workspace)
                .unwrap();
            state.sessions.push(Session {
                id,
                title: "printf x >> h7-run.txt".into(),
                kind: data::SessionKind::Terminal,
            });
            state
                .store
                .terminals
                .as_mut()
                .unwrap()
                .push(layout::TerminalSession {
                    id: id.0,
                    cwd: cwd.clone(),
                });
            state.panes.insert(pane_id, pane.clone());
            state.dock.update(cx, |dock, cx| {
                dock.add_panel_view(panel_handle(pane), DockPlacement::Bottom, None, window, cx)
            });
            state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
            (shell.owner.clone(), shell.config_dir.clone())
        })
    });
    let restored = cx.update(|window, cx| {
        cx.new(|cx| {
            Shell::with_owner(
                Rc::new(data::LocalData(owner.clone())),
                owner,
                &config,
                true,
                window,
                cx,
            )
            .unwrap()
        })
    });
    for workspace in [WorkspaceId(1), WorkspaceId(2), WorkspaceId(1)] {
        cx.update(|window, cx| {
            restored.update(cx, |shell, cx| {
                shell.select_workspace_id(workspace, window, cx);
                shell.refresh_bottom_panel(window, cx);
                if workspace == WorkspaceId(1) {
                    shell.bottom_outcome(
                        bottom_panel::Outcome::SelectTab { id: id.0 as usize },
                        window,
                        cx,
                    );
                }
                assert!(shell.run_tabs.is_empty());
                // There is no terminal backend to spawn a child or receive PTY writes.
                assert!(
                    shell
                        .workspaces
                        .iter()
                        .flat_map(|w| w.panes.values())
                        .all(|p| !matches!(p.read(cx).content, pane::PaneContent::Terminal(_)))
                );
                assert!(
                    matches!(&shell.workspaces[0].panes[&session_pane_id(id)].read(cx).content,
                pane::PaneContent::IcedSession(text) if text.contains("idle"))
                );
            })
        });
        draw(cx);
    }
    assert!(!cwd.join("h7-run.txt").exists());
}

#[gpui::test]
fn work_repo_row_run_launches_like_iced_without_a_notice(cx: &mut TestAppContext) {
    // Owner decision 2026-11-20: the sidebar ▶ runs, exactly like Iced's RunRepoCommand.
    let (root, shell, cx) = fixture(cx);
    let sidebar = cx.read(|cx| shell.read(cx).work_sidebars[&WorkspaceId(1)].clone());
    sidebar.update(cx, |_, cx| {
        cx.emit(work_sidebar::WorkSidebarOutcome::RunRepoCommand {
            repo: root.path().join("work"),
        })
    });
    wait_file(&root.path().join("work/h7-run.txt"), "x", cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.bottom.read(cx).input.tabs.len(), 1);
        assert_eq!(shell.owner.read(cx).error, None);
    });
    // Preview notices (from controls still unavailable) sit below the session strip.
    let owner = cx.read(|cx| shell.read(cx).owner.clone());
    owner.update(cx, |owner, cx| {
        owner.error = Some("Open in editor · not available in preview".into());
        cx.notify();
    });
    draw(cx);
    let notice = cx.debug_bounds("preview-notice").unwrap();
    let strip = cx.debug_bounds("pstrip-session-strip").unwrap();
    assert!(notice.origin.y > strip.bottom());
}

#[gpui::test]
fn title_tracks_workspace_and_sidebar_and_tabs_use_iced_label_alignment(cx: &mut TestAppContext) {
    let (_root, shell, cx) = fixture(cx);
    for (selector, mode) in [("Files", "Files"), ("Find", "Find"), ("Work", "Work")] {
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        draw(cx);
        assert_eq!(
            cx.window_title(),
            Some(format!("GitTerm V6 Development - H7 - {mode}"))
        );
    }
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.select_workspace_id(WorkspaceId(2), window, cx);
        })
    });
    draw(cx);
    assert_eq!(
        cx.window_title().as_deref(),
        Some("GitTerm V6 Development - second - Work")
    );
    assert!(cx.debug_bounds("Work").unwrap().origin.x < cx.debug_bounds("Files").unwrap().origin.x);
}
