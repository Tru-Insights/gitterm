//! Presentation and Tree action bridge; filesystem work stays with WorkspaceOwner.
use crate::{
    WorkspaceOwner,
    data::{FileNode, ShellData, WorkspaceId},
    files::{FileTree, Row},
    theme::Palette,
};
use gpui::base::actions::Confirm;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    list::ListItem,
    tree::{TreeEvent, TreeItem, TreeState, tree},
};
use std::{collections::HashMap, path::PathBuf, rc::Rc};

#[derive(Clone)]
enum EntryAction {
    Directory(PathBuf),
    Open(PathBuf),
    More(PathBuf),
}
pub(crate) struct FilesView {
    state: Entity<TreeState>,
    owner: Option<Entity<WorkspaceOwner>>,
    workspace: WorkspaceId,
    actions: HashMap<SharedString, EntryAction>,
    roots: Option<Vec<PathBuf>>,
    browsing_root: Option<PathBuf>,
}
impl FilesView {
    pub(crate) fn new(
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        cx: &mut Context<Self>,
    ) -> Self {
        // 0.9.1 binds Tree arrows, but binds Enter only in other controls.
        // Use its existing Confirm action for both folder and leaf activation.
        cx.bind_keys([KeyBinding::new(
            "enter",
            Confirm { secondary: false },
            Some(gpui::base::tree_key_context()),
        )]);
        let state = cx.new(|cx| TreeState::new(cx));
        let owner = data.local_owner();
        let roots = owner.as_ref().and_then(|owner| {
            let saved = owner.read(cx).iced_workspace(workspace)?;
            let work = gitterm_core::work::catalog::is_work(&saved.chats, false);
            work.then(|| work_roots(&saved.repos, PathBuf::from(&saved.dir).join("docs")))
        });
        let mut this = Self {
            state,
            owner,
            workspace,
            actions: HashMap::new(),
            roots,
            browsing_root: None,
        };
        if let Some(owner) = this.owner.clone() {
            cx.observe(&owner, |this, _, cx| this.sync(cx)).detach();
            cx.subscribe(&this.state, |this, _, event: &TreeEvent, cx| {
                let (TreeEvent::Expanded(id) | TreeEvent::Collapsed(id)) = event;
                if let Some(EntryAction::Directory(path)) = this.actions.get(id).cloned() {
                    this.owner.as_ref().unwrap().update(cx, |owner, cx| {
                        owner.toggle_directory(this.workspace, path, cx);
                    });
                }
            })
            .detach();
            this.sync(cx);
        } else {
            let items = sample_items(data.files(workspace, cx), "");
            this.state
                .update(cx, |state, cx| state.set_items(items, cx));
        }
        this
    }
    fn browse_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.browsing_root = Some(root.clone());
        if let Some(owner) = &self.owner {
            owner.update(cx, |owner, cx| {
                let tree = owner.files.entry(self.workspace).or_default();
                tree.set_root(root.clone());
                owner.toggle_directory(self.workspace, root, cx);
            });
        }
        cx.notify();
    }
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(owner) = self.owner.clone() else {
            return;
        };
        if let Some(root) = &self.browsing_root
            && owner
                .read(cx)
                .files
                .get(&self.workspace)
                .is_none_or(|tree| tree.root != *root)
        {
            self.browse_root(root.clone(), cx);
        }
        self.actions.clear();
        let items = owner
            .read(cx)
            .files
            .get(&self.workspace)
            .map(|files| local_items(files, &mut self.actions))
            .unwrap_or_default();
        self.state.update(cx, |state, cx| {
            let selected = state.selected_item().map(|item| item.id.clone());
            state.set_items(items, cx);
            let ix = selected.and_then(|id| state.index_of(&id));
            state.set_selected_index(ix, cx);
        });
        cx.notify();
    }
    fn activate(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        let Some(owner) = &self.owner else { return };
        match self.actions.get(id) {
            Some(EntryAction::Open(path)) => owner.update(cx, |owner, cx| {
                owner.open_file(self.workspace, path.clone(), cx);
            }),
            Some(EntryAction::More(path)) => owner.update(cx, |owner, cx| {
                owner.more_files(self.workspace, path.clone(), cx);
            }),
            _ => {}
        }
    }
    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        // TreeState handles Confirm for folders, but has no leaf activation event.
        let selected = self
            .state
            .read(cx)
            .selected_entry()
            .filter(|entry| !entry.is_folder() && !entry.is_disabled())
            .map(|entry| entry.item().id.clone());
        if let Some(id) = selected {
            self.activate(&id, cx);
        }
        cx.propagate();
    }
}

fn sample_items(nodes: Vec<FileNode>, parent: &str) -> Vec<TreeItem> {
    nodes
        .into_iter()
        .map(|node| {
            let path = format!("{parent}/{}", node.name);
            let children = sample_items(node.children, &path);
            TreeItem::new(path.clone(), node.name)
                .children(children)
                .expanded(path == "/src")
        })
        .collect()
}

fn local_items(
    files: &FileTree,
    actions: &mut HashMap<SharedString, EntryAction>,
) -> Vec<TreeItem> {
    fn level(
        rows: &mut std::iter::Peekable<std::vec::IntoIter<Row>>,
        depth: usize,
        parent: &str,
        actions: &mut HashMap<SharedString, EntryAction>,
    ) -> Vec<TreeItem> {
        let mut items = Vec::new();
        while rows.peek().is_some_and(|row| row_depth(row) == depth) {
            let row = rows.next().unwrap();
            let (id, label, action, expanded, directory) = match row {
                Row::Entry {
                    entry, expanded, ..
                } => (
                    format!("entry:{:?}", entry.path),
                    entry.name,
                    Some(if entry.is_dir {
                        EntryAction::Directory(entry.path)
                    } else {
                        EntryAction::Open(entry.path)
                    }),
                    expanded,
                    entry.is_dir,
                ),
                Row::More { path, .. } => (
                    format!("more:{path:?}"),
                    "Show more…".into(),
                    Some(EntryAction::More(path)),
                    false,
                    false,
                ),
                Row::Loading { .. } => (
                    format!("loading:{parent}"),
                    "Loading files…".into(),
                    None,
                    false,
                    false,
                ),
                Row::Empty { .. } => (
                    format!("empty:{parent}"),
                    "Empty directory".into(),
                    None,
                    false,
                    false,
                ),
                Row::Error { message, .. } => {
                    (format!("error:{parent}"), message, None, false, false)
                }
            };
            let mut children = if directory && expanded {
                level(rows, depth + 1, &id, actions)
            } else {
                vec![]
            };
            // A folder is defined by nonempty children. Keep collapsed folders
            // expandable without reading them; expanded empty/error folders have
            // their own disabled status child from the owner's rows.
            if directory && children.is_empty() {
                children
                    .push(TreeItem::new(format!("pending:{id}"), "Loading files…").disabled(true));
            }
            let disabled = action.is_none();
            if let Some(action) = action {
                actions.insert(id.clone().into(), action);
            }
            items.push(
                TreeItem::new(id, label)
                    .children(children)
                    .expanded(expanded)
                    .disabled(disabled),
            );
        }
        items
    }
    level(&mut files.rows().into_iter().peekable(), 0, "root", actions)
}
fn row_depth(row: &Row) -> usize {
    match row {
        Row::Entry { depth, .. }
        | Row::Loading { depth }
        | Row::Error { depth, .. }
        | Row::More { depth, .. }
        | Row::Empty { depth } => *depth,
    }
}
impl Render for FilesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let p = Palette::current(cx);
        let badges: HashMap<SharedString, (String, bool)> = self
            .owner
            .as_ref()
            .map(|owner| {
                let owner = owner.read(cx);
                let root = owner
                    .model
                    .workspaces
                    .iter()
                    .find(|w| w.id == self.workspace)
                    .map(|w| &w.root);
                owner
                    .model
                    .status_view(self.workspace)
                    .files
                    .into_iter()
                    .filter_map(|file| {
                        Some((
                            format!("entry:{:?}", root?.join(&file.path)).into(),
                            (file.status, file.staged),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut body = div().size_full().flex().flex_col();
        if let Some(roots) = &self.roots {
            if self.browsing_root.is_none() {
                for (ix, root) in roots.iter().enumerate() {
                    let root = root.clone();
                    let label = format!(
                        "📁 {}/",
                        root.file_name()
                            .unwrap_or(root.as_os_str())
                            .to_string_lossy()
                    );
                    body = body.child(
                        Button::new(("files-root", ix))
                            .ghost()
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browse_root(root.clone(), cx)
                            })),
                    );
                }
                return body;
            }
            body = body.child(
                Button::new("files-all-folders")
                    .ghost()
                    .label("← All folders")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.browsing_root = None;
                        cx.notify();
                    })),
            );
        }
        if let Some(root) = &self.browsing_root {
            body = body.child(
                div()
                    .px_2()
                    .text_color(rgb(p.blue))
                    .child(root.display().to_string()),
            );
        }
        body.capture_action(cx.listener(Self::confirm)).child(
            tree(&self.state, move |ix, entry, _, _, _| {
                let id = entry.item().id.clone();
                let view = view.clone();
                let badge = badges.get(&id);
                ListItem::new(ix)
                    .h(px(28.))
                    .w_full()
                    .justify_start()
                    .pl(px(8. + entry.depth() as f32 * 16.))
                    .text_color(rgb(if id.starts_with("error:") {
                        p.red
                    } else if entry.is_disabled() {
                        p.muted
                    } else {
                        p.text
                    }))
                    // One text child: ListItem stacks multiple children vertically.
                    .child(format!(
                        "{} {}{}{}",
                        if entry.is_folder() {
                            if entry.is_expanded() { "⌄" } else { "›" }
                        } else {
                            "·"
                        },
                        entry.item().label,
                        if entry.is_folder() { "/" } else { "" },
                        badge
                            .map(|(status, staged)| format!(
                                "  {status}{}",
                                if *staged { " ●" } else { "" }
                            ))
                            .unwrap_or_default()
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.activate(&id, cx));
                    })
            })
            .size_full(),
        )
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
