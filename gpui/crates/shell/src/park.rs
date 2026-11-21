//! Terminal-only adapter over core parking policy. The dock retains PTY ownership.
use crate::{Shell, WorkspaceDock};
use anyhow::{Result, ensure};
use gitterm_core::park::{self, Effect, Info, Intent, Key, Mode, Target};
use gpui::{App, Context, Window};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
#[path = "lists_tests.rs"]
mod list_tests;
#[cfg(test)]
#[path = "park_tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub(crate) struct Parked {
    pub info: Info,
    pub running: bool,
    pub done: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedPark {
    name: String,
    dir: PathBuf,
    note: String,
    parked_at: String,
    done: bool,
}
impl SavedPark {
    pub(crate) fn matches(&self, name: &str, dir: &std::path::Path) -> bool {
        self.name == name && self.dir == dir
    }
}
pub(crate) fn validate_saved(records: &[SavedPark]) -> Result<()> {
    let mut keys = std::collections::HashSet::new();
    for record in records {
        ensure!(
            record.dir.is_absolute(),
            "parked directory must be absolute"
        );
        ensure!(
            keys.insert((&record.name, &record.dir)),
            "duplicate parked workspace"
        );
    }
    Ok(())
}

impl WorkspaceDock {
    fn park_key(&self, cx: &App) -> Key {
        let dir = self
            .panes
            .values()
            .next()
            .and_then(|pane| pane.read(cx).workspace_dir(cx));
        Key {
            name: self.workspace.name.clone(),
            dir: dir.unwrap_or_default().to_string_lossy().into_owned(),
        }
    }
    pub(crate) fn park_target(&self, cx: &App) -> Target {
        let mut tabs: Vec<_> = self
            .panes
            .values()
            .filter_map(|pane| pane.read(cx).session.clone())
            .collect();
        tabs.sort_by_key(|tab| tab.tab_id);
        Target {
            uid: self.runtime_uid,
            tabs,
        }
    }
    pub(crate) fn on_bar(&self) -> bool {
        self.parked.is_none()
    }
    pub(crate) fn chat_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.kind == crate::data::SessionKind::Chat)
            .count()
    }
}

impl Shell {
    pub(crate) fn reopen_workspace(
        &mut self,
        id: crate::data::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|state| state.workspace.id == id)
        else {
            return;
        };
        let Some(parked) = &mut state.parked else {
            return;
        };
        // Explicit Add folder can reopen a previously completed workspace.
        parked.done = false;
        let key = state.park_key(cx);
        self.park_intent(None, Intent::Resume(key), window, cx);
    }
    pub(crate) fn wake_terminal(
        &mut self,
        pane: gpui::Entity<crate::pane::Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !pane.update(cx, |pane, _| std::mem::take(&mut pane.exit_pending)) {
            return;
        }
        let target = self
            .workspaces
            .iter()
            .find(|state| {
                state.parked.as_ref().is_some_and(|p| p.running)
                    && state.panes.values().any(|candidate| candidate == &pane)
            })
            .map(|state| state.park_target(cx));
        if let Some(target) = target {
            self.park_intent(
                Some(&target),
                Intent::Return {
                    uid: target.uid,
                    woke: Some(park::WakeReason::Finished),
                },
                window,
                cx,
            );
        }
    }
    pub(crate) fn restore_parked(&mut self, saved: Vec<SavedPark>, cx: &mut Context<Self>) {
        for state in &mut self.workspaces {
            let key = state.park_key(cx);
            if let Some(record) = saved
                .iter()
                .find(|record| record.name == key.name && record.dir.to_string_lossy() == key.dir)
            {
                state.parked = Some(Parked {
                    info: Info::new(&record.note, record.parked_at.clone()),
                    running: false, // A saved process is never restored or attached.
                    done: record.done,
                });
            }
        }
        self.select_bar_workspace(cx);
    }
    pub(crate) fn park_projection(&self, cx: &App) -> park::State {
        let mut state = park::State::default();
        for workspace in &self.workspaces {
            let key = workspace.park_key(cx);
            match &workspace.parked {
                Some(parked) if !parked.running => state.closed.push(park::ClosedWorkspace {
                    key,
                    parked: !parked.done,
                }),
                parked => state.open.push(park::OpenWorkspace {
                    target: workspace.park_target(cx),
                    key,
                    running: parked.as_ref().map(|parked| parked.info.clone()),
                    remote_id: None,
                    conversations: vec![],
                }),
            }
        }
        state
    }
    fn select_bar_workspace(&mut self, cx: &mut Context<Self>) {
        if self
            .workspaces
            .iter()
            .any(|state| state.on_bar() && Some(state.workspace.id) == self.active_workspace(cx))
        {
            return;
        }
        if let Some(id) = self
            .workspaces
            .iter()
            .find(|state| state.on_bar())
            .map(|state| state.workspace.id)
        {
            self.owner.update(cx, |owner, cx| {
                owner.dispatch(crate::Intent::Activate(id), Instant::now(), cx)
            });
        }
    }
    #[cfg(test)]
    pub(crate) fn park_workspace(
        &mut self,
        target: &Target,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        self.park_intent(
            Some(target),
            Intent::Park {
                uid: target.uid,
                mode,
                info: Info::new("", stamp),
            },
            window,
            cx,
        );
    }
    pub(crate) fn park_intent(
        &mut self,
        expected: Option<&Target>,
        intent: Intent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local {
            return;
        }
        let state = self.park_projection(cx);
        // UI closures capture the whole incarnation, never a selected index.
        if expected.is_some_and(|target| {
            !self
                .workspaces
                .iter()
                .any(|workspace| &workspace.park_target(cx) == target)
        }) {
            return;
        }
        let effect = state.reduce(intent);
        self.apply_park(effect, window, cx);
        self.select_bar_workspace(cx);
        self.save_parked(cx);
        self.sync_git_visibility(false, cx);
        self.sync_content_selection(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }
    pub(crate) fn apply_park(
        &mut self,
        effect: Effect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match effect {
            Effect::None | Effect::FilteredRestore { .. } => {}
            Effect::Park { target, mode, info } => {
                if self.park_projection(cx).resolve(&target).is_none() {
                    return;
                }
                let state = self
                    .workspaces
                    .iter_mut()
                    .find(|state| state.runtime_uid == target.uid)
                    .unwrap();
                // Save the existing terminal DTOs/layout before recording the park.
                crate::save_layout(state, cx);
                if mode == Mode::Stop {
                    for pane in state.panes.values() {
                        if let Some(target) = pane.read(cx).session.clone() {
                            pane.update(cx, |pane, cx| pane.stop_target(&target, cx));
                        }
                    }
                    self.owner.update(cx, |owner, _| {
                        owner.stopped_workspaces.insert(state.workspace.id);
                    });
                }
                state.parked = Some(Parked {
                    info,
                    running: mode == Mode::KeepRunning,
                    done: false,
                });
            }
            Effect::Return { uid, activate, .. } => {
                let Some(state) = self.workspaces.iter_mut().find(|state| {
                    state.runtime_uid == uid && state.parked.as_ref().is_some_and(|p| p.running)
                }) else {
                    return;
                };
                state.parked = None;
                if activate {
                    self.owner.update(cx, |owner, cx| {
                        owner.dispatch(
                            crate::Intent::Activate(state.workspace.id),
                            Instant::now(),
                            cx,
                        )
                    });
                }
            }
            Effect::RestoreStopped { index } => {
                let Some(state) = self
                    .workspaces
                    .iter_mut()
                    .filter(|state| state.parked.as_ref().is_some_and(|p| !p.running))
                    .nth(index)
                else {
                    return;
                };
                // Only explicit resume can create fresh ordinary terminals. There
                // are no stored commands, objectives, agent or worker identities.
                for pane in state.panes.values() {
                    pane.update(cx, |pane, cx| pane.resume_terminal(window, cx));
                }
                for session in &state.sessions {
                    let pane = state.panes[&crate::session_pane_id(session.id)].clone();
                    state.dock.update(cx, |dock, cx| {
                        if dock
                            .panel(gpui_component::dock::PanelId::from(pane.entity_id()))
                            .is_none()
                        {
                            dock.add_panel_view(
                                gpui_component::dock::panel_handle(pane),
                                gpui_component::dock::DockPlacement::Center,
                                None,
                                window,
                                cx,
                            );
                        }
                    });
                }
                state.parked = None;
                state.runtime_uid = crate::sessions::fresh_target().tab_id as u64;
                self.owner.update(cx, |owner, cx| {
                    owner.stopped_workspaces.remove(&state.workspace.id);
                    owner.dispatch(
                        crate::Intent::Activate(state.workspace.id),
                        Instant::now(),
                        cx,
                    );
                });
            }
            Effect::Done { stop, finish } => {
                let finish_id = self
                    .workspaces
                    .iter()
                    .filter(|state| state.parked.as_ref().is_some_and(|p| !p.running))
                    .nth(finish)
                    .map(|state| state.workspace.id)
                    .or_else(|| {
                        stop.as_ref().and_then(|(target, _)| {
                            self.workspaces
                                .iter()
                                .find(|state| state.runtime_uid == target.uid)
                                .map(|state| state.workspace.id)
                        })
                    });
                if let Some((target, info)) = stop {
                    if self.park_projection(cx).resolve(&target).is_none() {
                        return;
                    }
                    self.apply_park(
                        Effect::Park {
                            target,
                            info,
                            mode: Mode::Stop,
                        },
                        window,
                        cx,
                    );
                }
                if let Some(state) = self
                    .workspaces
                    .iter_mut()
                    .find(|state| Some(state.workspace.id) == finish_id)
                {
                    state.parked.as_mut().unwrap().done = true;
                }
            }
            Effect::Close { target, key } => {
                if self.park_projection(cx).resolve(&target).is_none() {
                    return;
                }
                self.apply_park(
                    Effect::Park {
                        target,
                        mode: Mode::Stop,
                        info: Info::new("", String::new()),
                    },
                    window,
                    cx,
                );
                let done = self.park_projection(cx).reduce(Intent::Done(key));
                self.apply_park(done, window, cx);
            }
        }
    }
    fn save_parked(&self, cx: &mut Context<Self>) {
        let workspaces = self
            .workspaces
            .iter()
            .filter_map(|state| {
                let parked = state.parked.as_ref()?;
                let key = state.park_key(cx);
                Some(SavedPark {
                    name: key.name,
                    dir: key.dir.into(),
                    note: parked.info.note.clone(),
                    parked_at: parked.info.parked_at.clone(),
                    done: parked.done,
                })
            })
            .collect();
        self.owner
            .update(cx, |owner, cx| owner.save_parked(workspaces, cx));
    }
}

impl Shell {
    pub(crate) fn workspace_sheet(&self, cx: &App) -> Option<crate::park_sheet::WorkspaceSheet> {
        let workspace = self.active_workspace(cx)?;
        let state = self
            .workspaces
            .iter()
            .find(|s| s.workspace.id == workspace)?;
        let chats = state
            .sessions
            .iter()
            .filter(|s| s.kind == crate::data::SessionKind::Chat)
            .count();
        let terminals = state
            .sessions
            .iter()
            .filter(|s| s.kind == crate::data::SessionKind::Terminal)
            .count();
        let working_chat = state
            .panes
            .values()
            .any(|pane| match &pane.read(cx).content {
                crate::pane::PaneContent::PreviewChat(view) => view.read(cx).is_working(),
                _ => false,
            });
        let owner = self.owner.read(cx);
        let saved = owner.iced_workspace(workspace);
        let others: Vec<_> = self
            .workspaces
            .iter()
            .filter(|s| s.workspace.id != workspace)
            .filter_map(|s| owner.iced_workspace(s.workspace.id))
            .collect();
        // Iced lists a workspace's repos by dir; a file without the list has `[dir]`.
        let repos_of = |config: &gitterm_core::workspace_config::WorkspaceConfig| {
            if config.repos.is_empty() {
                vec![config.dir.clone()]
            } else {
                config.repos.clone()
            }
        };
        let shared_repos: Vec<(String, String)> = saved
            .map(|saved| {
                repos_of(saved)
                    .into_iter()
                    .filter_map(|dir| {
                        let other = others.iter().find(|other| repos_of(other).contains(&dir))?;
                        let label = std::path::Path::new(&dir)
                            .file_name()
                            .map_or_else(|| dir.clone(), |name| name.to_string_lossy().into());
                        Some((label, other.name.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let own_roots = saved.map(|saved| saved.chats.clone()).unwrap_or_default();
        let own_tab_ids: Vec<String> = saved
            .map(|saved| {
                saved
                    .tabs
                    .iter()
                    .filter_map(|tab| tab.chat_session_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let other_roots: Vec<String> = others
            .iter()
            .flat_map(|other| other.chats.iter().cloned())
            .collect();
        let entries = self
            .work_sidebars
            .get(&workspace)
            .map(|sidebar| sidebar.read(cx).indexed_chats())
            .unwrap_or_default();
        let note_chats =
            crate::park_sheet::note_chats(entries, &own_roots, &own_tab_ids, &other_roots);
        crate::park_sheet::WorkspaceSheet::open(
            &self.park_projection(cx),
            state.runtime_uid,
            crate::park_sheet::WorkspaceFacts {
                shared_repos: &shared_repos,
                note_chats: &note_chats,
                owns_chats: chats > 0,
                terminals,
                chats,
                working_chat,
                others_have_chats: self
                    .workspaces
                    .iter()
                    .filter(|s| s.workspace.id != workspace)
                    .any(|s| {
                        s.sessions
                            .iter()
                            .any(|s| s.kind == crate::data::SessionKind::Chat)
                    }),
                ..Default::default()
            },
        )
    }
    pub(crate) fn open_park_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::AppContext as _;
        if self.park_sheet.is_some() || self.settings_sheet.is_some() {
            return;
        }
        let Some(model) = self.workspace_sheet(cx) else {
            return;
        };
        let target = model.target.clone();
        let previous_focus = if self.session_picker.is_some() {
            self.focus.clone()
        } else {
            window.focused(cx).unwrap_or_else(|| self.focus.clone())
        };
        self.session_picker = None;
        self.picker_subscription = None;
        self.cover_sheet(true, window, cx);
        let sheet = cx.new(|cx| crate::park_sheet::ParkSheet::workspace(model, window, cx));
        self.park_subscription = Some(cx.subscribe_in(
            &sheet,
            window,
            move |shell, _, outcome: &crate::park_sheet::Outcome, window, cx| {
                shell.park_sheet = None;
                shell.park_subscription = None;
                shell.cover_sheet(false, window, cx);
                let stamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .to_string();
                if let Some(intent) = outcome.intent(Some(&target), stamp) {
                    shell.park_intent(Some(&target), intent, window, cx);
                } else {
                    previous_focus.focus(window, cx);
                }
                cx.notify();
            },
        ));
        self.park_sheet = Some(sheet);
        cx.notify();
    }
}

impl Shell {
    /// Iced's Parked list order: live parks first, then stopped and saved
    /// entries newest first (stamp-less entries keep their order, last).
    pub(crate) fn parked_list_rows(&self, cx: &App) -> Vec<crate::park_sheet::ParkedRow> {
        use crate::park_sheet::ParkedRow;
        let colors =
            crate::workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark());
        let hsla = |color: u32| gpui::rgb(color).into();
        let owner = self.owner.read(cx);
        let mut running = vec![];
        let mut closed: Vec<(Option<SystemTime>, ParkedRow)> = vec![];
        for state in &self.workspaces {
            let Some(parked) = state.parked.as_ref().filter(|parked| !parked.done) else {
                continue;
            };
            let saved = owner.iced_workspace(state.workspace.id);
            let row = ParkedRow {
                id: state.park_key(cx),
                chats: saved.map_or_else(|| state.chat_count(), |saved| saved.chats.len()),
                info: Some(parked.info.clone()),
                running: parked.running,
                color: hsla(
                    colors.workspace(
                        saved
                            .map(|saved| saved.color)
                            .unwrap_or(gitterm_core::settings::WorkspaceColor::Lavender),
                    ),
                ),
                pr_news: None,
            };
            if parked.running {
                running.push(row);
            } else {
                let at = gitterm_core::work::summary::parse_stamp(&parked.info.parked_at);
                closed.push((at, row));
            }
        }
        for saved in self.saved_facts.snapshot().parked_rows {
            let at = saved
                .info
                .as_ref()
                .and_then(|info| gitterm_core::work::summary::parse_stamp(&info.parked_at));
            closed.push((
                at,
                ParkedRow {
                    id: saved.key,
                    chats: saved.chats,
                    info: saved.info,
                    running: false,
                    color: hsla(saved.color),
                    pr_news: saved.news.map(|news| (news.label, hsla(news.color))),
                },
            ));
        }
        closed.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        running
            .into_iter()
            .chain(closed.into_iter().map(|(_, row)| row))
            .collect()
    }
    fn mount_list_sheet(
        &mut self,
        sheet: gpui::Entity<crate::park_sheet::ParkSheet>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        self.session_picker = None;
        self.picker_subscription = None;
        self.cover_sheet(true, window, cx);
        self.park_subscription = Some(cx.subscribe_in(
            &sheet,
            window,
            move |shell, sheet, outcome: &crate::park_sheet::Outcome, window, cx| {
                use crate::park_sheet::Outcome;
                if matches!(outcome, Outcome::Done { .. }) {
                    // Iced keeps the list open and refreshes its rows.
                    if let Outcome::Done { id } = outcome {
                        shell.done_parked(id.clone(), window, cx);
                    }
                    let rows = shell.parked_list_rows(cx);
                    sheet.update(cx, |sheet, cx| sheet.set_rows(rows, cx));
                    return;
                }
                shell.park_sheet = None;
                shell.park_subscription = None;
                shell.cover_sheet(false, window, cx);
                previous_focus.focus(window, cx);
                match outcome {
                    Outcome::Resume { id } => shell.resume_parked(id.clone(), window, cx),
                    Outcome::ReadyGo(crate::park_sheet::ReadyHome::Open(id)) => {
                        shell.pick_workspace_id(*id, window, cx)
                    }
                    Outcome::ReadyGo(crate::park_sheet::ReadyHome::Parked(_)) => {
                        shell.chrome_unavailable("Resume", cx)
                    }
                    _ => {}
                }
                cx.notify();
            },
        ));
        self.park_sheet = Some(sheet);
        cx.notify();
    }
    pub(crate) fn open_parked_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::AppContext as _;
        if self.park_sheet.is_some() || self.settings_sheet.is_some() {
            return;
        }
        let rows = self.parked_list_rows(cx);
        let sheet = cx.new(|cx| crate::park_sheet::ParkSheet::parked(rows, window, cx));
        self.mount_list_sheet(sheet, window, cx);
    }
    pub(crate) fn open_ready_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::AppContext as _;
        if self.park_sheet.is_some() || self.settings_sheet.is_some() {
            return;
        }
        let groups = self.saved_facts.snapshot().ready_groups;
        let sheet = cx.new(|cx| crate::park_sheet::ParkSheet::ready(groups, window, cx));
        self.mount_list_sheet(sheet, window, cx);
    }
    fn own_parked(&self, key: &Key, cx: &App) -> bool {
        self.workspaces
            .iter()
            .any(|state| state.parked.is_some() && &state.park_key(cx) == key)
    }
    /// Resume restores this preview's own parks; plain terminals restart, but
    /// chats are a launch, and an Iced-saved park would write Iced's file.
    fn resume_parked(&mut self, id: Key, window: &mut Window, cx: &mut Context<Self>) {
        if !self.own_parked(&id, cx) {
            self.chrome_unavailable("Resume", cx);
            return;
        }
        let had_chats = self
            .workspaces
            .iter()
            .any(|state| state.park_key(cx) == id && state.chat_count() > 0);
        self.park_intent(None, Intent::Resume(id), window, cx);
        if had_chats {
            self.chrome_unavailable("Resume chats", cx);
        }
    }
    fn done_parked(&mut self, id: Key, window: &mut Window, cx: &mut Context<Self>) {
        if !self.own_parked(&id, cx) {
            self.chrome_unavailable("Done", cx);
            return;
        }
        let target = self
            .workspaces
            .iter()
            .find(|state| state.park_key(cx) == id)
            .map(|state| state.park_target(cx));
        self.park_intent(target.as_ref(), Intent::Done(id), window, cx);
    }
}
