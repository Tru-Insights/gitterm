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

pub mod acp;
pub mod claude;
pub mod transcript;

pub use gitterm_core::chat::types::{
    validate_images, HarnessCommand, HarnessEvent, ImageAttachment, ItemKind, MessageState,
    PermissionUpdate, RateLimitReading, RuntimeDecision, RuntimeRequestKind, TurnStatus,
    UserPrompt, IMAGE_MEDIA_TYPES, MAX_IMAGES_PER_MESSAGE, MAX_IMAGE_BYTES,
};

/// A live native harness session: the handle a chat tab sends commands to.
pub enum HarnessSession {
    Claude(claude::ClaudeSession),
    Acp(acp::AcpSession),
}

impl HarnessSession {
    pub fn close_turn_snapshots(&self) {
        match self {
            Self::Claude(session) => session.close_turn_snapshots(),
            Self::Acp(session) => session.close_turn_snapshots(),
        }
    }

    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        match self {
            Self::Claude(session) => session.send(command),
            Self::Acp(session) => session.send(command),
        }
    }

    /// Dispatch a reducer command; legacy/remote ports have no write receipts.
    pub fn send_chat(
        &self,
        command: HarnessCommand,
        correlation: gitterm_core::chat::types::CommandCorrelation,
    ) -> Result<bool, String> {
        match self {
            Self::Claude(session) => session.send_chat(command, correlation),
            Self::Acp(session) => session.send(command).map(|()| false),
        }
    }

    /// The running local `claude` process of a Claude chat (TRU-212).
    pub fn claude_pid(&self) -> Option<u32> {
        match self {
            Self::Claude(session) => session.local_pid(),
            Self::Acp(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_echo_serializes_for_the_page() {
        let echo = HarnessEvent::UserMessageEcho {
            id: Some("u1".into()),
            text: "hi".into(),
            image_count: 2,
            delivery: None,
        };
        let value = serde_json::to_value(&echo).unwrap();
        assert_eq!(
            value,
            json!({"type": "user_message_echo",
                   "data": {"id": "u1", "text": "hi", "image_count": 2}})
        );
        assert_eq!(serde_json::from_value::<HarnessEvent>(value).unwrap(), echo);

        // A GitTerm-sent message carries its card, so a phone draws it as one.
        let text =
            crate::delegations::from_gitterm("GitTerm PR watch: acme/api#12: checks failed.");
        let echo = HarnessEvent::UserMessageEcho {
            id: None,
            delivery: crate::delegations::delivery_card(&text),
            text,
            image_count: 0,
        };
        let value = serde_json::to_value(&echo).unwrap();
        assert_eq!(
            value["data"]["delivery"]["header"],
            "GitTerm PR watch: acme/api#12: checks failed."
        );
        assert_eq!(serde_json::from_value::<HarnessEvent>(value).unwrap(), echo);
    }
}
