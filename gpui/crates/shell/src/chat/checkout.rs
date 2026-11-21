//! Iced's checkout menu entries (`checkout_choices_for`): the workspace's own
//! checkout, then each prepared, unarchived task worktree of that workspace.
//! Read-only: it loads the task file and never writes it.
use gitterm_core::tasks::{
    TaskLifecycle, TaskRecord, TaskStore, TaskWorktreeState, WorkspaceLocationIdentity,
};
use gitterm_gpui_chat::composer::{CheckoutChoice, CheckoutSource};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

const CURRENT_CHECKOUT_LABEL: &str = "Current checkout";

fn dir_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

pub(super) fn choices(workspace_dir: &Path, tasks: &[TaskRecord]) -> Vec<CheckoutChoice> {
    let mut choices = vec![CheckoutChoice {
        label: CURRENT_CHECKOUT_LABEL.into(),
        detail: workspace_dir.display().to_string(),
        path: workspace_dir.display().to_string(),
    }];
    for task in tasks {
        let (WorkspaceLocationIdentity::Local { directory }, Some(worktree)) =
            (&task.workspace.location, task.worktree.path.as_deref())
        else {
            continue;
        };
        if task.worktree.state != TaskWorktreeState::Ready
            || task.lifecycle == TaskLifecycle::Archived
            || directory != workspace_dir
        {
            continue;
        }
        let path = worktree.display().to_string();
        if choices.iter().any(|choice| choice.path == path) {
            continue;
        }
        choices.push(CheckoutChoice {
            label: dir_name(worktree),
            detail: task.title.clone(),
            path,
        });
    }
    choices
}

/// `config` is the Iced config root the owner resolved; without one (samples,
/// tests) only the workspace's own checkout is offered.
pub(super) fn source(workspace_dir: PathBuf, config: Option<PathBuf>) -> CheckoutSource {
    Rc::new(move || {
        // shortcut: paths compare literally; Iced canonicalises. Upgrade if a
        // symlinked workspace directory hides its task worktrees.
        let tasks = config.as_deref().map_or_else(Vec::new, |root| {
            let path = TaskStore::path_for_config_root(root);
            match TaskStore::load(&path) {
                Ok(store) => store.tasks().to_vec(),
                Err(error) => {
                    eprintln!("Checkout menu: could not read {}: {error}", path.display());
                    Vec::new()
                }
            }
        });
        choices(&workspace_dir, &tasks)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(title: &str, dir: &str, tree: &str, ready: bool, archived: bool) -> TaskRecord {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../tests/fixtures/stage45/tasks/old.tasks.json"
        ))
        .unwrap();
        let mut record: TaskRecord = serde_json::from_value(fixture["tasks"][0].clone()).unwrap();
        record.title = title.into();
        record.workspace.location = WorkspaceLocationIdentity::Local {
            directory: dir.into(),
        };
        record.worktree.path = Some(tree.into());
        record.worktree.state = if ready {
            TaskWorktreeState::Ready
        } else {
            TaskWorktreeState::Preparing
        };
        if archived {
            record.lifecycle = TaskLifecycle::Archived;
        }
        record
    }

    #[test]
    fn menu_is_the_checkout_then_this_workspaces_ready_unarchived_worktrees() {
        let tasks = [
            task("Add chips", "/repo", "/wt/tru-143-chips", true, false),
            task("Other repo", "/other", "/wt/other", true, false),
            task("Not prepared", "/repo", "/wt/preparing", false, false),
            task("Done", "/repo", "/wt/archived", true, true),
            task("Same tree again", "/repo", "/wt/tru-143-chips", true, false),
        ];
        let summary: Vec<_> = choices(Path::new("/repo"), &tasks)
            .into_iter()
            .map(|c| (c.label, c.detail, c.path))
            .collect();
        assert_eq!(
            summary,
            [
                ("Current checkout", "/repo", "/repo"),
                ("tru-143-chips", "Add chips", "/wt/tru-143-chips"),
            ]
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        );
    }

    #[test]
    fn source_without_a_config_root_offers_only_the_workspace_checkout() {
        let menu = source("/repo".into(), None)();
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].label, "Current checkout");
    }
}
