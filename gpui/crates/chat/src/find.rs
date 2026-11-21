//! Current-transcript find. The model, including unmounted rows, owns searchable text.
use super::*;
use gpui_component::checkbox::Checkbox;

#[derive(Default)]
pub(super) struct TranscriptFind {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub current: Option<usize>,
    include_tools: bool,
    // Retain the original list while find owns its jumps/folds; restore its
    // exact pixel anchor and follow mode when the search closes.
    pub(super) saved_scroller: Option<Entity<MessageScrollerState>>,
    scroll_subscription: Option<Subscription>,
    opened: HashSet<u64>,
    opened_turns: HashSet<u64>,
    opened_runs: HashSet<u64>,
    opened_work: HashSet<u64>,
    opened_edits: HashSet<(u64, usize)>,
}
// HTML searches content roots, excluding native headers and tool labels.
fn body_ranges(row: &Row, text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if !row.edits().is_empty() {
        return find_ranges(text, query);
    }
    let Some(tool) = &row.tool else {
        return find_ranges(text, query);
    };
    let input = if tool.partial_input.is_empty() {
        serde_json::to_string_pretty(&tool.input).expect("tool JSON value")
    } else {
        tool.partial_input.clone()
    };
    let mut offset = 0;
    let mut ranges = Vec::new();
    for content in std::iter::once(input.as_str()).chain(tool.output.as_deref()) {
        if content.is_empty() {
            continue;
        }
        if let Some(start) = text[offset..].find(content) {
            let start = offset + start;
            ranges.extend(
                find_ranges(content, query)
                    .into_iter()
                    .map(|range| start + range.start..start + range.end),
            );
            offset = start + content.len();
        }
    }
    ranges
}
impl TranscriptFind {
    fn searches(&self, row: &Row) -> bool {
        matches!(
            row.kind,
            RowKind::User
                | RowKind::Assistant
                | RowKind::Thinking
                | RowKind::Worker
                | RowKind::Delegation
        ) || (self.include_tools && row.kind == RowKind::Tool)
    }
    fn row_matches(&self, model: &ChatModel, row: usize) -> Vec<SearchMatch> {
        let value = &model.rows[row];
        if !self.searches(value) || self.query.is_empty() {
            return vec![];
        }
        body_ranges(value, &value.find_body(), &self.query)
            .into_iter()
            .enumerate()
            .map(|(occurrence, range)| SearchMatch {
                row,
                occurrence,
                range,
            })
            .collect()
    }
    fn search(&mut self, model: &ChatModel, query: String) {
        self.query = query;
        self.matches = (0..model.rows.len())
            .flat_map(|row| self.row_matches(model, row))
            .collect();
        self.current = (!self.matches.is_empty()).then_some(0);
    }
    pub(super) fn refresh(&mut self, model: &ChatModel, changed: &[usize]) {
        let anchor = self.active().cloned();
        let old_index = self.current.unwrap_or(0);
        self.matches.retain(|found| !changed.contains(&found.row));
        for &row in changed {
            let matches = self.row_matches(model, row);
            self.matches.extend(matches);
        }
        self.matches
            .sort_by_key(|found| (found.row, found.occurrence));
        self.current = anchor
            .and_then(|old| self.matches.iter().position(|new| new == &old))
            .or_else(|| (!self.matches.is_empty()).then(|| old_index.min(self.matches.len() - 1)));
    }
    pub(super) fn active(&self) -> Option<&SearchMatch> {
        self.current.and_then(|index| self.matches.get(index))
    }
}
impl ChatView {
    fn replace_find_scroller(
        &mut self,
        scroller: Entity<MessageScrollerState>,
        cx: &mut Context<Self>,
    ) {
        self.scroller = scroller;
        for view in self.row_views.iter().flatten() {
            view.update(cx, |view, _| view.scroller = self.scroller.clone());
        }
    }
    pub(super) fn change_find(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.find_open || self.find.query == query {
            return;
        }
        self.find.search(&self.model, query);
        self.reveal_match(window, cx);
    }
    pub(super) fn update_row_find(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(view) = &self.row_views[index] {
            let query = if self.find_open && self.find.searches(&self.model.rows[index]) {
                self.find.query.clone()
            } else {
                String::new()
            };
            let active = self
                .find_open
                .then(|| self.find.active())
                .flatten()
                .filter(|found| found.row == index)
                .map(|found| found.occurrence);
            view.update(cx, |view, cx| {
                if view.query == query && view.active == active {
                    return;
                }
                view.query = query;
                view.active = active;
                view.highlight(cx);
                cx.notify();
            });
        }
    }
    fn reveal_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.row_views.len() {
            self.update_row_find(index, cx);
        }
        if let Some(found) = self.find.active().cloned() {
            if let Some(turn) = turns::containing(&turns::group(&self.model.rows), found.row) {
                let id = self.model.rows[turn.user_row].id;
                if self.folded_turns.remove(&id) {
                    self.find.opened_turns.insert(id);
                    self.sync_turn_visibility(cx);
                }
            }
            if let Some(work) = turns::work_folds(&self.model)
                .iter()
                .find(|fold| fold.range.contains(&found.row))
            {
                let id = self.model.rows[work.range.start].id;
                if self.open_work.insert(id) {
                    self.find.opened_work.insert(id);
                    self.sync_runs(cx);
                }
            }
            if let Some(run) = tool_runs::group(&self.model.rows, &self.model.completed_turns)
                .iter()
                .find(|run| run.range.contains(&found.row))
            {
                let id = self.model.rows[run.range.start].id;
                if self.open_runs.insert(id) {
                    self.find.opened_runs.insert(id);
                    self.sync_runs(cx);
                }
            }
            let view = self.row(found.row, window, cx);
            view.update(cx, |view, cx| {
                if !view.open {
                    self.find.opened.insert(view.row.id);
                    view.open = true;
                }
                for (index, edit) in view.row.edits().iter().enumerate() {
                    if !find_ranges(&format!("{}\n{}", edit.old, edit.new), &self.find.query)
                        .is_empty()
                        && view.expanded_edits.insert(index)
                    {
                        self.find.opened_edits.insert((view.row.id, index));
                    }
                }
                view.reveal_pending = true;
                view.highlight_signature = None;
                view.highlight(cx);
                cx.notify();
            });
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(found.row..found.row + 1, cx);
                state.scroll_to_item(found.row, cx);
            });
        }
        cx.notify();
    }
    pub(super) fn navigate(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.find_open || self.find.matches.is_empty() {
            return;
        }
        let count = self.find.matches.len();
        let current = self.find.current.unwrap_or(0);
        self.find.current = Some((current + if backwards { count - 1 } else { 1 }) % count);
        self.reveal_match(window, cx);
    }
    pub(super) fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_open {
            let top = self.scroller.read(cx).logical_scroll_top();
            self.find.saved_scroller = Some(self.scroller.clone());
            let temporary = cx.new(|cx| {
                let mut state = MessageScrollerState::new(self.model.rows.len(), cx);
                state.scroll_to_offset(top, cx);
                state
            });
            self.find.scroll_subscription = Some(cx.observe(&temporary, |_, _, cx| cx.notify()));
            self.replace_find_scroller(temporary, cx);
            self.find_open = true;
            self.find.search(&self.model, self.find.query.clone());
        }
        self.find_input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        self.reveal_match(window, cx);
    }
    pub(super) fn close_find(
        &mut self,
        _: &CloseFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.find_open {
            return;
        }
        self.find_open = false;
        for id in self.find.opened.drain() {
            self.folded.insert(id, false);
            for view in self.row_views.iter().flatten() {
                view.update(cx, |view, cx| {
                    if view.row.id == id && view.open {
                        view.open = false;
                        cx.notify();
                    }
                });
            }
        }
        for id in self.find.opened_turns.drain() {
            self.folded_turns.insert(id);
        }
        self.sync_turn_visibility(cx);
        for id in self.find.opened_runs.drain() {
            self.open_runs.remove(&id);
        }
        for id in self.find.opened_work.drain() {
            self.open_work.remove(&id);
        }
        self.sync_runs(cx);
        for (id, index) in self.find.opened_edits.drain() {
            if let Some(expanded) = self.retained_edit_expansion.get_mut(&id) {
                expanded.remove(&index);
            }
            for view in self.row_views.iter().flatten() {
                if view.read(cx).row.id == id {
                    view.update(cx, |view, cx| {
                        view.expanded_edits.remove(&index);
                        cx.notify();
                    });
                }
            }
        }
        self.find.matches.clear();
        self.find.current = None;
        for index in 0..self.row_views.len() {
            self.update_row_find(index, cx);
        }
        if let Some(saved) = self.find.saved_scroller.take() {
            // Events may have appended rows during find; preserve the original anchor.
            saved.update(cx, |state, cx| {
                let old_count = state.item_count();
                if old_count < self.model.rows.len() {
                    state.append(self.model.rows.len() - old_count, cx);
                }
            });
            self.replace_find_scroller(saved, cx);
        }
        self.find.scroll_subscription = None;
        self.composer.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }
    pub(super) fn reset_find(&mut self, cx: &mut Context<Self>) {
        self.find.opened.clear();
        self.find.opened_turns.clear();
        self.find.opened_runs.clear();
        self.find.opened_work.clear();
        self.find.opened_edits.clear();
        self.find.search(&self.model, self.find.query.clone());
        // Replay replaces the history. Reset both lists, retaining the original
        // entity and its scroll observer for when find closes.
        if let Some(saved) = &self.find.saved_scroller {
            saved.update(cx, |state, cx| state.reset(0, cx));
        }
    }
    pub(super) fn toggle_find_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.include_tools = !self.find.include_tools;
        self.find.search(&self.model, self.find.query.clone());
        self.reveal_match(window, cx);
    }
    pub(super) fn find_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .p(px(6.))
            .bg(rgb(self.appearance.palette().footer))
            .text_size(px(self.appearance.size(12.)))
            .rounded(px(8.))
            .child(div().flex_1().child(Input::new(&self.find_input)))
            .child(if self.find.query.is_empty() {
                String::new()
            } else if self.find.matches.is_empty() {
                "No matches".into()
            } else {
                format!(
                    "{} of {}",
                    self.find.current.map_or(0, |index| index + 1),
                    self.find.matches.len()
                )
            })
            .child(
                Button::new("previous")
                    .small()
                    .label("↑")
                    .tooltip("Previous match (Shift+Enter)")
                    .disabled(self.find.matches.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.navigate(true, window, cx))),
            )
            .child(
                Button::new("next")
                    .small()
                    .label("↓")
                    .tooltip("Next match (Enter)")
                    .disabled(self.find.matches.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.navigate(false, window, cx))),
            )
            .child(
                Checkbox::new("find-tools")
                    .label("Tool output")
                    .checked(self.find.include_tools)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.toggle_find_tools(window, cx)),
                    ),
            )
            .child(
                Button::new("close-find")
                    .small()
                    .label("×")
                    .tooltip("Close (Esc)")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_find(&CloseFind, window, cx)),
                    ),
            )
    }
}

impl RowView {
    pub(super) fn highlight(&mut self, cx: &mut Context<Self>) {
        let query = self.query.clone();
        let display = self.edit_text.as_ref().unwrap_or(&self.text).clone();
        let edit_matches = if self.edit_text.is_some() {
            self.row
                .edits()
                .iter()
                .map(|edit| {
                    find_ranges(
                        &crate::interactions::edit_lines(edit)
                            .rows
                            .into_iter()
                            .map(|line| line.text)
                            .collect::<Vec<_>>()
                            .join("\n"),
                        &query,
                    )
                    .len()
                })
                .sum()
        } else {
            0
        };
        let active = self
            .active
            .and_then(|index| index.checked_sub(edit_matches));
        let reveal = self.reveal_pending;
        let rendered = display.read(cx).rendered_text();
        let mut signature = std::hash::DefaultHasher::new();
        (rendered.as_str(), &query, active).hash(&mut signature);
        let signature = signature.finish();
        // Native highlight updates notify observers; avoid a render/notify cycle.
        if self.highlight_signature == Some(signature) {
            return;
        }
        self.highlight_signature = Some(signature);
        let revealed = display.update(cx, |text, cx| {
            let rendered = text.rendered_text();
            let ranges = if self.edit_text.is_some() {
                self.row
                    .tool
                    .as_ref()
                    .and_then(|tool| tool.output.as_deref())
                    .and_then(|output| rendered.as_str().find(output).map(|start| (start, output)))
                    .map(|(start, output)| {
                        find_ranges(output, &query)
                            .into_iter()
                            .map(|range| start + range.start..start + range.end)
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                body_ranges(&self.row, rendered.as_str(), &query)
            };
            let available = active.is_none() || active.is_some_and(|index| index < ranges.len());
            let mut highlights: Vec<_> = ranges
                .iter()
                .cloned()
                .map(|range| RangeHighlight::new(range, rgb(0x665c32)))
                .collect();
            if let Some(range) = active.and_then(|index| ranges.get(index)) {
                highlights.push(RangeHighlight::new(range.clone(), rgb(0x9a6a28)));
                if reveal {
                    _ = text.reveal_range(range.clone(), cx);
                }
            }
            // All ranges were computed against this very revision, never source byte offsets.
            text.set_range_highlights(highlights, cx)
                .expect("current rendered ranges");
            available
        });
        self.reveal_pending = reveal && !revealed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::parse_fixture;
    use gpui::{TestAppContext, VisualTestContext};
    use std::cell::Cell;

    fn setup<'a>(
        fixture: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<ChatView>, &'a mut VisualTestContext) {
        let records = parse_fixture(fixture).unwrap();
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let chat = cx.new(|cx| ChatView::new(records, window, cx));
            Root::new(chat, window, cx)
        });
        let chat = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<ChatView>().unwrap()
        });
        (chat, cx)
    }
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx.run_until_parked();
    }
    fn type_query(query: &str, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input(query);
        draw(cx);
    }
    #[gpui::test]
    fn find_5000_keyboard_counts_wraps_and_restores_retained_scroll(cx: &mut TestAppContext) {
        let (chat, cx) = setup(include_str!("../fixtures/long-5000.jsonl"), cx);
        draw(cx);
        // Preserve a user-scrolled position, not just the default tail.
        chat.update_in(cx, |view, _, cx| {
            view.scroller.update(cx, |state, cx| {
                state.scroll_to_item(2500, cx);
            });
        });
        draw(cx);
        // Width preferences center the chat; scroll inside its retained column.
        let center_x = cx.debug_bounds("native-chat").unwrap().center().x;
        cx.simulate_event(gpui::ScrollEvent {
            position: gpui::point(center_x, px(300.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-37.))),
            ..Default::default()
        });
        draw(cx);
        let (original, top) = chat.read_with(cx, |view, cx| {
            (
                view.scroller.clone(),
                view.scroller.read(cx).logical_scroll_top(),
            )
        });
        assert_ne!(top.offset_in_item, px(0.));
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        type_query("ROW-", cx);
        chat.read_with(cx, |view, cx| {
            assert!(view.find_open);
            assert_eq!(view.find.matches.len(), 2500);
            assert_eq!(view.find.current, Some(0));
            assert_eq!(view.scroller.read(cx).logical_scroll_top().item_ix, 1);
        });
        cx.simulate_keystrokes("shift-enter");
        draw(cx);
        chat.read_with(cx, |view, cx| {
            assert_eq!(view.find.current, Some(2499));
            assert_eq!(view.find.active().unwrap().row, 4999);
            assert!(view.scroller.read(cx).logical_scroll_top().item_ix > 4900);
        });
        cx.simulate_keystrokes("enter");
        draw(cx);
        assert_eq!(chat.read_with(cx, |view, _| view.find.current), Some(0));
        cx.simulate_keystrokes("cmd-g");
        draw(cx);
        assert_eq!(chat.read_with(cx, |view, _| view.find.current), Some(1));
        cx.simulate_keystrokes("cmd-shift-g");
        draw(cx);
        assert_eq!(chat.read_with(cx, |view, _| view.find.current), Some(0));
        // A new query resets to its first match, and user prompts participate.
        type_query("Synthetic prompt 4998", cx);
        assert_eq!(
            chat.read_with(cx, |view, _| view.find.active().unwrap().row),
            4998
        );
        type_query("no such synthetic marker", cx);
        assert!(chat.read_with(cx, |view, _| view.find.matches.is_empty()));
        cx.simulate_keystrokes("escape");
        draw(cx);
        chat.read_with(cx, |view, cx| {
            assert!(!view.find_open);
            assert_eq!(view.scroller, original);
            let restored = view.scroller.read(cx).logical_scroll_top();
            assert_eq!(restored.item_ix, top.item_ix);
            assert_eq!(restored.offset_in_item, top.offset_in_item);
            assert!(!view.scroller.read(cx).is_following_tail());
        });
        cx.update(|window, cx| {
            assert!(
                chat.read(cx)
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            )
        });
    }

    #[gpui::test]
    fn find_showcase_folds_tools_code_and_tail_restore(cx: &mut TestAppContext) {
        let (chat, cx) = setup(include_str!("../fixtures/showcase.jsonl"), cx);
        draw(cx);
        let original = chat.read_with(cx, |view, _| view.scroller.clone());
        let thinking = chat.read_with(cx, |view, _| {
            view.model
                .rows
                .iter()
                .position(|row| row.kind == RowKind::Thinking)
                .unwrap()
        });
        // A fold the user opened before find must remain open on close.
        let tool = chat.read_with(cx, |view, _| {
            view.model
                .rows
                .iter()
                .position(|row| row.kind == RowKind::Tool)
                .unwrap()
        });
        chat.update_in(cx, |view, window, cx| {
            view.row(tool, window, cx)
                .update(cx, |row, _| row.open = true);
        });
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        type_query("failure cases", cx);
        chat.read_with(cx, |view, cx| {
            assert_eq!(view.find.active().unwrap().row, thinking);
            assert!(view.row_views[thinking].as_ref().unwrap().read(cx).open);
        });
        type_query("saturating_add", cx);
        assert!(chat.read_with(cx, |view, _| view.find.matches.is_empty()));
        chat.update_in(cx, |view, window, cx| view.toggle_find_tools(window, cx));
        draw(cx);
        let found = chat.read_with(cx, |view, cx| {
            let found = view.find.active().unwrap().row;
            assert_eq!(view.model.rows[found].kind, RowKind::Tool);
            assert!(view.row_views[found].as_ref().unwrap().read(cx).open);
            found
        });
        type_query("Input", cx);
        chat.read_with(cx, |view, _| {
            assert!(
                view.find
                    .matches
                    .iter()
                    .all(|found| view.model.rows[found.row].kind != RowKind::Tool)
            )
        });
        // Find-opened folds survive query changes, until close.
        type_query("attempt.min(4)", cx);
        chat.read_with(cx, |view, _| {
            assert_eq!(view.find.matches.len(), 1);
            assert_eq!(
                view.model.rows[view.find.active().unwrap().row].kind,
                RowKind::Assistant
            );
        });
        cx.simulate_keystrokes("escape");
        draw(cx);
        chat.read_with(cx, |view, cx| {
            assert!(!view.folded[&view.model.rows[thinking].id]);
            assert!(!view.folded[&view.model.rows[found].id]);
            assert!(view.row_views[tool].as_ref().unwrap().read(cx).open);
            assert!(view.scroller.read(cx).is_following_tail());
        });
        // Reopening preserves and selects the query, as HTML does.
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        cx.simulate_input("bounded backoff");
        draw(cx);
        assert_eq!(
            chat.read_with(cx, |view, _| view.find.query.clone()),
            "bounded backoff"
        );
        chat.update_in(cx, |view, window, cx| {
            view.start_replay(&ClickEvent::default(), window, cx)
        });
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(40));
            draw(cx);
        }
        cx.simulate_keystrokes("escape");
        draw(cx);
        chat.read_with(cx, |view, cx| {
            assert_eq!(view.scroller, original);
            assert_eq!(view.scroller.read(cx).item_count(), view.model.rows.len());
            assert!(view.scroller.read(cx).is_following_tail());
        });
    }

    #[gpui::test]
    fn unchanged_find_updates_do_not_notify(cx: &mut TestAppContext) {
        let (chat, cx) = setup(include_str!("../fixtures/showcase.jsonl"), cx);
        draw(cx);
        cx.simulate_keystrokes("cmd-f");
        draw(cx);
        type_query("bounded backoff", cx);
        let row = chat.read_with(cx, |view, _| {
            view.row_views[view.find.active().unwrap().row]
                .as_ref()
                .unwrap()
                .clone()
        });
        let text = row.read_with(cx, |row, _| row.text.clone());
        let notifications = Rc::new(Cell::new(0));
        let observed = notifications.clone();
        let chat_subscription =
            cx.update(|_, cx| cx.observe(&chat, move |_, _| observed.set(observed.get() + 1)));
        let observed = notifications.clone();
        let row_subscription =
            cx.update(|_, cx| cx.observe(&row, move |_, _| observed.set(observed.get() + 1)));
        let observed = notifications.clone();
        let text_subscription =
            cx.update(|_, cx| cx.observe(&text, move |_, _| observed.set(observed.get() + 1)));
        // Bounded direct updates: a regression is a failed assertion, not an idle-loop hang.
        for _ in 0..5 {
            chat.update_in(cx, |view, window, cx| {
                view.change_find(view.find.query.clone(), window, cx);
                for index in 0..view.row_views.len() {
                    view.update_row_find(index, cx);
                }
            });
            row.update_in(cx, |row, _, cx| row.highlight(cx));
        }
        assert_eq!(notifications.get(), 0);
        drop((chat_subscription, row_subscription, text_subscription));
    }
}
