//! Live ACP composition; Preview never enters this path.
use super::*;
use gitterm_core::{
    chat::types::{AgentBackend, AgentBackendConfig},
    harness::{acp::AcpMcpServer, acp_launch},
    preview::service::ServiceContext,
};

impl ChatPane {
    pub(crate) fn backend_label(&self) -> String {
        match self.controller.config.backend() {
            AgentBackend::Claude => "Claude".into(),
            AgentBackend::Acp(agent) => agent.label().into(),
            AgentBackend::Pi => "pi".into(),
        }
    }
    pub(super) fn receive_acp_event(
        &mut self,
        event: &RuntimeEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(self.controller.config.backend(), AgentBackend::Acp(_)) {
            return false;
        }
        match event {
            RuntimeEvent::Harness(HarnessEvent::Error(error)) if !self.controller.ready => {
                self.resume_error = Some(error.clone())
            }
            RuntimeEvent::Harness(HarnessEvent::Ready { session_id, .. }) => {
                if self.history_loading {
                    let source = self.controller.source.clone();
                    if let Some(update) = self.controller.apply_acp_history(&source, vec![]) {
                        self.pending.push(update);
                    }
                    self.history_loading = false;
                }
                self.resume_error = None;
                self.awaiting_resume = false;
                if let Some(id) = session_id {
                    self.controller.adapter.bind_session(Some(id.clone()));
                    if let Err(error) = self.confirm_provider_id(id, cx) {
                        self.shutdown_runtime(cx);
                        self.resume_failed(format!("Cannot resume ACP: {error:#}"), cx);
                        return true;
                    }
                }
            }
            RuntimeEvent::Harness(HarnessEvent::HistoryReplayed { .. }) => {
                self.history_loading = false
            }
            _ => {}
        }
        false
    }
    pub(super) fn configure_acp(&mut self, config: AgentBackendConfig, cx: &mut Context<Self>) {
        let AgentBackendConfig::Acp {
            agent,
            model,
            mode,
            effort,
        } = &config
        else {
            return;
        };
        let settings = gitterm_gpui_chat::composer::LaunchSettings {
            model: model.clone(),
            permission_mode: mode.clone().unwrap_or_else(|| agent.default_mode().into()),
            effort: effort.clone(),
        };
        self.controller.config = config.clone();
        self.controller.seed_launch(&settings);
        if let Some(launch) = &self.deferred_live {
            *launch.settings.borrow_mut() = settings.clone();
        }
        self.view.update(cx, |view, cx| {
            view.set_backend(config.backend(), cx);
            view.set_live_permissions(cx);
            view.set_live_launch_settings(&settings, cx);
            view.set_read_only(config.read_only_note().map(str::to_owned), cx);
        });
    }

    pub(crate) fn load_acp_history(&mut self, cx: &mut Context<Self>) {
        if !self.history_loading
            || self.controller.runtime.is_some()
            || self.resume_error.is_some()
            || !matches!(self.controller.config.backend(), AgentBackend::Acp(_))
        {
            return;
        }
        if let Err(error) = self.launch_acp(cx) {
            self.history_loading = false;
            self.resume_failed(format!("{error:#}"), cx);
        }
    }

    pub(super) fn launch_acp(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let launch = self
            .deferred_live
            .as_ref()
            .context("Live launch policy missing")?;
        let servers = live_servers(&launch.services, &launch.caller, cx)?;
        let workspace = cx
            .try_global::<crate::work_sidebar::IcedWorkSource>()
            .and_then(|source| source.owner.upgrade())
            .and_then(|owner| owner.read(cx).iced_workspace(launch.workspace?).cloned());
        let settings = launch.settings.borrow().clone();
        self.controller.source = self
            .controller
            .adapter
            .begin_runtime(self.controller.target.clone());
        self.controller.ready = false;
        self.controller.correlations.clear();
        self.controller.seed_launch(&settings);
        let config = acp_launch::session_config(
            &self.controller.config,
            self.saved.cwd.clone(),
            self.saved.session_id.clone(),
            servers,
            None,
        )
        .context("This recorded ACP backend is read-only")?;
        let claim = match &self.live_claim {
            Some(claim) => claim.clone(),
            None => Arc::new(provider::ConversationClaim::claim(
                &launch.services,
                &launch.caller,
            )?),
        };
        if let Some(id) = &config.resume {
            claim.acp_id(config.agent, id)?;
        }
        let (status, receiver) = std::sync::mpsc::channel();
        let (runtime, events) = start_live_acp(
            &launch.services,
            config,
            claim.clone(),
            provider::LiveInstructions {
                workspace_off: workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.guidelines_off),
                places: workspace
                    .as_ref()
                    .and_then(gitterm_core::guidelines::config_document_places),
                status,
            },
        )?;
        self.live_claim = Some(claim);
        self.guidelines_status = Some(receiver);
        self.attach_runtime(runtime, events, cx);
        Ok(())
    }
}

pub(super) fn start_live_acp(
    services: &ServiceContext,
    config: acp_launch::AcpSessionConfig,
    claim: Arc<provider::ConversationClaim>,
    instructions: provider::LiveInstructions,
) -> Result<(
    OwnedRuntime,
    tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
)> {
    anyhow::ensure!(
        !services.is_preview(),
        "Live chat requires production services"
    );
    anyhow::ensure!(
        config.cwd.is_absolute() && config.cwd.is_dir(),
        "ACP workspace must be an absolute existing folder: {}",
        config.cwd.display()
    );
    let root = services.state_root().to_path_buf();
    #[cfg(test)]
    let program_override = tests::PROGRAM.with(|program| program.borrow().clone());
    let (session, events) = Session::spawn_acp_runtime(move |close| {
        if close.is_cancelled() {
            return Err("ACP chat closed before launch".into());
        }
        use gitterm_core::{
            guidelines,
            turn_snapshots::{Recorder, SnapshotRoots},
        };
        #[cfg(test)]
        let program = program_override
            .unwrap_or_else(|| acp_launch::resolve_program(config.agent.program()))?;
        #[cfg(not(test))]
        let program = acp_launch::resolve_program(config.agent.program())?;
        let mut config = config;
        let task_tools = config
            .mcp_servers
            .iter()
            .any(|server| server.name == "gitterm_tasks");
        let guidelines = guidelines::for_launch(
            &root,
            &config.cwd,
            instructions.workspace_off,
            guidelines::InstructionFile::Unknown,
        );
        let _ = instructions.status.send(guidelines.is_some());
        config.guidelines = guidelines::launch_instructions(
            guidelines,
            instructions.places.as_ref(),
            task_tools,
            &root.join("launch-instructions"),
        );
        let snapshots = Recorder::new(
            SnapshotRoots::new(root.join("turn-snapshots"), std::env::temp_dir())?,
            config.cwd.clone(),
            config.resume.clone(),
            Some(gitterm_core::chat_facts::facts_dir(&root)),
            dirs::home_dir(),
        )
        .with_close_token(close);
        Ok(acp_launch::local_launch(
            env!("GITTERM_DESKTOP_VERSION"),
            config,
            program,
            Box::new(snapshots),
        ))
    });
    Ok((
        OwnedRuntime {
            session,
            _lease: None,
            _conversation: Some(claim),
        },
        events,
    ))
}

fn live_servers(
    services: &ServiceContext,
    caller: &str,
    cx: &gpui::App,
) -> Result<Vec<AcpMcpServer>> {
    #[cfg(test)]
    if let Some(servers) = tests::SERVERS.with(|servers| servers.borrow().clone()) {
        return Ok(servers);
    }
    let host = cx
        .try_global::<crate::task_host::GpuiTaskHost>()
        .context("Live ACP requires the live task/browser host; it is unavailable")?
        .0
        .read(cx);
    validate_live_host(services, host.host().services())?;
    let servers = host.acp_mcp_servers(caller);
    anyhow::ensure!(
        servers.len() == 2,
        "Live ACP task/browser host has shut down"
    );
    Ok(servers)
}

pub(crate) fn live_acp_catalog(
    defaults: &gitterm_core::settings::ChatDefaults,
) -> Vec<crate::session_picker::AcpChat> {
    gitterm_core::chat::types::AcpAgent::ALL
        .into_iter()
        .map(|id| {
            let config = acp_launch::new_chat_config(defaults, id);
            crate::session_picker::AcpChat {
                id,
                model: config.chat_model().unwrap_or_default().into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;

impl Controller {
    pub(super) fn apply_acp_history(
        &mut self,
        source: &RuntimeStamp,
        entries: Vec<gitterm_core::harness::transcript::TranscriptEntry>,
    ) -> Option<Update> {
        let ticket = self
            .adapter
            .state()
            .history_ticket(self.target.clone(), self.config.backend())?;
        let entries = entries
            .into_iter()
            .map(|entry| gitterm_core::harness::transcript::TimedEntry { at: None, entry })
            .collect();
        let history = gitterm_core::chat::history::history_timeline(entries, &[])
            .into_iter()
            .map(|event| match event {
                gitterm_core::chat::history::HistoryEvent::Harness(event) => {
                    AgentEvent::Harness(event)
                }
                gitterm_core::chat::history::HistoryEvent::Other(value) => AgentEvent::Other(value),
            })
            .collect();
        match self.adapter.complete_history(
            &ticket,
            &self.target,
            self.config.backend(),
            Ok(history),
        ) {
            Ok(update) => update,
            Err(error) => self.adapter.receive(
                source,
                &self.target,
                AgentEvent::Harness(HarnessEvent::Error(error)),
                Some(now_ms()),
                |_, _| {},
            ),
        }
    }
}
