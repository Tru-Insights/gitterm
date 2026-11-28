use super::*;
#[cfg(unix)]
#[path = "native_chat_process.rs"]
mod process;
#[path = "native_chat_support.rs"]
mod support;

#[test]
fn native_chat_claude_parser_golden() {
    let mut parser = ClaudeFrameParser::new();
    let frames: Vec<Value> = include_str!("../fixtures/native-chat/claude/stream.jsonl")
        .lines()
        .flat_map(|line| parser.parse_line(line))
        .map(|frame| match frame {
            ParsedFrame::Event(event) => json!({"event": event}),
            ParsedFrame::PermissionRequest { request_id, tool_use_id, input, event } =>
                json!({"request_id":request_id,"tool_use_id":tool_use_id,"input":input,"event":event}),
            other => json!({"control": format!("{other:?}")}),
        })
        .collect();
    support::golden("claude/parser.json", json!(frames));
}

#[test]
fn native_chat_claude_picker_and_queue_ack_golden() {
    let mut parser = ClaudeFrameParser::new();
    let cases = [
        (
            "init",
            HostRequest::Initialize {
                resume: Some("synthetic-chat".into()),
            },
            json!({"current_permission_mode":"default","models":[],"commands":[]}),
        ),
        (
            "model",
            HostRequest::SetModel("synthetic-model".into()),
            json!({}),
        ),
        (
            "mode",
            HostRequest::SetPermissionMode("plan".into()),
            json!({"mode":"plan"}),
        ),
        (
            "effort",
            HostRequest::ConfirmEffort(Some("high".into())),
            json!({"sources":[{"source":"flagSettings","settings":{"effortLevel":"high"}}],"applied":{"effort":"high"}}),
        ),
        (
            "withdraw",
            HostRequest::WithdrawMessage("queued-1".into()),
            json!({"cancelled":false}),
        ),
    ];
    let mut events = Vec::new();
    for (id, request, body) in cases {
        parser.register_host_request(id.into(), request);
        events.extend(parser.parse_line(&json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":body}}).to_string()).into_iter().filter_map(|frame| match frame { ParsedFrame::Event(e) => Some(e), _ => None }));
    }
    for (id, request) in [
        (
            "rejected-model",
            HostRequest::SetModel("unsupported".into()),
        ),
        (
            "rejected-mode",
            HostRequest::SetPermissionMode("unsupported".into()),
        ),
        (
            "rejected-effort",
            HostRequest::ConfirmEffort(Some("high".into())),
        ),
    ] {
        parser.register_host_request(id.into(), request);
        events.extend(parser.parse_line(&json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"synthetic rejection"}}).to_string()).into_iter().filter_map(|frame| match frame { ParsedFrame::Event(e) => Some(e), _ => None }));
    }
    support::golden("claude/acknowledgements.json", json!(events));
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_claude_fake_process_lifecycle() {
    for stubborn in [false, true] {
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = tempfile::tempdir().unwrap();
        if stubborn {
            std::fs::write(dir.path().join("hang"), "").unwrap();
        }
        let program = process::script(
            dir.path(),
            "fake-claude",
            include_str!("../fixtures/native-chat/claude/process.py"),
        );
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, mut events) = mpsc::unbounded_channel();
        let process_id = std::sync::Arc::new(AtomicU32::new(0));
        let config = ClaudeSessionConfig {
            snapshot_facts_dir: None,
            cwd: dir.path().into(),
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        };
        let task = tokio::spawn(run_session_program(
            config,
            rx,
            tx,
            tokio_util::sync::CancellationToken::new(),
            program,
            process_id.clone(),
        ));
        let mut seen = Vec::new();
        let mut pid = 0;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            match &event {
                HarnessEvent::Ready { .. } => {
                    pid = process_id.load(Ordering::Relaxed);
                    assert_ne!(pid, 0);
                    commands
                        .send(HarnessCommand::SendUserMessage(UserPrompt {
                            text: "Inspect fixture".into(),
                            images: Vec::new(),
                            id: Some("prompt-1".into()),
                        }))
                        .unwrap();
                }
                HarnessEvent::RuntimeRequest {
                    request_id, kind, ..
                } => commands
                    .send(HarnessCommand::Answer {
                        request_id: request_id.clone(),
                        decision: match kind {
                            RuntimeRequestKind::Question { .. } => {
                                RuntimeDecision::AnswerQuestion {
                                    answers: std::collections::BTreeMap::from([(
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
                    .unwrap(),
                HarnessEvent::RuntimeRequestResolved { request_id }
                    if request_id == "question-1" =>
                {
                    commands.send(HarnessCommand::Interrupt).unwrap()
                }
                HarnessEvent::TurnCompleted { .. } => {
                    commands.send(HarnessCommand::Shutdown).unwrap()
                }
                HarnessEvent::Error(message) => panic!("fake child: {message}"),
                HarnessEvent::ProcessExited { .. } => {
                    seen.push(event);
                    break;
                }
                _ => {}
            }
            seen.push(event);
        }
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        process::assert_reaped(pid);
        assert_eq!(process_id.load(Ordering::Relaxed), 0);
        support::golden(
            if stubborn {
                "claude/shutdown-kill.json"
            } else {
                "claude/lifecycle.json"
            },
            json!(seen),
        );
        support::golden(
            "claude/stdin.json",
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("stdin.json")).unwrap())
                .unwrap(),
        );
    }
}

#[test]
fn native_chat_local_close_remote_detach_golden() {
    let mut cases = Vec::new();
    for (local, explicit_close) in [(true, false), (false, false), (false, true)] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = ClaudeSession {
            inner: Some(Session::from_port(tx, local)),
        };
        if explicit_close {
            session.send(HarnessCommand::Shutdown).unwrap();
        }
        drop(session);
        let mut sent = Vec::new();
        while let Ok(command) = rx.try_recv() {
            sent.push(command);
        }
        cases.push(json!({"local":local,"explicit_close":explicit_close,"commands":sent,"channel_closed":rx.is_closed()}));
    }
    support::golden("claude/close-detach.json", json!(cases));
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_claude_failed_answer_remains_pending_until_process_exit() {
    let dir = tempfile::tempdir().unwrap();
    let program = process::script(
        dir.path(),
        "fake-closed-stdin",
        include_str!("../fixtures/native-chat/claude/closed-stdin.py"),
    );
    let (commands, rx) = mpsc::unbounded_channel();
    let (tx, mut events) = mpsc::unbounded_channel();
    let config = ClaudeSessionConfig {
        snapshot_facts_dir: None,
        cwd: dir.path().into(),
        model: None,
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: None,
        mcp_servers: Vec::new(),
        chrome: false,
        guidelines_file: None,
        guidelines_content: None,
    };
    let task = tokio::spawn(run_session_program(
        config,
        rx,
        tx,
        tokio_util::sync::CancellationToken::new(),
        program,
        std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
    ));
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        match &event {
            HarnessEvent::Ready { .. } => commands
                .send(HarnessCommand::SendUserMessage(UserPrompt::from(
                    "Synthetic prompt",
                )))
                .unwrap(),
            HarnessEvent::RuntimeRequest { request_id, .. } => commands
                .send(HarnessCommand::Answer {
                    request_id: request_id.clone(),
                    decision: RuntimeDecision::Allow {
                        updated_input: None,
                        remember: None,
                    },
                })
                .unwrap(),
            HarnessEvent::Error(e) => {
                assert!(e.starts_with("answer p1: write to claude stdin:"), "{e}");
                assert!(e.ends_with("not delivered; request remains pending"), "{e}");
                assert!(!seen
                    .iter()
                    .any(|e| matches!(e, HarnessEvent::RuntimeRequestResolved { .. })));
                commands.send(HarnessCommand::Shutdown).unwrap();
                std::fs::write(dir.path().join("exit"), "").unwrap();
            }
            HarnessEvent::ProcessExited { .. } => {
                seen.push(event);
                break;
            }
            _ => {}
        }
        seen.push(event);
    }
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    let pid = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    process::assert_reaped(pid);
    // Pin the semantic error without an OS-specific Broken pipe suffix.
    let mut value = json!(seen);
    for e in value.as_array_mut().unwrap() {
        if e["type"] == "error" {
            e["data"] = json!("answer p1: write to claude stdin: <broken pipe>; not delivered; request remains pending");
        }
    }
    support::golden("claude/failed-answer.json", value);
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_claude_startup_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, rx) = mpsc::unbounded_channel();
    let (tx, mut events) = mpsc::unbounded_channel();
    let config = ClaudeSessionConfig {
        snapshot_facts_dir: None,
        cwd: dir.path().into(),
        model: None,
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: None,
        mcp_servers: Vec::new(),
        chrome: false,
        guidelines_file: None,
        guidelines_content: None,
    };
    let pid = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    run_session_program(
        config,
        rx,
        tx,
        tokio_util::sync::CancellationToken::new(),
        dir.path().join("absent-fake"),
        pid.clone(),
    )
    .await;
    assert!(
        matches!(events.recv().await, Some(HarnessEvent::Error(e)) if e.contains("failed to start") && e.contains("absent-fake"))
    );
    assert_eq!(
        events.recv().await,
        Some(HarnessEvent::ProcessExited { code: None })
    );
    assert_eq!(pid.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(commands.is_closed());
}

// Match the Iced WebView card: choose the offered localSettings object and
// send that exact object as remember, without rebuilding the permission rule.
#[cfg(unix)]
#[tokio::test]
async fn native_chat_claude_iced_offered_remember_flow() {
    let dir = tempfile::tempdir().unwrap();
    let program = process::script(
        dir.path(),
        "fake-remember",
        r#"#!/usr/bin/env python3
import json, signal, sys
signal.alarm(20)
def emit(v): print(json.dumps(v), flush=True)
init = json.loads(sys.stdin.readline())
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{}}})
choice = {'type':'addRules','destination':'localSettings','behavior':'allow','rules':[{'toolName':'Bash','ruleContent':'echo synthetic'}]}
emit({'type':'control_request','request_id':'remember','request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':'t1','input':{'command':'echo synthetic'},'permission_suggestions':[choice]}})
answer = json.loads(sys.stdin.readline())
assert answer['response']['response'] == {'behavior':'allow','toolUseID':'t1','updatedInput':{'command':'echo synthetic'},'updatedPermissions':[choice]}
assert not sys.stdin.read()
"#,
    );
    let (commands, rx) = mpsc::unbounded_channel();
    let (tx, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_session_program(
        ClaudeSessionConfig {
            snapshot_facts_dir: None,
            cwd: dir.path().into(),
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: None,
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        },
        rx,
        tx,
        tokio_util::sync::CancellationToken::new(),
        program,
        std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
    ));
    let mut resolved = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            HarnessEvent::RuntimeRequest {
                request_id,
                kind: RuntimeRequestKind::Permission { suggestions, .. },
                ..
            } => {
                let choice = suggestions
                    .into_iter()
                    .find(|s| s["destination"] == "localSettings")
                    .unwrap();
                commands
                    .send(HarnessCommand::Answer {
                        request_id,
                        decision: RuntimeDecision::Allow {
                            updated_input: None,
                            remember: Some(choice),
                        },
                    })
                    .unwrap();
            }
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                assert_eq!(request_id, "remember");
                assert!(!resolved);
                resolved = true;
                commands.send(HarnessCommand::Shutdown).unwrap();
            }
            HarnessEvent::Error(e) => panic!("{e}"),
            HarnessEvent::ProcessExited { code } => {
                assert_eq!(code, Some(0));
                break;
            }
            _ => {}
        }
    }
    assert!(resolved);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
}
