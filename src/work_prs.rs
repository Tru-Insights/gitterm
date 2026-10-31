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
//! again. News is notify only: nothing wakes a coordinator; "Tell
//! coordinator" sends one prompt when the user presses it.

use super::*;

use gitterm::merge_plan::{
    self, MergeMethod, MergePlan, OrderedRow, PrBrief, PrKey, PrRef, RowStatus,
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
/// Prefix of the prompt "Tell coordinator" sends.
const TELL_PREFIX: &str = "GitTerm PR watch";

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
    pub approve: bool,
    pub method: MergeMethod,
    /// The plan named the method.
    pub method_from_plan: bool,
    /// The mapped gh account; `None` for gh's active account.
    pub account: Option<String>,
    pub running: bool,
    pub result: Option<Result<String, String>>,
}

impl PrActionConfirm {
    fn action(&self) -> PrAction {
        if self.approve {
            PrAction::Approve
        } else {
            PrAction::Merge(self.method)
        }
    }
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
    /// The user cleared a hold: root chat, PR.
    ClearHold(String, PrKey),
    Review(PrKey),
    /// The checkout of the PR's head branch, if the overview knows one.
    ReviewResolved(PrKey, Option<PathBuf>),
    /// Open the confirmation: PR, approve (else merge).
    ActionOpen(PrKey, bool),
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
}

/// Everything the app keeps for PRs.
#[derive(Debug, Default)]
pub struct PrsState {
    /// Merge plans by root chat id.
    pub plans: HashMap<String, MergePlan>,
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
    /// The index was asked for once for the watch.
    pub index_requested: bool,
}

impl PrsState {
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
                || watched.is_none_or(|watched| watched.due(interval, now));
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
        &self,
        plan: MergePlan,
        previous: Option<MergePlan>,
        reply: Option<TaskControlReply>,
        kept_holds: Vec<PrKey>,
    ) -> Task<Event> {
        let dir = merge_plan::plans_dir(&config::global_config_dir());
        let root = plan.root_chat_id.clone();
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
                    "position": row.position,
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
            PrEvent::PlanWritten(write) => self.merge_plan_written(*write),
            PrEvent::ClearHold(root, key) => {
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
                    Some(checkout) => self.open_branch_diff(checkout, overview::DiffSource::VsBase),
                    None => {
                        self.open_pr_in_browser(&key);
                        Task::none()
                    }
                };
            }
            PrEvent::ActionOpen(key, approve) => {
                let entry = self
                    .prs
                    .plans
                    .values()
                    .find_map(|plan| plan.entry(&key).cloned());
                let method = entry.as_ref().and_then(|entry| entry.method);
                self.prs.action = Some(PrActionConfirm {
                    account: pr_watch::account_for(key.owner(), &self.github_accounts),
                    key,
                    approve,
                    method: method.unwrap_or_default(),
                    method_from_plan: method.is_some(),
                    running: false,
                    result: None,
                });
                self.prs.ready_open = false;
                return self.apply_webview_surfaces();
            }
            PrEvent::ActionMethod(method) => {
                if let Some(confirm) = self.prs.action.as_mut() {
                    if !confirm.running {
                        confirm.method = method;
                        confirm.result = None;
                    }
                }
            }
            PrEvent::ActionRun => {
                let Some(confirm) = self.prs.action.as_mut() else {
                    return Task::none();
                };
                if confirm.running {
                    return Task::none();
                }
                confirm.running = true;
                confirm.result = None;
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
                self.prs.due_now.insert(key.id());
                if let Some(confirm) = self.prs.action.as_mut().filter(|c| c.key == key) {
                    confirm.running = false;
                    confirm.result = Some(result);
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

    /// Review: the branch diff vs base when the overview owns a checkout of
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
        let prompt = format!(
            "{TELL_PREFIX}: {}{title}: {}. {url}",
            watched.key.label(),
            item.text
        );
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
        let mut head = Row::new().spacing(6).align_y(iced::Alignment::Center);
        if let Some(row) = row {
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
                other => (other.label().to_string(), theme.text_muted()),
            };
            head = head.push(text(label).size(10).color(color).font(mono));
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
        let action = |label: &'static str, event: Option<Event>| {
            let mut b = button(text(label).size(10).color(theme.text_primary()))
                .style(self.ghost_button_style())
                .padding([2, 8]);
            if let Some(event) = event {
                b = b.on_press(event);
            }
            b
        };
        body = body.push(
            row![
                action("Review", Some(Event::Pr(PrEvent::Review(key.clone())))),
                action(
                    "Approve",
                    open.then(|| Event::Pr(PrEvent::ActionOpen(key.clone(), true)))
                ),
                action(
                    "Merge",
                    open.then(|| Event::Pr(PrEvent::ActionOpen(key.clone(), false)))
                ),
            ]
            .spacing(4),
        );
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
        type Group<'g> = (&'g RootPrs, Vec<OrderedRow>, Vec<PrKey>);
        let groups: Vec<Group<'_>> = roots
            .iter()
            .filter_map(|root| {
                let plan = self.prs.plans.get(&root.root);
                let rows = plan
                    .map(|plan| merge_plan::order_plan(plan, &briefs))
                    .unwrap_or_default();
                let unplanned: Vec<PrKey> = root
                    .facts
                    .iter()
                    .map(PrRef::key)
                    .filter(|key| plan.is_none_or(|plan| plan.entry(key).is_none()))
                    .collect();
                (!rows.is_empty() || !unplanned.is_empty()).then_some((root, rows, unplanned))
            })
            .collect();
        if groups.is_empty() {
            return None;
        }
        let total: usize = groups
            .iter()
            .map(|(_, rows, more)| rows.len() + more.len())
            .sum();
        let mut section = Column::new().spacing(4).push(
            text(format!("PRS \u{00B7} MERGE ORDER \u{00B7} {total}"))
                .size(10)
                .color(theme.text_muted())
                .font(mono),
        );
        let several = groups.len() > 1;
        for (root, rows, unplanned) in groups {
            if several {
                section = section.push(
                    text(overview::shorten(&root.title, 50))
                        .size(10)
                        .color(theme.text_secondary()),
                );
            }
            if rows.is_empty() {
                section = section.push(
                    text("no merge plan yet; the coordinator sets one with merge_plan_set")
                        .size(10)
                        .color(theme.text_muted()),
                );
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
        }
        Some(section.into())
    }

    /// The bar's "⇡ n to merge" button for the Ready to merge list; hidden
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
        let mut label = format!("\u{21E1} {next} to merge");
        if news > 0 {
            label.push_str(&format!(" \u{00B7} {news} new"));
        }
        let color = if news > 0 {
            self.theme.accent()
        } else {
            self.theme.text_muted()
        };
        let hover = self.theme.text_secondary();
        Some(
            button(text(label).size(11).font(iced::Font::with_name("Menlo")))
                .style(move |_theme, status| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: if matches!(status, button::Status::Hovered) {
                        hover
                    } else {
                        color
                    },
                    ..Default::default()
                })
                .padding([6, 10])
                .on_press(Event::Pr(PrEvent::ReadyListToggle))
                .into(),
        )
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
                        .style(button::text)
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
                        .style(button::text)
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
                "github_accounts maps {owner} to {user:?}, which is not a GitHub login; fix config.json"
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
        if !confirm.approve {
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
        let done = matches!(confirm.result, Some(Ok(_)));
        match &confirm.result {
            Some(Ok(output)) => {
                card = card.push(
                    text(if output.is_empty() {
                        "Done.".to_string()
                    } else {
                        overview::shorten(output, 300)
                    })
                    .size(font_small - 1.0)
                    .color(theme.success()),
                );
            }
            Some(Err(error)) => {
                card = card.push(
                    text(overview::shorten(error, 400))
                        .size(font_small - 1.0)
                        .color(theme.danger()),
                );
            }
            None if confirm.running => {
                card = card.push(
                    text("Running\u{2026}")
                        .size(font_small - 1.0)
                        .color(theme.text_muted()),
                );
            }
            None => {}
        }
        let mut close = button(text(if done { "Close" } else { "Cancel" }).size(font_small))
            .style(button::text)
            .padding([6, 12]);
        if !confirm.running {
            close = close.on_press(Event::Pr(PrEvent::ActionClose));
        }
        let mut run = button(text(action.verb()).size(font_small))
            .style(button::primary)
            .padding([6, 12]);
        if !confirm.running && !done && account_ok {
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
