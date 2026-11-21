//! Iced's PR action confirmation (`view_pr_action_confirm`, work_prs.rs), as a
//! preview-safe panel over the Work sidebar's expanded-PR actions.
//!
//! Approve, Merge and Ready for review open it; nothing runs from it. Choosing
//! a merge method and Cancel are local. The verb button reports the preview
//! limitation like every other write control: no `gh` call, no network.
//!
//! [`Model`] holds every string and tone the panel shows, built from the live
//! PR read on each render, so tests assert on what is drawn.

use gitterm_core::{
    merge_plan::{MergeMethod, PrKey, PrState, RowStatus},
    prs::{self, ChecksState, PrAction, ReviewDecision, WatchedPr},
    work::summary,
};
use gpui::{
    Anchor, Context, FocusHandle, Window, anchored, deferred, div, point, prelude::*, px, rgb, rgba,
};
use gpui_component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use crate::{
    theme::{Palette, UI_FONT_SIZE},
    work_sidebar::WorkSidebar,
};

/// Which row action the panel confirms (Iced `ActionKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Approve,
    Merge,
    /// Mark a draft ready for review.
    Ready,
}

/// Open panel state (Iced `PrActionConfirm`, minus `running`: nothing runs).
pub(crate) struct PrConfirm {
    pub key: PrKey,
    pub kind: Kind,
    pub method: MergeMethod,
    /// The plan named the method.
    method_from_plan: bool,
    /// The mapped gh account; `None` for gh's active account.
    account: Option<String>,
    /// The preview notice after the verb button, in Iced's error slot.
    pub error: Option<String>,
    pub focus: FocusHandle,
}

impl PrConfirm {
    pub fn new(
        key: PrKey,
        kind: Kind,
        plan_method: Option<MergeMethod>,
        account: Option<String>,
        focus: FocusHandle,
    ) -> Self {
        Self {
            key,
            kind,
            method: plan_method.unwrap_or_default(),
            method_from_plan: plan_method.is_some(),
            account,
            error: None,
            focus,
        }
    }

    fn action(&self) -> PrAction {
        match self.kind {
            Kind::Approve => PrAction::Approve,
            Kind::Merge => PrAction::Merge(self.method),
            Kind::Ready => PrAction::Ready,
        }
    }

    pub fn verb(&self) -> &'static str {
        self.action().verb()
    }

    /// Iced disables the verb button for an account that is not a login.
    pub fn account_ok(&self) -> bool {
        self.account.as_deref().is_none_or(prs::valid_login)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Secondary,
    Muted,
    Accent,
    Success,
    Warning,
    Danger,
    Merged,
}

impl Tone {
    fn color(self, p: &Palette) -> u32 {
        match self {
            Tone::Secondary => p.subtext0,
            Tone::Muted => p.muted,
            Tone::Accent => p.blue,
            Tone::Success => p.green,
            Tone::Warning => p.yellow,
            Tone::Danger => p.red,
            // The palette has no mauve; a merged PR cannot be confirmed.
            Tone::Merged => p.lavender,
        }
    }
}

/// Everything the panel shows, in Iced's order.
#[derive(Debug, PartialEq)]
pub(crate) struct Model {
    pub heading: String,
    pub pr_title: Option<String>,
    /// The PR's live line, monospace: state, checks, review, comments, conflicts.
    pub live: Vec<(String, Tone)>,
    pub plan_note: Option<(String, Tone)>,
    pub account_line: String,
    pub account_tone: Tone,
    /// Merge only: the three methods, the selected flag, and the hint.
    pub methods: Option<(Vec<(MergeMethod, bool)>, &'static str)>,
    pub command: String,
    pub error: Option<String>,
    pub close_label: &'static str,
    pub run_label: &'static str,
    pub run_enabled: bool,
}

impl Model {
    pub fn build(
        confirm: &PrConfirm,
        watched: Option<&WatchedPr>,
        plan_status: Option<&RowStatus>,
    ) -> Self {
        let action = confirm.action();
        let key = &confirm.key;
        let owner = key.owner().to_string();
        let (account_line, account_tone) = match &confirm.account {
            Some(user) if prs::valid_login(user) => (
                format!("Runs as {user} (github_accounts maps {owner} to it)"),
                Tone::Secondary,
            ),
            Some(user) => (
                format!(
                    "github_accounts maps {owner} to {user:?}, which is not a GitHub login; fix github-accounts.json"
                ),
                Tone::Danger,
            ),
            // Iced names gh's active login here when it has read one; the
            // preview never runs gh.
            None => (
                format!("Runs as gh's active account: github_accounts has no entry for {owner}"),
                Tone::Secondary,
            ),
        };
        Self {
            heading: format!("{} {}", action.verb(), key.label()),
            pr_title: watched
                .and_then(|w| w.snapshot.core.as_ref())
                .map(|core| summary::shorten(&core.title, 90)),
            live: live_line(watched),
            plan_note: plan_status.and_then(plan_note),
            account_line,
            account_tone,
            methods: (confirm.kind == Kind::Merge).then(|| {
                (
                    MergeMethod::ALL
                        .map(|method| (method, method == confirm.method))
                        .to_vec(),
                    if confirm.method_from_plan {
                        "(the plan's)"
                    } else {
                        "(default; the plan names none)"
                    },
                )
            }),
            command: prs::action_command_line(action, key, confirm.account.as_deref()),
            error: confirm.error.as_deref().map(|e| summary::shorten(e, 400)),
            close_label: if confirm.error.is_some() {
                "Close"
            } else {
                "Cancel"
            },
            run_label: action.verb(),
            run_enabled: confirm.account_ok(),
        }
    }
}

/// Iced `pr_live_line`.
fn live_line(watched: Option<&WatchedPr>) -> Vec<(String, Tone)> {
    let Some(watched) = watched else {
        return vec![("not read yet".into(), Tone::Muted)];
    };
    let Some(core) = watched.snapshot.core.as_ref() else {
        let note = if watched.error.is_some() {
            "not read"
        } else {
            "not read yet"
        };
        return vec![(note.into(), Tone::Muted)];
    };
    let mut line = vec![];
    if let Some(state) = core.state {
        let tone = match state {
            PrState::Open => Tone::Secondary,
            PrState::Draft => Tone::Muted,
            PrState::Merged => Tone::Merged,
            PrState::Closed => Tone::Danger,
        };
        line.push((state.label().to_string(), tone));
    }
    if core.state.is_some_and(PrState::is_final) {
        return line;
    }
    let s = &watched.snapshot;
    let checks = s.checks();
    line.push((
        checks.label().into(),
        match checks {
            ChecksState::Pass => Tone::Success,
            ChecksState::Fail => Tone::Danger,
            ChecksState::Pending => Tone::Warning,
            ChecksState::None => Tone::Muted,
        },
    ));
    let review = s.review_decision();
    line.push((
        review.label().into(),
        match review {
            ReviewDecision::Approved => Tone::Success,
            ReviewDecision::ChangesRequested => Tone::Danger,
            _ => Tone::Muted,
        },
    ));
    if watched.unseen_comments > 0 {
        line.push((
            format!(
                "{} new comment{}",
                watched.unseen_comments,
                if watched.unseen_comments == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            Tone::Accent,
        ));
    }
    if s.conflicting() == Some(true) {
        line.push(("conflicts".into(), Tone::Danger));
    }
    line
}

/// Where the plan stands on the PR (Iced's note under the live line).
fn plan_note(status: &RowStatus) -> Option<(String, Tone)> {
    match status {
        RowStatus::Next => Some(("the plan's next to merge".into(), Tone::Accent)),
        RowStatus::Blocked(waits) => Some((
            format!("the plan has it wait for {}", waits.join(", ")),
            Tone::Warning,
        )),
        RowStatus::Unordered(reason) => Some((format!("unordered: {reason}"), Tone::Warning)),
        RowStatus::Held(hold) => Some((format!("the plan holds it: {hold}"), Tone::Warning)),
        _ => None,
    }
}

fn text(label: impl Into<gpui::SharedString>, size: f32, color: u32) -> gpui::Div {
    div()
        .text_size(px(size))
        .text_color(rgb(color))
        .child(label.into())
}

fn sheet_button(id: &'static str, label: &'static str) -> Button {
    Button::new(id)
        .accessibility_label(label)
        .child(div().text_size(px(UI_FONT_SIZE - 1.)).child(label))
        .small()
        .h(px(28.))
        .px(px(12.))
        .rounded(px(6.))
}

/// The sheet: a card centred over a dimmed backdrop covering the window, as
/// Iced's `view_sheet_card`. Drawn deferred so the narrow sidebar neither
/// clips nor lays it out.
pub(crate) fn view(
    model: Model,
    focus: &FocusHandle,
    window: &Window,
    cx: &mut Context<WorkSidebar>,
) -> impl IntoElement {
    let p = Palette::current(cx);
    let font = UI_FONT_SIZE;
    let small = UI_FONT_SIZE - 1.;
    let mut card = div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .w(px(560.))
        .py(px(20.))
        .px(px(24.))
        .bg(rgb(p.mantle))
        .border_1()
        .border_color(rgb(p.border))
        .rounded(px(8.))
        .child(text(model.heading.clone(), font, p.text));
    if let Some(title) = &model.pr_title {
        card = card.child(text(title.clone(), small, p.subtext0));
    }
    card = card.child(
        div().flex().items_center().gap(px(6.)).children(
            model
                .live
                .iter()
                .map(|(part, tone)| text(part.clone(), 10., tone.color(&p)).font_family("Menlo")),
        ),
    );
    if let Some((note, tone)) = &model.plan_note {
        card = card.child(text(note.clone(), small - 1., tone.color(&p)));
    }
    card = card.child(text(
        model.account_line.clone(),
        small,
        model.account_tone.color(&p),
    ));
    if let Some((methods, hint)) = &model.methods {
        let mut row =
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(text("Method:", small - 1., p.muted));
        for &(method, selected) in methods {
            let color = if selected { p.blue } else { p.text };
            row = row.child(
                Button::new(("pr-confirm-method", method as usize))
                    .debug_selector(move || format!("pr-confirm-method-{}", method.label()))
                    .accessibility_label(method.label())
                    .outline()
                    .small()
                    .h_auto()
                    .py(px(2.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .child(text(method.label(), small - 1., color))
                    .on_click(cx.listener(move |this, _, _, cx| this.confirm_method(method, cx))),
            );
        }
        card = card.child(row.child(text(*hint, 10., p.muted)));
    }
    card = card.child(
        div()
            .py(px(6.))
            .px(px(8.))
            .w_full()
            .bg(rgb(p.crust))
            .rounded(px(4.))
            .child(text(model.command.clone(), small, p.text).font_family("Menlo")),
    );
    if let Some(error) = &model.error {
        card = card.child(text(error.clone(), small - 1., p.red));
    }
    let backdrop = if p.base == 0x1e1e2e {
        0x11111bcc
    } else {
        0xdce0e8cc
    };
    let size = window.viewport_size();
    deferred(
        anchored()
            .position(point(px(0.), px(0.)))
            .anchor(Anchor::TopLeft)
            .child(
                div()
                    .id("pr-confirm")
                    .debug_selector(|| "pr-confirm".into())
                    .occlude()
                    .track_focus(focus)
                    .key_context("PrConfirm")
                    .w(size.width)
                    .h(size.height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(backdrop))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.confirm_close(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        card.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap(px(8.))
                                .child(
                                    sheet_button("pr-confirm-close", model.close_label)
                                        .debug_selector(|| "pr-confirm-cancel".into())
                                        .outline()
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.confirm_close(cx)),
                                        ),
                                )
                                .child(
                                    sheet_button("pr-confirm-run", model.run_label)
                                        .debug_selector(|| "pr-confirm-run".into())
                                        .primary()
                                        .disabled(!model.run_enabled)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.confirm_run(cx)),
                                        ),
                                ),
                        ),
                    ),
            ),
    )
    .with_priority(1)
}
