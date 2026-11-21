//! Iced's checkout menu entries (`checkout_choices_for`): the workspace's own
//! checkout, then each prepared, unarchived task worktree of that workspace.
//! Read-only: it loads the preview task host's store (Iced's file only when
//! the host is down) and never writes it.
use gitterm_core::chat::checkout;
use gitterm_core::tasks::{
    TaskLifecycle, TaskRecord, TaskWorktreeState, WorkspaceLocationIdentity,
};
use gitterm_gpui_chat::composer::{CheckoutChoice, CheckoutSource};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

/// Iced's rules, from core (canonical path comparison, as the Iced app).
pub(super) fn choices(workspace_dir: &Path, tasks: &[TaskRecord]) -> Vec<CheckoutChoice> {
    let tasks = tasks.iter().map(|task| checkout::TaskWorktreeInfo {
        title: &task.title,
        workspace_dir: match &task.workspace.location {
            WorkspaceLocationIdentity::Local { directory } => Some(directory.as_path()),
            WorkspaceLocationIdentity::RemoteAgent { .. } => None,
        },
        worktree: task.worktree.path.as_deref(),
        ready: task.worktree.state == TaskWorktreeState::Ready,
        archived: task.lifecycle == TaskLifecycle::Archived,
    });
    checkout::checkout_choices(workspace_dir, tasks, checkout::paths_equal)
        .into_iter()
        .map(|choice| CheckoutChoice {
            label: choice.label,
            detail: choice.detail,
            path: choice.path.display().to_string(),
        })
        .collect()
}

/// `config` is the Iced config root the owner resolved; without one (samples,
/// tests) only the workspace's own checkout is offered.
pub(super) fn source(workspace_dir: PathBuf, config: Option<PathBuf>) -> CheckoutSource {
    Rc::new(move |cx| {
        let tasks =
            config
                .as_deref()
                .map_or_else(Vec::new, |root| {
                    match crate::data::TaskSource::resolve(root, cx).load() {
                        Ok(store) => store.tasks().to_vec(),
                        Err(error) => {
                            eprintln!("Checkout menu: {error}");
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
    use gitterm_core::tasks::TaskStore;

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

    #[gpui::test]
    fn source_without_a_config_root_offers_only_the_workspace_checkout(
        cx: &mut gpui::TestAppContext,
    ) {
        let menu = cx.update(|cx| source("/repo".into(), None)(cx));
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].label, "Current checkout");
    }

    #[gpui::test]
    fn source_reads_the_host_store_not_iceds_file(cx: &mut gpui::TestAppContext) {
        use gitterm_core::tasks::TaskStoreDocument;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let iced = base.join("iced");
        std::fs::create_dir(&iced).unwrap();
        let write = |tasks: Vec<TaskRecord>| {
            let document = TaskStoreDocument {
                tasks,
                ..Default::default()
            };
            std::fs::write(
                TaskStore::path_for_config_root(&iced),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
        };
        write(vec![task("Add chips", "/repo", "/wt/chips", true, false)]);
        let owner = crate::work_sidebar::tests::start_preview_host(&base, &iced, cx);
        write(vec![]);
        let menu = cx.update(|cx| source("/repo".into(), Some(iced.clone()))(cx));
        assert_eq!(
            menu.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            ["/repo", "/wt/chips"]
        );
        crate::work_sidebar::tests::stop_preview_host(&owner, cx);
    }
}
