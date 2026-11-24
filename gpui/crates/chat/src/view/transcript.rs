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

/// Iced's `details.dlg.<state>` left border; later CSS rules win, so
/// blocked/waiting beats dismissed, which beats the outcome colours.
pub(super) fn card_border(
    card: &crate::delegation::DelegationCard,
    palette: crate::appearance::ChatPalette,
) -> u32 {
    match card.state_class() {
        "blocked" | "waiting" => palette.warn,
        _ if card.delegation.dismissed_at.is_some() => palette.muted,
        "cancelled" => palette.muted,
        "failed" | "interrupted" | "exited" => palette.err,
        "completed" | "done" => palette.result,
        _ => palette.accent,
    }
}

/// Iced's `.sev.<P0..P3>` badge: background, then text colour.
pub(super) fn severity_colors(
    severity: gitterm_core::tasks::ReviewSeverity,
    palette: crate::appearance::ChatPalette,
) -> (u32, u32) {
    use gitterm_core::tasks::ReviewSeverity::*;
    match severity {
        P0 => (palette.err, palette.base),
        P1 => (palette.tool, palette.base),
        P2 => (palette.warn, palette.base),
        P3 => (palette.strong, palette.fg),
    }
}

/// CSS `filter: brightness(1.1)`: each channel times 1.1, clamped.
pub(super) fn brightened(color: u32) -> u32 {
    [16, 8, 0].into_iter().fold(0, |out, shift| {
        let channel = ((color >> shift) & 0xff) as f32 * 1.1;
        out | ((channel.round().min(255.) as u32) << shift)
    })
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
        let appearance = self.appearance;
        let palette = appearance.palette();
        let id = self.row.id;
        let class = card.state_class().to_owned();
        let findings = card
            .findings()
            .map(|f| f.findings.clone())
            .unwrap_or_default();
        let selected = self
            .selected_findings
            .get_or_insert_with(|| findings.iter().map(|f| f.id.clone()).collect())
            .clone();
        let controls = card.controls(self.busy, &selected);
        let sent = card.sendable() && card.delivered();
        let mut sent_shown = false;
        // Iced's `.dlg-note`: muted, 11.5/14.
        let note = |text: String| {
            div()
                .text_size(px(appearance.size(11.5)))
                .text_color(rgb(palette.muted))
                .child(text)
                .into_any_element()
        };
        let mut actions = Vec::new();
        for (index, control) in controls.into_iter().enumerate() {
            if sent && !sent_shown && control.label != "Open worker tab" {
                actions.push(note("sent to chat ✓".into()));
                sent_shown = true;
            }
            let action = self.action.clone();
            let primary = control.primary;
            let disabled = control.disabled;
            // Iced's `.dlg-actions button`: padding 4px 10px, 12/14.
            actions.push(
                Button::new(("delegation-action", index))
                    .small()
                    // Iced's `button.primary`: accent fill and border, base
                    // text, brightness(1.1) on hover, opacity .4 disabled.
                    .when(primary, |button| {
                        button
                            .custom(
                                gpui_component::button::ButtonCustomVariant::new(cx)
                                    .color(rgb(palette.accent).into())
                                    .foreground(rgb(palette.base).into())
                                    .hover(rgb(brightened(palette.accent)).into())
                                    .active(rgb(brightened(palette.accent)).into()),
                            )
                            .border_1()
                            .bg(rgb(palette.accent))
                            .border_color(rgb(palette.accent))
                            .text_color(rgb(palette.base))
                            .when(disabled, |button| button.opacity(0.4))
                    })
                    .h_auto()
                    .px(px(10.))
                    .py(px(4.))
                    // A browser button's line height is `normal`, not the transcript's 1.5.
                    .line_height(gpui::relative(1.2))
                    // Button's own label is a fixed `text_sm`; Iced's is 12/14.
                    .accessibility_label(control.label.clone())
                    .child(
                        div()
                            .text_size(px(appearance.size(12.)))
                            .child(control.label.clone()),
                    )
                    .debug_selector({
                        let label = control.label.clone();
                        move || {
                            format!(
                                "delegation-action-{id}-{label}{}",
                                if primary { "-primary" } else { "" }
                            )
                        }
                    })
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
            actions.push(note("sent to chat ✓".into()));
        }
        let [_, target, state] = card.fold_parts(self.open, crate::composer::now_ms());
        let kind_color = match class.as_str() {
            "completed" | "done" => palette.result,
            "failed" | "interrupted" => palette.err,
            "blocked" | "waiting" => palette.warn,
            _ => palette.accent,
        };
        let state_color = match class.as_str() {
            "running" | "requested" => palette.accent,
            "waiting" | "blocked" => palette.warn,
            _ => palette.muted,
        };
        let (body, parts_bottom) = self.render_card_parts(card.parts(), cx);
        // Iced's `.dlg-findings` (margin 4px 0) and `.dlg-actions`
        // (margin-top 8px) collapse with the block margin above them.
        let list_top = f32::max(4. - parts_bottom, 0.);
        let actions_top = if findings.is_empty() {
            f32::max(8. - parts_bottom, 0.)
        } else {
            4.
        };
        // Iced lays each flex child out with `min-width: 0`: a long line
        // wraps (findings) or shortens with an ellipsis (summary, location)
        // instead of widening the card past the transcript.
        div()
            .debug_selector(move || format!("delegation-card-{id}"))
            .flex()
            .flex_col()
            .min_w_0()
            .text_size(px(appearance.size(13.)))
            // Iced's `details.dlg > summary`: padding 6px 10px, gap 8px.
            .child(
                Button::new("delegation-fold")
                    .ghost()
                    .h_auto()
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .justify_start()
                    .px(px(10.))
                    .py(px(6.))
                    .gap(px(8.))
                    .text_size(px(appearance.size(13.)))
                    .debug_selector(move || format!("delegation-fold-{id}"))
                    .tooltip(card.header())
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(appearance.size(10.)))
                            .text_color(rgb(palette.muted))
                            .child(if self.open { "▾" } else { "▸" }),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("delegation-kind-{id}"))
                            .min_w_0()
                            .text_ellipsis()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(appearance.size(12.5)))
                            .text_color(rgb(kind_color))
                            .child(card.header()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .text_color(rgb(palette.muted))
                            .child(target),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("delegation-state-{id}"))
                            .flex_shrink_0()
                            .font_family("Menlo")
                            .text_size(px(appearance.size(11.5)))
                            .text_color(rgb(state_color))
                            .child(state),
                    )
                    .on_click(cx.listener(Self::toggle)),
            )
            .when(self.open, |view| {
                // Iced's `.dlg-body`: padding 0 10px 8px 22px.
                view.child(
                    div()
                        .debug_selector(move || format!("delegation-body-{id}"))
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .pl(px(22.))
                        .pr(px(10.))
                        .pb(px(8.))
                        .children(body)
                        // Iced's `ul.dlg-findings`: margin 4px 0, no padding.
                        .when(!findings.is_empty(), |body| {
                            let delivered = card.delivered();
                            body.child(
                                div()
                                    .debug_selector(move || format!("delegation-findings-{id}"))
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .mt(px(list_top))
                                    .mb(px(4.))
                                    .children(findings.into_iter().enumerate().map(
                                        |(index, finding)| {
                                            self.render_finding(
                                                index, finding, &selected, delivered, cx,
                                            )
                                        },
                                    )),
                            )
                        })
                        // Iced's `.dlg-actions`: margin-top 8px, gap 6px.
                        .child(
                            div()
                                .debug_selector(move || format!("delegation-actions-{id}"))
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(6.))
                                .mt(px(actions_top))
                                .children(actions)
                                .child(note(
                                    card.delegation
                                        .delegation_id
                                        .chars()
                                        .take(8)
                                        .collect::<String>(),
                                )),
                        ),
                )
            })
            .into_any_element()
    }
    /// Iced's `li.dlg-finding`: checkbox, then `.dlg-fmain` with the
    /// severity badge, id and title, the location link and the body.
    fn render_finding(
        &self,
        index: usize,
        finding: gitterm_core::tasks::ReviewFinding,
        selected: &HashSet<String>,
        delivered: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let appearance = self.appearance;
        let palette = appearance.palette();
        let id = self.row.id;
        let fid = finding.id.clone();
        let location = crate::delegation::finding_location(&finding);
        let file = finding.file.clone();
        let open = self.action.clone();
        let (badge_bg, badge_fg) = severity_colors(finding.severity, palette);
        let severity = format!("{:?}", finding.severity);
        // `.dlg-finding`: flex, gap 6px, padding 4px 0, a top rule except first.
        div()
            .debug_selector({
                let fid = fid.clone();
                move || format!("finding-row-{id}-{fid}")
            })
            .flex()
            .min_w_0()
            .gap(px(6.))
            .items_start()
            .py(px(4.))
            .when(index > 0, |row| {
                row.border_t_1().border_color(rgb(palette.border))
            })
            .when(
                !self.query.is_empty()
                    && !find_ranges(&format!("{}\n{}", finding.title, finding.body), &self.query)
                        .is_empty(),
                |row| row.bg(rgb(0x665c32)),
            )
            .child(
                // A checkbox's UA margin is 3px 3px 3px 4px; Iced sets the top to 3px.
                div().flex_none().mt(px(3.)).ml(px(4.)).mr(px(3.)).child(
                    Checkbox::new(gpui::SharedString::from(format!("finding-{fid}")))
                        .checked(selected.contains(&fid))
                        .accessibility_label(finding.title.clone())
                        .disabled(delivered)
                        .debug_selector({
                            let fid = fid.clone();
                            move || format!("finding-select-{id}-{fid}")
                        })
                        .on_click(cx.listener({
                            let fid = fid.clone();
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
                ),
            )
            .child(
                div()
                    .debug_selector({
                        let fid = fid.clone();
                        move || format!("finding-main-{id}-{fid}")
                    })
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .min_w_0()
                            .child(
                                // `.sev`: mono 10.5/14, padding 0 5px, radius 4px.
                                div()
                                    .debug_selector({
                                        let fid = fid.clone();
                                        move || format!("finding-severity-{id}-{fid}")
                                    })
                                    .flex_none()
                                    .px(px(5.))
                                    .rounded(px(4.))
                                    .bg(rgb(badge_bg))
                                    .text_color(rgb(badge_fg))
                                    .font_family("Menlo")
                                    .text_size(px(appearance.size(10.5)))
                                    .line_height(gpui::relative(1.2))
                                    .child(severity),
                            )
                            // The text node between the badge and the id.
                            .child(div().flex_none().child("\u{a0}"))
                            .child(
                                // `.dlg-fid`: mono 11.5/14, muted, then a space.
                                div()
                                    .debug_selector({
                                        let fid = fid.clone();
                                        move || format!("finding-id-{id}-{fid}")
                                    })
                                    .flex_none()
                                    .font_family("Menlo")
                                    .text_size(px(appearance.size(11.5)))
                                    .text_color(rgb(palette.muted))
                                    .child(format!("{fid}\u{a0}")),
                            )
                            .child(
                                // `.dlg-ftitle`: weight 600.
                                div()
                                    .debug_selector({
                                        let fid = fid.clone();
                                        move || format!("finding-title-{id}-{fid}")
                                    })
                                    .flex_1()
                                    .min_w_0()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(finding.title.clone()),
                            ),
                    )
                    .children(location.map(|location| {
                        let file = file.clone().expect("a location has a file");
                        let tooltip = format!("Open {} in the file viewer", file.display());
                        // `.dlg-loc`: mono 11.5/14 accent, underlined, a pointer,
                        // the whole location (no ellipsis), as wide as its text.
                        div().flex().min_w_0().child(
                            div()
                                .id(gpui::SharedString::from(format!("finding-location-{fid}")))
                                .debug_selector({
                                    let fid = fid.clone();
                                    move || format!("finding-location-{id}-{fid}")
                                })
                                .min_w_0()
                                .cursor_pointer()
                                .font_family("Menlo")
                                .text_size(px(appearance.size(11.5)))
                                .text_color(rgb(palette.accent))
                                .underline()
                                .text_decoration_color(rgb(palette.accent))
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(tooltip.clone())
                                        .build(window, cx)
                                })
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    open(
                                        TranscriptAction::OpenFile {
                                            path: file.clone(),
                                            review: false,
                                        },
                                        cx,
                                    );
                                })
                                .child(location),
                        )
                    }))
                    .when(!finding.body.is_empty(), |main| {
                        // `.dlg-fbody`: muted 12/14, margin-top 2px.
                        main.child(
                            div()
                                .debug_selector(move || format!("finding-body-{id}-{fid}"))
                                .mt(px(2.))
                                .text_color(rgb(palette.muted))
                                .text_size(px(appearance.size(12.)))
                                .child(finding.body),
                        )
                    }),
            )
            .into_any_element()
    }
    /// Iced's `.dlg-body` blocks, each with its class's size, colour and box.
    /// Their vertical margins collapse as Iced's block margins do.
    fn render_card_parts(
        &self,
        parts: Vec<crate::delegation::CardPart>,
        cx: &mut Context<Self>,
    ) -> (Vec<gpui::AnyElement>, f32) {
        use crate::delegation::CardPart;
        let appearance = self.appearance;
        let palette = appearance.palette();
        let id = self.row.id;
        let query = self.query.clone();
        let hit = move |text: &str| !query.is_empty() && !find_ranges(text, &query).is_empty();
        let mut previous_bottom = 0.;
        let mut elements = Vec::new();
        // Each block's CSS margin top and bottom; adjacent ones collapse.
        let mut block = |top: f32, bottom: f32| {
            let block = div()
                .min_w_0()
                .mt(px(f32::max(top - previous_bottom, 0.)))
                .mb(px(bottom));
            previous_bottom = bottom;
            block
        };
        for (index, part) in parts.into_iter().enumerate() {
            match part {
                CardPart::Path(text) => elements.push(
                    block(2., 6.)
                        .debug_selector(move || format!("delegation-path-{id}"))
                        .font_family("Menlo")
                        .text_size(px(appearance.size(11.5)))
                        .text_color(rgb(palette.muted))
                        .when(hit(&text), |row| row.bg(rgb(0x665c32)))
                        .child(text)
                        .into_any_element(),
                ),
                CardPart::Banner(text) => elements.push(
                    block(4., 6.)
                        .debug_selector(move || format!("delegation-banner-{id}"))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(palette.warn))
                        .text_color(rgb(palette.warn))
                        .text_size(px(appearance.size(12.)))
                        .when(hit(&text), |row| row.bg(rgb(0x665c32)))
                        .child(text)
                        .into_any_element(),
                ),
                CardPart::Error(text) => elements.push(
                    block(4., 6.)
                        .id(("delegation-error", id))
                        .debug_selector(move || format!("delegation-error-{id}"))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(palette.err))
                        .text_color(rgb(palette.err))
                        .font_family("Menlo")
                        .text_size(px(appearance.size(12.)))
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .when(hit(&text), |row| row.bg(rgb(0x665c32)))
                        .child(text)
                        .into_any_element(),
                ),
                CardPart::Line(text) => elements.push(
                    block(2., 6.)
                        .debug_selector(move || format!("delegation-line-{id}-{index}"))
                        .text_color(rgb(palette.muted))
                        .text_size(px(appearance.size(12.)))
                        .when(hit(&text), |row| row.bg(rgb(0x665c32)))
                        .child(text)
                        .into_any_element(),
                ),
                CardPart::Activity(text) => elements.push(
                    block(0., 0.)
                        .debug_selector(move || format!("delegation-activity-{id}"))
                        .font_family("Menlo")
                        .text_size(px(appearance.size(11.5)))
                        .text_color(rgb(palette.muted))
                        .text_ellipsis()
                        .when(hit(&text), |row| row.bg(rgb(0x665c32)))
                        .child(text)
                        .into_any_element(),
                ),
                CardPart::Summary(text) => elements.push(
                    block(4., 6.)
                        .debug_selector(move || format!("delegation-summary-{id}"))
                        .when(hit(&text), |row| row.bg(rgb(0x665c32)))
                        .child(text)
                        .into_any_element(),
                ),
                // Iced renders the handoff summary as `.assistant` markdown.
                // It is the row's text view, so it keeps selection, copy and
                // find highlights.
                CardPart::Report(_) => elements.push(
                    block(6., 10.)
                        .debug_selector(move || format!("delegation-report-{id}"))
                        .child(self.copyable_text(cx))
                        .into_any_element(),
                ),
                CardPart::Section(title, items) => {
                    // `.dlg-section`: uppercase 11/14 with letter-spacing .05em,
                    // which GPUI text lacks: each letter is followed by that gap.
                    let size = appearance.size(11.);
                    elements.push(
                        block(6., 2.)
                            .debug_selector(move || format!("delegation-section-{id}-{index}"))
                            .flex()
                            .text_size(px(size))
                            .text_color(rgb(palette.muted))
                            .children(
                                section_letters(title)
                                    .into_iter()
                                    .map(|letter| div().mr(px(size * 0.05)).child(letter)),
                            )
                            .into_any_element(),
                    );
                    // A plain `<ul>`: 1em margins, content 40px in, the disc
                    // marker outside it, ending where the content starts.
                    let em = appearance.size(13.);
                    elements.push(
                        block(em, em)
                            .flex()
                            .flex_col()
                            .children(items.into_iter().map(|item| {
                                div()
                                    .flex()
                                    .when(hit(&item), |row| row.bg(rgb(0x665c32)))
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(40.))
                                            .flex()
                                            .justify_end()
                                            .child("•\u{a0}"),
                                    )
                                    .child(
                                        div()
                                            .debug_selector(move || {
                                                format!("delegation-section-item-{id}-{index}")
                                            })
                                            .flex_1()
                                            .min_w_0()
                                            .child(item),
                                    )
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        (elements, previous_bottom)
    }
}

/// `.dlg-section` text, uppercased, one string per letter for its spacing.
pub(super) fn section_letters(title: &str) -> Vec<String> {
    // A lone space is kept as a no-break space so its width survives.
    title
        .to_uppercase()
        .chars()
        .map(|c| if c == ' ' { "\u{a0}".into() } else { c.into() })
        .collect()
}
