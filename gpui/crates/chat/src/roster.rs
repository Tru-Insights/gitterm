//! Host-fed projection of Iced's compact worker/task roster.
use crate::composer::elapsed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Queued,
    Running,
    Waiting,
    Done,
    Failed,
}
impl WorkerState {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
    #[cfg(target_os = "macos")]
    fn color(self) -> u32 {
        match self {
            Self::Queued => 0x7f849c,
            Self::Running => 0x89b4fa,
            Self::Waiting => 0xf9e2af,
            Self::Done => 0x94e2d5,
            Self::Failed => 0xf38ba8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RosterEntry {
    pub id: String,
    /// Iced's worker label, e.g. "Worker · Codex" or "Consult · Claude".
    pub name: String,
    pub model: String,
    pub checkout: String,
    pub activity: String,
    pub state: WorkerState,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct RosterRow {
    pub id: String,
    pub name: String,
    pub what: String,
    pub elapsed: String,
    pub state: WorkerState,
}

/// Live rows first, newest first, matching agent_chat.html's rosterRows.
pub fn rows(input: &[RosterEntry], now_ms: u64) -> Vec<RosterRow> {
    let mut input: Vec<_> = input.iter().collect();
    input.sort_by_key(|row| {
        (
            std::cmp::Reverse(row.state.is_live()),
            std::cmp::Reverse(row.started_at_ms),
        )
    });
    input
        .into_iter()
        .map(|row| {
            let mut what = row.state.label().to_string();
            if !row.checkout.is_empty() {
                what.push_str(&format!(" · in {}", row.checkout));
            }
            if !row.activity.is_empty() {
                what.push_str(&format!(" — {}", row.activity));
            }
            RosterRow {
                id: row.id.clone(),
                name: if row.model.is_empty() {
                    row.name.clone()
                } else {
                    format!("{} · {}", row.name, row.model)
                },
                what,
                elapsed: elapsed(
                    row.ended_at_ms
                        .unwrap_or(now_ms)
                        .saturating_sub(row.started_at_ms),
                ),
                state: row.state,
            }
        })
        .collect()
}

/// Navigation belongs to the host; dismissal only hides this chat's roster row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterOutcome {
    Open { id: String },
    Dismiss { id: String },
}

#[cfg(target_os = "macos")]
pub(crate) fn render(
    input: &[RosterEntry],
    hidden: &std::collections::HashSet<String>,
    now_ms: u64,
    cx: &mut gpui::Context<crate::ChatView>,
) -> impl gpui::IntoElement {
    use gpui::{
        InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, div, px, rgb,
    };
    use gpui_component::button::Button;
    let rows: Vec<_> = rows(input, now_ms)
        .into_iter()
        .filter(|row| !hidden.contains(&row.id))
        .collect();
    div()
        .id("chat-roster")
        .max_h(px(98.))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(2.))
        .font_family("Menlo")
        .text_size(px(15. * 12. / 14.))
        .text_color(rgb(0x7f849c))
        .children((!rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .border_t_1()
                .border_color(rgb(0x45475a))
                .pt(px(4.))
                .px(px(6.))
                .pb(px(6.))
                .children(rows.into_iter().map(|row| {
                    let id = row.id.clone();
                    let dismiss_id = row.id.clone();
                    let selector = format!("roster-{}", row.id);
                    // Reuse the existing activity tick; no extra animation task per worker.
                    let opacity = if row.state == WorkerState::Running {
                        0.3 + 0.7 * ((now_ms % 1400) as f32 / 1400. * std::f32::consts::PI).sin()
                    } else {
                        1.
                    };
                    div()
                        .id(gpui::SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(4.))
                        .py(px(1.))
                        .cursor_pointer()
                        .rounded(px(4.))
                        .hover(|style| style.bg(rgb(0x45475a)))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.roster_outcome(RosterOutcome::Open { id: id.clone() }, cx)
                        }))
                        .child(
                            div()
                                .size(px(7.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(rgb(row.state.color()))
                                .opacity(opacity),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(0xbac2de))
                                .child(row.name),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(row.what))
                        .child(div().flex_shrink_0().child(row.elapsed))
                        .child(
                            Button::new(gpui::SharedString::from(format!(
                                "roster-dismiss-{}",
                                row.id
                            )))
                            .debug_selector(move || format!("roster-dismiss-{dismiss_id}"))
                            .label("×")
                            .tooltip("Hide from this list")
                            .accessibility_label("Hide from this list")
                            .h(px(16.))
                            .min_w(px(16.))
                            .px(px(4.))
                            .py_0()
                            .bg(gpui::rgba(0))
                            .border_0()
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    cx.stop_propagation();
                                    view.roster_outcome(
                                        RosterOutcome::Dismiss { id: row.id.clone() },
                                        cx,
                                    );
                                },
                            )),
                        )
                }))
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roster_projects_model_worktree_activity_and_elapsed_from_input() {
        let worker = |id: &str, state, started_at_ms, ended_at_ms| RosterEntry {
            id: id.into(),
            name: "Worker · Codex".into(),
            model: "gpt-6.1-sol".into(),
            checkout: "/tmp/TRU-274".into(),
            activity: "Running Bash".into(),
            state,
            started_at_ms,
            ended_at_ms,
        };
        let input = vec![
            worker("done", WorkerState::Done, 10_000, Some(12_000)),
            worker("old", WorkerState::Running, 1_000, None),
            worker("new", WorkerState::Running, 2_000, None),
        ];
        let rows = rows(&input, 187_000);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["new", "old", "done"]
        );
        assert_eq!(rows[0].name, "Worker · Codex · gpt-6.1-sol");
        assert_eq!(rows[0].what, "running · in /tmp/TRU-274 — Running Bash");
        assert_eq!(rows[1].elapsed, "3m 6s");
        assert_eq!(rows[2].elapsed, "2.0s");
        assert!(super::rows(&[], 0).is_empty());
        assert_eq!(elapsed(0), "0.0s");
    }
}
