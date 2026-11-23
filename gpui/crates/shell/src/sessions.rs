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
        state.sessions.retain(|session| session.id != id);
        let was_active = state.active_session == Some(id);
        if was_active {
            state.active_session = state.sessions.first().map(|session| session.id);
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
            match sibling {
                Some(sibling) => self.select_session_id(sibling, window, cx),
                None => {
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

    fn workspace(&self, id: crate::data::WorkspaceId) -> &crate::WorkspaceDock {
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
    ) -> crate::session_picker::PickerInputs {
        crate::session_picker::PickerInputs {
            chat: gitterm_core::settings::ChatDefaults::default().new_chat_selection(),
            local_workspace: self.local,
            // TODO(TRU-274 P-src): project the ACP catalog here.
            acp_chats: vec![],
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
        let inputs = self.picker_inputs(task);
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
            O::Terminal if self.local => self.new_terminal(window, cx),
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
    fn select_work_chat(
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
        let open: Vec<_> = saved
            .iter()
            .enumerate()
            .map(|(i, w)| OpenWork {
                uid: i as u64 + 1,
                name: &w.name,
                dir: &w.dir,
                chats: &w.chats,
                claims_new_chats: false,
                running_park: self.workspaces[i].parked.is_some(),
            })
            .collect();
        let existing = existing_work(&open, &[]);
        // shortcut: the chat index is not hosted yet; use Iced's open-workspace
        // and tab candidates in catalog order until that source is connected.
        let candidates = saved.iter().filter(|w| !w.is_legacy_remote() && !matches!(w.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })))
            .flat_map(|w| std::iter::once(std::path::PathBuf::from(w.home_dir())).chain(w.repos.iter().map(std::path::PathBuf::from)))
            .chain(saved.iter().filter(|w| w.location.is_none() && !w.is_legacy_remote()).flat_map(|w| w.tabs.iter().filter(|t| t.task_id.is_none()).map(|t| std::path::PathBuf::from(&t.dir))));
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
        use crate::workspace_sheet::WorkspaceSheetOutcome as O;
        use gitterm_core::work::catalog::ExistingWorkAction as A;
        match outcome {
            O::Create { .. } => {
                if let Some(sheet) = &self.new_work_sheet {
                    sheet.update(cx, |sheet, cx| {
                        sheet.show_error("Creating work isn't available in the preview".into(), cx)
                    });
                }
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
            for (session, tab) in crate::data::iced_sessions(saved).iter().zip(&saved.tabs) {
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
                        read_iced_history(&projects, &conversation, &tab.dir, provider)
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
    for timed in gitterm_core::chat::history::parse_claude_transcript_timed(&source) {
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
    if records.is_empty() {
        return Err("Transcript has no recorded history".into());
    }
    Ok(crate::chat::SavedChat {
        uid: format!("iced-{conversation}"),
        cwd: cwd.into(),
        provider,
        session_id: Some(conversation.into()),
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
                "/fixture/provider".into()
            )
            .is_err()
        );
        assert!(
            read_iced_history(
                dir.path(),
                "missing",
                "/fixture/cwd",
                "/fixture/provider".into()
            )
            .is_err()
        );
        std::fs::write(
            project.join("empty.jsonl"),
            "{\"type\":\"ai-title\",\"aiTitle\":\"No history\"}\n",
        )
        .unwrap();
        assert!(
            read_iced_history(
                dir.path(),
                "empty",
                "/fixture/cwd",
                "/fixture/provider".into()
            )
            .is_err()
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
}
