//! Harness adapters: a normalized event/command model for agent harnesses
//! that GitTerm drives natively (no terminal in between).
//!
//! The model is a minimal version of T3 Code's thread / turn / item /
//! runtime-request split:
//!
//! - a *session* is one long-lived harness process (`Ready` once it answers
//!   the handshake, `ProcessExited` when it goes away);
//! - a *turn* is one user message and everything the harness does about it
//!   (`TurnStarted` .. `TurnCompleted`);
//! - an *item* is a unit of work inside a turn that has its own lifecycle,
//!   today only tool calls (`ItemStarted` / `ItemInputDelta` /
//!   `ItemCompleted`);
//! - a *runtime request* is the harness blocking on the human: a permission
//!   prompt or a question (`RuntimeRequest` .. `RuntimeRequestResolved`).
//!
//! Payloads whose shape belongs to the harness and that GitTerm only passes
//! through (tool input, permission suggestions, questions, usage) stay
//! `serde_json::Value`. This module has no iced dependency; the UI layer
//! maps these events onto tabs and the webview.

pub mod claude;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A `PermissionUpdate` as the Claude control protocol defines it
/// (`addRules`, `setMode`, `addDirectories`, ... with a `destination`).
/// GitTerm only ever echoes one of the CLI's own suggestions back, so the
/// shape stays passthrough.
pub type PermissionUpdate = Value;

/// What a running item is. Only tool calls are modelled today; assistant
/// text and thinking stream as turn-level deltas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemKind {
    ToolCall { name: String, input: Value },
}

/// Why the harness is blocked on the human.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeRequestKind {
    /// Approve or deny one tool call.
    Permission {
        tool_name: String,
        input: Value,
        /// Rule updates the harness offers for "always allow". Passthrough.
        suggestions: Vec<PermissionUpdate>,
        decision_reason: Option<String>,
    },
    /// The model asked the user structured questions (AskUserQuestion).
    /// `questions` is the tool's `questions` array, passed through.
    Question { questions: Value },
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "message", rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed(String),
}

/// One normalized event from a harness session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// The harness answered the handshake and accepts input. `session_id`
    /// is known here only when resuming; a fresh session learns its id on
    /// the first `TurnStarted`.
    Ready {
        session_id: Option<String>,
        permission_mode: Option<String>,
        /// The harness's model list, passed through.
        models: Vec<Value>,
    },
    /// The harness began working on a user message. Carries the session id
    /// the harness reports for this turn (Claude assigns it on the first
    /// message and repeats it every turn).
    TurnStarted {
        session_id: Option<String>,
        model: Option<String>,
    },
    TextDelta(String),
    /// Streamed thinking. An empty string marks the start of a thinking
    /// block whose text the harness does not display (redacted/summarized).
    ThinkingDelta(String),
    ItemStarted {
        id: String,
        kind: ItemKind,
    },
    /// A fragment of a tool call's JSON input. Concatenating every fragment
    /// for an id yields the full input document.
    ItemInputDelta {
        id: String,
        partial_json: String,
    },
    ItemCompleted {
        id: String,
        output: String,
        is_error: bool,
    },
    RuntimeRequest {
        request_id: String,
        tool_use_id: String,
        kind: RuntimeRequestKind,
    },
    /// The request was answered, withdrawn by the harness, or became moot
    /// because its turn ended.
    RuntimeRequestResolved {
        request_id: String,
    },
    TurnCompleted {
        status: TurnStatus,
        /// The harness's usage block for the turn, passed through.
        usage: Value,
        /// Claude reports this cumulatively per process.
        cost_usd: Option<f64>,
    },
    Error(String),
    ProcessExited {
        code: Option<i32>,
    },
}

/// The human's answer to a `RuntimeRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeDecision {
    /// Run the tool. `updated_input` replaces the tool input (the original
    /// input is sent when `None`); `remember` is one of the request's own
    /// suggestions, echoed back so the harness persists the rule.
    Allow {
        #[serde(default)]
        updated_input: Option<Value>,
        #[serde(default)]
        remember: Option<PermissionUpdate>,
    },
    Deny {
        message: String,
    },
    /// Answers for a `Question` request: question text -> chosen label(s),
    /// multi-select labels joined with ", ".
    AnswerQuestion {
        answers: BTreeMap<String, String>,
    },
}

/// Commands the UI sends into a harness session.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessCommand {
    SendUserMessage(String),
    Answer {
        request_id: String,
        decision: RuntimeDecision,
    },
    Interrupt,
    SetPermissionMode(String),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_serialize_with_type_and_data() {
        let ev = HarnessEvent::TextDelta("hi".into());
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type": "text_delta", "data": "hi"})
        );
        let ev = HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed("boom".into()),
            usage: json!({}),
            cost_usd: Some(0.5),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type": "turn_completed", "data": {
                "status": {"state": "failed", "message": "boom"},
                "usage": {}, "cost_usd": 0.5}})
        );
    }

    #[test]
    fn decisions_deserialize_from_webview_json() {
        let allow: RuntimeDecision =
            serde_json::from_value(json!({"kind": "allow", "remember": {"type": "addRules"}}))
                .unwrap();
        assert_eq!(
            allow,
            RuntimeDecision::Allow {
                updated_input: None,
                remember: Some(json!({"type": "addRules"}))
            }
        );
        let answer: RuntimeDecision = serde_json::from_value(
            json!({"kind": "answer_question", "answers": {"Red or blue?": "Blue"}}),
        )
        .unwrap();
        assert_eq!(
            answer,
            RuntimeDecision::AnswerQuestion {
                answers: BTreeMap::from([("Red or blue?".to_string(), "Blue".to_string())])
            }
        );
    }
}
