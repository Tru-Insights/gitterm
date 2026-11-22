//! One-shot local review source. Hosts supply the process executor; no polling.
use crate::model::{Group, Patches};
use gitterm_core::{
    git_diff::DiffRequest,
    runtime::{DiffOutcome, DiffRuntime, StatusOutcome, StatusRuntime},
    status::{GitFileStatus, GitStatusSnapshot, StatusCommand},
};
use std::path::PathBuf;
use tokio::runtime::Handle;

use gitterm_core::{
    git_facts::{
        BaseChoice, BaseSource, GIT_TIMEOUT, base_candidates, choose_nearest, git_text,
        measure_bases, run_git_lines,
    },
    review::{
        model::ReviewTarget,
        state::{CollectStamp, Intent},
    },
    work::summary::StatusCounts,
};

/// Read-only branch choices; default selection uses the shared nearest-base policy.
pub async fn aggregate_target(root: PathBuf) -> Result<(ReviewTarget, Vec<BaseChoice>), String> {
    let mut counts = StatusCounts::default();
    run_git_lines(
        &root,
        &[
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v2",
            "--branch",
        ],
        GIT_TIMEOUT,
        |line| counts.observe(line, 1),
    )
    .await?;
    let mut candidates = base_candidates(None, counts.head.as_deref(), counts.upstream.as_deref());
    let measured = measure_bases(&root, &mut candidates, &mut 0).await?;
    let base = choose_nearest(&candidates, &measured).map(|(base, _)| base);
    let refs = git_text(
        &root,
        &[
            "--no-optional-locks",
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/",
            "refs/remotes/",
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let mut choices = Vec::new();
    for reference in refs.lines() {
        let (reference, source) = if let Some(local) = reference.strip_prefix("refs/heads/") {
            (local, BaseSource::Local)
        } else if let Some(remote) = reference.strip_prefix("refs/remotes/") {
            if remote.ends_with("/HEAD") {
                continue;
            }
            (remote, BaseSource::Remote)
        } else {
            continue;
        };
        choices.push(BaseChoice {
            reference: reference.into(),
            source,
            considered: 1,
        });
    }
    let target = ReviewTarget {
        repo: root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        checkout: root,
        branch: counts.head.unwrap_or_else(|| "(detached)".into()),
        owner: "local review".into(),
        base,
    };
    Ok((target, choices))
}

/// Preserve the effect's stamp verbatim. Only State decides whether it still applies.
pub async fn collect_aggregate(stamp: CollectStamp) -> Intent {
    collect_aggregate_with(stamp, None).await
}

/// `roots` is the host's read-only turn snapshot source; without it turn
/// sources fail loudly and every other source is unaffected.
pub async fn collect_aggregate_with(
    stamp: CollectStamp,
    roots: Option<gitterm_core::turn_snapshots::SnapshotRoots>,
) -> Intent {
    let result = match roots {
        Some(roots) => {
            gitterm_core::review::collect::load_review_source(
                stamp.target.clone(),
                stamp.source.clone(),
                &roots,
            )
            .await
        }
        None => {
            gitterm_core::review::collect::load_review_source_with_snapshots(
                stamp.target.clone(),
                stamp.source.clone(),
                async { Err("turn review requires a host-provided snapshot source".into()) },
            )
            .await
        }
    };
    Intent::CollectFinished {
        stamp,
        result: Box::new(result),
    }
}

pub async fn load(
    handle: &Handle,
    owner: usize,
    root: PathBuf,
) -> Result<(GitStatusSnapshot, Patches), String> {
    let (status_jobs, mut status_results) = StatusRuntime::new();
    let (diff_jobs, mut diff_results) = DiffRuntime::new();
    let result = async {
        let id = status_jobs
            .execute_with(
                handle,
                owner,
                StatusCommand::CollectStatus { root: root.clone() },
                gitterm_core::git_status::StatusReadOptions { fsmonitor: false },
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
