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
        session.reduce(gitterm_core::chat::state::Intent::SubmissionAccepted);
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
        tab.agent_session_mut()
            .unwrap()
            .bind_session(Some("conversation-1".into()));
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

// These bytes and ordered effects were captured before the park reducer moved.
fn park_app(dir: &Path) -> App {
    let mut app = app_with_tabs(dir);
    app.notifications_config.needs_you = false;
    for (idx, ws) in app.workspaces.iter_mut().enumerate() {
        let tab = &mut ws.tabs[0];
        tab.session_uid = format!("caller-{idx}");
        tab.chat_session_id = Some(format!("conversation-{idx}"));
        let mut session = AgentSession::new(tab::AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        });
        session.bind_session(tab.chat_session_id.clone());
        tab.kind = TabKind::Agent(session);
    }
    app
}

fn park_row(app: &App, command: &str) -> Value {
    json!({"command": command, "effects": Trace::take(),
        "active": app.active_workspace().map(|ws| ws.name.as_str()),
        "open": app.workspaces.iter().map(|ws| json!({"name": ws.name,
            "running_park": ws.running_park.is_some(), "woke": ws.woke.map(WakeReason::label),
            "callers": ws.tabs.iter().map(|tab| tab.session_uid.as_str()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "closed": app.closed_workspace_configs.iter().map(|cfg| json!({"name": cfg.name,
            "parked": cfg.is_parked(), "done": cfg.done_at.is_some(),
            "note": cfg.parked.as_ref().map(|p| p.note.as_str())})).collect::<Vec<_>>()})
}

fn stable_park_bytes(mut cfg: WorkspaceConfig) -> WorkspaceConfig {
    if let Some(info) = &mut cfg.parked {
        info.parked_at = "2000-01-01T00:00:00Z".into();
    }
    if cfg.done_at.is_some() {
        cfg.done_at = Some("2000-01-02T00:00:00Z".into());
    }
    cfg
}

#[test]
fn park_restore_done_close_characterization() {
    if isolated("park_restore_done_close_characterization", "sessions") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut rows = Vec::new();
    let mut bytes = Vec::new();
    for mode in [ParkMode::Stop, ParkMode::KeepRunning] {
        let mut app = park_app(dir.path());
        let uid = app.workspaces[0].uid;
        let key = ParkedKey::of(&app.workspace_config(&app.workspaces[0]));
        Trace::take();
        let _ = app.park_workspace(uid, "  synthetic note  ", mode);
        rows.push(park_row(&app, &format!("park {mode:?}")));
        let cfg = match app.workspace_index_by_uid(uid) {
            Some(i) => running_parked_config(
                app.workspace_config(&app.workspaces[i]),
                app.workspaces[i].running_park.as_ref().unwrap(),
            ),
            None => app.closed_workspace_configs[0].clone(),
        };
        bytes.push(stable_park_bytes(cfg));
        let _ = app.resume_parked_workspace(&key);
        rows.push(park_row(&app, &format!("resume {mode:?}")));
        bytes.push(stable_park_bytes(
            app.workspace_config(app.active_workspace().unwrap()),
        ));
        let _ = app.park_workspace(
            app.active_workspace().unwrap().uid,
            "done note",
            ParkMode::KeepRunning,
        );
        Trace::take();
        let _ = app.mark_parked_done(&key);
        rows.push(park_row(&app, &format!("done {mode:?}")));
        bytes.push(stable_park_bytes(
            app.closed_workspace_configs.last().unwrap().clone(),
        ));
    }
    let mut app = park_app(dir.path());
    let uid = app.workspaces[0].uid;
    let _ = app.park_workspace(uid, "wake", ParkMode::KeepRunning);
    Trace::take();
    let _ = app.wake_running_parked(uid, WakeReason::Finished);
    rows.push(park_row(&app, "wake without focus"));
    let _ = app.close_workspace(uid);
    rows.push(park_row(&app, "close"));
    let _ = app.close_workspace(uid);
    rows.push(park_row(&app, "stale close"));
    let _ = app.park_workspace(app.workspaces[0].uid, "", ParkMode::Stop);
    Trace::take();
    let last = app.workspaces[0].uid;
    let _ = app.close_workspace(last);
    rows.push(park_row(&app, "last bar close"));
    let _ = app.park_workspace(last, "", ParkMode::KeepRunning);
    rows.push(park_row(&app, "last bar park"));
    // Equal names/folders intentionally choose the first matching closed entry.
    let mut app = park_app(dir.path());
    let uid = app.workspaces[0].uid;
    let mut duplicate = app.workspace_config(&app.workspaces[0]);
    duplicate.active = false;
    app.closed_workspace_configs.push(duplicate);
    Trace::take();
    let _ = app.close_workspace(uid);
    rows.push(park_row(&app, "duplicate key close"));
    // Close re-runs Done after appending, including a different running duplicate.
    let mut app = park_app(dir.path());
    let uid = app.workspaces[0].uid;
    let key = ParkedKey::of(&app.workspace_config(&app.workspaces[0]));
    let mut old = app.workspace_config(&app.workspaces[0]);
    old.active = false;
    app.closed_workspace_configs.push(old);
    app.workspaces[1].name = key.name.clone();
    app.workspaces[1].dir = PathBuf::from(&key.dir);
    app.workspaces[1].running_park = Some(ParkedInfo {
        note: "running duplicate".into(),
        parked_at: "old".into(),
    });
    Trace::take();
    let _ = app.close_workspace(uid);
    rows.push(park_row(&app, "close with stopped and running duplicate"));
    assert_eq!(app.workspaces.len(), 1);
    assert_eq!(app.closed_workspace_configs.len(), 3);
    assert!(app.closed_workspace_configs[0].done_at.is_some());
    assert!(app.closed_workspace_configs[1..]
        .iter()
        .all(WorkspaceConfig::is_parked));
    // Cleanup sees the first closed key, even if Done will mark a later one.
    let mut app = park_app(dir.path());
    let mut old = app.workspace_config(&app.workspaces[0]);
    let key = ParkedKey::of(&old);
    old.active = false;
    old.done_at = Some("old".into());
    old.chats = vec!["old-snapshot".into()];
    old.tabs.clear();
    let mut newer = old.clone();
    newer.done_at = None;
    newer.chats = vec!["new-snapshot".into()];
    app.closed_workspace_configs = vec![old, newer];
    let cleanup = parked_snapshot_chats(&app.workspaces, &app.closed_workspace_configs, &key);
    assert_eq!(cleanup, vec!["old-snapshot".to_string()]);
    Trace::take();
    let _ = app.mark_parked_done(&key);
    rows.push(park_row(&app, "done with older done duplicate"));
    assert!(app
        .closed_workspace_configs
        .iter()
        .all(|cfg| cfg.done_at.is_some()));
    golden("sessions/park.trace.json", json!(rows));
    assert_eq!(
        serde_json::to_string(&bytes).unwrap(),
        fixture("sessions/park.saved.json").trim()
    );
}

#[test]
fn parked_restore_dedup_is_source_scoped_and_maps_the_active_tab() {
    if isolated(
        "parked_restore_dedup_is_source_scoped_and_maps_the_active_tab",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = park_app(dir.path());
    let mut cfg = app.workspace_config(&app.workspaces[0]);
    let mut second = cfg.tabs[0].clone();
    second.chat_session_id = Some("unopened".into());
    second.session_uid = Some("kept-caller".into());
    cfg.tabs.push(second);
    for remote in [None, Some("fixture-remote")] {
        cfg.location = remote.map(|id| config::WorkspaceLocationConfig::RemoteAgent {
            remote_id: id.into(),
            workspace_id: "fixture-workspace".into(),
            root: "opaque/root".into(),
        });
        for (active, expected) in [
            (Some(0), None),
            (Some(1), Some(0)),
            (Some(99), None),
            (None, None),
        ] {
            let mut filtered = cfg.clone();
            filtered.active_tab = active;
            app.drop_tabs_open_elsewhere(&mut filtered);
            if remote.is_none() {
                assert_eq!(filtered.tabs.len(), 1);
                assert_eq!(filtered.tabs[0].session_uid.as_deref(), Some("kept-caller"));
                assert_eq!(filtered.active_tab, expected);
            } else {
                assert_eq!(filtered.tabs.len(), 2);
                assert_eq!(filtered.active_tab, active.filter(|i| *i < 2));
            }
        }
    }
    app.workspaces[1].location = WorkspaceLocation::RemoteAgent {
        remote_id: "fixture-remote".into(),
        workspace_id: "fixture-workspace".into(),
        root: "opaque/root".into(),
    };
    app.workspaces[1].tabs[0].chat_session_id = Some("unopened".into());
    // Dedup includes hidden running tabs, but never dedups within the saved entry.
    app.workspaces[1].running_park = Some(ParkedInfo {
        note: String::new(),
        parked_at: "old".into(),
    });
    cfg.tabs.push(cfg.tabs[0].clone());
    cfg.active_tab = Some(0);
    app.drop_tabs_open_elsewhere(&mut cfg);
    assert_eq!(cfg.tabs.len(), 2);
    assert_eq!(cfg.tabs[0].session_uid.as_deref(), Some("caller-0"));
    assert_eq!(cfg.active_tab, Some(0));
}

#[test]
fn stopped_task_resume_launches_only_the_recorded_command() {
    if isolated(
        "stopped_task_resume_launches_only_the_recorded_command",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let mut saved = app.workspace_config(&app.workspaces[0]);
    saved.active = false;
    saved.dir = dir.path().display().to_string();
    saved.tabs[0].dir = saved.dir.clone();
    saved.tabs[0].repo_dir = Some(saved.dir.clone());
    saved.tabs[0].session_uid = Some("persisted-caller".into());
    saved.tabs[0].startup_command = Some("codex resume conversation-1".into());
    saved.tabs[0].chat_session_id = Some("conversation-1".into());
    saved.tabs[0].task_id = Some("fixture-task".into());
    saved.tabs[0].task_session_id = Some("child-1".into());
    let key = ParkedKey::of(&saved);
    app.closed_workspace_configs.push(saved);
    Trace::take();
    let _ = app.resume_parked_workspace(&key);
    let effects = Trace::take();
    let launch: Vec<_> = effects
        .iter()
        .filter(|s| s.starts_with("session-launch:"))
        .collect();
    assert_eq!(
        launch,
        vec!["session-launch:persisted-caller:codex resume conversation-1"]
    );
    let tab = &app.active_workspace().unwrap().tabs[0];
    assert_eq!(tab.session_uid, "persisted-caller");
    assert_eq!(tab.task_session_id.as_deref(), Some("child-1"));
    assert_eq!(tab.startup_command(), Some("codex resume conversation-1"));
    assert!(tab.worker_process.is_none());
}

#[test]
fn chat_receiver_survives_task_attachment_and_existing_tab_resume() {
    if isolated(
        "chat_receiver_survives_task_attachment_and_existing_tab_resume",
        "sessions",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let tab = &mut app.workspaces[0].tabs[0];
    tab.repo_path = dir.path().into();
    tab.set_local_dir(dir.path().into());
    tab.chat_session_id = Some("synthetic-pi".into());
    let mut chat = AgentSession::new(tab::AgentBackendConfig::Pi {
        model: "synthetic-model".into(),
        session_path: None,
        thinking: None,
    });
    chat.bind_session(Some("synthetic-pi".into()));
    chat.reduce(gitterm_core::chat::state::Intent::SubmissionAccepted);
    let receiver = chat.runtime_stamp(tab.session_target());
    tab.kind = TabKind::Agent(chat);

    app.anchor_open_sessions_to_task("fixture-task", dir.path());
    assert_eq!(
        app.workspaces[0].tabs[0].task_id.as_deref(),
        Some("fixture-task")
    );
    let attached = app.workspaces[0].tabs[0].session_target();
    let _ = app.update_event(Event::AgentRuntimeEvent(receiver.clone(), tab::AgentEvent::Other(json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"after attach"}}))));
    assert_eq!(
        app.workspaces[0].tabs[0].agent_session().unwrap().revision,
        1
    );
    // Deferred commands retain the original task binding even though the
    // receiver remains current and follows the chat into its task.
    let _ = app.update_event(Event::AgentRuntimeStop(receiver.clone()));
    assert!(matches!(
        app.workspaces[0].tabs[0].agent_session().unwrap().state,
        tab::AgentSessionState::Streaming
    ));

    let stamp_before_resume = app.workspaces[0].tabs[0]
        .agent_session()
        .unwrap()
        .runtime_stamp(attached);
    let store = app.task_host.store_mut().as_mut().unwrap();
    let mut saved = store
        .get("fixture-task")
        .unwrap()
        .sessions
        .last()
        .unwrap()
        .clone();
    saved.task_session_id = "resumed-child".into();
    saved.conversation = Some(HarnessConversationRef {
        backend: HarnessConversationBackend::Pi,
        session_id: "synthetic-pi".into(),
    });
    store
        .upsert_session("fixture-task", saved, "2000-01-01T00:05:00Z")
        .unwrap();
    let _ = app.resume_task_conversation("fixture-task", "synthetic-pi");
    assert!(app.task_ui_error.is_none(), "{:?}", app.task_ui_error);
    assert_eq!(
        app.workspaces[0].tabs[0].task_session_id.as_deref(),
        Some("resumed-child")
    );
    assert_eq!(app.workspaces[0].tabs.len(), 1);
    let _ = app.update_event(Event::AgentRuntimeStop(stamp_before_resume));
    assert!(matches!(
        app.workspaces[0].tabs[0].agent_session().unwrap().state,
        tab::AgentSessionState::Streaming
    ));
    let _ = app.update_event(Event::AgentRuntimeEvent(receiver.clone(), tab::AgentEvent::Other(json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"after resume"}}))));
    assert_eq!(
        app.workspaces[0].tabs[0].agent_session().unwrap().revision,
        2
    );
    let _ = app.update_event(Event::AgentRuntimeEvent(
        receiver,
        tab::AgentEvent::Other(json!({"type":"done"})),
    ));
    let chat = app.workspaces[0].tabs[0].agent_session().unwrap();
    assert_eq!(chat.revision, 3);
    assert!(matches!(chat.state, tab::AgentSessionState::Idle));
}
