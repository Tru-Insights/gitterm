//! `TurnEnded` for a launched terminal, as Iced's arm checks every tab.
use super::*;
use gitterm_core::task_mcp::TaskSessionEventRequest;

fn turn_ended(
    launches: &Launches,
    task: &str,
    session: &str,
    cx: &mut TestAppContext,
) -> serde_json::Value {
    dispatch_as(
        cx,
        &launches.runs.sender,
        TaskControlOperation::SessionEvent(TaskSessionEventRequest {
            task_id: task.to_string(),
            session_id: session.to_string(),
            event_type: "agent-turn-complete".into(),
        }),
        None,
    )
    .unwrap()
}

/// A launched terminal's turn ending makes its task wait for input; a
/// session the host does not hold changes nothing; a worker's final
/// session keeps its outcome.
#[gpui::test]
fn turn_ended_counts_launched_terminals(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |_| launches.agent.started(1));
    assert_eq!(
        stored_task(&launches.runs.owner, &task, cx).lifecycle,
        TaskLifecycle::Running
    );

    let other = turn_ended(&launches, &task, "not-held", cx);
    assert_eq!(other["waiting_for_input"], false, "{other}");
    assert_eq!(
        stored_task(&launches.runs.owner, &task, cx).lifecycle,
        TaskLifecycle::Running
    );

    let ended = turn_ended(&launches, &task, &session, cx);
    assert_eq!(ended["waiting_for_input"], true, "{ended}");
    wait_until(cx, |_| {
        launches.saved().get(&task).unwrap().lifecycle == TaskLifecycle::WaitingForInput
    });
    launches.kill(1);
}

#[gpui::test]
fn turn_ended_keeps_a_workers_final_terminal(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let launches = Launches::start(cx);
    let task = launches.create(cx);
    launches.runs.owner.update(cx, |owner, _| {
        owner.own_worker(worker_delegation("worker", &task, "gone"))
    });
    let reply = launches.launch(&task, Some("Codex"), cx).unwrap();
    let session = reply["session_id"].as_str().unwrap().to_string();
    wait_until(cx, |_| launches.agent.started(1));
    dispatch_as(
        cx,
        &launches.runs.sender,
        TaskControlOperation::UpdateHandoff(req(serde_json::json!({
            "task_id": task, "summary": "Finished", "status": "done"
        }))),
        Some(&session),
    )
    .unwrap();
    assert_eq!(
        stored_task(&launches.runs.owner, &task, cx).lifecycle,
        TaskLifecycle::Completed
    );
    turn_ended(&launches, &task, &session, cx);
    cx.run_until_parked();
    assert_eq!(
        stored_task(&launches.runs.owner, &task, cx).lifecycle,
        TaskLifecycle::Completed
    );
    launches.kill(1);
}
