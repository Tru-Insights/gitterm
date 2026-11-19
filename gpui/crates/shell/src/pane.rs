use crate::{
    data::{SessionKind, ShellData, WorkspaceId},
    theme::Palette,
};
use gitterm_gpui_chat::{ChatView, events::parse_fixture};
use gitterm_terminal::{TerminalConfig, TerminalLaunch, TerminalView};
use gpui::*;
use gpui_component::dock::{BasePanel, Panel, PanelEvent, PanelInfo, PanelState};
use std::rc::Rc;

pub const PANEL_NAME: &str = "GitTermShellPane";
#[derive(Clone)]
pub enum PaneKind {
    Terminal,
    Chat,
    Git,
    Files,
    Review(Entity<gitterm_gpui_review::ReviewView>),
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
    Review(Entity<gitterm_gpui_review::ReviewView>),
    Error(String),
}
pub struct Pane {
    pub id: String,
    pub title: String,
    pub content: PaneContent,
    terminal_title: Option<String>,
    terminal_cwd: Option<std::path::PathBuf>,
    workspace: WorkspaceId,
    data: Rc<dyn ShellData>,
    focus: FocusHandle,
    files: Option<Entity<crate::files_view::FilesView>>,
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
        let content = match kind.clone() {
            PaneKind::Terminal => {
                let launch = TerminalLaunch {
                    cwd: data.terminal_cwd(workspace, cx).or_else(dirs::home_dir),
                    ..Default::default()
                };
                // Headless fixtures must not run host rc/session-restore scripts.
                #[cfg(test)]
                let launch = TerminalLaunch {
                    program: "/bin/zsh".into(),
                    args: vec!["-d".into(), "-f".into(), "-i".into()],
                    ..launch
                };
                if launch.cwd.is_none() {
                    PaneContent::Error(format!("Start terminal {title}: cannot resolve HOME"))
                } else {
                    let pane = cx.weak_entity();
                    let cwd_pane = pane.clone();
                    PaneContent::Terminal(cx.new(|cx| {
                        TerminalView::launch(launch, TerminalConfig::default(), cx)
                            .with_title_callback(move |_, cx, title| {
                                let _ = pane.update(cx, |pane, cx| {
                                    pane.terminal_title = Some(title.to_string());
                                    cx.notify();
                                });
                            })
                            .with_cwd_callback(move |_, cx, cwd| {
                                let _ = cwd_pane.update(cx, |pane, cx| {
                                    pane.terminal_cwd = Some(cwd.to_path_buf());
                                    cx.notify();
                                });
                            })
                    }))
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
            PaneKind::Review(view) => PaneContent::Review(view),
        };
        let terminal_cwd = matches!(content, PaneContent::Terminal(_))
            .then(|| data.terminal_cwd(workspace, cx).or_else(dirs::home_dir))
            .flatten();
        if let PaneContent::Error(error) = &content {
            eprintln!("{error}");
        }
        Self {
            id,
            title,
            content,
            terminal_title: None,
            terminal_cwd,
            workspace,
            data: data.clone(),
            focus: cx.focus_handle(),
            files: matches!(kind, PaneKind::Files).then(|| {
                cx.new(|cx| crate::files_view::FilesView::new(workspace, data.clone(), cx))
            }),
        }
    }
    pub(crate) fn status_view(&self, cx: &App) -> crate::data::GitStatus {
        self.data.status(self.workspace, cx)
    }
    pub(crate) fn tab_title(&self) -> String {
        if matches!(self.content, PaneContent::Terminal(_)) {
            let title = self
                .terminal_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or(&self.title);
            match &self.terminal_cwd {
                Some(cwd) => format!("{title} · {}", cwd.display()),
                None => title.to_string(),
            }
        } else {
            self.title.clone()
        }
    }
}
impl Focusable for Pane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            PaneContent::Terminal(view) => view.read(cx).focus_handle().clone(),
            PaneContent::Chat(view) => view.focus_handle(cx),
            PaneContent::Review(view) => view.focus_handle(cx),
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
        self.tab_title()
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
            PaneContent::Review(view) => div()
                .id("review-pane")
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
                        let data = self.data.clone();
                        let workspace = self.workspace;
                        let selected = file.clone();
                        div()
                            .id(SharedString::from(format!(
                                "git-file-{group}-{}",
                                file.path
                            )))
                            .cursor_pointer()
                            .on_click(move |_, _, cx| {
                                if let Some(owner) = data.local_owner() {
                                    owner.update(cx, |owner, cx| {
                                        owner.open_changed_file(workspace, selected.clone(), cx)
                                    });
                                }
                            })
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
            PaneContent::Files => {
                let folder = self
                    .data
                    .terminal_cwd(self.workspace, cx)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| self.data.status(self.workspace, cx).repository);
                body.overflow_hidden()
                    .child(folder)
                    .child(div().flex_1().min_h_0().children(self.files.clone()))
            }
        }
    }
}
