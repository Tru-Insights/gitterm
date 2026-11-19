//! Shared isolated task fixture for host and service tests.

use crate::tasks::TaskRecord;
use std::path::{Path, PathBuf};

/// A ready local task with its worktree at `worktree`, for the tests of
/// this crate that exercise workers.
pub fn test_task(task_id: &str, worktree: &Path) -> TaskRecord {
    use crate::tasks::{
        ExecutorTarget, GitBase, IssueProvider, IssueReference, NewTaskRecord, RepositoryIdentity,
        StoppingBoundary, TaskWorktree, TaskWorktreeState, WorkspaceIdentity,
        WorkspaceLocationIdentity,
    };
    let mut task = TaskRecord::new_draft(
        NewTaskRecord {
            task_id: task_id.to_string(),
            title: "Fix the Excalidraw export".to_string(),
            objective: "Make the export keep embedded images; stop when tests pass.".to_string(),
            workspace: WorkspaceIdentity {
                name: "GitTerm".to_string(),
                location: WorkspaceLocationIdentity::Local {
                    directory: PathBuf::from("/repo"),
                },
            },
            repository: RepositoryIdentity {
                common_dir: PathBuf::from("/repo/.git"),
                remote_url: None,
            },
            issue: Some(IssueReference {
                provider: IssueProvider::Linear,
                key: "TRU-150".to_string(),
                url: None,
            }),
            base: GitBase {
                reference: "v6".to_string(),
                commit: "0123456789abcdef".to_string(),
            },
            branch: "tracey/tru-150-excalidraw-export".to_string(),
            executor: ExecutorTarget::Local,
            harness: None,
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
        },
        "2026-10-08T10:00:00Z",
    );
    task.worktree = TaskWorktree {
        state: TaskWorktreeState::Ready,
        path: Some(worktree.to_path_buf()),
    };
    task
}
