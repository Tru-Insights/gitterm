//! Iced's Review / Consult popover. Collects requests; the host owns execution.
use gitterm_core::settings::{ReviewConfig, ReviewerKind};

pub const KINDS: [(ReviewKind, &str); 2] = [
    (ReviewKind::Review, "Review"),
    (ReviewKind::Consult, "Consult"),
];
pub const REVIEWERS: [&str; 2] = ["Claude subagent", "Codex"];
pub const CLAUDE_MODELS: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];
pub const TARGETS: [(Target, &str); 3] = [
    (Target::Branch, "Branch vs"),
    (Target::Uncommitted, "Uncommitted changes"),
    (Target::Commit, "Commit"),
];
pub const FOCUS_PLACEHOLDER: &str = "e.g. error handling in the webview lifecycle";
pub const BRIEF_PLACEHOLDER: &str = "The question, the context it needs, and the answer you want. The consultant cannot see this chat.";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    #[default]
    Review,
    Consult,
}
impl ReviewKind {
    pub fn submit_label(self) -> &'static str {
        match self {
            Self::Review => "Start review",
            Self::Consult => "Ask",
        }
    }
    pub fn note_label(self) -> &'static str {
        match self {
            Self::Review => "Focus (optional)",
            Self::Consult => "Brief",
        }
    }
    pub fn reviewer_label(self) -> &'static str {
        match self {
            Self::Review => "Reviewer",
            Self::Consult => "Consultant",
        }
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Branch,
    #[default]
    Uncommitted,
    Commit,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    Branch { base: String },
    Uncommitted,
    Commit { sha: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewScope {
    pub reviewer: ReviewerKind,
    pub model: String,
    /// Consult has no diff target in Iced.
    pub target: Option<ReviewTarget>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    ContextRequested,
    Submit {
        kind: ReviewKind,
        scope: ReviewScope,
        note: String,
    },
}
#[derive(Debug, Default, Clone)]
pub struct ReviewContext {
    pub base: Option<String>,
    pub base_source: Option<String>,
    pub ahead: Option<u32>,
    pub head: Option<String>,
    pub dirty: bool,
    pub error: Option<String>,
    pub config: ReviewConfig,
}
impl ReviewContext {
    pub fn base_hint(&self) -> String {
        if self.base.as_ref().is_none_or(|base| base.is_empty()) {
            return "No base branch found; enter one.".into();
        }
        let source = match self.base_source.as_deref() {
            Some("task") => "this task's base",
            Some("agent-workflow") => "agent-workflow.config.json",
            Some("inferred") => "inferred from the repository",
            Some(source) => source,
            None => "?",
        };
        let mut hint = format!("Base from {source}");
        if let Some(ahead) = self.ahead {
            hint.push_str(&format!(
                " · {ahead} {} ahead",
                if ahead == 1 { "commit" } else { "commits" }
            ));
        }
        hint
    }
}
#[derive(Debug, Default, Clone)]
pub struct ReviewDraft {
    pub kind: ReviewKind,
    pub target: Target,
    pub reviewer: ReviewerKind,
    pub claude_model: String,
    pub codex_model: String,
    pub base: String,
    pub sha: String,
    pub note: String,
}
impl ReviewDraft {
    pub fn from_context(kind: ReviewKind, context: &ReviewContext) -> Self {
        let has_base = context.base.as_ref().is_some_and(|base| !base.is_empty());
        Self {
            kind,
            target: if has_base && (context.ahead.unwrap_or(0) > 0 || !context.dirty) {
                Target::Branch
            } else {
                Target::Uncommitted
            },
            reviewer: context.config.default_reviewer,
            claude_model: if context.config.subagent_model.is_empty() {
                "opus".into()
            } else {
                context.config.subagent_model.clone()
            },
            codex_model: context.config.codex_model.clone().unwrap_or_default(),
            base: context.base.clone().unwrap_or_default(),
            sha: context.head.clone().unwrap_or_default(),
            note: String::new(),
        }
    }
    pub fn blocked_reason(&self, busy: bool) -> Option<&'static str> {
        if self.reviewer != ReviewerKind::Codex && busy {
            Some("Waits until Claude finishes this turn")
        } else if self.kind == ReviewKind::Consult && self.note.trim().is_empty() {
            Some("Write the brief first")
        } else if self.kind == ReviewKind::Review
            && ((self.target == Target::Branch && self.base.trim().is_empty())
                || (self.target == Target::Commit && self.sha.trim().is_empty()))
        {
            Some("Fill in the target first")
        } else {
            None
        }
    }
    pub fn submit(&self, busy: bool) -> Option<ReviewOutcome> {
        if self.blocked_reason(busy).is_some() {
            return None;
        }
        Some(ReviewOutcome::Submit {
            kind: self.kind,
            scope: ReviewScope {
                reviewer: self.reviewer,
                model: if self.reviewer == ReviewerKind::Codex {
                    &self.codex_model
                } else {
                    &self.claude_model
                }
                .trim()
                .into(),
                target: (self.kind == ReviewKind::Review).then(|| match self.target {
                    Target::Branch => ReviewTarget::Branch {
                        base: self.base.trim().into(),
                    },
                    Target::Uncommitted => ReviewTarget::Uncommitted,
                    Target::Commit => ReviewTarget::Commit {
                        sha: self.sha.trim().into(),
                    },
                }),
            },
            note: self.note.trim().into(),
        })
    }
}

#[cfg(test)]
mod tests;
