//! Iced's composer effects, executed by the existing Live owners.
use super::*;
use gitterm_core::chat::{
    state::{CheckoutContext, Effect},
    types::{AgentBackend, SettingChange},
};
use gitterm_gpui_chat::{
    events::ComposerEvent,
    review::{ReviewKind, ReviewScope, ReviewTarget},
};

pub(super) fn new_chat_selection(cx: &gpui::App) -> gitterm_gpui_chat::composer::LaunchSettings {
    let config = cx
        .try_global::<crate::workspace_config::LiveWorkspaces>()
        .map(|store| store.0.root().to_path_buf())
        .or_else(|| {
            cx.try_global::<crate::work_sidebar::IcedWorkSource>()
                .map(|source| source.config.clone())
        });
    let selection = config
        .map(|root| crate::workspace_config::load_iced_settings_from(&root).chat)
        .unwrap_or_default()
        .new_chat_selection();
    gitterm_gpui_chat::composer::LaunchSettings {
        model: selection.model,
        effort: selection.effort,
        permission_mode: "default".into(),
    }
}

/// Resolve the owning workspace by pane identity, including an offered checkout
/// outside its root. Fixtures without a persisted binding keep the old inference.
pub(crate) fn delegation_workspace<'a>(
    uid: &str,
    cwd: &std::path::Path,
    workspaces: &'a [gitterm_core::workspace_config::WorkspaceConfig],
    cx: &gpui::App,
) -> Option<&'a gitterm_core::workspace_config::WorkspaceConfig> {
    let binding = cx
        .try_global::<crate::work_sidebar::IcedWorkSource>()
        .and_then(|source| source.owner.upgrade())
        .and_then(|owner| {
            owner.read(cx).chats.get(uid).and_then(|pane| {
                pane.read(cx)
                    .live_tab
                    .as_ref()
                    .map(|tab| (tab.workspace_name.clone(), tab.workspace_dir.clone()))
            })
        });
    match binding {
        Some((name, dir)) => workspaces
            .iter()
            .find(|workspace| workspace.name == name && workspace.dir == dir),
        None => workspaces
            .iter()
            .filter(|workspace| cwd.starts_with(&workspace.dir))
            .max_by_key(|workspace| workspace.dir.len()),
    }
}

impl ChatPane {
    /// Queue behind earlier remembered choices so an immediately opened chat
    /// sees the owner's current selection, even before its disk write finishes.
    pub(super) fn load_launch_defaults(&mut self, cx: &mut Context<Self>) {
        let Some(store) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|store| store.0.clone())
        else {
            return;
        };
        let receipt = store.snapshot();
        self.defaults_loading = true;
        self.view.update(cx, |view, cx| {
            view.set_read_only(Some("Loading chat settings".into()), cx)
        });
        cx.spawn(async move |pane, cx| {
            let settings = cx
                .background_executor()
                .spawn(async move { receipt.wait() })
                .await;
            let _ = pane.update(cx, |pane, cx| {
                if !pane.defaults_loading {
                    return;
                }
                pane.defaults_loading = false;
                // Resume and remote construction replace this fresh-chat setup.
                if pane.saved.session_id.is_some()
                    || pane.remote.is_some()
                    || pane.controller.config.backend() != AgentBackend::Claude
                {
                    return;
                }
                let result = settings.and_then(|snapshot| match snapshot.settings.loaded {
                    gitterm_core::desktop_store::LoadState::Valid(settings) => Ok(settings.chat),
                    gitterm_core::desktop_store::LoadState::Missing => Ok(Default::default()),
                    gitterm_core::desktop_store::LoadState::Refused(error) => Err(error),
                });
                match result {
                    Ok(defaults) => {
                        let selection = defaults.new_chat_selection();
                        let settings = gitterm_gpui_chat::composer::LaunchSettings {
                            model: selection.model,
                            effort: selection.effort,
                            permission_mode: "default".into(),
                        };
                        pane.controller.seed_launch(&settings);
                        pane.view
                            .update(cx, |view, cx| view.set_live_launch_settings(&settings, cx));
                    }
                    Err(error) => {
                        pane.resume_error = Some(format!("Load chat settings: {error}"));
                        pane.view.update(cx, |view, cx| {
                            view.set_status_error(format!("Load chat settings: {error}"), cx)
                        });
                    }
                }
            });
        })
        .detach();
    }
    pub(super) fn subscribe_controls(view: &Entity<ChatView>, cx: &mut Context<Self>) {
        cx.subscribe(view, |pane, _, event: &ComposerEvent, cx| match event {
            ComposerEvent::SettingConfigured(change) => {
                pane.configure_prelaunch(change.clone(), cx)
            }
            ComposerEvent::ChooseCheckout(path) => {
                if let Err(error) = pane.choose_checkout(path.clone(), cx) {
                    let view = pane.view.clone();
                    cx.defer(move |cx| {
                        view.update(cx, |view, cx| {
                            view.set_status_error(format!("Choose checkout: {error:#}"), cx)
                        })
                    });
                }
            }
        })
        .detach();
    }
    fn configure_prelaunch(&mut self, change: SettingChange, cx: &mut Context<Self>) {
        if !self.controller.live
            || self.controller.runtime.is_some()
            || self.deferred_live.is_none()
        {
            return;
        }
        if let Effect::Rejected(error) = self
            .controller
            .adapter
            .configure_setting(self.controller.config.clone(), change.clone())
        {
            let view = self.view.clone();
            cx.defer(move |cx| view.update(cx, |view, cx| view.set_status_error(error, cx)));
            return;
        }
        let mut settings = self
            .deferred_live
            .as_ref()
            .unwrap()
            .settings
            .borrow()
            .clone();
        match &change {
            SettingChange::Model(value) => settings.model.clone_from(value),
            SettingChange::Mode(value) => settings.permission_mode.clone_from(value),
            SettingChange::Effort(value) => settings.effort.clone_from(value),
        }
        self.controller.seed_launch(&settings);
        *self.deferred_live.as_ref().unwrap().settings.borrow_mut() = settings;
        self.remember_choice(change, cx);
        if self.live_tab.is_some()
            && let Err(error) = self.persist_live_tab(cx)
        {
            let view = self.view.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| {
                    view.set_status_error(format!("Save chat settings: {error:#}"), cx)
                })
            });
        }
    }
    pub(super) fn remember_choice(&mut self, change: SettingChange, cx: &mut Context<Self>) {
        if !self.controller.live
            || self.controller.config.backend() != AgentBackend::Claude
            || matches!(change, SettingChange::Mode(_))
        {
            return;
        }
        let Some(store) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|store| store.0.clone())
        else {
            let view = self.view.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| {
                    view.set_status_error(
                        "Live settings persistence owner is unavailable".into(),
                        cx,
                    )
                })
            });
            return;
        };
        let receipt = store.edit_settings(move |settings| match change {
            SettingChange::Model(model) => settings.chat.remember_model(&model),
            SettingChange::Effort(effort) => settings.chat.remember_effort(effort.as_deref()),
            SettingChange::Mode(_) => {}
        });
        cx.spawn(async move |pane, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { receipt.wait() })
                .await;
            if let Err(error) = result {
                let _ = pane.update(cx, |pane, cx| {
                    pane.view.update(cx, |view, cx| {
                        view.set_status_error(format!("Save chat defaults: {error}"), cx)
                    })
                });
            }
        })
        .detach();
    }
    fn live_checkout_source(&self) -> Option<gitterm_gpui_chat::composer::CheckoutSource> {
        let tab = self.live_tab.as_ref()?;
        Some(checkout::source(
            PathBuf::from(&tab.workspace_dir),
            Some(tab.services.state_root().to_path_buf()),
        ))
    }
    pub(super) fn refresh_live_checkout_source(&self, cx: &mut Context<Self>) {
        if let Some(source) = self.live_checkout_source() {
            self.view
                .update(cx, |view, cx| view.set_checkout_source(source, cx));
        }
    }
    pub(super) fn choose_checkout(&mut self, path: PathBuf, cx: &mut Context<Self>) -> Result<()> {
        anyhow::ensure!(
            self.controller.live,
            "Choose checkout is not available in preview"
        );
        anyhow::ensure!(self.remote.is_none(), "This chat runs on another machine");
        let workspace = self
            .live_tab
            .as_ref()
            .map(|tab| PathBuf::from(&tab.workspace_dir))
            .context("Chat has no owning workspace")?;
        let config = self
            .live_tab
            .as_ref()
            .unwrap()
            .services
            .state_root()
            .to_path_buf();
        let choices = checkout::source(workspace, Some(config))(cx);
        let selected = choices
            .iter()
            .find(|choice| {
                gitterm_core::chat::checkout::paths_equal(std::path::Path::new(&choice.path), &path)
            })
            .context("That checkout is no longer available in this workspace")?;
        let effect = self.controller.adapter.choose_checkout(
            self.controller.source.clone(),
            self.controller.target.clone(),
            CheckoutContext {
                choices: choices
                    .iter()
                    .map(|choice| PathBuf::from(&choice.path))
                    .collect(),
                locked_reason: None,
                process_running: self.controller.runtime.is_some(),
            },
            selected.path.clone().into(),
        );
        match effect {
            Effect::CheckoutBound { runtime, path } => {
                self.controller.source = runtime;
                self.saved.cwd = path.clone();
                self.live_tab.as_mut().unwrap().tab.dir = path.display().to_string();
                self.persist_live_tab(cx)?;
                let view = self.view.clone();
                cx.defer(move |cx| view.update(cx, |view, cx| view.set_checkout(path, cx)));
                self.changed(cx);
                Ok(())
            }
            Effect::None => Ok(()),
            Effect::Rejected(error) => Err(anyhow::anyhow!(error)),
            _ => anyhow::bail!("Unexpected checkout result"),
        }
    }
    pub(super) fn submit_review(
        &mut self,
        kind: ReviewKind,
        scope: ReviewScope,
        note: String,
        view: &Entity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.prepare_review(kind, scope, note);
        match result {
            Ok(ReviewAction::Prompt(prompt)) => {
                let receipt = self.dispatch(
                    ChatIntent::Send {
                        text: prompt,
                        images: vec![],
                    },
                    cx,
                );
                match receipt.state {
                    gitterm_gpui_chat::adapter::ReceiptState::Rejected(error) => {
                        view.update(cx, |view, cx| view.show_review_error(error, cx))
                    }
                    _ => {
                        view.update(cx, |view, cx| {
                            view.dismiss_review(window, cx);
                        });
                    }
                }
            }
            Ok(ReviewAction::Delegation(operation)) => {
                let Some(sender) = cx
                    .try_global::<crate::task_host::GpuiTaskHost>()
                    .map(|host| host.0.read(cx).host().command_sender())
                else {
                    view.update(cx, |view, cx| {
                        view.show_review_error("Live task host is unavailable".into(), cx)
                    });
                    return;
                };
                let caller = self.target().session_uid;
                let view = view.downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let result = sender.dispatch(*operation, Some(caller)).await;
                    let _ = view.update_in(cx, |view, window, cx| match result {
                        Ok(_) => {
                            view.dismiss_review(window, cx);
                        }
                        Err(error) => view.show_review_error(error, cx),
                    });
                })
                .detach();
            }
            Err(error) => view.update(cx, |view, cx| {
                view.show_review_error(format!("{error:#}"), cx)
            }),
        }
    }
    fn prepare_review(
        &self,
        kind: ReviewKind,
        scope: ReviewScope,
        note: String,
    ) -> Result<ReviewAction> {
        use gitterm_core::{review_request as r, task_mcp as m};
        anyhow::ensure!(
            self.controller.live,
            "Review / Consult is not available in preview"
        );
        anyhow::ensure!(
            self.controller.config.backend() == AgentBackend::Claude,
            "Review / Consult needs a Claude chat tab"
        );
        let reviewer = match scope.reviewer {
            gitterm_core::settings::ReviewerKind::Codex => r::PopoverReviewer::Codex,
            _ => r::PopoverReviewer::ClaudeSubagent,
        };
        let operation = match kind {
            ReviewKind::Review => {
                let target = match scope.target.context("Choose a review target")? {
                    ReviewTarget::Branch { base } => r::ReviewTarget::Branch { base },
                    ReviewTarget::Uncommitted => r::ReviewTarget::Uncommitted,
                    ReviewTarget::Commit { sha } => r::ReviewTarget::Commit { sha },
                };
                let request = r::ReviewRequest {
                    target,
                    focus: Some(note),
                    reviewer,
                    model: scope.model,
                };
                if reviewer == r::PopoverReviewer::ClaudeSubagent {
                    return r::review_prompt(&request)
                        .map(ReviewAction::Prompt)
                        .map_err(anyhow::Error::msg);
                }
                m::TaskControlOperation::RequestReview(
                    gitterm_core::delegations::review_request_from_popover(&request).into(),
                )
            }
            ReviewKind::Consult => {
                let request = r::ConsultRequest {
                    brief: note,
                    reviewer,
                    model: scope.model,
                };
                if reviewer == r::PopoverReviewer::ClaudeSubagent {
                    return r::consult_prompt(&request)
                        .map(ReviewAction::Prompt)
                        .map_err(anyhow::Error::msg);
                }
                m::TaskControlOperation::RequestConsult(m::ConsultDelegationRequest {
                    brief: request.brief,
                    user_model_choice: !request.model.trim().is_empty(),
                    model: Some(request.model),
                    ..Default::default()
                })
            }
        };
        Ok(ReviewAction::Delegation(Box::new(operation)))
    }
}
enum ReviewAction {
    Prompt(String),
    Delegation(Box<gitterm_core::task_mcp::TaskControlOperation>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::{
        desktop_store::DesktopStore, preview::service::ServiceContext, settings::ReviewerKind,
        workspace_config::WorkspaceConfig,
    };
    use gitterm_gpui_chat::adapter::ReceiptState;

    fn install(root: &std::path::Path, cx: &mut gpui::App) -> (ServiceContext, WorkspaceConfig) {
        let workspace: WorkspaceConfig = serde_json::from_value(serde_json::json!({"name":"Synthetic","dir":root,"abbrev":"S","color":"blue","tabs":[]})).unwrap();
        std::fs::write(root.join("workspaces.json"),serde_json::to_vec(&serde_json::json!({"active_workspace":0,"workspaces":[workspace.clone()],"future":true})).unwrap()).unwrap();
        let services = ServiceContext::production(root, "composer-test").unwrap();
        policy::install_live(services.clone(), cx).unwrap();
        cx.set_global(crate::workspace_config::LiveWorkspaces(
            DesktopStore::open(root).unwrap(),
        ));
        (services, workspace)
    }
    #[gpui::test]
    fn live_configured_and_remembered_model_effort_match_iced_json(cx: &mut gpui::TestAppContext) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        cx.background_executor.allow_parking();
        let (dir, preview) = super::super::tests::fixture();
        std::fs::write(dir.path().join("settings.json"),br#"{"chat":{"default_model":"sonnet","default_effort":"high","remember_last":true},"future":{"keep":42}}"#).unwrap();
        cx.update(|cx| {
            install(dir.path(), cx);
        });
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::new(
                RuntimeShutdown::new(),
                preview.clone(),
                dir.path().into(),
                window,
                cx,
            )
        });
        pane.update(cx,|pane,cx| {
            let selected = pane.deferred_live.as_ref().unwrap().settings.borrow().clone();
            assert_eq!(selected.model,"sonnet"); assert_eq!(selected.effort.as_deref(),Some("high"));
            let iced = live_config(dir.path().into(),&selected,vec![],false);
            assert!(iced.args().windows(2).any(|args| args == ["--model","sonnet"]));
            pane.remember_choice(SettingChange::Model("opus".into()),cx);
            pane.remember_choice(SettingChange::Effort(Some("max".into())),cx);
            cx.global::<crate::workspace_config::LiveWorkspaces>().0.drain().wait().unwrap();
            let settings = new_chat_selection(cx);
            assert_eq!(settings.model,"opus"); assert_eq!(settings.effort.as_deref(),Some("max"));
            let saved:serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap()).unwrap();
            assert_eq!(saved["future"]["keep"],42);
            let mut iced:gitterm_core::settings::ChatDefaults = serde_json::from_value(serde_json::json!({"default_model":"sonnet","default_effort":"high","remember_last":true})).unwrap();
            iced.remember_model("opus"); iced.remember_effort(Some("max"));
            assert_eq!(saved["chat"],serde_json::to_value(iced).unwrap());
        });
    }
    #[gpui::test]
    fn live_choose_checkout_changes_launch_cwd_and_same_v6_tab_fields(
        cx: &mut gpui::TestAppContext,
    ) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        cx.background_executor.allow_parking();
        let (dir, preview) = super::super::tests::fixture();
        let (services, workspace) = cx.update(|cx| install(dir.path(), cx));
        let previous = dir.path().join("previous");
        std::fs::create_dir(&previous).unwrap();
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::new(RuntimeShutdown::new(), preview, previous, window, cx)
        });
        pane.update(cx,|pane,cx| {
            let original = dir.path().to_path_buf();
            pane.live_tab = Some(resume::LiveTab::new(services,&workspace,serde_json::from_value(serde_json::json!({"dir":original,"session_uid":pane.saved.uid,"tab_kind":"agent","future_tab":true})).unwrap()));
            // The host's choices come from its task store, never from a path supplied by a page.
            pane.choose_checkout(original.clone(),cx).unwrap();
            assert_eq!(pane.saved.cwd,original);
            assert_eq!(live_config(pane.saved.cwd.clone(),&Default::default(),vec![],false).cwd,original);
            pane.shutdown.drain().unwrap();
            let saved:serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("workspaces.json")).unwrap()).unwrap();
            assert_eq!(saved["future"],true);
            assert_eq!(saved["workspaces"][0]["tabs"][0]["dir"],original.display().to_string());
            assert_eq!(saved["workspaces"][0]["tabs"][0]["repo_dir"],original.display().to_string());
            assert_eq!(saved["workspaces"][0]["tabs"][0]["future_tab"],true);
            assert!(pane.choose_checkout(dir.path().join("unknown"),cx).is_err());
            pane.controller.adapter.bind_session(Some("synthetic-session".into()));
            assert!(pane.choose_checkout(original,cx).unwrap_err().to_string().contains("locked"));
        });
    }
    #[gpui::test]
    fn live_review_consult_use_iced_prompts_and_delegation_ingress(cx: &mut gpui::TestAppContext) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        let (dir, preview) = super::super::tests::fixture();
        cx.update(|cx| {
            install(dir.path(), cx);
        });
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::new(
                RuntimeShutdown::new(),
                preview,
                dir.path().into(),
                window,
                cx,
            )
        });
        pane.read_with(cx, |pane, _| {
            use gitterm_core::{review_request as r, task_mcp::TaskControlOperation as Op};
            for kind in [ReviewKind::Review, ReviewKind::Consult] {
                for reviewer in [ReviewerKind::ClaudeSubagent, ReviewerKind::Codex] {
                    let scope = ReviewScope {
                        reviewer,
                        model: if reviewer == ReviewerKind::Codex {
                            "gpt-fixture"
                        } else {
                            "opus"
                        }
                        .into(),
                        target: Some(ReviewTarget::Uncommitted),
                    };
                    let action = pane
                        .prepare_review(kind, scope.clone(), "Synthetic focus".into())
                        .unwrap();
                    match action {
                        ReviewAction::Prompt(prompt) => {
                            let expected = if kind == ReviewKind::Review {
                                r::review_prompt(&r::ReviewRequest {
                                    target: r::ReviewTarget::Uncommitted,
                                    focus: Some("Synthetic focus".into()),
                                    reviewer: r::PopoverReviewer::ClaudeSubagent,
                                    model: scope.model,
                                })
                            } else {
                                r::consult_prompt(&r::ConsultRequest {
                                    brief: "Synthetic focus".into(),
                                    reviewer: r::PopoverReviewer::ClaudeSubagent,
                                    model: scope.model,
                                })
                            };
                            assert_eq!(prompt, expected.unwrap());
                        }
                        ReviewAction::Delegation(op) => match *op {
                            Op::RequestReview(request) => {
                                let expected: gitterm_core::task_mcp::ReviewDelegationRequest =
                                    gitterm_core::delegations::review_request_from_popover(
                                        &r::ReviewRequest {
                                            target: r::ReviewTarget::Uncommitted,
                                            focus: Some("Synthetic focus".into()),
                                            reviewer: r::PopoverReviewer::Codex,
                                            model: scope.model,
                                        },
                                    )
                                    .into();
                                assert_eq!(
                                    serde_json::to_value(request).unwrap(),
                                    serde_json::to_value(expected).unwrap()
                                );
                            }
                            Op::RequestConsult(request) => {
                                assert_eq!(request.brief, "Synthetic focus");
                                assert_eq!(request.model, Some(scope.model));
                                assert!(request.user_model_choice);
                                assert!(request.checkout.is_none());
                            }
                            _ => panic!("wrong ingress"),
                        },
                    }
                }
            }
        });
    }
    #[gpui::test]
    fn preview_refuses_composer_writes_launches_and_remember(cx: &mut gpui::TestAppContext) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        // Construct under Live to avoid a preview provider probe, then narrow this pane.
        let (dir, preview) = super::super::tests::fixture();
        cx.update(|cx| {
            install(dir.path(), cx);
        });
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::new(
                RuntimeShutdown::new(),
                preview,
                dir.path().into(),
                window,
                cx,
            )
        });
        pane.update(cx, |pane, cx| {
            pane.controller.live = false;
            assert!(
                pane.choose_checkout(dir.path().into(), cx)
                    .unwrap_err()
                    .to_string()
                    .contains("not available in preview")
            );
            assert!(
                pane.prepare_review(
                    ReviewKind::Consult,
                    ReviewScope {
                        reviewer: ReviewerKind::Codex,
                        model: String::new(),
                        target: None
                    },
                    "Synthetic".into()
                )
                .is_err()
            );
            pane.remember_choice(SettingChange::Model("opus".into()), cx);
            assert!(!dir.path().join("settings.json").exists());
            let (receipt, _) = pane.controller.dispatch(ChatIntent::Permission {
                request_id: "synthetic".into(),
                decision: gitterm_gpui_chat::PermissionDecision::AllowAlways {
                    suggestion: serde_json::json!({"destination":"localSettings"}),
                },
            });
            assert!(matches!(receipt.state, ReceiptState::Rejected(_)));
            assert!(pane.controller.runtime.is_none());
        });
    }
    #[gpui::test]
    fn live_new_chat_waits_for_earlier_remembered_selection(cx: &mut gpui::TestAppContext) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        cx.background_executor.allow_parking();
        let (dir, preview) = super::super::tests::fixture();
        std::fs::write(
            dir.path().join("settings.json"),
            br#"{"chat":{"default_model":"sonnet"}}"#,
        )
        .unwrap();
        cx.update(|cx| {
            install(dir.path(), cx);
        });
        let save = cx.update(|cx| {
            cx.global::<crate::workspace_config::LiveWorkspaces>()
                .0
                .edit_settings(move |settings| {
                    settings.chat.remember_model("opus");
                    settings.chat.remember_effort(Some("max"));
                })
        });
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::new(
                RuntimeShutdown::new(),
                preview,
                dir.path().into(),
                window,
                cx,
            )
        });
        save.wait().unwrap();
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(!pane.defaults_loading);
            let settings = pane.deferred_live.as_ref().unwrap().settings.borrow();
            assert_eq!(settings.model, "opus");
            assert_eq!(settings.effort.as_deref(), Some("max"));
        });
    }
    #[gpui::test]
    fn live_unsent_checkout_publishes_owner_keeps_choices_and_closes_without_resurrection(
        cx: &mut gpui::TestAppContext,
    ) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        cx.background_executor.allow_parking();
        let (dir, preview) = super::super::tests::fixture();
        cx.update(|cx| {
            install(dir.path(), cx);
        });
        let owner = cx.update(|cx| {
            let file =
                serde_json::from_slice(&std::fs::read(dir.path().join("workspaces.json")).unwrap())
                    .unwrap();
            let catalog = crate::workspace_config::IcedCatalog::from_document(dir.path(), file);
            let model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
            let mut metadata =
                crate::workspace_config::WorkspaceStore::new(&dir.path().join("layout"));
            metadata.iced = Some(catalog);
            cx.new(|cx| crate::owner::WorkspaceOwner::new(model, Some(metadata), cx))
        });
        let previous = dir.path().join("previous");
        std::fs::create_dir(&previous).unwrap();
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::new(RuntimeShutdown::new(), preview, previous, window, cx)
        });
        let uid = pane.read_with(cx, |pane, _| pane.saved.uid.clone());
        owner.update(cx, |owner, _| {
            owner.chats.insert(uid.clone(), pane.clone());
        });
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            pane.configure_prelaunch(SettingChange::Model("sonnet".into()), cx);
            pane.configure_prelaunch(SettingChange::Effort(Some("high".into())), cx);
            pane.choose_checkout(dir.path().into(), cx).unwrap();
            pane.shutdown.drain().unwrap();
        });
        cx.run_until_parked();
        owner.read_with(cx, |owner, _| {
            let saved = owner.iced_workspace(crate::data::WorkspaceId(1)).unwrap();
            let tab = saved
                .tabs
                .iter()
                .find(|tab| tab.session_uid.as_deref() == Some(&uid))
                .unwrap();
            assert!(tab.chat_session_id.is_none());
            assert_eq!(
                tab.agent_config.as_ref().unwrap().chat_model(),
                Some("sonnet")
            );
            assert_eq!(
                tab.agent_config.as_ref().unwrap().chat_effort(),
                Some("high")
            );
        });
        let outside = tempfile::tempdir_in("/private/tmp").unwrap();
        pane.update(cx, |pane, _| pane.saved.cwd = outside.path().into());
        cx.update(|_, cx| {
            let workspaces = &cx
                .global::<crate::work_sidebar::IcedWorkSource>()
                .workspaces;
            let owning = delegation_workspace(&uid, outside.path(), workspaces, cx).unwrap();
            assert_eq!(std::path::Path::new(&owning.dir), dir.path());
        });
        let target = pane.read_with(cx, |pane, _| pane.target());
        let close = owner
            .read_with(cx, |owner, cx| {
                owner.close_live_tab(
                    crate::data::WorkspaceId(1),
                    crate::data::SessionId(0),
                    &target,
                    cx,
                )
            })
            .unwrap()
            .expect("unsent tab must be in the owner's catalog");
        close.wait().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("workspaces.json")).unwrap())
                .unwrap();
        assert_eq!(saved["workspaces"][0]["tabs"], serde_json::json!([]));
    }
    #[gpui::test]
    fn live_restored_unsent_checkout_source_uses_owning_workspace(cx: &mut gpui::TestAppContext) {
        let _cleanup = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        cx.background_executor.allow_parking();
        let (dir, preview) = super::super::tests::fixture();
        let (services, workspace) = cx.update(|cx| install(dir.path(), cx));
        let worktree = tempfile::tempdir_in("/private/tmp").unwrap();
        let tab = resume::tab(worktree.path(), "synthetic-unsent".into(), None);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            ChatPane::resume(
                RuntimeShutdown::new(),
                preview,
                workspace,
                tab,
                None,
                window,
                cx,
            )
            .unwrap()
        });
        pane.read_with(cx, |pane, cx| {
            assert_eq!(pane.saved.cwd, worktree.path());
            let choices = pane.live_checkout_source().unwrap()(cx);
            assert_eq!(choices[0].path, dir.path().display().to_string());
            assert_eq!(
                pane.live_tab.as_ref().unwrap().services.state_root(),
                services.state_root()
            );
        });
    }
}
