//! Standalone Iced Work sidebar port. The host owns the core reducer, executes
//! its effects, refreshes workspace facts, and subscribes to `WorkSidebarOutcome`.
//! Imported catalogs grant read authority for background Work loads; actions
//! that mutate Iced state or launch a tool report the preview limitation.
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
};

use crate::pr_confirm;
use gitterm_core::work::pr_actions::{self, ActionKind, PrActionConfirm, RowGate};
use gitterm_core::{
    chat_facts::ChildKind,
    chats,
    work::{
        state::{Pane, State},
        summary::{self, BranchStatus, Coordinator, Focus},
    },
};
use gpui::{Entity, EventEmitter, Subscription, Window, div, prelude::*, px, rgb};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};

/// Read authority comes from the owner's resolved Iced catalog. Sample views
/// and tests without an imported catalog never consult the user's config.
#[derive(Clone)]
pub(crate) struct IcedWorkSource {
    pub config: PathBuf,
    pub workspaces: Vec<gitterm_core::workspace_config::WorkspaceConfig>,
    pub repos: Vec<gitterm_core::workspace_config::RepoConfig>,
    pub owner: gpui::WeakEntity<crate::WorkspaceOwner>,
}
impl gpui::Global for IcedWorkSource {}

/// Workspace/repository facts unavailable from the read-only Work reducer.
/// Paths are the same canonical repo paths supplied to the core summary.
#[derive(Clone, Debug)]
pub struct WorkspaceFacts {
    pub id: u64,
    pub name: String,
    pub dir: PathBuf,
    pub home: PathBuf,
    pub local: bool,
    pub repos: Vec<RepoFacts>,
    pub removed: Vec<PathBuf>,
    pub auto_added: Vec<PathBuf>,
    pub open_chats: HashSet<String>,
    pub owned_chats: Vec<String>,
    pub chat_owners: HashMap<String, String>,
    /// Host counts running tasks and chats, excluding chats counted as tasks.
    pub running_by_repo: HashMap<PathBuf, usize>,
    pub selected_checkout: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct RepoFacts {
    pub dir: PathBuf,
    pub local: bool,
    pub run_command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkSidebarOutcome {
    OpenChat {
        workspace: u64,
        chat: String,
    },
    Focus {
        workspace: u64,
        focus: Focus,
    },
    AddRepo,
    /// A listed repo is removed; an unlisted touched repo is hidden.
    RemoveRepo {
        workspace: u64,
        repo: PathBuf,
    },
    UndoAutoRepo {
        workspace: u64,
        repos: Vec<PathBuf>,
    },
    EditRunCommand {
        repo: PathBuf,
        command: Option<String>,
    },
    DismissAutoRepos {
        workspace: u64,
    },
    RunRepoCommand {
        repo: PathBuf,
    },
    OpenReview {
        checkout: PathBuf,
        vs_base: bool,
    },
    OpenEditor {
        dir: PathBuf,
    },
    GoToChatOwner {
        chat: String,
    },
    MakeWorkspace {
        chat: String,
    },
    Refresh {
        workspace: u64,
    },
    OpenFind,
    OpenDocument {
        workspace: u64,
        path: PathBuf,
    },
    OpenTask {
        task: String,
    },
    OpenUrl {
        url: String,
    },
    /// A PR or issue in the embedded Page, in its owner's or workspace's login
    /// store (Iced `Pr(OpenInViewer)`, `Issue(Open)`), not the OS browser.
    OpenPage {
        url: String,
        store: gitterm_core::page_store::PageStore,
    },
    /// The skills · MCPs · hooks line opens the Agent sidebar.
    OpenAgentConfig {
        workspace: u64,
    },
}

impl WorkSidebarOutcome {
    pub fn preview_error(&self) -> Option<String> {
        let action = match self {
            Self::AddRepo => "Add repo",
            Self::RemoveRepo { .. } => "Remove repo",
            Self::UndoAutoRepo { .. } => "Undo auto-added repos",
            Self::EditRunCommand { .. } => "Save run command",
            Self::DismissAutoRepos { .. } => "Dismiss auto-added repos",
            Self::OpenEditor { .. } => "Open in editor",
            Self::MakeWorkspace { .. } => "Make workspace",
            // RunRepoCommand and OpenTask are carried out by the shell (as in
            // Iced), so a "not available" banner here would contradict them.
            _ => return None,
        };
        Some(format!("{action} · not available in preview"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Fold {
    Repo(PathBuf),
    Pr(String),
    Done(String),
    Branch(PathBuf),
    Chat(String),
    Focus,
    Older,
    Also,
    Documents,
    AllDocuments,
    Document(String),
    Copies(String),
    Issues(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Emit(WorkSidebarOutcome),
    Toggle(Fold),
    Edit(PathBuf),
    DismissAuto,
    Preview(String),
    /// Iced `Pr(ActionOpen)`: Approve, Merge and Ready for review open a
    /// confirmation panel first.
    Confirm(gitterm_core::merge_plan::PrKey, ActionKind),
    /// Iced's disabled Approve/Merge (checks gate, draft): a click does nothing.
    Inert,
}

#[derive(Clone, Debug)]
struct Control {
    label: String,
    action: Action,
}

#[derive(Clone, Debug)]
pub(crate) struct PullRequestGroup {
    pub root: String,
    pub title: String,
    pub prs: Vec<gitterm_core::prs::WatchedPr>,
}

#[derive(Default)]
struct Row {
    label: String,
    small: bool,
    pr_number: Option<(u64, usize)>,
    pr_link: Option<Action>,
    repo_prefix: Option<String>,
    badges: Vec<String>,
    age: Option<String>,
    details: Vec<String>,
    action: Option<Action>,
    run: Option<Control>,
    controls: Vec<Control>,
    detail_controls: Vec<Control>,
    detail_rows: Vec<Row>,
    fold: Option<Fold>,
    expanded: bool,
    selected: bool,
    quiet: bool,
    heading: bool,
    indent: bool,
    editor: Option<PathBuf>,
    chat: bool,
    /// Index into `badges` of the one fact that may shrink and ellipsize so
    /// the row's title keeps its width (a chat's fan-out line).
    shrinking_badge: Option<usize>,
}

impl Row {
    fn text(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    fn control(&mut self, label: impl Into<String>, action: Action) {
        self.controls.push(Control {
            label: label.into(),
            action,
        });
    }
}

pub struct WorkSidebar {
    workspace: WorkspaceFacts,
    /// Shared reducer; blocking reads run through the read-only data adapter.
    state: Rc<RefCell<State>>,
    open: HashSet<Fold>,
    dismissed_auto: Vec<PathBuf>,
    editing: Option<(PathBuf, Entity<InputState>)>,
    edit_subscription: Option<Subscription>,
    tasks: Vec<gitterm_core::tasks::TaskRecord>,
    workers: Vec<gitterm_core::tasks::Delegation>,
    running_task_ids: HashSet<String>,
    running_chats: Vec<(PathBuf, Option<String>)>,
    repo_recency: Vec<(PathBuf, std::time::SystemTime)>,
    source: Option<IcedWorkSource>,
    pull_requests: Vec<PullRequestGroup>,
    previous_data: Option<crate::data::IcedWorkData>,
    pr_plans: HashMap<String, gitterm_core::merge_plan::MergePlan>,
    pr_accounts: BTreeMap<String, String>,
    issues: gitterm_core::issues::IssueTitlesFile,
    collapsed_issue_groups: Vec<gitterm_core::issues::IssueGroup>,
    loading: bool,
    loaded_at: Option<std::time::Instant>,
    focus: Focus,
    error: Option<String>,
    /// The checkout whose review is shown, as Iced's `open_review_checkout`.
    review_checkout: Option<PathBuf>,
    agent_line: Option<String>,
    confirm: Option<PrActionConfirm>,
    /// The open panel's focus, for Escape.
    confirm_focus: Option<gpui::FocusHandle>,
    /// gh's active login, as Iced reads it from a PR poll (`gh api user`).
    /// The preview never polls or runs gh, so nothing sets it yet.
    active_login: Option<String>,
}

impl WorkSidebar {
    /// Iced `page_navigation_context`'s routes: mapped GitHub owners, and the
    /// Linear workspaces of cached issues.
    pub(crate) fn page_routes(&self) -> crate::content::window::PageRoutes {
        crate::content::window::PageRoutes {
            github_accounts: self.pr_accounts.clone(),
            linear_workspaces: self
                .issues
                .issues
                .values()
                .filter_map(|issue| {
                    issue.workspace.clone().or_else(|| {
                        issue
                            .url
                            .as_deref()
                            .and_then(gitterm_core::issues::workspace_from_issue_url)
                    })
                })
                .collect(),
        }
    }
    pub fn new(workspace: WorkspaceFacts, state: Rc<RefCell<State>>) -> Self {
        Self {
            workspace,
            state,
            open: HashSet::new(),
            dismissed_auto: vec![],
            editing: None,
            edit_subscription: None,
            tasks: vec![],
            workers: vec![],
            running_task_ids: HashSet::new(),
            running_chats: vec![],
            repo_recency: vec![],
            source: None,
            pull_requests: vec![],
            previous_data: None,
            pr_plans: HashMap::new(),
            pr_accounts: BTreeMap::new(),
            issues: Default::default(),
            collapsed_issue_groups: vec![],
            loading: false,
            loaded_at: None,
            focus: Default::default(),
            error: None,
            review_checkout: None,
            agent_line: None,
            confirm: None,
            confirm_focus: None,
            active_login: None,
        }
    }

    pub fn set_agent_line(&mut self, line: Option<String>, cx: &mut Context<Self>) {
        if self.agent_line != line {
            self.agent_line = line;
            cx.notify();
        }
    }

    pub fn review_checkout(&self) -> Option<&std::path::Path> {
        self.review_checkout.as_deref()
    }

    pub fn set_review_checkout(&mut self, checkout: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.review_checkout != checkout {
            self.review_checkout = checkout;
            cx.notify();
        }
    }

    pub fn has_loaded(&self) -> bool {
        self.loaded_at.is_some()
    }

    /// Iced's chat index as last read for this workspace (empty until loaded).
    pub fn indexed_chats(&self) -> &[gitterm_core::chats::ChatIndexEntry] {
        self.previous_data
            .as_ref()
            .map_or(&[], |data| data.indexed_chats.as_slice())
    }

    pub fn tasks(&self) -> &[gitterm_core::tasks::TaskRecord] {
        &self.tasks
    }

    pub fn workers(&self) -> &[gitterm_core::tasks::Delegation] {
        &self.workers
    }

    /// Distinct live repo roots in Iced's newest-chat order, with exact activity
    /// times. Available after the background Work load; shortcut wiring is host-owned.
    pub fn repo_recency(&self) -> &[(PathBuf, std::time::SystemTime)] {
        &self.repo_recency
    }

    fn load(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.source.is_none() {
            self.source = cx.try_global::<IcedWorkSource>().cloned();
        }
        let Some(source) = self.source.clone() else {
            return;
        };
        let Some(saved) = self
            .workspace
            .id
            .checked_sub(1)
            .and_then(|id| source.workspaces.get(id as usize))
            .cloned()
        else {
            let message = format!(
                "Load Iced Work {}: workspace {} is missing from the imported catalog",
                source.config.display(),
                self.workspace.id
            );
            if self.error.as_ref() != Some(&message) {
                eprintln!("{message}");
                self.error = Some(message);
            }
            return;
        };
        if self.loading
            || (!force
                && self
                    .loaded_at
                    .is_some_and(|at| at.elapsed() < gitterm_core::work::state::VISIBLE_POLL))
        {
            return;
        }
        self.loading = true;
        let id = self.workspace.id;
        let focus = self.focus.clone();
        let requested_focus = focus.clone();
        // The catalog is already loaded. Never display an invented Repos 0
        // while the much larger transcript/Git load is pending.
        self.workspace.local = !saved.is_legacy_remote()
            && !matches!(
                saved.location,
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
            );
        self.workspace.home = saved.home_dir().into();
        self.workspace.dir = saved.dir.clone().into();
        self.workspace.owned_chats = saved.chats.clone();
        self.workspace.open_chats = saved
            .tabs
            .iter()
            .filter_map(|t| t.chat_session_id.clone())
            .collect();
        if self.previous_data.is_none() {
            self.workspace.repos = saved
            .repos
            .iter()
            .filter(|_| self.workspace.local)
            .map(|dir| RepoFacts {
                dir: dir.into(),
                local: self.workspace.local,
                run_command: source
                    .repos
                    .iter()
                    .find(|r| r.dir == *dir && !matches!(r.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })))
                    .and_then(|r| r.run_command.clone()),
            })
            .collect();
        }
        let config = source.config.clone();
        let previous = self.previous_data.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let job = cx.background_executor().spawn(async move {
            let plans = gitterm_core::merge_plan::load_plans(&gitterm_core::merge_plan::plans_dir(
                &source.config,
            ));
            let accounts = read_pr_accounts(&source.config);
            let result = crate::data::IcedWorkData::read(
                &source.config,
                id,
                &saved,
                focus,
                previous.as_ref(),
                |data| {
                    _ = sender.send((Ok(data.clone()), plans.clone(), accounts.clone()));
                },
            );
            _ = sender.send((result, plans, accounts));
        });
        cx.spawn(async move |view, cx| {
            while let Some((result, plans, accounts)) = receiver.recv().await {
                let updated = view.update(cx, |this, cx| {
                    if this.focus != requested_focus {
                        return false;
                    }
                    this.loaded_at = Some(std::time::Instant::now());
                    match result {
                        Ok(data) => {
                            this.previous_data = Some(data.clone());
                            this.pr_plans = plans;
                            this.pr_accounts = accounts;
                            data.apply(&mut this.state.borrow_mut());
                            this.workspace = data.facts;
                            this.tasks = data.tasks;
                            this.workers = data.workers;
                            this.running_task_ids = data.running_task_ids;
                            this.repo_recency = data.repo_recency;
                            this.pull_requests = data.pull_requests;
                            this.issues = data.issues;
                            this.collapsed_issue_groups = data.collapsed_issue_groups;
                            this.error = None;
                        }
                        Err(error) => {
                            let message = format!("Load Iced Work {}: {error:#}", config.display());
                            eprintln!("{message}");
                            this.error = Some(message);
                        }
                    }
                    cx.notify();
                    true
                });
                if !matches!(updated, Ok(true)) {
                    break;
                }
            }
            job.await;
            _ = view.update(cx, |this, cx| {
                this.loading = false;
                if this.focus != requested_focus {
                    this.load(true, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn emit(&mut self, outcome: WorkSidebarOutcome, cx: &mut Context<Self>) {
        let error = outcome.preview_error().or_else(|| match &outcome {
            WorkSidebarOutcome::OpenChat { chat, .. }
                if !self.workspace.open_chats.contains(chat) =>
            {
                Some("Open chat · not available in preview".into())
            }
            _ => None,
        });
        if let Some(error) = error {
            self.notice(error, cx);
            return;
        }
        match &outcome {
            WorkSidebarOutcome::Refresh { .. } => self.load(true, cx),
            WorkSidebarOutcome::Focus { focus, .. } => {
                self.open.remove(&Fold::Focus);
                self.focus = focus.clone();
                // The shell's focus outcome also invalidates its reducer.
                // Complete a new read after that notification has returned.
                self.loaded_at = None;
            }
            WorkSidebarOutcome::OpenUrl { url }
                if url.starts_with("https://") || url.starts_with("http://") =>
            {
                cx.open_url(url)
            }
            WorkSidebarOutcome::OpenDocument { workspace, path } => {
                if let Some(source) = &self.source {
                    _ = source.owner.update(cx, |owner, cx| {
                        owner.open_file(crate::data::WorkspaceId(*workspace), path.clone(), cx)
                    });
                }
            }
            _ => {}
        }
        cx.emit(outcome);
    }

    fn notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message.clone());
        if let Some(source) = &self.source {
            _ = source.owner.update(cx, |owner, cx| {
                owner.error = Some(message);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Call after the host reduces Work intents or changes workspace facts.
    /// Retain one entity per workspace so disclosures and drafts survive switches.
    pub fn refresh(&mut self, workspace: WorkspaceFacts, cx: &mut Context<Self>) {
        assert_eq!(workspace.id, self.workspace.id, "one sidebar per workspace");
        if workspace.auto_added != self.workspace.auto_added {
            self.dismissed_auto.clear();
        }
        self.workspace = workspace;
        cx.notify();
    }

    fn rows(&self) -> Vec<Row> {
        let state = self.state.borrow();
        let pane = state.pane(self.workspace.id);
        let mut title = Row::text(&self.workspace.name);
        title.heading = true;
        title.control("✎", Action::Preview("✎".into()));
        title.control(
            "↻",
            Action::Emit(WorkSidebarOutcome::Refresh {
                workspace: self.workspace.id,
            }),
        );
        if self.loading {
            title.badges.push("reading…".into());
        }
        let mut rows = vec![title];
        if let Some(error) = &self.error {
            rows.push(Row::text(error));
        }
        self.pull_request_rows(&mut rows);
        self.repo_rows(pane, &mut rows);
        // What the agent has here (Iced TRU-192): opens the Agent view.
        if let Some(line) = &self.agent_line {
            let mut row = Row::text("");
            row.small = true;
            row.control(
                line.clone(),
                Action::Emit(WorkSidebarOutcome::OpenAgentConfig {
                    workspace: self.workspace.id,
                }),
            );
            rows.push(row);
        }
        let Some(pane) = pane else {
            rows.push(Row::text("Loading…"));
            return rows;
        };
        let summary = &pane.summary;
        if !summary.has_facts {
            let mut row = Row::text(format!(
                "No chat facts for this workspace yet ({} chats). Chat tabs record what they touch as they run; for older chats, run Index older chats.",
                summary.chat_count
            ));
            row.detail_controls.push(Control {
                label: "Index older chats".into(),
                action: Action::Preview("Index older chats".into()),
            });
            rows.push(row);
        }
        if summary.owned {
            let roots = if self.workspace.owned_chats.is_empty() {
                summary.coordinators.len()
            } else {
                self.workspace.owned_chats.len()
            };
            let mut ownership = Row::text(format!(
                "owns {roots} chat{} · new chats start in {}",
                if roots == 1 { "" } else { "s" },
                summary::folder_name(&self.workspace.dir)
            ));
            ownership.small = true;
            ownership.quiet = true;
            rows.push(ownership);
        } else if !summary.coordinators.is_empty() {
            let mut focus = Row::text(format!(
                "Focus: {}",
                summary::shorten(&summary.focus_label(), 46)
            ));
            focus.expanded = self.open.contains(&Fold::Focus);
            focus
                .label
                .push_str(if focus.expanded { " ▴" } else { " ▾" });
            focus.action = Some(Action::Toggle(Fold::Focus));
            rows.push(focus);
            if self.open.contains(&Fold::Focus) {
                for (label, focus) in [
                    ("Most recently active chat".into(), Focus::Latest),
                    (
                        format!(
                            "All recent work · {} chats, last 7 days",
                            summary.recent_count()
                        ),
                        Focus::AllRecent,
                    ),
                ] {
                    let mut row = Row::text(label);
                    row.selected = summary.focus == focus;
                    row.action = Some(Action::Emit(WorkSidebarOutcome::Focus {
                        workspace: self.workspace.id,
                        focus,
                    }));
                    rows.push(row);
                }
                rows.push(Row::text(
                    "Repos and Issues show this work; or click a chat in Chats",
                ));
            }
        }
        if !summary.coordinators.is_empty() {
            let mut heading = Row::text("Chats");
            heading.heading = true;
            heading.badges.push(if summary.owned {
                summary.coordinators.len().to_string()
            } else if summary.recent_count() == 0 {
                "none in the last 7 days".into()
            } else {
                format!("{} in the last 7 days", summary.recent_count())
            });
            rows.push(heading);
            if summary.owned {
                for chat in &summary.coordinators {
                    rows.push(self.chat_row(chat, false, true, None));
                }
            } else {
                let recent: Vec<_> = summary.coordinators.iter().filter(|c| c.recent).collect();
                for chat in recent.iter().take(12) {
                    rows.push(self.chat_row(chat, summary.focused.contains(&chat.id), false, None));
                }
                Self::more_row(recent.len(), 12, &mut rows);
                let older: Vec<_> = summary.coordinators.iter().filter(|c| !c.recent).collect();
                if !older.is_empty() {
                    let expanded = self.open.contains(&Fold::Older);
                    let mut row = Row::text(format!("{} older {}", older.len(), marker(expanded)));
                    row.action = Some(Action::Toggle(Fold::Older));
                    rows.push(row);
                    if expanded {
                        for chat in older.iter().take(20) {
                            rows.push(self.chat_row(
                                chat,
                                summary.focused.contains(&chat.id),
                                false,
                                None,
                            ));
                        }
                        Self::more_row(older.len(), 20, &mut rows);
                    }
                }
            }
        }
        self.document_rows(&state, &mut rows);
        if !summary.also_worked.is_empty() {
            let mut also = Row::text("Also worked here");
            also.heading = true;
            also.badges.push(summary.also_worked.len().to_string());
            also.fold = Some(Fold::Also);
            also.expanded = self.open.contains(&Fold::Also);
            also.action = Some(Action::Toggle(Fold::Also));
            rows.push(also);
            if self.open.contains(&Fold::Also) {
                for item in summary.also_worked.iter().take(12) {
                    rows.push(self.chat_row(
                        &item.coordinator,
                        summary.focused.contains(&item.coordinator.id),
                        summary.owned,
                        Some(item.edits_here),
                    ));
                }
            }
        }
        self.issue_rows(&summary.issue_keys, &mut rows);
        for error in &pane.errors {
            rows.push(Row::text(summary::shorten(error, 200)));
        }
        self.task_rows(&mut rows);
        self.attention_rows(&mut rows);
        rows
    }

    fn pull_request_rows(&self, rows: &mut Vec<Row>) {
        use gitterm_core::{
            merge_plan::PrState,
            prs::{ChecksState, ReviewDecision},
        };
        let now = std::time::SystemTime::now().into();
        let groups: Vec<_> = self
            .pull_requests
            .iter()
            .filter_map(|group| {
                let prs: Vec<_> = group
                    .prs
                    .iter()
                    .filter(|pr| !pr.done() || gitterm_core::prs::done_pr_shown(pr.done_at(), now))
                    .collect();
                (!prs.is_empty()).then_some((group, prs))
            })
            .collect();
        if groups.is_empty() {
            return;
        }
        let several = groups.len() > 1;
        let mut heading = Row::text("Pull requests");
        heading.heading = true;
        let count = groups
            .iter()
            .flat_map(|(_, prs)| prs)
            .filter(|pr| !pr.done())
            .count();
        heading.badges.push(format!("{count} open"));
        let number_chars = groups
            .iter()
            .flat_map(|(group, prs)| {
                let done_open = self.open.contains(&Fold::Done(group.root.clone()));
                prs.iter().filter(move |pr| !pr.done() || done_open)
            })
            .map(|pr| pr.key.number.to_string().len() + 1)
            .max()
            .unwrap_or(0);
        rows.push(heading);
        for (group, prs) in groups {
            let briefs = group
                .prs
                .iter()
                .filter_map(|pr| pr.brief().map(|b| (pr.key.id(), b)))
                .collect();
            let ordered = self
                .pr_plans
                .get(&group.root)
                .map(|plan| gitterm_core::merge_plan::order_plan(plan, &briefs))
                .unwrap_or_default();
            let done = prs.iter().filter(|pr| pr.done()).count();
            let done_fold = Fold::Done(group.root.clone());
            let done_open = self.open.contains(&done_fold);
            let mut label = Row::text(summary::shorten(&group.title, 50));
            label.small = true;
            label.quiet = true;
            if done > 0 {
                let merged = prs
                    .iter()
                    .filter(|pr| pr.snapshot.state() == Some(PrState::Merged))
                    .count();
                let closed = done - merged;
                let count = match (merged, closed) {
                    (m, 0) => format!("{m} merged"),
                    (0, c) => format!("{c} closed"),
                    (m, c) => format!("{m} merged · {c} closed"),
                };
                let target = if several {
                    &mut label
                } else {
                    rows.last_mut().unwrap()
                };
                target.control(
                    format!("{count} {}", marker(done_open)),
                    Action::Toggle(done_fold),
                );
            }
            if several {
                rows.push(label);
            }
            for pr in prs
                .iter()
                .filter(|pr| !pr.done())
                .chain(prs.iter().filter(|pr| pr.done() && done_open))
            {
                let core = pr.snapshot.core.as_ref();
                let mut row = Row::text(
                    core.map(|c| c.title.as_str())
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| pr.key.label()),
                );
                row.pr_number = Some((pr.key.number, number_chars));
                row.pr_link = Some(Action::Emit(WorkSidebarOutcome::OpenPage {
                    url: pr.key.url(),
                    store: gitterm_core::page_store::PageStore::github(
                        pr.key.owner(),
                        &self.pr_accounts,
                    ),
                }));
                let plan_row = ordered.iter().find(|row| row.entry.key() == pr.key);
                let next = plan_row
                    .is_some_and(|row| row.status == gitterm_core::merge_plan::RowStatus::Next);
                let gate = RowGate::of(Some(pr));
                let draft = gate.draft;
                let gated = |name: &str| gate.label(name);
                // Iced enables Approve and Merge only for an open PR whose
                // checks pass; otherwise the label names why and a click is inert.
                let can_act = gate.can_act();
                let confirm = |kind| {
                    if can_act {
                        Action::Confirm(pr.key.clone(), kind)
                    } else {
                        Action::Inert
                    }
                };
                if next {
                    row.control(gated("Merge"), confirm(ActionKind::Merge));
                }
                let fold = Fold::Pr(pr.key.id());
                row.expanded = self.open.contains(&fold);
                row.fold = Some(fold.clone());
                row.action = Some(Action::Toggle(fold));
                if !row.expanded {
                    row.repo_prefix = Some(format!(
                        "{} · ",
                        pr.key.repo.rsplit('/').next().unwrap_or(&pr.key.repo)
                    ));
                }
                if core.is_some() && !pr.done() {
                    match pr.snapshot.checks() {
                        ChecksState::Pass => row.badges.push("✓".into()),
                        ChecksState::Fail => row.badges.push("✗".into()),
                        ChecksState::Pending => row.badges.push("●".into()),
                        ChecksState::None => {}
                    }
                    if pr.snapshot.review_decision() == ReviewDecision::ChangesRequested {
                        row.badges.push("changes requested".into());
                    }
                    if pr.unseen_comments > 0 {
                        row.badges.push(format!("{} new", pr.unseen_comments));
                    }
                    if pr.snapshot.conflicting() == Some(true) {
                        row.badges.push("conflicts".into());
                    }
                }
                if pr.snapshot.state() == Some(PrState::Draft) {
                    row.badges.push("draft".into());
                }
                if pr.error.is_some() {
                    row.badges.push("not read".into());
                }
                if row.expanded {
                    let mut meta = vec![pr.key.repo.clone()];
                    if let Some(core) = core {
                        if !core.head_ref.is_empty() && !core.base_ref.is_empty() {
                            meta.push(format!("{} → {}", core.head_ref, core.base_ref));
                        }
                        if !core.author.is_empty() {
                            meta.push(core.author.clone());
                        }
                    }
                    row.details.push(meta.join(" · "));
                    if let Some(plan_row) = plan_row {
                        use gitterm_core::merge_plan::RowStatus;
                        match &plan_row.status {
                            RowStatus::Held(hold) => {
                                let mut detail = Row::text(format!("hold: {hold}"));
                                detail.control("clear hold", Action::Preview("clear hold".into()));
                                row.detail_rows.push(detail);
                            }
                            RowStatus::Blocked(waits) => row.details.push(format!(
                                "waits for {}{}",
                                waits.join(", "),
                                if plan_row.stacked { " (stacked)" } else { "" }
                            )),
                            RowStatus::Unordered(reason) => {
                                row.details.push(format!("unordered: {reason}"))
                            }
                            _ => {}
                        }
                        row.details.extend(plan_row.entry.note.clone());
                    }
                    for news in pr.open_news() {
                        let mut detail =
                            Row::text(format!("● {}", summary::shorten(&news.text, 70)));
                        detail.age = summary::parse_stamp(&news.at).map(summary::age_label);
                        if news.told {
                            detail.badges.push("sent to chat ✓".into());
                        }
                        detail.control("×", Action::Preview("×".into()));
                        row.detail_rows.push(detail);
                    }
                    let own = pr_actions::own_pr(
                        &pr.key,
                        Some(pr),
                        &self.pr_accounts,
                        self.active_login.as_deref(),
                    );
                    let mut controls =
                        vec![("Review".to_string(), Action::Preview("Review".to_string()))];
                    if !own {
                        controls.push((gated("Approve"), confirm(ActionKind::Approve)));
                    }
                    if !next {
                        controls.push((gated("Merge"), confirm(ActionKind::Merge)));
                    }
                    if draft {
                        controls.push((
                            "Ready for review".into(),
                            Action::Confirm(pr.key.clone(), ActionKind::Ready),
                        ));
                    }
                    for (label, action) in controls {
                        row.detail_controls.push(Control { label, action });
                    }
                    row.detail_controls.push(Control {
                        label: "Open in browser".into(),
                        action: Action::Emit(WorkSidebarOutcome::OpenUrl { url: pr.key.url() }),
                    });
                    if let Some(error) = &pr.error {
                        row.details
                            .push(summary::shorten(&format!("not read: {error}"), 160));
                    }
                }
                rows.push(row);
            }
        }
    }

    fn issue_rows(&self, keys: &[String], rows: &mut Vec<Row>) {
        let groups = self
            .issues
            .grouped_keys(keys, std::time::SystemTime::now().into());
        let count: usize = groups.iter().map(|(_, keys)| keys.len()).sum();
        if count == 0 {
            return;
        }
        let mut heading = Row::text("Issues");
        heading.heading = true;
        heading.badges.push(count.to_string());
        rows.push(heading);
        for (group, keys) in groups {
            let fold = Fold::Issues(group.label().into());
            let expanded =
                self.open.contains(&fold) == self.collapsed_issue_groups.contains(&group);
            let mut row = Row::text(group.label());
            row.heading = true;
            row.badges.push(keys.len().to_string());
            row.expanded = expanded;
            row.fold = Some(fold.clone());
            row.action = Some(Action::Toggle(fold));
            rows.push(row);
            if expanded {
                for key in keys {
                    let issue = self.issues.issues.get(key);
                    let mut row = Row::text(match issue.and_then(|issue| issue.title.as_deref()) {
                        Some(title) => format!("{key}  {}", summary::shorten(title, 60)),
                        None => key.clone(),
                    });
                    row.badges
                        .extend(issue.and_then(|issue| issue.state.clone()));
                    if let Some(issue) = issue
                        && let Some(url) = issue.url.clone()
                    {
                        let workspace = issue
                            .workspace
                            .clone()
                            .or_else(|| gitterm_core::issues::workspace_from_issue_url(&url));
                        let store =
                            gitterm_core::page_store::PageStore::linear(workspace.as_deref());
                        row.action =
                            Some(Action::Emit(WorkSidebarOutcome::OpenPage { url, store }));
                    }
                    rows.push(row);
                }
            }
        }
    }

    fn task_rows(&self, rows: &mut Vec<Row>) {
        if !self.tasks.is_empty() {
            let mut header = Row::text("Tasks");
            header.heading = true;
            header.badges.push(self.tasks.len().to_string());
            rows.push(header);
            for task in crate::tasks::switcher_rows(&self.tasks) {
                let mut row = Row::text(task.issue.map_or_else(
                    || task.title.clone(),
                    |issue| format!("{issue} · {}", task.title),
                ));
                row.badges.push(task.state.into());
                if task.attention {
                    row.badges.push("!".into());
                }
                if task.unread {
                    row.badges.push("•".into());
                }
                row.details.push(task.branch);
                row.action = Some(Action::Emit(WorkSidebarOutcome::OpenTask {
                    task: task.task_id,
                }));
                rows.push(row);
            }
        }
        if !self.workers.is_empty() {
            let mut header = Row::text("Workers");
            header.heading = true;
            header.badges.push(self.workers.len().to_string());
            rows.push(header);
            for worker in &self.workers {
                use gitterm_core::tasks::{DelegationChild, DelegationStatus};
                let mut row = Row::text(summary::shorten(&worker.brief, 70));
                row.badges.push(worker.kind.label().into());
                row.badges.push(
                    match &worker.status {
                        DelegationStatus::Requested => "Queued",
                        DelegationStatus::Running => "Running",
                        DelegationStatus::Completed => "Completed",
                        DelegationStatus::Failed { .. } => "Failed",
                        DelegationStatus::Interrupted => "Interrupted",
                        DelegationStatus::Cancelled => "Cancelled",
                        DelegationStatus::Blocked => "Blocked",
                    }
                    .into(),
                );
                if let DelegationChild::TaskSession {
                    task_id,
                    conversation,
                    ..
                } = &worker.child
                {
                    row.action = Some(Action::Emit(if let Some(conversation) = conversation {
                        WorkSidebarOutcome::OpenChat {
                            workspace: self.workspace.id,
                            chat: conversation.session_id.clone(),
                        }
                    } else {
                        WorkSidebarOutcome::OpenTask {
                            task: task_id.clone(),
                        }
                    }));
                }
                rows.push(row);
            }
        }
    }

    fn document_rows(&self, state: &State, rows: &mut Vec<Row>) {
        let Some(documents) = state
            .documents(self.workspace.id)
            .and_then(|d| d.result.as_ref())
        else {
            return;
        };
        if documents.documents.is_empty() {
            return;
        }
        let mut header = Row::text("Documents");
        header.heading = true;
        header.badges.push(documents.documents.len().to_string());
        header.fold = Some(Fold::Documents);
        header.expanded = self.open.contains(&Fold::Documents);
        header.action = Some(Action::Toggle(Fold::Documents));
        rows.push(header);
        if !self.open.contains(&Fold::Documents) {
            return;
        }
        let all = self.open.contains(&Fold::AllDocuments);
        for doc in documents
            .documents
            .iter()
            .take(if all { usize::MAX } else { 5 })
        {
            let newest = doc.newest();
            let fold = Fold::Document(doc.key.clone());
            let copies = Fold::Copies(doc.key.clone());
            let expanded = self.open.contains(&fold);
            let copies_open = self.open.contains(&copies);
            let mut row = Row::text(summary::shorten(&doc.title, 60));
            row.age = Some(summary::age_label(newest.modified));
            row.fold = Some(fold);
            row.expanded = expanded;
            row.action = Some(Action::Emit(WorkSidebarOutcome::OpenDocument {
                workspace: self.workspace.id,
                path: newest.path.clone(),
            }));
            if doc.copies.len() > 1 && (expanded || copies_open || doc.differ) {
                row.control(
                    format!(
                        "{} copies{} {}",
                        doc.copies.len(),
                        if doc.differ { " differ" } else { "" },
                        marker(copies_open)
                    ),
                    Action::Toggle(copies),
                );
            }
            if expanded {
                row.details
                    .push(format!("{} · {}", newest.source.label(), doc.rel));
            }
            rows.push(row);
            if copies_open {
                for (index, copy) in doc.copies.iter().enumerate() {
                    let mut row = Row::text(format!(
                        "{}{} · {}",
                        if index == 0 { "newest · " } else { "" },
                        copy.source.label(),
                        summary::age_label(copy.modified)
                    ));
                    row.indent = true;
                    row.action = Some(Action::Emit(WorkSidebarOutcome::OpenDocument {
                        workspace: self.workspace.id,
                        path: copy.path.clone(),
                    }));
                    rows.push(row);
                }
            }
        }
        if documents.documents.len() > 5 {
            let mut row = Row::text(if all {
                "▾ show fewer".into()
            } else {
                format!("▸ {} more", documents.documents.len() - 5)
            });
            row.action = Some(Action::Toggle(Fold::AllDocuments));
            rows.push(row);
        }
        if documents.truncated {
            rows.push(Row::text(
                "Stopped reading at the scan's limit; some documents may be missing.",
            ));
        }
    }

    fn attention_rows(&self, rows: &mut Vec<Row>) {
        let tasks = crate::tasks::attention_tasks(&self.tasks);
        if tasks.is_empty() {
            return;
        }
        let mut header = Row::text("Needs you");
        header.heading = true;
        header.badges.push(tasks.len().to_string());
        rows.push(header);
        for task in tasks {
            let (_, icon, label) = task
                .attention
                .reason
                .map(crate::tasks::attention_presentation)
                .unwrap_or((3, "✓", "Ready to review"));
            let mut row = Row::text(format!("{icon} {}", task.title));
            row.badges.push(label.into());
            row.action = Some(Action::Emit(WorkSidebarOutcome::OpenTask {
                task: task.task_id.clone(),
            }));
            rows.push(row);
        }
    }

    fn more_row(count: usize, limit: usize, rows: &mut Vec<Row>) {
        if count > limit {
            let mut row = Row::text(format!("… {} more in Find", count - limit));
            row.action = Some(Action::Emit(WorkSidebarOutcome::OpenFind));
            rows.push(row);
        }
    }

    fn repo_rows(&self, pane: Option<&Pane>, rows: &mut Vec<Row>) {
        let ws = &self.workspace;
        let listed: Vec<_> = ws.repos.iter().map(|repo| repo.dir.clone()).collect();
        let docs = ws.dir.join("docs");
        let groups = summary::merge_repo_groups(
            &listed,
            &ws.removed,
            pane.map_or(&[], |p| p.summary.repos.as_slice()),
            pane.map_or(&[], |p| p.branches.as_slice()),
        );
        let mut header = Row::text("Repos");
        header.heading = true;
        header.badges.push(
            groups
                .iter()
                .filter(|group| group.dir != docs)
                .count()
                .to_string(),
        );
        if ws.local {
            header.control("+ Add repo…", Action::Emit(WorkSidebarOutcome::AddRepo));
        }
        rows.push(header);
        if !ws.auto_added.is_empty() && self.dismissed_auto != ws.auto_added {
            let names: Vec<_> = ws
                .auto_added
                .iter()
                .map(|dir| summary::folder_name(dir))
                .collect();
            let mut note = Row::text(format!(
                "{} added to this workspace's repos",
                names.join(", ")
            ));
            note.control(
                "Undo",
                Action::Emit(WorkSidebarOutcome::UndoAutoRepo {
                    workspace: ws.id,
                    repos: ws
                        .auto_added
                        .iter()
                        .filter(|dir| **dir != ws.home)
                        .cloned()
                        .collect(),
                }),
            );
            note.control("×", Action::DismissAuto);
            rows.push(note);
        }
        for group in groups.into_iter().filter(|group| group.dir != docs) {
            let repo = ws.repos.iter().find(|repo| repo.dir == group.dir);
            let local = repo.filter(|repo| repo.local);
            let (gone, live): (Vec<_>, Vec<_>) =
                group.rows.into_iter().partition(|row| row.owned.gone);
            let empty_note = (live.is_empty() && gone.is_empty()).then(|| {
                if group.edits.is_none() {
                    "no edits yet"
                } else if self.loading || pane.is_some_and(|p| p.last_full_refresh.is_none()) {
                    "reading…"
                } else {
                    "no live checkout of this work"
                }
            });
            let fold = Fold::Repo(group.dir.clone());
            let expanded = self.open.contains(&fold);
            let mut row = Row::text(summary::folder_name(&group.dir));
            row.quiet = empty_note.is_some() && group.edits.is_none();
            row.action = Some(Action::Toggle(fold.clone()));
            row.expanded = expanded;
            let removable = group.dir != ws.home;
            if local.is_some() || !live.is_empty() || !gone.is_empty() || removable {
                row.fold = Some(fold);
            }
            if let Some(repo) = local {
                if self
                    .editing
                    .as_ref()
                    .is_some_and(|(dir, _)| *dir == group.dir)
                {
                    row.editor = Some(group.dir.clone());
                } else if let Some(command) = &repo.run_command {
                    row.run = Some(Control {
                        label: format!("▶ {command}"),
                        action: Action::Emit(WorkSidebarOutcome::RunRepoCommand {
                            repo: group.dir.clone(),
                        }),
                    });
                }
                if expanded {
                    row.control(
                        "Open in editor",
                        Action::Emit(WorkSidebarOutcome::OpenEditor {
                            dir: group.dir.clone(),
                        }),
                    );
                    if row.editor.is_none() {
                        row.control(
                            if repo.run_command.is_some() {
                                "✎"
                            } else {
                                "+ run command"
                            },
                            Action::Edit(group.dir.clone()),
                        );
                    }
                }
            }
            if expanded {
                if let Some(oldest) = live.iter().map(|row| row.checked_at).min() {
                    row.age = Some(summary::age_label(oldest));
                }
                if removable {
                    row.control(
                        "×",
                        Action::Emit(WorkSidebarOutcome::RemoveRepo {
                            workspace: ws.id,
                            repo: group.dir.clone(),
                        }),
                    );
                }
            }
            let mut facts = vec![summary::repo_facts(
                group.dir == ws.home,
                group.edits,
                empty_note,
            )];
            if !live.is_empty() {
                facts.push(format!(
                    "{} branch{}",
                    live.len(),
                    if live.len() == 1 { "" } else { "es" }
                ));
            }
            let prs = live.iter().filter(|row| row.pr_label().is_some()).count();
            if prs > 0 {
                facts.push(format!("{prs} PR{}", if prs == 1 { "" } else { "s" }));
            }
            let running = ws.running_by_repo.get(&group.dir).copied().unwrap_or(0)
                + self
                    .running_chats
                    .iter()
                    .filter(|(checkout, task)| {
                        !task
                            .as_ref()
                            .is_some_and(|id| self.running_task_ids.contains(id))
                            && (*checkout == group.dir
                                || live.iter().any(|branch| branch.owned.checkout == *checkout))
                    })
                    .count();
            if running > 0 {
                facts.push(format!("{running} running"));
            }
            row.details.push(
                facts
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            rows.push(row);
            if expanded {
                for branch in live {
                    rows.push(self.branch_row(branch));
                }
                if !gone.is_empty() {
                    let names: Vec<_> = gone
                        .iter()
                        .map(|row| row.owned.branch.as_deref().unwrap_or("(detached)"))
                        .collect();
                    rows.push(Row::text(format!(
                        "▸ {} folder{} gone · {}",
                        gone.len(),
                        if gone.len() == 1 { "" } else { "s" },
                        summary::shorten(&names.join(", "), 60)
                    )));
                }
            }
        }
    }

    fn branch_row(&self, branch: &BranchStatus) -> Row {
        let fold = Fold::Branch(branch.owned.checkout.clone());
        let expanded = self.open.contains(&fold);
        let mut row = Row::text(summary::shorten(
            branch.owned.branch.as_deref().unwrap_or("(detached)"),
            40,
        ));
        row.indent = true;
        row.selected = self
            .review_checkout
            .as_ref()
            .or(self.workspace.selected_checkout.as_ref())
            == Some(&branch.owned.checkout);
        row.fold = Some(fold);
        row.expanded = expanded;
        row.action = Some(Action::Emit(WorkSidebarOutcome::OpenReview {
            checkout: branch.owned.checkout.clone(),
            vs_base: branch.status.as_ref().is_some_and(|s| s.files == 0) && branch.base.is_some(),
        }));
        if let Some(fact) = summary::branch_fact(branch.status.as_ref()) {
            row.badges.push(fact.label());
        }
        if expanded {
            let mut parts = vec![summary::short_checkout(
                &branch.owned,
                dirs::home_dir().as_deref(),
            )];
            match (summary::vs_base_label(branch), &branch.base) {
                (Some(label), _) => parts.push(label),
                (None, None) => parts.push("no base".into()),
                _ => {}
            }
            if let Some(status) = &branch.status {
                parts.push(summary::push_label(status.push_state()));
            }
            parts.extend(branch.pr_label());
            parts.push(branch.owner_label());
            if let Some(at) = branch.last_activity() {
                parts.push(chats::format_age(at));
            }
            row.details.push(parts.join(" · "));
            row.detail_controls.push(Control {
                label: "Open in editor".into(),
                action: Action::Emit(WorkSidebarOutcome::OpenEditor {
                    dir: branch.owned.checkout.clone(),
                }),
            });
        }
        if let Some(error) = &branch.error {
            row.details.push(summary::shorten(error, 160));
        }
        row
    }

    fn chat_row(
        &self,
        chat: &Coordinator,
        focused: bool,
        owning: bool,
        edits_here: Option<u64>,
    ) -> Row {
        let fold = Fold::Chat(chat.id.clone());
        let expanded = self.open.contains(&fold);
        let mut row = Row::text(format!("✦ {}", summary::shorten(&chat.title, 70)));
        row.chat = true;
        row.fold = Some(fold);
        row.expanded = expanded;
        row.selected = focused;
        row.action = Some(if owning {
            Action::Preview(row.label.clone())
        } else {
            Action::Emit(WorkSidebarOutcome::Focus {
                workspace: self.workspace.id,
                focus: Focus::Coordinator(chat.id.clone()),
            })
        });
        if self.workspace.open_chats.contains(&chat.id) {
            row.badges.push("open".into());
        }
        if let Some(edits) = edits_here {
            row.badges.push(format!(
                "{edits} edit{} here",
                if edits == 1 { "" } else { "s" }
            ));
        }
        if let Some(fan_out) = &chat.fan_out {
            row.shrinking_badge = Some(row.badges.len());
            row.badges.push(summary::shorten(fan_out, 28));
        }
        row.badges.push(chats::format_age(chat.active));
        if expanded {
            row.control(
                "↗",
                Action::Emit(WorkSidebarOutcome::OpenChat {
                    workspace: self.workspace.id,
                    chat: chat.id.clone(),
                }),
            );
            if let Some(latest) = &chat.latest {
                row.details
                    .push(format!("↳ {}", summary::shorten(latest, 90)));
            }
            for child in chat.children.iter().take(4) {
                let kind = match child.kind {
                    Some(ChildKind::Worker) => "worker",
                    Some(ChildKind::Review) => "review",
                    Some(ChildKind::Consult) => "consult",
                    Some(ChildKind::Subagent) => "subagent",
                    None => "chat",
                };
                row.details.push(format!(
                    "   {kind} · {} · {}",
                    summary::shorten(&child.title, 50),
                    chats::format_age(child.mtime)
                ));
            }
            if chat.children.len() > 4 {
                row.details
                    .push(format!("   … {} more", chat.children.len() - 4));
            }
        }
        let show_action = if owning {
            !self.workspace.owned_chats.contains(&chat.id)
        } else {
            focused
        };
        if show_action && (expanded || focused) {
            if let Some(owner) = self.workspace.chat_owners.get(&chat.id) {
                row.detail_controls.push(Control {
                    label: format!("→ Go to {}", summary::shorten(owner, 30)),
                    action: Action::Emit(WorkSidebarOutcome::GoToChatOwner {
                        chat: chat.id.clone(),
                    }),
                });
            } else {
                row.detail_controls.push(Control {
                    label: "⊞ Make workspace".into(),
                    action: Action::Emit(WorkSidebarOutcome::MakeWorkspace {
                        chat: chat.id.clone(),
                    }),
                });
            }
        }
        row
    }

    fn activate(&mut self, action: Action) -> Option<WorkSidebarOutcome> {
        match action {
            Action::Emit(WorkSidebarOutcome::RemoveRepo { ref repo, .. })
                if *repo == self.workspace.home =>
            {
                None
            }
            Action::Emit(outcome) => Some(outcome),
            Action::Toggle(fold) => {
                if !self.open.remove(&fold) {
                    self.open.insert(fold);
                }
                None
            }
            Action::DismissAuto => Some(WorkSidebarOutcome::DismissAutoRepos {
                workspace: self.workspace.id,
            }),
            Action::Edit(_) | Action::Preview(_) | Action::Confirm(..) | Action::Inert => None,
        }
    }

    fn dispatch(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Action::Preview(label) = action {
            self.notice(format!("{label} · not available in preview"), cx);
            return;
        }
        if let Action::Confirm(key, kind) = action {
            self.open_confirm(key, kind, window, cx);
            return;
        }
        if let Action::Edit(dir) = action {
            let Some(repo) = self
                .workspace
                .repos
                .iter()
                .find(|repo| repo.dir == dir && repo.local)
            else {
                return;
            };
            let value = repo.run_command.clone().unwrap_or_default();
            let input = cx.new(|cx| {
                let mut input = InputState::new(window, cx).placeholder("npm run dev");
                input.set_value(value, window, cx);
                input
            });
            self.edit_subscription =
                Some(
                    cx.subscribe_in(&input, window, |this, input, event, _, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            let value = input.read(cx).value().to_string();
                            if let Some((repo, _)) = this.editing.take() {
                                this.emit(Self::run_command_outcome(repo, &value), cx);
                            }
                        }
                        cx.notify();
                    }),
                );
            input.update(cx, |input, cx| input.focus(window, cx));
            self.editing = Some((dir, input));
        } else if let Some(outcome) = self.activate(action) {
            self.emit(outcome, cx);
        }
        cx.notify();
    }

    /// Iced `ActionOpen`: the method defaults to the plan's, else merge.
    fn open_confirm(
        &mut self,
        key: gitterm_core::merge_plan::PrKey,
        kind: ActionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.confirm = Some(PrActionConfirm::open(
            key,
            kind,
            self.pr_plans.values(),
            &self.pr_accounts,
        ));
        self.confirm_focus = Some(focus);
        cx.notify();
    }

    /// Iced `ActionMethod`: local to the open panel.
    pub(crate) fn confirm_method(
        &mut self,
        method: gitterm_core::merge_plan::MergeMethod,
        cx: &mut Context<Self>,
    ) {
        if let Some(confirm) = self.confirm.as_mut() {
            confirm.choose_method(method);
            cx.notify();
        }
    }

    /// Iced `ActionClose`: no other effect.
    pub(crate) fn confirm_close(&mut self, cx: &mut Context<Self>) {
        if pr_actions::close_action(&mut self.confirm) {
            self.confirm_focus = None;
        }
        cx.notify();
    }

    /// Iced `ActionRun` would run `gh`; the preview reports it instead, in
    /// the panel's error slot and the shell's notice.
    /// shortcut: the shell has no `ServiceContext` yet, so the notice text is
    /// the preview's own; ask `ServiceContext::pr_action` once it has one.
    pub(crate) fn confirm_run(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.as_mut().filter(|c| c.run_enabled()) else {
            return;
        };
        let message = format!("{} · not available in preview", confirm.action().verb());
        confirm.error = Some(message.clone());
        self.notice(message, cx);
    }

    /// The panel's live inputs: the PR's latest read and where its plan
    /// stands on it (Iced `view_pr_action_confirm`).
    fn confirm_model(&self, confirm: &PrActionConfirm) -> pr_confirm::Model {
        let id = confirm.key.id();
        let group = self
            .pull_requests
            .iter()
            .find(|group| group.prs.iter().any(|pr| pr.key.id() == id));
        let watched = group.and_then(|group| group.prs.iter().find(|pr| pr.key.id() == id));
        let status = group.and_then(|group| {
            let briefs = group
                .prs
                .iter()
                .filter_map(|pr| pr.brief().map(|b| (pr.key.id(), b)))
                .collect();
            let plan = self.pr_plans.get(&group.root)?;
            gitterm_core::merge_plan::order_plan(plan, &briefs)
                .into_iter()
                .find(|row| row.entry.key().id() == id)
                .map(|row| row.status)
        });
        pr_confirm::Model::build(
            confirm,
            watched,
            status.as_ref(),
            self.active_login.as_deref(),
        )
    }

    fn run_command_outcome(repo: PathBuf, value: &str) -> WorkSidebarOutcome {
        let command = value.trim();
        WorkSidebarOutcome::EditRunCommand {
            repo,
            command: (!command.is_empty()).then(|| command.to_string()),
        }
    }

    fn button(&self, id: String, control: Control, repo_row: bool, cx: &Context<Self>) -> Button {
        let p = crate::theme::Palette::current(cx);
        let run = matches!(
            control.action,
            Action::Emit(WorkSidebarOutcome::RunRepoCommand { .. })
        );
        let mono = run
            || matches!(
                control.action,
                Action::Emit(WorkSidebarOutcome::OpenAgentConfig { .. })
            );
        let color = control_color(&control.action, repo_row, &p);
        let tooltip = match control.label.as_str() {
            "↗" => "Open chat",
            "×" => "Remove or dismiss",
            "✎" if matches!(control.action, Action::Preview(_)) => "Rename workspace",
            "✎" => "Edit run command",
            "▸" => "Expand",
            "▾" => "Collapse",
            "↻" => "Refresh",
            _ => &control.label,
        }
        .to_string();
        let preview_label = control.label.clone();
        Button::new(id.clone())
            .debug_selector(move || id.clone())
            .ghost()
            .compact()
            .xsmall()
            .px_1()
            .flex_shrink_0()
            .text_size(px(10.))
            .when(mono, |button| button.font_family("Menlo"))
            .when(repo_row && !mono, |button| {
                button.font_family(".SystemUIFont")
            })
            .text_color(rgb(color))
            .tooltip(tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                let preview = match &control.action {
                    Action::Edit(_) | Action::DismissAuto => true,
                    Action::Emit(outcome) => {
                        outcome.preview_error().is_some()
                            || matches!(
                                outcome,
                                WorkSidebarOutcome::OpenChat { .. }
                                    | WorkSidebarOutcome::GoToChatOwner { .. }
                            )
                    }
                    _ => false,
                };
                this.dispatch(
                    if preview {
                        Action::Preview(preview_label.clone())
                    } else {
                        control.action.clone()
                    },
                    window,
                    cx,
                );
            }))
            .map(|button| {
                if repo_row {
                    // Like Iced, the run command keeps its full width and the
                    // action slots give way, wrapping their text.
                    button
                        .h_auto()
                        .accessibility_label(control.label.clone())
                        .map(|button| {
                            // Buttons keep their intrinsic one-line width; the
                            // slot clips at the row's right edge instead.
                            button.whitespace_nowrap().flex_shrink_0()
                        })
                        .child(div().whitespace_nowrap().child(control.label))
                } else {
                    button.label(control.label)
                }
            })
    }
}

fn age_label(index: usize, age: String, repo: bool, p: &crate::theme::Palette) -> gpui::Div {
    let base = div()
        .debug_selector(move || format!("work-age-{index}"))
        .font_family("Menlo")
        .text_size(px(10.))
        .text_color(rgb(p.muted));
    if repo {
        // Iced's age text wraps between words ("14s" / "ago") when the slot
        // is squeezed: each word is nowrap, the container wraps.
        base.flex()
            .flex_wrap()
            .flex_shrink_1()
            .min_w_0()
            .gap(px(4.))
            .children(
                age.split(' ')
                    .map(|word| div().whitespace_nowrap().child(word.to_string()))
                    .collect::<Vec<_>>(),
            )
    } else {
        base.child(age)
    }
}

fn control_color(action: &Action, repo_row: bool, p: &crate::theme::Palette) -> u32 {
    match action {
        Action::Emit(WorkSidebarOutcome::RunRepoCommand { .. }) => p.green,
        Action::Edit(_)
        | Action::Toggle(_)
        | Action::Inert
        | Action::Emit(WorkSidebarOutcome::RemoveRepo { .. }) => p.muted,
        // Iced draws the repo row's Open in editor in text_muted.
        Action::Emit(WorkSidebarOutcome::OpenEditor { .. }) if repo_row => p.muted,
        _ => p.blue,
    }
}

fn marker(expanded: bool) -> &'static str {
    if expanded { "▾" } else { "▸" }
}

fn read_pr_accounts(config: &std::path::Path) -> BTreeMap<String, String> {
    let path = config.join("github-accounts.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            eprintln!("Read {}: {error}", path.display());
            BTreeMap::new()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => {
            eprintln!("Read {}: {error}", path.display());
            BTreeMap::new()
        }
    }
}

impl EventEmitter<WorkSidebarOutcome> for WorkSidebar {}

impl Render for WorkSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load(false, cx);
        self.running_chats = self
            .source
            .as_ref()
            .and_then(|source| source.owner.upgrade())
            .map(|owner| owner.read(cx).running_work_chats(cx))
            .unwrap_or_default();
        let p = crate::theme::Palette::current(cx);
        let mut body = div()
            .id("work-sidebar")
            .debug_selector(|| "work-sidebar".into())
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_size(px(12.))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.editing.is_some() {
                    this.editing = None;
                    this.edit_subscription = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }));
        for (index, mut row) in self.rows().into_iter().enumerate() {
            let repo = matches!(row.action, Some(Action::Toggle(Fold::Repo(_))));
            let pr = row.pr_number.is_some();
            let compact = row.small || pr || repo;
            let mut label = div()
                .id(format!("work-label-{index}"))
                .debug_selector(|| format!("work-label-{index}"))
                .min_w_0();
            if let Some(prefix) = row.repo_prefix {
                let prefix_len = prefix.len();
                label = label.child(
                    gpui::StyledText::new(format!("{prefix}{}", row.label)).with_highlights([(
                        0..prefix_len,
                        gpui::HighlightStyle {
                            color: Some(rgb(p.muted).into()),
                            ..Default::default()
                        },
                    )]),
                );
            } else {
                label = label.child(row.label);
            }
            if pr {
                label = label
                    .flex_1()
                    .font_family(".SystemUIFont")
                    .text_size(px(12.))
                    .whitespace_normal()
                    .line_clamp(2)
                    .overflow_hidden();
            }
            if row.chat {
                // Wraps by word like Iced's Fill title. A `flex_1` title has a
                // basis of zero, so the fixed facts took the whole row and it
                // wrapped one letter per line. Start from the full row instead
                // and give way to the facts only down to half of it; the
                // fan-out line (shrink factor 2) gives way before the title.
                label = label
                    .flex_grow(1.)
                    .flex_shrink(1.)
                    .flex_basis(gpui::relative(1.))
                    .min_w(gpui::relative(0.5))
                    .font_family(".SystemUIFont")
                    .text_size(px(12.))
                    .whitespace_normal();
            }
            if row.small {
                label = label
                    .flex_1()
                    .font_family(".SystemUIFont")
                    .text_size(px(10.));
            }
            if row.quiet {
                label = label.text_color(rgb(if row.small { p.subtext0 } else { p.muted }));
            }
            if repo {
                // Iced's name slot never shrinks, wraps or ellipsizes.
                label = label
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .whitespace_nowrap();
            }
            if row.heading || repo {
                label = label.font_weight(gpui::FontWeight::BOLD);
            }
            if row.indent {
                // Iced shortens, then lets the branch name wrap in a fill slot.
                label = label.font_family("Menlo").flex_1().whitespace_normal();
            }
            let mut line = div()
                .debug_selector(|| format!("work-line-{index}"))
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.));
            if let Some((number, chars)) = row.pr_number {
                line = line.child(
                    self.button(
                        format!("work-pr-number-{index}"),
                        Control {
                            label: format!("#{number}"),
                            action: row.pr_link.take().expect("PR number has viewer action"),
                        },
                        false,
                        cx,
                    )
                    .debug_selector(|| format!("work-pr-number-{index}"))
                    .p_0()
                    .h_auto()
                    .w(px(chars as f32 * 12. * 0.61))
                    .text_size(px(12.))
                    .flex_shrink_0()
                    .font_family("Menlo")
                    .text_color(rgb(p.muted)),
                );
            }
            if repo {
                let action = row.action.take().unwrap();
                line = line.child(
                    Button::new(format!("work-name-{index}"))
                        .debug_selector(|| format!("work-name-{index}"))
                        .ghost()
                        .compact()
                        .h_auto()
                        .px(px(2.))
                        .py_0()
                        .flex_shrink_0()
                        .text_color(rgb(p.text))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.dispatch(action.clone(), window, cx);
                        }))
                        .child(label),
                );
            } else {
                line = line.child(label);
            }
            if let Some(run) = row.run {
                line = line.child(self.button(format!("work-run-{index}"), run, repo, cx));
            }
            if row.editor.is_some()
                && let Some((_, input)) = &self.editing
            {
                line = line.child(
                    div()
                        .id("work-run-command-input")
                        .debug_selector(|| "work-run-command-input".into())
                        .w(px(160.))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(Input::new(input)),
                );
            }
            if !row.heading && !pr && !row.small && !row.chat && !row.indent {
                line = line.child(
                    div()
                        .debug_selector(|| format!("work-space-{index}"))
                        .flex_1(),
                );
            }
            let shrinking_badge = row.shrinking_badge;
            for (badge_index, badge) in row.badges.into_iter().enumerate() {
                let color = if matches!(
                    badge.as_str(),
                    "conflicts" | "not read" | "changes requested" | "✗"
                ) {
                    rgb(p.red).into()
                } else if matches!(badge.as_str(), "draft" | "●") {
                    rgb(p.yellow).into()
                } else if badge == "open" || badge == "✓" {
                    rgb(p.green).into()
                } else if badge.contains("dirty") {
                    gpui_component::Theme::global(cx).warning
                } else {
                    rgb(p.muted).into()
                };
                let shrinks = shrinking_badge == Some(badge_index);
                line = line.child(
                    div()
                        .debug_selector(|| format!("work-badge-{index}-{badge_index}"))
                        .map(|badge| {
                            if shrinks {
                                badge
                                    .flex_shrink(2.)
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                            } else {
                                badge.flex_shrink_0()
                            }
                        })
                        .font_family("Menlo")
                        .text_size(px(10.))
                        .text_color(color)
                        .child(badge),
                );
            }
            if row.heading {
                line = line.child(div().flex_1());
            }
            let mut age = row.age;
            // Repo rows group the controls and age in one shrinking slot, like
            // Iced's `actions_slot`; other rows append them to the line.
            let mut actions = div()
                .debug_selector(|| format!("work-actions-{index}"))
                .min_w_0()
                .flex()
                .flex_shrink_1()
                .overflow_hidden()
                .items_center()
                .gap(px(6.));
            // The repo row's × sits beside the disclosure, outside the clipped
            // actions slot, so a narrow sidebar never hides it (TRU-274).
            let mut remove = None;
            for (control_index, control) in row.controls.into_iter().enumerate() {
                if matches!(
                    control.action,
                    Action::Emit(WorkSidebarOutcome::RemoveRepo { .. })
                ) && let Some(age) = age.take()
                {
                    let age = age_label(index, age, repo, &p);
                    if repo {
                        actions = actions.child(age);
                    } else {
                        line = line.child(age);
                    }
                }
                let id = if matches!(
                    control.action,
                    Action::Emit(WorkSidebarOutcome::OpenAgentConfig { .. })
                ) {
                    "work-agent-config".to_string()
                } else {
                    format!("work-control-{index}-{control_index}")
                };
                let is_remove = matches!(
                    control.action,
                    Action::Emit(WorkSidebarOutcome::RemoveRepo { .. })
                );
                let button = self.button(id, control, repo, cx);
                if repo && is_remove {
                    remove = Some(button);
                } else if repo {
                    actions = actions.child(button);
                } else {
                    line = line.child(button);
                }
            }
            if let Some(age) = age {
                let age = age_label(index, age, repo, &p);
                if repo {
                    actions = actions.child(age);
                } else {
                    line = line.child(age);
                }
            }
            if repo {
                line = line.child(actions);
            }
            if repo {
                // Iced constrains wrapping content to a fill slot beside the disclosure.
                line = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().min_w_0().overflow_hidden().child(line))
                    .children(remove);
            }
            if let Some(fold) = row.fold {
                let label = if fold == Fold::Focus {
                    if row.expanded { "▴" } else { "▾" }
                } else {
                    marker(row.expanded)
                };
                line = line.child(
                    self.button(
                        format!("work-fold-{index}"),
                        Control {
                            label: label.into(),
                            action: Action::Toggle(fold),
                        },
                        false,
                        cx,
                    )
                    .when(repo || pr, |button| {
                        button
                            .w(px(24.))
                            .debug_selector(move || format!("work-fold-{index}-{label}"))
                    }),
                );
            }
            let mut block = div()
                .w_full()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .rounded_md()
                .px_1()
                .py(px(if row.small {
                    0.
                } else if compact {
                    3.
                } else {
                    4.
                }))
                .child(line);
            if row.selected {
                block = block
                    .bg(rgb(p.surface))
                    .debug_selector(|| format!("work-selected-{index}"));
            }
            if row.indent {
                block = block.ml_2();
            }
            for (detail_index, detail) in row.details.into_iter().enumerate() {
                let mut detail = div()
                    .debug_selector(|| format!("work-facts-{index}-{detail_index}"))
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .child(detail);
                if repo {
                    detail = detail.font_family("Menlo");
                }
                block = block.child(detail);
            }
            for (detail_index, detail) in row.detail_rows.into_iter().enumerate() {
                let mut line = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_size(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(detail.label),
                    );
                if let Some(age) = detail.age {
                    line = line.child(age);
                }
                for badge in detail.badges {
                    line = line.child(badge);
                }
                for (control_index, control) in detail.controls.into_iter().enumerate() {
                    line = line.child(self.button(
                        format!("work-inline-{index}-{detail_index}-{control_index}"),
                        control,
                        false,
                        cx,
                    ));
                }
                block = block.child(line);
            }
            let mut actions = div().flex().flex_wrap().gap_1();
            for (control_index, control) in row.detail_controls.into_iter().enumerate() {
                actions = actions.child(self.button(
                    format!("work-detail-{index}-{control_index}"),
                    control,
                    false,
                    cx,
                ));
            }
            block = block.child(actions);
            body = if let Some(action) = row.action {
                body.child(
                    Button::new(format!("work-row-{index}"))
                        .ghost()
                        .compact()
                        .w_full()
                        .h_auto()
                        .p_0()
                        .text_color(rgb(p.text))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.dispatch(action.clone(), window, cx)
                        }))
                        .child(block),
                )
            } else {
                body.child(block)
            };
        }
        if let (Some(confirm), Some(focus)) = (&self.confirm, &self.confirm_focus) {
            body = body.child(pr_confirm::view(
                self.confirm_model(confirm),
                focus,
                window,
                cx,
            ));
        }
        body
    }
}

#[cfg(test)]
#[path = "pr_confirm_tests.rs"]
mod pr_confirm_tests;
#[cfg(test)]
#[path = "work_sidebar_tests.rs"]
pub(crate) mod tests;
