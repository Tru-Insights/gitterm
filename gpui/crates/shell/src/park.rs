//! Workspace parking adapters. Live saves through DesktopStore before applying
//! process/UI effects; Preview retains its isolated terminal-only metadata.
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
    pub(crate) fn is_parked(&self) -> bool {
        !self.done
    }
}

pub(crate) fn parked_count(
    closed_parked: usize,
    saved: &[SavedPark],
    hidden: usize,
    running_parked: usize,
) -> usize {
    closed_parked
        + saved.iter().filter(|record| record.is_parked()).count()
        + hidden
        + running_parked
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
            .find_map(|pane| pane.read(cx).workspace_dir(cx));
        Key {
            name: self.workspace.name.clone(),
            dir: dir.unwrap_or_default().to_string_lossy().into_owned(),
        }
    }

    pub(crate) fn park_target(&self, cx: &App) -> Target {
        let mut tabs: Vec<_> = self
            .panes
            .values()
            .filter_map(|pane| match &pane.read(cx).content {
                crate::pane::PaneContent::PreviewChat(view) => Some(view.read(cx).target()),
                _ => pane.read(cx).session.clone(),
            })
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
        if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_some()
        {
            if let Some(state) = self
                .workspaces
                .iter()
                .find(|w| w.workspace.id == id && w.parked.is_some())
            {
                let key = state.park_key(cx);
                self.resume_live_workspace(key, true, window, cx);
            }
            return;
        }
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
    pub(crate) fn select_bar_workspace(&mut self, cx: &mut Context<Self>) {
        if self
            .workspaces
            .iter()
            .any(|state| state.on_bar() && Some(state.workspace.id) == self.active_workspace(cx))
        {
            return;
        }
        let order = if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_some()
        {
            self.owner
                .read(cx)
                .iced_catalog()
                .map(|c| c.ordered_indices())
                .unwrap_or_default()
        } else {
            (0..self.workspaces.len()).collect()
        };
        let from = order
            .iter()
            .position(|&i| Some(self.workspaces[i].workspace.id) == self.active_workspace(cx))
            .unwrap_or(0);
        let parked: Vec<_> = order
            .iter()
            .map(|&i| !self.workspaces[i].on_bar())
            .collect();
        if let Some(id) = gitterm_core::workspace::next_on_bar(from, &parked)
            .map(|index| self.workspaces[order[index]].workspace.id)
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
        if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_some()
        {
            self.save_live_park(effect, window, cx);
            return;
        }
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
                // Live has just promoted this inventory into the shared DTOs.
                // Retaining its preview copy would restore each terminal twice.
                if cx
                    .try_global::<crate::workspace_config::LiveWorkspaces>()
                    .is_some()
                {
                    state.store.terminals = Some(vec![]);
                }
                crate::save_layout(state, cx);
                if mode == Mode::Stop {
                    for pane in state.panes.values() {
                        if let crate::pane::PaneContent::PreviewChat(view) = &pane.read(cx).content
                        {
                            let view = view.clone();
                            let target = view.read(cx).target();
                            view.update(cx, |view, cx| view.close(cx));
                            self.owner.update(cx, |owner, _| {
                                owner.chats.remove(&target.session_uid);
                            });
                            pane.update(cx, |pane, _| {
                                pane.content =
                                    crate::pane::PaneContent::IcedSession("Stopped chat".into());
                            });
                        }
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
                if shell.owner.read(cx).workspace_save_pending {
                    return;
                }
                let live = cx
                    .try_global::<crate::workspace_config::LiveWorkspaces>()
                    .is_some();
                let stamp = if live {
                    chrono::Utc::now().to_rfc3339()
                } else {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .to_string()
                };
                let intent = outcome.intent(Some(&target), stamp);
                if live && let Some(intent) = intent {
                    shell.park_intent(Some(&target), intent, window, cx);
                    return;
                }
                shell.park_sheet = None;
                shell.park_subscription = None;
                shell.cover_sheet(false, window, cx);
                if let Some(intent) = intent {
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
            if cx
                .try_global::<crate::workspace_config::LiveWorkspaces>()
                .is_some()
                && self.own_parked(&saved.key, cx)
            {
                continue;
            }
            if cx
                .try_global::<crate::workspace_config::LiveWorkspaces>()
                .is_some()
                && !owner.iced_catalog().is_some_and(|catalog| {
                    catalog
                        .closed
                        .iter()
                        .any(|w| saved.key.matches(w) && w.is_parked())
                })
            {
                continue;
            }
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
                if shell.owner.read(cx).workspace_save_pending {
                    return;
                }
                if cx
                    .try_global::<crate::workspace_config::LiveWorkspaces>()
                    .is_some()
                    && let Outcome::Resume { id }
                    | Outcome::ReadyGo(crate::park_sheet::ReadyHome::Parked(id)) = outcome
                {
                    shell.resume_live_workspace(id.clone(), false, window, cx);
                    return;
                }
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
        if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_some()
        {
            self.resume_live_workspace(id, false, window, cx);
            return;
        }
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
        if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_some()
            && !self.own_parked(&id, cx)
        {
            self.change_live_workspace(
                crate::workspace_config::LiveWorkspaceAction::Done(id),
                crate::sessions::WorkspaceCompletion::Park(Effect::None),
                window,
                cx,
            );
            return;
        }
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

impl Shell {
    pub(crate) fn resume_live_workspace(
        &mut self,
        key: Key,
        reopen: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self
            .workspaces
            .iter()
            .find(|w| w.park_key(cx) == key && w.parked.as_ref().is_some_and(|p| p.running))
        {
            let effect = self
                .park_projection(cx)
                .reduce(Intent::Resume(state.park_key(cx)));
            self.save_live_park(effect, window, cx);
        } else {
            let open = self
                .workspaces
                .iter()
                .filter(|w| w.on_bar() || w.parked.as_ref().is_some_and(|p| p.running))
                .map(|w| w.park_key(cx))
                .collect();
            self.change_live_workspace(
                crate::workspace_config::LiveWorkspaceAction::Resume { key, reopen, open },
                crate::sessions::WorkspaceCompletion::Open { created: false },
                window,
                cx,
            );
        }
    }
    fn save_live_park(&mut self, effect: Effect, window: &mut Window, cx: &mut Context<Self>) {
        use crate::workspace_config::LiveWorkspaceAction as A;
        let key_of = |uid| {
            self.workspaces
                .iter()
                .find(|w| w.runtime_uid == uid)
                .map(|w| w.park_key(cx))
        };
        let action = match &effect {
            Effect::Park { target, info, .. } => key_of(target.uid).map(|key| A::Park {
                active_tab: self.live_active_tab(target.uid),
                tabs: self.live_workspace_tabs(target.uid, cx),
                key,
                info: info.clone(),
                done: false,
            }),
            Effect::Return { uid, .. } => key_of(*uid).map(|key| {
                let order = self
                    .owner
                    .read(cx)
                    .iced_catalog()
                    .map(|c| c.ordered_indices())
                    .unwrap_or_default();
                let ordinal = order
                    .iter()
                    .filter(|&&i| {
                        self.workspaces[i].on_bar() || self.workspaces[i].runtime_uid == *uid
                    })
                    .position(|&i| self.workspaces[i].runtime_uid == *uid)
                    .unwrap_or(0);
                A::Return { key, ordinal }
            }),
            Effect::Done {
                stop: Some((target, info)),
                ..
            } => key_of(target.uid).map(|key| A::Park {
                active_tab: self.live_active_tab(target.uid),
                tabs: self.live_workspace_tabs(target.uid, cx),
                key,
                info: info.clone(),
                done: true,
            }),
            Effect::Done { stop: None, finish } => self
                .workspaces
                .iter()
                .filter(|w| w.parked.as_ref().is_some_and(|p| !p.running))
                .nth(*finish)
                .map(|w| A::Done(w.park_key(cx))),
            Effect::Close { key, target } => Some(A::Park {
                active_tab: self.live_active_tab(target.uid),
                tabs: self.live_workspace_tabs(target.uid, cx),
                key: key.clone(),
                info: Info::new("", chrono::Utc::now().to_rfc3339()),
                done: true,
            }),
            _ => None,
        };
        if let Some(action) = action {
            self.change_live_workspace(
                action,
                crate::sessions::WorkspaceCompletion::Park(effect),
                window,
                cx,
            );
        }
    }
}

impl Shell {
    fn live_active_tab(&self, uid: u64) -> Option<usize> {
        self.workspaces
            .iter()
            .find(|w| w.runtime_uid == uid)
            .and_then(|w| {
                w.sessions
                    .iter()
                    .position(|s| Some(s.id) == w.active_session)
            })
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use gpui::AppContext as _;

    #[gpui::test]
    fn live_stop_and_keep_running_apply_only_after_save(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let desktop = root.join("desktop");
        let home = root.join("home");
        std::fs::create_dir(&desktop).unwrap();
        std::fs::create_dir(&home).unwrap();
        let folders: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|name| {
                let dir = home.join(name);
                std::fs::create_dir(&dir).unwrap();
                dir
            })
            .collect();
        let document = serde_json::json!({"active_workspace":0,"workspaces":folders.iter().map(|dir| serde_json::json!({"name":dir.file_name().unwrap().to_str().unwrap(),"abbrev":"FX","dir":dir,"color":"blue","tabs":[]})).collect::<Vec<_>>()});
        std::fs::write(
            desktop.join("workspaces.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let live = gitterm_core::desktop_store::DesktopStore::open(&desktop).unwrap();
        cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(live.clone())));
        let mut store = crate::preview::tests::guarded_store(&root.join("preview"));
        store.iced = Some(crate::workspace_config::IcedCatalog::read(&desktop).unwrap());
        let model =
            crate::workspace_status::WorkspaceStatus::from_iced(store.iced.as_ref().unwrap());
        let layout = root.join("preview");
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            let data = std::rc::Rc::new(crate::data::LocalData(owner.clone()));
            Shell::with_owner(data, owner, &layout, true, window, cx).unwrap()
        });
        let (terminal, key) = cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.new_terminal(window, cx);
                let state = &shell.workspaces[0];
                let pane =
                    state.panes[&crate::session_pane_id(state.active_session.unwrap())].clone();
                let crate::pane::PaneContent::Terminal(terminal) = &pane.read(cx).content else {
                    panic!("terminal")
                };
                let terminal = terminal.clone();
                let target = state.park_target(cx);
                let key = state.park_key(cx);
                let before = std::fs::read(desktop.join("workspaces.json")).unwrap();
                // Opening and cancelling the park sheet has no persistence effect.
                shell.open_park_sheet(window, cx);
                shell.park_sheet = None;
                shell.park_subscription = None;
                shell.cover_sheet(false, window, cx);
                assert_eq!(
                    std::fs::read(desktop.join("workspaces.json")).unwrap(),
                    before
                );
                shell.park_workspace(&target, Mode::KeepRunning, window, cx);
                assert!(shell.workspaces[0].parked.is_none());
                (terminal, key)
            })
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert!(
                shell.read(cx).workspaces[0]
                    .parked
                    .as_ref()
                    .unwrap()
                    .running
            );
            assert_ne!(
                terminal.read(cx).backend().state(),
                gitterm_terminal::SessionState::Cancelled
            );
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.resume_live_workspace(key.clone(), false, window, cx)
            })
        });
        cx.run_until_parked();
        cx.read(|cx| assert!(shell.read(cx).workspaces[0].parked.is_none()));
        {
            use std::os::unix::fs::PermissionsExt;
            let bytes = std::fs::read(desktop.join("workspaces.json")).unwrap();
            std::fs::set_permissions(&desktop, std::fs::Permissions::from_mode(0o555)).unwrap();
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| {
                    let target = shell.workspaces[0].park_target(cx);
                    shell.park_workspace(&target, Mode::Stop, window, cx);
                })
            });
            cx.run_until_parked();
            std::fs::set_permissions(&desktop, std::fs::Permissions::from_mode(0o755)).unwrap();
            cx.read(|cx| {
                assert!(shell.read(cx).workspaces[0].parked.is_none());
                assert!(shell.read(cx).owner.read(cx).error.is_some());
                assert_ne!(
                    terminal.read(cx).backend().state(),
                    gitterm_terminal::SessionState::Cancelled
                );
            });
            assert_eq!(
                std::fs::read(desktop.join("workspaces.json")).unwrap(),
                bytes
            );
        }
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                let target = shell.workspaces[0].park_target(cx);
                shell.park_workspace(&target, Mode::Stop, window, cx);
                assert!(shell.workspaces[0].parked.is_none());
            })
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert!(
                !shell.read(cx).workspaces[0]
                    .parked
                    .as_ref()
                    .unwrap()
                    .running
            );
            assert_eq!(
                terminal.read(cx).backend().state(),
                gitterm_terminal::SessionState::Cancelled
            );
            assert_eq!(shell.read(cx).data.workspaces(cx).len(), 2);
        });
        let saved: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&std::fs::read(desktop.join("workspaces.json")).unwrap())
                .unwrap();
        let stopped = saved.workspaces.iter().find(|w| w.name == "one").unwrap();
        assert!(!stopped.active);
        assert!(!stopped.tabs.is_empty());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.resume_live_workspace(key.clone(), false, window, cx)
            })
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(shell.read(cx).visible_workspace_indices(cx), [1, 0]);
        });
        let resumed: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&std::fs::read(desktop.join("workspaces.json")).unwrap())
                .unwrap();
        assert_eq!(
            resumed
                .workspaces
                .iter()
                .filter(|w| w.active)
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["two", "one"]
        );
        assert_eq!(resumed.active_workspace, 1);
        live.drain().wait().unwrap();
    }
    #[gpui::test]
    fn done_on_a_saved_park_does_not_add_a_runtime_workspace(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = tempfile::tempdir().unwrap();
        let file = serde_json::from_value(serde_json::json!({"active_workspace":0,"workspaces":[
            {"name":"Open","abbrev":"O","dir":"/fixture/open","color":"blue","tabs":[]},
            {"name":"Saved","abbrev":"S","dir":"/fixture/saved","color":"blue","tabs":[],"active":false,"done_at":"2026-11-27T00:00:00Z"}
        ]})).unwrap();
        let catalog = crate::workspace_config::IcedCatalog::from_document(root.path(), file);
        let model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
        let mut store = crate::workspace_config::WorkspaceStore::new(root.path());
        let saved = crate::workspace_config::LiveWorkspaceSaved {
            workspace: catalog.closed[0].clone(),
            closed: catalog.closed.clone(),
            repos: catalog.repos.clone(),
        };
        store.iced = Some(catalog);
        cx.update(|cx| {
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            let id = owner.update(cx, |owner, cx| owner.publish_workspace(saved, false, cx));
            assert!(id.is_none());
            assert_eq!(owner.read(cx).model.workspaces.len(), 1);
            assert_eq!(owner.read(cx).iced_catalog().unwrap().workspaces.len(), 1);
            assert!(
                owner.read(cx).iced_catalog().unwrap().closed[0]
                    .done_at
                    .is_some()
            );
        });
    }
    #[gpui::test]
    fn confirmed_tab_publishes_only_after_save_and_failed_save_keeps_catalog(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::os::unix::fs::PermissionsExt;
        cx.background_executor.allow_parking();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        std::fs::write(path.join("workspaces.json"), br#"{"active_workspace":0,"workspaces":[{"name":"Open","abbrev":"O","dir":"/fixture/open","color":"blue","tabs":[]}]}"#).unwrap();
        let desktop = gitterm_core::desktop_store::DesktopStore::open(&path).unwrap();
        let mut store = crate::workspace_config::WorkspaceStore::new(&path);
        store.iced = Some(crate::workspace_config::IcedCatalog::read(&path).unwrap());
        let model =
            crate::workspace_status::WorkspaceStatus::from_iced(store.iced.as_ref().unwrap());
        let tab = crate::chat::resume::tab(
            std::path::Path::new("/fixture/open"),
            "caller".into(),
            Some("owned".into()),
        );
        let owner = cx.update(|cx| {
            cx.set_global(crate::workspace_config::LiveWorkspaces(desktop.clone()));
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            owner.update(cx, |owner, cx| {
                owner.confirm_live_tab("Open", "/fixture/open", tab.clone(), cx)
            });
            assert!(
                owner.read(cx).iced_catalog().unwrap().workspaces[0]
                    .tabs
                    .is_empty()
            );
            owner
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                owner.read(cx).iced_catalog().unwrap().workspaces[0].tabs[0]
                    .chat_session_id
                    .as_deref(),
                Some("owned")
            )
        });
        let bytes = std::fs::read(path.join("workspaces.json")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut changed = tab;
        changed.chat_session_id = Some("changed".into());
        cx.update(|cx| {
            owner.update(cx, |owner, cx| {
                owner.confirm_live_tab("Open", "/fixture/open", changed, cx)
            })
        });
        cx.run_until_parked();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        cx.read(|cx| {
            assert!(owner.read(cx).error.is_some());
            assert_eq!(
                owner.read(cx).iced_catalog().unwrap().workspaces[0].tabs[0]
                    .chat_session_id
                    .as_deref(),
                Some("owned")
            );
        });
        assert_eq!(std::fs::read(path.join("workspaces.json")).unwrap(), bytes);
        desktop.drain().wait().unwrap();
    }
}
