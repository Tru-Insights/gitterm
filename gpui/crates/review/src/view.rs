use crate::{
    fixture,
    model::{FileKey, Group, Patches, ReviewModel},
};
use gitterm_core::status::GitStatusSnapshot;
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, div, px, rgb, size,
};
use gpui_component::{
    Root, Sizable,
    button::Button,
    diff::{Diff, DiffFile, DiffMode, DiffState},
    list::ListItem,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

actions!(native_review, [NextFile, PreviousFile, ToggleViewed]);

mod aggregate;
use aggregate::AggregateView;

pub fn init(_: &mut App) {}

/// Fixture argument validation stays here; the host dispatches review routes.
pub fn run_fixture(args: Vec<OsString>) {
    let [path] = args.as_slice() else {
        eprintln!("Usage: gitterm-gpui --review <fixture-dir>");
        std::process::exit(2);
    };
    let model = match fixture::load_model(Path::new(path)) {
        Ok(model) => model,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    run(model, None);
}

pub fn run_repo(path: PathBuf) {
    run(ReviewModel::default(), Some(path));
}

fn run(model: ReviewModel, repo: Option<PathBuf>) {
    let executor = repo.as_ref().map(|_| {
        Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .build()
                .expect("create review executor"),
        )
    });
    let title = if repo.is_some() {
        "GitTerm · Repository review"
    } else {
        "GitTerm · Native review fixtures"
    };
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        init(cx);
        // Retain the one host executor through application shutdown, even after
        // the one-shot review load finishes. Job runtimes also close on drop.
        let process_executor = executor.clone();
        cx.on_app_quit(move |_| {
            let executor = process_executor.clone();
            async move {
                drop(executor);
            }
        })
        .detach();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(850.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| ReviewView::new(model, window, cx));
                if let (Some(path), Some(executor)) = (repo, executor) {
                    view.update(cx, |view, _| view.loading = true);
                    let review = view.downgrade();
                    cx.spawn(async move |cx| {
                        let result = crate::repo::load(executor.handle(), 0, path).await;
                        _ = review.update(cx, |view, cx| {
                            view.loading = false;
                            match result {
                                Ok((snapshot, patches)) => {
                                    if let Err(error) = view.replace(&snapshot, &patches, cx) {
                                        view.model.error = Some(error);
                                    }
                                }
                                Err(error) => view.model.error = Some(error),
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("open native review viewer");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}

pub struct ReviewView {
    model: ReviewModel,
    diff: Option<Entity<DiffState>>,
    diff_error: Option<String>,
    focus: FocusHandle,
    loading: bool,
    generation: u64,
    pending_selection: Option<FileKey>,
    repo_path: Option<PathBuf>,
    aggregate: Option<Entity<AggregateView>>,
    show_aggregate: bool,
}
impl ReviewView {
    pub fn new(model: ReviewModel, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut view = Self {
            model,
            diff: None,
            diff_error: None,
            focus,
            loading: false,
            generation: 0,
            pending_selection: None,
            repo_path: None,
            aggregate: None,
            show_aggregate: false,
        };
        view.prepare(cx);
        view
    }
    pub fn model(&self) -> &ReviewModel {
        &self.model
    }
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    pub fn select_file(&mut self, key: FileKey, cx: &mut Context<Self>) {
        self.show_aggregate = false;
        self.pending_selection = Some(key);
        self.apply_selection(cx);
        cx.notify();
    }
    fn apply_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = &self.pending_selection
            && let Some(index) = self.model.files.iter().position(|file| &file.key == key)
        {
            self.model.selected = Some(index);
            self.prepare(cx);
            if !self.loading {
                self.pending_selection = None;
            }
        }
    }
    pub fn begin_load(&mut self, cx: &mut Context<Self>) -> u64 {
        self.generation += 1;
        self.loading = true;
        self.model.error = None;
        cx.notify();
        self.generation
    }
    pub fn finish_load(
        &mut self,
        generation: u64,
        result: Result<(GitStatusSnapshot, Patches), String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        match result {
            Ok((snapshot, patches)) => {
                if let Err(error) = self.replace(&snapshot, &patches, cx) {
                    self.model.error = Some(error);
                }
                self.apply_selection(cx);
                self.pending_selection = None;
            }
            Err(error) => self.model.error = Some(error),
        }
        cx.notify();
    }
    /// Apply a newer status/patch set while retaining marks for unchanged entries.
    pub fn replace(
        &mut self,
        snapshot: &GitStatusSnapshot,
        patches: &Patches,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.model.replace(snapshot, patches)?;
        if snapshot.is_git_repo && self.repo_path.as_ref() != Some(&snapshot.repo_path) {
            self.repo_path = Some(snapshot.repo_path.clone());
            self.aggregate = None;
            self.show_aggregate = false;
        }
        self.prepare(cx);
        cx.notify();
        Ok(())
    }
    fn prepare(&mut self, cx: &mut Context<Self>) {
        self.diff = None;
        self.diff_error = None;
        if let Some(file) = self.model.selected.map(|ix| &self.model.files[ix])
            && !file.is_binary()
            && !file.collapsed()
        {
            match DiffFile::parse(&file.patch) {
                Ok(files) if files.len() == 1 && files[0].path().as_str() == file.key.1 => {
                    self.diff =
                        Some(cx.new(|cx| DiffState::new(files, cx).with_mode(DiffMode::Unified)));
                }
                Ok(_) => self.diff_error = Some(format!("Expected one patch for {}", file.key.1)),
                Err(error) => self.diff_error = Some(format!("Parse {}: {error}", file.key.1)),
            }
        }
    }
    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_selection = None;
        if index < self.model.files.len() && self.model.selected != Some(index) {
            self.model.selected = Some(index);
            self.prepare(cx);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }
    fn next(&mut self, _: &NextFile, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(false, window, cx);
    }
    fn previous(&mut self, _: &PreviousFile, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(true, window, cx);
    }
    fn navigate(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_aggregate
            && let Some(aggregate) = &self.aggregate
        {
            aggregate.update(cx, |view, cx| view.navigate(previous, cx));
            self.focus.focus(window, cx);
            return;
        }
        self.pending_selection = None;
        let before = self.model.selected;
        self.model.navigate(previous);
        if before != self.model.selected {
            self.prepare(cx);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }
    fn toggle_viewed(&mut self, _: &ToggleViewed, _: &mut Window, cx: &mut Context<Self>) {
        if self.show_aggregate {
            if let Some(aggregate) = &self.aggregate {
                aggregate.update(cx, |view, cx| view.toggle_viewed(cx));
            }
            return;
        }
        if let Some(index) = self.model.selected {
            self.model.files[index].viewed = !self.model.files[index].viewed;
            cx.notify();
        }
    }
    fn show_anyway(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.model.selected {
            self.model.files[index].expanded = true;
            self.prepare(cx);
            self.focus.focus(window, cx);
            cx.notify();
        }
    }
    pub fn open_branch_review(&mut self, cx: &mut Context<Self>) {
        if self.aggregate.is_none() {
            let Some(root) = self.repo_path.clone() else {
                return;
            };
            self.aggregate = Some(cx.new(|cx| AggregateView::new(root, cx)));
        }
        self.show_aggregate = true;
        cx.notify();
    }
}
impl Focusable for ReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for ReviewView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.show_aggregate
            && let Some(aggregate) = &self.aggregate
        {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgb(0x1e1e2e))
                .text_color(rgb(0xcdd6f4))
                .text_size(px(13.))
                .key_context("NativeReview")
                .track_focus(&self.focus)
                .on_action(cx.listener(Self::next))
                .on_action(cx.listener(Self::previous))
                .on_action(cx.listener(Self::toggle_viewed))
                .child(div().flex_1().min_h_0().child(aggregate.clone()));
        }
        let mut sidebar = div()
            .flex()
            .flex_col()
            .id("review-files")
            .w(px(320.))
            .h_full()
            .overflow_y_scroll()
            .flex_shrink_0()
            .bg(rgb(0x181825))
            .p_3();
        for group in [Group::Staged, Group::Unstaged, Group::Untracked] {
            sidebar = sidebar.child(div().py_2().text_color(rgb(0x9399b2)).child(group.label()));
            for (index, file) in self
                .model
                .files
                .iter()
                .enumerate()
                .filter(|(_, f)| f.key.0 == group)
            {
                let label = format!(
                    "{} {} {}",
                    if file.viewed { "✓" } else { "○" },
                    file.status,
                    file.title()
                );
                sidebar = sidebar.child(
                    ListItem::new(("review-file", index))
                        .w_full()
                        .accessibility_label(label.clone())
                        .child(label)
                        .selected(self.model.selected == Some(index))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.select(index, window, cx)),
                        ),
                );
            }
        }
        let mut body = div().flex_1().min_w_0().h_full().flex().flex_col();
        if self.loading {
            body = body.child(div().p_3().child("Loading repository…"));
        }
        if let Some(error) = &self.model.error {
            body = body.child(div().p_3().text_color(rgb(0xf38ba8)).child(error.clone()));
        }
        if let Some(index) = self.model.selected {
            let file = &self.model.files[index];
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .child(div().flex_1().child(file.title()))
                    .child(
                        Button::new("review-viewed")
                            .small()
                            .label(if file.viewed {
                                "✓ Viewed"
                            } else {
                                "Mark viewed"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_viewed(&ToggleViewed, window, cx)
                            })),
                    ),
            );
            if file.is_binary() {
                body = body.child(div().p_4().child("Binary file — no text diff"));
            } else if file.collapsed() {
                body = body.child(
                    div().p_4().child("Large diff collapsed. ").child(
                        Button::new("review-show-anyway")
                            .label("Show anyway")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.show_anyway(window, cx)),
                            ),
                    ),
                );
            } else if let Some(error) = &self.diff_error {
                body = body.child(div().p_4().text_color(rgb(0xf38ba8)).child(error.clone()));
            } else if let Some(diff) = &self.diff {
                body = body.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(Diff::new(diff).line_number(true).syntax_highlight(true)),
                );
            }
        } else if !self.loading {
            body = body.child(div().p_4().child("No changed files"));
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .text_size(px(13.))
            .key_context("NativeReview")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::previous))
            .on_action(cx.listener(Self::toggle_viewed))
            .child(div().p_2().child("Review"))
            .child(div().flex_1().min_h_0().flex().child(sidebar).child(body))
    }
}

#[cfg(test)]
mod tests;
