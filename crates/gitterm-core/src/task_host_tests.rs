use super::*;
use crate::preview::{Preview, ProtectedRoots};
use crate::task_commands::runtime::{command_channel, PendingReply};
use crate::task_commands::TaskProjection;
use crate::task_mcp::TaskControlOperation as Op;
use std::sync::Arc;

fn req<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn production(dir: &tempfile::TempDir) -> ServiceContext {
    ServiceContext::production(dir.path(), "host-test").unwrap()
}

fn preview(dir: &tempfile::TempDir) -> ServiceContext {
    let protected = ProtectedRoots::new(&dir.path().join("host"), []).unwrap();
    let preview = Preview::validate(&dir.path().join("preview"), protected).unwrap();
    ServiceContext::preview(Arc::new(preview)).unwrap()
}

/// Dispatches one operation on its own channel; the reply is still pending
/// when the host returned an effect.
fn dispatch(
    host: &TaskHost,
    operation: TaskControlOperation,
    caller: Option<&str>,
) -> (Option<HostEffect>, PendingReply) {
    let (sender, mut receiver) = command_channel();
    let pending = sender
        .enqueue(operation, caller.map(str::to_string))
        .unwrap();
    (host.dispatch(receiver.try_recv().unwrap()), pending)
}

fn answered(host: &TaskHost, operation: TaskControlOperation, caller: Option<&str>) -> Value {
    let (effect, pending) = dispatch(host, operation, caller);
    assert!(effect.is_none(), "{effect:?}");
    match wait(pending) {
        Ok(value) => value,
        Err(error) => Value::String(error),
    }
}

fn wait(pending: PendingReply) -> Result<Value, String> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(pending)
}

#[test]
fn preview_gate_denies_writes_and_launches_before_dispatch_and_lets_reads_through() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, _receiver) = command_channel();
    let host = TaskHost::new(None, preview(&dir), commands);
    for (operation, label) in [
        (
            Op::Create(req(
                json!({"title": "t", "objective": "o", "repository_path": "/repo"}),
            )),
            "task_create",
        ),
        (
            Op::UpdateHandoff(req(json!({"task_id": "t", "summary": "s"}))),
            "task_update_handoff",
        ),
        (
            Op::SessionEvent(TaskSessionEventRequest {
                task_id: "t".into(),
                session_id: "s".into(),
                event_type: "other".into(),
            }),
            "notify",
        ),
        (
            Op::MergePlanSet(req(json!({"entries": []}))),
            "merge_plan_set",
        ),
        (
            Op::LaunchSession(req(json!({"task_id": "t"}))),
            "task_launch_session",
        ),
        (
            Op::RequestReview(req(json!({"target": "uncommitted"}))),
            "review_request",
        ),
        (
            Op::RequestConsult(req(json!({"brief": "b"}))),
            "consult_request",
        ),
        (
            Op::DelegateTask(req(json!({"title": "t", "objective": "o"}))),
            "delegate_task",
        ),
    ] {
        assert_eq!(
            answered(&host, operation, Some("caller")),
            Value::String(format!("{label} · not available in preview"))
        );
    }
    // Reads reach their normal handling, including its own errors.
    assert_eq!(
        answered(&host, Op::ListDelegations(req(json!({}))), Some("c")),
        Value::String(STORE_UNAVAILABLE.into())
    );
    let (effect, _pending) = dispatch(&host, Op::PlanLimits, None);
    assert!(matches!(effect, Some(HostEffect::PlanLimits { .. })));
}

#[test]
fn mcp_launch_is_agent_origin_and_keeps_the_explicit_model_choice() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, _receiver) = command_channel();
    let host = TaskHost::new(None, production(&dir), commands);
    for choice in [false, true] {
        let operation = Op::LaunchSession(req(json!({
            "task_id": "t", "preset_name": "Codex", "user_model_choice": choice
        })));
        let (effect, _pending) = dispatch(&host, operation, Some("caller"));
        let Some(HostEffect::LaunchSession { launch, .. }) = effect else {
            panic!("expected a launch, got {effect:?}");
        };
        assert_eq!(launch.task_id, "t");
        assert_eq!(launch.preset_name.as_deref(), Some("Codex"));
        assert_eq!(launch.origin, TaskLaunchOrigin::Agent);
        assert_eq!(launch.user_model_choice, choice);
    }
}

#[test]
fn reads_carry_their_own_reply_and_the_authority_projection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("tasks.json"),
        include_bytes!("../../../tests/fixtures/stage45/tasks/old.tasks.json"),
    )
    .unwrap();
    let store = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    let (commands, _receiver) = command_channel();
    let host = TaskHost::new(Some(store), production(&dir), commands);
    let (effect, _pending) = dispatch(
        &host,
        Op::Get(req(json!({"task_id": "fixture-task"}))),
        None,
    );
    let Some(HostEffect::Read { reply, effect }) = effect else {
        panic!("expected a read");
    };
    assert_eq!(effect.reply, reply.id());
    let Ok(TaskProjection::Get(task)) = effect.projection else {
        panic!("expected the task");
    };
    assert_eq!(task.task_id, "fixture-task");
    let (effect, _pending) = dispatch(&host, Op::Get(req(json!({"task_id": "absent"}))), None);
    let Some(HostEffect::Read { effect, .. }) = effect else {
        panic!("expected a read");
    };
    assert_eq!(
        effect.projection.unwrap_err(),
        "Task absent is not available"
    );
}

#[test]
fn store_only_commands_are_answered_by_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, _receiver) = command_channel();
    let host = TaskHost::new(None, production(&dir), commands);
    let notify = |event_type: &str| {
        Op::SessionEvent(TaskSessionEventRequest {
            task_id: "t".into(),
            session_id: "s".into(),
            event_type: event_type.into(),
        })
    };
    assert_eq!(
        answered(&host, notify("other"), None),
        json!({"ignored": "other"})
    );
    let (effect, _pending) = dispatch(&host, notify("agent-turn-complete"), None);
    assert!(matches!(
        effect,
        Some(HostEffect::TurnEnded { ref task_id, ref session_id, .. })
            if task_id == "t" && session_id == "s"
    ));
    assert_eq!(
        answered(&host, Op::ListDelegations(req(json!({}))), None),
        Value::String(missing_caller_error("delegation_list"))
    );
    assert_eq!(
        answered(
            &host,
            Op::GetDelegation(req(json!({"delegation_id": "d"}))),
            None
        ),
        Value::String(missing_caller_error("delegation_get"))
    );
}

#[tokio::test]
async fn shutdown_stops_ingress_and_resolves_replies_still_held_by_deferred_work() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, mut receiver) = command_channel();
    let host = TaskHost::new(None, production(&dir), commands.clone());
    let pending = commands.enqueue(Op::PlanLimits, None).unwrap();
    let Some(HostEffect::PlanLimits { reply }) = host.dispatch(receiver.try_recv().unwrap()) else {
        panic!("expected plan limits");
    };
    let queued = commands.enqueue(Op::PlanLimits, None).unwrap();
    host.shutdown();
    assert_eq!(
        pending.await.unwrap_err(),
        "GitTerm closed the task command without a response"
    );
    assert!(queued.await.is_err());
    assert!(commands.enqueue(Op::PlanLimits, None).is_err());
    assert!(receiver.recv().await.is_none());
    // A late answer from deferred work is harmless.
    reply.send(Ok(json!("late")));
}

#[tokio::test]
async fn command_stream_yields_in_admission_order_and_ends_with_ingress() {
    use tokio_stream::StreamExt;
    let (sender, receiver) = command_channel();
    let mut stream = commands(Some(receiver));
    let first = sender
        .enqueue(Op::PlanLimits, Some("first".into()))
        .unwrap();
    let _second = sender
        .enqueue(Op::MergePlanGet, Some("second".into()))
        .unwrap();
    let a = stream.next().await.unwrap();
    let b = stream.next().await.unwrap();
    assert!(matches!(a.operation, Op::PlanLimits));
    assert!(matches!(b.operation, Op::MergePlanGet));
    assert_eq!(
        (a.caller.as_deref(), b.caller.as_deref()),
        (Some("first"), Some("second"))
    );
    a.reply.send(Ok(json!(1)));
    assert_eq!(first.await.unwrap(), json!(1));
    sender.close();
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
    assert!(commands(None).next().await.is_none());
}
