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
    pub model: WorkspaceStatus,
    pub error: Option<String>,
    git_visible: bool,
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
        let mut tasks = vec![];
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
            quit = Some(cx.on_app_quit(move |_, _| {
                let shutdown = shutdown.clone();
                async move {
                    shutdown.teardown().await;
                }
            }));
        }
        Self {
            model,
            error: None,
            store,
            jobs,
            executor,
            _tasks: tasks,
            _quit: quit,
            git_visible: true,
        }
    }
    pub fn dispatch(&mut self, intent: Intent, now: Instant, cx: &mut Context<Self>) {
        for command in self.model.dispatch(intent, now) {
            if let Command::Status { owner, command } = command
                && let Some(executor) = &self.executor
            {
                self.jobs.execute(executor.handle(), owner, command);
            }
        }
        cx.notify();
    }
    pub fn add_folder(
        &mut self,
        folder: PathBuf,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<crate::data::WorkspaceId> {
        let mut folders = self.model.folders();
        if !folders.contains(&folder) {
            folders.push(folder.clone());
        }
        self.store
            .as_ref()
            .expect("local owner has a store")
            .save(&folders)?;
        let id = self.model.add(folder, Instant::now());
        self.dispatch(Intent::Activate(id), Instant::now(), cx);
        self.dispatch(Intent::Opened, Instant::now(), cx);
        Ok(id)
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
