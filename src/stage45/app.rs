use super::*;

fn task_app(dir: &Path) -> App {
    let mut app = app_with_tabs(dir);
    add_task(&mut app, dir, dir);
    let store = app.task_host.store_mut().as_mut().unwrap();
    let task = store.get("fixture-task").unwrap().clone();
    store
        .begin_worktree_preparation("fixture-task", "t1")
        .unwrap();
    store
        .complete_worktree_preparation(
            "fixture-task",
            gitterm::tasks::CompletedWorktreePreparation {
                repository: task.repository,
                base: task.base,
                branch: task.branch,
                path: dir.into(),
            },
            "t2",
        )
        .unwrap();
    app
}
fn handoff(summary: &str) -> task_mcp::UpdateTaskHandoffRequest {
    serde_json::from_value(json!({"task_id":"fixture-task", "summary":summary, "decisions":[" pin ", " "], "next_steps":[], "blockers":[]})).unwrap()
}

#[test]
fn task_launch_queue_and_store_before_handoff_ack() {
    if isolated("task_launch_queue_and_store_before_handoff_ack", "app") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = task_app(dir.path());
    Trace::take();
    let mut rows = Vec::new();
    rows.push(json!({"command":"launch missing", "error":app.try_launch_task_child("absent", None, false, TaskLaunchOrigin::Agent, false).err()}));
    rows.push(json!({"command":"launch invalid preset", "error":app.try_launch_task_child("fixture-task", Some(usize::MAX), false, TaskLaunchOrigin::Agent, false).err()}));
    let original = app
        .task_host
        .store()
        .as_ref()
        .unwrap()
        .get("fixture-task")
        .unwrap()
        .clone();
    let source: Value = serde_json::from_str(&fixture("tasks/old.tasks.json")).unwrap();
    let mut unprepared: TaskRecord = serde_json::from_value(source["tasks"][0].clone()).unwrap();
    unprepared.task_id = "unprepared".into();
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .insert(unprepared)
        .unwrap();
    rows.push(json!({"command":"launch unprepared", "error":app.try_launch_task_child("unprepared",None,false,TaskLaunchOrigin::Agent,false).err()}));
    let mut missing = original.clone();
    missing.worktree.path = Some(dir.path().join("missing"));
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .replace(missing)
        .unwrap();
    rows.push(json!({"command":"launch missing worktree", "error":app.try_launch_task_child("fixture-task",None,false,TaskLaunchOrigin::Agent,false).err().map(|e|e.replace(&dir.path().display().to_string(),"<fixture>"))}));
    let mut remote = original.clone();
    remote.workspace.location = WorkspaceLocationIdentity::RemoteAgent {
        remote_id: "fixture-remote".into(),
        root: "opaque/root".into(),
    };
    remote.executor = ExecutorTarget::RemoteAgent {
        remote_id: "fixture-remote".into(),
    };
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .replace(remote)
        .unwrap();
    rows.push(json!({"command":"launch remote", "error":app.try_launch_task_child("fixture-task",None,false,TaskLaunchOrigin::Agent,false).err()}));
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .replace(original)
        .unwrap();
    let harness = Some(HarnessSelection {
        kind: HarnessKind::TerminalPreset {
            preset_name: "Codex".into(),
        },
        model: None,
    });
    let holders = HashSet::from(["other-task".into()]);
    for (origin, override_choice) in [
        (TaskLaunchOrigin::Agent, false),
        (TaskLaunchOrigin::User, true),
    ] {
        let position = queue_task_launch_if_full(
            app.task_host.store_mut().as_mut().unwrap(),
            &mut app.task_launch_queue,
            QueuedTaskLaunch {
                task_id: "fixture-task".into(),
                preset_idx: Some(0),
                origin,
                user_model_choice: override_choice,
            },
            harness.clone(),
            &holders,
            false,
            1,
        )
        .unwrap();
        let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
        rows.push(json!({"command":"queue launch", "position":position, "queue_len":app.task_launch_queue.len(),
            "lifecycle":saved.get("fixture-task").unwrap().lifecycle,
            "origin":format!("{:?}", app.task_launch_queue[0].origin), "override":app.task_launch_queue[0].user_model_choice,
            "harness":saved.get("fixture-task").unwrap().harness}));
    }
    let result = app
        .update_handoff_requested(handoff("  Synthetic progress  "), None)
        .unwrap();
    let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    let stored = saved.get("fixture-task").unwrap().handoff.as_ref().unwrap();
    rows.push(json!({"command":"handoff", "ack":result["handoff_status"], "saved_summary":stored.summary, "saved_decisions":stored.decisions,
        "delegation_id":result["delegation_id"], "writer":stored.updated_by_session_id}));
    let before = app
        .task_host
        .store()
        .as_ref()
        .unwrap()
        .get("fixture-task")
        .unwrap()
        .clone();
    std::fs::remove_file(dir.path().join("tasks.json")).unwrap();
    std::fs::create_dir(dir.path().join("tasks.json")).unwrap();
    let error = app
        .update_handoff_requested(handoff("Must not acknowledge"), None)
        .unwrap_err();
    rows.push(json!({"command":"handoff failed store", "error_has_operation_and_path":error.contains("tasks.json") && error.contains("atomically write"),
        "memory_unchanged":app.task_host.store().as_ref().unwrap().get("fixture-task").unwrap() == &before, "ack":false}));
    golden("tasks/launch-handoff.trace.json", json!(rows));
}

#[test]
fn park_modes_ordered_effects_and_identity() {
    if isolated("park_modes_ordered_effects_and_identity", "app") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut rows = Vec::new();
    for mode in [ParkMode::KeepRunning, ParkMode::Stop] {
        let mut app = app_with_tabs(dir.path());
        let uid = app.workspaces[0].uid;
        let session = app.workspaces[0].tabs[0].session_uid.clone();
        Trace::take();
        let _ = app.park_workspace(uid, "  synthetic park  ", mode);
        let effects = Trace::take();
        let running = app.workspace_index_by_uid(uid);
        let config = if let Some(idx) = running {
            running_parked_config(
                app.workspace_config(&app.workspaces[idx]),
                app.workspaces[idx].running_park.as_ref().unwrap(),
            )
        } else {
            app.closed_workspace_configs[0].clone()
        };
        rows.push(json!({"command":format!("park {mode:?}"), "effects":effects,
            "live_workspace_count":app.workspaces.len(), "active":app.active_workspace().unwrap().name,
            "session_alive":running.is_some_and(|i| app.workspaces[i].tabs[0].session_uid == session),
            "saved_active":config.active,"saved_note":config.parked.as_ref().unwrap().note,
            "saved_session":config.tabs[0].session_uid == Some(session.clone())}));
        if let Some(idx) = running {
            app.return_running_parked(idx, Some(WakeReason::NeedsYou));
            rows.push(json!({"command":"wake", "effects":Trace::take(), "active":app.active_workspace().unwrap().name,
                "same_uid":app.workspaces[idx].uid == uid, "same_session":app.workspaces[idx].tabs[0].session_uid == session,
                "parked":app.workspaces[idx].running_park.is_some(), "woke":app.workspaces[idx].woke.map(|w| w.label())}));
        }
    }
    golden("tasks/park.trace.json", json!(rows));
}

pub(super) fn chat(
    id: &str,
    backend: chats::ChatBackend,
    cwd: &str,
    title: &str,
) -> chats::ChatIndexEntry {
    chats::ChatIndexEntry {
        id: id.into(),
        backend,
        path: PathBuf::from(format!("/fixture/{id}.jsonl")),
        cwd: PathBuf::from(cwd),
        repo_root: None,
        is_worktree: false,
        branch: Some("fixture-branch".into()),
        title: title.into(),
        latest: None,
        mtime: std::time::UNIX_EPOCH,
        size: 10,
        dead_cwd: true,
        facts: None,
        root_id: None,
    }
}
#[test]
fn find_scope_backend_query_and_machine_identity() {
    if isolated("find_scope_backend_query_and_machine_identity", "app") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let input: Value = serde_json::from_str(&fixture("work/find.input.json")).unwrap();
    for row in input.as_array().unwrap() {
        let mut entry = chat(
            row["id"].as_str().unwrap(),
            chats::ChatBackend::from_label(row["backend"].as_str().unwrap()).unwrap(),
            row["cwd"].as_str().unwrap(),
            row["title"].as_str().unwrap(),
        );
        entry.repo_root = row["repo_root"].as_str().map(PathBuf::from);
        entry.facts = row
            .get("facts")
            .cloned()
            .map(|v| serde_json::from_value(v).unwrap());
        Arc::make_mut(&mut app.chat_index).push(entry);
    }
    let mut rows = Vec::new();
    for (scope, filter, query) in [
        (chats::ChatScope::Workspace, None, ""),
        (chats::ChatScope::Machine, None, ""),
        (
            chats::ChatScope::Everywhere,
            Some(chats::ChatBackend::Codex),
            "synthetic",
        ),
        (chats::ChatScope::Workspace, None, "tru-999"),
    ] {
        app.chat_scope = scope;
        app.chat_backend_filter = filter;
        app.chat_query = query.into();
        let (_, visible) = app.visible_local_chats().unwrap();
        rows.push(
            json!({"scope":format!("{scope:?}"),"backend":filter.map(|b| b.label()),"query":query,
            "ids":visible.iter().map(|e| e.id.as_str()).collect::<Vec<_>>() }),
        );
    }
    app.workspaces[0].location = WorkspaceLocation::RemoteAgent {
        remote_id: "fixture-remote".into(),
        workspace_id: "fixture-workspace".into(),
        root: "opaque/root".into(),
    };
    app.chat_scope = chats::ChatScope::Machine;
    rows.push(
        json!({"command":"remote machine", "local_list":app.visible_local_chats().is_some()}),
    );
    let local = chats::ChatRef {
        remote_id: None,
        id: "same-id".into(),
    };
    let remote = chats::ChatRef {
        remote_id: Some("fixture-remote".into()),
        id: "same-id".into(),
    };
    rows.push(json!({"command":"same transcript id on two machines", "identities_differ":local != remote}));
    golden("work/find.trace.json", json!(rows));
}

#[test]
fn overlay_inventory() {
    let mut names = Vec::new();
    macro_rules! overlay { ($($field:ident),+ $(,)?) => {
        let WebviewOverlays { $($field: _),+ } = WebviewOverlays::default();
        $(
        let mut layer = WebviewOverlays { $field:true, ..Default::default() };
        for viewer in [ViewerWebview::None, ViewerWebview::Page, ViewerWebview::PlansViewer] {
            assert_eq!(visible_webview_surface(viewer, ActiveTabSurface::AgentChat(7), layer.is_open()), VisibleSurface::None);
        }
        layer.$field = false;
        assert!(!layer.is_open());
        assert_eq!(visible_webview_surface(ViewerWebview::None, ActiveTabSurface::AgentChat(7), layer.is_open()), VisibleSurface::AgentChat(7));
        names.push(stringify!($field));
    )+}; }
    overlay!(
        usage,
        browser_evidence,
        task_switcher,
        task_tab_close,
        task_worktree_delete,
        task_publish,
        new_task,
        help,
        new_work,
        park,
        parked_list,
        pr_action,
        ready_list,
        workspace_settings,
        bar_menu,
        tab_picker,
        phone,
        remote_connect,
        folder_picker
    );
    golden("work/overlays.json", json!(names));
}

#[test]
fn review_ipc_range_normalization_and_stale_completions() {
    if isolated(
        "review_ipc_range_normalization_and_stale_completions",
        "app",
    ) {
        return;
    }
    use gitterm::review_pane::{ReviewCommit, ReviewIpc, ReviewSource, ReviewTarget};
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let _trace = Trace::start();
    let target = ReviewTarget {
        checkout: dir.path().into(),
        repo: "fixture".into(),
        branch: "fixture".into(),
        owner: "synthetic".into(),
        base: Some(overview::BaseChoice {
            reference: "main".into(),
            source: overview::BaseSource::Local,
            considered: 1,
        }),
    };
    let _ = app.start_review(
        target,
        ReviewSource::Commits(None),
        true,
        Some("a.rs".into()),
    );
    let mut rows = Vec::new();
    for pending in [true, false] {
        let stamp = app.review.pane().unwrap().stamp();
        let mut load = empty_review_load(app.review.pane().unwrap());
        load.commits = ["newest", "oldest"]
            .into_iter()
            .map(|id| ReviewCommit {
                sha: id.into(),
                short_sha: id.into(),
                subject: id.into(),
                author: "Fixture".into(),
                authored_at: 0,
            })
            .collect();
        let _ = app.review_loaded(stamp.clone(), Ok(load));
        let token = app.review.pane().unwrap().token.clone();
        let old_generation = stamp;
        let _ = app.handle_review_ipc(ReviewIpc::SelectCommits {
            token,
            first: "newest".into(),
            last: if pending { "newest" } else { "oldest" }.into(),
            pending,
        });
        let _ = app.review_loaded(old_generation, Err("stale completion".into()));
        let pane = app.review.pane().unwrap();
        rows.push(json!({"command":"select commits","pending":pending,"source":format!("{:?}",pane.source),"generation":pane.generation,"focus":pane.focus,"hide_noise":pane.hide_noise,"error":pane.error}));
    }
    let generation = app.review.pane().unwrap().stamp();
    let _ = app.handle_review_ipc(ReviewIpc::Close {
        token: "old-token".into(),
    });
    rows.push(
        json!({"command":"wrong token","still_open":app.viewer_webview==ViewerWebview::Review}),
    );
    let _ = app.review_loaded(generation.clone(), Err("current completion".into()));
    rows.push(json!({"command":"current completion","error":app.review.pane().unwrap().error}));
    app.viewer_webview = ViewerWebview::None;
    let _ = app.review_loaded(generation, Err("completion after close".into()));
    rows.push(json!({"command":"completion after close","error":app.review.pane().unwrap().error}));
    golden("review/iced.trace.json", json!(rows));
}

#[test]
fn task_stop_cross_workspace_and_cancel_without_consent() {
    if isolated(
        "task_stop_cross_workspace_and_cancel_without_consent",
        "app",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _trace = Trace::start();
    let mut app = task_app(dir.path());
    app.task_host
        .store_mut()
        .as_mut()
        .unwrap()
        .record_lifecycle_signal(
            "fixture-task",
            TaskLifecycle::Running,
            None,
            "2000-01-01T00:05:00Z",
        )
        .unwrap();
    for (index, ws) in app.workspaces.iter_mut().enumerate().take(2) {
        ws.tabs[0].task_id = Some("fixture-task".into());
        ws.tabs[0].task_session_id = Some(format!("fixture-session-{index}"));
        // An unrelated headless tab avoids the fallback PTY creation path.
        ws.tabs.push(TabState::new(100 + index, ws.dir.clone()));
    }
    Trace::take();
    let _ = app.update(Event::TaskStop("fixture-task".into()));
    let persisted = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    let task = persisted.get("fixture-task").unwrap();
    let stop = json!({"effects":Trace::take(),"remaining_task_tabs":app.workspaces.iter().flat_map(|w| &w.tabs).filter(|t|t.task_id.as_deref()==Some("fixture-task")).count(),
        "lifecycle":task.lifecycle,"worktree_kept":task.worktree.path.as_deref()==Some(dir.path()),"selected":app.selected_task_id});
    app.task_publish_prompt = Some(TaskPublishPrompt {
        task_id: "fixture-task".into(),
        action: TaskPublishAction::PushBranch,
    });
    let before = std::fs::read(dir.path().join("tasks.json")).unwrap();
    let _ = app.update(Event::TaskPublishCancel);
    golden(
        "tasks/stop-cancel.trace.json",
        json!({"stop":stop,"cancel":{"prompt_gone":app.task_publish_prompt.is_none(),"store_unchanged":std::fs::read(dir.path().join("tasks.json")).unwrap()==before,"publishing":!app.task_publish_busy.is_empty()}}),
    );
}

#[test]
fn launch_commands_brief_and_artifact_identity() {
    if isolated("launch_commands_brief_and_artifact_identity", "app") {
        return;
    }
    let source: Value = serde_json::from_str(&fixture("tasks/old.tasks.json")).unwrap();
    let task: TaskRecord = serde_json::from_value(source["tasks"][0].clone()).unwrap();
    let path = write_task_brief("fixture-session", &task, Some("fixture-delegation")).unwrap();
    let brief = std::fs::read_to_string(&path).unwrap();
    let mut commands = Vec::new();
    for command in ["codex", "claude", "pi", "agy", "unknown-agent"] {
        commands.push(json!({"preset":command,"mcp":task_mcp::configure_task_command(command,"http://127.0.0.1:1/mcp?caller=fixture-caller"),
            "prompt":command_with_initial_prompt(command,Path::new("/fixture/task-briefs/fixture-session.md")),
            "require_task_tools_accepts":gitterm::workers::require_task_tools(command).is_ok()}));
    }
    golden(
        "tasks/commands.trace.json",
        json!({"commands":commands,"brief":brief,
        "brief_relative_path":path.strip_prefix(config::global_config_dir()).unwrap().display().to_string(),
        "log_path":gitterm::delegations::log_path(Path::new("/fixture/config"),"fixture-delegation").display().to_string()}),
    );
}

#[test]
fn work_and_find_completion_identity_and_error_paths() {
    if isolated("work_and_find_completion_identity_and_error_paths", "app") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let _trace = Trace::start();
    let uid = app.workspaces[0].uid;
    let _ = app.start_work_documents_scan();
    let _ = app.start_work_documents_scan();
    assert!(app.work_documents[&uid].in_flight);
    app.activate_workspace_at(1);
    let _ = app.update(Event::WorkDocumentsScanned(
        uid,
        Box::new(Ok(gitterm::work_documents::ScanResult::default())),
    ));
    let loaded = json!({"background_owner_loaded":app.work_documents[&uid].result.is_some(),"in_flight":app.work_documents[&uid].in_flight,
        "active_has_result":app.work_documents.get(&app.workspaces[1].uid).is_some_and(|s|s.result.is_some())});
    let _ = app.update(Event::WorkDocumentsScanned(
        uid,
        Box::new(Err("synthetic document failure".into())),
    ));
    let failed = json!({"error":app.work_documents[&uid].error,"old_result_retained":app.work_documents[&uid].result.is_some()});
    app.work_documents.remove(&uid);
    let _ = app.update(Event::WorkDocumentsScanned(
        uid,
        Box::new(Ok(gitterm::work_documents::ScanResult::default())),
    ));
    let removed = !app.work_documents.contains_key(&uid);
    app.workspaces[1].location = WorkspaceLocation::RemoteAgent {
        remote_id: "fixture-remote".into(),
        workspace_id: "fixture-workspace".into(),
        root: "opaque/root".into(),
    };
    let remote_refused = app.work_documents_request(&app.workspaces[1]).is_none();
    let current = chats::ChatRef {
        remote_id: None,
        id: "new-selection".into(),
    };
    app.workspaces[1].tabs[0].selected_chat = Some(current.clone());
    let late = chats::ChatRef {
        remote_id: Some("fixture-remote".into()),
        id: "old-selection".into(),
    };
    let _ = app.update(Event::ChatPreviewLoaded(
        late.clone(),
        chats::ChatPreview::default(),
        Box::new(chat_summary::digest_messages(&[])),
    ));
    golden(
        "work/completions.trace.json",
        json!({"documents":{"loaded":loaded,"failure":failed,"removed_owner_ignored":removed,"remote_request_refused":remote_refused},
        "find":{"selection_still_current":app.workspaces[1].tabs[0].selected_chat.as_ref()==Some(&current),
        "late_preview_cached":app.chat_preview.as_ref().is_some_and(|(id,_)|id==&late)}}),
    );
}

#[test]
fn review_viewed_ipc_loaded_identity_and_corrupt_store() {
    if isolated("review_viewed_ipc_loaded_identity_and_corrupt_store", "app") {
        return;
    }
    use gitterm::review_pane::*;
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let _trace = Trace::start();
    let target = ReviewTarget {
        checkout: dir.path().into(),
        repo: "fixture".into(),
        branch: "fixture".into(),
        owner: "synthetic".into(),
        base: None,
    };
    let _ = app.start_review(target, ReviewSource::Uncommitted, false, None);
    let token = app.review.pane().unwrap().token.clone();
    let path = config::global_config_dir().join(VIEWED_FILE);
    let _ = app.handle_review_ipc(ReviewIpc::SetViewed {
        token: token.clone(),
        path: "a.rs".into(),
        viewed: true,
    });
    let loading_does_not_write = !path.exists();
    let file = ReviewFile {
        path: "a.rs".into(),
        old_path: None,
        status: FileStatus::Modified,
        kind: FileKind::Code,
        additions: 0,
        deletions: 0,
        truncated: false,
        omitted: false,
        note: None,
        hunks: vec![],
        content_id: Some("fixture-content-id".into()),
    };
    let mut no_id = file.clone();
    no_id.path = "unhashable.rs".into();
    no_id.content_id = None;
    let files = vec![file, no_id];
    let totals = ReviewTotals::of(&files);
    let stamp = app.review.pane().unwrap().stamp();
    let mut load = empty_review_load(app.review.pane().unwrap());
    load.diff.files = files;
    load.diff.totals = totals;
    let _ = app.review_loaded(stamp, Ok(load));
    for file in ["a.rs", "unhashable.rs", "unlisted.rs"] {
        let _ = app.handle_review_ipc(ReviewIpc::SetViewed {
            token: token.clone(),
            path: file.into(),
            viewed: true,
        });
    }
    let stored = read_json(&path);
    let marked = stored["checkouts"][dir.path().display().to_string()].clone();
    std::fs::write(&path, "{broken").unwrap();
    app.review
        .reduce(ReviewIntent::MarksLoaded(Err("retry fixture".into())));
    let _ = app.handle_review_ipc(ReviewIpc::SetViewed {
        token,
        path: "a.rs".into(),
        viewed: false,
    });
    golden(
        "review/viewed-ipc.trace.json",
        json!({"loading_does_not_write":loading_does_not_write,"marked_paths":marked,"corrupt_store_preserved":std::fs::read_to_string(path).unwrap()=="{broken"}),
    );
}

#[test]
fn task_mcp_receiver_is_taken_once() {
    if isolated("task_mcp_receiver_is_taken_once", "app") {
        return;
    }
    use iced::futures::{FutureExt, StreamExt};
    let _sender = init_task_control_channel();
    let mut first = Box::pin(task_control_stream());
    let first_pending = first.next().now_or_never().is_none();
    let mut second = Box::pin(task_control_stream());
    let second_closed = matches!(second.next().now_or_never(), Some(None));
    golden(
        "tasks/receiver.trace.json",
        json!({"first_pending":first_pending,"second_closed":second_closed,"receiver_slot_empty":task_mcp::take_command_receiver().is_none()}),
    );
}

#[test]
fn review_target_switch_and_base_guards() {
    if isolated("review_target_switch_and_base_guards", "app") {
        return;
    }
    use gitterm::review_pane::*;
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let _trace = Trace::start();
    let target = ReviewTarget {
        checkout: dir.path().join("first"),
        repo: "fixture".into(),
        branch: "feature".into(),
        owner: "synthetic".into(),
        base: None,
    };
    let _ = app.start_review(
        target.clone(),
        ReviewSource::Uncommitted,
        true,
        Some("a.rs".into()),
    );
    let first = app.review.pane().unwrap().clone();
    let _ = app.handle_review_ipc(ReviewIpc::SetSource {
        token: first.token.clone(),
        source: overview::DiffSource::VsBase,
        hide_noise: false,
    });
    let refused_base = app.review.pane().unwrap().generation == first.generation;
    let _ = app.handle_review_ipc(ReviewIpc::ShowCommits {
        token: first.token.clone(),
    });
    let refused_commits = app.review.pane().unwrap().generation == first.generation;
    let mut second = target;
    second.checkout = dir.path().join("second");
    let _ = app.start_review(second.clone(), ReviewSource::Uncommitted, false, None);
    let _ = app.review_loaded(first.stamp(), Err("wrong target".into()));
    let _ = app.handle_review_ipc(ReviewIpc::Close { token: first.token });
    let pane = app.review.pane().unwrap();
    let target_unchanged = pane.target == second;
    let stale_error = pane.error.clone();
    let generation = pane.stamp();
    let _ = app.review_loaded(
        generation,
        Ok(ReviewLoad {
            diff: ReviewDiff {
                checkout: second.checkout.clone(),
                branch: second.branch.clone(),
                base: None,
                source: overview::DiffSource::Uncommitted,
                merge_base: None,
                files: Vec::new(),
                totals: ReviewTotals::default(),
                unlisted: 0,
            },
            commits: Vec::new(),
            turns: Vec::new(),
            commits_truncated: true,
            commits_error: Some("fixture metadata".into()),
            canonical_checkout: second.checkout.clone(),
            live_paths: None,
        }),
    );
    let pane = app.review.pane().unwrap();
    golden(
        "review/target.trace.json",
        json!({"missing_base_refused":refused_base,"missing_commit_base_refused":refused_commits,
        "target_unchanged":target_unchanged,"stale_error":stale_error,"still_open":app.viewer_webview==ViewerWebview::Review,
        "loaded":pane.loaded.is_some(),"commits_truncated":pane.commits_truncated,"commits_error":pane.commits_error}),
    );
}

fn empty_review_load(pane: &gitterm_core::review::state::Pane) -> gitterm::review_pane::ReviewLoad {
    use gitterm::review_pane::*;
    ReviewLoad {
        diff: ReviewDiff {
            checkout: pane.target.checkout.clone(),
            branch: pane.target.branch.clone(),
            base: pane.target.base.clone(),
            source: overview::DiffSource::Uncommitted,
            merge_base: None,
            files: Vec::new(),
            totals: ReviewTotals::default(),
            unlisted: 0,
        },
        commits: Vec::new(),
        turns: Vec::new(),
        commits_truncated: false,
        commits_error: None,
        canonical_checkout: pane.target.checkout.clone(),
        live_paths: None,
    }
}
