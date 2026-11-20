use super::*;
use gitterm_core::{
    chat::context::ProbeStatus,
    chat::types::{HarnessCommand, RuntimeRequestKind},
    preview::{Preview, ProtectedRoots},
};
use gitterm_gpui_chat::{PermissionDecision, QuestionAnswer, adapter::ReceiptState};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

pub(super) fn fixture() -> (tempfile::TempDir, Arc<Preview>) {
    let dir = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = dir.path().join("preview");
    let preview = Arc::new(
        Preview::validate(
            &root,
            ProtectedRoots::new(&dir.path().join("host"), [dir.path().join("production")]).unwrap(),
        )
        .unwrap(),
    );
    (dir, preview)
}
pub(super) fn script(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
pub(super) fn independent_clone(cwd: &Path) {
    std::fs::create_dir_all(cwd.join(".git/objects/info")).unwrap();
    std::fs::create_dir_all(cwd.join(".git/refs/heads")).unwrap();
    std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        cwd.join(".git/config"),
        "[core]\nrepositoryformatversion = 0\n",
    )
    .unwrap();
}

#[test]
fn fake_help_missing_restricted_refuses_and_success_does_not_grant_authority() {
    let (dir, preview) = fixture();
    let help = "  --restricted  restricted\n  --strict-mcp-config  strict\n  --safe-mode  safe\n  --permission-mode <mode> (choices: \"default\", \"acceptEdits\")\n";
    for (help, supported) in [
        (help.to_owned(), true),
        (help.replace("--restricted", "--unknown"), false),
    ] {
        let path = script(
            dir.path(),
            "fake-help",
            &format!(
                "#!/bin/sh\n[ \"$1\" = --help ] || exit 9\nprintf '%s' {}\n",
                super::provider::quote(&help)
            ),
        );
        let provider = Provider::fixture(preview.clone(), &path).unwrap();
        let output = std::process::Command::new(path)
            .arg("--help")
            .env_clear()
            .envs(provider.spec.environment())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            super::fence::check_help(std::str::from_utf8(&output.stdout).unwrap()).is_ok(),
            supported
        );
        // Help alone cannot supply account evidence to the launch gate.
        assert!(
            super::auth::personal_auth(
                Some(0),
                &output.stdout,
                &preview.provider(),
                super::auth::AUTH_STATUS_SCHEMA
            )
            .is_err()
        );
    }
}

#[test]
fn workspace_requires_independent_clone_without_shared_metadata_or_preview_overlap() {
    let (dir, preview) = fixture();
    let provider = Provider::fixture(preview.clone(), Path::new("/fake/claude")).unwrap();
    let cwd = dir.path().join("clone");
    std::fs::create_dir(&cwd).unwrap();
    assert!(provider.validate_workspace(&cwd).is_err());
    independent_clone(&cwd);
    provider.validate_workspace(&cwd).unwrap();
    std::fs::create_dir(cwd.join("src")).unwrap();
    assert!(provider.validate_workspace(&cwd.join("src")).is_err());
    for redirect in [
        "commondir",
        "worktrees",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        let path = cwd.join(".git").join(redirect);
        std::fs::write(&path, ".\n").unwrap();
        assert!(provider.validate_workspace(&cwd).is_err(), "{redirect}");
        std::fs::remove_file(path).unwrap();
    }
    let linked = dir.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", cwd.join(".git").display()),
    )
    .unwrap();
    assert!(provider.validate_workspace(&linked).is_err());
    std::fs::remove_file(linked.join(".git")).unwrap();
    symlink(cwd.join(".git"), linked.join(".git")).unwrap();
    assert!(provider.validate_workspace(&linked).is_err());
    std::fs::rename(cwd.join(".git/config"), cwd.join("config-backup")).unwrap();
    symlink(cwd.join("config-backup"), cwd.join(".git/config")).unwrap();
    assert!(provider.validate_workspace(&cwd).is_err());
    independent_clone(&preview.root().join("clone"));
    assert!(
        provider
            .validate_workspace(&preview.root().join("clone"))
            .is_err()
    );
}
#[test]
fn provider_roots_refuse_protected_overlap_relative_alias_and_replacement() {
    let (dir, preview) = fixture();
    let home = dir.path().join("host");
    for root in [
        PathBuf::from("relative"),
        home.clone(),
        home.join(".claude"),
        home.join(".config/gitterm-v6/nested"),
    ] {
        assert!(crate::preview::config_dir(Some(root.into_os_string()), &home).is_err());
    }
    let alias = dir.path().join("alias");
    std::fs::create_dir(preview.root()).unwrap();
    symlink(preview.root(), &alias).unwrap();
    assert!(crate::preview::config_dir(Some(alias.into_os_string()), &home).is_err());
    let provider = Provider::fixture(preview.clone(), Path::new("/fake/claude")).unwrap();
    provider.prepare().unwrap();
    assert_eq!(
        provider.spec.environment()[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")],
        preview.provider()
    );
    std::fs::rename(preview.provider(), preview.root().join("old-provider")).unwrap();
    symlink(dir.path().join("production"), preview.provider()).unwrap();
    assert!(provider.prepare().is_err());
    assert!(Provider::fixture(preview, Path::new("/fake/claude")).is_err());
}
#[tokio::test]
async fn probe_both_states_and_environment_are_rooted_and_success_does_not_open_c2() {
    let (dir, preview) = fixture();
    for (method, code, expected) in [
        ("claude.ai", 0, ProbeStatus::ReportedLogin),
        ("none", 1, ProbeStatus::NotLoggedIn),
        ("api_key", 0, ProbeStatus::Unavailable),
    ] {
        let fake = script(
            dir.path(),
            "fake-claude",
            &format!(
                "#!/bin/sh\n/usr/bin/env > '{}'\nprintf '{{\"authMethod\":\"{method}\",\"configDirectory\":\"%s\"}}' \"$CLAUDE_CONFIG_DIR\"\nexit {code}\n",
                dir.path().join("environment").display()
            ),
        );
        let provider = Provider::fixture(preview.clone(), &fake).unwrap();
        assert_eq!(provider.fake_probe().await.unwrap(), expected);
        // The display probe's partial schema cannot authorize a launch.
        let partial = serde_json::json!({"authMethod":method,"configDirectory":preview.provider()});
        assert!(
            super::auth::personal_auth(
                Some(code),
                &serde_json::to_vec(&partial).unwrap(),
                &preview.provider(),
                super::auth::AUTH_STATUS_SCHEMA
            )
            .is_err()
        );
        let env = std::fs::read_to_string(dir.path().join("environment")).unwrap();
        for forbidden in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY_HELPER",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_API_KEY_HELPER",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "GITTERM_TASK_MCP_URL",
            "HTTP_PROXY",
            "DYLD_INSERT_LIBRARIES",
        ] {
            assert!(
                !provider
                    .spec
                    .environment()
                    .contains_key(std::ffi::OsStr::new(forbidden))
            );
            assert!(
                !env.lines()
                    .any(|line| line.starts_with(&format!("{forbidden}=")))
            );
        }
        assert!(provider.sign_in_command().contains("env -i"));
        assert!(provider.sign_in_command().contains(&format!(
            "CLAUDE_CONFIG_DIR={}",
            preview.provider().display()
        )));
        assert!(provider.sign_in_command().ends_with("Then /login"));
        for effect in [
            gitterm_core::preview::Effect::Worker,
            gitterm_core::preview::Effect::Resume,
            gitterm_core::preview::Effect::Helper,
            gitterm_core::preview::Effect::Agent,
        ] {
            assert!(
                crate::preview::executable(
                    Some(&preview),
                    effect,
                    Path::new("/phone/remote/imported-id")
                )
                .is_err()
            );
        }
    }
}
#[tokio::test]
async fn malformed_mismatched_oversized_and_hung_probe_fail_closed() {
    let (dir, preview) = fixture();
    for body in [
        "printf garbage",
        "printf '{\"authMethod\":\"claude.ai\",\"configDirectory\":\"/wrong\"}'",
        "/usr/bin/head -c 17000 /dev/zero",
        "exec /bin/sleep 5",
    ] {
        let fake = script(dir.path(), "fake-status", &format!("#!/bin/sh\n{body}\n"));
        let provider = Provider::fixture(preview.clone(), &fake).unwrap();
        assert_eq!(
            provider.fake_probe().await.unwrap(),
            ProbeStatus::Unavailable
        );
    }
}
fn event(controller: &mut Controller, event: HarnessEvent) {
    let source = controller.source.clone();
    assert!(
        controller
            .apply_runtime_outcome(&source, RuntimeEvent::Harness(event))
            .is_some()
    );
}
fn request(id: &str, kind: RuntimeRequestKind) -> HarnessEvent {
    HarnessEvent::RuntimeRequest {
        request_id: id.into(),
        tool_use_id: "tool".into(),
        kind,
    }
}
#[test]
fn uncorrelated_ports_refuse_send_stop_permission_question_and_stale_stamps() {
    let mut controller = Controller::new();
    let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
    controller.runtime = Some(Session::from_port(sender, true).into());
    controller.ready = true;
    let (receipt, _) = controller.dispatch(ChatIntent::Send {
        text: "Hello preview".into(),
        images: vec![],
    });
    assert_eq!(
        receipt.state,
        ReceiptState::Rejected("correlated writes require Session::spawn_runtime".into())
    );
    assert!(receipt.state.label().contains("Rejected"));
    assert!(commands.try_recv().is_err());
    event(
        &mut controller,
        HarnessEvent::TurnStarted {
            session_id: Some("preview-only".into()),
            model: None,
        },
    );
    let (receipt, _) = controller.dispatch(ChatIntent::Stop);
    assert_eq!(
        receipt.state,
        ReceiptState::Rejected("correlated writes require Session::spawn_runtime".into())
    );
    assert!(commands.try_recv().is_err());
    event(
        &mut controller,
        request(
            "permission",
            RuntimeRequestKind::Permission {
                tool_name: "Write".into(),
                input: serde_json::json!({}),
                suggestions: vec![],
                decision_reason: None,
            },
        ),
    );
    let (receipt, _) = controller.dispatch(ChatIntent::Permission {
        request_id: "permission".into(),
        decision: PermissionDecision::Deny,
    });
    assert_eq!(
        receipt.state,
        ReceiptState::Rejected("correlated writes require Session::spawn_runtime".into())
    );
    assert!(commands.try_recv().is_err());
    event(
        &mut controller,
        request(
            "question",
            RuntimeRequestKind::Question {
                questions: serde_json::json!([{"question":"Shape?","options":[{"label":"Square"}]}]),
            },
        ),
    );
    let (receipt, _) = controller.dispatch(ChatIntent::AnswerQuestion {
        request_id: "question".into(),
        answers: vec![QuestionAnswer {
            question: "Shape?".into(),
            selected: vec!["Square".into()],
            free_text: None,
        }],
    });
    assert_eq!(
        receipt.state,
        ReceiptState::Rejected("correlated writes require Session::spawn_runtime".into())
    );
    assert!(commands.try_recv().is_err());
    let stale = controller.source.clone();
    controller.source = controller.adapter.begin_runtime(controller.target.clone());
    assert!(
        controller
            .apply_runtime_outcome(
                &stale,
                RuntimeEvent::Harness(HarnessEvent::TextDelta("stale".into()))
            )
            .is_none()
    );
}

#[gpui::test]
fn pane_owns_one_receiver_projects_adapter_and_saves_only_accepted_events(
    cx: &mut gpui::TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (dir, preview) = fixture();
    let saved = SavedChat {
        uid: "synthetic-pane".into(),
        cwd: dir.path().to_owned(),
        provider: preview.provider(),
        session_id: None,
        records: vec![],
    };
    let (pane, cx) = cx
        .add_window_view(|window, cx| ChatPane::history(RuntimeShutdown::new(), saved, window, cx));
    let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    pane.update_in(cx, |pane, window, cx| {
        pane.install_runtime(
            Session::from_port(sender, true).into(),
            receiver,
            window,
            cx,
        );
    });
    events
        .send(RuntimeEvent::Harness(HarnessEvent::Ready {
            session_id: None,
            permission_mode: None,
            models: vec![],
            modes: vec![],
            commands: vec![],
        }))
        .unwrap();
    cx.run_until_parked();
    let callback = ChatPane::intent_callback(pane.downgrade());
    cx.update(|_, cx| {
        let receipt = callback(
            ChatIntent::Send {
                text: "Pane prompt".into(),
                images: vec![],
            },
            cx,
        );
        assert_eq!(
            receipt.state,
            ReceiptState::Rejected("correlated writes require Session::spawn_runtime".into())
        );
    });
    pane.update(cx, |pane, _| {
        assert!(pane.controller.ready);
        assert!(
            !pane
                .saved
                .records
                .iter()
                .any(|r| matches!(&r.entry, Entry::UserPrompt(text) if text == "Pane prompt"))
        );
        let stale = pane.controller.source.clone();
        pane.controller.source = pane
            .controller
            .adapter
            .begin_runtime(pane.controller.target.clone());
        assert!(
            pane.controller
                .apply_runtime_outcome(
                    &stale,
                    RuntimeEvent::Harness(HarnessEvent::TextDelta("stale".into()))
                )
                .is_none()
        );
        assert!(pane.saved.session_id.is_none());
    });
    assert!(commands.try_recv().is_err());
    pane.update(cx, |pane, cx| pane.close(cx));
    assert!(matches!(
        commands.try_recv().unwrap(),
        HarnessCommand::Shutdown
    ));
}

#[tokio::test]
async fn fake_runtime_probe_fresh_launch_send_answers_stop_and_shutdown() {
    let (dir, preview) = fixture();
    let python = std::process::Command::new("python3")
        .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let program = format!(
        r##"#!{}
import json, sys, os
from pathlib import Path
if sys.argv[1:] == ['--help']:
    print('  --restricted\n  --strict-mcp-config\n  --safe-mode\n  --permission-mode <mode> (choices: "manual")')
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status', '--json']:
    print(json.dumps({{'fixture_signed_in':True,'fixture_method':'fixture_personal','fixture_plan':'fixture_pro','fixture_root':os.environ['CLAUDE_CONFIG_DIR'],'fixture_provider':'fixture_first_party'}}))
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status']:
    print(json.dumps({{'authMethod':'claude.ai', 'configDirectory':os.environ['CLAUDE_CONFIG_DIR']}}))
    sys.exit(0)
assert not any(a.startswith('--resume') for a in sys.argv)
assert '--no-chrome' in sys.argv
assert os.environ['CLAUDE_CONFIG_DIR'].endswith('/providers/claude')
assert not any(k.startswith('ANTHROPIC_') for k in os.environ)
Path('launch.json').write_text(json.dumps({{'cwd':os.getcwd(), 'env':dict(os.environ), 'args':sys.argv}}))
def read(): return json.loads(sys.stdin.readline())
def emit(x): print(json.dumps(x), flush=True)
init = read()
emit({{'type':'control_response','response':{{'subtype':'success','request_id':init['request_id'],'response':{{'models':[]}}}}}})
prompt = read()
assert prompt['type'] == 'user'
emit({{'type':'system','subtype':'init','session_id':'preview-created-id','model':'synthetic'}})
emit({{'type':'control_request','request_id':'permission','request':{{'subtype':'can_use_tool','tool_name':'Write','tool_use_id':'write','input':{{'file_path':'sample'}}}}}})
assert read()['response']['response']['behavior'] == 'deny'
emit({{'type':'control_request','request_id':'question','request':{{'subtype':'can_use_tool','tool_name':'AskUserQuestion','tool_use_id':'ask','input':{{'questions':[{{'question':'Shape?','options':[{{'label':'Square'}}]}}]}}}}}})
assert read()['response']['response']['updatedInput']['answers'] == {{'Shape?':'Square'}}
assert read()['request']['subtype'] == 'interrupt'
emit({{'type':'result','subtype':'error_during_execution','is_error':True,'errors':['Interrupted'],'usage':{{}}}})
assert not sys.stdin.read()
"##,
        String::from_utf8(python.stdout).unwrap().trim()
    );
    let fake = script(dir.path(), "fake-claude", &program);
    let provider = Arc::new(Provider::fixture(preview.clone(), &fake).unwrap());
    let cwd = dir.path().join("checkout");
    independent_clone(&cwd);
    let (runtime, mut events) = provider
        .clone()
        .fake_start(cwd.clone())
        .await
        .unwrap()
        .unwrap();
    let mut controller = Controller::new();
    controller.runtime = Some(runtime);
    let mut written = 0;
    let mut raw_written = 0;
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        let intent = match &next {
            RuntimeEvent::Harness(next) => match next {
                HarnessEvent::Ready { .. } => Some(ChatIntent::Send {
                    text: "Synthetic test".into(),
                    images: vec![],
                }),
                HarnessEvent::RuntimeRequest {
                    request_id,
                    kind: RuntimeRequestKind::Permission { .. },
                    ..
                } => Some(ChatIntent::Permission {
                    request_id: request_id.clone(),
                    decision: PermissionDecision::Deny,
                }),
                HarnessEvent::RuntimeRequest {
                    request_id,
                    kind: RuntimeRequestKind::Question { .. },
                    ..
                } => Some(ChatIntent::AnswerQuestion {
                    request_id: request_id.clone(),
                    answers: vec![QuestionAnswer {
                        question: "Shape?".into(),
                        selected: vec!["Square".into()],
                        free_text: None,
                    }],
                }),
                HarnessEvent::RuntimeRequestResolved { request_id } if request_id == "question" => {
                    Some(ChatIntent::Stop)
                }
                HarnessEvent::TurnCompleted { .. } => {
                    controller
                        .runtime
                        .as_ref()
                        .unwrap()
                        .send(HarnessCommand::Shutdown)
                        .unwrap();
                    None
                }
                HarnessEvent::Error(error) => panic!("fake runtime: {error}"),
                _ => None,
            },
            RuntimeEvent::CommandOutcome { .. } => None,
        };
        let exited = matches!(
            next,
            RuntimeEvent::Harness(HarnessEvent::ProcessExited { .. })
        );
        let is_outcome = matches!(&next, RuntimeEvent::CommandOutcome { .. });
        if matches!(
            &next,
            RuntimeEvent::CommandOutcome {
                outcome: DeliveryOutcome::Written,
                ..
            }
        ) {
            raw_written += 1;
        }
        let source = controller.source.clone();
        if let Some(update) = controller.apply_runtime_outcome(&source, next) {
            for receipt in update.receipts {
                if receipt.state == ReceiptState::Written {
                    written += 1;
                }
                if is_outcome {
                    assert_eq!(receipt.state, ReceiptState::Written);
                } else {
                    assert!(matches!(
                        receipt.state,
                        ReceiptState::Resolved | ReceiptState::Uncertain(_)
                    ));
                }
            }
        }
        if let Some(intent) = intent {
            assert_eq!(
                controller.dispatch(intent).0.state,
                ReceiptState::ChannelAccepted
            );
        }
        if exited {
            break;
        }
    }
    controller
        .runtime
        .as_mut()
        .unwrap()
        .completed()
        .await
        .unwrap();
    assert_eq!(
        raw_written, 4,
        "send, deny, question and interrupt must all receive actual write outcomes"
    );
    assert_eq!(
        written, 2,
        "send and stop are Written; already resolved answers retain Resolved"
    );
    assert_eq!(controller.runtime.as_ref().unwrap().local_pid(), None);
    assert_eq!(
        controller.adapter.state().session_id.as_deref(),
        Some("preview-created-id")
    );
    let launch: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cwd.join("launch.json")).unwrap()).unwrap();
    assert_eq!(launch["cwd"], cwd.to_str().unwrap());
    assert_eq!(
        launch["env"]["CLAUDE_CONFIG_DIR"],
        preview.provider().to_str().unwrap()
    );
    let fake = script(
        dir.path(),
        "fake-unsigned",
        "#!/bin/sh\nprintf '{\"authMethod\":\"none\",\"configDirectory\":\"%s\"}' \"$CLAUDE_CONFIG_DIR\"\nexit 1\n",
    );
    let unsigned = Arc::new(Provider::fixture(preview, &fake).unwrap());
    assert!(unsigned.fake_start(cwd).await.unwrap().is_none());
}

#[test]
fn preview_history_roundtrips_in_guarded_catalog_and_invalid_metadata_preserves_file() {
    let (dir, preview) = fixture();
    let writer = Arc::new(crate::preview::Writer::new(preview.claim_writer().unwrap()));
    let mut store = crate::workspace_config::WorkspaceStore::guarded(writer.clone());
    let chat = SavedChat {
        uid: "synthetic-created".into(),
        cwd: dir.path().join("checkout"),
        provider: preview.provider(),
        session_id: Some("new-id-only".into()),
        records: vec![Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::TextDelta("Saved text".into())),
        }],
    };
    store.chats.push(chat);
    store.save(&[]).unwrap();
    let mut loaded = crate::workspace_config::WorkspaceStore::guarded(writer);
    loaded.load().unwrap();
    assert_eq!(loaded.chats.len(), 1);
    assert_eq!(loaded.chats[0].session_id.as_deref(), Some("new-id-only"));
    assert_eq!(loaded.chats[0].records, store.chats[0].records);
    assert_eq!(loaded.chats[0].provider, preview.provider());
    let path = preview.root().join("local-workspaces.json");
    let mut invalid: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    invalid["preview_chats"][0]["cwd"] = serde_json::json!("relative");
    let bytes = serde_json::to_vec(&invalid).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(loaded.load().is_err());
    assert!(loaded.save(&[]).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn sign_in_command_quotes_provider_and_executable_paths_without_shell_expansion() {
    let (dir, _) = fixture();
    let root = dir.path().join("preview ' $(false)");
    let preview = Arc::new(
        Preview::validate(
            &root,
            ProtectedRoots::new(&dir.path().join("host"), []).unwrap(),
        )
        .unwrap(),
    );
    let marker = dir.path().join("environment");
    let fake = script(
        dir.path(),
        "fake claude ' $(false)",
        &format!("#!/bin/sh\n/usr/bin/env > '{}'\n", marker.display()),
    );
    let provider = Provider::fixture(preview.clone(), &fake).unwrap();
    provider.prepare().unwrap();
    let command = provider.sign_in_command();
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", command.lines().next().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = std::fs::read_to_string(marker).unwrap();
    assert!(
        env.lines()
            .any(|line| line == format!("CLAUDE_CONFIG_DIR={}", preview.provider().display()))
    );
}

#[gpui::test]
fn quit_drains_a_closed_slow_child_before_gpui_timeout_and_later_observers(
    cx: &mut gpui::TestAppContext,
) {
    cx.background_executor.allow_parking();
    let (dir, preview) = fixture();
    let python = std::process::Command::new("python3")
        .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let fake = script(
        dir.path(),
        "fake-slow-claude",
        &format!(
            r##"#!{}
import json, sys, os, time
from pathlib import Path
if sys.argv[1:] == ['--help']:
    print('  --restricted\n  --strict-mcp-config\n  --safe-mode\n  --permission-mode <mode> (choices: "manual")')
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status', '--json']:
    print(json.dumps({{'fixture_signed_in':True,'fixture_method':'fixture_personal','fixture_plan':'fixture_pro','fixture_root':os.environ['CLAUDE_CONFIG_DIR'],'fixture_provider':'fixture_first_party'}}))
    sys.exit(0)
if sys.argv[1:] == ['auth', 'status']:
    print(json.dumps({{'authMethod':'claude.ai','configDirectory':os.environ['CLAUDE_CONFIG_DIR']}}))
    sys.exit(0)
init = json.loads(sys.stdin.readline())
print(json.dumps({{'type':'control_response','response':{{'subtype':'success','request_id':init['request_id'],'response':{{'models':[]}}}}}}), flush=True)
assert not sys.stdin.read()
time.sleep(0.45)
Path('reaped-marker').write_text('finished')
"##,
            String::from_utf8(python.stdout).unwrap().trim()
        ),
    );
    let provider = Arc::new(Provider::fixture(preview.clone(), &fake).unwrap());
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cwd = dir.path().join("checkout");
    independent_clone(&cwd);
    let (runtime, mut events) = executor
        .block_on(provider.clone().fake_start(cwd.clone()))
        .unwrap()
        .unwrap();
    executor.block_on(async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                RuntimeEvent::Harness(HarnessEvent::Ready { .. })
            ) {
                break;
            }
        }
    });
    let pid = runtime.local_pid().unwrap();
    assert!(super::lease::ProviderLease::claim(&preview).is_err());
    // Explicit pane close gives up the view, but the application still owns the
    // completion via its queue. This is exactly the prior detached-close gap.
    let mut app = cx.new_app();
    let owner = app.new(|cx| {
        crate::WorkspaceOwner::new(
            crate::workspace_status::WorkspaceStatus::default(),
            None,
            cx,
        )
    });
    let shutdown = owner.read_with(&app, |owner, _| owner.chat_shutdown.clone());
    assert!(shutdown.enqueue(runtime).is_ok());
    assert!(
        super::lease::ProviderLease::claim(&preview).is_err(),
        "lease must survive pane close"
    );
    let marker = cwd.join("reaped-marker");
    app.update(|app| {
        app.on_app_quit(move |_| {
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "finished");
            let output = std::process::Command::new("/bin/ps")
                .args(["-p", &pid.to_string(), "-o", "stat="])
                .output()
                .unwrap();
            assert!(
                output.stdout.is_empty(),
                "fake child still exists after quit drain"
            );
            std::future::ready(())
        })
        .detach();
    });
    let start = std::time::Instant::now();
    app.update(|app| app.shutdown());
    let lease =
        super::lease::ProviderLease::claim(&preview).expect("lease released after quit drain");
    drop(lease);
    assert!(start.elapsed() > gpui::SHUTDOWN_TIMEOUT);
}

#[gpui::test]
fn preview_pane_focus_forwards_to_the_native_chat(cx: &mut gpui::TestAppContext) {
    use gpui::Focusable as _;
    cx.update(gpui_component::init);
    let (dir, preview) = fixture();
    let saved = SavedChat {
        uid: "synthetic-focus".into(),
        cwd: dir.path().to_owned(),
        provider: preview.provider(),
        session_id: None,
        records: vec![],
    };
    let (pane, cx) = cx.add_window_view(|window, cx| {
        let chat = cx.new(|cx| ChatPane::history(RuntimeShutdown::new(), saved, window, cx));
        crate::pane::Pane::new(
            "preview-focus".into(),
            "Claude preview".into(),
            crate::pane::PaneKind::PreviewChat(chat),
            crate::data::WorkspaceId(1),
            Rc::new(crate::data::SampleData),
            window,
            cx,
        )
    });
    pane.update_in(cx, |pane, window, cx| {
        let crate::pane::PaneContent::PreviewChat(chat) = &pane.content else {
            unreachable!()
        };
        let expected = chat.read(cx).view.focus_handle(cx);
        assert_eq!(pane.focus_handle(cx), expected);
        pane.focus_handle(cx).focus(window, cx);
        assert!(expected.is_focused(window));
    });
}

#[test]
fn correlated_receipts_are_stamped_and_preserve_all_delivery_states() {
    use gitterm_gpui_chat::adapter::DeliveryOutcome as AdapterOutcome;
    for (outcome, expected) in [
        (DeliveryOutcome::Written, ReceiptState::Written),
        (
            DeliveryOutcome::Rejected("closed".into()),
            ReceiptState::Rejected("closed".into()),
        ),
        (
            DeliveryOutcome::Uncertain("partial write".into()),
            ReceiptState::Uncertain("partial write".into()),
        ),
    ] {
        let mut controller = Controller::new();
        let prepared = controller
            .adapter
            .admit(
                ChatIntent::Send {
                    text: "Synthetic".into(),
                    images: vec![],
                },
                Admission {
                    authority: session::State {
                        target: controller.target.clone(),
                        control: session::TurnControl::Harness,
                        task_live: false,
                        last_tab: false,
                    },
                    backend: AgentBackend::Claude,
                    process_running: true,
                    just_spawned: false,
                    message_id: "synthetic".into(),
                },
            )
            .unwrap();
        let id = prepared.id();
        let correlation = CommandCorrelation(42);
        controller.correlations.insert(correlation, id);
        controller
            .adapter
            .execute(prepared, &controller.target, now_ms(), |_| Ok(()));
        let source = controller.source.clone();
        let update = controller
            .apply_runtime_outcome(
                &source,
                RuntimeEvent::CommandOutcome {
                    correlation,
                    outcome,
                },
            )
            .unwrap();
        assert_eq!(update.receipts[0].id, id);
        assert_eq!(update.receipts[0].state, expected);
        assert_eq!(controller.adapter.receipt(id).unwrap().state, expected);
        assert!(
            controller
                .adapter
                .delivery(id, &controller.target, AdapterOutcome::Written)
                .is_none()
        );
        assert!(
            controller
                .apply_runtime_outcome(
                    &source,
                    RuntimeEvent::CommandOutcome {
                        correlation,
                        outcome: DeliveryOutcome::Written
                    }
                )
                .is_none()
        );
        let stale = source;
        controller.source = controller.adapter.begin_runtime(controller.target.clone());
        assert!(
            controller
                .apply_runtime_outcome(
                    &stale,
                    RuntimeEvent::CommandOutcome {
                        correlation,
                        outcome: DeliveryOutcome::Written
                    }
                )
                .is_none()
        );
    }
}

#[tokio::test]
async fn fake_process_rejected_and_partial_write_outcomes_reach_adapter() {
    for partial in [false, true] {
        let (dir, preview) = fixture();
        let python = std::process::Command::new("python3")
            .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert!(python.status.success());
        let body = include_str!("../../../../../tests/claude-process/preview_outcomes.py")
            .replacen(
                "#!/usr/bin/env python3",
                &format!("#!{}", String::from_utf8(python.stdout).unwrap().trim()),
                1,
            );
        let program = script(dir.path(), "fake-outcomes", &body);
        let provider = Arc::new(Provider::fixture(preview, &program).unwrap());
        let cwd = dir.path().join("clone");
        independent_clone(&cwd);
        if partial {
            std::fs::write(cwd.join("partial-mode"), b"").unwrap();
        }
        let (runtime, mut events) = provider.fake_start(cwd).await.unwrap().unwrap();
        let mut controller = Controller::new();
        controller.runtime = Some(runtime);
        while !controller.ready {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let RuntimeEvent::Harness(HarnessEvent::Error(error)) = &event {
                panic!("{error}");
            }
            let source = controller.source.clone();
            controller.apply_runtime_outcome(&source, event);
        }
        let intent = if partial {
            ChatIntent::Send {
                text: "x".repeat(4 * 1024 * 1024),
                images: vec![],
            }
        } else {
            // The pane saw a request which the runner no longer owns.
            event(
                &mut controller,
                request(
                    "already-gone",
                    RuntimeRequestKind::Permission {
                        tool_name: "Write".into(),
                        input: serde_json::json!({}),
                        suggestions: vec![],
                        decision_reason: None,
                    },
                ),
            );
            ChatIntent::Permission {
                request_id: "already-gone".into(),
                decision: PermissionDecision::Deny,
            }
        };
        let receipt = controller.dispatch(intent).0;
        assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
        let delivered = loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            let source = controller.source.clone();
            if let Some(update) = controller.apply_runtime_outcome(&source, next)
                && let Some(receipt) = update
                    .receipts
                    .into_iter()
                    .find(|next| next.id == receipt.id)
            {
                break receipt;
            }
        };
        if partial {
            assert!(matches!(delivered.state, ReceiptState::Uncertain(_)));
        } else {
            assert!(matches!(delivered.state, ReceiptState::Rejected(_)));
        }
        assert_eq!(controller.adapter.receipt(receipt.id), Some(&delivered));
        let runtime = controller.runtime.as_mut().unwrap();
        let _ = runtime.send(HarnessCommand::Shutdown);
        runtime.completed().await.unwrap();
    }
}
