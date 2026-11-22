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
    let (shell, vcx) = task_shell(launches, config, cx);
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
    launches.kill(1);
}

/// Gap 3, Iced's `close_final_workers` on the Tick after delivery: a
/// worker's final session closes once its result went out (here its
/// requesting chat is not open, so it is never sent automatically); its
/// pane closes, its child is reaped and its task ends Completed. Before
/// the worker reports, the tick closes nothing.
#[gpui::test]
fn a_final_worker_pane_closes_after_its_result_went_out(cx: &mut TestAppContext) {
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, "gone"))
    });
    let config = tempfile::tempdir().unwrap();
    let (shell, vcx, session) = adopted(&launches, &task, config.path(), cx);
    let (pid, descendant) = (launches.agent.pid(1), launches.agent.descendant(1));
    vcx.executor()
        .advance_clock(crate::task_host::DELIVERY_POLL);
    vcx.run_until_parked();
    assert!(session_holding(&shell, &session, vcx).is_some());

    report(&launches, &task, &session, "done", vcx);
    tick_until(vcx, |cx| session_holding(&shell, &session, cx).is_none());
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
