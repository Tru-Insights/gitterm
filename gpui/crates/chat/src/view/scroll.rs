//! Transcript scroll state exposes exact anchors and explicit follow suspension.
//! GPUI's public ListState supplies layout; older history is gated by the host.
use super::*;
use gpui::{FollowMode, ListAlignment, ListOffset, ListState, Pixels, list};
use gpui_component::scroll::ScrollableElement;

/// Iced's `atBottom()`: closer than this to the bottom counts as there.
const AT_BOTTOM: f32 = 40.;

pub struct TranscriptScrollerState {
    list: ListState,
    /// Iced's `follow` while the list stands short of the bottom: the reader
    /// is within 40px of it, opened a disclosure at the tail, or sent a
    /// prompt. Latest stays hidden and the next output scrolls to the bottom.
    held: bool,
    /// A work or run fold opened while following. Iced's `watchOpen` checks,
    /// once the fold has laid out, whether it left the bottom.
    watch: bool,
    /// Find owns this list: as in Iced, nothing follows while the bar shows.
    searching: bool,
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
                    state.user_scrolled();
                    cx.notify();
                });
            });
        });
        Self {
            list,
            held: false,
            watch: false,
            searching: false,
        }
    }
    /// The temporary list find scrolls: it never follows output.
    pub(crate) fn searching(count: usize, top: ListOffset, cx: &mut Context<Self>) -> Self {
        let mut state = Self::new(count, cx);
        state.searching = true;
        state.scroll_to_offset(top, cx);
        state
    }
    /// Iced's scroll listener: only the human scrolls, and being within
    /// 40px of the bottom is following. GPUI calls this for wheel and
    /// trackpad scrolls only; programmatic scrolls never reach it.
    fn user_scrolled(&mut self) {
        if self.searching {
            return;
        }
        // Scrolling back to the very bottom resumes the list's own tail mode.
        if self.list.is_scrolled_to_end() == Some(true) {
            self.list.set_follow_mode(FollowMode::Tail);
        }
        if let Some(distance) = self.distance_to_end() {
            self.held = !self.list.is_following_tail() && distance < px(AT_BOTTOM);
        }
    }
    /// How far the viewport's bottom is above the content's, by the heights
    /// the list knows. shortcut: rows below the viewport and its 400px
    /// overdraw may be unmeasured and count as 0, so a single scroll of more
    /// than 360px can read as near the bottom; upgrade if gpui exposes
    /// whether the rows below are measured.
    fn distance_to_end(&self) -> Option<Pixels> {
        if self.list.viewport_bounds().size.height <= px(0.) {
            return None;
        }
        let max = self.list.max_offset_for_scrollbar().y;
        let offset = -self.list.scroll_px_offset_for_scrollbar().y;
        Some((max - offset).max(px(0.)))
    }
    pub fn item_count(&self) -> usize {
        self.list.item_count()
    }
    /// Iced's `follow`: Latest shows exactly when this is false.
    pub fn is_following_tail(&self) -> bool {
        self.held || self.list.is_following_tail()
    }
    pub fn logical_scroll_top(&self) -> ListOffset {
        self.list.logical_scroll_top()
    }
    /// Iced's native disclosure toggle: what opens grows below its summary
    /// and the summary stays where it was, even at the tail. Following
    /// carries on (Latest stays hidden) until the next output scrolls down.
    /// Only a work or run fold (`watch`, Iced's `watchOpen`) stops following,
    /// and only when the opened content runs 40px or more past the bottom.
    /// A disclosure that closes keeps following, as Iced's scroll clamps.
    pub fn hold_for_toggle(&mut self, opening: bool, watch: bool, cx: &mut Context<Self>) {
        if opening && self.is_following_tail() {
            if self.list.is_following_tail() {
                self.list.pause_following_tail();
            }
            self.held = true;
            self.watch |= watch;
            cx.notify();
        }
    }
    /// After the list laid out: settle a fold's `watchOpen` check, and a
    /// scrollbar drag (which reaches no scroll handler) by Iced's 40px rule.
    fn settle(&mut self, cx: &mut Context<Self>) {
        let held = self.held;
        if self.searching {
            self.watch = false;
        } else if self.list.is_scrollbar_dragging() {
            if let Some(distance) = self.distance_to_end() {
                self.held = !self.list.is_following_tail() && distance < px(AT_BOTTOM);
            }
        } else if std::mem::take(&mut self.watch)
            && self.held
            && !self.list.is_following_tail()
            && self
                .distance_to_end()
                .is_some_and(|distance| distance >= px(AT_BOTTOM))
        {
            self.held = false;
        }
        if held != self.held {
            cx.notify();
        }
    }
    /// Iced's `scrollIfFollowing` for new output: a held follow scrolls to
    /// the bottom and follows again.
    pub(crate) fn follow_output(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.held) && !self.searching {
            self.watch = false;
            self.list.set_follow_mode(FollowMode::Tail);
            cx.notify();
        }
    }
    /// Iced's `showUserPrompt`: a new prompt sets follow, without scrolling
    /// until the output after it. Find's list ignores it; close restores.
    pub(crate) fn follow_prompt(&mut self, cx: &mut Context<Self>) {
        if !self.searching && !self.is_following_tail() {
            self.held = true;
            cx.notify();
        }
    }
    pub fn pause_follow(&mut self, cx: &mut Context<Self>) {
        self.held = false;
        self.list.set_follow_mode(FollowMode::Normal);
        cx.notify();
    }
    pub fn reset(&mut self, count: usize, cx: &mut Context<Self>) {
        self.held = false;
        self.watch = false;
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
        self.held = false;
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
        self.held = false;
        self.list.set_follow_mode(FollowMode::Normal);
        self.list.scroll_to_reveal_item(index);
        cx.notify();
    }
    /// Latest. While find shows, Iced scrolls to the bottom but keeps
    /// `follow` false, so output does not move the view until find closes.
    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.held = false;
        self.watch = false;
        if self.searching {
            self.list.scroll_to_end();
        } else {
            self.list.set_follow_mode(FollowMode::Tail);
            self.list.scroll_to_end();
        }
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
                            } else if view.find_open && index == 0 && view.history_start > 0 {
                                // Iced disables "Show earlier" while find owns
                                // the history; it stays at the top.
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        Button::new("show-earlier")
                                            .debug_selector(|| "show-earlier".into())
                                            .label(crate::interactions::earlier_label(hidden))
                                            .disabled(true),
                                    )
                                    .child(view.row(index, window, cx))
                                    .into_any_element()
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
            .child({
                // Prepaints after the list above, so its layout is current.
                let scroller = self.scroller.clone();
                gpui::canvas(
                    move |_, _, cx| scroller.update(cx, |state, cx| state.settle(cx)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_0()
            })
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

#[cfg(test)]
#[path = "scroll_tests.rs"]
mod tests;
