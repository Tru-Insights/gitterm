//! Iced's flat directory browser; filesystem work stays with WorkspaceOwner.
use crate::{
    WorkspaceOwner,
    data::{FileNode, ShellData, WorkspaceId},
    files::{Entry, Row},
    theme::{LABEL_FONT, LABEL_LINE_HEIGHT, Palette},
};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::ActiveTheme;
use std::{collections::HashMap, path::PathBuf, rc::Rc};

#[derive(Clone)]
enum EntryAction {
    Directory(PathBuf),
    Open(PathBuf),
}
pub(crate) struct FilesView {
    owner: Option<Entity<WorkspaceOwner>>,
    workspace: WorkspaceId,
    actions: HashMap<SharedString, EntryAction>,
    rows: Vec<Row>,
    roots: Option<Vec<PathBuf>>,
    browsing_root: Option<PathBuf>,
    directory: PathBuf,
    source_root: PathBuf,
    selected_file: Option<PathBuf>,
    samples: Vec<FileNode>,
}
impl FilesView {
    pub(crate) fn new(
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        cx: &mut Context<Self>,
    ) -> Self {
        let owner = data.local_owner();
        let roots = owner.as_ref().and_then(|owner| {
            let saved = owner.read(cx).iced_workspace(workspace)?;
            let work = gitterm_core::work::catalog::is_work(&saved.chats, false);
            work.then(|| work_roots(&saved.repos, PathBuf::from(&saved.dir).join("docs")))
        });
        let directory = owner
            .as_ref()
            .and_then(|owner| {
                owner
                    .read(cx)
                    .files
                    .get(&workspace)
                    .map(|tree| tree.root.clone())
            })
            .or_else(|| data.terminal_cwd(workspace, cx))
            .unwrap_or_default();
        let source_root = owner
            .as_ref()
            .and_then(|owner| {
                owner
                    .read(cx)
                    .model
                    .workspaces
                    .iter()
                    .find(|local| local.id == workspace)
                    .map(|local| local.root.clone())
            })
            .unwrap_or_else(|| directory.clone());
        let mut this = Self {
            owner,
            workspace,
            actions: HashMap::new(),
            rows: vec![],
            roots,
            browsing_root: None,
            source_root,
            directory,
            selected_file: None,
            samples: vec![],
        };
        if let Some(owner) = this.owner.clone() {
            cx.observe(&owner, |this, _, cx| this.sync(cx)).detach();
            cx.subscribe(&owner, |this, _, event: &crate::FileOpen, cx| {
                if event.workspace == this.workspace {
                    this.selected_file = event.from_files.then(|| event.path.clone());
                    cx.notify();
                }
            })
            .detach();
            this.sync(cx);
        } else {
            this.samples = data.files(workspace, cx);
            this.sync(cx);
        }
        this
    }
    fn browse_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.browsing_root = Some(root.clone());
        self.source_root = root.clone();
        self.navigate(root, cx);
    }
    fn navigate(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        self.directory = directory.clone();
        if let Some(owner) = &self.owner {
            owner.update(cx, |owner, cx| {
                owner
                    .files
                    .entry(self.workspace)
                    .or_default()
                    .set_root(directory.clone());
                owner.toggle_directory(self.workspace, directory, cx);
            });
        }
        self.sync(cx);
    }
    fn parent(&self) -> Option<PathBuf> {
        self.directory
            .parent()
            .filter(|parent| parent.starts_with(&self.source_root))
            .map(|parent| parent.to_path_buf())
    }
    fn sync(&mut self, cx: &mut Context<Self>) {
        self.actions.clear();
        if let Some(owner) = self.owner.clone() {
            if owner
                .read(cx)
                .files
                .get(&self.workspace)
                .is_some_and(|tree| tree.root != self.directory)
            {
                self.navigate(self.directory.clone(), cx);
                return;
            }
            let rows = owner
                .read(cx)
                .files
                .get(&self.workspace)
                .map(|tree| tree.rows())
                .unwrap_or_default();
            // The owner's shared tree API pages reads. Finish them automatically:
            // Iced exposes the complete directory, never an actionable pager.
            let more = rows
                .iter()
                .any(|row| matches!(row, Row::More { depth: 0, .. }));
            let loading = rows
                .iter()
                .any(|row| matches!(row, Row::Loading { depth: 0 }));
            let error = rows
                .iter()
                .any(|row| matches!(row, Row::Error { depth: 0, .. }));
            if more && !error {
                self.rows = vec![Row::Loading { depth: 0 }];
                if !loading {
                    owner.update(cx, |owner, cx| {
                        owner.more_files(self.workspace, self.directory.clone(), cx)
                    });
                }
            } else {
                self.rows = rows
                    .into_iter()
                    .filter(|row| match row {
                        Row::Entry { depth, .. }
                        | Row::Loading { depth }
                        | Row::Empty { depth }
                        | Row::Error { depth, .. } => *depth == 0,
                        Row::More { .. } => false,
                    })
                    .collect();
            }
        } else {
            let mut nodes = &self.samples;
            if let Ok(relative) = self.directory.strip_prefix(&self.source_root) {
                for name in relative.components() {
                    nodes = nodes
                        .iter()
                        .find(|node| node.name == name.as_os_str().to_string_lossy())
                        .map(|node| &node.children)
                        .expect("sample navigation only uses existing directories");
                }
            }
            self.rows = nodes
                .iter()
                .map(|node| Row::Entry {
                    entry: Entry {
                        path: self.directory.join(&node.name),
                        name: node.name.clone(),
                        is_dir: !node.children.is_empty(),
                    },
                    expanded: false,
                    depth: 0,
                })
                .collect();
        }
        for row in &self.rows {
            if let Row::Entry { entry, .. } = row {
                self.actions.insert(
                    entry_id(entry),
                    if entry.is_dir {
                        EntryAction::Directory(entry.path.clone())
                    } else {
                        EntryAction::Open(entry.path.clone())
                    },
                );
            }
        }
        cx.notify();
    }
    fn activate(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        match self.actions.get(id).cloned() {
            Some(EntryAction::Directory(path)) => self.navigate(path, cx),
            Some(EntryAction::Open(path)) => {
                self.selected_file = Some(path.clone());
                if let Some(owner) = &self.owner {
                    owner.update(cx, |owner, cx| owner.open_file(self.workspace, path, cx));
                }
                cx.notify();
            }
            None => {}
        }
    }
    fn edit_file(&self, cx: &mut Context<Self>) {
        if let Some(owner) = &self.owner {
            owner.update(cx, |owner, cx| {
                owner.error = Some("Edit file · not available in preview".into());
                cx.notify();
            });
        }
    }
}
fn entry_id(entry: &Entry) -> SharedString {
    format!("entry:{:?}", entry.path).into()
}

// Plain rows avoid Button's internally centered child. Iced uses an 8px
// outer inset and [4, 8] button padding, with no tree indentation.
fn file_button(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .flex_shrink_0()
        .flex()
        .items_start()
        .gap(px(4.))
        .px(px(8.))
        .py(px(4.))
        .cursor_pointer()
}
fn file_icon_color(name: &str, p: Palette) -> u32 {
    match std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
    {
        "ts" | "tsx" => p.blue,
        "js" | "jsx" | "json" => p.yellow,
        "md" => p.green,
        "rs" => p.peach,
        "toml" | "yml" | "yaml" => p.teal,
        "css" | "scss" => {
            if p.base == Palette::for_mode(true).base {
                0xcba6f7
            } else {
                0x8839ef
            }
        }
        "html" => p.red,
        _ => p.subtext0,
    }
}
impl Render for FilesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let font: f32 = cx.theme().font_size.into();
        let mut body = div()
            .id("files-browser")
            .size_full()
            .flex()
            .flex_col()
            .font_family(LABEL_FONT)
            .text_size(px(font))
            .line_height(relative(LABEL_LINE_HEIGHT))
            .text_color(rgb(p.text));
        let mut listing = div()
            .id("files-list")
            .debug_selector(|| "files-list".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_start()
            .gap(px(2.))
            .p(px(8.));
        if let Some(roots) = &self.roots {
            if self.browsing_root.is_none() {
                for (ix, root) in roots.iter().enumerate() {
                    let root = root.clone();
                    let label = format!(
                        "{}/",
                        root.file_name()
                            .unwrap_or(root.as_os_str())
                            .to_string_lossy()
                    );
                    listing = listing.child(
                        file_button(("files-root", ix))
                            .debug_selector(move || format!("files-root-{ix}"))
                            .w_full()
                            .text_color(rgb(p.blue))
                            .aria_label(format!("📁 {label}"))
                            .child(
                                div()
                                    .w(px(24.))
                                    .flex_shrink_0()
                                    .debug_selector(move || format!("files-root-icon-{ix}"))
                                    .child("📁"),
                            )
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browse_root(root.clone(), cx)
                            })),
                    );
                }
                return body.child(listing);
            }
            body = body.child(
                div().flex().flex_shrink_0().p(px(4.)).child(
                    file_button("files-all-folders")
                        .debug_selector(|| "files-all-folders".into())
                        .border_1()
                        .border_color(rgb(p.overlay0))
                        .rounded(px(6.))
                        .px(px(7.))
                        .py(px(3.))
                        .hover(move |row| row.bg(rgb(p.overlay)))
                        .text_size(px(font - 1.))
                        .text_color(rgb(p.blue))
                        .child(
                            div()
                                .debug_selector(|| "files-all-folders-label".into())
                                .child("← All folders"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.browsing_root = None;
                            cx.notify();
                        })),
                ),
            );
        }
        let root_name = self
            .source_root
            .file_name()
            .unwrap_or(self.source_root.as_os_str())
            .to_string_lossy();
        let relative = self
            .directory
            .strip_prefix(&self.source_root)
            .unwrap_or(&self.directory);
        let display = if relative.as_os_str().is_empty() {
            format!("{root_name}/")
        } else {
            format!("{root_name}/{}/", relative.display())
        };
        listing = listing.child(
            div()
                .flex_shrink_0()
                .py(px(4.))
                .text_color(rgb(p.blue))
                .child(display),
        );
        if let Some(parent) = self.parent() {
            listing = listing.child(
                file_button("files-parent")
                    .debug_selector(|| "files-parent".into())
                    .gap(px(8.))
                    .text_color(rgb(p.subtext0))
                    .aria_label(".. (parent)")
                    .child(
                        div()
                            .w(px(20.))
                            .debug_selector(|| "files-parent-icon".into())
                            .child(".."),
                    )
                    .child(div().text_size(px(font - 1.)).child("(parent)"))
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(parent.clone(), cx))),
            );
        }
        for (ix, row) in self.rows.iter().enumerate() {
            match row {
                Row::Entry { entry, .. } => {
                    let id = entry_id(entry);
                    let selected =
                        !entry.is_dir && self.selected_file.as_ref() == Some(&entry.path);
                    let label = format!("{}{}", entry.name, if entry.is_dir { "/" } else { "" });
                    let button = file_button(id.clone())
                        .debug_selector(move || format!("files-entry-{ix}"))
                        .flex_1()
                        .min_w_0()
                        .when(selected, |row| {
                            row.bg(rgb(p.blue))
                                .rounded(px(6.))
                                .hover(move |row| row.bg(rgb(p.blue).blend(rgba(0xffffff26))))
                        })
                        .text_color(rgb(if selected {
                            0xffffff
                        } else if entry.is_dir {
                            p.blue
                        } else {
                            p.text
                        }))
                        .aria_label(if entry.is_dir {
                            format!("📁 {label}")
                        } else {
                            label.clone()
                        })
                        .child(
                            div()
                                .w(px(24.))
                                .flex_shrink_0()
                                .text_color(rgb(if entry.is_dir {
                                    p.blue
                                } else {
                                    file_icon_color(&entry.name, p)
                                }))
                                .debug_selector(move || format!("files-icon-{ix}"))
                                .child(if entry.is_dir { "📁" } else { "  " }),
                        )
                        .child(
                            div()
                                .debug_selector(move || format!("files-name-{ix}"))
                                .child(label),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.activate(&id, cx)));
                    let mut row = div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .w_full()
                        .when(entry.is_dir, |row| row.bg(rgb(p.base)))
                        .child(button);
                    if !entry.is_dir {
                        row = row.child(
                            file_button(("files-edit", ix))
                                .debug_selector(move || format!("files-edit-{ix}"))
                                .px(px(6.))
                                .flex_shrink_0()
                                .text_size(px(font - 1.))
                                .text_color(rgb(p.subtext0))
                                .aria_label(format!("Edit {}", entry.name))
                                .child("✎")
                                .on_click(cx.listener(|this, _, _, cx| this.edit_file(cx))),
                        );
                    }
                    listing = listing.child(row);
                }
                Row::Loading { .. } => {
                    listing = listing.child(
                        div()
                            .text_size(px(font - 1.))
                            .text_color(rgb(p.subtext0))
                            .child("Loading..."),
                    )
                }
                Row::Empty { .. } => {
                    listing =
                        listing.child(div().text_color(rgb(p.subtext0)).child("Empty directory"))
                }
                Row::Error { message, .. } => {
                    listing = listing.child(
                        div()
                            .w_full()
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(6.))
                            .bg(rgb(p.red).opacity(0.12))
                            .text_size(px(font - 1.))
                            .text_color(rgb(p.red))
                            .child(message.clone()),
                    )
                }
                Row::More { .. } => unreachable!("pager rows are consumed by sync"),
            }
        }
        body.child(listing)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "review_tests.rs"]
mod review_tests;

// Iced lists the work's repositories first, then its docs directory, once each.
fn work_roots(repos: &[String], docs: PathBuf) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in repos
        .iter()
        .map(PathBuf::from)
        .chain(docs.is_dir().then_some(docs))
    {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

impl crate::Shell {
    pub(crate) fn files_sidebar(
        &self,
        sidebar: gpui::Stateful<gpui::Div>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let state = self.active(cx);
        sidebar.child(
            div().flex_1().min_h_0().child(
                state.panes[&format!("workspace-{}-files", state.workspace.layout_key)].clone(),
            ),
        )
    }
}
