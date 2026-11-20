//! Fixed context stamp, scrolling sessions and fixed workspace metadata.
//! Feed titles/status from the existing session/chat catalog; this component
//! owns neither sessions nor the new-session picker.
//!
//! Hookup: replace the shell's plain top strip with `render(&input, colors,
//! emit)`. Supply visible context-filtered sessions in host order and preserve
//! its selected ID. NewSession opens ShowTabPicker (or TaskChildAdd for a task),
//! never a terminal directly. Close outcomes enter existing host close policy;
//! no process or workspace is removed by this view. Metadata labels/colours for
//! mic and worktree follow come from the host. GPUI glyph rasterization and
//! native tooltip placement have not been visually certified against Iced.
use crate::data::{SessionId, SessionKind, WorkspaceId};
use crate::workspace_bar::{AttentionKind, Colors, Handler, News, control};
use gitterm_core::settings::WorkspaceColor;
use gpui::{SharedString, div, prelude::*, px, rgb, rgba};

#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    pub kind: SessionKind,
    pub display_title: String,
    pub chat_title: Option<String>,
    /// Durable task-session harness label takes precedence over chat titles.
    pub task_session_label: Option<String>,
    pub attention: Option<AttentionKind>,
    pub running: bool,
    pub pending_requests: usize,
}
#[derive(Clone, Debug)]
pub struct TaskContext {
    pub id: String,
    pub title: String,
    pub issue: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Input {
    pub workspace: Option<WorkspaceId>,
    pub workspace_name: String,
    pub workspace_color: WorkspaceColor,
    pub visible_workspace_count: usize,
    /// Already filtered to workspace or task context, in session order.
    pub sessions: Vec<Session>,
    pub active: Option<SessionId>,
    pub remote: bool,
    pub task: Option<TaskContext>,
    /// Core status::GitStatusState::branch_name, or the task branch.
    pub branch: String,
    pub pulse_bright: bool,
    pub mic: Option<News>,
    pub worktree_follow: Option<News>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    SelectSession(SessionId),
    CloseSession(SessionId),
    /// Open the session picker, or the task Continue/Start picker.
    NewSession {
        workspace: WorkspaceId,
        task: Option<String>,
    },
    CloseWorkspace(WorkspaceId),
    BackToWorkspace,
    TaskOverview,
    ToggleWorktreeFollow,
}

/// Iced's session_tab_label: free-text chat titles are not paths. Its fallback
/// uses a byte-length threshold but truncates only on Unicode boundaries.
pub fn label(
    chat_title: Option<&str>,
    display_title: &str,
    attention: bool,
    remote: bool,
) -> String {
    if let Some(title) = chat_title.map(str::trim).filter(|s| !s.is_empty()) {
        return if title.chars().count() > 20 {
            format!("{}…", title.chars().take(19).collect::<String>().trim_end())
        } else {
            title.to_string()
        };
    }
    let display = if attention {
        display_title.trim_start_matches(['*', '✳']).trim_start()
    } else {
        display_title
    };
    let display = if !remote && (display.starts_with('/') || display.starts_with('~')) {
        std::path::Path::new(display)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| display.to_string())
    } else {
        display.to_string()
    };
    if display.len() > 20 {
        format!("{}…", &display[..display.floor_char_boundary(19)])
    } else {
        display
    }
}
impl Session {
    pub fn label(&self, remote: bool) -> String {
        label(
            if self.task_session_label.is_some() {
                None
            } else {
                self.chat_title.as_deref()
            },
            self.task_session_label
                .as_deref()
                .unwrap_or(&self.display_title),
            self.attention.is_some(),
            remote,
        )
    }
    pub fn icon(&self, active: bool, pulse: bool, c: Colors) -> (&'static str, u32) {
        let needs = if pulse && !active { c.peach } else { c.yellow };
        if self.kind == SessionKind::Chat {
            if self.pending_requests > 0 {
                return ("● ", needs);
            }
            if self.running {
                return ("▶ ", c.green);
            }
        }
        match self.attention {
            Some(AttentionKind::NeedsYou) => ("● ", if pulse { c.peach } else { c.yellow }),
            Some(AttentionKind::Failed) => ("! ", c.red),
            Some(AttentionKind::Review) => ("✓ ", c.green),
            None if self.kind == SessionKind::Chat => ("✦ ", c.muted),
            None if self
                .task_session_label
                .as_deref()
                .unwrap_or(&self.display_title)
                .to_lowercase()
                .contains("claude") =>
            {
                ("✦ ", c.peach)
            }
            None => ("▶ ", c.green),
        }
    }
}
impl Input {
    pub fn add_label(&self) -> &'static str {
        match (&self.task, self.sessions.is_empty()) {
            (Some(_), true) => "Start with ▾",
            (Some(_), false) => "+ Continue ▾",
            _ => "+ Session",
        }
    }
    pub fn new_session(&self) -> Option<Outcome> {
        self.workspace.map(|workspace| Outcome::NewSession {
            workspace,
            task: self.task.as_ref().map(|t| t.id.clone()),
        })
    }
    pub fn close_workspace(&self) -> Option<Outcome> {
        self.workspace
            .filter(|_| self.visible_workspace_count > 1)
            .map(Outcome::CloseWorkspace)
    }
}
pub fn shortcut(position: usize) -> Option<String> {
    (position < 9).then(|| format!(" ⌘{}", position + 1))
}

/// 36px content plus 1px separator, as in Iced. Only sessions scroll.
pub fn render(input: &Input, c: Colors, emit: Handler<Outcome>) -> gpui::Stateful<gpui::Div> {
    let accent = if input.task.is_some() {
        c.blue
    } else {
        c.workspace(input.workspace_color)
    };
    let mut stamp = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.))
        .pl(px(8.))
        .child(div().w(px(3.)).h(px(22.)).rounded(px(2.)).bg(rgb(accent)));
    let (caption, title) = if let Some(task) = &input.task {
        stamp = stamp
            .child(
                control(
                    "pstrip-parent",
                    input.workspace_name.clone(),
                    "Back to workspace sessions",
                    c.muted,
                    c,
                    Outcome::BackToWorkspace,
                    emit.clone(),
                )
                .debug_selector(|| "pstrip-parent".into())
                .text_size(px(12.))
                .px(px(6.)),
            )
            .child(div().text_size(px(12.)).text_color(rgb(c.muted)).child("›"));
        (
            task.issue
                .as_ref()
                .map(|key| format!("TASK · {key}"))
                .unwrap_or_else(|| "TASK".into()),
            task.title.clone(),
        )
    } else {
        ("WORKSPACE".into(), input.workspace_name.clone())
    };
    stamp = stamp.child(
        div()
            .flex()
            .flex_col()
            .px(px(7.))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(accent))
                    .child(caption),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(c.text))
                    .child(title),
            ),
    );
    if input.task.is_some() {
        stamp = stamp.child(
            control(
                "pstrip-overview",
                "Overview",
                "Task overview",
                c.muted,
                c,
                Outcome::TaskOverview,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-overview".into())
            .text_size(px(12.))
            .when(input.active.is_none(), |s| {
                s.bg(rgb(c.base)).text_color(rgb(c.text))
            }),
        );
    }
    let mut tabs = div()
        .id("pstrip-session-scroll")
        .flex()
        .items_center()
        .gap(px(2.))
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .px(px(8.))
        .py(px(4.));
    for (position, session) in input.sessions.iter().enumerate() {
        let active = input.active == Some(session.id);
        let id = session.id;
        let (icon, icon_color) = session.icon(active, input.pulse_bright, c);
        let select = emit.clone();
        let mut tab = div()
            .id(SharedString::from(format!("pstrip-session-{}", id.0)))
            .debug_selector(move || format!("pstrip-session-{}", id.0))
            .role(gpui::Role::Tab)
            .aria_selected(active)
            .aria_label(
                session
                    .task_session_label
                    .as_ref()
                    .or(session.chat_title.as_ref())
                    .unwrap_or(&session.display_title)
                    .clone(),
            )
            .flex()
            .items_center()
            .rounded(px(6.))
            .px(px(10.))
            .py(px(4.))
            .cursor_pointer()
            .when(session.attention.is_none() && !active, |s| {
                s.hover(move |s| s.bg(rgb(c.surface0)))
            })
            .when(active, |s| s.bg(rgb(c.base)))
            .on_click(move |_, window, cx| select(Outcome::SelectSession(id), window, cx))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(icon_color))
                    .child(icon),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(if active { c.text } else { c.subtext0 }))
                    .child(session.label(input.remote)),
            );
        if let Some(reason) = session.attention {
            let color = reason.color(c);
            let alpha = if reason == AttentionKind::NeedsYou && input.pulse_bright {
                0x33
            } else {
                0x1f
            };
            tab = tab
                .bg(rgba((color << 8) | alpha))
                .border_1()
                .border_color(rgba((color << 8) | 0x80));
        }
        if let Some(hint) = shortcut(position) {
            tab = tab.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(c.surface2))
                    .child(hint),
            );
        }
        tabs = tabs.child(
            div().flex().flex_none().items_center().child(tab).child(
                control(
                    format!("pstrip-session-close-{}", id.0),
                    "×",
                    "Close session",
                    c.muted,
                    c,
                    Outcome::CloseSession(id),
                    emit.clone(),
                )
                .text_size(px(14.))
                .px(px(4.))
                .debug_selector(move || format!("pstrip-session-close-{}", id.0)),
            ),
        );
    }
    if let Some(outcome) = input.new_session() {
        tabs = tabs.child(
            control(
                "pstrip-new-session",
                input.add_label(),
                "New session",
                c.muted,
                c,
                outcome,
                emit.clone(),
            )
            .flex_none()
            .debug_selector(|| "pstrip-new-session".into()),
        );
    }
    let mut metadata = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .px(px(10.));
    if let Some(mic) = &input.mic {
        metadata = metadata.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(mic.color))
                .child(mic.label.clone()),
        );
    }
    if input.workspace.is_some() {
        metadata = metadata.child(
            div()
                .text_size(px(12.))
                .text_color(rgb(c.workspace(input.workspace_color)))
                .child(input.workspace_name.clone()),
        );
        if let Some(outcome) = input.close_workspace() {
            metadata = metadata.child(
                control(
                    "pstrip-close-workspace",
                    "×",
                    "Close workspace",
                    c.overlay0,
                    c,
                    outcome,
                    emit.clone(),
                )
                .text_size(px(12.))
                .px(px(4.))
                .py(px(2.))
                .debug_selector(|| "pstrip-close-workspace".into()),
            );
        }
    }
    if let Some(follow) = &input.worktree_follow {
        metadata = metadata.child(
            control(
                "pstrip-worktree",
                follow.label.clone(),
                "Toggle worktree follow",
                follow.color,
                c,
                Outcome::ToggleWorktreeFollow,
                emit,
            )
            .debug_selector(|| "pstrip-worktree".into()),
        );
    } else {
        metadata = metadata.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(c.muted))
                .child(format!("⎇ {}", input.branch)),
        );
    }
    div()
        .id("pstrip-session-strip")
        .debug_selector(|| "pstrip-session-strip".into())
        .whitespace_nowrap()
        .line_height(gpui::relative(1.3))
        .flex()
        .items_center()
        .w_full()
        .h(px(37.))
        .flex_none()
        .font_family("Menlo")
        .bg(rgb(c.base))
        .border_b_1()
        .border_color(rgb(c.surface0))
        .child(stamp)
        .child(tabs)
        .child(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_bar::tests::{click, draw};
    use gpui::{Context, Render, TestAppContext, Window};
    use std::{cell::RefCell, rc::Rc};

    fn session(id: u64) -> Session {
        Session {
            id: SessionId(id),
            kind: SessionKind::Chat,
            display_title: "/tmp/repo".into(),
            chat_title: Some("Fix login".into()),
            task_session_label: None,
            attention: None,
            running: false,
            pending_requests: 0,
        }
    }
    fn input() -> Input {
        Input {
            workspace: Some(WorkspaceId(1)),
            workspace_name: "GitTerm".into(),
            workspace_color: WorkspaceColor::Lavender,
            visible_workspace_count: 2,
            sessions: vec![session(11), session(12)],
            active: Some(SessionId(11)),
            remote: false,
            task: None,
            branch: "v6".into(),
            pulse_bright: false,
            mic: None,
            worktree_follow: None,
        }
    }
    #[test]
    fn titles_match_iced_trimming_paths_attention_and_unicode() {
        assert_eq!(
            label(Some(" /review PRJ-101 "), "repo", false, false),
            "/review PRJ-101"
        );
        assert_eq!(label(Some("   "), "repo", false, false), "repo");
        assert_eq!(label(None, "/tmp/acme/portal", false, false), "portal");
        assert_eq!(label(None, "~/acme/portal", false, false), "portal");
        assert_eq!(
            label(None, "/tmp/acme/portal", false, true),
            "/tmp/acme/portal"
        );
        assert_eq!(label(None, "* ✳ repo", true, false), "✳ repo");
        assert_eq!(label(None, "* repo", true, false), "repo");
        assert_eq!(label(None, "* repo", false, false), "* repo");
        assert_eq!(
            label(Some("Résumé café thème naïve über"), "x", false, false),
            "Résumé café thème n…"
        );
        assert_eq!(
            label(None, "Résumé café thème naïve über", false, false),
            "Résumé café thè…"
        );
        assert_eq!(
            label(None, "a-very-long-terminal-title", false, false),
            "a-very-long-termina…"
        );
        let mut s = session(1);
        s.task_session_label = Some("Codex".into());
        assert_eq!(s.label(false), "Codex");
        assert_eq!(shortcut(0), Some(" ⌘1".into()));
        assert_eq!(shortcut(8), Some(" ⌘9".into()));
        assert_eq!(shortcut(9), None);
    }
    #[test]
    fn live_requests_and_running_override_attention_icons() {
        let c = Colors::new(true);
        let mut s = session(1);
        assert_eq!(s.icon(false, false, c), ("✦ ", c.muted));
        s.attention = Some(AttentionKind::Failed);
        assert_eq!(s.icon(false, false, c), ("! ", c.red));
        s.running = true;
        assert_eq!(s.icon(false, false, c), ("▶ ", c.green));
        s.pending_requests = 1;
        assert_eq!(s.icon(false, true, c), ("● ", c.peach));
        assert_eq!(s.icon(true, true, c), ("● ", c.yellow));
        s.running = false;
        s.pending_requests = 0;
        s.attention = Some(AttentionKind::Review);
        assert_eq!(s.icon(false, false, c), ("✓ ", c.green));
        s.attention = Some(AttentionKind::NeedsYou);
        assert_eq!(s.icon(false, true, c), ("● ", c.peach));
        s.attention = None;
        s.kind = SessionKind::Terminal;
        assert_eq!(s.icon(false, false, c), ("▶ ", c.green));
        s.display_title = "Claude Code".into();
        assert_eq!(s.icon(false, false, c), ("✦ ", c.peach));
    }
    #[test]
    fn add_labels_are_context_aware_and_last_workspace_is_protected() {
        let mut i = input();
        assert_eq!(i.add_label(), "+ Session");
        assert_eq!(
            i.new_session(),
            Some(Outcome::NewSession {
                workspace: WorkspaceId(1),
                task: None
            })
        );
        i.task = Some(TaskContext {
            id: "task-1".into(),
            issue: Some("TRU-274".into()),
            title: "Parity".into(),
        });
        assert_eq!(i.add_label(), "+ Continue ▾");
        i.sessions.clear();
        assert_eq!(i.add_label(), "Start with ▾");
        assert_eq!(
            i.new_session(),
            Some(Outcome::NewSession {
                workspace: WorkspaceId(1),
                task: Some("task-1".into())
            })
        );
        i.visible_workspace_count = 1;
        assert_eq!(i.close_workspace(), None);
        i.workspace = None;
        assert_eq!(i.new_session(), None);
    }
    struct View {
        input: Input,
        events: Rc<RefCell<Vec<Outcome>>>,
    }
    impl Render for View {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let events = self.events.clone();
            render(
                &self.input,
                Colors::new(true),
                Rc::new(move |outcome, _, _| events.borrow_mut().push(outcome)),
            )
        }
    }
    #[gpui::test]
    fn clicks_emit_select_close_picker_and_workspace_close(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let events = Rc::new(RefCell::new(vec![]));
        let (view, cx) = cx.add_window_view(|_, _| View {
            input: input(),
            events: events.clone(),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        for selector in [
            "pstrip-session-12",
            "pstrip-session-close-11",
            "pstrip-new-session",
            "pstrip-close-workspace",
        ] {
            click(cx, selector);
        }
        assert_eq!(
            *events.borrow(),
            vec![
                Outcome::SelectSession(SessionId(12)),
                Outcome::CloseSession(SessionId(11)),
                Outcome::NewSession {
                    workspace: WorkspaceId(1),
                    task: None
                },
                Outcome::CloseWorkspace(WorkspaceId(1))
            ]
        );
        events.borrow_mut().clear();
        view.update(cx, |view, cx| {
            view.input.task = Some(TaskContext {
                id: "task-1".into(),
                issue: Some("TRU-274".into()),
                title: "Parity".into(),
            });
            view.input.visible_workspace_count = 1;
            view.input.worktree_follow = Some(News {
                label: "⎇ v6 · worktree parity".into(),
                color: 0x89b4fa,
            });
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("pstrip-close-workspace").is_none());
        for selector in [
            "pstrip-parent",
            "pstrip-overview",
            "pstrip-worktree",
            "pstrip-new-session",
        ] {
            click(cx, selector);
        }
        assert_eq!(
            *events.borrow(),
            [
                Outcome::BackToWorkspace,
                Outcome::TaskOverview,
                Outcome::ToggleWorktreeFollow,
                Outcome::NewSession {
                    workspace: WorkspaceId(1),
                    task: Some("task-1".into())
                }
            ]
        );
    }
}
