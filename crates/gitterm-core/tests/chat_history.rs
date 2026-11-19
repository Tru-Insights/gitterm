use gitterm_core::chat::history::{
    harness_payload, history_payloads, history_timeline, HistoryEvent, DELIVERY_SOURCE_LINE,
};
use gitterm_core::chat::html_render::{is_render_output, MAX_HTML_BYTES};
use gitterm_core::chat::types::{HarnessEvent, TurnStatus};
use gitterm_core::harness::transcript::{TimedEntry, TranscriptEntry};
use serde_json::{json, Value};

fn payloads(events: Vec<HistoryEvent>) -> Vec<Value> {
    events
        .into_iter()
        .map(|event| match event {
            HistoryEvent::Harness(event) => harness_payload(&event).unwrap(),
            HistoryEvent::Other(value) => value,
        })
        .collect()
}

#[test]
fn desktop_history_keeps_delivery_anchors_and_timing_while_phone_keeps_its_projection() {
    let text =
        format!("{DELIVERY_SOURCE_LINE}\nWorker result\nFinished\nFull record: delegation_get d1");
    let start = HarnessEvent::SubagentEvent {
        parent_tool_use_id: "a1".into(),
        event: Box::new(HarnessEvent::TurnStarted {
            session_id: None,
            model: None,
        }),
    };
    let end = HarnessEvent::TurnCompleted {
        status: TurnStatus::Completed,
        usage: Value::Null,
        cost_usd: None,
        rate_limit: None,
    };
    let entries = vec![
        TimedEntry {
            at: Some("2026-11-05T10:00:00Z".into()),
            entry: TranscriptEntry::UserPrompt(text.clone()),
        },
        TimedEntry {
            at: Some("2026-11-05T10:00:01Z".into()),
            entry: TranscriptEntry::Harness(start.clone()),
        },
        TimedEntry {
            at: Some("2026-11-05T10:00:04Z".into()),
            entry: TranscriptEntry::Harness(end.clone()),
        },
        TimedEntry {
            at: Some("malformed timestamp".into()),
            entry: TranscriptEntry::UserPrompt("next".into()),
        },
    ];
    let prompt = json!({"type":"user_prompt", "text":text, "delivery":{
        "header":"Worker result", "delegation_id":"d1", "body":"Finished\nFull record: delegation_get d1"
    }});
    let subagent_time =
        json!({"kind":"subagent_started_at", "id":"a1", "at_ms":1_793_872_801_000_i64});
    let phone = history_payloads(entries.clone());
    assert_eq!(
        phone,
        vec![
            prompt.clone(),
            harness_payload(&start).unwrap(),
            subagent_time.clone(),
            harness_payload(&end).unwrap(),
            json!({"type":"user_prompt", "text":"next"}),
        ]
    );
    assert_eq!(
        payloads(history_timeline(
            entries,
            &[("d1".into(), "2026-11-05T10:00:00Z".into())]
        )),
        vec![
            prompt,
            json!({"kind":"turn_timing", "start_ms":1_793_872_800_000_i64}),
            json!({"kind":"delegation_anchor", "delegation_id":"d1"}),
            harness_payload(&start).unwrap(),
            subagent_time,
            json!({"kind":"turn_timing", "end_ms":1_793_872_804_000_i64}),
            harness_payload(&end).unwrap(),
            json!({"type":"user_prompt", "text":"next"}),
        ]
    );
}

#[test]
fn visual_result_recognition_keeps_the_existing_strict_contract() {
    for height in [100, 4000] {
        assert!(is_render_output(
            &json!({"html_render":{"html":"<p>ok</p>", "height":height}}).to_string()
        ));
    }
    for value in [
        json!({"html_render":{"html":" ", "height":600}}),
        json!({"html_render":{"html":"ok", "height":99}}),
        json!({"html_render":{"html":"ok", "height":4001}}),
        json!({"html_render":{"html":"ok", "height":-1}}),
        json!({"html_render":{"html":"ok", "height":"600"}}),
        json!({"html_render":{"html":"ok", "height":600, "extra":true}}),
        json!({"html_render":{"html":"ok", "height":600}, "extra":true}),
        json!({"html_render":{"html":"é".repeat(MAX_HTML_BYTES / 2 + 1), "height":600}}),
    ] {
        assert!(!is_render_output(&value.to_string()), "{value}");
    }
    // Escaped control characters exercise the existing encoded-envelope allowance.
    assert!(is_render_output(
        &json!({"html_render":{"html":"\u{1}".repeat(MAX_HTML_BYTES), "height":600}}).to_string()
    ));
    assert!(!is_render_output("{partial"));
}
