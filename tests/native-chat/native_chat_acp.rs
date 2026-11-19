use super::*;
#[cfg(unix)]
#[path = "native_chat_process.rs"]
mod process;
#[path = "native_chat_support.rs"]
mod support;

std::thread_local! {
    static PROGRAM: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn program() -> Option<PathBuf> {
    PROGRAM.with(|p| p.borrow().clone())
}

#[test]
fn native_chat_acp_parser_golden() {
    let mut mapper = UpdateMapper::new();
    let mut events = Vec::new();
    let mut options = Vec::new();
    let mut malformed = 0;
    for line in include_str!("../fixtures/native-chat/acp/stream.jsonl").lines() {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            malformed += 1;
            continue;
        };
        match frame["method"].as_str() {
            Some("session/update") => {
                for mapped in mapper.map(&frame["params"]["update"]) {
                    match mapped {
                        Mapped::Event(e) => events.push(e),
                        other => panic!("unexpected {other:?}"),
                    }
                }
            }
            Some("session/request_permission") => {
                let (items, tool_use_id, kind) = mapper.permission(&frame["params"]);
                events.extend(items);
                events.push(HarnessEvent::RuntimeRequest {
                    request_id: "permission-1".into(),
                    tool_use_id,
                    kind,
                });
                options = frame["params"]["options"].as_array().unwrap().clone();
            }
            _ => {}
        }
    }
    assert_eq!(malformed, 1); // The process reader also ignores a partial JSON line.
    support::golden("acp/parser.json", json!(events));
    let decisions = [
        RuntimeDecision::Allow {
            updated_input: None,
            remember: None,
        },
        RuntimeDecision::Deny {
            message: "Synthetic denial".into(),
        },
        RuntimeDecision::AnswerQuestion {
            answers: std::collections::BTreeMap::new(),
        },
    ];
    support::golden(
        "acp/answers.json",
        json!(decisions
            .iter()
            .map(|d| permission_outcome(d, &options))
            .collect::<Vec<_>>()),
    );
}

#[test]
fn native_chat_acp_queue_withdraw_golden() {
    let mut queue = PromptQueue::default();
    let prompt = |id: &str| UserPrompt {
        text: format!("Synthetic {id}"),
        images: Vec::new(),
        id: Some(id.into()),
    };
    let first = queue.submit(prompt("first"), true);
    let second = queue.submit(prompt("second"), false);
    let third = queue.submit(prompt("third"), false);
    let cancelled = queue.withdraw("third");
    let refused = queue.withdraw("first");
    let next = queue.pop_next();
    support::golden(
        "acp/queue.json",
        json!({"first":first,"second":second,"third":third,"cancelled":cancelled,"refused":refused,"next":next,"empty":queue.is_empty()}),
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_acp_fake_process_lifecycle_and_resume() {
    for (resume, stubborn) in [(false, false), (true, false), (false, true)] {
        let dir = tempfile::tempdir().unwrap();
        if stubborn {
            std::fs::write(dir.path().join("hang"), "").unwrap();
        }
        let program = process::script(
            dir.path(),
            "fake-acp",
            include_str!("../fixtures/native-chat/acp/process.py"),
        );
        PROGRAM.with(|p| *p.borrow_mut() = Some(program));
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, mut events) = mpsc::unbounded_channel();
        let config = AcpSessionConfig {
            agent: AcpAgent::OpenCode,
            cwd: dir.path().into(),
            resume: resume.then(|| "synthetic-chat".into()),
            model: None,
            mode: None,
            effort: None,
            mcp_servers: Vec::new(),
            env: Vec::new(),
            wire_log_dir: None,
            guidelines: None,
            snapshot_facts_dir: None,
        };
        let task = tokio::spawn(run_session(
            config,
            rx,
            tx,
            tokio_util::sync::CancellationToken::new(),
        ));
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            match &event {
                HarnessEvent::Ready { .. } => commands
                    .send(HarnessCommand::SendUserMessage(UserPrompt {
                        text: "Inspect fixture".into(),
                        images: vec![super::super::ImageAttachment {
                            media_type: "image/png".into(),
                            data: "QQ==".into(),
                        }],
                        id: Some("prompt-1".into()),
                    }))
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
                HarnessEvent::RuntimeRequestResolved { .. } => {
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
        PROGRAM.with(|p| *p.borrow_mut() = None);
        let pid = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        process::assert_reaped(pid);
        let mut value = json!(seen);
        for event in value.as_array_mut().unwrap() {
            if event["type"] == "runtime_request" || event["type"] == "runtime_request_resolved" {
                event["data"]["request_id"] = json!("<request>");
            }
        }
        let suffix = if stubborn {
            "shutdown-kill"
        } else if resume {
            "resume"
        } else {
            "lifecycle"
        };
        support::golden(&format!("acp/{suffix}.json"), value);
        support::golden(
            &format!("acp/{suffix}-stdin.json"),
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("stdin.json")).unwrap())
                .unwrap(),
        );
    }
}

#[test]
fn native_chat_acp_close_sends_shutdown() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let session = AcpSession {
        commands: tx,
        snapshots_closed: tokio_util::sync::CancellationToken::new(),
    };
    drop(session);
    assert_eq!(rx.try_recv().unwrap(), HarnessCommand::Shutdown);
    assert!(rx.is_closed());
}

#[cfg(unix)]
#[tokio::test]
async fn native_chat_acp_startup_failure() {
    let dir = tempfile::tempdir().unwrap();
    PROGRAM.with(|p| *p.borrow_mut() = Some(dir.path().join("absent-fake")));
    let (commands, rx) = mpsc::unbounded_channel();
    let (tx, mut events) = mpsc::unbounded_channel();
    let config = AcpSessionConfig {
        agent: AcpAgent::OpenCode,
        cwd: dir.path().into(),
        resume: None,
        model: None,
        mode: None,
        effort: None,
        mcp_servers: Vec::new(),
        env: Vec::new(),
        wire_log_dir: None,
        guidelines: None,
        snapshot_facts_dir: None,
    };
    run_session(config, rx, tx, tokio_util::sync::CancellationToken::new()).await;
    PROGRAM.with(|p| *p.borrow_mut() = None);
    assert!(
        matches!(events.recv().await, Some(HarnessEvent::Error(e)) if e.contains("failed to start") && e.contains("absent-fake"))
    );
    assert_eq!(
        events.recv().await,
        Some(HarnessEvent::ProcessExited { code: None })
    );
    assert!(commands.is_closed());
}
