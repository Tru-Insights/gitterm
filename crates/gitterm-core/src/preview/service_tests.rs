use super::*;
use crate::preview::ProtectedRoots;

struct Fixture {
    dir: tempfile::TempDir,
    protected: ProtectedRoots,
    preview: Arc<Preview>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let protected = ProtectedRoots::new(&dir.path().join("host"), []).unwrap();
    // A child's environment is only built over a root that exists.
    fs::create_dir_all(dir.path().join("preview")).unwrap();
    let preview =
        Arc::new(Preview::validate(&dir.path().join("preview"), protected.clone()).unwrap());
    Fixture {
        dir,
        protected,
        preview,
    }
}

fn fixture_repo(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    fs::create_dir_all(root.join("repo")).unwrap();
    fs::write(root.join(FIXTURE_MARKER), b"").unwrap();
    root
}

/// Every classified item: Existing/Read are allowed in preview; everything
/// else is denied with exactly the preview UI's notice.
fn assert_preview(context: &ServiceContext, classes: &[(Class, &str, Access)]) {
    for (class, label, access) in classes {
        assert_eq!((class.label, class.access), (*label, *access));
        let decision = context.authorize(*class, None);
        match access {
            Access::Existing | Access::Read => assert_eq!(decision, Ok(()), "{label}"),
            _ => assert_eq!(
                decision.unwrap_err().notice(),
                format!("{label} · not available in preview"),
            ),
        }
    }
}

#[cfg(feature = "runtime")]
#[test]
fn every_task_mcp_operation_is_classified_reads_allowed_writes_denied() {
    use crate::task_mcp::TaskControlOperation as Op;
    use serde_json::json;
    fn req<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let operations = [
        (Op::PlanLimits, "plan_limits", Access::Read),
        (
            Op::HtmlRender(req(json!({"html": "<p>x</p>", "height": 200}))),
            "html_render",
            Access::Read,
        ),
        (Op::List(req(json!({}))), "task_list", Access::Read),
        (
            Op::Get(req(json!({"task_id": "t"}))),
            "task_get",
            Access::Read,
        ),
        (
            Op::GetDelegation(req(json!({"delegation_id": "d"}))),
            "delegation_get",
            Access::Read,
        ),
        (
            Op::ListDelegations(req(json!({}))),
            "delegation_list",
            Access::Read,
        ),
        (Op::MergePlanGet, "merge_plan_get", Access::Read),
        (
            Op::Create(req(json!({
                "title": "t", "objective": "o", "repository_path": "/repo"
            }))),
            "task_create",
            Access::Write,
        ),
        (
            Op::UpdateHandoff(req(json!({"task_id": "t", "summary": "s"}))),
            "task_update_handoff",
            Access::Write,
        ),
        (
            Op::SessionEvent(crate::task_mcp::TaskSessionEventRequest {
                task_id: "t".into(),
                session_id: "s".into(),
                event_type: "agent-turn-complete".into(),
            }),
            "notify",
            Access::Write,
        ),
        (
            Op::MergePlanSet(req(json!({"entries": []}))),
            "merge_plan_set",
            Access::Write,
        ),
        (
            Op::LaunchSession(req(json!({"task_id": "t"}))),
            "task_launch_session",
            Access::Process,
        ),
        (
            Op::RequestReview(req(json!({"target": "uncommitted"}))),
            "review_request",
            Access::Process,
        ),
        (
            Op::RequestConsult(req(json!({"brief": "b"}))),
            "consult_request",
            Access::Process,
        ),
        (
            Op::DelegateTask(req(json!({"title": "t", "objective": "o"}))),
            "delegate_task",
            Access::Process,
        ),
    ];
    let classes: Vec<_> = operations
        .iter()
        .map(|(op, label, access)| (classify_operation(op), *label, *access))
        .collect();
    assert_preview(&context, &classes);
    assert_eq!(
        context
            .operation(&operations[7].0, None)
            .unwrap_err()
            .notice(),
        "task_create · not available in preview"
    );
    assert!(context.operation(&Op::PlanLimits, None).is_ok());
}

#[cfg(feature = "runtime")]
#[test]
fn every_delegation_effect_is_classified() {
    use crate::delegation_request::ConsultDelegationRequest;
    use crate::delegation_state::{Authority, Caller, Effect as E, Intent, State};
    use crate::delegations::{self, CallerTab, SendBatch};
    use crate::plan_limits::{evaluate_policy, PlanLimitsFile};
    use crate::tasks::{Delegation, RepositoryIdentity};
    use std::collections::{HashMap, HashSet};
    let tab = CallerTab {
        session_uid: "caller".into(),
        chat_session_id: Some("chat".into()),
        workspace: "fixture".into(),
        cwd: "/fixture/repo".into(),
        remote: false,
        home: "/fixture/repo".into(),
        repos: vec![],
    };
    let caller = Caller {
        target: session::Target {
            tab_id: 1,
            session_uid: "caller".into(),
            task_id: None,
            task_session_id: None,
        },
        workspace_uid: 1,
        local_root: Some(tab.home.clone()),
        tab: tab.clone(),
    };
    let authority = Authority {
        checkout: "/fixture/repo".into(),
        repository: RepositoryIdentity {
            common_dir: "/fixture/repo/.git".into(),
            remote_url: None,
        },
        workdir: "/fixture/repo".into(),
        roots: vec![],
    };
    let new = || {
        Box::new(
            delegations::consult_delegation(
                &ConsultDelegationRequest {
                    brief: "Inspect parser".into(),
                    ..Default::default()
                },
                &tab,
                None,
                None,
            )
            .unwrap(),
        )
    };
    // Effects with private request ids come from the reducer itself.
    let mut state = State::default();
    let check = state
        .reduce(Intent::Request {
            new: new(),
            caller: caller.clone(),
        })
        .pop()
        .unwrap();
    let E::Check { request, .. } = &check else {
        panic!("expected Check, got {check:?}")
    };
    let request = *request;
    let insert = state
        .reduce(Intent::Checked {
            request,
            new: new(),
            caller: Some(&caller),
            result: Ok(authority.clone()),
        })
        .pop()
        .unwrap();
    let mut record = Delegation::new_requested(*new(), "t0");
    record.delegation_id = "queued".into();
    state.reduce(Intent::Stored {
        id: "queued".into(),
        caller: caller.clone(),
        authority,
    });
    let check_start = state
        .reduce(Intent::Admit {
            delegations: std::slice::from_ref(&record),
            running: &HashSet::new(),
            callers: &HashMap::from([("caller".into(), caller.clone())]),
            max_concurrent: 1,
            policy: &evaluate_policy(&PlanLimitsFile::default(), 1_800_000_000),
            running_workers: 0,
        })
        .pop()
        .unwrap();
    let effects = [
        (check, "Check delegation checkout", Access::Read),
        (E::Refresh("d".into()), "Refresh delegation", Access::Read),
        (E::Rejected("no".into()), "Reject delegation", Access::Read),
        (E::AlreadyDelivered, "Delivery", Access::Read),
        (insert, "Save delegation", Access::Write),
        (
            E::StartRejected {
                id: "d".into(),
                error: "e".into(),
            },
            "Save delegation",
            Access::Write,
        ),
        (
            E::RecordStarted {
                id: "d".into(),
                thread_id: "th".into(),
            },
            "Save delegation",
            Access::Write,
        ),
        (
            E::RecordCompletion("d".into()),
            "Save delegation",
            Access::Write,
        ),
        (
            E::PersistDismiss {
                id: "d".into(),
                cancel: true,
            },
            "Dismiss delegation",
            Access::Write,
        ),
        (
            E::RecordDelivery(vec!["d".into()]),
            "Save delivery",
            Access::Write,
        ),
        (check_start, "Start queued delegation", Access::Process),
        (E::Start("d".into()), "Start delegation", Access::Process),
        (E::Abort("d".into()), "Stop delegation", Access::Process),
        (E::Flush(1), "Deliver results", Access::Process),
        (
            E::Deliver {
                tab_id: 1,
                delivery: request,
                batch: SendBatch {
                    delegation_ids: vec!["d".into()],
                    message: "m".into(),
                },
            },
            "Deliver results",
            Access::Process,
        ),
    ];
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let classes: Vec<_> = effects
        .iter()
        .map(|(effect, label, access)| (classify_delegation_effect(effect), *label, *access))
        .collect();
    assert_preview(&context, &classes);
    assert!(context.delegation_effect(&effects[11].0, None).is_err());
}

#[cfg(feature = "runtime")]
#[test]
fn every_task_action_and_session_effect_is_classified() {
    use crate::task_commands::*;
    use crate::tasks::*;
    let record = crate::delegation_test_support::test_task("t", Path::new("/fixture/wt"));
    let prepared = || CompletedWorktreePreparation {
        repository: RepositoryIdentity {
            common_dir: "/fixture/repo/.git".into(),
            remote_url: None,
        },
        base: GitBase {
            reference: "main".into(),
            commit: "abc".into(),
        },
        branch: "task/t".into(),
        path: "/fixture/wt".into(),
    };
    let target = || session::Target {
        tab_id: 1,
        session_uid: "s".into(),
        task_id: Some("t".into()),
        task_session_id: None,
    };
    let launch = || SessionLaunch {
        task_id: "t".into(),
        preset_name: None,
        origin: TaskLaunchOrigin::Agent,
        user_model_choice: false,
    };
    let id = WorkerPreparationId(1);
    use TaskMutationAction as A;
    let actions = [
        (A::None, "Task", Access::Read),
        (
            A::InspectCleanup {
                task_id: "t".into(),
                repository: "/fixture/repo".into(),
                path: "/fixture/wt".into(),
                branch: "task/t".into(),
                base_commit: "abc".into(),
                process_running: false,
                pull_request_url: None,
            },
            "Inspect worktree",
            Access::Read,
        ),
        (
            A::ConfirmPublish {
                task_id: "t".into(),
                action: TaskPublish::PushBranch,
            },
            "Publish",
            Access::Read,
        ),
        (
            A::WorkerAuthorityChecked { id },
            "Check worker",
            Access::Read,
        ),
        (A::Session(session::Effect::None), "Session", Access::Read),
        (
            A::Session(session::Effect::ConfirmClose(target())),
            "Close session",
            Access::Read,
        ),
        (
            A::PrepareTask(TaskCreate {
                title: "t".into(),
                objective: "o".into(),
                repository_path: "/fixture/repo".into(),
                workspace_name: None,
                issue_key: None,
                base_reference: None,
                stopping_boundary: StoppingBoundary::PlanOnly,
                origin: TaskCreateOrigin::Control,
            }),
            "Create task",
            Access::Write,
        ),
        (
            A::PersistPreparing {
                record: Box::new(record.clone()),
            },
            "Save task",
            Access::Write,
        ),
        (
            A::PersistPreparation {
                task_id: "t".into(),
                prepared: prepared(),
            },
            "Save task",
            Access::Write,
        ),
        (
            A::PersistDelivery {
                task_id: "t".into(),
                delivery: DeliveryState::default(),
            },
            "Save delivery",
            Access::Write,
        ),
        (
            A::PersistHandoff {
                task_id: "t".into(),
                handoff: TaskHandoff {
                    summary: "s".into(),
                    decisions: vec![],
                    next_steps: vec![],
                    blockers: vec![],
                    updated_by_session_id: None,
                    updated_at: "t0".into(),
                },
                status: HandoffStatus::Done,
                hosted_session: None,
            },
            "Save handoff",
            Access::Write,
        ),
        (
            A::ArchiveTask {
                task_id: "t".into(),
            },
            "Close task",
            Access::Write,
        ),
        (
            A::RemoveWorktree {
                task_id: "t".into(),
                repository: "/fixture/repo".into(),
                path: "/fixture/wt".into(),
            },
            "Remove worktree",
            Access::Write,
        ),
        (
            A::ClearWorktree {
                task_id: "t".into(),
            },
            "Remove worktree",
            Access::Write,
        ),
        (
            A::QueueSession {
                request: launch(),
                preset_index: 0,
                position: 0,
            },
            "Queue session",
            Access::Write,
        ),
        (
            A::PersistWorkerPreparation {
                id,
                task_id: "t".into(),
                prepared: prepared(),
            },
            "Save task",
            Access::Write,
        ),
        (
            A::PersistWorkerPreparationFailure {
                task_id: "t".into(),
                error: "e".into(),
                cleanup_path: None,
            },
            "Save task",
            Access::Write,
        ),
        (
            A::PersistWorkerCheckoutReuse {
                id,
                replacement: Box::new(record),
            },
            "Save task",
            Access::Write,
        ),
        (
            A::Session(session::Effect::Close(target())),
            "Close session",
            Access::Process,
        ),
        (
            A::Session(session::Effect::InterruptHarness(target())),
            "Stop turn",
            Access::Process,
        ),
        (
            A::Session(session::Effect::CancelTaskTurn(target())),
            "Stop turn",
            Access::Process,
        ),
        (
            A::LaunchSession {
                request: launch(),
                preset_index: None,
            },
            "Launch session",
            Access::Process,
        ),
        (
            A::StartWorker {
                task_id: "t".into(),
                caller: Box::new(WorkerCaller {
                    target: target(),
                    chat_session_id: None,
                    workspace: WorkspaceIdentity {
                        name: "fixture".into(),
                        location: WorkspaceLocationIdentity::Local {
                            directory: "/fixture/repo".into(),
                        },
                    },
                    repository: None,
                    repository_workdir: None,
                    checkout: "/fixture/repo".into(),
                    home: "/fixture/repo".into(),
                    repos: vec![],
                    remote: false,
                }),
            },
            "Start worker",
            Access::Process,
        ),
        (
            A::Publish {
                task_id: "t".into(),
                action: TaskPublish::OpenDraftPr,
                path: "/fixture/wt".into(),
                branch: "task/t".into(),
                base: "main".into(),
                title: "t".into(),
                body: "b".into(),
            },
            "Publish",
            Access::Outside,
        ),
    ];
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let classes: Vec<_> = actions
        .iter()
        .map(|(action, label, access)| (classify_task_action(action), *label, *access))
        .collect();
    assert_preview(&context, &classes);
    assert!(context.task_action(&actions[0].0, None).is_ok());
}

#[test]
fn runner_process_and_browser_effects_are_classified() {
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let runners = [
        (RunnerEffect::HtmlPreview, "html_preview", Access::Outside),
        (RunnerEffect::NotifyIngress, "notify", Access::Write),
        (
            RunnerEffect::RestoreSession,
            "Restore session",
            Access::Outside,
        ),
        (
            RunnerEffect::QueuePromotion,
            "Start queued session",
            Access::Process,
        ),
        (
            RunnerEffect::AutomaticDelivery,
            "Automatic delivery",
            Access::Process,
        ),
    ];
    let processes = [
        (ProcessEffect::Terminal, "Terminal", Access::Existing),
        (ProcessEffect::Git, "Git", Access::Existing),
        (ProcessEffect::Snapshot, "Snapshot", Access::Write),
        (ProcessEffect::MutateHistory, "Edit history", Access::Write),
        (ProcessEffect::Agent, "Agent", Access::Process),
        (ProcessEffect::Worker, "Worker", Access::Process),
        (ProcessEffect::Helper, "Helper", Access::Process),
        (ProcessEffect::Resume, "Resume session", Access::Outside),
        (
            ProcessEffect::RestoreProcess,
            "Restore process",
            Access::Outside,
        ),
    ];
    let mut classes: Vec<_> = runners
        .iter()
        .map(|(e, label, access)| (classify_runner(*e), *label, *access))
        .collect();
    classes.extend(
        processes
            .iter()
            .map(|(e, label, access)| (classify_process(*e), *label, *access)),
    );
    assert_preview(&context, &classes);
    assert_eq!(
        context
            .runner(RunnerEffect::HtmlPreview, None)
            .unwrap_err()
            .to_string(),
        "html_preview · not available in preview"
    );
    // The existing stage 1–3 gate agrees on every process effect.
    for (effect, _, access) in processes {
        assert_eq!(
            f.preview
                .capabilities()
                .authorize(effect, Path::new("/workspace"))
                .is_ok(),
            access == Access::Existing
        );
    }
    assert!(context
        .browser("browser_snapshot", BrowserAccess::Read)
        .is_ok());
    for (tool, access) in [
        ("browser_open", BrowserAccess::Launch),
        ("browser_click", BrowserAccess::Interact),
        ("browser_network_capture", BrowserAccess::Scratch),
    ] {
        assert_eq!(
            context.browser(tool, access).unwrap_err().notice(),
            format!("{tool} · not available in preview")
        );
    }
}

#[test]
fn every_pr_action_run_is_outside_and_denied_in_preview() {
    use crate::merge_plan::{MergeMethod, PrKey};
    use crate::prs::PrAction;
    use crate::work::pr_actions::RunPrAction;
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let mut actions = vec![
        (PrAction::Approve, "Approve"),
        (PrAction::Ready, "Ready for review"),
    ];
    actions.extend(MergeMethod::ALL.map(|method| (PrAction::Merge(method), "Merge")));
    let classes: Vec<_> = actions
        .iter()
        .map(|(action, label)| (classify_pr_action(*action), *label, Access::Outside))
        .collect();
    assert_preview(&context, &classes);
    let run = RunPrAction {
        action: PrAction::Merge(MergeMethod::Squash),
        key: PrKey::new("example-org/example-repo", 7),
        account: None,
    };
    // The same notice the GPUI preview panel shows for its verb button.
    assert_eq!(
        context.pr_action(&run).unwrap_err().notice(),
        "Merge · not available in preview"
    );
    let production = ServiceContext::production(f.dir.path(), "fixture").unwrap();
    assert!(production.pr_action(&run).is_ok());
}

#[test]
fn phone_create_and_revoke_are_outside_and_denied_in_preview() {
    use crate::remote::phone_panel::{PhoneCall, PhoneRequest};
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let create = PhoneRequest::Create {
        device_name: "iPhone".into(),
        scope: "act",
    };
    let revoke = PhoneRequest::Revoke { id: "p1".into() };
    assert_preview(
        &context,
        &[
            (
                classify_phone_request(&create),
                "Show pairing code",
                Access::Outside,
            ),
            (classify_phone_request(&revoke), "Revoke", Access::Outside),
            (
                classify_phone_request(&PhoneRequest::List),
                "Paired phones",
                Access::Read,
            ),
        ],
    );
    let call = |request| PhoneCall {
        remote_id: "fixture".to_string(),
        request,
        client: (),
    };
    // The same notice the GPUI preview panel shows for its create button.
    assert_eq!(
        context.phone(&call(create.clone())).unwrap_err().notice(),
        "Show pairing code · not available in preview"
    );
    assert!(context.phone(&call(PhoneRequest::List)).is_ok());
    let production = ServiceContext::production(f.dir.path(), "fixture").unwrap();
    for request in [create, revoke, PhoneRequest::List] {
        assert!(production.phone(&call(request)).is_ok());
    }
}

#[cfg(unix)]
#[test]
fn context_only_comes_from_a_validated_root_and_rejects_real_v6_roots() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    let config = f.dir.path().join("host/.config");
    fs::create_dir_all(config.join("gitterm-v6")).unwrap();
    let alias = f.dir.path().join("alias");
    symlink(&config, &alias).unwrap();
    for root in [
        config.join("gitterm-v6"),
        config.join("gitterm-v6/preview"),
        alias.join("gitterm-v6/preview"),
        f.dir.path().join("host"),
    ] {
        assert!(
            Preview::validate(&root, f.protected.clone()).is_err(),
            "{}",
            root.display()
        );
    }
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    assert!(context.is_preview());
    assert_eq!(context.state_root(), f.preview.root());
    assert_eq!(
        context.browser_profile_dir().unwrap(),
        f.preview.root().join("browser-profile")
    );
    // A root swapped for a link to real V6 after validation fails closed.
    fs::rename(f.preview.root(), f.dir.path().join("moved")).ok();
    symlink(config.join("gitterm-v6"), f.preview.root()).unwrap();
    assert!(ServiceContext::preview(f.preview.clone()).is_err());
    assert!(context.browser_profile_dir().is_err());
}

#[test]
fn child_env_scrubs_production_service_env_and_pins_the_config_dir() {
    use crate::task_commands::mcp_protocol::*;
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let production = [
        TASK_MCP_URL_ENV,
        TASK_MCP_TOKEN_ENV,
        BROWSER_MCP_URL_ENV,
        BROWSER_MCP_TOKEN_ENV,
        CONFIG_DIR_ENV,
        "GITTERM_V6_AGENT_TOKEN",
        "GITTERM_V6_AGENT_ADDR",
        "GITTERM_V6_INSTANCE_ID",
        "GITTERM_GPUI_CONFIG_DIR",
    ];
    let inherited = production
        .iter()
        .map(|name| (OsString::from(name), OsString::from("production-secret")))
        .chain([("PATH".into(), "/usr/bin".into())]);
    let env = context.child_env(inherited).unwrap();
    assert!(env.iter().all(|(_, value)| value != "production-secret"));
    for name in production.iter().filter(|name| **name != CONFIG_DIR_ENV) {
        assert!(env.iter().all(|(key, _)| key != name), "{name}");
    }
    assert!(env.contains(&("PATH".into(), "/usr/bin".into())));
    let pinned: Vec<_> = env
        .iter()
        .filter(|(key, _)| key == CONFIG_DIR_ENV)
        .collect();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].1, f.preview.root().as_os_str());
    for name in [
        TASK_MCP_URL_ENV,
        TASK_MCP_TOKEN_ENV,
        BROWSER_MCP_URL_ENV,
        BROWSER_MCP_TOKEN_ENV,
    ] {
        assert!(is_scrubbed(OsStr::new(name)));
    }
    assert!(!is_scrubbed(OsStr::new("HOME")));
}

#[test]
fn preview_child_env_pins_provider_homes_and_drops_browser_control_env() {
    let f = fixture();
    let context = ServiceContext::preview(f.preview.clone()).unwrap();
    let inherited = [
        ("CODEX_HOME", "/real/codex"),
        ("CLAUDE_CONFIG_DIR", "/real/claude"),
        ("AGENT_BROWSER_HOME", "/real/browser"),
        ("HOME", "/Users/someone"),
    ]
    .map(|(name, value)| (OsString::from(name), OsString::from(value)));
    let env = context.child_env(inherited).unwrap();
    assert!(env
        .iter()
        .all(|(_, value)| !value.to_string_lossy().starts_with("/real/")));
    assert!(env.iter().all(|(key, _)| key != "AGENT_BROWSER_HOME"));
    for (name, dir) in PROVIDER_HOME_ENV {
        let values: Vec<_> = env.iter().filter(|(key, _)| key == name).collect();
        assert_eq!(values.len(), 1, "{name}");
        assert_eq!(values[0].1, f.preview.root().join(dir).as_os_str());
    }
    // HOME stays (shells and git need it); see the shortcut note in child_env.
    assert!(env.contains(&("HOME".into(), "/Users/someone".into())));
}

#[cfg(unix)]
#[test]
fn fixture_launch_needs_marked_temp_roots_and_leaves_preview_unchanged() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    let preview = || f.preview.clone();
    let good = fixture_repo(f.dir.path(), "fixture");
    let unmarked = f.dir.path().join("unmarked");
    fs::create_dir_all(&unmarked).unwrap();
    let linked_marker = f.dir.path().join("linked-marker");
    fs::create_dir_all(&linked_marker).unwrap();
    symlink(
        good.join(FIXTURE_MARKER),
        linked_marker.join(FIXTURE_MARKER),
    )
    .unwrap();
    let dir_marker = f.dir.path().join("dir-marker");
    fs::create_dir_all(dir_marker.join(FIXTURE_MARKER)).unwrap();
    let protected = f.dir.path().join("host/.config/gitterm-v6");
    fs::create_dir_all(&protected).unwrap();
    fs::write(protected.join(FIXTURE_MARKER), b"").unwrap();
    // A real checkout outside the temp dir, reached through a link inside it.
    let outside = std::env::current_dir().unwrap();
    let link_out = f.dir.path().join("link-out");
    symlink(&outside, &link_out).unwrap();
    for bad in [
        vec![],
        vec![unmarked],
        vec![linked_marker],
        vec![dir_marker],
        vec![protected],
        vec![link_out.clone()],
        vec![outside.clone()],
        vec![std::env::temp_dir()],
        vec![PathBuf::from("relative")],
    ] {
        assert!(
            ServiceContext::fixture_launch(preview(), bad.clone()).is_err(),
            "{bad:?}"
        );
    }

    let normal = ServiceContext::preview(preview()).unwrap();
    let fixture = ServiceContext::fixture_launch(preview(), [good.clone()]).unwrap();
    let launch = classify_runner(RunnerEffect::QueuePromotion);
    let inside = good.join("repo");
    fs::create_dir_all(good.join("escape-parent")).unwrap();
    symlink(&outside, good.join("escape-parent/escape")).unwrap();
    assert!(fixture.authorize(launch, Some(&inside)).is_ok());
    assert!(fixture
        .process(ProcessEffect::Worker, Some(&inside.join("missing/tail")))
        .is_ok());
    for target in [
        None,
        Some(outside.as_path()),
        Some(f.preview.root()),
        Some(&*good.join("escape-parent/escape")),
    ] {
        assert_eq!(
            fixture.authorize(launch, target).unwrap_err().notice(),
            "Start queued session · not available in preview"
        );
    }
    for effect in [RunnerEffect::HtmlPreview, RunnerEffect::RestoreSession] {
        assert!(fixture.runner(effect, Some(&inside)).is_err());
    }
    assert!(fixture
        .browser("browser_open", BrowserAccess::Launch)
        .is_err());
    // Building the fixture context changed nothing for normal preview.
    assert!(normal.authorize(launch, Some(&inside)).is_err());
    assert!(ServiceContext::preview(preview())
        .unwrap()
        .authorize(launch, Some(&inside))
        .is_err());
}

#[test]
fn production_is_a_distinct_constructor_that_allows_everything() {
    let f = fixture();
    let real = f.dir.path().join("host/.config/gitterm-v6");
    assert!(ServiceContext::production(Path::new("relative"), "1").is_err());
    // Iced accepts an absolute override containing `..`; production starts there too.
    let dotted = f.dir.path().join("host/.config/x/../gitterm-v6");
    assert_eq!(
        ServiceContext::production(&dotted, "1")
            .unwrap()
            .state_root(),
        dotted
    );
    let production = ServiceContext::production(&real, "123").unwrap();
    assert!(!production.is_preview());
    assert_eq!(production.state_root(), real);
    assert_eq!(production.task_mcp().instance_id(), "123");
    assert_eq!(
        production.browser_profile_dir().unwrap(),
        real.join("browser-profile")
    );
    for class in [
        classify_runner(RunnerEffect::HtmlPreview),
        classify_runner(RunnerEffect::RestoreSession),
        classify_runner(RunnerEffect::AutomaticDelivery),
        classify_process(ProcessEffect::Worker),
    ] {
        assert!(production.authorize(class, None).is_ok());
    }
    assert!(production
        .browser("browser_open", BrowserAccess::Launch)
        .is_ok());

    let preview = ServiceContext::preview(f.preview.clone()).unwrap();
    let other = ServiceContext::preview(f.preview.clone()).unwrap();
    assert!(preview
        .task_mcp()
        .instance_id()
        .starts_with("gpui-preview-"));
    assert_ne!(preview.task_mcp().token(), other.task_mcp().token());
    assert_ne!(preview.task_mcp().token(), production.task_mcp().token());
    assert!(!format!("{preview:?}").contains(preview.task_mcp().token()));
}

/// The least Git accepts as a Git dir; a common dir needs `objects` and
/// `refs` too.
fn git_dir_skeleton(dir: &Path) {
    fs::create_dir_all(dir.join("objects")).unwrap();
    fs::create_dir_all(dir.join("refs")).unwrap();
    fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
}

/// A linked-worktree layout: `checkout/.git` is a `gitdir:` file naming
/// `git_dir`, whose `commondir` (when given) names the shared repository.
fn worktree_layout(checkout: &Path, git_dir: &Path, commondir: Option<&str>) {
    fs::create_dir_all(checkout).unwrap();
    git_dir_skeleton(git_dir);
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    if let Some(common) = commondir {
        fs::write(git_dir.join("commondir"), format!("{common}\n")).unwrap();
    }
}

const WORKER_DENIED: &str = "Worker · not available in preview";

#[test]
fn fixture_gitdir_file_into_protected_root_refused() {
    let f = fixture();
    let protected = f.dir.path().join("host/.config/gitterm-v6/worktrees/admin");
    // The fixture root itself is a worktree whose admin dir is protected state.
    let root = fixture_repo(f.dir.path(), "fixture");
    worktree_layout(&root, &protected, None);
    assert!(ServiceContext::fixture_launch(f.preview.clone(), [root]).is_err());

    // A checkout inside an accepted fixture, redirected into protected state
    // after the context was built: the decision repeats the check.
    let good = fixture_repo(f.dir.path(), "good");
    let context = ServiceContext::fixture_launch(f.preview.clone(), [good.clone()]).unwrap();
    let checkout = good.join("repo");
    assert!(context
        .process(ProcessEffect::Worker, Some(&checkout))
        .is_ok());
    worktree_layout(&checkout, &protected, None);
    for target in [checkout.clone(), checkout.join("src/missing.rs")] {
        assert_eq!(
            context
                .process(ProcessEffect::Worker, Some(&target))
                .unwrap_err()
                .notice(),
            WORKER_DENIED,
            "{}",
            target.display()
        );
    }
    // The same layout with its admin dir inside the fixture is allowed.
    worktree_layout(&checkout, &good.join("admin/worktrees/repo"), None);
    assert!(context
        .process(ProcessEffect::Worker, Some(&checkout))
        .is_ok());
    // A malformed `.git` file is refused, not ignored.
    fs::write(checkout.join(".git"), b"not a pointer").unwrap();
    assert!(context
        .process(ProcessEffect::Worker, Some(&checkout))
        .is_err());
}

#[test]
fn fixture_commondir_outside_temp_refused() {
    let f = fixture();
    // A real checkout outside the temp dir.
    let outside = std::env::current_dir().unwrap();
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(!fs::canonicalize(&outside).unwrap().starts_with(&temp));
    let outside_text = outside.display().to_string();
    for commondir in [
        outside_text.as_str(),
        // Relative, as Git writes it, but climbing out of the fixture.
        "../../../..",
        // Names nothing: refused rather than treated as absent.
        "../missing",
    ] {
        let root = fixture_repo(f.dir.path(), "fixture");
        worktree_layout(&root, &root.join("common/worktrees/wt"), Some(commondir));
        assert!(
            ServiceContext::fixture_launch(f.preview.clone(), [root.clone()]).is_err(),
            "{commondir}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    // Git's own layout (relative commondir inside the fixture) is accepted,
    // as a root and as a target.
    let root = fixture_repo(f.dir.path(), "fixture");
    git_dir_skeleton(&root.join("common"));
    worktree_layout(&root, &root.join("common/worktrees/wt"), Some("../.."));
    let context = ServiceContext::fixture_launch(f.preview.clone(), [root.clone()]).unwrap();
    let checkout = root.join("repo");
    worktree_layout(
        &checkout,
        &root.join("common/worktrees/repo"),
        Some("../.."),
    );
    assert!(context
        .process(ProcessEffect::Worker, Some(&checkout))
        .is_ok());
    // The checkout's commondir now points at the real checkout.
    fs::write(
        root.join("common/worktrees/repo/commondir"),
        format!("{outside_text}\n"),
    )
    .unwrap();
    assert_eq!(
        context
            .process(ProcessEffect::Worker, Some(&checkout))
            .unwrap_err()
            .notice(),
        WORKER_DENIED
    );
}

/// Git from the test, clear of the caller's repository and config.
fn git(dir: &Path, args: &[&str]) -> String {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_none_or(|name| name.starts_with("GIT_")) {
            command.env_remove(name);
        }
    }
    let output = command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// A real repository beside the fixture (outside it, inside the temp dir).
fn outside_repo(dir: &Path) -> PathBuf {
    let outside = dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    git(&outside, &["init", "-q", "-b", "main"]);
    git(&outside, &["commit", "-q", "--allow-empty", "-m", "x"]);
    fs::canonicalize(outside).unwrap()
}

#[test]
fn fixture_gitdir_with_embedded_newline_refused() {
    let f = fixture();
    let outside = outside_repo(f.dir.path());
    let outside_head = fs::read(outside.join(".git/HEAD")).unwrap();
    let root = fixture_repo(f.dir.path(), "fixture");
    let context = ServiceContext::fixture_launch(f.preview.clone(), [root.clone()]).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let wt = root.join("wt");
    fs::create_dir_all(&wt).unwrap();
    fs::create_dir_all(root.join("admin")).unwrap();
    fs::create_dir_all(root.join("admin\n")).unwrap();
    // The first line names `admin`, inside the fixture; Git reads the whole
    // file and lands in the outside repository.
    let admin = root.join("admin");
    fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n/../../outside/.git\n", admin.display()),
    )
    .unwrap();
    // The layout is a real escape: Git (only reading) resolves it outside.
    let resolved = git(&wt, &["rev-parse", "--absolute-git-dir"]);
    assert_eq!(Path::new(resolved.trim_end()), outside.join(".git"));
    for target in [wt.clone(), wt.join("src/missing.rs")] {
        assert_eq!(
            context
                .process(ProcessEffect::Worker, Some(&target))
                .unwrap_err()
                .notice(),
            WORKER_DENIED
        );
    }
    assert!(ServiceContext::fixture_launch(f.preview.clone(), [wt.clone()]).is_err());

    // Any other control character in a `.git` file or `commondir` is refused
    // too, a CRLF ending included.
    git_dir_skeleton(&admin);
    for text in [
        format!("gitdir: {}\r\n", admin.display()),
        format!("gitdir: {}\0\n", admin.display()),
        format!("gitdir: {}\n\n", admin.display()),
    ] {
        fs::write(wt.join(".git"), text.as_bytes()).unwrap();
        assert!(
            context.process(ProcessEffect::Worker, Some(&wt)).is_err(),
            "{text:?}"
        );
    }
    fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    assert!(context.process(ProcessEffect::Worker, Some(&wt)).is_ok());
    fs::write(admin.join("commondir"), ".\n/../../../outside/.git\n").unwrap();
    assert!(context.process(ProcessEffect::Worker, Some(&wt)).is_err());

    // Deciding never wrote through the fixture path.
    assert_eq!(fs::read(outside.join(".git/HEAD")).unwrap(), outside_head);
}

#[cfg(unix)]
#[test]
fn fixture_admin_dirs_resolved_by_git() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    let outside = outside_repo(f.dir.path());
    let root = fixture_repo(f.dir.path(), "fixture");
    let context = ServiceContext::fixture_launch(f.preview.clone(), [root.clone()]).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let worker = |target: &Path| context.process(ProcessEffect::Worker, Some(target));

    // Git's own layout, a repository and a linked worktree, is allowed.
    let repo = root.join("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    git(&repo, &["worktree", "add", "-q", "../wt", "-b", "wt"]);
    assert!(worker(&repo.join("src")).is_ok());
    assert!(worker(&root.join("wt")).is_ok());

    // Git keeps a trailing space that a trimming parser drops: `link `
    // names the outside repository, `link` an admin dir inside.
    let checkout = root.join("spaced");
    fs::create_dir_all(&checkout).unwrap();
    git_dir_skeleton(&root.join("link"));
    symlink(outside.join(".git"), root.join("link ")).unwrap();
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {} \n", root.join("link").display()),
    )
    .unwrap();
    assert!(worker(&checkout).is_err());

    // A Git dir with no `.git` at all: its own `commondir` names the outside
    // repository, so Git writes refs there.
    let bare = root.join("bare");
    fs::create_dir_all(&bare).unwrap();
    fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        bare.join("commondir"),
        format!("{}\n", outside.join(".git").display()),
    )
    .unwrap();
    let common = git(&bare, &["rev-parse", "--git-common-dir"]);
    assert_eq!(Path::new(common.trim_end()), outside.join(".git"));
    assert!(worker(&bare).is_err());
    assert!(worker(&bare.join("refs/heads")).is_err());
    // The same Git dir sharing a common dir inside the fixture is allowed.
    fs::write(bare.join("commondir"), "../repo/.git\n").unwrap();
    assert!(worker(&bare).is_ok());
}

#[cfg(unix)]
#[test]
fn child_env_refuses_swapped_root() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    let preview = ServiceContext::preview(f.preview.clone()).unwrap();
    let fixture =
        ServiceContext::fixture_launch(f.preview.clone(), [fixture_repo(f.dir.path(), "fixture")])
            .unwrap();
    let production = ServiceContext::production(f.preview.root(), "1").unwrap();
    for context in [&preview, &fixture, &production] {
        assert!(context.child_env([]).is_ok());
    }
    // Moved away: nothing is at the root any more.
    let moved = f.dir.path().join("moved");
    fs::rename(f.preview.root(), &moved).unwrap();
    assert!(preview.child_env([]).is_err());
    assert!(fixture.child_env([]).is_err());
    // Replaced by a link, even to the directory it was.
    symlink(&moved, f.preview.root()).unwrap();
    assert!(preview.child_env([]).is_err());
    assert!(fixture.child_env([]).is_err());
    // Production pins whatever root it was given, as before.
    assert!(production.child_env([]).is_ok());
}

#[cfg(unix)]
#[test]
fn path_swapped_after_prep_refused() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    let root = fixture_repo(f.dir.path(), "fixture");
    let context = ServiceContext::fixture_launch(f.preview.clone(), [root.clone()]).unwrap();
    let worker = classify_process(ProcessEffect::Worker);
    let checkout = root.join("repo");
    let other = root.join("other");
    fs::create_dir_all(&other).unwrap();
    let outside = std::env::current_dir().unwrap();

    let granted = context
        .authorize_targets(worker, &[checkout.clone(), root.join("notes")])
        .unwrap();
    assert_eq!(context.recheck(&granted), Ok(()));
    // Swapped for a link to another folder of the same fixture: still
    // allowed on its own, but no longer the folder that was authorized.
    fs::rename(&checkout, root.join("prepared")).unwrap();
    symlink(&other, &checkout).unwrap();
    assert!(context.authorize(worker, Some(&checkout)).is_ok());
    assert_eq!(
        context.recheck(&granted).unwrap_err().notice(),
        WORKER_DENIED
    );
    // Swapped for a link out of the fixture.
    fs::remove_file(&checkout).unwrap();
    symlink(&outside, &checkout).unwrap();
    assert!(context.recheck(&granted).is_err());
    // Put back, then its admin dir redirected out of the fixture.
    fs::remove_file(&checkout).unwrap();
    fs::rename(root.join("prepared"), &checkout).unwrap();
    assert_eq!(context.recheck(&granted), Ok(()));
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {}\n", outside.display()),
    )
    .unwrap();
    assert!(context.recheck(&granted).is_err());

    // One denied target denies the set; no target is denied for a write.
    assert!(context
        .authorize_targets(worker, &[other.clone(), outside.clone()])
        .is_err());
    assert!(context.authorize_targets(worker, &[]).is_err());
    // Production authorizes and rechecks anything, with no filesystem check.
    let production = ServiceContext::production(f.preview.root(), "1").unwrap();
    let granted = production
        .authorize_targets(worker, std::slice::from_ref(&outside))
        .unwrap();
    assert_eq!(production.recheck(&granted), Ok(()));
}
