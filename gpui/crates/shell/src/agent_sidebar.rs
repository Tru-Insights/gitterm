//! Iced's Agent sidebar (`SidebarMode::Claude`): the agent configuration
//! read by `gitterm_core::agent_config`, in five folded sections. Selecting an
//! item opens its file in the read-only viewer. Nothing here writes.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use gitterm_core::agent_config::{AgentConfig, SECTIONS};
use gpui::{App, EventEmitter, Window, div, prelude::*, px, rgb};
use gpui_component::button::{Button, ButtonVariants};

/// The agent's home config directory (`~/.claude`). The app sets it with its
/// imported catalog; sample views and tests without one never read the
/// user's agent configuration.
#[derive(Clone)]
pub(crate) struct AgentHome(pub PathBuf);
impl gpui::Global for AgentHome {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentOutcome {
    BackToWork,
    OpenFile(PathBuf),
}

pub struct AgentSidebar {
    dir: PathBuf,
    home: Option<PathBuf>,
    config: AgentConfig,
    /// Iced starts every section folded.
    expanded: HashSet<&'static str>,
    selected: Option<(&'static str, usize)>,
}

impl AgentSidebar {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            home: None,
            config: AgentConfig::default(),
            expanded: HashSet::new(),
            selected: None,
        }
    }

    /// Iced reads the active tab's repo (`Tab::fetch_claude_config` uses
    /// `self.repo_path`), so the pane follows the active session.
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
    }

    /// Iced re-reads on entering the Agent view and on returning to Work.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.home = cx.try_global::<AgentHome>().map(|home| home.0.clone());
        self.config = self
            .home
            .as_deref()
            .map(|home| AgentConfig::load(home, &self.dir))
            .unwrap_or_default();
        cx.notify();
    }

    /// Work's "skills · MCPs · hooks" link, hidden when all are zero.
    pub fn summary(&self) -> Option<String> {
        gitterm_core::work::summary::agent_config_line(
            self.config.skills.len(),
            self.config.mcp_servers.len(),
            self.config.hooks.len(),
        )
    }

    /// The root the viewer reads `path` under, only for a file this pane lists.
    pub fn file_root(&self, path: &Path) -> Option<PathBuf> {
        SECTIONS
            .iter()
            .flat_map(|(_, key)| self.config.section(key))
            .any(|item| item.file_path == path)
            .then(|| {
                [self.home.as_deref(), Some(self.dir.as_path())]
                    .into_iter()
                    .flatten()
                    .filter(|root| path.starts_with(root))
                    .max_by_key(|root| root.components().count())
                    .map(Path::to_path_buf)
            })
            .flatten()
    }

    fn toggle(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    fn select(&mut self, key: &'static str, index: usize, cx: &mut Context<Self>) {
        self.selected = Some((key, index));
        if let Some(item) = self.config.section(key).get(index) {
            cx.emit(AgentOutcome::OpenFile(item.file_path.clone()));
        }
        cx.notify();
    }
}

impl EventEmitter<AgentOutcome> for AgentSidebar {}

impl Render for AgentSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::current(cx);
        let dark = gpui_component::Theme::global(cx).mode.is_dark();
        let (secondary, mauve) = if dark {
            (0xa6adc8, 0xcba6f7)
        } else {
            (0x6c6f85, 0x8839ef)
        };
        let green = if dark { 0xa6e3a1 } else { 0x40a02b };
        let dots = [green, p.blue, p.peach, mauve, secondary];
        let mut body = div()
            .id("agent-sidebar")
            .debug_selector(|| "agent-sidebar".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_size(px(12.));
        for ((label, key), dot) in SECTIONS.into_iter().zip(dots) {
            let items = self.config.section(key);
            let expanded = self.expanded.contains(key);
            let chevron = if expanded { "\u{25BC}" } else { "\u{25B6}" };
            let count = items.len();
            body = body.child(
                Button::new(format!("agent-section-{key}"))
                    .debug_selector(move || format!("agent-section-{key}"))
                    .ghost()
                    .rounded_none()
                    .w_full()
                    .h_auto()
                    .px(px(10.))
                    .py(px(6.))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .debug_selector(move || {
                                        format!("agent-chevron-{key}-{chevron}")
                                    })
                                    .text_size(px(11.))
                                    .text_color(rgb(secondary))
                                    .child(chevron),
                            )
                            .child(div().w(px(6.)))
                            .child(
                                div()
                                    .debug_selector(move || format!("agent-label-{label}"))
                                    .text_color(rgb(p.text))
                                    .child(label),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .debug_selector(move || format!("agent-count-{key}-{count}"))
                                    .px(px(6.))
                                    .py(px(1.))
                                    .rounded(px(8.))
                                    .bg(gpui::rgba(p.muted << 8 | 0x26))
                                    .text_size(px(11.))
                                    .text_color(rgb(p.muted))
                                    .child(count.to_string()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx))),
            );
            if !expanded {
                continue;
            }
            for (index, item) in items.iter().enumerate() {
                let selected = self.selected == Some((key, index));
                body = body.child(
                    div()
                        .flex()
                        .child(div().w(px(2.)).flex_shrink_0().when(selected, |border| {
                            border
                                .bg(rgb(p.blue))
                                .debug_selector(move || format!("agent-selected-{key}-{index}"))
                        }))
                        .child(
                            Button::new(format!("agent-item-{key}-{index}"))
                                .debug_selector(move || format!("agent-item-{key}-{index}"))
                                .ghost()
                                .rounded_none()
                                .flex_1()
                                .h_auto()
                                .pl(px(24.))
                                .pr(px(10.))
                                .py(px(4.))
                                .when(selected, |item| item.bg(rgb(p.surface)))
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .child(div().size(px(6.)).rounded(px(3.)).bg(rgb(dot)))
                                        .child(div().w(px(8.)))
                                        .child(
                                            div()
                                                .debug_selector(move || {
                                                    format!("agent-name-{key}-{index}")
                                                })
                                                .flex_1()
                                                .min_w_0()
                                                .whitespace_normal()
                                                .text_color(rgb(p.text))
                                                .child(item.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .debug_selector(move || {
                                                    format!("agent-scope-{key}-{index}")
                                                })
                                                .flex_shrink_0()
                                                .ml(px(4.))
                                                .px(px(4.))
                                                .py(px(1.))
                                                .rounded(px(4.))
                                                .bg(gpui::rgba(p.muted << 8 | 0x1f))
                                                .text_size(px(10.))
                                                .text_color(rgb(p.muted))
                                                .child(item.scope.badge()),
                                        ),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.select(key, index, cx)),
                                ),
                        ),
                );
            }
            if items.is_empty() {
                body = body.child(
                    div()
                        .pl(px(28.))
                        .pr(px(10.))
                        .py(px(4.))
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child("None found"),
                );
            }
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().p(px(4.)).child(
                    Button::new("agent-back-work")
                        .debug_selector(|| "agent-back-work".into())
                        .ghost()
                        .label("\u{2190} Work")
                        .text_color(rgb(p.blue))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(AgentOutcome::BackToWork))),
                ),
            )
            .child(body)
    }
}

impl crate::Shell {
    /// One Agent pane per workspace, read with its Work sidebar so Work's
    /// summary link shows counts before the pane is opened.
    pub(crate) fn ensure_agent_sidebar(
        &mut self,
        workspace: crate::WorkspaceId,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.agent_sidebars.contains_key(&workspace) {
            return;
        }
        let pane = cx.new(|cx| {
            let mut pane = AgentSidebar::new(dir);
            pane.reload(cx);
            pane
        });
        self.agent_subscriptions.insert(
            workspace,
            cx.subscribe_in(&pane, window, move |shell, _, outcome, _, cx| {
                shell.agent_outcome(workspace, outcome, cx)
            }),
        );
        self.agent_sidebars.insert(workspace, pane);
        self.sync_agent_line(workspace, cx);
    }

    /// The active session's repo, as Iced's tab `repo_path`; the workspace
    /// folder when the session has none.
    pub(crate) fn active_repo_dir(
        &self,
        workspace: crate::WorkspaceId,
        cx: &App,
    ) -> Option<PathBuf> {
        let state = self
            .workspaces
            .iter()
            .find(|w| w.workspace.id == workspace)?;
        let tab = state.active_session.and_then(|id| self.session_tab(id, cx));
        tab_repo_dir(tab).or_else(|| self.data.terminal_cwd(workspace, cx))
    }

    fn sync_agent_line(&self, workspace: crate::WorkspaceId, cx: &mut Context<Self>) {
        let line = self
            .agent_sidebars
            .get(&workspace)
            .and_then(|pane| pane.read(cx).summary());
        if let Some(sidebar) = self.work_sidebars.get(&workspace) {
            sidebar.update(cx, |sidebar, cx| sidebar.set_agent_line(line, cx));
        }
    }

    /// `SetSidebarMode(Claude)` / `SetSidebarMode(Workspace)`: both re-read.
    pub(crate) fn show_agent_sidebar(
        &mut self,
        workspace: crate::WorkspaceId,
        agent: bool,
        cx: &mut Context<Self>,
    ) {
        let dir = self.active_repo_dir(workspace, cx);
        if let Some(pane) = self.agent_sidebars.get(&workspace) {
            pane.update(cx, |pane, cx| {
                if let Some(dir) = dir {
                    pane.set_dir(dir);
                }
                pane.reload(cx)
            });
        }
        self.sync_agent_line(workspace, cx);
        if let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == workspace)
        {
            state.store.regions.sidebar_mode = if agent {
                crate::SidebarMode::Agent
            } else {
                crate::SidebarMode::Work
            };
            state.store.regions.sidebar_collapsed = false;
            crate::save_layout(state, cx);
        }
        cx.notify();
    }

    fn agent_outcome(
        &mut self,
        workspace: crate::WorkspaceId,
        outcome: &AgentOutcome,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            AgentOutcome::BackToWork => self.show_agent_sidebar(workspace, false, cx),
            // Iced's ViewFile: the read-only viewer.
            AgentOutcome::OpenFile(path) => self
                .owner
                .update(cx, |owner, cx| owner.open_file(workspace, path.clone(), cx)),
        }
    }

    /// The viewer root for a file the workspace's Agent pane lists.
    pub(crate) fn agent_file_root(
        &self,
        workspace: crate::WorkspaceId,
        path: &Path,
        cx: &App,
    ) -> Option<PathBuf> {
        self.agent_sidebars
            .get(&workspace)
            .and_then(|pane| pane.read(cx).file_root(path))
    }
}

/// A tab's repo as Iced's `repo_path`: its repo dir, else its launch dir.
fn tab_repo_dir(
    tab: Option<&gitterm_core::workspace_config::WorkspaceTabConfig>,
) -> Option<PathBuf> {
    tab.map(|tab| PathBuf::from(tab.repo_dir.as_deref().unwrap_or(&tab.dir)))
}

#[cfg(test)]
#[test]
fn the_agent_pane_reads_the_active_tabs_repo_like_iced() {
    let mut tab: gitterm_core::workspace_config::WorkspaceTabConfig =
        serde_json::from_value(serde_json::json!({"dir": "/ws"})).unwrap();
    assert_eq!(tab_repo_dir(Some(&tab)), Some(PathBuf::from("/ws")));
    tab.repo_dir = Some("/ws/repo".into());
    assert_eq!(tab_repo_dir(Some(&tab)), Some(PathBuf::from("/ws/repo")));
    assert_eq!(tab_repo_dir(None), None);
}

#[cfg(test)]
#[gpui::test]
fn a_long_plugin_name_never_overlaps_the_scope_tag(cx: &mut gpui::TestAppContext) {
    use gitterm_core::agent_config::{AgentConfigItem, ConfigScope};
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = AgentSidebar::new(PathBuf::from("/ws"));
        sidebar.config.plugins.push(AgentConfigItem {
            name: "frontend-design@claude-code-plugins".into(),
            file_path: PathBuf::from("/ws/p"),
            scope: ConfigScope::User,
        });
        sidebar.expanded.insert("plugins");
        sidebar
    });
    cx.simulate_resize(gpui::size(px(310.), px(700.)));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let name = cx.debug_bounds("agent-name-plugins-0").unwrap();
    let tag = cx.debug_bounds("agent-scope-plugins-0").unwrap();
    assert!(name.right() <= tag.left(), "{name:?} overlaps {tag:?}");
}
