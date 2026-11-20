//! Compatibility facade for Claude transcript history services.

pub use gitterm_core::chat::history::{
    is_agent_tool, load_claude_history, parse_claude_transcript, parse_claude_transcript_timed,
    TimedEntry, TranscriptEntry, MAX_RESULT_CHARS,
};

#[cfg(test)]
use super::HarnessEvent;
#[cfg(test)]
use serde_json::{json, Value};

#[cfg(test)]
#[allow(clippy::duplicate_mod)] // Shared characterization sources are loaded by multiple focused test modules.
#[path = "../../tests/native-chat/native_chat_history.rs"]
mod native_chat_tests;
