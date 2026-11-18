#![cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

pub mod data;
mod dialogs;
mod layout;
mod pane;
mod theme;

use anyhow::{Result, ensure};
use data::{Session, SessionId, ShellData, Workspace, WorkspaceId};
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
actions!(gitterm_shell, [ShowShortcuts]);

struct WorkspaceDock {
    workspace: Workspace,
    sessions: Vec<Session>,
    active_session: Option<SessionId>,
    dock: Entity<DockArea>,
    // Sole lifetime owner: a dock can drop every reference without losing a draft.
    panes: HashMap<String, Entity<Pane>>,
    store: LayoutStore,
    error: Option<String>,
}
pub struct Shell {
    data: Rc<dyn ShellData>,
    workspaces: Vec<WorkspaceDock>,
    active_workspace: WorkspaceId,
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
        let catalog = data.workspaces();
        ensure!(!catalog.is_empty(), "shell data has no workspaces");
        let active_workspace = catalog[0].id;
        let mut workspace_ids = HashSet::new();
        let mut session_ids = HashSet::new();
        let mut workspaces = vec![];
        for workspace in catalog {
            ensure!(
                workspace_ids.insert(workspace.id),
                "duplicate workspace ID {:?}",
                workspace.id
            );
            let sessions = data.sessions(workspace.id);
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
                        session.kind.into(),
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
                let id = format!("workspace-{}-{name}", workspace.id.0);
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
            workspaces.push(WorkspaceDock {
                active_session: sessions.first().map(|s| s.id),
                store: LayoutStore::new(config_dir.join(format!("layout-{}.json", workspace.id.0))),
                workspace,
                sessions,
                panes,
                dock,
                error: None,
            });
        }
        // Restoration resolves stable persisted IDs to the SAME entities. It never
        // constructs panes, even when a layout is loaded again within this process.
        let handles: HashMap<_, _> = workspaces
            .iter()
            .flat_map(|w| w.panes.iter().map(|(id, p)| (id.clone(), p.downgrade())))
            .collect();
        register_panel(cx, PANEL_NAME, move |context, _, _| {
            let gpui_component::dock::PanelInfo::Panel(value) = context.info() else {
                unreachable!("registry only builds leaves")
            };
            let id = value.as_str().expect("layout IDs validated before load");
            panel_handle(
                handles
                    .get(id)
                    .expect("layout IDs validated before load")
                    .upgrade()
                    .expect("shell owns pane until window closes"),
            )
        });
        let mut subscriptions = vec![];
        for state in &mut workspaces {
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
            let id = state.workspace.id;
            subscriptions.push(
                cx.subscribe(&state.dock, move |this, dock, _: &DockEvent, cx| {
                    let state = this
                        .workspaces
                        .iter_mut()
                        .find(|w| w.workspace.id == id)
                        .expect("workspace still owned");
                    if state.store.is_refused() {
                        return;
                    }
                    if let Err(e) = state.store.save(&dock.read(cx).dump(cx)) {
                        let error = format!("{e:#}");
                        if state.error.as_ref() != Some(&error) {
                            eprintln!("{error}");
                        }
                        state.error = Some(error);
                        cx.notify();
                    }
                }),
            );
        }
        Ok(Self {
            data,
            workspaces,
            active_workspace,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        })
    }
    fn active(&self) -> &WorkspaceDock {
        self.workspaces
            .iter()
            .find(|w| w.workspace.id == self.active_workspace)
            .expect("active workspace exists")
    }
    fn select_workspace(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.workspaces.get(index) {
            self.active_workspace = state.workspace.id;
            self.focus.focus(window, cx);
            cx.notify();
        }
    }
    fn select_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.active().sessions.get(index) {
            let id = session.id;
            self.workspaces
                .iter_mut()
                .find(|w| w.workspace.id == self.active_workspace)
                .expect("active workspace exists")
                .active_session = Some(id);
            self.show_pane(&session_pane_id(id), window, cx);
        }
    }
    fn show_pane(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.active();
        let Some(pane) = state.panes.get(id).cloned() else {
            return;
        };
        let panel_id = PanelId::from(pane.entity_id());
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
        pane.focus_handle(cx).focus(window, cx);
        cx.notify();
    }
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
        if session.kind == data::SessionKind::Terminal {
            bottom = bottom.panel_view(pane, cx);
            has_terminal = true;
        } else {
            center = center.panel_view(pane, cx);
        }
    }
    let git = state.panes[&format!("workspace-{}-git", state.workspace.id.0)].clone();
    let files = state.panes[&format!("workspace-{}-files", state.workspace.id.0)].clone();
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
        let state = self.active();
        let repository = self.data.status(self.active_workspace).repository;
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
            .children(self.workspaces.iter().enumerate().map(|(index, state)| {
                let workspace = &state.workspace;
                let count = self.data.status(workspace.id).files.len();
                Button::new(("workspace", index))
                    .ghost()
                    .selected(workspace.id == self.active_workspace)
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
                    .bg(rgb(if workspace.id == self.active_workspace {
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
                            .child(workspace.status.clone()),
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
                div()
                    .pl_4()
                    .text_color(rgb(p.muted))
                    .child(self.data.status(self.active_workspace).branch),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child("Local · GPUI sample data"),
            );
        let tabs = TabBar::new("sessions")
            .underline()
            .selected_index(active_session)
            .h(px(38.))
            .children(state.sessions.iter().map(|s| Tab::from(s.title.clone())))
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                this.select_session(*index, window, cx)
            }));
        let weak = cx.weak_entity();
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
                .child(more),
        );
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
            .on_action(cx.listener(|_, _: &ShowShortcuts, w, cx| dialogs::shortcuts(w, cx)))
            .flex()
            .font_family(".SystemUIFont")
            .text_size(px(UI_FONT_SIZE))
            .text_color(rgb(p.text))
            .bg(rgb(p.base))
            .child(sidebar)
            .child(content.child(div().flex_1().min_h_0().child(state.dock.clone())))
    }
}

pub fn run() {
    let home = dirs::home_dir().expect("resolve home for isolated GPUI config");
    let config = layout::config_dir(std::env::var_os("GITTERM_GPUI_CONFIG_DIR"), &home)
        .expect("resolve isolated GPUI config directory");
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
                    title: Some("GitTerm · GPUI shell · Sample data".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let shell = cx.new(|cx| {
                    Shell::new(Rc::new(data::SampleData), &config, window, cx)
                        .expect("create sample shell")
                });
                shell.focus_handle(cx).focus(window, cx);
                cx.new(|cx| Root::new(shell, window, cx))
            },
        )
        .expect("open GitTerm GPUI shell window");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::dock::{InsertTarget, PaneRef};

    // TestAppContext uses GPUI's headless test platform. No native window or input.
    #[gpui::test]
    fn pane_entity_and_draft_survive_moves_grouping_close_reopen_and_restore(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            Shell::new(Rc::new(data::SampleData), dir.path(), window, cx).unwrap()
        });
        let (dock, held, git) = cx.read(|cx| {
            let state = shell.read(cx).active();
            (
                state.dock.clone(),
                state.panes["session-11"].clone(),
                state.panes["workspace-1-git"].clone(),
            )
        });
        let id = PanelId::from(held.entity_id());
        let draft = cx.read(|cx| held.read(cx).draft.clone().unwrap());
        cx.update(|window, cx| {
            draft.update(cx, |s, cx| s.set_value("keep this draft", window, cx));
            held.update(cx, |p, _| p.sample_lines = 7);
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
                held.entity_id()
            );
            // Collapse and reopen an entire dock, then close its pane and reopen it.
            dock.update(cx, |dock, cx| {
                dock.toggle_dock(DockPlacement::Right, window, cx);
                dock.toggle_dock(DockPlacement::Right, window, cx);
                dock.remove_panel(held.clone(), window, cx);
            });
            assert!(dock.read(cx).panel(id).is_none());
            shell.update(cx, |s, cx| s.show_pane("session-11", window, cx));
            assert_eq!(
                dock.read(cx).panel(id).unwrap().view().entity_id(),
                held.entity_id()
            );
            let saved = dock.read(cx).dump(cx);
            dock.update(cx, |dock, cx| dock.load(saved, window, cx).unwrap());
            assert_eq!(
                dock.read(cx).panel(id).unwrap().view().entity_id(),
                held.entity_id()
            );
            assert_eq!(held.read(cx).sample_lines, 7);
            assert_eq!(
                held.read(cx).draft.as_ref().unwrap().entity_id(),
                draft.entity_id()
            );
            assert_eq!(draft.read(cx).value().as_str(), "keep this draft");
            shell.update(cx, |s, cx| {
                s.select_workspace(1, window, cx);
                s.select_session(1, window, cx);
                assert_eq!(s.active().active_session, Some(SessionId(22)));
                s.select_workspace(0, window, cx);
                assert_eq!(s.active().active_session, Some(SessionId(11)));
            });
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(draft.read(cx).value().as_str(), "keep this draft");
            let tree = dock.read(cx).layout(DockPlacement::Center).unwrap();
            assert!(matches!(
                tree.find_node(tree.find_panel_node(id).unwrap())
                    .unwrap()
                    .kind(),
                PaneRef::Tabs { .. }
            ));
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
            let state = shell.read(cx).active();
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
            let state = shell.read(cx).active();
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
            let dock = &restored.read(cx).active().dock;
            assert_eq!(
                dock.read(cx).dock_size(DockPlacement::Bottom),
                Some(px(321.))
            );
            assert!(!dock.read(cx).is_dock_open(DockPlacement::Bottom));
        });
    }
}
