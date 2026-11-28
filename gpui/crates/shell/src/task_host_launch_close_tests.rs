//! Issue 87: an adopted task pane closes and reports as Iced's task tab.
use super::*;
use crate::workspace_bar::AttentionKind;
use gpui::Focusable as _;

/// A shell over `launches` whose first launch, of `task` (Codex), is
/// adopted and started, with its task session.
fn adopted<'a>(
    launches: &Launches,
    task: &str,
    config: &Path,
    cx: &'a mut TestAppContext,
) -> (
    Entity<crate::Shell>,
    &'a mut gpui::VisualTestContext,
    String,
) {
    adopted_in(launches, task, config, false, cx)
}

/// [`adopted`], over a shell whose first pane is a General terminal (as
/// Iced's workspace has) when `general` is set.
fn adopted_in<'a>(
    launches: &Launches,
    task: &str,
    config: &Path,
    general: bool,
    cx: &'a mut TestAppContext,
) -> (
    Entity<crate::Shell>,
    &'a mut gpui::VisualTestContext,
    String,
) {
    let (shell, vcx) = task_shell(launches, config, cx);
    if general {
        shell.update_in(vcx, |shell, window, cx| shell.new_terminal(window, cx));
        vcx.run_until_parked();
    }
    let reply = launches.launch(task, Some("Codex"), vcx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(vcx, |cx| session_holding(&shell, &session, cx).is_some());
    wait_until(vcx, |_| launches.agent.started(1));
    (shell, vcx, session)
}

fn report(launches: &Launches, task: &str, session: &str, status: &str, cx: &mut TestAppContext) {
    dispatch_as(
        cx,
        &launches.runs.sender,
        TaskControlOperation::UpdateHandoff(req(serde_json::json!({
            "task_id": task, "summary": "Report", "status": status
        }))),
        Some(session),
    )
    .unwrap();
}

/// Gap 1, Iced's `TabClose` → `TaskTabClosePrompt` → `close_task_tab`
/// (src/main.rs): closing a live agent's pane asks first with Iced's
/// prompt; Stop and close ends the session, hangs up and reaps its child,
/// and the task, with no other live session, stops.
#[gpui::test]
fn closing_an_adopted_task_pane_stops_its_task(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted(&launches, &task, config.path(), cx);
    let (pid, descendant) = (launches.agent.pid(1), launches.agent.descendant(1));
    assert_eq!(
        stored_task(&launches.runs.owner, &task, vcx).lifecycle,
        TaskLifecycle::Running
    );
    let id = session_holding(&shell, &session, vcx).unwrap();
    shell.update_in(vcx, |shell, window, cx| {
        shell.strip_outcome(crate::session_strip::Outcome::CloseSession(id), window, cx)
    });
    let panel = vcx.read(|cx| shell.read(cx).task_panel.clone().unwrap());
    let asked = panel.read_with(vcx, |panel, _| {
        matches!(&panel.modal, Some(crate::task_panel::Modal::StopAndClose { target, .. })
            if target.task_session_id.as_deref() == Some(session.as_str()))
    });
    assert!(asked);
    assert!(session_holding(&shell, &session, vcx).is_some());
    assert!(exists(&pid));

    let inputs = vcx.read(|cx| shell.read(cx).task_inputs(cx));
    panel.update(vcx, |panel, cx| {
        panel.write(crate::task_panel::Write::StopAndClose, &inputs, cx)
    });
    vcx.run_until_parked();
    assert!(session_holding(&shell, &session, vcx).is_none());
    assert!(!panel.read_with(vcx, |panel, _| panel.modal_open()));
    wait_until(vcx, |_| !exists(&pid) && !exists(&descendant));
    assert_eq!(
        launches.saved().get(&task).unwrap().lifecycle,
        TaskLifecycle::Stopped
    );
    let tracked = launches
        .runs
        .owner
        .read_with(vcx, |owner, _| owner.launched_live(&session));
    assert_eq!(tracked, None);
}

/// Gap 2, Iced's `worker_reported` and `TurnEnded` arms
/// (`set_attention(HumanInputRequired)`): a worker reporting blocked, or
/// its turn ending, gives its pane the needs-you attention; a keystroke
/// into the terminal clears it, as Iced's input does.
#[gpui::test]
fn a_blocked_worker_pane_needs_the_user_until_typed_into(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, "gone"))
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted(&launches, &task, config.path(), cx);
    shell.update(vcx, |shell, _| shell.selected_task = Some(task.clone()));
    let id = session_holding(&shell, &session, vcx).unwrap();
    let attention = |cx: &mut gpui::VisualTestContext| {
        cx.read(|cx| {
            shell
                .read(cx)
                .strip_input(cx)
                .sessions
                .iter()
                .find(|strip| strip.id == id)
                .and_then(|strip| strip.attention)
        })
    };
    assert_eq!(attention(vcx), None);
    report(&launches, &task, &session, "blocked", vcx);
    assert_eq!(attention(vcx), Some(AttentionKind::NeedsYou));

    let pane = vcx.read(|cx| shell.read(cx).active(cx).panes[&crate::session_pane_id(id)].clone());
    vcx.update(|window, cx| pane.read(cx).focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    vcx.simulate_keystrokes("x");
    assert_eq!(attention(vcx), None);

    dispatch_as(
        vcx,
        &launches.runs.sender,
        TaskControlOperation::SessionEvent(gitterm_core::task_mcp::TaskSessionEventRequest {
            task_id: task.clone(),
            session_id: session.clone(),
            event_type: "agent-turn-complete".into(),
        }),
        None,
    )
    .unwrap();
    assert_eq!(attention(vcx), Some(AttentionKind::NeedsYou));
    // Tab is sent by its own action, not the key handler; it is input too.
    vcx.simulate_keystrokes("tab");
    assert_eq!(attention(vcx), None);
    launches.kill(1);
}

/// Gap 3, Iced's `close_final_workers` on the Tick after delivery: a
/// worker's final session closes once its result went out (here its
/// requesting chat is not open, so it is never sent automatically); its
/// pane closes, its child is reaped and its task ends Completed. Before
/// the worker reports, the tick closes nothing.
#[gpui::test]
fn a_final_worker_pane_closes_after_its_result_went_out(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, "gone"))
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted_in(&launches, &task, config.path(), true, cx);
    let (pid, descendant) = (launches.agent.pid(1), launches.agent.descendant(1));
    vcx.executor()
        .advance_clock(crate::task_host::DELIVERY_POLL);
    vcx.run_until_parked();
    assert!(session_holding(&shell, &session, vcx).is_some());
    // Iced's `FinalWorkerStopped` keeps another active tab and the selection.
    let general = session_tasks(&shell, vcx)[0].0;
    shell.update_in(vcx, |shell, window, cx| {
        shell.strip_outcome(
            crate::session_strip::Outcome::SelectSession(general),
            window,
            cx,
        )
    });

    report(&launches, &task, &session, "done", vcx);
    tick_until(vcx, |cx| session_holding(&shell, &session, cx).is_none());
    assert_eq!(landed(&shell, vcx), (Some(general), true));
    assert_eq!(vcx.read(|cx| shell.read(cx).selected_task.clone()), None);
    wait_until(vcx, |_| !exists(&pid) && !exists(&descendant));
    assert_eq!(
        launches.saved().get(&task).unwrap().lifecycle,
        TaskLifecycle::Completed
    );
    let tracked = launches
        .runs
        .owner
        .read_with(vcx, |owner, _| owner.launched_live(&session));
    assert_eq!(tracked, None);
}

/// Launches a plain terminal session of `task` and waits for its pane.
fn open(
    launches: &Launches,
    shell: &Entity<crate::Shell>,
    task: &str,
    cx: &mut gpui::VisualTestContext,
) -> crate::data::SessionId {
    let reply = launches.launch(task, None, cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |cx| session_holding(shell, &session, cx).is_some());
    session_holding(shell, &session, cx).unwrap()
}

/// The active workspace's sessions, each with its task.
fn session_tasks(
    shell: &Entity<crate::Shell>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<(crate::data::SessionId, Option<String>)> {
    cx.read(|cx| {
        let shell = shell.read(cx);
        shell
            .active(cx)
            .sessions
            .iter()
            .map(|s| (s.id, shell.session_task(s.id, cx)))
            .collect()
    })
}

/// The active session, and whether the dock shows its pane.
fn landed(
    shell: &Entity<crate::Shell>,
    cx: &mut gpui::VisualTestContext,
) -> (Option<crate::data::SessionId>, bool) {
    cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        let active = state.active_session;
        let shown =
            active.is_some_and(|id| crate::pane_id_visible(state, &crate::session_pane_id(id), cx));
        (active, shown)
    })
}

/// Selects `id` from the strip, closes it, and answers Iced's running-task
/// prompt with Stop and close.
fn stop_and_close(
    shell: &Entity<crate::Shell>,
    id: crate::data::SessionId,
    cx: &mut gpui::VisualTestContext,
) {
    shell.update_in(cx, |shell, window, cx| {
        shell.strip_outcome(crate::session_strip::Outcome::SelectSession(id), window, cx)
    });
    assert_eq!(landed(shell, cx), (Some(id), true));
    shell.update_in(cx, |shell, window, cx| {
        shell.strip_outcome(crate::session_strip::Outcome::CloseSession(id), window, cx)
    });
    let panel = cx.read(|cx| shell.read(cx).task_panel.clone().unwrap());
    let inputs = cx.read(|cx| shell.read(cx).task_inputs(cx));
    panel.update(cx, |panel, cx| {
        assert!(matches!(
            panel.modal,
            Some(crate::task_panel::Modal::StopAndClose { .. })
        ));
        panel.write(crate::task_panel::Write::StopAndClose, &inputs, cx)
    });
    cx.run_until_parked();
}

/// Issue 93, Iced's `close_task_tab` (src/main.rs): closing a task pane
/// lands on the first remaining pane of its task, which the strip makes
/// active and the dock shows.
#[gpui::test]
fn closing_a_task_pane_lands_on_its_sibling(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let (a, b) = (launches.create(cx), launches.create(cx));
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted_in(&launches, &a, config.path(), true, cx);
    let a1 = session_holding(&shell, &session, vcx).unwrap();
    let b1 = open(&launches, &shell, &b, vcx);
    let a2 = open(&launches, &shell, &a, vcx);
    let sessions = session_tasks(&shell, vcx);
    assert_eq!(sessions.len(), 4);
    assert_eq!(sessions[0].1, None);
    assert_eq!(
        sessions[1..].to_vec(),
        vec![(a1, Some(a.clone())), (b1, Some(b)), (a2, Some(a.clone()))]
    );

    stop_and_close(&shell, a1, vcx);
    assert!(session_holding(&shell, &session, vcx).is_none());
    assert_eq!(landed(&shell, vcx), (Some(a2), true));
    assert_eq!(vcx.read(|cx| shell.read(cx).selected_task.clone()), Some(a));
    launches.kill(1);
}

/// Issue 93, Iced's `close_task_tab` with no sibling: the first non-task
/// pane becomes active and shown, the Tasks rail opens and the task stays
/// selected (its overview).
#[gpui::test]
fn closing_a_task_s_last_pane_lands_on_its_overview(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let (a, b) = (launches.create(cx), launches.create(cx));
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted_in(&launches, &a, config.path(), true, cx);
    let a1 = session_holding(&shell, &session, vcx).unwrap();
    open(&launches, &shell, &b, vcx);
    let general = session_tasks(&shell, vcx)[0].0;

    stop_and_close(&shell, a1, vcx);
    assert_eq!(landed(&shell, vcx), (Some(general), true));
    vcx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.selected_task, Some(a.clone()));
        assert_eq!(
            shell.active(cx).store.regions.sidebar_mode,
            crate::layout::SidebarMode::Tasks
        );
    });
    launches.kill(1);
}

/// With only other tasks' panes left (no non-task pane), Iced lands on its
/// first tab (`unwrap_or(0)`); the Tasks rail opens with the task selected.
#[gpui::test]
fn closing_a_task_s_last_pane_with_no_general_pane_lands_on_the_first(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let (a, b) = (launches.create(cx), launches.create(cx));
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted_in(&launches, &a, config.path(), false, cx);
    let a1 = session_holding(&shell, &session, vcx).unwrap();
    let b1 = open(&launches, &shell, &b, vcx);
    assert!(
        session_tasks(&shell, vcx)
            .iter()
            .all(|(_, task)| task.is_some())
    );

    stop_and_close(&shell, a1, vcx);
    assert_eq!(landed(&shell, vcx), (Some(b1), true));
    vcx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.selected_task, Some(a.clone()));
        assert_eq!(
            shell.active(cx).store.regions.sidebar_mode,
            crate::layout::SidebarMode::Tasks
        );
    });
    launches.kill(1);
}

/// A shell of three plain terminal panes, the first of them selected.
fn three_normal_panes<'a>(
    launches: &Launches,
    config: &Path,
    cx: &'a mut TestAppContext,
) -> (
    Entity<crate::Shell>,
    &'a mut gpui::VisualTestContext,
    [crate::data::SessionId; 3],
) {
    let (shell, vcx) = task_shell(launches, config, cx);
    for _ in 0..3 {
        shell.update_in(vcx, |shell, window, cx| shell.new_terminal(window, cx));
        vcx.run_until_parked();
    }
    let ids: Vec<_> = session_tasks(&shell, vcx)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let ids: [crate::data::SessionId; 3] = ids[ids.len() - 3..].try_into().unwrap();
    (shell, vcx, ids)
}

fn select_and_close(
    shell: &Entity<crate::Shell>,
    select: crate::data::SessionId,
    close: crate::data::SessionId,
    cx: &mut gpui::VisualTestContext,
) {
    shell.update_in(cx, |shell, window, cx| {
        shell.strip_outcome(
            crate::session_strip::Outcome::SelectSession(select),
            window,
            cx,
        );
        shell.strip_outcome(
            crate::session_strip::Outcome::CloseSession(close),
            window,
            cx,
        )
    });
    cx.run_until_parked();
}

/// Issue 97, Iced's `Event::TabClose` (src/main.rs): closing the active
/// normal pane keeps its index, so the pane to its right is active and shown.
#[gpui::test]
fn closing_the_middle_normal_pane_lands_on_the_right_one(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, [_, b, c]) = three_normal_panes(&launches, config.path(), cx);
    select_and_close(&shell, b, b, vcx);
    assert_eq!(landed(&shell, vcx), (Some(c), true));
}

/// Closing the last normal pane lands on the new last one.
#[gpui::test]
fn closing_the_last_normal_pane_lands_on_the_new_last(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, [_, b, c]) = three_normal_panes(&launches, config.path(), cx);
    select_and_close(&shell, c, c, vcx);
    assert_eq!(landed(&shell, vcx), (Some(b), true));
}

/// Closing a normal pane that is not active leaves the active pane active
/// and shown. (Iced keeps the active *index*, so closing a tab left of the
/// active one shifts its active tab one to the right; GPUI keeps the pane.)
#[gpui::test]
fn closing_an_inactive_normal_pane_keeps_the_active_one(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, [a, b, c]) = three_normal_panes(&launches, config.path(), cx);
    select_and_close(&shell, b, c, vcx);
    assert_eq!(landed(&shell, vcx), (Some(b), true));
    select_and_close(&shell, b, a, vcx);
    assert_eq!(landed(&shell, vcx), (Some(b), true));
}

/// A shell whose launched Codex session of a fresh task is its worker's
/// session, reported blocked; with the pane focused.
fn blocked_worker<'a>(
    launches: &Launches,
    config: &Path,
    cx: &'a mut TestAppContext,
) -> (
    Entity<crate::Shell>,
    &'a mut gpui::VisualTestContext,
    String,
    String,
) {
    let task = launches.create(cx);
    let (shell, vcx, session) = adopted(launches, &task, config, cx);
    launches.runs.owner.update(vcx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, &session))
    });
    shell.update(vcx, |shell, _| shell.selected_task = Some(task.clone()));
    report(launches, &task, &session, "blocked", vcx);
    let id = session_holding(&shell, &session, vcx).unwrap();
    let pane = vcx.read(|cx| shell.read(cx).active(cx).panes[&crate::session_pane_id(id)].clone());
    vcx.update(|window, cx| pane.read(cx).focus_handle(cx).focus(window, cx));
    vcx.run_until_parked();
    (shell, vcx, task, session)
}

/// Issue 94a, Iced's workspace chip (`Workspace::attention_count`): a
/// launched pane needing input counts once, as needs-you, until typed into.
#[gpui::test]
fn a_blocked_worker_pane_counts_in_the_workspace_bar(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, _, _) = blocked_worker(&launches, config.path(), cx);
    let attention = |cx: &mut gpui::VisualTestContext| {
        cx.read(|cx| {
            let shell = shell.read(cx);
            let active = shell.active_workspace(cx);
            shell
                .bar_input(cx)
                .workspaces
                .iter()
                .find(|w| Some(w.id) == active)
                .and_then(|w| w.attention().map(|a| (a.count, a.kind)))
        })
    };
    assert_eq!(attention(vcx), Some((1, AttentionKind::NeedsYou)));
    vcx.simulate_keystrokes("x");
    assert_eq!(attention(vcx), None);
    launches.kill(1);
}

/// Issue 94b, Iced's tab label glyph (`● ` for `HumanInputRequired`): the
/// pane's dock title leads with it while the worker needs input.
#[gpui::test]
fn a_blocked_worker_pane_title_shows_the_needs_you_glyph(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, _, session) = blocked_worker(&launches, config.path(), cx);
    let id = session_holding(&shell, &session, vcx).unwrap();
    let title = |cx: &mut gpui::VisualTestContext| {
        cx.read(|cx| {
            let pane = &shell.read(cx).active(cx).panes[&crate::session_pane_id(id)];
            let pane = pane.read(cx);
            (pane.dock_title(cx), pane.tab_title())
        })
    };
    let (shown, plain) = title(vcx);
    assert_eq!(shown, format!("● {plain}"));
    vcx.simulate_keystrokes("x");
    let (shown, plain) = title(vcx);
    assert_eq!(shown, plain);
    launches.kill(1);
}

/// Issue 94c, Iced's `resumed_blocked` (src/main.rs, the terminal-write
/// arm): input into a blocked worker's session makes it working at once,
/// before its next report.
#[gpui::test]
fn typing_into_a_blocked_worker_makes_it_working(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, task, session) = blocked_worker(&launches, config.path(), cx);
    let id = session_holding(&shell, &session, vcx).unwrap();
    let uid = vcx.read(|cx| {
        shell.read(cx).active(cx).panes[&crate::session_pane_id(id)]
            .read(cx)
            .session
            .clone()
            .unwrap()
            .session_uid
    });
    let working = |cx: &mut gpui::VisualTestContext| {
        launches
            .runs
            .owner
            .read_with(cx, |owner, _| owner.terminal_working_sessions())
            .contains(&uid)
    };
    assert!(!working(vcx));
    assert_eq!(
        stored_task(&launches.runs.owner, &task, vcx).lifecycle,
        TaskLifecycle::WaitingForInput
    );
    vcx.simulate_keystrokes("x");
    vcx.run_until_parked();
    assert!(working(vcx));
    assert_eq!(
        stored_task(&launches.runs.owner, &task, vcx).lifecycle,
        TaskLifecycle::Running
    );
    launches.kill(1);
}
