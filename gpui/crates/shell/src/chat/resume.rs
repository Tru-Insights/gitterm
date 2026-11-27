//! Local Claude resume metadata and confirmed identity writes.
use super::*;
use gitterm_core::{
    chat::types::{AgentBackend, AgentBackendConfig},
    preview::service::ServiceContext,
    workspace_config::{WorkspaceConfig, WorkspaceTabConfig},
};
use std::path::Path;

#[derive(Clone)]
pub(super) struct LiveTab {
    pub services: ServiceContext,
    pub workspace_name: String,
    pub workspace_dir: String,
    pub tab: WorkspaceTabConfig,
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
        }
    }
    /// Read/modify the current document, retaining unknown fields and other tabs.
    /// The chat shutdown queue serializes these writes and drains them on quit.
    pub fn persist(&self) -> Result<()> {
        use std::io::Write;
        anyhow::ensure!(
            !self.services.is_preview(),
            "Cannot persist Live chat in Preview"
        );
        let root = self.services.state_root();
        let path = root.join("workspaces.json");
        let mut document: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("Read {}", path.display()))?,
        )
        .with_context(|| format!("Parse {}", path.display()))?;
        // Validate the shared contract before changing the raw JSON document.
        let _: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_value(document.clone())?;
        let workspace = document["workspaces"]
            .as_array_mut()
            .context("Missing workspaces")?
            .iter_mut()
            .find(|workspace| {
                workspace["name"] == self.workspace_name && workspace["dir"] == self.workspace_dir
            })
            .context("Live chat workspace no longer exists")?;
        let tabs = workspace["tabs"]
            .as_array_mut()
            .context("Missing workspace tabs")?;
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
            !tabs.iter().any(|tab| tab["chat_session_id"] == id
                && !tab["session_uid"].is_null()
                && tab["session_uid"] != uid),
            "Confirmed Claude session already belongs to another tab"
        );
        if let Some(tab) = tabs.iter_mut().find(|tab| {
            tab["session_uid"] == uid
                || (tab["chat_session_id"] == id && tab["session_uid"].is_null())
        }) {
            // Preserve fields a newer desktop may have written.
            let object = tab.as_object_mut().context("Invalid workspace tab")?;
            for (key, value) in serde_json::to_value(&self.tab)?
                .as_object()
                .context("Invalid live tab")?
            {
                object.insert(key.clone(), value.clone());
            }
        } else if !tabs.iter().any(|tab| tab["chat_session_id"] == id) {
            tabs.push(serde_json::to_value(&self.tab)?);
        } else {
            anyhow::bail!("Confirmed Claude session already belongs to another tab");
        }
        if let Some(chats) = workspace
            .get_mut("chats")
            .and_then(serde_json::Value::as_array_mut)
            && !chats.is_empty()
            && !chats.iter().any(|chat| chat == id)
        {
            chats.push(serde_json::Value::String(id.into()));
        }
        let bytes = serde_json::to_vec_pretty(&document)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(&path)
            .with_context(|| format!("Save confirmed chat to {}", path.display()))?;
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
        dir: cwd.to_string_lossy().into_owned(),
        repo_dir: None,
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
        claim.provider_id(id)?;
        let Some(tab) = &mut self.live_tab else {
            anyhow::bail!("Live chat has no workspace persistence binding");
        };
        if self.persisted_id.as_deref() == Some(id) {
            return Ok(());
        }
        tab.tab.chat_session_id = Some(id.into());
        tab.tab.session_uid = Some(self.saved.uid.clone());
        let tab = tab.clone();
        self.persisted_id = Some(id.into());
        let (reply, saved) = tokio::sync::oneshot::channel();
        self.shutdown
            .sender
            .send(ShutdownCommand::Persist(Box::new(tab.clone()), reply))
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        // Tests supply synthetic roots explicitly through resume_from.
        #[cfg(test)]
        let projects = None;
        #[cfg(not(test))]
        let projects = Some(provider::live_provider_root()?.join("projects"));
        Self::resume_from(shutdown, preview, workspace, tab, projects, window, cx)
    }
    fn resume_from(
        shutdown: Arc<RuntimeShutdown>,
        preview: Arc<gitterm_core::preview::Preview>,
        workspace: WorkspaceConfig,
        tab: WorkspaceTabConfig,
        projects: Option<PathBuf>,
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
        let id = tab
            .chat_session_id
            .clone()
            .context("Saved chat has no provider session identity")?;
        let policy::ChatLaunchPolicy::Live { services } = policy::current(preview.clone(), cx)
        else {
            anyhow::bail!("Conversation resume not available in preview");
        };
        let mut pane = Self::new(shutdown, preview, PathBuf::from(&tab.dir), window, cx);
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
        pane.controller.adapter.bind_session(Some(id.clone()));
        pane.saved.session_id = Some(id.clone());
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
        claim.provider_id(&id)?;
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
            view.set_read_only(Some("Restoring saved Claude session…".into()), cx);
        });
        let (ticket, _) = pane
            .controller
            .adapter
            .begin_history(pane.controller.target.clone(), config)
            .context("Saved chat history cannot be restored")?;
        pane.controller.seed_launch(&settings);
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
        if self.history_loading || self.controller.runtime.is_some() {
            return;
        }
        // Keep the old provider session untouched; only this pane's binding is reset.
        if !self.saved.cwd.is_dir() {
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
    fn workspace(dir: &Path) -> WorkspaceConfig {
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
        let live = LiveTab::new(services, &workspace, tab.clone());
        live.persist().unwrap();
        live.persist().unwrap();
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
        assert!(conflicting.persist().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::write(&path, b"{partial").unwrap();
        assert!(live.persist().is_err());
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
