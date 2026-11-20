use super::*;
use crate::chat::types::{RuntimeDecision, RuntimeRequestKind, UserPrompt};
use crate::harness::claude::{ClaudeSessionConfig, SnapshotFuture, SnapshotRecorder};
use crate::turn_snapshots::{Recorder, SnapshotRoots};
use std::collections::BTreeMap;
use std::ffi::OsString;
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
