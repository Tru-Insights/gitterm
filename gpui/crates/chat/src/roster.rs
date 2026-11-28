//! Host-fed projection of Iced's compact worker/task roster.
use crate::composer::elapsed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Queued,
    Running,
    Waiting,
    /// Iced's `blocked`: reported it cannot go on; not live, so it can be hidden.
    Blocked,
    Done,
    Failed,
}
impl WorkerState {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
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
    pub started_at_ms: Option<u64>,
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

/// Preserve the host's roster order; Iced supplies rows in its display order.
pub fn rows(input: &[RosterEntry], now_ms: u64) -> Vec<RosterRow> {
    input
        .iter()
        .map(|row| {
            let what = if row.checkout.is_empty() {
                String::new()
            } else {
                format!("· in {}", row.checkout)
            };
            let duration = match (row.state.is_live(), row.started_at_ms) {
                (true, Some(started_at_ms)) => Some(now_ms.saturating_sub(started_at_ms)),
                (false, Some(started_at_ms)) => row
                    .ended_at_ms
                    .map(|ended_at_ms| ended_at_ms.saturating_sub(started_at_ms)),
                (_, None) => None,
            };
            RosterRow {
                id: row.id.clone(),
                name: if row.model.is_empty() || row.name.ends_with(&format!(" · {}", row.model)) {
                    row.name.clone()
                } else {
                    format!("{} · {}", row.name, row.model)
                },
                what,
                elapsed: duration.map_or_else(String::new, elapsed),
                state: row.state,
            }
        })
        .collect()
}

/// Open reveals a card locally. Only explicit worker/file actions navigate the host;
/// dismissal hides this chat's roster row without changing the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterOutcome {
    Open {
        id: String,
    },
    OpenWorker {
        id: String,
    },
    OpenFile {
        path: std::path::PathBuf,
        review: bool,
        line: Option<usize>,
    },
    Dismiss {
        id: String,
    },
}

#[cfg(target_os = "macos")]
pub(crate) fn render(
    input: &[RosterEntry],
    hidden: &std::collections::HashSet<String>,
    now_ms: u64,
    appearance: crate::ChatAppearance,
    cx: &mut gpui::Context<crate::ChatView>,
) -> impl gpui::IntoElement {
    use gpui::{
        InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, div, px, rgb,
    };
    use gpui_component::button::Button;
    let palette = appearance.palette();
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
        .text_size(px(appearance.size(12.)))
        .text_color(rgb(palette.muted))
        .children((!rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .border_t_1()
                .border_color(rgb(palette.border))
                .pt(px(4.))
                .px(px(6.))
                .pb(px(6.))
                .children(rows.into_iter().map(|row| {
                    let id = row.id.clone();
                    let dismiss_id = row.id.clone();
                    let show_dismiss = shows_dismiss(row.state);
                    let selector = format!("roster-{}", row.id);
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
                        .hover(|style| style.bg(rgb(palette.border)))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.roster_outcome(RosterOutcome::Open { id: id.clone() }, window, cx)
                        }))
                        .child(
                            div()
                                .id(gpui::SharedString::from(format!("roster-dot-{}", row.id)))
                                .debug_selector({
                                    let id = row.id.clone();
                                    move || format!("roster-dot-{id}")
                                })
                                .size(px(7.))
                                .rounded_full()
                                .flex_shrink_0()
                                .bg(rgb(match row.state {
                                    WorkerState::Running => palette.accent,
                                    WorkerState::Queued => palette.muted,
                                    WorkerState::Waiting | WorkerState::Blocked => palette.warn,
                                    WorkerState::Done => palette.result,
                                    WorkerState::Failed => palette.err,
                                })),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .whitespace_nowrap()
                                .text_color(rgb(palette.fg))
                                .child(row.name),
                        )
                        // Iced's `.ro-what` gives checkout the flexible clipped area.
                        .child(
                            div()
                                .debug_selector({
                                    let id = row.id.clone();
                                    move || format!("roster-what-{id}")
                                })
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(row.what),
                        )
                        .child(
                            div()
                                .debug_selector({
                                    let id = row.id.clone();
                                    move || format!("roster-meta-{id}")
                                })
                                .whitespace_nowrap()
                                .child(row.elapsed),
                        )
                        .children(show_dismiss.then(|| {
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
                                move |view, _, window, cx| {
                                    cx.stop_propagation();
                                    view.roster_outcome(
                                        RosterOutcome::Dismiss { id: row.id.clone() },
                                        window,
                                        cx,
                                    );
                                },
                            ))
                        }))
                }))
        }))
}

/// Iced hides the `×` on live rows and shows it on finished ones
/// (assets/agent_chat.html `v.x.style.display = row.live ? 'none' : ''`).
pub fn shows_dismiss(state: WorkerState) -> bool {
    !state.is_live()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dismiss_shown_only_on_finished_rows() {
        for live in [
            WorkerState::Queued,
            WorkerState::Running,
            WorkerState::Waiting,
        ] {
            assert!(!shows_dismiss(live), "{live:?}");
        }
        for done in [WorkerState::Blocked, WorkerState::Done, WorkerState::Failed] {
            assert!(shows_dismiss(done), "{done:?}");
        }
    }
    #[test]
    fn roster_matches_iced_role_backend_model_checkout_and_elapsed() {
        let worker = |id: &str, state, started_at_ms: Option<u64>, ended_at_ms| RosterEntry {
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
            worker("new", WorkerState::Running, Some(2_000), None),
            worker("old", WorkerState::Running, Some(1_000), None),
            worker("done", WorkerState::Done, Some(10_000), Some(12_000)),
        ];
        let rows = rows(&input, 187_000);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["new", "old", "done"]
        );
        assert_eq!(rows[0].name, "Worker · Codex · gpt-6.1-sol");
        assert_eq!(rows[0].what, "· in /tmp/TRU-274");
        assert!(rows.iter().all(|row| row.what == "· in /tmp/TRU-274"));
        assert_eq!(rows[1].elapsed, "3m 6s");
        assert_eq!(rows[2].elapsed, "2.0s");
        assert!(super::rows(&[], 0).is_empty());
        assert_eq!(elapsed(0), "0.0s");
        let row = super::rows(
            &[worker(
                "saved",
                WorkerState::Blocked,
                Some(0),
                Some(1_014_000),
            )],
            2_000_000,
        )
        .remove(0);
        assert_eq!(
            format!("{} {} {} ×", row.name, row.what, row.elapsed),
            "Worker · Codex · gpt-6.1-sol · in /tmp/TRU-274 16m 54s ×"
        );
    }

    #[test]
    fn roster_keeps_review_and_consult_role_labels_and_iced_checkout_text() {
        let entry = |name: &str, model: &str| RosterEntry {
            id: name.into(),
            name: name.into(),
            model: model.into(),
            checkout: "/work/TRU-274-long-checkout".into(),
            activity: String::new(),
            state: WorkerState::Running,
            started_at_ms: Some(0),
            ended_at_ms: None,
        };

        let rows = rows(
            &[
                entry("Review", "claude-sonnet"),
                entry("Consult", "claude-opus"),
                entry("Codex review", "gpt-6.1-sol"),
            ],
            1_000,
        );

        assert_eq!(rows[0].name, "Review · claude-sonnet");
        assert_eq!(rows[1].name, "Consult · claude-opus");
        assert_eq!(rows[2].name, "Codex review · gpt-6.1-sol");
        assert!(
            rows.iter()
                .all(|row| row.what == "· in /work/TRU-274-long-checkout")
        );
        assert!(rows.iter().all(|row| row.elapsed == "1.0s"));

        let missing_time = RosterEntry {
            started_at_ms: None,
            ..entry("Worker · Codex", "gpt-6.1-sol")
        };
        assert!(super::rows(&[missing_time], 1_000)[0].elapsed.is_empty());
        let missing_end = RosterEntry {
            state: WorkerState::Done,
            started_at_ms: Some(0),
            ..entry("Worker · Codex", "gpt-6.1-sol")
        };
        assert!(super::rows(&[missing_end], 1_000)[0].elapsed.is_empty());
    }

    #[test]
    fn roster_preserves_iced_order_for_live_waiting_ended_and_finished_rows() {
        let worker = |id: &str, state, started_at_ms: Option<u64>, ended_at_ms| RosterEntry {
            id: id.into(),
            name: "Worker · Codex".into(),
            model: "gpt-6.1-sol".into(),
            checkout: String::new(),
            activity: String::new(),
            state,
            started_at_ms,
            ended_at_ms,
        };
        let iced_order = vec![
            worker("live", WorkerState::Running, Some(3_000), None),
            worker("finished", WorkerState::Done, Some(4_000), Some(5_000)),
            worker(
                "waiting-ended",
                WorkerState::Blocked,
                Some(2_000),
                Some(6_000),
            ),
        ];

        let rows = rows(&iced_order, 10_000);

        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["live", "finished", "waiting-ended"]
        );
    }
}
