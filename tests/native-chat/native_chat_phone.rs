use super::super::claude_chats::{ChatLimits, ChatProcess};
use super::super::phones::PhoneScope;
use super::*;
use crate::harness::{HarnessEvent, RuntimeRequestKind};
use tokio::sync::mpsc;
#[path = "native_chat_support.rs"]
mod support;

struct Fake(mpsc::UnboundedSender<HarnessCommand>);
impl ChatProcess for Fake {
    fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.0
            .send(command)
            .map_err(|_| "synthetic process gone".into())
    }
}

#[tokio::test]
async fn native_chat_phone_payload_reconnect_and_command_goldens() {
    let dir = tempfile::tempdir().unwrap();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let slot = Arc::new(Mutex::new(Some((event_rx, command_tx))));
    let chats = ClaudeChatRegistry::with_parts(
        ChatLimits {
            buffer_max_events: 2,
            ..ChatLimits::default()
        },
        Arc::new(move |_| {
            let (events, commands) = slot.lock().unwrap().take().unwrap();
            (Box::new(Fake(commands)), events)
        }),
        Arc::new(std::time::SystemTime::now),
    );
    let mut desktop = chats.start_or_attach(
        crate::harness::claude::ClaudeSessionConfig {
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
        None,
    );
    let events = [
        HarnessEvent::Ready {
            session_id: Some("synthetic-session".into()),
            permission_mode: Some("default".into()),
            models: Vec::new(),
            modes: Vec::new(),
            commands: Vec::new(),
        },
        HarnessEvent::RuntimeRequest {
            request_id: "permission-1".into(),
            tool_use_id: "tool-1".into(),
            kind: RuntimeRequestKind::Permission {
                tool_name: "Bash".into(),
                input: json!({"command":"true"}),
                suggestions: Vec::new(),
                decision_reason: None,
            },
        },
        HarnessEvent::TextDelta("first".into()),
        HarnessEvent::TextDelta("second".into()),
    ];
    let mut live = Vec::new();
    for event in events {
        event_tx.send(event).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), desktop.live.recv())
            .await
            .unwrap()
            .unwrap();
        live.push(serde_json::from_str::<Value>(&event_frame(&event, false)).unwrap());
    }
    let mut reconnects = Vec::new();
    for after in [None, Some(1), Some(3), Some(4), Some(99)] {
        let attachment = chats.attach(&desktop.opened.chat_id, after).unwrap();
        reconnects.push(json!({"after_seq":after,"head_seq":attachment.opened.head_seq,"gap":attachment.opened.gap,"session":attachment.opened.claude_session_id,
            "position":attachment.position,
            "redelivered":attachment.redelivered.iter().map(|e|serde_json::from_str::<Value>(&event_frame(e,true)).unwrap()).collect::<Vec<_>>(),
            "replay":attachment.replay.iter().map(|e|serde_json::from_str::<Value>(&event_frame(e,false)).unwrap()).collect::<Vec<_>>() }));
    }
    support::golden(
        "phone/sequence.json",
        json!({"live":live,"reconnect":reconnects}),
    );
    let mut phone = PhoneInfo {
        id: "synthetic-phone".into(),
        name: "Fixture phone".into(),
        scope: PhoneScope::Act,
        created_unix_secs: 0,
        last_seen_unix_secs: 0,
    };
    let commands = [
        json!({"type":"send","id":"00000000-0000-4000-8000-000000000001","text":"Synthetic hello"}),
        json!({"type":"answer","request_id":"permission-1","decision":{"kind":"deny","message":"Synthetic denial"}}),
        json!({"type":"interrupt"}),
        json!({"type":"shutdown"}),
        json!({"type":"set_model","data":"synthetic-model"}),
        json!({"type":"withdraw","id":"queued-1"}),
        json!({"type":"send","id":"invalid","text":"hi"}),
        json!({"type":"send","id":"00000000-0000-4000-8000-000000000001","text":" "}),
        json!({"type":"answer","request_id":"gone","decision":{"kind":"allow"}}),
        json!({"text":"missing type"}),
    ];
    let mut results = Vec::new();
    for input in commands {
        let result = act(&phone, &desktop.chat, &Message::text(input.to_string()));
        let forwarded = command_rx
            .try_recv()
            .ok()
            .map(|c| serde_json::to_value(c).unwrap());
        results.push(json!({"input":input,"reply":result,"forwarded":forwarded}));
    }
    phone.scope = PhoneScope::View;
    results.push(json!({"view_interrupt":act(&phone, &desktop.chat, &Message::text("{\"type\":\"interrupt\"}"))}));
    assert!(command_rx.try_recv().is_err());
    support::golden("phone/commands.json", json!(results));
    // A desktop/phone detach does not send Shutdown to the registry-owned process.
    drop(chats.attach(&desktop.opened.chat_id, Some(4)).unwrap());
    assert!(command_rx.try_recv().is_err());
    desktop.chat.send(HarnessCommand::Shutdown).unwrap();
    assert!(matches!(
        command_rx.try_recv(),
        Ok(HarnessCommand::Shutdown)
    ));
}

#[test]
fn native_chat_phone_hello_and_optional_payload_compatibility() {
    let hello: SocketHello =
        serde_json::from_str(include_str!("../fixtures/native-chat/phone/hello.json")).unwrap();
    assert_eq!(hello.kind, "hello");
    assert_eq!(hello.token, "synthetic-device-token");
    assert_eq!(hello.chat_id, "synthetic-chat");
    assert_eq!(hello.after_seq, Some(4));
    let legacy: SocketHello = serde_json::from_value(
        json!({"type":"hello","token":"synthetic-device-token","chat_id":"synthetic-chat"}),
    )
    .unwrap();
    assert_eq!(legacy.after_seq, None);
    let events = [
        HarnessEvent::UserMessageEcho {
            id: Some("prompt-1".into()),
            text: "Synthetic relay".into(),
            image_count: 2,
            delivery: None,
        },
        HarnessEvent::TurnCompleted {
            status: crate::harness::TurnStatus::Completed,
            usage: json!({"output_tokens":2}),
            cost_usd: Some(0.01),
            rate_limit: Some(crate::harness::RateLimitReading {
                observed_at: 0,
                info: json!({"status":"allowed","resetsAt":0}),
            }),
        },
    ];
    let frames: Vec<Value> = events
        .iter()
        .enumerate()
        .map(|(i, event)| {
            let seq = SeqEvent {
                seq: i as u64 + 1,
                json: serde_json::to_string(event).unwrap().into(),
                exited: false,
            };
            serde_json::from_str(&event_frame(&seq, false)).unwrap()
        })
        .collect();
    support::golden("phone/optional-payloads.json", json!(frames));
    let legacy_event: HarnessEvent = serde_json::from_value(json!({"type":"turn_completed","data":{"status":{"state":"completed"},"usage":null,"cost_usd":null}})).unwrap();
    assert!(matches!(
        legacy_event,
        HarnessEvent::TurnCompleted {
            rate_limit: None,
            ..
        }
    ));
}

// Called by the existing fake-socket test; this adds no connection or server.
pub(super) fn socket_golden(frames: Value) {
    support::golden("phone/socket.json", frames);
}
