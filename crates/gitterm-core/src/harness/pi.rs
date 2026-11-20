//! Pi stream DTOs and raw JSON parsing, independent of subprocess ownership.

use super::transcript::TranscriptEntry;
use crate::chat::types::{HarnessEvent, UserPrompt};

/// Parsed stream-json events from the agent subprocess. The variant taxonomy is
/// the union of pi and Claude Code event shapes; the parser (Step 3) decides
/// which variant each line maps to. Backend-specific details that don't fit the
/// shared shape are kept on the raw JSON via `Other`.
///
/// pi uses `toolcall_start/delta/end` (not `tool_use_*`), `_end` not `_stop`
/// suffixes, and tool results arrive as `role: "toolResult"`. Claude uses
/// content-block-style events embedded in `assistant` messages. The parser
/// normalizes both into these variants.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    SystemInit(serde_json::Value),
    AssistantText(String),
    AssistantThinking(String),
    ToolCallStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCallDelta {
        id: String,
        partial: String,
    },
    ToolCallEnd {
        id: String,
    },
    ToolResult {
        tool_use_id: String,
        output: String,
        is_error: bool,
    },
    Result(serde_json::Value),
    /// Backend-specific event that doesn't fit the normalized variants above.
    /// Kept as raw JSON so the UI layer can decide whether to render it.
    Other(serde_json::Value),
    /// Normalized event from a native harness session (Claude, TRU-140).
    Harness(HarnessEvent),
}

impl AgentEvent {
    /// The echo of a prompt the human submitted, as the chat page renders it.
    pub fn user_prompt(text: &str) -> Self {
        Self::Other(crate::chat::history::user_prompt_payload(text))
    }

    /// The echo of a submitted prompt. Attached images ride along as
    /// `images: [{media_type, data}]` so the page can show thumbnails in
    /// the human's own message. A prompt with an id carries it as `id`, and
    /// one sent while a turn runs is marked `queued: true`: the page shows
    /// it as queued, with a withdraw button, until its `message_lifecycle`
    /// says Claude picked it up. A plain text prompt is the plain echo.
    pub fn submitted_prompt(prompt: &UserPrompt, queued: bool) -> Self {
        if prompt.images.is_empty() && prompt.id.is_none() && !queued {
            return Self::user_prompt(&prompt.text);
        }
        let mut echo = crate::chat::history::user_prompt_payload(&prompt.text);
        if !prompt.images.is_empty() {
            echo["images"] = serde_json::json!(prompt.images);
        }
        if let Some(id) = &prompt.id {
            echo["id"] = serde_json::json!(id);
        }
        if queued {
            echo["queued"] = serde_json::json!(true);
        }
        Self::Other(echo)
    }

    /// The echo of a prompt another client of an agent-owned chat sent (a
    /// phone, TRU-151), as the page's own `user_prompt`: `id` lets the
    /// page and the tab tell it from their own echoes, `queued` marks it
    /// sent while a turn runs, and `image_count` says how many images it
    /// carried (their bytes are never relayed).
    pub fn relayed_prompt(id: Option<&str>, text: &str, image_count: usize, queued: bool) -> Self {
        let mut echo = crate::chat::history::user_prompt_payload(text);
        if let Some(id) = id {
            echo["id"] = serde_json::json!(id);
            if queued {
                echo["queued"] = serde_json::json!(true);
            }
        }
        if image_count > 0 {
            echo["image_count"] = serde_json::json!(image_count);
        }
        Self::Other(echo)
    }

    /// The id of a `user_prompt` echo that carries one.
    pub fn prompt_id(&self) -> Option<&str> {
        match self {
            Self::Other(value)
                if value.get("type").and_then(|v| v.as_str()) == Some("user_prompt") =>
            {
                value.get("id").and_then(|v| v.as_str())
            }
            _ => None,
        }
    }

    /// The JSON the chat webview's `__appendEvent` receives. Harness events
    /// are wrapped so the page can tell them from pi's raw stream shapes.
    pub fn webview_payload(&self) -> Option<serde_json::Value> {
        match self {
            Self::Other(value) => Some(value.clone()),
            Self::Harness(ev) => crate::chat::history::harness_payload(ev),
            // The typed pi variants are not produced yet.
            _ => None,
        }
    }
}

impl From<TranscriptEntry> for AgentEvent {
    fn from(entry: TranscriptEntry) -> Self {
        match entry {
            TranscriptEntry::UserPrompt(text) => Self::user_prompt(&text),
            TranscriptEntry::Harness(ev) => Self::Harness(ev),
        }
    }
}

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
