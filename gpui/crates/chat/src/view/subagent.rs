//! Iced's `details.subagent`: a subagent's activity nested in its Agent card.
use super::*;
use crate::model::{SubState, Subagent};

impl RowView {
    /// The block's `.sa-text`, kept in step with the row.
    pub(super) fn update_sub_text(&mut self, cx: &mut Context<Self>) {
        let Some(sub) = self.row.tool.as_ref().and_then(|tool| tool.sub.as_ref()) else {
            self.sub_text = None;
            return;
        };
        let source = crate::model::preformatted(&sub.text);
        if let Some(text) = &self.sub_text {
            text.update(cx, |text, cx| text.set_text(&source, cx));
        } else {
            self.sub_text = Some(cx.new(|cx| {
                TextViewState::markdown(&source, cx)
                    .selectable(true)
                    .scrollable(true)
            }));
        }
    }
    fn toggle_sub(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.sub_open = !self.sub_open;
        let opening = self.sub_open;
        self.scroller.update(cx, |state, cx| {
            state.hold_for_toggle(opening, false, cx);
            state.remeasure_items(self.index..self.index + 1, cx);
        });
        cx.notify();
    }
    /// The block and its `.sa-latest` line, which Iced puts in the tool card
    /// before the card's body (so both show only while the card is open).
    pub(super) fn render_subagent(
        &mut self,
        sub: &Subagent,
        input: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let palette = self.appearance.palette();
        let size = |n| px(self.appearance.size(n));
        let id = self.row.id;
        let edge = match sub.state {
            SubState::Running => palette.accent,
            SubState::Done => palette.result,
            SubState::Failed => palette.err,
            SubState::Stopped => palette.muted,
        };
        let label = match sub.state {
            SubState::Done => palette.result,
            SubState::Failed => palette.err,
            SubState::Running | SubState::Stopped => palette.accent,
        };
        let summary = Button::new("subagent")
            .ghost()
            .h_auto()
            .w_full()
            .px(px(8.))
            .py(px(4.))
            .justify_start()
            .debug_selector(move || format!("subagent-summary-{id}"))
            .flex()
            .items_baseline()
            .gap(px(8.))
            .cursor_pointer()
            .child(
                div()
                    .text_size(size(10.))
                    .text_color(rgb(palette.muted))
                    .child(if self.sub_open { "▾" } else { "▸" }),
            )
            .child(
                div()
                    .debug_selector(move || format!("subagent-label-{id}"))
                    .flex_shrink_0()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(size(11.))
                    .text_color(rgb(label))
                    // `letter-spacing: 0.05em`.
                    .child("SUBAGENT"),
            )
            .child(
                div()
                    .debug_selector(move || format!("subagent-desc-{id}"))
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(palette.fg))
                    .child(sub.description(input)),
            )
            .child(
                div()
                    .debug_selector(move || format!("subagent-state-{id}"))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .font_family("Menlo")
                    .text_size(size(11.))
                    .text_color(rgb(if sub.state == SubState::Running {
                        palette.accent
                    } else {
                        palette.muted
                    }))
                    .child(sub.state_text()),
            )
            .on_click(cx.listener(Self::toggle_sub));
        let chips = sub.tools.iter().enumerate().map(|(index, tool)| {
            div()
                .id(("subagent-tool", index))
                .debug_selector(move || format!("subagent-tool-{id}-{index}"))
                .font_family("Menlo")
                .text_size(size(11.))
                .px(px(5.))
                .rounded(px(4.))
                .border_1()
                .border_color(rgb(palette.border))
                .text_color(rgb(match tool.result {
                    None => palette.tool,
                    Some(false) => palette.result,
                    Some(true) => palette.err,
                }))
                .tooltip({
                    let tip = tool.tip.clone();
                    move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    }
                })
                .child(tool.name.clone())
        });
        let body = div()
            .pt(px(2.))
            .pr(px(8.))
            .pb(px(6.))
            .pl(px(22.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .my(px(2.))
                    .children(chips),
            )
            .child(
                div()
                    .debug_selector(move || format!("subagent-text-{id}"))
                    .mt(px(4.))
                    .text_size(size(12.))
                    .children(self.sub_text.clone().map(|text| {
                        TextView::new(&text)
                            .text_size(size(12.))
                            .text_color(rgb(palette.fg))
                            .selectable(true)
                            .scrollable(true)
                            .max_h(px(240.))
                    })),
            );
        let block = div()
            .debug_selector(move || format!("subagent-{id}"))
            .mt(px(6.))
            .mb(px(2.))
            .relative()
            .rounded(px(5.))
            .border_1()
            .border_l_0()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.alt))
            .text_size(size(12.5))
            // The 3px state-coloured left border, drawn as the tool card's.
            .child(
                div()
                    .debug_selector(move || format!("subagent-edge-{id}"))
                    .absolute()
                    .left_0()
                    .top(px(-1.))
                    .bottom(px(-1.))
                    .w(px(3.))
                    .rounded_l(px(5.))
                    .bg(rgb(edge)),
            )
            .child(
                div().pl(px(3.)).child(
                    Collapsible::new()
                        .open(self.sub_open)
                        .child(summary)
                        .content(body),
                ),
            );
        let latest = sub.latest().map(|line| {
            div()
                .debug_selector(move || format!("subagent-latest-{id}"))
                .pt(px(0.))
                .pr(px(8.))
                .pb(px(4.))
                .pl(px(22.))
                .whitespace_nowrap()
                .text_ellipsis()
                .overflow_hidden()
                .text_size(size(12.))
                .text_color(rgb(palette.muted))
                .child(line)
        });
        div()
            .ml(px(3.))
            .flex()
            .flex_col()
            .child(block)
            .children(latest)
            .into_any_element()
    }
}
