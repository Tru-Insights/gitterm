//! H8 exercises the mounted host with isolated, read-only Iced fixtures.
use super::*;
use gitterm_core::tasks::*;
use gpui::{TestAppContext, VisualTestContext};
use std::path::PathBuf;

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|w, cx| w.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn key(cx: &mut VisualTestContext, key: &str) {
    cx.simulate_keystrokes(key);
    draw(cx);
}
fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut result = vec![];
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(snapshot(&path));
        } else {
            result.push((path.clone(), std::fs::read(path).unwrap()));
        }
    }
    result.sort();
    result
}

struct Fixture {
    root: tempfile::TempDir,
    source: PathBuf,
    recent_new: PathBuf,
    recent_old: PathBuf,
    before: Vec<(PathBuf, Vec<u8>)>,
}
fn fixture(
    cx: &mut TestAppContext,
    open_attention: bool,
) -> (Fixture, Entity<Shell>, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::apply_settings(&workspace_config::IcedSettings::default(), None, cx);
        install_app_menus(cx);
    });
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    let work = root.path().join("work");
    let other = root.path().join("other");
    let recent_new = root.path().join("recent-new");
    let recent_old = root.path().join("recent-old");
    for dir in [&source, &work, &other, &recent_new, &recent_old] {
        std::fs::create_dir(dir).unwrap();
    }
    for dir in [&work, &other, &recent_new, &recent_old] {
        std::fs::create_dir(dir.join(".git")).unwrap();
    }
    std::fs::write(source.join("workspaces.json"), serde_json::json!({
        "active_workspace": 0,
        "workspaces": [
            {"name":"A","abbrev":"A","color":"blue","dir":work,"repos":[recent_old],"active_tab":1,"tabs":[
                {"dir":work,"session_uid":"general","startup_command":"MUST NOT RUN"},
                {"dir":work,"session_uid":"parent","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"parent-history"},
                {"dir":work,"session_uid":"child-one","task_id":"open","task_session_id":"child-one"},
                {"dir":work,"session_uid":"child-two","task_id":"open","task_session_id":"child-two"}
            ]},
            {"name":"B","abbrev":"B","color":"green","dir":other,"chats":["owned"],"tabs":[
                {"dir":other,"session_uid":"other-general"},
                {"dir":other,"session_uid":"other-child","task_id":"other","task_session_id":"other-child"}
            ]}
        ]
    }).to_string()).unwrap();
    let make = |id: &str, path: &Path, attention| {
        let mut task = tasks::fixture_task(id, &format!("Task {id}"), &format!("task/{id}"));
        task.workspace.location = WorkspaceLocationIdentity::Local {
            directory: path.into(),
        };
        task.repository.common_dir = path.join(".git");
        task.lifecycle = if attention {
            TaskLifecycle::WaitingForInput
        } else {
            TaskLifecycle::Ready
        };
        if attention {
            task.attempts.push(TaskExecutionAttempt {
                attempt_id: format!("attempt-{id}"),
                executor: ExecutorTarget::Local,
                harness: None,
                state: AttemptState::WaitingForInput,
                started_at: task.created_at.clone(),
                ended_at: None,
                session_ref: None,
                failure: None,
            });
            task.active_attempt_id = Some(format!("attempt-{id}"));
        }
        task.attention.reason = attention.then_some(TaskAttentionReason::RequiresInput);
        task
    };
    let mut open = make("open", &work, open_attention);
    open.sessions = ["child-one", "child-two"]
        .into_iter()
        .map(|id| TaskSessionRecord {
            task_session_id: id.into(),
            label: id.into(),
            harness: None,
            conversation: None,
            objective_delivery: ObjectiveDeliveryState::Unknown,
            launch_identity: None,
            created_at: open.created_at.clone(),
            updated_at: open.updated_at.clone(),
        })
        .collect();
    let worker: Delegation = serde_json::from_value(serde_json::json!({
        "delegation_id":"worker", "kind":"implement", "parent":{"session_uid":"parent","chat_session_id":"parent-history","workspace":"A","cwd":work},
        "child":{"runner":"task_session","task_id":"open","task_session_id":"child-two","preset_name":"Codex","model":"gpt-6.1-sol"},
        "brief":"Implement fixture", "status":{"state":"running"},
        "created_at":"2026-11-20T12:00:00Z","updated_at":"2026-11-20T12:01:00Z"
    })).unwrap();
    let mut closed_first = make("closed-first", &work, true);
    closed_first.updated_at = "2026-01-01T00:00:00Z".into();
    let mut closed_second = make("closed-second", &work, true);
    closed_second.updated_at = "2026-11-20T12:00:00Z".into();
    let document = TaskStoreDocument {
        tasks: vec![
            closed_first,
            closed_second,
            open,
            make("other", &other, open_attention),
        ],
        delegations: vec![worker],
        ..Default::default()
    };
    std::fs::write(
        source.join("tasks.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let home = root.path().join("fixture-home");
    let projects = home.join(".claude/projects/fixture");
    std::fs::create_dir_all(&projects).unwrap();
    for (id, dir, stamp) in [
        ("old", &recent_old, "2026-11-20T12:00:00Z"),
        ("new", &recent_new, "2026-11-20T13:00:00Z"),
    ] {
        std::fs::write(projects.join(format!("{id}.jsonl")), serde_json::json!({"type":"user","cwd":dir,"timestamp":stamp,"message":{"content":"Fixture request"}}).to_string()).unwrap();
    }
    let before = snapshot(&source);
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    store.chats.push(chat::SavedChat {
        uid: "iced-parent-history".into(),
        cwd: work,
        provider: store.guard.as_ref().unwrap().preview().provider(),
        session_id: Some("parent-history".into()),
        records: vec![],
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        let shell = cx.new(|cx| {
            let mut shell = Shell::with_owner(
                Rc::new(data::LocalData(owner.clone())),
                owner,
                &layout,
                true,
                window,
                cx,
            )
            .unwrap();
            shell.ensure_work_sidebars(window, cx);
            for state in &shell.workspaces {
                let id = state.workspace.id;
                let saved = shell.owner.read(cx).iced_workspace(id).unwrap();
                let data = data::IcedWorkData::read_with_home(
                    &source,
                    Some(&home),
                    id.0,
                    saved,
                    Default::default(),
                )
                .unwrap();
                shell.work_sidebars[&id].update(cx, |sidebar, _| {
                    work_sidebar::tests::h8_install(sidebar, data)
                });
            }
            shell
        });
        Root::new(shell, window, cx)
    });
    let shell = view.read_with(cx, |root, _| {
        root.view().clone().downcast::<Shell>().unwrap()
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    cx.update(|window, cx| shell.focus_handle(cx).focus(window, cx));
    draw(cx);
    (
        Fixture {
            root,
            source,
            recent_new,
            recent_old,
            before,
        },
        shell,
        cx,
    )
}

#[gpui::test]
fn cmd_t_escape_activate_overview_and_context_back_are_separate(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, true);
    let focus = cx.update(|window, cx| window.focused(cx).unwrap());
    key(cx, "cmd-t");
    assert!(cx.debug_bounds("task-switcher").is_some());
    cx.update(|window, cx| {
        let context = shell.update(cx, |s, cx| s.menu_context(window, cx));
        assert!(context.contains("IcedTaskSwitcher"));
        assert!(!context.contains("IcedMain"));
        assert!(!context.contains("IcedChat"));
    });
    let dark = cx.read(|cx| gpui_component::Theme::global(cx).mode.is_dark());
    key(cx, "cmd-shift-t");
    cx.read(|cx| assert_eq!(gpui_component::Theme::global(cx).mode.is_dark(), dark));
    assert!(cx.debug_bounds("task-switcher").is_some());
    let active = cx.read(|cx| shell.read(cx).active_workspace(cx));
    key(cx, "ctrl-1");
    cx.read(|cx| assert_eq!(shell.read(cx).active_workspace(cx), active));
    key(cx, "escape");
    assert!(cx.debug_bounds("task-switcher").is_none());
    cx.update(|w, _| assert!(focus.is_focused(w)));
    key(cx, "cmd-t");
    cx.simulate_input("Task open");
    draw(cx);
    key(cx, "enter");
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(s.selected_task.as_deref(), Some("open"));
        assert_eq!(
            s.session_tab(s.active(cx).active_session.unwrap(), cx)
                .unwrap()
                .session_uid
                .as_deref(),
            Some("child-two")
        );
        assert_eq!(s.strip_input(cx).sessions.len(), 2);
    });
    key(cx, "cmd-1");
    key(cx, "cmd-up");
    key(cx, "cmd-t");
    cx.simulate_input("Task open");
    draw(cx);
    key(cx, "enter");
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(
            s.session_tab(s.active(cx).active_session.unwrap(), cx)
                .unwrap()
                .task_session_id
                .as_deref(),
            Some("child-one")
        );
        assert_eq!(s.context_mru, [Some("open".into()), None]);
    });
    key(cx, "cmd-0");
    assert!(cx.debug_bounds("task-overview").is_some());
    cx.read(|cx| assert_eq!(shell.read(cx).selected_task.as_deref(), Some("open")));
    key(cx, "cmd-up");
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(s.selected_task, None);
        assert_eq!(s.strip_input(cx).sessions.len(), 2);
        assert_eq!(s.context_mru[0], None);
    });
    assert_eq!(snapshot(&fixture.source), fixture.before);
    assert!(fixture.root.path().join("preview").is_dir());
}

#[gpui::test]
fn new_work_button_follows_chat_recency(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, false);
    // ⌥⇧T is Iced's folder picker now (TRU-274), so only the button opens the sheet.
    for _ in 0..2 {
        click(cx, "pstrip-new-work");
        cx.read(|cx| {
            let s = shell.read(cx);
            let recent = &s.new_work_sheet.as_ref().unwrap().read(cx).draft().recent;
            assert_eq!(
                &recent[..2],
                &[fixture.recent_new.clone(), fixture.recent_old.clone()]
            );
        });
        click(cx, "workspace-sheet-recent-0");
        cx.read(|cx| {
            assert_eq!(
                shell
                    .read(cx)
                    .new_work_sheet
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .draft()
                    .repos
                    .as_slice(),
                std::slice::from_ref(&fixture.recent_new)
            )
        });
        key(cx, "escape");
    }
    assert_eq!(snapshot(&fixture.source), fixture.before);
}

#[gpui::test]
fn roster_periodic_read_finds_new_workers_and_preserves_local_dismissal(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, false);
    let path = fixture.source.join("tasks.json");
    let mut document: TaskStoreDocument =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut worker = document.delegations[0].clone();
    worker.delegation_id = "new-worker".into();
    document.delegations.push(worker);
    let task = document
        .tasks
        .iter_mut()
        .find(|task| task.task_id == "open")
        .unwrap();
    task.worktree.state = TaskWorktreeState::Ready;
    task.worktree.path = Some(fixture.root.path().join("worker-worktree"));
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(15));
    draw(cx);
    assert!(cx.debug_bounds("roster-dlg:new-worker").is_some());
    cx.read(|cx| {
        let shell = shell.read(cx);
        let facts = shell.roster_facts.as_ref().unwrap();
        let rows = roster_entries(&facts.tasks, &facts.workers, |worker| {
            shell.worker_tab(worker, cx)
        });
        assert!(
            rows.iter()
                .all(|row| Path::new(&row.checkout).file_name().unwrap() == "worker-worktree")
        );
    });
    // Iced hides the × on a live row and shows it once the worker finished.
    assert!(cx.debug_bounds("roster-dismiss-dlg:new-worker").is_none());
    let mut document: TaskStoreDocument =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for worker in &mut document.delegations {
        if worker.delegation_id == "new-worker" {
            worker.status = DelegationStatus::Failed {
                message: "fixture".into(),
            };
        }
    }
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let before = snapshot(&fixture.source);
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(15));
    draw(cx);
    click(cx, "roster-dismiss-dlg:new-worker");
    assert!(cx.debug_bounds("roster-dlg:new-worker").is_none());
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(15));
    draw(cx);
    assert!(cx.debug_bounds("roster-dlg:new-worker").is_none());
    assert_eq!(snapshot(&fixture.source), before);
}

#[gpui::test]
fn roster_is_fed_from_worker_fixture_and_row_click_stays_in_place(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, true);
    assert!(cx.debug_bounds("roster-dlg:worker").is_some());
    cx.read(|cx| {
        let s = shell.read(cx);
        let sidebar = s.work_sidebars[&WorkspaceId(1)].read(cx);
        let rows = roster_entries(sidebar.tasks(), sidebar.workers(), |worker| {
            s.worker_tab(worker, cx)
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Worker · Codex");
        assert_eq!(rows[0].model, "gpt-6.1-sol");
        assert_eq!(
            rows[0].state,
            gitterm_gpui_chat::model::roster::WorkerState::Waiting
        );
    });
    let before = cx.read(|cx| {
        let s = shell.read(cx);
        (s.selected_task.clone(), s.active(cx).active_session)
    });
    click(cx, "roster-dlg:worker");
    // Iced's roster row reveals and flashes its transcript card
    // (agent_chat.html revealRosterTarget); it never switches session or task.
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(
            (s.selected_task.clone(), s.active(cx).active_session),
            before
        );
        assert!(s.owner.read(cx).chats.is_empty());
        assert!(
            s.workspaces
                .iter()
                .all(|w| w.store.terminals.as_ref().unwrap().is_empty())
        );
    });
    let message = cx.update(|window, cx| {
        shell.update(cx, |s, cx| {
            s.open_roster("dlg:missing", window, cx);
            let ordinary: Vec<_> = s
                .active(cx)
                .sessions
                .iter()
                .filter(|t| s.session_task(t.id, cx).is_none())
                .map(|t| t.id)
                .collect();
            s.workspaces
                .iter_mut()
                .find(|w| w.workspace.id == WorkspaceId(1))
                .unwrap()
                .sessions
                .retain(|t| ordinary.contains(&t.id));
            s.open_worker_tab("dlg:worker", window, cx)
        })
    });
    draw(cx);
    // H13a: with no open worker session Iced reports it closed/queued; it never
    // opens the task overview or launches anything.
    cx.read(|cx| {
        let s = shell.read(cx);
        assert_eq!(s.selected_task, before.0);
    });
    assert_eq!(
        message.as_deref(),
        Some("The worker's tab is closed. Open its task from the Tasks rail to resume it.")
    );
    assert!(cx.debug_bounds("task-overview").is_none());
    assert_eq!(snapshot(&fixture.source), fixture.before);
}

#[gpui::test]
fn attention_cycles_open_sessions_in_iced_order(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, true);
    for expected in ["child-one", "child-two", "other-child", "child-one"] {
        cx.update(|window, cx| window.dispatch_action(Box::new(app_menus::AttentionJumpNext), cx));
        draw(cx);
        cx.read(|cx| {
            let s = shell.read(cx);
            assert_eq!(
                s.session_tab(s.active(cx).active_session.unwrap(), cx)
                    .unwrap()
                    .session_uid
                    .as_deref(),
                Some(expected)
            );
        });
    }
    assert_eq!(snapshot(&fixture.source), fixture.before);
}

#[gpui::test]
fn attention_without_session_attention_cycles_tasks_in_store_order(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, false);
    for expected in ["closed-first", "closed-second", "closed-first"] {
        cx.update(|window, cx| window.dispatch_action(Box::new(app_menus::AttentionJumpNext), cx));
        draw(cx);
        cx.read(|cx| assert_eq!(shell.read(cx).selected_task.as_deref(), Some(expected)));
        assert!(cx.debug_bounds("task-overview").is_some());
    }
    assert_eq!(snapshot(&fixture.source), fixture.before);
}

#[gpui::test]
fn collapsed_work_sidebar_still_loads_switcher_and_roster_facts(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (fixture, shell, cx) = fixture(cx, true);
    cx.update(|_, cx| {
        shell.update(cx, |s, cx| {
            s.work_sidebars.clear();
            s.work_subscriptions.clear();
            s.roster_observers.clear();
            s.run_sidebar_subscriptions.clear();
            for w in &mut s.workspaces {
                w.store.regions.sidebar_collapsed = true;
            }
            cx.notify();
        })
    });
    draw(cx);
    cx.read(|cx| {
        let s = shell.read(cx);
        assert!(s.work_sidebars.values().all(|v| v.read(cx).has_loaded()));
        assert_eq!(s.switcher_inputs(cx).tasks.len(), 3);
    });
    key(cx, "cmd-t");
    assert!(cx.debug_bounds("task-switcher").is_some());
    key(cx, "escape");
    assert_eq!(snapshot(&fixture.source), fixture.before);
}

#[test]
fn task_detail_uses_iced_lifecycle_and_attention_copy_without_live_guesses() {
    let mut task = tasks::fixture_task("copy", "Copy", "task/copy");
    for (state, expected) in [
        (TaskLifecycle::Ready, "Ready · not started"),
        (TaskLifecycle::Queued, "Queued"),
        (TaskLifecycle::Running, "Working"),
        (TaskLifecycle::WaitingForInput, "Needs you — input required"),
        (TaskLifecycle::Completed, "Done · review"),
        (TaskLifecycle::Failed, "Failed · agent stopped"),
    ] {
        task.lifecycle = state;
        assert_eq!(task_switcher::state_copy(&task, &[], false), expected);
    }
    task.attention.reason = Some(TaskAttentionReason::RemoteUnavailable);
    assert_eq!(
        task_switcher::state_copy(&task, &[], true),
        "Needs you — remote unavailable"
    );
}
