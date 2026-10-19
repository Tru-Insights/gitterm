//! The chat page (`assets/agent_chat.html`) and the JSON it renders, kept
//! free of iced and wry so both the desktop's chat tabs and the agent's
//! phone pages (TRU-151) build it the same way.
//!
//! The page takes events through `window.__appendEvent` / `__replay`:
//! harness events wrapped as `{"kind": "harness", "event": ...}`, the
//! human's prompts as `{"type": "user_prompt", "text": ...}`, and markers
//! such as `subagent_started_at`.

use serde_json::Value;

use crate::harness::transcript::{TimedEntry, TranscriptEntry};
use crate::harness::HarnessEvent;

/// Embedded HTML scaffold for the agent chat surface (Step 4 of TRU-29).
pub const AGENT_CHAT_HTML: &str = include_str!("../assets/agent_chat.html");
/// Vendored renderers inlined into the chat page (no network at runtime).
/// Licenses sit next to them in `assets/vendor/`.
pub const AGENT_CHAT_VENDOR_JS: [&str; 2] = [
    include_str!("../assets/vendor/marked.min.js"),
    include_str!("../assets/vendor/highlight.min.js"),
];
pub const AGENT_CHAT_VENDOR_SLOT: &str = "/*__VENDOR_JS__*/";

/// The chat page with the vendored scripts inlined into its slot.
pub fn agent_chat_html() -> String {
    debug_assert!(AGENT_CHAT_HTML.contains(AGENT_CHAT_VENDOR_SLOT));
    AGENT_CHAT_HTML.replacen(
        AGENT_CHAT_VENDOR_SLOT,
        &AGENT_CHAT_VENDOR_JS.join("\n;\n"),
        1,
    )
}

/// The page's echo of a prompt the human submitted.
pub fn user_prompt_payload(text: &str) -> Value {
    serde_json::json!({"type": "user_prompt", "text": text})
}

/// A harness event as the page's `__appendEvent` receives it. `None` (and
/// logged) when the event cannot be encoded.
pub fn harness_payload(event: &HarnessEvent) -> Option<Value> {
    match serde_json::to_value(event) {
        Ok(event) => Some(serde_json::json!({"kind": "harness", "event": event})),
        Err(e) => {
            eprintln!("[agent] could not serialize harness event {event:?}: {e}");
            None
        }
    }
}

/// The Agent call whose subagent `event` starts, if it is a subagent start.
pub fn subagent_started(event: &HarnessEvent) -> Option<&str> {
    match event {
        HarnessEvent::SubagentEvent {
            parent_tool_use_id,
            event,
        } if matches!(**event, HarnessEvent::TurnStarted { .. }) => {
            Some(parent_tool_use_id.as_str())
        }
        _ => None,
    }
}

/// The timeline marker for when Agent call `parent`'s subagent started
/// (Unix ms). The chat page uses it when the start was not seen live.
pub fn subagent_started_at(parent: &str, at_ms: i64) -> Value {
    serde_json::json!({ "kind": "subagent_started_at", "id": parent, "at_ms": at_ms })
}

/// A transcript read-back as page payloads, each subagent start followed
/// by when it started (from its transcript line). The desktop adds its
/// delegation anchors on top of this; an agent has none.
pub fn history_payloads(entries: Vec<TimedEntry>) -> Vec<Value> {
    let mut payloads = Vec::with_capacity(entries.len());
    for timed in entries {
        let at_ms = timed
            .at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis());
        match timed.entry {
            TranscriptEntry::UserPrompt(text) => payloads.push(user_prompt_payload(&text)),
            TranscriptEntry::Harness(event) => {
                let started = subagent_started(&event).map(str::to_string);
                if let Some(payload) = harness_payload(&event) {
                    payloads.push(payload);
                }
                if let (Some(parent), Some(at_ms)) = (started, at_ms) {
                    payloads.push(subagent_started_at(&parent, at_ms));
                }
            }
        }
    }
    payloads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_payloads_wrap_events_and_stamp_subagent_starts() {
        let entries = vec![
            TimedEntry {
                at: Some("2026-10-19T12:00:00Z".into()),
                entry: TranscriptEntry::UserPrompt("hi".into()),
            },
            TimedEntry {
                at: Some("2026-10-19T12:00:01Z".into()),
                entry: TranscriptEntry::Harness(HarnessEvent::SubagentEvent {
                    parent_tool_use_id: "toolu_a".into(),
                    event: Box::new(HarnessEvent::TurnStarted {
                        session_id: None,
                        model: None,
                    }),
                }),
            },
            TimedEntry {
                at: None,
                entry: TranscriptEntry::Harness(HarnessEvent::TextDelta("ok".into())),
            },
        ];
        let payloads = history_payloads(entries);
        assert_eq!(payloads.len(), 4);
        assert_eq!(payloads[0], user_prompt_payload("hi"));
        assert_eq!(payloads[1]["kind"], "harness");
        assert_eq!(payloads[1]["event"]["type"], "subagent_event");
        assert_eq!(payloads[2]["kind"], "subagent_started_at");
        assert_eq!(payloads[2]["id"], "toolu_a");
        assert_eq!(payloads[2]["at_ms"], 1_792_411_201_000_i64);
        assert_eq!(
            payloads[3],
            serde_json::json!({"kind": "harness", "event": {"type": "text_delta", "data": "ok"}})
        );
    }

    #[test]
    fn page_inlines_vendored_renderers() {
        let html = agent_chat_html();
        assert!(!html.contains(AGENT_CHAT_VENDOR_SLOT));
        assert!(html.contains("window.__appendEvent = function (event)"));
    }
}
