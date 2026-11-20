//! TRU-284: pre-extraction Iced traces. No GUI, startup, jobs or real user state.
use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default)]
struct Harness {
    clock: Option<Instant>,
    effects: Vec<String>,
}
thread_local! { static HARNESS: RefCell<Option<Harness>> = const { RefCell::new(None) }; }
pub(super) fn now() -> Instant {
    HARNESS
        .with(|h| h.borrow().as_ref().and_then(|h| h.clock))
        .unwrap_or_else(Instant::now)
}
pub(super) fn observe(effect: &str) {
    HARNESS.with(|h| {
        if let Some(h) = h.borrow_mut().as_mut() {
            h.effects.push(effect.into());
        }
    });
}
pub(super) fn request(effect: String) -> bool {
    HARNESS.with(|h| {
        if let Some(h) = h.borrow_mut().as_mut() {
            h.effects.push(effect);
            true
        } else {
            false
        }
    })
}
struct Trace;
impl Trace {
    fn start() -> Self {
        HARNESS.with(|h| {
            *h.borrow_mut() = Some(Harness {
                clock: Some(Instant::now()),
                effects: Vec::new(),
            })
        });
        Self
    }
    fn advance(ms: u64) {
        HARNESS.with(|h| {
            *h.borrow_mut().as_mut().unwrap().clock.as_mut().unwrap() += Duration::from_millis(ms)
        });
    }
    fn take() -> Vec<String> {
        HARNESS.with(|h| std::mem::take(&mut h.borrow_mut().as_mut().unwrap().effects))
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        HARNESS.with(|h| *h.borrow_mut() = None);
    }
}

// Uses the production initializer's values, but skips restore, MCP registration,
// config discovery and process spawning. State machines below are the real App.
fn headless(fixture: &Path) -> App {
    let config = Config::default();
    App {
        status_jobs: StatusSubscriptionData::new(),
        title: "Synthetic baseline".into(),
        workspaces: Vec::new(),
        repos: RepoRegistry::default(),
        stored_repos: Vec::new(),
        bottom_repo_picker_open: false,
        selection: SelectionState::default(),
        next_tab_id: 0,
        task_authority: TaskAuthority::default(),
        missing_task_worktrees_logged: HashSet::new(),
        review_config: config.review.clone(),
        model_policy: config.policy.clone(),
        delegation_runs: HashMap::new(),
        delegation_state: delegation_state::State::default(),
        worker_pr_scans: gitterm::worker_prs::Scans::default(),
        delegation_waiting_counts: HashMap::new(),
        chat_config: config.chat.clone(),
        task_store_error: None,
        task_ui_error: None,
        task_launch_cost_note: None,
        task_worktree_root: config.task_worktree_root.clone(),
        max_concurrent_local_tasks: config.max_concurrent_local_tasks,
        fd_watch: gitterm::resource_watch::FdWatch::default(),
        selected_task_id: None,
        task_rail_filter: TaskRailFilter::All,
        task_rows_seen: HashSet::new(),
        task_switcher_open: false,
        task_switcher_query: String::new(),
        task_switcher_selection: None,
        task_context_mru: VecDeque::from([None]),
        task_last_session_tab: HashMap::new(),
        task_progress_pending: HashMap::new(),
        task_launch_queue: Vec::new(),
        task_delivery_probe: HashMap::new(),
        task_publish_busy: HashSet::new(),
        task_publish_prompt: None,
        new_task_form: None,
        task_tab_close_prompt: None,
        task_worktree_delete_prompt: None,
        rail_worktrees: Vec::new(),
        rail_worktrees_repo_path: None,
        rail_worktrees_loading: false,
        worktree_list_cache: WorktreeListCache::default(),
        last_rail_worktrees_poll: Instant::now(),
        unmanaged_worktree_ops: HashMap::new(),
        theme: AppTheme::Dark,
        terminal_font_size: config.terminal_font_size,
        ui_font_size: config.ui_font_size,
        chat_font_size: config.chat_font_size,
        chat_width: config.chat_width,
        sidebar_width: config.sidebar_width.clamp(150.0, 600.0),
        scrollback_lines: config.scrollback_lines,
        sidebar_collapsed: false,
        task_rail_pinned: false,
        dragging_divider: false,
        show_hidden: config.show_hidden,
        window_size: (1400.0, 800.0), // Initial size, updated on resize
        log_server_state: log_server::ServerState::new(),
        terminal_log_mirroring_enabled: false,
        browser_mcp: None,
        task_mcp: None,
        browser_status: None,
        browser_action: None,
        browser_status_loading: false,
        browser_error: None,
        browser_error_sticky: false,
        browser_telemetry: BrowserTelemetrySnapshot::default(),
        browser_owner_labels: HashMap::new(),
        browser_evidence_open: false,
        browser_evidence_handles: HashMap::new(),
        console_expanded: config.console_expanded,
        console_height: config.console_height.clamp(32.0, 600.0),
        dragging_console_divider: false,
        editing_run_command: None,
        slide_offset: 0.0,
        slide_target: 0.0,
        slide_animating: false,
        slide_start_time: None,
        slide_start_offset: 0.0,
        slide_from: None,
        last_user_scroll: None,
        edge_peek_left: false,
        edge_peek_right: false,
        hovered_horizontal_strips: Vec::new(),
        attention_pulse_bright: false,
        needs_you_notifier: NeedsYouNotifier::new(Instant::now()),
        notifications_config: config.notifications.clone(),
        pr_watch_config: config.pr_watch.clone(),
        delegations_config: config.delegations.clone(),
        work_notes_config: config.work_notes.clone(),
        editor: config.editor.clone(),
        editor_error: None,
        work_notes: gitterm::work_notes::WorkNotes::new(fixture.join("notes"), false),
        current_modifiers: Modifiers::empty(),
        show_help: false,
        workspace_settings_open: false,
        workspace_settings_idx: 0,
        workspace_settings_new_key: String::new(),
        workspace_settings_new_value: String::new(),
        guidelines_settings: gitterm::guidelines::GuidelinesSettings::default(),
        guidelines_repo: None,
        tab_picker_visible: false,
        tab_picker_cli_open: false,
        task_handoff_copied: false,
        folder_picker: None,
        folder_picker_last_dirs: HashMap::new(),
        remote_connect_form: None,
        phone_panel: None,
        agent_presets: config.agent_presets.clone(),
        quick_commands: config.quick_commands.clone(),
        quick_commands_visible: false,
        remote_agent_defs: Vec::new(),
        remote_agent_connections: HashMap::new(),
        remote_session_defs: Vec::new(),
        remote_sessions: Vec::new(),
        remote_sessions_loading: false,
        remote_sessions_error: None,
        window_focused: true,
        terminal_redraws: TerminalRedrawQueue::default(),
        bottom_panel_focused: false,
        closed_workspace_configs: Vec::new(),
        workspaces_dirty: false,
        next_workspace_save_at: None,
        terminal_mirror_dirty: false,
        next_terminal_mirror_sync_at: Instant::now(),
        next_perf_report_at: Instant::now() + Duration::from_millis(PERF_REPORT_INTERVAL_MS),
        terminal_mirror_sync_in_flight: false,
        terminal_mirror_sync_queued: false,
        last_terminal_mirror_snapshot_hash: None,
        // Speech-to-text
        #[cfg(feature = "stt")]
        stt_enabled: config.stt_enabled,
        #[cfg(feature = "stt")]
        stt_recording: false,
        #[cfg(feature = "stt")]
        stt_context: None,
        #[cfg(feature = "stt")]
        stt_audio_buffer: Arc::new(Mutex::new(Vec::new())),
        #[cfg(feature = "stt")]
        stt_stream: None,
        #[cfg(feature = "stt")]
        stt_sample_rate: 48000,
        #[cfg(feature = "stt")]
        stt_transcribing: false,
        dictation_held: HashMap::new(),
        viewer_webview: ViewerWebview::None,
        viewer_page_url: None,
        viewer_page_generation: 0,
        viewer_page_store: None,
        viewer_page_navigation_context: None,
        agent_pages: Vec::new(),
        chat_index: Arc::default(),
        workspace_overviews: HashMap::new(),
        workspace_overview_summary_generation: 0,
        workspace_overview_presentations: HashMap::new(),
        work_documents: HashMap::new(),
        work_document_copies_open: None,
        work_documents_all: false,
        overview_focus_picks: overview::FocusPicks::load(&overview::FocusPicks::path_in(fixture)),
        workspace_rename: None,
        chat_add_to_workspace_open: false,
        new_work_sheet: None,
        park_sheet: None,
        parked_list_open: false,
        prs: work_prs::PrsState::load(fixture),
        issues: work_issues::IssuesState::load(fixture),
        plan_limits: plan_limits_view::PlanLimitsState::load(fixture),
        github_accounts: BTreeMap::new(),
        turn_snapshot_sweep_at: None,
        review: gitterm_core::review::state::State::default(),
        chat_index_loading: false,
        chat_index_loaded_at: None,
        v5_offer_awaiting_titles: HashSet::new(),
        v5_offer_unavailable: None,
        chat_query: String::new(),
        chat_scope: chats::ChatScope::default(),
        chat_backend_filter: None,
        chat_expanded_machines: std::collections::HashSet::new(),
        chat_expanded_roots: std::collections::HashSet::new(),
        chat_backfill_running: false,
        chat_backfill_note: None,
        chat_open_section_collapsed: false,
        collapsed_repo_groups: std::collections::HashSet::new(),
        bar_menu: None,
        bar_hint: None,
        bar_hint_seq: 0,
        bar_hint_in: None,
        panel_repo: None,
        remote_chat_indexes: HashMap::new(),
        remote_running_chats: HashMap::new(),
        remote_running_chats_revision: 0,
        overview_running_polled: HashMap::new(),
        overview_transcripts: HashMap::new(),
        overview_transcripts_revision: 0,
        codex_exec_watch: Arc::default(),
        codex_exec_polling: false,
        codex_exec_has_runs: false,
        chat_overview_cache: ChatOverviewCache::default(),
        chat_overview_idle_expanded: false,
        roster_reveal_held: HashMap::new(),
        chat_preview: None,
        chat_preview_digest: None,
        chat_digest_cache: ChatDigestCache::default(),
        chat_preview_raw_expanded: false,
        chat_summary_ask_expanded: false,
        chat_summary_reply_expanded: false,
        jev_client: None,
        chat_rank: chat_rank::ChatRankState::default(),
        chat_snippets: Arc::new(Mutex::new(chat_rank::SnippetCache::default())),
        usage_config: config.usage.clone(),
        settings_refused: None,
        usage_view: UsageViewState::default(),
        usage_window: gitterm::usage::UsageWindow::default(),
        usage_chart_tokens: false,
        usage_show_headless: true,
        usage_report: None,
        usage_scanning: false,
        usage_error: None,
        usage_cache: Arc::new(Mutex::new(gitterm::usage::UsageCache::default())),
    }
}

fn isolated(name: &str) -> bool {
    if std::env::var("GITTERM_V6_BASELINE_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let config = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("baseline_tests::{name}"), "--nocapture"])
        .env(config::CONFIG_DIR_ENV, config.path())
        .env("GITTERM_V6_BASELINE_CHILD", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
fn workspace(name: &str, id: usize) -> Workspace {
    let root = PathBuf::from(format!("/fixture/{name}"));
    let mut ws = Workspace::new(name.into(), root.clone(), WorkspaceColor::Blue);
    let mut tab = TabState::new(id, root);
    tab.status.last_poll = now();
    ws.tabs.push(tab);
    ws
}
pub(super) fn app_with_tabs(fixture: &Path) -> App {
    let mut app = headless(fixture);
    app.workspaces = vec![
        workspace("alpha", 10),
        workspace("beta", 20),
        workspace("gamma", 30),
    ];
    app.activate_workspace_at(0);
    app
}
fn snapshot(id: usize, root: &str, branch: &str, dirty: bool, repo: bool) -> GitStatusSnapshot {
    GitStatusSnapshot {
        tab_id: id,
        repo_path: root.into(),
        repo_name: "synthetic".into(),
        branch_name: branch.into(),
        is_git_repo: repo,
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: if dirty {
            vec![FileEntry {
                path: "new.txt".into(),
                status: "?".into(),
                is_staged: false,
            }]
        } else {
            Vec::new()
        },
        error: None,
    }
}
fn apply(app: &mut App, snapshot: GitStatusSnapshot) {
    let _ = app.update_event(Event::GitStatusLoaded(snapshot));
}
fn requests() -> Vec<String> {
    Trace::take()
        .into_iter()
        .filter(|s| s.starts_with("local:") || s.starts_with("source:"))
        .collect()
}

#[test]
fn workspace_selection_traces() {
    if isolated("workspace_selection_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for (name, index, usage, selected, expected) in [
        (
            "explicit",
            1,
            false,
            1,
            vec![
                "terminals",
                "dirty",
                "plans+docs",
                "worktrees",
                "files",
                "connect",
                "present",
                "surfaces",
            ],
        ),
        (
            "bar",
            2,
            false,
            2,
            vec![
                "terminals",
                "dirty",
                "plans+docs",
                "worktrees",
                "files",
                "connect",
                "present",
                "surfaces",
            ],
        ),
        ("same+usage", 0, true, 0, vec!["surfaces"]),
        ("invalid+usage", 99, true, 0, vec!["surfaces"]),
        ("invalid", 99, false, 0, vec![]),
    ] {
        let mut app = app_with_tabs(fixture.path());
        app.usage_view.open = usage;
        app.editing_run_command = Some((RepoId(1), "editing".into()));
        app.selected_task_id = Some("missing-task".into());
        Trace::take();
        let _ = app.update_event(Event::WorkspaceSelect(index));
        assert_eq!(app.active_workspace_index(), selected, "{name}");
        assert!(!app.usage_view.open, "{name}");
        assert_eq!(app.editing_run_command, None, "{name}");
        assert_eq!(app.selected_task_id.is_none(), selected != 0, "{name}");
        assert_eq!(Trace::take(), expected, "{name}");
        assert!(
            !app.active_tab().unwrap().status.git_status_loading,
            "selection has no immediate status: {name}"
        );
        if selected != 0 {
            assert_eq!(
                *app.log_server_state.plans_dir.read().unwrap(),
                Some(app.workspaces[selected].dir.join(".plans"))
            );
            assert_eq!(
                *app.log_server_state.docs_dir.read().unwrap(),
                Some(app.workspaces[selected].dir.join("docs"))
            );
        }
    }
    // The real keyboard mapper returns WorkspaceSelect; execute only its output.
    let mut app = app_with_tabs(fixture.path());
    let task = app.update_event(Event::KeyPressed(
        Key::Character("2".into()),
        Modifiers::CTRL,
    ));
    drop(task);
    assert_eq!(Trace::take(), ["select-command:1"]);
    let _ = app.update_event(Event::WorkspaceSelect(1));
    assert_eq!(app.active_workspace_index(), 1);
    assert_eq!(requests(), Vec::<String>::new());
}

#[test]
fn swipe_park_reorder_and_closed_id_traces() {
    if isolated("swipe_park_reorder_and_closed_id_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(fixture.path());
    app.usage_view.open = true;
    app.selected_task_id = Some("selected-task".into());
    app.viewer_webview = ViewerWebview::PlansViewer;
    let width = app.content_viewport_width();
    let _ = app.update_event(Event::SlideScrolled(width));
    assert_eq!(app.active_workspace_index(), 1);
    // characterized: swipe leaves Usage and selected task open while explicit selection closes/clears them.
    assert!(app.usage_view.open);
    assert_eq!(app.selected_task_id.as_deref(), Some("selected-task"));
    assert_eq!(app.viewer_webview, ViewerWebview::None);
    assert_eq!(
        Trace::take(),
        [
            "terminals",
            "dirty",
            "dismiss",
            "plans+docs",
            "worktrees",
            "files",
            "connect"
        ]
    );
    app.usage_view.open = false;
    let active_uid = app.workspaces[1].uid;
    let _ = app.park_keep_running(
        1,
        ParkedInfo {
            note: "synthetic".into(),
            parked_at: "2000-01-01T00:00:00Z".into(),
        },
    );
    assert_eq!(app.active_workspace_index(), 2);
    assert_eq!(app.visible_workspace_order(), [0, 2]);
    assert_eq!(app.workspace_index_by_uid(active_uid), Some(1));
    let _ = app.update_event(Event::WorkspaceSelect(1));
    assert!(app.settle_active_workspace().is_some());
    assert!(app.workspaces[1].running_park.is_none());
    // Reordering machine groups preserves the selected identity.
    app.workspaces[0].location = WorkspaceLocation::LegacyRemoteSession {
        session_name: "fixture-ssh".into(),
        root: "/remote/fixture".into(),
    };
    app.normalize_workspace_order();
    assert_eq!(app.active_workspace().unwrap().uid, active_uid);
    assert_eq!(app.active_workspace_index(), 0);
    let gone = app.workspaces[2].uid;
    app.workspaces.remove(2);
    assert_eq!(app.workspace_index_by_uid(gone), None);
    let before = app.active_workspace().unwrap().uid;
    let _ = app.park_workspace(gone, "", ParkMode::Stop);
    assert_eq!(app.active_workspace().unwrap().uid, before);
}

#[test]
fn status_backoff_completion_and_error_traces() {
    if isolated("status_backoff_completion_and_error_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for (label, repo, dirty, intervals) in [
        (
            "dirty",
            true,
            true,
            vec![
                5000, 5000, 5000, 10000, 10000, 10000, 10000, 10000, 10000, 15000,
            ],
        ),
        (
            "clean",
            true,
            false,
            vec![
                5000, 5000, 10000, 10000, 10000, 15000, 15000, 15000, 15000, 30000,
            ],
        ),
        ("non-repo", false, false, vec![20000; 10]),
    ] {
        let mut app = app_with_tabs(fixture.path());
        for (streak, interval) in intervals.into_iter().enumerate() {
            Trace::advance(1000);
            apply(
                &mut app,
                snapshot(10, "/fixture/alpha", "branch", dirty, repo),
            );
            let tab = app.active_tab().unwrap();
            assert_eq!(
                (
                    tab.status.git_unchanged_streak,
                    tab.status.git_poll_interval_ms,
                    tab.status.last_poll,
                    tab.status.git_status_loading
                ),
                (streak as u32, interval, now(), false),
                "{label}:{streak}"
            );
        }
        let before = app.active_tab().unwrap().status.last_poll;
        let hash = app.active_tab().unwrap().status.last_git_status_hash;
        Trace::advance(999);
        let mut failed = snapshot(10, "/fixture/alpha", "bad", false, false);
        failed.error = Some("remote status failed: synthetic disconnect".into());
        apply(&mut app, failed);
        let tab = app.active_tab().unwrap();
        assert_eq!(
            (
                tab.status.last_poll,
                tab.status.last_git_status_hash,
                tab.status.branch_name.as_str(),
                tab.status.git_status_error.as_deref()
            ),
            (
                before,
                hash,
                "branch",
                Some("remote status failed: synthetic disconnect")
            )
        );
        // characterized: repo name is excluded from the hash, so changing it keeps the unchanged streak.
        let streak = tab.status.git_unchanged_streak;
        let mut renamed = snapshot(10, "/fixture/alpha", "branch", dirty, repo);
        renamed.repo_name = "renamed".into();
        apply(&mut app, renamed);
        assert_eq!(
            app.active_tab().unwrap().status.git_unchanged_streak,
            streak + 1
        );
    }
    for order in [["old", "new"], ["new", "old"]] {
        let mut app = app_with_tabs(fixture.path());
        let _ = app.refresh_panels_for_context(None);
        let _ = app.refresh_panels_for_context(None);
        assert_eq!(
            requests(),
            ["local:10:/fixture/alpha", "local:10:/fixture/alpha"]
        );
        app.activate_workspace_at(1); // completion is tab addressed, even when inactive.
        for branch in order {
            apply(
                &mut app,
                snapshot(10, "/fixture/alpha", branch, false, true),
            );
            let tab = &app.workspaces[0].tabs[0];
            // characterized: any completion clears loading; there is no latest-request-wins guard.
            assert!(!tab.status.git_status_loading);
            assert_eq!(tab.status.branch_name, branch);
        }
        assert_eq!(app.workspaces[0].tabs[0].status.branch_name, order[1]);
        app.workspaces[0].tabs.clear();
        apply(
            &mut app,
            snapshot(10, "/fixture/alpha", "late", false, true),
        );
        assert!(app.workspaces[0].tabs.is_empty());
    }
}

#[test]
fn status_poll_visibility_and_reset_traces() {
    if isolated("status_poll_visibility_and_reset_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for (mode, selected, elapsed, loading, expected) in [
        (SidebarMode::Git, false, 4999, false, false),
        (SidebarMode::Git, false, 5000, false, true),
        (SidebarMode::Workspace, false, 5000, false, false),
        (SidebarMode::Workspace, false, 15000, false, true),
        (SidebarMode::Workspace, true, 5000, false, true),
        (SidebarMode::Git, false, 30000, true, false),
    ] {
        let mut app = app_with_tabs(fixture.path());
        let tab = app.active_tab_mut().unwrap();
        tab.sidebar_mode = mode;
        tab.selected_file = selected.then(|| "diff.txt".into());
        tab.status.git_status_loading = loading;
        Trace::advance(elapsed);
        Trace::take();
        let _ = app.update_event(Event::Tick);
        assert_eq!(
            requests(),
            if expected {
                vec![String::from("local:10:/fixture/alpha")]
            } else {
                vec![]
            },
            "{mode:?} selected={selected} elapsed={elapsed}"
        );
        assert_eq!(
            app.active_tab().unwrap().status.git_status_loading,
            loading || expected
        );
        assert!(!app.workspaces[1].tabs[0].status.git_status_loading);
    }
    let mut app = app_with_tabs(fixture.path());
    let tab = app.active_tab_mut().unwrap();
    tab.status.git_poll_interval_ms = 30000;
    tab.status.git_unchanged_streak = 9;
    tab.status.last_git_status_hash = Some(123);
    let before = tab.status.last_poll;
    let _ = app.update_event(Event::Terminal(
        10,
        iced_term::Event::BackendCall(10, iced_term::backend::Command::Write(b"x".to_vec())),
    ));
    let tab = app.active_tab().unwrap();
    assert_eq!(
        (
            tab.status.git_poll_interval_ms,
            tab.status.git_unchanged_streak,
            tab.status.last_git_status_hash,
            tab.status.last_poll
        ),
        (5000, 0, Some(123), before)
    );
    let _ = app.refresh_panels_for_context(None);
    let tab = app.active_tab().unwrap();
    assert_eq!(
        (
            tab.status.git_poll_interval_ms,
            tab.status.git_unchanged_streak,
            tab.status.last_git_status_hash,
            tab.status.last_poll,
            tab.status.git_status_loading
        ),
        (5000, 0, None, now(), true)
    );
    assert_eq!(requests(), ["local:10:/fixture/alpha"]);
}

fn add_task(app: &mut App, fixture: &Path, root: &Path) {
    use gitterm::tasks::{GitBase, RepositoryIdentity};
    let mut store = TaskStore::load(fixture.join("tasks.json")).unwrap();
    let mut task = TaskRecord::new_draft(
        NewTaskRecord {
            task_id: "fixture-task".into(),
            title: "Synthetic task".into(),
            objective: "Fixture".into(),
            workspace: TaskWorkspaceIdentity {
                name: "alpha".into(),
                location: WorkspaceLocationIdentity::Local {
                    directory: app.workspaces[0].dir.clone(),
                },
            },
            repository: RepositoryIdentity {
                common_dir: fixture.join("repo/.git"),
                remote_url: None,
            },
            issue: None,
            base: GitBase {
                reference: "main".into(),
                commit: "0000000000000000000000000000000000000000".into(),
            },
            branch: "fixture-task".into(),
            executor: ExecutorTarget::Local,
            harness: None,
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
        },
        "2000-01-01T00:00:00Z",
    );
    task.worktree.path = Some(root.to_path_buf());
    task.worktree.state = TaskWorktreeState::Ready;
    if store.get("fixture-task").is_some() {
        store.replace(task).unwrap();
    } else {
        store.insert(task).unwrap();
    }
    app.task_authority = Some(store).into();
    app.selected_task_id = Some("fixture-task".into());
}
fn follow(tab: &mut TabState, root: &Path) {
    tab.worktree_follow.apply(
        worktree_follow::FollowDecision::Follow(worktree_follow::ActiveWorktree {
            path: root.into(),
            branch: Some("fixture-follow".into()),
        }),
        Some("fixture-follow".into()),
    );
}
fn local_root(app: &App) -> PathBuf {
    match app.source_for_active_tab().unwrap() {
        WorkspaceSource::Local { root } => root,
        other => panic!("{other:?}"),
    }
}

#[test]
fn source_root_precedence_and_adoption_traces() {
    if isolated("source_root_precedence_and_adoption_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let task_root = fixture.path().join("task");
    let follow_root = fixture.path().join("followed");
    std::fs::create_dir(&task_root).unwrap();
    std::fs::create_dir(&follow_root).unwrap();
    for (task, picked, followed, expected) in [
        (true, true, true, task_root.clone()),
        (false, true, true, PathBuf::from("/fixture/picked")),
        (false, false, true, follow_root.clone()),
        (false, false, false, PathBuf::from("/fixture/alpha")),
    ] {
        let mut app = app_with_tabs(fixture.path());
        if task {
            add_task(&mut app, fixture.path(), &task_root);
        }
        if picked {
            app.panel_repo = Some((10, "/fixture/picked".into()));
        }
        if followed {
            follow(app.active_tab_mut().unwrap(), &follow_root);
        }
        assert_eq!(local_root(&app), expected);
        let _ = app.refresh_panels_for_task_context();
        assert!(requests().contains(&format!("local:10:{}", expected.display())));
    }
    let mut app = app_with_tabs(fixture.path());
    app.workspaces[0].tabs.clear();
    assert_eq!(local_root(&app), PathBuf::from("/fixture/alpha"));
    app.workspaces.clear();
    assert_eq!(
        app.source_for_active_tab().unwrap_err(),
        "No active workspace"
    );

    for guard in [
        "plain",
        "chat",
        "agent",
        "harness",
        "task-host",
        "task-session",
        "picked",
        "followed",
    ] {
        let mut app = app_with_tabs(fixture.path());
        let root = if guard == "task-host" || guard == "task-session" {
            task_root.clone()
        } else if guard == "followed" {
            follow_root.clone()
        } else {
            PathBuf::from("/fixture/discovered")
        };
        match guard {
            "chat" => app.active_tab_mut().unwrap().chat_session_id = Some("fixture-chat".into()),
            "agent" => {
                app.active_tab_mut().unwrap().kind = TabKind::Agent(AgentSession::new(
                    serde_json::from_value(
                        serde_json::json!({"backend":"claude","model":"fixture-model"}),
                    )
                    .unwrap(),
                ))
            }
            "harness" => {
                if let TabKind::Terminal(tt) = &mut app.active_tab_mut().unwrap().kind {
                    tt.startup_command = Some("codex".into());
                }
            }
            "task-host" | "task-session" => {
                add_task(&mut app, fixture.path(), &task_root);
                if guard == "task-session" {
                    app.active_tab_mut().unwrap().task_id = Some("fixture-task".into());
                }
            }
            "picked" => app.panel_repo = Some((10, root.clone())),
            "followed" => follow(app.active_tab_mut().unwrap(), &root),
            _ => {}
        }
        let launch = app.active_tab().unwrap().current_dir.clone();
        apply(
            &mut app,
            snapshot(10, root.to_str().unwrap(), "discovered", false, true),
        );
        let adopt = guard == "plain" || guard == "task-session";
        let tab = app.active_tab().unwrap();
        assert_eq!(
            tab.repo_path,
            if adopt {
                root
            } else {
                PathBuf::from("/fixture/alpha")
            },
            "{guard}"
        );
        assert_eq!(tab.current_dir, launch, "{guard}");
        assert_eq!(tab.status.branch_name, "discovered", "{guard}");
    }
}

#[test]
fn removed_followed_tree_and_source_failure_traces() {
    if isolated("removed_followed_tree_and_source_failure_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for active in [true, false] {
        let mut app = app_with_tabs(fixture.path());
        let missing = fixture.path().join("gone");
        follow(&mut app.workspaces[0].tabs[0], &missing);
        app.workspaces[0].tabs[0].status.last_git_status_hash = Some(123);
        app.workspaces[0].tabs[0].files.dir = SourcePath::Local(missing.join("sub"));
        if !active {
            app.activate_workspace_at(1);
        }
        Trace::take();
        apply(
            &mut app,
            snapshot(10, missing.to_str().unwrap(), "discard-this", false, false),
        );
        let tab = &app.workspaces[0].tabs[0];
        assert_eq!(tab.status.branch_name, "main");
        assert_eq!(tab.status.last_git_status_hash, None);
        assert_eq!(
            tab.worktree_follow.note.as_deref(),
            Some("worktree gone is gone; back to the workspace root")
        );
        assert_eq!(tab.status.git_status_loading, active);
        assert_eq!(
            requests(),
            if active {
                vec![String::from("local:10:/fixture/alpha")]
            } else {
                vec![]
            }
        );
        if active {
            assert_eq!(
                app.workspaces[0].tabs[0].files.dir,
                SourcePath::Local("/fixture/alpha".into())
            );
        }
    }
    for (kind, expected) in [
        (
            "legacy",
            "This workspace uses the legacy SSH remote prototype, which has no file browser",
        ),
        (
            "missing-agent",
            "Remote \"fixture-remote\" is not configured in remote-agents.json",
        ),
    ] {
        let mut app = app_with_tabs(fixture.path());
        app.workspaces[0].location = if kind == "legacy" {
            WorkspaceLocation::LegacyRemoteSession {
                session_name: "fixture-ssh".into(),
                root: "/opaque/repo".into(),
            }
        } else {
            WorkspaceLocation::RemoteAgent {
                remote_id: "fixture-remote".into(),
                workspace_id: "fixture-workspace".into(),
                root: "/opaque/repo".into(),
            }
        };
        assert_eq!(app.source_for_active_tab().unwrap_err(), expected);
        Trace::advance(30000);
        Trace::take();
        let _ = app.update_event(Event::Tick);
        assert!(requests().is_empty(), "{kind}");
        assert!(!app.active_tab().unwrap().status.git_status_loading);
    }
    let mut app = app_with_tabs(fixture.path());
    app.workspaces[0].location = WorkspaceLocation::RemoteAgent {
        remote_id: "fixture-remote".into(),
        workspace_id: "fixture-workspace".into(),
        root: "Q:/opaque/repo".into(),
    };
    app.remote_agent_defs.push(serde_json::from_value(serde_json::json!({"id":"fixture-remote","name":"Fixture","endpoint":"http://fixture.invalid:1","auth":{"type":"token","token_ref":"/fixture/no-token"}})).unwrap());
    let source = app.source_for_active_tab().unwrap();
    assert!(source.capabilities().git_status);
    assert!(matches!(&source,WorkspaceSource::RemoteAgent {root,..} if root=="Q:/opaque/repo"));
    Trace::take();
    let _ = App::execute_source_git_status(
        &app.status_jobs.runtime,
        source,
        10,
        "/fixture/fallback".into(),
    );
    assert_eq!(requests().len(), 1);
    apply(&mut app, snapshot(10, "/fixture/alpha", "good", true, true));
    let before = app.active_tab().unwrap().status.last_poll;
    let mut error = snapshot(10, "/fixture/fallback", "", false, false);
    error.error = Some("synthetic remote transport failure".into());
    apply(&mut app, error);
    let tab = app.active_tab().unwrap();
    assert_eq!(
        (
            tab.status.branch_name.as_str(),
            tab.status.last_poll,
            tab.status.git_status_error.as_deref()
        ),
        ("good", before, Some("synthetic remote transport failure"))
    );
}

#[test]
fn persistence_load_action_save_fixtures() {
    if isolated("persistence_load_action_save_fixtures") {
        return;
    }
    for (label, input, expected) in [
        (
            "old",
            include_str!("../tests/fixtures/core/persistence/old.workspaces.json"),
            include_str!("../tests/fixtures/core/persistence/old.saved.json"),
        ),
        (
            "new",
            include_str!("../tests/fixtures/core/persistence/new.workspaces.json"),
            include_str!("../tests/fixtures/core/persistence/new.saved.json"),
        ),
    ] {
        let path = WorkspacesFile::file_path();
        std::fs::write(&path, input).unwrap();
        let mut loaded = WorkspacesFile::load_normalized().unwrap();
        assert_eq!(loaded.workspaces.len(), 5);
        assert_eq!(
            loaded.workspaces[0].home_dir(),
            loaded.workspaces[1].home_dir()
        );
        assert_eq!(
            loaded.workspaces[0].collapsed_issue_groups,
            gitterm::issue_titles::IssueGroup::default_collapsed()
        );
        assert!(loaded.workspaces[1].collapsed_issue_groups.is_empty());
        assert!(loaded.workspaces[2].is_parked());
        assert!(!loaded.workspaces[3].is_parked());
        assert!(loaded.workspaces[4].is_parked());
        assert_eq!(
            loaded.workspace_env("Fixture Beta", None)["FIXTURE_MODE"],
            "beta"
        );
        // Same selection-preserving reorder action on old and new formats.
        loaded.workspaces.swap(0, 1);
        loaded.active_workspace = 1 - loaded.active_workspace;
        loaded.save();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::from_str::<serde_json::Value>(expected).unwrap(),
            "{label}"
        );
        let again = WorkspacesFile::load_normalized().unwrap();
        again.save();
        let second: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved, second, "normalization remains idempotent: {label}");
        assert_eq!(saved["active_workspace"], 0);
        assert!(saved["workspaces"][0].get("uid").is_none());
        assert_eq!(
            saved["workspaces"][0]["tabs"][0]["agent_config"]["agent"],
            "gemini"
        );
    }
}

#[test]
fn persistence_running_park_uses_plain_parked_format() {
    if isolated("persistence_running_park_uses_plain_parked_format") {
        return;
    }
    let input = include_str!("../tests/fixtures/core/persistence/new.workspaces.json");
    let mut file: WorkspacesFile = serde_json::from_str(input).unwrap();
    let info = ParkedInfo {
        note: "Synthetic running park".into(),
        parked_at: "2000-01-03T00:00:00Z".into(),
    };
    file.workspaces[1] = running_parked_config(file.workspaces[1].clone(), &info);
    file.active_workspace = persisted_active_index(0, &[false, true]);
    file.save();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(WorkspacesFile::file_path()).unwrap()).unwrap();
    assert_eq!(
        saved,
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../tests/fixtures/core/persistence/running-park.saved.json"
        ))
        .unwrap()
    );
    let restored = WorkspacesFile::load().unwrap();
    // characterized: keep-running is runtime-only; restart restores an ordinary stopped parked workspace.
    assert!(restored.workspaces[1].is_parked());
    assert_eq!(restored.workspaces[1].parked, Some(info));
    assert_eq!(
        restored.workspaces[1].tabs[0].session_uid.as_deref(),
        Some("fixture-session-b")
    );
}

#[test]
fn input_wake_and_enter_git_reset_traces() {
    if isolated("input_wake_and_enter_git_reset_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(fixture.path());
    for (interval, streak, expected) in [(30000, 9, 0), (5000, 2, 2)] {
        let tab = app.active_tab_mut().unwrap();
        tab.status.git_poll_interval_ms = interval;
        tab.status.git_unchanged_streak = streak;
        tab.status.last_git_status_hash = Some(123);
        let _ = app.update_event(Event::Terminal(
            10,
            iced_term::Event::BackendCall(10, iced_term::backend::Command::Write(b"x".to_vec())),
        ));
        let tab = app.active_tab().unwrap();
        assert_eq!(
            (
                tab.status.git_poll_interval_ms,
                tab.status.git_unchanged_streak,
                tab.status.last_git_status_hash
            ),
            (5000, expected, Some(123))
        );
    }
    let _ = app.update_event(Event::WindowFocused);
    Trace::advance(300000);
    app.active_tab_mut().unwrap().status.git_poll_interval_ms = 30000;
    let _ = app.update_event(Event::WindowFocused);
    assert_eq!(
        app.active_tab().unwrap().status.git_poll_interval_ms,
        30000,
        "exactly 300s is not sleep"
    );
    Trace::advance(300001);
    let _ = app.update_event(Event::WindowFocused);
    let tab = app.active_tab().unwrap();
    assert_eq!(
        (
            tab.status.git_poll_interval_ms,
            tab.status.git_unchanged_streak,
            tab.status.last_poll,
            tab.status.last_git_status_hash
        ),
        (5000, 0, now() - Duration::from_secs(60), Some(123))
    );
    Trace::take();
    let _ = app.set_sidebar_mode(SidebarMode::Git);
    assert_eq!(requests(), ["local:10:/fixture/alpha"]);
    let tab = app.active_tab().unwrap();
    assert_eq!(
        (
            tab.status.last_poll,
            tab.status.git_status_loading,
            tab.status.last_git_status_hash
        ),
        (now(), true, Some(123))
    );
    let _ = app.set_sidebar_mode(SidebarMode::Git);
    assert!(
        requests().is_empty(),
        "re-entering current mode doesn't force refresh"
    );
}

#[test]
fn stopping_park_preserves_identity_and_resume_request_prelude() {
    if isolated("stopping_park_preserves_identity_and_resume_request_prelude") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    for parked in [0, 1] {
        let mut app = app_with_tabs(fixture.path());
        app.activate_workspace_at(1);
        let selected = app.workspaces[1].uid;
        let _ = app.park_stopping(
            parked,
            ParkedInfo {
                note: "synthetic".into(),
                parked_at: "2000-01-01T00:00:00Z".into(),
            },
        );
        assert_eq!(app.workspaces.len(), 2);
        assert_eq!(app.closed_workspace_configs.len(), 1);
        assert!(app.closed_workspace_configs[0].is_parked());
        if parked == 0 {
            assert_eq!(app.active_workspace().unwrap().uid, selected);
        } else {
            assert_eq!(app.active_workspace().unwrap().name, "gamma");
        }
        assert!(
            requests().is_empty(),
            "parking doesn't immediately poll status"
        );
        let before = app.active_tab().unwrap().status.last_poll;
        let _ = app.after_resume();
        assert_eq!(
            requests(),
            [format!(
                "local:{}:{}",
                app.active_tab().unwrap().id,
                app.active_tab().unwrap().repo_path.display()
            )]
        );
        // characterized: resume issues a request without setting loading or advancing the poll clock.
        assert_eq!(
            (
                app.active_tab().unwrap().status.git_status_loading,
                app.active_tab().unwrap().status.last_poll
            ),
            (false, before)
        );
    }
}

#[test]
fn status_file_order_and_hash_traces() {
    if isolated("status_file_order_and_hash_traces") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = app_with_tabs(fixture.path());
    for (reverse, expected_streak) in [(false, 0), (false, 1), (true, 0)] {
        let entry = |path: &str, status: &str, staged| FileEntry {
            path: path.into(),
            status: status.into(),
            is_staged: staged,
        };
        let mut value = snapshot(10, "/fixture/alpha", "ordered", true, true);
        value.staged = vec![entry("z.txt", "M", true), entry("a.txt", "A", true)];
        if reverse {
            value.staged.reverse();
        }
        value.unstaged = vec![entry("z.txt", "D", false)];
        apply(&mut app, value);
        let tab = app.active_tab().unwrap();
        let expected = if reverse {
            ["a.txt", "z.txt"]
        } else {
            ["z.txt", "a.txt"]
        };
        assert_eq!(
            tab.status
                .staged
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            tab.status
                .unstaged
                .iter()
                .map(|f| (f.path.as_str(), f.status.as_str(), f.is_staged))
                .collect::<Vec<_>>(),
            [("z.txt", "D", false)]
        );
        assert_eq!(tab.status.untracked[0].path, "new.txt");
        assert_eq!(tab.status.git_unchanged_streak, expected_streak);
    }
}

#[path = "stage45/mod.rs"]
mod stage45;
