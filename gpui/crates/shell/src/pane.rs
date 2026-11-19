use crate::{
    data::{FileNode, SessionKind, ShellData, WorkspaceId},
    theme::Palette,
};
use gitterm_gpui_chat::{ChatView, events::parse_fixture};
use gitterm_terminal::{Backend, BackendSettings, TerminalConfig, TerminalView};
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    dock::{BasePanel, Panel, PanelEvent, PanelInfo, PanelState},
};
use std::{collections::HashSet, rc::Rc};

pub const PANEL_NAME: &str = "GitTermShellPane";
#[derive(Clone, Copy)]
pub enum PaneKind {
    Terminal,
    Chat,
    Git,
    Files,
}
impl From<SessionKind> for PaneKind {
    fn from(kind: SessionKind) -> Self {
        match kind {
            SessionKind::Terminal => Self::Terminal,
            SessionKind::Chat => Self::Chat,
        }
    }
}
pub enum PaneContent {
    Terminal(Entity<TerminalView>),
    Chat(Entity<ChatView>),
    Git,
    Files,
    Error(String),
}
pub struct Pane {
    pub id: String,
    pub title: String,
    pub content: PaneContent,
    workspace: WorkspaceId,
    data: Rc<dyn ShellData>,
    focus: FocusHandle,
    expanded: HashSet<String>,
}
impl Pane {
    pub fn new(
        id: String,
        title: String,
        kind: PaneKind,
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content =
            match kind {
                PaneKind::Terminal => {
                    let settings = BackendSettings {
                        working_directory: dirs::home_dir(),
                        ..Default::default()
                    };
                    // Headless fixtures must not run host rc/session-restore scripts.
                    #[cfg(test)]
                    let settings = BackendSettings {
                        program: "/bin/zsh".into(),
                        args: vec!["-d".into(), "-f".into(), "-i".into()],
                        ..settings
                    };
                    match settings.working_directory.as_ref() {
                        None => PaneContent::Error(format!(
                            "Start terminal {title}: cannot resolve HOME"
                        )),
                        Some(home) => match Backend::new(settings.clone()) {
                            Ok(backend) => PaneContent::Terminal(cx.new(|cx| {
                                TerminalView::new(backend, TerminalConfig::default(), cx)
                            })),
                            Err(error) => PaneContent::Error(format!(
                                "Start terminal {title} ({}) in {}: {error:#}",
                                settings.program,
                                home.display()
                            )),
                        },
                    }
                }
                PaneKind::Chat => PaneContent::Chat(cx.new(|cx| {
                    ChatView::new(
                        parse_fixture(include_str!("../../chat/fixtures/showcase.jsonl"))
                            .expect("bundled synthetic showcase fixture is valid"),
                        window,
                        cx,
                    )
                })),
                PaneKind::Git => PaneContent::Git,
                PaneKind::Files => PaneContent::Files,
            };
        if let PaneContent::Error(error) = &content {
            eprintln!("{error}");
        }
        Self {
            id,
            title,
            content,
            workspace,
            data,
            focus: cx.focus_handle(),
            expanded: HashSet::from(["src".into()]),
        }
    }
    pub(crate) fn status_view(&self, cx: &App) -> crate::data::GitStatus {
        self.data.status(self.workspace, cx)
    }
    fn files(
        &self,
        nodes: Vec<FileNode>,
        parent: &str,
        depth: usize,
        p: Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = vec![];
        for node in nodes {
            let path = if parent.is_empty() {
                node.name.clone()
            } else {
                format!("{parent}/{}", node.name)
            };
            let directory = !node.children.is_empty();
            let expanded = self.expanded.contains(&path);
            let label = format!(
                "{} {}",
                if directory {
                    if expanded { "⌄" } else { "›" }
                } else {
                    "·"
                },
                node.name
            );
            if directory {
                let key = path.clone();
                rows.push(
                    Button::new(SharedString::from(path.clone()))
                        .ghost()
                        .label(label)
                        .w_full()
                        .accessibility_label(format!(
                            "{} directory, {}",
                            node.name,
                            if expanded { "expanded" } else { "collapsed" }
                        ))
                        .pl(px(8. + depth as f32 * 16.))
                        .justify_start()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded.remove(&key) {
                                this.expanded.insert(key.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            } else {
                rows.push(
                    div()
                        .pl(px(16. + depth as f32 * 16.))
                        .py_1()
                        .text_color(rgb(p.muted))
                        .child(label)
                        .into_any_element(),
                );
            }
            if directory && expanded {
                rows.extend(self.files(node.children, &path, depth + 1, p, cx));
            }
        }
        rows
    }
}
impl Focusable for Pane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            PaneContent::Terminal(view) => view.read(cx).focus_handle().clone(),
            PaneContent::Chat(view) => view.focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}
impl EventEmitter<PanelEvent> for Pane {}
impl BasePanel for Pane {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }
    fn dump(&self, _: &App) -> PanelState {
        PanelState {
            panel_name: PANEL_NAME.into(),
            children: vec![],
            info: PanelInfo::panel(serde_json::json!(self.id)),
        }
    }
}
impl Panel for Pane {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.title.clone()
    }
    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}
impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let body = div()
            .id("pane-body")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(p.base))
            .text_color(rgb(p.text));
        match &self.content {
            PaneContent::Terminal(view) => div()
                .id(SharedString::from(self.id.clone()))
                .debug_selector({
                    let id = self.id.clone();
                    move || id
                })
                .size_full()
                .min_h_0()
                .child(view.clone()),
            PaneContent::Chat(view) => div()
                .id(SharedString::from(self.id.clone()))
                .debug_selector({
                    let id = self.id.clone();
                    move || id
                })
                .size_full()
                .min_h_0()
                .child(view.clone()),
            PaneContent::Error(error) => body.text_color(rgb(p.red)).child(error.clone()),
            PaneContent::Git => {
                let status = self.status_view(cx);
                body.child(status.repository)
                    .child(
                        div()
                            .text_color(rgb(p.muted))
                            .child(format!("⑂ {}", status.branch)),
                    )
                    .children(status.loading.then(|| div().child("Loading Git status…")))
                    .children(
                        status
                            .error
                            .map(|error| div().text_color(rgb(p.red)).child(error)),
                    )
                    .children(
                        (!status.loading && !status.is_git_repo)
                            .then(|| div().child("Not a Git repository")),
                    )
                    .child(format!("Changes · {}", status.files.len()))
                    .children(status.files.into_iter().map(|file| {
                        let group = if file.staged {
                            "staged"
                        } else if file.status == "??" {
                            "untracked"
                        } else {
                            "unstaged"
                        };
                        div()
                            .flex()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .w(px(24.))
                                    .text_color(rgb(if file.staged { p.green } else { p.blue }))
                                    .child(file.status),
                            )
                            .child(div().flex_1().child(file.path))
                            .child(div().text_color(rgb(p.muted)).child(group))
                    }))
            }
            PaneContent::Files => body
                .child(self.data.status(self.workspace, cx).repository)
                .children(self.files(self.data.files(self.workspace, cx), "", 0, p, cx)),
        }
    }
}
