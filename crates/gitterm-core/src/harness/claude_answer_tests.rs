use super::*;
use std::task::{Context, Poll};

#[derive(Default)]
struct Writer {
    steps: VecDeque<Result<usize, std::io::ErrorKind>>,
    bytes: Vec<u8>,
    calls: usize,
    fail_flush: bool,
}
impl AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.calls += 1;
        let n = match self.steps.pop_front().unwrap_or(Ok(bytes.len())) {
            Ok(n) => n.min(bytes.len()),
            Err(e) => return Poll::Ready(Err(e.into())),
        };
        self.bytes.extend_from_slice(&bytes[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(if self.fail_flush {
            Err(std::io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        })
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
fn permission_kind() -> RuntimeRequestKind {
    RuntimeRequestKind::Permission {
        tool_name: "Bash".into(),
        input: json!({}),
        suggestions: vec![],
        decision_reason: None,
    }
}
fn allow() -> RuntimeDecision {
    RuntimeDecision::Allow {
        updated_input: None,
        remember: None,
    }
}
fn setup(
    writer: Writer,
) -> (
    SessionIo<Writer>,
    HashMap<String, PendingRequest>,
    mpsc::UnboundedReceiver<HarnessEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut pending = HashMap::new();
    record_pending(
        &mut pending,
        "p1".into(),
        "tool-1".into(),
        json!({}),
        permission_kind(),
    )
    .unwrap();
    (
        SessionIo {
            stdin: Some(writer),
            wire: None,
            events: EventSink::Harness(tx),
        },
        pending,
        rx,
    )
}

#[tokio::test]
async fn answer_zero_delivery_can_be_explicitly_retried() {
    for step in [Ok(0), Err(std::io::ErrorKind::BrokenPipe)] {
        let (mut io, mut pending, mut events) = setup(Writer {
            steps: [step].into(),
            ..Default::default()
        });
        assert!(io
            .answer(&mut pending, "p1".into(), allow())
            .await
            .unwrap_err()
            .message
            .contains("not delivered"));
        assert!(!pending["p1"].delivery_uncertain);
        assert!(events.try_recv().is_err());
        io.answer(&mut pending, "p1".into(), allow()).await.unwrap();
        assert!(pending.is_empty());
        assert_eq!(
            events.try_recv().unwrap(),
            HarnessEvent::RuntimeRequestResolved {
                request_id: "p1".into()
            }
        );
        let bytes = &io.stdin.as_ref().unwrap().bytes;
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<Value>(bytes).is_ok());
    }
}

#[tokio::test]
async fn answer_partial_and_flush_failures_block_resend_and_duplicate_reset() {
    for writer in [
        Writer {
            steps: [Ok(4), Err(std::io::ErrorKind::Interrupted), Ok(0)].into(),
            ..Default::default()
        },
        Writer {
            steps: [Ok(4), Err(std::io::ErrorKind::BrokenPipe)].into(),
            ..Default::default()
        },
        Writer {
            fail_flush: true,
            ..Default::default()
        },
    ] {
        let (mut io, mut pending, mut events) = setup(writer);
        assert!(io
            .answer(&mut pending, "p1".into(), allow())
            .await
            .unwrap_err()
            .message
            .contains("delivery uncertain"));
        let writer = io.stdin.as_ref().unwrap();
        if writer.fail_flush {
            assert_eq!(writer.bytes.last(), Some(&b'\n'));
            assert!(serde_json::from_slice::<Value>(&writer.bytes).is_ok());
        } else {
            assert_eq!(writer.bytes.len(), 4);
        }
        let calls = writer.calls;
        assert!(record_pending(
            &mut pending,
            "p1".into(),
            "forged".into(),
            json!({"changed":true}),
            permission_kind()
        )
        .is_err());
        assert_eq!(pending["p1"].tool_use_id, "tool-1");
        assert!(pending["p1"].delivery_uncertain);
        assert!(io
            .answer(&mut pending, "p1".into(), allow())
            .await
            .unwrap_err()
            .message
            .contains("resending is blocked"));
        assert_eq!(io.stdin.as_ref().unwrap().calls, calls);
        assert!(events.try_recv().is_err());
    }
}

#[tokio::test]
async fn answer_interrupted_write_preserves_frame_and_closed_stdin_is_not_delivery() {
    let (mut io, mut pending, mut events) = setup(Writer {
        steps: [
            Err(std::io::ErrorKind::Interrupted),
            Ok(4),
            Err(std::io::ErrorKind::Interrupted),
        ]
        .into(),
        ..Default::default()
    });
    io.answer(&mut pending, "p1".into(), allow()).await.unwrap();
    assert!(serde_json::from_slice::<Value>(&io.stdin.as_ref().unwrap().bytes).is_ok());
    assert!(events.try_recv().is_ok());
    let (mut io, mut pending, mut events) = setup(Writer::default());
    io.stdin = None;
    assert!(io
        .answer(&mut pending, "p1".into(), allow())
        .await
        .unwrap_err()
        .message
        .contains("not delivered"));
    assert!(pending.contains_key("p1"));
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn remember_must_exactly_match_authoritative_permission_suggestion() {
    let offered = json!({"type":"addRules","destination":"localSettings","behavior":"allow","rules":[{"toolName":"Bash","ruleContent":"echo synthetic"}]});
    let mut forged = Vec::new();
    for (key, value) in [
        ("type", json!("setMode")),
        ("destination", json!("userSettings")),
        ("rules", json!([{"toolName":"Bash","ruleContent":"*"}])),
        ("extra", json!(true)),
    ] {
        let mut update = offered.clone();
        update[key] = value;
        forged.push(update);
    }
    let mut missing = offered.clone();
    missing.as_object_mut().unwrap().remove("destination");
    forged.push(missing);
    for update in forged {
        let (mut io, mut pending, mut events) = setup(Writer::default());
        if let RuntimeRequestKind::Permission { suggestions, .. } =
            &mut pending.get_mut("p1").unwrap().kind
        {
            suggestions.push(offered.clone());
        }
        let answer = |remember| RuntimeDecision::Allow {
            updated_input: None,
            remember: Some(remember),
        };
        assert!(io
            .answer(&mut pending, "p1".into(), answer(update))
            .await
            .unwrap_err()
            .message
            .contains("not offered"));
        assert_eq!(io.stdin.as_ref().unwrap().calls, 0);
        assert!(events.try_recv().is_err());
        assert!(pending.contains_key("p1"));
        io.answer(&mut pending, "p1".into(), answer(offered.clone()))
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&io.stdin.as_ref().unwrap().bytes).unwrap();
        assert_eq!(
            value["response"]["response"]["updatedPermissions"],
            json!([offered])
        );
        assert!(events.try_recv().is_ok());
        let calls = io.stdin.as_ref().unwrap().calls;
        assert!(io
            .answer(&mut pending, "p1".into(), answer(offered.clone()))
            .await
            .unwrap_err()
            .message
            .contains("no pending"));
        assert_eq!(io.stdin.as_ref().unwrap().calls, calls);
    }
}

#[tokio::test]
async fn remember_rejects_unknown_question_and_unoffered_requests_without_writes() {
    for kind in [
        permission_kind(),
        RuntimeRequestKind::Question {
            questions: json!([]),
        },
    ] {
        let (mut io, mut pending, mut events) = setup(Writer::default());
        pending.get_mut("p1").unwrap().kind = kind;
        for id in ["unknown", "p1"] {
            assert!(io
                .answer(
                    &mut pending,
                    id.into(),
                    RuntimeDecision::Allow {
                        updated_input: None,
                        remember: Some(json!({"type":"addRules","destination":"localSettings"}))
                    }
                )
                .await
                .is_err());
        }
        assert_eq!(io.stdin.as_ref().unwrap().calls, 0);
        assert!(events.try_recv().is_err());
        io.answer(
            &mut pending,
            "p1".into(),
            RuntimeDecision::Deny {
                message: "dismiss".into(),
            },
        )
        .await
        .unwrap();
        assert!(events.try_recv().is_ok());
    }
}

#[tokio::test]
async fn delivery_classification_uses_bytes_and_flush_not_error_text() {
    for (writer, uncertain) in [
        (
            Writer {
                steps: [Ok(0)].into(),
                ..Default::default()
            },
            false,
        ),
        (
            Writer {
                steps: [Err(std::io::ErrorKind::BrokenPipe)].into(),
                ..Default::default()
            },
            false,
        ),
        (
            Writer {
                steps: [Ok(4), Ok(0)].into(),
                ..Default::default()
            },
            true,
        ),
        (
            Writer {
                steps: [Ok(4), Err(std::io::ErrorKind::BrokenPipe)].into(),
                ..Default::default()
            },
            true,
        ),
        (
            Writer {
                fail_flush: true,
                ..Default::default()
            },
            true,
        ),
    ] {
        let (mut io, _, _) = setup(writer);
        let error = io
            .write_delivery("synthetic frame".into())
            .await
            .unwrap_err();
        assert_eq!(
            matches!(error.outcome(), DeliveryOutcome::Uncertain(_)),
            uncertain
        );
        assert_eq!(
            matches!(error.outcome(), DeliveryOutcome::Rejected(_)),
            !uncertain
        );
    }
}
