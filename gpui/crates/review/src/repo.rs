//! One-shot local review source. Hosts supply the process executor; no polling.
use crate::model::{Group, Patches};
use gitterm_core::{
    git_diff::DiffRequest,
    runtime::{DiffOutcome, DiffRuntime, StatusOutcome, StatusRuntime},
    status::{GitFileStatus, GitStatusSnapshot, StatusCommand},
};
use std::path::PathBuf;
use tokio::runtime::Handle;

pub async fn load(
    handle: &Handle,
    owner: usize,
    root: PathBuf,
) -> Result<(GitStatusSnapshot, Patches), String> {
    let (status_jobs, mut status_results) = StatusRuntime::new();
    let (diff_jobs, mut diff_results) = DiffRuntime::new();
    let result = async {
        let id = status_jobs
            .execute(
                handle,
                owner,
                StatusCommand::CollectStatus { root: root.clone() },
            )
            .ok_or("status runtime closed")?;
        let Some(StatusOutcome::Completed { job, mut snapshot }) = status_results.recv().await
        else {
            return Err("repository status cancelled".into());
        };
        if job != id {
            return Err("unexpected status job".into());
        }
        if let Some(error) = &snapshot.error {
            return Err(error.clone());
        }
        if !snapshot.is_git_repo {
            return Err(format!("not a Git repository: {}", root.display()));
        }
        let mut patches = Patches::new();
        for (group, statuses) in [
            (Group::Staged, &mut snapshot.staged),
            (Group::Unstaged, &mut snapshot.unstaged),
            (Group::Untracked, &mut snapshot.untracked),
        ] {
            let mut files = Vec::new();
            for status in statuses.iter() {
                let request = DiffRequest {
                    root: snapshot.repo_path.clone(),
                    file_path: status.path.clone(),
                    staged: status.is_staged,
                };
                let id = diff_jobs
                    .collect_diff(handle, owner, request.clone())
                    .ok_or("diff runtime closed")?;
                let Some(DiffOutcome::Completed {
                    job,
                    snapshot: diff,
                }) = diff_results.recv().await
                else {
                    return Err(format!("diff cancelled for {}", status.path));
                };
                if job != id || diff.request != request {
                    return Err("unexpected diff job".into());
                }
                for patch in diff.patches? {
                    let key = (group, patch.path.clone());
                    if patches.insert(key, patch.patch.into()).is_some() {
                        return Err(format!(
                            "duplicate {} patch for {}",
                            group.label(),
                            patch.path
                        ));
                    }
                    files.push(GitFileStatus {
                        path: patch.path,
                        ..status.clone()
                    });
                }
            }
            *statuses = files;
        }
        Ok((snapshot, patches))
    }
    .await;
    // Account for all blocking workers even on source failure.
    status_jobs.teardown().await;
    diff_jobs.teardown().await;
    result
}

#[cfg(test)]
pub(crate) mod tests;
