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
//! root's plan entries and the PRs its chats' facts recorded. Open
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
//! the chip. "Tell coordinator" still sends any one item when pressed, and
//! `pr_watch.auto_tell: false` in settings.json turns the rest off.

use super::*;

use gitterm::merge_plan::{
    self, MergeMethod, MergePlan, OrderedRow, PrBrief, PrKey, PrRef, PrState, RowStatus,
};
use gitterm::pr_watch::{self, NewsItem, NewsKind, PrAction, WatchedPr};
use gitterm::task_mcp::MergePlanSetRequest;
use serde_json::Value;

/// How long the PR homes (workspace -> roots -> PRs) are reused before the
/// tick rebuilds them; plan and index changes rebuild sooner.
const PR_HOMES_FRESH: Duration = Duration::from_secs(30);
/// PRs one poll reads at most; the rest are read by the next.
const PR_POLL_MAX_ITEMS: usize = 25;
/// A PR no workspace lists is forgotten this long after its last read.
const PR_FORGET_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);
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

/// The prompt that tells a coordinator its news: one line per item.
fn tell_prompt(lines: &[String]) -> String {
    lines.join("\n")
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
    Saved(Result<(), String>),
    PlanWritten(Box<MergePlanWrite>),
    /// Build only when this root has no plan, from its workspace watch.
    BuildPlan(u64, String),
    /// The user cleared a hold: root chat, PR.
    ClearHold(String, PrKey),
    Review(PrKey),
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
    /// Tell the coordinator: root chat, PR id, news key.
    NewsTell(String, String, String),
    ReadyListToggle,
    ReadyGo(PrHome),
    /// Fold or expand a root's merged and closed PRs: root chat.
    DoneToggle(String),
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

/// The fold line for done PRs: "\u{2713} 3 merged" or with "n closed".
fn done_summary(done: &[PrState]) -> String {
    let merged = done.iter().filter(|s| **s == PrState::Merged).count();
    let closed = done.iter().filter(|s| **s == PrState::Closed).count();
    match (merged, closed) {
        (m, 0) => format!("\u{2713} {m} merged"),
        (0, c) => format!("\u{2713} {c} closed"),
        (m, c) => format!("\u{2713} {m} merged \u{00B7} {c} closed"),
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
}

impl PrsState {
    /// Refuses even an empty existing plan, including one still being saved.
    fn prepare_backfill(&mut self, root: &str, keys: &[PrKey]) -> Result<MergePlan, String> {
        if self.plans.contains_key(root) || self.plan_writes.contains(root) {
            return Err(
                "a merge plan already exists or is being saved; backfill will not replace it"
                    .to_string(),
            );
        }
        let watched: Vec<_> = keys
            .iter()
            .map(|key| {
                self.watch
                    .prs
                    .get(&key.id())
                    .and_then(WatchedPr::brief)
                    .map(|brief| (key.clone(), brief))
                    .ok_or_else(|| {
                        format!(
                            "{} has no known PR state yet; wait for the PR watch and try again",
                            key.label()
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        let plan = merge_plan::backfill_plan(root, &watched, &chat_facts::now_stamp())?;
        self.plans.insert(root.to_string(), plan.clone());
        self.homes_built = None;
        Ok(plan)
    }

    /// An Approve / Merge run for `key` finished. Its PR is read at the
    /// next tick either way. The open confirmation for it closes on
    /// success and keeps the error on failure; returns whether it closed.
    pub fn action_done(&mut self, key: &PrKey, result: Result<String, String>) -> bool {
        self.due_now.insert(key.id());
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
    /// live tabs saw since.
    fn root_fact_prs(&self, root: &str) -> Vec<PrRef> {
        let mut prs: Vec<PrRef> = Vec::new();
        let mut push = |pr: &PrRef| {
            if !prs.iter().any(|known| known.key().id() == pr.key().id()) {
                prs.push(pr.clone());
            }
        };
        for entry in overview::chats_under(&self.chat_index, &[root.to_string()]) {
            for pr in entry.facts.iter().flat_map(|facts| facts.prs.iter()) {
                push(pr);
            }
        }
        for (chat, live) in &self.prs.live_prs {
            if chat == root || self.root_chat_of(chat) == root {
                for pr in live {
                    push(pr);
                }
            }
        }
        prs
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

    // -----------------------------------------------------------------------
    // The watch.

    /// From `Tick`: rebuild the homes when stale, then start one poll of
    /// the due PRs unless one runs.
    pub(super) fn poll_pr_watch(&mut self) -> Task<Event> {
        let mut tasks: Vec<Task<Event>> = Vec::new();
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
        for (key, interval) in wanted {
            if items.len() >= PR_POLL_MAX_ITEMS {
                break;
            }
            let id = key.id();
            let watched = self.prs.watch.prs.get(&id);
            if watched.is_some_and(WatchedPr::done) {
                continue;
            }
            let due = self.prs.due_now.contains(&id)
                || watched.is_none_or(|watched| watched.due(watched.interval_for(interval), now));
            if !due {
                continue;
            }
            items.push(pr_watch::PollItem {
                account: pr_watch::account_for(key.owner(), &self.github_accounts),
                snapshot: watched.map(|w| w.snapshot.clone()).unwrap_or_default(),
                etags: watched.map(|w| w.etags.clone()).unwrap_or_default(),
                key,
            });
        }
        if items.is_empty() {
            return Task::batch(tasks);
        }
        for item in &items {
            self.prs.due_now.remove(&item.key.id());
        }
        self.prs.in_flight = true;
        tasks.push(Task::perform(
            pr_watch::poll(pr_watch::PollRequest { items }),
            |outcome| Event::Pr(PrEvent::Polled(Box::new(outcome))),
        ));
        Task::batch(tasks)
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
            self.prs.due_now.insert(entry.key().id());
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
            PrEvent::BuildPlan(uid, root) => {
                // Recheck membership and existence at click time, not just render time.
                self.rebuild_pr_homes();
                let Some(roots) = self.home_roots(&PrHome::Open(uid)) else {
                    return Task::none();
                };
                if !roots.iter().any(|known| known.root == root) {
                    return Task::none();
                }
                let keys: Vec<_> = self
                    .keys_of_roots(roots)
                    .into_iter()
                    .map(|(_, key)| key)
                    .collect();
                match self.prs.prepare_backfill(&root, &keys) {
                    Ok(plan) => return self.write_merge_plan(plan, None, None, Vec::new()),
                    Err(error) => {
                        self.prs.plan_errors.insert(root, error);
                    }
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
                if self.prs.action_done(&key, result) {
                    // The overview row shows the new state once the
                    // `due_now` read lands.
                    return self.apply_webview_surfaces();
                }
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
            PrEvent::NewsTell(root, pr, news) => return self.tell_coordinator(&root, &pr, &news),
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
        let url = self
            .watched(key)
            .and_then(|watched| watched.snapshot.core.as_ref())
            .map(|core| core.url.clone())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| key.url());
        if let Err(error) = std::process::Command::new("open").arg(&url).spawn() {
            eprintln!("[pr-watch] cannot open {url} in the browser: {error}");
        }
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

    /// The chat tab hosting root chat `root`, or why there is none.
    fn coordinator_tab(&self, root: &str) -> Result<usize, &'static str> {
        let tab = self
            .workspaces
            .iter()
            .filter(|ws| matches!(ws.location, WorkspaceLocation::Local { .. }))
            .flat_map(|ws| ws.tabs.iter())
            .find(|tab| tab.chat_session_id.as_deref() == Some(root))
            .ok_or("open the coordinator's chat to tell it")?;
        if tab.agent_session().is_none() {
            return Err("the coordinator runs in a terminal tab");
        }
        Ok(tab.id)
    }

    /// "Tell coordinator": one short prompt naming the PR and the news,
    /// through the prompt-submit path of the coordinator's open chat tab.
    fn tell_coordinator(&mut self, root: &str, pr: &str, news: &str) -> Task<Event> {
        let tab_id = match self.coordinator_tab(root) {
            Ok(tab_id) => tab_id,
            Err(reason) => {
                eprintln!("[pr-watch] cannot tell coordinator {root}: {reason}");
                return Task::none();
            }
        };
        let Some(watched) = self.prs.watch.prs.get(pr) else {
            return Task::none();
        };
        let Some(item) = watched.news.iter().find(|item| item.key == news) else {
            return Task::none();
        };
        let prompt = tell_prompt(&[news_line(watched, item)]);
        if let Some(item) = self.news_item_mut(pr, news) {
            item.told = true;
        }
        eprintln!("[pr-watch] told coordinator {root}: {prompt}");
        let save = self.save_pr_watch();
        Task::batch([
            save,
            Task::done(Event::AgentSubmitPrompt(tab_id, prompt.into())),
        ])
    }

    /// Whether chat tab `tab_id` is mid-turn: the state the chat strip's
    /// running marker and held delegation sends go by.
    fn chat_turn_running(&self, tab_id: usize) -> bool {
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
    /// tick after its turn ends; a closed one keeps its news (the line says
    /// why) until it opens.
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
            let Ok(tab_id) = self.coordinator_tab(&root) else {
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

    /// One news item with Tell coordinator (or why not) and dismiss.
    fn view_pr_news_item<'a>(
        &'a self,
        root: &str,
        watched: &WatchedPr,
        item: &NewsItem,
        with_pr: bool,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font_small = self.ui_font_small();
        let label = if with_pr {
            format!("{} {}", watched.key.label(), item.text)
        } else {
            item.text.clone()
        };
        let age = overview::parse_stamp(&item.at)
            .map(overview::age_label)
            .unwrap_or_default();
        let mut line = Row::new()
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .push(
                text(format!("\u{25CF} {}", overview::shorten(&label, 70)))
                    .size(font_small - 1.0)
                    .color(news_color(theme, item.kind))
                    .width(Length::Fill),
            )
            .push(text(age).size(10).color(theme.text_muted()));
        let tell_label = if item.told {
            "told"
        } else {
            "Tell coordinator"
        };
        let mut tell = button(text(tell_label).size(10).color(theme.accent()))
            .style(self.ghost_button_style())
            .padding([2, 6]);
        let reason = match self.coordinator_tab(root) {
            Ok(_) if !item.told => {
                tell = tell.on_press(Event::Pr(PrEvent::NewsTell(
                    root.to_string(),
                    watched.key.id(),
                    item.key.clone(),
                )));
                None
            }
            Ok(_) => None,
            Err(reason) => Some(reason),
        };
        line = line.push(tell).push(
            button(text("\u{00D7}").size(10).color(theme.text_muted()))
                .style(self.ghost_button_style())
                .padding([2, 6])
                .on_press(Event::Pr(PrEvent::NewsDismiss(
                    watched.key.id(),
                    item.key.clone(),
                ))),
        );
        let mut item_col = Column::new().spacing(1).push(line);
        if let Some(reason) = reason {
            item_col = item_col.push(text(reason).size(10).color(theme.text_muted()));
        }
        item_col.into()
    }

    /// One PR row of the overview section.
    fn view_pr_row<'a>(
        &'a self,
        root: &str,
        key: &PrKey,
        row: Option<&OrderedRow>,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mono = iced::Font::with_name("Menlo");
        let font_small = self.ui_font_small();
        let watched = self.watched(key);
        let is_next = row.is_some_and(|row| row.status == RowStatus::Next);
        let draft = watched
            .and_then(WatchedPr::brief)
            .is_some_and(|brief| brief.state == merge_plan::PrState::Draft);
        let mut head = Row::new().spacing(6).align_y(iced::Alignment::Center);
        if let Some(row) = row.filter(|r| r.entry.unordered_reason.is_none()) {
            head = head.push(
                text(format!("{}", row.position))
                    .size(font_small)
                    .color(theme.text_muted())
                    .font(mono)
                    .width(Length::Fixed(16.0)),
            );
        }
        head = head.push(
            text(key.label())
                .size(font_small)
                .color(if is_next {
                    theme.accent()
                } else {
                    theme.text_primary()
                })
                .font(mono)
                .width(Length::Fill),
        );
        if let Some(row) = row {
            let (label, color) = match &row.status {
                RowStatus::Next => ("next to merge".to_string(), theme.accent()),
                RowStatus::Ready => ("ready".to_string(), theme.success()),
                RowStatus::Blocked(_) => ("blocked".to_string(), theme.warning()),
                RowStatus::Held(_) => ("held".to_string(), theme.warning()),
                RowStatus::Unordered(_) => ("unordered".to_string(), theme.warning()),
                other => (other.label().to_string(), theme.text_muted()),
            };
            if row.status != RowStatus::Draft {
                head = head.push(text(label).size(10).color(color).font(mono));
            }
        }
        // A draft can't be merged; say so where it can't be missed.
        if draft {
            let warning = theme.warning();
            head = head.push(
                container(text("draft").size(font_small).color(warning).font(mono))
                    .padding([1, 6])
                    .style(move |_| container::Style {
                        border: iced::Border {
                            color: warning,
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }
        let mut body = Column::new().spacing(2).push(head);
        if let Some(core) = watched.and_then(|w| w.snapshot.core.as_ref()) {
            if !core.title.is_empty() {
                body = body.push(
                    text(overview::shorten(&core.title, 70))
                        .size(font_small - 1.0)
                        .color(theme.text_secondary()),
                );
            }
        }
        body = body.push(self.pr_live_line(watched));
        if let Some(row) = row {
            match &row.status {
                RowStatus::Blocked(waits) => {
                    body = body.push(
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
                    body = body.push(
                        text(format!("unordered: {reason}"))
                            .size(10)
                            .color(theme.warning()),
                    );
                }
                RowStatus::Held(hold) => {
                    body = body.push(
                        row![
                            text(format!("hold: {hold}"))
                                .size(10)
                                .color(theme.warning())
                                .width(Length::Fill),
                            button(text("clear hold").size(10).color(theme.accent()))
                                .style(self.ghost_button_style())
                                .padding([2, 6])
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
                body = body.push(
                    text(overview::shorten(note, 120))
                        .size(10)
                        .color(theme.text_muted()),
                );
            }
        }
        if let Some(error) = watched.and_then(|w| w.error.as_deref()) {
            body = body.push(
                text(overview::shorten(&format!("not read: {error}"), 160))
                    .size(10)
                    .color(theme.danger()),
            );
        }
        let open = watched
            .and_then(WatchedPr::brief)
            .is_some_and(|brief| brief.state == merge_plan::PrState::Open);
        let action = |label: String, event: Option<Event>| {
            let mut b = button(text(label).size(10).color(theme.text_primary()))
                .style(self.ghost_button_style())
                .padding([2, 8]);
            if let Some(event) = event {
                b = b.on_press(event);
            }
            b
        };
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
        let mut actions = Row::new().spacing(4).push(action(
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
        actions = actions.push(action(
            gated("Merge"),
            (open && reason.is_none())
                .then(|| Event::Pr(PrEvent::ActionOpen(key.clone(), ActionKind::Merge))),
        ));
        if draft {
            actions = actions.push(
                button(text("Ready for review").size(10).color(theme.accent()))
                    .style(self.ghost_button_style())
                    .padding([2, 8])
                    .on_press(Event::Pr(PrEvent::ActionOpen(
                        key.clone(),
                        ActionKind::Ready,
                    ))),
            );
        }
        body = body.push(actions);
        if let Some(watched) = watched {
            for item in watched.open_news() {
                body = body.push(self.view_pr_news_item(root, watched, item, false));
            }
        }
        let accent = theme.accent();
        let surface = theme.surface0();
        container(body)
            .padding([5, 6])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: is_next.then(|| surface.into()),
                border: iced::Border {
                    color: if is_next {
                        accent
                    } else {
                        iced::Color::TRANSPARENT
                    },
                    width: if is_next { 1.0 } else { 0.0 },
                    radius: 6.0.into(),
                },
                ..Default::default()
            })
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
            rows: Vec<OrderedRow>,
            unplanned: Vec<PrKey>,
            done: Vec<(PrKey, PrState)>,
            done_rows: Vec<OrderedRow>,
            /// The plan lists PRs, finished ones included.
            planned: bool,
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
                let planned = plan.is_some();
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
                    rows,
                    unplanned,
                    done,
                    done_rows,
                    planned,
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
        let mut section = Column::new().spacing(4).push(
            text(format!("PRS \u{00B7} MERGE ORDER \u{00B7} {total}"))
                .size(10)
                .color(theme.text_muted())
                .font(mono),
        );
        let several = groups.len() > 1;
        for group in groups {
            let Group {
                root,
                rows,
                unplanned,
                done,
                done_rows,
                planned,
            } = group;
            if several {
                section = section.push(
                    text(overview::shorten(&root.title, 50))
                        .size(10)
                        .color(theme.text_secondary()),
                );
            }
            if !planned {
                section = section.push(
                    row![
                        text("no merge plan yet").size(10).color(theme.text_muted()),
                        button(text("Build plan from open PRs").size(10))
                            .style(self.ghost_button_style())
                            .padding([2, 6])
                            .on_press(Event::Pr(PrEvent::BuildPlan(uid, root.root.clone()))),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                );
            } else if self
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
                section = section.push(self.view_pr_row(&root.root, &row.entry.key(), Some(row)));
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
                section = section.push(self.view_pr_row(&root.root, key, None));
            }
            if !done.is_empty() {
                let states: Vec<PrState> = done.iter().map(|(_, state)| *state).collect();
                section = section.push(
                    button(
                        text(done_summary(&states))
                            .size(10)
                            .color(theme.text_muted())
                            .font(mono),
                    )
                    .style(self.ghost_button_style())
                    .padding([2, 4])
                    .on_press(Event::Pr(PrEvent::DoneToggle(root.root.clone()))),
                );
                if self.prs.done_open.contains(&root.root) {
                    for row in &done_rows {
                        section =
                            section.push(self.view_pr_row(&root.root, &row.entry.key(), Some(row)));
                    }
                    for (key, _) in done
                        .iter()
                        .filter(|(key, _)| !done_rows.iter().any(|r| r.entry.key() == *key))
                    {
                        section = section.push(self.view_pr_row(&root.root, key, None));
                    }
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
            for (_, row) in &next {
                let key = row.entry.key();
                let watched = self.watched(&key);
                let title = watched
                    .and_then(|w| w.snapshot.core.as_ref())
                    .map(|core| overview::shorten(&core.title, 60))
                    .unwrap_or_default();
                group = group
                    .push(
                        text(format!("next: {} {title}", key.label()))
                            .size(font_small - 1.0)
                            .color(theme.accent()),
                    )
                    .push(self.pr_live_line(watched));
            }
            for (root, watched, item) in &news {
                group = group.push(self.view_pr_news_item(root, watched, item, true));
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
            .width(Length::Fixed(560.0))
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
        assert_eq!(done_summary(&[Merged, Merged]), "\u{2713} 2 merged");
        assert_eq!(
            done_summary(&[Merged, Closed]),
            "\u{2713} 1 merged \u{00B7} 1 closed"
        );
        assert_eq!(done_summary(&[Closed]), "\u{2713} 1 closed");
    }

    use super::*;

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
    fn backfill_only_uses_selected_workspace_watch_and_never_replaces_a_plan() {
        let mut prs = PrsState::default();
        let key = PrKey::new("acme/api", 12);
        let mut watched = WatchedPr::new(key.clone());
        watched.snapshot.core = Some(pr_watch::PullCore {
            state: Some(PrState::Open),
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            ..Default::default()
        });
        prs.watch.prs.insert(key.id(), watched);
        let other = PrKey::new("other/repo", 1);
        let mut unrelated = WatchedPr::new(other.clone());
        unrelated.snapshot.core = Some(pr_watch::PullCore {
            state: Some(PrState::Open),
            head_ref: "other".to_string(),
            base_ref: "main".to_string(),
            ..Default::default()
        });
        prs.watch.prs.insert(other.id(), unrelated);
        let backfilled = prs
            .prepare_backfill("root", std::slice::from_ref(&key))
            .unwrap();
        assert_eq!(backfilled.entries.len(), 1);
        assert_eq!(backfilled.entries[0].key(), key);
        assert!(prs
            .prepare_backfill("root", std::slice::from_ref(&other))
            .is_err());
        assert_eq!(prs.plans["root"], backfilled);
        let agent_plan = MergePlan {
            root_chat_id: "agent".to_string(),
            ..Default::default()
        };
        prs.plans.insert("agent".to_string(), agent_plan.clone());
        assert!(prs.prepare_backfill("agent", &[key]).is_err());
        assert_eq!(prs.plans["agent"], agent_plan);
        prs.plan_writes.insert("pending".to_string());
        assert!(prs.prepare_backfill("pending", &[]).is_err());
        assert!(!prs.plans.contains_key("pending"));
        let unread = PrKey::new("acme/api", 99);
        assert!(prs
            .prepare_backfill("unread", &[unread])
            .unwrap_err()
            .contains("no known PR state"));
        assert!(!prs.plans.contains_key("unread"));
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
        // One item: the line "Tell coordinator" has always sent.
        assert_eq!(
            tell_prompt(&[news_line(&watched, &failed)]),
            "GitTerm PR watch: acme/api#12 \"PRJ-101: add the export\": checks failed: test. \
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
                "GitTerm PR watch: acme/api#12 \"PRJ-101: add the export\": checks failed: test. \
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
