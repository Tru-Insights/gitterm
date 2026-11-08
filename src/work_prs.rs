//! PRs of a piece of work in the app (TRU-173): the merge plan MCP tools,
//! the PR watch's schedule, the overview's "PRs · merge order" section, the
//! Approve / Merge confirmation, news on the workspace chip and the Parked
//! list, and the "Ready to merge" list. The pure parts live in
//! `gitterm::merge_plan` and `gitterm::pr_watch`.
//!
//! The watch is one background poll at a time, started from `Tick` when a
//! PR is due and applied by [`PrEvent::Polled`]; `update()` never waits on
//! GitHub. Which PRs: for every open local workspace and every parked one,
//! its root chats (the chats it owns, else its recent coordinators), each
//! root's plan entries, the PRs its chats' facts recorded, and PRs found
//! on recent delegated task branches. Open
//! workspaces' PRs are due every `pr_watch::OPEN_INTERVAL`, parked ones'
//! every `pr_watch::PARKED_INTERVAL`; a merged or closed PR is never read
//! again.
//!
//! Actionable news tells the coordinator by itself (TRU-197): merge
//! conflicts (once settled, `pr_watch::conflict_settled`), failed checks,
//! review comments and changes requested (`pr_watch::tells_by_itself`).
//! [`App::auto_tell_coordinators`] sends it from `Tick` to the root's chat
//! tab only while that tab's turn is idle (not `Streaming`), all of a
//! root's pending items as one prompt, one line each, and marks them told.
//! A coordinator not open in a chat tab gets nothing; its news waits, and
//! goes once the chat opens idle if it is younger than
//! `pr_watch::AUTO_TELL_MAX_AGE` and still holds. Informational news
//! (checks passed, merged, closed, approvals) only shows on the list and
//! the chip. An item that was sent says "sent to chat ✓" (TRU-208); there
//! is no button to send one by hand. `pr_watch.auto_tell: false` in
//! settings.json turns the sending off.

use super::*;

use gitterm::merge_plan::{
    self, MergeMethod, MergePlan, OrderedRow, PrBrief, PrKey, PrRef, PrState, RowStatus,
};
use gitterm::pr_watch::{self, NewsItem, NewsKind, PrAction, WatchedPr};
use gitterm::task_mcp::MergePlanSetRequest;
use serde_json::Value;
use std::collections::BTreeMap;

/// How long the PR homes (workspace -> roots -> PRs) are reused before the
/// tick rebuilds them; plan and index changes rebuild sooner.
const PR_HOMES_FRESH: Duration = Duration::from_secs(30);
/// PRs one poll reads at most; the rest are read by the next.
const PR_POLL_MAX_ITEMS: usize = 25;
/// An immediate user-triggered refresh is coalesced per PR within this window.
const PR_IMMEDIATE_DEBOUNCE: Duration = Duration::from_secs(10);
/// A PR no workspace lists is forgotten this long after its last read.
const PR_FORGET_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);
const TASK_PR_MAX_AGE: chrono::Duration = chrono::Duration::days(14);
/// Prefix of every line of a prompt that tells the coordinator news.
const TELL_PREFIX: &str = "GitTerm PR watch";

/// One news line for the coordinator: the PR, its title, the news, its URL.
fn news_line(watched: &WatchedPr, item: &NewsItem) -> String {
    let (title, url) = watched
        .snapshot
        .core
        .as_ref()
        .map(|core| (core.title.clone(), core.url.clone()))
        .unwrap_or_default();
    let url = if url.is_empty() {
        watched.key.url()
    } else {
        url
    };
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", overview::shorten(&title, 80))
    };
    format!(
        "{TELL_PREFIX}: {}{title}: {}. {url}",
        watched.key.label(),
        item.text
    )
}

fn pr_url_for(key: &PrKey, watched_url: Option<&str>) -> String {
    watched_url
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| key.url())
}

/// The prompt that tells a coordinator its news: one line per item.
fn tell_prompt(lines: &[String]) -> String {
    gitterm::delegations::from_gitterm(&lines.join("\n"))
}

/// Where a PR's work lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrHome {
    /// An open local workspace, by uid.
    Open(u64),
    Parked(ParkedKey),
}

/// One root chat of a workspace and the PRs its chats recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootPrs {
    pub root: String,
    pub title: String,
    pub facts: Vec<PrRef>,
}

/// The Approve / Merge confirmation.
#[derive(Debug, Clone)]
pub struct PrActionConfirm {
    pub key: PrKey,
    pub kind: ActionKind,
    pub method: MergeMethod,
    /// The plan named the method.
    pub method_from_plan: bool,
    /// The mapped gh account; `None` for gh's active account.
    pub account: Option<String>,
    pub running: bool,
    /// Why the last run failed; a run that succeeds closes the sheet.
    pub error: Option<String>,
}

impl PrActionConfirm {
    fn action(&self) -> PrAction {
        match self.kind {
            ActionKind::Approve => PrAction::Approve,
            ActionKind::Merge => PrAction::Merge(self.method),
            ActionKind::Ready => PrAction::Ready,
        }
    }
}

/// Which row action the confirmation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Approve,
    Merge,
    /// Mark a draft ready for review.
    Ready,
}

/// A plan write finished: restore `previous` on failure, answer the tool.
#[derive(Debug, Clone)]
pub struct MergePlanWrite {
    pub root: String,
    pub previous: Option<MergePlan>,
    pub reply: Option<TaskControlReply>,
    pub kept_holds: Vec<PrKey>,
    pub result: Result<(), String>,
}

#[derive(Debug, Clone)]
pub enum PrEvent {
    Polled(Box<pr_watch::PollOutcome>),
    TaskBranchesRead(Vec<TaskBranchResult>),
    Saved(Result<(), String>),
    PlanWritten(Box<MergePlanWrite>),
    /// The user cleared a hold: root chat, PR.
    ClearHold(String, PrKey),
    Review(PrKey),
    /// Open the PR on GitHub, whatever checkouts exist.
    OpenInBrowser(PrKey),
    /// Open the PR in the shared Viewer surface.
    OpenInViewer(PrKey),
    /// The checkout of the PR's head branch, if the overview knows one.
    ReviewResolved(PrKey, Option<PathBuf>),
    /// Open the confirmation for a row action on a PR.
    ActionOpen(PrKey, ActionKind),
    ActionMethod(MergeMethod),
    ActionRun,
    ActionDone(PrKey, Result<String, String>),
    ActionClose,
    /// Dismiss news: PR id, news key.
    NewsDismiss(String, String),
    ReadyListToggle,
    ReadyGo(PrHome),
    /// Fold or expand a root's merged and closed PRs: root chat.
    DoneToggle(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskBranch {
    repo: String,
    branch: String,
}

pub type TaskBranchResult = (TaskBranch, Result<Vec<PrRef>, String>);

/// Only recent task delegations can contribute branch PRs. The parent is
/// resolved to its current root when collecting the list, not when gh runs.
fn task_branches(
    store: &TaskStore,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<(String, TaskBranch)> {
    store
        .delegations()
        .iter()
        .filter_map(|delegation| {
            let created = chrono::DateTime::parse_from_rfc3339(&delegation.created_at).ok()?;
            if created < now - TASK_PR_MAX_AGE || created > now {
                return None;
            }
            let gitterm::tasks::DelegationChild::TaskSession { task_id, .. } = &delegation.child
            else {
                return None;
            };
            let parent = delegation.parent.chat_session_id.as_ref()?;
            let task = store.get(task_id)?;
            let slug = gitterm::gh_identity::parse_repository_slug(
                task.repository.remote_url.as_deref()?,
            )?;
            // PR identities and the watch currently support github.com only.
            if !slug.host.eq_ignore_ascii_case("github.com")
                || !merge_plan::valid_repo(&slug.full_name())
                || task.branch.is_empty()
            {
                return None;
            }
            Some((
                parent.clone(),
                TaskBranch {
                    repo: slug.full_name(),
                    branch: task.branch.clone(),
                },
            ))
        })
        .collect()
}

async fn lookup_task_branches<F, Fut>(
    branches: Vec<TaskBranch>,
    accounts: BTreeMap<String, String>,
    mut lookup: F,
) -> Vec<TaskBranchResult>
where
    F: FnMut(TaskBranch, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<PrRef>, String>>,
{
    let mut results = Vec::new();
    for branch in branches {
        let account = pr_watch::account_for(PrKey::new(&branch.repo, 1).owner(), &accounts);
        let result = lookup(branch.clone(), account).await;
        results.push((branch, result));
    }
    results
}

/// Facts, live tabs and cached task branches share one PR identity set.
fn collect_root_prs(
    index: &[chats::ChatIndexEntry],
    root: &str,
    prs: &PrsState,
    branches: &[(String, TaskBranch)],
    root_of: impl Fn(&str) -> String,
) -> Vec<PrRef> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |pr: &PrRef| {
        if seen.insert(pr.key().id()) {
            found.push(pr.clone());
        }
    };
    for entry in overview::chats_under(index, &[root.to_string()]) {
        for pr in entry.facts.iter().flat_map(|facts| &facts.prs) {
            push(pr);
        }
    }
    for (chat, live) in &prs.live_prs {
        if chat == root || root_of(chat) == root {
            for pr in live {
                push(pr);
            }
        }
    }
    for (parent, branch) in branches {
        if root_of(parent) == root {
            for pr in prs.task_branch_prs.get(branch).into_iter().flatten() {
                push(pr);
            }
        }
    }
    found
}

/// Splits `keys` into the open ones (state unknown counts as open) and the
/// done ones with their final state, keeping order.
fn split_done<K>(
    keys: Vec<K>,
    state_of: impl Fn(&K) -> Option<PrState>,
) -> (Vec<K>, Vec<(K, PrState)>) {
    let mut open = Vec::new();
    let mut done = Vec::new();
    for key in keys {
        match state_of(&key) {
            Some(state) if state.is_final() => done.push((key, state)),
            _ => open.push(key),
        }
    }
    (open, done)
}

/// Queued immediate work keeps its request order ahead of periodic candidates.
fn ordered_poll_ids(wanted: &[String], due_order: &VecDeque<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    due_order
        .iter()
        .chain(wanted)
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// The fold line for done PRs: "3 merged" or with "n closed".
fn done_summary(done: &[PrState]) -> String {
    let merged = done.iter().filter(|s| **s == PrState::Merged).count();
    let closed = done.iter().filter(|s| **s == PrState::Closed).count();
    match (merged, closed) {
        (m, 0) => format!("{m} merged"),
        (0, c) => format!("{c} closed"),
        (m, c) => format!("{m} merged \u{00B7} {c} closed"),
    }
}

/// A PR list's columns before the title (TRU-208): the plan position
/// (only when some PR of the list has one), then "#n" as wide as the
/// list's widest, each followed by [`PR_COLUMN_GAP`]. The opened row's
/// lines are indented by the same amount so they start under the title.
const PR_POSITION_WIDTH: f32 = 16.0;
const PR_COLUMN_GAP: f32 = 6.0;
/// Menlo's advance per character, as a fraction of the font size.
const MONO_ADVANCE: f32 = 0.61;
/// A generous average advance of the proportional UI font, as a fraction
/// of its size, for cutting text to the width it gets.
const TEXT_ADVANCE: f32 = 0.55;
/// The "Ready to merge" sheet's width.
const READY_LIST_WIDTH: f32 = 560.0;

/// The columns one PR list shares (TRU-208).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrColumns {
    /// Some PR of the list has a plan position.
    position: bool,
    /// Characters of the widest "#n".
    number_chars: usize,
    /// The list spans several repos, so a closed row names its repo.
    show_repo: bool,
}

/// Whether a list of PRs names the repo on each row: only when the list
/// has PRs of more than one repo (TRU-208).
fn show_repo_names<'a>(repos: impl IntoIterator<Item = &'a str>) -> bool {
    let mut repos = repos.into_iter();
    repos
        .next()
        .is_some_and(|first| repos.any(|repo| repo != first))
}

/// The repo's short name: the part after the last slash.
fn repo_short_name(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// The columns for a list of PRs: (number, in a plan) each.
fn pr_columns(prs: impl IntoIterator<Item = (u64, bool)>) -> PrColumns {
    let mut columns = PrColumns {
        position: false,
        number_chars: 0,
        show_repo: false,
    };
    for (number, planned) in prs {
        columns.position |= planned;
        columns.number_chars = columns.number_chars.max(number.to_string().len() + 1);
    }
    columns
}

/// The most characters of text at `font` that fit `lines` lines of
/// `width` px; at least 12 so a narrow sidebar still says something.
fn chars_that_fit(width: f32, font: f32, lines: usize) -> usize {
    let per_line = (width / (font * TEXT_ADVANCE)).floor().max(0.0) as usize;
    (per_line * lines).max(12)
}

/// What a PR row shows (TRU-208). Closed, it is one title line with its
/// facts; opened, it adds the meta line, the plan line, news, the actions
/// line and read errors: nothing the title line already says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrRowParts {
    /// Merge on the title line: the next-to-merge PR's filled button.
    merge_on_title: bool,
    /// The opened lines, news included.
    opened: bool,
    /// Merge on the actions line (any other PR, opened).
    merge_in_actions: bool,
    /// "Open on GitHub" on the actions line (opened).
    open_on_github: bool,
}

fn pr_title_row<'a>(
    line: Row<'a, Event, Theme, iced::Renderer>,
    title: Element<'a, Event, Theme, iced::Renderer>,
    facts: Element<'a, Event, Theme, iced::Renderer>,
    merge: Option<Element<'a, Event, Theme, iced::Renderer>>,
    disclosure: Element<'a, Event, Theme, iced::Renderer>,
) -> Row<'a, Event, Theme, iced::Renderer> {
    line.push(title)
        .push(facts)
        .push(merge.unwrap_or_else(|| iced::widget::Space::new().into()))
        .push(disclosure)
}

fn pr_row_parts(is_next: bool, expanded: bool) -> PrRowParts {
    PrRowParts {
        merge_on_title: is_next,
        opened: expanded,
        merge_in_actions: expanded && !is_next,
        open_on_github: expanded,
    }
}

fn pr_row_position(row: Option<&OrderedRow>) -> Option<usize> {
    row.filter(|row| !matches!(row.status, RowStatus::Unordered(_)))
        .map(|row| row.position)
}

fn pr_unordered_fact(row: Option<&OrderedRow>) -> Option<String> {
    match row.map(|row| &row.status) {
        Some(RowStatus::Unordered(reason)) => Some(format!("unordered: {reason}")),
        _ => None,
    }
}

/// Everything the app keeps for PRs.
#[derive(Debug, Default)]
pub struct PrsState {
    /// Merge plans by root chat id.
    pub plans: HashMap<String, MergePlan>,
    /// Serialize writes per root so a stale completion cannot replace a newer plan.
    pub plan_writes: HashSet<String>,
    pub plan_errors: HashMap<String, String>,
    pub watch: pr_watch::WatchFile,
    pub in_flight: bool,
    /// PRs to read at the next tick whatever their interval (a plan was
    /// just set, an action just ran).
    pub due_now: HashSet<String>,
    /// The order immediate refreshes were queued, used ahead of periodic work.
    due_order: VecDeque<String>,
    /// Requests inside the debounce window, due when the window expires.
    deferred_due: HashMap<String, Instant>,
    /// Last immediate user-triggered refresh request per PR.
    immediate_requested_at: HashMap<String, Instant>,
    /// Login per account (`None` = gh's active account), from polls.
    pub logins: HashMap<Option<String>, String>,
    pub homes: Vec<(PrHome, Vec<RootPrs>)>,
    pub homes_built: Option<Instant>,
    pub save_in_flight: bool,
    pub save_again: bool,
    /// PRs live tabs recorded in this process, by chat, until the index
    /// carries them.
    pub live_prs: HashMap<String, Vec<PrRef>>,
    pub action: Option<PrActionConfirm>,
    pub ready_open: bool,
    /// Roots whose merged and closed PRs are expanded (not persisted).
    pub done_open: HashSet<String>,
    /// The index was asked for once for the watch.
    pub index_requested: bool,
    /// In-memory discovery, refreshed even for branches that had no PR.
    task_branch_prs: HashMap<TaskBranch, Vec<PrRef>>,
    task_branches_in_flight: bool,
    task_branches_read_at: Option<Instant>,
    task_branches_refresh: bool,
}

impl PrsState {
    fn task_branches_due(&self, now: Instant) -> bool {
        !self.task_branches_in_flight
            && (self.task_branches_refresh
                || self
                    .task_branches_read_at
                    .is_none_or(|at| now.duration_since(at) >= pr_watch::OPEN_INTERVAL))
    }

    fn apply_task_branches(&mut self, results: Vec<TaskBranchResult>, mut log: impl FnMut(&str)) {
        self.task_branches_in_flight = false;
        for (branch, result) in results {
            match result {
                Ok(prs) => {
                    self.task_branch_prs.insert(branch, prs);
                }
                Err(error) => log(&format!(
                    "[pr-watch] task PRs for {} head {}: not read: {error}",
                    branch.repo, branch.branch
                )),
            }
        }
        self.homes_built = None;
    }

    /// Queue a refresh, deferring a coalesced request until its debounce ends.
    pub fn request_immediate_refresh(&mut self, id: String, now: Instant) -> bool {
        let Some(last) = self.immediate_requested_at.get(&id).copied() else {
            self.immediate_requested_at.insert(id.clone(), now);
            self.enqueue_due(id);
            return true;
        };
        let due_at = last + PR_IMMEDIATE_DEBOUNCE;
        if now < due_at {
            self.deferred_due
                .entry(id)
                .and_modify(|due| *due = (*due).min(due_at))
                .or_insert(due_at);
            return false;
        }
        self.immediate_requested_at.insert(id.clone(), now);
        self.enqueue_due(id);
        true
    }

    fn enqueue_due(&mut self, id: String) {
        if self.due_now.insert(id.clone()) {
            self.due_order.push_back(id);
        }
    }

    fn promote_deferred(&mut self, now: Instant) {
        let due: Vec<String> = self
            .deferred_due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.deferred_due.remove(&id);
            self.immediate_requested_at.insert(id.clone(), now);
            self.enqueue_due(id);
        }
    }

    fn take_due(&mut self, id: &str) {
        self.due_now.remove(id);
        if let Some(position) = self.due_order.iter().position(|queued| queued == id) {
            self.due_order.remove(position);
        }
    }

    fn request_open_workspace_refreshes(&mut self, homes: &[(PrHome, Vec<String>)], now: Instant) {
        for (home, ids) in homes {
            if matches!(home, PrHome::Open(_)) {
                for id in ids {
                    self.request_immediate_refresh(id.clone(), now);
                }
            }
        }
    }

    pub(super) fn request_row_refresh(&mut self, id: String, opened: bool, now: Instant) {
        if opened {
            self.task_branches_refresh = true;
            self.request_immediate_refresh(id, now);
        }
    }

    /// An Approve / Merge / Ready run for `key` finished. Its PR is read at the
    /// next tick either way. The open confirmation for it closes on
    /// success and keeps the error on failure; returns whether it closed.
    pub fn action_done(&mut self, key: &PrKey, result: Result<String, String>) -> bool {
        self.request_immediate_refresh(key.id(), Instant::now());
        if !self.action.as_ref().is_some_and(|c| c.key == *key) {
            return false;
        }
        match result {
            Ok(_) => {
                self.action = None;
                true
            }
            Err(error) => {
                if let Some(confirm) = self.action.as_mut() {
                    confirm.running = false;
                    confirm.error = Some(error);
                }
                false
            }
        }
    }

    /// Plans and the watch file from the config dir (small reads).
    pub fn load(config_dir: &Path) -> Self {
        Self {
            plans: merge_plan::load_plans(&merge_plan::plans_dir(config_dir)),
            watch: pr_watch::WatchFile::load(&pr_watch::WatchFile::path_in(config_dir)),
            ..Self::default()
        }
    }

    /// Records PRs a live tab saw in chat `chat`.
    pub fn note_live_prs(&mut self, chat: &str, prs: &[PrRef]) {
        let known = self.live_prs.entry(chat.to_string()).or_default();
        for pr in prs {
            if !known.iter().any(|k| k.key().id() == pr.key().id()) {
                known.push(pr.clone());
            }
        }
        self.homes_built = None;
    }
}

fn news_color(theme: &AppTheme, kind: NewsKind) -> iced::Color {
    match kind {
        NewsKind::ChecksFailed | NewsKind::Conflicting | NewsKind::Closed => theme.danger(),
        NewsKind::RequiredChecksPassed | NewsKind::Merged => theme.success(),
        NewsKind::NewActivity => theme.accent(),
    }
}

impl App {
    // -----------------------------------------------------------------------
    // Which PRs belong to which work.

    /// A workspace's root chats: the chats it owns, else the recent
    /// coordinators of its folder. With their titles.
    fn pr_roots_of(&self, owned: &[String], dir: &Path) -> Vec<(String, String)> {
        let title_of = |id: &str| {
            self.chat_index
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.title.clone())
                .unwrap_or_else(|| id.chars().take(8).collect())
        };
        if !owned.is_empty() {
            return owned.iter().map(|id| (id.clone(), title_of(id))).collect();
        }
        let work = overview::work_chats(&self.chat_index, dir);
        overview::coordinators(&work, &self.chat_index, std::time::SystemTime::now())
            .into_iter()
            .filter(|coordinator| coordinator.recent)
            .map(|coordinator| (coordinator.id, coordinator.title))
            .collect()
    }

    /// The root `chat` folds under: its index root, else its parent chain
    /// through delegations and tasks, else itself.
    pub(super) fn root_chat_of(&self, chat: &str) -> String {
        if let Some(entry) = self.chat_index.iter().find(|entry| entry.id == chat) {
            return entry.root_id.clone().unwrap_or_else(|| chat.to_string());
        }
        let links = self.chat_child_links();
        let mut current = chat.to_string();
        let mut seen: HashSet<String> = HashSet::from([current.clone()]);
        while let Some(link) = links.iter().find(|link| link.child.session_id == current) {
            if !seen.insert(link.parent.clone()) {
                break;
            }
            current = link.parent.clone();
        }
        current
    }

    /// The PRs root `root`'s chats recorded: facts in the index, and what
    /// live tabs saw since, plus delegated tasks' cached branch PRs.
    fn root_fact_prs(&self, root: &str) -> Vec<PrRef> {
        let branches = self
            .task_store
            .as_ref()
            .map(|store| task_branches(store, chrono::Utc::now()))
            .unwrap_or_default();
        collect_root_prs(&self.chat_index, root, &self.prs, &branches, |chat| {
            self.root_chat_of(chat)
        })
    }

    fn rebuild_pr_homes(&mut self) {
        let mut homes: Vec<(PrHome, Vec<RootPrs>)> = Vec::new();
        let roots_of = |app: &App, owned: &[String], dir: &Path| -> Vec<RootPrs> {
            app.pr_roots_of(owned, dir)
                .into_iter()
                .map(|(root, title)| RootPrs {
                    facts: app.root_fact_prs(&root),
                    root,
                    title,
                })
                .collect()
        };
        for ws in &self.workspaces {
            if matches!(ws.location, WorkspaceLocation::Local { .. }) {
                homes.push((PrHome::Open(ws.uid), roots_of(self, &ws.chats, &ws.dir)));
            }
        }
        for cfg in &self.closed_workspace_configs {
            let local = matches!(
                cfg.location,
                None | Some(config::WorkspaceLocationConfig::Local { .. })
            );
            if cfg.is_parked() && local {
                homes.push((
                    PrHome::Parked(ParkedKey::of(cfg)),
                    roots_of(self, &cfg.chats, Path::new(&cfg.dir)),
                ));
            }
        }
        // A root a tab here hosts, with a plan or PRs the index does not
        // carry yet (a new chat), belongs to that tab's workspace unless
        // another lists it.
        let listed: HashSet<String> = homes
            .iter()
            .flat_map(|(_, roots)| roots.iter().map(|root| root.root.clone()))
            .collect();
        let mut added: HashSet<String> = HashSet::new();
        for (home, roots) in homes.iter_mut() {
            let PrHome::Open(uid) = home else {
                continue;
            };
            let Some(ws) = self.workspaces.iter().find(|ws| ws.uid == *uid) else {
                continue;
            };
            for chat in ws
                .tabs
                .iter()
                .filter_map(|tab| tab.chat_session_id.as_deref())
            {
                let root = self.root_chat_of(chat);
                if listed.contains(&root) || added.contains(&root) {
                    continue;
                }
                let facts = self.root_fact_prs(&root);
                if facts.is_empty() && !self.prs.plans.contains_key(&root) {
                    continue;
                }
                added.insert(root.clone());
                let title = format!("chat {}", root.chars().take(8).collect::<String>());
                roots.push(RootPrs { root, title, facts });
            }
        }
        self.prs.homes = homes;
        self.prs.homes_built = Some(Instant::now());
    }

    /// The PRs of `roots`: plan entries first, then facts PRs, each once.
    fn keys_of_roots(&self, roots: &[RootPrs]) -> Vec<(String, PrKey)> {
        let mut out: Vec<(String, PrKey)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for root in roots {
            let plan = self.prs.plans.get(&root.root);
            let keys = plan
                .into_iter()
                .flat_map(|plan| plan.entries.iter().map(|entry| entry.key()))
                .chain(root.facts.iter().map(PrRef::key));
            for key in keys {
                if seen.insert(key.id()) {
                    out.push((root.root.clone(), key));
                }
            }
        }
        out
    }

    fn home_roots(&self, home: &PrHome) -> Option<&[RootPrs]> {
        self.prs
            .homes
            .iter()
            .find(|(known, _)| known == home)
            .map(|(_, roots)| roots.as_slice())
    }

    /// What ordering needs of every watched PR, by id.
    fn pr_briefs(&self) -> HashMap<String, PrBrief> {
        self.prs
            .watch
            .prs
            .iter()
            .filter_map(|(id, watched)| Some((id.clone(), watched.brief()?)))
            .collect()
    }

    /// The PR's author is the gh account Approve would run as (the mapped
    /// one, else the active one). Unknown either way: not own.
    fn is_own_pr(&self, key: &PrKey, watched: Option<&WatchedPr>) -> bool {
        let author = watched
            .and_then(|w| w.snapshot.core.as_ref())
            .map(|core| core.author.as_str());
        let account = pr_watch::account_for(key.owner(), &self.github_accounts);
        let login = match &account {
            Some(user) => Some(user.as_str()),
            None => self.prs.logins.get(&None).map(String::as_str),
        };
        pr_watch::is_own_pr(author, login)
    }

    fn watched(&self, key: &PrKey) -> Option<&WatchedPr> {
        self.prs.watch.prs.get(&key.id())
    }

    /// A window focus refreshes open-workspace PRs, while parked work remains
    /// on its slower schedule.
    pub(super) fn request_open_pr_refreshes(&mut self) {
        self.prs.task_branches_refresh = true;
        if self
            .prs
            .homes_built
            .is_none_or(|at| at.elapsed() >= PR_HOMES_FRESH)
        {
            self.rebuild_pr_homes();
        }
        let homes: Vec<(PrHome, Vec<String>)> = self
            .prs
            .homes
            .iter()
            .map(|(home, roots)| {
                (
                    home.clone(),
                    self.keys_of_roots(roots)
                        .into_iter()
                        .map(|(_, key)| key.id())
                        .collect(),
                )
            })
            .collect();
        self.prs
            .request_open_workspace_refreshes(&homes, Instant::now());
    }

    // -----------------------------------------------------------------------
    // The watch.

    /// From `Tick`: rebuild the homes when stale, then start one poll of
    /// the due PRs unless one runs.
    pub(super) fn poll_pr_watch(&mut self) -> Task<Event> {
        let mut tasks: Vec<Task<Event>> = Vec::new();
        tasks.push(self.poll_task_branch_prs());
        self.prs.promote_deferred(Instant::now());
        // Facts PRs come from the index; load it once if nothing has.
        if !self.prs.index_requested
            && self.chat_index_loaded_at.is_none()
            && !self.chat_index_loading
        {
            self.prs.index_requested = true;
            tasks.push(self.refresh_local_chat_index_if_stale());
        }
        if self
            .prs
            .homes_built
            .is_none_or(|at| at.elapsed() >= PR_HOMES_FRESH)
        {
            self.rebuild_pr_homes();
        }
        if self.prs.in_flight {
            return Task::batch(tasks);
        }
        let now = chrono::Utc::now();
        let mut wanted: Vec<(PrKey, Duration)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (home, roots) in &self.prs.homes {
            let interval = match home {
                PrHome::Open(_) => pr_watch::OPEN_INTERVAL,
                PrHome::Parked(_) => pr_watch::PARKED_INTERVAL,
            };
            for (_, key) in self.keys_of_roots(roots) {
                if seen.insert(key.id()) {
                    wanted.push((key, interval));
                }
            }
        }
        let mut items: Vec<pr_watch::PollItem> = Vec::new();
        let wanted_by_id: HashMap<String, (PrKey, Duration)> = wanted
            .iter()
            .cloned()
            .map(|(key, interval)| (key.id(), (key, interval)))
            .collect();
        let wanted_ids = wanted.iter().map(|(key, _)| key.id()).collect::<Vec<_>>();
        let ordered_ids = ordered_poll_ids(&wanted_ids, &self.prs.due_order);
        let mut selected = HashSet::new();
        for id in ordered_ids {
            if items.len() >= PR_POLL_MAX_ITEMS {
                break;
            }
            if !selected.insert(id.clone()) {
                continue;
            }
            let Some((key, interval)) = wanted_by_id.get(&id) else {
                continue;
            };
            let id = key.id();
            let watched = self.prs.watch.prs.get(&id);
            if watched.is_some_and(WatchedPr::done) {
                continue;
            }
            let due = self.prs.due_now.contains(&id)
                || watched.is_none_or(|watched| watched.due(watched.interval_for(*interval), now));
            if !due {
                continue;
            }
            items.push(pr_watch::PollItem {
                account: pr_watch::account_for(key.owner(), &self.github_accounts),
                snapshot: watched.map(|w| w.snapshot.clone()).unwrap_or_default(),
                etags: watched.map(|w| w.etags.clone()).unwrap_or_default(),
                key: key.clone(),
            });
        }
        if items.is_empty() {
            return Task::batch(tasks);
        }
        for item in &items {
            self.prs.take_due(&item.key.id());
        }
        self.prs.in_flight = true;
        tasks.push(Task::perform(
            pr_watch::poll(pr_watch::PollRequest { items }),
            |outcome| Event::Pr(PrEvent::Polled(Box::new(outcome))),
        ));
        Task::batch(tasks)
    }

    fn poll_task_branch_prs(&mut self) -> Task<Event> {
        if !self.prs.task_branches_due(Instant::now()) {
            return Task::none();
        }
        let branches = self
            .task_store
            .as_ref()
            .map(|store| task_branches(store, chrono::Utc::now()))
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let branches: Vec<_> = branches
            .into_iter()
            .map(|(_, branch)| branch)
            .filter(|branch| seen.insert(branch.clone()))
            .collect();
        self.prs
            .task_branch_prs
            .retain(|branch, _| seen.contains(branch));
        self.prs.task_branches_refresh = false;
        self.prs.task_branches_read_at = Some(Instant::now());
        self.prs.task_branches_in_flight = true;
        Task::perform(
            lookup_task_branches(
                branches,
                self.github_accounts.clone(),
                |branch, account| async move {
                    pr_watch::branch_prs(&branch.repo, &branch.branch, account).await
                },
            ),
            |results| Event::Pr(PrEvent::TaskBranchesRead(results)),
        )
    }

    fn apply_pr_poll(&mut self, outcome: pr_watch::PollOutcome) -> Task<Event> {
        self.prs.in_flight = false;
        let at = chat_facts::now_stamp();
        for (account, login) in outcome.logins {
            self.prs.logins.insert(account, login);
        }
        for result in outcome.results {
            let label = result.key.label();
            let watched = self
                .prs
                .watch
                .prs
                .entry(result.key.id())
                .or_insert_with(|| WatchedPr::new(result.key.clone()));
            match result.fetched {
                Ok((fetched, login)) => {
                    for item in watched.absorb(fetched, &login, &at) {
                        eprintln!("[pr-watch] {label}: {}", item.text);
                    }
                }
                Err(error) => {
                    eprintln!("[pr-watch] {label}: not read: {error}");
                    watched.failed(error, &at);
                }
            }
        }
        if outcome.skipped > 0 {
            eprintln!(
                "[pr-watch] {} PR(s) left for the next poll (deadline)",
                outcome.skipped
            );
        }
        self.prune_pr_watch();
        self.save_pr_watch()
    }

    /// Drops PRs no workspace lists any more once they were last read over
    /// [`PR_FORGET_AFTER`] ago. Only after the index loaded, so facts PRs
    /// are not mistaken for unlisted ones.
    fn prune_pr_watch(&mut self) {
        if self.chat_index_loaded_at.is_none() {
            return;
        }
        let listed: HashSet<String> = self
            .prs
            .homes
            .iter()
            .flat_map(|(_, roots)| self.keys_of_roots(roots))
            .map(|(_, key)| key.id())
            .collect();
        let now = chrono::Utc::now();
        let before = self.prs.watch.prs.len();
        self.prs.watch.prs.retain(|id, watched| {
            listed.contains(id)
                || watched
                    .checked_at
                    .as_deref()
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .and_then(|at| now.signed_duration_since(at).to_std().ok())
                    .is_none_or(|age| age < PR_FORGET_AFTER)
        });
        let dropped = before - self.prs.watch.prs.len();
        if dropped > 0 {
            eprintln!("[pr-watch] forgot {dropped} PR(s) no workspace lists");
        }
    }

    /// Writes the watch file off the UI thread, one write at a time.
    fn save_pr_watch(&mut self) -> Task<Event> {
        if self.prs.save_in_flight {
            self.prs.save_again = true;
            return Task::none();
        }
        self.prs.save_in_flight = true;
        let file = self.prs.watch.clone();
        let path = pr_watch::WatchFile::path_in(&config::global_config_dir());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || file.save(&path))
                    .await
                    .unwrap_or_else(|error| Err(format!("watch save task failed: {error}")))
            },
            |result| Event::Pr(PrEvent::Saved(result)),
        )
    }

    // -----------------------------------------------------------------------
    // The MCP tools.

    /// The root chat of the tab that called `tool`.
    fn merge_plan_root(&self, caller: Option<&str>, tool: &str) -> Result<String, String> {
        let caller = caller.ok_or_else(|| task_mcp::missing_plan_caller_error(tool))?;
        let tab = self
            .workspaces
            .iter()
            .filter(|ws| matches!(ws.location, WorkspaceLocation::Local { .. }))
            .flat_map(|ws| ws.tabs.iter())
            .find(|tab| tab.session_uid == caller)
            .ok_or_else(|| {
                format!("{tool}: no GitTerm tab on this Mac has session {caller}; merge plans are for this Mac's chats")
            })?;
        let chat = tab.chat_session_id.as_deref().ok_or_else(|| {
            format!(
                "{tool}: this tab has no conversation id yet; call it again after your first reply"
            )
        })?;
        Ok(self.root_chat_of(chat))
    }

    /// `merge_plan_set`: validate, store in memory, write the file off the
    /// UI thread; the tool's answer goes out when the write lands.
    pub(super) fn merge_plan_set(
        &mut self,
        caller: Option<&str>,
        request: MergePlanSetRequest,
        reply: TaskControlReply,
    ) -> Task<Event> {
        let prepared = self
            .merge_plan_root(caller, "merge_plan_set")
            .and_then(|root| Ok((root, merge_plan::validate_entries(request.entries)?)));
        let (root, entries) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                reply.send(Err(error));
                return Task::none();
            }
        };
        if self.prs.plan_writes.contains(&root) {
            reply.send(Err(
                "the merge plan is being saved; retry merge_plan_set after it finishes".to_string(),
            ));
            return Task::none();
        }
        let previous = self.prs.plans.get(&root).cloned();
        let mut plan = previous.clone().unwrap_or_else(|| MergePlan {
            root_chat_id: root.clone(),
            ..MergePlan::default()
        });
        let kept_holds = plan.replace_entries(entries, &chat_facts::now_stamp());
        for entry in &plan.entries {
            self.prs.enqueue_due(entry.key().id());
        }
        eprintln!(
            "[merge-plan] {root}: plan set with {} PR(s)",
            plan.entries.len()
        );
        self.prs.plans.insert(root.clone(), plan.clone());
        self.prs.homes_built = None;
        self.write_merge_plan(plan, previous, Some(reply), kept_holds)
    }

    fn write_merge_plan(
        &mut self,
        plan: MergePlan,
        previous: Option<MergePlan>,
        reply: Option<TaskControlReply>,
        kept_holds: Vec<PrKey>,
    ) -> Task<Event> {
        let dir = merge_plan::plans_dir(&config::global_config_dir());
        let root = plan.root_chat_id.clone();
        self.prs.plan_writes.insert(root.clone());
        self.prs.plan_errors.remove(&root);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || merge_plan::write_plan(&dir, &plan))
                    .await
                    .unwrap_or_else(|error| Err(format!("plan write task failed: {error}")))
            },
            move |result| {
                Event::Pr(PrEvent::PlanWritten(Box::new(MergePlanWrite {
                    root: root.clone(),
                    previous: previous.clone(),
                    reply: reply.clone(),
                    kept_holds: kept_holds.clone(),
                    result,
                })))
            },
        )
    }

    fn merge_plan_written(&mut self, write: MergePlanWrite) {
        self.prs.plan_writes.remove(&write.root);
        match write.result {
            Ok(()) => {
                if let Some(reply) = write.reply {
                    let mut value = self.merge_plan_value(&write.root);
                    value["kept_holds"] = Value::from(
                        write
                            .kept_holds
                            .iter()
                            .map(PrKey::label)
                            .collect::<Vec<_>>(),
                    );
                    reply.send(Ok(value));
                }
            }
            Err(error) => {
                eprintln!("[merge-plan] {}: {error}", write.root);
                self.prs.plan_errors.insert(
                    write.root.clone(),
                    format!("the merge plan was not saved: {error}"),
                );
                match write.previous {
                    Some(previous) => {
                        self.prs.plans.insert(write.root.clone(), previous);
                    }
                    None => {
                        self.prs.plans.remove(&write.root);
                    }
                }
                self.prs.homes_built = None;
                if let Some(reply) = write.reply {
                    reply.send(Err(format!("the merge plan was not saved: {error}")));
                }
            }
        }
    }

    pub(super) fn merge_plan_get(&self, caller: Option<&str>) -> Result<Value, String> {
        let root = self.merge_plan_root(caller, "merge_plan_get")?;
        Ok(self.merge_plan_value(&root))
    }

    /// The plan of `root` with live state per PR, the next to merge, and
    /// the root's PRs the plan does not list.
    fn merge_plan_value(&self, root: &str) -> Value {
        let plan = self.prs.plans.get(root);
        let rows = plan
            .map(|plan| merge_plan::order_plan(plan, &self.pr_briefs()))
            .unwrap_or_default();
        let entries: Vec<Value> = rows
            .iter()
            .map(|row| {
                let key = row.entry.key();
                let waits_for = match &row.status {
                    RowStatus::Blocked(waits) => waits.clone(),
                    _ => Vec::new(),
                };
                serde_json::json!({
                    "position": row.entry.unordered_reason.is_none().then_some(row.position),
                    "unordered_reason": row.entry.unordered_reason,
                    "pr": key.label(),
                    "repo": row.entry.repo,
                    "number": row.entry.number,
                    "url": key.url(),
                    "step": row.entry.step,
                    "after": row.entry.after,
                    "stacked_on": if row.stacked { row.waits.first().cloned() } else { None },
                    "hold": row.entry.hold,
                    "method": row.entry.method.unwrap_or_default().label(),
                    "note": row.entry.note,
                    "status": row.status.label(),
                    "waits_for": waits_for,
                    "live": self.pr_live_value(&key),
                })
            })
            .collect();
        let unplanned: Vec<Value> = self
            .root_fact_prs(root)
            .into_iter()
            .filter(|pr| plan.is_none_or(|plan| plan.entry(&pr.key()).is_none()))
            .map(|pr| {
                serde_json::json!({
                    "pr": pr.key().label(),
                    "repo": pr.repo,
                    "number": pr.number,
                    "url": pr.url,
                    "live": self.pr_live_value(&pr.key()),
                })
            })
            .collect();
        serde_json::json!({
            "root_chat_id": root,
            "updated_at": plan.map(|plan| plan.updated_at.clone()),
            "source": plan.and_then(|plan| plan.source.as_deref()),
            "next": merge_plan::next_to_merge(&rows).map(|row| row.entry.key().label()),
            "entries": entries,
            "unplanned_prs": unplanned,
        })
    }

    fn pr_live_value(&self, key: &PrKey) -> Value {
        let Some(watched) = self.watched(key) else {
            return Value::Null;
        };
        let Some(core) = watched.snapshot.core.as_ref() else {
            return serde_json::json!({ "checked_at": watched.checked_at, "error": watched.error });
        };
        let s = &watched.snapshot;
        serde_json::json!({
            "title": core.title,
            "state": core.state.map(|state| state.label()),
            "head": core.head_ref,
            "base": core.base_ref,
            "checks": s.checks().label(),
            "failing_checks": s.failing_checks(),
            "required_checks_passed": s.required_passed(),
            "review": s.review_decision().label(),
            "conflicting": s.conflicting(),
            "checked_at": watched.checked_at,
            "error": watched.error,
        })
    }

    // -----------------------------------------------------------------------
    // Events.

    pub(super) fn update_prs(&mut self, event: PrEvent) -> Task<Event> {
        match event {
            PrEvent::Polled(outcome) => return self.apply_pr_poll(*outcome),
            PrEvent::TaskBranchesRead(results) => {
                self.prs
                    .apply_task_branches(results, |message| eprintln!("{message}"));
                return self.poll_pr_watch();
            }
            PrEvent::Saved(result) => {
                self.prs.save_in_flight = false;
                if let Err(error) = result {
                    eprintln!("[pr-watch] state not saved: {error}");
                }
                if self.prs.save_again {
                    self.prs.save_again = false;
                    return self.save_pr_watch();
                }
            }
            PrEvent::PlanWritten(write) => self.merge_plan_written(*write),
            PrEvent::ClearHold(root, key) => {
                if self.prs.plan_writes.contains(&root) {
                    return Task::none();
                }
                let Some(plan) = self.prs.plans.get_mut(&root) else {
                    return Task::none();
                };
                let previous = plan.clone();
                if !plan.clear_hold(&key, &chat_facts::now_stamp()) {
                    return Task::none();
                }
                eprintln!("[merge-plan] {root}: hold on {} cleared", key.label());
                let plan = plan.clone();
                return self.write_merge_plan(plan, Some(previous), None, Vec::new());
            }
            PrEvent::Review(key) => return self.review_pr(key),
            PrEvent::OpenInBrowser(key) => {
                self.open_pr_in_browser(&key);
                return Task::none();
            }
            PrEvent::OpenInViewer(key) => {
                let url = self.pr_url(&key);
                let store = github_page_store(key.owner(), &self.github_accounts);
                return self.open_page_in_store(url, store);
            }
            PrEvent::ReviewResolved(key, checkout) => {
                return match checkout {
                    Some(checkout) => self.open_review(checkout, overview::DiffSource::VsBase),
                    None => {
                        self.open_pr_in_browser(&key);
                        Task::none()
                    }
                };
            }
            PrEvent::ActionOpen(key, kind) => {
                let entry = self
                    .prs
                    .plans
                    .values()
                    .find_map(|plan| plan.entry(&key).cloned());
                let method = entry.as_ref().and_then(|entry| entry.method);
                self.prs.action = Some(PrActionConfirm {
                    account: pr_watch::account_for(key.owner(), &self.github_accounts),
                    key,
                    kind,
                    method: method.unwrap_or_default(),
                    method_from_plan: method.is_some(),
                    running: false,
                    error: None,
                });
                self.prs.ready_open = false;
                return self.apply_webview_surfaces();
            }
            PrEvent::ActionMethod(method) => {
                if let Some(confirm) = self.prs.action.as_mut() {
                    if !confirm.running {
                        confirm.method = method;
                        confirm.error = None;
                    }
                }
            }
            PrEvent::ActionRun => {
                let Some(confirm) = self.prs.action.as_ref() else {
                    return Task::none();
                };
                if confirm.running {
                    return Task::none();
                }
                // Merge re-checks the latest read: the checks may have
                // turned pending or failing since the sheet opened.
                let reason = if confirm.kind != ActionKind::Merge {
                    None
                } else {
                    self.watched(&confirm.key)
                        .map(WatchedPr::checks_gate)
                        .unwrap_or(pr_watch::ChecksGate::Running)
                        .reason()
                };
                let Some(confirm) = self.prs.action.as_mut() else {
                    return Task::none();
                };
                if let Some(reason) = reason {
                    confirm.error = Some(format!("Not merged: {reason}"));
                    return Task::none();
                }
                confirm.running = true;
                confirm.error = None;
                let action = confirm.action();
                let key = confirm.key.clone();
                let account = confirm.account.clone();
                eprintln!(
                    "[pr-watch] user ran: {}",
                    pr_watch::action_command_line(action, &key, account.as_deref())
                );
                return Task::perform(
                    pr_watch::run_action(action, key.clone(), account),
                    move |result| Event::Pr(PrEvent::ActionDone(key.clone(), result)),
                );
            }
            PrEvent::ActionDone(key, result) => {
                match &result {
                    Ok(output) => eprintln!("[pr-watch] {}: done: {output}", key.label()),
                    Err(error) => eprintln!("[pr-watch] {}: {error}", key.label()),
                }
                let closed = self.prs.action_done(&key, result);
                let refresh = self.poll_pr_watch();
                if closed {
                    // The overview row shows the new state once the
                    // `due_now` read lands.
                    return Task::batch([self.apply_webview_surfaces(), refresh]);
                }
                return refresh;
            }
            PrEvent::ActionClose => {
                if self.prs.action.as_ref().is_some_and(|c| c.running) {
                    return Task::none();
                }
                self.prs.action = None;
                return self.apply_webview_surfaces();
            }
            PrEvent::NewsDismiss(pr, news) => {
                if let Some(item) = self.news_item_mut(&pr, &news) {
                    item.dismissed = true;
                    return self.save_pr_watch();
                }
            }
            PrEvent::DoneToggle(root) => {
                if !self.prs.done_open.remove(&root) {
                    self.prs.done_open.insert(root);
                }
            }
            PrEvent::ReadyListToggle => {
                self.prs.ready_open = !self.prs.ready_open;
                return self.apply_webview_surfaces();
            }
            PrEvent::ReadyGo(home) => {
                self.prs.ready_open = false;
                let go = match home {
                    PrHome::Open(uid) => match self.workspace_index_by_uid(uid) {
                        Some(idx) => {
                            let select = self.update(Event::WorkspaceSelect(idx));
                            Task::batch([select, self.show_workspace_overview()])
                        }
                        None => Task::none(),
                    },
                    PrHome::Parked(key) => self.update(Event::ParkedResume(key)),
                };
                return Task::batch([go, self.apply_webview_surfaces()]);
            }
        }
        Task::none()
    }

    fn news_item_mut(&mut self, pr: &str, news: &str) -> Option<&mut NewsItem> {
        self.prs
            .watch
            .prs
            .get_mut(pr)?
            .news
            .iter_mut()
            .find(|item| item.key == news)
    }

    fn open_pr_in_browser(&self, key: &PrKey) {
        webview::open_url_in_browser(&self.pr_url(key));
    }

    fn pr_url(&self, key: &PrKey) -> String {
        let url = self
            .watched(key)
            .and_then(|watched| watched.snapshot.core.as_ref())
            .map(|core| core.url.as_str());
        pr_url_for(key, url)
    }

    /// Review: the review pane (TRU-178) vs base when the overview owns a checkout of
    /// the PR's head branch in its repo, else the PR in the browser. Resets
    /// the row's new-comment count.
    fn review_pr(&mut self, key: PrKey) -> Task<Event> {
        let save = match self.prs.watch.prs.get_mut(&key.id()) {
            Some(watched) if watched.unseen_comments > 0 => {
                watched.unseen_comments = 0;
                self.save_pr_watch()
            }
            _ => Task::none(),
        };
        let head = self
            .watched(&key)
            .and_then(|watched| watched.snapshot.core.as_ref())
            .map(|core| core.head_ref.clone())
            .filter(|head| !head.is_empty());
        let candidates: Vec<(PathBuf, PathBuf)> = match (head, self.overview_key()) {
            (Some(head), Some(overview_key)) => self
                .workspace_overviews
                .get(&overview_key)
                .map(|state| {
                    state
                        .branches
                        .iter()
                        .filter(|row| !row.owned.gone && row.owned.branch.as_deref() == Some(&head))
                        .map(|row| (row.owned.repo.clone(), row.owned.checkout.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if candidates.is_empty() {
            self.open_pr_in_browser(&key);
            return save;
        }
        let repo = key.repo.clone();
        let resolve = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    candidates.into_iter().find_map(|(repo_dir, checkout)| {
                        match gitterm::gh_identity::repository_slug_for(&repo_dir) {
                            Ok(slug) if slug.full_name().eq_ignore_ascii_case(&repo) => {
                                Some(checkout)
                            }
                            Ok(_) => None,
                            Err(error) => {
                                eprintln!("[pr-watch] review: {error}");
                                None
                            }
                        }
                    })
                })
                .await
                .unwrap_or_else(|error| {
                    eprintln!("[pr-watch] review: checkout lookup failed: {error}");
                    None
                })
            },
            move |checkout| Event::Pr(PrEvent::ReviewResolved(key.clone(), checkout)),
        );
        Task::batch([save, resolve])
    }

    /// The chat tab hosting root chat `root`; `None` when it is not open,
    /// or runs in a terminal tab.
    fn coordinator_tab(&self, root: &str) -> Option<usize> {
        self.workspaces
            .iter()
            .filter(|ws| matches!(ws.location, WorkspaceLocation::Local { .. }))
            .flat_map(|ws| ws.tabs.iter())
            .find(|tab| tab.chat_session_id.as_deref() == Some(root))
            .filter(|tab| tab.agent_session().is_some())
            .map(|tab| tab.id)
    }

    /// Whether chat tab `tab_id` is mid-turn: the state the chat strip's
    /// running marker and held delegation sends go by.
    pub(super) fn chat_turn_running(&self, tab_id: usize) -> bool {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.agent_session())
            .is_some_and(|session| matches!(session.state, tab::AgentSessionState::Streaming))
    }

    /// From `Tick` (TRU-197): each root chat with news due by itself
    /// ([`pr_watch::auto_tell_due`]) whose coordinator is open in a chat
    /// tab and idle gets one prompt, one line per item, oldest first, and
    /// the items are marked told. A busy coordinator gets it on the first
    /// tick after its turn ends; a closed one keeps its news until it
    /// opens.
    pub(super) fn auto_tell_coordinators(&mut self) -> Task<Event> {
        if !self.pr_watch_config.auto_tell {
            return Task::none();
        }
        let now = chrono::Utc::now();
        // Root -> (at, PR id, news key, line).
        let mut due: HashMap<String, Vec<(String, String, String, String)>> = HashMap::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for (_, roots) in &self.prs.homes {
            for (root, key) in self.keys_of_roots(roots) {
                let Some(watched) = self.watched(&key) else {
                    continue;
                };
                for item in &watched.news {
                    if !pr_watch::auto_tell_due(watched, item, now)
                        || !seen.insert((key.id(), item.key.clone()))
                    {
                        continue;
                    }
                    due.entry(root.clone()).or_default().push((
                        item.at.clone(),
                        key.id(),
                        item.key.clone(),
                        news_line(watched, item),
                    ));
                }
            }
        }
        let mut tasks: Vec<Task<Event>> = Vec::new();
        for (root, mut items) in due {
            let Some(tab_id) = self.coordinator_tab(&root) else {
                continue;
            };
            if self.chat_turn_running(tab_id) {
                continue;
            }
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let lines: Vec<String> = items.iter().map(|(_, _, _, line)| line.clone()).collect();
            for (_, pr, news, _) in &items {
                if let Some(item) = self.news_item_mut(pr, news) {
                    item.told = true;
                }
            }
            let prompt = tell_prompt(&lines);
            eprintln!(
                "[pr-watch] told coordinator {root} by itself ({} item(s)): {prompt}",
                lines.len()
            );
            tasks.push(Task::done(Event::AgentSubmitPrompt(tab_id, prompt.into())));
        }
        if tasks.is_empty() {
            return Task::none();
        }
        tasks.push(self.save_pr_watch());
        Task::batch(tasks)
    }

    // -----------------------------------------------------------------------
    // News across a home.

    /// Open news of `home`'s PRs, newest first: (root, PR, item).
    fn home_news(&self, home: &PrHome) -> Vec<(String, &WatchedPr, &NewsItem)> {
        let Some(roots) = self.home_roots(home) else {
            return Vec::new();
        };
        let mut news: Vec<(String, &WatchedPr, &NewsItem)> = self
            .keys_of_roots(roots)
            .into_iter()
            .filter_map(|(root, key)| Some((root, self.watched(&key)?)))
            .flat_map(|(root, watched)| {
                watched
                    .open_news()
                    .map(move |item| (root.clone(), watched, item))
            })
            .collect();
        news.sort_by(|a, b| b.2.at.cmp(&a.2.at));
        news
    }

    /// The chip badge of an open workspace: its newest news, e.g. "#841
    /// checks failed".
    pub(super) fn workspace_pr_badge(&self, uid: u64) -> Option<(String, iced::Color)> {
        let news = self.home_news(&PrHome::Open(uid));
        let (_, watched, item) = news.first()?;
        let mut label = format!(
            "#{} {}",
            watched.key.number,
            overview::shorten(&item.text, 26)
        );
        if news.len() > 1 {
            label.push_str(&format!(" +{}", news.len() - 1));
        }
        Some((label, news_color(&self.theme, item.kind)))
    }

    /// The Parked list's news line for a parked workspace.
    pub(super) fn parked_pr_news(&self, key: &ParkedKey) -> Option<(String, iced::Color)> {
        let news = self.home_news(&PrHome::Parked(key.clone()));
        let (_, watched, item) = news.first()?;
        let mut line = format!("{} {}", watched.key.label(), item.text);
        if news.len() > 1 {
            line.push_str(&format!(" \u{00B7} +{} more", news.len() - 1));
        }
        Some((line, news_color(&self.theme, item.kind)))
    }

    /// The next-to-merge row of every plan of `home`, with its root.
    fn home_next_rows(&self, home: &PrHome) -> Vec<(String, OrderedRow)> {
        let Some(roots) = self.home_roots(home) else {
            return Vec::new();
        };
        let briefs = self.pr_briefs();
        roots
            .iter()
            .filter_map(|root| {
                let plan = self.prs.plans.get(&root.root)?;
                let rows = merge_plan::order_plan(plan, &briefs);
                let next = merge_plan::next_to_merge(&rows)?.clone();
                Some((root.root.clone(), next))
            })
            .collect()
    }

    fn home_name(&self, home: &PrHome) -> String {
        match home {
            PrHome::Open(uid) => self
                .workspaces
                .iter()
                .find(|ws| ws.uid == *uid)
                .map(|ws| ws.name.clone())
                .unwrap_or_default(),
            PrHome::Parked(key) => key.name.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // Views.

    /// A PR's live line: state, checks, review, new comments, conflicts.
    fn pr_live_line<'a>(
        &self,
        watched: Option<&WatchedPr>,
    ) -> Row<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mono = iced::Font::with_name("Menlo");
        let part = |label: String, color: iced::Color| text(label).size(10).color(color).font(mono);
        let mut line = Row::new().spacing(6).align_y(iced::Alignment::Center);
        let Some(watched) = watched else {
            return line.push(part("not read yet".to_string(), theme.text_muted()));
        };
        let Some(core) = watched.snapshot.core.as_ref() else {
            let note = match &watched.error {
                Some(_) => "not read".to_string(),
                None => "not read yet".to_string(),
            };
            return line.push(part(note, theme.text_muted()));
        };
        let s = &watched.snapshot;
        if let Some(state) = core.state {
            let color = match state {
                merge_plan::PrState::Open => theme.text_secondary(),
                merge_plan::PrState::Draft => theme.text_muted(),
                merge_plan::PrState::Merged => theme.mauve(),
                merge_plan::PrState::Closed => theme.danger(),
            };
            line = line.push(part(state.label().to_string(), color));
        }
        if core.state.is_some_and(merge_plan::PrState::is_final) {
            return line;
        }
        let checks = s.checks();
        line = line.push(part(
            checks.label().to_string(),
            match checks {
                pr_watch::ChecksState::Pass => theme.success(),
                pr_watch::ChecksState::Fail => theme.danger(),
                pr_watch::ChecksState::Pending => theme.warning(),
                pr_watch::ChecksState::None => theme.text_muted(),
            },
        ));
        let review = s.review_decision();
        line = line.push(part(
            review.label().to_string(),
            match review {
                pr_watch::ReviewDecision::Approved => theme.success(),
                pr_watch::ReviewDecision::ChangesRequested => theme.danger(),
                _ => theme.text_muted(),
            },
        ));
        if watched.unseen_comments > 0 {
            line = line.push(part(
                format!(
                    "{} new comment{}",
                    watched.unseen_comments,
                    if watched.unseen_comments == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                theme.accent(),
            ));
        }
        if s.conflicting() == Some(true) {
            line = line.push(part("conflicts".to_string(), theme.danger()));
        }
        line
    }

    /// Width of a "#n" column `chars` characters wide (TRU-208).
    fn pr_number_width(&self, chars: usize) -> f32 {
        (chars as f32 * self.ui_font_small() * MONO_ADVANCE).ceil()
    }

    /// "#n" in a column `chars` wide, as a button that opens the PR in the
    /// Viewer.
    fn pr_number_cell<'a>(
        &self,
        key: &PrKey,
        chars: usize,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        button(
            text(format!("#{}", key.number))
                .size(self.ui_font_small())
                .color(self.theme.text_muted())
                .font(iced::Font::with_name("Menlo")),
        )
        .style(self.flat_button_style())
        .padding(0)
        .width(Length::Fixed(self.pr_number_width(chars)))
        .on_press(Event::Pr(PrEvent::OpenInViewer(key.clone())))
        .into()
    }

    /// One news item with dismiss, and "sent to chat ✓" once it was sent
    /// to the coordinator (TRU-197 sends actionable news by itself).
    fn view_pr_news_item<'a>(
        &'a self,
        watched: &WatchedPr,
        item: &NewsItem,
        with_pr: bool,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font_small = self.ui_font_small();
        let age = overview::parse_stamp(&item.at)
            .map(overview::age_label)
            .unwrap_or_default();
        let mut line = Row::new().spacing(6).align_y(iced::Alignment::Center).push(
            text(format!("\u{25CF} {}", overview::shorten(&item.text, 70)))
                .size(font_small - 1.0)
                .color(news_color(theme, item.kind))
                .width(Length::Fill),
        );
        // Its "#n" sits in the number column beside it; the repo says which.
        if with_pr {
            line = line.push(
                text(watched.key.repo.clone())
                    .size(10)
                    .color(theme.text_muted()),
            );
        }
        line = line.push(text(age).size(10).color(theme.text_muted()));
        if item.told {
            line = line.push(
                text("sent to chat \u{2713}")
                    .size(10)
                    .color(theme.text_muted()),
            );
        }
        line = line.push(
            button(text("\u{00D7}").size(10).color(theme.text_muted()))
                .style(self.flat_button_style())
                .padding([2, 6])
                .on_press(Event::Pr(PrEvent::NewsDismiss(
                    watched.key.id(),
                    item.key.clone(),
                ))),
        );
        line.into()
    }

    /// The facts a PR's title line shows (TRU-208): checks (✓ ✗ ●),
    /// changes requested, "n new", conflicts, blocked, held, draft and
    /// "not read". Opening the row never repeats them.
    fn pr_facts(
        &self,
        watched: Option<&WatchedPr>,
        row: Option<&OrderedRow>,
        draft: bool,
    ) -> Vec<(String, iced::Color)> {
        let theme = &self.theme;
        let mut facts: Vec<(String, iced::Color)> = Vec::new();
        let live = watched
            .and_then(|w| w.snapshot.core.as_ref())
            .is_some_and(|core| !core.state.is_some_and(PrState::is_final));
        if let (Some(watched), true) = (watched, live) {
            let snapshot = &watched.snapshot;
            match snapshot.checks() {
                pr_watch::ChecksState::Pass => facts.push(("\u{2713}".into(), theme.success())),
                pr_watch::ChecksState::Fail => facts.push(("\u{2717}".into(), theme.danger())),
                pr_watch::ChecksState::Pending => {
                    facts.push(("\u{25CF}".into(), theme.warning()));
                }
                pr_watch::ChecksState::None => {}
            }
            if snapshot.review_decision() == pr_watch::ReviewDecision::ChangesRequested {
                facts.push(("changes requested".into(), theme.danger()));
            }
            if watched.unseen_comments > 0 {
                facts.push((format!("{} new", watched.unseen_comments), theme.accent()));
            }
            if snapshot.conflicting() == Some(true) {
                facts.push(("conflicts".into(), theme.danger()));
            }
        }
        match row.map(|row| &row.status) {
            Some(RowStatus::Blocked(_)) => facts.push(("blocked".into(), theme.warning())),
            Some(RowStatus::Held(_)) => facts.push(("held".into(), theme.warning())),
            _ => {}
        }
        if let Some(unordered) = pr_unordered_fact(row) {
            facts.push((unordered, theme.warning()));
        }
        // A draft can't be merged; say so where it can't be missed.
        if draft {
            facts.push(("draft".into(), theme.warning()));
        }
        if watched.is_some_and(|w| w.error.is_some()) {
            facts.push(("not read".into(), theme.danger()));
        }
        facts
    }

    /// The facts as a row, with the width they take.
    fn pr_facts_row<'a>(
        &self,
        facts: Vec<(String, iced::Color)>,
    ) -> (Row<'a, Event, Theme, iced::Renderer>, f32) {
        let mono = iced::Font::with_name("Menlo");
        let mut width = 0.0;
        let mut line = Row::new()
            .spacing(PR_COLUMN_GAP)
            .align_y(iced::Alignment::Center);
        for (label, color) in facts {
            width += label.chars().count() as f32 * 10.0 * MONO_ADVANCE + PR_COLUMN_GAP;
            line = line.push(text(label).size(10).color(color).font(mono));
        }
        (line, width)
    }

    /// Where a PR list's titles start, from the row's content edge.
    fn pr_title_indent(&self, columns: PrColumns) -> f32 {
        let position = if columns.position {
            PR_POSITION_WIDTH + PR_COLUMN_GAP
        } else {
            0.0
        };
        position + self.pr_number_width(columns.number_chars) + PR_COLUMN_GAP
    }

    /// One PR row (TRU-208). Closed: "#n", the title (two lines at most),
    /// its facts and ▸, plus the filled Merge on the next-to-merge PR. A
    /// click anywhere (or ▸) opens it, adding under the title only what the
    /// title line does not say: repo, head → base, author and when it was
    /// read; the plan's wait or hold; news; the actions; read errors.
    fn view_pr_row<'a>(
        &'a self,
        root: &str,
        key: &PrKey,
        row: Option<&OrderedRow>,
        columns: PrColumns,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mono = iced::Font::with_name("Menlo");
        let font_small = self.ui_font_small();
        let text_muted = theme.text_muted();
        let watched = self.watched(key);
        let row_key = overview::RowKey::Pr(key.id());
        let expanded = self.overview_row_open(&row_key);
        let is_next = row.is_some_and(|row| row.status == RowStatus::Next);
        let parts = pr_row_parts(is_next, expanded);
        let draft = watched
            .and_then(WatchedPr::brief)
            .is_some_and(|brief| brief.state == merge_plan::PrState::Draft);
        let core = watched.and_then(|w| w.snapshot.core.as_ref());
        let open = watched
            .and_then(WatchedPr::brief)
            .is_some_and(|brief| brief.state == merge_plan::PrState::Open);
        // Approve and Merge wait for the checks of the latest read.
        let reason = watched
            .map(WatchedPr::checks_gate)
            .unwrap_or(pr_watch::ChecksGate::Running)
            .reason();
        // A draft can't be approved or merged until it is marked ready.
        let gated = |name: &str| match reason {
            _ if draft => format!("{name} (draft)"),
            Some(reason) if open => format!("{name} ({reason})"),
            _ => name.to_string(),
        };
        let merge_event = (open && reason.is_none())
            .then(|| Event::Pr(PrEvent::ActionOpen(key.clone(), ActionKind::Merge)));
        let action = |label: String, event: Option<Event>| {
            button(text(label).size(10).color(theme.text_primary()))
                .style(self.flat_button_style())
                .padding([1, 6])
                .on_press_maybe(event)
        };

        // The title line. The sidebar less the overview's padding (16),
        // the row's (12) and room for the scrollbar (10) is what it gets.
        let indent = self.pr_title_indent(columns);
        let text_width = self.sidebar_panel_width() - 16.0 - 12.0 - 10.0 - indent;
        let (facts, facts_width) = self.pr_facts_row(self.pr_facts(watched, row, draft));
        let mut taken = facts_width + 18.0 + PR_COLUMN_GAP;
        // A closed row of a multi-repo list leads with its repo.
        let repo_prefix = (columns.show_repo && !expanded)
            .then(|| format!("{} \u{00B7} ", repo_short_name(&key.repo)));
        let prefix_chars = repo_prefix.as_ref().map_or(0, |p| p.chars().count());
        let mut line = Row::new()
            .spacing(PR_COLUMN_GAP)
            .align_y(iced::Alignment::Center);
        if columns.position {
            line = line.push(
                text(
                    pr_row_position(row).map_or_else(String::new, |position| format!("{position}")),
                )
                .size(font_small)
                .color(text_muted)
                .font(mono)
                .width(Length::Fixed(PR_POSITION_WIDTH)),
            );
        }
        line = line.push(self.pr_number_cell(key, columns.number_chars));
        let mut merge_on_title: Option<Element<'a, Event, Theme, iced::Renderer>> =
            parts.merge_on_title.then(|| {
                let label = gated("Merge");
                taken += label.chars().count() as f32 * 10.0 * TEXT_ADVANCE + 16.0 + PR_COLUMN_GAP;
                // The only filled button: the action that is due now.
                button(text(label).size(10).color(theme.bg_base()))
                    .style(self.primary_button_style())
                    .padding([1, 8])
                    .on_press_maybe(merge_event.clone())
                    .into()
            });
        let title = core
            .map(|core| core.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| key.label());
        let title_budget =
            chars_that_fit(text_width - taken, font_small, 2).saturating_sub(prefix_chars);
        let title_text = overview::shorten(&title, title_budget.max(12));
        let title_widget: Element<'a, Event, Theme, iced::Renderer> = match repo_prefix {
            Some(prefix) => iced::widget::rich_text::<(), Event, _, _>([
                iced::widget::span(prefix).color(text_muted),
                iced::widget::span(title_text),
            ])
            .size(font_small)
            .color(theme.text_primary())
            .width(Length::Fill)
            .into(),
            None => text(title_text)
                .size(font_small)
                .color(theme.text_primary())
                .width(Length::Fill)
                .into(),
        };
        line = pr_title_row(
            line,
            title_widget,
            facts.into(),
            merge_on_title.take(),
            self.overview_disclosure(expanded, row_key.clone()),
        );

        // Opened: only what the title line does not say.
        let mut detail = Column::new().spacing(2);
        if parts.opened {
            let repo = repo_short_name(&key.repo).to_string();
            let mut meta = vec![repo];
            if let Some(core) = core {
                if !core.head_ref.is_empty() && !core.base_ref.is_empty() {
                    meta.push(format!("{} \u{2192} {}", core.head_ref, core.base_ref));
                }
                if !core.author.is_empty() {
                    meta.push(core.author.clone());
                }
            }
            if let Some(at) = watched
                .and_then(|w| w.checked_at.as_deref())
                .and_then(overview::parse_stamp)
            {
                meta.push(format!("read {}", overview::age_label(at)));
            }
            detail = detail.push(
                text(overview::shorten(
                    &meta.join(" \u{00B7} "),
                    chars_that_fit(text_width, 10.0, 1),
                ))
                .size(10)
                .color(text_muted),
            );
            if let Some(row) = row {
                match &row.status {
                    RowStatus::Blocked(waits) => {
                        detail = detail.push(
                            text(format!(
                                "waits for {}{}",
                                waits.join(", "),
                                if row.stacked { " (stacked)" } else { "" }
                            ))
                            .size(10)
                            .color(theme.warning()),
                        );
                    }
                    RowStatus::Unordered(reason) => {
                        detail = detail.push(
                            text(format!("unordered: {reason}"))
                                .size(10)
                                .color(theme.warning()),
                        );
                    }
                    RowStatus::Held(hold) => {
                        detail = detail.push(
                            row![
                                text(format!("hold: {hold}"))
                                    .size(10)
                                    .color(theme.warning())
                                    .width(Length::Fill),
                                button(text("clear hold").size(10).color(theme.accent()))
                                    .style(self.flat_button_style())
                                    .padding([1, 6])
                                    .on_press(Event::Pr(PrEvent::ClearHold(
                                        root.to_string(),
                                        key.clone()
                                    ))),
                            ]
                            .spacing(6)
                            .align_y(iced::Alignment::Center),
                        );
                    }
                    _ => {}
                }
                if let Some(note) = &row.entry.note {
                    detail = detail.push(
                        text(overview::shorten(note, 120))
                            .size(10)
                            .color(text_muted),
                    );
                }
            }
            if let Some(watched) = watched {
                for item in watched.open_news() {
                    detail = detail.push(self.view_pr_news_item(watched, item, false));
                }
            }
            let mut actions = Row::new().spacing(2).align_y(iced::Alignment::Center);
            actions = actions.push(action(
                "Review".to_string(),
                Some(Event::Pr(PrEvent::Review(key.clone()))),
            ));
            if !self.is_own_pr(key, watched) {
                actions = actions.push(action(
                    gated("Approve"),
                    (open && reason.is_none())
                        .then(|| Event::Pr(PrEvent::ActionOpen(key.clone(), ActionKind::Approve))),
                ));
            }
            if parts.merge_in_actions {
                actions = actions.push(action(gated("Merge"), merge_event));
            }
            if draft {
                actions = actions.push(
                    button(text("Ready for review").size(10).color(theme.accent()))
                        .style(self.flat_button_style())
                        .padding([1, 6])
                        .on_press(Event::Pr(PrEvent::ActionOpen(
                            key.clone(),
                            ActionKind::Ready,
                        ))),
                );
            }
            if parts.open_on_github {
                actions = actions.push(action(
                    "Open in browser".to_string(),
                    Some(Event::Pr(PrEvent::OpenInBrowser(key.clone()))),
                ));
            }
            detail = detail.push(actions);
            if let Some(error) = watched.and_then(|w| w.error.as_deref()) {
                detail = detail.push(
                    text(overview::shorten(&format!("not read: {error}"), 160))
                        .size(10)
                        .color(theme.danger()),
                );
            }
        }
        // The opened lines start under the title.
        let detail = row![
            iced::widget::Space::new().width(Length::Fixed(indent)),
            detail.width(Length::Fill)
        ];
        // The row has no other action: a click anywhere on it opens it.
        let hover_bg = theme.bg_overlay();
        button(Column::new().spacing(2).push(line).push(detail))
            .padding([3, 6])
            .width(Length::Fill)
            .style(move |_, status| button::Style {
                background: matches!(status, button::Status::Hovered).then(|| hover_bg.into()),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .on_press(Event::OverviewRowToggle(row_key))
            .into()
    }

    /// "PRs · merge order" for the active workspace's overview: each root's
    /// plan in merge order, then the PRs its chats made that no plan lists.
    /// `None` when the work has no PRs.
    pub(super) fn view_overview_prs<'a>(
        &'a self,
    ) -> Option<Element<'a, Event, Theme, iced::Renderer>> {
        let uid = self.active_workspace()?.uid;
        let roots = self.home_roots(&PrHome::Open(uid))?;
        let briefs = self.pr_briefs();
        let theme = &self.theme;
        let mono = iced::Font::with_name("Menlo");
        struct Group<'g> {
            root: &'g RootPrs,
            planned: bool,
            rows: Vec<OrderedRow>,
            unplanned: Vec<PrKey>,
            done: Vec<(PrKey, PrState)>,
            done_rows: Vec<OrderedRow>,
        }
        let now = chrono::Utc::now();
        let shown = |key: &PrKey| {
            self.watched(key)
                .is_none_or(|w| pr_watch::done_pr_shown(w.done_at(), now))
        };
        let state_of = |key: &PrKey| self.watched(key).and_then(|w| w.snapshot.state());
        let groups: Vec<Group<'_>> = roots
            .iter()
            .filter_map(|root| {
                let plan = self.prs.plans.get(&root.root);
                let all_rows = plan
                    .map(|plan| merge_plan::order_plan(plan, &briefs))
                    .unwrap_or_default();
                let all_unplanned: Vec<PrKey> = root
                    .facts
                    .iter()
                    .map(PrRef::key)
                    .filter(|key| plan.is_none_or(|plan| plan.entry(key).is_none()))
                    .collect();
                if all_rows.is_empty() && all_unplanned.is_empty() {
                    return None;
                }
                let (rows, done_rows): (Vec<_>, Vec<_>) = all_rows
                    .into_iter()
                    .partition(|row| !state_of(&row.entry.key()).is_some_and(PrState::is_final));
                let (unplanned, mut done) = split_done(all_unplanned, state_of);
                for row in &done_rows {
                    if let Some(state) = state_of(&row.entry.key()) {
                        done.push((row.entry.key(), state));
                    }
                }
                // Done PRs older than a day leave the section entirely.
                done.retain(|(key, _)| shown(key));
                let done_rows: Vec<OrderedRow> = done_rows
                    .into_iter()
                    .filter(|row| shown(&row.entry.key()))
                    .collect();
                if rows.is_empty() && unplanned.is_empty() && done.is_empty() {
                    return None;
                }
                Some(Group {
                    root,
                    planned: plan.is_some(),
                    rows,
                    unplanned,
                    done,
                    done_rows,
                })
            })
            .collect();
        if groups.is_empty() {
            return None;
        }
        let total: usize = groups
            .iter()
            .map(|g| g.rows.len() + g.unplanned.len())
            .sum();
        // Merged and closed PRs fold to a count at the right of the group's
        // line, the section's header when there is one group (TRU-208).
        let done_fold = |root: &str, done: &[(PrKey, PrState)]| {
            let open = self.prs.done_open.contains(root);
            let states: Vec<PrState> = done.iter().map(|(_, state)| *state).collect();
            button(
                text(format!(
                    "{} {}",
                    done_summary(&states),
                    overview::fold_marker(open)
                ))
                .size(10)
                .color(theme.text_muted()),
            )
            .style(self.flat_button_style())
            .padding([0, 4])
            .on_press(Event::Pr(PrEvent::DoneToggle(root.to_string())))
        };
        // One set of columns for the rows the list shows, so titles line
        // up across groups; folded done PRs do not count.
        let shown: Vec<(u64, bool, String)> = groups
            .iter()
            .flat_map(|group| {
                let done_open = self.prs.done_open.contains(&group.root.root);
                let done_rows = group.done_rows.iter().filter(move |_| done_open);
                let done_unplanned = group
                    .done
                    .iter()
                    .filter(move |(key, _)| {
                        done_open && !group.done_rows.iter().any(|r| r.entry.key() == *key)
                    })
                    .map(|(key, _)| (key.number, false, key.repo.clone()));
                group
                    .rows
                    .iter()
                    .chain(done_rows)
                    .map(|row| {
                        let key = row.entry.key();
                        (key.number, true, key.repo)
                    })
                    .chain(
                        group
                            .unplanned
                            .iter()
                            .map(|key| (key.number, false, key.repo.clone())),
                    )
                    .chain(done_unplanned)
            })
            .collect();
        let mut columns = pr_columns(shown.iter().map(|(number, planned, _)| (*number, *planned)));
        columns.show_repo = show_repo_names(shown.iter().map(|(_, _, repo)| repo.as_str()));
        let several = groups.len() > 1;
        let header_fold = groups
            .first()
            .filter(|group| !several && !group.done.is_empty())
            .map(|group| done_fold(&group.root.root, &group.done).into());
        let mut section = Column::new().spacing(2).push(self.overview_section_header(
            "Pull requests",
            format!("{total} open"),
            header_fold,
        ));
        for group in groups {
            let Group {
                root,
                planned,
                rows,
                unplanned,
                done,
                done_rows,
            } = group;
            if several {
                let mut title = row![text(overview::shorten(&root.title, 50))
                    .size(10)
                    .color(theme.text_secondary())
                    .width(Length::Fill)]
                .align_y(iced::Alignment::Center);
                if !done.is_empty() {
                    title = title.push(done_fold(&root.root, &done));
                }
                section = section.push(title);
            }
            if planned
                && self
                    .prs
                    .plans
                    .get(&root.root)
                    .is_some_and(|p| p.source.as_deref() == Some("backfill"))
            {
                section = section.push(
                    text("built from open PRs · independent PRs by repo, then PR number ascending")
                        .size(10)
                        .color(theme.text_muted()),
                );
            }
            if let Some(error) = self.prs.plan_errors.get(&root.root) {
                section = section.push(text(error.clone()).size(10).color(theme.danger()));
            }
            for row in &rows {
                section = section.push(self.view_pr_row(
                    &root.root,
                    &row.entry.key(),
                    Some(row),
                    columns,
                ));
            }
            if !unplanned.is_empty() && !rows.is_empty() {
                section = section.push(
                    text("not in the plan")
                        .size(10)
                        .color(theme.text_muted())
                        .font(mono),
                );
            }
            for key in &unplanned {
                section = section.push(self.view_pr_row(&root.root, key, None, columns));
            }
            if !done.is_empty() && self.prs.done_open.contains(&root.root) {
                for row in &done_rows {
                    section = section.push(self.view_pr_row(
                        &root.root,
                        &row.entry.key(),
                        Some(row),
                        columns,
                    ));
                }
                for (key, _) in done
                    .iter()
                    .filter(|(key, _)| !done_rows.iter().any(|r| r.entry.key() == *key))
                {
                    section = section.push(self.view_pr_row(&root.root, key, None, columns));
                }
            }
        }
        Some(section.into())
    }

    /// The bar's "↑n" (n to merge) button for the Ready to merge list; hidden
    /// when no plan has a next PR and there is no news.
    pub(super) fn view_ready_button<'a>(
        &'a self,
    ) -> Option<Element<'a, Event, Theme, iced::Renderer>> {
        let homes: Vec<&PrHome> = self.prs.homes.iter().map(|(home, _)| home).collect();
        let next: usize = homes
            .iter()
            .map(|home| self.home_next_rows(home).len())
            .sum();
        let news: usize = homes.iter().map(|home| self.home_news(home).len()).sum();
        if next == 0 && news == 0 {
            return None;
        }
        // Short on the bar (TRU-192), the full text on hover.
        let mut full = format!("{next} to merge");
        if news > 0 {
            full.push_str(&format!(" \u{00B7} {news} new"));
        }
        let color = if news > 0 {
            self.theme.accent()
        } else {
            self.theme.text_muted()
        };
        let btn = self.bar_text_button(
            format!("\u{2191}{next}"),
            color,
            Some(Event::Pr(PrEvent::ReadyListToggle)),
        );
        Some(self.bar_hint_area(btn, full))
    }

    /// "Ready to merge" across this Mac's workspaces, open and parked: each
    /// plan's next PR and every open news item.
    pub(super) fn view_ready_list(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mono = iced::Font::with_name("Menlo");
        let mut rows = Column::new().spacing(8);
        let mut any = false;
        for (home, _) in &self.prs.homes {
            let next = self.home_next_rows(home);
            let news = self.home_news(home);
            if next.is_empty() && news.is_empty() {
                continue;
            }
            any = true;
            let parked = matches!(home, PrHome::Parked(_));
            let mut group = Column::new().spacing(3).push(
                row![
                    text(self.home_name(home))
                        .size(font_small)
                        .color(theme.text_primary())
                        .font(mono)
                        .width(Length::Fill),
                    button(text(if parked { "Resume" } else { "Open" }).size(font_small - 1.0))
                        .style(self.ghost_button_style())
                        .padding([2, 8])
                        .on_press(Event::Pr(PrEvent::ReadyGo(home.clone()))),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            );
            // "#n" in a column as wide as the card's widest; the title and
            // news start after it. The title line carries the facts, as in
            // the overview (TRU-208).
            let columns = pr_columns(
                next.iter()
                    .map(|(_, row)| (row.entry.key().number, false))
                    .chain(
                        news.iter()
                            .map(|(_, watched, _)| (watched.key.number, false)),
                    ),
            );
            let number_chars = columns.number_chars;
            let text_width = READY_LIST_WIDTH - 16.0 - self.pr_title_indent(columns);
            for (_, row) in &next {
                let key = row.entry.key();
                let watched = self.watched(&key);
                let draft = watched
                    .and_then(WatchedPr::brief)
                    .is_some_and(|brief| brief.state == merge_plan::PrState::Draft);
                let (facts, facts_width) =
                    self.pr_facts_row(self.pr_facts(watched, Some(row), draft));
                let taken = facts_width
                    + key.repo.chars().count() as f32 * 10.0 * TEXT_ADVANCE
                    + PR_COLUMN_GAP;
                let title = watched
                    .and_then(|w| w.snapshot.core.as_ref())
                    .map(|core| core.title.clone())
                    .unwrap_or_default();
                group = group.push(
                    row![
                        self.pr_number_cell(&key, number_chars),
                        text(overview::shorten(
                            &format!("next: {title}"),
                            chars_that_fit(text_width - taken, font_small - 1.0, 2),
                        ))
                        .size(font_small - 1.0)
                        .color(theme.accent())
                        .width(Length::Fill),
                        facts,
                        text(key.repo.clone()).size(10).color(theme.text_muted()),
                    ]
                    .spacing(PR_COLUMN_GAP)
                    .align_y(iced::Alignment::Center),
                );
            }
            for (_, watched, item) in &news {
                group = group.push(
                    row![
                        self.pr_number_cell(&watched.key, number_chars),
                        self.view_pr_news_item(watched, item, true)
                    ]
                    .spacing(PR_COLUMN_GAP)
                    .align_y(iced::Alignment::Center),
                );
            }
            let bg = theme.bg_base();
            rows = rows.push(
                container(group)
                    .padding([6, 8])
                    .style(move |_| container::Style {
                        background: Some(bg.into()),
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
            );
        }
        if !any {
            rows = rows.push(
                text("Nothing to merge and no news.")
                    .size(font_small)
                    .color(theme.text_muted()),
            );
        }
        let card = Column::new()
            .spacing(10)
            .width(Length::Fixed(READY_LIST_WIDTH))
            .push(text("Ready to merge").size(font).color(theme.text_primary()))
            .push(
                text("The next PR of each merge plan, and what changed on the work's PRs. GitTerm only watches; you approve and merge from the workspace overview.")
                    .size(font_small)
                    .color(theme.text_secondary()),
            )
            .push(container(scrollable(rows)).max_height(460.0))
            .push(
                row![
                    iced::widget::Space::new().width(Length::Fill),
                    button(text("Close").size(font_small))
                        .style(self.secondary_button_style())
                        .padding([6, 12])
                        .on_press(Event::Pr(PrEvent::ReadyListToggle)),
                ]
                .align_y(iced::Alignment::Center),
            );
        self.view_sheet_card(card)
    }

    /// The Approve / Merge confirmation: the exact command, the account it
    /// runs as and the merge method. Only its Run button runs anything.
    pub(super) fn view_pr_action_confirm(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mono = iced::Font::with_name("Menlo");
        let Some(confirm) = &self.prs.action else {
            return iced::widget::Space::new().into();
        };
        let action = confirm.action();
        let key = &confirm.key;
        let mut card = Column::new().spacing(10).width(Length::Fixed(560.0)).push(
            text(format!("{} {}", action.verb(), key.label()))
                .size(font)
                .color(theme.text_primary()),
        );
        if let Some(core) = self.watched(key).and_then(|w| w.snapshot.core.as_ref()) {
            card = card.push(
                text(overview::shorten(&core.title, 90))
                    .size(font_small)
                    .color(theme.text_secondary()),
            );
        }
        card = card.push(self.pr_live_line(self.watched(key)));
        // Where the plan stands on it.
        let briefs = self.pr_briefs();
        let plan_status = self.prs.plans.values().find_map(|plan| {
            merge_plan::order_plan(plan, &briefs)
                .into_iter()
                .find(|row| row.entry.key().id() == key.id())
        });
        if let Some(row) = plan_status {
            let note = match &row.status {
                RowStatus::Next => Some(("the plan's next to merge".to_string(), theme.accent())),
                RowStatus::Blocked(waits) => Some((
                    format!("the plan has it wait for {}", waits.join(", ")),
                    theme.warning(),
                )),
                RowStatus::Unordered(reason) => {
                    Some((format!("unordered: {reason}"), theme.warning()))
                }
                RowStatus::Held(hold) => {
                    Some((format!("the plan holds it: {hold}"), theme.warning()))
                }
                _ => None,
            };
            if let Some((note, color)) = note {
                card = card.push(text(note).size(font_small - 1.0).color(color));
            }
        }
        let owner = key.owner().to_string();
        let account_line = match &confirm.account {
            Some(user) if pr_watch::valid_login(user) => {
                format!("Runs as {user} (github_accounts maps {owner} to it)")
            }
            Some(user) => format!(
                "github_accounts maps {owner} to {user:?}, which is not a GitHub login; fix github-accounts.json"
            ),
            None => match self.prs.logins.get(&None) {
                Some(login) => format!(
                    "Runs as gh's active account ({login}): github_accounts has no entry for {owner}"
                ),
                None => format!(
                    "Runs as gh's active account: github_accounts has no entry for {owner}"
                ),
            },
        };
        let account_ok = confirm.account.as_deref().is_none_or(pr_watch::valid_login);
        card = card.push(text(account_line).size(font_small).color(if account_ok {
            theme.text_secondary()
        } else {
            theme.danger()
        }));
        if confirm.kind == ActionKind::Merge {
            let mut methods = Row::new().spacing(4).align_y(iced::Alignment::Center).push(
                text("Method:")
                    .size(font_small - 1.0)
                    .color(theme.text_muted()),
            );
            for method in MergeMethod::ALL {
                let selected = method == confirm.method;
                let mut b = button(text(method.label()).size(font_small - 1.0).color(
                    if selected {
                        theme.accent()
                    } else {
                        theme.text_primary()
                    },
                ))
                .style(self.ghost_button_style())
                .padding([2, 8]);
                if !confirm.running {
                    b = b.on_press(Event::Pr(PrEvent::ActionMethod(method)));
                }
                methods = methods.push(b);
            }
            methods = methods.push(
                text(if confirm.method_from_plan {
                    "(the plan's)"
                } else {
                    "(default; the plan names none)"
                })
                .size(10)
                .color(theme.text_muted()),
            );
            card = card.push(methods);
        }
        let command = pr_watch::action_command_line(action, key, confirm.account.as_deref());
        let code_bg = theme.bg_crust();
        card = card.push(
            container(
                text(command)
                    .size(font_small)
                    .color(theme.text_primary())
                    .font(mono),
            )
            .padding([6, 8])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(code_bg.into()),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
        if let Some(error) = &confirm.error {
            card = card.push(
                text(overview::shorten(error, 400))
                    .size(font_small - 1.0)
                    .color(theme.danger()),
            );
        } else if confirm.running {
            card = card.push(
                text("Running\u{2026}")
                    .size(font_small - 1.0)
                    .color(theme.text_muted()),
            );
        }
        let failed = confirm.error.is_some();
        let mut close = button(text(if failed { "Close" } else { "Cancel" }).size(font_small))
            .style(self.secondary_button_style())
            .padding([6, 12]);
        if !confirm.running {
            close = close.on_press(Event::Pr(PrEvent::ActionClose));
        }
        let mut run = button(text(action.verb()).size(font_small))
            .style(self.primary_button_style())
            .padding([6, 12]);
        if !confirm.running && account_ok {
            run = run.on_press(Event::Pr(PrEvent::ActionRun));
        }
        card = card.push(
            row![iced::widget::Space::new().width(Length::Fill), close, run]
                .spacing(8)
                .align_y(iced::Alignment::Center),
        );
        self.view_sheet_card(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_pr(number: u64) -> PrRef {
        let key = PrKey::new("acme/api", number);
        PrRef {
            repo: key.repo.clone(),
            number,
            url: key.url(),
        }
    }

    fn delegated_task_fixture() -> (tempfile::TempDir, TaskStore, chrono::DateTime<chrono::Utc>) {
        use gitterm::tasks::{
            Delegation, DelegationChild, DelegationKind, DelegationParent, GitBase, NewDelegation,
            RepositoryIdentity,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = TaskStore::load(dir.path().join("tasks.json")).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for (id, parent, age) in [
            ("X", "chat-a", 0),
            ("Y", "chat-b", 1),
            ("old", "chat-a", 15),
            ("boundary", "chat-a", 14),
        ] {
            let at = (now - chrono::Duration::days(age)).to_rfc3339();
            store
                .insert(TaskRecord::new_draft(
                    NewTaskRecord {
                        task_id: id.into(),
                        title: "Worker task".into(),
                        objective: "Implement a change".into(),
                        workspace: TaskWorkspaceIdentity {
                            name: "workspace".into(),
                            location: WorkspaceLocationIdentity::Local {
                                directory: dir.path().to_path_buf(),
                            },
                        },
                        repository: RepositoryIdentity {
                            common_dir: dir.path().join(".git"),
                            remote_url: Some("git@github.com:acme/api.git".into()),
                        },
                        issue: None,
                        base: GitBase {
                            reference: "v6".into(),
                            commit: "0123456789abcdef".into(),
                        },
                        branch: format!("task/{id}"),
                        executor: ExecutorTarget::Local,
                        harness: None,
                        stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
                    },
                    &at,
                ))
                .unwrap();
            store
                .insert_delegation(Delegation::new_requested(
                    NewDelegation {
                        kind: DelegationKind::Implement,
                        parent: DelegationParent {
                            session_uid: parent.into(),
                            chat_session_id: Some(parent.into()),
                            workspace: "workspace".into(),
                            cwd: dir.path().to_path_buf(),
                        },
                        child: DelegationChild::TaskSession {
                            task_id: id.into(),
                            task_session_id: None,
                            conversation: None,
                            preset_name: Some("Codex".into()),
                            model: None,
                            role: None,
                        },
                        brief: "Implement a change".into(),
                        target: None,
                        previous: None,
                        options: Default::default(),
                    },
                    &at,
                ))
                .unwrap();
        }
        (dir, store, now)
    }

    fn task_root(chat: &str) -> String {
        match chat {
            "chat-a" | "claude-worker" => "root-a".into(),
            "chat-b" => "root-b".into(),
            _ => chat.into(),
        }
    }

    #[tokio::test]
    async fn task_branch_prs_belong_to_the_delegating_chats_root() {
        let (_dir, store, now) = delegated_task_fixture();
        let branches = task_branches(&store, now);
        assert_eq!(branches.len(), 3);
        assert!(!branches
            .iter()
            .any(|(_, branch)| branch.branch == "task/old"));
        assert!(branches
            .iter()
            .any(|(_, branch)| branch.branch == "task/boundary"));
        let results = lookup_task_branches(
            branches.iter().map(|(_, branch)| branch.clone()).collect(),
            BTreeMap::from([("ACME".into(), "acme-bot".into())]),
            |branch, account| async move {
                assert_eq!(branch.repo, "acme/api");
                assert_eq!(account.as_deref(), Some("acme-bot"));
                Ok(match branch.branch.as_str() {
                    "task/X" => vec![task_pr(7)],
                    "task/Y" => vec![task_pr(8)],
                    _ => vec![],
                })
            },
        )
        .await;
        let mut prs = PrsState::default();
        prs.apply_task_branches(results, |message| panic!("{message}"));
        assert_eq!(
            collect_root_prs(&[], "root-a", &prs, &branches, task_root),
            vec![task_pr(7)]
        );
        assert_eq!(
            collect_root_prs(&[], "root-b", &prs, &branches, task_root),
            vec![task_pr(8)]
        );
    }

    #[test]
    fn task_branch_prs_deduplicate_claude_facts_and_live_prs() {
        let (_dir, store, now) = delegated_task_fixture();
        let branches = task_branches(&store, now);
        let branch = branches
            .iter()
            .find(|(_, branch)| branch.branch == "task/X")
            .unwrap()
            .1
            .clone();
        let mut prs = PrsState::default();
        prs.apply_task_branches(vec![(branch, Ok(vec![task_pr(7)]))], |_| {});
        prs.note_live_prs("claude-worker", &[task_pr(7)]);
        let index = vec![chats::ChatIndexEntry {
            id: "claude-worker".into(),
            backend: chats::ChatBackend::Claude,
            path: PathBuf::new(),
            cwd: PathBuf::new(),
            repo_root: None,
            is_worktree: true,
            branch: Some("task/X".into()),
            title: "Worker".into(),
            latest: None,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            dead_cwd: false,
            facts: Some(chat_facts::ChatFacts {
                prs: vec![PrRef {
                    repo: "Acme/API".into(),
                    ..task_pr(7)
                }],
                ..Default::default()
            }),
            root_id: Some("root-a".into()),
        }];
        let found = collect_root_prs(&index, "root-a", &prs, &branches, task_root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key().id(), "acme/api#7");
    }

    #[tokio::test]
    async fn task_branch_lookup_error_logs_context_and_preserves_known_prs() {
        let (_dir, store, now) = delegated_task_fixture();
        let branches = task_branches(&store, now);
        let branch = branches[0].1.clone();
        let mut prs = PrsState::default();
        prs.apply_task_branches(vec![(branch.clone(), Ok(vec![task_pr(7)]))], |_| {});
        let before = collect_root_prs(&[], "root-a", &prs, &branches, task_root);
        let results = lookup_task_branches(vec![branch], BTreeMap::new(), |_, _| async {
            Err("lookup unavailable".into())
        })
        .await;
        let mut logs = Vec::new();
        prs.apply_task_branches(results, |message| logs.push(message.to_string()));
        assert_eq!(
            logs,
            vec!["[pr-watch] task PRs for acme/api head task/X: not read: lookup unavailable"]
        );
        assert_eq!(
            collect_root_prs(&[], "root-a", &prs, &branches, task_root),
            before
        );
    }

    #[tokio::test]
    async fn empty_task_branch_answers_are_looked_up_again() {
        let branch = TaskBranch {
            repo: "acme/api".into(),
            branch: "task/X".into(),
        };
        let mut prs = PrsState::default();
        let first = lookup_task_branches(vec![branch.clone()], BTreeMap::new(), |_, _| async {
            Ok(vec![])
        })
        .await;
        prs.apply_task_branches(first, |_| {});
        let second = lookup_task_branches(vec![branch.clone()], BTreeMap::new(), |_, _| async {
            Ok(vec![task_pr(7)])
        })
        .await;
        prs.apply_task_branches(second, |_| {});
        assert_eq!(prs.task_branch_prs[&branch], vec![task_pr(7)]);
        prs.request_row_refresh("acme/api#7".into(), true, Instant::now());
        assert!(prs.task_branches_refresh);
    }

    #[test]
    fn task_branch_discovery_runs_on_start_and_refresh_without_overlapping() {
        let mut prs = PrsState::default();
        let now = Instant::now();
        assert!(prs.task_branches_due(now));
        prs.task_branches_read_at = Some(now);
        assert!(!prs.task_branches_due(now));
        assert!(prs.task_branches_due(now + pr_watch::OPEN_INTERVAL));
        prs.request_row_refresh("acme/api#7".into(), true, now);
        assert!(prs.task_branches_due(now));
        prs.task_branches_in_flight = true;
        assert!(!prs.task_branches_due(now + pr_watch::OPEN_INTERVAL));
    }

    #[test]
    fn focus_marks_open_workspace_prs_due_but_not_parked_prs() {
        let mut prs = PrsState::default();
        let now = Instant::now();
        prs.request_open_workspace_refreshes(
            &[
                (PrHome::Open(1), vec!["open-pr".to_string()]),
                (
                    PrHome::Parked(ParkedKey {
                        name: "parked".to_string(),
                        dir: "/parked".to_string(),
                    }),
                    vec!["parked-pr".to_string()],
                ),
            ],
            now,
        );
        assert!(prs.due_now.contains("open-pr"));
        assert!(!prs.due_now.contains("parked-pr"));
    }

    #[test]
    fn opening_a_pr_row_marks_only_that_pr_due() {
        let mut prs = PrsState::default();
        let now = Instant::now();
        prs.request_row_refresh("closed-row".to_string(), false, now);
        assert!(!prs.due_now.contains("closed-row"));
        prs.request_row_refresh("opened-row".to_string(), true, now);
        assert_eq!(prs.due_now, ["opened-row".to_string()].into());
    }

    #[test]
    fn immediate_refresh_is_debounced_per_pr_for_ten_seconds() {
        let mut prs = PrsState::default();
        let now = Instant::now();
        assert!(prs.request_immediate_refresh("same-pr".to_string(), now));
        prs.due_now.clear();
        assert!(!prs.request_immediate_refresh("same-pr".to_string(), now + Duration::from_secs(9)));
        assert!(prs.due_now.is_empty());
        assert_eq!(
            prs.deferred_due.get("same-pr"),
            Some(&(now + Duration::from_secs(10)))
        );
        prs.promote_deferred(now + Duration::from_secs(9));
        assert!(prs.due_now.is_empty());
        prs.promote_deferred(now + Duration::from_secs(10));
        assert!(prs.due_now.contains("same-pr"));
    }

    #[test]
    fn action_refresh_after_a_recent_row_read_is_deferred_not_lost() {
        let mut prs = PrsState::default();
        let now = Instant::now();
        prs.request_row_refresh("owner/repo#1".to_string(), true, now);
        prs.take_due("owner/repo#1"); // the row refresh completed

        assert!(!prs
            .request_immediate_refresh("owner/repo#1".to_string(), now + Duration::from_secs(2),)); // Ready / Approve / Merge completed during the debounce window
        prs.promote_deferred(now + Duration::from_secs(9));
        assert!(!prs.due_now.contains("owner/repo#1"));
        prs.promote_deferred(now + Duration::from_secs(10));
        assert!(prs.due_now.contains("owner/repo#1"));
    }

    #[test]
    fn focus_requests_during_a_full_poll_do_not_starve_the_next_pr_batch() {
        let mut prs = PrsState::default();
        let now = Instant::now();
        let ids = (1..=26)
            .map(|n| format!("owner/repo#{n}"))
            .collect::<Vec<_>>();
        for id in &ids {
            prs.request_immediate_refresh(id.clone(), now);
        }

        let first_batch = ordered_poll_ids(&ids, &prs.due_order)
            .into_iter()
            .take(PR_POLL_MAX_ITEMS)
            .collect::<Vec<_>>();
        for id in &first_batch {
            prs.take_due(id);
        }
        assert_eq!(
            prs.due_order.front().map(String::as_str),
            Some("owner/repo#26")
        );

        // Focus repeats while the first 25 PRs are still in flight. Their
        // requests append after the older, still queued 26th PR.
        for id in &ids {
            prs.request_immediate_refresh(id.clone(), now + Duration::from_secs(11));
        }
        let next_batch = ordered_poll_ids(&ids, &prs.due_order)
            .into_iter()
            .take(PR_POLL_MAX_ITEMS)
            .collect::<Vec<_>>();
        assert!(next_batch.iter().any(|id| id == "owner/repo#26"));
    }

    #[test]
    fn split_done_keeps_open_and_unknown_apart_from_final() {
        let states = |k: &&str| match *k {
            "a" => Some(PrState::Open),
            "b" => Some(PrState::Merged),
            "c" => Some(PrState::Closed),
            "d" => Some(PrState::Draft),
            _ => None,
        };
        let (open, done) = split_done(vec!["a", "b", "c", "d", "e"], states);
        assert_eq!(open, vec!["a", "d", "e"]);
        assert_eq!(done, vec![("b", PrState::Merged), ("c", PrState::Closed)]);
    }

    #[test]
    fn done_summary_names_merged_and_closed() {
        use PrState::{Closed, Merged};
        assert_eq!(done_summary(&[Merged, Merged]), "2 merged");
        assert_eq!(
            done_summary(&[Merged, Closed]),
            "1 merged \u{00B7} 1 closed"
        );
        assert_eq!(done_summary(&[Closed]), "1 closed");
    }

    #[test]
    fn the_pr_columns_fit_the_list() {
        // No plan: no position column; "#n" as wide as the widest.
        assert_eq!(
            pr_columns([(93, false), (591, false)]),
            PrColumns {
                position: false,
                number_chars: 4,
                show_repo: false
            }
        );
        // One planned PR gives the whole list the position column.
        assert_eq!(
            pr_columns([(7, false), (12345, true)]),
            PrColumns {
                position: true,
                number_chars: 6,
                show_repo: false
            }
        );
        assert_eq!(pr_columns([]).number_chars, 0);
    }

    #[test]
    fn the_repo_is_named_only_when_a_list_spans_repos() {
        assert!(!show_repo_names([]));
        assert!(!show_repo_names(["acme/api"]));
        assert!(!show_repo_names(["acme/api", "acme/api"]));
        assert!(show_repo_names(["acme/api", "acme/api", "acme/web"]));
        assert_eq!(repo_short_name("acme/bebop-server"), "bebop-server");
    }

    #[test]
    fn text_is_cut_to_the_lines_it_gets() {
        // 11 px font: 6.05 px a character, 33 a line in 200 px.
        assert_eq!(chars_that_fit(200.0, 11.0, 2), 66);
        assert_eq!(chars_that_fit(200.0, 11.0, 1), 33);
        assert_eq!(chars_that_fit(10.0, 11.0, 2), 12);
    }

    #[test]
    fn a_closed_pr_row_shows_no_news_and_only_the_due_merge() {
        let closed = pr_row_parts(false, false);
        assert!(!closed.opened && !closed.merge_on_title && !closed.merge_in_actions);
        assert!(!closed.open_on_github);
        let next_closed = pr_row_parts(true, false);
        assert!(next_closed.merge_on_title && !next_closed.opened);
        let opened = pr_row_parts(false, true);
        assert!(opened.opened && opened.merge_in_actions && !opened.merge_on_title);
        assert!(opened.open_on_github && pr_row_parts(true, true).open_on_github);
        // The next PR keeps Merge on its title line; the actions line
        // does not repeat it.
        let next_opened = pr_row_parts(true, true);
        assert!(next_opened.opened && next_opened.merge_on_title);
        assert!(!next_opened.merge_in_actions);
    }

    #[test]
    fn unordered_pr_rows_keep_the_warning_and_hide_the_numeric_position() {
        let row = OrderedRow {
            position: 3,
            entry: merge_plan::PlanEntry {
                repo: "acme/api".into(),
                number: 12,
                step: None,
                after: Vec::new(),
                hold: None,
                method: None,
                unordered_reason: Some("ambiguous branch dependency".into()),
                note: None,
            },
            waits: Vec::new(),
            stacked: false,
            status: RowStatus::Unordered("ambiguous branch dependency".into()),
        };

        assert_eq!(pr_row_position(Some(&row)), None);
        assert_eq!(
            pr_unordered_fact(Some(&row)).as_deref(),
            Some("unordered: ambiguous branch dependency")
        );
    }

    #[test]
    fn title_row_keeps_the_merge_and_disclosure_child_slots() {
        let title = || text("title").into();
        let facts = || text("facts").into();
        let disclosure = || text("disclosure").into();
        let merge = || text("merge").into();
        let prefix = || Row::new().push(text("position")).push(text("number"));
        let with_merge: Element<'_, Event, Theme, iced::Renderer> =
            pr_title_row(prefix(), title(), facts(), Some(merge()), disclosure()).into();
        let without_merge: Element<'_, Event, Theme, iced::Renderer> =
            pr_title_row(prefix(), title(), facts(), None, disclosure()).into();

        // Inspect the real Iced row widget trees built by production code.
        // The disclosure is appended last, so equal child counts preserve its index.
        let with_merge_children = with_merge.as_widget().children().len();
        let without_merge_children = without_merge.as_widget().children().len();
        assert_eq!(with_merge_children, 6);
        assert_eq!(without_merge_children, with_merge_children);
        assert_eq!(without_merge_children - 1, 5);
        assert!(pr_row_parts(true, false).merge_on_title);
        assert!(!pr_row_parts(false, false).merge_on_title);
    }

    #[test]
    fn pr_url_uses_watched_url_or_canonical_fallback() {
        let key = PrKey::new("owner/repo", 12);
        assert_eq!(
            pr_url_for(&key, Some("https://github.com/owner/repo/pull/12")),
            "https://github.com/owner/repo/pull/12"
        );
        assert_eq!(
            pr_url_for(&key, Some("")),
            "https://github.com/owner/repo/pull/12"
        );
        assert_eq!(
            pr_url_for(&key, None),
            "https://github.com/owner/repo/pull/12"
        );
    }

    fn confirming(key: &PrKey) -> PrsState {
        PrsState {
            action: Some(PrActionConfirm {
                key: key.clone(),
                kind: ActionKind::Merge,
                method: MergeMethod::default(),
                method_from_plan: false,
                account: None,
                running: true,
                error: None,
            }),
            ..PrsState::default()
        }
    }

    #[test]
    fn a_successful_action_closes_its_confirmation() {
        let key = PrKey::new("example-org/example-repo", 7);
        let mut prs = confirming(&key);
        assert!(prs.action_done(&key, Ok(String::new())));
        assert!(prs.action.is_none());
        assert!(prs.due_now.contains(&key.id()));
    }

    #[test]
    fn a_failed_action_keeps_its_confirmation_with_the_error() {
        let key = PrKey::new("example-org/example-repo", 7);
        let mut prs = confirming(&key);
        assert!(!prs.action_done(&key, Err("not mergeable".to_string())));
        let confirm = prs.action.as_ref().expect("confirmation stays open");
        assert!(!confirm.running);
        assert_eq!(confirm.error.as_deref(), Some("not mergeable"));
        assert!(prs.due_now.contains(&key.id()));
    }

    #[test]
    fn news_for_a_coordinator_is_one_line_per_item() {
        let mut watched = WatchedPr::new(PrKey::new("acme/api", 12));
        watched.snapshot.core = Some(pr_watch::PullCore {
            title: "PRJ-101: add the export".to_string(),
            url: "https://github.com/acme/api/pull/12".to_string(),
            ..pr_watch::PullCore::default()
        });
        let item = |key: &str, kind, text: &str| NewsItem {
            key: key.to_string(),
            kind,
            text: text.to_string(),
            at: "2026-11-01T12:00:00Z".to_string(),
            dismissed: false,
            told: false,
            approvals_only: false,
        };
        let failed = item(
            "checks_failed:a1",
            NewsKind::ChecksFailed,
            "checks failed: test",
        );
        let conflict = item(
            "conflicting:a1",
            NewsKind::Conflicting,
            "has merge conflicts",
        );
        // One item: the line a single news item has always sent.
        assert_eq!(
            tell_prompt(&[news_line(&watched, &failed)]),
            "GitTerm delivery (not typed by the user):\n\
             GitTerm PR watch: acme/api#12 \"PRJ-101: add the export\": checks failed: test. \
             https://github.com/acme/api/pull/12"
        );
        // Several: one prompt, a line each. A PR without a read yet falls
        // back to its URL and no title.
        let unread = WatchedPr::new(PrKey::new("acme/web", 3));
        assert_eq!(
            tell_prompt(&[
                news_line(&watched, &failed),
                news_line(&watched, &conflict),
                news_line(&unread, &conflict),
            ]),
            format!(
                "GitTerm delivery (not typed by the user):\n\
                 GitTerm PR watch: acme/api#12 \"PRJ-101: add the export\": checks failed: test. \
                 https://github.com/acme/api/pull/12\n\
                 GitTerm PR watch: acme/api#12 \"PRJ-101: add the export\": has merge conflicts. \
                 https://github.com/acme/api/pull/12\n\
                 GitTerm PR watch: acme/web#3: has merge conflicts. {}",
                PrKey::new("acme/web", 3).url()
            )
        );
    }

    #[test]
    fn another_prs_result_leaves_the_open_confirmation_alone() {
        let open = PrKey::new("example-org/example-repo", 7);
        let other = PrKey::new("example-org/example-repo", 8);
        let mut prs = confirming(&open);
        assert!(!prs.action_done(&other, Ok(String::new())));
        let confirm = prs.action.as_ref().expect("confirmation stays open");
        assert!(confirm.running);
        assert!(confirm.error.is_none());
        assert!(prs.due_now.contains(&other.id()));
    }
}
