//! UI intent seam. The app adapter decides how to deliver these to a session.
#[cfg(target_os = "macos")]
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ChatIntent {
    Send {
        text: String,
    },
    Stop,
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
    Deny,
}

/// Kept structured so the adapter can encode the question-text → joined-label map.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionAnswer {
    pub question: String,
    pub selected: Vec<String>,
    pub free_text: Option<String>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Question {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}
