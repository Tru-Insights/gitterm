use crate::host::ReviewHost;
use crate::model::AggregateModel;
use gitterm_core::{
    git_facts::{BaseChoice, DiffSource},
    review::{
        model::{EditReview, FileStatus, LineOp, ReviewFile, ReviewSource},
        state::{Effect, Intent},
        viewed::ViewedState,
    },
};
use gpui::{
    App, Context, Global, InteractiveElement, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement, Render, StatefulInteractiveElement, StyleRefinement, Styled, Window, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme, Disableable, Selectable, Sizable,
    button::Button,
    checkbox::Checkbox,
    list::ListItem,
    menu::{DropdownMenu, PopupMenuItem},
    text::{TextView, TextViewStyle},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

/// Only UI selection lives outside the authority.
pub(super) struct AggregateView {
    pub(super) model: AggregateModel,
    selected: Option<String>,
    ui: AggregateUi,
    bases: Vec<BaseChoice>,
    error: Option<String>,
    executor: Arc<tokio::runtime::Runtime>,
    pub(super) host: ReviewHost,
}

impl AggregateView {
    pub(super) fn new(
        root: PathBuf,
        source: ReviewSource,
        host: ReviewHost,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::open(root, source, None, host, cx)
    }

    pub(super) fn open_edit(open: EditReview, host: ReviewHost, cx: &mut Context<Self>) -> Self {
        Self::open(
            open.target.checkout.clone(),
            open.source.into(),
            Some(open),
            host,
            cx,
        )
    }

    fn open(
        root: PathBuf,
        source: ReviewSource,
        edit: Option<EditReview>,
        host: ReviewHost,
        cx: &mut Context<Self>,
    ) -> Self {
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
                    let (target, focus) = match edit {
                        Some(open) => (open.target, Some(open.focus)),
                        None => (target, None),
                    };
                    let hide_noise = noise_choice(&target.checkout, cx)
                        && !focus.as_deref().is_some_and(|path| {
                            gitterm_core::review::model::classify_path(path).is_noise()
                        });
                    view.dispatch(
                        Intent::Open {
                            hide_noise,
                            target,
                            source,
                            focus,
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
        // Iced's marks are read-only here; the host's preview root is the only write target.
        model.dispatch(Intent::MarksLoaded(crate::marks::load_merged(
            host.iced_marks_dir.as_deref(),
            host.preview_marks_dir.as_deref(),
        )));
        Self {
            model,
            selected: None,
            ui: AggregateUi::default(),
            bases: Vec::new(),
            error: None,
            executor,
            host,
        }
    }

    pub(super) fn dispatch(&mut self, intent: Intent, cx: &mut Context<Self>) {
        let focus = self
            .model
            .state()
            .pane()
            .filter(|pane| pane.loaded.is_none())
            .and_then(|pane| pane.focus.clone());
        let toggle = match &intent {
            Intent::ToggleViewed { path, viewed } => Some((path.clone(), *viewed)),
            _ => None,
        };
        let effects = self.model.dispatch(intent);
        if let Some((path, viewed)) = toggle
            && self
                .model
                .projection()
                .files
                .iter()
                .any(|row| row.file.path == path && (row.viewed == ViewedState::Viewed) == viewed)
        {
            self.ui.collapsed.insert(path, viewed);
        }
        let mut changed = false;
        for effect in effects {
            match effect {
                Effect::Collect(stamp) => {
                    self.error = None;
                    let worker = self.executor.spawn(crate::repo::collect_aggregate_with(
                        stamp.clone(),
                        self.host.snapshot_roots.clone(),
                    ));
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
                Effect::Render => {
                    self.error = None;
                    changed = true;
                }
                Effect::SaveMarks => {
                    self.error = self.save_marks().err();
                    changed = true;
                }
                Effect::OpenFile(full) => {
                    if !full.is_file() {
                        self.error = Some(format!("open: {} is not a file", full.display()));
                    } else if let Some(open) = self.host.open_file.clone() {
                        open(full, cx);
                    }
                    changed = true;
                }
                // No native control in this pane emits close IPC.
                Effect::Close => {}
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
        self.sync_rows();
        if changed
            && let Some(path) = focus
            && let Some(index) = self
                .ui
                .rows
                .iter()
                .position(|row| matches!(row, AggregateRow::Header(file) if file == &path))
        {
            self.selected = Some(path);
            // New rows have no measured heights yet. Anchor the section directly;
            // scroll_to_reveal_item only works once the list has laid them out.
            self.ui.list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }

    /// Persists the merged marks into the host's preview root. With no writer the
    /// marks stay in memory for this session.
    fn save_marks(&self) -> Result<(), String> {
        let (Some(save), Some(Ok(marks))) = (&self.host.save_marks, self.model.state().marks())
        else {
            return Ok(());
        };
        crate::marks::overlay(
            self.host.iced_marks_dir.as_deref(),
            self.host.preview_marks_dir.as_deref(),
            marks,
        )?
        .encode()
        .and_then(|bytes| save(bytes))
    }

    fn select_turn(&mut self, number: u64, cx: &mut Context<Self>) {
        if let Some(intent) = self.model.turn_intent(number) {
            self.dispatch(intent, cx);
        }
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
        self.scroll_selected();
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
            self.scroll_selected();
            cx.notify();
        }
    }

    fn sync_rows(&mut self) {
        let projection = self.model.projection();
        let generation = projection.pane.map(|pane| pane.generation);
        if generation != self.ui.generation {
            self.ui.collapsed.clear();
            self.ui.generation = generation;
        }
        let mut rows = Vec::new();
        for row in projection.files {
            let file = row.file;
            rows.push(AggregateRow::Header(file.path.clone()));
            if self
                .ui
                .collapsed
                .get(&file.path)
                .copied()
                .unwrap_or(row.viewed == ViewedState::Viewed || file.omitted)
            {
                continue;
            }
            for note in file_notes(file) {
                rows.push(AggregateRow::Note(file.path.clone(), note));
            }
            for (hunk, content) in file.hunks.iter().enumerate() {
                rows.push(AggregateRow::Hunk(file.path.clone(), hunk));
                for line in 0..content.lines.len() {
                    rows.push(AggregateRow::Line(file.path.clone(), hunk, line));
                }
            }
        }
        if rows != self.ui.rows {
            self.ui.list.reset(rows.len());
            self.ui.rows = rows;
        }
    }

    fn scroll_selected(&self) {
        if let Some(index) = self.ui.rows.iter().position(
            |row| matches!(row, AggregateRow::Header(path) if Some(path) == self.selected.as_ref()),
        ) {
            self.ui.list.scroll_to_reveal_item(index);
        }
    }

    fn select_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.selected = Some(path);
        self.scroll_selected();
        cx.notify();
    }

    fn toggle_noise(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.model.state().pane() else {
            return;
        };
        let path = pane.target.checkout.clone();
        let hide = !pane.hide_noise;
        if !cx.has_global::<NoiseChoices>() {
            cx.set_global(NoiseChoices::default());
        }
        cx.global_mut::<NoiseChoices>().0.insert(path, hide);
        self.dispatch(Intent::SetHideNoise(hide), cx);
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::Div {
        let Some(row) = self.ui.rows.get(index) else {
            return div();
        };
        let path = row.path();
        let Some(projected) = self
            .model
            .projection()
            .files
            .into_iter()
            .find(|row| row.file.path == path)
        else {
            return div();
        };
        let file = projected.file;
        let theme = cx.theme();
        match row {
            AggregateRow::Header(_) => {
                let path = path.to_string();
                let open_path = path.clone();
                let collapse_path = path.clone();
                let collapsed = self
                    .ui
                    .collapsed
                    .get(&path)
                    .copied()
                    .unwrap_or(projected.viewed == ViewedState::Viewed || file.omitted);
                let can_open = !matches!(
                    self.model.state().pane().map(|pane| &pane.source),
                    Some(ReviewSource::Commits(_))
                ) && file.status != FileStatus::Deleted
                    && self.host.open_file.is_some();
                let title = file
                    .old_path
                    .as_ref()
                    .map_or_else(|| path.clone(), |old| format!("{old} → {path}"));
                div()
                    .debug_selector(move || format!("aggregate-section-{index}"))
                    .when(projected.viewed == ViewedState::Viewed, |header| {
                        header.opacity(0.7)
                    })
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .bg(theme.muted)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new(("aggregate-collapse", index))
                            .small()
                            .label(if collapsed { "▸" } else { "▾" })
                            .accessibility_label(if collapsed {
                                "Expand file diff"
                            } else {
                                "Collapse file diff"
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.ui.collapsed.insert(collapse_path.clone(), !collapsed);
                                view.sync_rows();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_family(MONO_FONT)
                            .text_size(px(12.5))
                            .child(format!(
                                "{} {title}  +{} −{}",
                                file.status.letter(),
                                file.additions,
                                file.deletions
                            )),
                    )
                    .when(can_open, |header| {
                        header.child(
                            div().debug_selector(|| "aggregate-open".into()).child(
                                Button::new(("aggregate-open", index))
                                    .small()
                                    .label("Open")
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(intent) =
                                            view.model.open_file_intent(&open_path)
                                        {
                                            view.dispatch(intent, cx);
                                        }
                                    })),
                            ),
                        )
                    })
                    .child(
                        Button::new(("aggregate-viewed", index))
                            .small()
                            .label(match projected.viewed {
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
                                view.selected = Some(path.clone());
                                if let Some(intent) = view.model.viewed_intent(&path) {
                                    view.dispatch(intent, cx);
                                }
                            })),
                    )
            }
            AggregateRow::Note(_, note) => div().px_3().py_2().child(note.clone()),
            AggregateRow::Hunk(_, hunk) => code_row()
                .text_color(theme.info)
                .bg(theme.muted)
                .child(file.hunks[*hunk].header.clone()),
            AggregateRow::Line(_, hunk, line) => {
                let line = &file.hunks[*hunk].lines[*line];
                let (sign, color, background) = match line.op {
                    LineOp::Add => ("+", theme.success, theme.success.opacity(0.10)),
                    LineOp::Del => ("−", theme.danger, theme.danger.opacity(0.10)),
                    LineOp::Context => (" ", theme.muted_foreground, theme.background),
                };
                code_row()
                    .flex()
                    .gap_3()
                    .bg(background)
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .w(px(48.))
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .child(line.old_no.map_or_else(String::new, |n| n.to_string())),
                    )
                    .child(
                        div()
                            .w(px(48.))
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .child(line.new_no.map_or_else(String::new, |n| n.to_string())),
                    )
                    .child(div().text_color(color).child(sign))
                    .child(syntax_line(index, path, &line.text, cx))
            }
        }
    }
}

/// The small text under a turn, as Iced's `render_turns` writes it.
fn turn_detail(turn: &gitterm_core::review::model::ReviewTurn) -> String {
    format!(
        "{}{}",
        turn_age(turn.timestamp),
        if turn.start.is_none() {
            " · no start snapshot"
        } else if turn.end.is_none() {
            " · running or incomplete"
        } else {
            ""
        }
    )
}

/// Iced's `commit_age` buckets, shown beside each turn.
fn turn_age(timestamp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let seconds = now.saturating_sub(timestamp).max(0);
    match seconds {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..2_592_000 => format!("{}d ago", seconds / 86_400),
        _ => format!("{}mo ago", seconds / 2_592_000),
    }
}

/// Code surfaces are monospace like Iced's `--mono`; "monospace" is not a macOS family.
pub(crate) const MONO_FONT: &str = "Menlo";

#[cfg(test)]
mod tests;

impl Render for AggregateView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_rows();
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
                Button::new("aggregate-turns")
                    .small()
                    .label("Turns")
                    .selected(matches!(source, Some(ReviewSource::Turns { .. })))
                    .disabled(pane.is_none() || self.host.active_chat.is_none())
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(intent) = view.model.turns_intent(view.host.active_chat.clone())
                        {
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
        toolbar = toolbar.child(
            Checkbox::new("aggregate-hide-noise")
                .label("Hide tests & generated")
                .checked(pane.is_none_or(|pane| pane.hide_noise))
                .on_click(cx.listener(|view, _, _, cx| view.toggle_noise(cx))),
        );
        let mut top = div().flex().flex_col().child(toolbar).child(
            div().px_3().text_color(cx.theme().muted_foreground).child(
                if self.host.save_marks.is_some() {
                    "Aggregate review"
                } else {
                    "Aggregate review · Viewed marks are kept for this preview session"
                },
            ),
        );
        for notice in &projection.notices {
            top = top.child(
                div()
                    .px_3()
                    .text_color(cx.theme().warning)
                    .child(notice.clone()),
            );
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
                    .text_color(cx.theme().danger)
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
            .bg(cx.theme().sidebar)
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
        if let Some(pane) = pane
            && let ReviewSource::Turns { selected, .. } = &pane.source
        {
            sidebar = sidebar.child(div().py_2().child("Turns of the active chat"));
            if pane.turns.is_empty() {
                sidebar = sidebar.child(
                    div()
                        .py_2()
                        .child("No turn snapshots for this chat in this checkout."),
                );
            }
            for (index, turn) in pane.turns.iter().enumerate() {
                let number = turn.number;
                let detail = turn_detail(turn);
                let label = format!("Turn {number} {detail}");
                sidebar = sidebar.child(
                    ListItem::new(("aggregate-turn", index))
                        .accessibility_label(label)
                        .disabled(turn.start.is_none() || turn.end.is_none())
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(format!("Turn {number}"))
                                .child(div().text_color(cx.theme().muted_foreground).child(detail)),
                        )
                        .selected(*selected == Some(number))
                        .on_click(cx.listener(move |view, _, _, cx| view.select_turn(number, cx))),
                );
            }
        }
        sidebar = sidebar.child(div().py_2().child("Changed files"));
        let tree = FileTree::from_paths(projection.files.iter().map(|row| row.file.path.as_str()));
        sidebar = sidebar.child(self.render_tree(&tree, "", 0, cx));
        if let Some(pane) = pane
            && pane.hide_noise
            && let Some((diff, _)) = &pane.loaded
        {
            let noise = diff
                .files
                .iter()
                .filter(|file| file.kind.is_noise())
                .count();
            if noise > 0 {
                sidebar = sidebar.child(
                    div()
                        .py_2()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{noise} hidden (tests & generated)")),
                );
            }
        }
        let mut body = div().flex_1().min_w_0().h_full().flex().flex_col();
        if !projection.files.is_empty() {
            body = body.child(
                list(
                    self.ui.list.clone(),
                    cx.processor(|view, index, _, cx| {
                        view.render_row(index, cx).into_any_element()
                    }),
                )
                .flex_1()
                .min_h_0(),
            );
        } else if !loading {
            body = body.child(div().p_3().child(match source {
                Some(ReviewSource::Commits(None)) => "Select commits to review",
                Some(ReviewSource::Turns { selected: None, .. }) => {
                    "Select a completed turn to review its changes"
                }
                Some(ReviewSource::Turns { .. }) => "No changes in the selected turn",
                _ => "No changed files",
            }));
        }
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .flex()
            .flex_col()
            .child(top)
            .child(div().flex_1().min_h_0().flex().child(sidebar).child(body))
    }
}

/// A changed file's name as Iced's `.tf.viewed .nm` (muted, struck through)
/// and `.tf.stale .nm` (warn colour) style it.
pub(crate) fn file_name(path: String, viewed: ViewedState, cx: &App) -> gpui::Div {
    match viewed {
        ViewedState::ChangedSinceViewed => div().text_color(cx.theme().warning).child(path),
        ViewedState::Viewed => div()
            .text_color(cx.theme().muted_foreground)
            .line_through()
            .child(path),
        ViewedState::Unviewed => div().child(path),
    }
}

#[derive(Default)]
struct NoiseChoices(HashMap<PathBuf, bool>);
impl Global for NoiseChoices {}

fn noise_choice(checkout: &std::path::Path, cx: &App) -> bool {
    cx.try_global::<NoiseChoices>()
        .and_then(|choices| choices.0.get(checkout).copied())
        .unwrap_or(true)
}

struct AggregateUi {
    collapsed: HashMap<String, bool>,
    closed_folders: HashSet<String>,
    generation: Option<u64>,
    rows: Vec<AggregateRow>,
    list: ListState,
}

impl Default for AggregateUi {
    fn default() -> Self {
        Self {
            collapsed: HashMap::new(),
            closed_folders: HashSet::new(),
            generation: None,
            rows: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AggregateRow {
    Header(String),
    Note(String, String),
    Hunk(String, usize),
    Line(String, usize, usize),
}

impl AggregateRow {
    fn path(&self) -> &str {
        match self {
            Self::Header(path)
            | Self::Note(path, _)
            | Self::Hunk(path, _)
            | Self::Line(path, _, _) => path,
        }
    }
}

fn file_notes(file: &ReviewFile) -> Vec<String> {
    let mut notes = Vec::new();
    if file.omitted {
        notes.push("Diff omitted by the review budget.".into());
    }
    if file.truncated {
        notes.push(
            "Diff truncated at the review text limits; only collected lines are shown.".into(),
        );
    }
    if let Some(note) = &file.note {
        notes.push(note.clone());
    }
    if file.status == FileStatus::Binary {
        notes.push("Binary file — no text diff".into());
    } else if file.hunks.is_empty() && !file.omitted && file.note.is_none() {
        notes.push("No text changes (metadata only)".into());
    }
    notes
}

fn code_row() -> gpui::Div {
    div()
        .h(px(18.))
        .font_family(MONO_FONT)
        .text_size(px(12.))
        .line_height(px(18.))
        .whitespace_nowrap()
}

/// TextView and the status Diff use the same installed syntect backend and
/// highlight theme. A fence longer than any source run keeps source literal.
fn syntax_line(index: usize, path: &str, text: &str, cx: &App) -> TextView {
    let language = std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("text"));
    let fence = "`".repeat(
        text.split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0)
            .max(2)
            + 1,
    );
    TextView::markdown(
        ("aggregate-code", index),
        format!("{fence}{language}\n{text}\n{fence}"),
    )
    .selectable(false)
    .code_block_actions(|_, _, _| div())
    .style(TextViewStyle {
        paragraph_gap: gpui::rems(0.),
        highlight_theme: cx.theme().highlight_theme.clone(),
        is_dark: cx.theme().is_dark(),
        code_block: StyleRefinement::default()
            .p_0()
            .border_0()
            .rounded_none()
            .bg(gpui::transparent_black())
            .font_family(MONO_FONT)
            .text_size(px(12.))
            .line_height(px(18.)),
        ..Default::default()
    })
}

/// Iced's TreeDir insertion order: directories first, then files, each in
/// first-seen order. Directory keys include their full path.
#[derive(Default, Debug)]
struct FileTree {
    dirs: Vec<(String, FileTree)>,
    files: Vec<String>,
}

impl FileTree {
    fn from_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Self {
        let mut tree = Self::default();
        for path in paths {
            tree.insert(path, path);
        }
        tree
    }

    fn insert(&mut self, rest: &str, path: &str) {
        if let Some((name, rest)) = rest.split_once('/') {
            let index = self
                .dirs
                .iter()
                .position(|(dir, _)| dir == name)
                .unwrap_or_else(|| {
                    self.dirs.push((name.into(), Self::default()));
                    self.dirs.len() - 1
                });
            self.dirs[index].1.insert(rest, path);
        } else {
            self.files.push(path.into());
        }
    }
}

impl AggregateView {
    fn render_tree(
        &self,
        tree: &FileTree,
        prefix: &str,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut container = div();
        for (name, children) in &tree.dirs {
            let path = format!("{prefix}{name}/");
            let key = path.clone();
            let closed = self.ui.closed_folders.contains(&path);
            container = container.child(
                ListItem::new(gpui::SharedString::from(format!("aggregate-folder-{path}")))
                    .accessibility_label(format!(
                        "{} folder {name}",
                        if closed { "Expand" } else { "Collapse" }
                    ))
                    .child(
                        div()
                            .debug_selector({
                                let path = path.clone();
                                move || format!("aggregate-folder-{path}")
                            })
                            .pl(px(depth as f32 * 16.))
                            .child(format!("{} {name}", if closed { "▸" } else { "▾" })),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if !view.ui.closed_folders.remove(&key) {
                            view.ui.closed_folders.insert(key.clone());
                        }
                        cx.notify();
                    })),
            );
            if !closed {
                container = container.child(self.render_tree(children, &path, depth + 1, cx));
            }
        }
        let projection = self.model.projection();
        for path in &tree.files {
            let Some(row) = projection.files.iter().find(|row| row.file.path == *path) else {
                continue;
            };
            let selected = self.selected.as_ref() == Some(path);
            let name = path.rsplit('/').next().unwrap_or(path);
            let label = format!("{} {path}", row.file.status.letter());
            let key = path.clone();
            container = container.child(
                ListItem::new(gpui::SharedString::from(format!("aggregate-file-{path}")))
                    .accessibility_label(label)
                    .selected(selected)
                    .child(
                        div()
                            .pl(px(depth as f32 * 16.))
                            .font_family(MONO_FONT)
                            .text_size(px(12.))
                            .flex()
                            .gap_1()
                            .child(row.file.status.letter().to_string())
                            .child(file_name(name.into(), row.viewed, cx)),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.select_file(key.clone(), cx))),
            );
        }
        container
    }
}
