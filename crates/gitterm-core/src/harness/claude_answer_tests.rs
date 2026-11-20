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
    record_pending(&mut pending, "p1".into(), "tool-1".into(), json!({})).unwrap();
    (
        SessionIo {
            stdin: Some(writer),
            wire: None,
            events: tx,
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
            json!({"changed":true})
        )
        .is_err());
        assert_eq!(pending["p1"].tool_use_id, "tool-1");
        assert!(pending["p1"].delivery_uncertain);
        assert!(io
            .answer(&mut pending, "p1".into(), allow())
            .await
            .unwrap_err()
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
        .contains("not delivered"));
    assert!(pending.contains_key("p1"));
    assert!(events.try_recv().is_err());
}
