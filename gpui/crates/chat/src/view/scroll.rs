//! Transcript scroll state exposes exact anchors and explicit follow suspension.
//! GPUI's public ListState supplies layout; older history is gated by the host.
use super::*;
use gpui::{FollowMode, ListAlignment, ListOffset, ListState, list};
use gpui_component::scroll::ScrollableElement;

pub struct TranscriptScrollerState {
    list: ListState,
}
impl TranscriptScrollerState {
    pub fn new(count: usize, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(count, ListAlignment::Top, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        let owner = cx.weak_entity();
        list.set_scroll_handler(move |_, _, cx| {
            let owner = owner.clone();
            cx.defer(move |cx| {
                _ = owner.update(cx, |state, cx| {
                    // User scrolling back to the bottom resumes following, as Iced does.
                    if state.list.is_scrolled_to_end() == Some(true) {
                        state.list.set_follow_mode(FollowMode::Tail);
                    }
                    cx.notify();
                });
            });
        });
        Self { list }
    }
    pub fn item_count(&self) -> usize {
        self.list.item_count()
    }
    pub fn is_following_tail(&self) -> bool {
        self.list.is_following_tail()
    }
    pub fn logical_scroll_top(&self) -> ListOffset {
        self.list.logical_scroll_top()
    }
    /// Iced's native fold toggle: a fold that opens grows below its row,
    /// and the row stays where it was, even at the tail. A fold that closes
    /// keeps following, as Iced's scroll clamps to the new bottom.
    pub fn hold_for_toggle(&mut self, opening: bool, cx: &mut Context<Self>) {
        if opening && self.list.is_following_tail() {
            self.list.pause_following_tail();
            cx.notify();
        }
    }
    pub fn pause_follow(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Normal);
        cx.notify();
    }
    pub fn reset(&mut self, count: usize, cx: &mut Context<Self>) {
        self.list.reset(count);
        self.list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }
    pub fn append(&mut self, count: usize, cx: &mut Context<Self>) -> bool {
        if count == 0 {
            return true;
        }
        let end = self.item_count();
        self.list.splice(end..end, count);
        if end > 0 {
            self.list.remeasure_items(end - 1..end);
        }
        cx.notify();
        true
    }
    pub fn remeasure(&mut self, cx: &mut Context<Self>) {
        self.list.remeasure();
        cx.notify();
    }
    pub fn remeasure_items(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        if range.start > range.end || range.end > self.item_count() {
            return false;
        }
        self.list.remeasure_items(range);
        cx.notify();
        true
    }
    pub fn scroll_to_offset(&mut self, offset: ListOffset, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Normal);
        self.list.scroll_to(offset);
        cx.notify();
    }
    pub fn scroll_to_item(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        if index >= self.item_count() {
            return false;
        }
        self.scroll_to_offset(
            ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            },
            cx,
        );
        true
    }
    pub fn reveal_item(&mut self, index: usize, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Normal);
        self.list.scroll_to_reveal_item(index);
        cx.notify();
    }
    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Tail);
        self.list.scroll_to_end();
        cx.notify();
    }
}
impl ChatView {
    pub(super) fn messages(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.scroller.read(cx).list.clone();
        let count = state.item_count();
        let owner = cx.entity().downgrade();
        let hidden = block_starts(&self.model).partition_point(|index| *index < self.history_start);
        div()
            .id("messages")
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(
                list(state.clone(), move |index, window, cx| {
                    let child = owner
                        .update(cx, |view, cx| {
                            if !view.find_open && index < view.history_start {
                                if index + 1 == view.history_start {
                                    Button::new("show-earlier")
                                        .debug_selector(|| "show-earlier".into())
                                        .label(crate::interactions::earlier_label(hidden))
                                        .on_click(
                                            cx.listener(|view, _, _, cx| view.show_earlier(cx)),
                                        )
                                        .into_any_element()
                                } else {
                                    div().h(px(0.)).into_any_element()
                                }
                            } else {
                                view.row(index, window, cx).into_any_element()
                            }
                        })
                        .unwrap_or_else(|_| div().into_any_element());
                    div()
                        .w_full()
                        .px(px(14.))
                        .when(index == 0, |row| row.pt(px(12.)))
                        .when(index + 1 == count, |row| row.pb(px(12.)))
                        .child(child)
                        .into_any_element()
                })
                .size_full()
                .min_h_0(),
            )
            .vertical_scrollbar(&state)
            .into_any_element()
    }
}

/// Iced pages top-level blocks: a tool run or completed-work fold counts once.
pub(super) fn block_starts(model: &ChatModel) -> Vec<usize> {
    let work = turns::work_folds(model);
    let runs = tool_runs::group(model, &work);
    let mut ends = HashMap::new();
    for range in work
        .iter()
        .map(|fold| &fold.range)
        .chain(runs.iter().map(|run| &run.range))
    {
        ends.entry(range.start)
            .and_modify(|end: &mut usize| *end = (*end).max(range.end))
            .or_insert(range.end);
    }
    let mut starts = Vec::new();
    let mut index = 0;
    while index < model.rows.len() {
        starts.push(index);
        index = ends.get(&index).copied().unwrap_or(index + 1);
        // A withdrawn prompt is not a block of its own; it joins the one before.
        while index < model.rows.len()
            && (!model.rows[index].scope.is_empty()
                || model.rows[index].kind == crate::model::RowKind::Withdrawn)
        {
            index += 1;
        }
    }
    starts
}
pub(super) fn initial_start(model: &ChatModel) -> usize {
    let starts = block_starts(model);
    starts
        .get(crate::interactions::history_start(starts.len()))
        .copied()
        .unwrap_or(0)
}
