use super::*;
use crate::interactions::{TranscriptAction, edit_lines};
use gpui_component::checkbox::Checkbox;

pub(super) fn link_allowed(url: &str, event: &ClickEvent) -> bool {
    crate::interactions::http_link(url)
        && match event {
            ClickEvent::Mouse(click) => click.up.button == gpui::MouseButton::Left,
            ClickEvent::Keyboard(_) => true,
            ClickEvent::Touch(click) => !click.long_press,
        }
}
pub(super) fn open_link(
    url: &gpui::SharedString,
    event: &ClickEvent,
    _: &mut Window,
    cx: &mut App,
) {
    if link_allowed(url, event) {
        cx.open_url(url);
    }
}

impl ChatView {
    pub fn set_imported_offer(
        &mut self,
        offer: crate::imported::ImportedOffer,
        cx: &mut Context<Self>,
    ) {
        self.imported_offer = offer;
        cx.notify();
    }
    pub(super) fn render_imported_offer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let offer = &self.imported_offer;
        if offer.conversations.is_empty() {
            return div().into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child("This tab came from GitTerm V5.")
            .child(if offer.conversations.len() == 1 {
                "Its V5 conversation:"
            } else {
                "Its V5 conversations, likeliest first:"
            })
            .children(offer.conversations.iter().map(|conversation| {
                let title = conversation.title.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .child(title.clone())
                            .id(gpui::SharedString::from(format!(
                                "imported-title-{}",
                                conversation.id
                            )))
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(title.clone())
                                    .build(window, cx)
                            }),
                    )
                    .children(
                        (conversation.origin == "terminal")
                            .then(|| div().child("was a terminal tab")),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "continue-here-{}",
                            conversation.id
                        )))
                        .label("Continue here")
                        .debug_selector({
                            let id = conversation.id.clone();
                            move || format!("continue-here-{id}")
                        })
                        .disabled(offer.pending)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.transcript_action(
                                TranscriptAction::PreviewOnly {
                                    label: "Continue here".into(),
                                },
                                cx,
                            )
                        })),
                    )
            }))
            .child(
                "Close this chat in V5 first. Two apps sending to one conversation will tangle it.",
            )
            .into_any_element()
    }
    pub(super) fn transcript_action(&mut self, action: TranscriptAction, cx: &mut Context<Self>) {
        match action {
            TranscriptAction::PreviewOnly { label } => {
                self.set_status_error(crate::interactions::preview_notice(&label), cx)
            }
            TranscriptAction::Dismiss(request) => {
                let notice = match &self.dismiss_gate {
                    Some(gate) => gate(&request, cx),
                    None => crate::interactions::preview_notice(&request.label),
                };
                self.set_status_error(notice, cx)
            }
            TranscriptAction::OpenFile { path, review } => {
                cx.emit(roster::RosterOutcome::OpenFile { path, review })
            }
            TranscriptAction::OpenWorker { id } => {
                cx.emit(roster::RosterOutcome::OpenWorker { id })
            }
            TranscriptAction::Withdraw { message_id } => self.withdraw(message_id, cx),
        }
    }
    pub(super) fn trim_history(&mut self, cx: &mut Context<Self>) {
        if self.find_open {
            return;
        }
        let start = scroll::initial_start(&self.model);
        if self.history_start < start {
            let old = self.history_start;
            self.history_start = start;
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(old.saturating_sub(1)..start.min(state.item_count()), cx);
            });
        }
    }
    pub(super) fn show_earlier(&mut self, cx: &mut Context<Self>) {
        let old = self.history_start;
        let starts = scroll::block_starts(&self.model);
        let hidden = starts.partition_point(|index| *index < old);
        self.history_start = starts.get(hidden.saturating_sub(50)).copied().unwrap_or(0);
        self.scroller.update(cx, |state, cx| {
            state.remeasure_items(self.history_start.saturating_sub(1)..old, cx);
        });
        cx.notify();
    }
    pub(super) fn reveal_roster(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .model
            .rows
            .iter()
            .position(|row| row.roster_id.as_deref() == Some(id))
        else {
            return;
        };
        if index < self.history_start && !self.find_open {
            let old = self.history_start;
            let starts = scroll::block_starts(&self.model);
            let block = starts
                .partition_point(|start| *start <= index)
                .saturating_sub(1);
            self.history_start = starts[block / 50 * 50];
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(self.history_start.saturating_sub(1)..old, cx);
            });
        }
        if let Some(turn) = turns::containing(&turns::group(&self.model.rows), index) {
            self.folded_turns.remove(&self.model.rows[turn.user_row].id);
            self.sync_turn_visibility(cx);
        }
        let work_folds = turns::work_folds(&self.model);
        for fold in work_folds.iter().filter(|fold| fold.range.contains(&index)) {
            self.open_work.insert(self.model.rows[fold.range.start].id);
        }
        for run in tool_runs::group(&self.model, &work_folds)
            .iter()
            .filter(|run| run.range.contains(&index))
        {
            self.open_runs.insert(self.model.rows[run.range.start].id);
        }
        self.sync_runs(cx);
        let row = self.row(index, window, cx);
        let row_id = self.model.rows[index].id;
        if let Some(previous) = self.flashed_row {
            for row in self.row_views.iter().flatten() {
                if row.read(cx).row.id == previous {
                    row.update(cx, |row, cx| {
                        row.flashed = false;
                        cx.notify();
                    });
                }
            }
        }
        self.flashed_row = Some(row_id);
        self.folded.insert(row_id, true);
        row.update(cx, |row, cx| {
            row.open = true;
            row.flashed = true;
            cx.notify();
        });
        self.scroller.update(cx, |state, cx| {
            state.remeasure_items(index..index + 1, cx);
            state.reveal_item(index, cx);
        });
        self.flash_timer = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(900))
                .await;
            _ = view.update(cx, |view, cx| {
                if view.flashed_row == Some(row_id) {
                    view.flashed_row = None;
                    if let Some(row) = view.row_views.get(index).and_then(|row| row.as_ref()) {
                        row.update(cx, |row, cx| {
                            row.flashed = false;
                            cx.notify();
                        });
                    }
                    cx.notify();
                }
            });
        }));
    }
}
impl RowView {
    pub(super) fn update_edit_text(&mut self, cx: &mut Context<Self>) {
        if self.row.edits().is_empty() {
            self.edit_text = None;
            return;
        }
        let output = self
            .row
            .tool
            .as_ref()
            .and_then(|tool| tool.output.as_ref().map(|output| (tool.is_error, output)))
            .filter(|(_, output)| !output.trim().is_empty() && output.trim() != "null")
            .map(|(error, output)| {
                format!(
                    "**{}**\n\n{}",
                    if error { "Error" } else { "Result" },
                    crate::model::code(output, "text")
                )
            })
            .unwrap_or_default();
        if let Some(text) = &self.edit_text {
            text.update(cx, |text, cx| text.set_text(&output, cx));
        } else {
            self.edit_text = Some(cx.new(|cx| {
                TextViewState::markdown(&output, cx)
                    .selectable(true)
                    .scrollable(false)
            }));
        }
    }
    pub(super) fn expand_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        self.expanded_edits.insert(index);
        self.scroller.update(cx, |state, cx| {
            state.remeasure_items(self.index..self.index + 1, cx);
        });
        cx.notify();
    }
    pub(super) fn render_edits(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let palette = self.appearance.palette();
        self.row.edits().iter().enumerate().map(|(index,edit)| {
            let diff=edit_lines(edit);
            let expanded=self.expanded_edits.contains(&index);
            let added=diff.rows.iter().filter(|line|line.sign=='+').count();
            let removed=diff.rows.iter().filter(|line|line.sign=='-').count();
            let skip=|n|div().text_color(rgb(palette.muted)).child(format!("⋯ {n} unchanged line{}",if n==1{""}else{"s"}));
            div().flex().flex_col().gap(px(2.)).font_family("Menlo").text_size(px(self.appearance.size(12.)))
                .child(format!("{} · {}  +{added} −{removed}",edit.path,crate::interactions::edit_title(&self.row,index)))
                .children((diff.skipped_before>0).then(||skip(diff.skipped_before)))
                .children(diff.shown(expanded).iter().map(|line| {
                    div().flex().gap(px(8.)).w_full()
                        .text_color(rgb(match line.sign {'+'=>palette.user,'-'=>palette.err,_=>palette.muted}))
                        .when(!self.query.is_empty() && !find_ranges(&line.text,&self.query).is_empty(),|row|row.bg(rgb(0x665c32)))
                        .child(line.sign.to_string()).child(if line.text.is_empty(){"\u{200b}".into()}else{line.text.clone()})
                }))
                .children((diff.skipped_after>0).then(||skip(diff.skipped_after)))
                .children(diff.more_label(expanded).map(|label| Button::new(("edit-more",index)).ghost().small().label(label)
                    .debug_selector({let id=self.row.id;move||format!("edit-more-{id}-{index}")})
                    .on_click(cx.listener(move|row,_,_,cx|row.expand_edit(index,cx)))))
                .children((added==0 && removed==0).then(||div().child("No line changes")))
                .children(diff.capped.then(||div().child("Too long to compare line by line: shown as all removed, then all added")))
                .into_any_element()
        }).collect()
    }
    pub(super) fn render_delegation(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let card = self.row.delegation.clone().expect("delegation card");
        let palette = self.appearance.palette();
        let id = self.row.id;
        let controls = card.controls(self.busy);
        let sent = card.sendable() && card.delivered();
        let mut sent_shown = false;
        let mut actions = Vec::new();
        for (index, control) in controls.into_iter().enumerate() {
            if sent && !sent_shown && control.label != "Open worker tab" {
                actions.push(
                    div()
                        .text_color(rgb(palette.muted))
                        .child("sent to chat ✓")
                        .into_any_element(),
                );
                sent_shown = true;
            }
            let action = self.action.clone();
            actions.push(
                Button::new(("delegation-action", index))
                    .small()
                    .label(control.label.clone())
                    .debug_selector(move || format!("delegation-action-{id}-{}", control.label))
                    .tooltip(control.tooltip)
                    .disabled(control.disabled)
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        action(control.action.clone(), cx);
                    })
                    .into_any_element(),
            );
        }
        if sent && !sent_shown {
            actions.push(
                div()
                    .text_color(rgb(palette.muted))
                    .child("sent to chat ✓")
                    .into_any_element(),
            );
        }
        let findings = card
            .findings()
            .map(|f| f.findings.clone())
            .unwrap_or_default();
        let selected = self
            .selected_findings
            .get_or_insert_with(|| findings.iter().map(|f| f.id.clone()).collect())
            .clone();
        let [head, target, state] = card.fold_parts(self.open, crate::composer::now_ms());
        // Iced lays each flex child out with `min-width: 0`: a long line
        // wraps (findings) or shortens with an ellipsis (summary, location)
        // instead of widening the card past the transcript.
        div()
            .debug_selector(move || format!("delegation-card-{id}"))
            .flex()
            .flex_col()
            .min_w_0()
            .p(px(10.))
            .gap(px(6.))
            .child(
                Button::new("delegation-fold")
                    .ghost()
                    .h_auto()
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .justify_start()
                    .gap(px(8.))
                    .debug_selector(move || format!("delegation-fold-{id}"))
                    .tooltip(card.header())
                    .child(div().min_w_0().text_ellipsis().child(head))
                    .child(div().flex_1().min_w_0().text_ellipsis().child(target))
                    .child(div().flex_shrink_0().child(state))
                    .on_click(cx.listener(Self::toggle)),
            )
            .when(self.open, |view| {
                view.child(self.copyable_text(cx))
                    .children(findings.into_iter().map(|finding| {
                        let location = crate::delegation::finding_location(&finding);
                        let file = finding.file.clone();
                        let open = self.action.clone();
                        div()
                            .flex()
                            .min_w_0()
                            .gap(px(8.))
                            .items_start()
                            .when(
                                !self.query.is_empty()
                                    && !find_ranges(
                                        &format!("{}\n{}", finding.title, finding.body),
                                        &self.query,
                                    )
                                    .is_empty(),
                                |row| row.bg(rgb(0x665c32)),
                            )
                            .child(
                                Checkbox::new(gpui::SharedString::from(format!(
                                    "finding-{}",
                                    finding.id
                                )))
                                .checked(selected.contains(&finding.id))
                                .accessibility_label(finding.title.clone())
                                .disabled(card.delivered())
                                .debug_selector({
                                    let fid = finding.id.clone();
                                    move || format!("finding-select-{id}-{fid}")
                                })
                                .on_click(cx.listener({
                                    let fid = finding.id.clone();
                                    move |this, checked: &bool, _, cx| {
                                        if let Some(set) = this.selected_findings.as_mut() {
                                            if *checked {
                                                set.insert(fid.clone());
                                            } else {
                                                set.remove(&fid);
                                            }
                                        }
                                        cx.notify();
                                    }
                                })),
                            )
                            .child(
                                div()
                                    .debug_selector({
                                        let fid = finding.id.clone();
                                        move || format!("finding-main-{id}-{fid}")
                                    })
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.))
                                    .flex_1()
                                    .min_w_0()
                                    .child(format!(
                                        "{:?} {} {}",
                                        finding.severity, finding.id, finding.title
                                    ))
                                    .children(location.map(|location| {
                                        Button::new(gpui::SharedString::from(format!(
                                            "finding-location-{}",
                                            finding.id
                                        )))
                                        .ghost()
                                        .small()
                                        .max_w_full()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(div().min_w_0().text_ellipsis().child(location))
                                        .debug_selector({
                                            let fid = finding.id.clone();
                                            move || format!("finding-location-{id}-{fid}")
                                        })
                                        .tooltip(format!(
                                            "Open {} in the file viewer",
                                            file.as_ref().unwrap().display()
                                        ))
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.stop_propagation();
                                                open(
                                                    TranscriptAction::OpenFile {
                                                        path: file.clone().unwrap(),
                                                        review: false,
                                                    },
                                                    cx,
                                                );
                                            },
                                        )
                                    }))
                                    .child(finding.body),
                            )
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .children(actions)
                            .child(
                                div().text_color(rgb(palette.muted)).child(
                                    card.delegation
                                        .delegation_id
                                        .chars()
                                        .take(8)
                                        .collect::<String>(),
                                ),
                            ),
                    )
            })
            .into_any_element()
    }
}
