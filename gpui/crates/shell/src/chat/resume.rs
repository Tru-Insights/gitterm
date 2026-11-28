//! Local Claude resume metadata and confirmed identity writes.
use super::*;
use gitterm_core::{
    chat::types::{AgentBackend, AgentBackendConfig},
    preview::service::ServiceContext,
    workspace_config::{WorkspaceConfig, WorkspaceTabConfig},
};
use std::path::Path;

#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod store_tests;

#[derive(Clone)]
pub(super) struct LiveTab {
    pub services: ServiceContext,
    pub workspace_name: String,
    pub workspace_dir: String,
    pub tab: WorkspaceTabConfig,
    pub location: Option<gitterm_core::remote::config::WorkspaceLocationConfig>,
}
impl LiveTab {
    pub fn new(
        services: ServiceContext,
        workspace: &WorkspaceConfig,
        tab: WorkspaceTabConfig,
    ) -> Self {
        Self {
            services,
            workspace_name: workspace.name.clone(),
            workspace_dir: workspace.dir.clone(),
            tab,
            location: workspace.location.clone(),
        }
    }
    /// Share the workspace lifecycle's serialized owner. The chat shutdown queue
    /// waits for its receipt, so quit also drains writes not yet submitted here.
    pub fn persist(&self, store: &gitterm_core::desktop_store::DesktopStore) -> Result<()> {
        anyhow::ensure!(
            !self.services.is_preview(),
            "Cannot persist Live chat in Preview"
        );
        anyhow::ensure!(
            std::fs::canonicalize(self.services.state_root())? == store.root(),
            "Live chat persistence owner belongs to another state root"
        );
        let live = self.clone();
        store
            .transact_workspaces(move |file| {
                live.update_document(file)
                    .map_err(|error| format!("{error:#}"))
            })
            .wait()
            .map_err(anyhow::Error::msg)
    }
    fn update_document(
        &self,
        file: &mut gitterm_core::workspace_config::WorkspacesFile,
    ) -> Result<()> {
        let uid = self.tab.session_uid.as_deref();
        let bound = uid.is_some_and(|uid| {
            file.workspaces.iter().any(|workspace| {
                workspace.dir == self.workspace_dir
                    && workspace
                        .tabs
                        .iter()
                        .any(|tab| tab.session_uid.as_deref() == Some(uid))
            })
        });
        let mut matches = file.workspaces.iter_mut().filter(|workspace| {
            workspace.dir == self.workspace_dir
                && if bound {
                    workspace
                        .tabs
                        .iter()
                        .any(|tab| tab.session_uid.as_deref() == uid)
                } else {
                    workspace.name == self.workspace_name
                }
        });
        let workspace = matches
            .next()
            .context("Live chat workspace no longer exists")?;
        anyhow::ensure!(
            matches.next().is_none(),
            "Several saved workspaces have the same name and folder; this workspace cannot be changed in Live."
        );
        anyhow::ensure!(
            workspace.location == self.location,
            "Live chat workspace machine changed before saving"
        );
        let tabs = &mut workspace.tabs;
        let uid = self
            .tab
            .session_uid
            .as_deref()
            .context("Live chat has no caller identity")?;
        let id = self
            .tab
            .chat_session_id
            .as_deref()
            .context("Live chat has no confirmed provider identity")?;
        anyhow::ensure!(
            !tabs
                .iter()
                .any(|tab| tab.chat_session_id.as_deref() == Some(id)
                    && tab.session_uid.is_some()
                    && tab.session_uid.as_deref() != Some(uid)),
            "Confirmed Claude session already belongs to another tab"
        );
        if let Some(tab) = tabs.iter_mut().find(|tab| {
            tab.session_uid.as_deref() == Some(uid)
                || (tab.chat_session_id.as_deref() == Some(id) && tab.session_uid.is_none())
        }) {
            // Iced also drops nested agent_config unknowns by re-serializing its typed config.
            // Preserve other fields a newer desktop may have written.
            let mut value = serde_json::to_value(&*tab)?;
            let object = value.as_object_mut().context("Invalid workspace tab")?;
            for (key, value) in serde_json::to_value(&self.tab)?
                .as_object()
                .context("Invalid live tab")?
            {
                object.insert(key.clone(), value.clone());
            }
            *tab = serde_json::from_value(value)?;
        } else if !tabs
            .iter()
            .any(|tab| tab.chat_session_id.as_deref() == Some(id))
        {
            tabs.push(self.tab.clone());
        } else {
            anyhow::bail!("Confirmed Claude session already belongs to another tab");
        }
        if !workspace.chats.is_empty() && !workspace.chats.iter().any(|chat| chat == id) {
            workspace.chats.push(id.into());
        }
        Ok(())
    }
}

pub(super) fn default_config() -> AgentBackendConfig {
    AgentBackendConfig::Claude {
        model: "default".into(),
        permission_mode: Some("default".into()),
        effort: None,
    }
}
pub(crate) fn tab(cwd: &Path, uid: String, id: Option<String>) -> WorkspaceTabConfig {
    WorkspaceTabConfig {
        extra: Default::default(),
        dir: cwd.to_string_lossy().into_owned(),
        repo_dir: Some(cwd.to_string_lossy().into_owned()),
        startup_command: None,
        tab_kind: Some("agent".into()),
        agent_config: Some(default_config()),
        chat_session_id: id,
        task_id: None,
        task_session_id: None,
        worker_shutdown_refused: false,
        session_uid: Some(uid),
    }
}

impl ChatPane {
    pub(crate) fn rebind_live_workspace(
        &mut self,
        previous: &WorkspaceConfig,
        next: &WorkspaceConfig,
    ) {
        if let Some(live) = &mut self.live_tab
            && live.workspace_name == previous.name
            && live.workspace_dir == previous.dir
        {
            live.workspace_name = next.name.clone();
            live.workspace_dir = next.dir.clone();
        }
    }

    pub(super) fn init_live_tab(&mut self, cx: &mut Context<Self>) {
        let Some(launch) = &self.deferred_live else {
            return;
        };
        let workspace = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .and_then(|source| source.owner.upgrade())
            .and_then(|owner| owner.read(cx).iced_workspace(launch.workspace?).cloned());
        if let Some(workspace) = workspace {
            self.live_tab = Some(LiveTab::new(
                launch.services.clone(),
                &workspace,
                tab(&self.saved.cwd, self.saved.uid.clone(), None),
            ));
        }
    }
    pub(super) fn confirm_provider_id(&mut self, id: &str, cx: &mut Context<Self>) -> Result<()> {
        let claim = self
            .live_claim
            .as_ref()
            .context("Live chat has no conversation claim")?;
        self.claim_provider(claim, id)?;
        let Some(tab) = &mut self.live_tab else {
            anyhow::bail!("Live chat has no workspace persistence binding");
        };
        if self.persisted_id.as_deref() == Some(id) {
            return Ok(());
        }
        tab.tab.chat_session_id = Some(id.into());
        self.persist_live_tab(cx)
    }
    pub(super) fn persist_live_tab(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let tab = self
            .live_tab
            .as_mut()
            .context("Live chat has no workspace persistence binding")?;
        tab.tab.session_uid = Some(self.saved.uid.clone());
        tab.tab.repo_dir = Some(self.saved.cwd.to_string_lossy().into_owned());
        tab.tab.agent_config = Some(self.controller.config.clone());
        let tab = tab.clone();
        let store = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .context("Live workspace persistence owner is unavailable")?
            .0
            .clone();
        self.persisted_id = tab.tab.chat_session_id.clone();
        let (reply, saved) = tokio::sync::oneshot::channel();
        self.shutdown
            .sender
            .send(ShutdownCommand::Persist(
                Box::new(tab.clone()),
                store,
                reply,
            ))
            .map_err(|_| anyhow::anyhow!("Live chat persistence queue stopped"))?;
        if let Some(owner) = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .and_then(|source| source.owner.upgrade())
        {
            owner.update(cx, |owner, cx| {
                owner.confirm_live_tab(&tab.workspace_name, &tab.workspace_dir, tab.tab.clone(), cx)
            });
        }
        cx.spawn(async move |pane, cx| {
            let result = saved
                .await
                .unwrap_or_else(|_| Err("Live chat persistence queue stopped".into()));
            if let Err(error) = result {
                let _ = pane.update(cx, |pane, cx| {
                    pane.persisted_id = None;
                    pane.view
                        .update(cx, |view, cx| view.set_status_error(error, cx))
                });
            }
        })
        .detach();
        Ok(())
    }

    pub(crate) fn resume(
        shutdown: Arc<RuntimeShutdown>,
        preview: Arc<gitterm_core::preview::Preview>,
        workspace: WorkspaceConfig,
        tab: WorkspaceTabConfig,
        remote: Option<remote::Binding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        // Tests supply synthetic roots explicitly through resume_from.
        #[cfg(test)]
        let projects = None;
        #[cfg(not(test))]
        let projects = if remote.is_some() {
            None
        } else {
            Some(provider::live_provider_root()?.join("projects"))
        };
        Self::resume_configured(
            shutdown, preview, workspace, tab, projects, remote, window, cx,
        )
    }
    #[cfg(test)]
    fn resume_from(
        shutdown: Arc<RuntimeShutdown>,
        preview: Arc<gitterm_core::preview::Preview>,
        workspace: WorkspaceConfig,
        tab: WorkspaceTabConfig,
        projects: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        Self::resume_configured(
            shutdown, preview, workspace, tab, projects, None, window, cx,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn resume_configured(
        shutdown: Arc<RuntimeShutdown>,
        preview: Arc<gitterm_core::preview::Preview>,
        workspace: WorkspaceConfig,
        tab: WorkspaceTabConfig,
        projects: Option<PathBuf>,
        remote: Option<remote::Binding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        let config = tab
            .agent_config
            .clone()
            .context("Saved chat has no recorded backend configuration")?;
        anyhow::ensure!(
            config.backend() == AgentBackend::Claude,
            "Resume for this recorded backend is unavailable"
        );
        let id = tab.chat_session_id.clone();
        let policy::ChatLaunchPolicy::Live { services } = policy::current(preview.clone(), cx)
        else {
            anyhow::bail!("Conversation resume not available in preview");
        };
        let cwd = tab
            .repo_dir
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| {
                if remote.is_some() {
                    return None;
                }
                git2::Repository::discover(&tab.dir)
                    .ok()
                    .and_then(|repo| repo.workdir().map(PathBuf::from))
            })
            .unwrap_or_else(|| PathBuf::from(&tab.dir));
        let mut pane = Self::new(shutdown, preview, cwd, window, cx);
        pane.bind_remote(remote, cx);
        pane.controller.target.session_uid = tab
            .session_uid
            .clone()
            .unwrap_or_else(|| pane.saved.uid.clone());
        pane.controller.target.task_id = tab.task_id.clone();
        pane.controller.target.task_session_id = tab.task_session_id.clone();
        pane.saved.uid = pane.controller.target.session_uid.clone();
        pane.controller.adapter = ChatAdapter::default();
        pane.controller.source = pane
            .controller
            .adapter
            .begin_runtime(pane.controller.target.clone());
        pane.controller.adapter.bind_session(id.clone());
        pane.saved.session_id = id.clone();
        let launch = pane
            .deferred_live
            .as_mut()
            .context("Live launch policy missing")?;
        launch.caller = pane.saved.uid.clone();
        launch.workspace =
            cx.try_global::<crate::work_sidebar::IcedWorkSource>()
                .and_then(|source| {
                    source.workspaces.iter().position(|saved| {
                        saved.name == workspace.name && saved.dir == workspace.dir
                    })
                })
                .map(|index| crate::data::WorkspaceId(index as u64 + 1));
        let claim = Arc::new(provider::ConversationClaim::claim(
            &services,
            &pane.saved.uid,
        )?);
        if let Some(id) = &id {
            match &pane.remote {
                Some(remote) => claim.remote_provider_id(&remote.agent.id, id)?,
                None => claim.provider_id(id)?,
            }
        }
        pane.live_claim = Some(claim);
        let mut tab = tab;
        tab.session_uid = Some(pane.saved.uid.clone());
        pane.live_tab = Some(LiveTab::new(services, &workspace, tab));
        let AgentBackendConfig::Claude {
            model,
            permission_mode,
            effort,
        } = &config
        else {
            unreachable!()
        };
        let settings = gitterm_gpui_chat::composer::LaunchSettings {
            model: model.clone(),
            permission_mode: permission_mode.clone().unwrap_or_else(|| "default".into()),
            effort: effort.clone(),
        };
        *launch.settings.borrow_mut() = settings.clone();
        pane.view.update(cx, |view, cx| {
            view.set_live_launch_settings(&settings, cx);
        });
        if pane.remote.is_some() {
            pane.controller.config = config.clone();
            if id.is_some() {
                pane.reload_remote_history(true, cx);
            }
            pane.controller.seed_launch(&settings);
            return Ok(pane);
        }
        let history = id
            .map(|id| {
                pane.controller
                    .adapter
                    .begin_history(pane.controller.target.clone(), config)
                    .context("Saved chat history cannot be restored")
                    .map(|(ticket, _)| (id, ticket))
            })
            .transpose()?;
        pane.controller.seed_launch(&settings);
        // Iced restores an unsent tab as a fresh chat, retaining its caller and tab record.
        let Some((id, ticket)) = history else {
            return Ok(pane);
        };
        pane.view.update(cx, |view, cx| {
            view.set_read_only(Some("Restoring saved Claude session…".into()), cx);
        });
        pane.history_loading = true;
        let cwd = pane.saved.cwd.clone();
        let load = cx.background_executor().spawn(async move {
            let projects =
                projects.ok_or_else(|| "Claude transcript source unavailable".to_string())?;
            gitterm_core::chat::history::load_claude_session(&projects, &id, &cwd).map(|events| {
                events
                    .into_iter()
                    .map(|event| match event {
                        gitterm_core::chat::history::HistoryEvent::Harness(event) => {
                            AgentEvent::Harness(event)
                        }
                        gitterm_core::chat::history::HistoryEvent::Other(value) => {
                            AgentEvent::Other(value)
                        }
                    })
                    .collect()
            })
        });
        pane._history_load = Some(cx.spawn(async move |pane, cx| {
            let result = load.await;
            let _ = pane.update(cx, |pane, cx| {
                pane.history_loading = false;
                match pane.controller.adapter.complete_history(
                    &ticket,
                    &pane.controller.target,
                    AgentBackend::Claude,
                    result,
                ) {
                    Ok(Some(update)) => {
                        pane.pending.push(update);
                        pane.view
                            .update(cx, |view, cx| view.set_read_only(None, cx));
                        pane.changed(cx);
                    }
                    Ok(None) => {}
                    Err(error) => pane.resume_failed(error, cx),
                }
            });
        }));
        Ok(pane)
    }
    pub(super) fn resume_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.resume_error = Some(error.clone());
        self.view.update(cx, |view, cx| {
            view.set_read_only(Some(error.clone()), cx);
            view.set_status_error(error, cx);
        });
        cx.notify();
    }
    pub(super) fn fresh_chat(&mut self, cx: &mut Context<Self>) {
        if self.history_loading
            || (self.controller.runtime.is_some() && self.resume_error.is_none())
        {
            return;
        }
        self.shutdown_runtime(cx);
        // Keep the old provider session untouched; only this pane's binding is reset.
        if self.remote.is_none() && !self.saved.cwd.is_dir() {
            let Some(tab) = &self.live_tab else {
                return;
            };
            let cwd = PathBuf::from(&tab.workspace_dir);
            if !cwd.is_dir() {
                self.resume_failed(
                    format!("Chat directory is still missing: {}", cwd.display()),
                    cx,
                );
                return;
            }
            self.saved.cwd = cwd;
        }
        self.live_claim = None;
        let old_target = self.controller.target.clone();
        self.controller = Controller::new();
        self.controller.live = true;
        self.controller.remote = self.remote.is_some();
        self.controller.target.task_id = old_target.task_id;
        self.controller.target.task_session_id = old_target.task_session_id;
        self.saved.uid = self.controller.target.session_uid.clone();
        if let Some(launch) = &mut self.deferred_live {
            launch.caller = self.saved.uid.clone();
        }
        if let Some(owner) = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .and_then(|source| source.owner.upgrade())
        {
            let entity = cx.entity();
            owner.update(cx, |owner, _| {
                owner.chats.remove(&old_target.session_uid);
                owner.chats.insert(self.saved.uid.clone(), entity);
            });
        }
        self.controller.source = self
            .controller
            .adapter
            .begin_runtime(self.controller.target.clone());
        if let Some(launch) = &self.deferred_live {
            self.controller.seed_launch(&launch.settings.borrow());
        }
        self.persisted_id = None;
        self.saved.session_id = None;
        self.saved.records.clear();
        self.resume_error = None;
        if let Some(tab) = &mut self.live_tab {
            tab.tab.chat_session_id = None;
            tab.tab.session_uid = Some(self.saved.uid.clone());
            tab.tab.dir = self.saved.cwd.to_string_lossy().into_owned();
            tab.tab.repo_dir = Some(self.saved.cwd.to_string_lossy().into_owned());
        }
        self.pending.push(Update {
            presentation: Some(gitterm_gpui_chat::adapter::Presentation::Replay),
            ..Default::default()
        });
        let settings = self
            .deferred_live
            .as_ref()
            .map(|launch| launch.settings.borrow().clone())
            .unwrap_or_default();
        self.view.update(cx, |view, cx| {
            view.set_live_launch_settings(&settings, cx);
            view.set_status_error("Fresh chat · send a message to start".into(), cx);
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn install_store(root: &Path, cx: &mut gpui::App) {
        let store = gitterm_core::desktop_store::DesktopStore::open(root).unwrap();
        cx.set_global(crate::workspace_config::LiveWorkspaces(store));
    }
    pub(super) fn workspace(dir: &Path) -> WorkspaceConfig {
        serde_json::from_value(serde_json::json!({"name":"Synthetic", "dir":dir, "abbrev":"S", "color":"blue", "tabs":[]})).unwrap()
    }
    #[test]
    fn confirmed_identity_preserves_document_and_refuses_invalid_or_conflicting_state() {
        let dir = tempfile::tempdir_in("/private/tmp").unwrap();
        let services = ServiceContext::production(dir.path(), "resume-write-test").unwrap();
        let workspace = workspace(&dir.path().join("cwd"));
        let mut tab = tab(
            Path::new(&workspace.dir),
            "saved-caller".into(),
            Some("confirmed-id".into()),
        );
        tab.agent_config = Some(AgentBackendConfig::Claude {
            model: "opus".into(),
            permission_mode: Some("plan".into()),
            effort: Some("max".into()),
        });
        let path = dir.path().join("workspaces.json");
        let document = serde_json::json!({"future":"keep", "active_workspace":0, "workspaces":[{
            "name":workspace.name, "dir":workspace.dir, "abbrev":"S", "color":"blue", "future":42,
            "tabs":[{"dir":workspace.dir,"startup_command":"do not run","future":true}]
        }]});
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let store = gitterm_core::desktop_store::DesktopStore::open(dir.path()).unwrap();
        let live = LiveTab::new(services, &workspace, tab.clone());
        live.persist(&store).unwrap();
        live.persist(&store).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["future"], "keep");
        assert_eq!(saved["workspaces"][0]["future"], 42);
        assert_eq!(
            saved["workspaces"][0]["tabs"][0],
            document["workspaces"][0]["tabs"][0]
        );
        assert_eq!(saved["workspaces"][0]["tabs"].as_array().unwrap().len(), 2);
        let roundtrip: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_value(saved).unwrap();
        let iced = &roundtrip.workspaces[0].tabs[1];
        assert_eq!(iced.chat_session_id.as_deref(), Some("confirmed-id"));
        assert_eq!(
            serde_json::to_value(&iced.agent_config).unwrap(),
            serde_json::to_value(&tab.agent_config).unwrap()
        );
        let mut conflicting = live.clone();
        conflicting.tab.session_uid = Some("other-caller".into());
        let before = std::fs::read(&path).unwrap();
        assert!(conflicting.persist(&store).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(store);
        std::fs::write(&path, b"{partial").unwrap();
        let refused = gitterm_core::desktop_store::DesktopStore::open(dir.path()).unwrap();
        assert!(live.persist(&refused).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{partial");
    }
    #[gpui::test]
    fn async_history_is_shown_once_claims_before_send_and_requires_explicit_fresh(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let (dir, preview) = super::super::tests::fixture();
        let services = ServiceContext::production(&dir.path().join("live"), "resume-test").unwrap();
        cx.update(|cx| policy::install_live(services.clone(), cx).unwrap());
        let projects = dir.path().join("synthetic-projects");
        std::fs::create_dir_all(projects.join("slug")).unwrap();
        let id = format!(
            "synthetic-{}",
            dir.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace('.', "")
        );
        std::fs::write(
            projects.join("slug").join(format!("{id}.jsonl")),
            "{\"type\":\"user\",\"message\":{\"content\":\"restored once\"}}\n",
        )
        .unwrap();
        let workspace = workspace(dir.path());
        let mut tab = tab(dir.path(), "synthetic-caller".into(), Some(id));
        tab.agent_config = Some(AgentBackendConfig::Claude {
            model: "opus".into(),
            permission_mode: Some("plan".into()),
            effort: Some("max".into()),
        });
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = ChatPane::resume_from(
                RuntimeShutdown::new(),
                preview.clone(),
                workspace.clone(),
                tab.clone(),
                Some(projects.clone()),
                window,
                cx,
            )
            .unwrap();
            assert!(pane.history_loading);
            let receipt = pane.dispatch(
                ChatIntent::Send {
                    text: "blocked while loading".into(),
                    images: vec![],
                },
                cx,
            );
            assert!(matches!(
                receipt.state,
                gitterm_gpui_chat::adapter::ReceiptState::Rejected(_)
            ));
            assert!(pane.controller.runtime.is_none());
            pane
        });
        pane.read_with(cx, |pane, _| {
            assert!(pane.controller.runtime.is_none());
        });
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(!pane.history_loading);
            assert!(pane.resume_error.is_none(), "{:?}", pane.resume_error);
            assert_eq!(pane.user_prompts(), ["restored once"]);
            assert!(pane.controller.runtime.is_none());
            assert_eq!(
                pane.deferred_live.as_ref().unwrap().settings.borrow().model,
                "opus"
            );
            let config = pane
                .live_tab
                .as_ref()
                .unwrap()
                .tab
                .agent_config
                .clone()
                .unwrap();
            let mut adapter = ChatAdapter::default();
            adapter.bind_session(pane.saved.session_id.clone());
            assert!(
                adapter
                    .begin_history(pane.target(), config.clone())
                    .is_some()
            );
            assert!(adapter.begin_history(pane.target(), config).is_none());
        });
        std::fs::create_dir_all(services.state_root()).unwrap();
        let path = services.state_root().join("workspaces.json");
        let document = serde_json::json!({"active_workspace":0,"workspaces":[{
            "name":workspace.name,"dir":workspace.dir,"abbrev":"S","color":"blue","tabs":[tab.clone()]
        }]});
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        cx.update(|_, cx| install_store(services.state_root(), cx));
        pane.update(cx, |pane, cx| {
            assert!(pane.persisted_id.is_none());
            let id = pane.saved.session_id.clone().unwrap();
            pane.confirm_provider_id(&id, cx).unwrap();
            pane.shutdown.drain().unwrap();
        });
        let persisted: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.workspaces[0].tabs[0].chat_session_id,
            tab.chat_session_id
        );
        assert_eq!(
            persisted.workspaces[0].tabs[0].session_uid.as_deref(),
            Some("synthetic-caller")
        );
        cx.update(|window, cx| {
            // A second pane is refused even though no process was started.
            let mut failure = None;
            let duplicate = cx.new(|cx| {
                match ChatPane::resume_from(
                    RuntimeShutdown::new(),
                    preview.clone(),
                    workspace.clone(),
                    tab.clone(),
                    Some(projects.clone()),
                    window,
                    cx,
                ) {
                    Ok(_) => panic!("duplicate claim accepted"),
                    Err(error) => {
                        failure = Some(error);
                        ChatPane::history(
                            RuntimeShutdown::new(),
                            pane.read(cx).saved.clone(),
                            window,
                            cx,
                        )
                    }
                }
            });
            assert!(failure.unwrap().to_string().contains("already open"));
            drop(duplicate);
            pane.update(cx, |pane, cx| pane.close(cx));
        });
        tab.session_uid = Some("missing-caller".into());
        tab.chat_session_id = Some("missing-session".into());
        cx.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                *pane = ChatPane::resume_from(
                    RuntimeShutdown::new(),
                    preview,
                    workspace,
                    tab,
                    Some(projects),
                    window,
                    cx,
                )
                .unwrap()
            });
        });
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            assert!(pane.resume_error.as_ref().unwrap().contains("not found"));
            assert!(matches!(
                pane.dispatch(
                    ChatIntent::Send {
                        text: "must not launch".into(),
                        images: vec![]
                    },
                    cx
                )
                .state,
                gitterm_gpui_chat::adapter::ReceiptState::Rejected(_)
            ));
            assert!(pane.controller.runtime.is_none());
            assert_eq!(pane.saved.session_id.as_deref(), Some("missing-session"));
            let old_uid = pane.saved.uid.clone();
            pane.fresh_chat(cx);
            assert_ne!(pane.saved.uid, old_uid);
            assert_eq!(pane.deferred_live.as_ref().unwrap().caller, pane.saved.uid);
            assert!(pane.resume_error.is_none());
            assert!(pane.saved.session_id.is_none());
            assert!(pane.controller.runtime.is_none());
        });
        cx.run_until_parked();
    }
    #[gpui::test]
    fn missing_saved_cwd_offers_fresh_in_the_workspace_without_launching(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let (dir, preview) = super::super::tests::fixture();
        let services =
            ServiceContext::production(&dir.path().join("live"), "missing-cwd-test").unwrap();
        cx.update(|cx| policy::install_live(services, cx).unwrap());
        let workspace = workspace(dir.path());
        let tab = tab(
            &dir.path().join("gone"),
            "missing-cwd-caller".into(),
            Some("missing-cwd-session".into()),
        );
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::resume_from(
                RuntimeShutdown::new(),
                preview,
                workspace,
                tab,
                Some(dir.path().join("synthetic-projects")),
                window,
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            assert!(
                pane.resume_error
                    .as_deref()
                    .unwrap()
                    .contains("directory is missing")
            );
            assert!(pane.controller.runtime.is_none());
            pane.fresh_chat(cx);
            assert_eq!(pane.saved.cwd, dir.path());
            assert!(pane.saved.session_id.is_none());
            assert!(pane.resume_error.is_none());
            assert!(pane.controller.runtime.is_none());
        });
        cx.run_until_parked();
    }
}
