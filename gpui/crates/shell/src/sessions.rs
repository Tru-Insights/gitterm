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
        if !can_close(
            pane.read(cx).session.as_ref().unwrap(),
            requested,
            state.sessions.len() <= 1,
        ) {
            // Dock removal precedes the close decision; keep a protected last tab visible.
            let workspace = state.workspace.id;
            self.show_workspace_pane(workspace, &pane_id, window, cx);
            return;
        }
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
        if state.active_session == Some(id) {
            state.active_session = state.sessions.first().map(|session| session.id);
        }
        state
            .dock
            .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        crate::save_layout(state, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }
}

impl crate::Shell {
    fn picker_inputs(&self) -> crate::session_picker::PickerInputs {
        crate::session_picker::PickerInputs {
            chat: gitterm_core::settings::ChatDefaults::default().new_chat_selection(),
            local_workspace: self.local,
            // TODO(TRU-274 P-src): project the settings/ACP/task catalogs here.
            acp_chats: vec![],
            presets: vec![],
            task: None,
        }
    }
    pub(crate) fn open_session_picker(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gpui::AppContext as _;
        if self.active_workspace(cx).is_none() || self.park_sheet.is_some() {
            return;
        }
        let inputs = self.picker_inputs();
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
                if let Some(index) = self
                    .workspaces
                    .iter()
                    .filter(|s| s.on_bar())
                    .position(|s| s.workspace.id.0 == *workspace)
                {
                    self.select_workspace(index, window, cx);
                }
                if let gitterm_core::work::summary::Focus::Coordinator(chat) = focus {
                    self.select_work_chat(*workspace, chat, window, cx);
                }
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
            // TODO(TRU-274 Work actions slice): wire repo editing, run/review/editor,
            // ownership, refresh/index and Find after the owner exposes those paths.
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
            .enumerate()
            .find_map(|(index, state)| {
                if state.workspace.id.0 != workspace {
                    return None;
                }
                state
                    .sessions
                    .iter()
                    .enumerate()
                    .find_map(|(session_index, session)| {
                        let pane = state
                            .panes
                            .get(&crate::session_pane_id(session.id))?
                            .read(cx);
                        let crate::pane::PaneContent::PreviewChat(view) = &pane.content else {
                            return None;
                        };
                        view.read(cx)
                            .matches_chat(chat)
                            .then_some((index, session_index))
                    })
            });
        if let Some((workspace, session)) = selected {
            self.select_workspace(workspace, window, cx);
            self.select_session(session, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
