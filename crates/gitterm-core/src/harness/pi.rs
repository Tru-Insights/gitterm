//! Pi stream DTOs and raw JSON parsing, independent of subprocess ownership.

/// One canonical event type, owned by the shared chat reducer.
pub use crate::chat::state::AgentEvent;

/// The Agent call whose subagent `ev` starts, if it is a subagent start.
pub fn subagent_started(ev: &AgentEvent) -> Option<&str> {
    match ev {
        AgentEvent::Harness(event) => crate::chat::history::subagent_started(event),
        _ => None,
    }
}

/// The timeline marker for when Agent call `parent`'s subagent started
/// (Unix ms). The chat page uses it when the start was not seen live.
pub fn subagent_started_at(parent: &str, at_ms: i64) -> serde_json::Value {
    crate::chat::history::subagent_started_at(parent, at_ms)
}

/// Parse one stdout line, retaining non-JSON text and dropping the existing
/// high-volume events before they reach the conversation buffer.
pub fn parse_line(line: String) -> Option<serde_json::Value> {
    let value =
        serde_json::from_str::<serde_json::Value>(&line).unwrap_or(serde_json::Value::String(line));
    // Drop high-volume / low-value events at the source. The
    // tool-call info we want is in `turn_end.message.content`,
    // which already renders. Filtering here keeps Iced's
    // event channel from saturating (caused a panic in Step 3
    // testing) and keeps the conversation buffer compact.
    let drop = value
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| {
            matches!(
                t,
                "message_update"
                    | "message_start"
                    | "message_end"
                    | "tool_execution_start"
                    | "tool_execution_end"
            )
        })
        .unwrap_or(false);
    if drop {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_events_and_non_json_lines_keep_their_existing_shapes() {
        for value in [
            json!({"type":"turn_end","message":{"content":[]}}),
            json!({"type":"future_event","extra":17}),
            json!([1, 2]),
            json!(null),
        ] {
            assert_eq!(parse_line(value.to_string()), Some(value));
        }
        assert_eq!(
            parse_line("plain output".into()),
            Some(json!("plain output"))
        );
        assert_eq!(parse_line("".into()), Some(json!("")));
        for kind in [
            "message_update",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
        ] {
            assert_eq!(parse_line(json!({"type":kind}).to_string()), None);
        }
    }
}
