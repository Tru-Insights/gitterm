//! The 36px Iced workspace rail plus its 1px right border. All urgency facts
//! are supplied by the host; no timers, task store or navigation lives here.
//!
//! Hookup: render this at the full-height left edge, beside the column containing
//! strip/content/bar. Feed the same catalog/active/repos_on_bar as the bar and
//! the host's attention rows, running targets and actually frontmost target.
//! Jump selects the workspace then opens the returned tab/task via existing
//! host navigation. SelectWorkspace only selects; ToggleUsage uses the existing
//! Usage route. The host advances pulse_bright. Native GPUI tooltip placement
//! differs from Iced's explicitly right-anchored rail cards.
use std::time::Duration;

use crate::data::WorkspaceId;
use crate::workspace_bar::{Colors, Handler, News, Workspace, dot};
use gitterm_core::workspace;
use gpui::{SharedString, div, prelude::*, px, rgb};
use gpui_component::tooltip::Tooltip;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Tab(u64),
    Task(String),
}
#[derive(Clone, Debug)]
pub struct AttentionRow {
    pub workspace: WorkspaceId,
    pub target: Target,
    pub title: String,
    /// Iced scale: 0 input, 1 failure, 2 interrupted, 3 review.
    pub priority: u8,
    pub icon: String,
    pub label: String,
    pub waiting: Duration,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    NeedsYou,
    Failed,
    Interrupted,
    Review,
    Running,
}
impl Urgency {
    pub fn from_priority(priority: u8) -> Self {
        match priority {
            0 => Self::NeedsYou,
            1 => Self::Failed,
            2 => Self::Interrupted,
            _ => Self::Review,
        }
    }
    pub fn color(self, c: Colors, pulse: bool) -> u32 {
        match self {
            Self::NeedsYou if pulse => c.peach,
            Self::NeedsYou => c.yellow,
            Self::Failed => c.red,
            Self::Interrupted => c.peach,
            Self::Review => c.green,
            Self::Running => c.blue,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Input {
    /// Same host catalog projection as the workspace bar, in catalog order.
    pub workspaces: Vec<Workspace>,
    pub active: Option<WorkspaceId>,
    pub repos_on_bar: bool,
    pub attention: Vec<AttentionRow>,
    /// Excluded from background attention and running indicators, like Iced.
    pub frontmost: Option<(WorkspaceId, Target)>,
    pub running: Vec<(WorkspaceId, Target)>,
    pub pulse_bright: bool,
    pub usage_open: bool,
    pub usage_hint: String,
    pub usage_badge: Option<News>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    SelectWorkspace(WorkspaceId),
    Jump {
        workspace: WorkspaceId,
        target: Target,
    },
    ToggleUsage,
}
#[derive(Clone, Debug)]
pub struct Dot {
    pub workspace: WorkspaceId,
    pub name: String,
    pub rows: Vec<AttentionRow>,
    pub running: bool,
    pub urgency: Option<Urgency>,
    pub width: f32,
    pub height: f32,
    pub color: u32,
}
impl Dot {
    pub fn click(&self, option: bool) -> Outcome {
        if !option && let Some(row) = self.rows.first() {
            return Outcome::Jump {
                workspace: self.workspace,
                target: row.target.clone(),
            };
        }
        Outcome::SelectWorkspace(self.workspace)
    }
}
impl Input {
    pub fn dots(&self, c: Colors) -> Vec<Dot> {
        let chips: Vec<_> = self
            .workspaces
            .iter()
            .map(|w| w.placement.clone())
            .collect();
        let active = self
            .workspaces
            .iter()
            .position(|w| Some(w.id) == self.active)
            .unwrap_or(usize::MAX);
        workspace::visible_order(&workspace::bar_layout(&chips, active, self.repos_on_bar))
            .into_iter()
            .map(|index| {
                let w = &self.workspaces[index];
                let active = Some(w.id) == self.active;
                let frontmost = self
                    .frontmost
                    .as_ref()
                    .filter(|(id, _)| *id == w.id)
                    .map(|(_, t)| t);
                let mut rows: Vec<_> = self
                    .attention
                    .iter()
                    .filter(|r| r.workspace == w.id && Some(&r.target) != frontmost)
                    .cloned()
                    .collect();
                rows.sort_by(|a, b| {
                    a.priority
                        .cmp(&b.priority)
                        .then(b.waiting.cmp(&a.waiting))
                        .then(a.target.cmp(&b.target))
                });
                let running = self
                    .running
                    .iter()
                    .any(|(id, target)| *id == w.id && Some(target) != frontmost);
                let urgency = rows
                    .first()
                    .map(|r| Urgency::from_priority(r.priority))
                    .or_else(|| running.then_some(Urgency::Running));
                let pending = urgency.filter(|u| *u != Urgency::Running);
                let (width, height) = if active {
                    (4., 18.)
                } else if pending.is_some() {
                    (6., 6.)
                } else {
                    (4., 4.)
                };
                let color = if let Some(u) = pending {
                    u.color(c, self.pulse_bright)
                } else if running && !active {
                    c.blue
                } else if active {
                    c.workspace(w.color)
                } else {
                    c.surface2
                };
                Dot {
                    workspace: w.id,
                    name: w.name.clone(),
                    rows,
                    running,
                    urgency,
                    width,
                    height,
                    color,
                }
            })
            .collect()
    }
}
/// Durable task attention uses Iced's reason/age presentation without opening
/// a store or discovering a process. The caller resolves workspace identity.
pub(crate) fn task_rows<'a>(
    workspace: WorkspaceId,
    tasks: impl Iterator<Item = &'a gitterm_core::tasks::TaskRecord>,
    now: std::time::SystemTime,
) -> Vec<AttentionRow> {
    use gitterm_core::tasks::{TaskAttentionReason as R, TaskLifecycle};
    use objc2_foundation::{NSISO8601DateFormatOptions as O, NSISO8601DateFormatter, NSString};
    // Foundation is already a shell dependency; keep the formatter local to
    // this projection instead of introducing another date dependency or cache.
    let formatter = NSISO8601DateFormatter::new();
    tasks
        .filter(|task| task.lifecycle != TaskLifecycle::Archived)
        .filter_map(|task| {
            let (priority, icon, label) = match task.attention.reason? {
                R::RequiresInput => (0, "●", "Input or approval needed"),
                R::RemoteUnavailable => (0, "●", "Remote unavailable"),
                R::ExecutionFailed => (1, "!", "Task failed"),
                R::Interrupted => (2, "○", "Interrupted · resume"),
                R::CompletedUnread | R::ReadyForReview => (3, "✓", "Ready to review"),
            };
            let raised = task.attention.since.as_deref().unwrap_or(&task.updated_at);
            let options = O::WithInternetDateTime
                | if raised.contains('.') {
                    O::WithFractionalSeconds
                } else {
                    O::empty()
                };
            formatter.setFormatOptions(options);
            // Iced also gives malformed/future timestamps age zero; saved records
            // predating `since` use updated_at above.
            let waiting = formatter
                .dateFromString(&NSString::from_str(raised))
                .and_then(|date| {
                    now.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .and_then(|now| {
                            Duration::try_from_secs_f64(
                                now.as_secs_f64() - date.timeIntervalSince1970(),
                            )
                            .ok()
                        })
                })
                .unwrap_or_default();
            Some(AttentionRow {
                workspace,
                target: Target::Task(task.task_id.clone()),
                title: task.issue.as_ref().map_or_else(
                    || task.title.clone(),
                    |issue| format!("{} {}", issue.key, task.title),
                ),
                priority,
                icon: icon.into(),
                label: label.into(),
                waiting,
            })
        })
        .collect()
}

pub fn age(waiting: Duration) -> String {
    let secs = waiting.as_secs();
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}
fn tooltip(model: &Dot, c: Colors, pulse: bool) -> gpui::Div {
    let mut card = div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .font_family(crate::theme::LABEL_FONT)
        .line_height(gpui::relative(crate::theme::LABEL_LINE_HEIGHT))
        .text_size(px(11.))
        .text_color(rgb(c.text))
        .child(model.name.clone());
    for row in model.rows.iter().take(5) {
        let title = if row.title.chars().count() > 32 {
            format!("{}…", &row.title[..row.title.floor_char_boundary(31)])
        } else {
            row.title.clone()
        };
        let color = Urgency::from_priority(row.priority).color(c, pulse);
        card = card.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().text_color(rgb(color)).child(row.icon.clone()))
                .child(title)
                .child(div().text_color(rgb(color)).child(row.label.clone()))
                .child(div().text_color(rgb(c.muted)).child(age(row.waiting))),
        );
    }
    if model.rows.len() > 5 {
        card = card.child(
            div()
                .text_color(rgb(c.muted))
                .child(format!("+{} more", model.rows.len() - 5)),
        );
    }
    if model.rows.is_empty() && model.running {
        card = card.child(div().text_color(rgb(c.blue)).child("▶ Turn running"));
    }
    card
}

pub fn render(input: &Input, c: Colors, emit: Handler<Outcome>) -> gpui::Stateful<gpui::Div> {
    let mut dots = div().flex().flex_col().items_center().gap(px(8.));
    for model in input.dots(c) {
        let id = model.workspace;
        let on_click = emit.clone();
        let glyph = dot(model.color, model.width, model.height);
        let hint = model.clone();
        let pulse = input.pulse_bright;
        dots = dots.child(
            div()
                .id(SharedString::from(format!("pstrip-dot-{}", id.0)))
                .debug_selector(move || format!("pstrip-dot-{}", id.0))
                .role(gpui::Role::Button)
                .aria_label(model.name.clone())
                .aria_description(
                    "Jump to the most urgent waiting row; Option-click selects this workspace",
                )
                .flex()
                .items_center()
                .justify_center()
                .w(px(35.))
                .py(px(4.))
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(c.surface0)))
                .tooltip(move |window, cx| {
                    let hint = hint.clone();
                    Tooltip::element(move |_, _| tooltip(&hint, c, pulse)).build(window, cx)
                })
                .on_click(move |event, window, cx| {
                    on_click(model.click(event.modifiers().alt), window, cx)
                })
                .child(glyph),
        );
    }
    let color = if input.usage_open { c.blue } else { c.subtext0 };
    let chart = div().flex().items_end().gap(px(2.)).children(
        [6., 12., 9.].map(|height| div().w(px(3.)).h(px(height)).rounded(px(1.)).bg(rgb(color))),
    );
    let mut usage = div()
        .id("pstrip-usage")
        .role(gpui::Role::Button)
        .aria_label("Usage")
        .aria_expanded(input.usage_open)
        .debug_selector(|| "pstrip-usage".into())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .w(px(36.))
        .py(px(8.))
        .cursor_pointer()
        .when(input.usage_open, |s| s.bg(rgb(c.surface0)))
        .hover(move |s| s.bg(rgb(c.surface0)))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(16.))
                .child(chart),
        );
    if let Some(badge) = &input.usage_badge {
        usage = usage.child(
            div()
                .font_family(crate::theme::LABEL_FONT)
                .text_size(px(9.))
                .text_color(rgb(badge.color))
                .child(badge.label.clone()),
        );
    }
    let hint = input.usage_hint.clone();
    usage = usage
        .tooltip(move |window, cx| {
            Tooltip::new(hint.clone())
                .font_family(crate::theme::LABEL_FONT)
                .build(window, cx)
        })
        .on_click(move |_, window, cx| emit(Outcome::ToggleUsage, window, cx));
    div()
        .id("pstrip-spine")
        .debug_selector(|| "pstrip-spine".into())
        .flex()
        .flex_col()
        .items_center()
        .w(px(37.))
        .h_full()
        .flex_none()
        .bg(rgb(c.crust))
        .border_r_1()
        .border_color(rgb(c.surface0))
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .overflow_y_hidden()
                .child(dots),
        )
        .child(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_bar::tests::{click, draw};
    use gpui::{Context, Render, TestAppContext, Window};
    use std::{cell::RefCell, rc::Rc};

    fn row(workspace: u64, target: Target, priority: u8, seconds: u64) -> AttentionRow {
        AttentionRow {
            workspace: WorkspaceId(workspace),
            target,
            title: "Fix login".into(),
            priority,
            icon: "●".into(),
            label: "Input or approval needed".into(),
            waiting: Duration::from_secs(seconds),
        }
    }
    fn input() -> Input {
        let bar = crate::workspace_bar::tests::input();
        Input {
            workspaces: bar.workspaces,
            active: bar.active,
            repos_on_bar: true,
            attention: vec![
                row(1, Target::Tab(11), 3, 100),
                row(1, Target::Task("urgent".into()), 0, 5),
                row(1, Target::Tab(12), 0, 20),
            ],
            frontmost: None,
            running: vec![],
            pulse_bright: false,
            usage_open: false,
            usage_hint: "Usage".into(),
            usage_badge: None,
        }
    }
    #[test]
    fn core_order_and_stable_urgency_age_target_order() {
        let i = input();
        let c = Colors::new(true);
        let dots = i.dots(c);
        assert_eq!(
            dots.iter().map(|d| d.workspace).collect::<Vec<_>>(),
            [WorkspaceId(2), WorkspaceId(1)]
        );
        let d = &dots[1];
        assert_eq!(
            d.rows.iter().map(|r| r.target.clone()).collect::<Vec<_>>(),
            [
                Target::Tab(12),
                Target::Task("urgent".into()),
                Target::Tab(11)
            ]
        );
        assert_eq!(
            d.click(false),
            Outcome::Jump {
                workspace: WorkspaceId(1),
                target: Target::Tab(12)
            }
        );
        assert_eq!(d.click(true), Outcome::SelectWorkspace(WorkspaceId(1)));
        assert_eq!((d.width, d.height, d.color), (6., 6., c.yellow));
        let mut i = i;
        i.attention.push(row(1, Target::Tab(10), 0, 20));
        assert_eq!(i.dots(c)[1].rows[0].target, Target::Tab(10));
        i.active = Some(WorkspaceId(3));
        assert_eq!(
            i.dots(c).iter().map(|d| d.workspace).collect::<Vec<_>>(),
            [WorkspaceId(2), WorkspaceId(1), WorkspaceId(3)]
        );
    }
    #[test]
    fn frontmost_is_excluded_and_active_running_keeps_workspace_color() {
        let mut i = input();
        let c = Colors::new(false);
        i.attention = vec![row(2, Target::Tab(21), 0, 10)];
        i.frontmost = Some((WorkspaceId(2), Target::Tab(21)));
        i.running = vec![(WorkspaceId(2), Target::Tab(21))];
        let active = i.dots(c).remove(0);
        assert!(active.rows.is_empty());
        assert!(!active.running);
        assert_eq!(active.urgency, None);
        assert_eq!((active.width, active.height), (4., 18.));
        assert_eq!(active.color, c.workspace(i.workspaces[1].color));
        assert_eq!(
            active.click(false),
            Outcome::SelectWorkspace(WorkspaceId(2))
        );
        i.frontmost = None;
        i.attention.clear();
        assert_eq!(i.dots(c)[0].urgency, Some(Urgency::Running));
        assert_eq!(i.dots(c)[0].color, active.color);
        i.running.push((WorkspaceId(1), Target::Tab(11)));
        assert_eq!(i.dots(c)[1].color, c.blue);
        for (priority, urgency, color) in [
            (0, Urgency::NeedsYou, c.yellow),
            (1, Urgency::Failed, c.red),
            (2, Urgency::Interrupted, c.peach),
            (3, Urgency::Review, c.green),
        ] {
            i.attention = vec![row(1, Target::Tab(11), priority, 1)];
            let dot = i.dots(c).remove(1);
            assert_eq!(dot.urgency, Some(urgency));
            assert_eq!(dot.color, color);
        }
    }
    #[test]
    fn idle_active_urgency_and_pulse_states_match_iced_dot_geometry() {
        let mut i = input();
        let c = Colors::new(true);
        i.attention.clear();
        let dots = i.dots(c);
        assert_eq!(
            (dots[0].width, dots[0].height, dots[0].color),
            (4., 18., c.workspace(i.workspaces[1].color))
        );
        assert_eq!(
            (dots[1].width, dots[1].height, dots[1].color),
            (4., 4., c.surface2)
        );
        i.attention = vec![row(1, Target::Tab(11), 0, 1)];
        i.pulse_bright = true;
        let dot = i.dots(c).remove(1);
        assert_eq!((dot.width, dot.height, dot.color), (6., 6., c.peach));
        i.active = Some(WorkspaceId(1));
        let dot = i.dots(c).remove(1);
        assert_eq!((dot.width, dot.height, dot.color), (4., 18., c.peach));
    }

    #[test]
    fn durable_task_rows_keep_iced_reason_age_and_identity() {
        use gitterm_core::tasks::{TaskAttentionReason as R, TaskLifecycle};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/stage45/tasks/old.tasks.json"
        ))
        .unwrap();
        let mut task: gitterm_core::tasks::TaskRecord =
            serde_json::from_value(fixture["tasks"][0].clone()).unwrap();
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_120);
        task.attention.since = Some("2023-11-14T22:13:20.000+00:00".into());
        task.lifecycle = TaskLifecycle::Ready;
        for (reason, priority, icon, label) in [
            (R::RequiresInput, 0, "●", "Input or approval needed"),
            (R::RemoteUnavailable, 0, "●", "Remote unavailable"),
            (R::ExecutionFailed, 1, "!", "Task failed"),
            (R::Interrupted, 2, "○", "Interrupted · resume"),
            (R::CompletedUnread, 3, "✓", "Ready to review"),
            (R::ReadyForReview, 3, "✓", "Ready to review"),
        ] {
            task.attention.reason = Some(reason);
            let rows = task_rows(WorkspaceId(2), std::iter::once(&task), now);
            let row = &rows[0];
            assert_eq!(
                (row.priority, row.icon.as_str(), row.label.as_str()),
                (priority, icon, label)
            );
            assert_eq!(row.waiting, Duration::from_secs(120));
            assert_eq!(row.workspace, WorkspaceId(2));
            assert_eq!(row.target, Target::Task(task.task_id.clone()));
        }
        task.attention.since = None;
        task.updated_at = "2023-11-14T22:13:20Z".into();
        assert_eq!(
            task_rows(WorkspaceId(2), std::iter::once(&task), now)[0].waiting,
            Duration::from_secs(120)
        );
        task.attention.since = Some("invalid".into());
        assert_eq!(
            task_rows(WorkspaceId(2), std::iter::once(&task), now)[0].waiting,
            Duration::ZERO
        );
        task.lifecycle = TaskLifecycle::Archived;
        assert!(task_rows(WorkspaceId(2), std::iter::once(&task), now).is_empty());
        task.lifecycle = TaskLifecycle::Ready;
        task.attention.reason = None;
        assert!(task_rows(WorkspaceId(2), std::iter::once(&task), now).is_empty());
    }

    #[test]
    fn tooltip_age_boundaries_match_iced() {
        for (secs, expected) in [
            (0, "now"),
            (59, "now"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (86399, "23h"),
            (86400, "1d"),
        ] {
            assert_eq!(age(Duration::from_secs(secs)), expected);
        }
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
    fn dot_click_jumps_option_click_selects_and_usage_toggles(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let events = Rc::new(RefCell::new(vec![]));
        let (_, cx) = cx.add_window_view(|_, _| View {
            input: input(),
            events: events.clone(),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        click(cx, "pstrip-dot-1");
        let bounds = cx.debug_bounds("pstrip-dot-1").unwrap();
        cx.simulate_click(
            bounds.center(),
            gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        draw(cx);
        click(cx, "pstrip-dot-2");
        click(cx, "pstrip-usage");
        assert_eq!(
            *events.borrow(),
            [
                Outcome::Jump {
                    workspace: WorkspaceId(1),
                    target: Target::Tab(12)
                },
                Outcome::SelectWorkspace(WorkspaceId(1)),
                Outcome::SelectWorkspace(WorkspaceId(2)),
                Outcome::ToggleUsage
            ]
        );
    }
}
