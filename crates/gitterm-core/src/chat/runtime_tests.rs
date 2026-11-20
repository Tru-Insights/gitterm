use super::*;
use crate::chat::types::{RuntimeDecision, RuntimeRequestKind, UserPrompt};
use crate::harness::claude::{ClaudeSessionConfig, SnapshotFuture, SnapshotRecorder};
use crate::turn_snapshots::{Recorder, SnapshotRoots};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[path = "../../../../tests/native-chat/native_chat_process.rs"]
mod process;

fn launch(dir: &Path, script: &str, token: CancellationToken) -> Launch {
    // /usr/bin/python3 on macOS is an xcrun shim which adds SDK environment
    // variables. Use the interpreter itself for the exact delivery assertion.
    let python = std::process::Command::new("python3")
        .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let python = String::from_utf8(python.stdout).unwrap();
    let script = script.replacen("#!/usr/bin/env python3", &format!("#!{}", python.trim()), 1);
    let program = process::script(dir, "fake-claude", &script);
    let home = dir.join("home");
    let provider = dir.join("provider");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&provider).unwrap();
    let environment = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        (
            "LC_CTYPE".into(),
            if cfg!(target_os = "macos") {
                "UTF-8"
            } else {
                "C.UTF-8"
            }
            .into(),
        ),
        ("HOME".into(), home.clone().into_os_string()),
        ("CLAUDE_CONFIG_DIR".into(), provider.into_os_string()),
        ("SYNTHETIC_MCP_TOKEN".into(), "synthetic-only".into()),
    ];
    Launch {
        config: ClaudeSessionConfig {
            cwd: dir.into(),
            snapshot_facts_dir: None,
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        },
        program,
        environment,
        temporary_dir: dir.into(),
        snapshots: Box::new(
            Recorder::new(
                SnapshotRoots::new(dir.join("snapshots"), dir.into()).unwrap(),
                dir.into(),
                None,
                None,
                Some(home),
            )
            .with_close_token(token),
        ),
    }
}

async fn event(events: &mut mpsc::UnboundedReceiver<HarnessEvent>) -> HarnessEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .unwrap()
        .unwrap()
}

// Exercise the actual supervisor, with the same stdin and event goldens as C0.
#[tokio::test]
async fn supervisor_lifecycle_matches_c0_and_delivers_only_explicit_environment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let script = include_str!("../../../../tests/fixtures/native-chat/claude/process.py").replace(
        "frames = []",
        "import os\nPath('env.json').write_text(json.dumps(dict(os.environ)))\nframes = []",
    );
    let expected_env = launch(&path, &script, CancellationToken::new()).environment;
    let (mut session, mut events) = Session::spawn(move |token| Ok(launch(&path, &script, token)));
    let mut seen = Vec::new();
    let mut pid = 0;
    loop {
        let next = event(&mut events).await;
        match &next {
            HarnessEvent::Ready { .. } => {
                pid = session.local_pid().unwrap();
                session
                    .send(HarnessCommand::SendUserMessage(UserPrompt {
                        text: "Inspect fixture".into(),
                        images: Vec::new(),
                        id: Some("prompt-1".into()),
                    }))
                    .unwrap();
            }
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                session
                    .send(HarnessCommand::Answer {
                        request_id: request_id.clone(),
                        decision: match kind {
                            RuntimeRequestKind::Question { .. } => {
                                RuntimeDecision::AnswerQuestion {
                                    answers: BTreeMap::from([(
                                        "Pick a shape".into(),
                                        "Square".into(),
                                    )]),
                                }
                            }
                            _ => RuntimeDecision::Deny {
                                message: "Synthetic denial".into(),
                            },
                        },
                    })
                    .unwrap();
            }
            HarnessEvent::RuntimeRequestResolved { request_id } if request_id == "question-1" => {
                session.send(HarnessCommand::Interrupt).unwrap();
            }
            HarnessEvent::TurnCompleted { .. } => session.send(HarnessCommand::Shutdown).unwrap(),
            HarnessEvent::Error(error) => panic!("fake child: {error}"),
            _ => {}
        }
        let exited = matches!(next, HarnessEvent::ProcessExited { .. });
        seen.push(next);
        if exited {
            break;
        }
    }
    session.completed().await.unwrap();
    assert_eq!(session.local_pid(), None);
    process::assert_reaped(pid);
    assert_eq!(
        serde_json::to_value(seen).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../../tests/fixtures/native-chat/claude/lifecycle.json"
        ))
        .unwrap()
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(dir.path().join("stdin.json")).unwrap()
        )
        .unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../../tests/fixtures/native-chat/claude/stdin.json"
        ))
        .unwrap()
    );
    let delivered: BTreeMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("env.json")).unwrap())
            .unwrap();
    // CoreFoundation inserts/rewrites this OS-owned locale marker even after
    // env_clear, independently of the process launch environment.
    #[cfg(target_os = "macos")]
    let delivered = {
        let mut delivered = delivered;
        delivered.remove("__CF_USER_TEXT_ENCODING");
        delivered
    };
    assert_eq!(
        delivered,
        expected_env
            .into_iter()
            .map(|(k, v): (OsString, OsString)| (
                k.into_string().unwrap(),
                v.into_string().unwrap()
            ))
            .collect()
    );
}

const EOF_PEER: &str = r#"#!/usr/bin/env python3
import json, os, signal, sys
from pathlib import Path
signal.alarm(20)
Path('pid').write_text(str(os.getpid()))
init = json.loads(sys.stdin.readline())
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}}), flush=True)
assert not sys.stdin.read()
"#;

#[tokio::test]
async fn supervisor_drop_shuts_down_and_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let (session, mut events) = Session::spawn(move |token| Ok(launch(&path, EOF_PEER, token)));
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::Ready { .. }
    ));
    let pid = session.local_pid().unwrap();
    drop(session);
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    assert!(tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .unwrap()
        .is_none());
    process::assert_reaped(pid);
}

struct DelayedCleanup {
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Semaphore>,
    closed: Arc<AtomicBool>,
}
impl SnapshotRecorder for DelayedCleanup {
    fn observe(&mut self, _: &HarnessEvent) {}
    fn invalidate_end(&mut self) {}
    fn discard_turn(&mut self) {}
    fn bind<'a>(&'a mut self, _: &'a str) -> SnapshotFuture<'a> {
        Box::pin(async {})
    }
    fn begin(&mut self) -> SnapshotFuture<'_> {
        Box::pin(async {})
    }
    fn capture_end(&mut self) -> SnapshotFuture<'_> {
        Box::pin(async {})
    }
    fn finish(&mut self) -> SnapshotFuture<'_> {
        Box::pin(async move {
            self.entered.notify_one();
            self.gate.acquire().await.unwrap().forget();
        })
    }
    fn close(&mut self) -> SnapshotFuture<'_> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
        })
    }
}

#[tokio::test]
async fn supervisor_completion_waits_for_snapshot_cleanup_after_process_exit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let recorder = DelayedCleanup {
        entered: entered.clone(),
        gate: gate.clone(),
        closed: closed.clone(),
    };
    let (mut session, mut events) = Session::spawn(move |token| {
        let mut launch = launch(&path, EOF_PEER, token);
        launch.snapshots = Box::new(recorder);
        Ok(launch)
    });
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::Ready { .. }
    ));
    let pid = session.local_pid().unwrap();
    session.close_turn_snapshots();
    session.send(HarnessCommand::Shutdown).unwrap();
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    process::assert_reaped(pid);
    entered.notified().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), session.completed())
            .await
            .is_err()
    );
    assert!(!closed.load(Ordering::SeqCst));
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), session.completed())
        .await
        .unwrap()
        .unwrap();
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn supervisor_partial_output_crash_and_spawn_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let (mut session, mut events) = Session::spawn(move |token| {
        Ok(launch(
            &path,
            include_str!("../../../../tests/claude-process/partial.py"),
            token,
        ))
    });
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::Ready { .. }
    ));
    let pid = session.local_pid().unwrap();
    session
        .send(HarnessCommand::SendUserMessage(UserPrompt::from(
            "Synthetic prompt",
        )))
        .unwrap();
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::TextDelta("partial output".into())
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::TextDelta(" final fragment".into())
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::Error("claude exited unexpectedly (code Some(17))".into())
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: Some(17) }
    );
    session.completed().await.unwrap();
    process::assert_reaped(pid);
    assert!(session.send(HarnessCommand::Interrupt).is_err());
    assert_eq!(session.local_pid(), None);

    let path = dir.path().to_owned();
    let (mut session, mut events) = Session::spawn(move |token| {
        let mut launch = launch(&path, EOF_PEER, token);
        launch.program = path.join("absent-fake");
        Ok(launch)
    });
    assert!(
        matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("failed to start") && e.contains("absent-fake"))
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: None }
    );
    session.completed().await.unwrap();
    assert_eq!(session.local_pid(), None);
}

// The child accepts a prefix, then closes its pipe while keeping stdout alive.
// The answer exceeds pipe capacity, so the host cannot have accepted it all.
#[tokio::test]
async fn supervisor_partial_answer_is_uncertain_and_never_resent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let script = r#"#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path
signal.alarm(20)
init = json.loads(sys.stdin.readline())
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}}), flush=True)
print(json.dumps({'type':'control_request','request_id':'p1','request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':'t1','input':{}}}), flush=True)
Path('prefix').write_bytes(os.read(0, 16))
os.close(0)
while not Path('exit').exists(): time.sleep(0.01)
"#;
    let (mut session, mut events) = Session::spawn(move |token| Ok(launch(&path, script, token)));
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::Ready { .. }
    ));
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::RuntimeRequest { .. }
    ));
    let pid = session.local_pid().unwrap();
    session
        .send(HarnessCommand::Answer {
            request_id: "p1".into(),
            decision: RuntimeDecision::Deny {
                message: "x".repeat(4 * 1024 * 1024),
            },
        })
        .unwrap();
    assert!(
        matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("delivery uncertain"))
    );
    assert!(!std::fs::read(dir.path().join("prefix")).unwrap().is_empty());
    session
        .send(HarnessCommand::Answer {
            request_id: "p1".into(),
            decision: RuntimeDecision::Deny {
                message: "retry".into(),
            },
        })
        .unwrap();
    assert!(
        matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("resending is blocked"))
    );
    std::fs::write(dir.path().join("exit"), "").unwrap();
    // Resolution here closes a request whose process is gone; it is not a write receipt.
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::RuntimeRequestResolved {
            request_id: "p1".into()
        }
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    session.completed().await.unwrap();
    process::assert_reaped(pid);
    assert!(session
        .send(HarnessCommand::Answer {
            request_id: "p1".into(),
            decision: RuntimeDecision::Deny {
                message: "late".into()
            }
        })
        .is_err());
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn supervisor_withdrawal_and_turn_end_close_requests_before_late_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let script = r#"#!/usr/bin/env python3
import json, signal, sys
signal.alarm(20)
def emit(v): print(json.dumps(v), flush=True)
init = json.loads(sys.stdin.readline())
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}})
for id in ['withdrawn', 'turn-ended']:
    emit({'type':'control_request','request_id':id,'request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':id,'input':{}}})
    if id == 'withdrawn': emit({'type':'control_cancel_request','request_id':id})
emit({'type':'result','subtype':'success','is_error':False,'usage':{}})
assert not sys.stdin.read(), 'late answers must not be written'
"#;
    let (mut session, mut events) = Session::spawn(move |token| Ok(launch(&path, script, token)));
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::Ready { .. }
    ));
    for id in ["withdrawn", "turn-ended"] {
        assert!(
            matches!(event(&mut events).await, HarnessEvent::RuntimeRequest { request_id, .. } if request_id == id)
        );
        assert_eq!(
            event(&mut events).await,
            HarnessEvent::RuntimeRequestResolved {
                request_id: id.into()
            }
        );
    }
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::TurnCompleted { .. }
    ));
    for id in ["withdrawn", "turn-ended"] {
        session
            .send(HarnessCommand::Answer {
                request_id: id.into(),
                decision: RuntimeDecision::Deny {
                    message: "late".into(),
                },
            })
            .unwrap();
        assert!(
            matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("no pending Claude request"))
        );
    }
    session.send(HarnessCommand::Shutdown).unwrap();
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    session.completed().await.unwrap();
}

#[tokio::test]
async fn supervisor_validates_remember_before_any_write_and_keeps_plain_flows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let script = r#"#!/usr/bin/env python3
import json, signal, sys
signal.alarm(20)
def emit(v): print(json.dumps(v), flush=True)
def read(): return json.loads(sys.stdin.readline())
choice = {'type':'addRules','destination':'localSettings','behavior':'allow','rules':[{'toolName':'Bash','ruleContent':'echo synthetic'}]}
def request(id, tool='Bash'):
    emit({'type':'control_request','request_id':id,'request':{'subtype':'can_use_tool','tool_name':tool,'tool_use_id':id,'input':{'questions':[]},'permission_suggestions':[choice]}})
init = read()
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}})
request('remember')
answer = read()
assert answer['response']['request_id'] == 'remember'
assert answer['response']['response']['updatedPermissions'] == [choice]
request('question','AskUserQuestion')
answer = read()
assert answer['response']['request_id'] == 'question'
assert answer['response']['response']['updatedInput']['answers'] == {}
assert 'updatedPermissions' not in answer['response']['response']
request('plain')
answer = read()
assert answer['response']['request_id'] == 'plain'
assert answer['response']['response']['behavior'] == 'allow'
assert 'updatedPermissions' not in answer['response']['response']
request('deny')
answer = read()
assert answer['response']['request_id'] == 'deny'
assert answer['response']['response']['behavior'] == 'deny'
request('withdrawn')
emit({'type':'control_cancel_request','request_id':'withdrawn'})
assert not sys.stdin.read(), 'rejected remembers must never reach stdin'
"#;
    let (mut session, mut events) = Session::spawn(move |token| Ok(launch(&path, script, token)));
    assert!(matches!(
        event(&mut events).await,
        HarnessEvent::Ready { .. }
    ));
    let kind = match event(&mut events).await {
        HarnessEvent::RuntimeRequest {
            request_id, kind, ..
        } => {
            assert_eq!(request_id, "remember");
            kind
        }
        e => panic!("{e:?}"),
    };
    let offered = match kind {
        RuntimeRequestKind::Permission { suggestions, .. } => suggestions[0].clone(),
        _ => panic!("expected permission"),
    };
    let remember = |update| RuntimeDecision::Allow {
        updated_input: None,
        remember: Some(update),
    };
    let answer = |id: &str, decision| {
        session
            .send(HarnessCommand::Answer {
                request_id: id.into(),
                decision,
            })
            .unwrap()
    };
    for field in ["rules", "type", "destination", "extra"] {
        let mut forged = offered.clone();
        forged[field] = serde_json::json!("forged");
        answer("remember", remember(forged));
        assert!(
            matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("not offered") && e.contains("not delivered"))
        );
    }
    answer("unknown", remember(offered.clone()));
    assert!(matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("no pending")));
    answer("remember", remember(offered.clone()));
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::RuntimeRequestResolved {
            request_id: "remember".into()
        }
    );
    assert!(
        matches!(event(&mut events).await, HarnessEvent::RuntimeRequest { request_id, .. } if request_id == "question")
    );
    answer("remember", remember(offered.clone()));
    assert!(matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("no pending")));
    answer("question", remember(offered.clone()));
    assert!(
        matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("not offered"))
    );
    answer(
        "question",
        RuntimeDecision::AnswerQuestion {
            answers: BTreeMap::new(),
        },
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::RuntimeRequestResolved {
            request_id: "question".into()
        }
    );
    for (id, decision) in [
        (
            "plain",
            RuntimeDecision::Allow {
                updated_input: None,
                remember: None,
            },
        ),
        (
            "deny",
            RuntimeDecision::Deny {
                message: "denied".into(),
            },
        ),
    ] {
        assert!(
            matches!(event(&mut events).await, HarnessEvent::RuntimeRequest { request_id, .. } if request_id == id)
        );
        answer(id, decision);
        assert_eq!(
            event(&mut events).await,
            HarnessEvent::RuntimeRequestResolved {
                request_id: id.into()
            }
        );
    }
    assert!(
        matches!(event(&mut events).await, HarnessEvent::RuntimeRequest { request_id, .. } if request_id == "withdrawn")
    );
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::RuntimeRequestResolved {
            request_id: "withdrawn".into()
        }
    );
    answer("withdrawn", remember(offered));
    assert!(matches!(event(&mut events).await, HarnessEvent::Error(e) if e.contains("no pending")));
    session.send(HarnessCommand::Shutdown).unwrap();
    assert_eq!(
        event(&mut events).await,
        HarnessEvent::ProcessExited { code: Some(0) }
    );
    session.completed().await.unwrap();
}

async fn runtime_event(events: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .unwrap()
        .expect("runtime event stream closed")
}

async fn receipt(events: &mut mpsc::UnboundedReceiver<RuntimeEvent>, id: u64) -> DeliveryOutcome {
    match runtime_event(events).await {
        RuntimeEvent::CommandOutcome {
            correlation,
            outcome,
        } => {
            assert_eq!(correlation, CommandCorrelation(id));
            outcome
        }
        event => panic!("expected receipt {id}, got {event:?}"),
    }
}

#[tokio::test]
async fn correlated_full_write_encode_rejection_and_uncorrelated_compatibility() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let (mut session, mut events) = Session::spawn_runtime(move |token| {
        Ok(launch(
            &path,
            include_str!("../../../../tests/claude-process/write-outcomes.py"),
            token,
        ))
    });
    assert!(matches!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::Ready { .. })
    ));
    assert!(matches!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::RuntimeRequest { .. })
    ));
    session
        .send_correlated(
            HarnessCommand::Answer {
                request_id: "q1".into(),
                decision: RuntimeDecision::AnswerQuestion {
                    answers: BTreeMap::new(),
                },
            },
            CommandCorrelation(1),
        )
        .unwrap();
    assert!(
        matches!(receipt(&mut events, 1).await, DeliveryOutcome::Rejected(reason) if reason.contains("not an object"))
    );
    assert!(matches!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::Error(_))
    ));
    session
        .send_correlated(
            HarnessCommand::Answer {
                request_id: "q1".into(),
                decision: RuntimeDecision::Deny {
                    message: "declined".into(),
                },
            },
            CommandCorrelation(2),
        )
        .unwrap();
    // A successful answer still closes the request only after its write/flush.
    assert!(
        matches!(runtime_event(&mut events).await, RuntimeEvent::Harness(HarnessEvent::RuntimeRequestResolved { request_id }) if request_id == "q1")
    );
    assert_eq!(receipt(&mut events, 2).await, DeliveryOutcome::Written);
    session
        .send_correlated(
            HarnessCommand::SendUserMessage(UserPrompt::from("correlated")),
            CommandCorrelation(3),
        )
        .unwrap();
    assert_eq!(receipt(&mut events, 3).await, DeliveryOutcome::Written);
    session
        .send(HarnessCommand::SendUserMessage(UserPrompt::from(
            "uncorrelated",
        )))
        .unwrap();
    for (index, command) in [
        HarnessCommand::WithdrawQueuedMessage("message".into()),
        HarnessCommand::Interrupt,
        HarnessCommand::SetPermissionMode("plan".into()),
        HarnessCommand::SetModel("synthetic".into()),
        HarnessCommand::SetEffort(Some("high".into())),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 30 + index as u64;
        session
            .send_correlated(command, CommandCorrelation(id))
            .unwrap();
        assert_eq!(receipt(&mut events, id).await, DeliveryOutcome::Written);
    }
    // Local state changes and shutdown must never claim a stdin write.
    session
        .send_correlated(
            HarnessCommand::SetWorkspaceContext(None),
            CommandCorrelation(40),
        )
        .unwrap();
    assert!(matches!(
        receipt(&mut events, 40).await,
        DeliveryOutcome::Rejected(_)
    ));
    session
        .send_correlated(HarnessCommand::Shutdown, CommandCorrelation(41))
        .unwrap();
    assert!(matches!(
        receipt(&mut events, 41).await,
        DeliveryOutcome::Rejected(_)
    ));
    assert_eq!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::ProcessExited { code: Some(0) })
    );
    session.completed().await.unwrap();
    assert!(
        events.recv().await.is_none(),
        "no duplicate or uncorrelated receipts"
    );
}

#[tokio::test]
async fn correlated_partial_write_and_death_mid_write_are_uncertain() {
    for (dies, answer) in [(false, false), (true, false), (false, true), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let script = include_str!("../../../../tests/claude-process/write-failure.py")
            .replace(
                "EXIT_MID_WRITE = False",
                &format!("EXIT_MID_WRITE = {}", if dies { "True" } else { "False" }),
            )
            .replace(
                "ANSWER = False",
                &format!("ANSWER = {}", if answer { "True" } else { "False" }),
            );
        let (mut session, mut events) =
            Session::spawn_runtime(move |token| Ok(launch(&path, &script, token)));
        assert!(matches!(
            runtime_event(&mut events).await,
            RuntimeEvent::Harness(HarnessEvent::Ready { .. })
        ));
        let pid = session.local_pid().unwrap();
        let text = "x".repeat(4 * 1024 * 1024);
        let command = if answer {
            assert!(matches!(
                runtime_event(&mut events).await,
                RuntimeEvent::Harness(HarnessEvent::RuntimeRequest { .. })
            ));
            HarnessCommand::Answer {
                request_id: "p1".into(),
                decision: RuntimeDecision::Deny { message: text },
            }
        } else {
            HarnessCommand::SendUserMessage(UserPrompt::from(text.as_str()))
        };
        session
            .send_correlated(command, CommandCorrelation(10))
            .unwrap();
        assert!(matches!(
            receipt(&mut events, 10).await,
            DeliveryOutcome::Uncertain(_)
        ));
        assert!(!std::fs::read(dir.path().join("prefix")).unwrap().is_empty());
        assert!(matches!(
            runtime_event(&mut events).await,
            RuntimeEvent::Harness(HarnessEvent::Error(_))
        ));
        if !dies {
            // The already closed pipe definitively refuses a new command.
            session
                .send_correlated(HarnessCommand::Interrupt, CommandCorrelation(11))
                .unwrap();
            assert!(matches!(
                receipt(&mut events, 11).await,
                DeliveryOutcome::Rejected(_)
            ));
            assert!(matches!(
                runtime_event(&mut events).await,
                RuntimeEvent::Harness(HarnessEvent::Error(_))
            ));
            std::fs::write(dir.path().join("exit"), "").unwrap();
        }
        loop {
            match runtime_event(&mut events).await {
                RuntimeEvent::Harness(HarnessEvent::ProcessExited { code }) => {
                    assert_eq!(code, Some(if dies { 17 } else { 0 }));
                    break;
                }
                RuntimeEvent::Harness(HarnessEvent::RuntimeRequestResolved { request_id })
                    if answer =>
                {
                    assert_eq!(request_id, "p1")
                }
                RuntimeEvent::Harness(HarnessEvent::Error(_)) => {}
                event => panic!("unexpected event: {event:?}"),
            }
        }
        session.completed().await.unwrap();
        process::assert_reaped(pid);
        assert!(events.recv().await.is_none());
    }
}

#[tokio::test]
async fn correlated_written_receipt_precedes_partial_output_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let (mut session, mut events) = Session::spawn_runtime(move |token| {
        Ok(launch(
            &path,
            include_str!("../../../../tests/claude-process/partial.py"),
            token,
        ))
    });
    assert!(matches!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::Ready { .. })
    ));
    session
        .send_correlated(
            HarnessCommand::SendUserMessage(UserPrompt::from("synthetic")),
            CommandCorrelation(20),
        )
        .unwrap();
    assert_eq!(receipt(&mut events, 20).await, DeliveryOutcome::Written);
    let mut text = String::new();
    loop {
        match runtime_event(&mut events).await {
            RuntimeEvent::Harness(HarnessEvent::TextDelta(delta)) => text.push_str(&delta),
            RuntimeEvent::Harness(HarnessEvent::Error(_)) => {}
            RuntimeEvent::Harness(HarnessEvent::ProcessExited { code: Some(17) }) => break,
            event => panic!("unexpected event: {event:?}"),
        }
    }
    assert_eq!(text, "partial output final fragment");
    session.completed().await.unwrap();
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn correlated_commands_queued_before_prepare_failure_are_rejected_once() {
    let (gate, wait) = std::sync::mpsc::channel();
    let (mut session, mut events) = Session::spawn_runtime(move |_| {
        wait.recv().unwrap();
        Err("synthetic preparation failure".into())
    });
    for id in [1, 2] {
        session
            .send_correlated(HarnessCommand::Interrupt, CommandCorrelation(id))
            .unwrap();
    }
    gate.send(()).unwrap();
    assert!(matches!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::Error(_))
    ));
    for id in [1, 2] {
        assert!(matches!(
            receipt(&mut events, id).await,
            DeliveryOutcome::Rejected(_)
        ));
    }
    assert!(matches!(
        runtime_event(&mut events).await,
        RuntimeEvent::Harness(HarnessEvent::ProcessExited { code: None })
    ));
    session.completed().await.unwrap();
    assert!(session
        .send_correlated(HarnessCommand::Interrupt, CommandCorrelation(3))
        .is_err());
    assert!(events.recv().await.is_none());
}

#[test]
fn correlated_commands_cannot_enter_a_harness_only_port() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let session = Session::from_port(tx, false);
    assert!(session
        .send_correlated(HarnessCommand::Interrupt, CommandCorrelation(1))
        .is_err());
    assert!(rx.try_recv().is_err());
    session.send(HarnessCommand::Interrupt).unwrap();
    assert_eq!(rx.try_recv().unwrap(), HarnessCommand::Interrupt);
}

#[tokio::test]
async fn closed_queue_waits_for_reserved_command_and_rejects_it_once() {
    // Unbounded send reserves internally; a bounded permit exposes the same
    // in-flight state deterministically without a scheduler-dependent race.
    let (commands, mut queue) = mpsc::channel(2);
    commands
        .send(RuntimeCommand {
            command: HarnessCommand::Interrupt,
            correlation: Some(CommandCorrelation(1)),
        })
        .await
        .unwrap();
    let permit = commands.reserve().await.unwrap();
    queue.close();
    let (tx, mut events) = mpsc::unbounded_channel();
    let sink = EventSink::Runtime(tx);
    let drain = reject_closed_queue(|cx| queue.poll_recv(cx), &sink);
    tokio::pin!(drain);
    // The first command is drained, but the reserved second send still owns
    // acceptance. Completion here would silently lose its outcome.
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    permit.send(RuntimeCommand {
        command: HarnessCommand::Interrupt,
        correlation: Some(CommandCorrelation(2)),
    });
    tokio::time::timeout(Duration::from_secs(1), drain)
        .await
        .unwrap();
    for id in [1, 2] {
        assert!(matches!(
            receipt(&mut events, id).await,
            DeliveryOutcome::Rejected(_)
        ));
    }
    assert!(
        events.try_recv().is_err(),
        "each command gets exactly one outcome"
    );
    assert!(commands
        .send(RuntimeCommand {
            command: HarnessCommand::Interrupt,
            correlation: Some(CommandCorrelation(3)),
        })
        .await
        .is_err());
}
