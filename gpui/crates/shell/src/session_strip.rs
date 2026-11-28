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
    pub worktree_follow: Option<WorktreeFollow>,
}
/// Derived checkout facts are supplied by the owning chat; never infer them
/// from a saved tab's startup directory or the last checkout it touched.
#[derive(Clone, Debug)]
pub struct WorktreeFollow {
    pub following: bool,
    pub path: std::path::PathBuf,
    pub label: Option<String>,
    pub branch: Option<String>,
}
impl WorktreeFollow {
    pub fn text(&self, workspace_branch: &str) -> String {
        let label = self
            .label
            .clone()
            .unwrap_or_else(|| self.path.display().to_string());
        if self.following {
            format!(
                "⎇ {} · worktree {label}",
                self.branch.as_deref().unwrap_or("detached")
            )
        } else {
            format!("⎇ {workspace_branch} · workspace root · follow worktree {label}")
        }
    }
}

pub(crate) fn attention(
    task: Option<&gitterm_core::tasks::TaskAttention>,
    workers: &[gitterm_core::tasks::Delegation],
    session_uid: Option<&str>,
) -> Option<AttentionKind> {
    use gitterm_core::tasks::{DelegationStatus, TaskAttentionReason};
    let task = task.and_then(|task| match task.reason {
        Some(TaskAttentionReason::RequiresInput | TaskAttentionReason::RemoteUnavailable) => {
            Some(AttentionKind::NeedsYou)
        }
        Some(TaskAttentionReason::ExecutionFailed) => Some(AttentionKind::Failed),
        Some(TaskAttentionReason::CompletedUnread | TaskAttentionReason::ReadyForReview) => {
            Some(AttentionKind::Review)
        }
        None if task.unread => Some(AttentionKind::Review),
        _ => None,
    });
    workers
        .iter()
        .filter(|worker| {
            worker.dismissed_at.is_none() && Some(worker.parent.session_uid.as_str()) == session_uid
        })
        .filter_map(|worker| match worker.status {
            DelegationStatus::Blocked => Some(AttentionKind::NeedsYou),
            DelegationStatus::Failed { .. } => Some(AttentionKind::Failed),
            DelegationStatus::Completed => Some(AttentionKind::Review),
            _ => None,
        })
        .chain(task)
        .min_by_key(|kind| match kind {
            AttentionKind::NeedsYou => 0,
            AttentionKind::Failed => 1,
            AttentionKind::Review => 2,
        })
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
        .max_w(gpui::relative(0.25))
        .overflow_hidden()
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
        .debug_selector(|| "pstrip-session-scroll".into())
        .flex()
        .items_center()
        .gap(px(2.))
        .flex_initial()
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
                    .font_family("Menlo")
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
                    .id(SharedString::from(format!(
                        "pstrip-session-shortcut-{}",
                        id.0
                    )))
                    .debug_selector(move || format!("pstrip-session-shortcut-{}", id.0))
                    .text_size(px(10.))
                    .font_family("Menlo")
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
    // Reserve the add control's intrinsic width before shrinking the tab
    // viewport. Putting it inside that viewport clips it behind metadata.
    let new_session = input.new_session().map(|outcome| {
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
        .debug_selector(|| "pstrip-new-session".into())
    });
    let mut metadata = div()
        .id("pstrip-session-metadata")
        .debug_selector(|| "pstrip-session-metadata".into())
        .flex()
        .flex_none()
        .max_w(gpui::relative(0.35))
        .overflow_hidden()
        .items_center()
        .gap(px(4.))
        .px(px(10.));
    if let Some(mic) = &input.mic {
        metadata = metadata.child(
            div()
                .debug_selector(|| "pstrip-mic".into())
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
                follow.text(&input.branch),
                follow.text(&input.branch),
                if follow.following { c.blue } else { c.muted },
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
        .line_height(gpui::relative(crate::theme::LABEL_LINE_HEIGHT))
        .flex()
        .items_center()
        .w_full()
        .h(px(37.))
        .flex_none()
        .font_family(crate::theme::LABEL_FONT)
        .bg(rgb(c.base))
        .border_b_1()
        .border_color(rgb(c.surface0))
        .child(stamp)
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .child(tabs)
                .children(new_session),
        )
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
    fn worktree_follow_text_matches_following_root_and_detached_states() {
        let mut follow = WorktreeFollow {
            following: true,
            path: "/tmp/worktree".into(),
            label: Some("parity".into()),
            branch: Some("feature".into()),
        };
        assert_eq!(follow.text("v6"), "⎇ feature · worktree parity");
        follow.following = false;
        assert_eq!(
            follow.text("v6"),
            "⎇ v6 · workspace root · follow worktree parity"
        );
        follow.following = true;
        follow.branch = None;
        follow.label = None;
        assert_eq!(follow.text("v6"), "⎇ detached · worktree /tmp/worktree");
    }

    #[test]
    fn durable_task_attention_matches_iced_tab_states() {
        use gitterm_core::tasks::{TaskAttention, TaskAttentionReason as R};
        for (reason, expected) in [
            (R::RequiresInput, Some(AttentionKind::NeedsYou)),
            (R::RemoteUnavailable, Some(AttentionKind::NeedsYou)),
            (R::ExecutionFailed, Some(AttentionKind::Failed)),
            (R::CompletedUnread, Some(AttentionKind::Review)),
            (R::ReadyForReview, Some(AttentionKind::Review)),
            (R::Interrupted, None),
        ] {
            assert_eq!(
                attention(
                    Some(&TaskAttention {
                        reason: Some(reason),
                        ..Default::default()
                    }),
                    &[],
                    None
                ),
                expected
            );
        }
        assert_eq!(attention(None, &[], None), None);
        assert_eq!(
            attention(
                Some(&TaskAttention {
                    unread: true,
                    ..Default::default()
                }),
                &[],
                None
            ),
            Some(AttentionKind::Review)
        );
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
    fn new_session_keeps_its_full_width_when_tabs_overflow(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_, _| View {
            input: input(),
            events: Rc::new(RefCell::new(vec![])),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        view.update(cx, |view, cx| {
            view.input.sessions.clear();
            cx.notify();
        });
        draw(cx);
        let full_width = cx.debug_bounds("pstrip-new-session").unwrap().size.width;
        for width in [320., 640., 1024., 1400.] {
            for count in [1, 4, 30] {
                view.update(cx, |view, cx| {
                    view.input.sessions = (0..count)
                        .map(|id| {
                            let mut session = session(id);
                            session.chat_title =
                                Some("A long conversation title that truncates".into());
                            session
                        })
                        .collect();
                    view.input.workspace_name = if count == 30 {
                        "A very long workspace name that must yield to the session control".into()
                    } else {
                        "GitTerm".into()
                    };
                    cx.notify();
                });
                cx.simulate_resize(gpui::size(px(width), px(800.)));
                draw(cx);
                let add = cx.debug_bounds("pstrip-new-session").unwrap();
                let metadata = cx.debug_bounds("pstrip-session-metadata").unwrap();
                let strip = cx.debug_bounds("pstrip-session-strip").unwrap();
                let tabs = cx.debug_bounds("pstrip-session-scroll").unwrap();
                assert_eq!(add.size.width, full_width, "{count} tabs at {width}px");
                assert!(tabs.right() <= add.left());
                assert!(add.right() <= metadata.left());
                assert!(add.left() >= strip.left() && add.right() <= strip.right());
            }
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
            view.input.worktree_follow = Some(WorktreeFollow {
                following: true,
                path: "/tmp/parity".into(),
                label: Some("parity".into()),
                branch: Some("v6".into()),
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

    #[gpui::test]
    fn ordinary_session_tab_keeps_iced_shortcut_and_close_without_attention(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_, _| View {
            input: input(),
            events: Rc::new(RefCell::new(vec![])),
        });
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        draw(cx);
        view.read_with(cx, |view, _| {
            let active = view.input.sessions[0]
                .icon(true, false, Colors::new(true))
                .0;
            assert_eq!(active, "✦ ");
            assert_eq!(view.input.sessions[0].label(false), "Fix login");
        });
        assert!(cx.debug_bounds("pstrip-session-11").is_some());
        assert!(cx.debug_bounds("pstrip-session-shortcut-11").is_some());
        assert!(cx.debug_bounds("pstrip-session-close-11").is_some());
    }
}
