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
pub mod transcript;

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
    /// Activity inside a subagent the turn spawned (Claude's Agent tool).
    /// `parent_tool_use_id` is the parent's Agent tool call; `event` is one
    /// of the subagent's own text, thinking or item events. Kept apart from
    /// the turn's events so the subagent's text never streams into the
    /// parent's reply.
    SubagentEvent {
        parent_tool_use_id: String,
        event: Box<HarnessEvent>,
    },
    /// The harness confirmed a permission mode change GitTerm requested.
    /// Carries the mode now in effect.
    PermissionModeChanged(String),
    /// The harness confirmed a model change GitTerm requested. Carries the
    /// model value as requested (an alias such as `sonnet`, or `default`
    /// for the user's own default).
    ModelChanged(String),
    /// The harness confirmed an effort change GitTerm requested. `effort`
    /// is the level the session is now pinned to (`None`: the model's own
    /// default); `applied` is the level the harness reports in effect,
    /// when it said.
    EffortChanged {
        effort: Option<String>,
        applied: Option<String>,
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

/// One image attached to a user message, already encoded the way the
/// Anthropic content block wants it: a `media_type` such as `image/png` or
/// `image/jpeg` and the bytes as standard base64 (no `data:` prefix).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub media_type: String,
    pub data: String,
}

/// Debug output names the type and size, never the base64 payload.
impl std::fmt::Debug for ImageAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageAttachment")
            .field("media_type", &self.media_type)
            .field("bytes", &self.decoded_len())
            .finish()
    }
}

/// Most images one message may carry (the composer enforces the same cap).
pub const MAX_IMAGES_PER_MESSAGE: usize = 4;
/// Largest decoded image the composer may send, after its downscale.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// The image types the Anthropic API accepts in a base64 image block.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

impl ImageAttachment {
    /// Size of the image once the base64 is decoded (padding excluded).
    pub fn decoded_len(&self) -> usize {
        let padding = self.data.bytes().rev().take_while(|&b| b == b'=').count();
        (self.data.len() / 4 * 3).saturating_sub(padding)
    }
}

/// Checks a message's attachments against the composer's caps: at most
/// `MAX_IMAGES_PER_MESSAGE` images, each an accepted media type, non-empty,
/// and at most `MAX_IMAGE_BYTES` decoded. The error names the first
/// offending image so the chat can say why nothing was sent.
pub fn validate_images(images: &[ImageAttachment]) -> Result<(), String> {
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(format!(
            "{} images attached; a message can carry at most {MAX_IMAGES_PER_MESSAGE}",
            images.len()
        ));
    }
    for (i, image) in images.iter().enumerate() {
        let n = i + 1;
        if !IMAGE_MEDIA_TYPES.contains(&image.media_type.as_str()) {
            return Err(format!(
                "image {n} is {:?}; only PNG, JPEG, GIF and WebP can be sent",
                image.media_type
            ));
        }
        if image.data.is_empty() {
            return Err(format!("image {n} is empty"));
        }
        let bytes = image.decoded_len();
        if bytes > MAX_IMAGE_BYTES {
            return Err(format!(
                "image {n} is {:.1} MB; the limit is {} MB",
                bytes as f64 / (1024.0 * 1024.0),
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
    }
    Ok(())
}

/// A user message: the composer's text plus any images pasted or dropped
/// into it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserPrompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl From<String> for UserPrompt {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for UserPrompt {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

/// Commands the UI sends into a harness session.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessCommand {
    SendUserMessage(UserPrompt),
    Answer {
        request_id: String,
        decision: RuntimeDecision,
    },
    Interrupt,
    SetPermissionMode(String),
    /// Switch the running session's model (`default` resets it to the
    /// user's own default).
    SetModel(String),
    /// Pin the running session's effort level, or `None` to return to the
    /// model's default.
    SetEffort(Option<String>),
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
    fn model_and_effort_changes_serialize_for_the_page() {
        assert_eq!(
            serde_json::to_value(HarnessEvent::ModelChanged("sonnet".into())).unwrap(),
            json!({"type": "model_changed", "data": "sonnet"})
        );
        assert_eq!(
            serde_json::to_value(HarnessEvent::EffortChanged {
                effort: None,
                applied: Some("medium".into())
            })
            .unwrap(),
            json!({"type": "effort_changed", "data": {"effort": null, "applied": "medium"}})
        );
    }

    fn image(media_type: &str, decoded_bytes: usize) -> ImageAttachment {
        // Base64 length for `decoded_bytes` whole 3-byte groups.
        ImageAttachment {
            media_type: media_type.into(),
            data: "A".repeat(decoded_bytes.div_ceil(3) * 4),
        }
    }

    #[test]
    fn image_caps_allow_four_images_up_to_five_megabytes() {
        let four = vec![image("image/jpeg", MAX_IMAGE_BYTES / 3 * 3); 4];
        assert_eq!(validate_images(&four), Ok(()));
        assert_eq!(validate_images(&[]), Ok(()));
    }

    #[test]
    fn image_caps_reject_a_fifth_image_an_oversize_one_and_odd_types() {
        let five = vec![image("image/png", 10); 5];
        assert!(validate_images(&five).unwrap_err().contains("at most 4"));
        let big = [
            image("image/png", 10),
            image("image/png", MAX_IMAGE_BYTES + 3),
        ];
        assert!(validate_images(&big)
            .unwrap_err()
            .starts_with("image 2 is 5.0 MB"));
        assert!(validate_images(&[image("image/tiff", 10)])
            .unwrap_err()
            .contains("only PNG, JPEG, GIF and WebP"));
        assert!(validate_images(&[image("image/png", 0)])
            .unwrap_err()
            .contains("empty"));
    }

    #[test]
    fn decoded_len_discounts_base64_padding() {
        let one = ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        };
        assert_eq!(one.decoded_len(), 1);
        assert!(format!("{one:?}").contains("bytes: 1"));
        assert!(!format!("{one:?}").contains("QQ=="));
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
