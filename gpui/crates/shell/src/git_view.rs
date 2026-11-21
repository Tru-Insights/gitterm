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
    RemoveWorktree(PathBuf),
    EditFile(PathBuf),
}
impl EventEmitter<GitAction> for GitView {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    Worktree(PathBuf),
    Branch(String),
}

pub(crate) struct GitView {
    workspace: WorkspaceId,
    data: Rc<dyn ShellData>,
    mode: Mode,
    selected: Option<Selection>,
    selected_file: Option<(String, bool)>,
    snapshot: Option<Snapshot>,
    loading: bool,
    generation: u64,
}
impl GitView {
    pub(crate) fn new(
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(owner) = data.local_owner() {
            cx.observe(&owner, |this, _, cx| {
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
        }
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
                if !row.current {
                    if row.available && !row.detached && !row.prunable {
                        let path = path.clone();
                        actions = actions.child(
                            Button::new("git-adopt")
                                .ghost()
                                .label("Adopt as task")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(GitAction::AdoptWorktree(path.clone()))
                                })),
                        );
                    }
                    let path = path.clone();
                    actions =
                        actions.child(Button::new("git-remove").ghost().label("Remove").on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(GitAction::RemoveWorktree(path.clone()))
                            }),
                        ));
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
                        let status = gitterm_core::git_status::collect_repo_status(&row.path);
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
