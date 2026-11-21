//! Canonical harness DTOs plus the unchanged fixture display envelope.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::Path};

pub use gitterm_core::chat::types::{
    HarnessEvent, ItemKind, MessageState, RuntimeRequestKind, TurnStatus,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Entry {
    UserPrompt(String),
    /// Local sent prompts retain image bytes when a pane is restored.
    SubmittedPrompt(gitterm_core::chat::types::UserPrompt),
    Harness(HarnessEvent),
    /// Replay clocks are presentation metadata, retained when a chat is saved.
    TurnTiming {
        start_ms: Option<u64>,
        end_ms: Option<u64>,
        split_ms: Option<u64>,
    },
    /// Harness resolution does not tell the UI what the human chose.
    DisplayDecision {
        request_id: String,
        state: DecisionState,
        detail: String,
    },
    /// Host-supplied V5 pickup choices; rendering does not resume them.
    ImportedOffer(crate::imported::ImportedOffer),
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
                    "user_prompt"
                        | "submitted_prompt"
                        | "harness"
                        | "turn_timing"
                        | "display_decision"
                        | "display_delegation"
                        | "imported_offer"
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
