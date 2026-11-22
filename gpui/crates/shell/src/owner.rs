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
    /// Startup park metadata blocks PTY construction until explicit resume.
    pub(crate) stopped_workspaces: std::collections::HashSet<crate::data::WorkspaceId>,
    pub model: WorkspaceStatus,
    pub error: Option<String>,
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
        if executor.is_some() {
            tasks.push(cx.spawn(async move |owner, cx| {
                while let Some(outcome) = receiver.recv().await {
                    if owner
                        .update(cx, |owner, cx| match outcome {
                            StatusOutcome::Completed { snapshot, .. } => {
                                owner.dispatch(Intent::Completed(snapshot), Instant::now(), cx)
                            }
                            StatusOutcome::Cancelled { .. } => {}
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
                    std::mem::take(&mut workers.jobs)
                };
                async move {
                    shutdown.teardown().await;
                    for worker in workers {
                        let _ = worker.await;
                    }
                }
            }));
        }
        let chat_shutdown = crate::chat::RuntimeShutdown::new();
        let chat_quit = cx.on_app_quit(|owner: &mut Self, cx| {
            for chat in owner.chats.values() {
                chat.update(cx, |chat, _| chat.shutdown_runtime());
            }
            if let Err(error) = owner.chat_shutdown.drain() {
                eprintln!("Drain preview Claude runtimes on quit: {error:#}");
            }
            std::future::ready(())
        });
        Self {
            stopped_workspaces: Default::default(),
            model,
            files: Default::default(),
            reviews: Default::default(),
            file_jobs: Default::default(),
            file_sender,
            error: None,
            chats: Default::default(),
            chat_shutdown,
            _chat_quit: chat_quit,
            store,
            jobs,
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
    pub(crate) fn iced_catalog(&self) -> Option<&crate::workspace_config::IcedCatalog> {
        self.store.as_ref()?.iced.as_ref()
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
    pub fn dispatch(&mut self, intent: Intent, now: Instant, cx: &mut Context<Self>) {
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
                    self.jobs.execute_with(
                        executor.handle(),
                        owner,
                        command,
                        gitterm_core::git_status::StatusReadOptions { fsmonitor: false },
                    );
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
