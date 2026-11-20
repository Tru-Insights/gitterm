use super::*;
use gitterm_core::{
    chat::context::ProbeStatus,
    chat::types::{HarnessCommand, RuntimeRequestKind},
    preview::{Preview, ProtectedRoots},
};
use gitterm_gpui_chat::{PermissionDecision, QuestionAnswer, adapter::ReceiptState};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

fn fixture() -> (tempfile::TempDir, Arc<Preview>) {
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
fn script(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
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
    let provider = Provider::new(preview.clone(), Path::new("/fake/claude")).unwrap();
    provider.prepare().unwrap();
    assert_eq!(
        provider.spec.environment()[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")],
        preview.provider()
    );
    std::fs::rename(preview.provider(), preview.root().join("old-provider")).unwrap();
    symlink(dir.path().join("production"), preview.provider()).unwrap();
    assert!(provider.prepare().is_err());
    assert!(Provider::new(preview, Path::new("/fake/claude")).is_err());
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
        let provider = Provider::new(preview.clone(), &fake).unwrap();
        assert_eq!(provider.fake_probe().await.unwrap(), expected);
        assert!(provider.authorize().is_err());
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
        let provider = Provider::new(preview.clone(), &fake).unwrap();
        assert_eq!(
            provider.fake_probe().await.unwrap(),
            ProbeStatus::Unavailable
        );
    }
}
fn event(controller: &mut Controller, event: HarnessEvent) {
    let source = controller.source.clone();
    assert!(controller.apply_runtime_outcome(&source, event).is_some());
}
fn request(id: &str, kind: RuntimeRequestKind) -> HarnessEvent {
    HarnessEvent::RuntimeRequest {
        request_id: id.into(),
        tool_use_id: "tool".into(),
        kind,
    }
}
#[test]
fn adapter_channel_acceptance_stop_permission_question_and_stale_stamps() {
    let mut controller = Controller::new();
    let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
    controller.runtime = Some(Session::from_port(sender, true));
    controller.ready = true;
    let (receipt, _) = controller.dispatch(ChatIntent::Send {
        text: "Hello preview".into(),
        images: vec![],
    });
    assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    assert!(receipt.state.label().contains("delivery unconfirmed"));
    assert!(matches!(
        commands.try_recv().unwrap(),
        HarnessCommand::SendUserMessage(_)
    ));
    event(
        &mut controller,
        HarnessEvent::TurnStarted {
            session_id: Some("preview-only".into()),
            model: None,
        },
    );
    let (receipt, _) = controller.dispatch(ChatIntent::Stop);
    assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    assert!(matches!(
        commands.try_recv().unwrap(),
        HarnessCommand::Interrupt
    ));
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
    assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    assert!(
        matches!(commands.try_recv().unwrap(), HarnessCommand::Answer { request_id, .. } if request_id == "permission")
    );
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
    assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    assert!(
        matches!(commands.try_recv().unwrap(), HarnessCommand::Answer { request_id, .. } if request_id == "question")
    );
    let stale = controller.source.clone();
    controller.source = controller.adapter.begin_runtime(controller.target.clone());
    assert!(
        controller
            .apply_runtime_outcome(&stale, HarnessEvent::TextDelta("stale".into()))
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
    let (pane, cx) = cx.add_window_view(|window, cx| ChatPane::history(saved, window, cx));
    let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    pane.update_in(cx, |pane, window, cx| {
        pane.install_fake(Session::from_port(sender, true), receiver, window, cx);
    });
    events
        .send(HarnessEvent::Ready {
            session_id: None,
            permission_mode: None,
            models: vec![],
            modes: vec![],
            commands: vec![],
        })
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
        assert_eq!(receipt.state, ReceiptState::ChannelAccepted);
    });
    pane.update(cx, |pane, _| {
        assert!(pane.controller.ready);
        assert!(
            pane.saved
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
                .apply_runtime_outcome(&stale, HarnessEvent::TextDelta("stale".into()))
                .is_none()
        );
        assert!(pane.saved.session_id.is_none());
    });
    assert!(matches!(
        commands.try_recv().unwrap(),
        HarnessCommand::SendUserMessage(_)
    ));
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
    let provider = Arc::new(Provider::new(preview.clone(), &fake).unwrap());
    let cwd = dir.path().join("checkout");
    std::fs::create_dir(&cwd).unwrap();
    let (runtime, mut events) = provider
        .clone()
        .fake_start(cwd.clone())
        .await
        .unwrap()
        .unwrap();
    let mut controller = Controller::new();
    controller.runtime = Some(runtime);
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        let intent = match &next {
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
        };
        let exited = matches!(next, HarnessEvent::ProcessExited { .. });
        event(&mut controller, next);
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
    assert_eq!(controller.runtime.as_ref().unwrap().local_pid(), None);
    assert_eq!(
        controller.adapter.state().session_id.as_deref(),
        Some("preview-created-id")
    );
    assert!(provider.authorize().is_err());
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
    let unsigned = Arc::new(Provider::new(preview, &fake).unwrap());
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
    let provider = Provider::new(preview.clone(), &fake).unwrap();
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
