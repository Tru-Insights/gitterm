//! Frontend-independent delegation inputs. The MCP facade maps its transport DTOs here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a `task_update_handoff` says about the work (TRU-142 S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    /// Still working; the delegating chat's card shows the summary line.
    #[default]
    Progress,
    /// The objective is met: the delegation completes with this handoff.
    Done,
    /// Cannot go on without a human: blockers say why.
    Blocked,
}

impl HandoffStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

/// What `review_request` reviews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTargetKind {
    /// Staged, unstaged and untracked changes against HEAD.
    Uncommitted,
    /// The merge base with `base_ref` against the working tree, so it
    /// includes uncommitted changes (Codex `--base`).
    Base,
    /// One commit, named by `commit`.
    Commit,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ReviewDelegationRequest {
    /// `uncommitted`, `base` (needs `base_ref`) or `commit` (needs `commit`).
    pub target: ReviewTargetKind,
    /// Branch or ref to diff against, for target `base`.
    pub base_ref: Option<String>,
    /// Commit SHA (7 to 40 hex characters), for target `commit`.
    pub commit: Option<String>,
    /// Optional extra instructions for the reviewer.
    pub focus: Option<String>,
    /// Only `codex` is supported; omit it for Codex. For a Claude review,
    /// spawn a review subagent with your own Agent tool instead.
    pub reviewer: Option<String>,
    /// Codex model (`codex -m`). Omit it for GitTerm's configured Codex model.
    pub model: Option<String>,
    /// True only when the user explicitly chose this model. Overrides plan
    /// holds with a cost note; routine agent model selection is not an override.
    /// Requires `model`.
    #[serde(default)]
    pub user_model_choice: bool,
    /// Review this checkout instead of your tab's: an absolute path to a git
    /// checkout or worktree of one of your workspace's repos (or the
    /// workspace home).
    #[serde(default)]
    pub checkout: Option<PathBuf>,
    /// Codex reasoning effort: low, medium, high or xhigh. Omit it for
    /// Codex's configured effort.
    #[serde(default)]
    pub effort: Option<crate::workers::ReasoningEffort>,
    /// GitTerm stops the review after this many minutes and reports it as
    /// stopped at the time limit. Omit it for no limit.
    #[serde(default)]
    pub time_limit_minutes: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ConsultDelegationRequest {
    /// The full question or brief for the consultant: context, what to
    /// decide, and the answer shape you want. It runs read-only in your
    /// checkout and cannot see this conversation.
    pub brief: String,
    /// Codex model (`codex -m`). Omit it for GitTerm's configured Codex model.
    pub model: Option<String>,
    /// True only when the user explicitly chose this model. Overrides plan
    /// holds with a cost note; routine agent model selection is not an override.
    /// Requires `model`.
    #[serde(default)]
    pub user_model_choice: bool,
    /// Run in this checkout instead of your tab's (still read-only): an
    /// absolute path to a git checkout or worktree of one of your
    /// workspace's repos (or the workspace home).
    #[serde(default)]
    pub checkout: Option<PathBuf>,
    /// Codex reasoning effort: low, medium, high or xhigh. Omit it for
    /// Codex's configured effort.
    #[serde(default)]
    pub effort: Option<crate::workers::ReasoningEffort>,
    /// GitTerm stops the consult after this many minutes and reports it as
    /// stopped at the time limit. Omit it for no limit.
    #[serde(default)]
    pub time_limit_minutes: Option<u32>,
}

/// The target and run options used by the worker service, before path validation.
#[derive(Debug, Clone, Default)]
pub struct WorkerTargetRequest {
    pub checkout: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    pub base_reference: Option<String>,
    pub add_dirs: Vec<PathBuf>,
    pub model: Option<String>,
    pub user_model_choice: bool,
    pub effort: Option<crate::workers::ReasoningEffort>,
    pub time_limit_minutes: Option<u32>,
}
