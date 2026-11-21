use crate::model::AggregateModel;
use gitterm_core::{
    git_facts::{BaseChoice, DiffSource},
    review::{
        model::{FileStatus, LineOp, ReviewFile, ReviewSource},
        state::{Effect, Intent},
        viewed::{ViewedMarks, ViewedState},
    },
};
use gpui::{
    Context, InteractiveElement, IntoElement, ListHorizontalSizingBehavior, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, px, rgb, uniform_list,
};
use gpui_component::{
    Disableable, Selectable, Sizable,
    button::Button,
    list::ListItem,
    menu::{DropdownMenu, PopupMenuItem},
};
use std::{path::PathBuf, sync::Arc};

/// Only UI selection lives outside the authority.
pub(super) struct AggregateView {
    pub(super) model: AggregateModel,
    selected: Option<String>,
    bases: Vec<BaseChoice>,
    error: Option<String>,
    executor: Arc<tokio::runtime::Runtime>,
}

impl AggregateView {
    pub(super) fn new(root: PathBuf, source: ReviewSource, cx: &mut Context<Self>) -> Self {
        let executor = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("create aggregate review executor"),
        );
        let worker = executor.spawn(crate::repo::aggregate_target(root));
        // Keep the executor until the job finishes, even if this pane is dropped.
        let lifetime = executor.clone();
        cx.spawn(async move |view, cx| {
            let result = worker
                .await
                .unwrap_or_else(|error| Err(format!("Review branch worker failed: {error}")));
            _ = view.update(cx, |view, cx| match result {
                Ok((target, bases)) => {
                    view.bases = bases;
                    view.dispatch(
                        Intent::Open {
                            target,
                            source,
                            hide_noise: false,
                            focus: None,
                            page: AggregateModel::page(),
                        },
                        cx,
                    );
                }
                Err(error) => {
                    view.error = Some(error);
                    cx.notify();
                }
            });
            drop(lifetime);
        })
        .detach();
        let mut model = AggregateModel::default();
        // This preview never discovers or writes the user's settings directory.
        model.dispatch(Intent::MarksLoaded(Ok(ViewedMarks::default())));
        Self {
            model,
            selected: None,
            bases: Vec::new(),
            error: None,
            executor,
        }
    }

    pub(super) fn dispatch(&mut self, intent: Intent, cx: &mut Context<Self>) {
        let effects = self.model.dispatch(intent);
        let mut changed = false;
        for effect in effects {
            match effect {
                Effect::Collect(stamp) => {
                    self.error = None;
                    let worker = self
                        .executor
                        .spawn(crate::repo::collect_aggregate(stamp.clone()));
                    let lifetime = self.executor.clone();
                    cx.spawn(async move |view, cx| {
                        let intent = worker
                            .await
                            .unwrap_or_else(|error| Intent::CollectFinished {
                                stamp,
                                result: Box::new(Err(format!("Review collector failed: {error}"))),
                            });
                        _ = view.update(cx, |view, cx| view.dispatch(intent, cx));
                        drop(lifetime);
                    })
                    .detach();
                }
                Effect::Rejected(error) => {
                    self.error = Some(error);
                    changed = true;
                }
                Effect::Render | Effect::SaveMarks => {
                    self.error = None;
                    changed = true;
                }
                // No native control in this pane emits open-file or close IPC.
                Effect::Close | Effect::OpenFile(_) => {}
            }
        }
        // MarksLoaded and ToggleViewed need a native repaint even without Render.
        // A discarded completion has no effects and cannot disturb UI selection.
        if changed {
            let projection = self.model.projection();
            if projection.pane.is_some_and(|pane| pane.loaded.is_some())
                && !projection
                    .files
                    .iter()
                    .any(|row| Some(&row.file.path) == self.selected.as_ref())
            {
                self.selected = projection.files.first().map(|row| row.file.path.clone());
            }
        }
        cx.notify();
    }

    fn choose_base(&mut self, base: BaseChoice, cx: &mut Context<Self>) {
        if let Some(intent) = self.model.base_intent(base) {
            self.dispatch(intent, cx);
        }
    }

    fn select_commit(&mut self, sha: String, cx: &mut Context<Self>) {
        self.dispatch(self.model.commit_intent(sha), cx);
    }

    pub(super) fn toggle_viewed(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = &self.selected
            && let Some(intent) = self.model.viewed_intent(path)
        {
            self.dispatch(intent, cx);
        }
    }

    pub(super) fn navigate_to(&mut self, last: bool, cx: &mut Context<Self>) {
        let projection = self.model.projection();
        self.selected = if last {
            projection.files.last()
        } else {
            projection.files.first()
        }
        .map(|row| row.file.path.clone());
        cx.notify();
    }

    pub(super) fn navigate(&mut self, previous: bool, cx: &mut Context<Self>) {
        let projection = self.model.projection();
        if let Some(index) = projection
            .files
            .iter()
            .position(|row| Some(&row.file.path) == self.selected.as_ref())
        {
            let index = if previous {
                index.saturating_sub(1)
            } else {
                (index + 1).min(projection.files.len() - 1)
            };
            self.selected = Some(projection.files[index].file.path.clone());
            cx.notify();
        }
    }

    fn selected_file(&self) -> Option<&ReviewFile> {
        self.model
            .state()
            .pane()?
            .loaded
            .as_ref()?
            .0
            .file(self.selected.as_deref()?)
    }

    fn render_lines(&self, range: std::ops::Range<usize>) -> Vec<gpui::Div> {
        let Some(file) = self.selected_file() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for hunk in &file.hunks {
            rows.push((None, None, " ", hunk.header.as_str(), 0x89b4fa));
            for line in &hunk.lines {
                rows.push((
                    line.old_no,
                    line.new_no,
                    match line.op {
                        LineOp::Add => "+",
                        LineOp::Del => "−",
                        LineOp::Context => " ",
                    },
                    line.text.as_str(),
                    match line.op {
                        LineOp::Add => 0xa6e3a1,
                        LineOp::Del => 0xf38ba8,
                        LineOp::Context => 0xcdd6f4,
                    },
                ));
            }
        }
        rows.into_iter()
            .skip(range.start)
            .take(range.len())
            .map(|(old, new, sign, text, color)| {
                div()
                    .h(px(18.))
                    .flex()
                    .gap_3()
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .whitespace_nowrap()
                    .text_color(rgb(color))
                    .child(
                        div()
                            .w(px(48.))
                            .flex_shrink_0()
                            .child(old.map_or_else(String::new, |n| n.to_string())),
                    )
                    .child(
                        div()
                            .w(px(48.))
                            .flex_shrink_0()
                            .child(new.map_or_else(String::new, |n| n.to_string())),
                    )
                    .child(sign)
                    .child(text.to_string())
            })
            .collect()
    }
}

/// Code surfaces are monospace like Iced's `--mono`; "monospace" is not a macOS family.
pub(crate) const MONO_FONT: &str = "Menlo";

#[cfg(test)]
mod tests;

impl Render for AggregateView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let projection = self.model.projection();
        let pane = projection.pane;
        let source = pane.map(|pane| &pane.source);
        let no_base = pane.is_none_or(|pane| pane.target.base.is_none());
        let loading = pane.is_none_or(|pane| pane.loaded.is_none() && pane.error.is_none())
            && self.error.is_none();
        let bases = self.bases.clone();
        let selected_base = pane
            .and_then(|pane| pane.target.base.as_ref())
            .map(|base| base.reference.clone());
        let view = cx.entity().downgrade();
        let mut toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .p_2()
            .child(
                Button::new("aggregate-uncommitted")
                    .small()
                    .label("Uncommitted")
                    .selected(source == Some(&ReviewSource::Uncommitted))
                    .disabled(pane.is_none())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.dispatch(view.model.source_intent(DiffSource::Uncommitted), cx)
                    })),
            )
            .child(
                Button::new("aggregate-vs-base")
                    .small()
                    .label("Against base")
                    .selected(source == Some(&ReviewSource::VsBase))
                    .disabled(no_base)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.dispatch(view.model.source_intent(DiffSource::VsBase), cx)
                    })),
            )
            .child(
                Button::new("aggregate-commits")
                    .small()
                    .label("Commits")
                    .selected(matches!(source, Some(ReviewSource::Commits(_))))
                    .disabled(no_base)
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(intent) = view.model.commits_intent() {
                            view.dispatch(intent, cx);
                        }
                    })),
            )
            .child(
                Button::new("aggregate-base-picker")
                    .small()
                    .label(format!(
                        "Base: {} ▾",
                        pane.and_then(|pane| pane.target.base.as_ref())
                            .map_or("none", |base| base.reference.as_str())
                    ))
                    .disabled(self.bases.is_empty())
                    .dropdown_menu(move |mut menu, _, _| {
                        for base in &bases {
                            let choice = base.clone();
                            let view = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(base.reference.clone())
                                    .checked(selected_base.as_ref() == Some(&base.reference))
                                    .on_click(move |_, _, cx| {
                                        _ = view.update(cx, |view, cx| {
                                            view.choose_base(choice.clone(), cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .child(
                Button::new("aggregate-refresh")
                    .small()
                    .label("Refresh")
                    .disabled(pane.is_none())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.dispatch(
                            Intent::Collect {
                                page: AggregateModel::page(),
                            },
                            cx,
                        )
                    })),
            );
        if let Some(pane) = pane {
            toolbar = toolbar.child(format!("{} · {}", pane.target.repo, pane.target.branch));
        }
        let mut top = div().flex().flex_col().child(toolbar).child(
            div()
                .px_3()
                .text_color(rgb(0x9399b2))
                .child("Aggregate review · Viewed marks are kept for this preview session"),
        );
        for notice in &projection.notices {
            top = top.child(div().px_3().text_color(rgb(0xf9e2af)).child(notice.clone()));
        }
        for error in [
            self.error.as_deref(),
            pane.and_then(|pane| pane.error.as_deref()),
            projection.marks_error,
        ]
        .into_iter()
        .flatten()
        {
            top = top.child(
                div()
                    .px_3()
                    .text_color(rgb(0xf38ba8))
                    .child(error.to_string()),
            );
        }
        if loading {
            top = top.child(div().p_3().child("Loading review…"));
        }

        let mut sidebar = div()
            .id("aggregate-files")
            .w(px(320.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .bg(rgb(0x181825))
            .p_3();
        if let Some(pane) = pane
            && matches!(pane.source, ReviewSource::Commits(_))
        {
            sidebar = sidebar.child(
                div()
                    .py_2()
                    .child("Select a commit, then another for a range"),
            );
            for (index, commit) in pane.commits.iter().enumerate() {
                let sha = commit.sha.clone();
                let label = format!("{} {}", commit.short_sha, commit.subject);
                let selected = matches!(&pane.source, ReviewSource::Commits(Some(range)) if range.first == commit.sha || range.last == commit.sha);
                sidebar = sidebar.child(
                    ListItem::new(("aggregate-commit", index))
                        .accessibility_label(label.clone())
                        .child(label)
                        .selected(selected)
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.select_commit(sha.clone(), cx)),
                        ),
                );
            }
            if matches!(&pane.source, ReviewSource::Commits(Some(range)) if range.pending) {
                sidebar = sidebar.child(
                    div()
                        .py_2()
                        .child("Select another commit to finish the range"),
                );
            }
        }
        sidebar = sidebar.child(div().py_2().child("Changed files"));
        for (index, row) in projection.files.iter().enumerate() {
            let path = row.file.path.clone();
            let mark = match row.viewed {
                ViewedState::Viewed => "✓",
                ViewedState::ChangedSinceViewed => "Changed",
                ViewedState::Unviewed => "○",
            };
            let label = format!("{mark} {} {}", row.file.status.letter(), row.file.path);
            sidebar = sidebar.child(
                ListItem::new(("aggregate-file", index))
                    .accessibility_label(label.clone())
                    .child(div().font_family(MONO_FONT).text_size(px(12.)).child(label))
                    .selected(self.selected.as_ref() == Some(&row.file.path))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.selected = Some(path.clone());
                        cx.notify();
                    })),
            );
        }
        let mut body = div().flex_1().min_w_0().h_full().flex().flex_col();
        if let Some(row) = projection
            .files
            .iter()
            .find(|row| Some(&row.file.path) == self.selected.as_ref())
        {
            let file = row.file;
            let path = file.path.clone();
            let title = file
                .old_path
                .as_ref()
                .map_or_else(|| file.path.clone(), |old| format!("{old} → {}", file.path));
            body = body.child(
                div()
                    .p_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .font_family(MONO_FONT)
                            .text_size(px(12.5))
                            .child(format!("{title}  +{} −{}", file.additions, file.deletions)),
                    )
                    .child(
                        Button::new("aggregate-viewed")
                            .small()
                            .label(match row.viewed {
                                ViewedState::Viewed => "✓ Viewed",
                                ViewedState::ChangedSinceViewed => {
                                    "Changed since viewed · Mark viewed"
                                }
                                ViewedState::Unviewed => "Mark viewed",
                            })
                            .disabled(
                                file.content_id.is_none()
                                    || self
                                        .model
                                        .state()
                                        .marks()
                                        .is_none_or(|marks| marks.is_err()),
                            )
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(intent) = view.model.viewed_intent(&path) {
                                    view.dispatch(intent, cx);
                                }
                            })),
                    ),
            );
            if file.omitted {
                body = body.child(div().p_3().child("Diff omitted by the review budget."));
            }
            if file.truncated {
                body = body.child(div().p_3().child(
                    "Diff truncated at the review text limits; only collected lines are shown.",
                ));
            }
            if let Some(note) = &file.note {
                body = body.child(div().p_3().child(note.clone()));
            }
            if file.status == FileStatus::Binary {
                body = body.child(div().p_3().child("Binary file — no text diff"));
            } else if !file.hunks.is_empty() {
                let count = file.hunks.iter().map(|hunk| 1 + hunk.lines.len()).sum();
                body = body.child(
                    uniform_list(
                        "aggregate-lines",
                        count,
                        cx.processor(|view, range, _, _| view.render_lines(range)),
                    )
                    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                    .flex_1()
                    .min_h_0(),
                );
            } else if !file.omitted && file.note.is_none() {
                body = body.child(div().p_3().child("No text changes (metadata only)"));
            }
        } else if !loading {
            body = body.child(div().p_3().child(
                if matches!(source, Some(ReviewSource::Commits(None))) {
                    "Select commits to review"
                } else {
                    "No changed files"
                },
            ));
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(top)
            .child(div().flex_1().min_h_0().flex().child(sidebar).child(body))
    }
}
