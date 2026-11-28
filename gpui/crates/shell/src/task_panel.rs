//! Iced's task rail (`view_task_rail`), the task Overview's verbs
//! (`view_task_detail`) and the task modals (`view_new_task_modal`,
//! `view_task_publish_modal`, `view_task_worktree_delete_modal`,
//! `view_task_tab_close_modal`), mounted over the preview task host's seeded
//! store (H5b B8a).
//!
//! Live commands enter core's TaskAuthority and execute on the task host.
//! Preview retains the same read surfaces and refuses every task write.
use crate::{task_host::GpuiTaskHost, theme::Palette};
use gitterm_core::{
    preview::service::{ServiceContext, classify_task_action},
    session::{self, Target},
    task_commands::{
        SessionLaunch, TaskCreate, TaskCreateOrigin, TaskLaunchOrigin, TaskMutation,
        TaskMutationAction, TaskMutationCommand, TaskMutationFacts, TaskPublish,
    },
    task_host::TaskHost,
    task_worktree::{
        CleanupContext, inspect_cleanup, pr_base_branch, suggested_task_branch,
        suggested_task_worktree_path,
    },
    tasks::{
        CleanupInspection, CleanupRisk, HarnessConversationBackend, HarnessKind, StoppingBoundary,
        TaskCreatorKind, TaskLifecycle, TaskRecord, TaskStore, TaskWorktreeState,
        VerificationState, WorkspaceLocationIdentity, task_handoff_prompt,
    },
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable as _, IndexPath, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectState},
};
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "task_panel_tests.rs"]
mod tests;

/// Iced's `task-<8 hex>` shape. Shown in the form only; the preview never
/// stores it.
fn fresh_task_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!("task-{nanos:08x}")
}

/// Iced's `CUSTOM_TASK_BASE`.
const CUSTOM_BASE: &str = "Custom ref…";

/// Iced `task_base_choices`: `develop`, `main`, the current branch, the
/// rest by name, then `HEAD` and `Custom ref…`; with the preferred base.
pub(crate) fn base_choices(branches: &[(String, bool)]) -> (Vec<String>, String) {
    let current = branches
        .iter()
        .find(|(_, current)| *current)
        .map(|(name, _)| name.as_str());
    let has = |name: &str| branches.iter().any(|(branch, _)| branch == name);
    let preferred = if has("develop") {
        "develop"
    } else if has("main") {
        "main"
    } else {
        current.unwrap_or("HEAD")
    };
    let mut choices: Vec<String> = ["develop", "main"]
        .into_iter()
        .filter(|name| has(name))
        .map(str::to_owned)
        .collect();
    if let Some(current) = current
        && !choices.iter().any(|choice| choice == current)
    {
        choices.push(current.to_owned());
    }
    let mut remaining: Vec<String> = branches
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| !choices.contains(name))
        .collect();
    remaining.sort_by_key(|name| name.to_lowercase());
    choices.extend(remaining);
    choices.extend(["HEAD".to_owned(), CUSTOM_BASE.to_owned()]);
    (choices, preferred.to_owned())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RailFilter {
    #[default]
    All,
    NeedsYou,
    Archived,
}

/// The workspace side of Iced's `task_belongs_to_workspace`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Location {
    /// No saved location (a legacy remote workspace): no task matches.
    #[default]
    None,
    Local {
        dir: PathBuf,
        root: PathBuf,
    },
    Remote {
        remote_id: String,
        root: String,
    },
}
impl Location {
    pub(crate) fn local(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self::Local {
            root: dir.clone(),
            dir,
        }
    }
    fn dir(&self) -> Option<&Path> {
        match self {
            Self::Local { dir, .. } => Some(dir),
            _ => None,
        }
    }
}

/// What the shell knows that the store does not, read at each render.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inputs {
    pub workspace_name: String,
    /// Whether a workspace is active at all (Iced `active_workspace()`).
    pub has_workspace: bool,
    pub location: Location,
    /// The active session's repository, as Iced's active tab `repo_path`;
    /// `None` when the workspace has no active session (Iced `active_tab()`).
    pub active_repo: Option<PathBuf>,
    pub selected: Option<String>,
    pub general_sessions: usize,
    /// Iced's `sidebar_panel_width() <= 160`.
    pub compact: bool,
    /// The open task sessions in every workspace, in workspace then tab order
    /// (Iced `first_open_task_tab`).
    pub open_sessions: Vec<Target>,
    /// Each open session's tab title, by `session_uid`.
    pub titles: std::collections::HashMap<String, String>,
    /// Iced's `agent_presets`, in order.
    pub presets: Vec<gitterm_core::settings::AgentPreset>,
    /// The local chat tabs, by provider conversation id (Iced `find_chat_tab`).
    pub open_chats: Vec<OpenChat>,
    pub conversations: Vec<gitterm_core::task_commands::TaskConversationFact>,
}

/// An open chat tab and the provider conversation it carries.
#[derive(Clone, Debug)]
pub(crate) struct OpenChat {
    pub chat_id: String,
    pub target: Target,
}
impl Inputs {
    fn open_chat(&self, chat_id: &str) -> Option<&Target> {
        self.open_chats
            .iter()
            .find(|chat| chat.chat_id == chat_id)
            .map(|chat| &chat.target)
    }
    /// Iced's `Start with {first preset}`; `agent` when none is configured.
    fn default_agent(&self) -> &str {
        self.presets
            .first()
            .map_or("agent", |preset| preset.name.as_str())
    }
    pub(crate) fn open_session(&self, task_id: &str) -> Option<&Target> {
        self.open_sessions
            .iter()
            .find(|target| target.task_id.as_deref() == Some(task_id))
    }
    /// Iced `task_tab_indices`: the task's open sessions, in tab order.
    fn task_sessions<'a>(&'a self, task_id: &'a str) -> impl Iterator<Item = &'a Target> + 'a {
        self.open_sessions
            .iter()
            .filter(move |target| target.task_id.as_deref() == Some(task_id))
    }
}

/// Iced `task_state_copy`: the open task sessions decide a Ready task's copy.
pub(crate) fn state_line(task: &TaskRecord, inputs: &Inputs) -> String {
    let open: Vec<_> = inputs
        .open_sessions
        .iter()
        .filter(|target| target.task_id.as_deref() == Some(task.task_id.as_str()))
        .filter_map(|target| target.task_session_id.as_deref())
        .collect();
    crate::task_switcher::state_copy(task, &open, false)
}

/// The preview host's store and services; `None` when the host did not start.
pub(crate) fn host(cx: &App) -> Option<&TaskHost> {
    cx.try_global::<GpuiTaskHost>()
        .map(|host| host.0.read(cx).host())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize().unwrap_or_else(|_| a.to_owned())
            == b.canonicalize().unwrap_or_else(|_| b.to_owned())
}

/// Iced `task_belongs_to_workspace`.
pub(crate) fn in_workspace(task: &TaskRecord, location: &Location) -> bool {
    match (&task.workspace.location, location) {
        (WorkspaceLocationIdentity::Local { directory }, Location::Local { dir, root }) => {
            same_dir(directory, root) || same_dir(directory, dir)
        }
        (
            WorkspaceLocationIdentity::RemoteAgent {
                remote_id: task_remote,
                root: task_root,
            },
            Location::Remote { remote_id, root },
        ) => task_remote == remote_id && task_root == root,
        _ => false,
    }
}

fn needs_attention(task: &TaskRecord) -> bool {
    task.attention.reason.is_some() || task.attention.unread
}

/// Iced `view_task_rail`'s counts and rows for one workspace and filter.
pub(crate) struct Rail<'a> {
    pub active: usize,
    pub needs_you: usize,
    pub failed: Vec<&'a TaskRecord>,
    pub finished: Vec<&'a TaskRecord>,
    pub rows: Vec<&'a TaskRecord>,
}

pub(crate) fn rail<'a>(store: &'a TaskStore, location: &Location, filter: RailFilter) -> Rail<'a> {
    let tasks: Vec<_> = store
        .tasks()
        .iter()
        .filter(|task| in_workspace(task, location))
        .collect();
    let live = |task: &&TaskRecord| task.lifecycle != TaskLifecycle::Archived;
    let mut rows: Vec<_> = tasks
        .iter()
        .copied()
        .filter(|task| match filter {
            RailFilter::All => live(task),
            RailFilter::NeedsYou => live(task) && needs_attention(task),
            RailFilter::Archived => !live(task),
        })
        .collect();
    rows.sort_by(|a, b| {
        crate::tasks::sort_bucket(a)
            .cmp(&crate::tasks::sort_bucket(b))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    Rail {
        active: tasks.iter().filter(|task| live(task)).count(),
        needs_you: tasks
            .iter()
            .filter(|task| live(task) && needs_attention(task))
            .count(),
        failed: tasks
            .iter()
            .copied()
            .filter(|task| task.lifecycle == TaskLifecycle::Failed)
            .collect(),
        // Iced `finished_task_ids_for_workspace`.
        finished: tasks
            .iter()
            .copied()
            .filter(|task| {
                !task.lifecycle.is_active()
                    && live(task)
                    && store.task_worktree_missing(&task.task_id)
            })
            .collect(),
        rows,
    }
}

/// Iced's rail-row Archive: settled, not archived, not compact.
pub(crate) fn row_archivable(task: &TaskRecord, compact: bool) -> bool {
    !compact && task.lifecycle != TaskLifecycle::Archived && !task.lifecycle.is_active()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    /// `▸ Start with {agent} ▾`, or `+ Continue ▾` once a session is open;
    /// enabled while a session can be added.
    Start {
        continuing: bool,
        enabled: bool,
    },
    Resume,
    /// `Go to session` for the first open session that has an id.
    GoToSession(String),
    ReviewChanges,
    PushBranch,
    OpenDraftPr,
    Stop,
    CancelQueue,
    Archive {
        enabled: bool,
    },
}

/// Iced `view_task_detail`'s first verb: it names the first agent preset.
pub(crate) fn start_label(continuing: bool, agent: &str) -> String {
    if continuing {
        "+ Continue ▾".to_owned()
    } else {
        format!("▸ Start with {agent} ▾")
    }
}

/// Iced `view_task_detail`'s state verbs in its order. Publishing is never
/// busy in the preview.
pub(crate) fn verbs(task: &TaskRecord, inputs: &Inputs) -> Vec<Verb> {
    let first = inputs.open_session(&task.task_id);
    let can_add_session = task.worktree.state == TaskWorktreeState::Ready
        && task.worktree.path.is_some()
        && task.lifecycle != TaskLifecycle::Archived;
    let mut verbs = vec![Verb::Start {
        continuing: first.is_some(),
        enabled: can_add_session,
    }];
    if can_add_session
        && matches!(
            task.lifecycle,
            TaskLifecycle::Stopped | TaskLifecycle::Interrupted
        )
    {
        verbs.push(Verb::Resume);
    }
    if let Some(session) = first.and_then(|target| target.task_session_id.clone()) {
        verbs.push(Verb::GoToSession(session));
    }
    if can_add_session {
        verbs.push(Verb::ReviewChanges);
        if matches!(
            task.workspace.location,
            WorkspaceLocationIdentity::Local { .. }
        ) {
            verbs.push(Verb::PushBranch);
            if task.delivery.pr_url.is_none() {
                verbs.push(Verb::OpenDraftPr);
            }
        }
    }
    if task.lifecycle.is_active() && first.is_some() {
        verbs.push(Verb::Stop);
    }
    if task.lifecycle == TaskLifecycle::Queued {
        verbs.push(Verb::CancelQueue);
    }
    verbs.push(Verb::Archive {
        enabled: !task.lifecycle.is_active() && task.lifecycle != TaskLifecycle::Archived,
    });
    verbs
}

/// Iced `friendly_timestamp`.
fn friendly_timestamp(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|stamp| {
            let local = stamp.with_timezone(&chrono::Local);
            if local.date_naive() == chrono::Local::now().date_naive() {
                format!("today {}", local.format("%H:%M"))
            } else {
                local.format("%b %-d %H:%M").to_string()
            }
        })
        .unwrap_or_else(|| rfc3339.to_owned())
}

/// Iced `TASK_PROGRESS_STALE_SECS`.
const PROGRESS_STALE_SECS: i64 = 120;

/// Iced `view_task_detail`'s detail rows. The preview captures no live
/// progress and probes no PR, so stored values decide; `WAITING ON` names
/// tasks with a working session, which the preview never hosts.
pub(crate) fn detail_rows(task: &TaskRecord, inputs: &Inputs) -> Vec<(&'static str, String)> {
    let mut rows = vec![("STATE", state_line(task, inputs))];
    let delivery = &task.delivery;
    if let (Some(url), Some(number)) = (&delivery.pr_url, delivery.pr_number) {
        rows.push(("DELIVERY", format!("PR #{number} · {url}")));
    } else if let Some(commit) = &delivery.commit_sha {
        let short: String = commit.chars().take(10).collect();
        rows.push(("DELIVERY", format!("{short} · no PR")));
    }
    let worktree = task.worktree.path.as_ref().map_or_else(
        || "Not prepared".to_owned(),
        |path| match task.worktree.state {
            TaskWorktreeState::Missing => format!("{} (missing)", path.display()),
            _ => path.display().to_string(),
        },
    );
    let creator = match &task.created_by {
        None => "Created manually".to_owned(),
        Some(creator) => {
            let when = friendly_timestamp(&creator.created_at);
            match creator.kind {
                TaskCreatorKind::Manual => format!("Created manually · {when}"),
                TaskCreatorKind::Coordinator => creator.harness_label.as_ref().map_or_else(
                    || format!("Coordinator session · {when}"),
                    |harness| format!("{harness} coordinator · {when}"),
                ),
            }
        }
    };
    rows.extend([
        (
            "ISSUE",
            task.issue
                .as_ref()
                .map_or_else(|| "None".to_owned(), |issue| format!("{} ↗", issue.key)),
        ),
        (
            "BASE",
            format!(
                "{} @ {}",
                task.base.reference,
                task.base.commit.chars().take(10).collect::<String>()
            ),
        ),
        ("BRANCH", task.branch.clone()),
        ("WORKTREE", worktree),
        ("CREATED BY", creator),
        (
            "STOP WHEN",
            boundary_label(task.stopping_boundary).to_owned(),
        ),
    ]);
    if let Some(error) = &task.last_error {
        rows.push(("LAST ERROR", error.clone()));
    }
    if let Some(harness) = &task.harness {
        rows.push((
            "LAST AGENT",
            match &harness.kind {
                HarnessKind::TerminalPreset { preset_name } => preset_name.clone(),
                HarnessKind::NativeClaude => "Native Claude".to_owned(),
                HarnessKind::NativePi => "Native Pi".to_owned(),
            },
        ));
    }
    if task.lifecycle.is_active() {
        let progress = task.progress.as_ref();
        let phase = progress
            .and_then(|progress| progress.phase.clone())
            .unwrap_or_else(|| {
                match task.lifecycle {
                    TaskLifecycle::Preparing => "Preparing",
                    TaskLifecycle::Queued => "Queued",
                    TaskLifecycle::Running => "Working",
                    TaskLifecycle::WaitingForInput => "Waiting for input",
                    _ => "Active",
                }
                .to_owned()
            });
        let latest = progress
            .and_then(|progress| progress.last_update_line.clone())
            .unwrap_or_else(|| friendly_timestamp(&task.updated_at));
        let now = chrono::Utc::now();
        let since = |raw: &str| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .ok()
                .map(|stamp| now.signed_duration_since(stamp.with_timezone(&chrono::Utc)))
        };
        let activity = match progress
            .and_then(|progress| progress.last_activity_at.as_deref())
            .and_then(since)
        {
            Some(elapsed) => {
                let secs = elapsed.num_seconds().max(0);
                let friendly =
                    friendly_timestamp(&(now - chrono::Duration::seconds(secs)).to_rfc3339());
                if secs >= PROGRESS_STALE_SECS {
                    format!("no activity since {friendly}")
                } else {
                    friendly
                }
            }
            None => "Not observed yet".to_owned(),
        };
        rows.extend([
            ("PHASE", phase),
            ("LATEST UPDATE", latest),
            ("LAST ACTIVITY", activity),
        ]);
        if let Some(elapsed) = task
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.state.is_active())
            .and_then(|attempt| since(&attempt.started_at))
        {
            rows.push(("ELAPSED", format!("{}m", elapsed.num_minutes().max(0))));
        }
        rows.push((
            "CHANGED FILES",
            if task.changes.checked_at.is_some() {
                format!(
                    "{} changed · {} staged",
                    task.changes.changed, task.changes.staged
                )
            } else {
                "Not checked yet".to_owned()
            },
        ));
        let verification = match task.verification.state {
            VerificationState::Unknown => "Unknown",
            VerificationState::NotRun => "Not run",
            VerificationState::Running => "Running",
            VerificationState::Passed => "Passed",
            VerificationState::Failed => "Failed",
        };
        rows.push((
            "VERIFICATION",
            match &task.verification.command {
                Some(command) => format!("{verification} · {command}"),
                None => verification.to_owned(),
            },
        ));
    }
    rows
}

/// Iced shows `Delete worktree…` under TASK SETTINGS whenever a path is recorded.
pub(crate) fn delete_opener(task: &TaskRecord) -> bool {
    task.worktree.path.is_some()
}

/// Iced's tab-close rule: a task session that is still running asks first.
/// A session the task host launched is running while it is observably live
/// (Iced's `task_live_state`); for any other the preview hosts no process,
/// so "running" is the record's lifecycle.
pub(crate) fn close_needs_confirm(
    current: &Target,
    requested: &Target,
    last: bool,
    cx: &App,
) -> bool {
    let launched = requested.task_session_id.as_deref().and_then(|session| {
        cx.try_global::<GpuiTaskHost>()
            .and_then(|host| host.0.read(cx).launched_live(session))
    });
    let task_live = launched.unwrap_or_else(|| {
        requested.task_id.as_deref().is_some_and(|id| {
            host(cx)
                .and_then(|host| host.store().as_ref())
                .and_then(|store| store.get(id))
                .is_some_and(|task| task.lifecycle.is_active())
        })
    });
    let state = session::State {
        target: current.clone(),
        control: session::TurnControl::Terminal,
        task_live,
        last_tab: last,
    };
    matches!(
        state.reduce(session::Intent::Close {
            target: requested.clone(),
            confirmed: false,
        }),
        session::Effect::ConfirmClose(_)
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Write {
    ArchiveFailed,
    ArchiveFinished,
    Archive(String),
    Publish {
        task_id: String,
        action: TaskPublish,
    },
    PublishConfirm,
    Stop(String),
    CancelQueue(String),
    DeleteConfirm,
    CreateSubmit,
    StopAndClose,
    /// The task picker's choice (Iced `TaskChildLaunchPreset` / `Terminal`).
    Launch {
        task_id: String,
        preset: Option<usize>,
    },
    /// Iced `TaskResume` with a recorded conversation.
    Resume(String),
    /// A history row's `Resume` / `Reopen` (Iced `TaskResumeConversation`).
    ResumeConversation {
        task_id: String,
        chat_id: String,
    },
    /// A history row's `Open`, or `Resume` of an open conversation.
    OpenConversation {
        task_id: String,
        chat_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskPanelOutcome {
    Select(String),
    Back,
    Review(String),
    Notice(String),
    /// Iced `TaskChildAdd`: the task's session picker.
    Pick(String),
    /// Iced `TaskFocusSession`.
    FocusSession(String),
    /// Focus the open tab (by `session_uid`) carrying a conversation.
    FocusConversation(String),
}

/// Iced `TaskResume` resumes the newest recorded conversation.
fn latest_conversation(task: &TaskRecord) -> Option<&str> {
    task.sessions.iter().rev().find_map(|session| {
        session
            .conversation
            .as_ref()
            .map(|conversation| conversation.session_id.as_str())
    })
}

/// Iced `resume_task_conversation`'s checks before it launches.
fn worktree_check(task: &TaskRecord) -> Result<(), String> {
    match &task.worktree.path {
        None => Err(format!("Task {} has no prepared worktree", task.title)),
        Some(path) if !path.is_dir() => Err(format!(
            "Task {} worktree is gone at {}",
            task.title,
            path.display()
        )),
        Some(_) => Ok(()),
    }
}

/// Iced `resume_task_conversation`: the task must link the conversation.
fn link_check(task: &TaskRecord, chat_id: &str) -> Result<(), String> {
    let linked = task.sessions.iter().any(|session| {
        session
            .conversation
            .as_ref()
            .is_some_and(|conversation| conversation.session_id == chat_id)
    });
    if linked {
        Ok(())
    } else {
        Err(format!(
            "Task {} no longer links conversation {chat_id}",
            task.title
        ))
    }
}

/// Resuming a conversation that is not open is a launch.
fn resume_launch(task: &TaskRecord, chat_id: &str) -> Result<TaskMutationAction, String> {
    link_check(task, chat_id)?;
    worktree_check(task)?;
    Ok(launch(&task.task_id, None))
}

/// Resuming an open conversation focuses its tab (Iced
/// `resume_task_conversation`, the `find_chat_tab` branch), after the same
/// checks, in Iced's order.
fn focus_check(
    store: &TaskStore,
    task: &TaskRecord,
    chat_id: &str,
    tab: &Target,
) -> Result<(), String> {
    link_check(task, chat_id)?;
    if store.worker_session_is_final(&task.task_id, tab.task_session_id.as_deref()) {
        return Err(
            "This finished worker is closing. Reopen its conversation after its tab closes.".into(),
        );
    }
    worktree_check(task)?;
    match &tab.task_id {
        Some(other) if *other != task.task_id => Err(format!(
            "Conversation {chat_id} is already attached to another task"
        )),
        _ => Ok(()),
    }
}

/// Iced `preset_conversation_backend`.
fn preset_backend(
    preset: &gitterm_core::settings::AgentPreset,
) -> Option<HarnessConversationBackend> {
    let name = preset.name.to_lowercase();
    let executable = preset
        .command
        .split_whitespace()
        .next()
        .and_then(|word| Path::new(word.trim_matches(['\'', '"'])).file_name())
        .map(|word| word.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("claude") || executable == "claude" {
        Some(HarnessConversationBackend::Claude)
    } else if name.contains("codex") || executable == "codex" {
        Some(HarnessConversationBackend::Codex)
    } else if name == "pi" || executable == "pi" {
        Some(HarnessConversationBackend::Pi)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryState {
    Open,
    Resume,
    Reopen,
    TranscriptKept,
}
impl HistoryState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Resume => "Resume",
            Self::Reopen => "Reopen",
            Self::TranscriptKept => "Transcript kept",
        }
    }
}

pub(crate) struct HistoryRow {
    pub chat_id: String,
    pub badge: String,
    pub title: String,
    pub short_id: String,
    pub state: HistoryState,
}

/// Iced `view_task_detail`'s CONVERSATION HISTORY rows: each session with a
/// recorded conversation, in record order.
pub(crate) fn history(task: &TaskRecord, store: &TaskStore, inputs: &Inputs) -> Vec<HistoryRow> {
    let worktree_ready = task.worktree.path.is_some();
    task.sessions
        .iter()
        .filter_map(|session| {
            let conversation = session.conversation.as_ref()?;
            let backend = match conversation.backend {
                HarnessConversationBackend::Claude => "Claude",
                HarnessConversationBackend::Codex => "Codex",
                HarnessConversationBackend::Pi => "Pi",
            };
            // Iced `backend_badge`: the preset's icon, else the backend glyph.
            let badge = inputs
                .presets
                .iter()
                .find(|preset| preset_backend(preset) == Some(conversation.backend))
                .map(|preset| {
                    if preset.icon.is_empty() {
                        preset.name.chars().next().unwrap_or('?').to_string()
                    } else {
                        preset.icon.clone()
                    }
                })
                .unwrap_or_else(|| {
                    match conversation.backend {
                        HarnessConversationBackend::Claude => "✦",
                        HarnessConversationBackend::Codex => "⌁",
                        HarnessConversationBackend::Pi => "π",
                    }
                    .to_owned()
                });
            let state = if inputs.open_chat(&conversation.session_id).is_some() {
                HistoryState::Open
            } else if worktree_ready {
                if store.worker_session_is_final(&task.task_id, Some(&session.task_session_id)) {
                    HistoryState::Reopen
                } else {
                    HistoryState::Resume
                }
            } else {
                HistoryState::TranscriptKept
            };
            Some(HistoryRow {
                chat_id: conversation.session_id.clone(),
                badge,
                title: format!("{backend} · {}", session.label),
                short_id: conversation.session_id.chars().take(8).collect(),
                state,
            })
        })
        .collect()
}

fn launch(task_id: &str, preset_index: Option<usize>) -> TaskMutationAction {
    TaskMutationAction::LaunchSession {
        request: SessionLaunch {
            task_id: task_id.to_owned(),
            // The picked preset travels as `preset_index`.
            preset_name: None,
            origin: TaskLaunchOrigin::User,
            user_model_choice: false,
        },
        preset_index,
    }
}

pub(crate) struct CreateForm {
    pub task_id: String,
    workspace_name: String,
    repository_path: PathBuf,
    pub title: Entity<InputState>,
    pub objective: Entity<InputState>,
    pub issue: Entity<InputState>,
    pub base: Entity<SelectState<Vec<String>>>,
    pub custom_base: Entity<InputState>,
    pub base_custom: bool,
    /// Iced `base_touched`: a picked base survives the branch list loading.
    pub base_touched: bool,
    /// Iced's `Refreshing local branches…` until the read returns.
    pub branches_loading: bool,
    pub stopping_boundary: StoppingBoundary,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
    _branches: gpui::Task<()>,
}

pub(crate) enum Modal {
    Create(Box<CreateForm>),
    Publish {
        task_id: String,
        action: TaskPublish,
        error: Option<String>,
    },
    Delete {
        task_id: String,
        path: PathBuf,
        /// Iced `TaskWorktreeDeletePrompt::inspection`: `None` while the
        /// inspection runs.
        inspection: Option<Result<CleanupInspection, String>>,
        error: Option<String>,
        _inspect: gpui::Task<()>,
    },
    StopAndClose {
        target: Target,
        error: Option<String>,
    },
}

#[derive(Default)]
pub(crate) struct TaskPanel {
    pub filter: RailFilter,
    pub modal: Option<Modal>,
    /// The latest refusal, where Iced shows `task_ui_error`.
    pub notice: Option<String>,
    in_flight: std::collections::HashSet<(String, &'static str)>,
}
impl EventEmitter<TaskPanelOutcome> for TaskPanel {}

/// UI writes remain read-only even on a fixture-capable Preview host.
fn refuse(_services: &ServiceContext, action: &TaskMutationAction) -> String {
    gitterm_core::preview::service::preview_notice(classify_task_action(action).label)
}

impl TaskPanel {
    pub(crate) fn task_stopping(&self, task_id: &str) -> bool {
        self.in_flight.contains(&(task_id.to_owned(), "stop"))
    }

    fn modal_operation(&self) -> Option<(String, &'static str)> {
        match &self.modal {
            Some(Modal::Create(form)) => Some((form.task_id.clone(), "create")),
            Some(Modal::Publish { task_id, .. }) => Some((task_id.clone(), "publish")),
            Some(Modal::Delete { task_id, .. }) => Some((task_id.clone(), "delete")),
            Some(Modal::StopAndClose { target, .. }) => Some((target.session_uid.clone(), "close")),
            _ => None,
        }
    }

    fn busy(&self) -> bool {
        self.modal_operation()
            .is_some_and(|key| self.in_flight.contains(&key))
    }

    pub(crate) fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    pub(crate) fn select_filter(&mut self, filter: RailFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        if !self.busy() {
            self.modal = None;
        }
        cx.notify();
    }

    fn show_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        match &mut self.modal {
            Some(Modal::Create(form)) => form.error = Some(notice.clone()),
            Some(
                Modal::Publish { error, .. }
                | Modal::Delete { error, .. }
                | Modal::StopAndClose { error, .. },
            ) => *error = Some(notice.clone()),
            None => {}
        }
        self.notice = Some(notice.clone());
        cx.emit(TaskPanelOutcome::Notice(notice));
        cx.notify();
    }

    /// Iced `TaskCreateOpen`: the same preconditions, then an empty form.
    pub(crate) fn open_create(
        &mut self,
        inputs: &Inputs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = None;
        if host(cx).and_then(|host| host.store().as_ref()).is_none() {
            self.notice = Some("The task store is unavailable".into());
            cx.notify();
            return;
        }
        // Iced `TaskCreateOpen`, in order: workspace, local, active session,
        // Git repository.
        if !inputs.has_workspace {
            self.notice = Some("Open a local workspace before creating a task".into());
            cx.notify();
            return;
        }
        if inputs.location.dir().is_none() {
            self.notice =
                Some("Local task creation is available only from a workspace on this Mac".into());
            cx.notify();
            return;
        }
        let Some(session_repo) = inputs.active_repo.as_deref() else {
            self.notice = Some("Open a repository session before creating a task".into());
            cx.notify();
            return;
        };
        if !crate::pane::git_view::is_git_repo(session_repo) {
            self.notice = Some(format!(
                "{} is not a Git repository",
                session_repo.display()
            ));
            cx.notify();
            return;
        }
        // Creating a task from inside another task's worktree resolves the
        // owning repository, so the new task is a sibling and the base
        // never lands on the current task's branch.
        let dir = gitterm_core::task_host::canonical_task_workspace_directory(
            host(cx).and_then(|host| host.store().as_ref()),
            session_repo,
        );
        let mut input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let title = input("Short outcome", cx);
        let objective = input("What should the agent accomplish?", cx);
        let issue = input("TRU-105 (optional)", cx);
        let custom_base = input("branch, tag, remote ref, or SHA", cx);
        // Iced's choices before local branches load.
        let base = cx.new(|cx| {
            SelectState::new(
                vec!["HEAD".to_owned(), CUSTOM_BASE.to_owned()],
                Some(IndexPath::default().row(0)),
                window,
                cx,
            )
        });
        title.update(cx, |input, cx| input.focus(window, cx));
        let mut subscriptions = vec![];
        for field in [&title, &objective, &issue, &custom_base] {
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _, event, _, cx| match event {
                    InputEvent::Change => {
                        if let Some(Modal::Create(form)) = &mut this.modal {
                            form.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.write(Write::CreateSubmit, &Inputs::default(), cx)
                    }
                    _ => {}
                },
            ));
        }
        subscriptions.push(
            cx.subscribe(&base, |this, _, event: &SelectEvent<Vec<String>>, cx| {
                let SelectEvent::Confirm(choice) = event;
                if let Some(Modal::Create(form)) = &mut this.modal {
                    form.base_custom = choice.as_deref() == Some(CUSTOM_BASE);
                    form.base_touched = true;
                    form.error = None;
                }
                cx.notify();
            }),
        );
        // Iced reads the repository's local branches (a git read) when the
        // form opens; until then the choices are `HEAD` and `Custom ref…`.
        let read = dir.clone();
        let branches = cx.spawn_in(window, async move |panel, cx| {
            let branches = cx
                .background_executor()
                .spawn(async move { crate::pane::git_view::local_branches(&read) })
                .await;
            _ = panel.update_in(cx, |panel, window, cx| {
                panel.branches_loaded(branches, window, cx)
            });
        });
        self.modal = Some(Modal::Create(Box::new(CreateForm {
            task_id: fresh_task_id(),
            workspace_name: inputs.workspace_name.clone(),
            repository_path: dir,
            title,
            objective,
            issue,
            base,
            custom_base,
            base_custom: false,
            base_touched: false,
            branches_loading: true,
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            error: None,
            _subscriptions: subscriptions,
            _branches: branches,
        })));
        cx.notify();
    }

    /// Iced's worktree-snapshot arm for the open form: a repository's
    /// branches replace the choices (keeping a picked base, or moving it to
    /// `Custom ref…` when gone); a failed read keeps `HEAD` / `Custom ref…`.
    fn branches_loaded(
        &mut self,
        branches: Result<Vec<(String, bool)>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Create(form)) = &mut self.modal else {
            return;
        };
        form.branches_loading = false;
        let (choices, preferred) = match branches {
            Ok(branches) => base_choices(&branches),
            Err(_) => (
                vec!["HEAD".to_owned(), CUSTOM_BASE.to_owned()],
                "HEAD".to_owned(),
            ),
        };
        let picked = form.base.read(cx).selected_value().cloned();
        let select = if !form.base_touched {
            form.base_custom = false;
            Some(preferred)
        } else if !form.base_custom
            && !picked
                .as_ref()
                .is_some_and(|picked| choices.contains(picked))
        {
            form.base_custom = true;
            Some(CUSTOM_BASE.to_owned())
        } else {
            None
        };
        form.base.update(cx, |base, cx| {
            base.set_items(choices, window, cx);
            if let Some(select) = &select {
                base.set_selected_value(select, window, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn select_boundary(&mut self, boundary: StoppingBoundary, cx: &mut Context<Self>) {
        if let Some(Modal::Create(form)) = &mut self.modal {
            form.stopping_boundary = boundary;
            form.error = None;
            cx.notify();
        }
    }

    /// Iced `TaskWorktreeDeleteOpen`: the core inspection (read-only git, so
    /// the preview allows it) runs off the UI thread; until it returns the
    /// modal shows `Checking for unsaved work…`.
    pub(crate) fn open_delete(&mut self, task_id: &str, inputs: &Inputs, cx: &mut Context<Self>) {
        let Some(owner) = cx.try_global::<GpuiTaskHost>().map(|h| h.0.clone()) else {
            return;
        };
        let decided = owner.update(cx, |o, cx| {
            let mut facts = o.panel_facts(cx);
            facts
                .sessions
                .extend(inputs.open_sessions.iter().map(|target| session::State {
                    target: target.clone(),
                    control: session::TurnControl::Terminal,
                    task_live: true,
                    last_tab: false,
                }));
            o.host_mut()
                .reduce_mutation(
                    TaskMutationCommand {
                        reply: None,
                        mutation: TaskMutation::InspectDelete {
                            task_id: task_id.into(),
                        },
                    },
                    &facts,
                )
                .action
        });
        let inspect = match decided {
            Ok(action) => action,
            Err(e) => {
                self.show_notice(e, cx);
                return;
            }
        };
        let TaskMutationAction::InspectCleanup {
            repository,
            path,
            branch,
            base_commit,
            process_running,
            pull_request_url,
            ..
        } = &inspect
        else {
            return;
        };
        let (repository, path, branch, base_commit) = (
            repository.clone(),
            path.clone(),
            branch.clone(),
            base_commit.clone(),
        );
        let context = CleanupContext {
            process_running: *process_running,
            pull_request_url: pull_request_url.clone(),
        };
        let (inspection, job) = match owner.read(cx).host().services().task_action(&inspect, None) {
            Err(denied) => (Some(Err(denied.notice().to_owned())), gpui::Task::ready(())),
            Ok(()) => {
                let (id, worktree) = (task_id.to_owned(), path.clone());
                let job = cx.spawn(async move |panel, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            inspect_cleanup(&repository, &worktree, &branch, &base_commit, &context)
                                .map_err(|error| error.to_string())
                        })
                        .await;
                    _ = panel.update(cx, |panel, cx| {
                        if let Some(Modal::Delete {
                            task_id,
                            inspection,
                            ..
                        }) = &mut panel.modal
                            && *task_id == id
                        {
                            *inspection = Some(result);
                            cx.notify();
                        }
                    });
                });
                (None, job)
            }
        };
        self.modal = Some(Modal::Delete {
            task_id: task_id.to_owned(),
            path,
            inspection,
            error: None,
            _inspect: job,
        });
        cx.notify();
    }

    /// Iced `TaskTabClose` on a running task session.
    pub(crate) fn open_stop_and_close(&mut self, target: Target, cx: &mut Context<Self>) {
        self.modal = Some(Modal::StopAndClose {
            target,
            error: None,
        });
        cx.notify();
    }

    /// The core action Iced would run for `write`, or `None` where Iced does
    /// nothing (a vanished task, no failed tasks).
    fn action(
        &self,
        write: &Write,
        store: &TaskStore,
        inputs: &Inputs,
    ) -> Result<Option<TaskMutationAction>, String> {
        let archive = |tasks: Vec<&TaskRecord>| {
            tasks.first().map(|task| TaskMutationAction::ArchiveTask {
                task_id: task.task_id.clone(),
            })
        };
        let publish = |task_id: &str, action: TaskPublish, confirmed: bool| {
            // Iced `reduce_task_publish` policy (core `TaskMutation::Publish`).
            let task = store.get(task_id)?;
            if task.issue.is_none() && !confirmed {
                return Some(TaskMutationAction::ConfirmPublish {
                    task_id: task_id.to_owned(),
                    action,
                });
            }
            let (title, body) = task.draft_pr_copy();
            Some(TaskMutationAction::Publish {
                task_id: task_id.to_owned(),
                action,
                path: task.worktree.path.clone()?,
                branch: task.branch.clone(),
                base: pr_base_branch(&task.base.reference).to_owned(),
                title,
                body,
            })
        };
        Ok(match write {
            Write::ArchiveFailed => archive(rail(store, &inputs.location, self.filter).failed),
            Write::ArchiveFinished => archive(rail(store, &inputs.location, self.filter).finished),
            Write::Archive(task_id) => {
                store.get(task_id).map(|_| TaskMutationAction::ArchiveTask {
                    task_id: task_id.clone(),
                })
            }
            Write::Publish { task_id, action } => publish(task_id, *action, false),
            Write::PublishConfirm => match &self.modal {
                Some(Modal::Publish {
                    task_id, action, ..
                }) => publish(task_id, *action, true),
                _ => None,
            },
            // Iced `TaskStop` closes the task's tabs, then records Stopped.
            Write::Stop(task_id) => inputs
                .open_session(task_id)
                .map(|target| TaskMutationAction::Session(session::Effect::Close(target.clone()))),
            // Iced `TaskQueueCancel` records the task Ready again.
            Write::CancelQueue(task_id) => store.get(task_id).map(|task| {
                let mut record = task.clone();
                record.lifecycle = TaskLifecycle::Ready;
                TaskMutationAction::PersistPreparing {
                    record: Box::new(record),
                }
            }),
            // Iced `TaskWorktreeDeleteConfirm`: nothing until the inspection
            // succeeds, then the core `TaskMutation::DeleteWorktree` policy.
            Write::DeleteConfirm => match &self.modal {
                Some(Modal::Delete {
                    task_id,
                    path,
                    inspection: Some(Ok(inspection)),
                    ..
                }) => match store.get(task_id) {
                    None => return Err("Task is no longer in the task store".into()),
                    Some(_) if delete_blocked(inspection) => None,
                    Some(_) if !inspection.path_exists || !inspection.registered => {
                        Some(TaskMutationAction::ClearWorktree {
                            task_id: task_id.clone(),
                        })
                    }
                    Some(task) => match &task.workspace.location {
                        WorkspaceLocationIdentity::Local { directory } => {
                            Some(TaskMutationAction::RemoveWorktree {
                                task_id: task_id.clone(),
                                repository: directory.clone(),
                                path: path.clone(),
                            })
                        }
                        _ => return Err("Could not resolve the task's repository directory".into()),
                    },
                },
                _ => None,
            },
            // Decided by the core reducer in `write`.
            Write::CreateSubmit => None,
            Write::StopAndClose => match &self.modal {
                Some(Modal::StopAndClose { target, .. }) => Some(TaskMutationAction::Session(
                    session::Effect::Close(target.clone()),
                )),
                _ => None,
            },
            Write::Launch { task_id, preset } => {
                store.get(task_id).map(|_| launch(task_id, *preset))
            }
            // Iced `resume_task_conversation`'s worktree checks, then the
            // resume launch (its session-record refresh is a write too).
            Write::Resume(task_id) => match store.get(task_id) {
                Some(task) => match latest_conversation(task) {
                    Some(chat_id) => Some(resume_launch(task, chat_id)?),
                    None => None,
                },
                None => None,
            },
            Write::ResumeConversation { task_id, chat_id } => match store.get(task_id) {
                Some(task) => Some(resume_launch(task, chat_id)?),
                None => None,
            },
            // A UI action, decided in `write` before the launch policy.
            Write::OpenConversation { .. } => None,
        })
    }

    /// Runs Iced's decision for `write` up to the service check. A read
    /// (the no-issue publish confirmation) opens its modal; anything else
    /// shows the context's notice and changes nothing.
    pub(crate) fn write(&mut self, write: Write, inputs: &Inputs, cx: &mut Context<Self>) {
        if write == Write::CreateSubmit {
            self.submit_create(cx);
            return;
        }
        // Preserve the test host's existing ability to close a fixture session
        // it actually launched; ordinary Preview still owns no such process.
        if write == Write::StopAndClose
            && let Some(Modal::StopAndClose { target, .. }) = &self.modal
            && cx
                .try_global::<GpuiTaskHost>()
                .is_some_and(|h| h.0.read(cx).owns_fixture_terminal(target))
        {
            self.write_live(write, inputs, cx);
            return;
        }
        // Iced `resume_task_conversation` focuses the tab of an open conversation.
        let open = match &write {
            Write::OpenConversation { task_id, chat_id } => {
                Some((task_id.clone(), chat_id.clone()))
            }
            Write::Resume(task_id) => host(cx)
                .and_then(|host| host.store().as_ref())
                .and_then(|store| store.get(task_id))
                .and_then(latest_conversation)
                .map(|chat_id| (task_id.clone(), chat_id.to_owned())),
            _ => None,
        };
        if let Some((task_id, chat_id)) = open {
            match inputs.open_chat(&chat_id).cloned() {
                Some(tab) => self.focus_conversation(&task_id, &chat_id, &tab, cx),
                // The tab closed since the row drew: Iced resumes it.
                None if matches!(write, Write::OpenConversation { .. }) => {
                    self.write(Write::ResumeConversation { task_id, chat_id }, inputs, cx)
                }
                None => self.write_launch(write, inputs, cx),
            }
            return;
        }
        self.write_launch(write, inputs, cx)
    }

    fn focus_conversation(
        &mut self,
        task_id: &str,
        chat_id: &str,
        tab: &Target,
        cx: &mut Context<Self>,
    ) {
        let checked = host(cx)
            .and_then(|host| host.store().as_ref())
            .ok_or_else(|| "The task store is unavailable".to_owned())
            .and_then(|store| {
                let task = store
                    .get(task_id)
                    .ok_or_else(|| format!("Task {task_id} is not available"))?;
                focus_check(store, task, chat_id, tab)
            });
        match checked {
            Ok(()) => cx.emit(TaskPanelOutcome::FocusConversation(tab.session_uid.clone())),
            Err(error) => self.show_notice(error, cx),
        }
    }

    fn write_launch(&mut self, write: Write, inputs: &Inputs, cx: &mut Context<Self>) {
        if host(cx).is_some_and(|h| !h.services().is_preview()) {
            self.write_live(write, inputs, cx);
            return;
        }
        // Without a recorded conversation Iced's Resume opens the picker.
        if let Write::Resume(task_id) = &write
            && host(cx)
                .and_then(|host| host.store().as_ref())
                .and_then(|store| store.get(task_id))
                .is_some_and(|task| latest_conversation(task).is_none())
        {
            cx.emit(TaskPanelOutcome::Pick(task_id.clone()));
            return;
        }
        let Some(host) = host(cx) else {
            self.show_notice("The task store is unavailable".into(), cx);
            return;
        };
        let Some(store) = host.store().as_ref() else {
            self.show_notice("The task store is unavailable".into(), cx);
            return;
        };
        let action = match self.action(&write, store, inputs) {
            Ok(Some(action)) => action,
            Ok(None) => return,
            Err(error) => {
                self.show_notice(error, cx);
                return;
            }
        };
        if let TaskMutationAction::ConfirmPublish {
            task_id,
            action: publish,
        } = &action
            && host.services().task_action(&action, None).is_ok()
        {
            self.modal = Some(Modal::Publish {
                task_id: task_id.clone(),
                action: *publish,
                error: None,
            });
            cx.notify();
            return;
        }
        let notice = refuse(host.services(), &action);
        self.show_notice(notice, cx);
    }

    fn write_live(&mut self, write: Write, inputs: &Inputs, cx: &mut Context<Self>) {
        let Some(owner) = cx.try_global::<GpuiTaskHost>().map(|h| h.0.clone()) else {
            return;
        };
        let launch_task = match &write {
            Write::Launch { task_id, .. }
            | Write::Resume(task_id)
            | Write::ResumeConversation { task_id, .. } => Some(task_id),
            _ => None,
        };
        if launch_task.is_some_and(|id| self.task_stopping(id)) {
            return;
        }
        if let Write::Resume(task_id) = &write
            && owner
                .read(cx)
                .host()
                .store()
                .as_ref()
                .and_then(|s| s.get(task_id))
                .is_some_and(|t| latest_conversation(t).is_none())
        {
            cx.emit(TaskPanelOutcome::Pick(task_id.clone()));
            return;
        }
        if let Write::Launch { task_id, preset } = &write {
            match owner.update(cx, |o, cx| o.launch_from_ui(task_id, *preset, cx)) {
                Ok(_) => {
                    self.modal = None;
                    cx.emit(TaskPanelOutcome::Select(task_id.clone()));
                    cx.notify();
                }
                Err(e) => self.show_notice(e, cx),
            }
            return;
        }
        let mut mutations = Vec::new();
        match &write {
            Write::ArchiveFailed | Write::ArchiveFinished => {
                if let Some(store) = owner.read(cx).host().store().as_ref() {
                    // Iced batches every matching task in the workspace, regardless of rail filter.
                    for task in store
                        .tasks()
                        .iter()
                        .filter(|t| in_workspace(t, &inputs.location))
                    {
                        let matches = if write == Write::ArchiveFailed {
                            task.lifecycle == TaskLifecycle::Failed
                        } else {
                            !task.lifecycle.is_active()
                                && task.lifecycle != TaskLifecycle::Archived
                                && store.task_worktree_missing(&task.task_id)
                        };
                        if matches {
                            mutations.push(TaskMutation::Close {
                                task_id: task.task_id.clone(),
                            });
                        }
                    }
                }
            }
            Write::Archive(id) => mutations.push(TaskMutation::Close {
                task_id: id.clone(),
            }),
            Write::Stop(id) => mutations.push(TaskMutation::Stop {
                task_id: id.clone(),
            }),
            Write::CancelQueue(id) => mutations.push(TaskMutation::CancelQueue {
                task_id: id.clone(),
            }),
            Write::Publish { task_id, action } => mutations.push(TaskMutation::Publish {
                task_id: task_id.clone(),
                action: *action,
                confirmed: false,
            }),
            Write::PublishConfirm => {
                if let Some(Modal::Publish {
                    task_id, action, ..
                }) = &self.modal
                {
                    mutations.push(TaskMutation::Publish {
                        task_id: task_id.clone(),
                        action: *action,
                        confirmed: true,
                    });
                }
            }
            Write::DeleteConfirm => {
                if let Some(Modal::Delete {
                    task_id,
                    path,
                    inspection: Some(Ok(inspection)),
                    ..
                }) = &self.modal
                {
                    mutations.push(TaskMutation::DeleteWorktree {
                        task_id: task_id.clone(),
                        path: path.clone(),
                        inspection: inspection.clone(),
                    });
                }
            }
            Write::Resume(id) => {
                if let Some(chat_id) = owner
                    .read(cx)
                    .host()
                    .store()
                    .as_ref()
                    .and_then(|s| s.get(id))
                    .and_then(latest_conversation)
                {
                    mutations.push(TaskMutation::ResumeConversation {
                        task_id: id.clone(),
                        chat_id: chat_id.into(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    });
                }
            }
            Write::ResumeConversation { task_id, chat_id } => {
                mutations.push(TaskMutation::ResumeConversation {
                    task_id: task_id.clone(),
                    chat_id: chat_id.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                })
            }
            Write::StopAndClose => {
                if let Some(Modal::StopAndClose { target, .. }) = &self.modal {
                    mutations.push(TaskMutation::CloseSession {
                        target: target.clone(),
                        confirmed: true,
                    });
                }
            }
            _ => {}
        }
        for mutation in mutations {
            let key = match &mutation {
                TaskMutation::Close { task_id } => (task_id.clone(), "archive"),
                TaskMutation::Stop { task_id } => (task_id.clone(), "stop"),
                TaskMutation::CancelQueue { task_id } => (task_id.clone(), "cancel"),
                TaskMutation::Publish { task_id, .. } => (task_id.clone(), "publish"),
                TaskMutation::DeleteWorktree { task_id, .. } => (task_id.clone(), "delete"),
                TaskMutation::ResumeConversation { task_id, .. } => (task_id.clone(), "resume"),
                TaskMutation::CloseSession { target, .. } => (target.session_uid.clone(), "close"),
                _ => unreachable!("panel mutation"),
            };
            if self.in_flight.contains(&key) {
                continue;
            }

            let archive_id = match &mutation {
                TaskMutation::Close { task_id } => Some(task_id.clone()),
                _ => None,
            };
            let action = owner.update(cx, |o, cx| {
                let mut facts = o.panel_facts(cx);
                facts
                    .sessions
                    .extend(inputs.open_sessions.iter().map(|target| session::State {
                        target: target.clone(),
                        control: session::TurnControl::Terminal,
                        task_live: true,
                        last_tab: false,
                    }));
                facts.conversations = inputs.conversations.clone();
                o.panel_resume_facts(&mutation, &mut facts);
                o.host_mut()
                    .reduce_mutation(
                        TaskMutationCommand {
                            reply: None,
                            mutation,
                        },
                        &facts,
                    )
                    .action
            });
            let action = match action {
                Ok(a) => a,
                Err(error) => {
                    let error = if let Some(task_id) = archive_id {
                        match owner.read(cx).host().store().as_ref() {
                            None => "GitTerm's task store is unavailable".into(),
                            Some(store) if store.get(&task_id).is_none() => format!(
                                "failed to archive task store {}: task {task_id} does not exist",
                                store.path().display()
                            ),
                            Some(store) => format!(
                                "failed to replace task store {}: {error}",
                                store.path().display()
                            ),
                        }
                    } else {
                        error
                    };
                    self.show_notice(error, cx);
                    continue;
                }
            };
            if matches!(action, TaskMutationAction::None) {
                continue;
            }
            if let TaskMutationAction::ConfirmPublish { task_id, action } = action {
                self.modal = Some(Modal::Publish {
                    task_id,
                    action,
                    error: None,
                });
                cx.notify();
                continue;
            }
            let archived_selected = match &action {
                TaskMutationAction::ArchiveTask { task_id } => {
                    inputs.selected.as_deref() == Some(task_id)
                }
                _ => false,
            };
            match owner.update(cx, |o, cx| o.execute_panel_action(action, cx)) {
                Ok(job) => {
                    self.in_flight.insert(key.clone());
                    if archived_selected {
                        cx.emit(TaskPanelOutcome::Back);
                    }
                    cx.notify();
                    cx.spawn(async move |panel, cx| {
                        let result = job.await;
                        _ = panel.update(cx, |panel, cx| {
                            panel.in_flight.remove(&key);
                            if let Err(error) = result {
                                panel.show_notice(error, cx);
                            } else if panel.modal_operation().as_ref() == Some(&key) {
                                panel.modal = None;
                                panel.notice = None;
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                Err(error) => self.show_notice(error, cx),
            }
        }
    }

    /// Iced `TaskCreateSubmit`: the form's raw values, as Iced sends them.
    fn create_input(&self, cx: &App) -> Option<TaskCreate> {
        let Some(Modal::Create(form)) = &self.modal else {
            return None;
        };
        let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
        // Iced clears the base when Custom ref… is picked; the field holds it.
        let base = if form.base_custom {
            read(&form.custom_base)
        } else {
            form.base
                .read(cx)
                .selected_value()
                .cloned()
                .unwrap_or_default()
        };
        Some(TaskCreate {
            title: read(&form.title),
            objective: read(&form.objective),
            repository_path: form.repository_path.clone(),
            workspace_name: Some(form.workspace_name.clone()),
            issue_key: Some(read(&form.issue)),
            base_reference: Some(base),
            stopping_boundary: form.stopping_boundary,
            origin: TaskCreateOrigin::Form,
        })
    }

    /// Iced runs the form through the shared `TaskMutation::Create` reducer;
    /// its error stays in the form. The normalized preparation then meets
    /// the service check.
    fn submit_create(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.create_input(cx) else {
            return;
        };
        let Some(owner) = cx.try_global::<GpuiTaskHost>().map(|host| host.0.clone()) else {
            self.show_notice("The task store is unavailable".into(), cx);
            return;
        };
        if owner.read(cx).host().store().is_none() {
            self.show_notice("The task store is unavailable".into(), cx);
            return;
        }
        if !owner.read(cx).host().services().is_preview() {
            let task_id = match &self.modal {
                Some(Modal::Create(form)) => form.task_id.clone(),
                _ => return,
            };
            let key = (task_id.clone(), "create");
            if self.in_flight.contains(&key) {
                return;
            }
            match owner.update(cx, |owner, cx| owner.create_from_panel(input, task_id, cx)) {
                Ok(reply) => {
                    self.in_flight.insert(key.clone());
                    cx.notify();
                    cx.spawn(async move |panel, cx| {
                        let result = reply.await.unwrap_or_else(|_| {
                            Err("The task host closed before task creation completed".into())
                        });
                        _ = panel.update(cx, |panel, cx| {
                            panel.in_flight.remove(&key);
                            match result {
                                Ok(_) if panel.modal_operation().as_ref() == Some(&key) => {
                                    panel.modal = None
                                }
                                Ok(_) => {}
                                Err(error) => panel.show_notice(error, cx),
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                Err(error) => self.show_notice(error, cx),
            }
        } else {
            let decided = owner.update(cx, |owner, _| {
                owner
                    .host_mut()
                    .reduce_mutation(
                        TaskMutationCommand {
                            reply: None,
                            mutation: TaskMutation::Create(input),
                        },
                        &TaskMutationFacts::default(),
                    )
                    .action
            });
            match decided {
                Ok(action) => {
                    self.show_notice(refuse(owner.read(cx).host().services(), &action), cx)
                }
                Err(error) => {
                    if let Some(Modal::Create(form)) = &mut self.modal {
                        form.error = Some(error);
                    }
                    cx.notify();
                }
            }
        }
    }
}

// ---- Rendering: free functions so the shell places each surface where Iced does.

fn muted(p: &Palette) -> gpui::Rgba {
    rgb(p.muted)
}

fn ghost(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Button {
    let id = id.into();
    let selector = id.clone();
    Button::new(gpui::ElementId::Name(id))
        .debug_selector(move || selector.to_string())
        .ghost()
        .small()
        .label(label.into())
}

/// Iced `view_task_rail`, for the sidebar's Tasks mode.
pub(crate) fn rail_view(panel: &Entity<TaskPanel>, inputs: Inputs, cx: &App) -> gpui::AnyElement {
    let p = Palette::current(cx);
    let state = panel.read(cx);
    let filter = state.filter;
    let store = host(cx).and_then(|host| host.store().as_ref());
    let model = store.map(|store| rail(store, &inputs.location, filter));
    let inputs = std::rc::Rc::new(inputs);
    let mut content = div()
        .id("task-rail")
        .debug_selector(|| "task-rail".into())
        .flex()
        .flex_col()
        .gap(px(8.))
        .px(px(8.))
        .py(px(10.))
        .size_full()
        .overflow_y_scroll()
        .text_size(px(12.));

    let new_task = {
        let (panel, inputs) = (panel.clone(), inputs.clone());
        ghost("task-new", "+ New task")
            .text_color(rgb(p.blue))
            .on_click(move |_, window, cx| {
                panel.update(cx, |panel, cx| panel.open_create(&inputs, window, cx))
            })
    };
    content = content.child(
        div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(10.))
                    .text_color(muted(&p))
                    .child(inputs.workspace_name.to_uppercase()),
            )
            .child(new_task),
    );

    let general_selected = inputs.selected.is_none();
    let general = if general_selected {
        format!(
            "● General{}",
            if inputs.compact {
                String::new()
            } else {
                format!(
                    " · {} session{}",
                    inputs.general_sessions,
                    if inputs.general_sessions == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            }
        )
    } else if inputs.compact {
        "← Sessions".into()
    } else {
        "← Sessions · General".into()
    };
    let back = panel.clone();
    content = content.child(
        div()
            .id("task-general")
            .debug_selector(|| "task-general".into())
            .px(px(9.))
            .py(px(8.))
            .rounded(px(6.))
            .when(general_selected, |row| {
                row.bg(rgb(p.surface))
                    .border_1()
                    .border_color(rgb(p.surface2))
            })
            .hover(|row| row.bg(rgb(p.overlay)))
            .child(general)
            .on_click(move |_, _, cx| back.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Back))),
    );

    let Some(model) = model else {
        return content
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.red))
                    .child("The task store is unavailable"),
            )
            .into_any_element();
    };
    content = content.child(
        div()
            .text_size(px(10.))
            .text_color(muted(&p))
            .child(format!("TASKS · {}", model.active)),
    );
    if !inputs.compact {
        let chip = |id: &'static str, label: String, chip_filter: RailFilter| {
            let active = filter == chip_filter;
            let panel = panel.clone();
            ghost(id, label)
                .xsmall()
                .rounded(px(10.))
                .border_1()
                .border_color(rgb(if active { p.lavender } else { p.border }))
                .when(active, |chip| {
                    chip.bg(rgb(p.lavender)).text_color(rgb(p.crust))
                })
                .when(!active, |chip| chip.text_color(muted(&p)))
                .on_click(move |_, _, cx| {
                    panel.update(cx, |panel, cx| panel.select_filter(chip_filter, cx))
                })
        };
        let danger_chip = |id: &'static str, label: String, write: Write| {
            let (panel, inputs) = (panel.clone(), inputs.clone());
            ghost(id, label)
                .xsmall()
                .rounded(px(10.))
                .border_1()
                .border_color(rgb(p.red))
                .text_color(rgb(p.red))
                .on_click(move |_, _, cx| {
                    panel.update(cx, |panel, cx| panel.write(write.clone(), &inputs, cx))
                })
        };
        content = content.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .child(chip("task-filter-all", "All".into(), RailFilter::All))
                .child(chip(
                    "task-filter-needs-you",
                    format!("Needs you · {}", model.needs_you),
                    RailFilter::NeedsYou,
                ))
                .child(chip(
                    "task-filter-archived",
                    "Archived".into(),
                    RailFilter::Archived,
                ))
                .when(!model.failed.is_empty(), |row| {
                    row.child(danger_chip(
                        "task-archive-failed",
                        format!("Archive failed · {}", model.failed.len()),
                        Write::ArchiveFailed,
                    ))
                })
                .when(!model.finished.is_empty(), |row| {
                    row.child(danger_chip(
                        "task-archive-finished",
                        format!("Archive {} finished", model.finished.len()),
                        Write::ArchiveFinished,
                    ))
                }),
        );
    }
    if let Some(notice) = &state.notice {
        content = content.child(
            div()
                .id("task-rail-notice")
                .debug_selector(|| "task-rail-notice".into())
                .text_size(px(10.))
                .text_color(rgb(p.red))
                .child(notice.clone()),
        );
    }
    if model.rows.is_empty() {
        content = content.child(div().text_size(px(11.)).text_color(rgb(p.subtext0)).child(
            match filter {
                RailFilter::All => "No active tasks",
                RailFilter::NeedsYou => "Nothing needs you",
                RailFilter::Archived => "No archived tasks",
            },
        ));
    }
    for task in model.rows {
        let selected = inputs.selected.as_deref() == Some(task.task_id.as_str());
        let issue = task.issue.as_ref().map(|issue| issue.key.clone());
        let mut title = div()
            .flex()
            .gap(px(7.))
            .child(div().text_color(muted(&p)).child("▸"))
            .child(
                div()
                    .text_color(rgb(if issue.is_some() { p.blue } else { p.text }))
                    .child(issue.clone().unwrap_or_else(|| task.title.clone())),
            );
        if !inputs.compact {
            title = title.child(if issue.is_some() {
                div().text_color(rgb(p.text)).child(task.title.clone())
            } else {
                div().text_size(px(9.)).text_color(muted(&p)).child("local")
            });
        }
        let state_line = state_line(task, &inputs);
        let id = task.task_id.clone();
        let select = panel.clone();
        let row = div()
            .id(SharedString::from(format!("task-row-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("task-row-{id}")
            })
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(3.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(6.))
            .when(selected, |row| {
                row.bg(rgb(p.surface))
                    .border_1()
                    .border_color(rgb(p.surface2))
            })
            .hover(|row| row.bg(rgb(p.overlay)))
            .child(title)
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(muted(&p))
                    .child(state_line),
            )
            .on_click({
                let id = id.clone();
                move |_, _, cx| {
                    select.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Select(id.clone())))
                }
            });
        let mut line = div().flex().items_center().gap(px(2.)).child(row);
        if row_archivable(task, inputs.compact) {
            let (panel, inputs) = (panel.clone(), inputs.clone());
            line = line.child(
                ghost(format!("task-row-archive-{id}"), "Archive")
                    .xsmall()
                    .text_color(muted(&p))
                    .on_click(move |_, _, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.write(Write::Archive(id.clone()), &inputs, cx)
                        })
                    }),
            );
        }
        content = content.child(line);
    }
    content.into_any_element()
}

/// Iced `view_task_detail` for the selected task: its card with the state
/// verbs, and TASK SETTINGS with `Delete worktree…`.
pub(crate) fn overview_view(
    panel: &Entity<TaskPanel>,
    inputs: Inputs,
    cx: &App,
) -> gpui::AnyElement {
    let p = Palette::current(cx);
    let task = inputs
        .selected
        .as_deref()
        .and_then(|id| host(cx)?.store().as_ref()?.get(id))
        .cloned();
    let page = div()
        .id("task-overview")
        .debug_selector(|| "task-overview".into())
        .flex_1()
        .h_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .overflow_y_scroll();
    let Some(task) = task else {
        let back = panel.clone();
        return page
            .child("Task not found")
            .child(
                div()
                    .text_color(muted(&p))
                    .child("This task is no longer in the task store."),
            )
            .child(
                ghost("task-overview-back", "Back to General").on_click(move |_, _, cx| {
                    back.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Back))
                }),
            )
            .into_any_element();
    };
    let inputs = std::rc::Rc::new(inputs);
    let agent = inputs.default_agent().to_owned();
    let mut row = div().flex().flex_wrap().gap(px(10.));
    for verb in verbs(&task, &inputs) {
        let (writer, inputs, id) = (panel.clone(), inputs.clone(), task.task_id.clone());
        let write = move |write: Write| {
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                writer.update(cx, |panel, cx| panel.write(write.clone(), &inputs, cx))
            }
        };
        let emit = |outcome: TaskPanelOutcome| {
            let panel = panel.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                panel.update(cx, |_, cx| cx.emit(outcome.clone()))
            }
        };
        row = row.child(match verb {
            Verb::Start {
                continuing,
                enabled,
            } => ghost("task-start", start_label(continuing, &agent))
                .text_color(rgb(p.blue))
                .disabled(!enabled)
                .on_click(emit(TaskPanelOutcome::Pick(id))),
            Verb::Resume => ghost("task-resume", "▸ Resume").on_click(write(Write::Resume(id))),
            Verb::GoToSession(session) => ghost("task-go-to-session", "Go to session")
                .on_click(emit(TaskPanelOutcome::FocusSession(session))),
            Verb::ReviewChanges => {
                let review = id;
                let panel = panel.clone();
                ghost("task-review-changes", "Review changes").on_click(move |_, _, cx| {
                    panel.update(cx, |_, cx| {
                        cx.emit(TaskPanelOutcome::Review(review.clone()))
                    });
                })
            }
            Verb::PushBranch => {
                ghost("task-push-branch", "Push branch").on_click(write(Write::Publish {
                    task_id: id,
                    action: TaskPublish::PushBranch,
                }))
            }
            Verb::OpenDraftPr => {
                ghost("task-open-draft-pr", "Open draft PR").on_click(write(Write::Publish {
                    task_id: id,
                    action: TaskPublish::OpenDraftPr,
                }))
            }
            Verb::Stop => ghost("task-stop", "■ Stop")
                .text_color(rgb(p.red))
                .on_click(write(Write::Stop(id))),
            Verb::CancelQueue => {
                ghost("task-cancel-queue", "Cancel queue").on_click(write(Write::CancelQueue(id)))
            }
            Verb::Archive { enabled } => ghost("task-archive", "Archive")
                .disabled(!enabled)
                .on_click(write(Write::Archive(id))),
        });
    }
    let value_row = |label: &'static str, value: String| {
        div()
            .flex()
            .gap(px(12.))
            .child(
                div()
                    .w(px(130.))
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(muted(&p))
                    .child(label),
            )
            .child(div().text_color(rgb(p.subtext0)).child(value))
    };
    let mut details = div()
        .id("task-details")
        .debug_selector(|| "task-details".into())
        .flex()
        .flex_col()
        .gap(px(9.));
    for (label, value) in detail_rows(&task, &inputs) {
        details = details.child(value_row(label, value));
    }
    let handoff = match &task.handoff {
        Some(handoff) => {
            let list = |items: &[String]| {
                if items.is_empty() {
                    "none".to_owned()
                } else {
                    items.join(" · ")
                }
            };
            div()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(
                    div()
                        .text_color(rgb(p.subtext0))
                        .child(handoff.summary.clone()),
                )
                .child(value_row("DECISIONS", list(&handoff.decisions)))
                .child(value_row("NEXT STEPS", list(&handoff.next_steps)))
                .child(value_row("BLOCKERS", list(&handoff.blockers)))
        }
        None => div()
            .text_size(px(11.))
            .text_color(muted(&p))
            .child("No durable handoff recorded yet."),
    };
    let mut sessions = div()
        .id("task-sessions")
        .debug_selector(|| "task-sessions".into())
        .flex()
        .flex_col()
        .gap(px(5.));
    let mut session_count = 0;
    for (index, target) in inputs.task_sessions(&task.task_id).enumerate() {
        session_count += 1;
        let short = target
            .task_session_id
            .as_deref()
            .map_or_else(|| "legacy".to_owned(), |id| id.chars().take(8).collect());
        let title = inputs
            .titles
            .get(&target.session_uid)
            .cloned()
            .unwrap_or_default();
        let focus = target.task_session_id.clone();
        let panel = panel.clone();
        sessions = sessions.child(
            div()
                .id(("task-session", index))
                .debug_selector(move || format!("task-session-{index}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(9.))
                .py(px(6.))
                .rounded(px(6.))
                .when(focus.is_some(), |row| {
                    row.hover(|row| row.bg(rgb(p.overlay)))
                })
                .child(div().flex_1().child(title))
                .child(div().text_size(px(10.)).text_color(muted(&p)).child(short))
                .on_click(move |_, _, cx| {
                    if let Some(session) = focus.clone() {
                        panel.update(cx, |_, cx| cx.emit(TaskPanelOutcome::FocusSession(session)));
                    }
                }),
        );
    }
    if session_count == 0 {
        sessions = sessions.child(
            div()
                .text_size(px(11.))
                .text_color(muted(&p))
                .child("No sessions yet. Add an agent or terminal to begin."),
        );
    }
    let mut conversations = div()
        .id("task-history")
        .debug_selector(|| "task-history".into())
        .flex()
        .flex_col()
        .gap(px(5.));
    let rows = host(cx)
        .and_then(|host| host.store().as_ref())
        .map(|store| history(&task, store, &inputs))
        .unwrap_or_default();
    if rows.is_empty() {
        // Iced's `Looking for agent conversations…` needs its chat index.
        conversations = conversations.child(
            div()
                .text_size(px(11.))
                .text_color(muted(&p))
                .child("No resumable agent conversations found yet."),
        );
    }
    for (index, row) in rows.into_iter().enumerate() {
        let (panel, inputs, task_id) = (panel.clone(), inputs.clone(), task.task_id.clone());
        let enabled = row.state != HistoryState::TranscriptKept;
        let (state, chat_id) = (row.state, row.chat_id.clone());
        conversations = conversations.child(
            div()
                .id(("task-history-row", index))
                .debug_selector(move || format!("task-history-row-{index}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(9.))
                .py(px(6.))
                .rounded(px(6.))
                .when(enabled, |row| row.hover(|row| row.bg(rgb(p.overlay))))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(p.peach))
                                        .child(row.badge),
                                )
                                .child(div().child(row.title)),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(muted(&p))
                                .child(row.short_id),
                        ),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(match state {
                            HistoryState::Open => rgb(p.blue),
                            HistoryState::TranscriptKept => muted(&p),
                            _ => rgb(p.subtext0),
                        })
                        .child(state.label()),
                )
                .on_click(move |_, _, cx| {
                    if !enabled {
                        return;
                    }
                    let (task_id, chat_id) = (task_id.clone(), chat_id.clone());
                    let write = if state == HistoryState::Open {
                        Write::OpenConversation { task_id, chat_id }
                    } else {
                        Write::ResumeConversation { task_id, chat_id }
                    };
                    panel.update(cx, |panel, cx| panel.write(write, &inputs, cx))
                }),
        );
    }
    let section = |label: &'static str| div().text_size(px(10.)).text_color(muted(&p)).child(label);
    let mut page = page
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(14.))
                .p(px(24.))
                .bg(rgb(p.surface))
                .border_1()
                .border_color(rgb(p.border))
                .rounded(px(8.))
                .child(div().text_size(px(20.)).child(task.title.clone()))
                .child(
                    div()
                        .text_color(rgb(p.subtext0))
                        .child(task.objective.clone()),
                )
                .child(details)
                .child(row),
        )
        .child(section("LATEST HANDOFF"))
        .child(handoff)
        .child(section("SESSIONS"))
        .child(sessions)
        .child(section("CONVERSATION HISTORY"))
        .child(conversations);
    if let Some(notice) = &panel.read(cx).notice {
        page = page.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(p.red))
                .child(notice.clone()),
        );
    }
    if delete_opener(&task) {
        let (panel, inputs, id) = (panel.clone(), inputs.clone(), task.task_id.clone());
        page = page
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(muted(&p))
                    .child("TASK SETTINGS"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        ghost("task-delete-worktree-open", "Delete worktree…")
                            .text_color(rgb(p.red))
                            .on_click(move |_, _, cx| {
                                panel.update(cx, |panel, cx| panel.open_delete(&id, &inputs, cx))
                            }),
                    )
                    .child(div().text_size(px(10.)).text_color(muted(&p)).child(
                        "Removes the working copy from disk. The branch and task record stay.",
                    )),
            );
    }
    page.into_any_element()
}

/// Iced's lines for the risks that block a worktree delete (the core
/// `TaskMutation::DeleteWorktree` policy refuses the same three).
fn blocking_lines(inspection: &CleanupInspection) -> Vec<String> {
    inspection
        .risks
        .iter()
        .filter_map(|risk| match risk {
            CleanupRisk::ActiveProcess => {
                Some("An agent or session is still attached — stop it first".to_owned())
            }
            CleanupRisk::UncommittedChanges => {
                Some("The worktree has uncommitted changes".to_owned())
            }
            CleanupRisk::UnpushedCommits { count } => Some(format!(
                "{count} commit{} not pushed anywhere",
                if *count == 1 { "" } else { "s" }
            )),
            _ => None,
        })
        .collect()
}

fn delete_blocked(inspection: &CleanupInspection) -> bool {
    !blocking_lines(inspection).is_empty()
}

fn boundary_label(boundary: StoppingBoundary) -> &'static str {
    match boundary {
        StoppingBoundary::PlanOnly => "Plan only",
        StoppingBoundary::ImplementUntilTestsPass => "Tests pass",
        StoppingBoundary::PrepareDraftPr => "Draft PR",
    }
}

/// The open task modal over the window, as Iced stacks it over the main view.
pub(crate) fn modal_view(
    panel: &Entity<TaskPanel>,
    inputs: Inputs,
    cx: &App,
) -> Option<gpui::AnyElement> {
    let p = Palette::current(cx);
    let state = panel.read(cx);
    let modal = state.modal.as_ref()?;
    let inputs = std::rc::Rc::new(inputs);
    let task_title = |task_id: &str, fallback: &'static str| {
        host(cx)
            .and_then(|host| host.store().as_ref())
            .and_then(|store| store.get(task_id))
            .map_or_else(|| fallback.to_owned(), |task| task.title.clone())
    };
    let dismiss = |id: &'static str, label: &'static str| {
        let panel = panel.clone();
        ghost(id, label)
            .disabled(state.busy())
            .on_click(move |_, _, cx| panel.update(cx, |panel, cx| panel.dismiss(cx)))
    };
    let confirm = |id: &'static str, label: String, write: Write| {
        let (panel, inputs) = (panel.clone(), inputs.clone());
        ghost(id, label)
            .disabled(state.busy())
            .text_color(rgb(p.red))
            .on_click(move |_, _, cx| {
                panel.update(cx, |panel, cx| panel.write(write.clone(), &inputs, cx))
            })
    };
    let error = |error: &Option<String>| {
        error
            .clone()
            .map(|error| div().text_size(px(11.)).text_color(rgb(p.red)).child(error))
    };
    let buttons = || div().flex().justify_end().gap(px(8.));
    let mut card = div()
        .id("task-modal")
        .debug_selector(|| "task-modal".into())
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(26.))
        .py(px(22.))
        .bg(rgb(p.mantle))
        .border_1()
        .border_color(rgb(p.border))
        .rounded(px(7.))
        .text_size(px(12.))
        .text_color(rgb(p.text));
    match modal {
        Modal::Publish {
            task_id,
            action,
            error: err,
        } => {
            let (heading, label) = match action {
                TaskPublish::PushBranch => ("Push without a linked issue?", "Push anyway"),
                TaskPublish::OpenDraftPr => {
                    ("Open a draft PR without a linked issue?", "Open PR anyway")
                }
            };
            card = card
                .w(px(560.))
                .child(div().text_size(px(16.)).child(heading))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted(&p))
                        .child(format!("“{}”", task_title(task_id, "this task"))),
                )
                .child(div().text_color(rgb(p.subtext0)).child(
                    "This task has no linked issue. Commits and pull requests in this repo \
                     carry an issue key — link an issue to the task first, or continue \
                     without one.",
                ))
                .children(error(err))
                .child(
                    buttons()
                        .child(dismiss("task-publish-cancel", "Cancel"))
                        .child(confirm(
                            "task-publish-confirm",
                            label.into(),
                            Write::PublishConfirm,
                        )),
                );
        }
        Modal::Delete {
            task_id,
            path,
            inspection,
            error: err,
            ..
        } => {
            card = card
                .w(px(560.))
                .child(div().text_size(px(16.)).child("Delete worktree?"))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted(&p))
                        .child(format!(
                            "“{}” · {}",
                            task_title(task_id, "this task"),
                            path.display()
                        )),
                );
            // Iced `view_task_worktree_delete_modal`: the confirm label comes
            // from the inspection; none while pending, failed or blocked.
            let secondary = |line: &str| div().text_color(rgb(p.subtext0)).child(line.to_owned());
            let mut label = None;
            match inspection {
                None => card = card.child(secondary("Checking for unsaved work…")),
                Some(Err(failure)) => {
                    card = card.child(
                        div()
                            .text_color(rgb(p.red))
                            .child(format!("Could not inspect the worktree: {failure}")),
                    )
                }
                Some(Ok(inspection)) => {
                    let blocking = blocking_lines(inspection);
                    if !blocking.is_empty() {
                        card = card.child(secondary(
                            "This worktree still holds work that would be lost:",
                        ));
                        for line in blocking {
                            card = card.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(p.red))
                                    .child(format!("· {line}")),
                            );
                        }
                    } else if !inspection.path_exists || !inspection.registered {
                        card = card.child(secondary(
                            "The worktree is no longer on disk. This clears it from the task record.",
                        ));
                        label = Some("Clear worktree record");
                    } else {
                        card = card.child(secondary(
                            "No unsaved work detected. The task branch and record stay; only the \
                             working copy is deleted.",
                        ));
                        if let Some(CleanupRisk::ExistingPullRequest { url }) = inspection
                            .risks
                            .iter()
                            .find(|risk| matches!(risk, CleanupRisk::ExistingPullRequest { .. }))
                        {
                            card =
                                card.child(div().text_size(px(11.)).text_color(muted(&p)).child(
                                    format!("A pull request exists for this branch: {url}"),
                                ));
                        }
                        label = Some("Delete worktree");
                    }
                }
            }
            card = card.children(error(err)).child(
                buttons()
                    .child(dismiss("task-delete-keep", "Keep worktree"))
                    .children(label.map(|label| {
                        confirm(
                            "task-delete-confirm",
                            if state.busy() { "Deleting…" } else { label }.into(),
                            Write::DeleteConfirm,
                        )
                    })),
            );
        }
        Modal::StopAndClose { target, error: err } => {
            let title = target
                .task_id
                .as_deref()
                .map_or_else(|| "this task".to_owned(), |id| task_title(id, "this task"));
            card =
                card.w(px(510.))
                    .child(div().text_size(px(16.)).child("Stop this task session?"))
                    .child(
                        div()
                            .text_color(rgb(p.subtext0))
                            .child(format!("A session of “{title}” is running in this tab.")),
                    )
                    .child(div().text_size(px(11.)).text_color(muted(&p)).child(
                        "Closing stops it. The task and its worktree stay available to resume.",
                    ))
                    .children(error(err))
                    .child(
                        buttons()
                            .child(dismiss("task-close-keep", "Keep open"))
                            .child(confirm(
                                "task-stop-and-close",
                                "Stop and close".into(),
                                Write::StopAndClose,
                            )),
                    );
        }
        Modal::Create(form) => {
            card = card
                .w(px(620.))
                .children(create_form(panel, form, &inputs, &p, cx))
        }
    }
    Some(
        div()
            .id("task-modal-backdrop")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::Rgba {
                a: 0.82,
                ..rgb(p.crust)
            })
            .child(card)
            .into_any_element(),
    )
}

fn create_form(
    panel: &Entity<TaskPanel>,
    form: &CreateForm,
    inputs: &std::rc::Rc<Inputs>,
    p: &Palette,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    let busy = panel.read(cx).busy();
    let label = |text: &'static str| div().text_size(px(10.)).text_color(muted(p)).child(text);
    let field = |text: &'static str, selector: &'static str, input: &Entity<InputState>| {
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(label(text))
            .child(
                div()
                    .debug_selector(move || selector.into())
                    .child(Input::new(input).small().disabled(busy)),
            )
    };
    let issue = form.issue.read(cx).value().trim().to_owned();
    let branch = suggested_task_branch(
        &form.task_id,
        (!issue.is_empty()).then_some(issue.as_str()),
        form.title.read(cx).value().as_ref(),
    );
    let worktree = branch
        .as_deref()
        .and_then(|branch| {
            suggested_task_worktree_path(
                &form.repository_path,
                &crate::workspace_config::iced_config_dir().join("worktrees"),
                branch,
            )
        })
        .map_or_else(|| "—".to_owned(), |path| path.display().to_string());
    let mut base = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(label("BASE"))
        .child(Select::new(&form.base).small().w_full());
    if form.base_custom {
        base = base.child(
            div()
                .debug_selector(|| "task-create-custom-base".into())
                .child(Input::new(&form.custom_base).small()),
        );
    } else if form.branches_loading {
        base = base.child(
            div()
                .text_size(px(9.))
                .text_color(muted(p))
                .child("Refreshing local branches…"),
        );
    }
    let mut boundaries = div().flex().gap(px(6.));
    for boundary in [
        StoppingBoundary::PlanOnly,
        StoppingBoundary::ImplementUntilTestsPass,
        StoppingBoundary::PrepareDraftPr,
    ] {
        let selected = boundary == form.stopping_boundary;
        let panel = panel.clone();
        boundaries = boundaries.child(
            ghost(
                format!("task-boundary-{boundary:?}"),
                boundary_label(boundary),
            )
            .border_1()
            .border_color(rgb(if selected { p.blue } else { p.border }))
            .when(selected, |chip| {
                chip.bg(rgb(p.blue)).text_color(rgb(p.base))
            })
            .on_click(move |_, _, cx| {
                panel.update(cx, |panel, cx| panel.select_boundary(boundary, cx))
            }),
        );
    }
    let cancel = panel.clone();
    let submit = (panel.clone(), inputs.clone());
    vec![
        div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().text_size(px(17.)).child("New task"))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(muted(p))
                            .child(format!("{} · This Mac", form.workspace_name)),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(muted(p))
                    .child(form.task_id.clone()),
            )
            .into_any_element(),
        field("TITLE", "task-create-title", &form.title).into_any_element(),
        field("OBJECTIVE", "task-create-objective", &form.objective).into_any_element(),
        div()
            .flex()
            .gap(px(8.))
            .child(field("LINEAR ISSUE", "task-create-issue", &form.issue).flex_1())
            .child(base)
            .into_any_element(),
        label("STOPPING BOUNDARY").into_any_element(),
        boundaries.into_any_element(),
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(8.))
            .bg(rgb(p.base))
            .border_1()
            .border_color(rgb(p.border))
            .rounded(px(3.))
            .text_size(px(10.))
            .text_color(rgb(p.subtext0))
            .child(format!(
                "BRANCH     {}",
                branch.unwrap_or_else(|| "—".into())
            ))
            .child(format!("WORKTREE   {worktree}"))
            .into_any_element(),
        div()
            .children(
                form.error
                    .clone()
                    .map(|error| div().text_size(px(11.)).text_color(rgb(p.red)).child(error)),
            )
            .into_any_element(),
        div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .child(
                ghost("task-create-cancel", "Cancel")
                    .disabled(busy)
                    .on_click(move |_, _, cx| cancel.update(cx, |panel, cx| panel.dismiss(cx))),
            )
            .child(
                ghost(
                    "task-create-submit",
                    if busy {
                        "Checking repository…"
                    } else {
                        "Create task"
                    },
                )
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    let (panel, inputs) = &submit;
                    panel.update(cx, |panel, cx| panel.write(Write::CreateSubmit, inputs, cx))
                }),
            )
            .into_any_element(),
    ]
}

// ---- Shell hookup.

impl crate::Shell {
    /// The panel, created with its outcome subscription on first use.
    pub(crate) fn ensure_task_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TaskPanel> {
        if let Some(panel) = &self.task_panel {
            return panel.clone();
        }
        let panel = cx.new(|_| TaskPanel::default());
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |shell, _, outcome: &TaskPanelOutcome, window, cx| {
                match outcome {
                    TaskPanelOutcome::Select(task) => shell.activate_rail_task(task, window, cx),
                    // Iced `TaskContextBack` keeps the rail as the host tab's sidebar.
                    TaskPanelOutcome::Back => {
                        shell.general_context(false, window, cx);
                        shell.show_task_rail(cx);
                    }
                    // Iced lands on a task tab with Git Changes; the preview opens
                    // the worktree's review, a read.
                    TaskPanelOutcome::Review(task_id) => {
                        shell.review_task_changes(task_id, window, cx)
                    }
                    TaskPanelOutcome::Notice(notice) => shell.owner.update(cx, |owner, cx| {
                        owner.error = Some(notice.clone());
                        cx.notify();
                    }),
                    TaskPanelOutcome::Pick(task) => shell.open_task_picker(task, window, cx),
                    TaskPanelOutcome::FocusSession(session) => {
                        shell.focus_task_session(session, window, cx)
                    }
                    TaskPanelOutcome::FocusConversation(uid) => {
                        shell.focus_open_session(uid, window, cx)
                    }
                }
            },
        );
        let observe = cx.observe(&panel, |_, _, cx| cx.notify());
        self._subscriptions.extend([subscription, observe]);
        if let Some(host) = cx.try_global::<GpuiTaskHost>().map(|host| host.0.clone()) {
            self._subscriptions
                .push(cx.observe(&host, |_, _, cx| cx.notify()));
            self._subscriptions.push(cx.subscribe_in(
                &host,
                window,
                |shell, _, stop: &crate::task_host::StopTask, window, cx| {
                    shell.stop_panel_task(stop.clone(), window, cx);
                },
            ));
        }
        self.task_panel = Some(panel.clone());
        panel
    }

    fn stop_panel_task(
        &mut self,
        mut stop: crate::task_host::StopTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending {
            cx.spawn_in(window, async move |shell, cx| {
                loop {
                    if !shell
                        .read_with(cx, |s, cx| s.owner.read(cx).workspace_save_pending)
                        .unwrap_or(false)
                    {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(10))
                        .await;
                }
                _ = shell.update_in(cx, |shell, window, cx| {
                    shell.stop_panel_task(stop, window, cx)
                });
            })
            .detach();
            return;
        }
        if stop.targets.is_empty() {
            let result = cx
                .try_global::<GpuiTaskHost>()
                .map(|h| h.0.clone())
                .ok_or_else(|| "The task host closed before Stop completed".to_owned())
                .and_then(|host| {
                    host.update(cx, |o, cx| o.panel_stop_completed(&stop.task_id, cx))
                });
            stop.complete(result);
            return;
        }
        let target = stop.targets.remove(0);
        let replacement = self
            .workspaces
            .iter()
            .find(|state| {
                state.sessions.len() == 1
                    && state
                        .panes
                        .values()
                        .any(|p| p.read(cx).session.as_ref() == Some(&target))
            })
            .map(|state| {
                (
                    state.workspace.id,
                    self.data.terminal_cwd(state.workspace.id, cx),
                )
            });
        if let Some((workspace, cwd)) = replacement {
            let previous = self.active_workspace(cx);
            self.select_workspace_id(workspace, window, cx);
            self.new_terminal_launch(crate::DockPlacement::Center, cwd, None, window, cx);
            if let Some(previous) = previous {
                self.select_workspace_id(previous, window, cx);
            }
        }
        self.close_target_with(&target, true, window, cx);
        cx.spawn_in(window, async move |shell, cx| {
            loop {
                let pending = shell
                    .read_with(cx, |shell, cx| shell.owner.read(cx).workspace_save_pending)
                    .unwrap_or(false);
                if !pending {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(10))
                    .await;
            }
            _ = shell.update_in(cx, |shell, window, cx| {
                if shell
                    .find_open_session(|current| current == &target, cx)
                    .is_none()
                {
                    shell.stop_panel_task(stop, window, cx);
                } else {
                    stop.complete(Err("The task session could not be closed".into()));
                }
            });
        })
        .detach();
    }

    pub(crate) fn sync_task_git_changes(&self, cx: &mut Context<Self>) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let task = self
            .active(cx)
            .active_session
            .and_then(|id| self.session_task(id, cx));
        let root = task
            .as_deref()
            .and_then(|id| host(cx)?.store().as_ref()?.get(id)?.worktree.path.clone())
            .or_else(|| self.active_repo_dir(workspace, cx));
        let Some(root) = root else {
            return;
        };
        if self
            .owner
            .read(cx)
            .model
            .workspaces
            .iter()
            .any(|w| w.id == workspace && w.root != root)
        {
            self.owner.update(cx, |owner, cx| {
                owner.dispatch(
                    crate::workspace_status::Intent::ContextChanged { workspace, root },
                    std::time::Instant::now(),
                    cx,
                )
            });
        }
    }

    fn review_task_changes(&mut self, task_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if host(cx).is_some_and(|h| h.services().is_preview()) {
            if let Some(panel) = &self.task_panel {
                panel.update(cx, |p, cx| {
                    p.show_notice("Launch session · not available in preview".into(), cx)
                });
            }
            return;
        }
        let inputs = self.task_inputs(cx);
        if let Some(target) = inputs.open_session(task_id) {
            self.focus_open_session(&target.session_uid, window, cx);
        } else if let Some(owner) = cx.try_global::<GpuiTaskHost>().map(|h| h.0.clone()) {
            match owner.update(cx, |o, cx| o.launch_from_ui(task_id, None, cx)) {
                Ok(_) => {}
                Err(e) => {
                    self.owner.update(cx, |o, cx| {
                        o.error = Some(e);
                        cx.notify();
                    });
                    return;
                }
            }
        }
        self.selected_task = Some(task_id.into());
        self.remember_context(Some(task_id.into()));
        self.sync_task_git_changes(cx);
        // TerminalLaunched is delivered after this callback; select Changes on
        // the same workspace sidebar that will follow the adopted task tab.
        self.update_regions(
            |r| {
                r.sidebar_mode = crate::layout::SidebarMode::Git;
                r.sidebar_collapsed = false;
            },
            cx,
        );
        let id = format!("workspace-{}-git", self.active(cx).workspace.layout_key);
        if let Some(git) = self
            .active(cx)
            .panes
            .get(&id)
            .and_then(|p| p.read(cx).git.clone())
        {
            git.update(cx, |git, cx| git.show_changes(cx));
        }
        let task_id = task_id.to_owned();
        cx.spawn_in(window, async move |shell, cx| {
            cx.background_executor()
                .timer(std::time::Duration::ZERO)
                .await;
            _ = shell.update_in(cx, |s, window, cx| {
                if let Some(target) = s.task_inputs(cx).open_session(&task_id).cloned() {
                    s.focus_open_session(&target.session_uid, window, cx);
                    s.sync_task_git_changes(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// A rail row, archived ones included: the rail lists the active
    /// workspace's tasks from the host's snapshot, so the selection resolves
    /// there too (the Work sidebar's list leaves archived tasks out).
    pub(crate) fn activate_rail_task(
        &mut self,
        task: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let location = self.task_inputs(cx).location;
        let listed = host(cx)
            .and_then(|host| host.store().as_ref())
            .and_then(|store| store.get(task))
            .is_some_and(|record| in_workspace(record, &location));
        if listed {
            self.activate_task_in(workspace, task, window, cx);
        }
    }

    /// Iced closes the open task modal on Escape; focus returns to the shell.
    pub(crate) fn dismiss_task_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(panel) = self
            .task_panel
            .clone()
            .filter(|_| self.task_panel_modal_open(cx))
        else {
            return false;
        };
        panel.update(cx, |panel, cx| panel.dismiss(cx));
        self.focus.focus(window, cx);
        true
    }

    pub(crate) fn task_panel_modal_open(&self, cx: &App) -> bool {
        self.task_panel
            .as_ref()
            .is_some_and(|panel| panel.read(cx).modal_open())
    }

    /// Iced `TaskChildAdd`: the session picker over the task, with the
    /// task's handoff prompt copied to the clipboard (a local write the
    /// preview allows) and the picker's `handoff copied` line.
    pub(crate) fn open_task_picker(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = host(cx)
            .and_then(|host| host.store().as_ref())
            .and_then(|store| store.get(task_id))
        else {
            return;
        };
        let handoff = task_handoff_prompt(task, None);
        let inputs = self.task_inputs(cx);
        let context = crate::session_picker::TaskContext::from_task(
            task,
            inputs.task_sessions(task_id).count(),
            true,
        );
        self.open_picker(Some(context), window, cx);
        // Only when the picker opened, so its line never claims a copy.
        if self.session_picker.is_some() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(handoff));
        }
    }

    /// The task picker's choice is a launch: it stops at the preview notice.
    pub(crate) fn launch_task_child(
        &mut self,
        task_id: &str,
        preset: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inputs = self.task_inputs(cx);
        let panel = self.ensure_task_panel(window, cx);
        panel.update(cx, |panel, cx| {
            panel.write(
                Write::Launch {
                    task_id: task_id.to_owned(),
                    preset,
                },
                &inputs,
                cx,
            )
        });
    }

    fn find_open_session(
        &self,
        matches: impl Fn(&Target) -> bool,
        cx: &App,
    ) -> Option<(crate::data::WorkspaceId, crate::data::SessionId)> {
        self.workspaces.iter().find_map(|state| {
            state.sessions.iter().find_map(|tab| {
                let pane = state.panes.get(&crate::session_pane_id(tab.id))?;
                matches(pane.read(cx).session.as_ref()?).then_some((state.workspace.id, tab.id))
            })
        })
    }

    /// Focus the open tab with this `session_uid` (a history row's `Open`).
    pub(crate) fn focus_open_session(
        &mut self,
        uid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.find_open_session(|target| target.session_uid == uid, cx) {
            Some((workspace, id)) => {
                if self.active_workspace(cx) != Some(workspace) {
                    self.select_workspace_id(workspace, window, cx);
                }
                self.select_host_session(workspace, id, window, cx);
                cx.notify();
            }
            None => {
                if let Some(panel) = &self.task_panel {
                    panel.update(cx, |panel, cx| {
                        panel.notice = Some("That conversation's tab is no longer open".into());
                        cx.notify();
                    });
                }
            }
        }
    }

    /// Iced `TaskFocusSession`: the open tab carrying that task session, in
    /// any workspace.
    pub(crate) fn focus_task_session(
        &mut self,
        session: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.find_open_session(
            |target| target.task_session_id.as_deref() == Some(session),
            cx,
        );
        match found {
            Some((workspace, id)) => {
                if self.active_workspace(cx) != Some(workspace) {
                    self.select_workspace_id(workspace, window, cx);
                }
                self.select_host_session(workspace, id, window, cx);
                cx.notify();
            }
            None => {
                let notice = format!(
                    "Task session {} is no longer open",
                    session.chars().take(8).collect::<String>()
                );
                if let Some(panel) = &self.task_panel {
                    panel.update(cx, |panel, cx| {
                        panel.notice = Some(notice);
                        cx.notify();
                    });
                }
            }
        }
    }

    /// Iced sets the host tab's sidebar to Tasks on entering the Overview.
    pub(crate) fn show_task_rail(&mut self, cx: &mut Context<Self>) {
        self.update_regions(
            |regions| {
                regions.sidebar_mode = crate::layout::SidebarMode::Tasks;
                regions.sidebar_collapsed = false;
            },
            cx,
        );
    }

    pub(crate) fn task_inputs(&self, cx: &App) -> Inputs {
        let Some(workspace) = self.active_workspace(cx) else {
            return Inputs::default();
        };
        let state = self.active(cx);
        let mut titles = std::collections::HashMap::new();
        // Iced `task_session_count` / `first_open_task_tab` / `task_state_copy`
        // search every workspace: a task's sessions can live outside this one.
        let targets: Vec<Target> = self
            .workspaces
            .iter()
            .flat_map(|other| other.sessions.iter().map(move |session| (other, session)))
            .filter_map(|(other, session)| {
                let target = other
                    .panes
                    .get(&crate::session_pane_id(session.id))?
                    .read(cx)
                    .session
                    .clone()?;
                titles.insert(target.session_uid.clone(), session.title.clone());
                Some(target)
            })
            .collect();
        use gitterm_core::remote::config::WorkspaceLocationConfig;
        let saved = self
            .owner
            .read(cx)
            .iced_workspace(workspace)
            .and_then(|saved| saved.location.as_ref());
        let location = match (saved, self.data.terminal_cwd(workspace, cx)) {
            (
                Some(WorkspaceLocationConfig::RemoteAgent {
                    remote_id, root, ..
                }),
                _,
            ) => Location::Remote {
                remote_id: remote_id.clone(),
                root: root.clone(),
            },
            (Some(WorkspaceLocationConfig::Local { root }), Some(dir)) => Location::Local {
                dir,
                root: root.into(),
            },
            (_, Some(dir)) => Location::local(dir),
            (_, None) => Location::None,
        };
        let open_chats = self
            .workspaces
            .iter()
            // Iced `find_chat_tab` matches workspaces that are not on a remote agent.
            .filter(|other| {
                !matches!(
                    self.owner
                        .read(cx)
                        .iced_workspace(other.workspace.id)
                        .and_then(|saved| saved.location.as_ref()),
                    Some(WorkspaceLocationConfig::RemoteAgent { .. })
                )
            })
            .flat_map(|other| other.sessions.iter().map(move |session| (other, session)))
            .filter_map(|(other, session)| {
                let pane = other
                    .panes
                    .get(&crate::session_pane_id(session.id))?
                    .read(cx);
                let chat_id = match &pane.content {
                    crate::pane::PaneContent::PreviewChat(view) => view.read(cx).chat_session_id(),
                    _ => pane.chat_session_id.clone(),
                }?;
                Some(OpenChat {
                    chat_id,
                    target: pane.session.clone()?,
                })
            })
            .collect();
        Inputs {
            has_workspace: true,
            active_repo: state
                .active_session
                .and_then(|_| self.active_repo_dir(workspace, cx)),
            presets: self.agent_presets.clone(),
            open_chats,
            conversations: self
                .work_sidebars
                .iter()
                .flat_map(|(workspace, sidebar)| {
                    let remote = self
                        .owner
                        .read(cx)
                        .iced_workspace(*workspace)
                        .is_some_and(|w| {
                            matches!(
                                w.location,
                                Some(WorkspaceLocationConfig::RemoteAgent { .. })
                            )
                        });
                    sidebar.read(cx).indexed_chats().iter().map(move |entry| {
                        gitterm_core::task_commands::TaskConversationFact {
                            chat_id: entry.id.clone(),
                            cwd: entry.cwd.clone(),
                            remote,
                        }
                    })
                })
                .collect(),
            workspace_name: state.workspace.name.clone(),
            location,
            selected: self.selected_task.clone(),
            general_sessions: state
                .sessions
                .iter()
                .filter(|session| self.session_task(session.id, cx).is_none())
                .count(),
            compact: state.store.regions.sidebar_size() <= 160.,
            open_sessions: targets
                .into_iter()
                .filter(|target| target.task_id.is_some())
                .collect(),
            titles,
        }
    }
}
