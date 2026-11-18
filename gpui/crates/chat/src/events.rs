//! Wire-compatible display subset of src/harness/mod.rs at ab542113.
//! The envelope's Display variants are fixture metadata, never harness events.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::Path};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemKind {
    ToolCall { name: String, input: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeRequestKind {
    Permission {
        tool_name: String,
        input: Value,
        suggestions: Vec<Value>,
        decision_reason: Option<String>,
    },
    Question {
        questions: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "message", rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Queued,
    Started,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HarnessEvent {
    Ready(Value),
    ConversationReset {
        trigger: Option<String>,
    },
    TurnStarted {
        session_id: Option<String>,
        model: Option<String>,
    },
    TextDelta(String),
    ThinkingDelta(String),
    ItemStarted {
        id: String,
        kind: ItemKind,
    },
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
    RuntimeRequestResolved {
        request_id: String,
    },
    TurnCompleted {
        status: TurnStatus,
        usage: Value,
        cost_usd: Option<f64>,
    },
    SubagentEvent {
        parent_tool_use_id: String,
        event: Box<HarnessEvent>,
    },
    MessageLifecycle {
        id: String,
        state: MessageState,
    },
    MessageWithdrawRefused {
        id: String,
        reason: String,
    },
    PermissionModeChanged(String),
    ModelChanged(String),
    EffortChanged {
        effort: Option<String>,
        applied: Option<String>,
    },
    HistoryReplayed {
        entries: Vec<Entry>,
    },
    Error(String),
    ProcessExited {
        code: Option<i32>,
    },
    RemoteDisconnected {
        machine: String,
        error: String,
    },
    RemoteReconnected {
        machine: String,
        history_gap: bool,
    },
    UserMessageEcho {
        id: Option<String>,
        text: String,
        #[serde(default)]
        image_count: usize,
        delivery: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Entry {
    UserPrompt(String),
    Harness(HarnessEvent),
    /// Harness resolution does not tell the UI what the human chose.
    DisplayDecision {
        request_id: String,
        state: DecisionState,
        detail: String,
    },
    /// Delegations are app-owned, not harness-owned.
    DisplayDelegation {
        id: String,
        title: String,
        state: String,
        body: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Answered,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub at_ms: Option<u64>,
    pub entry: Entry,
}

#[derive(Debug)]
pub struct FixtureError(pub String);
impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for FixtureError {}

/// Accept normalized events, TranscriptEntry lines, or timestamped fixture envelopes.
/// Unknown/malformed events fail with a line number; no silently missing history.
pub fn parse_fixture(source: &str) -> Result<Vec<Record>, FixtureError> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let parse = || -> Result<Record, serde_json::Error> {
                let value: Value = serde_json::from_str(line)?;
                if value.get("entry").is_some() {
                    return serde_json::from_value(value);
                }
                let kind = value["type"].as_str().unwrap_or("");
                let entry = if matches!(
                    kind,
                    "user_prompt" | "harness" | "display_decision" | "display_delegation"
                ) {
                    serde_json::from_value(value)?
                } else {
                    Entry::Harness(serde_json::from_value(value)?)
                };
                Ok(Record { at_ms: None, entry })
            };
            parse().map_err(|err| FixtureError(format!("fixture line {}: {err}", index + 1)))
        })
        .collect()
}

pub fn load_fixture(path: &Path) -> Result<Vec<Record>, FixtureError> {
    let source = std::fs::read_to_string(path)
        .map_err(|err| FixtureError(format!("read {}: {err}", path.display())))?;
    parse_fixture(&source).map_err(|err| FixtureError(format!("{}: {err}", path.display())))
}
