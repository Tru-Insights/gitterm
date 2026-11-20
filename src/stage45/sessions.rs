use super::*;

#[test]
fn session_tab_bytes_keep_old_defaults_and_current_identities() {
    let bytes = fixture("sessions/tabs.saved.json");
    let tabs: Vec<WorkspaceTabConfig> = serde_json::from_str(bytes.trim()).unwrap();
    assert_eq!(serde_json::to_string(&tabs).unwrap(), bytes.trim());
    assert!(tabs[0].session_uid.is_none());
    assert_eq!(tabs[1].session_uid.as_deref(), Some("caller-1"));
    assert_eq!(tabs[1].task_session_id.as_deref(), Some("child-1"));
    assert_eq!(tabs[1].chat_session_id.as_deref(), Some("conversation-1"));
    for tab in &tabs {
        let before = serde_json::to_vec(tab).unwrap();
        let opened = tab.session_identity().open(|| "legacy-caller".into());
        assert_eq!(opened.needs_persist, tab.session_uid.is_none());
        assert_eq!(opened.identity.task_id, tab.task_id);
        assert_eq!(opened.identity.task_session_id, tab.task_session_id);
        assert_eq!(opened.identity.chat_session_id, tab.chat_session_id);
        assert_eq!(serde_json::to_vec(tab).unwrap(), before);
    }
    let bytes = fixture("sessions/workspaces.saved.json");
    let workspaces: Vec<WorkspaceConfig> = serde_json::from_str(bytes.trim()).unwrap();
    assert_eq!(serde_json::to_string(&workspaces).unwrap(), bytes.trim());
    assert!(workspaces[0].is_parked());
    assert!(!workspaces[1].is_parked());
}

#[test]
fn open_stop_close_and_native_restore_preserve_session_identity() {
    if isolated(
        "open_stop_close_and_native_restore_preserve_session_identity",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(dir.path());
    let first = app.workspaces[0].tabs[0].session_uid.clone();
    let second = app.workspaces[1].tabs[0].session_uid.clone();
    assert_ne!(first, second);
    assert!(uuid::Uuid::parse_str(&first).is_ok());
    let config = tab::AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: None,
        effort: None,
    };
    for ws in app.workspaces.iter_mut().take(2) {
        let mut session = AgentSession::new(config.clone());
        session.state = tab::AgentSessionState::Streaming;
        ws.tabs[0].kind = TabKind::Agent(session);
    }
    let _ = app.update_event(Event::AgentStopRequested(20));
    assert!(matches!(
        app.workspaces[0].tabs[0].agent_session().unwrap().state,
        tab::AgentSessionState::Streaming
    ));
    assert!(matches!(
        app.workspaces[1].tabs[0].agent_session().unwrap().state,
        tab::AgentSessionState::Stopped
    ));
    assert_eq!(app.workspaces[1].tabs[0].session_uid, second);
    let _ = app.update_event(Event::AgentStopRequested(usize::MAX));
    app.workspaces[0]
        .tabs
        .push(TabState::new(11, dir.path().into()));
    let _ = app.update_event(Event::TabClose(1));
    assert_eq!(app.workspaces[0].tabs.len(), 1);
    assert_eq!(app.workspaces[0].tabs[0].session_uid, first);
    let _ = app.update_event(Event::TabClose(0));
    assert_eq!(app.workspaces[0].tabs.len(), 1);

    let mut saved = app.workspace_config(&app.workspaces[0]);
    saved.tabs[0].chat_session_id = Some("conversation-1".into());
    let restored = app.restore_workspace_from_config(&saved, true);
    let tab = &restored.tabs[0];
    assert_eq!(tab.session_uid, first);
    assert_eq!(tab.chat_session_id.as_deref(), Some("conversation-1"));
    let session = tab.agent_session().unwrap();
    assert_eq!(session.session_id.as_deref(), Some("conversation-1"));
    assert!(session.harness.is_none());
    assert!(session.task_handle.is_none());
    assert!(session.conversation.is_empty());
}

#[test]
fn park_restore_native_session_never_replays_a_prompt() {
    if isolated(
        "park_restore_native_session_never_replays_a_prompt",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for mode in [ParkMode::Stop, ParkMode::KeepRunning] {
        let mut app = app_with_tabs(dir.path());
        let tab = &mut app.workspaces[0].tabs[0];
        tab.kind = TabKind::Agent(AgentSession::new(tab::AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        }));
        tab.chat_session_id = Some("conversation-1".into());
        tab.agent_session_mut().unwrap().session_id = Some("conversation-1".into());
        let caller = tab.session_uid.clone();
        let key = ParkedKey::of(&app.workspace_config(&app.workspaces[0]));
        let uid = app.workspaces[0].uid;
        let _ = app.park_workspace(uid, "  resume here  ", mode);
        let _ = app.resume_parked_workspace(&key);
        let tab = &app.active_workspace().unwrap().tabs[0];
        assert_eq!(tab.session_uid, caller);
        assert_eq!(tab.chat_session_id.as_deref(), Some("conversation-1"));
        assert_eq!(
            tab.agent_session().unwrap().session_id.as_deref(),
            Some("conversation-1")
        );
        assert!(tab.agent_session().unwrap().harness.is_none());
        assert!(tab.agent_session().unwrap().task_handle.is_none());
        assert!(tab.agent_session().unwrap().conversation.is_empty());
        if mode == ParkMode::KeepRunning {
            assert_eq!(app.active_workspace().unwrap().uid, uid);
            assert_eq!(tab.id, 10);
        }
    }
}

#[test]
fn close_confirmation_revalidates_the_exact_session_after_removal_or_move() {
    if isolated(
        "close_confirmation_revalidates_the_exact_session_after_removal_or_move",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for replacement in [
        "same-task-sibling",
        "restored-incarnation",
        "rebound-session",
        "moved-target",
    ] {
        let mut app = app_with_tabs(dir.path());
        let tab = &mut app.workspaces[0].tabs[0];
        tab.task_id = Some("task-1".into());
        tab.task_session_id = Some("child-1".into());
        tab.task_live_state = Some(TaskSessionLiveState::Working);
        let original = tab.session_target();
        let mut sibling = TabState::new(11, dir.path().into());
        sibling.task_id = Some("task-1".into());
        sibling.task_session_id = Some("child-2".into());
        app.workspaces[0].tabs.push(sibling);
        let _ = app.update_event(Event::TabClose(0));
        assert_eq!(app.task_tab_close_prompt.as_ref().unwrap().target, original);
        match replacement {
            "same-task-sibling" => {
                app.workspaces[0].tabs.remove(0);
            }
            "restored-incarnation" => {
                app.workspaces[0].tabs[0].id = 99;
            }
            "rebound-session" => {
                app.workspaces[0].tabs[0].task_session_id = Some("child-3".into());
            }
            _ => {
                let target = app.workspaces[0].tabs.remove(0);
                app.workspaces[1].tabs.push(target);
                app.activate_workspace_at(2);
            }
        }
        let before: Vec<_> = app
            .workspaces
            .iter()
            .flat_map(|ws| &ws.tabs)
            .map(TabState::session_target)
            .collect();
        let _ = app.update_event(Event::TaskTabStopAndClose);
        let after: Vec<_> = app
            .workspaces
            .iter()
            .flat_map(|ws| &ws.tabs)
            .map(TabState::session_target)
            .collect();
        if replacement == "moved-target" {
            assert_eq!(after.len() + 1, before.len());
            assert!(!after.contains(&original));
            assert_eq!(app.active_workspace().unwrap().name, "gamma");
        } else {
            assert_eq!(after, before, "{replacement}");
        }
    }
}

#[test]
fn terminal_restore_selects_caller_before_launch_and_never_injects_objective() {
    if isolated(
        "terminal_restore_selects_caller_before_launch_and_never_injects_objective",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(dir.path());
    let mut saved = app.workspace_config(&app.workspaces[0]);
    add_task(&mut app, dir.path(), dir.path());
    saved.dir = dir.path().display().to_string();
    saved.tabs[0].dir = saved.dir.clone();
    saved.tabs[0].repo_dir = Some(saved.dir.clone());
    saved.tabs[0].startup_command = Some("codex resume conversation-1".into());
    saved.tabs[0].chat_session_id = Some("conversation-1".into());
    saved.tabs[0].task_id = Some("fixture-task".into());
    saved.tabs[0].task_session_id = Some("child-1".into());
    for uid in [Some("persisted-caller".to_string()), None] {
        saved.tabs[0].session_uid = uid.clone();
        Trace::take();
        let restored = app.restore_workspace_from_config(&saved, true);
        let tab = &restored.tabs[0];
        let trace = Trace::take();
        let launch: Vec<_> = trace
            .iter()
            .filter(|s| s.starts_with("session-launch:"))
            .collect();
        assert_eq!(
            launch,
            vec![&format!(
                "session-launch:{}:codex resume conversation-1",
                tab.session_uid
            )]
        );
        if let Some(uid) = uid {
            assert_eq!(tab.session_uid, uid);
        } else {
            assert!(uuid::Uuid::parse_str(&tab.session_uid).is_ok());
            assert!(app.workspaces_dirty);
        }
        assert_eq!(tab.task_session_id.as_deref(), Some("child-1"));
        assert_eq!(tab.chat_session_id.as_deref(), Some("conversation-1"));
        assert_eq!(tab.startup_command(), Some("codex resume conversation-1"));
        assert!(tab.worker_process.is_none());
    }
}
