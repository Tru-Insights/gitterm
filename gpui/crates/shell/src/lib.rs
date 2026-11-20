#![cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(test)]
mod center_tests;
mod chat;
pub mod content;
#[cfg(test)]
mod content_shell_tests;
pub mod data;
mod dialogs;
pub mod files;
mod files_view;
pub mod find_sidebar;
#[cfg(test)]
mod hookup_tests;
mod layout;
mod owner;
mod pane;
mod park;
pub mod park_sheet;
pub mod preview;
#[cfg(test)]
mod region_tests;
pub mod remote;
mod review_reload;
pub mod session_picker;
pub mod session_strip;
mod sessions;
pub mod settings;
pub mod spine;
pub mod tasks;
mod theme;
pub mod usage;
pub mod work_sidebar;
pub mod workspace_bar;
pub mod workspace_config;
pub mod workspace_sheet;
pub mod workspace_status;
pub use owner::{FileOpen, WorkspaceOwner};
use std::{cell::RefCell, time::Instant};
use workspace_status::{Intent, WorkspaceStatus};

use anyhow::{Result, ensure};
use data::{Session, SessionId, ShellData, Workspace, WorkspaceId};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, Anchor, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, Styled as _,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, px, rgb,
    size,
};
use gpui_component::{
    Disableable, Root, Selectable,
    button::{Button, ButtonVariants},
    dock::{
        DockArea, DockEvent, DockLayout, DockPlacement, DockSkin, PanelId, PanelStyle,
        panel_handle, register_panel,
    },
    menu::{DropdownMenu, PopupMenuItem},
    tab::{Tab, TabBar},
};
use layout::{LayoutStore, SidebarMode};
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
actions!(
    gitterm_shell,
    [ShowShortcuts, NewTerminal, NewClaudeChat, CloseWorkspace]
);

struct WorkspaceDock {
    runtime_uid: u64,
    parked: Option<park::Parked>,
    workspace: Workspace,
    sessions: Vec<Session>,
    active_session: Option<SessionId>,
    dock: Entity<DockArea>,
    // Lifetime owner: hiding retains the pane; explicit session close releases it.
    panes: HashMap<String, Entity<Pane>>,
    store: LayoutStore,
    error: Option<String>,
}
pub struct Shell {
    data: Rc<dyn ShellData>,
    content: Entity<content::window::ContentWindow>,
    workspaces: Vec<WorkspaceDock>,
    owner: Entity<WorkspaceOwner>,
    registry: Rc<RefCell<HashMap<String, gpui::WeakEntity<Pane>>>>,
    config_dir: std::path::PathBuf,
    local: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    resize: Option<(bool, f32, f32)>,
    session_picker: Option<Entity<session_picker::SessionPicker>>,
    picker_subscription: Option<Subscription>,
    park_sheet: Option<Entity<park_sheet::ParkSheet>>,
    park_subscription: Option<Subscription>,
    work_state: Rc<RefCell<gitterm_core::work::state::State>>,
    work_sidebars: HashMap<WorkspaceId, Entity<work_sidebar::WorkSidebar>>,
    work_subscriptions: HashMap<WorkspaceId, Subscription>,
    #[cfg(test)]
    dock_fixture: bool,
}
impl Shell {
    #[cfg(test)]
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
        let content = cx.new(|cx| content::window::ContentWindow::new(window, cx));
        let catalog = data.workspaces(cx);
        let saved_parked = if local {
            owner.read(cx).saved_parked()
        } else {
            vec![]
        };
        let stopped_workspaces = catalog
            .iter()
            .filter(|workspace| {
                data.terminal_cwd(workspace.id, cx).is_some_and(|dir| {
                    saved_parked
                        .iter()
                        .any(|record| record.matches(&workspace.name, &dir))
                })
            })
            .map(|workspace| workspace.id)
            .collect();
        owner.update(cx, |owner, _| owner.stopped_workspaces = stopped_workspaces);
        if owner.read(cx).model.active().is_none()
            && let Some(first) = catalog.first()
        {
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
                owner.read(cx).writer(),
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
        let mut subscriptions = vec![cx.observe(&content, |this, _, cx| {
            for state in &this.workspaces {
                for pane in state.panes.values() {
                    if matches!(pane.read(cx).content, pane::PaneContent::Content(_)) {
                        pane.update(cx, |_, cx| cx.notify());
                    }
                }
            }
            cx.notify();
        })];
        for state in &mut workspaces {
            load_workspace_layout(state, window, cx)?;
            if owner.read(cx).iced_workspace(state.workspace.id).is_some()
                && let Some(id) = state.active_session
                && let Some(pane) = state.panes.get(&session_pane_id(id))
            {
                let panel = PanelId::from(pane.entity_id());
                state
                    .dock
                    .update(cx, |dock, cx| dock.select_panel(panel, window, cx));
            }
            subscriptions.push(subscribe_dock(state, cx));
            subscriptions.push(observe_content_dock(&state.dock, cx));
            if local {
                for pane in state.panes.values() {
                    subscriptions.push(sessions::subscribe(pane, window, cx));
                    subscriptions.push(cx.observe_in(pane, window, |shell, pane, window, cx| {
                        shell.wake_terminal(pane.clone(), window, cx);
                        cx.notify();
                    }));
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
                if event.from_files && content::window::supported(&event.path) {
                    this.open_content(event, window, cx);
                } else {
                    this.open_review(event, window, cx);
                }
            },
        ));
        if local {
            owner.update(cx, |owner, cx| {
                owner.dispatch(Intent::Opened, Instant::now(), cx)
            });
        }
        let mut shell = Self {
            data,
            content,
            workspaces,
            owner,
            registry,
            config_dir: config_dir.to_path_buf(),
            local,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
            resize: None,
            session_picker: None,
            picker_subscription: None,
            park_sheet: None,
            park_subscription: None,
            work_state: Default::default(),
            work_sidebars: HashMap::new(),
            work_subscriptions: HashMap::new(),
            #[cfg(test)]
            dock_fixture: false,
        };
        shell.restore_parked(saved_parked, cx);
        Ok(shell)
    }
    fn open_content(&mut self, event: &FileOpen, window: &mut Window, cx: &mut Context<Self>) {
        if !self.local {
            return;
        }
        let Some(root) = self.data.terminal_cwd(event.workspace, cx) else {
            return;
        };
        let Ok(relative) = event.path.strip_prefix(&root) else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!(
                    "Cannot preview {}: outside workspace root",
                    event.path.display()
                ));
                cx.notify();
            });
            return;
        };
        let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == event.workspace)
        else {
            return;
        };
        let id = content_pane_id(event.workspace, &event.path);
        if !state.panes.contains_key(&id) {
            let dark = gpui_component::Theme::global(cx).mode.is_dark();
            let pane = cx.new(|cx| {
                Pane::new(
                    id.clone(),
                    event
                        .path
                        .file_name()
                        .unwrap_or(event.path.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    PaneKind::Content(self.content.clone()),
                    event.workspace,
                    self.data.clone(),
                    window,
                    cx,
                )
            });
            self.content.update(cx, |content, cx| {
                content.open(
                    id.clone(),
                    content::window::ContentRequest {
                        root: root.clone(),
                        relative: relative.to_owned(),
                        workspace: event.workspace.0,
                        pane: pane.entity_id().as_u64(),
                        dark,
                    },
                    window,
                    cx,
                )
            });
            self.registry
                .borrow_mut()
                .insert(id.clone(), pane.downgrade());
            state.panes.insert(id.clone(), pane);
        }
        self.content
            .update(cx, |content, _| content.select(Some(id.clone())));
        self.show_workspace_pane(event.workspace, &id, window, cx);
    }
    fn sync_content_selection(&self, cx: &mut Context<Self>) {
        let mut visible: Vec<String> = self
            .workspaces
            .iter()
            .filter(|w| Some(w.workspace.id) == self.active_workspace(cx))
            .flat_map(|state| {
                state
                    .panes
                    .iter()
                    .filter(|(id, pane)| {
                        matches!(pane.read(cx).content, pane::PaneContent::Content(_))
                            && content_pane_visible(state, id, cx)
                    })
                    .map(|(id, _)| id.clone())
            })
            .collect();
        visible.sort();
        let current = self.content.read(cx).selected.clone();
        let selected = current
            .filter(|id| visible.contains(id))
            .or_else(|| visible.into_iter().next());
        let focus = selected
            .as_ref()
            .and_then(|id| {
                self.workspaces
                    .iter()
                    .find_map(|state| state.panes.get(id).map(|pane| pane.focus_handle(cx)))
            })
            .unwrap_or_else(|| self.focus.clone());
        self.content.update(cx, |content, _| {
            content.set_focus_target(self.focus.clone(), focus);
            content.select(selected);
        });
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
    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.content.update(cx, |content, cx| {
            content.picker = true;
            content.sync_covers(window, cx);
        });
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
                this.content.update(cx, |content, cx| {
                    content.picker = false;
                    content.sync_covers(window, cx);
                });
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
                self.owner.read(cx).writer(),
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
            self._subscriptions
                .push(observe_content_dock(&state.dock, cx));
            for pane in state.panes.values() {
                self._subscriptions
                    .push(sessions::subscribe(pane, window, cx));
                self._subscriptions
                    .push(cx.observe_in(pane, window, |shell, pane, window, cx| {
                        shell.wake_terminal(pane.clone(), window, cx);
                        cx.notify();
                    }));
            }
            self.workspaces.push(state);
        }
        self.reopen_workspace(id, window, cx);
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
            .is_some_and(|w| {
                !w.store.regions.sidebar_collapsed
                    && w.store.regions.sidebar_mode == SidebarMode::Files
            });
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
        if let Some(id) = self
            .visible_workspace_indices(cx)
            .get(index)
            .map(|&i| self.workspaces[i].workspace.id)
        {
            if Some(id) != self.active_workspace(cx) {
                self.session_picker = None;
                self.picker_subscription = None;
                self.park_sheet = None;
                self.park_subscription = None;
                self.cover_sheet(false, window, cx);
            }
            self.owner.update(cx, |owner, cx| {
                owner.dispatch(Intent::Activate(id), Instant::now(), cx)
            });
            self.sync_git_visibility(false, cx);
            self.sync_content_selection(cx);
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
    fn new_claude_chat(
        &mut self,
        history: Option<chat::SavedChat>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local {
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let Some(cwd) = self.data.terminal_cwd(workspace, cx) else {
            return;
        };
        let Some(preview) = self.owner.read(cx).preview() else {
            return;
        };
        let shutdown = self.owner.read(cx).chat_shutdown.clone();
        let view = cx.new(|cx| match history {
            Some(saved) => chat::ChatPane::history(shutdown, saved, window, cx),
            None => chat::ChatPane::new(shutdown, preview, cwd, window, cx),
        });
        let target = view.read(cx).target();
        let owner = self.owner.clone();
        self._subscriptions.push(
            cx.subscribe(&view, move |_, _, saved: &chat::SavedChat, cx| {
                owner.update(cx, |owner, cx| owner.save_chat(saved.clone(), cx));
            }),
        );
        self.owner.update(cx, |owner, _| {
            owner.chats.insert(target.session_uid.clone(), view.clone());
        });
        let mut id = layout::stable_path_hash(Path::new(&target.session_uid));
        while self
            .workspaces
            .iter()
            .any(|w| w.sessions.iter().any(|s| s.id.0 == id))
        {
            id = id.wrapping_add(1);
        }
        let pane_id = session_pane_id(SessionId(id));
        let pane = cx.new(|cx| {
            Pane::new(
                pane_id.clone(),
                "Claude preview".into(),
                PaneKind::PreviewChat(view),
                workspace,
                self.data.clone(),
                window,
                cx,
            )
        });
        self._subscriptions
            .push(sessions::subscribe(&pane, window, cx));
        self.registry
            .borrow_mut()
            .insert(pane_id.clone(), pane.downgrade());
        let state = self
            .workspaces
            .iter_mut()
            .find(|state| state.workspace.id == workspace)
            .expect("selected workspace");
        state.panes.insert(pane_id.clone(), pane);
        state.sessions.push(Session {
            id: SessionId(id),
            title: "Claude preview".into(),
            kind: data::SessionKind::Chat,
        });
        state.active_session = Some(SessionId(id));
        self.show_pane(&pane_id, window, cx);
    }
    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal_in(DockPlacement::Center, window, cx);
    }
    fn new_terminal_in(
        &mut self,
        placement: DockPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local || !self.workspaces.iter().any(|state| state.on_bar()) {
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
        let Some(cwd) = self.data.terminal_cwd(state.workspace.id, cx) else {
            self.owner.update(cx, |owner, cx| {
                owner.error =
                    Some("Remote workspace sessions are read-only in this preview".into());
                cx.notify();
            });
            return;
        };
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
            .push(sessions::subscribe(&pane, window, cx));
        self._subscriptions
            .push(cx.observe_in(&pane, window, |shell, pane, window, cx| {
                shell.wake_terminal(pane.clone(), window, cx);
                cx.notify();
            }));
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
        if placement == DockPlacement::Center {
            state.active_session = Some(SessionId(id));
        }
        if placement == DockPlacement::Bottom {
            let pane = state.panes[&pane_id].clone();
            state.dock.update(cx, |dock, cx| {
                dock.add_panel_view(panel_handle(pane), placement, None, window, cx);
            });
            state.store.regions.bottom_visible = true;
        }
        self.show_pane(&pane_id, window, cx);
    }
    #[cfg(test)]
    fn close_session(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .active(cx)
            .panes
            .get(&session_pane_id(id))
            .and_then(|pane| pane.read(cx).session.clone());
        if let Some(target) = target {
            self.close_target(&target, window, cx);
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
        self.sync_content_selection(cx);
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
        save_layout(state, cx);
    }
    Ok(())
}
fn save_layout(state: &mut WorkspaceDock, cx: &mut Context<Shell>) {
    if state.store.is_refused() {
        return;
    }
    let save = state.store.save_async(&state.dock.read(cx).dump(cx));
    let id = state.workspace.id;
    cx.spawn(async move |shell, cx| {
        if let Err(error) = crate::preview::finish(save).await {
            let error = format!("Save layout: {error:#}");
            eprintln!("{error}");
            let _ = shell.update(cx, |shell, cx| {
                if let Some(state) = shell
                    .workspaces
                    .iter_mut()
                    .find(|state| state.workspace.id == id)
                {
                    state.error = Some(error);
                    cx.notify();
                }
            });
        }
    })
    .detach();
}
fn observe_content_dock(dock: &Entity<DockArea>, cx: &mut Context<Shell>) -> Subscription {
    // Zoom changes notify the dock without emitting a persisted layout change.
    cx.observe(dock, |shell, _, cx| {
        shell.sync_content_selection(cx);
        cx.notify();
    })
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
        }
        let removed: Vec<_> = state
            .panes
            .iter()
            .filter(|(_, pane)| {
                matches!(pane.read(cx).content, pane::PaneContent::Content(_))
                    && dock
                        .read(cx)
                        .panel(PanelId::from(pane.entity_id()))
                        .is_none()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in removed {
            state.panes.remove(&id);
            this.registry.borrow_mut().remove(&id);
            this.content.update(cx, |content, _| content.close(&id));
        }
        save_layout(state, cx);
        this.sync_content_selection(cx);
        this.sync_git_visibility(true, cx);
        cx.notify();
    })
}
fn content_pane_id(workspace: WorkspaceId, path: &Path) -> String {
    format!("workspace-{}-content-{:?}", workspace.0, path)
}
fn content_pane_visible(state: &WorkspaceDock, id: &str, cx: &App) -> bool {
    let dock = state.dock.read(cx);
    if let Some(zoomed) = dock.zoomed_group() {
        let panel = PanelId::from(state.panes[id].entity_id());
        return [
            DockPlacement::Center,
            DockPlacement::Left,
            DockPlacement::Right,
            DockPlacement::Bottom,
        ]
        .into_iter()
        .filter_map(|placement| dock.layout(placement)?.find_node(zoomed))
        .any(|node| {
            matches!(node.kind(), gpui_component::dock::PaneRef::Tabs { panels, active_ix }
                if panels.get(active_ix) == Some(&panel))
        });
    }
    pane_id_visible(state, id, cx)
}
fn pane_visible(state: &WorkspaceDock, kind: &str, cx: &App) -> bool {
    pane_id_visible(
        state,
        &format!("workspace-{}-{kind}", state.workspace.layout_key),
        cx,
    )
}
fn pane_id_visible(state: &WorkspaceDock, id: &str, cx: &App) -> bool {
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
    visible(&layout.center, id)
        || [&layout.left_dock, &layout.right_dock, &layout.bottom_dock]
            .into_iter()
            .flatten()
            .any(|d| d.open() && visible(d.panel(), id))
}
fn build_workspace(
    workspace: Workspace,
    data: Rc<dyn ShellData>,
    config_dir: &Path,
    preview: Option<std::sync::Arc<crate::preview::Writer>>,
    session_ids: &mut HashSet<u64>,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Result<WorkspaceDock> {
    let path = layout::workspace_layout_path(config_dir, &workspace.layout_key);
    let mut store = match preview {
        Some(guard) => LayoutStore::guarded(path, guard),
        None => {
            #[cfg(test)]
            {
                LayoutStore::new(path)
            }
            #[cfg(not(test))]
            anyhow::bail!("missing preview context before layout load")
        }
    };
    let local = data.local_owner().is_some();
    let stopped = data
        .local_owner()
        .is_some_and(|owner| owner.read(cx).stopped_workspaces.contains(&workspace.id));
    let imported = data
        .local_owner()
        .is_some_and(|owner| owner.read(cx).iced_workspace(workspace.id).is_some());
    let sessions = if imported {
        let mut sessions = data.sessions(workspace.id, cx);
        let mut terminals = store
            .load_terminals()
            .unwrap_or_else(|error| {
                eprintln!(
                    "load preview terminal sessions {}: {error:#}",
                    store.path.display()
                );
                None
            })
            .unwrap_or_default();
        terminals.retain(|terminal| {
            !session_ids.contains(&terminal.id) && !sessions.iter().any(|s| s.id.0 == terminal.id)
        });
        sessions.extend(terminals.iter().map(|terminal| {
            Session {
                id: SessionId(terminal.id),
                title: terminal
                    .cwd
                    .file_name()
                    .unwrap_or(terminal.cwd.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                kind: data::SessionKind::Terminal,
            }
        }));
        store.terminals = Some(terminals);
        sessions
    } else if local {
        let restored = store.load_terminals().unwrap_or_else(|error| {
            eprintln!("load terminal sessions {}: {error:#}", store.path.display());
            None
        });
        let mut terminals = restored.unwrap_or_default();
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
                if let Some(text) = data.imported_session(workspace.id, session.id, cx) {
                    PaneKind::IcedSession(text)
                } else if let Some(terminals) = &store.terminals {
                    let cwd = terminals
                        .iter()
                        .find(|t| t.id == session.id.0)
                        .unwrap()
                        .cwd
                        .clone();
                    if stopped {
                        PaneKind::StoppedTerminal(cwd, true)
                    } else {
                        PaneKind::CenterTerminal(cwd)
                    }
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
        runtime_uid: sessions::fresh_target().tab_id as u64,
        parked: None,
        active_session: data
            .active_session(workspace.id, cx)
            .or_else(|| sessions.first().map(|s| s.id)),
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
    state.dock.update(cx, |dock, cx| {
        dock.set_center(DockLayout::h_split().child(center, None), window, cx);
        if has_terminal {
            dock.set_dock(DockPlacement::Bottom, bottom, window, cx);
            dock.set_dock_size(DockPlacement::Bottom, px(200.), window, cx);
        }
    });
}
impl Shell {
    fn spine(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let p = Palette::current(cx);
        div()
            .id("spine")
            .debug_selector(|| "spine".into())
            .w(px(layout::SPINE_WIDTH + 1.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(rgb(p.surface))
            .bg(rgb(p.mantle))
            .flex()
            .flex_col()
            .children(
                self.visible_workspace_indices(cx)
                    .into_iter()
                    .enumerate()
                    .map(|(index, i)| {
                        let state = &self.workspaces[i];
                        Button::new(("spine-workspace", index))
                            .ghost()
                            .label(
                                state
                                    .workspace
                                    .name
                                    .chars()
                                    .take(2)
                                    .collect::<String>()
                                    .to_uppercase(),
                            )
                            .tooltip(state.workspace.name.clone())
                            .w(px(36.))
                            .h(px(36.))
                            .selected(Some(state.workspace.id) == self.active_workspace(cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_workspace(index, window, cx)
                            }))
                    }),
            )
    }
    fn visible_workspace_indices(&self, cx: &App) -> Vec<usize> {
        if let Some(catalog) = self.owner.read(cx).iced_catalog() {
            let visible = catalog.visible_order(self.active_workspace(cx));
            return visible
                .into_iter()
                .filter(|&i| self.workspaces[i].on_bar())
                .collect();
        }
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| w.on_bar())
            .map(|(i, _)| i)
            .collect()
    }
    fn workspace_bar_items(&self, cx: &App) -> Vec<(usize, WorkspaceId, String)> {
        self.visible_workspace_indices(cx)
            .into_iter()
            .enumerate()
            .map(|(index, i)| {
                let state = &self.workspaces[i];
                let active = Some(state.workspace.id) == self.active_workspace(cx);
                let label = if active {
                    state.workspace.name.clone()
                } else {
                    state
                        .workspace
                        .name
                        .chars()
                        .take(2)
                        .collect::<String>()
                        .to_uppercase()
                };
                (index, state.workspace.id, label)
            })
            .collect()
    }
    fn workspace_bar(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let p = Palette::current(cx);
        let layout = self
            .owner
            .read(cx)
            .iced_catalog()
            .map(|catalog| catalog.bar_layout(self.active_workspace(cx)));
        let menu_items: Vec<_> = layout
            .as_ref()
            .into_iter()
            .flat_map(|layout| &layout.sections)
            .flat_map(|section| &section.workspaces)
            .filter_map(|&i| self.workspaces.get(i))
            .filter(|w| w.on_bar())
            .map(|w| (w.workspace.id, w.workspace.name.clone()))
            .collect();
        let owner = cx.entity().downgrade();
        let mut chips: Vec<_> = self
            .workspace_bar_items(cx)
            .into_iter()
            .map(|(index, id, label)| {
                div()
                    .id(("workspace-chip", index))
                    .debug_selector(move || format!("workspace-chip-{index}"))
                    .h_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .bg(rgb(if Some(id) == self.active_workspace(cx) {
                        p.surface
                    } else {
                        p.mantle
                    }))
                    .child(
                        Button::new(("workspace", index))
                            .ghost()
                            .selected(Some(id) == self.active_workspace(cx))
                            .label(label)
                            .h(px(27.))
                            .accessibility_label(
                                self.workspaces
                                    .iter()
                                    .find(|w| w.workspace.id == id)
                                    .expect("bar workspace")
                                    .workspace
                                    .name
                                    .clone(),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_workspace(index, window, cx)
                            })),
                    )
            })
            .collect();
        let split = layout
            .as_ref()
            .map(|layout| layout.work.len())
            .unwrap_or(chips.len());
        let after = chips.split_off(split.min(chips.len()));
        div()
            .id("workspace-bar")
            .debug_selector(|| "workspace-bar".into())
            .h(px(layout::WORKSPACE_BAR_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(rgb(p.mantle))
            .border_t_1()
            .border_color(rgb(p.surface))
            .children(chips)
            .when(layout.is_some(), |bar| {
                bar.child(
                    Button::new("iced-workspaces-menu")
                        .ghost()
                        .label("Repos ▾")
                        .dropdown_menu(move |menu, _, _| {
                            menu_items.iter().fold(menu, |menu, (id, name)| {
                                let owner = owner.clone();
                                let id = *id;
                                menu.item(PopupMenuItem::new(name.clone()).on_click(
                                    move |_, window, cx| {
                                        _ = owner.update(cx, |shell, cx| {
                                            shell.owner.update(cx, |owner, cx| {
                                                owner.dispatch(
                                                    Intent::Activate(id),
                                                    Instant::now(),
                                                    cx,
                                                )
                                            });
                                            shell.sync_git_visibility(false, cx);
                                            shell.sync_content_selection(cx);
                                            shell.focus.focus(window, cx);
                                            cx.notify();
                                        });
                                    },
                                ))
                            })
                        }),
                )
            })
            .children(after)
            .child(
                Button::new("new-workspace")
                    .ghost()
                    .label("+")
                    .tooltip("Workspaces are read from Iced GitTerm")
                    .disabled(self.owner.read(cx).iced_catalog().is_some())
                    .h(px(27.))
                    .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx))),
            )
    }
    fn update_regions(
        &mut self,
        change: impl FnOnce(&mut layout::RegionLayout),
        cx: &mut Context<Self>,
    ) {
        let active = self.active_workspace(cx);
        if let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| Some(w.workspace.id) == active)
        {
            change(&mut state.store.regions);
            save_layout(state, cx);
        }
        self.sync_git_visibility(true, cx);
        cx.notify();
    }
    fn sidebar(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let state = self.active(cx);
        let regions = &state.store.regions;
        let p = Palette::current(cx);
        let mut sidebar = div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .w(px(regions.sidebar_size()))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(p.mantle))
            .overflow_hidden();
        let mut modes = div()
            .flex()
            .when(regions.sidebar_collapsed, |row| row.flex_col());
        for (mode, title, icon) in [
            (SidebarMode::Work, "Work", "W"),
            (SidebarMode::Files, "Files", "F"),
            (SidebarMode::Find, "Find", "⌕"),
        ] {
            modes = modes.child(
                Button::new(title)
                    .ghost()
                    .label(if regions.sidebar_collapsed {
                        icon
                    } else {
                        title
                    })
                    .tooltip(title)
                    .h(px(36.))
                    .selected(regions.sidebar_mode == mode)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_regions(
                            |regions| {
                                regions.sidebar_mode = mode;
                                regions.sidebar_collapsed = false;
                            },
                            cx,
                        )
                    })),
            );
        }
        sidebar = sidebar.child(modes).child(
            Button::new("collapse-sidebar")
                .ghost()
                .label(if regions.sidebar_collapsed {
                    "›"
                } else {
                    "‹"
                })
                .tooltip(if regions.sidebar_collapsed {
                    "Expand sidebar"
                } else {
                    "Collapse sidebar"
                })
                .h(px(28.))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_regions(|r| r.sidebar_collapsed = !r.sidebar_collapsed, cx)
                })),
        );
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Files {
            for (kind, title) in [("git", "Git"), ("files", "Files")] {
                sidebar = sidebar.child(div().px_2().child(title)).child(
                    div().flex_1().min_h_0().child(
                        state.panes[&format!("workspace-{}-{kind}", state.workspace.layout_key)]
                            .clone(),
                    ),
                );
            }
        }
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Work {
            sidebar = sidebar.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .children(self.work_sidebars.get(&state.workspace.id).cloned()),
            );
        }
        sidebar
    }
    fn start_resize(
        &mut self,
        bottom: bool,
        position: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let regions = &self.active(cx).store.regions;
        self.resize = Some((
            bottom,
            position,
            if bottom {
                regions.bottom_height
            } else {
                regions.sidebar_width
            },
        ));
        self.content.update(cx, |content, cx| {
            content.resizing = true;
            content.sync_covers(window, cx);
        });
        cx.stop_propagation();
    }
    fn finish_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.resize.take().is_none() {
            return;
        }
        let active = self.active_workspace(cx);
        if let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| Some(w.workspace.id) == active)
        {
            save_layout(state, cx);
        }
        self.content.update(cx, |content, cx| {
            content.resizing = false;
            content.sync_covers(window, cx);
        });
    }
    fn sidebar_divider(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let collapsed = self.active(cx).store.regions.sidebar_collapsed;
        div()
            .id("sidebar-divider")
            .debug_selector(|| "sidebar-divider".into())
            .w(px(if collapsed { 1. } else { 4. }))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(Palette::current(cx).surface))
            .cursor_col_resize()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if !collapsed {
                        this.start_resize(false, f32::from(event.position.x), window, cx);
                    }
                }),
            )
    }
    fn session_content(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let state = self.active(cx);
        #[cfg(test)]
        if self.dock_fixture {
            return div()
                .id("dock-fixture")
                .flex_1()
                .min_w_0()
                .h_full()
                .child(state.dock.clone());
        }
        let selected = self
            .content
            .read(cx)
            .selected
            .as_ref()
            .and_then(|id| state.panes.get(id));
        let bottom = bottom_pane_ids(state, cx);
        let active = state.active_session.map(session_pane_id);
        let pane = selected
            .or_else(|| {
                active
                    .as_ref()
                    .filter(|id| pane_id_visible(state, id, cx) && !bottom.contains(id))
                    .and_then(|id| state.panes.get(id))
            })
            .or_else(|| {
                state
                    .panes
                    .iter()
                    .find(|(id, _)| {
                        !id.ends_with("-git")
                            && !id.ends_with("-files")
                            && pane_id_visible(state, id, cx)
                            && !bottom.contains(id)
                    })
                    .map(|(_, pane)| pane)
            });
        div()
            .id("session-content")
            .debug_selector(|| "session-content".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .children(pane.cloned())
    }
    fn bottom_panel(&self, cx: &Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let state = self.active(cx);
        let ids = bottom_pane_ids(state, cx);
        let pane = ids.iter().find_map(|id| {
            state
                .panes
                .get(id)
                .filter(|_| pane_id_visible(state, id, cx))
        });
        if !state.store.regions.has_bottom(pane.is_some()) {
            return None;
        }
        Some(
            div()
                .id("bottom-region")
                .flex()
                .flex_col()
                .flex_shrink_0()
                .child(
                    div()
                        .id("bottom-divider")
                        .debug_selector(|| "bottom-divider".into())
                        .h(px(layout::BOTTOM_DIVIDER))
                        .flex_shrink_0()
                        .cursor_row_resize()
                        .bg(rgb(Palette::current(cx).surface))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                                this.start_resize(true, f32::from(event.position.y), window, cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .id("bottom-panel")
                        .debug_selector(|| "bottom-panel".into())
                        .h(px(state.store.regions.bottom_height))
                        .overflow_hidden()
                        .children(pane.cloned()),
                ),
        )
    }
    fn open_bottom_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.local {
            return;
        }
        let existing = bottom_pane_ids(self.active(cx), cx);
        if !existing.is_empty() {
            let state = self.active(cx);
            if !state.dock.read(cx).is_dock_open(DockPlacement::Bottom) {
                state.dock.update(cx, |dock, cx| {
                    dock.toggle_dock(DockPlacement::Bottom, window, cx)
                });
            }
            self.update_regions(|r| r.bottom_visible = !r.bottom_visible, cx);
            return;
        }
        self.new_terminal_in(DockPlacement::Bottom, window, cx);
        let active = self.active_workspace(cx);
        let state = self
            .workspaces
            .iter_mut()
            .find(|w| Some(w.workspace.id) == active)
            .unwrap();
        save_layout(state, cx);
        cx.notify();
    }
}
fn bottom_pane_ids(state: &WorkspaceDock, cx: &App) -> Vec<String> {
    let mut layout = state.dock.read(cx).dump(cx);
    let Some(bottom) = layout.bottom_dock.take() else {
        return vec![];
    };
    layout.center = bottom.panel().clone();
    layout.left_dock = None;
    layout.right_dock = None;
    layout::pane_order(&layout)
        .into_iter()
        .filter(|id| {
            state.sessions.iter().any(|session| {
                session.kind == data::SessionKind::Terminal && session_pane_id(session.id) == *id
            })
        })
        .collect()
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_content_selection(cx);
        self.content
            .update(cx, |content, cx| content.sync_covers(window, cx));
        self.ensure_work_sidebars(window, cx);
        let p = Palette::current(cx);
        if !self.workspaces.iter().any(|state| state.on_bar()) {
            return div()
                .size_full()
                .flex()
                .bg(rgb(p.base))
                .text_color(rgb(p.text))
                .child(self.spine(cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .h(px(layout::SESSION_HEIGHT + 1.))
                                .flex_shrink_0()
                                .border_b_1()
                                .border_color(rgb(p.surface)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .p_4()
                                .child("No local workspaces yet")
                                .child(self.parked_controls(cx))
                                .children(
                                    self.owner
                                        .read(cx)
                                        .error
                                        .clone()
                                        .map(|error| div().text_color(rgb(p.red)).child(error)),
                                ),
                        )
                        .child(self.workspace_bar(cx)),
                )
                .into_any_element();
        }
        let state = self.active(cx);
        let active_session = state
            .active_session
            .and_then(|id| state.sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let sidebar = self.sidebar(cx);
        let tabs = TabBar::new("sessions")
            .underline()
            .selected_index(active_session)
            .h(px(layout::SESSION_HEIGHT))
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
        let close_target = close_session
            .and_then(|id| state.panes.get(&session_pane_id(id)))
            .and_then(|pane| pane.read(cx).session.clone());
        let state_session_count = state.sessions.len();
        let close_label = state
            .sessions
            .iter()
            .find(|session| Some(session.id) == close_session)
            .map(|session| format!("Close {} permanently", session.title))
            .unwrap_or_else(|| "Close session permanently".into());
        let history_owner = weak.clone();
        let history = self
            .owner
            .read(cx)
            .chat_history(self.data.terminal_cwd(state.workspace.id, cx).as_deref());
        let local = self.local;
        let more = Button::new("more")
            .ghost()
            .label("⋯")
            .tooltip("More")
            .debug_selector(|| "more".into())
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                let history_owner = history_owner.clone();
                let history = history.clone();
                let weak = weak.clone();
                let close_owner = weak.clone();
                menu.item({
                    let owner = weak.clone();
                    PopupMenuItem::new("New terminal")
                        .disabled(!local)
                        .on_click(move |_, window, cx| {
                            _ = owner.update(cx, |shell, cx| shell.new_terminal(window, cx));
                        })
                })
                .item({
                    let owner = weak.clone();
                    PopupMenuItem::new("Open bottom terminal")
                        .disabled(!local)
                        .on_click(move |_, window, cx| {
                            _ = owner
                                .update(cx, |shell, cx| shell.open_bottom_terminal(window, cx));
                        })
                })
                .submenu("Claude preview history", window, cx, move |menu, _, _| {
                    history.iter().fold(menu, |menu, saved| {
                        let owner = history_owner.clone();
                        let saved = saved.clone();
                        menu.item(
                            PopupMenuItem::new(format!(
                                "Claude · {}",
                                saved.session_id.as_deref().unwrap_or(&saved.uid)
                            ))
                            .on_click(move |_, window, cx| {
                                _ = owner.update(cx, |shell, cx| {
                                    shell.new_claude_chat(Some(saved.clone()), window, cx)
                                });
                            }),
                        )
                    })
                })
                .item(
                    PopupMenuItem::new("Keyboard shortcuts     ⌥/")
                        .on_click(|_, w, cx| dialogs::shortcuts(w, cx)),
                )
                .item({
                    let weak = close_owner.clone();
                    let close_target = close_target.clone();
                    PopupMenuItem::new(close_label.clone())
                        .disabled(close_target.is_none() || state_session_count <= 1)
                        .on_click(move |_, window, cx| {
                            if let Some(target) = close_target.clone() {
                                _ = weak
                                    .update(cx, |this, cx| this.close_target(&target, window, cx));
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
            });
        let mut content = div().flex_1().min_w_0().h_full().flex().flex_col().child(
            div()
                .flex()
                .items_center()
                .h(px(layout::SESSION_HEIGHT))
                .bg(rgb(p.mantle))
                .child(div().flex_1().min_w_0().child(tabs))
                .child(
                    Button::new("new-session")
                        .ghost()
                        .label("+ Session")
                        .debug_selector(|| "new-session".into())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_session_picker(window, cx)),
                        ),
                )
                .child(more)
                .child(
                    Button::new("close-workspace")
                        .ghost()
                        .label("×")
                        .tooltip("Close workspace (⌘⇧W)")
                        .debug_selector(|| "close-workspace".into())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_park_sheet(window, cx)),
                        ),
                )
                .border_b_1()
                .border_color(rgb(p.surface))
                .h(px(layout::SESSION_HEIGHT + 1.))
                .flex_shrink_0()
                .id("session-strip")
                .debug_selector(|| "session-strip".into()),
        );
        if let Some(error) = &self.owner.read(cx).error {
            content = content.child(div().p_2().text_color(rgb(p.red)).child(error.clone()));
        }
        if let Some(error) = &state.error {
            content = content.child(div().p_2().text_color(rgb(p.red)).child(error.clone()));
        }
        div()
            .relative()
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
            .on_action(cx.listener(|this, _: &CloseWorkspace, w, cx| this.open_park_sheet(w, cx)))
            .on_action(cx.listener(|this, _: &NewTerminal, w, cx| this.new_terminal(w, cx)))
            .on_action(
                cx.listener(|this, _: &NewClaudeChat, w, cx| this.new_claude_chat(None, w, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowShortcuts, w, cx| {
                dialogs::shortcuts(w, cx);
                this.content
                    .update(cx, |content, cx| content.sync_covers(w, cx));
            }))
            .flex()
            .font_family(".SystemUIFont")
            .text_size(px(UI_FONT_SIZE))
            .text_color(rgb(p.text))
            .bg(rgb(p.base))
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    if let Some((bottom, start, original)) = this.resize {
                        let active = this.active_workspace(cx);
                        if let Some(state) = this
                            .workspaces
                            .iter_mut()
                            .find(|w| Some(w.workspace.id) == active)
                        {
                            if bottom {
                                state.store.regions.bottom_height =
                                    (original + start - f32::from(event.position.y)).clamp(
                                        32.,
                                        (f32::from(window.viewport_size().height) - 140.).max(32.),
                                    );
                            } else {
                                state
                                    .store
                                    .regions
                                    .resize_sidebar(original + f32::from(event.position.x) - start);
                            }
                            cx.notify();
                        }
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.finish_resize(window, cx)),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.finish_resize(window, cx)),
            )
            .child(self.spine(cx))
            .child(
                content
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .child(sidebar)
                            .child(self.sidebar_divider(cx))
                            .child(self.session_content(cx)),
                    )
                    .children(self.bottom_panel(cx))
                    .child(self.workspace_bar(cx)),
            )
            .children(self.session_picker.clone())
            .children(self.park_sheet.clone())
            .into_any_element()
    }
}

#[cfg(test)]
fn load_local_workspaces(
    store: &mut workspace_config::WorkspaceStore,
    folders: Vec<std::path::PathBuf>,
) -> Result<(WorkspaceStatus, preview::SaveResult)> {
    let mut model = WorkspaceStatus::default();
    // A removed saved folder still reaches the normal status/error UI.
    for folder in store.load()? {
        model.add(folder, Instant::now());
    }
    for folder in folders {
        model.add(workspace_config::local_folder(&folder)?, Instant::now());
    }
    let save = store.save_async(&model.folders())?;
    Ok((model, save))
}

fn bind_workspace_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "cmd-shift-w",
        CloseWorkspace,
        Some("GitTermShell"),
    )]);
}

pub fn run(folders: Vec<std::path::PathBuf>, sample: bool) -> Result<()> {
    let preview = preview::PreviewContext::from_environment(sample)?;
    let config = preview.roots.layout();
    let mut store = workspace_config::WorkspaceStore::guarded(preview.writer());
    let (model, startup_save) = if sample {
        (WorkspaceStatus::default(), None)
    } else {
        ensure!(
            folders.is_empty(),
            "Preview workspaces come from Iced; add work in Iced GitTerm"
        );
        let model = workspace_config::load_iced_workspaces(&mut store);
        (model, None)
    };
    gpui_kit::application().run(move |cx| {
        preview.release_on_quit(cx);
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
        bind_workspace_keys(cx);
        cx.bind_keys([KeyBinding::new(
            "alt-/",
            ShowShortcuts,
            Some("GitTermShell"),
        )]);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(layout::WINDOW_WIDTH), px(layout::WINDOW_HEIGHT)),
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
                        let data: Rc<dyn ShellData> = Rc::new(data::SampleData);
                        let model =
                            WorkspaceStatus::sample(data.workspaces(cx).into_iter().map(|w| w.id));
                        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
                        Shell::with_owner(data, owner, &config, false, window, cx)
                            .expect("create sample shell")
                    } else {
                        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
                        if let Some(save) = startup_save {
                            owner.update(cx, |owner, cx| owner.watch_save(save, cx));
                        }
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
        let config_root = config.path().canonicalize().unwrap();
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
                    Some(crate::preview::tests::guarded_store(config_root.as_path())),
                    cx,
                )
            });
            let data = Rc::new(data::LocalData(owner.clone()));
            Shell::with_owner(data, owner, config_root.as_path(), true, window, cx).unwrap()
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
        let mut store = crate::preview::tests::guarded_store(config_root.as_path());
        assert_eq!(store.load().unwrap(), [repo.path().canonicalize().unwrap()]);
        // Local terminals start in their workspace folder, not $HOME.
        let folder = repo.path().canonicalize().unwrap();
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.new_terminal(window, cx)));
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
        cx.update(|window, cx| {
            dock.update(cx, |dock, cx| {
                dock.set_dock(
                    DockPlacement::Right,
                    DockLayout::tabs().panel_view(panel_handle(git.clone()), cx),
                    window,
                    cx,
                );
            })
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
        shell.update(cx, |shell, _| shell.dock_fixture = true);
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
        cx.simulate_resize(size(px(layout::WINDOW_WIDTH), px(layout::WINDOW_HEIGHT)));
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
            assert!(!dock.read(cx).has_dock(DockPlacement::Right));
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
