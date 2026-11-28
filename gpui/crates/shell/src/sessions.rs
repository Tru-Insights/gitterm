//! Runtime identities belong to panes, independently of saved layout IDs.
use gitterm_core::session::{self, SavedIdentity, Target};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Removed(pub Target);

pub(crate) fn subscribe(
    pane: &gpui::Entity<crate::pane::Pane>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::Shell>,
) -> gpui::Subscription {
    cx.subscribe_in(pane, window, |shell, pane, event: &Removed, window, cx| {
        let missing = shell.workspaces.iter().any(|state| {
            state.store.terminals.is_some()
                && state.sessions.iter().any(|session| {
                    state.panes.get(&crate::session_pane_id(session.id)) == Some(pane)
                })
                && state
                    .dock
                    .read(cx)
                    .panel(gpui_component::dock::PanelId::from(pane.entity_id()))
                    .is_none()
        });
        if missing {
            shell.close_target(&event.0, window, cx);
        }
    })
}

pub(crate) fn fresh_target() -> Target {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let tab_id = NEXT.fetch_add(1, Ordering::Relaxed);
    let identity = SavedIdentity::default()
        .open(|| format!("gpui-{}-{tab_id}", std::process::id()))
        .identity;
    Target {
        tab_id,
        session_uid: identity.session_uid,
        task_id: identity.task_id,
        task_session_id: identity.task_session_id,
    }
}

pub(crate) fn can_close(current: &Target, requested: &Target, last: bool) -> bool {
    let state = session::State {
        target: current.clone(),
        control: session::TurnControl::Terminal,
        task_live: false,
        last_tab: last,
    };
    matches!(
        state.reduce(session::Intent::Close {
            target: requested.clone(),
            confirmed: false,
        }),
        session::Effect::Close(_)
    )
}

impl crate::Shell {
    pub(crate) fn close_target(
        &mut self,
        requested: &Target,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        self.close_target_with(requested, false, window, cx);
    }

    /// Iced's `CloseSession`: `confirmed` once its running-task prompt was
    /// answered (or a final worker closes), so it is not asked again.
    pub(crate) fn close_target_with(
        &mut self,
        requested: &Target,
        confirmed: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.close_target_saved(requested, confirmed, false, window, cx);
    }

    fn close_target_saved(
        &mut self,
        requested: &Target,
        confirmed: bool,
        persisted: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        let live = self.live_chat_enabled(cx);
        let Some(state) = self.workspaces.iter_mut().find(|state| {
            state
                .panes
                .values()
                .any(|pane| pane.read(cx).session.as_ref() == Some(requested))
        }) else {
            return;
        };
        let Some(id) = state.sessions.iter().find_map(|session| {
            let pane = state.panes.get(&crate::session_pane_id(session.id))?;
            (pane.read(cx).session.as_ref() == Some(requested)).then_some(session.id)
        }) else {
            return;
        };
        let pane_id = crate::session_pane_id(id);
        let pane = &state.panes[&pane_id];
        // Iced's running-task prompt (`TaskTabClose` → Stop and close / Keep open).
        if !confirmed
            && crate::task_panel::close_needs_confirm(
                pane.read(cx).session.as_ref().unwrap(),
                requested,
                state.sessions.len() <= 1,
                cx,
            )
        {
            // The prompt shows the pane it asks about; the strip's active
            // session follows, so a Stop and close lands from it.
            let workspace = state.workspace.id;
            self.show_session(workspace, id, window, cx);
            let panel = self.ensure_task_panel(window, cx);
            panel.update(cx, |panel, cx| {
                panel.open_stop_and_close(requested.clone(), cx)
            });
            return;
        }
        if !can_close(
            pane.read(cx).session.as_ref().unwrap(),
            requested,
            state.sessions.len() <= 1,
        ) {
            // Dock removal precedes the close decision; keep a protected last tab visible.
            let workspace = state.workspace.id;
            self.show_session(workspace, id, window, cx);
            return;
        }
        if !persisted && live {
            let workspace = state.workspace.id;
            let save = self
                .owner
                .read(cx)
                .close_live_tab(state.workspace.id, id, requested, cx);
            match save {
                Err(error) => {
                    self.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!("Close terminal: {error}"));
                        cx.notify();
                    });
                    self.show_session(workspace, id, window, cx);
                    return;
                }
                Ok(Some(save)) => {
                    self.owner
                        .update(cx, |owner, _| owner.workspace_save_pending = true);
                    let work = cx.background_executor().spawn(async move { save.wait() });
                    let requested = requested.clone();
                    cx.spawn_in(window, async move |shell, cx| {
                        let result = work.await;
                        _ = shell.update_in(cx, |shell, window, cx| {
                            shell
                                .owner
                                .update(cx, |owner, _| owner.workspace_save_pending = false);
                            match result {
                                Ok(saved) => {
                                    shell.owner.update(cx, |owner, cx| {
                                        owner.publish_workspace(saved, false, cx);
                                    });
                                    shell.close_target_saved(&requested, true, true, window, cx);
                                }
                                Err(error) => {
                                    shell.owner.update(cx, |owner, cx| {
                                        owner.error = Some(format!("Close terminal: {error}"));
                                        cx.notify();
                                    });
                                    shell.show_session(workspace, id, window, cx);
                                }
                            }
                        });
                    })
                    .detach();
                    return;
                }
                Ok(None) => {}
            }
        }
        // Iced's `close_task_tab`: a launched task session's close folds into
        // its task, then dropping the PTY hangs up its child and reaps it.
        if let (Some(session), Some(host)) = (
            requested.task_session_id.as_deref(),
            cx.try_global::<crate::task_host::GpuiTaskHost>()
                .map(|host| host.0.clone()),
        ) {
            host.update(cx, |host, cx| host.session_closed(session, cx));
        }
        let pane = &state.panes[&pane_id];
        pane.update(cx, |pane, cx| pane.stop_target(requested, cx));
        self.owner.update(cx, |owner, _| {
            owner.chats.remove(&requested.session_uid);
        });
        let pane = state.panes.remove(&pane_id).unwrap();
        self.registry.borrow_mut().remove(&pane_id);
        if let Some(terminals) = &mut state.store.terminals {
            terminals.retain(|terminal| terminal.id != id.0);
        }
        // Iced's `TabClose` keeps the closed tab's index, so the tab to its
        // right becomes active, or the last when it was the last.
        let closed_at = state.sessions.iter().position(|session| session.id == id);
        state.sessions.retain(|session| session.id != id);
        let was_active = state.active_session == Some(id);
        if was_active {
            state.active_session = closed_at
                .and_then(|at| state.sessions.get(at).or(state.sessions.last()))
                .map(|session| session.id);
        }
        state
            .dock
            .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        crate::save_layout(state, cx);
        let workspace = state.workspace.id;
        match requested.task_id.clone() {
            // Iced's `close_task_tab` lands on a sibling task tab, else on
            // the task rail with the task selected.
            Some(task) => self.land_after_task_close(workspace, task, window, cx),
            None if was_active => {
                if let Some(next) = self.workspace(workspace).active_session {
                    self.show_session(workspace, next, window, cx);
                }
            }
            None => {}
        }
        self.persist_remote_tabs(workspace, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Iced's `close_task_tab` landing in `workspace` after a pane of `task`
    /// closed: the first remaining pane of the task, in strip order;
    /// otherwise the first non-task pane, the Tasks rail and the task
    /// selected (its overview). The strip's active session and the pane the
    /// dock shows agree after either.
    fn land_after_task_close(
        &mut self,
        workspace: crate::data::WorkspaceId,
        task: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let state = self.workspace(workspace);
        let sibling = state
            .sessions
            .iter()
            .map(|session| session.id)
            .find(|id| self.session_task(*id, cx).as_deref() == Some(task.as_str()));
        let general = state
            .sessions
            .iter()
            .map(|session| session.id)
            .find(|id| self.session_task(*id, cx).is_none());
        if self.active_workspace(cx) == Some(workspace) {
            let first = state.sessions.first().map(|session| session.id);
            match (sibling, general, first) {
                (Some(sibling), _, _) => self.select_session_id(sibling, window, cx),
                (None, Some(_), _) => {
                    self.selected_task = Some(task);
                    self.general_context(true, window, cx);
                }
                // Only other tasks' panes are left: Iced lands on its first
                // tab (`unwrap_or(0)`), with the Tasks rail and the closed
                // task selected.
                (None, None, Some(first)) => {
                    self.select_session_id(first, window, cx);
                    self.selected_task = Some(task);
                    self.show_task_rail(cx);
                }
                (None, None, None) => {
                    self.selected_task = Some(task);
                    self.general_context(true, window, cx);
                }
            }
            return;
        }
        // Another workspace (a final worker's): Iced moves only its active tab.
        let next = sibling
            .or(general)
            .or_else(|| state.sessions.first().map(|session| session.id));
        if let Some(next) = next {
            self.show_session(workspace, next, window, cx);
        }
    }

    /// Iced's `FinalWorkerStopped` around `close_task_tab`: another tab
    /// that was active stays active, and the task selection stays when one
    /// did or the workspace is not the active one.
    pub(crate) fn close_final_worker(
        &mut self,
        target: &Target,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(workspace) = self.workspaces.iter().find(|state| {
            state
                .panes
                .values()
                .any(|pane| pane.read(cx).session.as_ref() == Some(target))
        }) else {
            return;
        };
        let workspace_id = workspace.workspace.id;
        let other_active = workspace.active_session.filter(|id| {
            workspace
                .panes
                .get(&crate::session_pane_id(*id))
                .is_none_or(|pane| pane.read(cx).session.as_ref() != Some(target))
        });
        let preserve_selection =
            other_active.is_some() || self.active_workspace(cx) != Some(workspace_id);
        let selection = self.selected_task.clone();
        self.close_target_with(target, true, window, cx);
        if let Some(active) = other_active
            && self
                .workspace(workspace_id)
                .sessions
                .iter()
                .any(|session| session.id == active)
        {
            self.show_session(workspace_id, active, window, cx);
        }
        if preserve_selection {
            self.selected_task = selection;
            self.remember_context(self.selected_task.clone());
        }
        cx.notify();
    }

    /// Makes session `id` of `workspace` its active session and the pane
    /// its dock shows: the active workspace's through `select_session_id`.
    fn show_session(
        &mut self,
        workspace: crate::data::WorkspaceId,
        id: crate::data::SessionId,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.active_workspace(cx) == Some(workspace) {
            self.select_session_id(id, window, cx);
        } else {
            self.workspace_mut(workspace).active_session = Some(id);
            self.show_workspace_pane(workspace, &crate::session_pane_id(id), window, cx);
        }
    }

    pub(crate) fn workspace(&self, id: crate::data::WorkspaceId) -> &crate::WorkspaceDock {
        self.workspaces
            .iter()
            .find(|state| state.workspace.id == id)
            .expect("closing workspace exists")
    }

    fn workspace_mut(&mut self, id: crate::data::WorkspaceId) -> &mut crate::WorkspaceDock {
        self.workspaces
            .iter_mut()
            .find(|state| state.workspace.id == id)
            .expect("closing workspace exists")
    }
}

impl crate::Shell {
    pub(crate) fn picker_inputs(
        &self,
        task: Option<crate::session_picker::TaskContext>,
        cx: &gpui::App,
    ) -> crate::session_picker::PickerInputs {
        crate::session_picker::PickerInputs {
            chat: gitterm_core::settings::ChatDefaults::default().new_chat_selection(),
            local_workspace: self.local,
            acp_chats: if self.live_chat_enabled(cx) {
                crate::chat::live_acp_catalog(&self.chat_defaults)
            } else {
                vec![]
            },
            presets: self.agent_presets.clone(),
            task,
        }
    }
    pub(crate) fn open_session_picker(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_picker(None, window, cx)
    }
    /// The session picker, over a task when `task` is set (Iced `TaskChildAdd`).
    pub(crate) fn open_picker(
        &mut self,
        task: Option<crate::session_picker::TaskContext>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gpui::AppContext as _;
        if self.active_workspace(cx).is_none()
            || self.park_sheet.is_some()
            || self.new_work_sheet.is_some()
            || self.settings_sheet.is_some()
        {
            return;
        }
        let inputs = self.picker_inputs(task, cx);
        let previous_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        let opening_workspace = self.active_workspace(cx);
        let picker = cx.new(|cx| crate::session_picker::SessionPicker::new(inputs.clone(), cx));
        self.picker_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            move |shell, picker, event: &crate::session_picker::SessionPickerEvent, window, cx| {
                let label = match event {
                    crate::session_picker::SessionPickerEvent::Selected(outcome) => {
                        picker.read(cx).outcome_label(outcome)
                    }
                    _ => None,
                };
                shell.session_picker = None;
                shell.picker_subscription = None;
                shell.cover_sheet(false, window, cx);
                previous_focus.focus(window, cx);
                if opening_workspace == shell.active_workspace(cx)
                    && let crate::session_picker::SessionPickerEvent::Selected(outcome) = event
                {
                    shell.select_picker_outcome(
                        outcome,
                        label
                            .as_deref()
                            .expect("selected picker row has its Iced label"),
                        window,
                        cx,
                    );
                }
                cx.notify();
            },
        ));
        self.cover_sheet(true, window, cx);
        picker.update(cx, |picker, cx| picker.open(inputs, window, cx));
        self.session_picker = Some(picker);
        cx.notify();
    }
    pub(crate) fn select_picker_outcome(
        &mut self,
        outcome: &crate::session_picker::SessionPickerOutcome,
        label: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::session_picker::SessionPickerOutcome as O;
        match outcome {
            O::NewChat if self.local => self.new_claude_chat(None, window, cx),
            O::AcpChat { id } if self.local && self.live_chat_enabled(cx) => {
                self.new_acp_chat(*id, window, cx)
            }
            O::Terminal if self.local => self.new_terminal(window, cx),
            O::Cli { preset } => self.launch_agent_preset(*preset, window, cx),
            O::CliResume { preset, .. } => self.launch_cli_preset(*preset, true, window, cx),
            O::Continue { task_id, preset } => self.launch_task_child(task_id, *preset, window, cx),
            _ => {
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some(format!("{label} · unavailable in preview"));
                    cx.notify();
                });
            }
        }
    }
    pub(crate) fn cover_sheet(
        &self,
        covered: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.content.update(cx, |content, cx| {
            content.picker = covered;
            content.sync_covers(window, cx);
        });
    }
    pub(crate) fn ensure_work_sidebars(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gitterm_core::work::state::{Inputs, Intent, Source};
        use gpui::AppContext as _;
        let mut created = vec![];
        for workspace in &self.workspaces {
            let id = workspace.workspace.id;
            if self.work_sidebars.contains_key(&id) {
                continue;
            }
            let dir = self.data.terminal_cwd(id, cx).unwrap_or_default();
            // TODO(TRU-274 Work source slice): share the owner's Work reducer and
            // index when available. Until then this is a fixture-free empty pane.
            self.work_state
                .borrow_mut()
                .reduce(Intent::SetInputs(Inputs {
                    workspace: id.0,
                    source: Source::Folder(dir.clone()),
                    tasks: vec![],
                    focus: Default::default(),
                    documents: None,
                }));
            created.push((id, dir.clone()));
            let facts = crate::work_sidebar::WorkspaceFacts {
                id: id.0,
                name: workspace.workspace.name.clone(),
                home: dir.clone(),
                dir,
                local: self.local,
                repos: vec![],
                removed: vec![],
                auto_added: vec![],
                open_chats: Default::default(),
                owned_chats: vec![],
                chat_owners: Default::default(),
                running_by_repo: Default::default(),
                selected_checkout: None,
            };
            let sidebar =
                cx.new(|_| crate::work_sidebar::WorkSidebar::new(facts, self.work_state.clone()));
            self.work_subscriptions.insert(
                id,
                cx.subscribe_in(
                    &sidebar,
                    window,
                    |shell, _, outcome: &crate::work_sidebar::WorkSidebarOutcome, window, cx| {
                        shell.work_outcome(outcome, window, cx);
                    },
                ),
            );
            self.work_sidebars.insert(id, sidebar);
        }
        for (id, dir) in created {
            self.ensure_agent_sidebar(id, dir, window, cx);
        }
    }
    /// Mark the branch row whose review is open; clear it once another viewer
    /// replaces it or the review closes.
    pub(crate) fn sync_review_selection(&mut self, cx: &mut gpui::Context<Self>) {
        for (id, sidebar) in &self.work_sidebars {
            let open = self.branch_reviews.get(id).filter(|checkout| {
                self.content.read(cx).is_visiting(
                    *id,
                    &format!("{}-branch-review", crate::content_pane_id(*id, checkout)),
                )
            });
            let open = open.cloned();
            sidebar.update(cx, |sidebar, cx| sidebar.set_review_checkout(open, cx));
        }
    }
    pub(crate) fn work_outcome(
        &mut self,
        outcome: &crate::work_sidebar::WorkSidebarOutcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::work_sidebar::WorkSidebarOutcome as O;
        match outcome {
            O::OpenChat { workspace, chat } => self.select_work_chat(*workspace, chat, window, cx),
            O::Focus { workspace, focus } => {
                self.select_workspace_id(crate::WorkspaceId(*workspace), window, cx);
                if let Some(dir) = self.data.terminal_cwd(crate::WorkspaceId(*workspace), cx) {
                    self.work_state.borrow_mut().reduce(
                        gitterm_core::work::state::Intent::SetInputs(
                            gitterm_core::work::state::Inputs {
                                workspace: *workspace,
                                source: gitterm_core::work::state::Source::Folder(dir),
                                tasks: vec![],
                                focus: focus.clone(),
                                documents: None,
                            },
                        ),
                    );
                }
                cx.notify();
            }
            O::OpenReview { checkout, vs_base } => {
                if let Some(active) = self.active_workspace(cx) {
                    self.branch_reviews.insert(active, checkout.clone());
                }
                self.open_work_review(checkout.clone(), *vs_base, window, cx);
            }
            O::OpenFind => {
                if let Some(active) = self.active_workspace(cx)
                    && let Some(state) = self
                        .workspaces
                        .iter_mut()
                        .find(|w| w.workspace.id == active)
                {
                    state.store.regions.sidebar_mode = crate::layout::SidebarMode::Find;
                    state.store.regions.sidebar_collapsed = false;
                }
                cx.notify();
            }
            O::OpenAgentConfig { workspace } => {
                self.show_agent_sidebar(crate::WorkspaceId(*workspace), true, cx)
            }
            O::OpenPage { url, store } => self.open_page(url.clone(), store.clone(), window, cx),
            // Write/launch controls stop at the sidebar's preview notice.
            _ => {}
        }
    }
    pub(crate) fn select_work_chat(
        &mut self,
        workspace: u64,
        chat: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let selected = self
            .workspaces
            .iter()
            .filter(|s| s.on_bar())
            .find_map(|state| {
                if state.workspace.id.0 != workspace {
                    return None;
                }
                state.sessions.iter().find_map(|session| {
                    let pane = state
                        .panes
                        .get(&crate::session_pane_id(session.id))?
                        .read(cx);
                    let crate::pane::PaneContent::PreviewChat(view) = &pane.content else {
                        return None;
                    };
                    view.read(cx)
                        .matches_chat(chat)
                        .then_some((state.workspace.id, session.id))
                })
            });
        if let Some((workspace, session)) = selected {
            self.select_workspace_id(workspace, window, cx);
            self.select_session_id(session, window, cx);
        } else if self.live_chat_enabled(cx) {
            if let Err(error) =
                self.open_saved_live_chat(crate::WorkspaceId(workspace), chat, None, window, cx)
            {
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some(format!("Resume chat: {error:#}"));
                    cx.notify();
                });
            }
        } else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Conversation resume not available in preview".into());
                cx.notify();
            });
        }
    }
}

impl crate::Shell {
    fn unavailable(&self, label: &str, cx: &mut gpui::Context<Self>) {
        self.owner.update(cx, |owner, cx| {
            owner.error = Some(format!("{label} · unavailable in preview"));
            cx.notify();
        });
    }
    pub(crate) fn bar_outcome(
        &mut self,
        outcome: crate::workspace_bar::Outcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::workspace_bar::Outcome as O;
        match outcome {
            O::SelectWorkspace(id) => self.pick_workspace_id(id, window, cx),
            O::NewWork => self.open_new_work_sheet(window, cx),
            O::ToggleTerminal if self.active_workspace(cx).is_some() => {
                self.open_bottom_terminal(window, cx)
            }
            O::OpenSettings(id) => self.open_settings_sheet(id, window, cx),
            O::OpenReposMenu => self.unavailable("Repos ▾", cx),
            O::OpenReady => self.open_ready_list(window, cx),
            O::OpenParked => self.open_parked_list(window, cx),
            O::Browser(_) => self.unavailable("Browser", cx),
            O::OpenMoreMenu => self.unavailable("Help and browser actions", cx),
            O::ToggleTerminal => self.unavailable("Terminal", cx),
            O::AddWorkspace(key) => {
                let machine = key.strip_prefix("agent:").map(str::to_string);
                self.open_folder_picker(
                    crate::workspace_sheet::FolderPickerPurpose::NewWorkspace,
                    machine,
                    window,
                    cx,
                )
            }
            O::OpenFolder => {
                let machine = self.active_machine(cx);
                self.open_folder_picker(
                    crate::workspace_sheet::FolderPickerPurpose::OpenFolder,
                    machine,
                    window,
                    cx,
                )
            }
            O::ShowMachineChats(key) => self.show_machine_chats(&key, window, cx),
            O::PairPhone(key) => self.open_phone_panel(&key, window, cx),
        }
    }
    pub(crate) fn spine_outcome(
        &mut self,
        outcome: crate::spine::Outcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::spine::Outcome as O;
        match outcome {
            O::SelectWorkspace(id) => self.pick_workspace_id(id, window, cx),
            O::Jump { workspace, target } => {
                self.select_workspace_id(workspace, window, cx);
                if self.active_workspace(cx) == Some(workspace) {
                    match target {
                        crate::spine::Target::Tab(id) => {
                            self.select_session_id(crate::data::SessionId(id), window, cx)
                        }
                        crate::spine::Target::Task(_) => self.unavailable("Task", cx),
                    }
                }
            }
            O::ToggleUsage => self.unavailable("Usage", cx),
        }
    }
    pub(crate) fn strip_outcome(
        &mut self,
        outcome: crate::session_strip::Outcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::session_strip::Outcome as O;
        match outcome {
            O::SelectSession(id) => self.select_session_id(id, window, cx),
            O::CloseSession(id) => {
                let target = self
                    .active(cx)
                    .panes
                    .get(&crate::session_pane_id(id))
                    .and_then(|p| p.read(cx).session.clone());
                if let Some(target) = target {
                    self.close_target(&target, window, cx);
                } else {
                    self.unavailable("Closing Iced sessions", cx);
                }
            }
            O::NewSession { workspace, .. } if self.active_workspace(cx) == Some(workspace) => {
                self.open_session_picker(window, cx)
            }
            O::CloseWorkspace(id) if self.active_workspace(cx) == Some(id) => {
                self.open_park_sheet(window, cx)
            }
            O::BackToWorkspace | O::TaskOverview => self.unavailable("Task overview", cx),
            O::ToggleWorktreeFollow => self.unavailable("Worktree follow", cx),
            _ => {}
        }
    }
    pub(crate) fn open_new_work_sheet(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        use gitterm_core::work::catalog::{
            NewWorkSheet, OpenWork, existing_work, new_work_recent_repos,
        };
        use gpui::AppContext as _;
        self.session_picker = None;
        self.picker_subscription = None;
        self.park_sheet = None;
        self.park_subscription = None;
        self.settings_sheet = None;
        self.settings_subscription = None;
        let owner = self.owner.read(cx);
        let saved = owner
            .iced_catalog()
            .map(|c| c.workspaces.as_slice())
            .unwrap_or_default();
        let order = owner
            .iced_catalog()
            .map(|c| c.ordered_indices())
            .unwrap_or_default();
        let open: Vec<_> = order
            .iter()
            .map(|&i| (i, &saved[i]))
            .filter(|(i, _)| self.workspaces[*i].parked.as_ref().is_none_or(|p| !p.done))
            .map(|(i, w)| OpenWork {
                uid: i as u64 + 1,
                name: &w.name,
                dir: &w.dir,
                chats: &w.chats,
                claims_new_chats: owner.iced_catalog().is_some_and(|c| c.claims.contains(&i)),
                running_park: self.workspaces[i].parked.is_some(),
            })
            .collect();
        let closed = owner
            .iced_catalog()
            .map(|c| c.closed.as_slice())
            .unwrap_or_default();
        let closed: Vec<_> = closed
            .iter()
            .filter(|w| {
                !saved.iter().enumerate().any(|(i, open)| {
                    open.name == w.name
                        && open.dir == w.dir
                        && self.workspaces[i].parked.as_ref().is_none_or(|p| !p.done)
                })
            })
            .map(|w| gitterm_core::work::catalog::ClosedWork {
                name: &w.name,
                dir: &w.dir,
                chats: &w.chats,
                active: w.active,
                parked_at: w.parked.as_ref().map(|p| p.parked_at.as_str()),
                done_at: w.done_at.as_deref(),
            })
            .collect();
        let existing = existing_work(&open, &closed);
        let candidates = self.recent_repo_candidates(cx);
        let draft = NewWorkSheet {
            recent: new_work_recent_repos(candidates, |p| p.join(".git").is_dir()),
            ..Default::default()
        };
        let previous_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        let sheet = cx.new(|cx| {
            crate::workspace_sheet::WorkspaceSheet::new(
                draft,
                existing,
                dirs::home_dir(),
                window,
                cx,
            )
        });
        self.new_work_subscription = Some(cx.subscribe_in(
            &sheet,
            window,
            move |shell, _, outcome: &crate::workspace_sheet::WorkspaceSheetOutcome, window, cx| {
                shell.new_work_outcome(outcome, window, cx);
                if shell.new_work_sheet.is_none() {
                    previous_focus.focus(window, cx);
                }
            },
        ));
        self.new_work_sheet = Some(sheet);
        self.cover_sheet(true, window, cx);
        cx.notify();
    }
    pub(crate) fn new_work_outcome(
        &mut self,
        outcome: &crate::workspace_sheet::WorkspaceSheetOutcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        use crate::workspace_sheet::WorkspaceSheetOutcome as O;
        use gitterm_core::work::catalog::ExistingWorkAction as A;
        match outcome {
            O::Create { name, start }
                if cx
                    .try_global::<crate::workspace_config::LiveWorkspaces>()
                    .is_some() =>
            {
                if let Some(home) = dirs::home_dir() {
                    self.change_live_workspace(
                        crate::workspace_config::LiveWorkspaceAction::Create {
                            name: name.clone(),
                            start: start.clone(),
                            home,
                        },
                        WorkspaceCompletion::Open { created: true },
                        window,
                        cx,
                    );
                } else if let Some(sheet) = &self.new_work_sheet {
                    sheet.update(cx, |sheet, cx| {
                        sheet.show_error("no home directory to put ~/Work in".into(), cx)
                    });
                }
            }
            O::Create { .. } => {
                if let Some(sheet) = &self.new_work_sheet {
                    sheet.update(cx, |sheet, cx| {
                        sheet.show_error("Creating work isn't available in the preview".into(), cx)
                    });
                }
            }
            O::OpenExisting {
                id: A::Resume(key) | A::Reopen(key),
            } if cx
                .try_global::<crate::workspace_config::LiveWorkspaces>()
                .is_some() =>
            {
                let reopen = matches!(outcome, O::OpenExisting { id: A::Reopen(_) });
                self.resume_live_workspace(key.clone(), reopen, window, cx);
            }
            O::OpenExisting { id } => {
                let id = match id {
                    A::Switch(id) => Some(crate::WorkspaceId(*id)),
                    A::Resume(key) | A::Reopen(key) => self
                        .workspaces
                        .iter()
                        .find(|w| {
                            w.workspace.name == key.name
                                && self
                                    .data
                                    .terminal_cwd(w.workspace.id, cx)
                                    .is_some_and(|d| d == std::path::Path::new(&key.dir))
                        })
                        .map(|w| w.workspace.id),
                };
                if let Some(id) = id.filter(|id| {
                    self.workspaces
                        .iter()
                        .any(|w| w.workspace.id == *id && w.on_bar())
                }) {
                    self.select_workspace_id(id, window, cx);
                } else if let Some(sheet) = &self.new_work_sheet {
                    sheet.update(cx, |sheet, cx| {
                        sheet.show_error("This work is unavailable in preview".into(), cx)
                    });
                }
            }
            O::Cancel => {
                self.new_work_sheet = None;
                self.new_work_subscription = None;
                self.cover_sheet(false, window, cx);
            }
            O::PickFolder { purpose } => self.open_folder_picker(*purpose, None, window, cx),
        }
        cx.notify();
    }

    fn load_iced_strip_titles(&self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(catalog) = self.owner.read(cx).iced_catalog() else {
            return;
        };
        let config = catalog.source.clone();
        let mut bindings = vec![];
        let mut conversations = vec![];
        for state in &self.workspaces {
            let Some(saved) = self.owner.read(cx).iced_workspace(state.workspace.id) else {
                continue;
            };
            if saved.is_legacy_remote()
                || matches!(
                    saved.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
                )
            {
                continue;
            }
            conversations.extend(saved.chats.iter().cloned());
            for (session, tab) in self
                .owner
                .read(cx)
                .iced_sessions(saved)
                .iter()
                .zip(&saved.tabs)
            {
                if tab.tab_kind.as_deref() != Some("agent") {
                    continue;
                }
                let Some(conversation) = &tab.chat_session_id else {
                    continue;
                };
                conversations.push(conversation.clone());
                let Some(current) = state.sessions.iter().find(|s| s.id == session.id) else {
                    continue;
                };
                if let Some(pane) = state.panes.get(&crate::session_pane_id(session.id)) {
                    bindings.push((
                        state.workspace.id,
                        session.id,
                        pane.clone(),
                        current.title.clone(),
                        conversation.clone(),
                    ));
                }
            }
        }
        if bindings.is_empty() {
            return;
        }
        // Headless tests only use explicitly supplied synthetic transcript homes.
        #[cfg(test)]
        let home: Option<std::path::PathBuf> = None;
        #[cfg(not(test))]
        let home = dirs::home_dir();
        cx.spawn_in(window, async move |shell, cx| {
            let titles = cx
                .background_executor()
                .spawn(
                    async move { read_iced_strip_titles(&config, home.as_deref(), &conversations) },
                )
                .await;
            _ = shell.update_in(cx, |shell, _, cx| {
                for (workspace, id, pane, previous, conversation) in bindings {
                    let Some(title) = titles.get(&conversation) else {
                        continue;
                    };
                    let Some(state) = shell
                        .workspaces
                        .iter_mut()
                        .find(|s| s.workspace.id == workspace)
                    else {
                        continue;
                    };
                    // A closed/replaced pane or newer title must not receive stale work.
                    if state.panes.get(&crate::session_pane_id(id)) != Some(&pane) {
                        continue;
                    }
                    if let Some(session) = state.sessions.iter_mut().find(|s| s.id == id)
                        && session.title == previous
                    {
                        session.title = title.clone();
                        pane.update(cx, |pane, cx| {
                            pane.title = title.clone();
                            pane.notification_chat_title = Some(title.clone());
                            cx.notify();
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn load_active_iced_history(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        // Labels belong to every open tab, independently of history availability.
        self.load_iced_strip_titles(window, cx);
        if self.live_chat_enabled(cx) {
            // Iced restores every open agent tab. Register background panes too,
            // so delivery can reach them before they are ever selected. Resume
            // loads ACP history only for the selected tab.
            let selected = self.active_workspace(cx).and_then(|workspace| {
                let state = self
                    .workspaces
                    .iter()
                    .find(|state| state.workspace.id == workspace)?;
                Some((workspace, state.active_session?))
            });
            let mut tabs: Vec<_> = self
                .workspaces
                .iter()
                .flat_map(|state| {
                    state.sessions.iter().filter_map(|session| {
                        let pane = state.panes.get(&crate::session_pane_id(session.id))?;
                        if !matches!(
                            pane.read(cx).content,
                            crate::pane::PaneContent::IcedSession(_)
                        ) {
                            return None;
                        }
                        let tab = self
                            .data
                            .imported_chat(state.workspace.id, session.id, cx)?;
                        Some((state.workspace.id, session.id, tab))
                    })
                })
                .collect();
            // A saved conversation can appear in more than one tab. Keep the
            // selected tab's existing priority when resume reuses its pane.
            tabs.sort_by_key(|(workspace, id, _)| Some((*workspace, *id)) != selected);
            for (workspace, id, tab) in tabs {
                if let Err(error) =
                    self.mount_live_chat(workspace, tab, Some(id), false, window, cx)
                {
                    self.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!(
                            "Resume chat in workspace {}: {error:#}",
                            workspace.0
                        ));
                        cx.notify();
                    });
                }
            }
            if let Some((workspace, id)) = selected
                && let Some(pane) = self
                    .workspace(workspace)
                    .panes
                    .get(&crate::session_pane_id(id))
                && let crate::pane::PaneContent::PreviewChat(chat) = &pane.read(cx).content
            {
                chat.clone()
                    .update(cx, |chat, cx| chat.load_acp_history(cx));
            }
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let Some(state) = self.workspaces.iter().find(|w| w.workspace.id == workspace) else {
            return;
        };
        let Some(id) = state.active_session else {
            return;
        };
        let Some(tab) = self.data.imported_chat(workspace, id, cx) else {
            return;
        };
        let pane = state.panes[&crate::session_pane_id(id)].clone();
        if !matches!(
            pane.read(cx).content,
            crate::pane::PaneContent::IcedSession(_)
        ) {
            return;
        }
        // Read the task store with the transcript: the shell's roster facts may
        // not be loaded yet when a chat opens at launch.
        let card_source = self.owner.read(cx).iced_catalog().map(|catalog| {
            let source = catalog.source.clone();
            let tasks = crate::data::TaskSource::resolve(&source, cx);
            (source, tasks, tab.clone())
        });
        let Some(conversation) = tab.chat_session_id else {
            return;
        };
        let saved = self
            .owner
            .read(cx)
            .chat_history(Some(std::path::Path::new(&tab.dir)))
            .into_iter()
            .find(|s| s.session_id.as_deref() == Some(&conversation));
        let provider = self
            .owner
            .read(cx)
            .preview()
            .map(|p| p.provider())
            .unwrap_or_else(|| std::path::PathBuf::from("/unavailable"));
        let shutdown = self.owner.read(cx).chat_shutdown.clone();
        // Headless tests must never inspect the user's harness directories.
        #[cfg(test)]
        let projects: Option<std::path::PathBuf> = None;
        #[cfg(not(test))]
        let projects = dirs::home_dir().map(|home| home.join(".claude/projects"));
        cx.spawn_in(window, async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    saved.map(Ok).unwrap_or_else(|| {
                        let projects =
                            projects.ok_or_else(|| "transcript source unavailable".to_string())?;
                        let delegations = card_source
                            .and_then(|(source, tasks, tab)| {
                                match crate::data::RosterFacts::read(&source, &tasks) {
                                    Ok(facts) => Some(chat_delegation_times(&facts, &tab)),
                                    Err(error) => {
                                        // The chat still opens; its cards go at the end.
                                        eprintln!("read Iced delegations for history: {error:#}");
                                        None
                                    }
                                }
                            })
                            .unwrap_or_default();
                        read_iced_history(
                            &projects,
                            &conversation,
                            &tab.dir,
                            provider,
                            &delegations,
                        )
                    })
                })
                .await;
            _ = shell.update_in(cx, |shell, window, cx| {
                let Some(state) = shell
                    .workspaces
                    .iter_mut()
                    .find(|w| w.workspace.id == workspace)
                else {
                    return;
                };
                if state.panes.get(&crate::session_pane_id(id)) != Some(&pane)
                    || !matches!(
                        pane.read(cx).content,
                        crate::pane::PaneContent::IcedSession(_)
                    )
                {
                    return;
                }
                match result {
                    Ok(saved) => {
                        use gpui::AppContext as _;
                        let view = cx
                            .new(|cx| crate::chat::ChatPane::history(shutdown, saved, window, cx));
                        // Imported history has no live target and is never registered
                        // as a preview-owned runtime or persisted as a preview chat.
                        pane.update(cx, |p, cx| {
                            p.content = crate::pane::PaneContent::PreviewChat(view);
                            cx.notify();
                        });
                    }
                    Err(error) => pane.update(cx, |p, cx| {
                        p.content = crate::pane::PaneContent::IcedSession(format!(
                            "{}\nhistory unavailable in preview\n{error}",
                            p.title
                        ));
                        cx.notify();
                    }),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Iced's `delegation_times_for_tab`: the chat's delegations as
/// (id, created_at), oldest first, matched to their parent tab by core's rule.
fn chat_delegation_times(
    facts: &crate::data::RosterFacts,
    tab: &gitterm_core::workspace_config::WorkspaceTabConfig,
) -> Vec<(String, String)> {
    let Some(this) = facts
        .tabs
        .iter()
        .position(|t| t.session_uid == tab.session_uid && t.chat_session_id == tab.chat_session_id)
    else {
        return Vec::new();
    };
    let tabs = facts.tabs.iter().enumerate().filter_map(|(index, t)| {
        (t.tab_kind.as_deref() == Some("agent")).then_some((
            index,
            t.session_uid.as_deref()?,
            t.chat_session_id.as_deref(),
            false,
        ))
    });
    facts
        .workers
        .iter()
        .rev()
        .filter(|d| gitterm_core::delegations::parent_tab_id(&d.parent, tabs.clone()) == Some(this))
        .map(|d| (d.delegation_id.clone(), d.created_at.clone()))
        .collect()
}

/// Use Iced's index rules (generated title, summary/first real user message),
/// even for tabs whose history has never been opened or cannot be imported.
fn read_iced_strip_titles(
    config: &std::path::Path,
    home: Option<&std::path::Path>,
    conversations: &[String],
) -> std::collections::HashMap<String, String> {
    use gitterm_core::{chat_facts, chats::ChatBackend};
    let entries = home
        .map(|home| crate::workspace_config::read_iced_chat_index(config, home))
        .unwrap_or_default();
    let mut titles = std::collections::HashMap::new();
    let mut add_children = |facts: &chat_facts::ChatFacts| {
        for child in &facts.children {
            if let Some(title) = child
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                titles
                    .entry(child.session_id.clone())
                    .or_insert_with(|| title.to_owned());
            }
        }
    };
    for facts in entries.iter().filter_map(|entry| entry.facts.as_ref()) {
        add_children(facts);
    }
    // Facts can name children whose transcript is absent. A facts file has no
    // title for its own chat; never mistake issue keys or a UUID for that title.
    for conversation in conversations {
        for backend in [ChatBackend::Claude, ChatBackend::Pi, ChatBackend::Codex] {
            let facts =
                chat_facts::facts_path(&chat_facts::facts_dir(config), backend, conversation)
                    .and_then(|path| chat_facts::load_facts(&path));
            match facts {
                Ok(Some(facts)) => add_children(&facts),
                Ok(None) => {}
                Err(error) => eprintln!("[strip-titles] {error}"),
            }
        }
    }
    // A conversation's own indexed title wins over a parent's recorded brief.
    for entry in entries {
        titles.insert(entry.id, entry.title);
    }
    titles
}

fn read_iced_history(
    projects: &std::path::Path,
    conversation: &str,
    cwd: &str,
    provider: std::path::PathBuf,
    delegations: &[(String, String)],
) -> Result<crate::chat::SavedChat, String> {
    use gitterm_core::harness::transcript::TranscriptEntry;
    use gitterm_gpui_chat::events::{Entry, Record};
    // Reuse the core RFC3339 conversion so this importer follows history_timeline.
    let timestamp_ms = |at: Option<&str>| {
        let timed = gitterm_core::harness::transcript::TimedEntry {
            at: at.map(str::to_string),
            entry: TranscriptEntry::Harness(
                gitterm_core::chat::types::HarnessEvent::TurnCompleted {
                    status: gitterm_core::chat::types::TurnStatus::Completed,
                    usage: serde_json::Value::Null,
                    cost_usd: None,
                    rate_limit: None,
                },
            ),
        };
        gitterm_core::chat::history::history_timeline(vec![timed], &[])
            .into_iter()
            .find_map(|event| match event {
                gitterm_core::chat::history::HistoryEvent::Other(value)
                    if value["kind"] == "turn_timing" =>
                {
                    value["end_ms"]
                        .as_i64()
                        .and_then(|ms| u64::try_from(ms).ok())
                }
                _ => None,
            })
    };
    if conversation.is_empty()
        || !conversation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Invalid conversation identity".into());
    }
    let entries =
        std::fs::read_dir(projects).map_err(|e| format!("Read transcript projects: {e}"))?;
    let path = entries
        .filter_map(Result::ok)
        .map(|e| e.path().join(format!("{conversation}.jsonl")))
        .find(|p| p.is_file())
        .ok_or_else(|| "Transcript not found".to_string())?;
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("Read transcript {}: {e}", path.display()))?;
    let mut records = Vec::new();
    let mut last_activity_ms = None;
    let timeline = gitterm_core::chat::history::parse_claude_transcript_timed(&source);
    // Iced's history_timeline rule: each card goes back where it was requested;
    // a card with no place stays out here and lands at the end.
    let mut anchors = gitterm_core::chat::history::history_anchor_positions(&timeline, delegations)
        .into_iter()
        .peekable();
    let anchor = |id: String| Record {
        at_ms: None,
        entry: Entry::DisplayDelegation {
            id,
            title: String::new(),
            state: "requested".into(),
            body: String::new(),
        },
    };
    for (index, timed) in timeline.into_iter().enumerate() {
        while let Some((_, id)) = anchors.next_if(|(at, _)| *at == index) {
            records.push(anchor(id));
        }
        let at_ms = timestamp_ms(timed.at.as_deref());
        let entry = match timed.entry {
            TranscriptEntry::UserPrompt(text) => {
                last_activity_ms = None;
                Entry::UserPrompt(text)
            }
            TranscriptEntry::Harness(event) => {
                if matches!(
                    event,
                    gitterm_core::chat::types::HarnessEvent::TurnCompleted { .. }
                ) {
                    if let Some(end_ms) = last_activity_ms.take() {
                        records.push(Record {
                            at_ms: None,
                            entry: Entry::TurnTiming {
                                start_ms: None,
                                end_ms: Some(end_ms),
                                split_ms: None,
                            },
                        });
                    }
                } else if at_ms.is_some() {
                    last_activity_ms = at_ms;
                }
                Entry::Harness(event)
            }
        };
        let is_prompt = matches!(entry, Entry::UserPrompt(_));
        records.push(Record { at_ms, entry });
        // Match history_timeline: start timing markers follow their prompt.
        if is_prompt && let Some(start_ms) = at_ms {
            records.push(Record {
                at_ms: None,
                entry: Entry::TurnTiming {
                    start_ms: Some(start_ms),
                    end_ms: None,
                    split_ms: None,
                },
            });
        }
    }
    // A transcript with no usable entry is an empty chat, as in Iced.
    records.extend(anchors.map(|(_, id)| anchor(id)));
    Ok(crate::chat::SavedChat {
        uid: format!("iced-{conversation}"),
        cwd: cwd.into(),
        provider,
        session_id: Some(conversation.into()),
        records,
    })
}

impl crate::Shell {
    pub(crate) fn live_chat_enabled(&self, cx: &gpui::App) -> bool {
        self.owner.read(cx).preview().is_some_and(|preview| {
            matches!(
                crate::chat::policy::current(preview, cx),
                crate::chat::policy::ChatLaunchPolicy::Live { .. }
            )
        })
    }

    fn live_chat_location(
        &self,
        id: &str,
        cx: &gpui::App,
    ) -> Option<(crate::WorkspaceId, crate::data::SessionId)> {
        self.workspaces.iter().find_map(|state| {
            state.sessions.iter().find_map(|session| {
                let pane = state
                    .panes
                    .get(&crate::session_pane_id(session.id))?
                    .read(cx);
                match &pane.content {
                    crate::pane::PaneContent::PreviewChat(view)
                        if view.read(cx).matches_chat(id) =>
                    {
                        Some((state.workspace.id, session.id))
                    }
                    _ => None,
                }
            })
        })
    }

    /// One mounting path for restored tabs and saved conversations opened by either rail.
    fn mount_live_chat(
        &mut self,
        workspace: crate::WorkspaceId,
        tab: gitterm_core::workspace_config::WorkspaceTabConfig,
        existing: Option<crate::data::SessionId>,
        activate: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> anyhow::Result<()> {
        use gpui::AppContext as _;
        if let Some(id) = &tab.chat_session_id
            && let Some((workspace, session)) = self.live_chat_location(id, cx)
        {
            if activate {
                self.reopen_workspace(workspace, window, cx);
                self.select_workspace_id(workspace, window, cx);
                self.select_session_id(session, window, cx);
            }
            return Ok(());
        }
        let owner = self.owner.read(cx);
        let saved = owner
            .iced_workspace(workspace)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Saved chat workspace unavailable"))?;
        anyhow::ensure!(
            !saved.is_legacy_remote()
                && !matches!(
                    saved.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
                ),
            "Remote chat resume unavailable"
        );
        let preview = owner
            .preview()
            .ok_or_else(|| anyhow::anyhow!("Chat context unavailable"))?;
        let shutdown = owner.chat_shutdown.clone();
        let fallback = crate::chat::SavedChat {
            uid: fresh_target().session_uid,
            cwd: tab.dir.clone().into(),
            provider: preview.provider(),
            session_id: tab.chat_session_id.clone(),
            records: vec![],
        };
        let mut error = None;
        let view = cx.new(|cx| {
            match crate::chat::ChatPane::resume(shutdown.clone(), preview, saved, tab, window, cx) {
                Ok(pane) => pane,
                Err(failure) => {
                    error = Some(failure);
                    crate::chat::ChatPane::history(shutdown, fallback, window, cx)
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        self.install_live_pane(workspace, view, existing, activate, window, cx);
        Ok(())
    }

    fn install_live_pane(
        &mut self,
        workspace: crate::WorkspaceId,
        view: gpui::Entity<crate::chat::ChatPane>,
        existing: Option<crate::data::SessionId>,
        activate: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gpui::AppContext as _;
        let target = view.read(cx).target();
        let title = view.read(cx).backend_label();
        self._subscriptions
            .push(crate::task_host::forward_turn_ends(&view, cx));
        self.owner.update(cx, |owner, _| {
            owner.chats.insert(target.session_uid.clone(), view.clone());
        });
        let id = existing.unwrap_or_else(|| {
            let mut id = crate::layout::stable_path_hash(std::path::Path::new(&target.session_uid));
            while self
                .workspaces
                .iter()
                .any(|w| w.sessions.iter().any(|s| s.id.0 == id))
            {
                id = id.wrapping_add(1);
            }
            crate::data::SessionId(id)
        });
        let pane_id = crate::session_pane_id(id);
        if let Some(pane) = self.workspace(workspace).panes.get(&pane_id) {
            pane.update(cx, |pane, cx| {
                pane.content = crate::pane::PaneContent::PreviewChat(view);
                pane.session = Some(target.clone());
                cx.notify();
            });
        } else {
            let pane = cx.new(|cx| {
                crate::pane::Pane::new(
                    pane_id.clone(),
                    title.clone(),
                    crate::pane::PaneKind::PreviewChat(view),
                    workspace,
                    self.data.clone(),
                    window,
                    cx,
                )
            });
            self._subscriptions.push(subscribe(&pane, window, cx));
            self.registry
                .borrow_mut()
                .insert(pane_id.clone(), pane.downgrade());
            let state = self
                .workspaces
                .iter_mut()
                .find(|state| state.workspace.id == workspace)
                .expect("selected workspace");
            state.panes.insert(pane_id.clone(), pane);
            state.sessions.push(crate::data::Session {
                id,
                title,
                kind: crate::data::SessionKind::Chat,
            });
        }
        if activate {
            self.reopen_workspace(workspace, window, cx);
            self.select_workspace_id(workspace, window, cx);
            self.select_session_id(id, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn open_saved_live_chat(
        &mut self,
        workspace: crate::WorkspaceId,
        id: &str,
        indexed: Option<gitterm_core::chats::ChatIndexEntry>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.live_chat_enabled(cx),
            "Conversation resume not available in preview"
        );
        if let Some((workspace, session)) = self.live_chat_location(id, cx) {
            self.reopen_workspace(workspace, window, cx);
            self.select_workspace_id(workspace, window, cx);
            self.select_session_id(session, window, cx);
            return Ok(());
        }
        let saved = self
            .owner
            .read(cx)
            .iced_workspace(workspace)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Saved chat workspace unavailable"))?;
        if let Some(tab) = saved
            .tabs
            .iter()
            .find(|tab| tab.chat_session_id.as_deref() == Some(id))
            .cloned()
        {
            let existing = self
                .owner
                .read(cx)
                .iced_sessions(&saved)
                .iter()
                .zip(&saved.tabs)
                .find(|(_, tab)| tab.chat_session_id.as_deref() == Some(id))
                .map(|(session, _)| session.id)
                .filter(|id| {
                    self.workspace(workspace)
                        .panes
                        .get(&crate::session_pane_id(*id))
                        .is_some_and(|pane| {
                            matches!(
                                pane.read(cx).content,
                                crate::pane::PaneContent::IcedSession(_)
                            )
                        })
                });
            return self.mount_live_chat(workspace, tab, existing, true, window, cx);
        }
        // A closed tab can still carry the recorded configuration in another
        // workspace. Prefer it over the index's backend/cwd-only metadata.
        let recorded = self.owner.read(cx).iced_catalog().and_then(|catalog| catalog.workspaces.iter().enumerate().find_map(|(index, workspace)| {
            if workspace.is_legacy_remote() || matches!(workspace.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })) { return None; }
            workspace.tabs.iter().find(|tab| tab.chat_session_id.as_deref() == Some(id)).cloned().map(|tab| (crate::WorkspaceId(index as u64 + 1), tab))
        }));
        if let Some((workspace, tab)) = recorded {
            return self.mount_live_chat(workspace, tab, None, true, window, cx);
        }
        let indexed = indexed.or_else(|| {
            self.work_sidebars.get(&workspace).and_then(|sidebar| {
                sidebar
                    .read(cx)
                    .indexed_chats()
                    .iter()
                    .find(|entry| entry.id == id)
                    .cloned()
            })
        });
        let indexed = indexed
            .ok_or_else(|| anyhow::anyhow!("Saved conversation metadata unavailable: {id}"))?;
        anyhow::ensure!(
            indexed.backend == gitterm_core::chats::ChatBackend::Claude,
            "Resume for this recorded backend is unavailable"
        );
        let tab =
            crate::chat::resume::tab(&indexed.cwd, fresh_target().session_uid, Some(id.into()));
        self.mount_live_chat(workspace, tab, None, true, window, cx)
    }
}

pub(crate) enum WorkspaceCompletion {
    Park(gitterm_core::park::Effect),
    Open { created: bool },
}

impl crate::Shell {
    pub(crate) fn open_recent_work_sheet(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_new_work_sheet(window, cx);
    }
}

impl crate::Shell {
    pub(crate) fn change_live_workspace(
        &mut self,
        action: crate::workspace_config::LiveWorkspaceAction,
        completion: WorkspaceCompletion,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(store) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|s| s.0.clone())
        else {
            return;
        };
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        self.owner
            .update(cx, |owner, _| owner.workspace_save_pending = true);
        let selected = match &completion {
            WorkspaceCompletion::Open { .. }
            | WorkspaceCompletion::Park(gitterm_core::park::Effect::Return {
                activate: true,
                ..
            }) => None,
            WorkspaceCompletion::Park(_) => self
                .workspaces
                .iter()
                .find(|w| Some(w.workspace.id) == self.active_workspace(cx) && w.on_bar())
                .map(|w| gitterm_core::park::Key {
                    name: w.workspace.name.clone(),
                    dir: self
                        .owner
                        .read(cx)
                        .iced_workspace(w.workspace.id)
                        .map(|w| w.dir.clone())
                        .unwrap_or_default(),
                }),
        };
        let save = cx.background_executor().spawn(async move {
            crate::workspace_config::save_live_workspace(&store, action, selected)
        });
        cx.spawn_in(window, async move |shell, cx| {
            let saved = save.await;
            let _ = shell.update_in(cx, |shell, window, cx| {
                shell
                    .owner
                    .update(cx, |owner, _| owner.workspace_save_pending = false);
                match saved {
                    Err(error) => {
                        if let Some(sheet) = &shell.new_work_sheet {
                            sheet.update(cx, |sheet, cx| sheet.show_error(error.clone(), cx));
                        }
                        shell.owner.update(cx, |owner, cx| {
                            owner.error = Some(format!("Save workspace: {error}"));
                            cx.notify();
                        });
                    }
                    Ok(saved) => {
                        let claims_new_chats =
                            matches!(&completion, WorkspaceCompletion::Open { created: true });
                        let id = shell.owner.update(cx, |owner, cx| {
                            owner.publish_workspace(saved, claims_new_chats, cx)
                        });
                        match completion {
                            WorkspaceCompletion::Park(effect) => {
                                let close_sheet = matches!(
                                    effect,
                                    gitterm_core::park::Effect::Park { .. }
                                        | gitterm_core::park::Effect::Close { .. }
                                        | gitterm_core::park::Effect::Return { activate: true, .. }
                                );
                                shell.apply_park(effect, window, cx);
                                shell.select_bar_workspace(cx);
                                if close_sheet {
                                    shell.park_sheet = None;
                                    shell.park_subscription = None;
                                    shell.cover_sheet(false, window, cx);
                                }
                            }
                            WorkspaceCompletion::Open { created } => {
                                let Some(id) = id else {
                                    return;
                                };
                                if shell.workspaces.iter().any(|w| {
                                    w.workspace.id == id
                                        && w.parked.as_ref().is_some_and(|p| !p.running)
                                }) {
                                    shell
                                        .owner
                                        .update(cx, |owner, _| owner.resumed_workspace_order(id));
                                }
                                if let Err(error) = shell.mount_workspace(id, window, cx) {
                                    shell.owner.update(cx, |owner, cx| {
                                        owner.error = Some(format!("Open workspace: {error:#}"));
                                        cx.notify();
                                    });
                                    return;
                                }
                                if let Some(state) =
                                    shell.workspaces.iter_mut().find(|w| w.workspace.id == id)
                                {
                                    state.parked = None;
                                }
                                shell.owner.update(cx, |owner, _| {
                                    owner.stopped_workspaces.remove(&id);
                                });
                                shell.select_workspace_id(id, window, cx);
                                shell.restore_live_workspace_sessions(id, window, cx);
                                if created {
                                    shell.new_work_sheet = None;
                                    shell.new_work_subscription = None;
                                    shell.cover_sheet(false, window, cx);
                                }
                            }
                        }
                        if let Some(sheet) = &shell.park_sheet {
                            let rows = shell.parked_list_rows(cx);
                            sheet.update(cx, |sheet, cx| sheet.set_rows(rows, cx));
                        }
                        shell.refresh_saved_facts(cx);
                        shell.sync_git_visibility(false, cx);
                        shell.sync_content_selection(cx);
                        shell.focus.focus(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn reconcile_stopped_workspace(
        &mut self,
        workspace: crate::WorkspaceId,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gpui::AppContext as _;
        let mut sessions = self.data.sessions(workspace, cx);
        let active = self.data.active_session(workspace, cx);
        let bottom = crate::bottom_pane_ids(self.workspace(workspace), cx);
        let state = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == workspace)
            .unwrap();
        // Stop clears runtime targets. Reconstruct missing main panes from the
        // filtered DTO identities, keeping bottom panes in their own inventory.
        let removed: Vec<_> = state
            .sessions
            .iter()
            .filter(|old| {
                !bottom.contains(&crate::session_pane_id(old.id))
                    && !sessions.iter().any(|s| s.id == old.id)
            })
            .map(|s| crate::session_pane_id(s.id))
            .collect();
        sessions.extend(
            state
                .sessions
                .iter()
                .filter(|s| bottom.contains(&crate::session_pane_id(s.id)))
                .cloned(),
        );
        state.sessions = sessions;
        state.active_session = active;
        for id in removed {
            if let Some(pane) = state.panes.remove(&id) {
                state
                    .dock
                    .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
            }
            self.registry.borrow_mut().remove(&id);
            self.run_tabs.remove(&id);
        }
        for session in &state.sessions {
            let id = crate::session_pane_id(session.id);
            if state.panes.contains_key(&id) {
                continue;
            }
            let text = self
                .data
                .imported_session(workspace, session.id, cx)
                .expect("saved session");
            let pane = cx.new(|cx| {
                crate::pane::Pane::new(
                    id.clone(),
                    session.title.clone(),
                    crate::pane::PaneKind::IcedSession(text),
                    workspace,
                    self.data.clone(),
                    window,
                    cx,
                )
            });
            self._subscriptions.push(subscribe(&pane, window, cx));
            self._subscriptions
                .push(cx.observe_in(&pane, window, |shell, pane, window, cx| {
                    shell.wake_terminal(pane.clone(), window, cx);
                    cx.notify();
                }));
            self.registry
                .borrow_mut()
                .insert(id.clone(), pane.downgrade());
            state.panes.insert(id, pane.clone());
            state.dock.update(cx, |dock, cx| {
                dock.add_panel_view(
                    crate::panel_handle(pane),
                    gpui_component::dock::DockPlacement::Center,
                    None,
                    window,
                    cx,
                )
            });
        }
    }

    /// Saved terminal and chat DTOs remain the source of truth when a stopped
    /// workspace is restored. Keep-running parks never reconstruct these panes.
    pub(crate) fn restore_live_workspace_sessions(
        &mut self,
        workspace: crate::WorkspaceId,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.restore_live_workspace(workspace, true, window, cx);
    }

    pub(crate) fn restore_live_workspace(
        &mut self,
        workspace: crate::WorkspaceId,
        chats: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(saved) = self.owner.read(cx).iced_workspace(workspace).cloned() else {
            return;
        };
        if saved.is_legacy_remote()
            || matches!(
                saved.location,
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
            )
        {
            self.restore_remote_terminals(workspace, &saved, window, cx);
            return;
        }
        let imported = self.owner.read(cx).iced_sessions(&saved);
        if saved.done_at.is_none()
            && crate::bottom_pane_ids(self.workspace(workspace), cx).is_empty()
            && !saved.bottom_terminals.is_empty()
        {
            let bottom_visible = self.workspace(workspace).store.regions.bottom_visible;
            let selected = self.active_workspace(cx);
            self.owner.update(cx, |owner, cx| {
                owner.dispatch(
                    crate::workspace_status::Intent::Activate(workspace),
                    std::time::Instant::now(),
                    cx,
                )
            });
            for terminal in &saved.bottom_terminals {
                self.new_terminal_at(
                    gpui_component::dock::DockPlacement::Bottom,
                    Some(terminal.dir.clone().into()),
                    window,
                    cx,
                );
            }
            if let Some(first) = crate::bottom_pane_ids(self.workspace(workspace), cx).first() {
                self.show_workspace_pane(workspace, first, window, cx);
            }
            self.workspace_mut(workspace).store.regions.bottom_visible = bottom_visible;
            if let Some(selected) = selected {
                self.owner.update(cx, |owner, cx| {
                    owner.dispatch(
                        crate::workspace_status::Intent::Activate(selected),
                        std::time::Instant::now(),
                        cx,
                    )
                });
            }
        }
        let bottom = crate::bottom_pane_ids(self.workspace(workspace), cx);
        let active = self.workspace(workspace).active_session;
        let sessions = self.workspace(workspace).sessions.clone();
        for session in &sessions {
            let pane_id = crate::session_pane_id(session.id);
            let pane = self.workspace(workspace).panes[&pane_id].clone();
            if bottom.contains(&pane_id) {
                if !self.run_tabs.contains_key(&pane_id) {
                    pane.update(cx, |pane, cx| pane.resume_terminal(window, cx));
                }
                continue;
            }
            let target = pane.read(cx).task_session(cx);
            let tab = target
                .as_ref()
                .and_then(|target| {
                    saved
                        .tabs
                        .iter()
                        .find(|tab| tab.session_uid.as_deref() == Some(&target.session_uid))
                })
                .or_else(|| {
                    imported
                        .iter()
                        .position(|s| s.id == session.id)
                        .and_then(|i| saved.tabs.get(i))
                });
            let Some(tab) = tab.cloned() else { continue };
            if tab.tab_kind.as_deref() == Some("agent") {
                if !chats {
                    continue;
                }
                if matches!(
                    &self.workspace(workspace).panes[&crate::session_pane_id(session.id)]
                        .read(cx)
                        .content,
                    crate::pane::PaneContent::PreviewChat(_)
                ) {
                    continue;
                }
                if let Err(error) =
                    self.mount_live_chat(workspace, tab, Some(session.id), false, window, cx)
                {
                    self.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!("Resume chat: {error:#}"));
                        cx.notify();
                    });
                }
            } else {
                let pane =
                    self.workspace(workspace).panes[&crate::session_pane_id(session.id)].clone();
                if pane.read(cx).session.is_none() {
                    let Some(host) = cx
                        .try_global::<crate::task_host::GpuiTaskHost>()
                        .map(|h| h.0.clone())
                    else {
                        continue;
                    };
                    let name = self.workspace(workspace).workspace.name.clone();
                    let result = host.update(cx, |host, cx| {
                        host.open_live_terminal(&name, &tab, true, cx)
                    });
                    match result {
                        Ok(launched) => {
                            if let Some(view) =
                                host.update(cx, |host, _| host.take_terminal(&launched.session_id))
                            {
                                self.adopt_terminal_view(&pane, view, launched.target, window, cx);
                                pane.update(cx, |pane, _| {
                                    pane.terminal_cwd = Some(launched.cwd);
                                    pane.exited = false;
                                    pane.startup_command = launched.startup_command;
                                    pane.chat_session_id = launched.chat_session_id;
                                });
                            }
                        }
                        Err(error) => pane.update(cx, |pane, cx| {
                            let identity = tab
                                .session_identity()
                                .open(|| fresh_target().session_uid)
                                .identity;
                            pane.session = Some(Target {
                                session_uid: identity.session_uid,
                                task_id: tab.task_id.clone(),
                                task_session_id: tab.task_session_id.clone(),
                                ..fresh_target()
                            });
                            pane.startup_command = tab.startup_command.clone();
                            pane.chat_session_id = tab.chat_session_id.clone();
                            pane.terminal_cwd = Some(tab.dir.clone().into());
                            pane.exited = true;
                            pane.content = crate::pane::PaneContent::Error(format!(
                                "Restore terminal: {error}"
                            ));
                            cx.notify();
                        }),
                    }
                }
            }
        }
        if let Some(active) = active {
            self.workspace_mut(workspace).active_session = Some(active);
            self.show_workspace_pane(workspace, &crate::session_pane_id(active), window, cx);
        }
    }
}

impl crate::Shell {
    pub(crate) fn launch_agent_preset(
        &mut self,
        index: usize,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.launch_cli_preset(index, false, window, cx);
    }

    fn launch_cli_preset(
        &mut self,
        index: usize,
        resume: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending
            || !self.local
            || !self.workspaces.iter().any(|state| state.on_bar())
        {
            return;
        }
        if !self.live_chat_enabled(cx) {
            self.preview_unavailable("Launch agent preset", cx);
            return;
        }
        let Some(preset) = self.agent_presets.get(index).cloned() else {
            return;
        };
        if self.launch_remote_agent_terminal(Some(&preset.name), window, cx) {
            return;
        }
        let Some(host) = cx
            .try_global::<crate::task_host::GpuiTaskHost>()
            .map(|h| h.0.clone())
        else {
            return;
        };
        let state = self.active(cx);
        let cwd = state
            .active_session
            .and_then(|id| state.panes.get(&crate::session_pane_id(id)))
            .and_then(|pane| pane.read(cx).task_folder(cx))
            .or_else(|| self.data.terminal_cwd(state.workspace.id, cx));
        let Some(cwd) = cwd else {
            self.preview_unavailable("Remote terminal", cx);
            return;
        };
        let name = state.workspace.name.clone();
        let (command, chat_session_id) = if resume {
            (
                preset
                    .resume_command
                    .clone()
                    .unwrap_or_else(|| preset.command.clone()),
                None,
            )
        } else {
            gitterm_core::workers::with_preassigned_session_id(&preset.command)
        };
        let tab = gitterm_core::workspace_config::WorkspaceTabConfig {
            dir: cwd.to_string_lossy().into_owned(),
            startup_command: Some(command),
            chat_session_id: chat_session_id.clone(),
            extra: Default::default(),
            repo_dir: None,
            tab_kind: None,
            agent_config: None,
            task_id: None,
            task_session_id: None,
            worker_shutdown_refused: false,
            session_uid: None,
        };
        match host.update(cx, |host, cx| {
            host.open_live_terminal(&name, &tab, false, cx)
        }) {
            Ok(launched) => {
                if let Some(pane) = self.adopt_task_terminal(&host, &launched, window, cx) {
                    pane.update(cx, |pane, _| {
                        pane.startup_command = launched.startup_command;
                        pane.chat_session_id = chat_session_id;
                        pane.title = preset.name;
                    });
                }
            }
            Err(error) => self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!("Launch {}: {error}", preset.name));
                cx.notify();
            }),
        }
    }

    pub(crate) fn live_workspace_bottom_terminals(
        &self,
        uid: u64,
        cx: &gpui::App,
    ) -> Vec<gitterm_core::workspace_config::BottomTerminalConfig> {
        let Some(state) = self.workspaces.iter().find(|w| w.runtime_uid == uid) else {
            return vec![];
        };
        crate::bottom_pane_ids(state, cx)
            .into_iter()
            .filter(|id| !self.run_tabs.contains_key(id))
            .filter_map(|id| state.panes.get(&id)?.read(cx).task_folder(cx))
            .map(|dir| gitterm_core::workspace_config::BottomTerminalConfig {
                dir: dir.to_string_lossy().into_owned(),
            })
            .collect()
    }

    pub(crate) fn live_workspace_tabs(
        &self,
        uid: u64,
        cx: &gpui::App,
    ) -> Vec<gitterm_core::workspace_config::WorkspaceTabConfig> {
        let Some(state) = self.workspaces.iter().find(|w| w.runtime_uid == uid) else {
            return vec![];
        };
        let Some(saved) = self.owner.read(cx).iced_workspace(state.workspace.id) else {
            return vec![];
        };
        let imported = self.owner.read(cx).iced_sessions(saved);
        let bottom = crate::bottom_pane_ids(state, cx);
        state
            .sessions
            .iter()
            .filter(|session| {
                let id = crate::session_pane_id(session.id);
                !bottom.contains(&id) && !self.run_tabs.contains_key(&id)
            })
            .filter_map(|session| {
                let pane = state
                    .panes
                    .get(&crate::session_pane_id(session.id))?
                    .read(cx);
                let target = match &pane.content {
                    crate::pane::PaneContent::PreviewChat(view) => Some(view.read(cx).target()),
                    _ => pane.session.clone(),
                };
                let existing = target
                    .as_ref()
                    .and_then(|target| {
                        saved
                            .tabs
                            .iter()
                            .find(|tab| tab.session_uid.as_deref() == Some(&target.session_uid))
                    })
                    .cloned()
                    // Index-derived legacy ids change after closing an earlier tab.
                    // The original DTO stays owned by its runtime pane.
                    .or_else(|| pane.imported_tab.clone())
                    .or_else(|| {
                        imported
                            .iter()
                            .position(|s| s.id == session.id)
                            .and_then(|i| saved.tabs.get(i))
                            .cloned()
                    });
                let cwd = pane.task_folder(cx).unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|t| t.dir.clone())
                        .unwrap_or_else(|| saved.dir.clone())
                        .into()
                });
                let mut tab = if let Some(tab) = existing {
                    tab
                } else if let crate::pane::PaneContent::PreviewChat(view) = &pane.content {
                    crate::chat::resume::tab(
                        &cwd,
                        view.read(cx).target().session_uid,
                        view.read(cx).chat_session_id(),
                    )
                } else {
                    gitterm_core::workspace_config::WorkspaceTabConfig {
                        extra: Default::default(),
                        dir: cwd.to_string_lossy().into_owned(),
                        repo_dir: Some(if saved.is_legacy_remote() || matches!(saved.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })) {
                            cwd.to_string_lossy().into_owned()
                        } else { saved.dir.clone() }),
                        startup_command: None,
                        tab_kind: None,
                        agent_config: None,
                        chat_session_id: None,
                        task_id: target.as_ref().and_then(|t| t.task_id.clone()),
                        task_session_id: target.as_ref().and_then(|t| t.task_session_id.clone()),
                        worker_shutdown_refused: false,
                        session_uid: target.as_ref().map(|t| t.session_uid.clone()),
                    }
                };
                if let Some(target) = target {
                    tab.session_uid = Some(target.session_uid);
                    tab.task_id = target.task_id;
                    tab.task_session_id = target.task_session_id;
                }
                if pane.startup_command.is_some() {
                    tab.startup_command = pane.startup_command.clone();
                }
                if pane.chat_session_id.is_some() {
                    tab.chat_session_id = pane.chat_session_id.clone();
                }
                tab.dir = pane.terminal_saved_dir.as_ref().unwrap_or(&cwd).to_string_lossy().into_owned();
                if matches!(saved.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })) && pane.startup_command.is_some() {
                    tab.repo_dir = Some(cwd.to_string_lossy().into_owned());
                    tab.tab_kind = None;
                    tab.agent_config = None;
                    tab.chat_session_id = pane.chat_session_id.clone();
                    tab.worker_shutdown_refused = false;
                }
                if let crate::pane::PaneContent::PreviewChat(view) = &pane.content {
                    tab.chat_session_id = view.read(cx).chat_session_id();
                }
                Some(tab)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn golive19_preview_remote_buttons_and_new_terminal_are_inert(cx: &mut gpui::TestAppContext) {
        let (_root, shell, cx) = crate::h2_tests::fixture_with(cx, false, |document| {
            document["workspaces"][0]["remote_session"] = serde_json::json!({"label":"Placeholder","host_name":"Remote","ssh_target":"user@remote.example","identity_file":"/tmp/placeholder-key","session_name":"placeholder","remote_dir":"/srv/root"});
            document["workspaces"][0]["tabs"] = serde_json::json!([]);
        });
        let before = shell.read_with(cx, |shell, cx| shell.active(cx).sessions.len());
        shell.update_in(cx, |shell, window, cx| {
            for kind in [
                gitterm_core::remote::launch::SessionKind::Shell,
                gitterm_core::remote::launch::SessionKind::Codex,
                gitterm_core::remote::launch::SessionKind::Claude,
            ] {
                shell.open_remote_session_terminal(kind, window, cx);
            }
            shell.new_terminal(window, cx);
        });
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.active(cx).sessions.len(), before);
            assert!(
                shell
                    .owner
                    .read(cx)
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("preview")
            );
        });
    }

    #[gpui::test]
    fn golive10_preview_presets_remain_read_only(cx: &mut gpui::TestAppContext) {
        let (_root, shell, cx) = crate::h2_tests::fixture_with(cx, false, |_| {});
        let before = shell.read_with(cx, |shell, cx| shell.active(cx).sessions.len());
        shell.update_in(cx, |shell, window, cx| {
            shell.launch_agent_preset(0, window, cx)
        });
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.active(cx).sessions.len(), before);
            assert!(
                shell
                    .owner
                    .read(cx)
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("preview")
            );
        });
    }

    #[gpui::test]
    fn live_restore_keeps_selected_tab_when_background_tab_shares_conversation(
        cx: &mut gpui::TestAppContext,
    ) {
        let live = tempfile::tempdir().unwrap();
        let services = gitterm_core::preview::service::ServiceContext::production(
            live.path(),
            "restore-selection-test",
        )
        .unwrap();
        cx.update(|cx| crate::chat::policy::install_live(services, cx).unwrap());
        let (_root, shell, cx) = crate::h2_tests::fixture_with(cx, false, |file| {
            file["active_workspace"] = serde_json::json!(1);
            for (index, uid) in [(0, "background-copy"), (1, "selected-original")] {
                file["workspaces"][index]["active_tab"] = serde_json::json!(0);
                file["workspaces"][index]["tabs"] = serde_json::json!([{
                    "dir": "/fixture/work", "tab_kind": "agent",
                    "agent_config": {"backend":"claude", "model":"default"},
                    "session_uid": uid, "chat_session_id": "same-conversation"
                }]);
            }
        });
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.active_workspace(cx), Some(crate::WorkspaceId(2)));
            let state = shell.active(cx);
            let pane = state.panes[&crate::session_pane_id(state.active_session.unwrap())].read(cx);
            let crate::pane::PaneContent::PreviewChat(view) = &pane.content else {
                panic!("selected conversation must be mounted in its selected tab");
            };
            assert_eq!(view.read(cx).target().session_uid, "selected-original");
            assert!(shell.owner.read(cx).chats.contains_key("selected-original"));
            assert!(!shell.owner.read(cx).chats.contains_key("background-copy"));
        });
    }

    #[test]
    fn strip_fixture_resolves_all_saved_tabs_and_facts_without_loading_history() {
        use gitterm_core::workspace_config::WorkspacesFile;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("iced");
        let home = dir.path().join("home");
        let projects = home.join(".claude/projects/fixture");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(config.join("chat-meta")).unwrap();
        // workspaces.json has identities/config, not a persisted chat-title field.
        let source = serde_json::json!({
            "active_workspace": 0,
            "workspaces": [{
                "name": "Synthetic work", "abbrev": "SW", "dir": "/fixture/repo",
                "color": "lavender", "chats": ["parent"],
                "tabs": (["saved", "summary", "first", "facts-child", "uuid-only"]
                    .map(|id| serde_json::json!({
                        "dir": "/fixture/repo", "tab_kind": "agent",
                        "agent_config": {"backend": "claude", "model": "fixture"},
                        "chat_session_id": id
                    })))
            }]
        });
        let bytes = serde_json::to_vec(&source).unwrap();
        std::fs::write(config.join("workspaces.json"), &bytes).unwrap();
        let saved: WorkspacesFile = serde_json::from_slice(&bytes).unwrap();
        let transcripts = [
            (
                "saved",
                concat!(
                    "{\"type\":\"user\",\"cwd\":\"/fixture/repo\",\"message\":{\"content\":\"Original prompt\"}}\n",
                    "{\"type\":\"ai-title\",\"aiTitle\":\"Old title\"}\n",
                    "{\"type\":\"ai-title\",\"aiTitle\":\" Saved generated title \"}\n"
                ),
            ),
            (
                "summary",
                concat!(
                    "{\"type\":\"summary\",\"cwd\":\"/fixture/repo\",\"summary\":\"Summary title\"}\n",
                    "{\"type\":\"user\",\"message\":{\"content\":\"Later question\"}}\n"
                ),
            ),
            (
                "first",
                concat!(
                    "{\"type\":\"user\",\"cwd\":\"/fixture/repo\",\"isMeta\":true,\"message\":{\"content\":\"Skip metadata\"}}\n",
                    "{\"type\":\"user\",\"message\":{\"content\":\"<system-reminder>skip</system-reminder>\"}}\n",
                    "{\"type\":\"user\",\"message\":{\"content\":\"First real question\\nMore text\"}}\n"
                ),
            ),
        ];
        for (id, source) in &transcripts {
            std::fs::write(projects.join(format!("{id}.jsonl")), source).unwrap();
        }
        let facts = serde_json::json!({"children": [
            {"session_id": "facts-child", "kind": "worker", "title": "Recorded child title"},
            {"session_id": "saved", "kind": "worker", "title": "Older parent brief"}
        ]});
        let facts_bytes = serde_json::to_vec(&facts).unwrap();
        let facts_path = config.join("chat-meta/claude-parent.json");
        std::fs::write(&facts_path, &facts_bytes).unwrap();
        let workspace = &saved.workspaces[0];
        let conversations: Vec<_> = workspace
            .chats
            .iter()
            .cloned()
            .chain(
                workspace
                    .tabs
                    .iter()
                    .filter_map(|tab| tab.chat_session_id.clone()),
            )
            .collect();
        let titles = read_iced_strip_titles(&config, Some(&home), &conversations);
        assert_eq!(titles["saved"], "Saved generated title");
        assert_eq!(titles["summary"], "Summary title");
        assert_eq!(titles["first"], "First real question");
        assert_eq!(titles["facts-child"], "Recorded child title");
        assert!(!titles.contains_key("uuid-only"));
        assert_eq!(
            crate::session_strip::label(None, &workspace.tabs[4].dir, false, false),
            "repo"
        );
        assert_eq!(
            std::fs::read(config.join("workspaces.json")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read(facts_path).unwrap(), facts_bytes);
        for (id, source) in transcripts {
            assert_eq!(
                std::fs::read_to_string(projects.join(format!("{id}.jsonl"))).unwrap(),
                source
            );
        }
    }

    #[test]
    fn e03_a_transcript_with_no_usable_entry_opens_an_empty_chat() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("fixture-project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(
            project.join("fixture-empty.jsonl"),
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"Ignore metadata\"}}\n",
        )
        .unwrap();
        let saved = read_iced_history(
            dir.path(),
            "fixture-empty",
            "/fixture/cwd",
            "/fixture/provider".into(),
            &[],
        )
        .expect("an empty history is not an error");
        assert!(saved.records.is_empty());
    }

    #[test]
    fn imported_transcript_is_read_only_and_preserves_recorded_history() {
        use gitterm_gpui_chat::events::Entry;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("fixture-project");
        std::fs::create_dir(&project).unwrap();
        let path = project.join("fixture-chat.jsonl");
        let source = concat!(
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"Ignore metadata\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"<system-reminder>skip</system-reminder>\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"Recorded question\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Recorded answer\"}]}}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"Old title\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\" Current Iced title \"}\n"
        );
        std::fs::write(&path, source).unwrap();
        let saved = read_iced_history(
            dir.path(),
            "fixture-chat",
            "/fixture/cwd",
            "/fixture/provider".into(),
            &[],
        )
        .unwrap();
        assert_eq!(saved.session_id.as_deref(), Some("fixture-chat"));
        assert!(
            saved.records.iter().any(
                |r| matches!(&r.entry, Entry::UserPrompt(text) if text == "Recorded question")
            )
        );
        assert!(
            saved
                .records
                .iter()
                .any(|r| matches!(&r.entry, Entry::Harness(_)))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(
            read_iced_history(
                dir.path(),
                "../escape",
                "/fixture/cwd",
                "/fixture/provider".into(),
                &[]
            )
            .is_err()
        );
        assert!(
            read_iced_history(
                dir.path(),
                "missing",
                "/fixture/cwd",
                "/fixture/provider".into(),
                &[]
            )
            .is_err()
        );
        std::fs::write(
            project.join("empty.jsonl"),
            "{\"type\":\"ai-title\",\"aiTitle\":\"No history\"}\n",
        )
        .unwrap();
        // Iced opens an empty page for a transcript with no usable entry.
        assert!(
            read_iced_history(
                dir.path(),
                "empty",
                "/fixture/cwd",
                "/fixture/provider".into(),
                &[]
            )
            .unwrap()
            .records
            .is_empty()
        );
    }

    #[test]
    fn imported_transcript_puts_cards_where_they_were_requested() {
        use gitterm_gpui_chat::events::Entry;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("fixture-project");
        std::fs::create_dir(&project).unwrap();
        let source = concat!(
            "{\"type\":\"user\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"message\":{\"content\":\"Question\"}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T10:00:05Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"mcp__gitterm_tasks__delegate_task\",\"input\":{}}]}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-01-01T10:00:06Z\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-1\",\"content\":\"{\\\"delegation_id\\\":\\\"d-tool\\\"}\",\"is_error\":false}]}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T10:00:07Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Started a worker\"}]}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-01-01T10:30:00Z\",\"message\":{\"content\":\"Later question\"}}\n"
        );
        std::fs::write(project.join("cards-chat.jsonl"), source).unwrap();
        let delegations = [
            ("d-tool".to_string(), "2026-01-01T10:00:05Z".to_string()),
            ("d-page".to_string(), "2026-01-01T10:10:00Z".to_string()),
            ("d-after".to_string(), "2026-01-02T00:00:00Z".to_string()),
        ];
        let saved = read_iced_history(
            dir.path(),
            "cards-chat",
            "/fixture/cwd",
            "/fixture/provider".into(),
            &delegations,
        )
        .unwrap();
        let order: Vec<String> = saved
            .records
            .iter()
            .filter_map(|r| match &r.entry {
                Entry::DisplayDelegation { id, .. } => Some(format!("card {id}")),
                Entry::UserPrompt(text) => Some(format!("prompt {text}")),
                Entry::Harness(gitterm_core::chat::types::HarnessEvent::ItemCompleted {
                    ..
                }) => Some("result".into()),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            [
                "prompt Question",
                "result",
                "card d-tool",
                "card d-page",
                "prompt Later question",
                "card d-after",
            ]
        );
    }

    #[test]
    fn imported_transcript_keeps_real_work_timing_and_excludes_idle_time() {
        use gitterm_gpui_chat::events::Entry;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("fixture-project");
        std::fs::create_dir(&project).unwrap();
        let source = concat!(
            "{\"type\":\"user\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"message\":{\"content\":\"Question\"}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T10:00:30Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"Bash\",\"input\":{\"command\":\"work\"}}]}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-01-01T10:02:00Z\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-1\",\"content\":\"done\",\"is_error\":false}]}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-01-01T10:30:00Z\",\"message\":{\"content\":\"Later question\"}}\n"
        );
        std::fs::write(project.join("timed-chat.jsonl"), source).unwrap();
        let saved = read_iced_history(
            dir.path(),
            "timed-chat",
            "/fixture/cwd",
            "/fixture/provider".into(),
            &[],
        )
        .unwrap();
        let mut timing = (None, None);
        for record in &saved.records {
            if let Entry::TurnTiming {
                start_ms, end_ms, ..
            } = &record.entry
            {
                timing.0 = timing.0.or(*start_ms);
                timing.1 = timing.1.or(*end_ms);
            }
        }
        assert_eq!(timing, (Some(1_767_261_600_000), Some(1_767_261_720_000)));
        assert!(saved.records.iter().any(|record| {
            matches!(&record.entry, Entry::Harness(_)) && record.at_ms.is_some()
        }));
        assert!(saved.records.iter().any(|record| {
            matches!(
                &record.entry,
                Entry::Harness(gitterm_core::chat::types::HarnessEvent::TurnCompleted { .. })
            ) && record.at_ms.is_none()
        }));
        let model = gitterm_gpui_chat::model::ChatModel::from_records(&saved.records);
        let duration = model.completed_turns[0].duration_ms.unwrap();
        assert_eq!(duration, 120_000);
    }
    #[test]
    fn runtime_replacement_and_every_stale_binding_are_rejected() {
        let current = fresh_target();
        assert!(can_close(&current, &current, false));
        assert!(!can_close(&current, &current, true));
        assert!(!can_close(&fresh_target(), &current, false));
        for field in 0..4 {
            let mut stale = current.clone();
            match field {
                0 => stale.tab_id += 1,
                1 => stale.session_uid.push('x'),
                2 => stale.task_id = Some("fixture".into()),
                _ => stale.task_session_id = Some("fixture".into()),
            }
            assert!(!can_close(&current, &stale, false));
        }
    }

    /// Issue 98: a replayed Claude transcript folds each finished turn, as Iced.
    /// The transcript is synthetic: prompt; text, tools, text, thinking, tools;
    /// a final text; then a second prompt and turn.
    #[gpui::test]
    fn replayed_claude_transcript_folds_each_finished_turn(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, VisualTestContext};
        fn draw(cx: &mut VisualTestContext) {
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        let at = |s: u32| format!("2026-01-01T00:{:02}:{:02}.000Z", s / 60, s % 60);
        let assistant = |s: u32, content: serde_json::Value| serde_json::json!({"type":"assistant","timestamp":at(s),"message":{"content":content}});
        let user = |s: u32, content: serde_json::Value| serde_json::json!({"type":"user","timestamp":at(s),"message":{"content":content}});
        let result = |s: u32, id: &str| {
            user(
                s,
                serde_json::json!([{"type":"tool_result","tool_use_id":id,"content":"ok"}]),
            )
        };
        let bash = |id: &str| serde_json::json!({"type":"tool_use","id":id,"name":"Bash","input":{"command":id}});
        let lines = [
            user(0, "Check the build".into()),
            assistant(
                1,
                serde_json::json!([{"type":"text","text":"Looking at the build."}]),
            ),
            assistant(2, serde_json::json!([bash("one"), bash("two")])),
            result(3, "one"),
            result(4, "two"),
            assistant(
                5,
                serde_json::json!([{"type":"text","text":"Two checks pass."}]),
            ),
            assistant(
                6,
                serde_json::json!([{"type":"thinking","thinking":"Read the config"}]),
            ),
            assistant(
                7,
                serde_json::json!([bash("three"), {"type":"tool_use","id":"four","name":"Read","input":{"file_path":"a.toml"}}]),
            ),
            result(8, "three"),
            result(9, "four"),
            assistant(
                65,
                serde_json::json!([{"type":"text","text":"The build is fine."}]),
            ),
            user(70, "And the tests?".into()),
            assistant(
                71,
                serde_json::json!([{"type":"thinking","thinking":"Run them"}]),
            ),
            assistant(72, serde_json::json!([bash("five")])),
            result(80, "five"),
            assistant(81, serde_json::json!([{"type":"text","text":"They pass."}])),
        ];
        let projects = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(projects.path().join("fixture")).unwrap();
        std::fs::write(
            projects.path().join("fixture/synthetic.jsonl"),
            lines.map(|line| line.to_string()).join("\n"),
        )
        .unwrap();
        let saved = read_iced_history(
            projects.path(),
            "synthetic",
            "/fixture/repo",
            projects.path().join("provider"),
            &[],
        )
        .unwrap();
        // The transcript has no turn markers: each prompt and the end of the
        // file end the turn before them, as in Iced's history replay.
        let model = gitterm_gpui_chat::model::ChatModel::from_records(&saved.records);
        assert_eq!(
            model
                .completed_turns
                .iter()
                .map(|turn| (turn.rows.clone(), turn.duration_ms))
                .collect::<Vec<_>>(),
            [(1..9, Some(65_000)), (10..13, Some(11_000))]
        );
        let (_root, shell, cx) = crate::h6_tests::sample(cx);
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::app_menus::TabSelect { index: 0 }), cx)
        });
        draw(cx);
        cx.update(|window, cx| {
            let shutdown = shell.read(cx).owner.read(cx).chat_shutdown.clone();
            let preview = cx.new(|cx| crate::chat::ChatPane::history(shutdown, saved, window, cx));
            let state = shell.read(cx).active(cx);
            let pane = state.panes[&crate::session_pane_id(state.active_session.unwrap())].clone();
            pane.update(cx, |pane, cx| {
                pane.content = crate::pane::PaneContent::PreviewChat(preview);
                cx.notify();
            });
        });
        draw(cx);
        // Rows: 0 prompt, 1 text, 2-3 tools, 4 text, 5 thinking, 6-7 tools,
        // 8 final text, 9 prompt, 10 thinking, 11 tool, 12 final text.
        for fold in ["work-fold-1", "work-fold-10"] {
            assert!(cx.debug_bounds(fold).is_some(), "{fold} drawn");
        }
        for hidden in [2, 3, 4, 5, 6, 7, 11] {
            let selector = Box::leak(format!("transcript-row-{hidden}").into_boxed_str());
            assert!(cx.debug_bounds(selector).is_none(), "row {hidden} folded");
        }
        for shown in [0, 8, 9, 12] {
            let selector = Box::leak(format!("transcript-row-{shown}").into_boxed_str());
            assert!(cx.debug_bounds(selector).is_some(), "row {shown} shown");
        }
        let fold = cx.debug_bounds("work-fold-1").unwrap();
        cx.simulate_click(fold.center(), gpui::Modifiers::none());
        draw(cx);
        // Opened: narration as text, each tool run between paragraphs one line.
        for shown in ["transcript-row-4", "tool-run-2", "tool-run-5"] {
            assert!(cx.debug_bounds(shown).is_some(), "{shown} shown");
        }
        for hidden in ["transcript-row-3", "transcript-row-6", "transcript-row-7"] {
            assert!(cx.debug_bounds(hidden).is_none(), "{hidden} in its run");
        }
    }
    /// Issue 98: a real Claude turn's transcript shape (its text replaced by
    /// placeholders) folds into one closed "Worked for" row, as in Iced. The
    /// turn has thinking, narration, Bash calls among metadata lines, an
    /// answered AskUserQuestion, ToolSearch and MCP calls, a final text, a stop
    /// hook summary, and then a GitTerm delivery and the next turn.
    #[gpui::test]
    fn real_claude_turn_shape_folds_into_one_worked_row(cx: &mut gpui::TestAppContext) {
        use gitterm_gpui_chat::model::RowKind;
        use gpui::{AppContext as _, VisualTestContext};
        fn draw(cx: &mut VisualTestContext) {
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        let projects = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(projects.path().join("fixture")).unwrap();
        std::fs::write(
            projects.path().join("fixture/real-shape.jsonl"),
            include_str!("chat/fixtures/claude-turn-with-question.jsonl"),
        )
        .unwrap();
        let saved = read_iced_history(
            projects.path(),
            "real-shape",
            "/fixture/repo",
            projects.path().join("provider"),
            &[],
        )
        .unwrap();
        let model = gitterm_gpui_chat::model::ChatModel::from_records(&saved.records);
        // Rows: 0 prompt, 1..=50 the turn's work and narration, 51 its final
        // text, 52 the delivery card, 53..=57 the next turn, 58 the last prompt.
        let turn = &model.completed_turns[0];
        assert_eq!(turn.rows, 1..52);
        assert_eq!(turn.duration_ms, Some(470_458));
        let rows = &model.rows[1..52];
        // The answered question is an ordinary tool card, as Iced shows it.
        assert!(rows.iter().all(|row| matches!(
            row.kind,
            RowKind::Thinking | RowKind::Assistant | RowKind::Tool
        )));
        assert!(rows.iter().any(|row| {
            row.tool
                .as_ref()
                .is_some_and(|tool| tool.name == "AskUserQuestion")
        }));
        assert_eq!(model.rows[51].kind, RowKind::Assistant);
        assert!(model.rows[52].delivery.is_some());
        assert_eq!(model.rows[58].kind, RowKind::User);
        let (_root, shell, cx) = crate::h6_tests::sample(cx);
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::app_menus::TabSelect { index: 0 }), cx)
        });
        draw(cx);
        cx.update(|window, cx| {
            let shutdown = shell.read(cx).owner.read(cx).chat_shutdown.clone();
            let preview = cx.new(|cx| crate::chat::ChatPane::history(shutdown, saved, window, cx));
            let state = shell.read(cx).active(cx);
            let pane = state.panes[&crate::session_pane_id(state.active_session.unwrap())].clone();
            pane.update(cx, |pane, cx| {
                pane.content = crate::pane::PaneContent::PreviewChat(preview);
                cx.notify();
            });
        });
        draw(cx);
        let label = cx
            .debug_bounds("work-fold-label-1")
            .expect("one Worked row for the first turn");
        assert!(label.size.width > gpui::px(0.));
        assert!(cx.debug_bounds("work-fold-1").is_some());
        for hidden in 2..51 {
            let selector = Box::leak(format!("transcript-row-{hidden}").into_boxed_str());
            assert!(cx.debug_bounds(selector).is_none(), "row {hidden} folded");
            let fold = Box::leak(format!("work-fold-{hidden}").into_boxed_str());
            assert!(
                cx.debug_bounds(fold).is_none(),
                "no second fold at {hidden}"
            );
        }
        for shown in [0, 51] {
            let selector = Box::leak(format!("transcript-row-{shown}").into_boxed_str());
            assert!(cx.debug_bounds(selector).is_some(), "row {shown} shown");
        }
    }
}

impl crate::Shell {
    fn new_acp_chat(
        &mut self,
        agent: gitterm_core::chat::types::AcpAgent,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.owner.read(cx).workspace_save_pending
            || !gitterm_core::chat::types::AcpAgent::ALL.contains(&agent)
        {
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let Some(cwd) = self.data.terminal_cwd(workspace, cx) else {
            return;
        };
        let mut tab = crate::chat::resume::tab(&cwd, fresh_target().session_uid, None);
        tab.agent_config = Some(gitterm_core::harness::acp_launch::new_chat_config(
            &self.chat_defaults,
            agent,
        ));
        if let Err(error) = self.mount_live_chat(workspace, tab, None, true, window, cx) {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!("New {} chat: {error:#}", agent.label()));
                cx.notify();
            });
        }
    }
}
