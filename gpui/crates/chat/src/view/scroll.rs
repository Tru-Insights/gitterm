//! Transcript scroll state exposes exact anchors and explicit follow suspension.
//! GPUI's public ListState supplies layout; older history is gated by the host.
use super::*;
use gpui::{FollowMode, ListAlignment, ListOffset, ListState, list};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollbarThumbStyle};

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
    /// A key scrolls the transcript as it scrolls a WebKit overflow box: a
    /// line is 40px, a page is the viewport less a 40px overlap (at least
    /// 7/8 of it). Reaching the end resumes following.
    pub fn scroll_key(&mut self, key: ScrollKey, cx: &mut Context<Self>) {
        let viewport = self.list.viewport_bounds().size.height;
        let page = (viewport * 0.875).max(viewport - px(40.)).max(px(1.));
        let step = match key {
            ScrollKey::LineUp => -px(40.),
            ScrollKey::LineDown => px(40.),
            ScrollKey::PageUp => -page,
            ScrollKey::PageDown => page,
            ScrollKey::Top => {
                self.scroll_to_offset(
                    ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.),
                    },
                    cx,
                );
                return;
            }
            ScrollKey::End => return self.scroll_to_end(cx),
        };
        let remaining =
            self.list.max_offset_for_scrollbar().y + self.list.scroll_px_offset_for_scrollbar().y;
        if step > px(0.) && step >= remaining {
            return self.scroll_to_end(cx);
        }
        self.list.scroll_by(step);
        cx.notify();
    }
}
#[derive(Clone, Copy)]
pub enum ScrollKey {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    End,
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
                                    view.show_earlier_control(hidden, cx)
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
            // Iced's `#messages::-webkit-scrollbar`: 10px, always drawn, the
            // thumb in strong-border with radius 4 (no hover or active change).
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::vertical(&state)
                        .mode(ScrollbarMode::Always)
                        .styles(|styles| {
                            let thumb = |thumb: ScrollbarThumbStyle| {
                                thumb
                                    .bg(rgb(self.appearance.palette().strong))
                                    .width(px(10.))
                                    .inset(px(0.))
                                    .radius(px(4.))
                            };
                            styles
                                .track(|track| track.width(px(10.)))
                                .thumb(thumb)
                                .thumb_hover(thumb)
                                .thumb_active(thumb)
                        }),
                ),
            )
            .into_any_element()
    }
    /// Iced's `#messages > .earlier`: a centred pill, block fill, strong
    /// border, radius 12, padding 3×12, `s(12)`, 10px above the first block.
    fn show_earlier_control(&self, hidden: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let palette = self.appearance.palette();
        div()
            .w_full()
            .flex()
            .justify_center()
            .mb(px(10.))
            .child(
                div()
                    .id("show-earlier")
                    // A button stays reachable by Tab and Enter/Space.
                    .tab_index(0)
                    .debug_selector(|| "show-earlier".into())
                    .cursor_pointer()
                    .px(px(12.))
                    .py(px(3.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(palette.strong))
                    .bg(rgb(palette.block))
                    .text_color(rgb(palette.fg))
                    .text_size(px(self.appearance.size(12.)))
                    .line_height(gpui::relative(1.5))
                    .hover(|pill| pill.bg(rgb(palette.border)))
                    .on_click(cx.listener(|view, _, _, cx| view.show_earlier(cx)))
                    .child(crate::interactions::earlier_label(hidden)),
            )
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
        // A withdrawn prompt or a request is not a block of its own; it joins the one before.
        while index < model.rows.len()
            && (!model.rows[index].scope.is_empty() || model.rows[index].hidden())
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
