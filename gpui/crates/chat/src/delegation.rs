//! Iced delegation-card projection. The preview deliberately owns no task mutations.
use crate::interactions::TranscriptAction;
use gitterm_core::tasks::{
    Delegation, DelegationChild, DelegationKind, DelegationStatus, ReviewTargetMode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationCard {
    pub delegation: Delegation,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default)]
    pub held: bool,
    #[serde(default)]
    pub worker: Option<Value>,
    /// The chat checkout's HEAD, as Iced's `__setDelegationHead` supplies it,
    /// for the "branch has moved" banner. The view stamps it on every card.
    #[serde(default)]
    pub checkout_head: Option<String>,
}
/// One block of Iced's `.dlg-body`, named after its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardPart {
    /// `.dlg-path`: branch · worktree path.
    Path(String),
    /// `.dlg-banner`: a warn-bordered box.
    Banner(String),
    /// `.dlg-error`: an err-bordered mono box.
    Error(String),
    /// `.dlg-line`: a muted line.
    Line(String),
    /// `.dlg-activity`: a muted mono line.
    Activity(String),
    /// `.dlg-summary`: a review's plain summary.
    Summary(String),
    /// A worker's handoff summary, rendered as markdown.
    Report(String),
    /// `.dlg-section` title and its list.
    Section(&'static str, Vec<String>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub label: String,
    pub tooltip: String,
    pub disabled: bool,
    /// Iced's `button.primary`: Send, and Open worker tab while it waits.
    pub primary: bool,
    pub action: TranscriptAction,
}
impl DelegationCard {
    pub fn is_worker(&self) -> bool {
        self.delegation.kind == DelegationKind::Implement
    }
    fn worker_text(&self, key: &str) -> Option<&str> {
        self.worker
            .as_ref()?
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
    }
    pub fn worker_state(&self) -> &str {
        self.worker_text("session_state")
            .unwrap_or(self.delegation.status.label())
    }
    pub fn delivered(&self) -> bool {
        self.delegation.delivered || self.delegation.delivered_at.is_some()
    }
    pub fn header(&self) -> String {
        let d = &self.delegation;
        let model = match &d.child {
            DelegationChild::TaskSession { model, .. }
            | DelegationChild::CodexReview { model, .. }
            | DelegationChild::CodexConsult { model, .. } => model.as_deref(),
            _ => None,
        }
        .unwrap_or("default model");
        if self.is_worker() {
            let preset = self
                .worker_text("preset")
                .or(match &d.child {
                    DelegationChild::TaskSession { preset_name, .. } => preset_name.as_deref(),
                    _ => None,
                })
                .unwrap_or("agent");
            let mut head = format!(
                "Worker · {preset} · {}",
                self.worker_text("model").unwrap_or(model)
            );
            if let Some(role) = self.worker_text("role") {
                head.push_str(&format!(" · {role}"));
            }
            head
        } else {
            format!(
                "{} · {model}",
                match d.kind {
                    DelegationKind::Consult => "Codex consult",
                    DelegationKind::Review => "Codex review",
                    _ => "Delegation",
                }
            )
        }
    }
    /// The card's fold summary: marker, kind · model, target, state.
    pub fn fold_label(&self, open: bool, now_ms: u64) -> String {
        let [head, target, state] = self.fold_parts(open, now_ms);
        format!("{head} · {target} · {state}")
    }
    /// Iced's summary spans: `marker kind · model`, the target, the state.
    /// The first two shorten with an ellipsis; the state always shows.
    pub fn fold_parts(&self, open: bool, now_ms: u64) -> [String; 3] {
        [
            format!("{} {}", if open { "▾" } else { "▸" }, self.header()),
            self.target(),
            self.state_text(now_ms),
        ]
    }
    pub fn target(&self) -> String {
        if self.is_worker() {
            let title = self
                .worker_text("title")
                .unwrap_or_else(|| self.delegation.brief.lines().next().unwrap_or_default());
            return self
                .worker_text("issue_key")
                .map_or_else(|| title.into(), |key| format!("{key} · {title}"));
        }
        let d = &self.delegation;
        d.target
            .as_ref()
            .map(|t| {
                let mut text = match &t.mode {
                    ReviewTargetMode::Uncommitted => "uncommitted changes".into(),
                    ReviewTargetMode::Base { reference } => {
                        format!("vs {reference} (+ uncommitted)")
                    }
                    ReviewTargetMode::Commit { sha } => format!("commit {}", short_head(sha)),
                };
                if let Some(focus) = t.focus.as_ref().filter(|s| !s.is_empty()) {
                    text.push_str(&format!(" · focus: {focus}"));
                }
                text
            })
            .unwrap_or_else(|| {
                if d.kind == DelegationKind::Consult {
                    d.brief.clone()
                } else {
                    String::new()
                }
            })
    }
    pub fn state_text(&self, now_ms: u64) -> String {
        let d = &self.delegation;
        let state = if self.is_worker() {
            self.worker_state()
        } else {
            d.status.label()
        };
        let text = match state {
            "running" => {
                let stamp = if self.is_worker() {
                    &d.created_at
                } else {
                    &d.updated_at
                };
                let elapsed = gitterm_core::work::summary::parse_stamp(stamp)
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|t| now_ms.saturating_sub(t.as_millis() as u64) / 1000);
                format!(
                    "running {}",
                    elapsed
                        .map(|s| format!("{}:{:02}", s / 60, s % 60))
                        .unwrap_or_default()
                )
            }
            "requested" => "queued".into(),
            "waiting" => "needs you".into(),
            "exited" => "worker exited".into(),
            "completed" if !self.is_worker() => self.findings().map_or_else(
                || "answered".into(),
                |f| {
                    if f.findings.is_empty() {
                        return "no findings".into();
                    }
                    let counts = [
                        gitterm_core::tasks::ReviewSeverity::P0,
                        gitterm_core::tasks::ReviewSeverity::P1,
                        gitterm_core::tasks::ReviewSeverity::P2,
                        gitterm_core::tasks::ReviewSeverity::P3,
                    ]
                    .iter()
                    .filter_map(|severity| {
                        let n = f
                            .findings
                            .iter()
                            .filter(|f| f.severity == *severity)
                            .count();
                        (n > 0).then(|| format!("{n} {severity:?}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                    format!(
                        "{} finding{} ({counts})",
                        f.findings.len(),
                        if f.findings.len() == 1 { "" } else { "s" }
                    )
                },
            ),
            other => other.into(),
        };
        if d.dismissed_at.is_some() {
            format!("dismissed · {text}")
        } else {
            text
        }
    }
    pub fn findings(&self) -> Option<&gitterm_core::tasks::ReviewFindings> {
        self.delegation.result.as_ref()?.findings.as_ref()
    }
    pub fn body(&self) -> String {
        let d = &self.delegation;
        let mut body = String::new();
        if self.is_worker() {
            let path = self.worker_text("worktree_path");
            let branch = self.worker_text("branch");
            if path.is_some() || branch.is_some() {
                body.push_str(&format!(
                    "{}{}\n\n",
                    crate::model::literal(branch.unwrap_or_default()),
                    path.map(|p| format!(" · {}", crate::model::literal(p)))
                        .unwrap_or_default()
                ));
            }
            let banner = match self.worker_state() {
                "waiting" => Some("The worker is waiting for you. Open its tab to answer."),
                "exited" => Some(
                    "The worker stopped without reporting. Open its tab, or the task, to resume it.",
                ),
                "interrupted" => {
                    Some("GitTerm restarted; the worker tab is back. Resume it there.")
                }
                "queued" => Some("Waiting for a free task slot."),
                "blocked" => Some("The worker is blocked and needs you."),
                "cancelled" => Some("No longer tracked. The worker tab and task are untouched."),
                "failed" => Some(failed_message(d, "The worker failed")),
                _ => None,
            };
            if let Some(banner) = banner {
                body.push_str(&crate::model::literal(banner));
                body.push_str("\n\n");
            }
            if !matches!(
                d.status,
                DelegationStatus::Completed | DelegationStatus::Blocked
            ) {
                if let Some(progress) = self.worker_text("progress") {
                    body.push_str(&format!("Latest: {}", crate::model::literal(progress)));
                }
                return body;
            }
        } else {
            match &d.status {
                DelegationStatus::Running | DelegationStatus::Requested => {
                    return crate::model::literal(&self.activity.clone().unwrap_or_else(|| {
                        if d.status == DelegationStatus::Requested {
                            "Waiting for a free Codex slot".into()
                        } else {
                            "Starting Codex…".into()
                        }
                    }));
                }
                DelegationStatus::Failed { .. } => {
                    return crate::model::literal(failed_message(d, "Codex failed"));
                }
                DelegationStatus::Interrupted => {
                    return "GitTerm restarted while this was running. Re-run it to try again."
                        .into();
                }
                DelegationStatus::Cancelled => return "Cancelled.".into(),
                // Iced's renderCard renders a result for `completed` only.
                DelegationStatus::Completed => {}
                _ => return body,
            }
        }
        if let Some(result) = &d.result {
            if let Some(reviewed) = &result.reviewed {
                body.push_str(&format!(
                    "Reviewed at {}{}{}\n\n",
                    short_head(&reviewed.head),
                    if reviewed.dirty { ", dirty" } else { "" },
                    if !reviewed.target_description.is_empty()
                        && reviewed.target_description != "consult"
                    {
                        format!(" · {}", crate::model::literal(&reviewed.target_description))
                    } else {
                        String::new()
                    }
                ));
                if let Some(banner) = self.moved_banner(&reviewed.head) {
                    body.push_str(&crate::model::literal(&banner));
                    body.push_str("\n\n");
                }
            }
            if let Some(f) = &result.findings {
                body.push_str(&format!(
                    "Verdict: {}{}\n\n{}",
                    match f.verdict {
                        gitterm_core::tasks::ReviewVerdict::Correct => "correct",
                        gitterm_core::tasks::ReviewVerdict::NeedsChanges => "needs changes",
                        gitterm_core::tasks::ReviewVerdict::Unknown => "unknown",
                    },
                    if f.structured {
                        ""
                    } else {
                        " · unstructured reply"
                    },
                    crate::model::literal(&f.summary)
                ));
            } else if let Some(h) = &result.handoff {
                body.push_str(&h.summary);
                for (title, items) in [
                    ("Decisions", &h.decisions),
                    ("Next steps", &h.next_steps),
                    ("Blockers", &h.blockers),
                ] {
                    if !items.is_empty() {
                        body.push_str(&format!("\n\n**{title}**\n"));
                        for item in items {
                            body.push_str(&format!("\n- {}", crate::model::literal(item)));
                        }
                    }
                }
            }
        }
        body
    }
    /// Iced's card class (`details.dlg.<state>`): the worker's session
    /// state for a worker, else the delegation's status.
    pub fn state_class(&self) -> &str {
        if self.is_worker() {
            self.worker_state()
        } else {
            self.delegation.status.label()
        }
    }
    /// Iced's `.dlg-body` blocks in order (renderCard / renderWorkerBody /
    /// renderResult), each with its own class. The findings list and the
    /// actions row follow them. `body()` is the same content as one
    /// markdown string, for find and copy.
    pub fn parts(&self) -> Vec<CardPart> {
        let d = &self.delegation;
        let mut parts = Vec::new();
        if self.is_worker() {
            let path = self.worker_text("worktree_path");
            let branch = self.worker_text("branch");
            if path.is_some() || branch.is_some() {
                parts.push(CardPart::Path(format!(
                    "{}{}",
                    branch.unwrap_or_default(),
                    path.map(|p| format!(" · {p}")).unwrap_or_default()
                )));
            }
            let failed = failed_message(d, "The worker failed").to_owned();
            match self.worker_state() {
                "waiting" => parts.push(CardPart::Banner(
                    "The worker is waiting for you. Open its tab to answer.".into(),
                )),
                "exited" => parts.push(CardPart::Banner(
                    "The worker stopped without reporting. Open its tab, or the task, to resume it."
                        .into(),
                )),
                "interrupted" => parts.push(CardPart::Line(
                    "GitTerm restarted; the worker tab is back. Resume it there.".into(),
                )),
                "queued" => parts.push(CardPart::Line("Waiting for a free task slot.".into())),
                "blocked" => parts.push(CardPart::Banner(
                    "The worker is blocked and needs you.".into(),
                )),
                "failed" => parts.push(CardPart::Error(failed)),
                "cancelled" => parts.push(CardPart::Line(
                    "No longer tracked. The worker tab and task are untouched.".into(),
                )),
                _ => {}
            }
            if !matches!(
                d.status,
                DelegationStatus::Completed | DelegationStatus::Blocked
            ) {
                if let Some(progress) = self.worker_text("progress") {
                    parts.push(CardPart::Activity(format!("Latest: {progress}")));
                }
                return parts;
            }
        } else {
            match &d.status {
                DelegationStatus::Running | DelegationStatus::Requested => {
                    parts.push(CardPart::Activity(self.activity.clone().unwrap_or_else(
                        || {
                            if d.status == DelegationStatus::Requested {
                                "Waiting for a free Codex slot".into()
                            } else {
                                "Starting Codex…".into()
                            }
                        },
                    )));
                    return parts;
                }
                DelegationStatus::Failed { .. } => {
                    parts.push(CardPart::Error(failed_message(d, "Codex failed").into()));
                    return parts;
                }
                DelegationStatus::Interrupted => {
                    parts.push(CardPart::Line(
                        "GitTerm restarted while this was running. Re-run it to try again.".into(),
                    ));
                    return parts;
                }
                DelegationStatus::Cancelled => {
                    parts.push(CardPart::Line("Cancelled.".into()));
                    return parts;
                }
                // Iced's renderCard has no body for a blocked review or consult.
                DelegationStatus::Completed => {}
                _ => return parts,
            }
        }
        let Some(result) = &d.result else {
            return parts;
        };
        if let Some(reviewed) = &result.reviewed {
            parts.push(CardPart::Line(format!(
                "Reviewed at {}{}{}",
                short_head(&reviewed.head),
                if reviewed.dirty { ", dirty" } else { "" },
                if !reviewed.target_description.is_empty()
                    && reviewed.target_description != "consult"
                {
                    format!(" · {}", reviewed.target_description)
                } else {
                    String::new()
                }
            )));
            if let Some(banner) = self.moved_banner(&reviewed.head) {
                parts.push(CardPart::Banner(banner));
            }
        }
        if let Some(f) = &result.findings {
            parts.push(CardPart::Line(format!(
                "Verdict: {}{}",
                match f.verdict {
                    gitterm_core::tasks::ReviewVerdict::Correct => "correct",
                    gitterm_core::tasks::ReviewVerdict::NeedsChanges => "needs changes",
                    gitterm_core::tasks::ReviewVerdict::Unknown => "unknown",
                },
                if f.structured {
                    ""
                } else {
                    " · unstructured reply"
                }
            )));
            if !f.summary.is_empty() {
                parts.push(CardPart::Summary(f.summary.clone()));
            }
        } else if let Some(h) = &result.handoff {
            parts.push(CardPart::Report(h.summary.clone()));
            for (title, items) in [
                ("Decisions", &h.decisions),
                ("Next steps", &h.next_steps),
                ("Blockers", &h.blockers),
            ] {
                if !items.is_empty() {
                    parts.push(CardPart::Section(title, items.clone()));
                }
            }
        }
        parts
    }
    /// Iced's renderResult banner when the checkout's HEAD is no longer the
    /// reviewed commit.
    fn moved_banner(&self, reviewed: &str) -> Option<String> {
        let head = self.checkout_head.as_deref().filter(|h| !h.is_empty())?;
        (!reviewed.is_empty() && head != reviewed).then(|| {
            format!(
                "The branch has moved since (now at {}); line numbers may have drifted.",
                short_head(head)
            )
        })
    }
    /// The handoff summary, the one block Iced renders as markdown.
    pub fn report(&self) -> String {
        self.parts()
            .into_iter()
            .find_map(|part| match part {
                CardPart::Report(text) => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }
    pub fn sendable(&self) -> bool {
        matches!(
            self.delegation.status,
            DelegationStatus::Completed | DelegationStatus::Failed { .. }
        ) || (self.is_worker() && self.delegation.status == DelegationStatus::Blocked)
    }
    /// The actions row. `selected` is the card's checked finding ids
    /// (Iced's `card.selected`); with findings and none checked, Send is
    /// disabled (`refreshCardSend`).
    pub fn controls(&self, busy: bool, selected: &HashSet<String>) -> Vec<Control> {
        let d = &self.delegation;
        let worker = self.is_worker();
        let mut controls = Vec::new();
        let mut add = |label: &str, tooltip: &str, disabled, primary, action| {
            controls.push(Control {
                label: label.into(),
                tooltip: tooltip.into(),
                disabled,
                primary,
                action,
            })
        };
        if worker && d.status != DelegationStatus::Cancelled {
            add(
                "Open worker tab",
                if self.worker.as_ref().is_some_and(|w| w["tab_open"] == true) {
                    "Switch to the worker's terminal tab"
                } else {
                    "The worker tab is not open"
                },
                false,
                self.worker_state() == "waiting",
                TranscriptAction::OpenWorker {
                    id: d.delegation_id.clone(),
                },
            );
        }
        if self.sendable() && !self.delivered() && d.dismissed_at.is_none() {
            let label = if self.held {
                "Queued, sends when Claude finishes"
            } else {
                "Send to Claude"
            };
            // Iced's refreshCardSend runs last and sets the title for every
            // card it can send, a worker's included; a held button has none.
            let none = !selected_any(self.findings(), selected);
            add(
                label,
                if self.held {
                    ""
                } else if none {
                    "Select at least one finding"
                } else if busy {
                    "Claude is working; it is sent when this turn ends"
                } else {
                    "Send the selected findings as a message"
                },
                self.held || none,
                true,
                TranscriptAction::SendDelegation {
                    id: d.delegation_id.clone(),
                    finding_ids: self
                        .findings()
                        .into_iter()
                        .flat_map(|findings| &findings.findings)
                        .filter(|finding| selected.contains(&finding.id))
                        .map(|finding| finding.id.clone())
                        .collect(),
                },
            );
        }
        if !worker
            && !matches!(
                d.status,
                DelegationStatus::Running | DelegationStatus::Requested
            )
        {
            add(
                "Re-run",
                "",
                false,
                false,
                TranscriptAction::RerunDelegation {
                    id: d.delegation_id.clone(),
                },
            );
        }
        if d.dismissed_at.is_none() {
            let label = if worker
                && matches!(
                    d.status,
                    DelegationStatus::Running
                        | DelegationStatus::Requested
                        | DelegationStatus::Interrupted
                ) {
                "Stop tracking"
            } else if !worker && d.status.is_active() {
                "Cancel"
            } else {
                "Dismiss"
            };
            add(
                label,
                if label == "Stop tracking" {
                    "Stop waiting for this worker's report; its tab and task stay as they are"
                } else {
                    ""
                },
                false,
                false,
                TranscriptAction::Dismiss(crate::interactions::DismissRequest {
                    id: d.delegation_id.clone(),
                    label: label.into(),
                    active: d.status.is_active(),
                }),
            );
        }
        controls
    }
}
pub fn finding_location(f: &gitterm_core::tasks::ReviewFinding) -> Option<String> {
    let file = f.file.as_ref()?;
    Some(format!(
        "{}{}",
        file.display(),
        f.line_start
            .filter(|n| *n > 0)
            .map(|start| format!(
                ":{start}{}",
                f.line_end
                    .filter(|end| *end != start && *end > 0)
                    .map(|end| format!("-{end}"))
                    .unwrap_or_default()
            ))
            .unwrap_or_default()
    ))
}
/// Iced's `shortHead`: seven characters, `?` when there is none.
fn short_head(value: &str) -> String {
    if value.is_empty() {
        return "?".into();
    }
    value.chars().take(7).collect()
}
/// Iced's `(d.status && d.status.message) || fallback`.
fn failed_message<'a>(d: &'a Delegation, fallback: &'a str) -> &'a str {
    match &d.status {
        DelegationStatus::Failed { message } if !message.is_empty() => message,
        _ => fallback,
    }
}
/// False when the card lists findings and none is checked.
fn selected_any(
    findings: Option<&gitterm_core::tasks::ReviewFindings>,
    selected: &HashSet<String>,
) -> bool {
    findings.is_none_or(|f| f.findings.is_empty()) || !selected.is_empty()
}

#[cfg(test)]
pub(crate) mod tests;
