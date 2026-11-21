use super::*;
use crate::delegation_request::ConsultDelegationRequest;
use crate::plan_limits::{evaluate_policy, LimitWindow, PlanLimits, PlanLimitsFile, Provider};
use crate::tasks::{DelegationResult, DelegationStatus, RepositoryIdentity, TaskHandoff};

fn caller() -> Caller {
    let tab = CallerTab {
        session_uid: "caller".into(),
        chat_session_id: Some("chat".into()),
        workspace: "fixture".into(),
        cwd: "/fixture/repo".into(),
        remote: false,
        home: "/fixture/repo".into(),
        repos: vec![],
    };
    Caller {
        target: crate::session::Target {
            tab_id: 1,
            session_uid: tab.session_uid.clone(),
            task_id: None,
            task_session_id: None,
        },
        workspace_uid: 1,
        local_root: Some(tab.home.clone()),
        tab,
    }
}
fn authority() -> Authority {
    Authority {
        checkout: "/fixture/repo".into(),
        repository: RepositoryIdentity {
            common_dir: "/fixture/repo/.git".into(),
            remote_url: None,
        },
        workdir: "/fixture/repo".into(),
        roots: vec![Some("/fixture/repo".into())],
    }
}

fn request(previous: Option<String>) -> Box<NewDelegation> {
    Box::new(
        delegations::consult_delegation(
            &ConsultDelegationRequest {
                brief: "Inspect parser".into(),
                ..Default::default()
            },
            &caller().tab,
            None,
            previous,
        )
        .unwrap(),
    )
}
fn record(id: &str) -> Delegation {
    let mut d = Delegation::new_requested(*request(None), "t0");
    d.delegation_id = id.into();
    d
}
fn stored(state: &mut State, id: &str) {
    state.reduce(Intent::Stored {
        id: id.into(),
        caller: caller(),
        authority: authority(),
    });
}
fn admit(
    state: &mut State,
    records: &[Delegation],
    cap: usize,
    limits: &PlanLimitsFile,
    workers: usize,
) -> Vec<Effect> {
    let effects = state.reduce(Intent::Admit {
        delegations: records,
        running: &HashSet::new(),
        callers: &HashMap::from([("caller".into(), caller())]),
        max_concurrent: cap,
        policy: &evaluate_policy(limits, 1_800_000_000),
        running_workers: workers,
    });
    let mut completed = Vec::new();
    for effect in effects {
        match effect {
            Effect::CheckStart { id, check, .. } => {
                completed.extend(state.reduce(Intent::StartChecked {
                    delegation: records.iter().find(|d| d.delegation_id == id).unwrap(),
                    check,
                    caller: Some(&caller()),
                    result: Ok(authority()),
                    running: &HashSet::new(),
                    max_concurrent: cap,
                    policy: &evaluate_policy(limits, 1_800_000_000),
                    running_workers: workers,
                }))
            }
            effect => completed.push(effect),
        }
    }
    completed
}
fn starts(effects: &[Effect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Start(id) | Effect::CheckStart { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}
fn hold() -> PlanLimitsFile {
    let mut file = PlanLimitsFile::default();
    file.absorb(PlanLimits {
        provider: Provider::Codex,
        observed_at: 1_800_000_000,
        local: Some(true),
        windows: vec![LimitWindow {
            label: "7d".into(),
            used_percent: Some(99.0),
            resets_at: Some(1_800_086_400),
        }],
        alerts: vec![],
        plan_type: None,
        credits: None,
    });
    file
}

#[test]
fn admission_reserves_before_launch_and_preserves_cap_policy_and_override() {
    let mut state = State::default();
    let mut records = vec![record("a"), record("b")];
    for d in &records {
        stored(&mut state, &d.delegation_id);
    }
    assert!(admit(&mut state, &records, 2, &hold(), 0).is_empty());
    records[1].options.user_model_choice = true;
    assert_eq!(
        starts(&admit(&mut state, &records, 0, &hold(), 0)),
        vec!["b"]
    );
    assert!(state.busy());
    assert!(admit(&mut state, &records, 1, &PlanLimitsFile::default(), 0).is_empty());
    // No host handle has been installed: reservation still owns the slot.
    assert_eq!(
        starts(&admit(
            &mut state,
            &records,
            2,
            &PlanLimitsFile::default(),
            0
        )),
        vec!["a"]
    );
    assert!(admit(&mut state, &records, 2, &PlanLimitsFile::default(), 0).is_empty());
}

#[test]
fn host_handles_and_reservations_are_counted_as_a_union() {
    let mut state = State::default();
    let records = vec![record("a"), record("b")];
    for d in &records {
        stored(&mut state, &d.delegation_id);
    }
    assert_eq!(
        starts(&admit(
            &mut state,
            &records[..1],
            1,
            &PlanLimitsFile::default(),
            0
        )),
        vec!["a"]
    );
    let effects = state.reduce(Intent::Admit {
        delegations: &records,
        running: &HashSet::from(["a".into()]),
        callers: &HashMap::from([("caller".into(), caller())]),
        max_concurrent: 2,
        policy: &evaluate_policy(&PlanLimitsFile::default(), 1_800_000_000),
        running_workers: 0,
    });
    assert_eq!(starts(&effects), vec!["b"]);
}

#[test]
fn checked_request_rejects_stale_incarnations_roots_and_duplicate_callbacks() {
    for change in 0..7 {
        let mut state = State::default();
        let Effect::Check {
            request: ticket,
            new,
            ..
        } = state
            .reduce(Intent::Request {
                new: request(None),
                caller: caller(),
            })
            .pop()
            .unwrap()
        else {
            panic!()
        };
        let mut live = caller();
        match change {
            0 => live.target.tab_id += 1,
            1 => live.tab.chat_session_id = Some("other".into()),
            2 => live.tab.cwd = "/other".into(),
            3 => live.tab.repos.push("/extra".into()),
            4 => live.tab.workspace = "moved".into(),
            5 => live.tab.remote = true,
            _ => live.local_root = Some("/other".into()),
        }
        assert!(matches!(
            state
                .reduce(Intent::Checked {
                    request: ticket,
                    new: new.clone(),
                    caller: Some(&live),
                    result: Ok(authority())
                })
                .as_slice(),
            [Effect::Rejected(_)]
        ));
        assert!(state
            .reduce(Intent::Checked {
                request: ticket,
                new,
                caller: Some(&caller()),
                result: Ok(authority())
            })
            .is_empty());
    }
}

#[test]
fn queued_admission_revalidates_the_original_caller() {
    let mut state = State::default();
    stored(&mut state, "a");
    let mut live = caller();
    live.target.tab_id += 1;
    let effects = state.reduce(Intent::Admit {
        delegations: &[record("a")],
        running: &HashSet::new(),
        callers: &HashMap::from([("caller".into(), live)]),
        max_concurrent: 1,
        policy: &evaluate_policy(&PlanLimitsFile::default(), 1_800_000_000),
        running_workers: 0,
    });
    assert!(matches!(effects.as_slice(), [Effect::StartRejected { .. }]));
    // Even a failed terminal save cannot make this stale request runnable.
    assert!(admit(&mut state, &[record("a")], 1, &PlanLimitsFile::default(), 0).is_empty());
}

#[test]
fn finish_cancel_and_rerun_are_idempotent_when_store_writes_fail() {
    for cancel in [false, true] {
        let mut state = State::default();
        let d = record("a");
        stored(&mut state, "a");
        assert_eq!(
            starts(&admit(
                &mut state,
                std::slice::from_ref(&d),
                1,
                &PlanLimitsFile::default(),
                0
            )),
            vec!["a"]
        );
        if cancel {
            assert!(state
                .reduce(Intent::Dismiss { delegation: &d })
                .iter()
                .any(|e| matches!(e, Effect::Abort(_))));
            assert!(!state
                .reduce(Intent::Dismiss { delegation: &d })
                .iter()
                .any(|e| matches!(e, Effect::Abort(_))));
            assert!(state.reduce(Intent::Finished { delegation: &d }).is_empty());
        } else {
            assert!(matches!(
                state.reduce(Intent::Finished { delegation: &d }).as_slice(),
                [Effect::RecordCompletion(_)]
            ));
            assert!(state.reduce(Intent::Finished { delegation: &d }).is_empty());
        }
        assert!(!state.busy());
        assert!(state
            .reduce(Intent::Progress {
                delegation: &d,
                event: CodexRunEvent::Activity {
                    description: "late".into()
                }
            })
            .is_empty());
        // Durable status is deliberately still Requested, modelling a failed
        // started/terminal/cancellation save or run-construction failure.
        assert!(admit(&mut state, &[d], 1, &PlanLimitsFile::default(), 0).is_empty());
        let Effect::Check {
            request: ticket,
            new,
            ..
        } = state
            .reduce(Intent::Request {
                new: request(Some("a".into())),
                caller: caller(),
            })
            .pop()
            .unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            state
                .reduce(Intent::Request {
                    new: new.clone(),
                    caller: caller()
                })
                .as_slice(),
            [Effect::Rejected(_)]
        ));
        assert!(matches!(
            state
                .reduce(Intent::Checked {
                    request: ticket,
                    new,
                    caller: Some(&caller()),
                    result: Ok(authority())
                })
                .as_slice(),
            [Effect::Insert { .. }]
        ));
        stored(&mut state, "rerun");
        assert_eq!(
            starts(&admit(
                &mut state,
                &[record("rerun")],
                1,
                &PlanLimitsFile::default(),
                0
            )),
            vec!["rerun"]
        );
    }
}

fn completed() -> Delegation {
    let mut d = record("done");
    d.status = DelegationStatus::Completed;
    d.result = Some(DelegationResult {
        handoff: Some(TaskHandoff {
            summary: "Parser inspected".into(),
            decisions: vec![],
            next_steps: vec![],
            blockers: vec![],
            updated_by_session_id: None,
            updated_at: "t1".into(),
        }),
        findings: None,
        reviewed: None,
    });
    d
}
fn flush(state: &mut State, d: &Delegation, busy: bool) -> Vec<Effect> {
    state.reduce(Intent::Flush {
        tab_id: 1,
        recipient_exists: true,
        delegations: &[d],
        streaming: busy,
        automatic: true,
    })
}
fn delivered(effects: &[Effect]) -> Vec<(RequestId, Vec<String>)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Deliver {
                delivery, batch, ..
            } => Some((*delivery, batch.delegation_ids.clone())),
            _ => None,
        })
        .collect()
}
fn submitted(state: &mut State, delivery: RequestId, accepted: bool) -> Vec<Effect> {
    state.reduce(Intent::Submitted {
        tab_id: 1,
        delivery,
        accepted,
    })
}
#[test]
fn delivery_is_held_until_turn_end_and_receipts_suppress_repeat_flushes() {
    let mut state = State::default();
    let mut d = completed();
    assert!(delivered(&flush(&mut state, &d, true)).is_empty());
    assert_eq!(state.held(1).unwrap().len(), 1);
    assert!(delivered(&flush(&mut state, &d, true)).is_empty());
    let out = delivered(&flush(&mut state, &d, false));
    assert_eq!(out.len(), 1);
    let (first, ids) = out[0].clone();
    assert_eq!(ids, ["done"]);
    assert_eq!(state.submitting(1), Some(first));
    // Reserved: a flush before the completion neither queues nor resubmits.
    assert!(delivered(&flush(&mut state, &d, false)).is_empty());
    assert!(state.held(1).unwrap().is_empty());
    assert!(state
        .reduce(Intent::ManualSend {
            tab_id: 1,
            delegation: &d,
            finding_ids: &[],
            streaming: false,
        })
        .is_empty());
    // Accepted: the host records the receipt. The store keeps it in memory
    // even when the disk save fails, so no later flush repeats it.
    assert!(matches!(
        submitted(&mut state, first, true).as_slice(),
        [Effect::RecordDelivery(ids)] if ids == &["done"]
    ));
    assert_eq!(state.submitting(1), None);
    assert!(submitted(&mut state, first, true).is_empty(), "stale");
    d.delivered = true;
    assert!(delivered(&flush(&mut state, &d, false)).is_empty());
    d.delivered = false;
    let (second, _) = delivered(&flush(&mut state, &d, false))[0].clone();
    assert_ne!(second, first);
    // A stale completion is ignored and keeps the reservation.
    assert!(submitted(&mut state, first, false).is_empty());
    assert_eq!(state.submitting(1), Some(second));
    // Rejected: the result stays undelivered and the next flush retries.
    assert!(matches!(
        submitted(&mut state, second, false).as_slice(),
        [Effect::Refresh(id)] if id == "done"
    ));
    assert_eq!(delivered(&flush(&mut state, &d, false)).len(), 1);
}

#[test]
fn manual_send_deduplicates_and_does_not_follow_a_rebound_parent() {
    let mut state = State::default();
    let d = completed();
    for _ in 0..2 {
        state.reduce(Intent::ManualSend {
            tab_id: 1,
            delegation: &d,
            finding_ids: &[],
            streaming: true,
        });
    }
    assert_eq!(state.held(1).unwrap().len(), 1);
    let effects = state.reduce(Intent::Flush {
        tab_id: 1,
        recipient_exists: true,
        delegations: &[],
        streaming: false,
        automatic: false,
    });
    assert!(!effects.iter().any(|e| matches!(e, Effect::Deliver { .. })));
    assert!(state.held(1).unwrap().is_empty());
}

#[test]
fn burst_policy_accounts_for_existing_workers_and_same_batch_admissions() {
    let mut policy = evaluate_policy(&PlanLimitsFile::default(), 1_800_000_000);
    policy.policy.codex.burst.busy = true;
    let records = vec![record("a"), record("b")];
    for workers in [0, 1] {
        let mut state = State::default();
        for d in &records {
            stored(&mut state, &d.delegation_id);
        }
        let effects = state.reduce(Intent::Admit {
            delegations: &records,
            running: &HashSet::new(),
            callers: &HashMap::from([("caller".into(), caller())]),
            max_concurrent: 2,
            policy: &policy,
            running_workers: workers,
        });
        assert_eq!(
            starts(&effects),
            if workers == 0 { vec!["a"] } else { vec![] }
        );
    }
}

#[test]
fn rerun_rebinds_a_fresh_authorized_request_but_keeps_its_checkout() {
    let mut old = record("old");
    old.parent.chat_session_id = None;
    old.parent.workspace = "previous name".into();
    let mut live = caller();
    live.tab.cwd = "/fixture/another".into();
    let new = rerun(&old, &live.tab).unwrap();
    assert_eq!(new.parent.chat_session_id, live.tab.chat_session_id);
    assert_eq!(new.parent.workspace, live.tab.workspace);
    assert_eq!(new.parent.cwd, live.tab.cwd);
    assert_eq!(
        new.options.checkout.as_deref(),
        Some(std::path::Path::new("/fixture/repo"))
    );
    assert!(matches!(
        State::default()
            .reduce(Intent::Request {
                new: Box::new(new),
                caller: live
            })
            .as_slice(),
        [Effect::Check { .. }]
    ));
}

fn reserve_check(state: &mut State, d: &Delegation) -> RequestId {
    let effects = state.reduce(Intent::Admit {
        delegations: std::slice::from_ref(d),
        running: &HashSet::new(),
        callers: &HashMap::from([("caller".into(), caller())]),
        max_concurrent: 1,
        policy: &evaluate_policy(&PlanLimitsFile::default(), 1_800_000_000),
        running_workers: 0,
    });
    let [Effect::CheckStart { check, .. }] = effects.as_slice() else {
        panic!("expected one preflight: {effects:?}");
    };
    *check
}
fn checked_start(
    state: &mut State,
    d: &Delegation,
    check: RequestId,
    result: Result<Authority, String>,
    limits: &PlanLimitsFile,
) -> Vec<Effect> {
    state.reduce(Intent::StartChecked {
        delegation: d,
        check,
        caller: Some(&caller()),
        result,
        running: &HashSet::new(),
        max_concurrent: 1,
        policy: &evaluate_policy(limits, 1_800_000_000),
        running_workers: 0,
    })
}

#[test]
fn preflight_rechecks_policy_and_ignores_cancelled_duplicate_and_superseded_tickets() {
    let mut state = State::default();
    let d = record("a");
    stored(&mut state, "a");
    let old = reserve_check(&mut state, &d);
    assert!(state.busy());
    assert!(matches!(
        checked_start(&mut state, &d, old, Ok(authority()), &hold()).as_slice(),
        [Effect::Refresh(_)]
    ));
    assert!(!state.busy());
    let current = reserve_check(&mut state, &d);
    assert_ne!(old, current);
    assert!(checked_start(
        &mut state,
        &d,
        old,
        Ok(authority()),
        &PlanLimitsFile::default()
    )
    .is_empty());
    assert_eq!(
        starts(&checked_start(
            &mut state,
            &d,
            current,
            Ok(authority()),
            &PlanLimitsFile::default()
        )),
        vec!["a"]
    );
    assert!(checked_start(
        &mut state,
        &d,
        current,
        Ok(authority()),
        &PlanLimitsFile::default()
    )
    .is_empty());
    state.reduce(Intent::Dismiss { delegation: &d });
    assert!(checked_start(
        &mut state,
        &d,
        current,
        Ok(authority()),
        &PlanLimitsFile::default()
    )
    .is_empty());

    let mut state = State::default();
    stored(&mut state, "a");
    let check = reserve_check(&mut state, &d);
    state.reduce(Intent::Dismiss { delegation: &d });
    assert!(checked_start(
        &mut state,
        &d,
        check,
        Ok(authority()),
        &PlanLimitsFile::default()
    )
    .is_empty());
}

#[test]
fn changed_or_failed_filesystem_preflight_cannot_launch_or_retry() {
    for fails in [false, true] {
        let mut state = State::default();
        let d = record("a");
        stored(&mut state, "a");
        let check = reserve_check(&mut state, &d);
        let mut changed = authority();
        changed.repository.common_dir = "/fixture/replaced/.git".into();
        let result = if fails {
            Err("checkout check failed".into())
        } else {
            Ok(changed)
        };
        assert!(matches!(
            checked_start(&mut state, &d, check, result, &PlanLimitsFile::default()).as_slice(),
            [Effect::StartRejected { .. }]
        ));
        assert!(admit(&mut state, &[d], 1, &PlanLimitsFile::default(), 0).is_empty());
    }
}
