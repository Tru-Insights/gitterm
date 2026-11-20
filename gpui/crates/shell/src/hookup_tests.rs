//! Shell hookups on GPUI's headless platform; all state is under temporary roots.
use super::*;
use gpui::{TestAppContext, VisualTestContext};

struct Fixture {
    _root: tempfile::TempDir,
    config: std::path::PathBuf,
    folders: Vec<std::path::PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let folders = ["one", "two"]
            .map(|name| {
                let path = root.path().join(name);
                std::fs::create_dir(&path).unwrap();
                path
            })
            .to_vec();
        Self {
            config: root.path().join("preview"),
            _root: root,
            folders,
        }
    }
    fn shell(&self, window: &mut Window, cx: &mut Context<Shell>) -> Shell {
        let mut model = WorkspaceStatus::default();
        for folder in &self.folders {
            model.add(folder.clone(), Instant::now());
        }
        let mut store = preview::tests::guarded_store(&self.config);
        store.load().unwrap();
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &self.config,
            true,
            window,
            cx,
        )
        .unwrap()
    }
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}
fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

#[gpui::test]
fn session_trigger_opens_picker_and_new_chat_is_native_not_terminal(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    draw(cx);
    assert!(cx.debug_bounds("work-sidebar").is_some());
    click(cx, "pstrip-new-session");
    cx.read(|cx| assert!(shell.read(cx).session_picker.is_some()));
    assert!(cx.debug_bounds("session-picker").is_some());
    // The plain fixture directory fails the existing independent-clone gate,
    // so no Claude process can launch while testing the real NewChat path.
    click(cx, "session-picker-row-0");
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.session_picker.is_none());
        let state = shell.active(cx);
        assert_eq!(state.sessions.len(), 1);
        let session = &state.sessions[0];
        assert_eq!(session.kind, data::SessionKind::Chat);
        assert!(matches!(
            state.panes[&session_pane_id(session.id)].read(cx).content,
            pane::PaneContent::PreviewChat(_)
        ));
        assert!(state.store.terminals.as_ref().unwrap().is_empty());
        assert_eq!(shell.owner.read(cx).chats.len(), 1);
    });
    click(cx, "pstrip-new-session");
    click(cx, "session-picker-row-2");
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        assert_eq!(state.sessions.len(), 2);
        let selected = state
            .sessions
            .iter()
            .find(|s| Some(s.id) == state.active_session)
            .unwrap();
        assert_eq!(selected.kind, data::SessionKind::Terminal);
        assert_eq!(
            state
                .sessions
                .iter()
                .filter(|s| s.kind == data::SessionKind::Chat)
                .count(),
            1
        );
    });
}

#[gpui::test]
fn header_and_shortcut_open_same_sheet_with_bottom_terminal_and_chat_facts(
    cx: &mut TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        install_app_menus(cx);
    });
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_terminal(window, cx);
            shell.new_terminal_in(DockPlacement::Bottom, window, cx);
        })
    });
    draw(cx);
    cx.read(|cx| {
        let sheet = shell.read(cx).workspace_sheet(cx).unwrap();
        assert_eq!(sheet.kind, park_sheet::Kind::Close);
        assert!(sheet.lines()[0].contains("2 terminals"));
    });
    click(cx, "pstrip-close-workspace");
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_some()));
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    cx.simulate_keystrokes("cmd-shift-w");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_some()));
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.select_picker_outcome(
                &session_picker::SessionPickerOutcome::NewChat,
                "New chat",
                window,
                cx,
            );
        })
    });
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).workspace_sheet(cx).unwrap().kind,
            park_sheet::Kind::Park
        )
    });
    click(cx, "pstrip-close-workspace");
    click(cx, "Park");
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.workspaces[0].parked.is_some());
        assert!(shell.park_sheet.is_none());
        assert_eq!(
            shell.active_workspace(cx),
            Some(shell.workspaces[1].workspace.id)
        );
        assert!(
            shell.workspace_sheet(cx).is_none(),
            "last visible workspace remains guarded"
        );
    });
}

#[gpui::test]
fn work_outcomes_select_existing_chat_and_other_controls_do_not_mutate(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_claude_chat(None, window, cx);
            let workspace = shell.active_workspace(cx).unwrap().0;
            let chat = shell.owner.read(cx).chats.keys().next().unwrap().clone();
            let session = shell.active(cx).active_session;
            shell.select_workspace(1, window, cx);
            shell.work_outcome(
                &work_sidebar::WorkSidebarOutcome::OpenChat {
                    workspace,
                    chat: chat.clone(),
                },
                window,
                cx,
            );
            assert_eq!(shell.active(cx).active_session, session);
            assert_eq!(shell.active_workspace(cx).unwrap().0, workspace);
            shell.select_workspace(1, window, cx);
            shell.work_outcome(
                &work_sidebar::WorkSidebarOutcome::Focus {
                    workspace,
                    focus: gitterm_core::work::summary::Focus::Coordinator(chat),
                },
                window,
                cx,
            );
            assert_eq!(shell.active_workspace(cx).unwrap().0, workspace);
            shell.work_outcome(&work_sidebar::WorkSidebarOutcome::AddRepo, window, cx);
            assert_eq!(shell.workspaces.len(), 2);
            assert_eq!(shell.active(cx).sessions.len(), 1);
        })
    });
}

#[test]
fn shell_renders_the_iced_components() {
    let source = include_str!("lib.rs");
    for renderer in [
        "session_strip::render(",
        "workspace_bar::render(",
        "spine::render(",
    ] {
        assert!(source.contains(renderer), "missing {renderer}");
    }
    assert!(!source.contains("TabBar::new(\"sessions\")"));
}

#[gpui::test]
fn changing_workspace_dismisses_picker_and_unavailable_outcomes_keep_the_iced_label(
    cx: &mut TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let fixture = Fixture::new();
    let (shell, cx) = cx.add_window_view(|window, cx| fixture.shell(window, cx));
    draw(cx);
    click(cx, "pstrip-new-session");
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.select_workspace(1, window, cx);
            assert!(shell.session_picker.is_none());
            assert!(!shell.content.read(cx).picker);
            shell.select_picker_outcome(
                &session_picker::SessionPickerOutcome::CliResume {
                    preset: 0,
                    command: "unused".into(),
                },
                "Claude (resume)",
                window,
                cx,
            );
            assert_eq!(
                shell.owner.read(cx).error.as_deref(),
                Some("Claude (resume) · unavailable in preview")
            );
            assert!(shell.active(cx).sessions.is_empty());
        })
    });
}
