#![cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(test)]
mod center_tests;
pub mod data;
mod dialogs;
pub mod files;
mod files_view;
mod layout;
mod owner;
mod pane;
mod review_reload;
mod theme;
pub mod workspace_config;
pub mod workspace_status;
pub use owner::{FileOpen, WorkspaceOwner};
use std::{cell::RefCell, time::Instant};
use workspace_status::{Intent, WorkspaceStatus};

use anyhow::{Result, ensure};
use data::{Session, SessionId, ShellData, Workspace, WorkspaceId};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, Anchor, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render,
    Styled as _, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    px, rgb, size,
};
use gpui_component::{
    Root, Selectable,
    button::{Button, ButtonVariants},
    dock::{
        DockArea, DockEvent, DockLayout, DockPlacement, DockSkin, PanelId, PanelStyle,
        panel_handle, register_panel,
    },
    menu::{DropdownMenu, PopupMenuItem},
    tab::{Tab, TabBar},
};
use layout::LayoutStore;
use pane::{PANEL_NAME, Pane, PaneKind};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
};
use theme::{Palette, UI_FONT_SIZE};

#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
struct SelectSession {
    index: usize,
}
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
struct SelectWorkspace {
    index: usize,
}
actions!(gitterm_shell, [ShowShortcuts, NewTerminal]);

struct WorkspaceDock {
    workspace: Workspace,
    sessions: Vec<Session>,
    active_session: Option<SessionId>,
    dock: Entity<DockArea>,
    // Lifetime owner: dock removal hides a pane; session close releases it.
    panes: HashMap<String, Entity<Pane>>,
    store: LayoutStore,
    error: Option<String>,
}
pub struct Shell {
    data: Rc<dyn ShellData>,
    workspaces: Vec<WorkspaceDock>,
    owner: Entity<WorkspaceOwner>,
    registry: Rc<RefCell<HashMap<String, gpui::WeakEntity<Pane>>>>,
    config_dir: std::path::PathBuf,
    local: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
impl Shell {
    pub fn new(
        data: Rc<dyn ShellData>,
        config_dir: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        let model = WorkspaceStatus::sample(data.workspaces(cx).into_iter().map(|w| w.id));
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        Self::with_owner(data, owner, config_dir, false, window, cx)
    }
    fn with_owner(
        data: Rc<dyn ShellData>,
        owner: Entity<WorkspaceOwner>,
        config_dir: &Path,
        local: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        gitterm_gpui_chat::init(cx);
        gitterm_gpui_review::init(cx);
        if local {
            cx.bind_keys([KeyBinding::new("cmd-t", NewTerminal, Some("GitTermShell"))]);
        }
        let catalog = data.workspaces(cx);
        if let Some(first) = catalog.first() {
            owner.update(cx, |owner, cx| {
                owner.dispatch(Intent::Activate(first.id), Instant::now(), cx)
            });
        }
        let mut workspace_ids = HashSet::new();
        let mut session_ids = HashSet::new();
        let mut workspaces = vec![];
        for workspace in catalog {
            ensure!(
                workspace_ids.insert(workspace.id),
                "duplicate workspace ID {:?}",
                workspace.id
            );
            workspaces.push(build_workspace(
                workspace,
                data.clone(),
                config_dir,
                &mut session_ids,
                window,
                cx,
            )?);
        }
        // Restoration resolves stable persisted IDs to the SAME entities. It never
        // constructs panes, even when a layout is loaded again within this process.
        let handles: HashMap<_, _> = workspaces
            .iter()
            .flat_map(|w| w.panes.iter().map(|(id, p)| (id.clone(), p.downgrade())))
            .collect();
        let registry = Rc::new(RefCell::new(handles));
        let handles = registry.clone();
        register_panel(cx, PANEL_NAME, move |context, _, _| {
            let gpui_component::dock::PanelInfo::Panel(value) = context.info() else {
                unreachable!("registry only builds leaves")
            };
            let id = value.as_str().expect("layout IDs validated before load");
            panel_handle(
                handles
                    .borrow()
                    .get(id)
                    .expect("layout IDs validated before load")
                    .upgrade()
                    .expect("shell owns pane until window closes"),
            )
        });
        let mut subscriptions = vec![];
        for state in &mut workspaces {
            load_workspace_layout(state, window, cx)?;
            subscriptions.push(subscribe_dock(state, cx));
            if local {
                for pane in state.panes.values() {
                    subscriptions.push(cx.observe(pane, |_, _, cx| cx.notify()));
                }
            }
        }
        subscriptions.push(cx.observe(&owner, |this, _, cx| {
            for workspace in &this.workspaces {
                for pane in workspace.panes.values() {
                    pane.update(cx, |_, cx| cx.notify());
                }
            }
            cx.notify();
        }));
        subscriptions.push(cx.subscribe_in(
            &owner,
            window,
            |this, _, event: &FileOpen, window, cx| {
                this.open_review(event, window, cx);
            },
        ));
        if local {
            owner.update(cx, |owner, cx| {
                owner.dispatch(Intent::Opened, Instant::now(), cx)
            });
        }
        Ok(Self {
            data,
            workspaces,
            owner,
            registry,
            config_dir: config_dir.to_path_buf(),
            local,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        })
    }
    fn open_review(&mut self, event: &FileOpen, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = &event.changed else { return };
        if !self.local {
            return;
        }
        let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == event.workspace)
        else {
            return;
        };
        let id = format!("workspace-{}-review", state.workspace.layout_key);
        let mut created = false;
        let pane = state
            .panes
            .entry(id.clone())
            .or_insert_with(|| {
                created = true;
                let review = cx
                    .new(|cx| gitterm_gpui_review::ReviewView::new(Default::default(), window, cx));
                self.owner.update(cx, |owner, _| {
                    owner.reviews.insert(event.workspace, review.clone());
                });
                let pane = cx.new(|cx| {
                    Pane::new(
                        id.clone(),
                        "Review".into(),
                        PaneKind::Review(review),
                        event.workspace,
                        self.data.clone(),
                        window,
                        cx,
                    )
                });
                self.registry
                    .borrow_mut()
                    .insert(id.clone(), pane.downgrade());
                pane
            })
            .clone();
        let pane::PaneContent::Review(review) = &pane.read(cx).content else {
            unreachable!()
        };
        let review = review.clone();
        use gitterm_gpui_review::model::Group;
        let group = if file.staged {
            Group::Staged
        } else if file.status == "??" {
            Group::Untracked
        } else {
            Group::Unstaged
        };
        review.update(cx, |review, cx| {
            review.select_file((group, file.path.clone()), cx)
        });
        if created || review.read(cx).model().error.is_some() {
            self.owner
                .update(cx, |owner, cx| owner.reload_review(event.workspace, cx));
        }
        // Use the event's workspace even if selection changed before delivery.
        self.show_workspace_pane(event.workspace, &id, window, cx);
    }
    fn add_folder_button(&self, cx: &Context<Self>) -> Button {
        Button::new("add-folder")
            .ghost()
            .label("Add folder…")
            .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx)))
    }
    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let response = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add workspace folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = match response.await {
                Ok(Ok(Some(paths))) => {
                    cx.background_executor()
                        .spawn(async move {
                            paths
                                .first()
                                .map(|path| workspace_config::local_folder(path))
                                .transpose()
                        })
                        .await
                }
                Ok(Ok(None)) => Ok(None),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            };
            _ = this.update_in(cx, |this, window, cx| {
                let result = result.and_then(|folder| {
                    if let Some(folder) = folder {
                        this.add_folder(folder, window, cx)?;
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    let error = format!("Add folder: {error:#}");
                    eprintln!("{error}");
                    this.owner.update(cx, |owner, cx| {
                        owner.error = Some(error);
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }
    fn add_folder(
        &mut self,
        folder: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let id = self
            .owner
            .update(cx, |owner, cx| owner.add_folder(folder, cx))?;
        if !self.workspaces.iter().any(|w| w.workspace.id == id) {
            let workspace = self
                .data
                .workspaces(cx)
                .into_iter()
                .find(|w| w.id == id)
                .expect("added workspace");
            let mut session_ids = self
                .workspaces
                .iter()
                .flat_map(|w| w.sessions.iter().map(|s| s.id.0))
                .collect();
            let mut state = build_workspace(
                workspace,
                self.data.clone(),
                &self.config_dir,
                &mut session_ids,
                window,
                cx,
            )?;
            self.registry.borrow_mut().extend(
                state
                    .panes
                    .iter()
                    .map(|(id, p)| (id.clone(), p.downgrade())),
            );
            load_workspace_layout(&mut state, window, cx)?;
            self._subscriptions.push(subscribe_dock(&state, cx));
            for pane in state.panes.values() {
                self._subscriptions
                    .push(cx.observe(pane, |_, _, cx| cx.notify()));
            }
            self.workspaces.push(state);
        }
        self.sync_git_visibility(false, cx);
        self.focus.focus(window, cx);
        cx.notify();
        Ok(())
    }
    fn sync_git_visibility(&self, enter: bool, cx: &mut Context<Self>) {
        let visible = self
            .workspaces
            .iter()
            .find(|w| Some(w.workspace.id) == self.active_workspace(cx))
            .is_some_and(|w| pane_visible(w, "git", cx));
        let reviews: Vec<_> = self
            .workspaces
            .iter()
            .map(|w| {
                (
                    w.workspace.id,
                    Some(w.workspace.id) == self.active_workspace(cx)
                        && pane_visible(w, "review", cx),
                )
            })
            .collect();
        self.owner.update(cx, |owner, cx| {
            owner.set_git_visible(visible, enter, cx);
            for (workspace, visible) in reviews {
                owner.set_review_visible(workspace, visible, cx);
            }
        });
    }
    fn active_workspace(&self, cx: &App) -> Option<WorkspaceId> {
        self.owner.read(cx).model.active()
    }
    fn active(&self, cx: &App) -> &WorkspaceDock {
        self.workspaces
            .iter()
            .find(|w| Some(w.workspace.id) == self.active_workspace(cx))
            .expect("active workspace exists")
    }
    fn select_workspace(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.get(index).is_some() {
            self.owner.update(cx, |owner, cx| {
                owner.dispatch(Intent::SelectIndex(index), Instant::now(), cx)
            });
            self.sync_git_visibility(false, cx);
            self.focus.focus(window, cx);
            cx.notify();
        }
    }
    fn select_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.is_empty() {
            return;
        }
        if let Some(session) = self.active(cx).sessions.get(index) {
            let id = session.id;
            let active = self.active_workspace(cx);
            self.workspaces
                .iter_mut()
                .find(|w| Some(w.workspace.id) == active)
                .expect("active workspace exists")
                .active_session = Some(id);
            self.show_pane(&session_pane_id(id), window, cx);
        }
    }
    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.local || self.workspaces.is_empty() {
            return;
        }
        let used: HashSet<_> = self
            .workspaces
            .iter()
            .flat_map(|w| w.sessions.iter().map(|s| s.id.0))
            .collect();
        let active = self.active_workspace(cx);
        let state = self
            .workspaces
            .iter_mut()
            .find(|w| Some(w.workspace.id) == active)
            .unwrap();
        let cwd = self
            .data
            .terminal_cwd(state.workspace.id, cx)
            .expect("local workspace folder");
        let mut id = layout::stable_path_hash(&cwd);
        while used.contains(&id) {
            id = id.wrapping_add(1);
        }
        let title = cwd
            .file_name()
            .unwrap_or(cwd.as_os_str())
            .to_string_lossy()
            .into_owned();
        let pane_id = session_pane_id(SessionId(id));
        let pane = cx.new(|cx| {
            Pane::new(
                pane_id.clone(),
                title.clone(),
                PaneKind::CenterTerminal(cwd.clone()),
                state.workspace.id,
                self.data.clone(),
                window,
                cx,
            )
        });
        self._subscriptions
            .push(cx.observe(&pane, |_, _, cx| cx.notify()));
        self.registry
            .borrow_mut()
            .insert(pane_id.clone(), pane.downgrade());
        state.panes.insert(pane_id.clone(), pane);
        state.sessions.push(Session {
            id: SessionId(id),
            title,
            kind: data::SessionKind::Terminal,
        });
        state
            .store
            .terminals
            .as_mut()
            .expect("local sessions")
            .push(layout::TerminalSession { id, cwd });
        state.active_session = Some(SessionId(id));
        self.show_pane(&pane_id, window, cx);
    }
    fn close_session(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active_workspace(cx);
        let state = self
            .workspaces
            .iter_mut()
            .find(|state| Some(state.workspace.id) == active)
            .expect("active workspace exists");
        if let Some(pane) = state.panes.remove(&session_pane_id(id)) {
            pane.update(cx, |pane, cx| pane.close_terminal(cx));
            self.registry.borrow_mut().remove(&session_pane_id(id));
            if let Some(terminals) = &mut state.store.terminals {
                terminals.retain(|terminal| terminal.id != id.0);
            }
            state.sessions.retain(|session| session.id != id);
            state
                .dock
                .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
            if state.active_session == Some(id) {
                state.active_session = state.sessions.first().map(|session| session.id);
            }
            self.focus.focus(window, cx);
            cx.notify();
        }
    }
    fn show_pane(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.active(cx).workspace.id;
        self.show_workspace_pane(workspace, id, window, cx);
    }
    fn show_workspace_pane(
        &mut self,
        workspace: WorkspaceId,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.workspaces.iter().find(|w| w.workspace.id == workspace) else {
            return;
        };
        let Some(pane) = state.panes.get(id).cloned() else {
            return;
        };
        let panel_id = PanelId::from(pane.entity_id());
        let restoring = state.dock.read(cx).panel(panel_id).is_none();
        state.dock.update(cx, |dock, cx| {
            if dock.panel(panel_id).is_none() {
                dock.add_panel_view(
                    panel_handle(pane.clone()),
                    DockPlacement::Center,
                    None,
                    window,
                    cx,
                );
            }
            for placement in [
                DockPlacement::Left,
                DockPlacement::Right,
                DockPlacement::Bottom,
            ] {
                if dock
                    .layout(placement)
                    .is_some_and(|tree| tree.contains_panel(panel_id))
                    && !dock.is_dock_open(placement)
                {
                    dock.toggle_dock(placement, window, cx);
                }
            }
            dock.select_panel(panel_id, window, cx);
        });
        // Restoring a closed view retains the owner's jobs and last-poll clock.
        // Selecting an existing Git tab still dispatches the Git-entry intent.
        if restoring {
            self.sync_git_visibility(false, cx);
        }
        pane.focus_handle(cx).focus(window, cx);
        cx.notify();
    }
}
fn load_workspace_layout(
    state: &mut WorkspaceDock,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Result<()> {
    let known = state.panes.keys().cloned().collect();
    let loaded = match state.store.load(&known) {
        Ok(loaded) => loaded,
        Err(e) => {
            let error = format!("{e:#}");
            eprintln!("{error}");
            state.error = Some(format!(
                "{error} · Using the default layout. Saves disabled until this file is repaired."
            ));
            None
        }
    };
    if let Some(layout) = loaded {
        state
            .dock
            .update(cx, |dock, cx| dock.load(layout, window, cx))?;
    } else {
        default_layout(state, window, cx);
    }
    if state.store.terminals.is_some() {
        for session in &state.sessions {
            let pane = state.panes[&session_pane_id(session.id)].clone();
            state.dock.update(cx, |dock, cx| {
                if dock.panel(PanelId::from(pane.entity_id())).is_none() {
                    dock.add_panel_view(
                        panel_handle(pane),
                        DockPlacement::Center,
                        None,
                        window,
                        cx,
                    );
                }
            });
        }
        let run = state.panes[&format!("workspace-{}-run", state.workspace.layout_key)].clone();
        state.dock.update(cx, |dock, cx| {
            if dock.panel(PanelId::from(run.entity_id())).is_none() {
                dock.add_panel_view(panel_handle(run), DockPlacement::Bottom, None, window, cx);
            }
        });
        if !state.store.is_refused()
            && let Err(error) = state.store.save(&state.dock.read(cx).dump(cx))
        {
            state.error = Some(format!("{error:#}"));
        }
    }
    Ok(())
}
fn subscribe_dock(state: &WorkspaceDock, cx: &mut Context<Shell>) -> Subscription {
    let id = state.workspace.id;
    cx.subscribe(&state.dock, move |this, dock, _: &DockEvent, cx| {
        let state = this
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == id)
            .expect("owned workspace");
        if let Some(terminals) = &mut state.store.terminals {
            let removed: HashSet<_> = state
                .sessions
                .iter()
                .filter(|session| {
                    let pane = &state.panes[&session_pane_id(session.id)];
                    dock.read(cx)
                        .panel(PanelId::from(pane.entity_id()))
                        .is_none()
                })
                .map(|session| session.id.0)
                .collect();
            for id in &removed {
                let id = session_pane_id(SessionId(*id));
                if let Some(pane) = state.panes.remove(&id) {
                    pane.update(cx, |pane, cx| pane.close_terminal(cx));
                }
                this.registry.borrow_mut().remove(&id);
            }
            terminals.retain(|session| !removed.contains(&session.id));
            let layout = dock.read(cx).dump(cx);
            let order = layout::pane_order(&layout);
            terminals.sort_by_key(|session| {
                order
                    .iter()
                    .position(|id| *id == session_pane_id(SessionId(session.id)))
                    .unwrap_or(usize::MAX)
            });
            state.sessions.sort_by_key(|session| {
                terminals
                    .iter()
                    .position(|t| t.id == session.id.0)
                    .unwrap_or(usize::MAX)
            });
            state
                .sessions
                .retain(|session| !removed.contains(&session.id.0));
            if state
                .active_session
                .is_some_and(|id| removed.contains(&id.0))
            {
                state.active_session = state.sessions.first().map(|s| s.id);
            }
        }
        if !state.store.is_refused()
            && let Err(e) = state.store.save(&dock.read(cx).dump(cx))
        {
            let error = format!("{e:#}");
            eprintln!("{error}");
            state.error = Some(error);
        }
        this.sync_git_visibility(true, cx);
        cx.notify();
    })
}
fn pane_visible(state: &WorkspaceDock, kind: &str, cx: &App) -> bool {
    use gpui_component::dock::{PanelInfo, PanelState};
    fn visible(node: &PanelState, id: &str) -> bool {
        match &node.info {
            PanelInfo::Panel(value) => value.as_str() == Some(id),
            PanelInfo::Tabs { active_index } => node
                .children
                .get(*active_index)
                .is_some_and(|n| visible(n, id)),
            PanelInfo::Stack { .. } => node.children.iter().any(|n| visible(n, id)),
        }
    }
    let layout = state.dock.read(cx).dump(cx);
    let id = format!("workspace-{}-{kind}", state.workspace.layout_key);
    visible(&layout.center, &id)
        || [&layout.left_dock, &layout.right_dock, &layout.bottom_dock]
            .into_iter()
            .flatten()
            .any(|d| d.open() && visible(d.panel(), &id))
}
fn build_workspace(
    workspace: Workspace,
    data: Rc<dyn ShellData>,
    config_dir: &Path,
    session_ids: &mut HashSet<u64>,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Result<WorkspaceDock> {
    let mut store = LayoutStore::new(layout::workspace_layout_path(
        config_dir,
        &workspace.layout_key,
    ));
    let local = data.local_owner().is_some();
    let sessions = if local {
        let restored = store.load_terminals().unwrap_or_else(|error| {
            eprintln!("load terminal sessions {}: {error:#}", store.path.display());
            None
        });
        let cwd = data
            .terminal_cwd(workspace.id, cx)
            .expect("local workspace folder");
        let mut terminals = restored.unwrap_or_else(|| {
            vec![layout::TerminalSession {
                // The old single dock terminal used the bare folder hash.
                id: layout::stable_path_hash(&cwd).wrapping_add(1),
                cwd,
            }]
        });
        // An identity already owned elsewhere cannot construct a second pane.
        terminals.retain(|session| !session_ids.contains(&session.id));
        let sessions = terminals
            .iter()
            .map(|terminal| Session {
                id: SessionId(terminal.id),
                title: terminal
                    .cwd
                    .file_name()
                    .unwrap_or(terminal.cwd.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                kind: data::SessionKind::Terminal,
            })
            .collect();
        store.terminals = Some(terminals);
        sessions
    } else {
        data.sessions(workspace.id, cx)
    };
    let mut panes = HashMap::new();
    for session in &sessions {
        ensure!(
            session_ids.insert(session.id.0),
            "duplicate session ID {:?}",
            session.id
        );
        let id = session_pane_id(session.id);
        let pane = cx.new(|cx| {
            Pane::new(
                id.clone(),
                session.title.clone(),
                if let Some(terminals) = &store.terminals {
                    PaneKind::CenterTerminal(
                        terminals
                            .iter()
                            .find(|t| t.id == session.id.0)
                            .unwrap()
                            .cwd
                            .clone(),
                    )
                } else {
                    session.kind.into()
                },
                workspace.id,
                data.clone(),
                window,
                cx,
            )
        });
        panes.insert(id, pane);
    }
    if local {
        let id = format!("workspace-{}-run", workspace.layout_key);
        panes.insert(
            id.clone(),
            cx.new(|cx| {
                Pane::new(
                    id,
                    "Run terminal".into(),
                    PaneKind::Terminal,
                    workspace.id,
                    data.clone(),
                    window,
                    cx,
                )
            }),
        );
    }
    for (name, title, kind) in [
        ("git", "Git status", PaneKind::Git),
        ("files", "Files", PaneKind::Files),
    ] {
        let id = format!("workspace-{}-{name}", workspace.layout_key);
        panes.insert(
            id.clone(),
            cx.new(|cx| {
                Pane::new(
                    id,
                    title.into(),
                    kind,
                    workspace.id,
                    data.clone(),
                    window,
                    cx,
                )
            }),
        );
    }
    let (dock, skin) =
        DockSkin::dock_area(format!("workspace-{}", workspace.id.0), Some(1), window, cx);
    skin.set_panel_style(PanelStyle::TabBar, cx);
    skin.set_close_button_visible(true, cx);
    Ok(WorkspaceDock {
        active_session: sessions.first().map(|s| s.id),
        store,
        workspace,
        sessions,
        panes,
        dock,
        error: None,
    })
}

fn session_pane_id(id: SessionId) -> String {
    format!("session-{}", id.0)
}
fn default_layout(state: &WorkspaceDock, window: &mut Window, cx: &mut App) {
    let mut center = DockLayout::tabs();
    let mut bottom = DockLayout::tabs();
    let mut has_terminal = false;
    for session in &state.sessions {
        let pane = panel_handle(state.panes[&session_pane_id(session.id)].clone());
        if session.kind == data::SessionKind::Terminal && state.store.terminals.is_none() {
            bottom = bottom.panel_view(pane, cx);
            has_terminal = true;
        } else {
            center = center.panel_view(pane, cx);
        }
    }
    if state.store.terminals.is_some() {
        let run = state.panes[&format!("workspace-{}-run", state.workspace.layout_key)].clone();
        bottom = bottom.panel_view(panel_handle(run), cx);
        has_terminal = true;
    }
    let git = state.panes[&format!("workspace-{}-git", state.workspace.layout_key)].clone();
    let files = state.panes[&format!("workspace-{}-files", state.workspace.layout_key)].clone();
    state.dock.update(cx, |dock, cx| {
        dock.set_center(DockLayout::h_split().child(center, None), window, cx);
        dock.set_dock(
            DockPlacement::Right,
            DockLayout::tabs()
                .panel_view(panel_handle(git), cx)
                .panel_view(panel_handle(files), cx),
            window,
            cx,
        );
        dock.set_dock_size(DockPlacement::Right, px(300.), window, cx);
        if has_terminal {
            dock.set_dock(DockPlacement::Bottom, bottom, window, cx);
            dock.set_dock_size(DockPlacement::Bottom, px(265.), window, cx);
        }
    });
}
impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        if self.workspaces.is_empty() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .p_4()
                .gap_3()
                .child("No local workspaces yet")
                .child("Add a local folder to see live Git status.")
                .child(self.add_folder_button(cx))
                .children(
                    self.owner
                        .read(cx)
                        .error
                        .clone()
                        .map(|error| div().text_color(rgb(p.red)).child(error)),
                )
                .into_any_element();
        }
        let state = self.active(cx);
        let repository = self
            .data
            .status(self.active_workspace(cx).expect("active workspace"), cx)
            .repository;
        let active_session = state
            .active_session
            .and_then(|id| state.sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let sidebar = div()
            .flex()
            .flex_col()
            .w(px(224.))
            .flex_shrink_0()
            .h_full()
            .bg(rgb(p.mantle))
            .border_r_1()
            .border_color(rgb(p.surface))
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("GitTerm"),
            )
            .child(div().text_color(rgb(p.muted)).mb_4().child("Workspaces"))
            .when(self.local, |sidebar| {
                sidebar.child(self.add_folder_button(cx))
            })
            .children(self.workspaces.iter().enumerate().map(|(index, state)| {
                let workspace = &state.workspace;
                let count = self.data.status(workspace.id, cx).files.len();
                Button::new(("workspace", index))
                    .ghost()
                    .selected(Some(workspace.id) == self.active_workspace(cx))
                    .accessibility_label(format!(
                        "{}, {}, {} changes",
                        workspace.name, workspace.status, count
                    ))
                    .h(px(62.))
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_start()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(if Some(workspace.id) == self.active_workspace(cx) {
                        p.surface
                    } else {
                        p.mantle
                    }))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(workspace.name.clone())
                            .child(div().text_color(rgb(p.blue)).child(count.to_string())),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(self.data.status(workspace.id, cx).branch),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_workspace(index, window, cx)
                    }))
            }))
            .child(
                div()
                    .mt_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_color(rgb(p.muted))
                    .child("Repositories")
                    .child(
                        Button::new("new-task")
                            .ghost()
                            .label("+")
                            .tooltip("New task")
                            .on_click({
                                let repository = repository.clone();
                                move |_, window, cx| {
                                    dialogs::new_task(repository.clone(), window, cx)
                                }
                            }),
                    ),
            )
            .children(
                state
                    .workspace
                    .repositories
                    .iter()
                    .map(|repo| div().p_2().child(format!("⌄  {repo}"))),
            )
            .child(
                div().pl_4().text_color(rgb(p.muted)).child(
                    self.data
                        .status(self.active_workspace(cx).expect("active workspace"), cx)
                        .branch,
                ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(if self.local {
                        "Local · Live Git status"
                    } else {
                        "Local · GPUI sample data"
                    }),
            );
        let tabs = TabBar::new("sessions")
            .underline()
            .selected_index(active_session)
            .h(px(38.))
            .children(state.sessions.iter().map(|s| {
                Tab::from(if self.local {
                    state.panes[&session_pane_id(s.id)].read(cx).tab_title()
                } else {
                    s.title.clone()
                })
            }))
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                this.select_session(*index, window, cx)
            }));
        let weak = cx.weak_entity();
        let close_session = state.active_session;
        let close_label = state
            .sessions
            .iter()
            .find(|session| Some(session.id) == close_session)
            .map(|session| format!("Close {} permanently", session.title))
            .unwrap_or_else(|| "Close session permanently".into());
        let mut panes: Vec<_> = state
            .panes
            .iter()
            .map(|(id, pane)| (id.clone(), pane.read(cx).title.clone()))
            .collect();
        panes.sort_by(|a, b| a.0.cmp(&b.0));
        let more = Button::new("more")
            .ghost()
            .label("⋯")
            .tooltip("More")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                let repository = repository.clone();
                let weak = weak.clone();
                let panes = panes.clone();
                let close_owner = weak.clone();
                menu.item(
                    PopupMenuItem::new("New task…")
                        .on_click(move |_, w, cx| dialogs::new_task(repository.clone(), w, cx)),
                )
                .item(
                    PopupMenuItem::new("Keyboard shortcuts     ⌥/")
                        .on_click(|_, w, cx| dialogs::shortcuts(w, cx)),
                )
                .separator()
                .submenu("Show pane", window, cx, move |menu, _, _| {
                    panes.iter().fold(menu, |menu, (id, title)| {
                        let weak = weak.clone();
                        let id = id.clone();
                        menu.item(PopupMenuItem::new(title.clone()).on_click(
                            move |_, window, cx| {
                                _ = weak.update(cx, |this, cx| this.show_pane(&id, window, cx));
                            },
                        ))
                    })
                })
                .item({
                    let weak = close_owner.clone();
                    PopupMenuItem::new(close_label.clone())
                        .disabled(close_session.is_none())
                        .on_click(move |_, window, cx| {
                            if let Some(id) = close_session {
                                _ = weak.update(cx, |this, cx| this.close_session(id, window, cx));
                            }
                        })
                })
                .item(
                    PopupMenuItem::new("Toggle Mocha / Latte").on_click(|_, w, cx| {
                        theme::apply(
                            !gpui_component::Theme::global(cx).mode.is_dark(),
                            Some(w),
                            cx,
                        );
                    }),
                )
                .separator()
                .item(PopupMenuItem::label("Browser stopped · sample"))
                .item(PopupMenuItem::new("Open browser").on_click(|_, w, cx| {
                    dialogs::notice("Open browser · sample action only", w, cx)
                }))
            });
        let mut content = div().flex_1().min_w_0().h_full().flex().flex_col().child(
            div()
                .flex()
                .items_center()
                .h(px(38.))
                .bg(rgb(p.mantle))
                .child(div().flex_1().min_w_0().child(tabs))
                .when(self.local, |bar| {
                    bar.child(
                        Button::new("new-terminal")
                            .ghost()
                            .label("+")
                            .tooltip("New terminal · ⌘T")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_terminal(window, cx)),
                            ),
                    )
                })
                .child(more),
        );
        if let Some(error) = &self.owner.read(cx).error {
            content = content.child(div().p_2().text_color(rgb(p.red)).child(error.clone()));
        }
        if let Some(error) = &state.error {
            content = content.child(div().p_2().text_color(rgb(p.red)).child(error.clone()));
        }
        div()
            .size_full()
            .track_focus(&self.focus)
            .key_context("GitTermShell")
            .on_action(
                cx.listener(|this, a: &SelectSession, w, cx| this.select_session(a.index, w, cx)),
            )
            .on_action(
                cx.listener(|this, a: &SelectWorkspace, w, cx| {
                    this.select_workspace(a.index, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &NewTerminal, w, cx| this.new_terminal(w, cx)))
            .on_action(cx.listener(|_, _: &ShowShortcuts, w, cx| dialogs::shortcuts(w, cx)))
            .flex()
            .font_family(".SystemUIFont")
            .text_size(px(UI_FONT_SIZE))
            .text_color(rgb(p.text))
            .bg(rgb(p.base))
            .child(sidebar)
            .child(content.child(div().flex_1().min_h_0().child(state.dock.clone())))
            .into_any_element()
    }
}

pub fn run(folders: Vec<std::path::PathBuf>, sample: bool) -> Result<()> {
    let home = dirs::home_dir().expect("resolve home for isolated GPUI config");
    let config = layout::config_dir(std::env::var_os("GITTERM_GPUI_CONFIG_DIR"), &home)?;
    let config = if sample {
        layout::config_dir(Some(config.join("sample").into_os_string()), &home)?
    } else {
        config
    };
    let mut store = workspace_config::WorkspaceStore::new(&config);
    let mut model = WorkspaceStatus::default();
    if !sample {
        for folder in store.load()? {
            model.add(folder, Instant::now());
        }
        for folder in folders {
            model.add(workspace_config::local_folder(&folder)?, Instant::now());
        }
        store.save(&model.folders())?;
    }
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        theme::apply(true, None, cx);
        for (index, (session_key, workspace_key)) in [
            ("cmd-1", "ctrl-1"),
            ("cmd-2", "ctrl-2"),
            ("cmd-3", "ctrl-3"),
            ("cmd-4", "ctrl-4"),
            ("cmd-5", "ctrl-5"),
            ("cmd-6", "ctrl-6"),
            ("cmd-7", "ctrl-7"),
            ("cmd-8", "ctrl-8"),
            ("cmd-9", "ctrl-9"),
        ]
        .into_iter()
        .enumerate()
        {
            cx.bind_keys([
                KeyBinding::new(session_key, SelectSession { index }, Some("GitTermShell")),
                KeyBinding::new(
                    workspace_key,
                    SelectWorkspace { index },
                    Some("GitTermShell"),
                ),
            ]);
        }
        cx.bind_keys([KeyBinding::new(
            "alt-/",
            ShowShortcuts,
            Some("GitTermShell"),
        )]);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some(
                        if sample {
                            "GitTerm · GPUI shell · Sample data"
                        } else {
                            "GitTerm · GPUI · Local workspaces"
                        }
                        .into(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let shell = cx.new(|cx| {
                    if sample {
                        Shell::new(Rc::new(data::SampleData), &config, window, cx)
                            .expect("create sample shell")
                    } else {
                        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
                        let data = Rc::new(data::LocalData(owner.clone()));
                        let shell = Shell::with_owner(data, owner, &config, true, window, cx)
                            .expect("create local shell");
                        shell.sync_git_visibility(false, cx);
                        shell
                    }
                });
                shell.focus_handle(cx).focus(window, cx);
                cx.new(|cx| Root::new(shell, window, cx))
            },
        )
        .expect("open GitTerm GPUI shell window");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::dock::{InsertTarget, PaneRef};

    #[gpui::test]
    fn live_local_status_crosses_application_bridge_and_survives_closed_git_pane(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let repo = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = gitterm_core::git_status::git_command()
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "live-fixture"]);
        std::fs::write(repo.path().join("staged.txt"), "staged").unwrap();
        std::fs::write(repo.path().join("untracked.txt"), "untracked").unwrap();
        git(&["add", "staged.txt"]);
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| {
                WorkspaceOwner::new(
                    WorkspaceStatus::default(),
                    Some(workspace_config::WorkspaceStore::new(config.path())),
                    cx,
                )
            });
            let data = Rc::new(data::LocalData(owner.clone()));
            Shell::with_owner(data, owner, config.path(), true, window, cx).unwrap()
        });
        draw(cx); // Empty state is renderable and has no implicit workspace.
        assert!(cx.read(|cx| shell.read(cx).active_workspace(cx)).is_none());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .add_folder(
                        workspace_config::local_folder(repo.path()).unwrap(),
                        window,
                        cx,
                    )
                    .unwrap();
                shell.select_workspace(0, window, cx);
                shell.owner.update(cx, |owner, cx| {
                    owner.dispatch(Intent::Maintenance { focused: true }, Instant::now(), cx)
                });
            })
        });
        let (owner, dock, pane, git_id) = cx.read(|cx| {
            let shell = shell.read(cx);
            let state = shell.active(cx);
            (
                shell.owner.clone(),
                state.dock.clone(),
                state.panes[&format!("workspace-{}-git", state.workspace.layout_key)].clone(),
                format!("workspace-{}-git", state.workspace.layout_key),
            )
        });
        cx.update(|window, cx| {
            dock.update(cx, |dock, cx| dock.remove_panel(pane.clone(), window, cx))
        });
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if cx.read(|cx| owner.read(cx).model.workspaces[0].status.branch_name == "live-fixture")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "runtime outcome never reached the application owner"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let before = cx.read(|cx| owner.read(cx).model.workspaces[0].status.last_poll);
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.show_pane(&git_id, window, cx)));
        draw(cx);
        cx.read(|cx| {
            let status = pane.read(cx).status_view(cx);
            assert_eq!(status.branch, "live-fixture");
            assert!(!status.loading);
            assert!(status.error.is_none());
            assert!(
                status
                    .files
                    .iter()
                    .any(|f| f.path == "staged.txt" && f.staged)
            );
            assert!(
                status
                    .files
                    .iter()
                    .any(|f| f.path == "untracked.txt" && f.status == "??")
            );
            assert_eq!(owner.read(cx).model.workspaces[0].status.last_poll, before);
        });
        let mut store = workspace_config::WorkspaceStore::new(config.path());
        assert_eq!(store.load().unwrap(), [repo.path().canonicalize().unwrap()]);
        // Local terminals start in their workspace folder, not $HOME.
        let folder = repo.path().canonicalize().unwrap();
        cx.read(|cx| {
            let state = shell.read(cx).active(cx);
            let terminals: Vec<_> = state
                .panes
                .values()
                .filter(|pane| matches!(pane.read(cx).content, pane::PaneContent::Terminal(_)))
                .collect();
            assert!(!terminals.is_empty());
            for terminal in terminals {
                assert!(
                    terminal
                        .read(cx)
                        .tab_title()
                        .contains(folder.file_name().unwrap().to_str().unwrap()),
                    "{}",
                    terminal.read(cx).tab_title()
                );
            }
        });
        // A real subsequent maintenance request sees a filesystem change.
        std::fs::write(repo.path().join("later.txt"), "later").unwrap();
        cx.update(|_, cx| {
            owner.update(cx, |owner, cx| {
                owner.dispatch(
                    Intent::Maintenance { focused: true },
                    Instant::now() + std::time::Duration::from_secs(15),
                    cx,
                );
            })
        });
        loop {
            cx.run_until_parked();
            if cx.read(|cx| {
                pane.read(cx)
                    .status_view(cx)
                    .files
                    .iter()
                    .any(|f| f.path == "later.txt")
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "maintenance did not refresh the live pane"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        eprintln!(
            "live status arrived via GPUI owner: staged.txt, untracked.txt, later.txt (isolated config)"
        );
    }

    fn terminal(pane: &Entity<Pane>, cx: &App) -> Entity<gitterm_terminal::TerminalView> {
        let pane::PaneContent::Terminal(view) = &pane.read(cx).content else {
            panic!("expected real terminal");
        };
        view.clone()
    }
    fn chat(pane: &Entity<Pane>, cx: &App) -> Entity<gitterm_gpui_chat::ChatView> {
        let pane::PaneContent::Chat(view) = &pane.read(cx).content else {
            panic!("expected native chat");
        };
        view.clone()
    }
    fn wait_for_text(
        view: &Entity<gitterm_terminal::TerminalView>,
        needle: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if view.read_with(cx, |view, _| {
                view.backend()
                    .get_all_text()
                    .is_some_and(|text| text.contains(needle))
            }) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "terminal never produced {needle}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    // Test platform only: owned PTYs, no native window, clipboard or OS input.
    #[gpui::test]
    fn terminal_and_chat_survive_moves_grouping_close_reopen_workspace_and_restore(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), window, cx).unwrap()
        });
        let (dock, held, git) = cx.read(|cx| {
            let state = shell.read(cx).active(cx);
            (
                state.dock.clone(),
                vec![
                    state.panes["session-11"].clone(),
                    state.panes["session-12"].clone(),
                ],
                state.panes["workspace-1-git"].clone(),
            )
        });
        let terminal = cx.read(|cx| terminal(&held[1], cx));
        terminal.update_in(cx, |view, _, _| {
            view.backend_mut().handle(gitterm_terminal::Command::Write(
                b"printf 'SURVIVAL_%s\\n' history\r".to_vec(),
            ));
        });
        wait_for_text(&terminal, "SURVIVAL_history", cx);
        let chat = cx.read(|cx| chat(&held[0], cx));
        let scroller = cx.read(|cx| chat.read(cx).scroller().clone());
        cx.update(|_, cx| scroller.update(cx, |state, cx| state.scroll_to_item(2, cx)));
        let scroll = cx.read(|cx| scroller.read(cx).logical_scroll_top());
        assert_eq!(scroll.item_ix, 2);
        cx.update(|window, cx| {
            for pane in &held {
                let id = PanelId::from(pane.entity_id());
                let node = dock
                    .read(cx)
                    .layout(DockPlacement::Right)
                    .unwrap()
                    .find_panel_node(PanelId::from(git.entity_id()))
                    .unwrap();
                dock.update(cx, |dock, cx| {
                    dock.move_panel(
                        id,
                        InsertTarget::Tabs {
                            node,
                            ix: None,
                            activate: true,
                        },
                        window,
                        cx,
                    )
                });
                assert!(
                    dock.read(cx)
                        .layout(DockPlacement::Right)
                        .unwrap()
                        .contains_panel(id)
                );
                assert_eq!(
                    dock.read(cx).panel(id).unwrap().view().entity_id(),
                    pane.entity_id()
                );
                dock.update(cx, |dock, cx| {
                    dock.toggle_dock(DockPlacement::Right, window, cx);
                    dock.toggle_dock(DockPlacement::Right, window, cx);
                    dock.remove_panel(pane.clone(), window, cx);
                });
                assert!(dock.read(cx).panel(id).is_none());
                let pane_id = pane.read(cx).id.clone();
                shell.update(cx, |s, cx| s.show_pane(&pane_id, window, cx));
                let saved = dock.read(cx).dump(cx);
                dock.update(cx, |dock, cx| dock.load(saved, window, cx).unwrap());
                assert_eq!(
                    dock.read(cx).panel(id).unwrap().view().entity_id(),
                    pane.entity_id()
                );
            }
            shell.update(cx, |s, cx| {
                s.select_workspace(1, window, cx);
                s.select_session(1, window, cx);
                assert_eq!(s.active(cx).active_session, Some(SessionId(22)));
                s.select_workspace(0, window, cx);
            });
            assert_eq!(
                self::terminal(&held[1], cx).entity_id(),
                terminal.entity_id()
            );
            assert_eq!(self::chat(&held[0], cx).entity_id(), chat.entity_id());
            assert_eq!(scroller.read(cx).logical_scroll_top(), scroll);
            for pane in &held {
                let tree = dock.read(cx).layout(DockPlacement::Center).unwrap();
                let id = PanelId::from(pane.entity_id());
                assert!(matches!(
                    tree.find_node(tree.find_panel_node(id).unwrap())
                        .unwrap()
                        .kind(),
                    PaneRef::Tabs { .. }
                ));
            }
        });
        wait_for_text(&terminal, "SURVIVAL_history", cx);
    }

    #[gpui::test]
    fn permanent_session_close_releases_terminal_and_removes_reopen_entry(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let (shell, cx) = cx.add_window_view(|w, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), w, cx).unwrap()
        });
        let weak =
            cx.read(|cx| terminal(&shell.read(cx).active(cx).panes["session-12"], cx).downgrade());
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                s.close_session(SessionId(12), window, cx);
                assert!(!s.active(cx).panes.contains_key("session-12"));
                assert!(s.active(cx).sessions.iter().all(|s| s.id != SessionId(12)));
                s.show_pane("session-12", window, cx);
            })
        });
        cx.run_until_parked();
        cx.read(|_| {
            assert!(
                weak.upgrade().is_none(),
                "permanent close retained a terminal"
            )
        });
    }

    #[gpui::test]
    fn clicks_and_input_are_scoped_to_the_focused_terminal(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let (shell, cx) = cx.add_window_view(|w, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), w, cx).unwrap()
        });
        let (first, second, dock) = cx.read(|cx| {
            let shell = shell.read(cx);
            (
                shell.workspaces[0].panes["session-12"].clone(),
                shell.workspaces[1].panes["session-22"].clone(),
                shell.active(cx).dock.clone(),
            )
        });
        cx.update(|window, cx| {
            dock.update(cx, |dock, cx| {
                dock.remove_panel(first.clone(), window, cx);
                dock.set_center(
                    DockLayout::h_split()
                        .child(
                            DockLayout::tabs().panel_view(panel_handle(first.clone()), cx),
                            None,
                        )
                        .child(
                            DockLayout::tabs().panel_view(panel_handle(second.clone()), cx),
                            None,
                        ),
                    window,
                    cx,
                );
            })
        });
        cx.simulate_resize(size(px(1440.), px(900.)));
        draw(cx);
        let first_view = cx.read(|cx| terminal(&first, cx));
        let second_view = cx.read(|cx| terminal(&second, cx));
        for (selector, view, other, label) in [
            ("session-12", &first_view, &second_view, "FIRST"),
            ("session-22", &second_view, &first_view, "SECOND"),
        ] {
            let bounds = cx.debug_bounds(selector).expect("visible terminal pane");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.update(|window, cx| {
                assert!(view.read(cx).focus_handle().is_focused(window));
                assert!(!other.read(cx).focus_handle().is_focused(window));
            });
            cx.simulate_keystrokes("cmd-f");
            draw(cx);
            cx.simulate_input("TERMINAL_FIND_ONLY");
            cx.read(|cx| {
                assert!(view.read(cx).is_find_open());
                assert_eq!(view.read(cx).find_query(), "TERMINAL_FIND_ONLY");
                assert!(!other.read(cx).is_find_open());
            });
            cx.simulate_keystrokes("cmd-g cmd-shift-g escape");
            draw(cx);
            cx.read(|cx| assert!(!view.read(cx).is_find_open()));
            cx.simulate_input(&format!("printf 'FOCUS_%s\\n' {label}"));
            cx.simulate_keystrokes("enter");
            wait_for_text(view, &format!("FOCUS_{label}"), cx);
            cx.simulate_input(
                "printf '\\033]7;file:///tmp/reported-pane\\007\\033]2;REPORTED_PANE\\007'\n",
            );
            let deadline = std::time::Instant::now();
            loop {
                draw(cx);
                let pane = if selector == "session-12" {
                    &first
                } else {
                    &second
                };
                if cx.read(|cx| pane.read(cx).tab_title()) == "REPORTED_PANE · /tmp/reported-pane"
                {
                    break;
                }
                assert!(
                    deadline.elapsed().as_secs() < 5,
                    "pane reports not delivered"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }

            other.read_with(cx, |view, _| {
                assert!(
                    !view
                        .backend()
                        .get_all_text()
                        .unwrap()
                        .contains(&format!("FOCUS_{label}"))
                )
            });
        }
        let chat_pane = cx.read(|cx| shell.read(cx).active(cx).panes["session-11"].clone());
        cx.update(|window, cx| {
            dock.update(cx, |dock, cx| {
                dock.set_dock(
                    DockPlacement::Right,
                    DockLayout::tabs().panel_view(panel_handle(chat_pane.clone()), cx),
                    window,
                    cx,
                );
            });
        });
        draw(cx);
        let bounds = cx.debug_bounds("session-11").expect("visible chat pane");
        cx.simulate_click(
            bounds.origin + gpui::point(px(12.), px(100.)),
            gpui::Modifiers::none(),
        );
        let chat = cx.read(|cx| chat(&chat_pane, cx));
        cx.update(|window, cx| {
            assert!(
                chat.focus_handle(cx).contains_focused(window, cx),
                "click must focus docked chat"
            )
        });
        draw(cx);
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        cx.simulate_input("CHAT_ONLY_QUERY");
        cx.update(|window, cx| {
            let focus = chat.focus_handle(cx);
            assert!(focus.contains_focused(window, cx));
            assert!(!focus.is_focused(window), "Find must focus its child input");
            for terminal in [&first_view, &second_view] {
                assert!(!terminal.read(cx).focus_handle().is_focused(window));
                assert!(
                    !terminal
                        .read(cx)
                        .backend()
                        .get_all_text()
                        .unwrap()
                        .contains("CHAT_ONLY_QUERY")
                );
            }
        });
    }

    #[gpui::test]
    fn corrupt_file_starts_default_dock_and_remains_intact(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout-1.json");
        std::fs::write(&path, "{corrupt").unwrap();
        let (shell, cx) = cx.add_window_view(|w, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), w, cx).unwrap()
        });
        cx.update(|window, cx| {
            let state = shell.read(cx).active(cx);
            assert!(
                state
                    .error
                    .as_ref()
                    .unwrap()
                    .contains(path.to_str().unwrap())
            );
            let dock = state.dock.clone();
            assert!(dock.read(cx).has_dock(DockPlacement::Bottom));
            assert!(dock.read(cx).has_dock(DockPlacement::Right));
            assert!(
                dock.read(cx)
                    .panel(PanelId::from(state.panes["session-11"].entity_id()))
                    .is_some()
            );
            dock.update(cx, |dock, cx| {
                dock.toggle_dock(DockPlacement::Bottom, window, cx)
            });
        });
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{corrupt");
    }

    #[gpui::test]
    fn saved_workspace_layout_restores_position_and_size(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let (shell, cx) = cx.add_window_view(|w, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), w, cx).unwrap()
        });
        cx.update(|window, cx| {
            let state = shell.read(cx).active(cx);
            let dock = state.dock.clone();
            dock.update(cx, |dock, cx| {
                dock.set_dock_size(DockPlacement::Bottom, px(321.), window, cx);
                dock.toggle_dock(DockPlacement::Bottom, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let restored =
                cx.new(|cx| Shell::new(Rc::new(data::SampleData), dir.path(), window, cx).unwrap());
            let dock = &restored.read(cx).active(cx).dock;
            assert_eq!(
                dock.read(cx).dock_size(DockPlacement::Bottom),
                Some(px(321.))
            );
            assert!(!dock.read(cx).is_dock_open(DockPlacement::Bottom));
        });
    }
}
