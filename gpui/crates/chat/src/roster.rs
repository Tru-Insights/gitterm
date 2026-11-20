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
            let backend = row
                .name
                .strip_prefix("Worker · ")
                .or_else(|| row.name.strip_prefix("Consult · "))
                .or_else(|| row.name.strip_prefix("Review · "))
                .unwrap_or(&row.name);
            let backend = match backend {
                "Codex review" | "Codex consult" => "Codex",
                other => other,
            };
            let checkout = std::path::Path::new(&row.checkout)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&row.checkout);
            let what = if checkout.is_empty() {
                String::new()
            } else {
                format!("· in {checkout}")
            };
            RosterRow {
                id: row.id.clone(),
                name: if row.model.is_empty() {
                    backend.into()
                } else {
                    format!("{} · {}", backend, row.model)
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
        .overflow_x_scroll()
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
                    let selector = format!("roster-{}", row.id);
                    div()
                        .id(gpui::SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .flex()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .items_center()
                        .gap(px(8.))
                        .px(px(4.))
                        .py(px(1.))
                        .cursor_pointer()
                        .rounded(px(4.))
                        .hover(|style| style.bg(rgb(palette.border)))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.roster_outcome(RosterOutcome::Open { id: id.clone() }, cx)
                        }))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(palette.fg))
                                .child(row.name),
                        )
                        .child(div().flex_shrink_0().child(row.what))
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
    fn roster_matches_iced_backend_model_worktree_and_elapsed() {
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
        assert_eq!(rows[0].name, "Codex · gpt-6.1-sol");
        assert_eq!(rows[0].what, "· in TRU-274");
        assert_eq!(rows[1].elapsed, "3m 6s");
        assert_eq!(rows[2].elapsed, "2.0s");
        assert!(super::rows(&[], 0).is_empty());
        assert_eq!(elapsed(0), "0.0s");
        let row = super::rows(
            &[worker("saved", WorkerState::Waiting, 0, Some(1_014_000))],
            2_000_000,
        )
        .remove(0);
        assert_eq!(
            format!("{} {} {} ×", row.name, row.what, row.elapsed),
            "Codex · gpt-6.1-sol · in TRU-274 16m 54s ×"
        );
    }
}
