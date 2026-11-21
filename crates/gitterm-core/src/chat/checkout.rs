//! The chat checkout chip's choices and label (TRU-143), shared by both
//! frontends. Moved unchanged from the Iced crate's `tab::agent`.
use serde::Serialize;
use std::path::{Path, PathBuf};

/// One task's worktree as the chat's checkout chip sees it (TRU-143).
#[derive(Debug, Clone, Copy)]
pub struct TaskWorktreeInfo<'a> {
    pub title: &'a str,
    /// The task's local workspace directory (`None` for remote workspaces).
    pub workspace_dir: Option<&'a Path>,
    pub worktree: Option<&'a Path>,
    /// The worktree is prepared (`TaskWorktreeState::Ready`).
    pub ready: bool,
    pub archived: bool,
}

/// A directory a chat can start in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckoutChoice {
    pub label: String,
    pub detail: String,
    pub path: PathBuf,
}

/// The label of the workspace's own checkout in the chip.
pub const CURRENT_CHECKOUT_LABEL: &str = "Current checkout";

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What the checkout chip offers before a chat's first message: the
/// workspace's current checkout, then every prepared, unarchived task
/// worktree of that workspace, each once. `same_path` compares directories
/// (the app canonicalises; tests compare literally).
pub fn checkout_choices<'a>(
    workspace_dir: &Path,
    tasks: impl IntoIterator<Item = TaskWorktreeInfo<'a>>,
    same_path: impl Fn(&Path, &Path) -> bool,
) -> Vec<CheckoutChoice> {
    let mut choices = vec![CheckoutChoice {
        label: CURRENT_CHECKOUT_LABEL.to_string(),
        detail: workspace_dir.display().to_string(),
        path: workspace_dir.to_path_buf(),
    }];
    for task in tasks {
        let (Some(task_workspace), Some(worktree)) = (task.workspace_dir, task.worktree) else {
            continue;
        };
        if !task.ready || task.archived || !same_path(task_workspace, workspace_dir) {
            continue;
        }
        if choices.iter().any(|c| same_path(&c.path, worktree)) {
            continue;
        }
        choices.push(CheckoutChoice {
            label: dir_name(worktree),
            detail: task.title.to_string(),
            path: worktree.to_path_buf(),
        });
    }
    choices
}

/// The chip's label for a chat running in `tab_dir`: the task worktree's
/// name for a task-linked tab, "Current checkout" for the workspace's own
/// directory, else the directory's name (a task worktree picked in the chip).
pub fn checkout_label(
    tab_dir: &Path,
    task_worktree: Option<&Path>,
    workspace_dir: &Path,
    same_path: impl Fn(&Path, &Path) -> bool,
) -> String {
    if let Some(worktree) = task_worktree {
        return dir_name(worktree);
    }
    if same_path(tab_dir, workspace_dir) {
        return CURRENT_CHECKOUT_LABEL.to_string();
    }
    dir_name(tab_dir)
}

/// Iced's directory comparison: canonical paths when both resolve, else
/// literal. Use this as `same_path` outside tests.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task<'a>(
        title: &'a str,
        workspace: &'a str,
        worktree: &'a str,
        ready: bool,
        archived: bool,
    ) -> TaskWorktreeInfo<'a> {
        TaskWorktreeInfo {
            title,
            workspace_dir: Some(Path::new(workspace)),
            worktree: Some(Path::new(worktree)),
            ready,
            archived,
        }
    }

    #[test]
    fn checkout_choices_are_the_checkout_then_this_workspaces_ready_worktrees() {
        let same = |a: &Path, b: &Path| a == b;
        let ws = Path::new("/repo");
        let tasks = [
            task("Add chips", "/repo", "/wt/tru-143-chips", true, false),
            task("Other repo", "/other", "/wt/other", true, false),
            task("Not prepared", "/repo", "/wt/preparing", false, false),
            task("Done", "/repo", "/wt/archived", true, true),
            task("Same tree again", "/repo", "/wt/tru-143-chips", true, false),
            TaskWorktreeInfo {
                title: "Remote",
                workspace_dir: None,
                worktree: Some(Path::new("/wt/remote")),
                ready: true,
                archived: false,
            },
            task("Fix review", "/repo", "/wt/tru-142-review", true, false),
        ];
        let choices = checkout_choices(ws, tasks, same);
        let summary: Vec<(&str, &str, &Path)> = choices
            .iter()
            .map(|c| (c.label.as_str(), c.detail.as_str(), c.path.as_path()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Current checkout", "/repo", Path::new("/repo")),
                ("tru-143-chips", "Add chips", Path::new("/wt/tru-143-chips")),
                (
                    "tru-142-review",
                    "Fix review",
                    Path::new("/wt/tru-142-review")
                ),
            ]
        );
        // No tasks: the checkout alone.
        assert_eq!(checkout_choices(ws, [], same).len(), 1);
    }

    #[test]
    fn checkout_label_names_the_task_worktree_or_the_checkout() {
        let same = |a: &Path, b: &Path| a == b;
        let ws = Path::new("/repo");
        assert_eq!(checkout_label(ws, None, ws, same), "Current checkout");
        assert_eq!(
            checkout_label(Path::new("/wt/tru-143"), None, ws, same),
            "tru-143"
        );
        assert_eq!(
            checkout_label(ws, Some(Path::new("/wt/task-tree")), ws, same),
            "task-tree"
        );
    }
}
