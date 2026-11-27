//! Task create-record assembly, shared by every frontend (TRU-274).
//!
//! The reducer normalizes a create request into a [`TaskCreate`]; this turns
//! that into the record a frontend prepares a worktree from: the owning
//! repository workspace, the workspace name and the worktree request.

use crate::chat::checkout::paths_equal;
use crate::task_commands::TaskCreate;
use crate::task_mcp::TaskStoppingBoundary;
use crate::task_worktree::PrepareTaskWorktreeRequest;
use crate::tasks::{StoppingBoundary, TaskRecord, TaskStore, WorkspaceLocationIdentity};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Resolve Iced's saved worktree-root setting against the admitted V6 root.
/// Missing settings use Iced's default; malformed settings are never guessed.
pub fn saved_task_worktree_root(config_root: &Path) -> Result<PathBuf, String> {
    let path = config_root.join("settings.json");
    let settings: crate::settings::DesktopSettings = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("read task worktree root from {}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(error) => {
            return Err(format!(
                "read task worktree root from {}: {error}",
                path.display()
            ))
        }
    };
    Ok(settings
        .task_worktree_root
        .0
        .unwrap_or_else(|| config_root.join("worktrees")))
}

/// The task MCP's stopping boundary as the store's.
pub fn stopping_boundary_from_control(boundary: Option<TaskStoppingBoundary>) -> StoppingBoundary {
    match boundary.unwrap_or(TaskStoppingBoundary::ImplementUntilTestsPass) {
        TaskStoppingBoundary::PlanOnly => StoppingBoundary::PlanOnly,
        TaskStoppingBoundary::ImplementUntilTestsPass => StoppingBoundary::ImplementUntilTestsPass,
        TaskStoppingBoundary::PrepareDraftPr => StoppingBoundary::PrepareDraftPr,
    }
}

/// Managed task worktrees are execution directories inside their owning
/// repository workspace, never peer workspaces. Follow any worktree ancestry
/// back to that logical workspace.
pub fn canonical_task_workspace_directory(store: Option<&TaskStore>, directory: &Path) -> PathBuf {
    let Some(store) = store else {
        return directory.to_path_buf();
    };
    let mut canonical = directory.to_path_buf();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(canonical.clone()) {
            break;
        }
        let Some(owner) = store.tasks().iter().find(|task| {
            task.worktree
                .path
                .as_deref()
                .is_some_and(|worktree| canonical.starts_with(worktree))
        }) else {
            break;
        };
        let WorkspaceLocationIdentity::Local { directory } = &owner.workspace.location else {
            break;
        };
        if paths_equal(&canonical, directory) {
            break;
        }
        canonical = directory.clone();
    }
    canonical
}

/// What a create prepares: everything but the pending launch's frontend state.
#[derive(Debug, Clone)]
pub struct AssembledTaskCreate {
    pub task_id: String,
    pub workspace_name: String,
    /// The owning repository workspace (`request.repository_path`).
    pub workspace_dir: PathBuf,
    pub title: String,
    pub objective: String,
    pub issue_key: Option<String>,
    pub stopping_boundary: StoppingBoundary,
    pub request: PrepareTaskWorktreeRequest,
}

/// Assemble the create record from the reducer's normalized `input`.
/// `worktree_root` is asked for the owning workspace, so a frontend can
/// derive the root from it or fail the create.
pub fn assemble_task_create(
    store: Option<&TaskStore>,
    input: TaskCreate,
    worktree_root: impl FnOnce(&Path) -> Result<PathBuf, String>,
) -> Result<AssembledTaskCreate, String> {
    let Some(base_reference) = input.base_reference else {
        return Err("task_create: the reducer left no base reference".into());
    };
    let repository_path = canonical_task_workspace_directory(store, &input.repository_path);
    let came_from_managed_worktree = !paths_equal(&repository_path, &input.repository_path);
    let workspace_name = (!came_from_managed_worktree)
        .then_some(input.workspace_name)
        .flatten()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            repository_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Task workspace".to_string());
    let worktree_root = worktree_root(&repository_path)?;
    let task_id = TaskRecord::fresh_id();
    Ok(AssembledTaskCreate {
        request: PrepareTaskWorktreeRequest {
            repository_path: repository_path.clone(),
            worktree_root,
            task_id: task_id.clone(),
            title: input.title.clone(),
            issue_key: input.issue_key.clone(),
            base_reference,
        },
        task_id,
        workspace_name,
        workspace_dir: repository_path,
        title: input.title,
        objective: input.objective,
        issue_key: input.issue_key,
        stopping_boundary: input.stopping_boundary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_commands::TaskCreateOrigin;

    #[test]
    fn saved_worktree_root_uses_iced_default_and_explicit_setting() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(
            saved_task_worktree_root(root).unwrap(),
            root.join("worktrees")
        );
        let custom = root.join("custom");
        std::fs::write(
            root.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({"task_worktree_root": custom})).unwrap(),
        )
        .unwrap();
        assert_eq!(saved_task_worktree_root(root).unwrap(), custom);
        std::fs::write(root.join("settings.json"), b"{").unwrap();
        assert!(saved_task_worktree_root(root)
            .unwrap_err()
            .contains("settings.json"));
    }

    fn create(workspace_name: Option<&str>, base: Option<&str>) -> TaskCreate {
        TaskCreate {
            title: "Do it".into(),
            objective: "Objective".into(),
            repository_path: PathBuf::from("/nonexistent/repo-dir"),
            workspace_name: workspace_name.map(str::to_string),
            issue_key: Some("TRU-1".into()),
            base_reference: base.map(str::to_string),
            stopping_boundary: StoppingBoundary::PlanOnly,
            origin: TaskCreateOrigin::Control,
        }
    }

    #[test]
    fn boundary_maps_each_variant_and_defaults() {
        use TaskStoppingBoundary as B;
        assert_eq!(
            stopping_boundary_from_control(Some(B::PlanOnly)),
            StoppingBoundary::PlanOnly
        );
        assert_eq!(
            stopping_boundary_from_control(Some(B::ImplementUntilTestsPass)),
            StoppingBoundary::ImplementUntilTestsPass
        );
        assert_eq!(
            stopping_boundary_from_control(Some(B::PrepareDraftPr)),
            StoppingBoundary::PrepareDraftPr
        );
        assert_eq!(
            stopping_boundary_from_control(None),
            StoppingBoundary::ImplementUntilTestsPass
        );
    }

    #[test]
    fn assembles_the_create_record() {
        let record = assemble_task_create(None, create(Some("  Named  "), Some("main")), |repo| {
            Ok(repo.join("wt"))
        })
        .unwrap();
        assert_eq!(record.workspace_name, "Named");
        assert_eq!(record.workspace_dir, PathBuf::from("/nonexistent/repo-dir"));
        assert_eq!(record.stopping_boundary, StoppingBoundary::PlanOnly);
        assert_eq!(record.request.base_reference, "main");
        assert_eq!(record.request.task_id, record.task_id);
        assert_eq!(record.request.title, "Do it");
        assert_eq!(record.request.issue_key.as_deref(), Some("TRU-1"));
        assert_eq!(
            record.request.worktree_root,
            PathBuf::from("/nonexistent/repo-dir/wt")
        );
    }

    #[test]
    fn blank_name_falls_back_to_directory_and_missing_base_is_an_error() {
        let record =
            assemble_task_create(
                None,
                create(Some(" "), Some("main")),
                |_| Ok(PathBuf::new()),
            )
            .unwrap();
        assert_eq!(record.workspace_name, "repo-dir");
        let missing = assemble_task_create(None, create(None, None), |_| Ok(PathBuf::new()));
        assert!(missing.is_err());
    }
}
