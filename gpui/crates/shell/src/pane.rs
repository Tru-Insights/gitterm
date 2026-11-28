use crate::{
    data::{SessionKind, ShellData, WorkspaceId},
    theme::Palette,
};
use gitterm_gpui_chat::{ChatView, events::parse_fixture};
use gitterm_terminal::{TerminalConfig, TerminalLaunch, TerminalView};
use gpui::*;
use gpui_component::dock::{BasePanel, Panel, PanelEvent, PanelInfo, PanelState};
use std::rc::Rc;

#[path = "git_view.rs"]
pub(crate) mod git_view;

pub const PANEL_NAME: &str = "GitTermShellPane";
#[derive(Clone)]
pub enum PaneKind {
    IcedSession(String),
    Terminal,
    CenterTerminal(std::path::PathBuf),
    BottomTerminal(std::path::PathBuf),
    StoppedTerminal(std::path::PathBuf, bool),
    Chat,
    PreviewChat(Entity<crate::chat::ChatPane>),
    Git,
    Files,
    Content(Entity<crate::content::window::ContentWindow>),
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
    IcedSession(String),
    Terminal(Entity<TerminalView>),
    Chat(Entity<ChatView>),
    PreviewChat(Entity<crate::chat::ChatPane>),
    Git,
    Files,
    Content(Entity<crate::content::window::ContentWindow>),
    Review(Entity<gitterm_gpui_review::ReviewView>),
    Error(String),
}
pub struct Pane {
    pub(crate) session: Option<gitterm_core::session::Target>,
    pub id: String,
    pub title: String,
    pub(crate) notification_chat_title: Option<String>,
    pub content: PaneContent,
    /// Imported DTO owned by this runtime pane, including unmounted native agents.
    pub(crate) imported_tab: Option<gitterm_core::workspace_config::WorkspaceTabConfig>,
    pub(crate) startup_command: Option<String>,
    pub(crate) chat_session_id: Option<String>,
    pub(crate) terminal_title: Option<String>,
    center_terminal: bool,
    pub(crate) exited: bool,
    pub(crate) exit_pending: bool,
    pub(crate) terminal_cwd: Option<std::path::PathBuf>,
    /// Iced's restored tracked cwd, until the local shell reports a new cwd.
    pub(crate) terminal_saved_dir: Option<std::path::PathBuf>,
    workspace: WorkspaceId,
    data: Rc<dyn ShellData>,
    focus: FocusHandle,
    files: Option<Entity<crate::files_view::FilesView>>,
    pub(crate) git: Option<Entity<git_view::GitView>>,
    _terminal_events: Option<Subscription>,
    _quit: Subscription,
    _release: Subscription,
}
impl Pane {
    pub(crate) fn workspace_dir(&self, cx: &App) -> Option<std::path::PathBuf> {
        self.data.terminal_cwd(self.workspace, cx)
    }
    pub fn new(
        id: String,
        title: String,
        kind: PaneKind,
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = matches!(
            kind,
            PaneKind::Terminal | PaneKind::CenterTerminal(_) | PaneKind::BottomTerminal(_)
        )
        .then(crate::sessions::fresh_target);
        let content = match kind.clone() {
            PaneKind::IcedSession(text) => PaneContent::IcedSession(text),
            PaneKind::StoppedTerminal(..) => PaneContent::Error("Terminal stopped".into()),
            PaneKind::Terminal | PaneKind::CenterTerminal(_) | PaneKind::BottomTerminal(_) => {
                let launch = TerminalLaunch {
                    cwd: match &kind {
                        PaneKind::CenterTerminal(cwd) | PaneKind::BottomTerminal(cwd) => {
                            Some(cwd.clone())
                        }
                        _ => data.terminal_cwd(workspace, cx).or_else(dirs::home_dir),
                    },
                    ..Default::default()
                };
                // Headless fixtures must not run host rc/session-restore scripts.
                #[cfg(test)]
                let launch = TerminalLaunch {
                    program: "/bin/zsh".into(),
                    args: vec!["-d".into(), "-f".into(), "-i".into()],
                    ..launch
                };
                let launch = if let Some(host) = cx
                    .try_global::<crate::task_host::GpuiTaskHost>()
                    .map(|h| h.0.clone())
                    .filter(|h| h.read(cx).is_live())
                {
                    let name = data
                        .workspaces(cx)
                        .into_iter()
                        .find(|w| w.id == workspace)
                        .map(|w| w.name)
                        .unwrap_or_default();
                    match host.read(cx).live_terminal_launch(
                        &name,
                        launch.cwd.as_deref().expect("local cwd"),
                        None,
                        session.as_ref().expect("terminal identity"),
                        matches!(kind, PaneKind::BottomTerminal(_)),
                        cx,
                    ) {
                        Ok(launch) => launch,
                        Err(error) => {
                            return Self::new(
                                id,
                                title,
                                PaneKind::IcedSession(format!("Terminal: {error}")),
                                workspace,
                                data,
                                window,
                                cx,
                            );
                        }
                    }
                } else {
                    launch
                };
                let preview = data
                    .local_owner()
                    .and_then(|owner| owner.read(cx).preview());
                if let Err(error) = crate::preview::executable(
                    preview.as_deref(),
                    gitterm_core::preview::Effect::Terminal,
                    launch
                        .cwd
                        .as_deref()
                        .unwrap_or(std::path::Path::new("<terminal>")),
                ) {
                    PaneContent::Error(error.to_string())
                } else if launch.cwd.is_none() {
                    PaneContent::Error(format!("Start terminal {title}: cannot resolve HOME"))
                } else {
                    let pane = cx.weak_entity();
                    let cwd_pane = pane.clone();
                    let exit_pane = pane.clone();
                    let title_target = session.clone();
                    let exit_target = session.clone();
                    let cwd_target = session.clone();
                    PaneContent::Terminal(cx.new(|cx| {
                        TerminalView::launch(launch, TerminalConfig::default(), cx)
                            .with_title_callback(move |_, cx, title| {
                                let _ = pane.update(cx, |pane, cx| {
                                    if pane.session != title_target {
                                        return;
                                    }
                                    pane.terminal_title = Some(title.to_string());
                                    cx.notify();
                                });
                            })
                            .with_exit_callback(move |_, cx| {
                                _ = exit_pane.update(cx, |pane, cx| {
                                    if pane.session != exit_target {
                                        return;
                                    }
                                    pane.mark_exited(cx);
                                    cx.notify();
                                });
                            })
                            .with_cwd_callback(move |_, cx, cwd| {
                                let _ = cwd_pane.update(cx, |pane, cx| {
                                    if pane.session != cwd_target {
                                        return;
                                    }
                                    pane.terminal_cwd = Some(cwd.to_path_buf());
                                    pane.terminal_saved_dir = None;
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
            PaneKind::PreviewChat(view) => PaneContent::PreviewChat(view),
            PaneKind::Git => PaneContent::Git,
            PaneKind::Files => PaneContent::Files,
            PaneKind::Content(view) => PaneContent::Content(view),
            PaneKind::Review(view) => PaneContent::Review(view),
        };
        let terminal_cwd = match &kind {
            PaneKind::CenterTerminal(cwd)
            | PaneKind::BottomTerminal(cwd)
            | PaneKind::StoppedTerminal(cwd, _) => Some(cwd.clone()),
            PaneKind::Terminal => data.terminal_cwd(workspace, cx).or_else(dirs::home_dir),
            _ => None,
        };
        if let PaneContent::Error(error) = &content {
            eprintln!("{error}");
        }
        let terminal_events = if matches!(
            kind,
            PaneKind::CenterTerminal(_) | PaneKind::BottomTerminal(_) | PaneKind::Terminal
        ) {
            if let PaneContent::Terminal(view) = &content {
                Some(cx.observe_in(view, window, |_, view, window, cx| {
                    let view = view.clone();
                    // Callbacks update this pane, so deliver after its observer releases it.
                    window.defer(cx, move |window, cx| {
                        view.update(cx, |view, cx| view.process_events(window, cx));
                    });
                }))
            } else {
                None
            }
        } else {
            None
        };
        Self {
            session: match &kind {
                PaneKind::PreviewChat(view) => Some(view.read(cx).target()),
                _ => session,
            },
            _terminal_events: terminal_events,
            _release: cx.on_release(|pane: &mut Self, cx| pane.revoke_caller(cx)),
            _quit: cx.on_app_quit(|pane: &mut Self, cx| {
                pane.revoke_caller(cx);
                if let Some(barrier) = cx.try_global::<crate::lifetime::Barrier>().cloned()
                    && let PaneContent::Terminal(view) = &pane.content
                {
                    let backend = view.update(cx, |view, _| {
                        std::mem::replace(
                            view.backend_mut(),
                            gitterm_terminal::Backend::new_deferred(Default::default()),
                        )
                    });
                    barrier.register("terminal-pty", move || {
                        drop(backend);
                        Ok(())
                    });
                }
                std::future::ready(())
            }),
            id,
            title,
            content,
            terminal_title: None,
            imported_tab: None,
            startup_command: None,
            chat_session_id: None,
            notification_chat_title: None,
            center_terminal: matches!(
                kind,
                PaneKind::CenterTerminal(_) | PaneKind::StoppedTerminal(_, true)
            ),
            exited: false,
            exit_pending: false,
            terminal_cwd,
            terminal_saved_dir: None,
            workspace,
            data: data.clone(),
            focus: cx.focus_handle(),
            git: matches!(kind, PaneKind::Git)
                .then(|| cx.new(|cx| git_view::GitView::new(workspace, data.clone(), cx))),
            files: matches!(kind, PaneKind::Files).then(|| {
                cx.new(|cx| crate::files_view::FilesView::new(workspace, data.clone(), cx))
            }),
        }
    }
    /// The task session this pane holds: a chat's live target, else the
    /// terminal's (a B10b task terminal).
    pub(crate) fn task_session(&self, cx: &App) -> Option<gitterm_core::session::Target> {
        let target = match &self.content {
            PaneContent::PreviewChat(view) => view.read(cx).target(),
            _ => self.session.clone()?,
        };
        target.task_id.is_some().then_some(target)
    }
    /// Iced's tab attention `HumanInputRequired` on the launched task
    /// terminal this pane holds.
    pub(crate) fn needs_input(&self, cx: &App) -> bool {
        self.session
            .as_ref()
            .and_then(|target| target.task_session_id.as_deref())
            .zip(cx.try_global::<crate::task_host::GpuiTaskHost>())
            .is_some_and(|(session, host)| host.0.read(cx).needs_input(session))
    }
    /// The dock tab's title: Iced's tab label leads with `● ` while its
    /// session needs input.
    pub(crate) fn dock_title(&self, cx: &App) -> String {
        let title = self.tab_title();
        if self.needs_input(cx) {
            format!("● {title}")
        } else {
            title
        }
    }
    /// The folder the pane's session runs in: a chat's working folder, else
    /// the terminal's.
    pub(crate) fn task_folder(&self, cx: &App) -> Option<std::path::PathBuf> {
        match &self.content {
            PaneContent::PreviewChat(view) => Some(view.read(cx).cwd().to_path_buf()),
            _ => self.terminal_cwd.clone(),
        }
    }
    fn revoke_caller(&self, cx: &App) {
        if let Some(target) = &self.session {
            crate::task_host::revoke_caller(&target.session_uid, cx);
            if let Some(session) = &target.task_session_id {
                crate::task_host::revoke_caller(session, cx);
            }
        }
    }
    /// An adopted terminal's child exited: as the exit callback of a pane's
    /// own terminal marks it.
    pub(crate) fn mark_exited(&mut self, cx: &App) {
        self.revoke_caller(cx);
        self.exited = true;
        self.exit_pending = true;
    }
    pub(crate) fn terminal_alive(&self) -> bool {
        matches!(self.content, PaneContent::Terminal(_)) && !self.exited
    }
    pub(crate) fn close_terminal(&mut self, cx: &mut App) {
        self.revoke_caller(cx);
        if let PaneContent::Terminal(view) = &self.content {
            view.update(cx, |view, _| view.backend_mut().cancel());
        }
    }
    /// Whole-session teardown, distinct from core's terminal StopTurn no-op.
    pub(crate) fn stop_target(&mut self, target: &gitterm_core::session::Target, cx: &mut App) {
        if self.session.as_ref() != Some(target) {
            return;
        }
        if let PaneContent::PreviewChat(view) = &self.content {
            view.update(cx, |view, cx| view.close(cx));
        }
        self.close_terminal(cx);
        self.session = None;
        self.content = PaneContent::Error("Terminal stopped".into());
        self._terminal_events = None;
        self.exited = true;
    }
    pub(crate) fn resume_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cwd) = self.terminal_cwd.clone() else {
            return;
        };
        if self.session.is_some() {
            return;
        }
        *self = Self::new(
            self.id.clone(),
            self.title.clone(),
            if self.center_terminal {
                PaneKind::CenterTerminal(cwd)
            } else {
                PaneKind::BottomTerminal(cwd)
            },
            self.workspace,
            self.data.clone(),
            window,
            cx,
        );
    }
    #[cfg(test)]
    pub(crate) fn status_view(&self, cx: &App) -> crate::data::GitStatus {
        self.data.status(self.workspace, cx)
    }
    pub(crate) fn notification_terminal_title(&self) -> Option<&str> {
        self.terminal_title.as_deref()
    }
    pub(crate) fn tab_title(&self) -> String {
        if matches!(self.content, PaneContent::Terminal(_)) {
            let title = self
                .terminal_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or(&self.title);
            if self.center_terminal {
                return if self.exited {
                    format!("{title} · Exited")
                } else {
                    title.to_string()
                };
            }
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
            PaneContent::PreviewChat(view) => view.focus_handle(cx),
            PaneContent::Review(view) => view.focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}
impl EventEmitter<PanelEvent> for Pane {}
impl EventEmitter<crate::sessions::Removed> for Pane {}
impl BasePanel for Pane {
    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = &self.session {
            cx.emit(crate::sessions::Removed(target.clone()));
        }
    }
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
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.dock_title(cx)
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
            PaneContent::PreviewChat(view) => div()
                .id("preview-chat-pane")
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
            PaneContent::Content(view) => div().id("content-pane").size_full().min_h_0().child(
                crate::content::window::ContentWindow::body(view, &self.id, cx),
            ),
            PaneContent::IcedSession(text) => body.child(text.clone()),
            PaneContent::Error(error) => body.text_color(rgb(p.red)).child(error.clone()),
            PaneContent::Git => div()
                .id("git-left-view")
                .size_full()
                .min_h_0()
                .children(self.git.clone()),
            PaneContent::Files => div()
                .id("files-left-view")
                .size_full()
                .min_h_0()
                .children(self.files.clone()),
        }
    }
}

impl crate::Shell {
    /// Mount the retained Git subview when the Work repo route is selected.
    pub(crate) fn git_sidebar(&self, sidebar: Stateful<Div>, cx: &Context<Self>) -> Stateful<Div> {
        let state = self.active(cx);
        sidebar.child(
            div().flex_1().min_h_0().child(
                state.panes[&format!("workspace-{}-git", state.workspace.layout_key)].clone(),
            ),
        )
    }
}
