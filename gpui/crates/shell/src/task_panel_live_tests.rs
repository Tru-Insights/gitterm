// Included in task_host_tests::live_writes to reuse the synthetic HOME,
// admitted Live services, recorder and fake harness binaries.
use crate::task_panel::{
    Inputs as PanelInputs, Location as PanelLocation, Modal as PanelModal, TaskPanel,
    Write as PanelWrite,
};

fn panel_inputs(repo: &Path) -> PanelInputs {
    PanelInputs {
        workspace_name: "Placeholder".into(),
        has_workspace: true,
        location: PanelLocation::local(repo),
        active_repo: Some(repo.into()),
        presets: vec![
            req(json!({"name":"Claude","command":"claude --model placeholder"})),
            req(json!({"name":"Codex","command":"codex -m placeholder"})),
        ],
        ..Default::default()
    }
}
fn panel_record(host: &Entity<TaskHostOwner>, id: &str, cx: &mut TestAppContext) -> TaskRecord {
    host.read_with(cx, |h, _| {
        h.host().store().as_ref().unwrap().get(id).unwrap().clone()
    })
}
fn panel_new(host: &Entity<TaskHostOwner>, cx: &mut TestAppContext) -> Entity<TaskPanel> {
    cx.update(|cx| {
        cx.set_global(crate::task_host::GpuiTaskHost(host.clone()));
        cx.new(|_| TaskPanel::default())
    })
}
fn panel_write(
    panel: &Entity<TaskPanel>,
    write: PanelWrite,
    inputs: &PanelInputs,
    cx: &mut TestAppContext,
) {
    panel.update(cx, |p, cx| p.write(write, inputs, cx));
    cx.run_until_parked();
}

#[gpui::test]
fn task_panel_live_stop_waits_and_preserves_new_session(cx: &mut TestAppContext) {
    if child("task_panel_live_stop_waits_and_preserves_new_session") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    worker_settings(&fixture);
    golive10_workspace(
        &fixture,
        &repo_fx.repo,
        json!([{"dir":repo_fx.repo,"session_uid":"placeholder-home"}]),
        json!([]),
    );
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &repo_fx.repo);
    let id = created["task_id"].as_str().unwrap().to_owned();
    let queued = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
        "title":"Placeholder queued", "objective":"Placeholder objective", "repository_path":repo_fx.repo
    })))).unwrap()["task_id"].as_str().unwrap().to_owned();
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    fake.prepare_recorder(&host, vcx);
    let panel = shell.update_in(vcx, |s, w, cx| s.ensure_task_panel(w, cx));
    let launch = PanelWrite::Launch {
        task_id: id.clone(),
        preset: Some(1),
    };
    panel_write(&panel, launch.clone(), &panel_inputs(&repo_fx.repo), vcx);
    wait_until(vcx, |_| fake.file("ready", 1).exists());
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| s.task_inputs(cx).open_session(&id).is_some())
    });
    let inputs = shell.read_with(vcx, |s, cx| s.task_inputs(cx));
    let old = panel_record(&host, &id, vcx).sessions[0]
        .task_session_id
        .clone();
    shell.update(vcx, |s, cx| {
        s.owner.update(cx, |o, _| o.workspace_save_pending = true)
    });
    panel_write(&panel, PanelWrite::Stop(id.clone()), &inputs, vcx);
    panel_write(&panel, launch.clone(), &inputs, vcx);
    assert_eq!(
        panel_record(&host, &id, vcx).sessions.len(),
        1,
        "Stop must remain pending through workspace persistence"
    );
    // Another ingress can start a session despite the panel's pending guard.
    host.update(vcx, |h, cx| h.launch_from_ui(&id, Some(1), cx))
        .unwrap();
    wait_until(vcx, |_| fake.file("ready", 2).exists());
    let new = panel_record(&host, &id, vcx).sessions[1]
        .task_session_id
        .clone();
    host.update(vcx, |h, _| {
        h.host_mut()
            .store_mut()
            .as_mut()
            .unwrap()
            .record_lifecycle_signal(&queued, TaskLifecycle::Queued, None, "2026-11-20T12:00:00Z")
            .unwrap();
        h.host_mut()
            .launch_queue_mut()
            .push(gitterm_core::task_host::QueuedTaskLaunch {
                task_id: queued.clone(),
                preset_idx: Some(1),
                origin: TaskLaunchOrigin::User,
                user_model_choice: false,
            });
    });
    shell.update(vcx, |s, cx| {
        s.owner.update(cx, |o, _| o.workspace_save_pending = false)
    });
    vcx.executor().advance_clock(Duration::from_millis(20));
    wait_until(vcx, |cx| {
        cx.executor().advance_clock(Duration::from_millis(20));
        host.read_with(cx, |h, _| h.launched_live(&old).is_none())
    });
    assert_eq!(
        host.read_with(vcx, |h, _| h.launched_live(&new)),
        Some(true)
    );
    wait_until(vcx, |cx| {
        cx.executor().advance_clock(Duration::from_millis(20));
        !panel.read_with(cx, |p, _| p.task_stopping(&id))
    });
    vcx.run_until_parked();
    assert_eq!(
        panel_record(&host, &id, vcx).lifecycle,
        TaskLifecycle::Running
    );
    assert_eq!(
        panel_record(&host, &queued, vcx).lifecycle,
        TaskLifecycle::Queued,
        "A superseded Stop must not promote the queue"
    );
    assert!(host.read_with(vcx, |h, _| {
        h.host().launch_queue().iter().any(|q| q.task_id == queued)
    }));
    // Once the closure acknowledgement arrives the panel accepts Continue again.
    panel_write(&panel, launch, &inputs, vcx);
    assert_eq!(panel_record(&host, &id, vcx).sessions.len(), 3);
    host.update(vcx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_launch_keeps_duplicate_preset_index(cx: &mut TestAppContext) {
    if child("task_panel_live_launch_keeps_duplicate_preset_index") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    std::fs::write(
        fixture.iced.join("settings.json"),
        serde_json::to_vec(&json!({
            "agent_presets": [
                {"name":"Duplicate", "command":"claude --model placeholder-first"},
                {"name":"duplicate", "command":"codex -m placeholder-second"}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &repo_fx.repo);
    let id = created["task_id"].as_str().unwrap();
    fake.prepare_recorder(&host, cx);
    let panel = panel_new(&host, cx);
    let mut inputs = panel_inputs(&repo_fx.repo);
    inputs.presets = req(json!([
        {"name":"Duplicate", "command":"claude --model placeholder-first"},
        {"name":"duplicate", "command":"codex -m placeholder-second"}
    ]));
    panel_write(
        &panel,
        PanelWrite::Launch {
            task_id: id.into(),
            preset: Some(1),
        },
        &inputs,
        cx,
    );
    wait_until(cx, |_| fake.file("ready", 1).exists());
    let args = fake.text("args", 1);
    assert!(args.contains("placeholder-second"), "{args}");
    assert!(!args.contains("placeholder-first"), "{args}");
    assert_eq!(panel_record(&host, id, cx).sessions[0].label, "duplicate");
    host.update(cx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_form_uses_form_authority_and_prepares_worktree(cx: &mut TestAppContext) {
    if child("task_panel_live_form_uses_form_authority_and_prepares_worktree") {
        return;
    }
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    golive10_workspace(
        &fixture,
        &repo_fx.repo,
        json!([{"dir":repo_fx.repo,"session_uid":"placeholder-home"}]),
        json!([]),
    );
    let (host, sender) = start(&fixture, cx);
    // Control remains caller-authorized; the UI does not impersonate a caller.
    assert!(
        dispatch_as(cx, &sender, create_in(&repo_fx.repo), None)
            .unwrap_err()
            .contains("caller")
    );
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    let panel = shell.update_in(vcx, |s, w, cx| s.ensure_task_panel(w, cx));
    let inputs = panel_inputs(&repo_fx.repo);
    let id = panel.update_in(vcx, |p, w, cx| {
        p.open_create(&inputs, w, cx);
        let Some(PanelModal::Create(form)) = &p.modal else {
            panic!("missing form")
        };
        form.title
            .update(cx, |i, cx| i.set_value("  Placeholder task  ", w, cx));
        form.objective
            .update(cx, |i, cx| i.set_value("  Placeholder objective  ", w, cx));
        form.issue.update(cx, |i, cx| i.set_value("TRU-274", w, cx));
        form.task_id.clone()
    });
    panel_write(&panel, PanelWrite::CreateSubmit, &inputs, vcx);
    wait_until(vcx, |cx| {
        assert!(
            panel.read_with(cx, |p, _| p.notice.is_none()),
            "{:?}",
            panel.read_with(cx, |p, _| p.notice.clone())
        );
        host.read_with(cx, |h, _| {
            h.host()
                .store()
                .as_ref()
                .unwrap()
                .get(&id)
                .is_some_and(|t| t.worktree.state == gitterm_core::tasks::TaskWorktreeState::Ready)
        })
    });
    let saved = panel_record(&host, &id, vcx);
    assert_eq!(saved.title, "Placeholder task");
    assert_eq!(saved.objective, "Placeholder objective");
    assert_eq!(
        saved.created_by.unwrap().kind,
        gitterm_core::tasks::TaskCreatorKind::Manual
    );
    assert!(saved.worktree.path.unwrap().join("README").is_file());
    wait_until(vcx, |cx| !panel.read_with(cx, |p, _| p.modal_open()));
    assert!(shell.read_with(vcx, |s, _| s.selected_task.is_none()));
    host.update(vcx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_start_continue_and_resume_use_host_terminal(cx: &mut TestAppContext) {
    if child("task_panel_live_start_continue_and_resume_use_host_terminal") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    worker_settings(&fixture);
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &repo_fx.repo);
    let id = created["task_id"].as_str().unwrap();
    fake.prepare_recorder(&host, cx);
    let panel = panel_new(&host, cx);
    let inputs = panel_inputs(&repo_fx.repo);
    for run in 1..=2 {
        panel_write(
            &panel,
            PanelWrite::Launch {
                task_id: id.into(),
                preset: Some(1),
            },
            &inputs,
            cx,
        );
        wait_until(cx, |_| fake.file("ready", run).exists());
        let saved = panel_record(&host, id, cx);
        assert_eq!(saved.lifecycle, TaskLifecycle::Running);
        assert_eq!(
            PathBuf::from(fake.text("pwd", run).trim()),
            saved.worktree.path.unwrap()
        );
        let args = fake.text("args", run);
        assert!(args.contains("-m\nplaceholder"), "{args}");
        assert!(fake.text("env", run).contains("GITTERM_V6_TASK_MCP_URL="));
        let session = saved.sessions.last().unwrap().task_session_id.clone();
        host.update(cx, |h, cx| h.session_closed(&session, cx));
        cx.run_until_parked();
    }
    // Resume the stored conversation, preserving its identity and command.
    let mut saved = panel_record(&host, id, cx);
    let session = saved.sessions.last_mut().unwrap();
    session.conversation = Some(gitterm_core::tasks::HarnessConversationRef {
        backend: gitterm_core::tasks::HarnessConversationBackend::Codex,
        session_id: "placeholder-history".into(),
    });
    let session_id = session.task_session_id.clone();
    host.update(cx, |h, _| {
        h.host_mut()
            .store_mut()
            .as_mut()
            .unwrap()
            .replace(saved)
            .unwrap()
    });
    panel_write(&panel, PanelWrite::Resume(id.into()), &inputs, cx);
    wait_until(cx, |_| fake.file("ready", 3).exists());
    let args = fake.text("args", 3);
    assert!(args.contains("resume\nplaceholder-history"), "{args}");
    let saved = panel_record(&host, id, cx);
    assert_eq!(saved.sessions.last().unwrap().task_session_id, session_id);
    assert_eq!(saved.lifecycle, TaskLifecycle::Running);
    host.update(cx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_stop_all_sessions_and_cancel_queue(cx: &mut TestAppContext) {
    if child("task_panel_live_stop_all_sessions_and_cancel_queue") {
        return;
    }
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    worker_settings(&fixture);
    golive10_workspace(
        &fixture,
        &repo_fx.repo,
        json!([{"dir":repo_fx.repo,"session_uid":"placeholder-home"}]),
        json!([]),
    );
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &repo_fx.repo);
    let id = created["task_id"].as_str().unwrap().to_owned();
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    fake.prepare_recorder(&host, vcx);
    let panel = shell.update_in(vcx, |s, w, cx| s.ensure_task_panel(w, cx));
    let mut inputs = panel_inputs(&repo_fx.repo);
    for run in 1..=2 {
        panel_write(
            &panel,
            PanelWrite::Launch {
                task_id: id.clone(),
                preset: Some(1),
            },
            &inputs,
            vcx,
        );
        wait_until(vcx, |_| fake.file("ready", run).exists());
    }
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| s.task_inputs(cx).open_sessions.len() == 2)
    });
    inputs = shell.read_with(vcx, |s, cx| s.task_inputs(cx));
    shell.update(vcx, |s, cx| {
        s.owner.update(cx, |o, _| o.workspace_save_pending = true)
    });
    host.update(vcx, |h, _| {
        let store = h.host_mut().store_mut().as_mut().unwrap();
        let mut task = store.get(&id).unwrap().clone();
        let mut session = task.sessions.last().unwrap().clone();
        session.task_session_id = "placeholder-unadopted-task-session".into();
        session.conversation = Some(gitterm_core::tasks::HarnessConversationRef {
            backend: gitterm_core::tasks::HarnessConversationBackend::Codex,
            session_id: "placeholder-unadopted-history".into(),
        });
        task.sessions.push(session);
        store.replace(task).unwrap();
    });
    panel_write(
        &panel,
        PanelWrite::ResumeConversation {
            task_id: id.clone(),
            chat_id: "placeholder-unadopted-history".into(),
        },
        &inputs,
        vcx,
    );
    wait_until(vcx, |_| fake.file("ready", 3).exists());
    let unadopted = panel_record(&host, &id, vcx)
        .sessions
        .last()
        .unwrap()
        .task_session_id
        .clone();
    let unadopted_pid = fake.text("pid", 3).trim().to_owned();
    let unadopted_uid = host.read_with(vcx, |h, cx| {
        h.panel_facts(cx)
            .sessions
            .iter()
            .find(|s| s.target.task_session_id.as_deref() == Some(&unadopted))
            .unwrap()
            .target
            .session_uid
            .clone()
    });
    assert_ne!(unadopted_uid, unadopted);
    panel_write(&panel, PanelWrite::Stop(id.clone()), &inputs, vcx);
    assert_eq!(
        panel_record(&host, &id, vcx).lifecycle,
        TaskLifecycle::Running
    );
    shell.update(vcx, |s, cx| {
        s.owner.update(cx, |o, _| o.workspace_save_pending = false)
    });
    vcx.executor().advance_clock(Duration::from_millis(20));
    wait_until(vcx, |cx| {
        host.read_with(cx, |h, _| h.launched_live(&unadopted).is_none())
            && !super::launch_tests::exists(&unadopted_pid)
    });
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| s.task_inputs(cx).open_sessions.is_empty())
    });
    wait_until(vcx, |cx| {
        panel_record(&host, &id, cx).lifecycle == TaskLifecycle::Stopped
    });
    host.read_with(vcx, |h, _| {
        for caller in [&unadopted, &unadopted_uid] {
            assert!(
                h.host()
                    .require_issued_caller(Some(caller), "task_update_handoff")
                    .is_err()
            );
        }
    });
    // Cancel is a lifecycle signal, not a whole-record replacement; attempts settle too.
    host.update(vcx, |h, _| {
        h.host_mut()
            .store_mut()
            .as_mut()
            .unwrap()
            .record_lifecycle_signal(&id, TaskLifecycle::Queued, None, "2026-11-20T12:00:00Z")
            .unwrap();
        h.host_mut()
            .launch_queue_mut()
            .push(gitterm_core::task_host::QueuedTaskLaunch {
                task_id: id.clone(),
                preset_idx: Some(1),
                origin: TaskLaunchOrigin::User,
                user_model_choice: false,
            });
    });
    panel_write(&panel, PanelWrite::CancelQueue(id.clone()), &inputs, vcx);
    assert_eq!(
        panel_record(&host, &id, vcx).lifecycle,
        TaskLifecycle::Ready
    );
    assert!(host.read_with(vcx, |h, _| h.host().launch_queue().is_empty()));
    host.update(vcx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_archives_every_workspace_match(cx: &mut TestAppContext) {
    if child("task_panel_live_archives_every_workspace_match") {
        return;
    }
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    let (host, sender) = start(&fixture, cx);
    let ids: Vec<_> =
        (0..4)
            .map(|n| {
                dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
            "title": format!("Placeholder task {n}"), "objective":"Placeholder objective",
            "repository_path": repo_fx.repo,
        })))).unwrap()["task_id"].as_str().unwrap().to_owned()
            })
            .collect();
    host.update(cx, |h, _| {
        let store = h.host_mut().store_mut().as_mut().unwrap();
        for id in &ids[..2] {
            store
                .record_lifecycle_signal(id, TaskLifecycle::Failed, None, "2026-11-20T12:00:00Z")
                .unwrap();
        }
    });
    let missing = panel_record(&host, &ids[2], cx).worktree.path.unwrap();
    git(
        &repo_fx.repo,
        &["worktree", "remove", missing.to_str().unwrap()],
    );
    let panel = panel_new(&host, cx);
    let inputs = panel_inputs(&repo_fx.repo);
    panel_write(&panel, PanelWrite::ArchiveFailed, &inputs, cx);
    for id in &ids[..2] {
        assert_eq!(
            panel_record(&host, id, cx).lifecycle,
            TaskLifecycle::Archived
        );
    }
    panel_write(&panel, PanelWrite::ArchiveFinished, &inputs, cx);
    assert_eq!(
        panel_record(&host, &ids[2], cx).lifecycle,
        TaskLifecycle::Archived
    );
    assert_eq!(
        panel_record(&host, &ids[3], cx).lifecycle,
        TaskLifecycle::Ready
    );
    panel_write(&panel, PanelWrite::Archive(ids[3].clone()), &inputs, cx);
    assert_eq!(
        panel_record(&host, &ids[3], cx).lifecycle,
        TaskLifecycle::Archived
    );
    host.update(cx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_publish_allows_stop_and_cancel(cx: &mut TestAppContext) {
    if child("task_panel_live_publish_allows_stop_and_cancel") {
        return;
    }
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    let bare = repo_fx.root.join("origin.git");
    git(
        &repo_fx.root,
        &["init", "--bare", "-q", bare.to_str().unwrap()],
    );
    git(
        &repo_fx.repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    git(
        &repo_fx.repo,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            bare.to_str().unwrap(),
        ],
    );
    let trace = repo_fx.root.join("gh-trace");
    let marker = repo_fx.root.join("pr-created");
    let started = repo_fx.root.join("publication-started");
    let release = repo_fx.root.join("publication-release");
    let q = gitterm_core::workers::shell_word;
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$PWD|$GH_TOKEN|$*" >> {trace}
case "$1 $2" in
 '--version ') exit 0;;
 'auth status') printf 'github.com\n  ✓ Logged in to github.com account placeholder\n  - Active account: true\n  ✓ Logged in to github.com account main-placeholder\n  - Active account: false\n';;
 'auth token') echo placeholder-token;;
 'api '*) echo '{{"permissions":{{"push":true}}}}';;
 'pr create') touch {marker};;
 'pr list') touch {started}; n=0; while [ ! -f {release} ]; do n=$((n + 1)); [ $n -lt 1000 ] || exit 1; /bin/sleep 0.01; done; if [ -f {marker} ]; then echo '[{{"url":"https://github.com/owner/repo/pull/1","number":1,"headRefOid":"placeholder","isDraft":true}}]'; else echo '[]'; fi;;
esac
"#,
        trace = q(trace.to_str().unwrap()),
        marker = q(marker.to_str().unwrap()),
        started = q(started.to_str().unwrap()),
        release = q(release.to_str().unwrap())
    );
    let gh = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/bin/gh");
    std::fs::write(&gh, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let linked = repo_fx.root.join("ordinary-linked");
    git(
        &repo_fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "ordinary-linked",
            linked.to_str().unwrap(),
            "main",
        ],
    );
    golive10_workspace(&fixture, &linked, json!([]), json!([]));
    let mut workspaces: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.iced.join("workspaces.json")).unwrap())
            .unwrap();
    workspaces["workspaces"][0]["env_overrides"]["GITTERM_GH_ACCOUNT"] = json!("placeholder");
    let mut main_workspace = workspaces["workspaces"][0].clone();
    main_workspace["name"] = json!("Main Placeholder");
    main_workspace["dir"] = json!(repo_fx.repo);
    main_workspace["env_overrides"]["GITTERM_GH_ACCOUNT"] = json!("main-placeholder");
    workspaces["workspaces"]
        .as_array_mut()
        .unwrap()
        .push(main_workspace);
    std::fs::write(
        fixture.iced.join("workspaces.json"),
        serde_json::to_vec(&workspaces).unwrap(),
    )
    .unwrap();
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &linked);
    let id = created["task_id"].as_str().unwrap();
    host.update(cx, |h, _| {
        let store = h.host_mut().store_mut().as_mut().unwrap();
        let mut task = store.get(id).unwrap().clone();
        task.issue = None;
        store.replace(task).unwrap();
    });
    host.update(cx, |h, _| h.pause_live_worker_tick());
    let panel = panel_new(&host, cx);
    // Acknowledge the shell boundary here; F1 exercises real pane closures.
    let _stop_subscription = cx.update(|cx| {
        cx.subscribe(&host, |host, stop: &crate::task_host::StopTask, cx| {
            let result = host.update(cx, |h, cx| h.panel_stop_completed(&stop.task_id, cx));
            stop.complete(result);
        })
    });
    let inputs = panel_inputs(&repo_fx.repo);
    for action in [
        gitterm_core::task_commands::TaskPublish::PushBranch,
        gitterm_core::task_commands::TaskPublish::OpenDraftPr,
    ] {
        let stopping = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
            "title":format!("Placeholder stopping {action:?}"), "objective":"Placeholder objective", "repository_path":repo_fx.repo
        })))).unwrap()["task_id"].as_str().unwrap().to_owned();
        let queued = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({
            "title":format!("Placeholder queued {action:?}"), "objective":"Placeholder objective", "repository_path":repo_fx.repo
        })))).unwrap()["task_id"].as_str().unwrap().to_owned();
        host.update(cx, |h, _| {
            h.host_mut()
                .store_mut()
                .as_mut()
                .unwrap()
                .record_lifecycle_signal(
                    &queued,
                    TaskLifecycle::Queued,
                    None,
                    "2026-11-20T12:00:00Z",
                )
                .unwrap();
            h.host_mut()
                .launch_queue_mut()
                .push(gitterm_core::task_host::QueuedTaskLaunch {
                    task_id: queued.clone(),
                    preset_idx: Some(1),
                    origin: TaskLaunchOrigin::User,
                    user_model_choice: false,
                });
        });
        panel_write(
            &panel,
            PanelWrite::Publish {
                task_id: id.into(),
                action,
            },
            &inputs,
            cx,
        );
        assert!(panel.read_with(cx, |p, _| matches!(
            p.modal,
            Some(PanelModal::Publish { .. })
        )));
        assert!(
            !trace.exists()
                || !std::fs::read_to_string(&trace)
                    .unwrap()
                    .contains("pr create")
        );
        panel_write(&panel, PanelWrite::PublishConfirm, &inputs, cx);
        wait_until(cx, |_| started.exists());
        let open = &inputs;
        panel_write(&panel, PanelWrite::CancelQueue(queued.clone()), open, cx);
        assert_eq!(
            panel_record(&host, &queued, cx).lifecycle,
            TaskLifecycle::Ready,
            "Cancel queue must run while {action:?} is pending"
        );
        panel_write(&panel, PanelWrite::Stop(stopping.clone()), open, cx);
        wait_until(cx, |cx| {
            cx.executor().advance_clock(Duration::from_millis(20));
            panel_record(&host, &stopping, cx).lifecycle == TaskLifecycle::Stopped
        });
        assert!(
            panel.read_with(cx, |p, _| matches!(
                &p.modal, Some(PanelModal::Publish { task_id, .. }) if task_id == id
            )),
            "Other tasks' completions must preserve the pending publication modal"
        );
        assert!(host.read_with(cx, |h, cx| {
            h.panel_facts(cx).publish_busy.contains(&id.to_owned())
        }));
        assert!(!release.exists());
        std::fs::write(&release, "placeholder release").unwrap();

        wait_until(cx, |cx| {
            host.read_with(cx, |h, cx| h.panel_facts(cx).publish_busy.is_empty())
        });
        std::fs::remove_file(&started).unwrap();
        std::fs::remove_file(&release).unwrap();
        assert!(
            panel.read_with(cx, |p, _| p.notice.is_none()),
            "{:?}",
            panel.read_with(cx, |p, _| p.notice.clone())
        );
    }
    let saved = panel_record(&host, id, cx);
    assert!(saved.delivery.commit_sha.is_some());
    assert_eq!(saved.delivery.pr_number, Some(1));
    assert!(git(&bare, &["rev-parse", &saved.branch]).len() >= 40);
    let trace = std::fs::read_to_string(trace).unwrap();
    assert!(
        trace.contains("auth token --hostname github.com --user placeholder"),
        "{trace}"
    );
    assert!(
        trace.contains("|placeholder-token|pr create --draft --head"),
        "{trace}"
    );
    assert!(trace.contains("--base main --title"), "{trace}");
    host.update(cx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_delete_inspection_guards_remove_and_clear(cx: &mut TestAppContext) {
    if child("task_panel_live_delete_inspection_guards_remove_and_clear") {
        return;
    }
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &repo_fx.repo);
    let id = created["task_id"].as_str().unwrap();
    let panel = panel_new(&host, cx);
    let inputs = panel_inputs(&repo_fx.repo);
    let path = panel_record(&host, id, cx).worktree.path.unwrap();
    let inspect = |cx: &mut TestAppContext| {
        panel.update(cx, |p, cx| p.open_delete(id, &inputs, cx));
        wait_until(cx, |cx| {
            panel.read_with(cx, |p, _| {
                matches!(
                    p.modal,
                    Some(PanelModal::Delete {
                        inspection: Some(_),
                        ..
                    })
                )
            })
        });
    };
    std::fs::write(path.join("unsaved"), "placeholder").unwrap();
    inspect(cx);
    panel_write(&panel, PanelWrite::DeleteConfirm, &inputs, cx);
    assert!(path.is_dir());
    assert!(panel_record(&host, id, cx).worktree.path.is_some());
    std::fs::remove_file(path.join("unsaved")).unwrap();
    git(
        &path,
        &["commit", "--allow-empty", "-qm", "Placeholder unpushed"],
    );
    inspect(cx);
    panel_write(&panel, PanelWrite::DeleteConfirm, &inputs, cx);
    assert!(path.is_dir());
    // Bring the branch back to its base; then the same confirmed effect removes it.
    git(
        &path,
        &[
            "reset",
            "--hard",
            created["base"]["commit"].as_str().unwrap(),
        ],
    );
    inspect(cx);
    panel_write(&panel, PanelWrite::DeleteConfirm, &inputs, cx);
    wait_until(cx, |cx| {
        assert!(
            panel.read_with(cx, |p, _| p.notice.is_none()),
            "{:?}",
            panel.read_with(cx, |p, _| p.notice.clone())
        );
        panel_record(&host, id, cx).worktree.path.is_none()
    });
    wait_until(cx, |cx| !panel.read_with(cx, |p, _| p.modal_open()));
    assert!(!path.exists());
    assert!(
        git(
            &repo_fx.repo,
            &["branch", "--list", &panel_record(&host, id, cx).branch]
        )
        .contains("task/")
    );
    // Missing paths clear metadata without invoking git removal.
    let created = dispatch(cx, &sender, TaskControlOperation::Create(req(json!({"title":"Placeholder other task", "objective":"Placeholder objective", "repository_path":repo_fx.repo})))).unwrap();
    let other = created["task_id"].as_str().unwrap();
    let other_path = panel_record(&host, other, cx).worktree.path.unwrap();
    git(
        &repo_fx.repo,
        &["worktree", "remove", other_path.to_str().unwrap()],
    );
    panel.update(cx, |p, cx| p.open_delete(other, &inputs, cx));
    wait_until(cx, |cx| {
        panel.read_with(cx, |p, _| {
            matches!(
                p.modal,
                Some(PanelModal::Delete {
                    inspection: Some(_),
                    ..
                })
            )
        })
    });
    panel_write(&panel, PanelWrite::DeleteConfirm, &inputs, cx);
    wait_until(cx, |cx| {
        panel_record(&host, other, cx).worktree.path.is_none()
    });
    host.update(cx, |h, cx| h.quit(cx));
}

#[gpui::test]
fn task_panel_live_review_lands_in_task_session_with_git_changes(cx: &mut TestAppContext) {
    if child("task_panel_live_review_lands_in_task_session_with_git_changes") {
        return;
    }
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, repo_fx) = fixture();
    golive10_workspace(
        &fixture,
        &repo_fx.repo,
        json!([{"dir":repo_fx.repo,"session_uid":"placeholder-home"}]),
        json!([]),
    );
    let (host, sender) = start(&fixture, cx);
    let created = create(cx, &sender, &repo_fx.repo);
    let id = created["task_id"].as_str().unwrap();
    let path = panel_record(&host, id, cx).worktree.path.unwrap();
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    let panel = shell.update_in(vcx, |s, w, cx| s.ensure_task_panel(w, cx));
    panel.update(vcx, |_, cx| {
        cx.emit(crate::task_panel::TaskPanelOutcome::Review(id.into()))
    });
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| s.task_inputs(cx).open_session(id).is_some())
    });
    shell.read_with(vcx, |s, cx| {
        assert_eq!(s.selected_task.as_deref(), Some(id));
        assert_eq!(
            s.active(cx).store.regions.sidebar_mode,
            crate::layout::SidebarMode::Git
        );
        let pane =
            &s.active(cx).panes[&format!("workspace-{}-git", s.active(cx).workspace.layout_key)];
        assert!(
            pane.read(cx)
                .git
                .as_ref()
                .unwrap()
                .read(cx)
                .changes_selected()
        );
        assert_eq!(s.owner.read(cx).model.workspaces[0].root, path);
    });
    std::fs::write(path.join("README"), "placeholder-task-change\n").unwrap();
    std::fs::write(repo_fx.repo.join("README"), "placeholder-main-change\n").unwrap();
    shell.update(vcx, |s, cx| {
        s.owner.update(cx, |o, cx| {
            o.dispatch(
                crate::workspace_status::Intent::EnterGit,
                Instant::now(),
                cx,
            )
        })
    });
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| {
            s.owner
                .read(cx)
                .model
                .status_view(s.active(cx).workspace.id)
                .files
                .iter()
                .any(|f| f.path == "README")
        })
    });
    shell.update_in(vcx, |s, window, cx| {
        let workspace = s.active(cx).workspace.id;
        s.open_review(
            &crate::FileOpen {
                workspace,
                path: path.join("README"),
                changed: Some(crate::data::ChangedFile {
                    path: "README".into(),
                    status: "M".into(),
                    staged: false,
                }),
                from_files: false,
            },
            window,
            cx,
        );
    });
    wait_until(vcx, |cx| {
        shell.read_with(cx, |s, cx| {
            s.owner.read(cx).reviews.values().any(|v| {
                v.read(cx).model().files.iter().any(|f| {
                    f.patch.contains("placeholder-task-change")
                        && !f.patch.contains("placeholder-main-change")
                })
            })
        })
    });
    let count = panel_record(&host, id, vcx).sessions.len();
    panel.update(vcx, |_, cx| {
        cx.emit(crate::task_panel::TaskPanelOutcome::Review(id.into()))
    });
    vcx.run_until_parked();
    assert_eq!(
        panel_record(&host, id, vcx).sessions.len(),
        count,
        "review focuses the existing session"
    );
    shell.update_in(vcx, |s, window, cx| {
        let workspace = s.active(cx).workspace.id;
        let general = s
            .active(cx)
            .sessions
            .iter()
            .find(|session| s.session_task(session.id, cx).is_none())
            .unwrap()
            .id;
        s.select_host_session(workspace, general, window, cx);
    });
    assert_eq!(
        shell.read_with(vcx, |s, cx| s.owner.read(cx).model.workspaces[0]
            .root
            .clone()),
        repo_fx.repo
    );
    host.update(vcx, |h, cx| h.quit(cx));
}
