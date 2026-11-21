//! The host supplies Iced's V5 pickup offer; this view never resumes a harness.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedConversation {
    pub id: String,
    pub title: String,
    pub origin: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedOffer {
    pub conversations: Vec<ImportedConversation>,
    #[serde(default)]
    pub pending: bool,
}
