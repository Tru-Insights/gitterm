//! Iced's shared session attention and chrome presentation rules.
use crate::chat::types::{HarnessEvent, TurnStatus};
use std::time::Duration;

pub const PULSE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionReason {
    HumanInputRequired,
    AgentFailed,
    CompletedUnread,
    /// A Codex review or consult this tab requested has its result (TRU-142).
    DelegationReady(crate::tasks::DelegationKind),
    /// A Codex review or consult this tab requested failed.
    DelegationFailed(crate::tasks::DelegationKind),
    /// A worker this tab delegated reported itself blocked (TRU-142 S6):
    /// it needs the human.
    DelegationBlocked(crate::tasks::DelegationKind),
}

impl AttentionReason {
    /// Rank on the shared attention scale: 0 input needed · 1 failed ·
    /// 2 interrupted (task-only) · 3 ready to review. Failures outrank
    /// completions by construction.
    pub fn priority(self) -> u8 {
        match self {
            Self::HumanInputRequired | Self::DelegationBlocked(_) => 0,
            Self::AgentFailed | Self::DelegationFailed(_) => 1,
            Self::CompletedUnread | Self::DelegationReady(_) => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::HumanInputRequired => crate::attention_notifications::INPUT_REQUIRED,
            Self::AgentFailed => "Agent failed",
            Self::CompletedUnread => "Ready to review",
            Self::DelegationReady(crate::tasks::DelegationKind::Review) => "Review ready",
            Self::DelegationReady(crate::tasks::DelegationKind::Consult) => "Consult ready",
            Self::DelegationReady(crate::tasks::DelegationKind::Implement) => "Worker done",
            Self::DelegationFailed(crate::tasks::DelegationKind::Review) => "Review failed",
            Self::DelegationFailed(crate::tasks::DelegationKind::Consult) => "Consult failed",
            Self::DelegationFailed(crate::tasks::DelegationKind::Implement) => "Worker failed",
            Self::DelegationBlocked(crate::tasks::DelegationKind::Implement) => {
                crate::attention_notifications::WORKER_BLOCKED
            }
            Self::DelegationBlocked(_) => crate::attention_notifications::DELEGATION_BLOCKED,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::HumanInputRequired | Self::DelegationBlocked(_) => "●",
            Self::AgentFailed | Self::DelegationFailed(_) => "!",
            Self::CompletedUnread | Self::DelegationReady(_) => "✓",
        }
    }
}

/// What a chat tab's icon in the session strip says about its live session
/// (TRU-140). Terminal tabs get this from Claude's own title; a chat tab
/// has no title, so the strip reads the agent session instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatTabMarker {
    /// A permission prompt or question waits on the human: the strip's
    /// "needs you" dot. `pulse` is off on the tab in front, whose page
    /// already shows the request card.
    NeedsYou { pulse: bool },
    /// A turn is running: the strip's ▶ marker.
    Running,
}

/// The marker for a chat tab: a pending request outranks a running turn
/// (the human is the blocker); neither means the tab falls back to its
/// attention icon or the idle chat glyph. Clears as soon as the requests
/// are answered and the turn ends, since both inputs are live state.
pub fn chat_tab_marker(
    turn_running: bool,
    pending_requests: usize,
    is_active: bool,
) -> Option<ChatTabMarker> {
    if pending_requests > 0 {
        Some(ChatTabMarker::NeedsYou { pulse: !is_active })
    } else if turn_running {
        Some(ChatTabMarker::Running)
    } else {
        None
    }
}

pub fn harness_event_attention_reason(event: &HarnessEvent) -> Option<AttentionReason> {
    match event {
        HarnessEvent::RuntimeRequest { .. } => Some(AttentionReason::HumanInputRequired),
        HarnessEvent::TurnCompleted { status, .. } => match status {
            TurnStatus::Completed => Some(AttentionReason::CompletedUnread),
            TurnStatus::Failed(_) => Some(AttentionReason::AgentFailed),
            TurnStatus::Interrupted => None,
        },
        HarnessEvent::Error(_) => Some(AttentionReason::AgentFailed),
        _ => None,
    }
}

pub fn terminal_title_attention_reason(title: &str) -> Option<AttentionReason> {
    title
        .starts_with('✳')
        .then_some(AttentionReason::HumanInputRequired)
}
