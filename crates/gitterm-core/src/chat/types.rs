//! Neutral chat, backend and harness wire types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::transcript;

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

/// Where a user message is in the harness's command queue. Claude reports
/// this as `command_lifecycle {command_uuid, state}` for every user message
/// sent with a `uuid`: `queued` on receipt, `started` when a turn takes it
/// (the next turn, or the running one at its next tool boundary),
/// `completed` when that turn ends, or `cancelled` once withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Queued,
    Started,
    Completed,
    Cancelled,
}

/// A Claude `rate_limit_event` as it arrived (TRU-211). `info` is the
/// frame's `rate_limit_info`, passed through: the desktop reads it with
/// `plan_limits::ClaudeLimits`, so an agent and a desktop of different
/// versions agree on the wire whatever fields the CLI adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitReading {
    /// Unix seconds when the frame reached GitTerm (the agent machine's
    /// clock for a remote chat).
    pub observed_at: i64,
    pub info: Value,
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
        /// The session modes the harness offers (ACP agents), in the chat
        /// page's `{value, displayName, description}` shape. Empty for
        /// Claude, whose permission modes the page knows.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modes: Vec<Value>,
        /// The slash commands and skills the session accepts, passed
        /// through as `{name, description, argumentHint}`, for the
        /// composer's `/` menu. Empty for ACP agents.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        commands: Vec<Value>,
    },
    /// The harness cleared the conversation (Claude's `/clear`); the next
    /// `TurnStarted` carries the new session's id.
    ConversationReset {
        trigger: Option<String>,
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
        /// The turn's last `rate_limit_event` (Claude only, TRU-211): the
        /// plan-limit numbers `/usage` shows. Carried here rather than as an
        /// event of its own so a remote chat can relay it to an older
        /// desktop, which ignores the unknown field; an unknown event type
        /// would reach it as an undecodable event and an error in the chat.
        /// Absent when the turn reported none, and from older peers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<RateLimitReading>,
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
    /// A user message GitTerm sent with an id moved through the harness's
    /// queue. The chat clears a message's "queued" marker on `Started` and
    /// drops the message on `Cancelled`.
    MessageLifecycle {
        id: String,
        state: MessageState,
    },
    /// A withdraw (`WithdrawQueuedMessage`) did not take: the message was
    /// already picked up, or the harness refused. It stays sent.
    MessageWithdrawRefused {
        id: String,
        reason: String,
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
    /// A resumed ACP session's earlier timeline, as the agent replayed it
    /// through `session/load` (TRU-153). Emitted once, before `Ready`; the
    /// tab takes it as its history read-back rather than showing it live.
    HistoryReplayed {
        entries: Vec<transcript::TranscriptEntry>,
    },
    Error(String),
    ProcessExited {
        code: Option<i32>,
    },
    /// A remote chat lost its connection to the machine it runs on and is
    /// reconnecting (TRU-152). The chat keeps running there. Raised by the
    /// desktop's remote handle, never by a harness.
    RemoteDisconnected {
        machine: String,
        error: String,
    },
    /// The remote chat is attached again. `history_gap`: events were missed
    /// beyond what the agent buffers, so the timeline is reloaded from the
    /// transcript.
    RemoteReconnected {
        machine: String,
        history_gap: bool,
    },
    /// A user message a client of an agent-owned chat sent (TRU-151),
    /// recorded by the agent in the chat's event stream so every other
    /// client (the desktop, a phone) shows it too. `id` is the message's
    /// `UserPrompt::id`: a client that already drew the message under that
    /// id skips it. Images are never relayed, only how many there were.
    /// Raised by the agent, never by a harness or a local chat.
    UserMessageEcho {
        id: Option<String>,
        text: String,
        #[serde(default)]
        image_count: usize,
        /// Set when GitTerm, not the user, sent the message, so every
        /// client draws it as a card (`delegations::delivery_card`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivery: Option<DeliveryCard>,
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
/// into it. `id`, when set, is sent as the message's `uuid`; Claude then
/// reports the message's queue state (`MessageLifecycle`) under it and it
/// can be withdrawn while still queued.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UserPrompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
    pub id: Option<String>,
}

impl From<String> for UserPrompt {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            id: None,
        }
    }
}

impl From<&str> for UserPrompt {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

/// Commands the UI sends into a harness session. Serializable so a remote
/// chat can carry them to the agent machine (TRU-152).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HarnessCommand {
    /// Claude accepts a message at any time: one sent mid-turn is queued
    /// by the CLI and reaches the running turn at its next tool boundary,
    /// or starts the next turn when none comes.
    SendUserMessage(UserPrompt),
    /// Drop a message (by its `UserPrompt::id`) the harness has queued but
    /// not yet picked up. Does not interrupt the running turn.
    WithdrawQueuedMessage(String),
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
    /// Replace the workspace note used by Claude's next SessionStart hook
    /// when a running chat moves to another workspace.
    SetWorkspaceContext(Option<String>),
    Shutdown,
}

/// How the chat view shows a GitTerm-sent prompt: its first line as the
/// header, the delegation id when it reports exactly one, and the rest
/// folded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeliveryCard {
    pub header: String,
    pub delegation_id: String,
    pub body: String,
}

/// Saved Gemini chats retain their identity, but never start another process.
pub const GEMINI_REPLACED_NOTE: &str = "Gemini was replaced by Antigravity; this chat can't be continued. Saved Gemini tabs contain no transcript, so earlier messages are unavailable after restart.";
/// Which ACP agent a chat runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpAgent {
    Antigravity,
    OpenCode,
    /// Legacy saved state only; excluded from new-chat choices.
    Gemini,
}

impl AcpAgent {
    pub const ALL: [AcpAgent; 2] = [AcpAgent::Antigravity, AcpAgent::OpenCode];

    pub fn label(self) -> &'static str {
        match self {
            Self::Antigravity => "Antigravity",
            Self::Gemini => "Gemini",
            Self::OpenCode => "OpenCode",
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Self::Antigravity => "agy",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
        }
    }

    /// The session mode a new chat starts in: Antigravity's `default` prompts
    /// for approval; OpenCode's `build` agent asks before edits and shell
    /// under GitTerm's permission config.
    pub fn default_mode(self) -> &'static str {
        match self {
            Self::Antigravity | Self::Gemini => "default",
            Self::OpenCode => "build",
        }
    }

    /// The read-only planning mode both agents report as `plan`.
    pub fn plan_mode(self) -> &'static str {
        "plan"
    }

    /// The MCP server name this agent sees. Antigravity accepts these names
    /// (verified with 1.5.11); retain the hyphens used by Gemini's policy engine, which
    /// splits `mcp_<server>_<tool>` at the first underscore after `mcp_`,
    /// so its server names use hyphens.
    pub fn mcp_server_name(self, name: &str) -> String {
        match self {
            Self::Antigravity | Self::Gemini => name.replace('_', "-"),
            Self::OpenCode => name.to_string(),
        }
    }
}

/// Which agent backend this tab is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentBackend {
    Pi,
    Claude,
    /// An Agent Client Protocol agent (Antigravity, OpenCode; TRU-153).
    Acp(AcpAgent),
}

impl AgentBackend {
    /// A native harness chat (one long-lived process per tab driven through
    /// `HarnessSession`): Claude or an ACP agent. Pi runs per turn.
    pub fn is_native(self) -> bool {
        matches!(self, Self::Claude | Self::Acp(_))
    }
}

/// Per-backend configuration. Tagged by the `backend` discriminator so this
/// round-trips cleanly through `workspaces.json` next to the existing fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum AgentBackendConfig {
    Pi {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<bool>,
    },
    Claude {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// An ACP agent's chat (TRU-153). `model` empty keeps the agent's own
    /// choice; `mode` is the agent's session mode (Antigravity `default`,
    /// `accept-edits`, `auto`, `plan`; OpenCode `build`, `plan`).
    Acp {
        agent: AcpAgent,
        #[serde(default)]
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
}

impl AgentBackendConfig {
    pub fn backend(&self) -> AgentBackend {
        match self {
            Self::Pi { .. } => AgentBackend::Pi,
            Self::Claude { .. } => AgentBackend::Claude,
            Self::Acp { agent, .. } => AgentBackend::Acp(*agent),
        }
    }

    pub fn read_only_note(&self) -> Option<&'static str> {
        matches!(
            self,
            Self::Acp {
                agent: AcpAgent::Gemini,
                ..
            }
        )
        .then_some(GEMINI_REPLACED_NOTE)
    }

    /// The chat's model as configured (Claude and ACP tabs).
    pub fn chat_model(&self) -> Option<&str> {
        match self {
            Self::Claude { model, .. } | Self::Acp { model, .. } => Some(model),
            Self::Pi { .. } => None,
        }
    }

    /// The chat's effort as configured (Claude and ACP tabs).
    pub fn chat_effort(&self) -> Option<&str> {
        match self {
            Self::Claude { effort, .. } | Self::Acp { effort, .. } => effort.as_deref(),
            Self::Pi { .. } => None,
        }
    }
}

/// High-level lifecycle of the agent subprocess as observed by the UI.
///
/// State transitions (Step 3 will own these):
/// - `Idle` → `Streaming` on first `SystemInit` after a prompt is submitted
/// - `Streaming` → `Idle` on `Result` event (turn complete)
/// - any → `Stopped` on user stop request
/// - any → `Errored` on subprocess crash or fatal parse failure
#[derive(Debug, Clone, Default)]
pub enum AgentSessionState {
    #[default]
    Idle,
    Streaming,
    Stopped,
    Errored(String),
}

/// Whether a resumed session's earlier timeline has been read back from
/// its transcript (Claude backend; TRU-140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryLoad {
    NotLoaded,
    Loading,
    Loaded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// TurnCompleted as it was before TRU-211, standing in for an older
    /// desktop or agent on the other end of a remote chat.
    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    #[serde(tag = "type", content = "data", rename_all = "snake_case")]
    enum OldPeerEvent {
        TurnCompleted {
            status: TurnStatus,
            usage: Value,
            cost_usd: Option<f64>,
        },
    }

    #[test]
    fn rate_limit_on_turn_completed_is_compatible_both_ways() {
        let info =
            json!({"status": "allowed", "unifiedWindows": {"five_hour": {"utilization": 0.5}}});
        let new = HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: json!({"input_tokens": 1}),
            cost_usd: Some(0.25),
            rate_limit: Some(RateLimitReading {
                observed_at: 1_800_000_000,
                info: info.clone(),
            }),
        };
        let wire = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<HarnessEvent>(&wire).unwrap(), new);
        // An older peer ignores the field it does not know.
        assert_eq!(
            serde_json::from_str::<OldPeerEvent>(&wire).unwrap(),
            OldPeerEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: json!({"input_tokens": 1}),
                cost_usd: Some(0.25),
            }
        );
        // An older peer's event has none.
        let old = serde_json::to_string(&OldPeerEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            usage: Value::Null,
            cost_usd: None,
        })
        .unwrap();
        assert_eq!(
            serde_json::from_str::<HarnessEvent>(&old).unwrap(),
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                usage: Value::Null,
                cost_usd: None,
                rate_limit: None,
            }
        );
        // No reading: the wire shape is exactly the old one.
        let none = HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            usage: Value::Null,
            cost_usd: None,
            rate_limit: None,
        };
        assert_eq!(serde_json::to_string(&none).unwrap(), old);
    }

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
            rate_limit: None,
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type": "turn_completed", "data": {
                "status": {"state": "failed", "message": "boom"},
                "usage": {}, "cost_usd": 0.5}})
        );
    }

    #[test]
    fn message_lifecycle_serializes_for_the_page() {
        assert_eq!(
            serde_json::to_value(HarnessEvent::MessageLifecycle {
                id: "u1".into(),
                state: MessageState::Started
            })
            .unwrap(),
            json!({"type": "message_lifecycle", "data": {"id": "u1", "state": "started"}})
        );
        assert_eq!(
            serde_json::to_value(HarnessEvent::MessageWithdrawRefused {
                id: "u1".into(),
                reason: "already picked up".into()
            })
            .unwrap(),
            json!({"type": "message_withdraw_refused",
                   "data": {"id": "u1", "reason": "already picked up"}})
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
    fn commands_round_trip_through_json_for_remote_chats() {
        let commands = vec![
            HarnessCommand::SendUserMessage(UserPrompt {
                text: "hi".into(),
                images: vec![ImageAttachment {
                    media_type: "image/png".into(),
                    data: "QQ==".into(),
                }],
                id: Some("u1".into()),
            }),
            HarnessCommand::WithdrawQueuedMessage("u1".into()),
            HarnessCommand::Answer {
                request_id: "r1".into(),
                decision: RuntimeDecision::Allow {
                    updated_input: Some(json!({"command": "ls"})),
                    remember: Some(json!({"type": "addRules"})),
                },
            },
            HarnessCommand::Answer {
                request_id: "r2".into(),
                decision: RuntimeDecision::AnswerQuestion {
                    answers: BTreeMap::from([("Red?".to_string(), "Blue".to_string())]),
                },
            },
            HarnessCommand::Interrupt,
            HarnessCommand::SetPermissionMode("plan".into()),
            HarnessCommand::SetModel("sonnet".into()),
            HarnessCommand::SetEffort(None),
            HarnessCommand::SetEffort(Some("high".into())),
            HarnessCommand::Shutdown,
        ];
        for command in commands {
            let json = serde_json::to_string(&command).unwrap();
            let back: HarnessCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(back, command, "{json}");
        }
        assert_eq!(
            serde_json::to_value(HarnessCommand::Interrupt).unwrap(),
            json!({"type": "interrupt"})
        );
        // The base64 payload rides the wire but never a Debug line.
        let prompt = HarnessCommand::SendUserMessage(UserPrompt {
            text: String::new(),
            images: vec![ImageAttachment {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
            id: None,
        });
        assert!(serde_json::to_string(&prompt).unwrap().contains("QUJD"));
        assert!(!format!("{prompt:?}").contains("QUJD"));
    }

    #[test]
    fn events_round_trip_through_json_for_remote_chats() {
        let events = vec![
            HarnessEvent::Ready {
                session_id: Some("s".into()),
                permission_mode: Some("default".into()),
                models: vec![json!({"value": "haiku"})],
                modes: Vec::new(),
                commands: vec![json!({"name": "checkpoint", "description": "Save"})],
            },
            HarnessEvent::Ready {
                session_id: Some("g".into()),
                permission_mode: Some("plan".into()),
                models: Vec::new(),
                modes: vec![json!({"value": "plan", "displayName": "Plan"})],
                commands: Vec::new(),
            },
            HarnessEvent::HistoryReplayed {
                entries: vec![
                    transcript::TranscriptEntry::UserPrompt("hi".into()),
                    transcript::TranscriptEntry::Harness(HarnessEvent::TextDelta("yo".into())),
                ],
            },
            HarnessEvent::TurnStarted {
                session_id: None,
                model: Some("haiku".into()),
            },
            HarnessEvent::TextDelta("hi".into()),
            HarnessEvent::ThinkingDelta(String::new()),
            HarnessEvent::ItemStarted {
                id: "t1".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                },
            },
            HarnessEvent::ItemInputDelta {
                id: "t1".into(),
                partial_json: "{\"a".into(),
            },
            HarnessEvent::ItemCompleted {
                id: "t1".into(),
                output: "ok".into(),
                is_error: false,
            },
            HarnessEvent::RuntimeRequest {
                request_id: "r1".into(),
                tool_use_id: "t1".into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: json!({}),
                    suggestions: vec![json!({"type": "setMode"})],
                    decision_reason: None,
                },
            },
            HarnessEvent::RuntimeRequest {
                request_id: "r2".into(),
                tool_use_id: "t2".into(),
                kind: RuntimeRequestKind::Question {
                    questions: json!([{"question": "Red?"}]),
                },
            },
            HarnessEvent::RuntimeRequestResolved {
                request_id: "r1".into(),
            },
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: json!({"input_tokens": 4}),
                cost_usd: Some(0.00378657),
                rate_limit: None,
            },
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                usage: Value::Null,
                cost_usd: None,
                rate_limit: None,
            },
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed("boom".into()),
                usage: json!({}),
                cost_usd: None,
                rate_limit: None,
            },
            HarnessEvent::SubagentEvent {
                parent_tool_use_id: "a1".into(),
                event: Box::new(HarnessEvent::TextDelta("sub".into())),
            },
            HarnessEvent::MessageLifecycle {
                id: "u1".into(),
                state: MessageState::Cancelled,
            },
            HarnessEvent::MessageWithdrawRefused {
                id: "u1".into(),
                reason: "late".into(),
            },
            HarnessEvent::PermissionModeChanged("plan".into()),
            HarnessEvent::ModelChanged("sonnet".into()),
            HarnessEvent::EffortChanged {
                effort: Some("low".into()),
                applied: None,
            },
            HarnessEvent::Error("bad".into()),
            HarnessEvent::ProcessExited { code: Some(0) },
            HarnessEvent::ProcessExited { code: None },
            HarnessEvent::RemoteDisconnected {
                machine: "mini".into(),
                error: "connection reset".into(),
            },
            HarnessEvent::RemoteReconnected {
                machine: "mini".into(),
                history_gap: true,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let back: HarnessEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, event, "{json}");
        }
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
