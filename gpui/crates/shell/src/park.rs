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
    pub(crate) fn parked_controls(&self, cx: &Context<Self>) -> gpui::AnyElement {
        use gpui::{IntoElement as _, ParentElement as _, Styled as _, div};
        use gpui_component::button::{Button, ButtonVariants as _};
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.workspaces.iter().filter_map(|state| {
                let parked = state.parked.as_ref().filter(|parked| !parked.done)?;
                let key = state.park_key(cx);
                let resume = key.clone();
                let done = key;
                let target = state.park_target(cx);
                let stop = target.clone();
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(format!(
                            "{} · {}",
                            state.workspace.name,
                            if parked.running {
                                "Parked · running"
                            } else {
                                "Parked · stopped"
                            }
                        ))
                        .child(
                            Button::new(("resume-workspace", state.workspace.id.0))
                                .ghost()
                                .label("Resume")
                                .on_click(cx.listener(move |shell, _, window, cx| {
                                    shell.park_intent(
                                        None,
                                        Intent::Resume(resume.clone()),
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .children(parked.running.then(|| {
                            Button::new(("stop-workspace", state.workspace.id.0))
                                .ghost()
                                .label("Stop terminals")
                                .on_click(cx.listener(move |shell, _, window, cx| {
                                    shell.park_workspace(&stop, Mode::Stop, window, cx)
                                }))
                        }))
                        .child(
                            Button::new(("done-workspace", state.workspace.id.0))
                                .ghost()
                                .label("Mark done")
                                .on_click(cx.listener(move |shell, _, window, cx| {
                                    shell.park_intent(
                                        Some(&target),
                                        Intent::Done(done.clone()),
                                        window,
                                        cx,
                                    )
                                })),
                        ),
                )
            }))
            .into_any_element()
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
