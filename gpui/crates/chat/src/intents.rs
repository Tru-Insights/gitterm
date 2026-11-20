//! UI intent seam. The app adapter decides how to deliver these to a session.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ChatIntent {
    Send {
        text: String,
        images: Vec<gitterm_core::chat::types::ImageAttachment>,
    },
    Stop,
    Withdraw {
        message_id: String,
    },
    /// Rejection-only defensive seam. C11a never emits setting changes.
    /// Hosts must reject this even if a caller forges it outside the composer.
    ChangeMode {
        mode: String,
    },
    /// Bind an answer to the exact request the UI displayed.
    Answer {
        identity: gitterm_core::chat::state::RequestIdentity,
        answer: Box<ChatIntent>,
    },
    Permission {
        request_id: String,
        decision: PermissionDecision,
    },
    AnswerQuestion {
        request_id: String,
        answers: Vec<QuestionAnswer>,
    },
    DismissQuestion {
        request_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow,
    /// The exact offered localSettings rule or ACP option, never an invented rule.
    AllowAlways {
        suggestion: Value,
    },
    Deny {
        reason: String,
    },
}

/// Kept structured so the adapter can encode the question-text → joined-label map.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionAnswer {
    pub question: String,
    pub selected: Vec<String>,
    pub free_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Question {
    pub question: String,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub header: String,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct QuestionOption {
    pub label: String,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub description: String,
}

impl ChatIntent {
    pub(crate) fn request_id(&self) -> Option<&str> {
        match self {
            Self::Answer { answer, .. } => answer.request_id(),
            Self::Permission { request_id, .. }
            | Self::AnswerQuestion { request_id, .. }
            | Self::DismissQuestion { request_id } => Some(request_id),
            _ => None,
        }
    }
}
