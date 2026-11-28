use crate::{
    workspace_config::WorkspaceStore,
    workspace_status::{Command, Intent, WorkspaceStatus},
};
use gitterm_core::runtime::{StatusOutcome, StatusRuntime};
use gpui::*;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

/// Process/window owner. Dock panels hold read handles only. The executor and
/// outcome bridge survive every dock close, move, hide and reconstruction.
pub struct WorkspaceOwner {
    pub(crate) worktree_operations: crate::pane::git_view::WorktreeOperations,
    /// Startup park metadata blocks PTY construction until explicit resume.
    pub(crate) stopped_workspaces: std::collections::HashSet<crate::data::WorkspaceId>,
    pub(crate) notifications: Option<crate::notifications::Notifications>,
    pub model: WorkspaceStatus,
    pub error: Option<String>,
    pub(crate) workspace_save_pending: bool,
    pub(crate) chat_shutdown: Arc<crate::chat::RuntimeShutdown>,
    _chat_quit: Subscription,
    pub(crate) chats: std::collections::HashMap<String, Entity<crate::chat::ChatPane>>,
    pub files: std::collections::HashMap<crate::data::WorkspaceId, crate::files::FileTree>,
    file_jobs: crate::files::FileJobs,
    pub(crate) reviews: std::collections::HashMap<
        crate::data::WorkspaceId,
        Entity<gitterm_gpui_review::ReviewView>,
    >,
    file_sender: tokio::sync::mpsc::UnboundedSender<crate::files::Outcome>,
    git_visible: bool,
    review_loads:
        std::collections::HashMap<crate::data::WorkspaceId, crate::review_reload::ReviewReload>,
    review_workers: Arc<std::sync::Mutex<ReviewWorkers>>,
    store: Option<WorkspaceStore>,
    jobs: Arc<StatusRuntime>,
    status_contexts: std::collections::HashMap<crate::data::WorkspaceId, u64>,
    status_requests:
        std::collections::HashMap<gitterm_core::runtime::JobId, (crate::data::WorkspaceId, u64)>,
    executor: Option<Arc<tokio::runtime::Runtime>>,
    _tasks: Vec<Task<()>>,
    _quit: Option<Subscription>,
}
impl WorkspaceOwner {
    pub fn new(
        model: WorkspaceStatus,
        store: Option<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(catalog) = store.as_ref().and_then(|store| store.iced.as_ref()) {
            cx.set_global(crate::work_sidebar::IcedWorkSource {
                config: catalog.source.clone(),
                workspaces: catalog.workspaces.clone(),
                repos: catalog.repos.clone(),
                owner: cx.entity().downgrade(),
            });
        }
        let (jobs, mut receiver) = StatusRuntime::new();
        let jobs = Arc::new(jobs);
        let executor = store.as_ref().map(|_| {
            Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .build()
                    .expect("create GPUI status executor"),
            )
        });
        let (file_sender, mut file_receiver) =
            tokio::sync::mpsc::unbounded_channel::<crate::files::Outcome>();
        let mut tasks = vec![cx.spawn(async move |owner, cx| {
            while let Some(outcome) = file_receiver.recv().await {
                if owner
                    .update(cx, |owner, cx| {
                        if let Some(tree) = owner.files.get_mut(&outcome.request.workspace) {
                            tree.complete(outcome);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })];
        let review_workers = Arc::new(std::sync::Mutex::new(ReviewWorkers::default()));
        let mut quit = None;
        let managed_shutdown = cx.try_global::<crate::lifetime::Barrier>().is_some();
        if executor.is_some() {
            tasks.push(cx.spawn(async move |owner, cx| {
                while let Some(outcome) = receiver.recv().await {
                    if owner
                        .update(cx, |owner, cx| match outcome {
                            StatusOutcome::Completed { job, snapshot } => {
                                if let Some((workspace, context)) =
                                    owner.status_requests.remove(&job)
                                {
                                    owner.complete_context_status(workspace, context, snapshot, cx);
                                }
                            }
                            StatusOutcome::Cancelled { job, .. } => {
                                owner.status_requests.remove(&job);
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
            // This is the only maintenance clock. SchedulePoll commands are left
            // to the next tick, preserving core backoff and the hidden Git floor.
            tasks.push(cx.spawn(async move |owner, cx| {
                let mut last = Instant::now();
                loop {
                    cx.background_executor().timer(Duration::from_secs(5)).await;
                    let now = Instant::now();
                    if owner
                        .update(cx, |owner, cx| {
                            if now.saturating_duration_since(last) > Duration::from_secs(300) {
                                owner.dispatch(Intent::Wake, now, cx);
                            }
                            owner.dispatch(
                                Intent::Maintenance {
                                    focused: owner.git_visible,
                                },
                                now,
                                cx,
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                    last = now;
                }
            }));
            let shutdown = jobs.clone();
            let workers = review_workers.clone();
            quit = Some(cx.on_app_quit(move |_, _| {
                let shutdown = shutdown.clone();
                let workers = {
                    let mut workers = workers.lock().unwrap();
                    workers.closed = true;
                    if managed_shutdown {
                        Vec::new()
                    } else {
                        std::mem::take(&mut workers.jobs)
                    }
                };
                shutdown.close();
                async move {
                    if !managed_shutdown {
                        shutdown.teardown().await;
                    }
                    for worker in workers {
                        let _ = worker.await;
                    }
                }
            }));
        }
        let chat_shutdown = crate::chat::RuntimeShutdown::new();
        if let Some(barrier) = cx.try_global::<crate::lifetime::Barrier>() {
            let chats = chat_shutdown.clone();
            barrier.register("chat-runtimes", move || chats.drain());
            let jobs = jobs.clone();
            let workers = review_workers.clone();
            let executor = executor.clone();
            barrier.register("status-review-jobs", move || {
                let workers = {
                    let mut workers = workers.lock().unwrap();
                    workers.closed = true;
                    std::mem::take(&mut workers.jobs)
                };
                if let Some(executor) = executor {
                    executor.block_on(async {
                        jobs.teardown().await;
                        for worker in workers {
                            worker.await.map_err(anyhow::Error::from)?;
                        }
                        Ok::<_, anyhow::Error>(())
                    })?;
                }
                Ok(())
            });
        }
        let chat_quit = cx.on_app_quit(|owner: &mut Self, cx| {
            for chat in owner.chats.values() {
                chat.update(cx, |chat, cx| chat.quit_runtime(cx));
            }
            if cx.try_global::<crate::lifetime::Barrier>().is_none()
                && let Err(error) = owner.chat_shutdown.drain()
            {
                eprintln!("Drain preview Claude runtimes on quit: {error:#}");
            }
            std::future::ready(())
        });
        let notifications = crate::chat::policy::is_live(cx).then(|| {
            tasks.push(crate::notifications::start(cx));
            crate::notifications::Notifications::new(Instant::now())
        });
        Self {
            worktree_operations: Default::default(),
            notifications,
            stopped_workspaces: Default::default(),
            model,
            files: Default::default(),
            reviews: Default::default(),
            file_jobs: Default::default(),
            file_sender,
            error: None,
            workspace_save_pending: false,
            chats: Default::default(),
            chat_shutdown,
            _chat_quit: chat_quit,
            store,
            jobs,
            status_contexts: Default::default(),
            status_requests: Default::default(),
            executor,
            _tasks: tasks,
            _quit: quit,
            git_visible: true,
            review_loads: Default::default(),
            review_workers,
        }
    }
    pub(crate) fn running_work_chats(&self, cx: &App) -> Vec<(PathBuf, Option<String>)> {
        self.chats
            .iter()
            .filter_map(|(uid, chat)| {
                let chat = chat.read(cx);
                if !chat.is_working() {
                    return None;
                }
                let target = chat.target();
                let saved_tab = self
                    .iced_catalog()
                    .into_iter()
                    .flat_map(|catalog| &catalog.workspaces)
                    .flat_map(|workspace| &workspace.tabs)
                    .find(|tab| tab.session_uid.as_deref() == Some(&target.session_uid));
                let dir = saved_tab
                    .map(|tab| PathBuf::from(tab.repo_dir.as_ref().unwrap_or(&tab.dir)))
                    .or_else(|| {
                        self.store
                            .as_ref()?
                            .chats
                            .iter()
                            .find(|chat| &chat.uid == uid)
                            .map(|chat| chat.cwd.clone())
                    })?;
                Some((dir, target.task_id))
            })
            .collect()
    }

    pub(crate) fn layout_key(
        &self,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
    ) -> String {
        self.store
            .as_ref()
            .map(|store| store.layout_key(saved))
            .unwrap_or_else(|| crate::workspace_config::iced_layout_key(saved))
    }
    pub(crate) fn iced_sessions(
        &self,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
    ) -> Vec<crate::data::Session> {
        crate::data::iced_sessions_with_key(saved, &self.layout_key(saved))
    }
    pub(crate) fn prepare_rename_alias(
        &self,
        target: &gitterm_core::workspace_config::WorkspaceTarget,
        draft: &str,
    ) -> anyhow::Result<
        Option<(
            crate::workspace_config::IdentityAliases,
            crate::preview::SaveResult,
        )>,
    > {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Workspace store is unavailable"))?;
        let catalog = store
            .iced
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Live catalog is unavailable"))?;
        let file = gitterm_core::workspace_config::WorkspacesFile {
            workspaces: catalog.workspaces.clone(),
            ..Default::default()
        };
        let index = target.index(&file).map_err(anyhow::Error::msg)?;
        store.prepare_rename_alias(&file.workspaces[index], draft)
    }
    pub(crate) fn install_aliases(&mut self, aliases: crate::workspace_config::IdentityAliases) {
        if let Some(store) = &mut self.store {
            store.identity_aliases = aliases;
        }
    }

    pub(crate) fn iced_workspace(
        &self,
        id: crate::data::WorkspaceId,
    ) -> Option<&gitterm_core::workspace_config::WorkspaceConfig> {
        self.store
            .as_ref()?
            .iced
            .as_ref()?
            .workspaces
            .get(id.0.checked_sub(1)? as usize)
    }
    /// Refresh the read catalog after the Live desktop owner acknowledges a save.
    pub(crate) fn apply_desktop_document(
        &mut self,
        root: &std::path::Path,
        file: gitterm_core::workspace_config::WorkspacesFile,
        cx: &mut Context<Self>,
    ) {
        let mut catalog = crate::workspace_config::IcedCatalog::from_document(root, file);
        if let Some(previous) = self.iced_catalog() {
            catalog.claims = previous.claims.clone();
            if previous.workspaces.len() == catalog.workspaces.len() {
                catalog.order = previous.ordered_indices();
            }
            for (old, next) in previous.workspaces.iter().zip(&catalog.workspaces) {
                if old.name != next.name && old.dir == next.dir {
                    for chat in self.chats.values() {
                        chat.update(cx, |chat, _| chat.rebind_live_workspace(old, next));
                    }
                }
            }
        }
        for (local, saved) in self.model.workspaces.iter_mut().zip(&catalog.workspaces) {
            local.status.repo_name = saved.name.clone();
        }
        cx.set_global(crate::work_sidebar::IcedWorkSource {
            config: root.to_path_buf(),
            workspaces: catalog.workspaces.clone(),
            repos: catalog.repos.clone(),
            owner: cx.entity().downgrade(),
        });
        if let Some(store) = self.store.as_mut() {
            store.iced = Some(catalog);
        }
        cx.notify();
    }

    pub(crate) fn iced_catalog(&self) -> Option<&crate::workspace_config::IcedCatalog> {
        self.store.as_ref()?.iced.as_ref()
    }
    /// Iced removes a closed tab from its saved workspace. Commit that
    /// removal before the shell discards its runtime pane.
    pub(crate) fn close_live_tab(
        &self,
        workspace: crate::data::WorkspaceId,
        id: crate::data::SessionId,
        target: &gitterm_core::session::Target,
        cx: &App,
    ) -> Result<
        Option<gitterm_core::desktop_store::Receipt<crate::workspace_config::LiveWorkspaceSaved>>,
        String,
    > {
        let Some(saved) = self.iced_workspace(workspace) else {
            return Ok(None);
        };
        let at = saved
            .tabs
            .iter()
            .position(|tab| tab.session_uid.as_deref() == Some(&target.session_uid))
            .or_else(|| {
                self.iced_sessions(saved)
                    .iter()
                    .position(|session| session.id == id)
                    .filter(|&at| at < saved.tabs.len())
            });
        let Some(at) = at else { return Ok(None) };
        let desktop = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .ok_or("Live workspace store is unavailable; terminal was kept open")?
            .0
            .clone();
        let key = gitterm_core::park::Key {
            name: saved.name.clone(),
            dir: saved.dir.clone(),
        };
        let expected = serde_json::to_value(&saved.tabs[at]).map_err(|e| e.to_string())?;
        let uid = saved.tabs[at].session_uid.clone();
        Ok(Some(desktop.transact_workspaces(move |file| {
            let mut matches = file.workspaces.iter_mut().filter(|w| key.matches(w));
            let workspace = matches.next().ok_or("Terminal workspace no longer exists")?;
            if matches.next().is_some() {
                return Err("Several saved workspaces have the same name and folder; terminal was kept open".into());
            }
            let index = if let Some(uid) = &uid {
                workspace.tabs.iter().position(|tab| tab.session_uid.as_ref() == Some(uid))
            } else {
                workspace.tabs.get(at).filter(|tab| serde_json::to_value(tab).ok().as_ref() == Some(&expected)).map(|_| at)
            }.ok_or("Saved terminal changed before close; terminal was kept open")?;
            workspace.tabs.remove(index);
            // Iced selects the right sibling, or the last tab when closing it.
            workspace.active_tab = workspace.active_tab.and_then(|active| {
                (!workspace.tabs.is_empty()).then(|| if active > index { active - 1 } else { active.min(workspace.tabs.len() - 1) })
            });
            Ok(crate::workspace_config::LiveWorkspaceSaved {
                workspace: workspace.clone(), repos: file.repos.clone(),
                closed: file.workspaces.iter().filter(|w| !w.active).cloned().collect(),
            })
        })))
    }
    pub(crate) fn chat_history(
        &self,
        cwd: Option<&std::path::Path>,
    ) -> Vec<crate::chat::SavedChat> {
        self.store
            .as_ref()
            .map(|store| {
                store
                    .chats
                    .iter()
                    .filter(|chat| {
                        Some(chat.cwd.as_path()) == cwd
                            && self
                                .preview()
                                .is_some_and(|p| p.provider() == chat.provider)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub(crate) fn confirm_live_tab(
        &mut self,
        name: &str,
        dir: &str,
        tab: gitterm_core::workspace_config::WorkspaceTabConfig,
        cx: &mut Context<Self>,
    ) {
        if let Some(desktop) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|s| s.0.clone())
        {
            let name = name.to_owned();
            let dir = dir.to_owned();
            let tab = tab.clone();
            let claim = self.iced_catalog().is_some_and(|c| {
                c.workspaces
                    .iter()
                    .position(|w| w.name == name && w.dir == dir)
                    .is_some_and(|i| c.claims.contains(&i))
            });
            let save = desktop.transact_workspaces(move |file| {
                let mut matches = file
                    .workspaces
                    .iter_mut()
                    .filter(|w| w.name == name && w.dir == dir);
                let workspace = matches.next().ok_or("Live chat workspace no longer exists")?;
                if matches.next().is_some() {
                    return Err("Several saved workspaces have the same name and folder; this workspace cannot be changed in Live.".into());
                }
                let uid = tab
                    .session_uid
                    .as_deref()
                    .ok_or("Live chat has no caller identity")?;
                let id = tab
                    .chat_session_id
                    .as_deref()
                    .ok_or("Live chat has no confirmed provider identity")?;
                if workspace.tabs.iter().any(|saved| {
                    saved.chat_session_id.as_deref() == Some(id)
                        && saved
                            .session_uid
                            .as_deref()
                            .is_some_and(|saved_uid| saved_uid != uid)
                }) {
                    return Err("Confirmed Claude session already belongs to another tab".into());
                }
                if (claim || !workspace.chats.is_empty())
                    && let Some(id) = &tab.chat_session_id
                    && !workspace.chats.contains(id)
                {
                    workspace.chats.push(id.clone());
                }
                if let Some(saved) = workspace.tabs.iter_mut().find(|t| {
                    t.session_uid == tab.session_uid
                        || (t.session_uid.is_none() && t.chat_session_id == tab.chat_session_id)
                }) {
                    *saved = tab;
                } else {
                    workspace.tabs.push(tab);
                }
                let workspace = workspace.clone();
                Ok(crate::workspace_config::LiveWorkspaceSaved {
                    workspace,
                    repos: file.repos.clone(),
                    closed: file
                        .workspaces
                        .iter()
                        .filter(|w| !w.active)
                        .cloned()
                        .collect(),
                })
            });
            let work = cx.background_executor().spawn(async move { save.wait() });
            cx.spawn(async move |owner, cx| {
                let result = work.await;
                let _ = owner.update(cx, |owner, cx| match result {
                    Ok(saved) => {
                        owner.publish_workspace(saved, false, cx);
                    }
                    Err(error) => {
                        owner.error = Some(format!("Save confirmed workspace chat: {error}"));
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }
        let claims = self.iced_catalog().is_some_and(|c| {
            c.workspaces
                .iter()
                .position(|w| w.name == name && w.dir == dir)
                .is_some_and(|i| c.claims.contains(&i))
        });
        if let Some(catalog) = self.store.as_mut().and_then(|store| store.iced.as_mut())
            && let Some(workspace) = catalog
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.name == name && workspace.dir == dir)
        {
            if (claims || !workspace.chats.is_empty())
                && let Some(id) = &tab.chat_session_id
                && !workspace.chats.contains(id)
            {
                workspace.chats.push(id.clone());
            }
            if let Some(saved) = workspace.tabs.iter_mut().find(|saved| {
                saved.session_uid == tab.session_uid
                    || (saved.session_uid.is_none() && saved.chat_session_id == tab.chat_session_id)
            }) {
                *saved = tab;
            } else {
                workspace.tabs.push(tab);
            }
            cx.notify();
        }
    }
    pub(crate) fn save_chat(&mut self, chat: crate::chat::SavedChat, cx: &mut Context<Self>) {
        let Some(preview) = self.preview() else {
            return;
        };
        if chat.provider != preview.provider() || !self.chats.contains_key(&chat.uid) {
            return;
        }
        let Some(store) = &mut self.store else { return };
        if let Some(saved) = store.chats.iter_mut().find(|saved| saved.uid == chat.uid) {
            *saved = chat;
        } else {
            store.chats.push(chat);
        }
        match store.save_async(&self.model.folders()) {
            Ok(save) => self.watch_save(save, cx),
            Err(error) => {
                self.error = Some(format!("Save preview chat history: {error:#}"));
                cx.notify();
            }
        }
    }
    pub(crate) fn writer(&self) -> Option<Arc<crate::preview::Writer>> {
        self.store.as_ref().and_then(|s| s.guard.clone())
    }
    pub(crate) fn saved_parked(&self) -> Vec<crate::park::SavedPark> {
        self.store
            .as_ref()
            .map(|store| store.parked.clone())
            .unwrap_or_default()
    }
    pub(crate) fn save_parked(
        &mut self,
        records: Vec<crate::park::SavedPark>,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = &mut self.store else { return };
        store.parked = records;
        match store.save_async(&self.model.folders()) {
            Ok(save) => self.watch_save(save, cx),
            Err(error) => {
                self.error = Some(format!("Save parked workspaces: {error:#}"));
                cx.notify();
            }
        }
    }
    /// The guarded preview writer, when this owner has one.
    pub(crate) fn preview_writer(&self) -> Option<Arc<crate::preview::Writer>> {
        self.store.as_ref()?.guard.clone()
    }
    pub(crate) fn preview(&self) -> Option<Arc<gitterm_core::preview::Preview>> {
        self.store
            .as_ref()
            .and_then(|s| s.guard.as_ref().map(|g| g.preview().clone()))
    }
    fn complete_context_status(
        &mut self,
        workspace: crate::data::WorkspaceId,
        context: u64,
        snapshot: gitterm_core::status::GitStatusSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self
            .status_contexts
            .get(&workspace)
            .copied()
            .unwrap_or_default()
            == context
        {
            self.dispatch(Intent::Completed(snapshot), Instant::now(), cx);
        }
    }

    pub fn dispatch(&mut self, intent: Intent, now: Instant, cx: &mut Context<Self>) {
        if let Intent::ContextChanged { workspace, .. } = &intent {
            *self.status_contexts.entry(*workspace).or_default() += 1;
        }
        let completed = match &intent {
            Intent::Completed(snapshot) => Some(crate::data::WorkspaceId(snapshot.tab_id as u64)),
            _ => None,
        };
        let refresh_files = matches!(intent, Intent::SelectIndex(_) | Intent::Activate(_));
        for command in self.model.dispatch(intent, now) {
            if let Command::Status { owner, command } = command
                && let Some(executor) = &self.executor
            {
                let target = self
                    .model
                    .workspaces
                    .iter()
                    .find(|w| w.id.0 as usize == owner)
                    .map(|w| w.folder.as_path())
                    .unwrap_or(std::path::Path::new("<status>"));
                if let Err(error) = crate::preview::executable(
                    self.preview().as_deref(),
                    gitterm_core::preview::Effect::Git,
                    target,
                ) {
                    self.error = Some(error.to_string());
                } else {
                    if let Some(job) = self.jobs.execute_with(
                        executor.handle(),
                        owner,
                        command,
                        gitterm_core::git_status::StatusReadOptions { fsmonitor: false },
                    ) {
                        let workspace = crate::data::WorkspaceId(owner as u64);
                        let context = *self.status_contexts.entry(workspace).or_default();
                        self.status_requests.insert(job, (workspace, context));
                    }
                }
            }
        }
        // shortcut: patches with unchanged paths/groups/status letters stay stale,
        // as in Iced; use content-based invalidation if live patch refresh is needed.
        if let Some(workspace) = completed {
            self.request_review(workspace, false, cx);
        }
        if refresh_files
            && self.executor.is_some()
            && let Some(workspace) = self
                .model
                .workspaces
                .iter()
                .find(|w| Some(w.id) == self.model.active() && !w.remote)
        {
            let request = self
                .files
                .entry(workspace.id)
                .or_default()
                .refresh(workspace.id, workspace.folder.clone());
            self.run_file_job(request);
        }
        cx.notify();
    }
    fn run_file_job(&mut self, request: crate::files::Request) {
        if let Some(executor) = &self.executor {
            self.file_jobs
                .execute(executor.handle(), request, self.file_sender.clone());
        }
    }
    pub fn toggle_directory(
        &mut self,
        workspace: crate::data::WorkspaceId,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = self
            .files
            .get_mut(&workspace)
            .and_then(|tree| tree.toggle(workspace, path))
        {
            self.run_file_job(request);
        }
        cx.notify();
    }
    pub fn more_files(
        &mut self,
        workspace: crate::data::WorkspaceId,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = self
            .files
            .get_mut(&workspace)
            .and_then(|tree| tree.more(workspace, path))
        {
            self.run_file_job(request);
        }
        cx.notify();
    }
    pub fn open_file(
        &mut self,
        workspace: crate::data::WorkspaceId,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(local) = self.model.workspaces.iter().find(|w| w.id == workspace) else {
            return;
        };
        let changed = self
            .model
            .status_view(workspace)
            .files
            .into_iter()
            .find(|file| local.root.join(&file.path) == path);
        let intent = FileOpen {
            workspace,
            path,
            changed,
            from_files: true,
        };
        eprintln!(
            "FileOpen workspace={} path={} changed={}",
            workspace.0,
            intent.path.display(),
            intent.changed.is_some()
        );
        cx.emit(intent);
    }
    pub fn open_changed_file(
        &mut self,
        workspace: crate::data::WorkspaceId,
        changed: crate::data::ChangedFile,
        cx: &mut Context<Self>,
    ) {
        if let Some(local) = self.model.workspaces.iter().find(|w| w.id == workspace) {
            cx.emit(FileOpen {
                workspace,
                path: local.root.join(&changed.path),
                changed: Some(changed),
                from_files: false,
            });
        }
    }
    pub(crate) fn reload_review(
        &mut self,
        workspace: crate::data::WorkspaceId,
        cx: &mut Context<Self>,
    ) {
        self.request_review(workspace, true, cx);
    }
    pub(crate) fn set_review_visible(
        &mut self,
        workspace: crate::data::WorkspaceId,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        let state = self.review_loads.entry(workspace).or_default();
        let showing = visible && !state.visible;
        state.visible = visible;
        if showing {
            self.request_review(workspace, false, cx);
        }
    }
    fn request_review(
        &mut self,
        workspace: crate::data::WorkspaceId,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .model
            .workspaces
            .iter()
            .find(|w| w.id == workspace)
            .map(|w| w.root.as_path())
            .unwrap_or(std::path::Path::new("<review>"));
        if let Err(error) = crate::preview::executable(
            self.preview().as_deref(),
            gitterm_core::preview::Effect::Git,
            target,
        ) {
            self.error = Some(error.to_string());
            return;
        }
        let Some(view) = self.reviews.get(&workspace).cloned() else {
            return;
        };
        let Some(executor) = self.executor.clone() else {
            return;
        };
        let Some(local) = self.model.workspaces.iter().find(|w| w.id == workspace) else {
            return;
        };
        let state = self.review_loads.entry(workspace).or_default();
        if !force && !state.visible {
            return;
        }
        let mut workers = self.review_workers.lock().unwrap();
        if workers.closed || !state.request(crate::review_reload::SnapshotKey::new(local), force) {
            return;
        }
        let root = local.root.clone();
        let generation = view.update(cx, |view, cx| view.begin_load(cx));
        let view = view.downgrade();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let handle = executor.handle().clone();
        workers.jobs.retain(|job| !job.is_finished());
        workers.jobs.push(executor.spawn(async move {
            let result = gitterm_gpui_review::repo::load(&handle, workspace.0 as usize, root).await;
            let _ = sender.send(result);
        }));
        drop(workers);
        cx.spawn(async move |owner, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|error| Err(format!("Review worker failed: {error}")));
            let alive = view
                .update(cx, |view, cx| view.finish_load(generation, result, cx))
                .is_ok();
            _ = owner.update(cx, |owner, cx| {
                if let Some(state) = owner.review_loads.get_mut(&workspace) {
                    state.finish();
                }
                if alive {
                    owner.request_review(workspace, false, cx);
                }
            });
            // Keep the executor alive until repo::load has joined its workers,
            // even if the owner or the weak review view has gone away.
            drop(executor);
        })
        .detach();
    }
    pub fn add_folder(
        &mut self,
        folder: PathBuf,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<crate::data::WorkspaceId> {
        anyhow::ensure!(
            self.iced_catalog().is_none(),
            "Workspaces come from Iced; add work in Iced GitTerm"
        );
        let mut folders = self.model.folders();
        if !folders.contains(&folder) {
            folders.push(folder.clone());
        }
        let save = self
            .store
            .as_ref()
            .expect("local owner has a store")
            .save_async(&folders)?;
        self.watch_save(save, cx);
        let id = self.model.add(folder, Instant::now());
        self.dispatch(Intent::Activate(id), Instant::now(), cx);
        self.dispatch(Intent::Opened, Instant::now(), cx);
        Ok(id)
    }
    pub(crate) fn watch_save(&self, save: crate::preview::SaveResult, cx: &mut Context<Self>) {
        cx.spawn(async move |owner, cx| {
            if let Err(error) = crate::preview::finish(Ok(save)).await {
                let _ = owner.update(cx, |owner, cx| {
                    let error = format!("Save local folders: {error:#}");
                    eprintln!("{error}");
                    owner.error = Some(error);
                    cx.notify();
                });
            }
        })
        .detach();
    }
    pub fn set_git_visible(&mut self, visible: bool, enter: bool, cx: &mut Context<Self>) {
        if enter && visible && !self.git_visible {
            self.dispatch(Intent::EnterGit, Instant::now(), cx);
        }
        self.git_visible = visible;
    }
}
impl Drop for WorkspaceOwner {
    fn drop(&mut self) {
        self.jobs.close();
    }
}

/// File activation shared by Files and Git status; unchanged files retain the log fallback.
#[derive(Clone, Debug)]
pub struct FileOpen {
    pub workspace: crate::data::WorkspaceId,
    pub path: PathBuf,
    pub changed: Option<crate::data::ChangedFile>,
    pub from_files: bool,
}
impl EventEmitter<FileOpen> for WorkspaceOwner {}

#[derive(Default)]
struct ReviewWorkers {
    closed: bool,
    jobs: Vec<tokio::task::JoinHandle<()>>,
}

impl WorkspaceOwner {
    pub(crate) fn resumed_workspace_order(&mut self, id: crate::WorkspaceId) {
        if let Some(catalog) = self.store.as_mut().and_then(|s| s.iced.as_mut()) {
            let index = id.0 as usize - 1;
            catalog.order = catalog.ordered_indices();
            catalog.order.retain(|&i| i != index);
            catalog.order.push(index);
        }
    }
    pub(crate) fn publish_workspace(
        &mut self,
        saved: crate::workspace_config::LiveWorkspaceSaved,
        claims_new_chats: bool,
        cx: &mut Context<Self>,
    ) -> Option<crate::WorkspaceId> {
        let catalog = self
            .store
            .as_mut()
            .and_then(|s| s.iced.as_mut())
            .expect("Live catalog");
        catalog.order = catalog.ordered_indices();
        let workspace = saved.workspace;
        let index = catalog
            .workspaces
            .iter()
            .position(|w| w.name == workspace.name && w.dir == workspace.dir)
            .unwrap_or(catalog.workspaces.len());
        let id = crate::WorkspaceId(index as u64 + 1);
        if claims_new_chats {
            catalog.claims.insert(index);
        }
        if index == catalog.workspaces.len() && workspace.active {
            let mut single = crate::workspace_config::IcedCatalog::default();
            single.workspaces.push(workspace.clone());
            let mut model = WorkspaceStatus::from_iced(&single);
            let mut local = model.workspaces.remove(0);
            local.id = id;
            self.model.workspaces.push(local);
            catalog.workspaces.push(workspace);
            catalog.order.push(index);
        } else if index < catalog.workspaces.len() {
            catalog.workspaces[index] = workspace;
        }
        let id = (index < catalog.workspaces.len()).then_some(id);
        catalog.closed = saved.closed;
        catalog.repos = saved.repos;
        cx.set_global(crate::work_sidebar::IcedWorkSource {
            config: catalog.source.clone(),
            workspaces: catalog.workspaces.clone(),
            repos: catalog.repos.clone(),
            owner: cx.entity().downgrade(),
        });
        self.error = None;
        cx.notify();
        id
    }
}

#[cfg(test)]
mod task_panel_status_tests {
    use super::*;
    use ::core::prelude::v1::test;
    #[gpui::test]
    fn task_panel_context_rejects_late_main_status_and_keeps_toplevel_discovery(
        cx: &mut TestAppContext,
    ) {
        let mut model = WorkspaceStatus::default();
        let workspace = model.add("/tmp/placeholder-main".into(), Instant::now());
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        let snapshot = |path: &str| gitterm_core::status::GitStatusSnapshot {
            tab_id: workspace.0 as usize,
            repo_path: path.into(),
            repo_name: "Placeholder".into(),
            branch_name: "placeholder".into(),
            is_git_repo: true,
            staged: vec![],
            unstaged: vec![],
            untracked: vec![],
            error: None,
        };
        owner.update(cx, |o, cx| {
            o.dispatch(
                Intent::ContextChanged {
                    workspace,
                    root: "/tmp/placeholder-task".into(),
                },
                Instant::now(),
                cx,
            );
            o.complete_context_status(workspace, 0, snapshot("/tmp/placeholder-main"), cx);
            assert_eq!(
                o.model.workspaces[0].root,
                PathBuf::from("/tmp/placeholder-task")
            );
            o.complete_context_status(workspace, 1, snapshot("/tmp/placeholder-task"), cx);
            o.dispatch(
                Intent::ContextChanged {
                    workspace,
                    root: "/tmp/placeholder-main/subdir".into(),
                },
                Instant::now(),
                cx,
            );
            o.complete_context_status(workspace, 1, snapshot("/tmp/placeholder-task"), cx);
            assert_eq!(
                o.model.workspaces[0].root,
                PathBuf::from("/tmp/placeholder-main/subdir")
            );
            o.complete_context_status(workspace, 2, snapshot("/tmp/placeholder-main"), cx);
            assert_eq!(
                o.model.workspaces[0].root,
                PathBuf::from("/tmp/placeholder-main")
            );
        });
    }
}
