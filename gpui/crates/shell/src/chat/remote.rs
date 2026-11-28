//! Live remote chats use the core supervision and the same single pane receiver.
use super::*;
use gitterm_core::remote::config::RemoteAgentConfig;

#[derive(Clone)]
pub(crate) struct Binding {
    pub agent: RemoteAgentConfig,
    pub client: Arc<dyn crate::remote_client::RemoteClient>,
}
/// A close must outlive a canceled history load and even the pane itself.
/// The application's shutdown queue owns this lookup, stop, and completion.
pub(super) struct PendingStop {
    remote: Binding,
    config: gitterm_core::harness::claude::ClaudeSessionConfig,
    _claim: Option<Arc<provider::ConversationClaim>>,
}
impl PendingStop {
    pub(super) fn run(self, executor: &tokio::runtime::Runtime) -> Result<()> {
        let id = self
            .config
            .resume
            .as_deref()
            .context("Missing saved remote chat id")?;
        let Some(id) = self
            .remote
            .client
            .running_chat(&self.remote.agent, id)
            .map_err(anyhow::Error::msg)
            .context("List running remote chat for close")?
        else {
            return Ok(());
        };
        let (mut session, events) = self
            .remote
            .client
            .chat_runtime(&self.remote.agent, self.config, Some(id))
            .map_err(anyhow::Error::msg)
            .context("Attach remote chat for close")?;
        let sent = session.send(gitterm_core::chat::types::HarnessCommand::Shutdown);
        session.request_shutdown();
        let completed = executor.block_on(session.completed());
        drop(events);
        sent.map_err(anyhow::Error::msg)
            .context("Send remote chat Shutdown")?;
        completed
            .map_err(anyhow::Error::msg)
            .context("Reap stopped remote chat")
    }
}
impl ChatPane {
    pub(super) fn stop_restoring_remote(&self) -> Result<()> {
        if !self.history_loading || self.controller.runtime.is_some() {
            return Ok(());
        }
        let Some(remote) = self.remote.clone() else {
            return Ok(());
        };
        let launch = self
            .deferred_live
            .as_ref()
            .context("Missing remote restore launch")?;
        let mut config = live_config(
            self.saved.cwd.clone(),
            &launch.settings.borrow(),
            vec![],
            false,
        );
        config.resume = self.saved.session_id.clone();
        self.shutdown
            .sender
            .send(ShutdownCommand::StopRemote(Box::new(PendingStop {
                remote,
                config,
                _claim: self.live_claim.clone(),
            })))
            .map_err(|_| anyhow::anyhow!("Remote chat shutdown queue stopped"))
    }
    pub(crate) fn bind_remote(&mut self, binding: Option<Binding>, cx: &mut Context<Self>) {
        self.controller.remote = binding.is_some();
        self.remote = binding;
        if self.remote.is_some() {
            self.view
                .update(cx, |view, cx| view.enable_live_permissions(cx));
        }
    }
    pub(super) fn claim_provider(
        &self,
        claim: &provider::ConversationClaim,
        id: &str,
    ) -> Result<()> {
        match &self.remote {
            Some(remote) => claim.remote_provider_id(&remote.agent.id, id),
            None => match self.controller.config.backend() {
                gitterm_core::chat::types::AgentBackend::Acp(agent) => claim.acp_id(agent, id),
                _ => claim.provider_id(id),
            },
        }
    }
    pub(super) fn launch_remote(
        &mut self,
        attach: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let launch = self
            .deferred_live
            .as_ref()
            .context("Live launch policy missing")?;
        anyhow::ensure!(!launch.services.is_preview(), "Remote chats require Live");
        let remote = self.remote.as_ref().context("Remote chat has no machine")?;
        let settings = launch.settings.borrow().clone();
        let workspace = self.live_tab.as_ref().and_then(|tab| {
            cx.try_global::<crate::work_sidebar::IcedWorkSource>()
                .and_then(|source| source.owner.upgrade())
                .and_then(|owner| {
                    owner
                        .read(cx)
                        .iced_catalog()?
                        .workspaces
                        .iter()
                        .find(|workspace| {
                            workspace.name == tab.workspace_name
                                && workspace.dir == tab.workspace_dir
                        })
                        .cloned()
                })
        });
        // These paths belong to the remote. No local cwd validation, provider
        // probing, task/browser MCP descriptors or snapshot recorder crosses machines.
        let mut config = live_config(
            self.saved.cwd.clone(),
            &settings,
            vec![],
            workspace.as_ref().is_some_and(|w| w.claude_in_chrome),
        );
        config.resume = self.saved.session_id.clone();
        if attach.is_none() {
            use gitterm_core::guidelines;
            let instructions = guidelines::for_launch(
                launch.services.state_root(),
                &config.cwd,
                workspace.as_ref().is_some_and(|w| w.guidelines_off),
                guidelines::InstructionFile::Unknown,
            );
            config.guidelines_content = instructions.as_ref().map(|g| g.text.clone());
            // Core's wire config reads the shared guideline file, as Iced does.
            config.guidelines_file = instructions.map(|g| g.path);
        }
        let claim = match &self.live_claim {
            Some(claim) => claim.clone(),
            None => Arc::new(provider::ConversationClaim::claim(
                &launch.services,
                &launch.caller,
            )?),
        };
        if let Some(id) = &config.resume {
            self.claim_provider(&claim, id)?;
        }
        let (session, mut events) = remote
            .client
            .chat_runtime(&remote.agent, config, attach)
            .map_err(anyhow::Error::msg)?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        cx.background_executor()
            .spawn(async move {
                while let Some(event) = events.recv().await {
                    if tx.send(RuntimeEvent::Harness(event)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        self.live_claim = Some(claim.clone());
        self.controller.seed_launch(&settings);
        self.attach_runtime(
            OwnedRuntime {
                session,
                _lease: None,
                _conversation: Some(claim),
            },
            rx,
            cx,
        );
        Ok(())
    }

    /// History precedes the first attach, including redelivered pending requests.
    /// Gap repair uses core's history tickets to buffer live events during reload.
    pub(super) fn reload_remote_history(&mut self, attach_running: bool, cx: &mut Context<Self>) {
        let Some(remote) = self.remote.clone() else {
            return;
        };
        if !attach_running {
            if self.saved.session_id.is_none() {
                return;
            }
            self.pending.clear();
            self.pending.push(self.controller.adapter.reset_history());
        }
        let Some((ticket, id)) = self.controller.adapter.begin_history(
            self.controller.target.clone(),
            self.controller.config.clone(),
        ) else {
            return;
        };
        self.history_loading = true;
        let load = cx.background_executor().spawn(async move {
            let events = remote.client.chat_history(&remote.agent, &id)?;
            let running = if attach_running {
                remote.client.running_chat(&remote.agent, &id)
            } else {
                Ok(None)
            };
            Ok::<_, String>((events, running))
        });
        self._history_load = Some(cx.spawn(async move |pane, cx| {
            let result = load.await;
            let _ = pane.update(cx, |pane, cx| {
                pane.history_loading = false;
                let (history, running) = match result {
                    Ok((events, running)) => (Ok(events), running),
                    Err(error) => (Err(error), Ok(None)),
                };
                match pane.controller.adapter.complete_history(
                    &ticket,
                    &pane.controller.target,
                    gitterm_core::chat::types::AgentBackend::Claude,
                    history,
                ) {
                    Ok(Some(update)) => {
                        pane.pending.push(update);
                        pane.view
                            .update(cx, |view, cx| view.set_read_only(None, cx));
                        pane.changed(cx);
                        match running {
                            Ok(Some(id)) if pane.controller.runtime.is_none() => {
                                if let Err(error) = pane.launch_remote(Some(id), cx) {
                                    pane.resume_failed(
                                        format!("Attach remote chat: {error:#}"),
                                        cx,
                                    );
                                }
                            }
                            Err(error) => pane.view.update(cx, |view, cx| {
                                view.set_status_error(
                                    format!("List running remote chats: {error}"),
                                    cx,
                                )
                            }),
                            _ => {}
                        }
                    }
                    Ok(None) => {}
                    Err(error) => pane.resume_failed(error, cx),
                }
            });
        }));
    }
}
