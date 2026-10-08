//! Codex delegations requested from a chat tab (TRU-142 slices S3b, S4, S5).
//!
//! The pure rules behind the delegation tools and the chat card: turning a
//! request into a `NewDelegation` for the calling tab, which queued runs may
//! start under the concurrency cap, the records `delegation_get` and
//! `delegation_list` return, the bounded "Send to Claude" message, and the
//! hold-until-the-turn-ends rule. The Iced app (and the headless smoke
//! example) own the store and the runner tasks; this module owns no state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::agentd::git::git_command;
use crate::codex_runner::{describe_target, CodexRun, CodexRunKind};
use crate::review::{is_commit_sha, is_model_name, is_safe_ref};
use crate::task_mcp::{
    ConsultDelegationRequest, HandoffStatus, ReviewDelegationRequest, ReviewTargetKind,
};
use crate::tasks::{
    Delegation, DelegationChild, DelegationKind, DelegationParent, DelegationResult,
    DelegationStatus, NewDelegation, ReviewSeverity, ReviewTarget, ReviewTargetMode, ReviewVerdict,
    TaskHandoff, TaskLifecycle, TaskRecord, TaskStore,
};
use crate::workers::WorkerChoice;

/// Directory under the GitTerm config root holding each run's JSONL log
/// (`<id>.jsonl`) and Codex's final message (`<id>.last.md`).
pub const DELEGATIONS_DIR: &str = "delegations";
/// Upper bound of the message "Send to Claude" submits.
pub const MAX_SEND_BYTES: usize = 8 * 1024;
const MAX_BRIEF_CHARS: usize = 20_000;
const MAX_FOCUS_CHARS: usize = 2_000;
const FINDING_BODY_CHARS: usize = 400;
const LIST_SUMMARY_CHARS: usize = 300;
const STATUS_FILTERS: [&str; 7] = [
    "requested",
    "running",
    "completed",
    "blocked",
    "failed",
    "interrupted",
    "cancelled",
];
const PROGRESS_LINE_CHARS: usize = 200;

/// The tab a delegation request came from, as the app resolved it from the
/// caller identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerTab {
    pub session_uid: String,
    pub chat_session_id: Option<String>,
    pub workspace: String,
    /// The tab's checkout; Codex runs here.
    pub cwd: PathBuf,
    /// The tab's workspace lives on another machine.
    pub remote: bool,
}

impl CallerTab {
    fn parent(&self) -> DelegationParent {
        DelegationParent {
            session_uid: self.session_uid.clone(),
            chat_session_id: self.chat_session_id.clone(),
            workspace: self.workspace.clone(),
            cwd: self.cwd.clone(),
        }
    }

    fn refuse_remote(&self, tool: &str) -> Result<(), String> {
        if self.remote {
            return Err(format!(
                "{tool} runs Codex in the calling tab's checkout, and the tab's workspace \
                 {:?} is remote; remote delegations are not supported yet",
                self.workspace
            ));
        }
        Ok(())
    }
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `model` trimmed, `None` when blank, or an error naming the bad value.
fn codex_model(
    requested: Option<&str>,
    configured: Option<&str>,
) -> Result<Option<String>, String> {
    match clean(requested).or_else(|| clean(configured)) {
        None => Ok(None),
        Some(model) if is_model_name(&model) => Ok(Some(model)),
        Some(model) => Err(format!("model {model:?} is not a Codex model name")),
    }
}

/// The review delegation `review_request` (or the Review… popover's Codex
/// path) asks for. `configured_model` is `review.codex_model`.
pub fn review_delegation(
    request: &ReviewDelegationRequest,
    caller: &CallerTab,
    configured_model: Option<&str>,
    previous: Option<String>,
) -> Result<NewDelegation, String> {
    caller.refuse_remote("review_request")?;
    if let Some(reviewer) = clean(request.reviewer.as_deref()) {
        if !reviewer.eq_ignore_ascii_case("codex") {
            return Err(format!(
                "reviewer {reviewer:?} is not supported: review_request runs Codex only. For a \
                 Claude review, spawn a review subagent with your own Agent tool"
            ));
        }
    }
    let base_ref = clean(request.base_ref.as_deref());
    let commit = clean(request.commit.as_deref());
    let mode = match request.target {
        ReviewTargetKind::Uncommitted => {
            if base_ref.is_some() || commit.is_some() {
                return Err("target \"uncommitted\" takes neither base_ref nor commit".to_string());
            }
            ReviewTargetMode::Uncommitted
        }
        ReviewTargetKind::Base => {
            if commit.is_some() {
                return Err("target \"base\" takes base_ref, not commit".to_string());
            }
            let Some(reference) = base_ref else {
                return Err(
                    "target \"base\" needs base_ref (the branch to diff against)".to_string(),
                );
            };
            if !is_safe_ref(&reference) {
                return Err(format!(
                    "base_ref {reference:?} is not a valid git reference"
                ));
            }
            ReviewTargetMode::Base { reference }
        }
        ReviewTargetKind::Commit => {
            if base_ref.is_some() {
                return Err("target \"commit\" takes commit, not base_ref".to_string());
            }
            let Some(sha) = commit else {
                return Err("target \"commit\" needs commit (a SHA)".to_string());
            };
            if !is_commit_sha(&sha) {
                return Err(format!(
                    "commit {sha:?} is not a commit SHA (7 to 40 hex characters)"
                ));
            }
            ReviewTargetMode::Commit { sha }
        }
    };
    let focus = clean(request.focus.as_deref());
    if focus
        .as_deref()
        .is_some_and(|focus| focus.chars().count() > MAX_FOCUS_CHARS)
    {
        return Err(format!("focus is longer than {MAX_FOCUS_CHARS} characters"));
    }
    let model = codex_model(request.model.as_deref(), configured_model)?;
    let target = ReviewTarget { mode, focus };
    Ok(NewDelegation {
        kind: DelegationKind::Review,
        parent: caller.parent(),
        child: DelegationChild::CodexReview {
            thread_id: None,
            model,
        },
        brief: format!("Codex review of {}", describe_target(&target)),
        target: Some(target),
        previous,
    })
}

/// The consult delegation `consult_request` (or the Consult… popover's Codex
/// path) asks for.
pub fn consult_delegation(
    request: &ConsultDelegationRequest,
    caller: &CallerTab,
    configured_model: Option<&str>,
    previous: Option<String>,
) -> Result<NewDelegation, String> {
    caller.refuse_remote("consult_request")?;
    let brief = request.brief.trim();
    if brief.is_empty() {
        return Err("consult_request needs a brief".to_string());
    }
    if brief.chars().count() > MAX_BRIEF_CHARS {
        return Err(format!(
            "the brief is longer than {MAX_BRIEF_CHARS} characters; summarise it"
        ));
    }
    Ok(NewDelegation {
        kind: DelegationKind::Consult,
        parent: caller.parent(),
        child: DelegationChild::CodexConsult {
            thread_id: None,
            model: codex_model(request.model.as_deref(), configured_model)?,
        },
        brief: brief.to_string(),
        target: None,
        previous,
    })
}

/// The Codex review the Review… popover asks for, in `review_request`'s
/// terms, so both paths validate alike. The page's "branch vs base" maps to
/// Codex `--base`, which also covers uncommitted changes.
pub fn review_request_from_popover(
    request: &crate::review::ReviewRequest,
) -> ReviewDelegationRequest {
    use crate::review::ReviewTarget as PageTarget;
    let (target, base_ref, commit) = match &request.target {
        PageTarget::Branch { base } => (ReviewTargetKind::Base, Some(base.clone()), None),
        PageTarget::Uncommitted => (ReviewTargetKind::Uncommitted, None, None),
        PageTarget::Commit { sha } => (ReviewTargetKind::Commit, None, Some(sha.clone())),
    };
    ReviewDelegationRequest {
        target,
        base_ref,
        commit,
        focus: request.focus.clone(),
        reviewer: Some("codex".to_string()),
        model: Some(request.model.clone()),
    }
}

/// The request a Re-run of `delegation` makes: same kind, target, brief and
/// model, following it as `previous`.
pub fn rerun_delegation(delegation: &Delegation) -> Result<NewDelegation, String> {
    let child = match &delegation.child {
        DelegationChild::CodexReview { model, .. } => DelegationChild::CodexReview {
            thread_id: None,
            model: model.clone(),
        },
        DelegationChild::CodexConsult { model, .. } => DelegationChild::CodexConsult {
            thread_id: None,
            model: model.clone(),
        },
        other => {
            return Err(format!(
                "delegation {} is not a Codex run ({other:?}) and cannot be re-run here",
                delegation.delegation_id
            ))
        }
    };
    Ok(NewDelegation {
        kind: delegation.kind,
        parent: delegation.parent.clone(),
        child,
        brief: delegation.brief.clone(),
        target: delegation.target.clone(),
        previous: Some(delegation.delegation_id.clone()),
    })
}

/// The implementation delegation `delegate_task` asks for: a worker in task
/// `task_id`, which the app has just created in the calling tab's
/// repository. The worker's session is attached when its launch starts.
pub fn worker_delegation(
    caller: &CallerTab,
    task_id: &str,
    title: &str,
    objective: &str,
    choice: &WorkerChoice,
) -> Result<NewDelegation, String> {
    caller.refuse_remote("delegate_task")?;
    let title = title.trim();
    let objective = objective.trim();
    if title.is_empty() || objective.is_empty() {
        return Err("delegate_task needs a title and an objective".to_string());
    }
    if objective.chars().count() > MAX_BRIEF_CHARS {
        return Err(format!(
            "the objective is longer than {MAX_BRIEF_CHARS} characters; summarise it"
        ));
    }
    Ok(NewDelegation {
        kind: DelegationKind::Implement,
        parent: caller.parent(),
        child: DelegationChild::TaskSession {
            task_id: task_id.to_string(),
            task_session_id: None,
            conversation: None,
            preset_name: Some(choice.preset_name.clone()),
            model: choice.model.clone(),
            role: Some(choice.role),
        },
        brief: format!("{title}\n\n{objective}"),
        target: None,
        previous: None,
    })
}

/// The section a delegated worker's brief ends with: who is waiting, and
/// how to report through `task_update_handoff`.
pub fn report_back_section(delegation_id: &str, task_id: &str) -> String {
    format!(
        "Report back:\n\
This task is GitTerm delegation {delegation_id}: a coordinator chat handed it to you and is \
waiting for your report. Report through GitTerm's task_update_handoff tool for task {task_id}, \
from this session:\n\
- As you make progress, call it with status \"progress\" and a short summary of where you are.\n\
- When the objective is met (stop where \"Stop when\" says), call it once with status \"done\".\n\
- If you cannot go on without a human decision, access or missing context, call it with status \
\"blocked\" and say why in blockers.\n\
- Every report carries summary, decisions, next_steps and blockers. Keep them concise; do not \
paste transcripts or diffs.\n\
- Do not merge or push unless the objective says so."
    )
}

/// Which open worker delegation a `task_update_handoff` from task
/// `task_id` reports to. `hosted_session` is the task session the calling
/// tab hosts (S1 caller attribution), never an id the caller typed.
/// `Ok(None)`: no worker delegation is open on the task, so the handoff is
/// only a task handoff. An open one that the caller does not host is an
/// error, so nobody but the worker can complete or block it.
pub fn worker_report_target(
    open: &[&Delegation],
    task_id: &str,
    hosted_session: Option<&str>,
) -> Result<Option<String>, String> {
    if open.is_empty() {
        return Ok(None);
    }
    let hosted = |delegation: &&&Delegation| match &delegation.child {
        DelegationChild::TaskSession {
            task_session_id: Some(session),
            ..
        } => Some(session.as_str()) == hosted_session,
        _ => false,
    };
    match open.iter().find(hosted) {
        Some(delegation) => Ok(Some(delegation.delegation_id.clone())),
        None => Err(format!(
            "task {task_id} is delegation {}'s worker task, and only the worker's own GitTerm tab \
             can report done or blocked; this call did not come from it. Record notes with \
             status \"progress\" instead",
            open[0].delegation_id
        )),
    }
}

/// The worker's report as the delegation's result: a snapshot, so a later
/// task handoff cannot overwrite what the parent was told.
pub fn worker_result(handoff: TaskHandoff) -> DelegationResult {
    DelegationResult {
        handoff: Some(handoff),
        findings: None,
        reviewed: None,
    }
}

/// Stores a `task_update_handoff` (S6): the task handoff as before, and for
/// `done` / `blocked` from the worker's own session (`hosted_session`, the
/// task session the calling tab hosts) a snapshot into the open worker
/// delegation, which completes or blocks. Returns that delegation's id, or
/// `None` when the handoff only updated the task. The target is checked
/// before anything is written, so a refused report stores nothing.
pub fn record_handoff(
    store: &mut TaskStore,
    task_id: &str,
    handoff: TaskHandoff,
    status: HandoffStatus,
    hosted_session: Option<&str>,
    timestamp: &str,
) -> Result<Option<String>, String> {
    let report_to = if status == HandoffStatus::Progress {
        None
    } else {
        worker_report_target(
            &store.open_worker_delegations(task_id),
            task_id,
            hosted_session,
        )?
    };
    store
        .update_handoff(task_id, handoff.clone(), timestamp)
        .map_err(|error| error.to_string())?;
    if let Some(delegation_id) = &report_to {
        let result = worker_result(handoff);
        let stored = if status == HandoffStatus::Done {
            store.complete_delegation(delegation_id, result, timestamp)
        } else {
            store.block_delegation(delegation_id, result, timestamp)
        };
        stored.map_err(|error| {
            format!(
                "the handoff was saved on task {task_id}, but delegation {delegation_id} could \
                 not record it: {error}"
            )
        })?;
    }
    Ok(report_to)
}

/// The worker's tab as GitTerm sees it, for the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTab {
    /// No open tab hosts the worker's session.
    Closed,
    /// Open, with no live harness signal (restored, or the harness exited).
    Idle,
    Working,
    /// The harness waits for the human (Codex notify, Claude/pi title).
    AwaitingInput,
}

/// The card's worker state: queued, running, waiting, interrupted,
/// exited, or the delegation's own outcome (done, blocked, failed,
/// cancelled).
pub fn worker_session_state(
    status: &DelegationStatus,
    lifecycle: Option<TaskLifecycle>,
    tab: WorkerTab,
) -> &'static str {
    match status {
        DelegationStatus::Completed => return "done",
        DelegationStatus::Blocked => return "blocked",
        DelegationStatus::Failed { .. } => return "failed",
        DelegationStatus::Cancelled => return "cancelled",
        DelegationStatus::Requested | DelegationStatus::Running | DelegationStatus::Interrupted => {
        }
    }
    if lifecycle == Some(TaskLifecycle::Queued) {
        return "queued";
    }
    match tab {
        WorkerTab::AwaitingInput => "waiting",
        WorkerTab::Working => "running",
        WorkerTab::Closed => {
            if *status == DelegationStatus::Requested {
                "starting"
            } else {
                "exited"
            }
        }
        WorkerTab::Idle => match lifecycle {
            Some(TaskLifecycle::Interrupted) => "interrupted",
            Some(TaskLifecycle::Stopped | TaskLifecycle::Failed | TaskLifecycle::Completed) => {
                "exited"
            }
            _ => "running",
        },
    }
}

/// What the chat card and roster show about a worker beyond the record:
/// the task, where it works, its live state and latest progress line.
pub fn worker_card_info(
    delegation: &Delegation,
    task: Option<&TaskRecord>,
    tab: WorkerTab,
) -> Value {
    let (preset, model, role) = match &delegation.child {
        DelegationChild::TaskSession {
            preset_name,
            model,
            role,
            ..
        } => (
            preset_name.clone(),
            model.clone(),
            role.map(|role| role.label()),
        ),
        _ => (None, None, None),
    };
    let progress = task.and_then(|task| task.handoff.as_ref()).map(|handoff| {
        let line = handoff.summary.trim().lines().next().unwrap_or_default();
        (
            truncate_chars(line, PROGRESS_LINE_CHARS),
            handoff.updated_at.clone(),
        )
    });
    json!({
        "task_id": task.map(|task| task.task_id.clone()),
        "title": task.map(|task| task.title.clone()),
        "issue_key": task.and_then(|task| task.issue.as_ref()).map(|issue| issue.key.clone()),
        "preset": preset,
        "model": model,
        "role": role,
        "worktree_path": task.and_then(|task| task.worktree.path.clone()),
        "branch": task.map(|task| task.branch.clone()),
        "session_state": worker_session_state(
            &delegation.status,
            task.map(|task| task.lifecycle),
            tab,
        ),
        "tab_open": tab != WorkerTab::Closed,
        "progress": progress.as_ref().map(|(line, _)| line.clone()),
        "progress_at": progress.map(|(_, at)| at),
    })
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| {
            format!(
                "could not run git {} in {}: {error}",
                args.join(" "),
                cwd.display()
            )
        })?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Refuses a checkout Codex cannot review: not a directory, not a git work
/// tree, or without a commit. Blocking; call off the UI thread.
pub fn check_checkout(cwd: &Path) -> Result<(), String> {
    if !cwd.is_dir() {
        return Err(format!(
            "the calling tab's checkout {} is not a directory",
            cwd.display()
        ));
    }
    git_stdout(cwd, &["rev-parse", "--show-toplevel"]).map_err(|error| {
        format!(
            "the calling tab's directory {} is not a git checkout: {error}",
            cwd.display()
        )
    })?;
    git_stdout(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"]).map_err(|_| {
        format!(
            "the git checkout {} has no commits yet; Codex reviews need one",
            cwd.display()
        )
    })?;
    Ok(())
}

/// The checkout's HEAD commit, for the "branch has moved" banner. Blocking.
pub fn current_head(cwd: &Path) -> Result<String, String> {
    git_stdout(cwd, &["rev-parse", "HEAD"])
        .map_err(|error| format!("could not read HEAD in {}: {error}", cwd.display()))
}

pub fn log_path(config_root: &Path, delegation_id: &str) -> PathBuf {
    config_root
        .join(DELEGATIONS_DIR)
        .join(format!("{delegation_id}.jsonl"))
}

/// The Codex run that carries `delegation` out.
pub fn codex_run(delegation: &Delegation, config_root: &Path) -> Result<CodexRun, String> {
    let id = &delegation.delegation_id;
    let Some((_, model)) = delegation.codex_child() else {
        return Err(format!("delegation {id} is not a Codex run"));
    };
    let kind = match delegation.kind {
        DelegationKind::Review => CodexRunKind::Review(
            delegation
                .target
                .clone()
                .ok_or_else(|| format!("review delegation {id} has no target"))?,
        ),
        DelegationKind::Consult => CodexRunKind::Consult {
            brief: delegation.brief.clone(),
        },
        DelegationKind::Implement => {
            return Err(format!(
                "delegation {id} is an implementation, not a Codex run"
            ))
        }
    };
    Ok(CodexRun {
        kind,
        cwd: delegation.parent.cwd.clone(),
        model: model.map(str::to_string),
        log_path: log_path(config_root, id),
    })
}

/// Queued Codex runs to start now, oldest first: `requested` delegations
/// with a Codex child that are not running, up to the free slots under
/// `max_concurrent` (zero counts as 1). `delegations` is in store order
/// (oldest first).
pub fn runs_to_start(
    delegations: &[Delegation],
    running: &HashSet<String>,
    max_concurrent: usize,
) -> Vec<String> {
    let free = max_concurrent.max(1).saturating_sub(running.len());
    delegations
        .iter()
        .filter(|delegation| delegation.status == DelegationStatus::Requested)
        .filter(|delegation| delegation.codex_child().is_some())
        .filter(|delegation| !running.contains(&delegation.delegation_id))
        .take(free)
        .map(|delegation| delegation.delegation_id.clone())
        .collect()
}

/// The full record `delegation_get` returns: the stored delegation plus
/// where its log is.
pub fn delegation_record(delegation: &Delegation, config_root: &Path) -> Result<Value, String> {
    let mut value = serde_json::to_value(delegation).map_err(|error| {
        format!(
            "could not encode delegation {}: {error}",
            delegation.delegation_id
        )
    })?;
    if delegation.codex_child().is_some() {
        value["log_path"] = Value::String(
            log_path(config_root, &delegation.delegation_id)
                .display()
                .to_string(),
        );
    }
    Ok(value)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One row of `delegation_list`: enough to pick a delegation, not the result.
pub fn delegation_summary(delegation: &Delegation) -> Value {
    let mut row = json!({
        "delegation_id": delegation.delegation_id,
        "kind": delegation.kind.label(),
        "status": delegation.status.label(),
        "brief": truncate_chars(&delegation.brief, LIST_SUMMARY_CHARS),
        "created_at": delegation.created_at,
        "updated_at": delegation.updated_at,
    });
    if let DelegationStatus::Failed { message } = &delegation.status {
        row["error"] = Value::String(truncate_chars(message, LIST_SUMMARY_CHARS));
    }
    if let DelegationChild::TaskSession { task_id, .. } = &delegation.child {
        row["task_id"] = Value::String(task_id.clone());
    }
    if let Some(delivered_at) = &delegation.delivered_at {
        row["delivered_at"] = Value::String(delivered_at.clone());
    }
    if let Some(result) = &delegation.result {
        if let Some(findings) = &result.findings {
            row["verdict"] = Value::String(verdict_label(findings.verdict).to_string());
            row["finding_count"] = json!(findings.findings.len());
            row["summary"] = Value::String(truncate_chars(&findings.summary, LIST_SUMMARY_CHARS));
        } else if let Some(handoff) = &result.handoff {
            row["summary"] = Value::String(truncate_chars(&handoff.summary, LIST_SUMMARY_CHARS));
        }
    }
    row
}

/// `delegation_list`'s `status` filter, checked.
pub fn parse_status_filter(status: Option<&str>) -> Result<Option<&'static str>, String> {
    let Some(status) = clean(status) else {
        return Ok(None);
    };
    STATUS_FILTERS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(&status))
        .map(|known| Some(*known))
        .ok_or_else(|| {
            format!(
                "status {status:?} is not one of {}",
                STATUS_FILTERS.join(", ")
            )
        })
}

/// `delegation_list`'s reply. `delegations` are the caller's, newest first
/// (`TaskStore::delegations_for_parent`).
pub fn delegation_list(delegations: &[&Delegation], status: Option<&str>) -> Result<Value, String> {
    let status = parse_status_filter(status)?;
    let rows: Vec<Value> = delegations
        .iter()
        .filter(|delegation| status.is_none_or(|status| delegation.status.label() == status))
        .map(|delegation| delegation_summary(delegation))
        .collect();
    Ok(json!({ "delegations": rows }))
}

pub fn verdict_label(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::Correct => "correct",
        ReviewVerdict::NeedsChanges => "needs_changes",
        ReviewVerdict::Unknown => "unknown",
    }
}

fn severity_label(severity: ReviewSeverity) -> &'static str {
    match severity {
        ReviewSeverity::P0 => "P0",
        ReviewSeverity::P1 => "P1",
        ReviewSeverity::P2 => "P2",
        ReviewSeverity::P3 => "P3",
    }
}

fn short_head(head: &str) -> &str {
    head.get(..7).unwrap_or(head)
}

/// Appends `block` (and a newline) when the whole message, with `footer`,
/// still fits `MAX_SEND_BYTES`.
fn push_if_fits(message: &mut String, block: &str, footer: &str) -> bool {
    if message.len() + block.len() + 1 + footer.len() > MAX_SEND_BYTES {
        return false;
    }
    message.push_str(block);
    message.push('\n');
    true
}

/// Cuts `text` at a char boundary to at most `max_bytes` bytes.
fn cut_to_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The one message "Send to Claude" submits for a completed delegation: a
/// header with the verdict and counts, the selected findings (`selected`
/// finding ids, in reviewer order) or the consult's answer, and a pointer to
/// `delegation_get`. Never longer than `MAX_SEND_BYTES`; what does not fit
/// is named and left to `delegation_get`.
pub fn compose_send_message(
    delegation: &Delegation,
    selected: &[String],
) -> Result<String, String> {
    let id = &delegation.delegation_id;
    let sendable = match delegation.kind {
        DelegationKind::Implement => delegation.status.has_result(),
        DelegationKind::Review | DelegationKind::Consult => {
            delegation.status == DelegationStatus::Completed
        }
    };
    if !sendable {
        return Err(format!(
            "delegation {id} is {}; only a completed result can be sent",
            delegation.status.label()
        ));
    }
    let Some(result) = &delegation.result else {
        return Err(format!("delegation {id} has no result"));
    };
    let footer = format!("Full record: delegation_get {id}");
    let reviewed = result
        .reviewed
        .as_ref()
        .map(|reviewed| {
            format!(
                " at {}{}",
                short_head(&reviewed.head),
                if reviewed.dirty { ", dirty" } else { "" }
            )
        })
        .unwrap_or_default();
    let mut message = String::new();
    match delegation.kind {
        DelegationKind::Review => {
            let Some(findings) = &result.findings else {
                return Err(format!("review delegation {id} has no findings"));
            };
            let known: HashSet<&str> = findings.findings.iter().map(|f| f.id.as_str()).collect();
            if let Some(unknown) = selected.iter().find(|fid| !known.contains(fid.as_str())) {
                return Err(format!("delegation {id} has no finding {unknown}"));
            }
            let chosen: Vec<_> = findings
                .findings
                .iter()
                .filter(|finding| selected.contains(&finding.id))
                .collect();
            if chosen.is_empty() && !findings.findings.is_empty() {
                return Err("select at least one finding to send".to_string());
            }
            let mut counts: Vec<(ReviewSeverity, usize)> = Vec::new();
            for finding in &chosen {
                match counts
                    .iter_mut()
                    .find(|(severity, _)| *severity == finding.severity)
                {
                    Some((_, count)) => *count += 1,
                    None => counts.push((finding.severity, 1)),
                }
            }
            counts.sort();
            let counts = counts
                .iter()
                .map(|(severity, count)| format!("{count} {}", severity_label(*severity)))
                .collect::<Vec<_>>()
                .join(", ");
            let target = delegation
                .target
                .as_ref()
                .map(|target| format!(" of {}", describe_target(target)))
                .unwrap_or_default();
            let header = if findings.findings.is_empty() {
                format!(
                    "Codex review {id}{target}{reviewed}: verdict {}, no findings.",
                    verdict_label(findings.verdict)
                )
            } else {
                format!(
                    "Codex review {id}{target}{reviewed}: verdict {}, {} selected of {} findings ({counts}).",
                    verdict_label(findings.verdict),
                    chosen.len(),
                    findings.findings.len()
                )
            };
            push_if_fits(
                &mut message,
                cut_to_bytes(&header, MAX_SEND_BYTES / 2),
                &footer,
            );
            if !findings.structured || findings.findings.is_empty() {
                let summary = truncate_chars(&findings.summary, 2_000);
                if !summary.is_empty() {
                    push_if_fits(&mut message, &summary, &footer);
                }
            }
            let omission = |count: usize| {
                format!(
                    "… {count} more selected findings omitted; read them with delegation_get {id}.\n"
                )
            };
            let mut omitted = 0;
            for finding in &chosen {
                let location = match (&finding.file, finding.line_start) {
                    (Some(file), Some(line)) => format!(" {}:{line}", file.display()),
                    (Some(file), None) => format!(" {}", file.display()),
                    _ => String::new(),
                };
                let line = format!(
                    "{} [{}]{location} {}",
                    finding.id,
                    severity_label(finding.severity),
                    finding.title.trim()
                );
                let body = truncate_chars(&finding.body, FINDING_BODY_CHARS);
                let block = if body.is_empty() {
                    line.clone()
                } else {
                    format!("{line}\n    {}", body.replace('\n', "\n    "))
                };
                // Leave room for the longest omission note.
                let room_footer = format!("{}{footer}", omission(chosen.len()));
                if !push_if_fits(&mut message, &block, &room_footer)
                    && !push_if_fits(&mut message, cut_to_bytes(&line, 300), &room_footer)
                {
                    omitted += 1;
                }
            }
            if omitted > 0 {
                message.push_str(&omission(omitted));
            }
        }
        DelegationKind::Consult | DelegationKind::Implement => {
            let Some(handoff) = &result.handoff else {
                return Err(format!(
                    "{} delegation {id} has no handoff",
                    delegation.kind.label()
                ));
            };
            let header = if delegation.kind == DelegationKind::Consult {
                format!("Codex consult {id}{reviewed}:")
            } else {
                worker_send_header(delegation)
            };
            push_if_fits(
                &mut message,
                cut_to_bytes(&header, MAX_SEND_BYTES / 4),
                &footer,
            );
            let mut sections = vec![handoff.summary.trim().to_string()];
            for (heading, items) in [
                ("Decisions", &handoff.decisions),
                ("Next steps", &handoff.next_steps),
                ("Blockers", &handoff.blockers),
            ] {
                if !items.is_empty() {
                    let list = items
                        .iter()
                        .map(|item| format!("- {}", item.trim()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    sections.push(format!("{heading}:\n{list}"));
                }
            }
            let note = "… (cut; the full answer is in delegation_get)\n";
            let budget = MAX_SEND_BYTES - message.len() - footer.len() - note.len() - 2;
            let body = sections.join("\n\n");
            let cut = cut_to_bytes(&body, budget);
            message.push_str(cut);
            message.push('\n');
            if cut.len() < body.len() {
                message.push_str(note);
            }
        }
    }
    message.push_str(&footer);
    debug_assert!(message.len() <= MAX_SEND_BYTES);
    Ok(message)
}

/// The first line of a worker's "Send to Claude" message: who reported
/// what, on which task and branch.
fn worker_send_header(delegation: &Delegation) -> String {
    let id = &delegation.delegation_id;
    let outcome = if delegation.status == DelegationStatus::Blocked {
        "is blocked"
    } else {
        "reports done"
    };
    let (task_id, who) = match &delegation.child {
        DelegationChild::TaskSession {
            task_id,
            preset_name,
            model,
            ..
        } => {
            let who = [preset_name.as_deref(), model.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            (task_id.as_str(), who)
        }
        _ => ("?", String::new()),
    };
    let title = delegation.brief.lines().next().unwrap_or_default().trim();
    let who = if who.is_empty() {
        String::new()
    } else {
        format!(" ({who})")
    };
    format!("Worker {id}{who} {outcome}: {title}. Task {task_id}; task_get {task_id} has its worktree and branch.")
}

/// A "Send to Claude" message waiting for the parent's turn to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldSend {
    pub delegation_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendDecision {
    /// Submit now and stamp `delivered_at`.
    SendNow,
    /// Claude is mid-turn: hold it and send when the turn ends.
    Hold,
    /// Sent before (survives replay and restart): do nothing.
    AlreadyDelivered,
    /// Already waiting for the turn to end: do nothing.
    AlreadyHeld,
}

/// What a press of "Send to Claude" does.
pub fn decide_send(
    streaming: bool,
    delivered: bool,
    held: &[HeldSend],
    delegation_id: &str,
) -> SendDecision {
    if delivered {
        SendDecision::AlreadyDelivered
    } else if held.iter().any(|send| send.delegation_id == delegation_id) {
        SendDecision::AlreadyHeld
    } else if streaming {
        SendDecision::Hold
    } else {
        SendDecision::SendNow
    }
}

/// When the parent's turn ends: the oldest held message to send now. One per
/// turn, because sending starts the next turn; the rest wait for its end.
pub fn next_held_send(held: &mut Vec<HeldSend>, streaming: bool) -> Option<HeldSend> {
    if streaming || held.is_empty() {
        return None;
    }
    Some(held.remove(0))
}

/// The chat card payload (`__appendEvent({"kind":"delegation", ...})`):
/// the record, the latest Codex activity line, and whether a send is held.
pub fn card_payload(
    delegation: &Delegation,
    activity: Option<&str>,
    held: bool,
    worker: Option<Value>,
    config_root: &Path,
) -> Result<Value, String> {
    let mut payload = json!({
        "kind": "delegation",
        "delegation": delegation_record(delegation, config_root)?,
        "activity": activity,
        "held": held,
    });
    if let Some(worker) = worker {
        payload["worker"] = worker;
    }
    Ok(payload)
}

/// The timeline marker recorded in the parent tab's conversation where the
/// request was made, so a rebuilt page puts the card back there.
pub fn anchor_payload(delegation_id: &str) -> Value {
    json!({ "kind": "delegation_anchor", "delegation_id": delegation_id })
}

/// A ready local task with its worktree at `worktree`, for the tests of
/// this crate that exercise workers.
#[cfg(test)]
pub(crate) fn test_task(task_id: &str, worktree: &Path) -> TaskRecord {
    use crate::tasks::{
        ExecutorTarget, GitBase, IssueProvider, IssueReference, NewTaskRecord, RepositoryIdentity,
        StoppingBoundary, TaskWorktree, TaskWorktreeState, WorkspaceIdentity,
        WorkspaceLocationIdentity,
    };
    let mut task = TaskRecord::new_draft(
        NewTaskRecord {
            task_id: task_id.to_string(),
            title: "Fix the Excalidraw export".to_string(),
            objective: "Make the export keep embedded images; stop when tests pass.".to_string(),
            workspace: WorkspaceIdentity {
                name: "GitTerm".to_string(),
                location: WorkspaceLocationIdentity::Local {
                    directory: PathBuf::from("/repo"),
                },
            },
            repository: RepositoryIdentity {
                common_dir: PathBuf::from("/repo/.git"),
                remote_url: None,
            },
            issue: Some(IssueReference {
                provider: IssueProvider::Linear,
                key: "TRU-150".to_string(),
                url: None,
            }),
            base: GitBase {
                reference: "v5".to_string(),
                commit: "0123456789abcdef".to_string(),
            },
            branch: "tracey/tru-150-excalidraw-export".to_string(),
            executor: ExecutorTarget::Local,
            harness: None,
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
        },
        "2026-10-08T10:00:00Z",
    );
    task.worktree = TaskWorktree {
        state: TaskWorktreeState::Ready,
        path: Some(worktree.to_path_buf()),
    };
    task
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{
        DelegationResult, ReviewFinding, ReviewFindings, ReviewedState, Reviewer, TaskHandoff,
    };

    fn caller() -> CallerTab {
        CallerTab {
            session_uid: "tab-uid".into(),
            chat_session_id: Some("claude-1".into()),
            workspace: "GitTerm".into(),
            cwd: PathBuf::from("/repo"),
            remote: false,
        }
    }

    fn review_request(target: ReviewTargetKind) -> ReviewDelegationRequest {
        ReviewDelegationRequest {
            target,
            base_ref: None,
            commit: None,
            focus: None,
            reviewer: None,
            model: None,
        }
    }

    #[test]
    fn review_requests_map_targets_and_refuse_bad_input() {
        let new = review_delegation(
            &review_request(ReviewTargetKind::Uncommitted),
            &caller(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(new.kind, DelegationKind::Review);
        assert_eq!(new.parent.session_uid, "tab-uid");
        assert_eq!(new.parent.cwd, PathBuf::from("/repo"));
        assert_eq!(new.target.unwrap().mode, ReviewTargetMode::Uncommitted);
        assert_eq!(
            new.child,
            DelegationChild::CodexReview {
                thread_id: None,
                model: None
            }
        );

        let mut base = review_request(ReviewTargetKind::Base);
        base.base_ref = Some(" v5 ".into());
        base.focus = Some("webview lifecycle".into());
        base.reviewer = Some("Codex".into());
        let new = review_delegation(&base, &caller(), Some("gpt-6-astra"), None).unwrap();
        let target = new.target.unwrap();
        assert_eq!(
            target.mode,
            ReviewTargetMode::Base {
                reference: "v5".into()
            }
        );
        assert_eq!(target.focus.as_deref(), Some("webview lifecycle"));
        assert!(new.brief.contains("against v5"), "{}", new.brief);
        // The configured Codex model applies when the request names none.
        assert_eq!(
            new.child,
            DelegationChild::CodexReview {
                thread_id: None,
                model: Some("gpt-6-astra".into())
            }
        );

        let mut commit = review_request(ReviewTargetKind::Commit);
        commit.commit = Some("4d018aa".into());
        commit.model = Some("gpt-6-sol".into());
        let new = review_delegation(&commit, &caller(), Some("ignored"), None).unwrap();
        assert_eq!(
            new.child,
            DelegationChild::CodexReview {
                thread_id: None,
                model: Some("gpt-6-sol".into())
            }
        );

        let errors = [
            review_request(ReviewTargetKind::Base),
            review_request(ReviewTargetKind::Commit),
            ReviewDelegationRequest {
                base_ref: Some("--upload-pack=x".into()),
                ..review_request(ReviewTargetKind::Base)
            },
            ReviewDelegationRequest {
                commit: Some("HEAD~1".into()),
                ..review_request(ReviewTargetKind::Commit)
            },
            ReviewDelegationRequest {
                commit: Some("abcdef1".into()),
                ..review_request(ReviewTargetKind::Uncommitted)
            },
            ReviewDelegationRequest {
                reviewer: Some("claude".into()),
                ..review_request(ReviewTargetKind::Uncommitted)
            },
            ReviewDelegationRequest {
                model: Some("x; rm -rf".into()),
                ..review_request(ReviewTargetKind::Uncommitted)
            },
        ];
        for request in errors {
            assert!(
                review_delegation(&request, &caller(), None, None).is_err(),
                "{request:?}"
            );
        }
        let remote = CallerTab {
            remote: true,
            ..caller()
        };
        let error = review_delegation(
            &review_request(ReviewTargetKind::Uncommitted),
            &remote,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("remote"), "{error}");
    }

    #[test]
    fn consult_requests_need_a_brief() {
        let new = consult_delegation(
            &ConsultDelegationRequest {
                brief: "  Should we split main.rs?  ".into(),
                model: None,
            },
            &caller(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(new.kind, DelegationKind::Consult);
        assert_eq!(new.brief, "Should we split main.rs?");
        assert!(new.target.is_none());
        assert!(consult_delegation(
            &ConsultDelegationRequest {
                brief: "  ".into(),
                model: None
            },
            &caller(),
            None,
            None
        )
        .is_err());
        assert!(consult_delegation(
            &ConsultDelegationRequest {
                brief: "x".repeat(MAX_BRIEF_CHARS + 1),
                model: None
            },
            &caller(),
            None,
            None
        )
        .is_err());
    }

    fn delegation(id: &str, status: DelegationStatus) -> Delegation {
        let mut delegation = Delegation::new_requested(
            review_delegation(
                &review_request(ReviewTargetKind::Uncommitted),
                &caller(),
                None,
                None,
            )
            .unwrap(),
            "2026-10-08T10:00:00Z",
        );
        delegation.delegation_id = id.into();
        delegation.status = status;
        delegation
    }

    #[test]
    fn queued_runs_start_oldest_first_up_to_the_cap() {
        let delegations = vec![
            delegation("a", DelegationStatus::Running),
            delegation("b", DelegationStatus::Requested),
            delegation("c", DelegationStatus::Completed),
            delegation("d", DelegationStatus::Requested),
            delegation("e", DelegationStatus::Requested),
        ];
        let running: HashSet<String> = ["a".to_string()].into();
        assert_eq!(
            runs_to_start(&delegations, &running, 2),
            vec!["b".to_string()]
        );
        assert_eq!(
            runs_to_start(&delegations, &running, 3),
            vec!["b".to_string(), "d".to_string()]
        );
        // Zero counts as one slot, which "a" holds.
        assert!(runs_to_start(&delegations, &running, 0).is_empty());
        let none = HashSet::new();
        assert_eq!(runs_to_start(&delegations, &none, 0), vec!["b".to_string()]);
        // A requested delegation already being started is not started twice.
        let starting: HashSet<String> = ["b".to_string()].into();
        assert_eq!(
            runs_to_start(&delegations, &starting, 2),
            vec!["d".to_string()]
        );
    }

    #[test]
    fn list_filters_by_status_and_keeps_the_given_newest_first_order() {
        let newer = delegation("newer", DelegationStatus::Requested);
        let older = delegation(
            "older",
            DelegationStatus::Failed {
                message: "codex exited with code 1".into(),
            },
        );
        let list = delegation_list(&[&newer, &older], None).unwrap();
        let ids: Vec<&str> = list["delegations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["delegation_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["newer", "older"]);
        assert_eq!(list["delegations"][1]["error"], "codex exited with code 1");
        let failed = delegation_list(&[&newer, &older], Some("FAILED")).unwrap();
        assert_eq!(failed["delegations"].as_array().unwrap().len(), 1);
        assert!(delegation_list(&[&newer], Some("done")).is_err());
    }

    fn completed_review(findings: usize) -> Delegation {
        let mut delegation = delegation("d-7", DelegationStatus::Completed);
        delegation.result = Some(DelegationResult {
            handoff: None,
            findings: Some(ReviewFindings {
                verdict: ReviewVerdict::NeedsChanges,
                summary: "Two regressions".into(),
                findings: (1..=findings)
                    .map(|n| ReviewFinding {
                        id: format!("F{n}"),
                        severity: if n == 1 {
                            ReviewSeverity::P1
                        } else {
                            ReviewSeverity::P2
                        },
                        title: format!("Problem number {n}"),
                        body: "x".repeat(600),
                        file: Some(PathBuf::from("src/main.rs")),
                        line_start: Some(100 + n as u32),
                        line_end: Some(100 + n as u32),
                        confidence: Some(0.9),
                    })
                    .collect(),
                reviewer: Reviewer {
                    harness: "codex".into(),
                    model: None,
                    conversation_id: Some("t".into()),
                },
                structured: true,
            }),
            reviewed: Some(ReviewedState {
                head: "4d018aa0123456789".into(),
                dirty: true,
                target_description: "uncommitted changes".into(),
            }),
        });
        delegation
    }

    #[test]
    fn the_send_message_names_ids_counts_and_the_full_record_within_the_cap() {
        let review = completed_review(3);
        let message = compose_send_message(&review, &["F1".into(), "F3".into()]).unwrap();
        assert!(message.starts_with("Codex review d-7 of uncommitted changes at 4d018aa, dirty: verdict needs_changes, 2 selected of 3 findings (1 P1, 1 P2)."), "{message}");
        assert!(
            message.contains("\nF1 [P1] src/main.rs:101 Problem number 1\n"),
            "{message}"
        );
        assert!(
            message.contains("\nF3 [P2] src/main.rs:103 Problem number 3\n"),
            "{message}"
        );
        assert!(!message.contains("F2 [P2]"));
        assert!(message.ends_with("Full record: delegation_get d-7"));
        // Bodies are cut.
        assert!(!message.contains(&"x".repeat(FINDING_BODY_CHARS + 1)));

        // Many findings: capped at 8 KB, the omission named, the pointer kept.
        let big = completed_review(60);
        let all: Vec<String> = (1..=60).map(|n| format!("F{n}")).collect();
        let message = compose_send_message(&big, &all).unwrap();
        assert!(message.len() <= MAX_SEND_BYTES, "{}", message.len());
        assert!(
            message.contains("more selected findings omitted; read them with delegation_get d-7")
        );
        assert!(message.ends_with("Full record: delegation_get d-7"));
        assert!(message.contains("60 selected of 60 findings (1 P1, 59 P2)"));

        assert!(compose_send_message(&review, &[]).is_err());
        assert!(compose_send_message(&review, &["F9".into()]).is_err());
        assert!(compose_send_message(&delegation("r", DelegationStatus::Running), &[]).is_err());

        let clean = completed_review(0);
        let message = compose_send_message(&clean, &[]).unwrap();
        assert!(message.contains("no findings"), "{message}");
        assert!(message.contains("Two regressions"));
    }

    #[test]
    fn a_consult_message_carries_the_answer_and_is_cut_to_the_cap() {
        let mut consult = completed_review(0);
        consult.kind = DelegationKind::Consult;
        consult.result = Some(DelegationResult {
            handoff: Some(TaskHandoff {
                summary: "Split the module.".into(),
                decisions: vec!["Keep main.rs as the shell".into()],
                next_steps: vec!["Move the inbox".into()],
                blockers: Vec::new(),
                updated_by_session_id: None,
                updated_at: "t".into(),
            }),
            findings: None,
            reviewed: None,
        });
        let message = compose_send_message(&consult, &[]).unwrap();
        assert_eq!(
            message,
            "Codex consult d-7:\nSplit the module.\n\nDecisions:\n- Keep main.rs as the shell\n\nNext steps:\n- Move the inbox\nFull record: delegation_get d-7"
        );
        if let Some(handoff) = consult.result.as_mut().and_then(|r| r.handoff.as_mut()) {
            handoff.summary = "é".repeat(10_000);
        }
        let message = compose_send_message(&consult, &[]).unwrap();
        assert!(message.len() <= MAX_SEND_BYTES);
        assert!(message.contains("(cut; the full answer is in delegation_get)"));
        assert!(message.ends_with("Full record: delegation_get d-7"));
    }

    #[test]
    fn sends_hold_while_streaming_and_flush_one_per_turn_end() {
        let mut held = Vec::new();
        assert_eq!(decide_send(false, false, &held, "a"), SendDecision::SendNow);
        assert_eq!(decide_send(true, false, &held, "a"), SendDecision::Hold);
        held.push(HeldSend {
            delegation_id: "a".into(),
            message: "A".into(),
        });
        held.push(HeldSend {
            delegation_id: "b".into(),
            message: "B".into(),
        });
        assert_eq!(
            decide_send(true, false, &held, "a"),
            SendDecision::AlreadyHeld
        );
        assert_eq!(
            decide_send(false, true, &held, "c"),
            SendDecision::AlreadyDelivered
        );
        // A turn that is still going (a queued message restarted it) flushes nothing.
        assert_eq!(next_held_send(&mut held, true), None);
        assert_eq!(next_held_send(&mut held, false).unwrap().delegation_id, "a");
        assert_eq!(next_held_send(&mut held, false).unwrap().delegation_id, "b");
        assert_eq!(next_held_send(&mut held, false), None);
    }

    #[test]
    fn codex_runs_follow_the_delegation() {
        let review = delegation("r-1", DelegationStatus::Requested);
        let run = codex_run(&review, Path::new("/cfg")).unwrap();
        assert_eq!(run.cwd, PathBuf::from("/repo"));
        assert_eq!(run.log_path, PathBuf::from("/cfg/delegations/r-1.jsonl"));
        assert!(matches!(run.kind, CodexRunKind::Review(_)));
        let record = delegation_record(&review, Path::new("/cfg")).unwrap();
        assert_eq!(record["log_path"], "/cfg/delegations/r-1.jsonl");
        assert_eq!(record["status"]["state"], "requested");
        let rerun = rerun_delegation(&completed_review(1)).unwrap();
        assert_eq!(rerun.previous.as_deref(), Some("d-7"));
        assert_eq!(rerun.target, review.target);
    }

    fn worker_choice() -> WorkerChoice {
        crate::workers::resolve_worker(
            &[crate::workers::PresetRef {
                name: "Codex",
                command: "codex",
            }],
            None,
            None,
            None,
            &crate::workers::ModelPolicy::default(),
        )
        .unwrap()
    }

    fn handoff(summary: &str, blockers: &[&str]) -> TaskHandoff {
        TaskHandoff {
            summary: summary.to_string(),
            decisions: vec!["Kept the PNG path".to_string()],
            next_steps: vec!["Review the branch".to_string()],
            blockers: blockers.iter().map(|item| item.to_string()).collect(),
            updated_by_session_id: Some("session-1".to_string()),
            updated_at: "2026-10-08T10:05:00Z".to_string(),
        }
    }

    fn session(id: &str) -> crate::tasks::TaskSessionRecord {
        crate::tasks::TaskSessionRecord {
            task_session_id: id.to_string(),
            label: "Codex".to_string(),
            harness: None,
            conversation: None,
            objective_delivery: crate::tasks::ObjectiveDeliveryState::Delivered,
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn worker_delegations_carry_the_choice_and_refuse_remote_or_empty_requests() {
        let choice = worker_choice();
        let new =
            worker_delegation(&caller(), "task-1", " Fix export ", "Do it.", &choice).unwrap();
        assert_eq!(new.kind, DelegationKind::Implement);
        assert_eq!(new.brief, "Fix export\n\nDo it.");
        assert_eq!(new.parent.session_uid, "tab-uid");
        assert_eq!(
            new.child,
            DelegationChild::TaskSession {
                task_id: "task-1".into(),
                task_session_id: None,
                conversation: None,
                preset_name: Some("Codex".into()),
                model: Some("gpt-6-luna".into()),
                role: Some(crate::workers::WorkerRole::Scoped),
            }
        );
        let mut remote = caller();
        remote.remote = true;
        assert!(worker_delegation(&remote, "t", "a", "b", &choice)
            .unwrap_err()
            .contains("remote"));
        assert!(worker_delegation(&caller(), "t", "a", " ", &choice).is_err());
    }

    #[test]
    fn the_report_back_section_names_the_delegation_and_the_statuses() {
        let section = report_back_section("d-42", "task-9");
        assert!(section.starts_with("Report back:\n"), "{section}");
        for needle in [
            "GitTerm delegation d-42",
            "task_update_handoff tool for task task-9",
            "status \"progress\"",
            "status \"done\"",
            "status \"blocked\"",
            "summary, decisions, next_steps and blockers",
            "Do not merge or push unless the objective says so.",
        ] {
            assert!(section.contains(needle), "{needle} missing from {section}");
        }
    }

    #[test]
    fn only_the_workers_own_session_reports_done_or_blocked() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(temp.path().join(crate::tasks::TASKS_FILE_NAME)).unwrap();
        store.insert(test_task("task-1", temp.path())).unwrap();
        // No delegation: done is just a task handoff.
        let none = record_handoff(
            &mut store,
            "task-1",
            handoff("Plain task", &[]),
            HandoffStatus::Done,
            None,
            "t1",
        )
        .unwrap();
        assert_eq!(none, None);

        let delegation = Delegation::new_requested(
            worker_delegation(&caller(), "task-1", "Fix", "Do it.", &worker_choice()).unwrap(),
            "t2",
        );
        let id = delegation.delegation_id.clone();
        store.insert_delegation(delegation).unwrap();
        store
            .upsert_session("task-1", session("session-1"), "t3")
            .unwrap();
        store
            .attach_worker_session(&id, "session-1", None, "t3")
            .unwrap();

        // The coordinator (or another task tab) cannot complete it, and the
        // refused call stores nothing.
        let error = record_handoff(
            &mut store,
            "task-1",
            handoff("I say it is done", &[]),
            HandoffStatus::Done,
            Some("session-2"),
            "t4",
        )
        .unwrap_err();
        assert!(
            error.contains("only the worker's own GitTerm tab"),
            "{error}"
        );
        assert_eq!(
            store
                .get("task-1")
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Plain task"
        );
        // Progress from anyone only updates the task handoff.
        assert_eq!(
            record_handoff(
                &mut store,
                "task-1",
                handoff("Halfway", &[]),
                HandoffStatus::Progress,
                None,
                "t5",
            )
            .unwrap(),
            None
        );
        assert_eq!(
            store.delegation(&id).unwrap().status,
            DelegationStatus::Running
        );

        let blocked = record_handoff(
            &mut store,
            "task-1",
            handoff("Need a key", &["No staging access"]),
            HandoffStatus::Blocked,
            Some("session-1"),
            "t6",
        )
        .unwrap();
        assert_eq!(blocked.as_deref(), Some(id.as_str()));
        assert_eq!(
            store.delegation(&id).unwrap().status,
            DelegationStatus::Blocked
        );
        let done = record_handoff(
            &mut store,
            "task-1",
            handoff("Export fixed, tests pass", &[]),
            HandoffStatus::Done,
            Some("session-1"),
            "t7",
        )
        .unwrap();
        assert_eq!(done.as_deref(), Some(id.as_str()));
        let completed = store.delegation(&id).unwrap();
        assert_eq!(completed.status, DelegationStatus::Completed);
        assert_eq!(
            completed
                .result
                .as_ref()
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Export fixed, tests pass"
        );
        // A later task handoff does not change the snapshot the parent got.
        record_handoff(
            &mut store,
            "task-1",
            handoff("Tidying up", &[]),
            HandoffStatus::Progress,
            Some("session-1"),
            "t8",
        )
        .unwrap();
        assert_eq!(
            store
                .delegation(&id)
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .handoff
                .as_ref()
                .unwrap()
                .summary,
            "Export fixed, tests pass"
        );
    }

    #[test]
    fn the_worker_card_state_mirrors_the_task_session() {
        use DelegationStatus as S;
        use WorkerTab as T;
        let running = Some(TaskLifecycle::Running);
        assert_eq!(
            worker_session_state(&S::Requested, Some(TaskLifecycle::Queued), T::Closed),
            "queued"
        );
        assert_eq!(
            worker_session_state(&S::Requested, Some(TaskLifecycle::Ready), T::Closed),
            "starting"
        );
        assert_eq!(
            worker_session_state(&S::Running, running, T::Working),
            "running"
        );
        assert_eq!(
            worker_session_state(
                &S::Running,
                Some(TaskLifecycle::WaitingForInput),
                T::AwaitingInput
            ),
            "waiting"
        );
        assert_eq!(
            worker_session_state(&S::Running, Some(TaskLifecycle::Stopped), T::Idle),
            "exited"
        );
        assert_eq!(
            worker_session_state(&S::Running, running, T::Closed),
            "exited"
        );
        assert_eq!(
            worker_session_state(&S::Running, Some(TaskLifecycle::Interrupted), T::Idle),
            "interrupted"
        );
        assert_eq!(
            worker_session_state(&S::Running, running, T::Idle),
            "running"
        );
        assert_eq!(
            worker_session_state(&S::Completed, running, T::AwaitingInput),
            "done"
        );
        assert_eq!(
            worker_session_state(&S::Blocked, running, T::Working),
            "blocked"
        );
        assert_eq!(
            worker_session_state(
                &S::Failed {
                    message: "x".into()
                },
                running,
                T::Closed
            ),
            "failed"
        );
    }

    #[test]
    fn worker_cards_and_sends_carry_the_task_and_the_handoff() {
        let temp = tempfile::tempdir().unwrap();
        let mut task = test_task("task-1", Path::new("/worktrees/tru-150"));
        task.handoff = Some(handoff("Wrote the failing test\nthen more", &[]));
        let mut worker = Delegation::new_requested(
            worker_delegation(
                &caller(),
                "task-1",
                "Fix export",
                "Do it.",
                &worker_choice(),
            )
            .unwrap(),
            "t1",
        );
        worker.delegation_id = "d-9".into();
        let info = worker_card_info(&worker, Some(&task), WorkerTab::Working);
        assert_eq!(info["preset"], "Codex");
        assert_eq!(info["model"], "gpt-6-luna");
        assert_eq!(info["role"], "scoped");
        assert_eq!(info["branch"], "tracey/tru-150-excalidraw-export");
        assert_eq!(info["worktree_path"], "/worktrees/tru-150");
        assert_eq!(info["issue_key"], "TRU-150");
        assert_eq!(info["session_state"], "running");
        assert_eq!(info["progress"], "Wrote the failing test");
        assert_eq!(info["tab_open"], true);
        let card = card_payload(&worker, None, false, Some(info), temp.path()).unwrap();
        assert_eq!(card["worker"]["title"], "Fix the Excalidraw export");
        assert!(card["delegation"].get("log_path").is_none());

        // Nothing to send while it runs; a blocked or done report sends.
        assert!(compose_send_message(&worker, &[]).is_err());
        worker.status = DelegationStatus::Blocked;
        worker.result = Some(worker_result(handoff("Need a key", &["No staging access"])));
        let blocked = compose_send_message(&worker, &[]).unwrap();
        assert!(
            blocked.starts_with(
                "Worker d-9 (Codex · gpt-6-luna) is blocked: Fix export. Task task-1;"
            ),
            "{blocked}"
        );
        assert!(
            blocked.contains("Blockers:\n- No staging access"),
            "{blocked}"
        );
        assert!(
            blocked.ends_with("Full record: delegation_get d-9"),
            "{blocked}"
        );
        worker.status = DelegationStatus::Completed;
        worker.result = Some(worker_result(handoff(&"x".repeat(20_000), &[])));
        let done = compose_send_message(&worker, &[]).unwrap();
        assert!(done.contains("reports done"), "{done}");
        assert!(done.len() <= MAX_SEND_BYTES);
        assert!(done.contains("(cut; the full answer is in delegation_get)"));
        let row = delegation_summary(&worker);
        assert_eq!(row["task_id"], "task-1");
        assert_eq!(row["kind"], "implement");
    }
}
