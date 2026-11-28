// Every case runs in live_writes' subprocess: HOME and the absolute V6 root
// are synthetic before services, provider detection, or GPUI are initialized.
use gitterm_core::{
    task_host::{DeliveryCompletion, DeliveryEffect, DeliveryReceipt, Submission},
    tasks::{DelegationResult, DelegationStatus, TaskHandoff},
};
use gitterm_gpui_chat::interactions::TranscriptAction as CardAction;
use std::{cell::RefCell, rc::Rc};

struct LiveDelivery {
    fixture: Fixture,
    git: GitFixture,
    chats: Chats,
    owner: Entity<TaskHostOwner>,
    sender: TaskControlSender,
    parent: String,
    tab: usize,
    sent: Rc<RefCell<Vec<Submission>>>,
    _fake: LiveFake,
}
impl LiveDelivery {
    fn new(cx: &mut TestAppContext) -> Self {
        cx.background_executor.allow_parking();
        let fake = LiveFake::new();
        let (fixture, git) = fixture();
        fresh_limits(&fixture, false);
        let (chats, parent) = open_live_caller(&fixture, &git, cx);
        fixture.write_iced(
            vec![task_at("worker", TaskLifecycle::Ready, &git.worktree)],
            vec![],
        );
        let (owner, sender) = start(&fixture, cx);
        let tab = chats.owner.read_with(cx, |owner, cx| {
            owner.chats[&parent].read(cx).target().tab_id
        });
        let sent = Rc::new(RefCell::new(Vec::new()));
        let captured = sent.clone();
        owner.update(cx, |owner, _| {
            owner.submit_hook = Some(Box::new(move |submission| {
                captured.borrow_mut().push(submission.clone());
                Some(true)
            }))
        });
        Self {
            fixture,
            git,
            chats,
            owner,
            sender,
            parent,
            tab,
            sent,
            _fake: fake,
        }
    }
    fn pane(&self, cx: &mut TestAppContext) -> Entity<crate::chat::ChatPane> {
        self.chats
            .owner
            .read_with(cx, |owner, _| owner.chats[&self.parent].clone())
    }
    fn result(&self, id: &str, cx: &mut TestAppContext) {
        let mut d = worker_delegation(id, "worker", &format!("child-{id}"));
        d.parent.session_uid = self.parent.clone();
        d.parent.chat_session_id = Some("placeholder-chat".into());
        d.parent.cwd = self.git.repo.clone();
        d.status = DelegationStatus::Completed;
        d.result = Some(DelegationResult {
            handoff: Some(TaskHandoff {
                summary: format!("Finished {id}"),
                decisions: vec![],
                next_steps: vec![],
                blockers: vec![],
                updated_by_session_id: None,
                updated_at: "2026-11-20T12:02:00Z".into(),
            }),
            findings: None,
            reviewed: None,
        });
        self.insert_result(d, cx);
    }
    fn insert_result(&self, mut d: Delegation, cx: &mut TestAppContext) {
        let result = d.result.take().unwrap();
        let id = d.delegation_id.clone();
        d.status = DelegationStatus::Running;
        self.owner.update(cx, |owner, _| {
            let store = owner.host_mut().store_mut().as_mut().unwrap();
            store.insert_delegation(d).unwrap();
            store
                .complete_delegation(&id, result, "2026-11-20T12:02:00Z")
                .unwrap();
        });
    }
    fn tick(&self, cx: &mut TestAppContext) {
        self.owner.update(cx, |owner, cx| owner.deliver_results(cx));
        cx.run_until_parked();
    }
    fn record(&self, id: &str, cx: &mut TestAppContext) -> Delegation {
        self.owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .delegation(id)
                .unwrap()
                .clone()
        })
    }
    fn chats(&self, cx: &mut TestAppContext) -> Vec<gitterm_core::task_host::DeliveryChat> {
        vec![
            self.pane(cx)
                .read_with(cx, |pane, _| gitterm_core::task_host::DeliveryChat {
                    tab_id: self.tab,
                    session_uid: self.parent.clone(),
                    chat_session_id: pane.chat_session_id(),
                    remote: false,
                    streaming: pane.is_working(),
                }),
        ]
    }
    fn action(&self, action: CardAction, cx: &mut TestAppContext) {
        self.owner
            .update(cx, |owner, cx| owner.card_action(self.tab, action, cx));
        cx.run_until_parked();
    }
    fn busy(&self, busy: bool, cx: &mut TestAppContext) {
        use gitterm_core::chat::{
            runtime::RuntimeEvent,
            types::{HarnessEvent, TurnStatus},
        };
        self.pane(cx).update(cx, |pane, cx| {
            let event = if busy {
                HarnessEvent::TurnStarted {
                    session_id: None,
                    model: None,
                }
            } else {
                HarnessEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: json!({}),
                    cost_usd: None,
                    rate_limit: None,
                }
            };
            pane.receive_runtime_event(&pane.runtime_source(), RuntimeEvent::Harness(event), cx);
        });
    }
    fn stop(&self, cx: &mut TestAppContext) {
        self.owner.update(cx, |owner, _| owner.shutdown());
    }
}

#[gpui::test]
fn live_delivery_reserves_once_and_rejects_stale_completion(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_delivery_reserves_once_and_rejects_stale_completion") {
        return;
    }
    let d = LiveDelivery::new(cx);
    d.result("once", cx);
    let chats = d.chats(cx);
    let mut first = None;
    d.owner.update(cx, |owner, _| {
        for effect in owner.host_mut().flush_delivery(&chats, d.tab, true) {
            if let DeliveryEffect::Submit(s) = effect {
                first = Some(s);
            }
        }
        assert!(
            owner
                .host()
                .delivery_pending(d.tab, first.as_ref().unwrap().delivery)
        );
    });
    // A real Live tick must not submit a batch already reserved by core.
    d.tick(cx);
    assert!(d.sent.borrow().is_empty());
    let first = first.unwrap();
    d.owner.update(cx, |owner, _| {
        assert_eq!(
            owner.host_mut().delivery_completed(
                d.tab,
                first.delivery,
                DeliveryCompletion::Rejected
            ),
            DeliveryReceipt::NotSubmitted
        );
    });
    d.tick(cx);
    assert_eq!(d.sent.borrow().len(), 1);
    let second = d.sent.borrow()[0].clone();
    assert_ne!(first.delivery, second.delivery);
    d.owner.update(cx, |owner, _| {
        assert_eq!(
            owner.host_mut().delivery_completed(
                d.tab,
                first.delivery,
                DeliveryCompletion::Accepted
            ),
            DeliveryReceipt::Stale
        );
    });
    d.tick(cx);
    assert_eq!(d.sent.borrow().len(), 1);
    assert!(d.record("once", cx).is_delivered());
    d.stop(cx);
}

#[gpui::test]
fn live_delivery_receipt_save_failure_retries_without_duplicate(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_delivery_receipt_save_failure_retries_without_duplicate") {
        return;
    }
    let d = LiveDelivery::new(cx);
    d.result("receipt", cx);
    let before = std::fs::read(d.fixture.source()).unwrap();
    let root = d.fixture.iced.clone();
    let captured = d.sent.clone();
    d.owner.update(cx, |owner, _| {
        owner.submit_hook = Some(Box::new(move |submission| {
            captured.borrow_mut().push(submission.clone());
            live_set_mode(&root, 0o555);
            Some(true)
        }))
    });
    d.tick(cx);
    live_set_mode(&d.fixture.iced, 0o700);
    assert_eq!(d.sent.borrow().len(), 1);
    assert_eq!(std::fs::read(d.fixture.source()).unwrap(), before);
    assert!(
        d.record("receipt", cx).is_delivered(),
        "accepted receipt remains in memory"
    );
    d.tick(cx);
    d.tick(cx);
    assert_eq!(d.sent.borrow().len(), 1);
    assert!(
        TaskStore::load(d.fixture.source())
            .unwrap()
            .delegation("receipt")
            .unwrap()
            .is_delivered()
    );
    d.stop(cx);
}

#[gpui::test]
fn live_delivery_busy_closed_and_restart_preserve_undelivered_results(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_delivery_busy_closed_and_restart_preserve_undelivered_results") {
        return;
    }
    let mut d = LiveDelivery::new(cx);
    d.result("restart", cx);
    d.busy(true, cx);
    d.tick(cx);
    assert!(d.sent.borrow().is_empty());
    assert!(
        d.owner
            .read_with(cx, |owner, _| owner.host().delivery_held(d.tab, "restart"))
    );
    d.chats.owner.update(cx, |owner, _| {
        owner.chats.remove(&d.parent);
    });
    d.tick(cx);
    assert!(d.sent.borrow().is_empty());
    assert!(!d.record("restart", cx).is_delivered());
    d.stop(cx);
    // Wait for the old listener/runtime reaper to release the synthetic lease.
    wait_until(cx, |_| HostLock::acquire(&d.fixture.iced).is_ok());
    let (restarted, _) = start(&d.fixture, cx);
    d.owner = restarted;
    let sent = d.sent.clone();
    d.owner.update(cx, |owner, _| {
        owner.submit_hook = Some(Box::new(move |s| {
            sent.borrow_mut().push(s.clone());
            Some(true)
        }))
    });
    // Resume as Chat gets a new tab/UID but the same provider conversation.
    d.parent = d
        .chats
        .open_in(&d.fixture, Some("placeholder-chat"), &d.git.repo, cx);
    d.tab = d.pane(cx).read_with(cx, |pane, _| pane.target().tab_id);
    cx.executor().advance_clock(crate::task_host::DELIVERY_POLL);
    cx.run_until_parked();
    assert!(d.record("restart", cx).is_delivered());
    d.tick(cx);
    d.tick(cx);
    assert_eq!(d.sent.borrow().len(), 1);
    assert!(
        TaskStore::load(d.fixture.source())
            .unwrap()
            .delegation("restart")
            .unwrap()
            .is_delivered()
    );
    d.stop(cx);
}

#[gpui::test]
fn live_delivery_restores_background_parent_without_selecting_it(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_delivery_restores_background_parent_without_selecting_it") {
        return;
    }
    let mut d = LiveDelivery::new(cx);
    d.result("background-restart", cx);
    // Persist two open chats, with a different chat selected in another workspace.
    let tab = |uid: &str| crate::chat::resume::tab(&d.git.repo, uid.into(), None);
    std::fs::write(
        d.fixture.iced.join("workspaces.json"),
        serde_json::to_vec(&json!({"active_workspace": 0, "workspaces": [
            {"name":"Selected", "abbrev":"S", "dir":d.git.repo, "color":"blue",
             "active_tab":0, "tabs":[tab("selected-caller")]},
            {"name":"Background", "abbrev":"B", "dir":d.git.repo, "color":"green",
             "active_tab":0, "tabs":[tab(&d.parent)]}
        ]})).unwrap(),
    ).unwrap();
    d.stop(cx);
    wait_until(cx, |_| HostLock::acquire(&d.fixture.iced).is_ok());
    let (restarted, _) = start(&d.fixture, cx);
    d.owner = restarted;
    let services = d.owner.read_with(cx, |owner, _| owner.host().services().clone());
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::chat::policy::install_live(services, cx).unwrap();
        cx.set_global(crate::task_host::GpuiTaskHost(d.owner.clone()));
    });
    let mut store = crate::preview::tests::guarded_store(&d.fixture.roots.root().join("shell"));
    let model = crate::workspace_config::load_iced_workspaces_from(&mut store, &d.fixture.iced);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        crate::Shell::with_owner(
            Rc::new(crate::data::LocalData(owner.clone())), owner, &layout, true, window, cx,
        ).unwrap()
    });
    let selected = shell.read_with(cx, |shell, cx| {
        (shell.active_workspace(cx), shell.active(cx).active_session)
    });
    let pane = shell.read_with(cx, |shell, cx| {
        shell.owner.read(cx).chats.get(&d.parent).cloned()
    }).expect("restored background parent must be registered before any selection");
    // Exercise the actual chat send path, with only a synthetic provider process.
    let program = super::delivery_tests::fake_claude(d.fixture.iced.parent().unwrap());
    pane.update_in(cx, |pane, window, cx| {
        pane.start_fake_runtime(d.fixture.roots.clone(), &program, window, cx)
    }).unwrap();
    d.owner.update(cx, |owner, _| owner.submit_hook = None);
    wait_until(cx, |cx| pane.read_with(cx, |pane, _| pane.runtime_ready()));
    for _ in 0..3 {
        d.tick(cx);
    }
    wait_until(cx, |_| d.git.repo.join("prompts.jsonl").exists());
    assert!(d.record("background-restart", cx).is_delivered());
    assert_eq!(pane.read_with(cx, |pane, _| pane.user_prompts().len()), 1);
    assert_eq!(std::fs::read_to_string(d.git.repo.join("prompts.jsonl")).unwrap().lines().count(), 1);
    assert_eq!(shell.read_with(cx, |shell, cx| {
        (shell.active_workspace(cx), shell.active(cx).active_session)
    }), selected);
    assert!(TaskStore::load(d.fixture.source()).unwrap()
        .delegation("background-restart").unwrap().is_delivered());
    pane.update(cx, |pane, cx| pane.close(cx));
    d.stop(cx);
}

#[gpui::test]
fn live_card_manual_send_waits_when_auto_tell_is_off(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_card_manual_send_waits_when_auto_tell_is_off") {
        return;
    }
    let d = LiveDelivery::new(cx);
    std::fs::write(
        d.fixture.iced.join("settings.json"),
        r#"{"delegations":{"auto_tell":false}}"#,
    )
    .unwrap();
    d.result("manual", cx);
    d.busy(true, cx);
    d.action(
        CardAction::SendDelegation {
            id: "manual".into(),
            finding_ids: vec![],
        },
        cx,
    );
    assert!(d.sent.borrow().is_empty());
    assert!(
        d.owner
            .read_with(cx, |owner, _| owner.host().delivery_held(d.tab, "manual"))
    );
    d.busy(false, cx);
    d.owner
        .update(cx, |owner, cx| owner.chat_turn_ended(d.tab, cx));
    assert_eq!(d.sent.borrow().len(), 1);
    d.action(
        CardAction::SendDelegation {
            id: "manual".into(),
            finding_ids: vec![],
        },
        cx,
    );
    d.tick(cx);
    assert_eq!(d.sent.borrow().len(), 1);
    d.stop(cx);
}

#[gpui::test]
fn live_card_send_selected_findings_and_dismiss(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_card_send_selected_findings_and_dismiss") {
        return;
    }
    let d = LiveDelivery::new(cx);
    let review = finished_review("review", &d, cx);
    d.insert_result(review, cx);
    d.action(
        CardAction::SendDelegation {
            id: "review".into(),
            finding_ids: vec!["selected".into()],
        },
        cx,
    );
    assert_eq!(d.sent.borrow().len(), 1);
    let message = &d.sent.borrow()[0].batch.message;
    assert!(message.contains("Selected finding"), "{message}");
    assert!(!message.contains("Unselected finding"), "{message}");
    d.action(
        CardAction::Dismiss(gitterm_gpui_chat::interactions::DismissRequest {
            id: "review".into(),
            label: "Dismiss".into(),
            active: false,
        }),
        cx,
    );
    let record = d.record("review", cx);
    assert!(record.dismissed_at.is_some());
    assert_eq!(record.status, DelegationStatus::Completed);
    d.stop(cx);
}

fn finished_review(id: &str, d: &LiveDelivery, _: &mut TestAppContext) -> Delegation {
    req(json!({
        "delegation_id": id, "kind":"review",
        "parent":{"session_uid":d.parent,"chat_session_id":"placeholder-chat","workspace":"FX","cwd":d.git.repo},
        "child":{"runner":"codex_review","model":"placeholder"}, "brief":"Review the change",
        "target":{"mode":{"type":"uncommitted"}},
        "options":{"user_model_choice":true},
        "status":{"state":"completed"},
        "result":{"findings":{"verdict":"needs_changes","summary":"Review summary","structured":true,"reviewer":{"harness":"codex"},"findings":[
            {"id":"selected","severity":"P1","title":"Selected finding","body":"Fix it"},
            {"id":"unselected","severity":"P2","title":"Unselected finding","body":"Other issue"}
        ]},"reviewed":{"head":"old-reviewed-head","dirty":true,"target_description":"uncommitted"}},
        "created_at":"2026-11-20T12:00:00Z", "updated_at":"2026-11-20T12:01:00Z"
    }))
}

#[gpui::test]
fn live_card_rerun_uses_current_caller_and_fake_runner(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_card_rerun_uses_current_caller_and_fake_runner") {
        return;
    }
    let d = LiveDelivery::new(cx);
    let review = finished_review("review", &d, cx);
    d.insert_result(review, cx);
    d.action(
        CardAction::RerunDelegation {
            id: "review".into(),
        },
        cx,
    );
    wait_until(cx, |cx| {
        d.owner.read_with(cx, |owner, _| {
            owner.host().store().as_ref().unwrap().delegations().len() == 2
        })
    });
    let rerun = d.owner.read_with(cx, |owner, _| {
        owner.host().store().as_ref().unwrap().delegations()[1].clone()
    });
    assert_ne!(rerun.delegation_id, "review");
    assert_eq!(rerun.parent.session_uid, d.parent);
    assert!(
        matches!(&rerun.child, gitterm_core::tasks::DelegationChild::CodexReview { model: Some(model), .. } if model == "placeholder")
    );
    wait_until(cx, |cx| {
        d.record(&rerun.delegation_id, cx).status.is_final()
    });
    assert!(d._fake.file("ready", 1).exists());
    assert!(d.record(&rerun.delegation_id, cx).result.is_some());
    assert_eq!(rerun.previous.as_deref(), Some("review"));
    d.stop(cx);
}

#[gpui::test]
fn live_card_cancel_and_stop_tracking_leave_worker_task_untouched(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_card_cancel_and_stop_tracking_leave_worker_task_untouched") {
        return;
    }
    let d = LiveDelivery::new(cx);
    // A queued Codex request and a tracked worker: the same dismissal intent
    // cancels each, while only a headless run can be aborted.
    let mut review = finished_review("cancel", &d, cx);
    review.status = DelegationStatus::Requested;
    review.result = None;
    let mut worker = worker_delegation("tracking", "worker", "worker-session");
    worker.parent = review.parent.clone();
    d.owner.update(cx, |owner, _| {
        let store = owner.host_mut().store_mut().as_mut().unwrap();
        store.insert_delegation(review).unwrap();
        store.insert_delegation(worker).unwrap();
    });
    let before = stored_task(&d.owner, "worker", cx);
    for (id, label) in [("cancel", "Cancel"), ("tracking", "Stop tracking")] {
        d.action(
            CardAction::Dismiss(gitterm_gpui_chat::interactions::DismissRequest {
                id: id.into(),
                label: label.into(),
                active: true,
            }),
            cx,
        );
        let record = d.record(id, cx);
        assert_eq!(record.status, DelegationStatus::Cancelled);
        assert!(record.dismissed_at.is_some());
    }
    assert_eq!(stored_task(&d.owner, "worker", cx), before);
    assert!(d.sent.borrow().is_empty());
    d.stop(cx);
}

#[gpui::test]
fn live_delivery_reaches_fake_parent_process_once(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_delivery_reaches_fake_parent_process_once") {
        return;
    }
    let mut d = LiveDelivery::new(cx);
    let saved = crate::chat::SavedChat {
        uid: "synthetic-requester".into(),
        cwd: d.git.outside.clone(),
        provider: d.fixture.roots.provider(),
        session_id: None,
        records: vec![],
    };
    let (pane, window) = cx.add_window_view(|window, cx| {
        crate::chat::ChatPane::history(crate::chat::RuntimeShutdown::new(), saved, window, cx)
    });
    let program = super::delivery_tests::fake_claude(d.fixture.iced.parent().unwrap());
    pane.update_in(window, |pane, window, cx| {
        pane.start_fake_runtime(d.fixture.roots.clone(), &program, window, cx)
    })
    .unwrap();
    d.parent = pane.read_with(cx, |pane, _| pane.target().session_uid);
    d.tab = pane.read_with(cx, |pane, _| pane.target().tab_id);
    d.chats.owner.update(cx, |owner, _| {
        owner.chats.clear();
        owner.chats.insert(d.parent.clone(), pane.clone());
    });
    d.owner.update(cx, |owner, _| owner.submit_hook = None);
    wait_until(cx, |cx| pane.read_with(cx, |pane, _| pane.runtime_ready()));
    d.result("real-channel", cx);
    // UID matches even though this is a fresh conversation.
    d.tick(cx);
    wait_until(cx, |_| d.git.outside.join("prompts.jsonl").exists());
    for _ in 0..3 {
        d.tick(cx);
    }
    assert_eq!(pane.read_with(cx, |pane, _| pane.user_prompts().len()), 1);
    let prompts = std::fs::read_to_string(d.git.outside.join("prompts.jsonl")).unwrap();
    assert_eq!(prompts.lines().count(), 1);
    assert!(prompts.contains("Finished real-channel"));
    assert!(d.record("real-channel", cx).is_delivered());
    pane.update(cx, |pane, cx| pane.close(cx));
    d.stop(cx);
}

#[gpui::test]
fn live_delivery_moved_head_matches_iced_banner_and_delivery(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_delivery_moved_head_matches_iced_banner_and_delivery") {
        return;
    }
    let d = LiveDelivery::new(cx);
    let mut review = finished_review("moved", &d, cx);
    let old_head = gitterm_core::delegations::current_head(&d.git.repo).unwrap();
    review
        .result
        .as_mut()
        .unwrap()
        .reviewed
        .as_mut()
        .unwrap()
        .head = old_head;
    git(
        &d.git.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "Moved head",
        ],
    );
    let new_head = gitterm_core::delegations::current_head(&d.git.repo).unwrap();
    let card = gitterm_gpui_chat::delegation::DelegationCard {
        delegation: review.clone(),
        activity: None,
        held: false,
        worker: None,
        checkout_head: Some(new_head),
    };
    assert!(card.parts().iter().any(|part| matches!(part, gitterm_gpui_chat::delegation::CardPart::Banner(text) if text.starts_with("The branch has moved since"))));
    d.insert_result(review, cx);
    d.tick(cx);
    assert!(d.record("moved", cx).is_delivered());
    d.tick(cx);
    assert_eq!(d.sent.borrow().len(), 1);
    d.stop(cx);
}

#[gpui::test]
fn live_card_cancel_stops_fake_running_consult(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    if child("live_card_cancel_stops_fake_running_consult") {
        return;
    }
    let d = LiveDelivery::new(cx);
    let program = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/bin/codex");
    let script = std::fs::read_to_string(&program).unwrap();
    // Keep the fake exec alive after its thread starts, so Cancel must stop
    // its actual process group and ignore the subsequent completion.
    let script = script.replace(
        "  echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'",
        "  exec /bin/sleep 60",
    );
    assert!(script.contains("  exec /bin/sleep 60"));
    std::fs::write(&program, script).unwrap();
    d.owner
        .read_with(cx, |owner, _| owner.claude_mcp_servers(&d.parent));
    let result = dispatch_as(
        cx,
        &d.sender,
        TaskControlOperation::RequestConsult(req(json!({
            "brief":"Wait for cancellation", "checkout":d.git.repo, "model":"placeholder"
        }))),
        Some(&d.parent),
    )
    .unwrap();
    let id = result["delegation_id"].as_str().unwrap();
    wait_until(cx, |cx| {
        d.record(id, cx).status == DelegationStatus::Running
    });
    let pid: u32 = d._fake.text("pid", 1).trim().parse().unwrap();
    d.action(
        CardAction::Dismiss(gitterm_gpui_chat::interactions::DismissRequest {
            id: id.into(),
            label: "Cancel".into(),
            active: true,
        }),
        cx,
    );
    wait_until(cx, |_| {
        gitterm_core::worker_process::macos::info(pid)
            .unwrap()
            .is_none()
    });
    let record = d.record(id, cx);
    assert_eq!(record.status, DelegationStatus::Cancelled);
    assert!(record.dismissed_at.is_some());
    d.tick(cx);
    assert!(d.sent.borrow().is_empty());
    d.stop(cx);
}
