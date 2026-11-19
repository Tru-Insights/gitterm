//! Neutral transcript entries shared by harnesses and chat consumers.

use serde::{Deserialize, Serialize};

use crate::chat::types::HarnessEvent;

/// One timeline entry recovered from a transcript. Serializable so an
/// agent can return a remote session's history (TRU-152).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// Something the human typed (or a compaction summary standing in for
    /// the earlier conversation).
    UserPrompt(String),
    Harness(HarnessEvent),
}

/// A timeline entry and when its transcript line was written (the line's
/// RFC 3339 `timestamp`, when it has one). The time places delegation
/// cards, which are not in the transcript, back where they were (TRU-142).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedEntry {
    pub at: Option<String>,
    pub entry: TranscriptEntry,
}
