//! Shared worktree lookup rules.
pub use crate::work::summary::WorktreeInfo;
use std::path::{Path, PathBuf};
/// Parses `git worktree list --porcelain`. Bare entries have no working
/// tree and are skipped.
pub fn parse_worktree_porcelain(text: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut current: Option<(WorktreeInfo, bool)> = None;
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some((entry, bare)) = current.take() {
                if !bare {
                    out.push(entry);
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some((entry, bare)) = current.take() {
                if !bare {
                    out.push(entry);
                }
            }
            current = Some((
                WorktreeInfo {
                    path: PathBuf::from(path),
                    branch: None,
                },
                false,
            ));
            continue;
        }
        let Some((entry, bare)) = current.as_mut() else {
            continue;
        };
        if let Some(reference) = line.strip_prefix("branch ") {
            let short = reference.strip_prefix("refs/heads/").unwrap_or(reference);
            entry.branch = Some(short.to_string());
        } else if line == "bare" {
            *bare = true;
        }
    }
    out
}

/// Lists the worktrees of the repository containing `repo`, with each
/// existing path canonicalized so matches survive symlinked roots.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeInfo>, String> {
    let output = crate::git_status::git_command()
        .args(["--no-optional-locks", "worktree", "list", "--porcelain"])
        .current_dir(repo)
        .output()
        .map_err(|error| {
            format!(
                "failed to run git worktree list in {}: {error}",
                repo.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git worktree list in {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(
        parse_worktree_porcelain(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .map(|entry| WorktreeInfo {
                path: resolve_for_match(&entry.path),
                branch: entry.branch,
            })
            .collect(),
    )
}

/// `path` with its deepest existing ancestor canonicalized and `.` / `..`
/// folded lexically, so a file that does not exist yet (a Write target)
/// still compares against canonical worktree roots.
pub use crate::chat::tool_input::{
    bash_leading_dir, resolve_for_match, tool_call_candidates, CompletedToolCall, ToolInputTracker,
};

/// The innermost worktree containing `path` (worktrees can nest: Claude
/// Code puts its worktrees under `<repo>/.claude/worktrees/`).
pub fn containing_worktree<'a>(
    path: &Path,
    worktrees: &'a [WorktreeInfo],
) -> Option<&'a WorktreeInfo> {
    worktrees
        .iter()
        .filter(|worktree| path.starts_with(&worktree.path))
        .max_by_key(|worktree| worktree.path.components().count())
}
