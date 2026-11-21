#![cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

mod agent_sidebar;
#[cfg(test)]
mod agent_tests;
pub mod app_menus;
pub mod bottom_panel;
#[cfg(test)]
mod center_tests;
mod chat;
pub mod connect_form;
pub mod phone;
pub mod content;
#[cfg(test)]
mod content_shell_tests;
pub mod data;
mod dialogs;
pub mod files;
mod files_view;
pub mod find_sidebar;
pub mod folder_picker;
#[cfg(test)]
mod h2_tests;
#[cfg(test)]
mod h4_tests;
#[cfg(test)]
mod h5_tests;
#[cfg(test)]
mod h6_tests;
#[cfg(test)]
mod h7_tests;
#[cfg(test)]
mod h8_tests;
#[cfg(test)]
mod h9_tests;
#[cfg(test)]
mod h10_tests;
#[cfg(test)]
mod hookup_tests;
mod layout;
mod owner;
mod pane;
#[cfg(test)]
mod page_tests;
mod park;
pub mod park_sheet;
pub mod preview;
#[cfg(test)]
mod region_tests;
pub mod remote;
pub mod remote_client;
mod remote_shell;
#[cfg(test)]
mod remote_shell_tests;
#[cfg(test)]
mod small_parity_tests;
mod review_reload;
pub mod session_picker;
pub mod session_strip;
mod sessions;
pub mod settings;
pub mod spine;
pub mod task_switcher;
pub mod tasks;
mod theme;
pub mod usage;
pub mod work_sidebar;
pub mod workspace_bar;
pub mod workspace_config;
pub mod workspace_settings_sheet;
pub mod workspace_sheet;
pub mod workspace_status;
pub use owner::{FileOpen, WorkspaceOwner};
use std::{cell::RefCell, time::Instant};
use workspace_status::{Intent, WorkspaceStatus};

use anyhow::{Result, ensure};
use data::{Session, SessionId, ShellData, Workspace, WorkspaceId};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, px, rgb,
    size,
};
use gpui_component::{
    Root,
    button::{Button, ButtonVariants},
    dock::{
        DockArea, DockEvent, DockLayout, DockPlacement, DockSkin, PanelId, PanelStyle,
        panel_handle, register_panel,
    },
};
use layout::{LayoutStore, SidebarMode};
use pane::{PANEL_NAME, Pane, PaneKind};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
};
use theme::Palette;

actions!(
    gitterm_shell,
    [NewTerminal, NewClaudeChat, ToggleBottomPanel]
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
    bottom: Entity<bottom_panel::BottomPanel>,
    run_tabs: HashMap<String, bottom_panel::RunTab>,
    run_url_poll: Option<gpui::Task<()>>,
    plans_open: bool,
    plans_return_focus: Option<FocusHandle>,
    usage_open: bool,
    usage_return_focus: Option<FocusHandle>,
    usage_page: Entity<usage::UsagePage>,
    usage_input: usage::UsageInput,
    usage_cache: gitterm_core::usage::UsageCache,
    usage_scan_task: Option<gpui::Task<()>>,
    codex_limits_task: Option<gpui::Task<()>>,
    /// Where usage sources are read from; tests point it at a fixture.
    usage_home: Option<std::path::PathBuf>,
    run_sidebar_subscriptions: HashMap<WorkspaceId, Subscription>,
    workspaces: Vec<WorkspaceDock>,
    owner: Entity<WorkspaceOwner>,
    registry: Rc<RefCell<HashMap<String, gpui::WeakEntity<Pane>>>>,
    config_dir: std::path::PathBuf,
    local: bool,
    selected_files: HashMap<WorkspaceId, FileOpen>,
    chat_find: HashSet<gpui::EntityId>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    resize: Option<(bool, f32, f32)>,
    task_switcher: Option<Entity<task_switcher::TaskSwitcher>>,
    task_switcher_subscription: Option<Subscription>,
    task_switcher_return_focus: Option<FocusHandle>,
    task_last_session: HashMap<String, SessionId>,
    selected_task: Option<String>,
    context_mru: Vec<Option<String>>,
    roster_subscriptions: HashMap<gpui::EntityId, (Subscription, Subscription)>,
    roster_observers: HashMap<WorkspaceId, Subscription>,
    roster_facts: Option<data::RosterFacts>,
    session_picker: Option<Entity<session_picker::SessionPicker>>,
    picker_subscription: Option<Subscription>,
    park_sheet: Option<Entity<park_sheet::ParkSheet>>,
    park_subscription: Option<Subscription>,
    new_work_sheet: Option<Entity<workspace_sheet::WorkspaceSheet>>,
    settings_sheet: Option<Entity<workspace_settings_sheet::WorkspaceSettingsSheet>>,
    settings_subscription: Option<Subscription>,
    new_work_subscription: Option<Subscription>,
    find_state: Rc<RefCell<find_sidebar::State>>,
    find_views: HashMap<String, find_sidebar::FindViews>,
    #[cfg(test)]
    find_home: Option<std::path::PathBuf>,
    work_state: Rc<RefCell<gitterm_core::work::state::State>>,
    saved_facts: workspace_bar::SavedFactsCache,
    saved_facts_error: Option<workspace_bar::News>,
    saved_facts_catalog_key: Option<String>,
    saved_facts_generation: u64,
    bar_menu: Option<workspace_bar::Menu>,
    work_sidebars: HashMap<WorkspaceId, Entity<work_sidebar::WorkSidebar>>,
    work_subscriptions: HashMap<WorkspaceId, Subscription>,
    agent_sidebars: HashMap<WorkspaceId, Entity<agent_sidebar::AgentSidebar>>,
    agent_subscriptions: HashMap<WorkspaceId, Subscription>,
    /// The branch checkout last opened in review per workspace; the sidebar
    /// marks it only while that review is still the visible viewer.
    branch_reviews: HashMap<WorkspaceId, std::path::PathBuf>,
    /// Sidebar panel width shared by every workspace, like Iced's one global
    /// setting. Read once from Iced's settings (read-only); a drag changes it
    /// for the session and is never written back to Iced.
    sidebar_width: f32,
    /// Remote machines, read-only: picker, connect form, sessions.
    remote: remote_shell::RemoteState,
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
    #[cfg(test)]
    fn with_owner(
        data: Rc<dyn ShellData>,
        owner: Entity<WorkspaceOwner>,
        config_dir: &Path,
        local: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        Self::with_owner_settings(
            data,
            owner,
            config_dir,
            local,
            &Default::default(),
            window,
            cx,
        )
    }
    // Keep the host-loaded settings explicit; shell construction must not reread Iced state.
    #[allow(clippy::too_many_arguments)]
    fn with_owner_settings(
        data: Rc<dyn ShellData>,
        owner: Entity<WorkspaceOwner>,
        config_dir: &Path,
        local: bool,
        settings: &workspace_config::IcedSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        let client_height = f32::from(window.viewport_size().height);
        let sidebar_width = layout::clamp_sidebar(settings.sidebar_width);
        let bottom = cx.new(|_| {
            bottom_panel::BottomPanel::new(bottom_panel::Input::from_settings(
                settings,
                client_height,
            ))
        });
        let usage_input = usage::UsageInput::default();
        let usage_page = cx.new(|_| usage::UsagePage::from_input(usage_input.clone()));
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
            if !state.store.path.exists() {
                state.store.regions.bottom_height = bottom.read(cx).input.height;
                state.store.regions.bottom_visible = settings.console_expanded;
            }
            load_workspace_layout(state, window, cx)?;
            state.store.regions.resize_sidebar(sidebar_width);
            if !bottom_pane_ids(state, cx).is_empty() {
                state.store.regions.bottom_opened = true;
            }
            subscriptions.push(subscribe_git(state, window, cx));
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
            this.refresh_saved_facts_if_catalog_changed(cx);
            cx.notify();
        }));
        subscriptions.push(cx.subscribe_in(
            &owner,
            window,
            |this, _, event: &FileOpen, window, cx| {
                this.selected_files.insert(event.workspace, event.clone());
                this.open_file_viewer(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &bottom,
            window,
            |shell, _, outcome: &bottom_panel::Outcome, window, cx| {
                shell.bottom_outcome(outcome.clone(), window, cx);
            },
        ));
        subscriptions.push(cx.subscribe(
            &usage_page,
            |shell, _, intent: &usage::UsageIntent, cx| {
                shell.usage_intent(intent.clone(), cx);
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
            bottom,
            run_tabs: HashMap::new(),
            run_url_poll: None,
            plans_open: false,
            plans_return_focus: None,
            usage_open: false,
            usage_return_focus: None,
            usage_page,
            usage_input,
            usage_cache: gitterm_core::usage::UsageCache::default(),
            usage_scan_task: None,
            codex_limits_task: None,
            usage_home: None,
            run_sidebar_subscriptions: HashMap::new(),
            workspaces,
            owner,
            registry,
            config_dir: config_dir.to_path_buf(),
            local,
            selected_files: HashMap::new(),
            chat_find: HashSet::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
            resize: None,
            task_switcher: None,
            task_switcher_subscription: None,
            task_switcher_return_focus: None,
            task_last_session: HashMap::new(),
            selected_task: None,
            context_mru: vec![None],
            roster_subscriptions: HashMap::new(),
            roster_observers: HashMap::new(),
            roster_facts: None,
            session_picker: None,
            picker_subscription: None,
            park_sheet: None,
            park_subscription: None,
            new_work_sheet: None,
            settings_sheet: None,
            settings_subscription: None,
            new_work_subscription: None,
            find_state: Default::default(),
            find_views: HashMap::new(),
            #[cfg(test)]
            find_home: None,
            work_state: Default::default(),
            saved_facts: workspace_bar::SavedFactsCache::default(),
            saved_facts_error: None,
            saved_facts_catalog_key: None,
            saved_facts_generation: 0,
            bar_menu: None,
            work_sidebars: HashMap::new(),
            work_subscriptions: HashMap::new(),
            agent_sidebars: HashMap::new(),
            agent_subscriptions: HashMap::new(),
            branch_reviews: HashMap::new(),
            sidebar_width,
            remote: Default::default(),
            #[cfg(test)]
            dock_fixture: false,
        };
        shell.reload_remotes(cx);
        shell.selected_task = shell
            .active_workspace(cx)
            .and_then(|_| shell.active(cx).active_session)
            .and_then(|id| shell.session_task(id, cx));
        shell.remember_context(shell.selected_task.clone());
        shell.restore_parked(saved_parked, cx);
        shell.refresh_saved_facts(cx);
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15))
                    .await;
                if weak
                    .update(cx, |shell, cx| shell.refresh_saved_facts(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        shell.load_active_iced_history(window, cx);
        if let Some(fonts) = shell.workspaces.iter().find_map(|w| w.store.regions.fonts) {
            theme::set_font_sizes(fonts, cx);
        }
        if let Some(size) = shell
            .workspaces
            .iter()
            .find_map(|w| w.store.regions.chat_font_size)
        {
            theme::set_chat_font_size(size, cx);
        }
        if let Some(width) = shell
            .workspaces
            .iter()
            .find_map(|w| w.store.regions.chat_width)
        {
            theme::set_chat_width(width, cx);
        }
        Ok(shell)
    }
    fn sync_content_selection(&self, cx: &mut Context<Self>) {
        let find_selected = self.active_workspace(cx).is_some()
            && self
                .find_state
                .borrow()
                .selected(&self.find_owner(cx))
                .is_some();
        let mut visible: Vec<String> = self
            .workspaces
            .iter()
            .filter(|w| {
                (!find_selected || self.find_preview_file_overlay(cx))
                    && Some(w.workspace.id) == self.active_workspace(cx)
            })
            .flat_map(|state| {
                state
                    .panes
                    .iter()
                    .filter(|(id, pane)| {
                        matches!(
                            pane.read(cx).content,
                            pane::PaneContent::Content(_) | pane::PaneContent::Review(_)
                        ) && content_pane_visible(state, id, cx)
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
    /// The OS folder picker (Iced's "Use macOS dialog…").
    pub(crate) fn pick_folder(
        &mut self,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.content.update(cx, |content, cx| {
            content.picker = true;
            content.sync_covers(window, cx);
        });
        let sheet = self.new_work_sheet.clone();
        let response = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.to_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = match response.await {
                Ok(Ok(Some(paths))) => Ok(paths.first().cloned()),
                Ok(Ok(None)) => Ok(None),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            };
            _ = this.update_in(cx, |this, window, cx| {
                this.finish_folder_pick(picked, sheet, window, cx)
            });
        })
        .detach();
    }
    /// A folder chosen on this Mac (OS picker or in-app picker): back to the
    /// New workspace sheet that asked for it, else added as a local folder.
    pub(crate) fn finish_folder_pick(
        &mut self,
        picked: Result<Option<std::path::PathBuf>>,
        sheet: Option<Entity<workspace_sheet::WorkspaceSheet>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = match picked {
                Ok(Some(path)) => {
                    cx.background_executor()
                        .spawn(async move { workspace_config::local_folder(&path).map(Some) })
                        .await
                }
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            _ = this.update_in(cx, |this, window, cx| {
                this.cover_sheet(this.new_work_sheet.is_some(), window, cx);
                if let Some(sheet) = sheet {
                    if this.new_work_sheet.as_ref() != Some(&sheet) {
                        return;
                    }
                    let (folder, error) = match result {
                        Ok(folder) => (folder, None),
                        Err(error) => (None, Some(format!("Pick folder: {error:#}"))),
                    };
                    sheet.update(cx, |sheet, cx| {
                        sheet.folder_picker_finished(folder, window, cx);
                        if let Some(error) = error {
                            sheet.show_error(error, cx);
                        }
                    });
                    return;
                }
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
            state.store.regions.resize_sidebar(self.sidebar_width);
            self._subscriptions.push(subscribe_git(&state, window, cx));
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
                    && matches!(
                        w.store.regions.sidebar_mode,
                        SidebarMode::Files | SidebarMode::Git
                    )
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
            self.pick_workspace_id(id, window, cx);
        }
    }
    fn select_workspace_id(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .workspaces
            .iter()
            .any(|w| w.workspace.id == id && w.on_bar())
        {
            return;
        }
        if Some(id) != self.active_workspace(cx) || self.new_work_sheet.is_some()
            || self.settings_sheet.is_some() {
            self.session_picker = None;
            self.picker_subscription = None;
            self.park_sheet = None;
            self.park_subscription = None;
            self.new_work_sheet = None;
            self.new_work_subscription = None;
            self.settings_sheet = None;
            self.settings_subscription = None;
            self.cover_sheet(false, window, cx);
        }
        self.owner.update(cx, |owner, cx| {
            owner.dispatch(Intent::Activate(id), Instant::now(), cx)
        });
        self.refresh_saved_facts(cx);
        self.selected_task = self
            .active(cx)
            .active_session
            .and_then(|id| self.session_task(id, cx));
        self.remember_context(self.selected_task.clone());
        self.load_active_iced_history(window, cx);
        self.sync_git_visibility(false, cx);
        self.sync_content_selection(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }
    fn select_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.is_empty() {
            return;
        }
        if let Some(session) = self.strip_input(cx).sessions.get(index) {
            let id = session.id;
            let active = self.active_workspace(cx);
            self.workspaces
                .iter_mut()
                .find(|w| Some(w.workspace.id) == active)
                .expect("active workspace exists")
                .active_session = Some(id);
            self.selected_task = self.session_task(id, cx);
            self.remember_context(self.selected_task.clone());
            if let Some(task) = &self.selected_task {
                self.task_last_session.insert(task.clone(), id);
            }
            self.show_pane(&session_pane_id(id), window, cx);
            self.load_active_iced_history(window, cx);
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
        self.new_terminal_at(placement, None, window, cx);
    }
    fn new_terminal_at(
        &mut self,
        placement: DockPlacement,
        cwd: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_terminal_launch(placement, cwd, None, window, cx);
    }
    fn new_terminal_launch(
        &mut self,
        placement: DockPlacement,
        cwd: Option<std::path::PathBuf>,
        launch: Option<gitterm_terminal::TerminalLaunch>,
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
        let Some(cwd) = cwd.or_else(|| self.data.terminal_cwd(state.workspace.id, cx)) else {
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
                if launch.is_some() {
                    PaneKind::StoppedTerminal(cwd.clone(), true)
                } else {
                    PaneKind::CenterTerminal(cwd.clone())
                },
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
            state.store.regions.bottom_opened = true;
        }
        if let Some(launch) = launch {
            let pane = self.active(cx).panes[&pane_id].clone();
            self.launch_run_terminal(&pane, launch, window, cx);
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
fn terminal_alive(pane: &Pane, cx: &App) -> bool {
    match &pane.content {
        pane::PaneContent::Terminal(view) => {
            pane.terminal_alive()
                && matches!(
                    view.read(cx).backend().state(),
                    gitterm_terminal::SessionState::Running
                )
        }
        _ => false,
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
fn subscribe_git(
    state: &WorkspaceDock,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Subscription {
    let workspace = state.workspace.id;
    let git = state.panes[&format!("workspace-{}-git", state.workspace.layout_key)]
        .read(cx)
        .git
        .clone()
        .expect("Git pane");
    cx.subscribe_in(
        &git,
        window,
        move |shell, _, action: &pane::git_view::GitAction, window, cx| {
            shell.git_action(workspace, action, window, cx);
        },
    )
}
fn subscribe_dock(state: &WorkspaceDock, cx: &mut Context<Shell>) -> Subscription {
    let id = state.workspace.id;
    cx.subscribe(&state.dock, move |this, dock, _: &DockEvent, cx| {
        let imported = this.owner.read(cx).iced_workspace(id).is_some();
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
                let position = terminals.iter().position(|t| t.id == session.id.0);
                if imported {
                    (position.is_some(), position.unwrap_or(0))
                } else {
                    (false, position.unwrap_or(usize::MAX))
                }
            });
        }
        let removed: Vec<_> = state
            .panes
            .iter()
            .filter(|(id, pane)| {
                matches!(pane.read(cx).content, pane::PaneContent::Content(_))
                    && dock
                        .read(cx)
                        .panel(PanelId::from(pane.entity_id()))
                        .is_none()
                    // Iced keeps a closed Page hidden, not destroyed.
                    && this.content.read(cx).page(id).is_none()
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
                        // Like Iced, restore a plain shell now, without a saved run command.
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
    for session in &state.sessions {
        let pane = panel_handle(state.panes[&session_pane_id(session.id)].clone());
        center = center.panel_view(pane, cx);
    }
    state.dock.update(cx, |dock, cx| {
        dock.set_center(DockLayout::h_split().child(center, None), window, cx);
    });
}
impl Shell {
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
    fn chrome_workspaces(&self, cx: &App) -> Vec<workspace_bar::Workspace> {
        self.workspaces
            .iter()
            .map(|state| {
                let saved = self.owner.read(cx).iced_workspace(state.workspace.id);
                let group = match saved.and_then(|w| w.location.as_ref()) {
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent {
                        remote_id,
                        ..
                    }) => format!("agent:{remote_id}"),
                    _ => saved
                        .filter(|w| w.is_legacy_remote())
                        .map(|w| format!("ssh:{}", w.remote_session.as_ref().unwrap().session_name))
                        .unwrap_or_else(|| "local".into()),
                };
                let mut attention = vec![];
                if let (Some(saved), Some(sidebar)) =
                    (saved, self.work_sidebars.get(&state.workspace.id))
                {
                    use gitterm_core::tasks::{DelegationStatus, TaskAttentionReason};
                    let sidebar = sidebar.read(cx);
                    for tab in &saved.tabs {
                        let task = tab
                            .task_id
                            .as_ref()
                            .and_then(|id| sidebar.tasks().iter().find(|t| &t.task_id == id));
                        let mut kind = task.and_then(|task| {
                            if task.attention.reason.is_none() && !task.attention.unread {
                                return None;
                            }
                            Some(match task.attention.reason {
                                Some(TaskAttentionReason::ExecutionFailed) => {
                                    workspace_bar::AttentionKind::Failed
                                }
                                Some(
                                    TaskAttentionReason::CompletedUnread
                                    | TaskAttentionReason::ReadyForReview,
                                )
                                | None => workspace_bar::AttentionKind::Review,
                                _ => workspace_bar::AttentionKind::NeedsYou,
                            })
                        });
                        for worker in sidebar.workers().iter().filter(|worker| {
                            worker.dismissed_at.is_none()
                                && tab.session_uid.as_deref()
                                    == Some(worker.parent.session_uid.as_str())
                        }) {
                            let next = match worker.status {
                                DelegationStatus::Failed { .. } => {
                                    Some(workspace_bar::AttentionKind::Failed)
                                }
                                DelegationStatus::Blocked | DelegationStatus::Interrupted => {
                                    Some(workspace_bar::AttentionKind::NeedsYou)
                                }
                                DelegationStatus::Completed => {
                                    Some(workspace_bar::AttentionKind::Review)
                                }
                                _ => None,
                            };
                            let priority = |kind| match kind {
                                workspace_bar::AttentionKind::NeedsYou => 0,
                                workspace_bar::AttentionKind::Failed => 1,
                                workspace_bar::AttentionKind::Review => 2,
                            };
                            kind = kind
                                .into_iter()
                                .chain(next)
                                .min_by_key(|kind| priority(*kind));
                        }
                        attention.extend(kind);
                    }
                }
                workspace_bar::Workspace {
                    id: state.workspace.id,
                    name: state.workspace.name.clone(),
                    color: saved
                        .map(|w| w.color)
                        .unwrap_or(gitterm_core::settings::WorkspaceColor::Lavender),
                    placement: gitterm_core::workspace::BarChip {
                        group,
                        work: saved
                            .is_some_and(|w| gitterm_core::work::catalog::is_work(&w.chats, false)),
                        parked: !state.on_bar(),
                    },
                    attention: attention
                        .iter()
                        .min_by_key(|kind| match kind {
                            workspace_bar::AttentionKind::NeedsYou => 0,
                            workspace_bar::AttentionKind::Failed => 1,
                            workspace_bar::AttentionKind::Review => 2,
                        })
                        .map(|kind| workspace_bar::Attention {
                            count: attention.len(),
                            kind: *kind,
                            reason: "Sessions need attention".into(),
                        }),
                    woke: None,
                    pr_news: None,
                }
            })
            .collect()
    }
    fn repos_on_bar(&self, cx: &App) -> bool {
        self.owner
            .read(cx)
            .iced_catalog()
            .is_none_or(|c| c.repos_on_bar)
    }
    fn bar_input(&self, cx: &App) -> workspace_bar::Input {
        let active = self
            .workspaces
            .iter()
            .find(|w| Some(w.workspace.id) == self.active_workspace(cx));
        let colors = workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark());
        let mut workspaces = self.chrome_workspaces(cx);
        let facts = self.saved_facts.snapshot();
        for workspace in &mut workspaces {
            let count = facts
                .attention_counts
                .get(&workspace.id)
                .copied()
                .filter(|count| *count > 0);
            let kind = workspace
                .attention
                .as_ref()
                .map_or(workspace_bar::AttentionKind::NeedsYou, |attention| {
                    attention.kind
                });
            workspace.attention = count.map(|count| workspace_bar::Attention {
                count,
                kind,
                reason: "Sessions need attention".into(),
            });
        }
        let mut machine_news = facts.machines.clone();
        if let Some(error) = &self.saved_facts_error {
            machine_news.push(error.clone());
        }
        let mut news = facts.news.clone();
        for workspace in &mut workspaces {
            workspace.pr_news = news.remove(&workspace.id);
        }
        let hidden = gitterm_core::workspace::bar_layout(
            &workspaces
                .iter()
                .map(|w| w.placement.clone())
                .collect::<Vec<_>>(),
            workspaces
                .iter()
                .position(|w| Some(w.id) == self.active_workspace(cx))
                .unwrap_or(usize::MAX),
            self.repos_on_bar(cx),
        )
        .hidden
        .len();
        workspace_bar::Input {
            workspaces,
            active: self.active_workspace(cx),
            repos_on_bar: self.repos_on_bar(cx),
            repos_open: self.bar_menu == Some(workspace_bar::Menu::Repos),
            machine_news,
            repo_machines: facts.repo_machines.clone(),
            remote_workspaces: facts.remote_workspaces.clone(),
            pulse_bright: false,
            ready_count: facts.ready,
            ready_news_count: facts.news_count,
            // Iced counts both saved parks and open work hidden in Repos.
            parked_count: facts.parked
                + self.owner.read(cx).saved_parked().len()
                + hidden
                + self.workspaces.iter().filter(|w| !w.on_bar()).count(),
            terminal_expanded: active.is_some_and(|w| w.store.regions.has_bottom(false)),
            terminal_running: active.is_some_and(|state| {
                bottom_pane_ids(state, cx).iter().any(|id| {
                    self.run_tabs.contains_key(id)
                        && state
                            .panes
                            .get(id)
                            .is_some_and(|pane| terminal_alive(pane.read(cx), cx))
                })
            }),
            browser: workspace_bar::Browser {
                color: colors.muted,
                driving: false,
                hint: "Browser idle".into(),
                action: Some(workspace_bar::BrowserAction::Open),
                has_evidence: false,
            },
            more_open: self.bar_menu == Some(workspace_bar::Menu::More),
        }
    }
    fn refresh_saved_facts_if_catalog_changed(&mut self, cx: &mut Context<Self>) {
        let key = self
            .owner
            .read(cx)
            .iced_catalog()
            .map(Self::saved_facts_catalog_key);
        if key != self.saved_facts_catalog_key {
            self.refresh_saved_facts(cx);
        }
    }
    fn saved_facts_catalog_key(catalog: &workspace_config::IcedCatalog) -> String {
        format!(
            "{}:{}",
            catalog.source.display(),
            serde_json::to_string(&catalog.workspaces).unwrap_or_default()
        )
    }
    fn refresh_saved_facts(&mut self, cx: &mut Context<Self>) {
        let Some(catalog) = self.owner.read(cx).iced_catalog() else {
            self.roster_facts = None;
            self.saved_facts
                .replace(workspace_bar::SavedFacts::default());
            self.saved_facts_error = None;
            self.saved_facts_catalog_key = None;
            self.saved_facts_generation = self.saved_facts_generation.wrapping_add(1);
            cx.notify();
            return;
        };
        let source = catalog.source.clone();
        let workspaces = catalog.workspaces.clone();
        let key = Self::saved_facts_catalog_key(catalog);
        self.saved_facts_catalog_key = Some(key);
        self.saved_facts_generation = self.saved_facts_generation.wrapping_add(1);
        let generation = self.saved_facts_generation;
        let roots = {
            let state = self.work_state.borrow();
            self.workspaces
                .iter()
                .map(|workspace| {
                    (
                        workspace.workspace.id,
                        state
                            .pane(workspace.workspace.id.0)
                            .map(|pane| {
                                pane.summary
                                    .coordinators
                                    .iter()
                                    .filter(|root| root.recent)
                                    .map(|root| root.id.clone())
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                })
                .collect()
        };
        let colors = workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark());
        cx.spawn(async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    (
                        workspace_bar::SavedFacts::read_from(&source, &workspaces, &roots, colors),
                        data::RosterFacts::read_from(&source),
                    )
                })
                .await;
            let _ = shell.update(cx, |shell, cx| {
                if shell.saved_facts_generation != generation {
                    return;
                }
                let Some(catalog) = shell.owner.read(cx).iced_catalog() else {
                    return;
                };
                if Self::saved_facts_catalog_key(catalog)
                    != shell.saved_facts_catalog_key.as_deref().unwrap_or_default()
                {
                    return;
                }
                let source = catalog.source.clone();
                match result.1 {
                    Ok(facts) => {
                        shell.roster_facts = Some(facts);
                        shell.sync_rosters(cx);
                    }
                    Err(error) => eprintln!("read Iced roster: {error:#}"),
                }
                match result.0 {
                    Ok(facts) => {
                        shell.saved_facts.replace(facts);
                        shell.saved_facts_error = None;
                    }
                    Err(error) => {
                        let path = source.join("workspaces.json");
                        eprintln!("read workspace bar facts {}: {error:#}", path.display());
                        shell.saved_facts_error = Some(workspace_bar::News {
                            label: format!("Workspace bar: {error:#}"),
                            color: workspace_bar::Colors::new(
                                gpui_component::Theme::global(cx).mode.is_dark(),
                            )
                            .red,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn spine_input(&self, cx: &App) -> spine::Input {
        spine::Input {
            workspaces: self.chrome_workspaces(cx),
            active: self.active_workspace(cx),
            repos_on_bar: self.repos_on_bar(cx),
            attention: self
                .workspaces
                .iter()
                .flat_map(|state| {
                    let root = self.data.terminal_cwd(state.workspace.id, cx);
                    self.work_sidebars
                        .get(&state.workspace.id)
                        .map(|sidebar| {
                            spine::task_rows(
                                state.workspace.id,
                                sidebar.read(cx).tasks().iter().filter(|task| {
                                    match &task.workspace.location {
                                        gitterm_core::tasks::WorkspaceLocationIdentity::Local {
                                            directory,
                                        } => root.as_ref() == Some(directory),
                                        _ => false,
                                    }
                                }),
                                std::time::SystemTime::now(),
                            )
                        })
                        .unwrap_or_default()
                })
                .collect(),
            frontmost: self
                .workspaces
                .iter()
                .find(|w| Some(w.workspace.id) == self.active_workspace(cx))
                .and_then(|w| {
                    w.active_session
                        .map(|s| (w.workspace.id, spine::Target::Tab(s.0)))
                }),
            running: self
                .workspaces
                .iter()
                .flat_map(|state| {
                    state.sessions.iter().filter_map(|session| {
                        state
                            .panes
                            .get(&session_pane_id(session.id))
                            .and_then(|pane| match &pane.read(cx).content {
                                pane::PaneContent::PreviewChat(view)
                                    if view.read(cx).is_working() =>
                                {
                                    Some((state.workspace.id, spine::Target::Tab(session.id.0)))
                                }
                                _ => None,
                            })
                    })
                })
                .collect(),
            pulse_bright: false,
            usage_open: self.usage_open,
            usage_hint: "Usage".into(),
            usage_badge: None,
        }
    }
    fn strip_input(&self, cx: &App) -> session_strip::Input {
        let state = self
            .workspaces
            .iter()
            .find(|w| Some(w.workspace.id) == self.active_workspace(cx));
        let saved = state.and_then(|w| self.owner.read(cx).iced_workspace(w.workspace.id));
        session_strip::Input {
            workspace: state.map(|w| w.workspace.id),
            workspace_name: state.map(|w| w.workspace.name.clone()).unwrap_or_default(),
            workspace_color: saved
                .map(|w| w.color)
                .unwrap_or(gitterm_core::settings::WorkspaceColor::Lavender),
            visible_workspace_count: self.visible_workspace_indices(cx).len(),
            sessions: state
                .into_iter()
                .flat_map(|w| &w.sessions)
                .filter(|s| !bottom_pane_ids(state.unwrap(), cx).contains(&session_pane_id(s.id)))
                .filter(|s| self.session_task(s.id, cx) == self.selected_task)
                .map(|s| {
                    let tab = saved.and_then(|saved| {
                        data::iced_sessions(saved)
                            .iter()
                            .position(|session| session.id == s.id)
                            .and_then(|index| saved.tabs.get(index))
                    });
                    // Imported UUIDs are identities, never chat titles. Until the
                    // index resolves one, use Iced's ordinary folder fallback.
                    let untitled = tab.is_some_and(|tab| {
                        tab.chat_session_id.as_deref() == Some(s.title.as_str())
                    });
                    let sidebar = state
                        .and_then(|state| self.work_sidebars.get(&state.workspace.id))
                        .map(|sidebar| sidebar.read(cx));
                    let tasks = sidebar
                        .map(|sidebar| sidebar.tasks())
                        .or_else(|| {
                            self.roster_facts
                                .as_ref()
                                .map(|facts| facts.tasks.as_slice())
                        })
                        .unwrap_or_default();
                    let workers = sidebar
                        .map(|sidebar| sidebar.workers())
                        .or_else(|| {
                            self.roster_facts
                                .as_ref()
                                .map(|facts| facts.workers.as_slice())
                        })
                        .unwrap_or_default();
                    let task = tab
                        .and_then(|tab| tab.task_id.as_ref())
                        .and_then(|id| tasks.iter().find(|task| &task.task_id == id));
                    let running = state
                        .and_then(|state| state.panes.get(&session_pane_id(s.id)))
                        .is_some_and(|pane| {
                            matches!(&pane.read(cx).content,
                            pane::PaneContent::PreviewChat(view) if view.read(cx).is_working())
                        });
                    session_strip::Session {
                        id: s.id,
                        kind: s.kind,
                        display_title: if untitled {
                            tab.unwrap().dir.clone()
                        } else {
                            state.unwrap().panes[&session_pane_id(s.id)]
                                .read(cx)
                                .tab_title()
                        },
                        chat_title: (s.kind == data::SessionKind::Chat && !untitled)
                            .then(|| s.title.clone()),
                        task_session_label: task.and_then(|task| {
                            tab.and_then(|tab| tab.task_session_id.as_ref())
                                .and_then(|id| {
                                    task.sessions
                                        .iter()
                                        .find(|session| &session.task_session_id == id)
                                })
                                .map(|session| session.label.clone())
                        }),
                        attention: session_strip::attention(
                            task.map(|task| &task.attention),
                            workers,
                            tab.and_then(|tab| tab.session_uid.as_deref()),
                        ),
                        running,
                        // ChatPane does not yet expose its pending-request projection.
                        pending_requests: 0,
                    }
                })
                .collect(),
            active: state.and_then(|w| w.active_session),
            remote: state.is_some_and(|w| self.data.terminal_cwd(w.workspace.id, cx).is_none()),
            task: self
                .selected_task
                .as_ref()
                .and_then(|id| self.task_records(cx).into_iter().find(|t| &t.task_id == id))
                .map(|t| session_strip::TaskContext {
                    id: t.task_id,
                    title: t.title,
                    issue: t.issue.map(|i| i.key),
                }),
            branch: state
                .map(|w| self.data.status(w.workspace.id, cx).branch)
                .unwrap_or_default(),
            pulse_bright: false,
            mic: None,
            worktree_follow: None,
        }
    }
    fn spine(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let weak = cx.weak_entity();
        spine::render(
            &self.spine_input(cx),
            workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark()),
            Rc::new(move |outcome, window, cx| {
                _ = weak.update(cx, |shell, cx| match outcome {
                    spine::Outcome::ToggleUsage => shell.toggle_usage(window, cx),
                    spine::Outcome::Jump {
                        target: spine::Target::Task(task),
                        ..
                    } => {
                        shell.activate_task(&task, window, cx);
                    }
                    outcome => shell.spine_outcome(outcome, window, cx),
                });
            }),
        )
        .w(px(layout::SPINE_WIDTH + 1.))
    }
    fn chrome_unavailable(&self, label: &str, cx: &mut Context<Self>) {
        self.owner.update(cx, |owner, cx| {
            owner.error = Some(format!("{label} · not available in preview"));
            cx.notify();
        });
    }
    fn workspace_bar(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let weak = cx.weak_entity();
        let menu_owner = weak.clone();
        workspace_bar::render(
            &self.bar_input(cx),
            workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark()),
            Rc::new(move |outcome, window, cx| {
                _ = weak.update(cx, |shell, cx| {
                    use workspace_bar::Outcome as O;
                    match outcome {
                        O::NewWork => shell.open_recent_work_sheet(window, cx),
                        O::OpenSettings(id) => shell.open_settings_sheet(id, window, cx),
                        O::OpenReady => shell.open_ready_list(window, cx),
                        O::OpenParked => shell.open_parked_list(window, cx),
                        O::Browser(workspace_bar::BrowserAction::Open) => {
                            shell.chrome_unavailable("Open browser", cx)
                        }
                        O::Browser(workspace_bar::BrowserAction::Focus) => {
                            shell.chrome_unavailable("Focus browser", cx)
                        }
                        outcome => shell.bar_outcome(outcome, window, cx),
                    }
                });
            }),
            Rc::new(move |change, _, cx| {
                let _ = menu_owner.update(cx, |shell, cx| {
                    if change.open {
                        shell.bar_menu = Some(change.menu);
                    } else if shell.bar_menu == Some(change.menu) {
                        shell.bar_menu = None;
                    }
                    cx.notify();
                });
            }),
        )
        .h(px(layout::WORKSPACE_BAR_HEIGHT))
    }
    fn session_strip(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let weak = cx.weak_entity();
        session_strip::render(
            &self.strip_input(cx),
            workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark()),
            Rc::new(move |outcome, window, cx| {
                _ = weak.update(cx, |shell, cx| match outcome {
                    session_strip::Outcome::BackToWorkspace => {
                        shell.general_context(false, window, cx)
                    }
                    session_strip::Outcome::TaskOverview => shell.general_context(true, window, cx),
                    session_strip::Outcome::ToggleWorktreeFollow => {
                        let input = shell.strip_input(cx);
                        if let Some(follow) = &input.worktree_follow {
                            shell.chrome_unavailable(&follow.text(&input.branch), cx);
                        }
                    }
                    outcome => shell.strip_outcome(outcome, window, cx),
                });
            }),
        )
        .h(px(layout::SESSION_HEIGHT + 1.))
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
    const SIDEBAR_MODES: [(SidebarMode, &'static str, &'static str); 3] = [
        (SidebarMode::Work, "Work", "▦"),
        (SidebarMode::Files, "Files", "📁"),
        (SidebarMode::Find, "Find", "≋"),
    ];
    fn sidebar_chevron(collapsed: bool) -> &'static str {
        if collapsed { "▶" } else { "◀" }
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
            .items_center()
            .when(regions.sidebar_collapsed, |row| row.flex_col());
        for (mode, title, icon) in Self::SIDEBAR_MODES {
            let selected = regions.sidebar_mode == mode
                || (mode == SidebarMode::Work
                    && matches!(regions.sidebar_mode, SidebarMode::Git | SidebarMode::Agent));
            modes = modes.child(
                div()
                    .when(!regions.sidebar_collapsed, |s| s.flex_1().border_b_2())
                    .when(regions.sidebar_collapsed, |s| s.w_full().border_l_2())
                    .min_w_0()
                    .border_color(if selected {
                        rgb(p.blue).into()
                    } else {
                        gpui::transparent_black()
                    })
                    .child(
                        Button::new(title)
                            .debug_selector(move || title.into())
                            .ghost()
                            .label(if regions.sidebar_collapsed {
                                icon
                            } else {
                                title
                            })
                            .h(px(if regions.sidebar_collapsed { 40. } else { 26. }))
                            .w_full()
                            .when(!regions.sidebar_collapsed, |s| {
                                s.justify_start().px(px(10.))
                            })
                            .when(regions.sidebar_collapsed, |s| s.justify_center().px_0())
                            .rounded_none()
                            .text_size(px(12.))
                            .text_color(rgb(if selected { p.text } else { p.subtext0 }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update_regions(
                                    |regions| {
                                        regions.sidebar_mode = mode;
                                        regions.sidebar_collapsed = false;
                                    },
                                    cx,
                                )
                            })),
                    ),
            );
        }
        let collapse = Button::new("collapse-sidebar")
            .debug_selector(|| "collapse-sidebar".into())
            .ghost()
            .label(Self::sidebar_chevron(regions.sidebar_collapsed))
            .text_size(px(10.))
            .text_color(rgb(p.subtext0))
            .h(px(if regions.sidebar_collapsed { 30. } else { 28. }))
            .when(regions.sidebar_collapsed, |s| s.w_full())
            .when(!regions.sidebar_collapsed, |s| {
                s.custom(
                    gpui_component::button::ButtonCustomVariant::new(cx)
                        .foreground(rgb(p.subtext0).into()),
                )
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.update_regions(|r| r.sidebar_collapsed = !r.sidebar_collapsed, cx)
            }));
        if regions.sidebar_collapsed {
            sidebar = sidebar.child(modes).child(div().flex_1()).child(collapse);
        } else {
            sidebar = sidebar.child(modes.child(collapse));
        }
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Files {
            sidebar = self.files_sidebar(sidebar, cx);
        }
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Git {
            sidebar = self.git_sidebar(sidebar, cx);
        }
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Agent {
            sidebar = sidebar.children(self.agent_sidebars.get(&state.workspace.id).cloned());
        }
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Work {
            sidebar = sidebar.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .children(self.work_sidebars.get(&state.workspace.id).cloned()),
            );
        }
        if !regions.sidebar_collapsed && regions.sidebar_mode == SidebarMode::Find {
            sidebar = sidebar.child(
                div().flex_1().min_h_0().children(
                    self.find_views
                        .get(&self.find_owner(cx))
                        .map(|views| views.sidebar.clone()),
                ),
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
        if let Some(task) = self
            .selected_task
            .as_ref()
            .and_then(|id| self.task_records(cx).into_iter().find(|t| &t.task_id == id))
            && state
                .active_session
                .and_then(|id| self.session_task(id, cx))
                .is_none()
        {
            return div()
                .id("task-overview")
                .debug_selector(|| "task-overview".into())
                .flex_1()
                .h_full()
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .child(task.title.clone())
                .child(task_switcher::state_copy(&task, &[], false))
                .child(task.objective.clone());
        }
        #[cfg(test)]
        if self.dock_fixture {
            return div()
                .id("dock-fixture")
                .flex_1()
                .min_w_0()
                .h_full()
                .child(state.dock.clone());
        }
        if state.active_session.is_none() {
            if let Some(home) = self.remote_session_home(cx) {
                return home;
            }
            return div()
                .id("session-content")
                .debug_selector(|| "session-content".into())
                .flex_1()
                .min_w_0()
                .h_full();
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
            .when(
                pane.is_some_and(|pane| {
                    matches!(
                        pane.read(cx).content,
                        pane::PaneContent::Chat(_) | pane::PaneContent::PreviewChat(_)
                    )
                }),
                |pane| pane.font_family(".SystemUIFont"),
            )
            .when(
                pane.is_some_and(|p| matches!(p.read(cx).content, pane::PaneContent::Terminal(_))),
                |host| {
                    host.capture_any_mouse_down(|event, _, cx| {
                        if event.button == gpui::MouseButton::Right {
                            cx.stop_propagation();
                        }
                    })
                    .capture_any_mouse_up(|event, _, cx| {
                        if event.button == gpui::MouseButton::Right {
                            cx.stop_propagation();
                        }
                    })
                },
            )
            .children(pane.cloned())
    }
    fn refresh_bottom_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_tabs.retain(|id, _| {
            self.workspaces
                .iter()
                .any(|state| state.panes.contains_key(id))
        });
        let now = Instant::now();
        for state in &self.workspaces {
            for (id, pane) in &state.panes {
                if let Some(run) = self.run_tabs.get_mut(id)
                    && run.detected_url.is_none()
                    && now >= run.next_url_scan
                {
                    run.next_url_scan = now + std::time::Duration::from_secs(2);
                    if let pane::PaneContent::Terminal(view) = &pane.read(cx).content {
                        run.detected_url = view
                            .read(cx)
                            .backend()
                            .get_all_text()
                            .as_deref()
                            .and_then(bottom_panel::output_url);
                    }
                }
            }
        }
        let Some(state) = self
            .workspaces
            .iter()
            .find(|w| Some(w.workspace.id) == self.active_workspace(cx))
        else {
            return;
        };
        let ids = bottom_pane_ids(state, cx);
        let selected = ids
            .iter()
            .find(|id| pane_id_visible(state, id, cx))
            .or_else(|| ids.first());
        let mut input = self.bottom.read(cx).input.clone();
        input.opened = state.store.regions.bottom_opened || !ids.is_empty();
        input.expanded = state.store.regions.bottom_visible;
        input.window_height = f32::from(window.viewport_size().height);
        input.height =
            bottom_panel::clamp_height(state.store.regions.bottom_height, input.window_height);
        input.tabs = state
            .sessions
            .iter()
            .filter(|s| ids.contains(&session_pane_id(s.id)))
            .map(|s| {
                let pane = state.panes[&session_pane_id(s.id)].read(cx);
                bottom_panel::TerminalTab {
                    id: s.id.0 as usize,
                    title: Some(pane.tab_title()),
                    alive: terminal_alive(pane, cx),
                    run_repo: self
                        .run_tabs
                        .get(&session_pane_id(s.id))
                        .map(|run| run.repo.clone()),
                }
            })
            .collect();
        input.active = state
            .sessions
            .iter()
            .find(|s| selected == Some(&session_pane_id(s.id)))
            .map(|s| s.id.0 as usize);
        input.repos = self.bottom_repos(cx);
        let content = selected
            .and_then(|id| state.panes.get(id))
            .cloned()
            .map(|pane| {
                Rc::new(move |_: &mut Window, _: &mut App| {
                    div()
                        .size_full()
                        .capture_any_mouse_down(|event, _, cx| {
                            if event.button == gpui::MouseButton::Right {
                                cx.stop_propagation();
                            }
                        })
                        .capture_any_mouse_up(|event, _, cx| {
                            if event.button == gpui::MouseButton::Right {
                                cx.stop_propagation();
                            }
                        })
                        .child(pane.clone())
                        .into_any_element()
                }) as bottom_panel::ContentSlot
            });
        self.bottom.update(cx, |panel, cx| {
            panel.set_input(input, cx);
            panel.set_content(content, cx);
        });
    }
    fn bottom_repos(&self, cx: &App) -> Vec<bottom_panel::Repo> {
        let state = self.active(cx);
        let owner = self.owner.read(cx);
        if let Some(saved) = owner.iced_workspace(state.workspace.id) {
            let dirs = if saved.repos.is_empty() {
                vec![saved.home_dir().to_owned()]
            } else {
                saved.repos.clone()
            };
            return dirs
                .iter()
                .filter_map(|dir| {
                    let key = saved.repo_key(dir)?;
                    let home = saved.own_home_repo();
                    let repo = owner
                        .iced_catalog()
                        .and_then(|catalog| catalog.repos.iter().find(|repo| repo.key() == key))
                        .or_else(|| (home.key() == key).then_some(&home));
                    let command = repo.and_then(|repo| repo.run_command.clone());
                    let action = command.as_deref().map(|command| {
                        gitterm_core::run::run_tab_action(self.bottom_facts(cx), &key, command)
                    });
                    let run_state = match action {
                        Some(gitterm_core::run::RunTabAction::Focus(_)) => {
                            bottom_panel::RunState::Focus
                        }
                        Some(gitterm_core::run::RunTabAction::Restart(_)) => {
                            bottom_panel::RunState::Restart
                        }
                        _ => bottom_panel::RunState::Run,
                    };
                    let url = self
                        .run_tabs
                        .values()
                        .find(|run| run.repo == key)
                        .and_then(|run| run.detected_url.clone());
                    Some(bottom_panel::Repo {
                        key,
                        name: Path::new(dir)
                            .file_name()
                            .unwrap_or(Path::new(dir).as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                        command,
                        run_state,
                        url,
                    })
                })
                .collect();
        }
        self.data
            .terminal_cwd(state.workspace.id, cx)
            .into_iter()
            .map(|dir| bottom_panel::Repo {
                key: gitterm_core::workspace_config::RepoKey::local(&dir),
                name: state.workspace.name.clone(),
                command: None,
                run_state: bottom_panel::RunState::Run,
                url: None,
            })
            .collect()
    }
    fn open_bottom_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_workspace(cx).is_none() {
            return;
        }
        self.update_regions(
            |r| {
                r.bottom_visible = !r.bottom_opened || !r.bottom_visible;
                r.bottom_opened = true;
            },
            cx,
        );
        if !self.active(cx).store.regions.bottom_visible {
            self.focus_main_terminal(window, cx);
        } else {
            let state = self.active(cx);
            let ids = bottom_pane_ids(state, cx);
            if let Some(id) = ids
                .iter()
                .find(|id| pane_id_visible(state, id, cx))
                .or_else(|| ids.first())
            {
                self.show_pane(id, window, cx);
            }
        }
    }
    fn focus_main_terminal(&self, window: &mut Window, cx: &mut App) {
        let state = self.active(cx);
        let bottom = bottom_pane_ids(state, cx);
        let pane = state
            .active_session
            .map(session_pane_id)
            .filter(|id| !bottom.contains(id))
            .and_then(|id| state.panes.get(&id));
        pane.map(|p| p.focus_handle(cx))
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
    }
    fn preview_unavailable(&self, label: &str, cx: &mut Context<Self>) {
        self.owner.update(cx, |owner, cx| {
            owner.error = Some(format!("{label} is not available in preview"));
            cx.notify();
        });
    }
    fn bottom_outcome(
        &mut self,
        outcome: bottom_panel::Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use bottom_panel::Outcome as O;
        if self.active_workspace(cx).is_none() {
            return;
        }
        match outcome {
            O::Toggle => self.open_bottom_terminal(window, cx),
            O::Resize { height } => self.update_regions(
                |r| {
                    r.bottom_height =
                        bottom_panel::clamp_height(height, f32::from(window.viewport_size().height))
                },
                cx,
            ),
            O::ToggleMirroring => self.preview_unavailable("Terminal log mirroring", cx),
            O::AddTerminal { repo } => {
                let cwd = match repo {
                    None => None,
                    Some(key) if self.bottom_repos(cx).iter().any(|r| r.key == key) => match key {
                        gitterm_core::workspace_config::RepoKey::Local(dir) => Some(dir),
                        _ => {
                            self.preview_unavailable("Remote terminal", cx);
                            return;
                        }
                    },
                    Some(_) => {
                        self.preview_unavailable("Repository terminal", cx);
                        return;
                    }
                };
                let Some(home) = self.data.terminal_cwd(self.active(cx).workspace.id, cx) else {
                    return;
                };
                let cwd = gitterm_core::run::new_bottom_terminal_dir(&home, cwd);
                self.new_terminal_at(DockPlacement::Bottom, Some(cwd), window, cx);
            }
            O::SelectTab { id } | O::CloseTab { id } => {
                let pane_id = session_pane_id(SessionId(id as u64));
                if !bottom_pane_ids(self.active(cx), cx).contains(&pane_id) {
                    return;
                }
                if matches!(outcome, O::SelectTab { .. }) {
                    self.show_pane(&pane_id, window, cx);
                } else {
                    let tabs = &self.bottom.read(cx).input;
                    let closed = tabs.tabs.iter().position(|tab| tab.id == id).unwrap();
                    let selected = tabs.tabs.iter().position(|tab| Some(tab.id) == tabs.active);
                    let after = gitterm_core::run::bottom_selection_after_close(
                        selected,
                        closed,
                        tabs.tabs.len() - 1,
                    );
                    let remaining: Vec<_> = tabs
                        .tabs
                        .iter()
                        .filter(|tab| tab.id != id)
                        .map(|tab| tab.id)
                        .collect();
                    let previous_focus = window.focused(cx);
                    let closed_had_focus = self.active(cx).panes[&pane_id]
                        .focus_handle(cx)
                        .contains_focused(window, cx);
                    let target = self.active(cx).panes[&pane_id].read(cx).session.clone();
                    if let Some(target) = target {
                        if self.active(cx).sessions.len() > 1 {
                            self.close_target(&target, window, cx);
                        } else {
                            // Iced permits the last bottom terminal to close; its main-session guard does not apply here.
                            let state = self
                                .workspaces
                                .iter_mut()
                                .find(|w| w.panes.contains_key(&pane_id))
                                .unwrap();
                            let pane = state.panes.remove(&pane_id).unwrap();
                            pane.update(cx, |p, cx| p.stop_target(&target, cx));
                            self.registry.borrow_mut().remove(&pane_id);
                            if let Some(terminals) = &mut state.store.terminals {
                                terminals.retain(|t| t.id != id as u64);
                            }
                            state.sessions.clear();
                            state.active_session = None;
                            state
                                .dock
                                .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
                            save_layout(state, cx);
                        }
                        self.run_tabs.remove(&pane_id);
                        if let Some(id) = after.and_then(|index| remaining.get(index)) {
                            self.show_pane(&session_pane_id(SessionId(*id as u64)), window, cx);
                            if selected == Some(closed) && closed_had_focus {
                                self.focus_main_terminal(window, cx);
                            } else if let Some(focus) = previous_focus {
                                focus.focus(window, cx);
                            }
                        } else {
                            self.focus_main_terminal(window, cx);
                        }
                    }
                }
            }
            O::OpenUrl { repo } => {
                let tabs = &self.bottom.read(cx).input;
                if let Some(index) = tabs.tabs.iter().position(|tab| {
                    Some(tab.id) == tabs.active && tab.run_repo.as_ref() == Some(&repo)
                }) && let Some(url) =
                    gitterm_core::run::run_tab_url(self.bottom_facts(cx).get(index).copied())
                {
                    cx.open_url(url);
                }
            }
            O::Run { repo } | O::Focus { repo } | O::Restart { repo } => {
                self.run_repo(repo, window, cx)
            }
            O::QuickCommand { id } => self.quick_command(id, window, cx),
        }
        cx.notify();
    }
    fn bottom_facts<'a>(
        &'a self,
        cx: &'a App,
    ) -> Vec<gitterm_core::run::BottomTerminalFact<'a, &'a gitterm_core::workspace_config::RepoKey>>
    {
        let state = self.active(cx);
        let ids = bottom_pane_ids(state, cx);
        state
            .sessions
            .iter()
            .filter_map(|session| {
                let id = session_pane_id(session.id);
                if !ids.contains(&id) {
                    return None;
                }
                let pane = state.panes[&id].read(cx);
                Some(gitterm_core::run::BottomTerminalFact {
                    alive: terminal_alive(pane, cx),
                    run: self
                        .run_tabs
                        .get(&id)
                        .map(|run| gitterm_core::run::RunTabFact {
                            repo: &run.repo,
                            command: &run.command,
                            detected_url: run.detected_url.as_deref(),
                        }),
                })
            })
            .collect()
    }
    fn run_repo(
        &mut self,
        key: gitterm_core::workspace_config::RepoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gitterm_core::run::{self, RunCommandUnavailable as U, RunTabAction};
        let repos = self.bottom_repos(cx);
        let repo = repos.iter().find(|repo| repo.key == key);
        let command = match run::resolve_run_command(
            repo.is_some(),
            repo.map(|repo| run::RunCommandFact {
                is_local: key.is_local(),
                command: repo.command.as_deref(),
            }),
        ) {
            Ok(command) => command.to_owned(),
            Err(reason) => {
                let message = match reason {
                    U::NotListed => "no such repo to run".to_owned(),
                    U::MissingRepo => return,
                    U::RemoteRepo => "only this Mac's repos run here".to_owned(),
                    U::MissingCommand => format!(
                        "{} has no run command",
                        match &key {
                            gitterm_core::workspace_config::RepoKey::Local(dir) =>
                                dir.display().to_string(),
                            _ => repo.map(|r| r.name.clone()).unwrap_or_default(),
                        }
                    ),
                };
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some(message);
                    cx.notify();
                });
                return;
            }
        };
        let action = run::run_tab_action(self.bottom_facts(cx), &key, &command);
        let ids: Vec<_> = self
            .bottom
            .read(cx)
            .input
            .tabs
            .iter()
            .map(|tab| session_pane_id(SessionId(tab.id as u64)))
            .collect();
        if let RunTabAction::Focus(index) = action {
            self.update_regions(
                |regions| {
                    regions.bottom_visible = true;
                    regions.bottom_opened = true;
                },
                cx,
            );
            self.show_pane(&ids[index], window, cx);
            return;
        }
        let gitterm_core::workspace_config::RepoKey::Local(cwd) = &key else {
            return;
        };
        let owner = self.owner.read(cx);
        let env = owner
            .iced_catalog()
            .and_then(|catalog| catalog.repos.iter().find(|repo| repo.key() == key))
            .map(|repo| repo.env.clone())
            .unwrap_or_default();
        let overrides = owner
            .iced_workspace(self.active(cx).workspace.id)
            .map(|ws| ws.env_overrides.clone())
            .unwrap_or_default();
        let launch = gitterm_terminal::TerminalLaunch {
            cwd: Some(cwd.clone()),
            env: gitterm_core::workspace_config::layered_env(&env, &overrides)
                .into_iter()
                .map(|(k, v)| (k, Some(v)))
                .collect(),
            startup_command: Some(command.clone()),
            ..Default::default()
        };
        let id = if let RunTabAction::Restart(index) = action {
            let id = ids[index].clone();
            let pane = self.active(cx).panes[&id].clone();
            self.launch_run_terminal(&pane, launch, window, cx);
            id
        } else {
            self.new_terminal_launch(
                DockPlacement::Bottom,
                Some(cwd.clone()),
                Some(launch),
                window,
                cx,
            );
            let Some(id) = self
                .active(cx)
                .sessions
                .last()
                .map(|s| session_pane_id(s.id))
            else {
                return;
            };
            id
        };
        self.run_tabs.insert(
            id.clone(),
            bottom_panel::RunTab {
                repo: key,
                command: command.clone(),
                detected_url: None,
                next_url_scan: Instant::now(),
            },
        );
        if self.run_url_poll.is_none() {
            self.run_url_poll = Some(cx.spawn(async move |shell, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(2))
                        .await;
                    if shell
                        .update(cx, |shell, cx| {
                            if shell
                                .run_tabs
                                .values()
                                .any(|run| run.detected_url.is_none())
                            {
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        self.active(cx).panes[&id].update(cx, |pane, cx| {
            pane.title = command;
            cx.notify();
        });
        self.update_regions(
            |regions| {
                regions.bottom_opened = true;
                regions.bottom_visible = true;
            },
            cx,
        );
        self.show_pane(&id, window, cx);
    }
    fn launch_run_terminal(
        &mut self,
        pane: &Entity<Pane>,
        launch: gitterm_terminal::TerminalLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        let launch = {
            let mut launch = launch;
            launch.program = "/bin/zsh".into();
            launch.args = vec!["-d".into(), "-l".into()];
            launch.env.insert(
                "HOME".into(),
                Some(self.config_dir.to_string_lossy().into_owned()),
            );
            launch.env.insert(
                "ZDOTDIR".into(),
                Some(self.config_dir.to_string_lossy().into_owned()),
            );
            launch
        };
        let preview = self.owner.read(cx).preview();
        if let Err(error) = preview::executable(
            preview.as_deref(),
            gitterm_core::preview::Effect::Terminal,
            launch.cwd.as_deref().unwrap_or(Path::new("<terminal>")),
        ) {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(error.to_string());
                cx.notify();
            });
            return;
        }
        pane.update(cx, |pane, cx| {
            pane.close_terminal(cx);
            pane.session = Some(sessions::fresh_target());
            pane.exit_pending = false;
        });
        let target = pane.read(cx).session.clone();
        let weak = pane.downgrade();
        let view = cx.new(|cx| {
            gitterm_terminal::TerminalView::launch(launch, Default::default(), cx)
                .with_exit_callback(move |_, cx| {
                    _ = weak.update(cx, |pane, cx| {
                        if pane.session == target {
                            pane.exit_pending = true;
                            cx.notify();
                        }
                    });
                })
        });
        self._subscriptions
            .push(cx.observe_in(&view, window, |_, view, window, cx| {
                let view = view.clone();
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| view.process_events(window, cx));
                });
                cx.notify();
            }));
        pane.update(cx, |pane, cx| {
            pane.content = pane::PaneContent::Terminal(view);
            cx.notify();
        });
    }
    fn quick_command(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        use gitterm_core::run::{self, QuickCommandTarget};
        let panel = &self.bottom.read(cx).input;
        let selected = panel
            .tabs
            .iter()
            .position(|tab| Some(tab.id) == panel.active);
        let home = self.data.terminal_cwd(self.active(cx).workspace.id, cx);
        let launch = run::resolve_quick_command(
            &panel.quick_commands,
            id,
            selected.and_then(|index| self.bottom_facts(cx).get(index).copied()),
            home.as_deref(),
        );
        let Ok(Some(launch)) = launch else {
            return;
        };
        let pane = match launch.target {
            QuickCommandTarget::SelectedPlainTerminal => {
                let Some(id) = panel.active else {
                    return;
                };
                self.active(cx).panes[&session_pane_id(SessionId(id as u64))].clone()
            }
            QuickCommandTarget::Spawn { cwd } => {
                self.new_terminal_at(DockPlacement::Bottom, Some(cwd), window, cx);
                let Some(session) = self.active(cx).sessions.last() else {
                    return;
                };
                self.active(cx).panes[&session_pane_id(session.id)].clone()
            }
        };
        let view = match &pane.read(cx).content {
            pane::PaneContent::Terminal(view) => Some(view.clone()),
            _ => None,
        };
        if let Some(view) = view {
            view.update(cx, |view, _| {
                view.backend_mut()
                    .handle(gitterm_terminal::Command::Write(launch.bytes));
            });
        }
        self.update_regions(
            |regions| {
                regions.bottom_visible = true;
                regions.bottom_opened = true;
            },
            cx,
        );
        pane.focus_handle(cx).focus(window, cx);
    }
    fn git_action(
        &mut self,
        workspace: WorkspaceId,
        action: &pane::git_view::GitAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use pane::git_view::GitAction as A;
        match action {
            A::BackToWork => {
                if let Some(state) = self
                    .workspaces
                    .iter_mut()
                    .find(|w| w.workspace.id == workspace)
                {
                    state.store.regions.sidebar_mode = SidebarMode::Work;
                    save_layout(state, cx);
                }
                self.sync_git_visibility(true, cx);
            }
            A::EditFile(_) => self.editor_preview_notice(cx),
            A::OpenWorktree(_) | A::AdoptWorktree(_) | A::RemoveWorktree(_) => {
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some("Worktree actions are not available in preview".into());
                    cx.notify();
                });
            }
        }
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

impl Shell {
    fn has_dialog(window: &mut Window, cx: &mut App) -> bool {
        use gpui_component::WindowExt;
        window.root::<Root>().flatten().is_some() && window.has_active_dialog(cx)
    }
    fn focused_terminal(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<gitterm_terminal::TerminalView>> {
        self.workspaces
            .iter()
            .flat_map(|state| state.panes.values())
            .find_map(|pane| {
                let pane::PaneContent::Terminal(view) = &pane.read(cx).content else {
                    return None;
                };
                view.read(cx)
                    .focus_handle()
                    .contains_focused(window, cx)
                    .then(|| view.clone())
            })
    }
    /// Iced's empty-content `Open Folder` button: the same folder flow as ⌥⇧T.
    fn open_folder_button(&self, cx: &Context<Self>) -> gpui::AnyElement {
        use gpui_component::button::{Button, ButtonVariants as _};
        div()
            .id("empty-open-folder")
            .debug_selector(|| "empty-open-folder".into())
            .child(
                Button::new("empty-open-folder-button")
                    .primary()
                    .label("Open Folder")
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.bar_outcome(workspace_bar::Outcome::OpenFolder, w, cx)
                    })),
            )
            .into_any_element()
    }
    fn active_main_terminal(&self, cx: &App) -> Option<Entity<gitterm_terminal::TerminalView>> {
        let state = self
            .workspaces
            .iter()
            .find(|s| Some(s.workspace.id) == self.active_workspace(cx))?;
        let id = session_pane_id(state.active_session?);
        if bottom_pane_ids(state, cx).contains(&id) {
            return None;
        }
        match &state.panes.get(&id)?.read(cx).content {
            pane::PaneContent::Terminal(view) => Some(view.clone()),
            _ => None,
        }
    }
    fn focused_chat(&self, window: &Window, cx: &App) -> Option<gpui::EntityId> {
        self.workspaces
            .iter()
            .flat_map(|state| state.panes.values())
            .find_map(|pane| {
                (matches!(
                    pane.read(cx).content,
                    pane::PaneContent::Chat(_) | pane::PaneContent::PreviewChat(_)
                ) && pane.focus_handle(cx).contains_focused(window, cx))
                .then(|| pane.entity_id())
            })
    }
    fn active_chat_pane(&self, cx: &App) -> Option<Entity<Pane>> {
        let state = self
            .workspaces
            .iter()
            .find(|s| Some(s.workspace.id) == self.active_workspace(cx))?;
        let pane = state.panes.get(&session_pane_id(state.active_session?))?;
        matches!(
            pane.read(cx).content,
            pane::PaneContent::Chat(_) | pane::PaneContent::PreviewChat(_)
        )
        .then(|| pane.clone())
    }
    fn active_agent_tab(&self, cx: &App) -> bool {
        let strip = self.strip_input(cx);
        strip
            .sessions
            .iter()
            .any(|s| Some(s.id) == strip.active && s.kind == data::SessionKind::Chat)
    }
    fn menu_context(&self, window: &mut Window, cx: &mut App) -> String {
        use gpui_component::WindowExt;
        let mut context = String::from("GitTermShell");
        if self.usage_open {
            context.push_str(" IcedUsage");
            return context;
        }
        if self.task_switcher.is_some() {
            context.push_str(" IcedTaskSwitcher");
            return context;
        }
        if self.selected_task.is_some() {
            context.push_str(" IcedTaskContext");
        }
        let chat_pane = self.focused_chat(window, cx);
        let chat = chat_pane.is_some();
        let modal = self.plans_open
            || self.session_picker.is_some()
            || self.park_sheet.is_some()
            || self.new_work_sheet.is_some()
            || self.settings_sheet.is_some()
            || Self::has_dialog(window, cx);
        if !modal {
            context.push_str(" IcedMain");
        }
        if Self::has_dialog(window, cx) {
            context.push_str(" IcedHelp");
        }
        if self.active_agent_tab(cx) {
            context.push_str(" IcedAgentTab");
        }
        let editable = window.root::<Root>().flatten().is_some() && window.has_focused_input(cx);
        if chat && editable {
            context.push_str(" IcedEditable");
        }
        if chat && !modal {
            context.push_str(" IcedChat");
            if chat_pane.is_some_and(|pane| self.chat_find.contains(&pane)) {
                context.push_str(" IcedChatFind");
            }
        }
        if !modal
            && (chat
                || (!editable
                    && self.focused_terminal(window, cx).is_none()
                    && self.active_chat_pane(cx).is_some()
                    && self.content.read(cx).selected.is_none()
                    && self
                        .find_state
                        .borrow()
                        .selected(&self.find_owner(cx))
                        .is_none()))
        {
            context.push_str(" IcedChatFindAllowed");
        }
        if self
            .focused_terminal(window, cx)
            .is_some_and(|v| v.read(cx).is_find_open())
        {
            context.push_str(" IcedTerminalSearch");
        }
        if !modal
            && !chat
            && self.focused_terminal(window, cx).is_none()
            && self
                .active_workspace(cx)
                .and_then(|id| self.selected_files.get(&id))
                .is_some_and(|file| file.changed.is_some())
            && (self.content.read(cx).selected.is_some()
                || self.workspaces.iter().any(|s| {
                    Some(s.workspace.id) == self.active_workspace(cx)
                        && s.panes.iter().any(|(id, p)| {
                            matches!(p.read(cx).content, pane::PaneContent::Review(_))
                                && pane_id_visible(s, id, cx)
                        })
                }))
        {
            context.push_str(" IcedDiff");
        }
        context
    }
    fn menu_root(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        use app_menus::*;
        div()
            .track_focus(&self.focus)
            .key_context(self.menu_context(window, cx).as_str())
            .capture_key_down(cx.listener(Self::guard_app_shortcut))
            .on_action(cx.listener(|_, _: &Minimize, w, _| w.minimize_window()))
            .on_action(cx.listener(|_, _: &Maximize, w, _| w.zoom_window()))
            .on_action(cx.listener(|_, _: &Fullscreen, w, _| w.toggle_fullscreen()))
            .on_action(
                cx.listener(|this, a: &SetChatWidth, _, cx| this.set_chat_width(a.width, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenPlansViewer, w, cx| this.toggle_plans(w, cx)))
            .on_action(cx.listener(|this, _: &ChatsOverviewOpen, w, cx| {
                if this.active_workspace(cx).is_none() {
                    return;
                }
                if this.usage_open {
                    this.toggle_usage(w, cx);
                }
                if this.plans_open {
                    this.toggle_plans(w, cx);
                }
                this.find_state.borrow_mut().reduce(
                    gitterm_core::find::state::Intent::ClosePreview(this.find_owner(cx)),
                );
                this.update_regions(
                    |r| {
                        r.sidebar_mode = SidebarMode::Find;
                        r.sidebar_collapsed = false;
                    },
                    cx,
                );
                this.ensure_find_views(w, cx);
                this.focus.focus(w, cx);
            }))
            .on_action(cx.listener(|this, _: &DebugSpawnAgentTab, _, cx| {
                this.preview_unavailable("Debug agent tab", cx)
            }))
            .on_action(cx.listener(|this, _: &LaunchAgentPreset, _, cx| {
                this.preview_unavailable("Launch agent preset", cx)
            }))
            .on_action(cx.listener(|this, _: &TaskChildLaunchPreset, _, cx| {
                this.preview_unavailable("Launch task child preset", cx)
            }))
            .on_action(cx.listener(|this, _: &ParkSubmit, _, cx| {
                if let Some(sheet) = &this.park_sheet {
                    sheet.update(cx, |sheet, cx| sheet.submit(cx));
                }
            }))
            .on_action(cx.listener(|_, _: &CycleMode, w, cx| {
                Self::chat_action("native_chat::CycleMode", w, cx)
            }))
            .on_action(cx.listener(|_, _: &SubmitReview, w, cx| {
                Self::chat_action("native_chat::SubmitReview", w, cx)
            }))
            .on_action(cx.listener(|_, _: &CloseReview, w, cx| {
                // The page closes review before find; preserve the host's
                // chat-find marker until the next Escape closes find itself.
                Self::chat_action("native_chat::CloseFind", w, cx)
            }))
            .on_action(cx.listener(|_, _: &SubmitPrompt, w, cx| {
                Self::chat_action("native_chat::SubmitPrompt", w, cx)
            }))
            .on_action(
                cx.listener(|_, _: &Newline, w, cx| {
                    Self::chat_action("native_chat::Newline", w, cx)
                }),
            )
            .on_action(cx.listener(|_, _: &SlashNext, w, cx| {
                Self::chat_action("native_chat::SlashNext", w, cx)
            }))
            .on_action(cx.listener(|_, _: &SlashPrevious, w, cx| {
                Self::chat_action("native_chat::SlashPrevious", w, cx)
            }))
            .on_action(cx.listener(|_, _: &FillSlash, w, cx| {
                Self::chat_action("native_chat::FillSlash", w, cx)
            }))
            .on_action(cx.listener(|_, _: &SubmitSlash, w, cx| {
                Self::chat_action("native_chat::SubmitSlash", w, cx)
            }))
            .on_action(cx.listener(|_, _: &CloseSlash, w, cx| {
                Self::chat_action("native_chat::CloseSlash", w, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.update_regions(|r| r.sidebar_collapsed = !r.sidebar_collapsed, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ConsoleToggle, w, cx| this.open_bottom_terminal(w, cx)),
            )
            .on_action(
                cx.listener(|this, a: &WorkspaceSelect, w, cx| {
                    this.select_workspace(a.index, w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, a: &TabSelect, w, cx| this.select_session(a.index, w, cx)),
            )
            .on_action(cx.listener(|this, _: &WorkspaceClose, w, cx| this.open_park_sheet(w, cx)))
            .on_action(
                cx.listener(|this, _: &OpenFolder, w, cx| {
                    // Iced's ⌥⇧T / ⌘O: the in-app folder picker, not the new-work sheet.
                    this.bar_outcome(workspace_bar::Outcome::OpenFolder, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleHelp, w, cx| {
                use gpui_component::WindowExt;
                if Self::has_dialog(w, cx) {
                    w.close_dialog(cx);
                } else {
                    dialogs::shortcuts(w, cx);
                }
                this.content
                    .update(cx, |content, cx| content.sync_covers(w, cx));
            }))
            .on_action(
                cx.listener(|this, _: &IncreaseUiFont, _, cx| this.resize_font(true, 1., cx)),
            )
            .on_action(
                cx.listener(|this, _: &DecreaseUiFont, _, cx| this.resize_font(true, -1., cx)),
            )
            .on_action(
                cx.listener(|this, _: &IncreaseChatFont, _, cx| this.resize_chat_font(1., cx)),
            )
            .on_action(
                cx.listener(|this, _: &DecreaseChatFont, _, cx| this.resize_chat_font(-1., cx)),
            )
            .on_action(
                cx.listener(|this, _: &IncreaseTerminalFont, _, cx| {
                    this.resize_content_font(1., cx)
                }),
            )
            .on_action(cx.listener(|this, _: &DecreaseTerminalFont, _, cx| {
                this.resize_content_font(-1., cx)
            }))
            .on_action(cx.listener(|this, _: &UsageViewToggle, w, cx| this.toggle_usage(w, cx)))
            .on_action(cx.listener(|this, _: &RunRepoCommand, w, cx| {
                if let Some(saved) = this
                    .active_workspace(cx)
                    .and_then(|id| this.owner.read(cx).iced_workspace(id))
                    && let Some(repo) = saved.repo_key(saved.home_dir())
                {
                    this.run_repo(repo, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleTheme, _, cx| {
                this.chrome_unavailable("Toggle Light/Dark Theme", cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminalLogMirroring, _, cx| {
                this.chrome_unavailable("Toggle Terminal Log Mirroring", cx);
            }))
            .on_action(cx.listener(|this, action: &PreviewUnavailable, _, cx| {
                this.chrome_unavailable(&action.label, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearTerminal, _, cx| {
                // Iced writes to the active main tab, never the bottom panel.
                if let Some(view) = this.active_main_terminal(cx) {
                    view.update(cx, |v, _| {
                        v.backend_mut()
                            .handle(gitterm_terminal::Command::Write(b"clear\n".to_vec()));
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSearch, w, cx| {
                if let Some(view) = this.focused_terminal(w, cx) {
                    if view.read(cx).is_find_open() {
                        view.update(cx, |v, cx| v.close_find(w, cx));
                    } else {
                        view.update(cx, |v, cx| v.open_find(cx));
                    }
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SearchNext, w, cx| {
                if let Some(view) = this.focused_terminal(w, cx) {
                    view.update(cx, |v, cx| {
                        v.navigate_find(false, cx);
                    });
                }
            }))
            .on_action(
                cx.listener(|this, _: &TaskSwitcherOpen, w, cx| this.open_task_switcher(w, cx)),
            )
            .on_action(cx.listener(|this, _: &TaskSwitcherClose, _, cx| {
                this.switcher_command(task_switcher::Command::Close, cx)
            }))
            .on_action(cx.listener(|this, _: &TaskSwitcherNext, _, cx| {
                this.switcher_command(task_switcher::Command::Next, cx)
            }))
            .on_action(cx.listener(|this, _: &TaskSwitcherPrevious, _, cx| {
                this.switcher_command(task_switcher::Command::Previous, cx)
            }))
            .on_action(cx.listener(|this, _: &TaskSwitcherActivateFirst, _, cx| {
                this.switcher_command(task_switcher::Command::Activate, cx)
            }))
            .on_action(
                cx.listener(|this, _: &TaskContextBack, w, cx| this.general_context(false, w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &TaskOverview, w, cx| this.general_context(true, w, cx)),
            )
            .on_action(cx.listener(|this, _: &AttentionJumpNext, w, cx| this.attention_jump(w, cx)))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(|this, a: &FileSelectByIndex, w, cx| {
                this.select_file(&a.selection, w, cx)
            }))
            .on_action(cx.listener(|this, _: &EditFile, _, cx| {
                this.editor_preview_notice(cx);
            }))
            .on_action(cx.listener(|this, _: &AgentFindOpen, w, cx| {
                let focused = this.focused_chat(w, cx);
                let pane = focused
                    .and_then(|id| {
                        this.workspaces
                            .iter()
                            .flat_map(|s| s.panes.values())
                            .find(|p| p.entity_id() == id)
                            .cloned()
                    })
                    .or_else(|| this.active_chat_pane(cx));
                if let Some(pane) = pane {
                    this.chat_find.insert(pane.entity_id());
                    pane.focus_handle(cx).focus(w, cx);
                    Self::chat_action("native_chat::Find", w, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|_, _: &Copy, w, cx| {
                use gpui_component::WindowExt;
                if w.has_focused_input(cx) {
                    w.dispatch_action(Box::new(gpui_component::input::Copy), cx);
                } else {
                    Self::chat_action("native_chat::CopyPlain", w, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &CopyMarkdown, w, cx| {
                Self::chat_action("native_chat::CopyMarkdown", w, cx)
            }))
            .on_action(cx.listener(|_, _: &FindNext, w, cx| {
                Self::chat_action("native_chat::NextMatch", w, cx)
            }))
            .on_action(cx.listener(|_, _: &FindPrevious, w, cx| {
                Self::chat_action("native_chat::PreviousMatch", w, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseFind, w, cx| {
                if let Some(pane) = this.focused_chat(w, cx) {
                    this.chat_find.remove(&pane);
                }
                Self::chat_action("native_chat::CloseFind", w, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|_, _: &Cut, w, cx| {
                w.dispatch_action(Box::new(gpui_component::input::Cut), cx)
            }))
            .on_action(cx.listener(|_, _: &Paste, w, cx| {
                w.dispatch_action(Box::new(gpui_component::input::Paste), cx)
            }))
            .on_action(cx.listener(|_, _: &SelectAll, w, cx| {
                w.dispatch_action(Box::new(gpui_component::input::SelectAll), cx)
            }))
            .on_action(cx.listener(|_, _: &Undo, w, cx| {
                w.dispatch_action(Box::new(gpui_component::input::Undo), cx)
            }))
            .on_action(cx.listener(|_, _: &Redo, w, cx| {
                w.dispatch_action(Box::new(gpui_component::input::Redo), cx)
            }))
            .on_action(cx.listener(|this, _: &NewTerminal, w, cx| this.new_terminal(w, cx)))
            .on_action(
                cx.listener(|this, _: &ToggleBottomPanel, w, cx| this.open_bottom_terminal(w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NewClaudeChat, w, cx| this.new_claude_chat(None, w, cx)),
            )
    }
    fn chat_action(name: &str, window: &mut Window, cx: &mut App) {
        // The chat action types are private. Use GPUI's registered action contract
        // rather than duplicating the chat's find/copy/input implementation.
        let action = cx
            .build_action(name, None)
            .expect("native chat action is registered");
        window.dispatch_action(action, cx);
    }
    fn request_usage_scan(&mut self, cx: &mut Context<Self>) {
        if self.usage_input.scanning {
            return;
        }
        let Some(home) = self.usage_home.clone().or_else(dirs::home_dir) else {
            self.usage_input.error = Some("Could not resolve home directory for usage scan".into());
            self.sync_usage(cx);
            return;
        };
        let sources = gitterm_core::usage::UsageSources::under(&home);
        let window = self.usage_input.window;
        let usage_config = serde_json::from_value::<gitterm_core::usage::UsageConfig>(
            workspace_config::load_iced_settings().usage,
        )
        .unwrap_or_default();
        let pricing = gitterm_core::usage::Pricing::with_overrides(&usage_config);
        let mut cache = std::mem::take(&mut self.usage_cache);
        self.usage_input.scanning = true;
        self.usage_input.error = None;
        self.sync_usage(cx);
        self.usage_scan_task = Some(cx.spawn(async move |shell, cx| {
            let (report, cache) = cx
                .background_executor()
                .spawn(async move {
                    let report = gitterm_core::usage::scan_usage_local(
                        &sources, window, &pricing, &mut cache,
                    );
                    (report, cache)
                })
                .await;
            let _ = shell.update(cx, |shell, cx| {
                shell.usage_scan_task = None;
                shell.usage_cache = cache;
                shell.usage_input.scanning = false;
                if let Some(next) = usage::follow_up_window(window, shell.usage_input.window) {
                    shell.usage_input.window = next;
                    shell.request_usage_scan(cx);
                } else {
                    shell.usage_input.report = Some(Box::new(report));
                    shell.usage_input.error = None;
                }
                shell.sync_usage(cx);
            });
        }));
    }
    /// Iced's Refresh reads the newest Codex rollout's limits. Iced then
    /// saves them to its plan-limits file; this keeps the reading in memory
    /// only (shortcut: lost on exit, until GPUI owns that file).
    fn request_codex_limits(&mut self, cx: &mut Context<Self>) {
        if self.codex_limits_task.is_some() {
            return;
        }
        let Some(home) = self.usage_home.clone().or_else(dirs::home_dir) else {
            return;
        };
        let root = gitterm_core::usage::UsageSources::under(&home).codex_sessions;
        self.codex_limits_task = Some(cx.spawn(async move |shell, cx| {
            let reading = cx
                .background_executor()
                .spawn(async move { gitterm_core::plan_limits::latest_codex_limits(&root) })
                .await;
            let _ = shell.update(cx, |shell, cx| {
                shell.codex_limits_task = None;
                if let Some(reading) = reading {
                    shell.usage_input.plan_limits.absorb(reading);
                }
                shell.sync_usage(cx);
            });
        }));
    }
    fn sync_usage(&self, cx: &mut Context<Self>) {
        self.usage_page.update(cx, |page, cx| {
            let mut input = self.usage_input.clone();
            input.now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            page.set_input(input);
            cx.notify();
        });
        cx.notify();
    }
    /// Iced: choosing a workspace from the bar or Ctrl+digit closes Usage.
    pub(crate) fn pick_workspace_id(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.usage_open {
            self.toggle_usage(window, cx);
        }
        self.select_workspace_id(id, window, cx);
    }
    fn toggle_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.usage_open {
            self.usage_return_focus = window.focused(cx);
        }
        self.usage_open = !self.usage_open;
        self.content.update(cx, |content, cx| {
            content.picker = self.plans_open || self.task_switcher.is_some() || self.usage_open;
            content.sync_covers(window, cx);
        });
        if self.usage_open {
            self.refresh_usage_limits(cx);
            self.request_usage_scan(cx);
            self.focus.focus(window, cx);
        } else if let Some(focus) = self.usage_return_focus.take() {
            focus.focus(window, cx);
        }
        cx.notify();
    }
    fn refresh_usage_limits(&mut self, cx: &mut Context<Self>) {
        if let Some(source) = self
            .owner
            .read(cx)
            .iced_catalog()
            .map(|catalog| catalog.source.clone())
            .filter(|source| source.is_absolute())
        {
            self.usage_input.plan_limits = gitterm_core::plan_limits::PlanLimitsFile::load(
                &gitterm_core::plan_limits::PlanLimitsFile::path_in(&source),
            );
        }
    }
    fn usage_intent(&mut self, intent: usage::UsageIntent, cx: &mut Context<Self>) {
        use usage::UsageIntent as I;
        match intent {
            I::SelectWindow(window) => {
                if self.usage_input.window != window {
                    self.usage_input.window = window;
                    self.request_usage_scan(cx);
                }
            }
            I::Refresh => {
                self.refresh_usage_limits(cx);
                self.request_codex_limits(cx);
                self.request_usage_scan(cx);
            }
            I::SelectRepo(repo) => self.usage_input.repo = repo,
            I::ChartTokens(tokens) => self.usage_input.chart_tokens = tokens,
            I::ToggleHeadless => self.usage_input.show_headless = !self.usage_input.show_headless,
            I::PinChanged(pin) => {
                if self.usage_input.plan_limits.pin != pin {
                    self.preview_unavailable("Routing pin", cx);
                }
            }
        }
        self.sync_usage(cx);
    }
    fn resize_content_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        if self.active_agent_tab(cx) {
            self.resize_chat_font(delta, cx);
        } else {
            self.resize_font(false, delta, cx);
        }
    }
    fn set_chat_width(&mut self, width: gitterm_core::settings::ChatWidth, cx: &mut Context<Self>) {
        theme::set_chat_width(width, cx);
        for state in &mut self.workspaces {
            state.store.regions.chat_width = Some(width);
            save_layout(state, cx);
            for pane in state.panes.values() {
                pane.update(cx, |pane, cx| {
                    if let pane::PaneContent::Chat(view) = &pane.content {
                        view.update(cx, |view, cx| view.set_width(width, cx));
                    } else if let pane::PaneContent::PreviewChat(view) = &pane.content {
                        view.update(cx, |view, cx| view.set_width(width, cx));
                    }
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
    fn toggle_plans(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plans_open {
            self.plans_open = false;
            if let Some(focus) = self.plans_return_focus.take() {
                focus.focus(window, cx);
            }
        } else if self.active_workspace(cx).is_some() {
            self.plans_return_focus = window.focused(cx);
            self.plans_open = true;
            self.focus.focus(window, cx);
        }
        cx.notify();
    }
    fn plans_viewer(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let p = Palette::current(cx);
        let workspace = self.active(cx).workspace.id;
        let state = self.work_state.borrow();
        let documents = state.documents(workspace.0);
        let mut list = div()
            .id("plans-documents")
            .debug_selector(|| "plans-documents".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        if let Some(result) = documents.and_then(|docs| docs.result.as_ref()) {
            if result.documents.is_empty() {
                list = list.child("No documents found.");
            }
            for (index, document) in result.documents.iter().enumerate() {
                let path = document.newest().path.clone();
                list = list.child(
                    Button::new(("plan", index))
                        .debug_selector(move || format!("plan-{index}"))
                        .label(format!("{} · {}", document.title, document.rel))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_plans(window, cx);
                            this.owner.update(cx, |owner, cx| {
                                owner.open_file(workspace, path.clone(), cx)
                            });
                        })),
                );
            }
            if result.truncated {
                list = list.child("Some documents may be missing: scan limit reached.");
            }
        } else {
            list = list.child("Loading documents…");
        }
        if let Some(error) = documents.and_then(|docs| docs.error.as_ref()) {
            list = list.child(div().text_color(rgb(p.red)).child(error.clone()));
        }
        div()
            .id("plans-viewer")
            .debug_selector(|| "plans-viewer".into())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .p_4()
            .gap_3()
            .bg(rgb(p.base))
            .child(
                div().flex().justify_between().child("Documents").child(
                    Button::new("close-plans")
                        .label("Close")
                        .on_click(cx.listener(|this, _, w, cx| this.toggle_plans(w, cx))),
                ),
            )
            .child(list)
    }
    fn resize_chat_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        let size = (theme::chat_appearance(cx).font_size + delta).clamp(11., 22.);
        theme::set_chat_font_size(size, cx);
        for state in &mut self.workspaces {
            state.store.regions.chat_font_size = Some(size);
            save_layout(state, cx);
            for pane in state.panes.values() {
                pane.update(cx, |pane, cx| {
                    if let pane::PaneContent::Chat(view) = &pane.content {
                        view.update(cx, |view, cx| {
                            view.set_appearance(theme::chat_appearance(cx), cx)
                        });
                    }
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
    fn resize_font(&mut self, ui: bool, delta: f32, cx: &mut Context<Self>) {
        let mut fonts = theme::font_sizes(cx);
        let value = if ui {
            &mut fonts.ui
        } else {
            &mut fonts.terminal
        };
        *value = (*value + delta).clamp(10., 24.);
        theme::set_font_sizes(fonts, cx);
        for state in &mut self.workspaces {
            state.store.regions.fonts = Some(fonts);
            save_layout(state, cx);
            for pane in state.panes.values() {
                if let pane::PaneContent::Terminal(view) = &pane.read(cx).content {
                    theme::sync_terminal_typography(&view.clone(), cx);
                }
            }
        }
        cx.notify();
    }
    fn escape(&mut self, _: &app_menus::Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.task_switcher.is_some() {
            self.task_switcher_outcome(&task_switcher::Outcome::Close, window, cx);
            return;
        }
        if self.usage_open {
            self.toggle_usage(window, cx);
            return;
        }
        if self.session_picker.is_some()
            || self.park_sheet.is_some()
            || self.new_work_sheet.is_some()
            || self.settings_sheet.is_some()
            || self.phone_panel().is_some()
            || Self::has_dialog(window, cx)
        {
            cx.propagate();
            return;
        }
        if self.bar_menu.take().is_some() {
            cx.notify();
            return;
        }
        if self
            .bottom
            .update(cx, |bottom, cx| bottom.dismiss_quick_picker(cx))
        {
            return;
        }
        if self.plans_open {
            self.toggle_plans(window, cx);
            return;
        }
        if self
            .focused_terminal(window, cx)
            .is_some_and(|v| v.read(cx).is_find_open())
        {
            // Terminal search owns its Escape handler.
            cx.propagate();
        } else if self
            .focused_chat(window, cx)
            .is_some_and(|pane| self.chat_find.remove(&pane))
        {
            Self::chat_action("native_chat::CloseFind", window, cx);
            cx.notify();
        } else if let Some(workspace) = self.active_workspace(cx).filter(|id| {
            self.selected_files.contains_key(id) || self.content.read(cx).selected.is_some()
        }) {
            self.selected_files.remove(&workspace);
            self.close_file_viewer(workspace, window, cx);
            if let Some(session) = self
                .workspaces
                .iter()
                .find(|s| s.workspace.id == workspace)
                .and_then(|s| s.active_session)
            {
                self.show_workspace_pane(workspace, &session_pane_id(session), window, cx);
            }
            if self.active(cx).store.regions.sidebar_mode == SidebarMode::Find
                && self
                    .find_state
                    .borrow()
                    .selected(&self.find_owner(cx))
                    .is_some()
            {
                self.focus.focus(window, cx);
            }
            cx.notify();
        } else if self.active_workspace(cx).is_some()
            && self.active(cx).store.regions.sidebar_mode == SidebarMode::Find
            && self
                .find_state
                .borrow()
                .selected(&self.find_owner(cx))
                .is_some()
        {
            let owner = self.find_owner(cx);
            self.find_outcome(&owner, &find_sidebar::FindOutcome::ClosePreview, window, cx);
        } else {
            self.focus.focus(window, cx);
        }
    }
    fn guard_app_shortcut(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.task_switcher.is_some() {
            return;
        }
        // Page-local review, composer pickers and find consume Escape before
        // the host's quick commands, viewers and navigation.
        if event.keystroke.key == "escape"
            && !self.plans_open
            && !self.usage_open
            && self.session_picker.is_none()
            && self.park_sheet.is_none()
            && self.new_work_sheet.is_none()
            && self.settings_sheet.is_none()
            && !Self::has_dialog(window, cx)
            && self.bar_menu.is_none()
            && self.focused_chat(window, cx).is_some()
        {
            return;
        }
        if event.keystroke.key == "escape"
            && !self.usage_open
            && self.session_picker.is_none()
            && self.park_sheet.is_none()
            && self.new_work_sheet.is_none()
            && self.settings_sheet.is_none()
            && !Self::has_dialog(window, cx)
            && self
                .bottom
                .update(cx, |bottom, cx| bottom.dismiss_quick_picker(cx))
        {
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape"
            && (self.session_picker.is_some()
                || self.park_sheet.is_some()
                || self.new_work_sheet.is_some()
            || self.settings_sheet.is_some()
                || self.phone_panel().is_some()
                || Self::has_dialog(window, cx)
                || self
                    .focused_terminal(window, cx)
                    .is_some_and(|v| v.read(cx).is_find_open()))
        {
            return;
        }
        let stroke = &event.keystroke;
        let reserved = cx.key_bindings().borrow().bindings().any(|binding| {
            binding.action().name().starts_with("gitterm_iced::")
                && binding.keystrokes().first().is_some_and(|key| {
                    key.key() == stroke.key && *key.modifiers() == stroke.modifiers
                })
                && (stroke.modifiers.platform
                    || stroke.modifiers.control
                    || stroke.modifiers.alt
                    || stroke.key == "escape")
        });
        if reserved {
            // Unimplemented actions stay unavailable for native menu validation,
            // but their reserved chords must never fall through to a PTY.
            cx.stop_propagation();
        }
    }
    fn select_file(
        &mut self,
        selection: &app_menus::FileSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use app_menus::FileSelection;
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let files = self.data.status(workspace, cx).files;
        let current = self.selected_files.get(&workspace).and_then(|selected| {
            files.iter().position(|file| {
                selected
                    .changed
                    .as_ref()
                    .is_some_and(|s| s.path == file.path && s.staged == file.staged)
            })
        });
        let index = match selection {
            FileSelection::Next => current.and_then(|i| i.checked_add(1)),
            FileSelection::Previous => current.and_then(|i| i.checked_sub(1)),
            FileSelection::First => Some(0),
            FileSelection::Last => files.len().checked_sub(1),
        };
        if let Some(file) = index.and_then(|i| files.get(i)).cloned() {
            self.owner
                .update(cx, |owner, cx| owner.open_changed_file(workspace, file, cx));
        }
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active_workspace(cx).is_some() {
            let state = self.active(cx);
            window.set_window_title(&window_title(
                "GitTerm V6 Development",
                &state.workspace.name,
                state.store.regions.sidebar_mode,
            ));
        }
        for state in &self.workspaces {
            for pane in state.panes.values() {
                if let pane::PaneContent::Chat(view) = &pane.read(cx).content {
                    view.clone()
                        .update(cx, |view, cx| view.set_width(theme::chat_width(cx), cx));
                }
                let terminal = match &pane.read(cx).content {
                    pane::PaneContent::Terminal(view) => Some(view.clone()),
                    _ => None,
                };
                if let Some(view) = terminal {
                    theme::sync_terminal_typography(&view, cx);
                }
            }
        }
        self.sync_content_selection(cx);
        if let Some(switcher) = &self.task_switcher {
            let inputs = self.switcher_inputs(cx);
            switcher.update(cx, |s, cx| s.set_inputs(inputs, cx));
        }
        self.content.update(cx, |content, cx| {
            content.picker = self.plans_open
                || self.task_switcher.is_some()
                || self.usage_open
                || self.session_picker.is_some()
                || self.park_sheet.is_some()
                || self.new_work_sheet.is_some()
            || self.settings_sheet.is_some()
                || self.phone_panel().is_some();
            content.sync_covers(window, cx);
        });
        self.ensure_work_sidebars(window, cx);
        self.sync_review_selection(cx);
        self.ensure_rosters(window, cx);
        for (id, sidebar) in &self.work_sidebars {
            if !self.run_sidebar_subscriptions.contains_key(id) {
                let subscription = cx.subscribe_in(
                    sidebar,
                    window,
                    |shell, _, outcome: &work_sidebar::WorkSidebarOutcome, window, cx| {
                        if let work_sidebar::WorkSidebarOutcome::OpenTask { task } = outcome {
                            shell.activate_task(task, window, cx);
                        }
                        // The repo row's ▶ is an explicit Run, as in Iced (RunRepoCommand).
                        if let work_sidebar::WorkSidebarOutcome::RunRepoCommand { repo } = outcome {
                            shell.run_repo(
                                gitterm_core::workspace_config::RepoKey::local(repo),
                                window,
                                cx,
                            );
                        }
                    },
                );
                self.run_sidebar_subscriptions.insert(*id, subscription);
            }
        }
        self.ensure_find_views(window, cx);
        self.refresh_bottom_panel(window, cx);
        let p = Palette::current(cx);
        if !self.workspaces.iter().any(|state| state.on_bar()) {
            return self
                .menu_root(window, cx)
                .relative()
                .size_full()
                .flex()
                .font_family(gpui_component::Theme::global(cx).font_family.clone())
                .text_size(gpui_component::Theme::global(cx).font_size)
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
                        .child(self.session_strip(cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .p_4()
                                .when(self.usage_open, |area| area.child(self.usage_page.clone()))
                                .when(!self.usage_open, |area| {
                                    area.child("No local workspaces yet")
                                        .child(self.open_folder_button(cx))
                                })
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
                .children(self.new_work_sheet.clone())
                .children(self.remote_overlays())
                .into_any_element();
        }
        let state = self.active(cx);
        // Work loads on render. Bootstrap its read-only facts even when a
        // restored workspace has Files/Git selected or its sidebar collapsed.
        let background_work: Vec<_> = self
            .work_sidebars
            .iter()
            .filter(|(id, sidebar)| {
                self.owner.read(cx).iced_catalog().is_some()
                    && !sidebar.read(cx).has_loaded()
                    && (**id != state.workspace.id
                        || state.store.regions.sidebar_collapsed
                        || state.store.regions.sidebar_mode != SidebarMode::Work)
            })
            .map(|(_, sidebar)| sidebar.clone())
            .collect();
        let sidebar = self.sidebar(cx);
        let mut content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.session_strip(cx));
        if let Some(error) = &state.error {
            content = content.child(div().p_2().text_color(rgb(p.red)).child(error.clone()));
        }
        self.menu_root(window, cx)
            .relative()
            .size_full()
            .flex()
            .font_family(gpui_component::Theme::global(cx).font_family.clone())
            .text_size(gpui_component::Theme::global(cx).font_size)
            .text_color(rgb(p.text))
            .bg(rgb(p.base))
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    if let Some((bottom, start, original)) = this.resize {
                        if bottom {
                            let active = this.active_workspace(cx);
                            if let Some(state) = this
                                .workspaces
                                .iter_mut()
                                .find(|w| Some(w.workspace.id) == active)
                            {
                                state.store.regions.bottom_height =
                                    (original + start - f32::from(event.position.y)).clamp(
                                        32.,
                                        (f32::from(window.viewport_size().height) - 140.).max(32.),
                                    );
                            }
                        } else {
                            this.sidebar_width = layout::clamp_sidebar(
                                original + f32::from(event.position.x) - start,
                            );
                            for workspace in &mut this.workspaces {
                                workspace.store.regions.resize_sidebar(this.sidebar_width);
                            }
                        }
                        cx.notify();
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
                            .child(if self.usage_open {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .child(self.usage_page.clone())
                                    .into_any_element()
                            } else {
                                self.find_content(cx).into_any_element()
                            }),
                    )
                    .child(self.bottom.clone())
                    .children(self.owner.read(cx).error.clone().map(|error| {
                        div()
                            .id("preview-notice")
                            .debug_selector(|| "preview-notice".into())
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(12.))
                            .text_color(rgb(p.red))
                            .child(div().flex_1().child(error))
                            .child(
                                Button::new("dismiss-preview-notice")
                                    .ghost()
                                    .label("×")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.owner.update(cx, |owner, cx| {
                                            owner.error = None;
                                            cx.notify();
                                        });
                                    })),
                            )
                    }))
                    .child(self.workspace_bar(cx)),
            )
            .child(
                div()
                    .absolute()
                    .size(px(0.))
                    .invisible()
                    .overflow_hidden()
                    .children(background_work),
            )
            .children(self.session_picker.clone())
            .children(self.park_sheet.clone())
            .children(self.new_work_sheet.clone())
            .children(self.settings_sheet.clone())
            .children(self.task_switcher.clone())
            .when(self.plans_open, |view| view.child(self.plans_viewer(cx)))
            .children(self.remote_overlays())
            .child(content::visual::reconciler(self.content.clone()))
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

/// Bind before menus: native accelerators are derived from the installed keymap.
fn install_app_menus(cx: &mut App) {
    cx.bind_keys(app_menus::key_bindings());
    cx.set_menus(app_menus::menus());
    cx.on_action(|_: &app_menus::Hide, cx| cx.hide());
    cx.on_action(|_: &app_menus::HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &app_menus::ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &app_menus::Quit, cx| cx.quit());
    cx.on_action(|_: &app_menus::About, _| {
        #[cfg(not(test))]
        {
            let mtm = objc2::MainThreadMarker::new().expect("GPUI runs on the main thread");
            objc2_app_kit::NSApplication::sharedApplication(mtm).orderFrontStandardAboutPanel(None);
        }
    });
}

pub fn run(folders: Vec<std::path::PathBuf>, sample: bool) -> Result<()> {
    let settings = workspace_config::load_iced_settings();
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
    let title_workspace = model
        .active()
        .and_then(|active| {
            model
                .workspaces
                .iter()
                .find(|workspace| workspace.id == active)
        })
        .map(|workspace| workspace.status.repo_name.clone())
        .unwrap_or_else(|| {
            if sample {
                "Sample data".into()
            } else {
                "GitTerm".into()
            }
        });
    let window_title = window_title(
        "GitTerm V6 Development",
        &title_workspace,
        SidebarMode::Work,
    );
    gpui_kit::application().run(move |cx| {
        preview.release_on_quit(cx);
        gpui_kit::init(cx);
        theme::apply_settings(&settings, None, cx);
        install_app_menus(cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(layout::WINDOW_WIDTH), px(layout::WINDOW_HEIGHT)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some(window_title.clone().into()),
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
                        Shell::with_owner_settings(
                            data, owner, &config, false, &settings, window, cx,
                        )
                        .expect("create sample shell")
                    } else {
                        content::visual::install(cx);
                        // Read-only: the Agent pane lists the agent's config.
                        if let Some(home) = dirs::home_dir() {
                            cx.set_global(agent_sidebar::AgentHome(home.join(".claude")));
                        }
                        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
                        if let Some(save) = startup_save {
                            owner.update(cx, |owner, cx| owner.watch_save(save, cx));
                        }
                        let data = Rc::new(data::LocalData(owner.clone()));
                        let shell = Shell::with_owner_settings(
                            data, owner, &config, true, &settings, window, cx,
                        )
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

fn window_title(app_name: &str, workspace: &str, sidebar_mode: SidebarMode) -> String {
    let mode = match sidebar_mode {
        SidebarMode::Work => "Work",
        SidebarMode::Files => "Files",
        SidebarMode::Find => "Find",
        SidebarMode::Git => "Git",
        SidebarMode::Agent => "Agent",
    };
    format!("{app_name} - {workspace} - {mode}")
}

impl Shell {
    pub(crate) fn open_recent_work_sheet(
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
}
impl Shell {
    fn session_tab<'a>(
        &self,
        id: SessionId,
        cx: &'a App,
    ) -> Option<&'a gitterm_core::workspace_config::WorkspaceTabConfig> {
        let state = self
            .workspaces
            .iter()
            .find(|w| w.sessions.iter().any(|s| s.id == id))?;
        let saved = self.owner.read(cx).iced_workspace(state.workspace.id)?;
        let index = data::iced_sessions(saved).iter().position(|s| s.id == id)?;
        saved.tabs.get(index)
    }

    fn session_task(&self, id: SessionId, cx: &App) -> Option<String> {
        self.session_tab(id, cx)
            .and_then(|t| t.task_id.clone())
            .or_else(|| {
                self.workspaces
                    .iter()
                    .find_map(|w| w.panes.get(&session_pane_id(id)))
                    .and_then(|p| p.read(cx).session.as_ref()?.task_id.clone())
            })
    }

    fn task_records(&self, cx: &App) -> Vec<gitterm_core::tasks::TaskRecord> {
        let mut records = vec![];
        for w in &self.workspaces {
            if let Some(sidebar) = self.work_sidebars.get(&w.workspace.id) {
                for task in sidebar.read(cx).tasks() {
                    if !records
                        .iter()
                        .any(|t: &gitterm_core::tasks::TaskRecord| t.task_id == task.task_id)
                    {
                        records.push(task.clone());
                    }
                }
            }
        }
        records
    }

    fn remember_context(&mut self, task: Option<String>) {
        self.context_mru.retain(|id| id != &task);
        self.context_mru.insert(0, task);
    }

    fn switcher_inputs(&self, cx: &App) -> task_switcher::Inputs {
        let active = self.active_workspace(cx);
        let tasks = active
            .and_then(|id| self.work_sidebars.get(&id))
            .map(|s| s.read(cx).tasks())
            .unwrap_or_default();
        task_switcher::Inputs {
            tasks: tasks.iter().filter_map(|task| {
                let open: Vec<_> = self.workspaces.iter().flat_map(|w| &w.sessions)
                    .filter_map(|s| self.session_tab(s.id, cx))
                    .filter(|t| t.task_id.as_deref() == Some(&task.task_id))
                    .filter_map(|t| t.task_session_id.as_deref()).collect();
                let streaming = self.workspaces.iter().flat_map(|w| &w.sessions)
                    .filter(|s| self.session_task(s.id, cx).as_deref() == Some(&task.task_id))
                    .any(|s| self.workspaces.iter().find_map(|w| w.panes.get(&session_pane_id(s.id))).is_some_and(|p| {
                        matches!(&p.read(cx).content, pane::PaneContent::PreviewChat(v) if v.read(cx).is_working())
                    }));
                task_switcher::TaskRow::from_task(task, task_switcher::state_copy(task, &open, streaming))
            }).collect(),
            general_session_count: self.workspaces.iter().filter(|w| Some(w.workspace.id) == active).flat_map(|w| &w.sessions)
                .filter(|s| self.session_task(s.id, cx).is_none()).count(),
            current_task_id: self.selected_task.clone(),
            context_mru: self.context_mru.clone(),
        }
    }

    fn open_task_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.is_empty() || self.task_switcher.is_some() {
            return;
        }
        self.task_switcher_return_focus = window.focused(cx);
        let inputs = self.switcher_inputs(cx);
        let switcher = cx.new(|cx| task_switcher::TaskSwitcher::new(inputs.clone(), window, cx));
        self.task_switcher_subscription = Some(cx.subscribe_in(
            &switcher,
            window,
            move |shell, _, outcome: &task_switcher::Outcome, window, cx| {
                shell.task_switcher_outcome(outcome, window, cx);
            },
        ));
        switcher.update(cx, |s, cx| s.open(inputs, window, cx));
        self.task_switcher = Some(switcher);
        self.cover_sheet(true, window, cx);
        cx.notify();
    }

    fn task_switcher_outcome(
        &mut self,
        outcome: &task_switcher::Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.task_switcher = None;
        self.task_switcher_subscription = None;
        self.cover_sheet(false, window, cx);
        let previous_focus = self.task_switcher_return_focus.take();
        match outcome {
            task_switcher::Outcome::Activate { task_id } => self.activate_task(task_id, window, cx),
            task_switcher::Outcome::ContextBack => self.general_context(false, window, cx),
            task_switcher::Outcome::Close => previous_focus
                .unwrap_or_else(|| self.focus.clone())
                .focus(window, cx),
        }
        cx.notify();
    }

    fn switcher_command(&mut self, command: task_switcher::Command, cx: &mut Context<Self>) {
        if let Some(switcher) = &self.task_switcher {
            switcher.update(cx, |s, cx| s.command(command, cx));
        }
    }

    fn select_host_session(
        &mut self,
        workspace: WorkspaceId,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mru = self.context_mru.clone();
        if self.active_workspace(cx) != Some(workspace) {
            self.select_workspace_id(workspace, window, cx);
        }
        self.context_mru = mru;
        self.selected_task = self.session_task(id, cx);
        if let Some(index) = self
            .strip_input(cx)
            .sessions
            .iter()
            .position(|s| s.id == id)
        {
            self.select_session(index, window, cx);
        }
    }

    fn activate_task(&mut self, task: &str, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self
            .workspaces
            .iter()
            .find(|w| {
                w.on_bar()
                    && self
                        .work_sidebars
                        .get(&w.workspace.id)
                        .is_some_and(|s| s.read(cx).tasks().iter().any(|t| t.task_id == task))
            })
            .map(|w| w.workspace.id);
        let Some(workspace) = workspace else {
            return;
        };
        let mru = self.context_mru.clone();
        if self.active_workspace(cx) != Some(workspace) {
            self.select_workspace_id(workspace, window, cx);
        }
        let state = self.active(cx);
        let child = self
            .task_last_session
            .get(task)
            .copied()
            .filter(|id| {
                state.sessions.iter().any(|s| s.id == *id)
                    && self.session_task(*id, cx).as_deref() == Some(task)
            })
            .or_else(|| {
                state
                    .sessions
                    .iter()
                    .rev()
                    .find(|s| self.session_task(s.id, cx).as_deref() == Some(task))
                    .map(|s| s.id)
            });
        if let Some(id) = child {
            self.select_host_session(workspace, id, window, cx);
        } else {
            self.general_context(false, window, cx);
            self.selected_task = Some(task.into());
            self.remember_context(self.selected_task.clone());
        }
        self.context_mru = mru;
        self.remember_context(Some(task.into()));
        self.selected_files.remove(&workspace);
        self.close_file_viewer(workspace, window, cx);
        cx.notify();
    }

    fn general_context(&mut self, overview: bool, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.selected_task.clone();
        if overview && task.is_none() {
            return;
        }
        let mru = self.context_mru.clone();
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let general = self
            .active(cx)
            .sessions
            .iter()
            .find(|s| self.session_task(s.id, cx).is_none())
            .map(|s| s.id);
        self.selected_task = None;
        if let Some(id) = general {
            self.select_host_session(workspace, id, window, cx);
        } else if let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == workspace)
        {
            state.active_session = None;
        }
        // A read-only preview never creates Iced's missing General host terminal.
        if overview {
            self.selected_task = task;
            self.context_mru = mru;
        }
        self.remember_context(self.selected_task.clone());
        self.selected_files.remove(&workspace);
        self.close_file_viewer(workspace, window, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn attention_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gitterm_core::tasks::TaskLifecycle;
        let Some(active) = self
            .workspaces
            .iter()
            .position(|w| Some(w.workspace.id) == self.active_workspace(cx))
        else {
            return;
        };
        let tab = self.workspaces[active]
            .sessions
            .iter()
            .position(|s| Some(s.id) == self.workspaces[active].active_session)
            .unwrap_or(0);
        let tasks = self.task_records(cx);
        let needs = |task: &gitterm_core::tasks::TaskRecord| {
            task.attention.reason.is_some() || task.attention.unread
        };
        let found = gitterm_core::workspace::next_attention(
            &self.visible_workspace_indices(cx),
            active,
            tab,
            |w| self.workspaces[w].sessions.len(),
            |w, t| {
                self.session_task(self.workspaces[w].sessions[t].id, cx)
                    .is_some_and(|id| tasks.iter().any(|task| task.task_id == id && needs(task)))
            },
        );
        if self.usage_open {
            self.toggle_usage(window, cx);
        }
        if let Some((w, t)) = found {
            self.select_host_session(
                self.workspaces[w].workspace.id,
                self.workspaces[w].sessions[t].id,
                window,
                cx,
            );
            return;
        }
        let mut candidates: Vec<_> = tasks
            .iter()
            .filter(|task| task.lifecycle != TaskLifecycle::Archived && needs(task))
            .filter(|task| {
                !self
                    .workspaces
                    .iter()
                    .flat_map(|w| &w.sessions)
                    .any(|s| self.session_task(s.id, cx).as_deref() == Some(&task.task_id))
            })
            .map(|t| &t.task_id)
            .collect();
        if let Some(inputs) = self
            .work_state
            .borrow()
            .inputs(self.workspaces[active].workspace.id.0)
        {
            candidates.sort_by_key(|id| {
                inputs
                    .tasks
                    .iter()
                    .position(|t| &t.task_id == *id)
                    .unwrap_or(usize::MAX)
            });
        }
        let next = self
            .selected_task
            .as_ref()
            .and_then(|id| candidates.iter().position(|t| *t == id))
            .map(|i| (i + 1) % candidates.len())
            .unwrap_or(0);
        if let Some(task) = candidates.get(next) {
            self.activate_task(task, window, cx);
        }
    }

    fn recent_repo_candidates(&self, cx: &App) -> Vec<std::path::PathBuf> {
        use gitterm_core::remote::config::WorkspaceLocationConfig;
        // Every local Work load reads the same machine-wide chat index. Keep
        // its exact order, including equal-time ties, rather than re-sorting.
        let recent = self
            .workspaces
            .iter()
            .filter_map(|w| self.work_sidebars.get(&w.workspace.id))
            .map(|s| s.read(cx).repo_recency())
            .find(|rows| !rows.is_empty())
            .unwrap_or_default();
        let saved = self
            .owner
            .read(cx)
            .iced_catalog()
            .map(|c| c.workspaces.as_slice())
            .unwrap_or_default();
        let local = |w: &gitterm_core::workspace_config::WorkspaceConfig| {
            !w.is_legacy_remote()
                && !matches!(
                    w.location,
                    Some(WorkspaceLocationConfig::RemoteAgent { .. })
                )
        };
        let tasks = self.task_records(cx);
        recent
            .iter()
            .map(|(p, _)| p.clone())
            .chain(
                saved
                    .iter()
                    .filter(|w| local(w))
                    .map(|w| std::path::PathBuf::from(&w.dir)),
            )
            .chain(
                saved
                    .iter()
                    .filter(|w| local(w))
                    .flat_map(|w| &w.tabs)
                    .filter(|t| t.task_id.is_none())
                    .map(|t| std::path::PathBuf::from(t.repo_dir.as_deref().unwrap_or(&t.dir))),
            )
            .filter(|dir| {
                !tasks
                    .iter()
                    .filter_map(|t| t.worktree.path.as_ref())
                    .any(|tree| dir.starts_with(tree))
            })
            .collect()
    }

    fn worker_tab(
        &self,
        worker: &gitterm_core::tasks::Delegation,
        cx: &App,
    ) -> gitterm_core::delegations::WorkerTab {
        use gitterm_core::{
            delegations::WorkerTab,
            tasks::{DelegationChild, TaskLifecycle},
        };
        let DelegationChild::TaskSession {
            task_id,
            task_session_id,
            ..
        } = &worker.child
        else {
            return WorkerTab::Closed;
        };
        if let Some(facts) = &self.roster_facts {
            return facts.worker_tab(task_id, task_session_id.as_deref());
        }
        let open = self.workspaces.iter().flat_map(|w| &w.sessions).find(|s| {
            self.session_tab(s.id, cx).is_some_and(|t| {
                t.task_id.as_ref() == Some(task_id)
                    && task_session_id
                        .as_ref()
                        .is_none_or(|id| t.task_session_id.as_ref() == Some(id))
            })
        });
        let Some(open) = open else {
            return WorkerTab::Closed;
        };
        let streaming = self.workspaces.iter().find_map(|w| w.panes.get(&session_pane_id(open.id))).is_some_and(|p| {
            matches!(&p.read(cx).content, pane::PaneContent::PreviewChat(v) if v.read(cx).is_working())
        });
        if streaming {
            WorkerTab::Working
        } else if self
            .task_records(cx)
            .iter()
            .any(|t| &t.task_id == task_id && t.lifecycle == TaskLifecycle::WaitingForInput)
        {
            WorkerTab::AwaitingInput
        } else {
            WorkerTab::Idle
        }
    }

    fn roster_for_view(
        &self,
        workspace: WorkspaceId,
        view: &Entity<chat::ChatPane>,
        cx: &App,
    ) -> chat::RosterProjection {
        let Some(facts) = &self.roster_facts else {
            // An imported chat waits for the first authoritative store read.
            if self.owner.read(cx).iced_catalog().is_some() {
                return chat::RosterProjection::default();
            }
            // Sample/test hosts have no Iced catalog.
            return self
                .work_sidebars
                .get(&workspace)
                .map(|sidebar| {
                    let sidebar = sidebar.read(cx);
                    chat::RosterProjection::new(sidebar.tasks(), sidebar.workers(), |worker| {
                        self.worker_tab(worker, cx)
                    })
                })
                .unwrap_or_default();
        };
        let chats: Vec<_> = self
            .workspaces
            .iter()
            .filter(|w| {
                self.owner
                    .read(cx)
                    .iced_workspace(w.workspace.id)
                    .is_some_and(|saved| {
                        !saved.is_legacy_remote()
                            && !matches!(saved.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. }))
                    })
            })
            .flat_map(|w| w.panes.iter().map(move |(key, pane)| (w, key, pane)))
            .filter_map(|(workspace, key, pane)| match &pane.read(cx).content {
                pane::PaneContent::PreviewChat(view) => {
                    let tab = workspace
                        .sessions
                        .iter()
                        .find(|session| session_pane_id(session.id) == *key)
                        .and_then(|session| self.session_tab(session.id, cx));
                    Some((view, tab))
                }
                _ => None,
            })
            .collect();
        let workers: Vec<_> = facts
            .workers
            .iter()
            .filter(|worker| {
                // An original Iced chat still owns its results even when its
                // history pane has not been mounted in the preview.
                if facts.tabs.iter().any(|tab| {
                    tab.tab_kind.as_deref() == Some("agent")
                        && tab.session_uid.as_deref() == Some(worker.parent.session_uid.as_str())
                }) {
                    return chats
                        .iter()
                        .find(|(v, _)| v.entity_id() == view.entity_id())
                        .is_some_and(|(_, tab)| {
                            tab.is_some_and(|tab| {
                                tab.session_uid.as_deref()
                                    == Some(worker.parent.session_uid.as_str())
                            })
                        });
                }
                roster_chat_owner(
                    worker,
                    chats.iter().map(|(v, _)| v.entity_id()),
                    |id, identity| {
                        chats
                            .iter()
                            .find(|(v, _)| v.entity_id() == id)
                            .is_some_and(|(v, tab)| {
                                tab.is_some_and(|tab| tab.session_uid.as_deref() == Some(identity))
                                    || v.read(cx).matches_chat(identity)
                            })
                    },
                ) == Some(view.entity_id())
            })
            .cloned()
            .collect();
        chat::RosterProjection::new(&facts.tasks, &workers, |worker| self.worker_tab(worker, cx))
    }

    fn sync_rosters(&self, cx: &mut Context<Self>) {
        for w in &self.workspaces {
            for pane in w.panes.values() {
                if let pane::PaneContent::PreviewChat(view) = &pane.read(cx).content {
                    let rows = self.roster_for_view(w.workspace.id, view, cx);
                    view.clone()
                        .update(cx, |view, cx| view.set_roster(rows, cx));
                }
            }
        }
    }

    fn ensure_rosters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (id, sidebar) in &self.work_sidebars {
            if !self.roster_observers.contains_key(id) {
                let observer = cx.observe(sidebar, |shell, _, cx| {
                    shell.sync_rosters(cx);
                    if let Some(switcher) = &shell.task_switcher {
                        let inputs = shell.switcher_inputs(cx);
                        switcher.update(cx, |s, cx| s.set_inputs(inputs, cx));
                    }
                    cx.notify();
                });
                self.roster_observers.insert(*id, observer);
            }
        }
        for w in &self.workspaces {
            for pane in w.panes.values() {
                let pane::PaneContent::PreviewChat(view) = &pane.read(cx).content else {
                    continue;
                };
                if self.roster_subscriptions.contains_key(&view.entity_id()) {
                    continue;
                }
                let view = view.clone();
                let subscription = cx.subscribe_in(
                    &view,
                    window,
                    |shell,
                     view,
                     outcome: &gitterm_gpui_chat::model::roster::RosterOutcome,
                     window,
                     cx| {
                        match chat::navigation(outcome) {
                            Some(chat::Navigation::Worker(id)) => {
                                if let Some(message) = shell.open_worker_tab(id, window, cx) {
                                    view.update(cx, |pane, cx| pane.show_notice(message, cx));
                                }
                            }
                            Some(chat::Navigation::File(path, review)) => {
                                shell.open_chat_file(view, path, review, window, cx)
                            }
                            None => {}
                        }
                    },
                );
                let activity = cx.observe(&view, |shell, _, cx| shell.sync_rosters(cx));
                self.roster_subscriptions
                    .insert(view.entity_id(), (subscription, activity));
                let rows = self.roster_for_view(w.workspace.id, &view, cx);
                view.update(cx, |view, cx| view.set_roster(rows, cx));
            }
        }
    }

    #[cfg(test)]
    fn open_roster(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.open_worker_tab(id, window, cx);
    }

    fn open_worker_tab(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let worker = self
            .roster_facts
            .iter()
            .flat_map(|facts| facts.workers.iter())
            .chain(
                self.work_sidebars
                    .values()
                    .flat_map(|s| s.read(cx).workers()),
            )
            .find(|w| w.delegation_id == id.trim_start_matches("dlg:"))
            .cloned();
        let Some(worker) = worker else {
            return Some(format!("delegation {id} has no worker tab"));
        };
        let target = chat::worker_tab_target(&worker, |session| {
            self.workspaces
                .iter()
                .filter(|workspace| workspace.on_bar())
                .find_map(|workspace| {
                    workspace
                        .sessions
                        .iter()
                        .find(|tab| {
                            self.session_tab(tab.id, cx)
                                .is_some_and(|tab| tab.task_session_id.as_deref() == Some(session))
                        })
                        .map(|tab| (workspace.workspace.id, tab.id))
                })
        });
        match target {
            Ok((workspace, session)) => self.select_host_session(workspace, session, window, cx),
            Err(message) => return Some(message),
        }
        None
    }
}

// Prefer the original tab, then a resumed chat, as Iced's parent_tab_id does.
fn roster_chat_owner<Id: Copy>(
    worker: &gitterm_core::tasks::Delegation,
    chats: impl Iterator<Item = Id> + Clone,
    matches: impl Fn(Id, &str) -> bool,
) -> Option<Id> {
    chats
        .clone()
        .find(|id| matches(*id, &worker.parent.session_uid))
        .or_else(|| {
            let chat = worker.parent.chat_session_id.as_deref()?;
            chats.into_iter().find(|id| matches(*id, chat))
        })
}

fn roster_entries(
    tasks: &[gitterm_core::tasks::TaskRecord],
    workers: &[gitterm_core::tasks::Delegation],
    tab: impl Fn(&gitterm_core::tasks::Delegation) -> gitterm_core::delegations::WorkerTab,
) -> Vec<gitterm_gpui_chat::model::roster::RosterEntry> {
    use gitterm_core::tasks::{DelegationChild, DelegationKind, DelegationStatus};
    use gitterm_gpui_chat::model::roster::{RosterEntry, WorkerState};
    let stamp = |s: &str| {
        gitterm_core::work::summary::parse_stamp(s)
            .and_then(|s| s.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64)
    };
    let mut workers: Vec<_> = workers
        .iter()
        .filter(|w| w.dismissed_at.is_none())
        .collect();
    workers.sort_by_key(|worker| std::cmp::Reverse(stamp(&worker.created_at)));
    let mut rows: Vec<_> = workers
        .into_iter()
        .map(|worker| {
            let (name, model, task) = match &worker.child {
                DelegationChild::TaskSession {
                    task_id,
                    preset_name,
                    model,
                    ..
                } => (
                    format!("Worker · {}", preset_name.as_deref().unwrap_or("agent")),
                    model.clone().unwrap_or_else(|| "default model".into()),
                    tasks.iter().find(|t| &t.task_id == task_id),
                ),
                DelegationChild::CodexReview { model, .. } => (
                    "Codex review".into(),
                    model.clone().unwrap_or_else(|| "default model".into()),
                    None,
                ),
                DelegationChild::CodexConsult { model, .. } => (
                    "Codex consult".into(),
                    model.clone().unwrap_or_else(|| "default model".into()),
                    None,
                ),
                DelegationChild::ClaudeSubagent { .. } => (
                    match worker.kind {
                        DelegationKind::Review => "Review",
                        DelegationKind::Consult => "Consult",
                        DelegationKind::Implement => "Worker",
                    }
                    .into(),
                    String::new(),
                    None,
                ),
            };
            let mut live = false;
            let state = if worker.superseded_by.is_some() {
                WorkerState::Failed
            } else if matches!(worker.child, DelegationChild::TaskSession { .. }) {
                match gitterm_core::delegations::worker_session_state(
                    &worker.status,
                    task.map(|t| t.lifecycle),
                    tab(worker),
                ) {
                    "queued" | "starting" => {
                        live = true;
                        WorkerState::Queued
                    }
                    "running" => {
                        live = true;
                        WorkerState::Running
                    }
                    "waiting" => {
                        live = true;
                        WorkerState::Waiting
                    }
                    "blocked" => WorkerState::Waiting,
                    "done" => WorkerState::Done,
                    _ => WorkerState::Failed,
                }
            } else {
                match worker.status {
                    DelegationStatus::Requested => {
                        live = true;
                        WorkerState::Queued
                    }
                    DelegationStatus::Running => {
                        live = true;
                        WorkerState::Running
                    }
                    DelegationStatus::Completed => WorkerState::Done,
                    DelegationStatus::Blocked => WorkerState::Waiting,
                    _ => WorkerState::Failed,
                }
            };
            RosterEntry {
                id: format!("dlg:{}", worker.delegation_id),
                name,
                model,
                checkout: roster_checkout(worker, task),
                activity: task
                    .and_then(|t| t.handoff.as_ref())
                    .or_else(|| worker.result.as_ref().and_then(|r| r.handoff.as_ref()))
                    .map(|h| {
                        h.summary
                            .trim()
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(200)
                            .collect()
                    })
                    .unwrap_or_default(),
                state,
                started_at_ms: stamp(
                    if !matches!(worker.child, DelegationChild::TaskSession { .. })
                        && worker.status == DelegationStatus::Running
                    {
                        &worker.updated_at
                    } else {
                        &worker.created_at
                    },
                ),
                ended_at_ms: (!live).then(|| stamp(&worker.updated_at)),
            }
        })
        .collect();
    // Input is newest first (including equal timestamps), matching card replay.
    rows.sort_by_key(|row| std::cmp::Reverse(row.ended_at_ms.is_none()));
    let mut finished = 0;
    rows.retain(|row| {
        if row.ended_at_ms.is_some() && row.state != WorkerState::Waiting {
            finished += 1;
            finished <= 10
        } else {
            true
        }
    });
    rows
}

fn roster_checkout(
    worker: &gitterm_core::tasks::Delegation,
    task: Option<&gitterm_core::tasks::TaskRecord>,
) -> String {
    task.and_then(|task| task.worktree.path.as_ref())
        .or(worker.options.checkout.as_ref())
        .or(Some(&worker.parent.cwd))
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::dock::{InsertTarget, PaneRef};

    #[test]
    fn roster_entries_project_delegation_and_task_checkouts() {
        let delegation: gitterm_core::tasks::Delegation = serde_json::from_value(serde_json::json!({
            "delegation_id": "checkout-option",
            "kind": "implement",
            "parent": {"session_uid":"parent", "workspace":"repo", "cwd":"/repo/session-checkout"},
            "child": {"runner":"codex_consult", "model":"gpt-6"},
            "brief":"fixture",
            "status": {"state":"running"},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z",
            "options": {"checkout":"/repo/delegation-checkout"}
        }))
        .unwrap();
        assert_eq!(
            roster_checkout(&delegation, None),
            "/repo/delegation-checkout"
        );

        let task: gitterm_core::tasks::TaskRecord = serde_json::from_value(serde_json::json!({
            "schema_version":1,
            "task_id":"task-1", "title":"fixture", "objective":"fixture",
            "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z",
            "workspace":{"name":"repo","location":{"kind":"local","directory":"/repo"}},
            "repository":{"common_dir":"/repo/.git","remote_url":null},
            "base":{"reference":"main","commit":"abc"}, "branch":"worker",
            "worktree":{"state":"ready","path":"/repo/task-worktree"},
            "executor":{"kind":"local"}, "stopping_boundary":"implement_until_tests_pass",
            "lifecycle":"running", "attention":{}, "changes":{}, "verification":{"state":"unknown"}, "delivery":{}
        }))
        .unwrap();
        let mut task_delegation = delegation.clone();
        task_delegation.delegation_id = "task-worktree".into();
        task_delegation.child = gitterm_core::tasks::DelegationChild::TaskSession {
            task_id: "task-1".into(),
            task_session_id: None,
            conversation: None,
            preset_name: Some("Codex".into()),
            model: Some("gpt-6".into()),
            role: None,
        };
        task_delegation.options.checkout = None;
        let rows = roster_entries(&[task], &[delegation, task_delegation], |_| {
            gitterm_core::delegations::WorkerTab::Closed
        });
        assert_eq!(rows[0].checkout, "/repo/delegation-checkout");
        assert_eq!(rows[1].checkout, "/repo/task-worktree");
    }

    fn roster_fixture(source: &Path) -> gitterm_core::tasks::TaskStoreDocument {
        use gitterm_core::tasks::*;
        let mut task = tasks::fixture_task("worker-task", "Fixture", "task/worker");
        task.worktree.path = Some("/other-repo/worktrees/real-worker-tree".into());
        task.worktree.state = TaskWorktreeState::Ready;
        let workers = (0..16).map(|i| {
            serde_json::from_value(serde_json::json!({
                "delegation_id": format!("worker-{i}"), "kind":"implement",
                "parent":{"session_uid":"original", "chat_session_id":"conversation", "workspace":"home", "cwd":"/parent-cwd"},
                "child":{"runner":"task_session", "task_id":"worker-task", "task_session_id":"child", "preset_name":"Codex", "model":"gpt-6-luna"},
                "brief":"fixture", "status":{"state": if i == 14 { "running" } else if i == 15 { "blocked" } else { "completed" }},
                "result": if i == 14 { serde_json::Value::Null } else { serde_json::json!({"handoff":{"summary":"Fixture result","updated_at":"2026-01-01T00:01:00Z"}}) },
                "created_at":format!("2026-01-01T00:00:{i:02}Z"),
                "updated_at":"2026-01-01T00:01:00Z",
                "options":{"checkout":"/requested-parent-checkout"}
            })).unwrap()
        }).collect();
        std::fs::write(source.join("workspaces.json"), serde_json::json!({
            "active_workspace":0,
            "workspaces":[{"name":"home", "abbrev":"H", "color":"blue", "dir":"/home", "tabs":[
                {"dir":"/other-repo/worktrees/real-worker-tree", "task_id":"worker-task", "task_session_id":"child"}
            ]}]
        }).to_string()).unwrap();
        TaskStoreDocument {
            tasks: vec![task],
            delegations: workers,
            ..Default::default()
        }
    }

    fn write_roster_fixture(source: &Path, document: &gitterm_core::tasks::TaskStoreDocument) {
        std::fs::write(
            source.join("tasks.json"),
            serde_json::to_vec(document).unwrap(),
        )
        .unwrap();
    }

    fn fixture_roster(
        facts: &data::RosterFacts,
    ) -> Vec<gitterm_gpui_chat::model::roster::RosterEntry> {
        roster_entries(&facts.tasks, &facts.workers, |_| {
            facts.worker_tab("worker-task", Some("child"))
        })
    }

    #[test]
    fn roster_store_matches_iced_rows_order_worktrees_and_parent_identity() {
        let source = tempfile::tempdir().unwrap();
        let mut document = roster_fixture(source.path());
        let mut unrelated = document.delegations[14].clone();
        unrelated.delegation_id = "other-chat".into();
        unrelated.parent.session_uid = "other".into();
        unrelated.parent.chat_session_id = Some("other-conversation".into());
        document.delegations.push(unrelated);
        write_roster_fixture(source.path(), &document);
        let before = std::fs::read(source.path().join("tasks.json")).unwrap();
        let mut facts = data::RosterFacts::read_from(source.path()).unwrap();
        let chats = [
            (1, "resumed", "conversation"),
            (2, "original", "conversation"),
            (3, "other", "other-conversation"),
        ];
        let owner = |worker: &gitterm_core::tasks::Delegation, with_original| {
            roster_chat_owner(
                worker,
                chats
                    .iter()
                    .filter(move |(id, _, _)| with_original || *id != 2)
                    .map(|(id, _, _)| *id),
                |id, identity| {
                    chats.iter().any(|(candidate, uid, chat)| {
                        *candidate == id && (*uid == identity || *chat == identity)
                    })
                },
            )
        };
        assert_eq!(owner(&facts.workers[1], true), Some(2));
        assert_eq!(owner(&facts.workers[1], false), Some(1));
        facts
            .workers
            .retain(|worker| owner(worker, true) == Some(2));
        let rows = fixture_roster(&facts);
        let expected: Vec<_> = [14, 15]
            .into_iter()
            .chain((4..14).rev())
            .map(|i| format!("dlg:worker-{i}"))
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>(),
            expected
        );
        assert!(
            rows.iter()
                .all(|row| row.checkout == "/other-repo/worktrees/real-worker-tree")
        );
        let rendered = gitterm_gpui_chat::model::roster::rows(&rows, rows[0].started_at_ms + 1_000);
        assert!(
            rendered
                .iter()
                .all(|row| row.what == "· in real-worker-tree")
        );
        assert_eq!(
            std::fs::read(source.path().join("tasks.json")).unwrap(),
            before
        );
    }

    #[test]
    fn roster_refresh_adds_new_workers_and_removes_dismissed_or_old_finished_rows() {
        use gitterm_core::tasks::DelegationStatus;
        let source = tempfile::tempdir().unwrap();
        let mut document = roster_fixture(source.path());
        write_roster_fixture(source.path(), &document);
        let first = data::RosterFacts::read_from(source.path()).unwrap();
        assert!(
            fixture_roster(&first)
                .iter()
                .any(|row| row.id == "dlg:worker-14")
        );
        document.delegations[14].status = DelegationStatus::Completed;
        document.delegations[14].result = document.delegations[13].result.clone();
        document.delegations[14].dismissed_at = Some("2026-01-01T00:02:00Z".into());
        document.delegations[15].status = DelegationStatus::Completed;
        let mut new = document.delegations[13].clone();
        new.delegation_id = "new-worker".into();
        new.status = DelegationStatus::Running;
        new.result = None;
        new.created_at = "2026-01-01T00:02:00Z".into();
        new.updated_at = new.created_at.clone();
        document.delegations.push(new);
        write_roster_fixture(source.path(), &document);
        let next = data::RosterFacts::read_from(source.path()).unwrap();
        let rows = fixture_roster(&next);
        assert_eq!(rows[0].id, "dlg:new-worker");
        assert!(!rows.iter().any(|row| row.id == "dlg:worker-14"));
        assert!(!rows.iter().any(|row| row.id == "dlg:worker-4"));
        assert_eq!(rows.len(), 11);
        assert!(
            rows.iter()
                .find(|row| row.id == "dlg:worker-15")
                .unwrap()
                .ended_at_ms
                .is_some()
        );
    }

    #[test]
    fn roster_elapsed_advances_while_running_and_stops_when_finished() {
        let source = tempfile::tempdir().unwrap();
        let document = roster_fixture(source.path());
        write_roster_fixture(source.path(), &document);
        let facts = data::RosterFacts::read_from(source.path()).unwrap();
        let rows = fixture_roster(&facts);
        let start = rows[0].started_at_ms;
        let first = gitterm_gpui_chat::model::roster::rows(&rows, start + 10_000);
        let next = gitterm_gpui_chat::model::roster::rows(&rows, start + 11_000);
        let elapsed = |rows: &[gitterm_gpui_chat::model::roster::RosterRow], id: &str| {
            rows.iter()
                .find(|row| row.id == id)
                .unwrap()
                .elapsed
                .clone()
        };
        assert_eq!(elapsed(&first, "dlg:worker-14"), "10.0s");
        assert_eq!(elapsed(&next, "dlg:worker-14"), "11.0s");
        assert_eq!(
            elapsed(&first, "dlg:worker-13"),
            elapsed(&next, "dlg:worker-13")
        );
        assert_eq!(
            elapsed(&first, "dlg:worker-15"),
            elapsed(&next, "dlg:worker-15")
        );
        let mut closed = facts;
        closed.tabs.clear();
        assert!(
            fixture_roster(&closed)
                .iter()
                .find(|row| row.id == "dlg:worker-14")
                .unwrap()
                .ended_at_ms
                .is_some()
        );
    }

    #[test]
    fn window_title_matches_iced_workspace_and_sidebar_format() {
        assert_eq!(
            window_title(
                "GitTerm V6 Development",
                "GitTerm V6 Upgrade",
                SidebarMode::Work
            ),
            "GitTerm V6 Development - GitTerm V6 Upgrade - Work"
        );
        assert_eq!(
            window_title("GitTerm V6 Development", "GitRepo", SidebarMode::Git),
            "GitTerm V6 Development - GitRepo - Git"
        );
    }

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
        cx.update(|cx| {
            gpui_component::init(cx);
            install_app_menus(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let (shell, cx) = cx.add_window_view(|w, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), w, cx).unwrap()
        });
        shell.update(cx, |shell, cx| {
            shell.dock_fixture = true;
            shell.workspaces[0].active_session = Some(SessionId(12));
            cx.notify();
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
        shell.update(cx, |shell, cx| {
            shell.workspaces[0].active_session = Some(SessionId(11));
            cx.notify();
        });
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
            assert!(!dock.read(cx).has_dock(DockPlacement::Bottom));
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
                dock.set_dock(DockPlacement::Bottom, DockLayout::tabs(), window, cx);
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
