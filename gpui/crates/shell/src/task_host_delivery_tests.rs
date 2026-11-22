//! H5b B11: fixture-mode delivery of finished results into the GPUI chat
//! that requested them, through core's delivery reducer.
use super::*;
use gitterm_core::{
    task_host::{DeliveryChat, DeliveryCompletion, DeliveryEffect, DeliveryReceipt, Submission},
    task_mcp::TaskControlOperation as Op,
    tasks::{DelegationResult, DelegationStatus, TaskHandoff},
};
use std::{cell::RefCell, rc::Rc};

/// A host over an Iced store holding the running task `worker`, whose
/// worktree is in the fixture, and the chat panes it delivers into.
struct Delivery {
    fixture: Fixture,
    git: GitFixture,
    chats: Chats,
    owner: Entity<TaskHostOwner>,
    sender: TaskControlSender,
    /// What the submit hook was handed, in order.
    submitted: Rc<RefCell<Vec<Submission>>>,
}
impl Delivery {
    fn start(fixture_mode: bool, imported: Vec<Delegation>, cx: &mut TestAppContext) -> Self {
        cx.background_executor.allow_parking();
        let fixture = Fixture::new();
        let git = GitFixture::new(&fixture);
        fixture.write_iced(
            vec![task_at("worker", TaskLifecycle::Running, &git.worktree)],
            imported,
        );
        let chats = Chats::new(&fixture, cx);
        let (owner, sender) = if fixture_mode {
            git.start(&fixture, cx)
        } else {
            fixture.start(cx)
        };
        Self {
            fixture,
            git,
            chats,
            owner,
            sender,
            submitted: Default::default(),
        }
    }
    /// A requesting chat pane; returns its caller id.
    fn open(&self, conversation: Option<&str>, cx: &mut TestAppContext) -> String {
        self.chats.open(&self.fixture, conversation, cx)
    }
    fn tab(&self, uid: &str, cx: &mut TestAppContext) -> usize {
        self.chats
            .owner
            .read_with(cx, |owner, cx| owner.chats[uid].read(cx).target().tab_id)
    }
    /// The chat as core sees it, for calls made around the owner.
    fn chat(&self, uid: &str, cx: &mut TestAppContext) -> Vec<DeliveryChat> {
        vec![DeliveryChat {
            tab_id: self.tab(uid, cx),
            session_uid: uid.to_string(),
            chat_session_id: None,
            remote: false,
            streaming: false,
        }]
    }
    /// Stores the worker `id` as this host's own, requested by `uid`, and
    /// completes it.
    fn own_result(&self, id: &str, uid: &str, cx: &mut TestAppContext) {
        let finished = finished(worker_delegation(id, "worker", &format!("child-{id}")), uid);
        let mut running = finished.clone();
        running.status = DelegationStatus::Running;
        running.result = None;
        self.owner.update(cx, |owner, _| {
            owner.own_worker(running);
            owner
                .host_mut()
                .store_mut()
                .as_mut()
                .unwrap()
                .complete_delegation(id, finished.result.unwrap(), "2026-11-20T12:02:00Z")
                .unwrap();
        });
    }
    /// Each write into a chat answers with the next of `answers`.
    fn hook(&self, answers: Vec<Option<bool>>, cx: &mut TestAppContext) {
        let submitted = self.submitted.clone();
        let mut answers = answers.into_iter();
        self.owner.update(cx, |owner, _| {
            owner.submit_hook = Some(Box::new(move |submission| {
                submitted.borrow_mut().push(submission.clone());
                answers.next().expect("an unexpected submission")
            }))
        });
    }
    fn tick(&self, cx: &mut TestAppContext) {
        self.owner.update(cx, |owner, cx| owner.deliver_results(cx));
        cx.run_until_parked();
    }
    fn submissions(&self) -> usize {
        self.submitted.borrow().len()
    }
    fn delivered(&self, id: &str, cx: &mut TestAppContext) -> bool {
        self.owner.read_with(cx, |owner, _| {
            let store = owner.host().store().as_ref().unwrap();
            store.delegation(id).unwrap().is_delivered()
        })
    }
    /// Whether a submission is still out with chat `tab_id`.
    fn pending(&self, tab_id: usize, cx: &mut TestAppContext) -> bool {
        self.owner.read_with(cx, |owner, _| {
            owner.host().delegations().submitting(tab_id).is_some()
        })
    }
    fn complete(
        &self,
        tab_id: usize,
        submission: &Submission,
        completion: DeliveryCompletion,
        cx: &mut TestAppContext,
    ) -> DeliveryReceipt {
        self.owner.update(cx, |owner, _| {
            owner
                .host_mut()
                .delivery_completed(tab_id, submission.delivery, completion)
        })
    }
}

/// `delegation`, finished with a handoff and requested by chat `uid`.
fn finished(mut delegation: Delegation, uid: &str) -> Delegation {
    delegation.parent.session_uid = uid.to_string();
    delegation.parent.chat_session_id = None;
    delegation.status = DelegationStatus::Completed;
    delegation.result = Some(DelegationResult {
        handoff: Some(handoff(&format!("Result of {}", delegation.delegation_id))),
        findings: None,
        reviewed: None,
    });
    delegation
}

fn handoff(summary: &str) -> TaskHandoff {
    TaskHandoff {
        summary: summary.to_string(),
        decisions: vec![],
        next_steps: vec![],
        blockers: vec![],
        updated_by_session_id: None,
        updated_at: "2026-11-20T12:02:00Z".into(),
    }
}

fn submits(effects: &[DeliveryEffect]) -> usize {
    effects
        .iter()
        .filter(|effect| matches!(effect, DeliveryEffect::Submit(_)))
        .count()
}

/// A flush and a tick that run while a submission is out with the chat
/// submit nothing more; the one submission is recorded once.
#[gpui::test]
fn delivery_pending_flush_does_not_resubmit(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    let parent = delivery.open(None, cx);
    delivery.own_result("d1", &parent, cx);
    delivery.hook(vec![Some(true)], cx);
    let chats = delivery.chat(&parent, cx);
    let tab = chats[0].tab_id;
    let during = Rc::new(RefCell::new(None));
    let seen = during.clone();
    delivery.owner.update(cx, |owner, _| {
        owner.before_continuation = Some((
            "submit",
            Box::new(move |owner| {
                let host = owner.host_mut();
                let busy = host.delegations().submitting(tab).is_some();
                let all = |_: &Delegation| true;
                let tick = host.deliver_finished_where(&chats, true, all);
                let flushed = tick
                    .tabs
                    .iter()
                    .map(|&tab| submits(&host.flush_delivery_where(&chats, tab, true, all)))
                    .sum::<usize>();
                *seen.borrow_mut() = Some((busy, flushed));
            }),
        ))
    });
    delivery.tick(cx);
    assert_eq!(*during.borrow(), Some((true, 0)), "(reserved, resubmitted)");
    assert_eq!(delivery.submissions(), 1);
    assert_eq!(delivery.submitted.borrow()[0].tab_id, tab);
    assert_eq!(delivery.submitted.borrow()[0].batch.delegation_ids, ["d1"]);
    assert!(delivery.delivered("d1", cx));
    assert!(!delivery.pending(tab, cx));
    let saved = gitterm_core::tasks::TaskStore::load(delivery.fixture.copy()).unwrap();
    assert!(saved.delegation("d1").unwrap().is_delivered());
    // Delivered: later ticks send nothing (the hook has no answer left).
    delivery.tick(cx);
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 1);
}

/// A chat that refuses the prompt, and a write that gives no receipt,
/// both leave the result undelivered; the next tick retries it.
#[gpui::test]
fn delivery_rejection_and_receipt_failure_preserve_retry(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    let parent = delivery.open(None, cx);
    let tab = delivery.tab(&parent, cx);
    delivery.own_result("d1", &parent, cx);
    // The pane itself refuses: it has no running Claude.
    delivery.tick(cx);
    assert!(!delivery.delivered("d1", cx));
    assert!(!delivery.pending(tab, cx));

    delivery.hook(vec![None, Some(false), Some(true)], cx);
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 1);
    assert!(!delivery.delivered("d1", cx));
    assert!(!delivery.pending(tab, cx));
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 2);
    assert!(!delivery.delivered("d1", cx));
    assert!(!delivery.pending(tab, cx));
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 3);
    assert!(delivery.delivered("d1", cx));
    // Each retry was a new submission of the same result.
    let submitted = delivery.submitted.borrow();
    for submission in submitted.iter() {
        assert_eq!(submission.batch.delegation_ids, ["d1"]);
    }
    assert_ne!(submitted[0].delivery, submitted[1].delivery);
    assert_ne!(submitted[1].delivery, submitted[2].delivery);
}

/// A completion that arrives after its submission was given up on (no
/// receipt), or after it was accepted (a later disconnect), is stale: it
/// neither marks nor unmarks the result, and submits nothing.
#[gpui::test]
fn late_completion_after_timeout_does_not_resubmit(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    let parent = delivery.open(None, cx);
    let tab = delivery.tab(&parent, cx);
    delivery.own_result("d1", &parent, cx);
    delivery.hook(vec![None, Some(true)], cx);
    delivery.tick(cx);
    let lost = delivery.submitted.borrow()[0].clone();
    assert!(!delivery.delivered("d1", cx));
    assert_eq!(
        delivery.complete(tab, &lost, DeliveryCompletion::Accepted, cx),
        DeliveryReceipt::Stale
    );
    cx.run_until_parked();
    assert!(!delivery.delivered("d1", cx));
    assert_eq!(delivery.submissions(), 1);

    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 2);
    let accepted = delivery.submitted.borrow()[1].clone();
    assert!(delivery.delivered("d1", cx));
    assert_eq!(
        delivery.complete(tab, &accepted, DeliveryCompletion::Rejected, cx),
        DeliveryReceipt::Stale
    );
    assert!(delivery.delivered("d1", cx));
    // No rollback, so nothing is retried (the hook has no answer left).
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 2);
}

/// A fake Claude that answers the launch and records each user turn in
/// `prompts.jsonl` in its folder.
fn fake_claude(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let python = std::process::Command::new("python3")
        .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let program = format!(
        r##"#!{}
import json, sys, os
if sys.argv[1:] == ['--help']:
    print('  --restricted\n  --strict-mcp-config\n  --safe-mode\n  --permission-mode <mode> (choices: "manual")')
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status', '--json']:
    print(json.dumps({{'fixture_signed_in':True,'fixture_method':'fixture_personal','fixture_plan':'fixture_pro','fixture_root':os.environ['CLAUDE_CONFIG_DIR'],'fixture_provider':'fixture_first_party'}}))
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status']:
    print(json.dumps({{'authMethod':'claude.ai', 'configDirectory':os.environ['CLAUDE_CONFIG_DIR']}}))
    sys.exit(0)
def emit(x): print(json.dumps(x), flush=True)
init = json.loads(sys.stdin.readline())
emit({{'type':'control_response','response':{{'subtype':'success','request_id':init['request_id'],'response':{{'models':[]}}}}}})
for line in sys.stdin:
    message = json.loads(line)
    if message.get('type') != 'user':
        continue
    with open('prompts.jsonl', 'a') as prompts:
        prompts.write(line)
    emit({{'type':'system','subtype':'init','session_id':'fake-delivery-chat','model':'synthetic'}})
    emit({{'type':'result','subtype':'success','is_error':False,'result':'ok','usage':{{}}}})
"##,
        String::from_utf8(python.stdout).unwrap().trim()
    );
    let path = dir.join("fake-claude");
    std::fs::write(&path, program).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn prompts(checkout: &Path) -> Vec<String> {
    std::fs::read_to_string(checkout.join("prompts.jsonl"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// A worker this host delegated reports done from its own pane; the next
/// delivery tick writes its result into the requesting chat's Claude, once.
/// With the fixture's `delegations.auto_tell` off it stays on the card.
#[gpui::test]
fn fixture_result_delivered_once_to_the_requesting_chat(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    let settings = delivery.fixture.iced.join("settings.json");
    std::fs::write(&settings, r#"{"delegations": {"auto_tell": false}}"#).unwrap();
    // The requesting chat runs a fake Claude in its own clone; a second
    // chat hosts the worker's session.
    let checkout = delivery.git.outside.clone();
    let saved = crate::chat::SavedChat {
        uid: "requester".into(),
        cwd: checkout.clone(),
        provider: delivery.fixture.roots.provider(),
        session_id: None,
        records: vec![],
    };
    let (pane, window) = cx.add_window_view(|window, cx| {
        crate::chat::ChatPane::history(crate::chat::RuntimeShutdown::new(), saved, window, cx)
    });
    let program = fake_claude(delivery.fixture.iced.parent().unwrap());
    let preview = delivery.fixture.roots.clone();
    pane.update_in(window, |pane, window, cx| {
        pane.start_fake_runtime(preview, &program, window, cx)
    })
    .unwrap();
    let parent = pane.read_with(cx, |pane, _| pane.target().session_uid);
    delivery.chats.owner.update(cx, |owner, _| {
        owner.chats.insert(parent.clone(), pane.clone());
    });
    let hosted = delivery
        .chats
        .open_in(&delivery.fixture, None, &delivery.git.worktree, cx);
    delivery.chats.owner.update(cx, |owner, cx| {
        owner.chats[&hosted].update(cx, |pane, _| pane.bind_task_session("worker", "child-d1"))
    });
    let mut worker = worker_delegation("d1", "worker", "child-d1");
    worker.parent.session_uid = parent.clone();
    worker.parent.chat_session_id = None;
    delivery
        .owner
        .update(cx, |owner, _| owner.own_worker(worker));
    wait_until(cx, |cx| pane.read_with(cx, |pane, _| pane.runtime_ready()));

    let done = Op::UpdateHandoff(req(serde_json::json!({
        "task_id": "worker", "summary": "Finished the fixture change", "status": "done"
    })));
    let value = dispatch_as(cx, &delivery.sender, done, Some(&hosted)).unwrap();
    assert_eq!(value["delegation_id"], "d1", "{value}");
    // Writing into the chat is synchronous within the tick, so the pane's
    // recorded turns show every submission once the executor is parked.
    let sent = |cx: &mut TestAppContext| pane.read_with(cx, |pane, _| pane.user_prompts());
    // auto_tell off: the tick leaves the result on its card.
    cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
    cx.run_until_parked();
    assert!(sent(cx).is_empty());
    assert!(!delivery.delivered("d1", cx));

    std::fs::write(&settings, r#"{"delegations": {"auto_tell": true}}"#).unwrap();
    cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
    cx.run_until_parked();
    assert!(delivery.delivered("d1", cx));
    let prompt = sent(cx);
    assert_eq!(prompt.len(), 1, "{prompt:?}");
    assert!(
        prompt[0].contains("Finished the fixture change"),
        "{}",
        prompt[0]
    );
    assert!(prompt[0].contains("d1"), "{}", prompt[0]);
    // It reached the requesting chat's Claude.
    wait_until(cx, |_| !prompts(&checkout).is_empty());
    assert!(prompts(&checkout)[0].contains("Finished the fixture change"));
    for _ in 0..3 {
        cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
        cx.run_until_parked();
    }
    assert_eq!(sent(cx).len(), 1);
    assert_eq!(prompts(&checkout).len(), 1);
    pane.update(cx, |pane, cx| pane.close(cx));
}

/// Records imported from Iced's store are never delivered, after the seed
/// or a re-seed, even when their chat is open and they would pass the
/// fixture gate; the host's own result in the same chat is.
#[gpui::test]
fn imported_delegation_never_delivered(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![imported("seeded")], cx);
    // The pane reopens the requesting conversation, so core resolves it.
    let parent = delivery.open(Some("parent-history"), cx);
    let tab = delivery.tab(&parent, cx);
    delivery.hook(vec![Some(true)], cx);
    let chats = vec![DeliveryChat {
        tab_id: tab,
        session_uid: parent.clone(),
        chat_session_id: Some("parent-history".into()),
        remote: false,
        streaming: false,
    }];
    // Only ownership keeps it back: the fixture gate alone would send it.
    let resolved = delivery.owner.update(cx, |owner, _| {
        owner
            .host_mut()
            .deliver_finished_where(&chats, true, |_| true)
            .tabs
    });
    assert_eq!(resolved, [tab]);
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 0);
    assert!(!delivery.delivered("seeded", cx));

    delivery.fixture.write_iced(
        vec![task_at(
            "worker",
            TaskLifecycle::Running,
            &delivery.git.worktree,
        )],
        vec![imported("seeded"), imported("reseeded")],
    );
    assert_eq!(
        delivery
            .owner
            .update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Reseeded
    );
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 0);
    assert!(!delivery.delivered("seeded", cx) && !delivery.delivered("reseeded", cx));

    delivery.own_result("own", &parent, cx);
    delivery.tick(cx);
    assert_eq!(delivery.submissions(), 1);
    assert_eq!(delivery.submitted.borrow()[0].batch.delegation_ids, ["own"]);
    assert!(delivery.delivered("own", cx));
    assert!(!delivery.delivered("seeded", cx) && !delivery.delivered("reseeded", cx));
}

/// A finished worker record in Iced's store, for the conversation the
/// test pane reopens.
fn imported(id: &str) -> Delegation {
    let mut delegation = finished(worker_delegation(id, "worker", "child"), "parent");
    delegation.parent.chat_session_id = Some("parent-history".into());
    delegation
}

/// Plain preview sends no finished result anywhere: its tick does nothing,
/// and core's gate denies automatic delivery for the open chat.
#[gpui::test]
fn preview_never_auto_delivers(cx: &mut TestAppContext) {
    let delivery = Delivery::start(false, vec![imported("d1")], cx);
    let parent = delivery.open(Some("parent-history"), cx);
    let tab = delivery.tab(&parent, cx);
    delivery.hook(vec![], cx);
    delivery.tick(cx);
    for _ in 0..2 {
        cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
        cx.run_until_parked();
    }
    assert_eq!(delivery.submissions(), 0);
    let chats = vec![DeliveryChat {
        tab_id: tab,
        session_uid: parent.clone(),
        chat_session_id: Some("parent-history".into()),
        remote: false,
        streaming: false,
    }];
    delivery.owner.update(cx, |owner, _| {
        let host = owner.host_mut();
        assert!(host.deliver_finished(&chats, true).tabs.is_empty());
        assert!(
            host.deliver_finished_where(&chats, true, |_| true)
                .tabs
                .is_empty()
        );
        assert_eq!(submits(&host.flush_delivery(&chats, tab, true)), 0);
        assert_eq!(
            submits(&host.flush_delivery_where(&chats, tab, true, |_| true)),
            0
        );
    });
    assert!(!delivery.delivered("d1", cx));
    let saved = gitterm_core::tasks::TaskStore::load(delivery.fixture.copy()).unwrap();
    assert!(!saved.delegation("d1").unwrap().is_delivered());
}

/// Iced flushes a chat's delivery when its turn ends (the `TurnCompleted`
/// arm), not only from the tick: a result that finished while no tick ran
/// reaches the requesting chat as soon as that chat's turn ends. The test
/// clock never advances, so the tick never fires.
#[gpui::test]
fn turn_end_flushes_delivery_without_waiting_for_the_tick(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    std::fs::write(
        delivery.fixture.iced.join("settings.json"),
        r#"{"delegations": {"auto_tell": true}}"#,
    )
    .unwrap();
    let owner = delivery.owner.clone();
    cx.update(|cx| cx.set_global(crate::task_host::GpuiTaskHost(owner)));
    let checkout = delivery.git.outside.clone();
    let saved = crate::chat::SavedChat {
        uid: "requester".into(),
        cwd: checkout.clone(),
        provider: delivery.fixture.roots.provider(),
        session_id: None,
        records: vec![],
    };
    let (pane, window) = cx.add_window_view(|window, cx| {
        crate::chat::ChatPane::history(crate::chat::RuntimeShutdown::new(), saved, window, cx)
    });
    let program = fake_claude(delivery.fixture.iced.parent().unwrap());
    let preview = delivery.fixture.roots.clone();
    pane.update_in(window, |pane, window, cx| {
        pane.start_fake_runtime(preview, &program, window, cx)
    })
    .unwrap();
    // The shell's wiring for a live chat pane.
    let _turn_ends = cx.update(|cx| crate::task_host::forward_turn_ends(&pane, cx));
    let parent = pane.read_with(cx, |pane, _| pane.target().session_uid);
    delivery.chats.owner.update(cx, |owner, _| {
        owner.chats.insert(parent.clone(), pane.clone());
    });
    let hosted = delivery
        .chats
        .open_in(&delivery.fixture, None, &delivery.git.worktree, cx);
    delivery.chats.owner.update(cx, |owner, cx| {
        owner.chats[&hosted].update(cx, |pane, _| pane.bind_task_session("worker", "child-d1"))
    });
    let mut worker = worker_delegation("d1", "worker", "child-d1");
    worker.parent.session_uid = parent.clone();
    worker.parent.chat_session_id = None;
    delivery
        .owner
        .update(cx, |owner, _| owner.own_worker(worker));
    wait_until(cx, |cx| pane.read_with(cx, |pane, _| pane.runtime_ready()));

    let done = Op::UpdateHandoff(req(serde_json::json!({
        "task_id": "worker", "summary": "Finished the fixture change", "status": "done"
    })));
    dispatch_as(cx, &delivery.sender, done, Some(&hosted)).unwrap();
    cx.run_until_parked();
    // No tick ran and no turn ended: nothing is delivered yet.
    assert!(!delivery.delivered("d1", cx));
    assert!(prompts(&checkout).is_empty());

    // The user's turn ends; its end sends the held result at once.
    pane.update(cx, |pane, cx| {
        pane.submit_prompt("Fixture question".into(), cx);
    });
    wait_until(cx, |cx| delivery.delivered("d1", cx));
    wait_until(cx, |_| prompts(&checkout).len() == 2);
    let sent = prompts(&checkout);
    assert!(sent[0].contains("Fixture question"), "{}", sent[0]);
    assert!(
        sent[1].contains("Finished the fixture change"),
        "{}",
        sent[1]
    );
    // The delivered turn's own end finds nothing more to send.
    cx.run_until_parked();
    assert_eq!(prompts(&checkout).len(), 2);
    pane.update(cx, |pane, cx| pane.close(cx));
}

/// Iced's `auto_deliver_delegations` logs finished results whose chat is
/// not open, once per change of that chat's count: the tick records the
/// counts it logged against, and keeps them while nothing changes.
#[gpui::test]
fn tick_counts_results_waiting_for_a_closed_chat(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    std::fs::write(
        delivery.fixture.iced.join("settings.json"),
        r#"{"delegations": {"auto_tell": true}}"#,
    )
    .unwrap();
    let counts = |cx: &mut TestAppContext| {
        delivery
            .owner
            .read_with(cx, |owner, _| owner.delegation_waiting_counts.clone())
    };
    delivery.own_result("d1", "closed-chat", cx);
    delivery.tick(cx);
    assert_eq!(counts(cx), [("closed-chat".to_string(), 1)].into());
    delivery.tick(cx);
    assert_eq!(counts(cx), [("closed-chat".to_string(), 1)].into());
    delivery.own_result("d2", "closed-chat", cx);
    delivery.tick(cx);
    assert_eq!(counts(cx), [("closed-chat".to_string(), 2)].into());
    assert!(!delivery.delivered("d1", cx));
}

/// Iced's `delegation_card` shows a result held for its chat's turn end:
/// the pane takes core's held delegations, and the card flag follows them.
#[gpui::test]
fn roster_card_shows_a_held_result(cx: &mut TestAppContext) {
    let delivery = Delivery::start(true, vec![], cx);
    std::fs::write(
        delivery.fixture.iced.join("settings.json"),
        r#"{"delegations": {"auto_tell": true}}"#,
    )
    .unwrap();
    let owner = delivery.owner.clone();
    cx.update(|cx| cx.set_global(crate::task_host::GpuiTaskHost(owner)));
    let parent = delivery.open(None, cx);
    let tab = delivery.tab(&parent, cx);
    delivery.own_result("d1", &parent, cx);
    let pane = delivery
        .chats
        .owner
        .read_with(cx, |owner, _| owner.chats[&parent].clone());
    let held = |cx: &mut TestAppContext| pane.read_with(cx, |pane, _| pane.delivery_held().clone());
    assert!(held(cx).is_empty());

    // The chat is mid-turn: core holds the finished result for its end.
    let mut busy = delivery.chat(&parent, cx);
    busy[0].streaming = true;
    delivery.owner.update(cx, |owner, _| {
        let effects = owner
            .host_mut()
            .flush_delivery_where(&busy, tab, true, |_| true);
        assert_eq!(submits(&effects), 0);
        assert!(owner.host().delivery_held(tab, "d1"));
    });
    delivery
        .owner
        .update(cx, |owner, cx| owner.show_held(tab, cx));
    assert_eq!(held(cx), ["d1".to_string()].into());

    // The turn ends: the flush delivers it and the card is no longer held.
    delivery.hook(vec![Some(true)], cx);
    delivery
        .owner
        .update(cx, |owner, cx| owner.chat_turn_ended(tab, cx));
    cx.run_until_parked();
    assert!(delivery.delivered("d1", cx));
    assert!(held(cx).is_empty());

    // The flag lands on exactly the held cards.
    let card = |id: &str| gitterm_gpui_chat::delegation::DelegationCard {
        delegation: finished(
            worker_delegation(id, "worker", &format!("child-{id}")),
            &parent,
        ),
        activity: None,
        held: false,
        worker: None,
    };
    let shown = crate::chat::with_held(&[card("d1"), card("d2")], &["d2".to_string()].into());
    assert_eq!(
        shown.iter().map(|card| card.held).collect::<Vec<_>>(),
        [false, true]
    );
}
