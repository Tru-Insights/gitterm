use super::*;
use crate::review::*;
use gitterm_core::settings::ReviewerKind;
use gpui_component::{
    IndexPath,
    input::Textarea,
    radio::Radio,
    select::{Select, SelectEvent, SelectState},
};

pub(crate) struct ReviewComposer {
    pub open: bool,
    pub draft: ReviewDraft,
    context: Option<ReviewContext>,
    error: Option<String>,
    base: Entity<InputState>,
    sha: Entity<InputState>,
    codex_model: Entity<InputState>,
    reviewer: Entity<SelectState<Vec<String>>>,
    model: Entity<SelectState<Vec<String>>>,
    note: Entity<TextareaState>,
    _subscriptions: Vec<Subscription>,
}
impl ReviewComposer {
    pub fn new(window: &mut Window, cx: &mut Context<ChatView>) -> Self {
        let base = cx.new(|cx| InputState::new(window, cx).placeholder("base branch"));
        let sha = cx.new(|cx| InputState::new(window, cx).placeholder("SHA"));
        let codex_model =
            cx.new(|cx| InputState::new(window, cx).placeholder("Codex default model"));
        let reviewer = cx.new(|cx| {
            SelectState::new(
                REVIEWERS.map(String::from).to_vec(),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let model = cx.new(|cx| {
            SelectState::new(
                CLAUDE_MODELS.map(String::from).to_vec(),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let note = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder(FOCUS_PLACEHOLDER)
        });
        let mut subscriptions = vec![
            cx.subscribe(&reviewer, |view, _, event, cx| {
                let SelectEvent::Confirm(value) = event;
                view.review.draft.reviewer = if value.as_deref() == Some("Codex") {
                    ReviewerKind::Codex
                } else {
                    ReviewerKind::ClaudeSubagent
                };
                cx.notify();
            }),
            cx.subscribe(&model, |view, _, event, cx| {
                let SelectEvent::Confirm(value) = event;
                if let Some(value) = value {
                    view.review.draft.claude_model = value.clone();
                }
                cx.notify();
            }),
        ];
        for (index, input) in [&base, &sha, &codex_model].into_iter().enumerate() {
            subscriptions.push(cx.subscribe(input, move |view, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    match index {
                        0 => {
                            view.review.draft.base = value;
                            view.review.draft.target = Target::Branch;
                        }
                        1 => {
                            view.review.draft.sha = value;
                            view.review.draft.target = Target::Commit;
                        }
                        _ => view.review.draft.codex_model = value,
                    }
                    cx.notify();
                }
            }));
        }
        subscriptions.push(cx.subscribe(&note, |view, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                view.review.draft.note = input.read(cx).value().to_string();
                cx.notify();
            }
        }));
        Self {
            open: false,
            draft: ReviewDraft::default(),
            context: None,
            error: None,
            base,
            sha,
            codex_model,
            reviewer,
            model,
            note,
            _subscriptions: subscriptions,
        }
    }
    pub fn load(
        &mut self,
        context: ReviewContext,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) {
        if !self.open {
            return;
        }
        self.draft = ReviewDraft::from_context(self.draft.kind, &context);
        self.base.update(cx, |input, cx| {
            input.set_value(self.draft.base.clone(), window, cx)
        });
        self.sha.update(cx, |input, cx| {
            input.set_value(self.draft.sha.clone(), window, cx)
        });
        self.codex_model.update(cx, |input, cx| {
            input.set_value(self.draft.codex_model.clone(), window, cx)
        });
        self.reviewer.update(cx, |select, cx| {
            select.set_selected_index(
                Some(
                    IndexPath::default()
                        .row(usize::from(self.draft.reviewer == ReviewerKind::Codex)),
                ),
                window,
                cx,
            )
        });
        let mut models = CLAUDE_MODELS.map(String::from).to_vec();
        if !models.contains(&self.draft.claude_model) {
            models.insert(0, self.draft.claude_model.clone());
        }
        let selected = models
            .iter()
            .position(|model| model == &self.draft.claude_model)
            .unwrap();
        self.model.update(cx, |select, cx| {
            select.set_items(models, window, cx);
            select.set_selected_index(Some(IndexPath::default().row(selected)), window, cx);
        });
        self.context = Some(context);
        self.reset_note(window, cx);
    }
    fn choose_kind(&mut self, kind: ReviewKind, window: &mut Window, cx: &mut Context<ChatView>) {
        if self.draft.kind == kind {
            return;
        }
        let reviewer = self.draft.reviewer;
        self.draft.kind = kind;
        if let Some(context) = self.context.clone() {
            self.load(context, window, cx);
            self.draft.reviewer = reviewer;
            self.reviewer.update(cx, |select, cx| {
                select.set_selected_index(
                    Some(IndexPath::default().row(usize::from(reviewer == ReviewerKind::Codex))),
                    window,
                    cx,
                )
            });
        }
        self.error = None;
        cx.notify();
    }
    fn reset_note(&mut self, window: &mut Window, cx: &mut Context<ChatView>) {
        self.draft.note.clear();
        self.note.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(
                if self.draft.kind == ReviewKind::Review {
                    FOCUS_PLACEHOLDER
                } else {
                    BRIEF_PLACEHOLDER
                },
                window,
                cx,
            );
            input.focus_handle(cx).focus(window, cx);
        });
    }
    pub fn render(
        &self,
        busy: bool,
        appearance: ChatAppearance,
        cx: &mut Context<ChatView>,
    ) -> impl IntoElement {
        let palette = appearance.palette();
        let label = |text: &'static str| {
            div()
                .mt(px(8.))
                .text_size(px(appearance.size(11.)))
                .text_color(rgb(palette.muted))
                .child(text.to_uppercase())
        };
        let hint = |text: String| {
            div()
                .ml(px(22.))
                .text_size(px(appearance.size(11.)))
                .text_color(rgb(palette.muted))
                .child(text)
        };
        let row = || div().flex().gap(px(6.)).items_center().my(px(4.));
        let codex = self.draft.reviewer == ReviewerKind::Codex;
        let blocked = self.draft.blocked_reason(busy);
        let mut panel = div()
            .id("review-composer")
            .debug_selector(|| "review-composer".into())
            .absolute()
            .bottom(gpui::relative(1.))
            .left_0()
            .mb(px(6.))
            .w(px(340.))
            .max_h(px(600.))
            .overflow_y_scroll()
            .px(px(12.))
            .py(px(10.))
            .bg(rgb(palette.alt))
            .border_1()
            .border_color(rgb(palette.strong))
            .rounded(px(8.))
            .font_family("Menlo")
            .text_size(px(appearance.size(12.5)))
            .text_color(rgb(palette.fg))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(row().children(KINDS.map(|(kind, text)| {
                Radio::new(text)
                    .debug_selector(move || format!("review-kind-{}", text.to_lowercase()))
                    .label(text)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .checked(self.draft.kind == kind)
                    .on_change(cx.listener(move |view, _, window, cx| {
                        view.review.choose_kind(kind, window, cx);
                        cx.notify();
                    }))
            })));
        let Some(context) = &self.context else {
            return panel.child(hint("Reading the repository…".into()));
        };
        panel = panel.child(label(self.draft.kind.reviewer_label()))
            .child(row().child(Select::new(&self.reviewer).w(px(150.)).bg(rgb(palette.alt)).text_color(rgb(palette.fg)).text_size(px(appearance.size(12.))).border_color(rgb(palette.strong)).rounded(px(5.))).child(if codex { Input::new(&self.codex_model).aria_label("Codex default model").bg(rgb(palette.alt)).text_color(rgb(palette.fg)).text_size(px(appearance.size(12.))).border_color(rgb(palette.strong)).rounded(px(5.)).into_any_element() } else { Select::new(&self.model).bg(rgb(palette.alt)).text_color(rgb(palette.fg)).text_size(px(appearance.size(12.))).border_color(rgb(palette.strong)).rounded(px(5.)).into_any_element() }))
            .child(hint(if codex { "Runs in the background in this checkout; a card shows its progress and result. No Claude turn is spent." } else { "Claude spawns one subagent in this chat; its report comes back as a reply." }.into()));
        if self.draft.kind == ReviewKind::Review {
            panel = panel
                .children(
                    context
                        .error
                        .iter()
                        .map(|error| hint(error.clone()).text_color(rgb(palette.warn))),
                )
                .child(label("Target"));
            for (target, text) in TARGETS {
                let mut target_row = row().child(
                    Radio::new(text)
                        .debug_selector(move || {
                            format!(
                                "review-target-{}",
                                match target {
                                    Target::Branch => "branch",
                                    Target::Uncommitted => "uncommitted",
                                    Target::Commit => "commit",
                                }
                            )
                        })
                        .label(text)
                        .checked(self.draft.target == target)
                        .on_change(cx.listener(move |view, _, _, cx| {
                            view.review.draft.target = target;
                            cx.notify();
                        })),
                );
                target_row = match target {
                    Target::Branch => target_row.child(
                        Input::new(&self.base)
                            .aria_label("base branch")
                            .bg(rgb(palette.alt))
                            .text_color(rgb(palette.fg))
                            .text_size(px(appearance.size(12.)))
                            .border_color(rgb(palette.strong))
                            .rounded(px(5.)),
                    ),
                    Target::Commit => target_row.child(
                        Input::new(&self.sha)
                            .aria_label("SHA")
                            .bg(rgb(palette.alt))
                            .text_color(rgb(palette.fg))
                            .text_size(px(appearance.size(12.)))
                            .border_color(rgb(palette.strong))
                            .rounded(px(5.)),
                    ),
                    Target::Uncommitted => target_row,
                };
                panel = panel.child(target_row);
                if target == Target::Branch {
                    panel = panel.child(hint(context.base_hint()).text_color(rgb(if context.base.is_some() { palette.muted } else { palette.warn })))
                        .children((codex && self.draft.target == Target::Branch).then(|| hint("Codex compares the working tree with the merge base, so uncommitted changes are included.".into())));
                } else if target == Target::Uncommitted {
                    panel = panel.child(hint(
                        if context.dirty {
                            "Staged, unstaged and untracked changes against HEAD"
                        } else {
                            "The working tree is clean"
                        }
                        .into(),
                    ));
                }
            }
        }
        panel
            .children(self.error.iter().map(|error| {
                div()
                    .id("review-error")
                    .debug_selector(|| "review-error".into())
                    .text_color(rgb(palette.err))
                    .child(error.clone())
            }))
            .child(label(self.draft.kind.note_label()))
            .child(
                div()
                    .min_h(px(if self.draft.kind == ReviewKind::Review {
                        44.
                    } else {
                        90.
                    }))
                    .child(
                        Textarea::new(&self.note)
                            .aria_label(self.draft.kind.note_label())
                            .bg(rgb(palette.alt))
                            .text_color(rgb(palette.fg))
                            .text_size(px(appearance.size(12.)))
                            .border_color(rgb(palette.strong))
                            .rounded(px(5.)),
                    ),
            )
            .child(
                row()
                    .justify_end()
                    .mt(px(8.))
                    .child(
                        Button::new("review-cancel")
                            .debug_selector(|| "review-cancel".into())
                            .label("Cancel")
                            .bg(rgb(palette.block))
                            .text_color(rgb(palette.fg))
                            .border_color(rgb(palette.strong))
                            .text_size(px(appearance.size(13.)))
                            .px(px(14.))
                            .py(px(7.))
                            .h_auto()
                            .on_click(
                                cx.listener(|view, _, window, cx| view.close_review(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("review-submit")
                            .debug_selector(|| "review-submit".into())
                            .label(self.draft.kind.submit_label())
                            .bg(rgb(palette.accent))
                            .text_color(rgb(palette.base))
                            .border_color(rgb(palette.accent))
                            .text_size(px(appearance.size(13.)))
                            .px(px(14.))
                            .py(px(7.))
                            .h_auto()
                            .disabled(blocked.is_some())
                            .tooltip(blocked.unwrap_or_default())
                            .on_click(cx.listener(|view, _, _, cx| view.submit_review(cx))),
                    ),
            )
    }
}
impl ChatView {
    pub(crate) fn toggle_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.open {
            self.close_review(window, cx);
            return;
        }
        self.composer_state.picker = None;
        self.review.open = true;
        self.review.context = None;
        self.review.error = None;
        self.focus.focus(window, cx);
        cx.emit(ReviewOutcome::ContextRequested);
        cx.notify();
    }
    pub(crate) fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        self.focus_composer(window, cx);
        cx.notify();
    }
    pub fn set_review_context(
        &mut self,
        context: ReviewContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review.load(context, window, cx);
        cx.notify();
    }
    pub fn submit_review(&mut self, cx: &mut Context<Self>) {
        if self.review.open
            && self.review.context.is_some()
            && let Some(outcome) = self
                .review
                .draft
                .submit(self.composer_state.streaming || self.composer_state.pending_request)
        {
            cx.emit(outcome);
        }
    }
    /// Returns whether the popover consumed the host's Escape action.
    pub fn dismiss_review(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.review.open {
            return false;
        }
        self.close_review(window, cx);
        true
    }
    pub fn show_review_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.review.error = Some(error);
        cx.notify();
    }
}
