//! Iced's Git sidebar: pinned mode/selection controls, independently scrolling rows.
use crate::{
    data::{ChangedFile, GitStatus, ShellData, WorkspaceId},
    theme::Palette,
};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    Selectable,
    button::{Button, ButtonVariants},
};
use std::{path::PathBuf, rc::Rc};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Changes,
    Worktrees,
}

/// Worktree operations stay with the host, alongside task/session ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GitAction {
    BackToWork,
    OpenWorktree(PathBuf),
    AdoptWorktree(PathBuf),
    RemoveWorktree { path: PathBuf, force: bool },
    EditFile(PathBuf),
}
impl EventEmitter<GitAction> for GitView {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    Worktree(PathBuf),
    Branch(String),
}

type TabSelection = (Mode, Option<Selection>, Option<(String, bool)>);

pub(crate) struct GitView {
    workspace: WorkspaceId,
    data: Rc<dyn ShellData>,
    mode: Mode,
    selected: Option<Selection>,
    selected_file: Option<(String, bool)>,
    snapshot: Option<Snapshot>,
    loading: bool,
    generation: u64,
    operations: std::collections::HashMap<PathBuf, WorktreeOperation>,
    session: Option<crate::data::SessionId>,
    selections: std::collections::HashMap<Option<crate::data::SessionId>, TabSelection>,
}
impl GitView {
    pub(crate) fn new(
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(owner) = data.local_owner() {
            cx.observe(&owner, |this, _, cx| {
                this.sync_session(cx);
                if this.mode == Mode::Worktrees {
                    this.load_worktrees(cx);
                }
                cx.notify();
            })
            .detach();
        }
        Self {
            workspace,
            data,
            mode: Mode::Changes,
            selected: None,
            selected_file: None,
            snapshot: None,
            loading: false,
            generation: 0,
            operations: Default::default(),
            session: None,
            selections: Default::default(),
        }
    }
    fn sync_session(&mut self, cx: &mut Context<Self>) {
        let session = self.data.local_owner().and_then(|owner| {
            owner
                .read(cx)
                .model
                .active_sessions
                .get(&self.workspace)
                .copied()
        });
        if session == self.session {
            return;
        }
        self.selections.insert(
            self.session,
            (self.mode, self.selected.clone(), self.selected_file.clone()),
        );
        self.session = session;
        let (mode, selected, file) = self.selections.get(&session).cloned().unwrap_or_default();
        self.mode = mode;
        self.selected = selected;
        self.selected_file = file;
        self.snapshot = None;
        self.generation += 1;
        self.loading = false;
    }
    fn root(&self, cx: &App) -> Option<PathBuf> {
        if let Some(owner) = self.data.local_owner() {
            return owner
                .read(cx)
                .model
                .workspaces
                .iter()
                .find(|w| w.id == self.workspace && !w.remote)
                .map(|w| w.root.clone());
        }
        self.data.terminal_cwd(self.workspace, cx)
    }
    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            if mode == Mode::Worktrees {
                self.load_worktrees(cx);
            }
            cx.notify();
        }
    }
    fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(root) = self.root(cx) else { return };
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        let work = cx.background_executor().spawn({
            let root = root.clone();
            async move { collect(root) }
        });
        cx.spawn(async move |view, cx| {
            let snapshot = work.await;
            _ = view.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                // A followed checkout may have changed while the job was running.
                if this.root(cx).as_ref() == Some(&root) {
                    this.snapshot = Some(snapshot);
                } else {
                    this.snapshot = None;
                    if this.mode == Mode::Worktrees {
                        this.load_worktrees(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn open_file(&mut self, file: ChangedFile, cx: &mut Context<Self>) {
        self.selected_file = Some((file.path.clone(), file.staged));
        if let Some(owner) = self.data.local_owner() {
            owner.update(cx, |owner, cx| {
                owner.open_changed_file(self.workspace, file, cx);
            });
        }
        cx.notify();
    }
    fn selection_panel(&self, snapshot: &Snapshot, cx: &Context<Self>) -> Option<Div> {
        let p = Palette::current(cx);
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(rgb(p.base))
            .border_1()
            .border_color(rgb(p.border))
            .rounded_md();
        let mut actions = div().flex().gap_1();
        match self.selected.as_ref()? {
            Selection::Worktree(path) => {
                let row = snapshot.worktrees.iter().find(|row| &row.path == path)?;
                panel = panel
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child("Selected worktree"),
                    )
                    .child(row.label())
                    .child(div().text_color(rgb(p.subtext0)).child(compact_path(path)));
                if row.available && !row.prunable {
                    actions = actions.child(self.open_button(path, cx));
                } else {
                    actions = actions.child("Unavailable");
                }
                let managed = managed_task(path, cx);
                let op = self.operations.get(path);
                if let Some(task) = &managed {
                    panel = panel.child(format!(
                        "task: {}",
                        task.issue
                            .as_ref()
                            .map(|i| i.key.as_str())
                            .unwrap_or(&task.title)
                    ));
                }
                if let Some(error) = op.and_then(|op| op.error.as_ref()) {
                    panel = panel.child(div().text_color(rgb(p.red)).child(error.clone()));
                }
                if !row.current && managed.is_none() && op.is_some_and(|op| op.busy) {
                    actions = actions.child("working…");
                } else if !row.current && managed.is_none() {
                    if row.available && !row.detached && !row.prunable {
                        let path = path.clone();
                        actions = actions.child(
                            Button::new("git-adopt")
                                .ghost()
                                .label("Adopt as task")
                                .text_color(rgb(p.blue))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(GitAction::AdoptWorktree(path.clone()))
                                })),
                        );
                    }
                    let path = path.clone();
                    let force = op.is_some_and(|op| op.offer_force);
                    actions = actions.child(
                        Button::new("git-remove")
                            .ghost()
                            .label(if force { "Force remove" } else { "Remove" })
                            .text_color(rgb(if force { p.red } else { p.subtext0 }))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(GitAction::RemoveWorktree {
                                    path: path.clone(),
                                    force,
                                })
                            })),
                    );
                }
            }
            Selection::Branch(name) => {
                let row = snapshot.branches.iter().find(|row| &row.name == name)?;
                panel = panel
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child("Selected branch"),
                    )
                    .child(name.clone())
                    .child(div().text_color(rgb(p.subtext0)).child(row.head_detail()));
                if let Some(path) = &row.worktree {
                    panel = panel.child(compact_path(path));
                    actions = actions.child(self.open_button(path, cx));
                } else {
                    actions = actions.child("No worktree");
                }
            }
        }
        Some(panel.child(actions))
    }
    pub(crate) fn begin_operation(
        &mut self,
        path: &std::path::Path,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Result<Option<String>, String> {
        if self.operations.get(path).is_some_and(|op| op.busy) {
            return Err("Worktree operation is already running".into());
        }
        let previous = self
            .operations
            .get(path)
            .filter(|op| op.offer_force)
            .and_then(|op| op.error.clone());
        gitterm_core::task_worktree::removal_force(force, previous.as_deref())?;
        self.operations.insert(
            path.to_path_buf(),
            WorktreeOperation {
                busy: true,
                offer_force: false,
                error: None,
            },
        );
        cx.notify();
        Ok(previous)
    }
    pub(crate) fn finish_operation(
        &mut self,
        path: &std::path::Path,
        result: Result<(), String>,
        removing: bool,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.operations.remove(path);
                if !path.exists()
                    && let Some(snapshot) = &mut self.snapshot
                {
                    snapshot.worktrees.retain(|row| row.path != path);
                    for branch in &mut snapshot.branches {
                        if branch.worktree.as_deref() == Some(path) {
                            branch.worktree = None;
                        }
                    }
                }
                self.load_worktrees(cx);
            }
            Err(error) => {
                self.operations.insert(
                    path.to_path_buf(),
                    WorktreeOperation {
                        busy: false,
                        offer_force: removing && gitterm_core::task_worktree::offer_force(&error),
                        error: Some(error),
                    },
                );
            }
        }
        cx.notify();
    }

    fn open_button(&self, path: &std::path::Path, cx: &Context<Self>) -> Button {
        let has_session = self.data.local_owner().is_some_and(|owner| {
            owner
                .read(cx)
                .iced_workspace(self.workspace)
                .is_some_and(|saved| {
                    saved.tabs.iter().any(|tab| {
                        std::path::Path::new(&tab.dir) == path
                            || tab
                                .repo_dir
                                .as_deref()
                                .is_some_and(|dir| std::path::Path::new(dir) == path)
                    })
                })
        });
        let path = path.to_path_buf();
        Button::new("git-open-worktree")
            .ghost()
            .label(if has_session {
                "Go to session"
            } else {
                "Open Terminal"
            })
            .on_click(
                cx.listener(move |_, _, _, cx| cx.emit(GitAction::OpenWorktree(path.clone()))),
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Staged,
    Unstaged,
    Untracked,
}
impl Group {
    fn label(self) -> &'static str {
        match self {
            Self::Staged => "S T A G E D",
            Self::Unstaged => "U N S T A G E D",
            Self::Untracked => "U N T R A C K E D",
        }
    }
    fn of(file: &ChangedFile) -> Self {
        if file.staged {
            Self::Staged
        } else if file.status == "??" || file.status == "?" {
            Self::Untracked
        } else {
            Self::Unstaged
        }
    }
}
fn sections(status: &GitStatus) -> Vec<(Group, Vec<ChangedFile>)> {
    [Group::Staged, Group::Unstaged, Group::Untracked]
        .into_iter()
        .filter_map(|group| {
            let files: Vec<_> = status
                .files
                .iter()
                .filter(|file| Group::of(file) == group)
                .cloned()
                .collect();
            (!files.is_empty()).then_some((group, files))
        })
        .collect()
}
fn file_badge(file: &ChangedFile) -> &str {
    if file.status == "??" {
        "?"
    } else {
        &file.status
    }
}
fn heading(label: &'static str, count: usize, color: u32, p: Palette) -> Div {
    div()
        .flex()
        .gap_2()
        .text_size(px(10.))
        .text_color(rgb(p.muted))
        .child(label)
        .child(div().text_color(rgb(color)).child(count.to_string()))
}
fn compact_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(home)
    {
        return if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", relative.display())
        };
    }
    path.display().to_string()
}
impl Render for GitView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let mauve = if gpui_component::Theme::global(cx).mode.is_dark() {
            0xcba6f7
        } else {
            0x8839ef
        };
        let mut toggle = div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .border_1()
            .border_color(rgb(p.surface))
            .rounded_md()
            .bg(rgb(p.base));
        for (mode, label, id) in [
            (Mode::Changes, "Changes", "git-changes"),
            (Mode::Worktrees, "Worktrees", "git-worktrees"),
        ] {
            toggle = toggle.child(
                div().flex_1().debug_selector(move || id.into()).child(
                    Button::new(id)
                        .ghost()
                        .label(label)
                        .w_full()
                        .selected(self.mode == mode)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx))),
                ),
            );
        }
        let mut header = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(
                Button::new("git-back-work")
                    .debug_selector(|| "git-back-work".into())
                    .ghost()
                    .label("← Work")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(GitAction::BackToWork))),
            )
            .child(toggle);
        let mut rows = div()
            .id("git-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_2();
        let status = self.data.status(self.workspace, cx);
        match self.mode {
            Mode::Changes => {
                if let Some(error) = status.error.clone() {
                    rows = rows.child(div().text_color(rgb(p.red)).child(error));
                }
                if status.is_git_repo {
                    rows = rows.child(
                        div()
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .bg(rgb(p.base))
                            .text_color(rgb(mauve))
                            .child(format!("◆ {}", status.branch)),
                    );
                }
                let loading = status.loading && !status.checked;
                if loading {
                    rows = rows.child("Loading git status...");
                }
                for (group, files) in sections(&status) {
                    rows = rows.child(heading(
                        group.label(),
                        files.len(),
                        match group {
                            Group::Staged => p.green,
                            Group::Unstaged => p.yellow,
                            Group::Untracked => p.subtext0,
                        },
                        p,
                    ));
                    for file in files {
                        let selected =
                            self.selected_file.as_ref() == Some(&(file.path.clone(), file.staged));
                        let color = match file_badge(&file) {
                            "A" => p.green,
                            "M" => p.yellow,
                            "D" => p.red,
                            "R" => p.blue,
                            _ => p.subtext0,
                        };
                        let mut row = div().flex().items_center().child(
                            div()
                                .id(SharedString::from(format!(
                                    "git-file-{:?}-{}",
                                    group, file.path
                                )))
                                .debug_selector({
                                    let selector = format!("git-file-{:?}-{}", group, file.path);
                                    move || selector.clone()
                                })
                                .flex_1()
                                .flex()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .when(selected, |row| {
                                    row.bg(rgb(p.blue)).text_color(rgb(0xffffff))
                                })
                                .child(
                                    div()
                                        .w(px(20.))
                                        .text_color(rgb(color))
                                        .child(file_badge(&file).to_string()),
                                )
                                .child(file.path.clone())
                                .on_click(cx.listener({
                                    let file = file.clone();
                                    move |this, _, _, cx| this.open_file(file.clone(), cx)
                                })),
                        );
                        if file.status != "D"
                            && let Some(root) = self.root(cx)
                        {
                            let path = root.join(&file.path);
                            row = row.child(
                                Button::new(SharedString::from(format!(
                                    "git-edit-{:?}-{}",
                                    group, file.path
                                )))
                                .ghost()
                                .label("✎")
                                .on_click(cx.listener(
                                    move |_, _, _, cx| cx.emit(GitAction::EditFile(path.clone())),
                                )),
                            );
                        }
                        rows = rows.child(row);
                    }
                }
                if status.files.is_empty() && !loading {
                    rows = rows.child(if status.is_git_repo {
                        "No changes"
                    } else {
                        "Not a git repository"
                    });
                }
            }
            Mode::Worktrees => {
                let snapshot = self
                    .snapshot
                    .as_ref()
                    .filter(|snapshot| Some(&snapshot.root) == self.root(cx).as_ref());
                if let Some(snapshot) = snapshot {
                    if let Some(panel) = self.selection_panel(snapshot, cx) {
                        header = header.child(panel);
                    }
                    if let Some(error) = &snapshot.error {
                        rows = rows.child(div().text_color(rgb(p.red)).child(error.clone()));
                    }
                    if !snapshot.worktrees.is_empty() {
                        rows = rows.child(heading(
                            snapshot.section_counts()[0].0,
                            snapshot.section_counts()[0].1,
                            p.blue,
                            p,
                        ));
                        for row in &snapshot.worktrees {
                            let selection = Selection::Worktree(row.path.clone());
                            let selected = self.selected.as_ref() == Some(&selection);
                            let count = row.counts.iter().sum::<usize>();
                            let badge = row.badge();
                            rows = rows.child(
                                div()
                                    .id(SharedString::from(format!(
                                        "git-worktree-{}",
                                        row.path.display()
                                    )))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(selected, |row| {
                                        row.bg(rgb(p.surface)).border_1().border_color(rgb(p.blue))
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_color(rgb(if row.current {
                                                        mauve
                                                    } else {
                                                        p.subtext0
                                                    }))
                                                    .child(if row.current { "◆" } else { "◇" }),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_color(rgb(if row.current {
                                                        p.text
                                                    } else {
                                                        p.subtext0
                                                    }))
                                                    .child(row.label()),
                                            )
                                            .child(
                                                div()
                                                    .text_color(rgb(
                                                        if row.prunable || !row.available {
                                                            p.red
                                                        } else if count == 0 {
                                                            p.green
                                                        } else {
                                                            p.yellow
                                                        },
                                                    ))
                                                    .child(badge),
                                            )
                                            .children(row.current.then(|| {
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(p.blue))
                                                    .child("current")
                                            })),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .text_size(px(11.))
                                            .text_color(rgb(p.subtext0))
                                            .child(div().flex_1().child(compact_path(&row.path)))
                                            .children((count > 0).then(|| {
                                                div().text_size(px(10.)).child(format!(
                                                    "S {} U {} ? {}",
                                                    row.counts[0], row.counts[1], row.counts[2]
                                                ))
                                            })),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.selected = Some(selection.clone());
                                        cx.notify();
                                    })),
                            );
                        }
                    } else if !self.loading && snapshot.error.is_none() {
                        rows = rows.child(if status.is_git_repo {
                            "No linked worktrees"
                        } else {
                            "Not a git repository"
                        });
                    }
                    if !snapshot.branches.is_empty() {
                        rows = rows.child(heading(
                            snapshot.section_counts().last().unwrap().0,
                            snapshot.section_counts().last().unwrap().1,
                            p.blue,
                            p,
                        ));
                        for row in &snapshot.branches {
                            let selection = Selection::Branch(row.name.clone());
                            let selected = self.selected.as_ref() == Some(&selection);
                            rows = rows.child(
                                div()
                                    .id(SharedString::from(format!("git-branch-{}", row.name)))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(selected, |row| {
                                        row.bg(rgb(p.surface)).border_1().border_color(rgb(p.blue))
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_color(rgb(if row.current {
                                                        mauve
                                                    } else if row.worktree.is_some() {
                                                        p.blue
                                                    } else {
                                                        p.subtext0
                                                    }))
                                                    .child(row.marker()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_color(rgb(if row.current {
                                                        p.text
                                                    } else {
                                                        p.subtext0
                                                    }))
                                                    .child(row.name.clone()),
                                            )
                                            .children(row.worktree.as_ref().map(|_| {
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(p.blue))
                                                    .child(if row.current {
                                                        "current"
                                                    } else {
                                                        "worktree"
                                                    })
                                            })),
                                    )
                                    .child(
                                        div().text_size(px(11.)).text_color(rgb(p.subtext0)).child(
                                            row.worktree
                                                .as_ref()
                                                .map(|p| compact_path(p))
                                                .unwrap_or_else(|| row.head_detail()),
                                        ),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.selected = Some(selection.clone());
                                        cx.notify();
                                    })),
                            );
                        }
                    }
                } else {
                    rows = rows.child(if self.root(cx).is_some() {
                        "Loading worktrees..."
                    } else {
                        "Worktrees unavailable for this source"
                    });
                }
            }
        }
        div()
            .id("git-view")
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(p.mantle))
            .text_color(rgb(p.text))
            .child(header)
            .child(rows)
    }
}

#[derive(Default)]
struct Snapshot {
    root: PathBuf,
    worktrees: Vec<Worktree>,
    branches: Vec<Branch>,
    error: Option<String>,
}
impl Snapshot {
    fn section_counts(&self) -> Vec<(&'static str, usize)> {
        [
            ("W O R K T R E E S", self.worktrees.len()),
            ("B R A N C H E S", self.branches.len()),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .collect()
    }
}
#[derive(Default)]
struct Worktree {
    path: PathBuf,
    branch: String,
    head: String,
    current: bool,
    detached: bool,
    prunable: bool,
    available: bool,
    counts: [usize; 3],
}
impl Worktree {
    fn label(&self) -> String {
        let head = self.head.get(..7).unwrap_or(&self.head);
        if self.detached {
            if head.is_empty() {
                "detached".into()
            } else {
                format!("detached {head}")
            }
        } else if !self.branch.is_empty() {
            self.branch.clone()
        } else if head.is_empty() {
            "(unknown)".into()
        } else {
            head.into()
        }
    }
    fn badge(&self) -> String {
        let count = self.counts.iter().sum::<usize>();
        if self.prunable {
            "prunable".into()
        } else if !self.available {
            "unavailable".into()
        } else if count == 0 {
            "clean".into()
        } else {
            format!("{count} changed")
        }
    }
}
struct Branch {
    name: String,
    head: String,
    current: bool,
    upstream: Option<String>,
    worktree: Option<PathBuf>,
}
impl Branch {
    fn marker(&self) -> &'static str {
        if self.current {
            "◆"
        } else if self.worktree.is_some() {
            "◇"
        } else {
            "∙"
        }
    }
    fn head_detail(&self) -> String {
        let head = self.head.get(..7).unwrap_or(&self.head);
        match &self.upstream {
            Some(upstream) => format!("{head} → {upstream}"),
            None => head.into(),
        }
    }
}
fn git(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let what = format!("git {} in {}", args.join(" "), root.display());
    let output = gitterm_core::git_status::git_command()
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("{what}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("{what}: {error}"))
}
/// Whether `root` is inside a Git work tree: Iced's
/// `Repository::discover(path).is_ok()` for a tab's `is_git_repo`.
pub(crate) fn is_git_repo(root: &std::path::Path) -> bool {
    git(root, &["--no-optional-locks", "rev-parse", "--git-dir"]).is_ok()
}
/// Local branch names, each with whether it is checked out here: the
/// `refs/heads` read Iced's worktree snapshot feeds the task base picker.
pub(crate) fn local_branches(root: &std::path::Path) -> Result<Vec<(String, bool)>, String> {
    let output = git(
        root,
        &[
            "--no-optional-locks",
            "for-each-ref",
            "refs/heads",
            "--format=%(refname:short)%00%(HEAD)",
        ],
    )?;
    output
        .lines()
        .map(|record| match record.split_once('\0') {
            Some((name, marker)) => Ok((name.to_owned(), marker == "*")),
            None => Err(format!(
                "Read branches in {}: invalid ref record",
                root.display()
            )),
        })
        .collect()
}
fn collect(root: PathBuf) -> Snapshot {
    let mut snapshot = Snapshot {
        root: root.clone(),
        ..Default::default()
    };
    let result = (|| {
        // -z avoids Git's quoted-path encoding and preserves spaces/newlines in paths.
        let output = git(
            &root,
            &[
                "--no-optional-locks",
                "worktree",
                "list",
                "--porcelain",
                "-z",
            ],
        )?;
        let mut current: Option<Worktree> = None;
        for record in output.split('\0') {
            if record.is_empty() {
                if let Some(mut row) = current.take() {
                    if row.path.is_dir() {
                        let status = gitterm_core::git_status::collect_repo_status_with(
                            &row.path,
                            gitterm_core::git_status::StatusReadOptions { fsmonitor: false },
                        );
                        row.available = status.is_git_repo;
                        row.counts = [
                            status.staged.len(),
                            status.unstaged.len(),
                            status.untracked.len(),
                        ];
                    }
                    snapshot.worktrees.push(row);
                }
            } else if let Some(path) = record.strip_prefix("worktree ") {
                let path = PathBuf::from(path);
                let same = |a: &std::path::Path, b: &std::path::Path| match (
                    a.canonicalize(),
                    b.canonicalize(),
                ) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => a == b,
                };
                current = Some(Worktree {
                    current: same(&path, &root),
                    path,
                    ..Default::default()
                });
            } else if let Some(row) = &mut current {
                if let Some(head) = record.strip_prefix("HEAD ") {
                    row.head = head.into();
                } else if let Some(branch) = record.strip_prefix("branch ") {
                    row.branch = branch.strip_prefix("refs/heads/").unwrap_or(branch).into();
                } else if record == "detached" {
                    row.detached = true;
                } else if record.starts_with("prunable") {
                    row.prunable = true;
                }
            }
        }
        snapshot.worktrees.sort_by(|a, b| {
            b.current
                .cmp(&a.current)
                .then_with(|| {
                    b.counts
                        .iter()
                        .sum::<usize>()
                        .cmp(&a.counts.iter().sum::<usize>())
                })
                .then_with(|| a.branch.cmp(&b.branch))
                .then_with(|| a.path.cmp(&b.path))
        });
        let branches = git(
            &root,
            &[
                "--no-optional-locks",
                "for-each-ref",
                "refs/heads",
                "--format=%(refname:short)%00%(objectname)%00%(HEAD)%00%(upstream:short)",
            ],
        )?;
        for record in branches.lines() {
            let fields: Vec<_> = record.split('\0').collect();
            let [name, head, marker, upstream] = fields.as_slice() else {
                return Err(format!(
                    "Read branches in {}: invalid ref record",
                    root.display()
                ));
            };
            snapshot.branches.push(Branch {
                name: (*name).into(),
                head: (*head).into(),
                current: *marker == "*",
                upstream: (!upstream.is_empty()).then(|| (*upstream).into()),
                worktree: snapshot
                    .worktrees
                    .iter()
                    .find(|row| !row.detached && row.branch == *name)
                    .map(|row| row.path.clone()),
            });
        }
        snapshot.branches.sort_by(|a, b| {
            b.current
                .cmp(&a.current)
                .then_with(|| b.worktree.is_some().cmp(&a.worktree.is_some()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok::<_, String>(())
    })();
    snapshot.error = result.err();
    snapshot
}

#[cfg(test)]
#[path = "git_view_tests.rs"]
mod tests;

#[derive(Default)]
struct WorktreeOperation {
    busy: bool,
    offer_force: bool,
    error: Option<String>,
}

/// Shared by every Git view. Keep the lease through persistence acknowledgement.
#[derive(Clone, Default)]
pub(crate) struct WorktreeOperations(
    std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
);
pub(crate) struct WorktreeLease {
    operations: WorktreeOperations,
    path: PathBuf,
}
impl WorktreeOperations {
    pub(crate) fn begin(&self, path: &std::path::Path) -> Result<WorktreeLease, String> {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !self.0.lock().unwrap().insert(path.clone()) {
            return Err("Worktree operation is already running".into());
        }
        Ok(WorktreeLease {
            operations: self.clone(),
            path,
        })
    }
}
impl Drop for WorktreeLease {
    fn drop(&mut self) {
        self.operations.0.lock().unwrap().remove(&self.path);
    }
}

fn recheck_removal_ownership(
    root: &std::path::Path,
    repository: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), String> {
    let tasks =
        gitterm_core::tasks::TaskStore::load(root.join("tasks.json")).map_err(|e| e.to_string())?;
    let managed = tasks.tasks().iter().find(|task| {
        task.worktree.path.as_deref().is_some_and(|owned| {
            gitterm_core::workspace_config::RepoKey::local(owned)
                == gitterm_core::workspace_config::RepoKey::local(path)
        })
    });
    gitterm_core::task_worktree::check_unmanaged_removal(
        Some(repository),
        path,
        managed.map(|task| task.title.as_str()),
    )
}

fn managed_task(path: &std::path::Path, cx: &App) -> Option<gitterm_core::tasks::TaskRecord> {
    cx.try_global::<crate::task_host::GpuiTaskHost>()?
        .0
        .read(cx)
        .host()
        .store()
        .as_ref()?
        .tasks()
        .iter()
        .find(|t| {
            t.worktree.path.as_deref().is_some_and(|p| {
                gitterm_core::workspace_config::RepoKey::local(p)
                    == gitterm_core::workspace_config::RepoKey::local(path)
            })
        })
        .cloned()
}

impl crate::Shell {
    pub(crate) fn live_worktree_action(
        &mut self,
        workspace: WorkspaceId,
        action: &GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gitterm_core::{
            task_worktree as rules,
            workspace_config::{RepoKey, WorkspaceTarget},
        };
        let Some(desktop) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .cloned()
        else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Worktree actions are not available in preview".into());
                cx.notify();
            });
            return;
        };
        let Some(saved) = self.owner.read(cx).iced_workspace(workspace).cloned() else {
            return;
        };
        let target = WorkspaceTarget::of(&saved);
        let repository = PathBuf::from(&saved.dir);
        let (path, force) = match action {
            GitAction::OpenWorktree(path) | GitAction::AdoptWorktree(path) => (path.clone(), false),
            GitAction::RemoveWorktree { path, force } => (path.clone(), *force),
            _ => return,
        };
        let Some(view) = self
            .workspaces
            .iter()
            .find(|w| w.workspace.id == workspace)
            .and_then(|w| w.panes.values().find_map(|p| p.read(cx).git.clone()))
        else {
            return;
        };
        let local = !saved.is_legacy_remote()
            && !matches!(
                saved.location,
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
            );
        let managed = managed_task(&path, cx);
        if matches!(action, GitAction::RemoveWorktree { .. }) {
            if let Err(error) = rules::check_unmanaged_removal(
                local.then_some(repository.as_path()),
                &path,
                managed.as_ref().map(|t| t.title.as_str()),
            ) {
                view.update(cx, |v, cx| v.finish_operation(&path, Err(error), false, cx));
                return;
            }
        } else if matches!(action, GitAction::AdoptWorktree(_)) {
            if managed.is_some() {
                return;
            }
            if !local {
                view.update(cx, |v, cx| {
                    v.finish_operation(
                        &path,
                        Err("Adopting a worktree needs a local workspace".into()),
                        false,
                        cx,
                    )
                });
                return;
            }
        }
        let lease = match self.owner.read(cx).worktree_operations.begin(&path) {
            Ok(lease) => lease,
            Err(error) => {
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some(error);
                    cx.notify();
                });
                return;
            }
        };
        let selected_before = self
            .owner
            .read(cx)
            .iced_sessions(&saved)
            .iter()
            .zip(&saved.tabs)
            .find(|(_, tab)| {
                RepoKey::local(std::path::Path::new(&tab.dir)) == RepoKey::local(&path)
                    || tab.repo_dir.as_deref().is_some_and(|dir| {
                        RepoKey::local(std::path::Path::new(dir)) == RepoKey::local(&path)
                    })
            })
            .map(|(session, _)| session.id);
        let previous = match view.update(cx, |v, cx| v.begin_operation(&path, force, cx)) {
            Ok(previous) => previous,
            Err(error) => {
                self.owner.update(cx, |o, cx| {
                    o.error = Some(error);
                    cx.notify();
                });
                return;
            }
        };
        let removing = matches!(action, GitAction::RemoveWorktree { .. });
        let action = action.clone();
        let path_for_job = path.clone();
        let repository_for_job = repository.clone();
        let store = desktop.0.clone();
        let target_for_job = target.clone();
        let work = cx.background_executor().spawn(async move {
            match &action {
                GitAction::OpenWorktree(_) => {
                    rules::check_open_worktree(&repository_for_job, &path_for_job)?;
                    let file = store
                        .try_edit_workspaces(move |file| {
                            file.open_worktree_tab(&target_for_job, &path_for_job)?;
                            Ok(())
                        })
                        .wait()?;
                    Ok(WorktreeResult::Open(file))
                }
                GitAction::AdoptWorktree(_) => {
                    let resolution =
                        rules::resolve_worktree_adoption_blocking(&rules::AdoptWorktreeRequest {
                            repository_path: repository_for_job,
                            worktree_path: path_for_job,
                        })
                        .map_err(|e| e.to_string())?;
                    Ok(WorktreeResult::Adopt(resolution))
                }
                GitAction::RemoveWorktree { .. } => {
                    let force = rules::removal_force(force, previous.as_deref())?;
                    recheck_removal_ownership(store.root(), &repository_for_job, &path_for_job)?;
                    rules::remove_unmanaged_worktree_blocking(
                        &repository_for_job,
                        &path_for_job,
                        force,
                    )?;
                    Ok(WorktreeResult::Removed)
                }
                _ => unreachable!(),
            }
        });
        cx.spawn_in(window, async move |shell, cx| {
            let _lease = lease;
            let result = work.await;
            let result = match result {
                Ok(WorktreeResult::Adopt(resolution)) => {
                    let save = shell.update(cx, |shell, cx| {
                        shell.adopt_worktree(&resolution, &saved.name, &path, cx)
                    });
                    match save {
                        Ok(Ok(save)) => {
                            let result = cx
                                .background_executor()
                                .spawn(async move { save.wait().await })
                                .await;
                            result.map(WorktreeResult::Adopted)
                        }
                        Ok(Err(error)) => Err(error),
                        Err(error) => Err(error.to_string()),
                    }
                }
                result => result,
            };
            _ = shell.update_in(cx, |shell, window, cx| {
                let result = match result {
                    Ok(WorktreeResult::Open(file)) => {
                        let identity = target.index(&file).ok().and_then(|index| {
                            let ws = &file.workspaces[index];
                            ws.tabs
                                .get(ws.active_tab?)
                                .map(|tab| tab.session_identity())
                        });
                        shell.repo_document_saved(desktop.0.root(), file, cx);
                        let selected_imported = shell
                            .owner
                            .read(cx)
                            .iced_workspace(workspace)
                            .and_then(|saved| {
                                shell
                                    .owner
                                    .read(cx)
                                    .iced_sessions(saved)
                                    .get(saved.active_tab?)
                                    .map(|session| session.id)
                            });
                        shell.select_workspace_id(workspace, window, cx);
                        let existing = shell
                            .workspaces
                            .iter()
                            .find(|w| w.workspace.id == workspace)
                            .and_then(|w| {
                                w.sessions
                                    .iter()
                                    .find(|s| {
                                        Some(s.id) == selected_before
                                            || Some(s.id) == selected_imported
                                            || w.panes
                                                .get(&crate::session_pane_id(s.id))
                                                .is_some_and(|pane| {
                                                    pane.read(cx).session.as_ref().is_some_and(
                                                        |target| {
                                                            identity.as_ref().and_then(|saved| {
                                                                saved.session_uid.as_deref()
                                                            }) == Some(target.session_uid.as_str())
                                                        },
                                                    )
                                                })
                                            || w.panes
                                                .get(&crate::session_pane_id(s.id))
                                                .and_then(|p| p.read(cx).task_folder(cx))
                                                .is_some_and(|cwd| {
                                                    RepoKey::local(&cwd) == RepoKey::local(&path)
                                                })
                                            || w.store.terminals.as_ref().is_some_and(|tabs| {
                                                tabs.iter().any(|tab| {
                                                    tab.id == s.id.0
                                                        && RepoKey::local(&tab.cwd)
                                                            == RepoKey::local(&path)
                                                })
                                            })
                                    })
                                    .map(|s| s.id)
                            });
                        if let Some(session) = existing {
                            shell.select_session_id(session, window, cx);
                        } else {
                            shell.new_terminal_at(
                                gpui_component::dock::DockPlacement::Center,
                                Some(path.clone()),
                                window,
                                cx,
                            );
                        }
                        if let Some(identity) = identity {
                            let state = shell.active(cx);
                            if let Some(pane) = state
                                .active_session
                                .and_then(|id| state.panes.get(&crate::session_pane_id(id)))
                            {
                                pane.update(cx, |pane, cx| {
                                    if let Some(uid) = identity.session_uid {
                                        pane.session
                                            .get_or_insert_with(crate::sessions::fresh_target)
                                            .session_uid = uid;
                                    }
                                    cx.notify();
                                });
                            }
                        }
                        // The V6 tab is now the restore authority, including
                        // when Open focused an older GPUI-only terminal.
                        if let Some(state) = shell
                            .workspaces
                            .iter_mut()
                            .find(|w| w.workspace.id == workspace)
                        {
                            if let (Some(active), Some(terminals)) =
                                (state.active_session, &mut state.store.terminals)
                            {
                                terminals.retain(|terminal| terminal.id != active.0);
                            }
                            crate::save_layout(state, cx);
                        }
                        Ok(())
                    }
                    Ok(WorktreeResult::Adopted(file)) => {
                        shell.repo_document_saved(desktop.0.root(), file, cx);
                        Ok(())
                    }
                    Ok(WorktreeResult::Adopt(_)) => unreachable!(),
                    Ok(WorktreeResult::Removed) => Ok(()),
                    Err(error) => Err(error),
                };
                view.update(cx, |v, cx| v.finish_operation(&path, result, removing, cx));
                cx.notify();
            });
        })
        .detach();
    }

    fn adopt_worktree(
        &mut self,
        resolution: &gitterm_core::task_worktree::AdoptedWorktreeResolution,
        name: &str,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Result<AdoptionSave, String> {
        use gitterm_core::{task_worktree as rules, tasks::*};
        let host = cx
            .try_global::<crate::task_host::GpuiTaskHost>()
            .map(|h| h.0.clone())
            .ok_or("The task store is unavailable")?;
        let desktop = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .cloned()
            .ok_or("The workspace store is unavailable")?;
        let task_id = format!("task-{}", &TaskRecord::fresh_id()[..8]);
        let timestamp = chrono::Utc::now().to_rfc3339();
        let mut anchors = Vec::new();
        for state in &self.workspaces {
            let saved = self.owner.read(cx).iced_workspace(state.workspace.id);
            let imported = saved
                .map(|saved| self.owner.read(cx).iced_sessions(saved))
                .unwrap_or_default();
            for session in &state.sessions {
                let Some(pane) = state.panes.get(&crate::session_pane_id(session.id)) else {
                    continue;
                };
                let document = saved.and_then(|saved| {
                    let uid = pane
                        .read(cx)
                        .session
                        .as_ref()
                        .map(|target| target.session_uid.as_str());
                    uid.and_then(|uid| {
                        saved
                            .tabs
                            .iter()
                            .position(|tab| tab.session_uid.as_deref() == Some(uid))
                    })
                    .or_else(|| imported.iter().position(|s| s.id == session.id))
                    .and_then(|index| saved.tabs.get(index).map(|tab| (saved, index, tab)))
                });
                let cwd = pane.read(cx).task_folder(cx).or_else(|| {
                    document
                        .map(|(_, _, tab)| PathBuf::from(tab.repo_dir.as_ref().unwrap_or(&tab.dir)))
                });
                let Some(cwd) = cwd.filter(|cwd| cwd.starts_with(path)) else {
                    continue;
                };
                if pane
                    .read(cx)
                    .session
                    .as_ref()
                    .is_some_and(|target| target.task_id.is_some())
                    || document.is_some_and(|(_, _, tab)| tab.task_id.is_some())
                {
                    continue;
                }
                let terminal = state
                    .store
                    .terminals
                    .as_ref()
                    .and_then(|tabs| tabs.iter().find(|tab| tab.id == session.id.0));
                if terminal.is_some_and(|tab| tab.task_id.is_some()) {
                    continue;
                }
                let source = if terminal.is_some() {
                    AnchorSource::Terminal {
                        workspace: state.workspace.id,
                        session: session.id,
                    }
                } else if let Some((saved, index, tab)) = document {
                    AnchorSource::Document {
                        target: Box::new(gitterm_core::workspace_config::WorkspaceTarget::of(
                            saved,
                        )),
                        index,
                        expected_uid: tab.session_uid.clone(),
                        cwd: cwd.clone(),
                    }
                } else {
                    return Err(format!(
                        "No persisted anchor for session {} in {}",
                        session.id.0,
                        cwd.display()
                    ));
                };
                let uid = pane
                    .read(cx)
                    .session
                    .as_ref()
                    .map(|target| target.session_uid.clone())
                    .or_else(|| document.and_then(|(_, _, tab)| tab.session_uid.clone()))
                    .unwrap_or_else(TaskRecord::fresh_id);
                anchors.push(AdoptionAnchor {
                    source,
                    pane: pane.clone(),
                    uid,
                    session_id: TaskRecord::fresh_id(),
                    label: if matches!(
                        pane.read(cx).content,
                        crate::pane::PaneContent::PreviewChat(_)
                    ) {
                        "Agent"
                    } else {
                        "Terminal"
                    },
                });
            }
        }
        host.update(cx, |host, cx| {
            let store = host
                .host_mut()
                .store_mut()
                .as_mut()
                .ok_or("The task store is unavailable")?;
            if store.tasks().iter().any(|t| {
                t.worktree.path.as_deref().is_some_and(|p| {
                    gitterm_core::workspace_config::RepoKey::local(p)
                        == gitterm_core::workspace_config::RepoKey::local(path)
                })
            }) {
                return Err("Worktree already belongs to a task".into());
            }
            rules::persist_adopted_worktree(
                store,
                resolution,
                name.to_owned(),
                path,
                task_id.clone(),
                &timestamp,
            )
            .map_err(|e| e.to_string())?;
            for anchor in &anchors {
                store
                    .upsert_session(
                        &task_id,
                        TaskSessionRecord {
                            task_session_id: anchor.session_id.clone(),
                            label: anchor.label.into(),
                            harness: None,
                            conversation: None,
                            objective_delivery: ObjectiveDeliveryState::NotDelivered,
                            launch_identity: None,
                            created_at: timestamp.clone(),
                            updated_at: timestamp.clone(),
                        },
                        &timestamp,
                    )
                    .map_err(|e| e.to_string())?;
            }
            cx.notify();
            Ok::<(), String>(())
        })?;
        let documents: Vec<_> = anchors
            .iter()
            .filter_map(|anchor| match &anchor.source {
                AnchorSource::Document {
                    target,
                    index,
                    expected_uid,
                    cwd,
                } => Some((
                    target.clone(),
                    *index,
                    expected_uid.clone(),
                    cwd.clone(),
                    anchor.uid.clone(),
                    anchor.session_id.clone(),
                )),
                _ => None,
            })
            .collect();
        let document_task = task_id.clone();
        let workspaces = desktop.0.try_edit_workspaces(move |file| {
            for (target, index, expected_uid, cwd, uid, session_id) in documents {
                let workspace = target.index(file)?;
                let tabs = &mut file.workspaces[workspace].tabs;
                let index = if let Some(expected) = &expected_uid {
                    tabs.iter()
                        .position(|tab| tab.session_uid.as_ref() == Some(expected))
                        .ok_or("Adoption tab no longer exists")?
                } else {
                    index
                };
                let tab = tabs.get_mut(index).ok_or("Adoption tab no longer exists")?;
                if tab.session_uid != expected_uid {
                    return Err("Adoption tab identity changed".into());
                }
                if tab.task_id.is_some()
                    || !(std::path::Path::new(&tab.dir) == cwd
                        || tab
                            .repo_dir
                            .as_deref()
                            .is_some_and(|dir| std::path::Path::new(dir) == cwd))
                {
                    return Err("Adoption tab identity changed".into());
                }
                tab.session_uid = Some(uid);
                tab.task_id = Some(document_task.clone());
                tab.task_session_id = Some(session_id);
            }
            Ok(())
        });
        let mut layouts = Vec::new();
        for state in &mut self.workspaces {
            let mut changed = false;
            for anchor in &anchors {
                if let AnchorSource::Terminal { workspace, session } = anchor.source
                    && workspace == state.workspace.id
                {
                    let terminal = state
                        .store
                        .terminals
                        .as_mut()
                        .and_then(|tabs| tabs.iter_mut().find(|tab| tab.id == session.0))
                        .ok_or("Adoption terminal anchor no longer exists")?;
                    terminal.task_id = Some(task_id.clone());
                    terminal.task_session_id = Some(anchor.session_id.clone());
                    changed = true;
                }
            }
            if changed {
                layouts.push(
                    state
                        .store
                        .save_async(&state.dock.read(cx).dump(cx))
                        .map_err(|e| format!("Save adopted terminal: {e:#}"))?,
                );
            }
        }
        for anchor in anchors {
            anchor.pane.update(cx, |pane, cx| {
                let target = pane
                    .session
                    .get_or_insert_with(crate::sessions::fresh_target);
                target.session_uid = anchor.uid;
                target.task_id = Some(task_id.clone());
                target.task_session_id = Some(anchor.session_id);
                cx.notify();
            });
        }
        Ok(AdoptionSave {
            workspaces,
            layouts,
        })
    }
}

enum AnchorSource {
    Document {
        target: Box<gitterm_core::workspace_config::WorkspaceTarget>,
        index: usize,
        expected_uid: Option<String>,
        cwd: PathBuf,
    },
    Terminal {
        workspace: WorkspaceId,
        session: crate::data::SessionId,
    },
}
struct AdoptionAnchor {
    source: AnchorSource,
    pane: Entity<crate::pane::Pane>,
    uid: String,
    session_id: String,
    label: &'static str,
}
struct AdoptionSave {
    workspaces:
        gitterm_core::desktop_store::Receipt<gitterm_core::workspace_config::WorkspacesFile>,
    layouts: Vec<crate::preview::SaveResult>,
}
impl AdoptionSave {
    async fn wait(self) -> Result<gitterm_core::workspace_config::WorkspacesFile, String> {
        // Drain every submitted save, even if another part of the adoption fails.
        let document = self.workspaces.wait();
        let mut errors = Vec::new();
        for layout in self.layouts {
            if let Err(error) = crate::preview::finish(Ok(layout)).await {
                errors.push(format!("Save adopted terminal: {error:#}"));
            }
        }
        match document {
            Ok(file) if errors.is_empty() => Ok(file),
            Ok(_) => Err(errors.join("; ")),
            Err(error) => {
                errors.push(error);
                Err(errors.join("; "))
            }
        }
    }
}

enum WorktreeResult {
    Open(gitterm_core::workspace_config::WorkspacesFile),
    Adopt(gitterm_core::task_worktree::AdoptedWorktreeResolution),
    Adopted(gitterm_core::workspace_config::WorkspacesFile),
    Removed,
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use ::core::prelude::v1::test;
    use gpui::TestAppContext;

    #[gpui::test]
    fn live_open_uses_the_saved_tab_identity_without_duplicate_restoration(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (root, shell, cx) = crate::h5_tests::fixture(cx);
        let main = root.path().join("work");
        let linked = root.path().join("linked");
        let git = |args: &[&str]| {
            let output = gitterm_core::git_status::git_command()
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["add", "file.txt"]);
        git(&[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "initial",
        ]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ]);
        let desktop =
            gitterm_core::desktop_store::DesktopStore::open(&root.path().join("iced")).unwrap();
        cx.update(|_, cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
        let id = cx.read(|cx| shell.read(cx).active_workspace(cx).unwrap());
        for _ in 0..2 {
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| {
                    shell.git_action(id, &GitAction::OpenWorktree(linked.clone()), window, cx)
                })
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                cx.run_until_parked();
                let complete = cx.read(|cx| {
                    let shell = shell.read(cx);
                    let state = shell.active(cx);
                    state.sessions.len() == 2
                        && state
                            .panes
                            .values()
                            .filter_map(|p| p.read(cx).git.as_ref())
                            .all(|view| {
                                !view
                                    .read(cx)
                                    .operations
                                    .get(&linked)
                                    .is_some_and(|op| op.busy)
                            })
                });
                if complete {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "Live open timed out");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        cx.read(|cx| {
            let shell = shell.read(cx);
            let state = shell.active(cx);
            let owner = shell.owner.read(cx);
            let saved = owner.iced_workspace(id).unwrap();
            assert_eq!(saved.tabs.len(), 1);
            let imported = crate::data::iced_sessions(saved)[0].id;
            assert_eq!(state.active_session, Some(imported));
            assert!(
                !state
                    .store
                    .terminals
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|t| t.id == imported.0),
                "V6 owns this tab's restore; no second local terminal entry"
            );
            assert_eq!(
                state.panes[&crate::session_pane_id(imported)]
                    .read(cx)
                    .session
                    .as_ref()
                    .unwrap()
                    .session_uid,
                saved.tabs[0].session_uid.as_deref().unwrap()
            );
            let disk: gitterm_core::workspace_config::WorkspacesFile = serde_json::from_slice(
                &std::fs::read(root.path().join("iced/workspaces.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(disk.workspaces[0].tabs.len(), 1);
            assert_eq!(disk.workspaces[0].tabs[0].dir, linked.to_string_lossy());
        });
    }

    #[test]
    fn review_regression_f3_shared_canonical_guard_and_background_ownership_recheck() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let path = root.path().join("linked");
        std::fs::create_dir(&path).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let shared = WorktreeOperations::default();
        let other_workspace = shared.clone();
        let adopting = shared.begin(&path).unwrap();
        assert!(other_workspace.begin(&alias).is_err());
        drop(adopting);
        let _removing = other_workspace.begin(&alias).unwrap();
        recheck_removal_ownership(root.path(), root.path(), &path).unwrap();
        let mut tasks =
            gitterm_core::tasks::TaskStore::load(root.path().join("tasks.json")).unwrap();
        let mut task = crate::tasks::fixture_task("owned", "Placeholder", "task/placeholder");
        task.worktree.path = Some(path.clone());
        task.worktree.state = gitterm_core::tasks::TaskWorktreeState::Ready;
        tasks.insert(task).unwrap();
        assert!(
            recheck_removal_ownership(root.path(), root.path(), &path)
                .unwrap_err()
                .contains("belongs to task")
        );
        assert!(path.is_dir());
    }

    fn saved_worktree_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let main = root.path().join("main");
        let linked = root.path().join("linked");
        std::fs::create_dir(&main).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec![
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
            vec![
                "worktree",
                "add",
                "-q",
                "-b",
                "task/placeholder",
                linked.to_str().unwrap(),
            ],
        ] {
            let output = gitterm_core::git_status::git_command()
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let source = root.path().join("iced");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("workspaces.json"), serde_json::json!({"active_workspace":0,"workspaces":[{
            "name":"Placeholder", "abbrev":"P", "color":"blue", "dir":main, "repos":[main], "tabs":[
                {"dir":main, "session_uid":"main-shell"},
                {"dir":linked, "repo_dir":linked, "session_uid":"linked-shell"}
            ]
        }]}).to_string()).unwrap();
        (root, main, linked)
    }

    #[gpui::test]
    fn review_regression_f5_open_after_restart_focuses_imported_worktree_tab(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let (root, _, linked) = saved_worktree_fixture();
        let source = root.path().join("iced");
        let desktop = gitterm_core::desktop_store::DesktopStore::open(&source).unwrap();
        cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
        let mut store = crate::preview::tests::guarded_store(&root.path().join("preview"));
        let model = crate::workspace_config::load_iced_workspaces_from(&mut store, &source);
        let layout = store.guard.as_ref().unwrap().preview().layout();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            crate::Shell::with_owner(
                Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                &layout,
                true,
                window,
                cx,
            )
            .unwrap()
        });
        let imported = cx.read(|cx| shell.read(cx).active(cx).sessions[1].id);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(
                shell.active(cx).panes[&crate::session_pane_id(imported)]
                    .read(cx)
                    .task_folder(cx)
                    .is_none()
            );
        });
        for _ in 0..2 {
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| {
                    shell.live_worktree_action(
                        WorkspaceId(1),
                        &GitAction::OpenWorktree(linked.clone()),
                        window,
                        cx,
                    )
                })
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                cx.run_until_parked();
                if cx.read(|cx| shell.read(cx).active(cx).active_session == Some(imported)) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "saved tab did not focus"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.active(cx).sessions.len(), 2);
            assert!(matches!(
                shell.active(cx).panes[&crate::session_pane_id(imported)]
                    .read(cx)
                    .content,
                crate::pane::PaneContent::IcedSession(_)
            ));
        });
    }

    #[gpui::test]
    async fn review_regression_f6_adoption_persists_document_and_gpui_terminal_anchors(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        use crate::task_host::{GpuiTaskHost, HostConfig, Ingress, TaskHostOwner};
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let (root, main, linked) = saved_worktree_fixture();
        let source = root.path().join("iced");
        let desktop = gitterm_core::desktop_store::DesktopStore::open(&source).unwrap();
        cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
        let mut store = crate::preview::tests::guarded_store(&root.path().join("preview"));
        let model = crate::workspace_config::load_iced_workspaces_from(&mut store, &source);
        let roots = store.guard.as_ref().unwrap().preview().clone();
        let layout = roots.layout();
        let (sender, receiver) = gitterm_core::task_commands::runtime::command_channel();
        let host = cx
            .update(|cx| {
                TaskHostOwner::start(
                    HostConfig {
                        roots,
                        iced_config: source.clone(),
                        ingress: Ingress::Channel(sender, receiver),
                    },
                    cx,
                )
            })
            .unwrap();
        cx.update(|cx| cx.set_global(GpuiTaskHost(host.clone())));
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            crate::Shell::with_owner(
                Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                &layout,
                true,
                window,
                cx,
            )
            .unwrap()
        });
        let resolution = gitterm_core::task_worktree::resolve_worktree_adoption_blocking(
            &gitterm_core::task_worktree::AdoptWorktreeRequest {
                repository_path: main,
                worktree_path: linked.clone(),
            },
        )
        .unwrap();
        let save = cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                let state = &mut shell.workspaces[0];
                let imported = state.sessions[1].id;
                let pane = state
                    .panes
                    .remove(&crate::session_pane_id(imported))
                    .unwrap();
                let runtime = crate::data::SessionId(888);
                pane.update(cx, |pane, _| {
                    let mut target = crate::sessions::fresh_target();
                    target.session_uid = "linked-shell".into();
                    pane.session = Some(target);
                    pane.id = crate::session_pane_id(runtime);
                });
                state.sessions[1].id = runtime;
                state.panes.insert(crate::session_pane_id(runtime), pane);
                let id = crate::data::SessionId(777);
                let pane_id = crate::session_pane_id(id);
                let pane = cx.new(|cx| {
                    crate::pane::Pane::new(
                        pane_id.clone(),
                        "Extra".into(),
                        crate::pane::PaneKind::StoppedTerminal(linked.clone(), true),
                        WorkspaceId(1),
                        shell.data.clone(),
                        window,
                        cx,
                    )
                });
                let state = &mut shell.workspaces[0];
                state.sessions.push(crate::data::Session {
                    id,
                    title: "Extra".into(),
                    kind: crate::data::SessionKind::Terminal,
                });
                state.panes.insert(pane_id, pane);
                // A runtime session without either durable source is refused.
                assert!(
                    shell
                        .adopt_worktree(&resolution, "Placeholder", &linked, cx)
                        .err()
                        .unwrap()
                        .contains("No persisted anchor")
                );
                shell.workspaces[0].store.terminals.as_mut().unwrap().push(
                    crate::layout::TerminalSession {
                        id: 777,
                        cwd: linked.clone(),
                        startup_command: None,
                        task_id: None,
                        task_session_id: None,
                        session_uid: None,
                        chat_session_id: None,
                        bottom: false,
                    },
                );
                shell
                    .adopt_worktree(&resolution, "Placeholder", &linked, cx)
                    .unwrap()
            })
        });
        let file = save.wait().await.unwrap();
        let document = &file.workspaces[0].tabs[1];
        let task_id = document.task_id.as_deref().unwrap();
        let layout_path = cx.read(|cx| shell.read(cx).workspaces[0].store.path.clone());
        let mut restored = crate::layout::LayoutStore::new(layout_path);
        let terminal = restored
            .load_terminals()
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|tab| tab.id == 777)
            .unwrap();
        assert_eq!(terminal.task_id.as_deref(), Some(task_id));
        let task = cx.read(|cx| {
            host.read(cx)
                .host()
                .store()
                .as_ref()
                .unwrap()
                .get(task_id)
                .unwrap()
                .clone()
        });
        assert!(
            task.sessions
                .iter()
                .any(|session| Some(&session.task_session_id) == terminal.task_session_id.as_ref())
        );
        assert!(
            task.sessions
                .iter()
                .any(|session| Some(&session.task_session_id) == document.task_session_id.as_ref())
        );
    }

    #[gpui::test]
    fn force_is_offered_only_for_a_failed_normal_removal(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let path = PathBuf::from("/fixture/linked");
        let view = cx.new(|cx| GitView::new(WorkspaceId(1), Rc::new(crate::data::SampleData), cx));
        view.update(cx, |view, cx| {
            assert!(view.begin_operation(&path, true, cx).is_err());
            view.begin_operation(&path, false, cx).unwrap();
            assert!(view.begin_operation(&path, false, cx).is_err());
            view.finish_operation(
                &path,
                Err("contains modified or untracked files, use --force".into()),
                true,
                cx,
            );
            let error = view.begin_operation(&path, true, cx).unwrap();
            assert!(
                error
                    .as_deref()
                    .is_some_and(gitterm_core::task_worktree::offer_force)
            );
            view.finish_operation(&path, Err("not registered".into()), true, cx);
            assert!(view.begin_operation(&path, true, cx).is_err());
        });
    }
}

#[cfg(test)]
mod tab_context_tests {
    use super::*;
    use crate::{
        WorkspaceOwner,
        data::{LocalData, SessionId},
        workspace_status::WorkspaceStatus,
    };
    use ::core::prelude::v1::test;

    #[gpui::test]
    fn tab_context_gap3_git_mode_and_selection_return_with_the_tab(cx: &mut TestAppContext) {
        let _app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let root = tempfile::tempdir().unwrap();
        let mut model = WorkspaceStatus::default();
        let workspace = model.add(root.path().into(), std::time::Instant::now());
        let first = SessionId(41);
        let second = SessionId(42);
        model.active_sessions.insert(workspace, first);
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        let view = cx.new(|cx| GitView::new(workspace, Rc::new(LocalData(owner.clone())), cx));
        view.update(cx, |view, cx| {
            view.sync_session(cx);
            view.mode = Mode::Worktrees;
            view.selected = Some(Selection::Branch("feature".into()));
            view.selected_file = Some(("file.txt".into(), true));
        });
        for (id, expected) in [(second, false), (first, true)] {
            owner.update(cx, |owner, cx| {
                owner.model.active_sessions.insert(workspace, id);
                cx.notify();
            });
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.mode == Mode::Worktrees, expected);
                assert_eq!(
                    view.selected,
                    expected.then(|| Selection::Branch("feature".into()))
                );
                assert_eq!(
                    view.selected_file,
                    expected.then(|| ("file.txt".into(), true))
                );
            });
        }
    }
}
