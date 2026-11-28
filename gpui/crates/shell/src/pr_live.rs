//! One Live PR watch shared by every workspace, with Iced's files and services.
use gitterm_core::{
    chat_facts,
    merge_plan::{self, PrRef},
    prs,
    work::pr_live::{Github, Guard},
};
use gpui::{App, Global};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub watch: prs::WatchFile,
    pub plans: HashMap<String, merge_plan::MergePlan>,
    pub accounts: BTreeMap<String, String>,
    pub logins: BTreeMap<Option<String>, String>,
    pub groups: HashMap<String, Vec<crate::work_sidebar::PullRequestGroup>>,
    pub found: HashMap<String, Vec<PrRef>>,
    pub index: Vec<gitterm_core::chats::ChatIndexEntry>,
    pub links: Vec<chat_facts::ChildLink>,
    pub error: Option<String>,
    // Iced's task-branch lookup state: scoped to this admitted root, forgotten
    // outside the 14-day attribution window; refreshed on completion or Refresh.
    branch_reads:
        HashMap<gitterm_core::work::pr_discovery::TaskBranch, (String, Vec<prs::BranchPr>)>,
}
#[derive(Clone)]
pub(crate) struct LivePrs {
    pub root: PathBuf,
    pub services: Arc<gitterm_core::preview::service::ServiceContext>,
    snapshot: Arc<Mutex<Snapshot>>,
    serial: Arc<Mutex<()>>,
    stopped: Arc<AtomicBool>,
    client: ClientFactory,
    wake: Arc<tokio::sync::Notify>,
    refresh_requested: Arc<AtomicBool>,
}
pub(crate) type ClientFactory =
    Arc<dyn Fn() -> Result<Box<dyn Github<Credential = String> + Send>, String> + Send + Sync>;
#[cfg(not(test))]
fn default_client() -> ClientFactory {
    Arc::new(|| Ok(Box::new(gitterm_core::work::pr_live::Gh::new()?)))
}
#[cfg(test)]
fn default_client() -> ClientFactory {
    Arc::new(|| Err("PR client not supplied by test".into()))
}
impl Global for LivePrs {}
impl LivePrs {
    pub fn install(services: Arc<gitterm_core::preview::service::ServiceContext>, cx: &mut App) {
        assert!(!services.is_preview());
        let this = Self {
            root: services.state_root().into(),
            services,
            snapshot: Default::default(),
            serial: Default::default(),
            stopped: Default::default(),
            client: default_client(),
            wake: Default::default(),
            refresh_requested: Default::default(),
        };
        let drain = this.clone();
        if let Some(barrier) = cx.try_global::<crate::lifetime::Barrier>() {
            barrier.register("pr-watch", move || {
                drain.stopped.store(true, Ordering::SeqCst);
                let _serial = drain
                    .serial
                    .lock()
                    .map_err(|_| anyhow::anyhow!("PR watch lock poisoned"))?;
                Ok(())
            });
        }
        cx.set_global(this.clone());
        cx.spawn(async move |cx| {
            loop {
                if this.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let worker = this.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { worker.refresh() })
                    .await;
                if let Err(error) = result {
                    this.snapshot.lock().unwrap().error = Some(error);
                }
                cx.update(|cx| cx.refresh_windows());
                tokio::select! {
                    _ = cx.background_executor().timer(Duration::from_secs(30)) => {},
                    _ = this.wake.notified() => {},
                }
            }
        })
        .detach();
    }
    pub fn read<R>(&self, read: impl FnOnce(&Snapshot) -> R) -> R {
        read(&self.snapshot.lock().unwrap())
    }
    pub fn snapshot(&self) -> Snapshot {
        self.read(Clone::clone)
    }
    fn refresh(&self) -> Result<(), String> {
        let _serial = self.serial.lock().map_err(|_| "PR watch lock poisoned")?;
        if self.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let catalog = crate::workspace_config::IcedCatalog::read(&self.root)
            .map_err(|e| format!("PR workspaces: {e:#}"))?;
        let store = gitterm_core::tasks::TaskStore::load(self.root.join("tasks.json"))
            .map_err(|e| e.to_string())?;
        let workspaces: Vec<_> = catalog
            .workspaces
            .iter()
            .chain(&catalog.closed)
            .cloned()
            .collect();
        let links = crate::data::iced_chat_links(&workspaces, &store);
        let facts = chat_facts::facts_dir(&self.root);
        #[cfg(not(test))]
        let mut index = dirs::home_dir()
            .map(|home| crate::workspace_config::read_iced_chat_index(&self.root, &home))
            .unwrap_or_default();
        #[cfg(test)]
        let mut index = Vec::new();
        for entry in chat_facts::saved_facts_index(&facts, &links, |message| eprintln!("{message}"))
        {
            if !index
                .iter()
                .any(|e: &gitterm_core::chats::ChatIndexEntry| e.id == entry.id)
            {
                index.push(entry);
            }
        }
        chat_facts::resolve_tree(&mut index, &links);
        let mut snapshot = self.snapshot();
        snapshot.watch = prs::WatchFile::load(&prs::WatchFile::path_in(&self.root));
        snapshot.plans = merge_plan::load_plans(&merge_plan::plans_dir(&self.root));
        snapshot.accounts = read_accounts(&self.root)?;
        if self.refresh_requested.swap(false, Ordering::SeqCst) {
            for pr in snapshot.watch.prs.values_mut() {
                pr.checked_at = None;
            }
            for (stamp, _) in snapshot.branch_reads.values_mut() {
                stamp.clear();
            }
        }
        snapshot.links = links;
        snapshot.index = index;
        snapshot.groups.clear();
        snapshot.found.clear();
        let uid_to_chat = workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .filter_map(|t| Some((t.session_uid.clone()?, t.chat_session_id.clone()?)))
            .collect();
        let branches = gitterm_core::work::pr_discovery::task_branches(
            &store,
            chrono::Utc::now(),
            &uid_to_chat,
        );
        snapshot
            .branch_reads
            .retain(|branch, _| branches.iter().any(|(_, wanted)| branch == wanted));
        let mut client = (self.client)()?;
        // Iced publishes one branch lookup per batch; a slow backlog cannot
        // hold the completed workers behind all remaining reads.
        for (_, branch) in &branches {
            let signature = store
                .delegations()
                .iter()
                .filter(|d| {
                    gitterm_core::work::pr_discovery::task_branch(
                        &store,
                        d,
                        chrono::Utc::now(),
                        &uid_to_chat,
                    )
                    .is_some_and(|(_, b)| b == *branch)
                })
                .map(|d| format!("{}:{}", d.created_at, d.status.is_sendable()))
                .collect::<Vec<_>>()
                .join(";");
            if snapshot
                .branch_reads
                .get(branch)
                .is_some_and(|(read, _)| *read == signature)
            {
                continue;
            }
            let previous = snapshot
                .branch_reads
                .remove(branch)
                .map(|(_, prs)| prs)
                .unwrap_or_default();
            let found = client.branch_prs(
                &branch.repo,
                &branch.branch,
                prs::account_for(
                    merge_plan::PrKey::new(&branch.repo, 1).owner(),
                    &snapshot.accounts,
                ),
            );
            let found = match found {
                Ok(found) => found,
                Err(error) => {
                    eprintln!("[pr-watch] task PRs: {error}");
                    previous
                }
            };
            snapshot
                .branch_reads
                .insert(branch.clone(), (signature, found));
            break;
        }
        let mut wanted = Vec::new();
        for workspace in &workspaces {
            if workspace.is_legacy_remote()
                || matches!(
                    workspace.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
                )
                || (!workspace.active && !workspace.is_parked())
            {
                continue;
            }
            let mut roots: Vec<_> = if workspace.chats.is_empty() {
                let chats = gitterm_core::work::summary::work_chats(
                    &snapshot.index,
                    std::path::Path::new(&workspace.dir),
                );
                gitterm_core::work::summary::coordinators(
                    &chats,
                    &snapshot.index,
                    std::time::SystemTime::now(),
                )
                .into_iter()
                .filter(|c| c.recent)
                .map(|c| (c.id, c.title))
                .collect()
            } else {
                workspace
                    .chats
                    .iter()
                    .map(|id| {
                        (
                            id.clone(),
                            snapshot
                                .index
                                .iter()
                                .find(|e| e.id == *id)
                                .map(|e| e.title.clone())
                                .unwrap_or_else(|| id.chars().take(8).collect()),
                        )
                    })
                    .collect()
            };
            for chat in workspace
                .tabs
                .iter()
                .filter_map(|t| t.chat_session_id.as_deref())
            {
                let root =
                    chat_facts::root_chat_of(&snapshot.index, chat, || snapshot.links.clone());
                if !roots.iter().any(|(id, _)| *id == root) {
                    roots.push((root.clone(), root.chars().take(8).collect()));
                }
            }
            let interval = if workspace.active {
                prs::OPEN_INTERVAL
            } else {
                prs::PARKED_INTERVAL
            };
            let mut groups = Vec::new();
            for (root, title) in roots {
                let saved = chat_facts::load_root_prs(
                    &facts,
                    &snapshot.index,
                    &snapshot.links,
                    &root,
                    |m| eprintln!("{m}"),
                );
                let branch_prs = branches
                    .iter()
                    .filter(|(origin, _)| {
                        chat_facts::root_chat_of(&snapshot.index, &origin.chat, || {
                            snapshot.links.clone()
                        }) == root
                    })
                    .flat_map(|(origin, branch)| {
                        snapshot
                            .branch_reads
                            .get(branch)
                            .into_iter()
                            .flat_map(|(_, prs)| prs)
                            .filter(move |found| {
                                found.created_at >= origin.started_at
                                    && origin.before.is_none_or(|before| found.created_at < before)
                            })
                            .map(|found| &found.pr)
                    });
                let found =
                    chat_facts::root_prs(&snapshot.index, &root, saved.iter().chain(branch_prs));
                let mut keys: Vec<_> = snapshot
                    .plans
                    .get(&root)
                    .into_iter()
                    .flat_map(|p| &p.entries)
                    .map(|e| e.key())
                    .collect();
                for pr in &found {
                    if !keys.iter().any(|k| k.id() == pr.key().id()) {
                        keys.push(pr.key());
                    }
                }
                snapshot.found.insert(root.clone(), found);
                wanted.extend(keys.iter().cloned().map(|key| (key, interval)));
                let prs = keys
                    .into_iter()
                    .map(|key| {
                        snapshot
                            .watch
                            .prs
                            .get(&key.id())
                            .cloned()
                            .unwrap_or_else(|| prs::WatchedPr::new(key))
                    })
                    .collect();
                groups.push(crate::work_sidebar::PullRequestGroup { root, title, prs });
            }
            snapshot
                .groups
                .insert(crate::workspace_config::iced_layout_key(workspace), groups);
        }
        let logins = gitterm_core::work::pr_live::poll_and_save(
            &mut snapshot.watch,
            wanted,
            &snapshot.accounts,
            &prs::WatchFile::path_in(&self.root),
            &mut *client,
        )?;
        snapshot.logins.extend(logins);
        for groups in snapshot.groups.values_mut() {
            for group in groups {
                for pr in &mut group.prs {
                    if let Some(watched) = snapshot.watch.prs.get(&pr.key.id()) {
                        *pr = watched.clone();
                    }
                }
            }
        }
        snapshot.error = None;
        if branches
            .iter()
            .any(|(_, branch)| !snapshot.branch_reads.contains_key(branch))
            || snapshot
                .branch_reads
                .values()
                .any(|(stamp, _)| stamp.is_empty())
        {
            self.wake.notify_one();
        }
        *self.snapshot.lock().unwrap() = snapshot;
        Ok(())
    }
    pub fn confirm(
        &self,
        guard: &Guard,
        confirm: &mut gitterm_core::work::pr_actions::PrActionConfirm,
    ) -> Result<String, String> {
        self.execute(guard, confirm, &mut *(self.client)()?)
    }
    pub fn execute(
        &self,
        guard: &Guard,
        confirm: &mut gitterm_core::work::pr_actions::PrActionConfirm,
        github: &mut (impl Github + ?Sized),
    ) -> Result<String, String> {
        let _serial = self.serial.lock().map_err(|_| "PR watch lock poisoned")?;
        if self.stopped.load(Ordering::SeqCst) {
            return Err("PR watch is shutting down".into());
        }
        let run = gitterm_core::work::pr_actions::RunPrAction {
            action: confirm.action(),
            key: confirm.key.clone(),
            account: confirm.account.clone(),
        };
        self.services
            .pr_action(&run)
            .map_err(|e| e.notice().to_string())?;
        if prs::account_for(confirm.key.owner(), &read_accounts(&self.root)?) != confirm.account {
            return Err("Account mapping changed; refresh and reopen".into());
        }
        let result = guard.execute(confirm, github);
        // An action success or failure requests the next read, like Iced.
        // Read the whole saved document even if startup's first poll failed.
        let mut snapshot = self.snapshot.lock().unwrap();
        snapshot.watch = prs::WatchFile::load(&prs::WatchFile::path_in(&self.root));
        if let Some(pr) = snapshot.watch.prs.get_mut(&confirm.key.id()) {
            pr.checked_at = None;
        }
        snapshot.watch.save(&prs::WatchFile::path_in(&self.root))?;
        self.wake.notify_one();
        result
    }
    pub fn request_refresh(&self) {
        self.refresh_requested.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
    pub fn note_live_prs(&self, chat: &str, prs: &[PrRef]) -> Result<(), String> {
        let _serial = self.serial.lock().map_err(|_| "PR watch lock poisoned")?;
        if self.stopped.load(Ordering::SeqCst) {
            return Err("PR watch is shutting down".into());
        }
        let dir = chat_facts::facts_dir(&self.root);
        let path = chat_facts::facts_path(&dir, gitterm_core::chats::ChatBackend::Claude, chat)?;
        let mut facts = chat_facts::load_facts(&path)?.unwrap_or_default();
        for pr in prs {
            facts.add_pr(pr.clone());
            if !facts.live_created_prs.contains(&pr.key().id()) {
                facts.live_created_prs.push(pr.key().id());
            }
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Save PR facts {}: {e}", dir.display()))?;
        let bytes =
            serde_json::to_vec_pretty(&facts).map_err(|e| format!("Encode PR facts: {e}"))?;
        merge_plan::write_atomically(&path, &bytes)?;
        let mut snapshot = self.snapshot.lock().unwrap();
        let root = chat_facts::root_chat_of(&snapshot.index, chat, || snapshot.links.clone());
        let found = snapshot.found.entry(root).or_default();
        for pr in prs {
            if !found.iter().any(|p| p.key().id() == pr.key().id()) {
                found.push(pr.clone());
            }
        }
        self.wake.notify_one();
        Ok(())
    }
    pub fn review(&self, key: &merge_plan::PrKey) -> Result<(), String> {
        let _serial = self.serial.lock().map_err(|_| "PR watch lock poisoned")?;
        if self.stopped.load(Ordering::SeqCst) {
            return Err("PR watch is shutting down".into());
        }
        let mut snapshot = self.snapshot.lock().unwrap();
        snapshot.watch = prs::WatchFile::load(&prs::WatchFile::path_in(&self.root));
        if gitterm_core::work::pr_actions::take_unseen_comments(&mut snapshot.watch, key) {
            snapshot.watch.save(&prs::WatchFile::path_in(&self.root))?;
        }
        Ok(())
    }
    pub fn root_chat(&self, conversation: &str, links: &[chat_facts::ChildLink]) -> String {
        self.read(|snapshot| {
            chat_facts::root_chat_of(&snapshot.index, conversation, || links.to_vec())
        })
    }
    #[cfg(test)]
    pub(crate) fn with_client(
        root: &std::path::Path,
        snapshot: Snapshot,
        client: ClientFactory,
    ) -> Self {
        Self {
            root: root.into(),
            services: Arc::new(
                gitterm_core::preview::service::ServiceContext::production(root, "example-pr-test")
                    .unwrap()
                    .for_gpui_live(),
            ),
            snapshot: Arc::new(Mutex::new(snapshot)),
            serial: Default::default(),
            stopped: Default::default(),
            client,
            wake: Default::default(),
            refresh_requested: Default::default(),
        }
    }
    pub fn merge_plan(
        &self,
        conversation: &str,
        links: &[chat_facts::ChildLink],
    ) -> serde_json::Value {
        let mut snapshot = self.snapshot();
        snapshot.links = links.to_vec();
        for entry in chat_facts::saved_facts_index(&chat_facts::facts_dir(&self.root), links, |m| {
            eprintln!("{m}")
        }) {
            if let Some(old) = snapshot.index.iter_mut().find(|e| e.id == entry.id) {
                old.facts = entry.facts;
            } else {
                snapshot.index.push(entry);
            }
        }
        chat_facts::resolve_tree(&mut snapshot.index, links);
        let root =
            chat_facts::root_chat_of(&snapshot.index, conversation, || snapshot.links.clone());
        let saved = chat_facts::load_root_prs(
            &chat_facts::facts_dir(&self.root),
            &snapshot.index,
            &snapshot.links,
            &root,
            |m| eprintln!("{m}"),
        );
        let found = chat_facts::root_prs(
            &snapshot.index,
            &root,
            saved
                .iter()
                .chain(snapshot.found.get(&root).into_iter().flatten()),
        );
        let plans = merge_plan::load_plans(&merge_plan::plans_dir(&self.root));
        let watch = prs::WatchFile::load(&prs::WatchFile::path_in(&self.root));
        prs::merge_plan_value(&root, plans.get(&root), &watch, found)
    }
}
fn read_accounts(root: &std::path::Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join("github-accounts.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("Read {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(format!("Read {}: {e}", path.display())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gitterm_core::{
        merge_plan::{PrKey, PrState},
        prs::{Fetched, PrSnapshot, PullCore, WatchedPr},
        work::pr_actions::RunPrAction,
    };
    #[derive(Clone)]
    pub(crate) struct Fake {
        pub watched: WatchedPr,
        pub writes: Arc<Mutex<Vec<RunPrAction>>>,
    }
    impl Github for Fake {
        type Credential = String;
        fn identity(&mut self, _: Option<&str>) -> Result<(String, String), String> {
            Ok(("fixture".into(), "reviewer".into()))
        }
        fn fetch(
            &mut self,
            _: &PrKey,
            _: &PrSnapshot,
            _: &BTreeMap<String, String>,
            _: &String,
        ) -> Result<Fetched, String> {
            Ok(Fetched {
                snapshot: self.watched.snapshot.clone(),
                etags: Default::default(),
            })
        }
        fn run(&mut self, run: &RunPrAction, _: &str, _: &String) -> Result<String, String> {
            self.writes.lock().unwrap().push(run.clone());
            Ok("done".into())
        }
    }
    pub(crate) fn fixture() -> (tempfile::TempDir, LivePrs, Fake) {
        let dir = tempfile::tempdir().unwrap();
        let mut watched = WatchedPr::new(PrKey::new("owner/repo", 53));
        watched.snapshot.core = Some(PullCore {
            title: "Example".into(),
            head_sha: "head-one".into(),
            state: Some(PrState::Open),
            ..Default::default()
        });
        watched.unseen_comments = 2;
        let fake = Fake {
            watched: watched.clone(),
            writes: Default::default(),
        };
        let mut snapshot = Snapshot::default();
        snapshot.watch.prs.insert(watched.key.id(), watched);
        snapshot
            .watch
            .save(&prs::WatchFile::path_in(dir.path()))
            .unwrap();
        let clone = fake.clone();
        let live = LivePrs::with_client(
            dir.path(),
            snapshot,
            Arc::new(move || Ok(Box::new(clone.clone()))),
        );
        (dir, live, fake)
    }
    #[test]
    fn live_pr_mcp_uses_full_watch_and_unplanned_live_sources() {
        let (dir, live, fake) = fixture();
        let plan = serde_json::from_value(serde_json::json!({"root_chat_id":"root", "entries":[{"repo":"owner/repo", "number":53,"step":1,"method":"squash"}]})).unwrap();
        merge_plan::write_plan(&merge_plan::plans_dir(dir.path()), &plan).unwrap();
        let found = PrRef {
            repo: "owner/repo".into(),
            number: 54,
            url: "https://github.com/owner/repo/pull/54".into(),
        };
        live.snapshot
            .lock()
            .unwrap()
            .found
            .insert("root".into(), vec![found.clone()]);
        let value = live.merge_plan("root", &[]);
        assert_eq!(
            value,
            prs::merge_plan_value("root", Some(&plan), &live.snapshot().watch, vec![found])
        );
        assert_eq!(value["entries"][0]["live"]["title"], "Example");
        assert_eq!(value["unplanned_prs"][0]["number"], 54);
        assert!(fake.writes.lock().unwrap().is_empty());
    }
    #[test]
    fn live_pr_action_preserves_watch_before_first_poll() {
        let (dir, _live, fake) = fixture();
        let clone = fake.clone();
        let live = LivePrs::with_client(
            dir.path(),
            Snapshot::default(),
            Arc::new(move || Ok(Box::new(clone.clone()))),
        );
        let guard = Guard::capture(Some(&fake.watched), Some("reviewer")).unwrap();
        let mut confirm = gitterm_core::work::pr_actions::PrActionConfirm::open(
            fake.watched.key.clone(),
            gitterm_core::work::pr_actions::ActionKind::Approve,
            [],
            &BTreeMap::new(),
        );
        live.confirm(&guard, &mut confirm).unwrap();
        let reloaded = prs::WatchFile::load(&prs::WatchFile::path_in(dir.path()));
        assert_eq!(
            reloaded.prs[&fake.watched.key.id()].snapshot,
            fake.watched.snapshot
        );
        assert_eq!(reloaded.prs[&fake.watched.key.id()].unseen_comments, 2);
    }
    #[test]
    fn live_pr_review_reset_is_saved_for_iced() {
        let (dir, live, fake) = fixture();
        live.review(&fake.watched.key).unwrap();
        let reloaded = prs::WatchFile::load(&prs::WatchFile::path_in(dir.path()));
        assert_eq!(reloaded.prs[&fake.watched.key.id()].unseen_comments, 0);
        assert_eq!(
            reloaded.prs[&fake.watched.key.id()].snapshot,
            fake.watched.snapshot
        );
        assert!(fake.writes.lock().unwrap().is_empty());
    }
    #[test]
    fn live_pr_workspace_watch_reloads_in_iced_format() {
        let (dir, live, fake) = fixture();
        let file = serde_json::json!({"active_workspace":0,"workspaces":[
            {"name":"Example one","abbrev":"ONE","color":"blue","dir":dir.path(),"tabs":[],"chats":["root-one"]},
            {"name":"Example two","abbrev":"TWO","color":"blue","dir":dir.path(),"tabs":[],"chats":["root-two"],"active":false}
        ]});
        std::fs::write(
            dir.path().join("workspaces.json"),
            serde_json::to_vec(&file).unwrap(),
        )
        .unwrap();
        let plan = serde_json::from_value(serde_json::json!({"root_chat_id":"root-one", "entries":[{"repo":"owner/repo", "number":53,"step":1}]})).unwrap();
        merge_plan::write_plan(&merge_plan::plans_dir(dir.path()), &plan).unwrap();
        live.note_live_prs(
            "root-two",
            &[PrRef {
                repo: "owner/repo".into(),
                number: 54,
                url: "https://github.com/owner/repo/pull/54".into(),
            }],
        )
        .unwrap();
        live.refresh().unwrap();
        let first = live.snapshot();
        assert_eq!(
            first
                .groups
                .values()
                .map(|groups| groups.iter().map(|g| g.prs.len()).sum::<usize>())
                .sum::<usize>(),
            2
        );
        assert!(first.watch.prs["owner/repo#53"].checked_at.is_some());
        assert!(first.watch.prs["owner/repo#54"].checked_at.is_some());
        let path = prs::WatchFile::path_in(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        let clone = fake.clone();
        let reloaded = LivePrs::with_client(
            dir.path(),
            Snapshot::default(),
            Arc::new(move || Ok(Box::new(clone.clone()))),
        );
        reloaded.refresh().unwrap();
        assert_eq!(reloaded.snapshot().watch, prs::WatchFile::load(&path));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(fake.writes.lock().unwrap().is_empty());
    }
    #[test]
    fn live_pr_created_facts_join_the_merge_plan_before_polling() {
        let (dir, live, _) = fixture();
        let pr = PrRef {
            repo: "owner/repo".into(),
            number: 54,
            url: "https://github.com/owner/repo/pull/54".into(),
        };
        live.note_live_prs("root", &[pr.clone()]).unwrap();
        assert_eq!(
            live.merge_plan("root", &[])["unplanned_prs"][0]["number"],
            54
        );
        let path = chat_facts::facts_path(
            &chat_facts::facts_dir(dir.path()),
            gitterm_core::chats::ChatBackend::Claude,
            "root",
        )
        .unwrap();
        let facts = chat_facts::load_facts(&path).unwrap().unwrap();
        assert_eq!(facts.prs, vec![pr]);
        assert_eq!(facts.live_created_prs, vec!["owner/repo#54"]);
    }
}
