use super::*;
use crate::preview::{Preview, ProtectedRoots};
use crate::task_commands::runtime::{command_channel, PendingReply};
use crate::task_commands::TaskProjection;
use crate::task_mcp::TaskControlOperation as Op;
use std::path::{Path, PathBuf};
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

/// A fixture-launch host over `dir`: its one fixture holds `repo/`, and the
/// store has a task `inside` (worktree `repo/`) and a task `outside`
/// (worktree a real checkout outside the temp dir). Returns the fixture's
/// `repo/` and the outside checkout.
fn target_host(dir: &tempfile::TempDir, fixture_mode: bool) -> (TaskHost, PathBuf, PathBuf) {
    use crate::delegation_test_support::test_task;
    let root = dir.path().join("fixture");
    std::fs::create_dir_all(root.join("repo")).unwrap();
    std::fs::write(root.join(crate::preview::service::FIXTURE_MARKER), b"").unwrap();
    let repo = std::fs::canonicalize(root.join("repo")).unwrap();
    let outside = std::env::current_dir().unwrap();
    let services = if fixture_mode {
        let protected = ProtectedRoots::new(&dir.path().join("host"), []).unwrap();
        let preview = Preview::validate(&dir.path().join("preview"), protected).unwrap();
        ServiceContext::fixture_launch(Arc::new(preview), [root]).unwrap()
    } else {
        production(dir)
    };
    let mut store = TaskStore::load(dir.path().join("tasks.json")).unwrap();
    store.insert(test_task("inside", &repo)).unwrap();
    store.insert(test_task("outside", &outside)).unwrap();
    let (commands, _receiver) = command_channel();
    (
        TaskHost::new(Some(store), services, commands),
        repo,
        outside,
    )
}

/// The notice when the gate denied `operation`, else `None` (dispatched).
fn denial(
    host: &TaskHost,
    operation: TaskControlOperation,
    caller_task: Option<&str>,
) -> Option<String> {
    let label = crate::preview::service::classify_operation(&operation).label;
    let (sender, mut receiver) = command_channel();
    let pending = sender.enqueue(operation, Some("caller".into())).unwrap();
    let effect = host.dispatch_with_caller_task(receiver.try_recv().unwrap(), caller_task);
    if effect.is_some() {
        return None;
    }
    let notice = wait(pending).unwrap_err();
    assert_eq!(notice, format!("{label} · not available in preview"));
    Some(notice)
}

fn delegate(value: Value) -> TaskControlOperation {
    let mut request = json!({"title": "t", "objective": "o"});
    request
        .as_object_mut()
        .unwrap()
        .extend(value.as_object().unwrap().clone());
    Op::DelegateTask(req(request))
}

#[test]
fn fixture_write_allowed_only_for_resolved_target() {
    let dir = tempfile::tempdir().unwrap();
    let (host, repo, outside) = target_host(&dir, true);
    let path = |p: &Path| p.display().to_string();
    let allowed = [
        (Op::LaunchSession(req(json!({"task_id": "inside"}))), None),
        (
            Op::UpdateHandoff(req(json!({"task_id": "inside", "summary": "s"}))),
            None,
        ),
        (
            Op::Create(req(json!({
                "title": "t", "objective": "o", "repository_path": path(&repo)
            }))),
            None,
        ),
        (
            Op::RequestReview(req(
                json!({"target": "uncommitted", "checkout": path(&repo)}),
            )),
            None,
        ),
        // No checkout: the caller's task worktree.
        (
            Op::RequestConsult(req(json!({"brief": "b"}))),
            Some("inside"),
        ),
        (delegate(json!({})), Some("inside")),
    ];
    for (operation, caller_task) in allowed {
        assert_eq!(
            denial(&host, operation.clone(), caller_task),
            None,
            "{operation:?}"
        );
    }
    let denied = [
        (Op::LaunchSession(req(json!({"task_id": "outside"}))), None),
        // Unknown task: unresolved, so denied.
        (Op::LaunchSession(req(json!({"task_id": "absent"}))), None),
        (
            Op::UpdateHandoff(req(json!({"task_id": "outside", "summary": "s"}))),
            None,
        ),
        (
            Op::Create(req(json!({
                "title": "t", "objective": "o", "repository_path": path(&outside)
            }))),
            None,
        ),
        (
            Op::RequestReview(req(
                json!({"target": "uncommitted", "checkout": path(&outside)}),
            )),
            Some("inside"),
        ),
        // The caller's task is unknown, or its worktree is outside.
        (Op::RequestConsult(req(json!({"brief": "b"}))), None),
        (
            Op::RequestConsult(req(json!({"brief": "b"}))),
            Some("outside"),
        ),
        (delegate(json!({})), None),
        // No checkout at all.
        (Op::MergePlanSet(req(json!({"entries": []}))), None),
    ];
    for (operation, caller_task) in denied {
        assert!(
            denial(&host, operation.clone(), caller_task).is_some(),
            "{operation:?}"
        );
    }
    // Plain dispatch knows no caller task.
    let (effect, _pending) = dispatch(
        &host,
        Op::RequestConsult(req(json!({"brief": "b"}))),
        Some("c"),
    );
    assert!(effect.is_none());
    // Reads are not target-checked.
    let (effect, _pending) = dispatch(&host, Op::Get(req(json!({"task_id": "outside"}))), None);
    assert!(matches!(effect, Some(HostEffect::Read { .. })));
}

#[test]
fn fixture_checkout_with_outside_repo_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (host, repo, outside) = target_host(&dir, true);
    let (repo_text, outside_text) = (repo.display().to_string(), outside.display().to_string());
    assert_eq!(
        denial(&host, delegate(json!({"checkout": repo_text})), None),
        None
    );
    assert_eq!(
        denial(&host, delegate(json!({"repo": repo_text})), None),
        None
    );
    // Every named target is checked, even a pair the request handler refuses.
    assert!(denial(
        &host,
        delegate(json!({"checkout": repo_text, "repo": outside_text})),
        Some("inside")
    )
    .is_some());
    // A checkout in the fixture that is a worktree of a repository outside it.
    let worktree = repo.parent().unwrap().join("wt");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), format!("gitdir: {outside_text}\n")).unwrap();
    let worktree_text = worktree.display().to_string();
    assert!(denial(&host, delegate(json!({"checkout": worktree_text})), None).is_some());
    assert!(denial(
        &host,
        Op::RequestReview(req(
            json!({"target": "uncommitted", "checkout": worktree_text})
        )),
        None
    )
    .is_some());
}

#[test]
fn fixture_add_dir_outside_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (host, repo, outside) = target_host(&dir, true);
    let inside_dir = repo.join("docs").display().to_string();
    let repo_text = repo.display().to_string();
    assert_eq!(
        denial(
            &host,
            delegate(json!({"checkout": repo_text, "add_dirs": [inside_dir]})),
            None
        ),
        None
    );
    assert!(denial(
        &host,
        delegate(json!({
            "checkout": repo_text,
            "add_dirs": [inside_dir, outside.display().to_string()]
        })),
        None
    )
    .is_some());
    // add_dirs alone do not stand in for the checkout the worker runs in.
    assert!(denial(&host, delegate(json!({"add_dirs": [inside_dir]})), None).is_some());
}

/// Two queued consults, `in` (checkout in the fixture) and `out` (checkout
/// outside), stored with the caller [`queue_caller`] admission expects.
fn queue_delegations(host: &mut TaskHost, repo: &Path, outside: &Path) {
    use crate::delegation_state::{Authority, Intent};
    let caller = queue_caller(repo);
    for (id, checkout) in [("in", repo), ("out", outside)] {
        let new = crate::delegations::consult_delegation(
            &req(json!({"brief": format!("Inspect {id}")})),
            &caller.tab,
            None,
            None,
        )
        .unwrap();
        let mut delegation = Delegation::new_requested(new, "t0");
        delegation.delegation_id = id.into();
        delegation.options.checkout = Some(checkout.to_path_buf());
        host.store_mut()
            .as_mut()
            .unwrap()
            .insert_delegation(delegation)
            .unwrap();
        host.delegations_mut().reduce(Intent::Stored {
            id: id.into(),
            caller: caller.clone(),
            authority: Authority {
                checkout: checkout.to_path_buf(),
                repository: crate::tasks::RepositoryIdentity {
                    common_dir: checkout.join(".git"),
                    remote_url: None,
                },
                workdir: checkout.to_path_buf(),
                roots: vec![Some(checkout.to_path_buf())],
            },
        });
    }
}

fn queue_caller(repo: &Path) -> crate::delegation_state::Caller {
    let tab = crate::delegations::CallerTab {
        session_uid: "caller".into(),
        chat_session_id: None,
        workspace: "fixture".into(),
        cwd: repo.to_path_buf(),
        remote: false,
        home: repo.to_path_buf(),
        repos: vec![],
    };
    crate::delegation_state::Caller {
        target: crate::session::Target {
            tab_id: 1,
            session_uid: tab.session_uid.clone(),
            task_id: None,
            task_session_id: None,
        },
        workspace_uid: 1,
        local_root: Some(repo.to_path_buf()),
        tab,
    }
}

/// The ids admission starts checking, and those it rejects.
fn admit(host: &mut TaskHost, repo: &Path) -> (Vec<String>, Vec<String>) {
    use crate::delegation_state::Effect;
    let policy =
        crate::plan_limits::evaluate_policy(&crate::plan_limits::PlanLimitsFile::default(), 0);
    let running = std::collections::HashSet::new();
    let capacity = RunnerCapacity {
        running: &running,
        max_concurrent: 4,
        policy: &policy,
        running_workers: 0,
    };
    let callers = std::collections::HashMap::from([("caller".to_string(), queue_caller(repo))]);
    let (mut started, mut rejected) = (Vec::new(), Vec::new());
    for effect in host.admit_delegations(capacity, &callers) {
        match effect {
            Effect::CheckStart { id, .. } => started.push(id),
            Effect::StartRejected { id, .. } => rejected.push(id),
            other => panic!("unexpected {other:?}"),
        }
    }
    (started, rejected)
}

#[test]
fn mixed_allowed_and_denied_headless_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo, outside) = target_host(&dir, true);
    queue_delegations(&mut host, &repo, &outside);
    assert_eq!(admit(&mut host, &repo), (vec!["in".into()], vec![]));
    // The denied run stays queued, untouched, and is never admitted.
    let store = host.store().as_ref().unwrap();
    assert_eq!(
        store.delegation("out").unwrap().status,
        DelegationStatus::Requested
    );
    assert_eq!(admit(&mut host, &repo), (vec![], vec![]));
}

#[test]
fn production_dispatch_and_admission_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo, outside) = target_host(&dir, false);
    let outside_text = outside.display().to_string();
    for (operation, caller_task) in [
        (Op::LaunchSession(req(json!({"task_id": "outside"}))), None),
        (Op::LaunchSession(req(json!({"task_id": "absent"}))), None),
        (
            Op::Create(req(json!({
                "title": "t", "objective": "o", "repository_path": outside_text
            }))),
            None,
        ),
        (Op::RequestConsult(req(json!({"brief": "b"}))), None),
        (
            Op::RequestReview(req(
                json!({"target": "uncommitted", "checkout": outside_text}),
            )),
            Some("outside"),
        ),
        (
            delegate(json!({"repo": outside_text, "add_dirs": [outside_text]})),
            None,
        ),
        (delegate(json!({})), None),
        (Op::MergePlanSet(req(json!({"entries": []}))), None),
    ] {
        assert_eq!(
            denial(&host, operation.clone(), caller_task),
            None,
            "{operation:?}"
        );
    }
    queue_delegations(&mut host, &repo, &outside);
    let (mut started, rejected) = admit(&mut host, &repo);
    started.sort();
    assert_eq!(
        (started, rejected),
        (vec!["in".into(), "out".into()], vec![])
    );
}

/// Iced's `task_control_value` before it moved here, kept verbatim over a
/// stand-in for its tabs: the core fn must answer the same JSON.
#[test]
fn task_control_value_matches_iced() {
    struct Tab {
        id: u64,
        task_id: Option<String>,
        task_session_id: Option<String>,
        repo_name: String,
    }
    struct Workspace {
        name: String,
        tabs: Vec<Tab>,
        active_tab: usize,
    }
    let tab = |id, task: Option<&str>, session: Option<&str>, repo: &str| Tab {
        id,
        task_id: task.map(str::to_string),
        task_session_id: session.map(str::to_string),
        repo_name: repo.into(),
    };
    let workspaces = [
        Workspace {
            name: "First".into(),
            tabs: vec![
                tab(1, Some("t1"), Some("s1"), "api"),
                tab(2, None, None, "other"),
            ],
            active_tab: 0,
        },
        Workspace {
            name: "Second".into(),
            tabs: vec![
                tab(3, Some("t1"), None, "web"),
                tab(4, Some("t2"), Some("s4"), "api"),
            ],
            active_tab: 0,
        },
    ];
    let active_workspace_index = 1;
    let old = |task: &TaskRecord| -> Result<Value, String> {
        let mut value = serde_json::to_value(task)
            .map_err(|error| format!("could not serialize task {}: {error}", task.task_id))?;
        let sessions = workspaces
            .iter()
            .enumerate()
            .flat_map(|(workspace_idx, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| tab.task_id.as_deref() == Some(task.task_id.as_str()))
                    .map(move |tab| {
                        serde_json::json!({
                            "session_id": tab.task_session_id,
                            "label": tab.repo_name,
                            "workspace": workspace.name,
                            "is_active": workspace_idx == active_workspace_index
                                && workspace.active_tab < workspace.tabs.len()
                                && workspace.tabs[workspace.active_tab].id == tab.id,
                        })
                    })
            })
            .collect::<Vec<_>>();
        value
            .as_object_mut()
            .ok_or_else(|| format!("task {} did not serialize as an object", task.task_id))?
            .insert(
                "open_sessions".to_string(),
                serde_json::Value::Array(sessions),
            );
        Ok(value)
    };
    // The new call site, as Iced's main.rs now builds it.
    let new = |task: &TaskRecord| {
        let sessions: Vec<OpenSession> = workspaces
            .iter()
            .enumerate()
            .flat_map(|(workspace_idx, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| tab.task_id.as_deref() == Some(task.task_id.as_str()))
                    .map(move |tab| OpenSession {
                        session_id: tab.task_session_id.clone(),
                        label: tab.repo_name.clone(),
                        workspace: workspace.name.clone(),
                        is_active: workspace_idx == active_workspace_index
                            && workspace.active_tab < workspace.tabs.len()
                            && workspace.tabs[workspace.active_tab].id == tab.id,
                    })
            })
            .collect();
        task_control_value(task, &sessions)
    };
    for id in ["t1", "t2", "t3"] {
        let task = crate::delegation_test_support::test_task(id, Path::new("/repo/wt"));
        let expected = old(&task).unwrap();
        assert_eq!(new(&task).unwrap(), expected, "{id}");
        // Byte-identical too, not only equal as maps.
        assert_eq!(
            serde_json::to_string(&new(&task).unwrap()).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }
    let sessions = old(&crate::delegation_test_support::test_task(
        "t1",
        Path::new("/w"),
    ))
    .unwrap()["open_sessions"]
        .clone();
    assert_eq!(
        sessions,
        json!([
            {"session_id": "s1", "label": "api", "workspace": "First", "is_active": false},
            {"session_id": null, "label": "web", "workspace": "Second", "is_active": true},
        ])
    );
}

#[test]
fn fixture_merge_plan_set_targets_the_caller_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let (host, repo, outside) = target_host(&dir, true);
    let dispatched = |checkout: Option<&Path>| {
        let (sender, mut receiver) = command_channel();
        let _pending = sender
            .enqueue(
                Op::MergePlanSet(req(json!({"entries": []}))),
                Some("caller".into()),
            )
            .unwrap();
        host.dispatch_with_caller(receiver.try_recv().unwrap(), None, checkout)
            .is_some()
    };
    assert!(dispatched(Some(&repo)));
    assert!(!dispatched(Some(&outside)));
    assert!(!dispatched(None));
}
