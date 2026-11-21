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

/// Stands in for an unpublished terminal: records when it is dropped.
struct Child(std::rc::Rc<std::cell::Cell<bool>>);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

fn session() -> LaunchedSession {
    LaunchedSession {
        label: "Fake".into(),
        harness: None,
        conversation: None,
        objective_delivery: Default::default(),
        timestamp: "2000-01-01T00:00:00Z".into(),
    }
}

fn ticket(task_id: &str) -> launch::LaunchTicket {
    launch::LaunchTicket::for_test(task_id, Some("worker"))
}

#[test]
fn failed_publish_drops_the_unpublished_child_and_saves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, _receiver) = command_channel();
    for store in [
        None,
        Some(TaskStore::load(dir.path().join("tasks.json")).unwrap()),
    ] {
        let with_store = store.is_some();
        let mut host = TaskHost::new(store, production(&dir), commands.clone());
        let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
        let error = host
            .publish_launch(ticket("absent"), session(), Child(dropped.clone()))
            .err()
            .unwrap();
        assert!(dropped.get(), "{error}");
        if with_store {
            assert!(error.contains("task absent does not exist"), "{error}");
            assert!(host.store().as_ref().unwrap().tasks().is_empty());
        } else {
            assert_eq!(error, "GitTerm's task store became unavailable");
        }
    }
}

#[test]
fn preview_host_never_promotes_queued_work() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, _receiver) = command_channel();
    let mut host = TaskHost::new(None, preview(&dir), commands);
    host.restore_launch_queue(vec![QueuedTaskLaunch {
        task_id: "t".into(),
        preset_idx: Some(0),
        origin: TaskLaunchOrigin::Agent,
        user_model_choice: false,
    }]);
    assert!(matches!(host.next_queued_launch(0, 1), Promotion::Stop));
    assert_eq!(host.launch_queue().len(), 1);
    let policy =
        crate::plan_limits::evaluate_policy(&crate::plan_limits::PlanLimitsFile::default(), 0);
    let running = std::collections::HashSet::new();
    let capacity = RunnerCapacity {
        running: &running,
        max_concurrent: 1,
        policy: &policy,
        running_workers: 0,
    };
    assert!(host
        .admit_delegations(capacity, &Default::default())
        .is_empty());
}

/// A host with a store at `dir/tasks.json` holding finished consults `ids`
/// requested by the chat [`chat`] returns.
fn delivery_host(dir: &tempfile::TempDir, services: ServiceContext, ids: &[&str]) -> TaskHost {
    let mut store = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    let caller = crate::delegations::CallerTab {
        session_uid: "chat-uid".into(),
        chat_session_id: None,
        workspace: "fixture".into(),
        cwd: "/fixture/repo".into(),
        remote: false,
        home: "/fixture/repo".into(),
        repos: vec![],
    };
    for id in ids {
        let new = crate::delegations::consult_delegation(
            &req(json!({"brief": format!("Inspect {id}")})),
            &caller,
            None,
            None,
        )
        .unwrap();
        let mut delegation = Delegation::new_requested(new, "t0");
        delegation.delegation_id = id.to_string();
        store.insert_delegation(delegation).unwrap();
        store
            .complete_delegation(
                id,
                crate::tasks::DelegationResult {
                    handoff: Some(crate::tasks::TaskHandoff {
                        summary: format!("Result {id}"),
                        decisions: vec![],
                        next_steps: vec![],
                        blockers: vec![],
                        updated_by_session_id: None,
                        updated_at: "t1".into(),
                    }),
                    findings: None,
                    reviewed: None,
                },
                "t1",
            )
            .unwrap();
    }
    let (commands, _receiver) = command_channel();
    TaskHost::new(Some(store), services, commands)
}

fn chat(streaming: bool) -> Vec<DeliveryChat> {
    vec![DeliveryChat {
        tab_id: 7,
        session_uid: "chat-uid".into(),
        chat_session_id: None,
        remote: false,
        streaming,
    }]
}

fn submissions(effects: Vec<DeliveryEffect>) -> Vec<Submission> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            DeliveryEffect::Submit(submission) => Some(submission),
            _ => None,
        })
        .collect()
}

fn is_delivered(host: &TaskHost, id: &str) -> bool {
    host.store()
        .as_ref()
        .unwrap()
        .delegation(id)
        .unwrap()
        .is_delivered()
}

#[test]
fn delivery_reserves_each_submission_until_its_identity_stamped_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = delivery_host(&dir, production(&dir), &["a"]);
    let tick = host.deliver_finished(&chat(false), true);
    assert_eq!(tick.tabs, [7]);
    let out = submissions(host.flush_delivery(&chat(false), 7, true));
    assert_eq!(out.len(), 1);
    let first = out[0].clone();
    assert_eq!(first.batch.delegation_ids, ["a"]);
    assert!(host.delivery_pending(7, first.delivery));
    // A flush, a tick and a manual send while it is out submit nothing.
    assert!(submissions(host.flush_delivery(&chat(false), 7, true)).is_empty());
    assert!(host.deliver_finished(&chat(false), true).tabs == [7]);
    assert!(submissions(host.flush_delivery(&chat(false), 7, true)).is_empty());
    assert!(host
        .send_delegation(&chat(false), 7, "a", &[], true)
        .is_empty());
    assert!(!host.delivery_held(7, "a"));

    // Rejected: undelivered, and the next flush retries under a new stamp.
    assert_eq!(
        host.delivery_completed(7, first.delivery, DeliveryCompletion::Rejected),
        DeliveryReceipt::NotSubmitted
    );
    assert!(!is_delivered(&host, "a"));
    let second = submissions(host.flush_delivery(&chat(false), 7, true))
        .pop()
        .unwrap();
    assert_ne!(second.delivery, first.delivery);
    // The first stamp is stale now, even reporting acceptance.
    assert_eq!(
        host.delivery_completed(7, first.delivery, DeliveryCompletion::Accepted),
        DeliveryReceipt::Stale
    );
    assert!(!is_delivered(&host, "a"));
    assert!(host.delivery_pending(7, second.delivery));
    assert_eq!(
        host.delivery_completed(7, second.delivery, DeliveryCompletion::Accepted),
        DeliveryReceipt::Saved
    );
    let saved = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    assert!(saved.delegation("a").unwrap().is_delivered());
    assert_eq!(
        host.delivery_completed(7, second.delivery, DeliveryCompletion::Accepted),
        DeliveryReceipt::Stale
    );
    assert!(submissions(host.flush_delivery(&chat(false), 7, true)).is_empty());
    assert!(matches!(
        host.send_delegation(&chat(false), 7, "a", &[], true)
            .as_slice(),
        [DeliveryEffect::AlreadyDelivered]
    ));
}

#[test]
fn delivery_receipt_save_failure_is_kept_and_saved_by_a_later_tick() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = delivery_host(&dir, production(&dir), &["a"]);
    let submission = submissions(host.flush_delivery(&chat(false), 7, true))
        .pop()
        .unwrap();
    let path = dir.path().join("tasks.json");
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let receipt = host.delivery_completed(7, submission.delivery, DeliveryCompletion::Accepted);
    assert!(
        matches!(&receipt, DeliveryReceipt::SaveFailed(error) if error.contains("tasks.json")),
        "{receipt:?}"
    );
    assert!(is_delivered(&host, "a"));
    let tick = host.deliver_finished(&chat(false), true);
    assert!(tick.receipt_error.is_some());
    // The receipt is in memory: nothing is submitted again.
    for tab_id in tick.tabs {
        assert!(submissions(host.flush_delivery(&chat(false), tab_id, true)).is_empty());
    }
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, bytes).unwrap();
    assert!(host
        .deliver_finished(&chat(false), true)
        .receipt_error
        .is_none());
    let saved = TaskStore::load(&path).unwrap();
    assert!(saved.delegation("a").unwrap().is_delivered());
}

#[test]
fn delivery_holds_while_busy_and_waits_for_a_closed_chat() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = delivery_host(&dir, production(&dir), &["a", "b"]);
    assert!(submissions(host.flush_delivery(&chat(true), 7, true)).is_empty());
    assert!(host.delivery_held(7, "a") && host.delivery_held(7, "b"));
    let out = submissions(host.flush_delivery(&chat(false), 7, true));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].batch.delegation_ids, ["a", "b"]);
    // The requesting chat is closed: its results wait, one entry each.
    let dir = tempfile::tempdir().unwrap();
    let mut host = delivery_host(&dir, production(&dir), &["c"]);
    let tick = host.deliver_finished(&[], true);
    assert_eq!(tick.waiting, ["chat-uid"]);
    assert!(tick.tabs.is_empty());
    assert_eq!(
        host.chat_delegation(&[], 7, "c").unwrap_err(),
        "delegation c belongs to another tab"
    );
}

/// Only automatic delivery is gated here; a manual send's gate is H5b's.
#[test]
fn preview_host_never_delivers_automatically() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = delivery_host(&dir, preview(&dir), &["a"]);
    let tick = host.deliver_finished(&chat(false), true);
    assert!(tick.tabs.is_empty() && tick.waiting.is_empty());
    assert!(submissions(host.flush_delivery(&chat(false), 7, true)).is_empty());
    assert!(!host.delivery_held(7, "a"));
    let dir = tempfile::tempdir().unwrap();
    let mut host = delivery_host(&dir, production(&dir), &["a"]);
    assert_eq!(host.deliver_finished(&chat(false), true).tabs, [7]);
}
