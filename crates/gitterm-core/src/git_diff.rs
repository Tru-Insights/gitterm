//! Shared local diff collection. The legacy line preview keeps its original semantics.

use git2::{Repository, Status, StatusOptions};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Header,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiffLine {
    pub content: String,
    pub kind: DiffLineKind,
    /// 1-based; `None` when the line doesn't exist on that side.
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

/// Collect the diff for one repo-relative file, staged (HEAD→index) or
/// unstaged (index→worktree). Untracked files render as an added-lines
/// preview capped at `max_untracked_preview_lines`.
pub fn collect_file_diff(
    repo_root: &Path,
    file_path: &str,
    is_staged: bool,
    max_untracked_preview_lines: usize,
) -> Vec<FileDiffLine> {
    let mut lines = Vec::new();
    // A chat tab keeps the folder it started in, which can be below the
    // repo root (TRU-237); `file_path` is relative to the repo root.
    let Ok(repo) = Repository::open(repo_root).or_else(|_| Repository::discover(repo_root)) else {
        return lines;
    };
    let repo_root = repo.workdir().unwrap_or(repo_root).to_path_buf();

    // Use no_refresh + pathspec so status doesn't rewrite .git/index.lock
    // (would contend with concurrent git commands) and only inspects this file.
    let mut untracked_opts = StatusOptions::new();
    untracked_opts
        .no_refresh(true)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .pathspec(file_path);
    let is_untracked = repo
        .statuses(Some(&mut untracked_opts))
        .ok()
        .map(|statuses| {
            statuses
                .iter()
                .any(|e| e.path() == Some(file_path) && e.status().contains(Status::WT_NEW))
        })
        .unwrap_or(false);

    if is_untracked {
        let full_path = repo_root.join(file_path);
        if let Ok(content) = std::fs::read_to_string(&full_path) {
            let total_lines = content.lines().count();
            lines.push(FileDiffLine {
                content: format!("@@ -0,0 +1,{} @@ (new file)", total_lines),
                kind: DiffLineKind::Header,
                old_line: None,
                new_line: None,
            });
            for (i, line) in content
                .lines()
                .take(max_untracked_preview_lines)
                .enumerate()
            {
                lines.push(FileDiffLine {
                    content: line.to_string(),
                    kind: DiffLineKind::Addition,
                    old_line: None,
                    new_line: Some((i + 1) as u32),
                });
            }
            if total_lines > max_untracked_preview_lines {
                lines.push(FileDiffLine {
                    content: format!(
                        "... truncated to first {} lines ({} total)",
                        max_untracked_preview_lines, total_lines
                    ),
                    kind: DiffLineKind::Header,
                    old_line: None,
                    new_line: None,
                });
            }
        }
        return lines;
    }

    let mut diff_opts = git2::DiffOptions::new();
    diff_opts.pathspec(file_path);
    let diff = if is_staged {
        let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts))
    } else {
        repo.diff_index_to_workdir(None, Some(&mut diff_opts))
    };

    if let Ok(diff) = diff {
        let _ = diff.print(git2::DiffFormat::Patch, |_delta, hunk, line| {
            let content = String::from_utf8_lossy(line.content())
                .trim_end()
                .to_string();
            match line.origin() {
                'H' => {
                    if let Some(h) = hunk {
                        lines.push(FileDiffLine {
                            content: format!(
                                "@@ -{},{} +{},{} @@",
                                h.old_start(),
                                h.old_lines(),
                                h.new_start(),
                                h.new_lines()
                            ),
                            kind: DiffLineKind::Header,
                            old_line: None,
                            new_line: None,
                        });
                    }
                }
                '+' => lines.push(FileDiffLine {
                    content,
                    kind: DiffLineKind::Addition,
                    old_line: None,
                    new_line: line.new_lineno(),
                }),
                '-' => lines.push(FileDiffLine {
                    content,
                    kind: DiffLineKind::Deletion,
                    old_line: line.old_lineno(),
                    new_line: None,
                }),
                ' ' => lines.push(FileDiffLine {
                    content,
                    kind: DiffLineKind::Context,
                    old_line: line.old_lineno(),
                    new_line: line.new_lineno(),
                }),
                _ => {}
            }
            true
        });
    }

    lines
}

/// Initial review display limits; collection retains the full patch for expansion
/// and for viewed-mark invalidation, including content below the display limit.
pub const LARGE_LINES: usize = 2_000;
pub const LARGE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub root: std::path::PathBuf,
    pub file_path: String,
    pub staged: bool,
}

#[derive(Debug, Clone)]
pub struct FilePatch {
    pub path: String,
    pub old_path: Option<String>,
    pub patch: String,
    pub binary: bool,
    pub large: bool,
}

/// Full patches for a status entry. Untracked directory entries expand to files.
/// This complements the unchanged Iced line-preview API above: patch headers,
/// whitespace and no-newline markers are retained for native diff parsers.
pub fn collect_patches(request: &DiffRequest) -> Result<Vec<FilePatch>, String> {
    collect_patches_inner(request).map_err(|error| {
        format!(
            "git diff {} in {}: {error}",
            request.file_path,
            request.root.display()
        )
    })
}

fn collect_patches_inner(request: &DiffRequest) -> Result<Vec<FilePatch>, git2::Error> {
    let repo = Repository::open(&request.root).or_else(|_| Repository::discover(&request.root))?;
    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let mut diff = if request.staged {
        let head = match repo.head() {
            Ok(head) => Some(head.peel_to_tree()?),
            Err(error) if error.code() == git2::ErrorCode::UnbornBranch => None,
            Err(error) => return Err(error),
        };
        repo.diff_tree_to_index(head.as_ref(), None, Some(&mut options))?
    } else {
        repo.diff_index_to_workdir(None, Some(&mut options))?
    };
    // Rename detection needs both endpoints before selecting a per-file delta.
    diff.find_similar(Some(git2::DiffFindOptions::new().renames(true)))?;
    let requested = Path::new(&request.file_path);
    let mut patches = Vec::new();
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or_else(|| git2::Error::from_str("delta has no path"))?;
        if path != requested && !(request.file_path.ends_with('/') && path.starts_with(requested)) {
            continue;
        }
        let mut text = Vec::new();
        // Patch::from_diff returns None for binary files; Diff::print retains
        // their binary marker as well as mode-only and rename-only headers.
        diff.print(git2::DiffFormat::Patch, |printed, _, line| {
            if printed
                .new_file()
                .path()
                .or_else(|| printed.old_file().path())
                == Some(path)
            {
                if matches!(line.origin(), '+' | '-' | ' ') {
                    text.push(line.origin() as u8);
                }
                text.extend_from_slice(line.content());
            }
            true
        })?;
        let patch = String::from_utf8_lossy(&text).into_owned();
        let binary = delta.flags().contains(git2::DiffFlags::BINARY)
            || patch
                .lines()
                .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch");
        let old_path = (delta.status() == git2::Delta::Renamed)
            .then(|| {
                delta
                    .old_file()
                    .path()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .flatten();
        patches.push(FilePatch {
            path: path.to_string_lossy().into_owned(),
            old_path,
            large: patch.len() > LARGE_BYTES
                || patch.lines().take(LARGE_LINES + 1).count() > LARGE_LINES,
            patch,
            binary,
        });
    }
    if patches.is_empty() {
        return Err(git2::Error::from_str(
            "status entry has no matching diff (repository may have changed)",
        ));
    }
    Ok(patches)
}
