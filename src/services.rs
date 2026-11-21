use crate::{
    add_word_diffs_to_lines, build_syntax_highlight_lines, DiffLine, DiffLineType, DiffSnapshot,
    FileEntry, FileLoadSnapshot, FileSyntaxSnapshot, FileVersionSignature, GitBranchEntry,
    GitStatusSnapshot, GitWorktreeEntry, GitWorktreesSnapshot, RemoteHostConfig,
    RemoteSessionsSnapshot, TabState,
};
use std::path::PathBuf;
use std::time::Instant;

macro_rules! perf_log {
    ($($arg:tt)*) => {{
        if crate::perf_enabled() {
            eprintln!("[perf] {}", format_args!($($arg)*));
        }
    }};
}

const MAX_UNTRACKED_DIFF_PREVIEW_LINES: usize = 3000;

pub(crate) fn collect_git_status(tab_id: usize, repo_path: PathBuf) -> GitStatusSnapshot {
    let started = Instant::now();

    let snapshot = gitterm_core::runtime::collect_snapshot(tab_id, repo_path.clone());

    let elapsed = started.elapsed();
    perf_log!(
        "git_status tab={} repo={} git={} changed={} took={}ms",
        tab_id,
        repo_path.display(),
        snapshot.is_git_repo,
        snapshot.staged.len() + snapshot.unstaged.len() + snapshot.untracked.len(),
        elapsed.as_millis()
    );

    if elapsed > std::time::Duration::from_millis(200) {
        eprintln!(
            "[FREEZE-DEBUG] Git status took {}ms for {} on thread '{}'",
            elapsed.as_millis(),
            repo_path.display(),
            std::thread::current().name().unwrap_or("unnamed")
        );
    }

    snapshot
}

pub(crate) fn collect_git_worktrees(tab_id: usize, repo_path: PathBuf) -> GitWorktreesSnapshot {
    let started = Instant::now();
    let mut snapshot = GitWorktreesSnapshot {
        tab_id,
        repo_path: repo_path.clone(),
        is_git_repo: false,
        worktrees: Vec::new(),
        branches: Vec::new(),
        error: None,
    };

    let output = match std::process::Command::new("git")
        .args(["--no-optional-locks", "worktree", "list", "--porcelain"])
        .current_dir(&repo_path)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            snapshot.error = if stderr.is_empty() {
                Some("Not a git repository".to_string())
            } else {
                Some(stderr)
            };
            return snapshot;
        }
        Err(e) => {
            snapshot.error = Some(format!("Failed to run git worktree list: {}", e));
            return snapshot;
        }
    };

    snapshot.is_git_repo = true;

    let finalize_entry = |mut entry: GitWorktreeEntry| {
        if entry.path.is_dir() {
            let status = collect_git_status(tab_id, entry.path.clone());
            entry.is_git_repo = status.is_git_repo;
            entry.staged_count = status.staged.len();
            entry.unstaged_count = status.unstaged.len();
            entry.untracked_count = status.untracked.len();
            if entry.branch_name.is_empty() && status.is_git_repo && status.branch_name != "main" {
                entry.branch_name = status.branch_name;
            }
        }
        entry
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut current: Option<GitWorktreeEntry> = None;
    for line in stdout.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(entry) = current.take() {
                snapshot.worktrees.push(finalize_entry(entry));
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                snapshot.worktrees.push(finalize_entry(entry));
            }
            let path = PathBuf::from(path);
            current = Some(GitWorktreeEntry {
                is_current: paths_equal(&path, &repo_path),
                path,
                branch_name: String::new(),
                head: String::new(),
                is_detached: false,
                is_prunable: false,
                is_git_repo: false,
                staged_count: 0,
                unstaged_count: 0,
                untracked_count: 0,
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = head.to_string();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch_name = branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string();
            } else if line == "detached" {
                entry.is_detached = true;
            } else if line.starts_with("prunable") {
                entry.is_prunable = true;
            }
        }
    }

    snapshot.worktrees.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then_with(|| b.total_changes().cmp(&a.total_changes()))
            .then_with(|| a.branch_name.cmp(&b.branch_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let checked_out_branches: std::collections::HashMap<String, PathBuf> = snapshot
        .worktrees
        .iter()
        .filter(|worktree| !worktree.is_detached && !worktree.branch_name.is_empty())
        .map(|worktree| (worktree.branch_name.clone(), worktree.path.clone()))
        .collect();

    if let Ok(branch_output) = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "for-each-ref",
            "refs/heads",
            "--format=%(refname:short)%00%(objectname)%00%(HEAD)%00%(upstream:short)",
        ])
        .current_dir(&repo_path)
        .output()
    {
        if branch_output.status.success() {
            let stdout = String::from_utf8_lossy(&branch_output.stdout);
            for line in stdout.lines() {
                let mut parts = line.split('\0');
                let name = parts.next().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let head = parts.next().unwrap_or("").to_string();
                let is_current = parts.next().unwrap_or("") == "*";
                let upstream = parts
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let worktree_path = checked_out_branches.get(&name).cloned();
                snapshot.branches.push(GitBranchEntry {
                    name,
                    head,
                    is_current,
                    upstream,
                    worktree_path,
                });
            }
        }
    }

    snapshot.branches.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then_with(|| b.worktree_path.is_some().cmp(&a.worktree_path.is_some()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let elapsed = started.elapsed();
    perf_log!(
        "git_worktrees tab={} repo={} worktrees={} branches={} took={}ms",
        tab_id,
        repo_path.display(),
        snapshot.worktrees.len(),
        snapshot.branches.len(),
        elapsed.as_millis()
    );

    snapshot
}

pub(crate) fn collect_remote_sessions(host: RemoteHostConfig) -> RemoteSessionsSnapshot {
    let started = Instant::now();
    let snapshot = gitterm_core::remote::ssh_sessions::collect_remote_sessions(&host);
    let elapsed = started.elapsed();
    perf_log!(
        "remote_sessions host={} sessions={} took={}ms",
        host.name,
        snapshot.sessions.len(),
        elapsed.as_millis()
    );
    snapshot
}

fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Fallback git status collection using the git2 library, used when the `git` CLI is not found.
pub(crate) fn collect_diff(
    tab_id: usize,
    repo_path: PathBuf,
    file_path: String,
    is_staged: bool,
) -> DiffSnapshot {
    let started = Instant::now();

    let mut lines: Vec<DiffLine> = gitterm::agentd::git::collect_file_diff(
        &repo_path,
        &file_path,
        is_staged,
        MAX_UNTRACKED_DIFF_PREVIEW_LINES,
    )
    .into_iter()
    .map(diff_line)
    .collect();
    add_word_diffs_to_lines(&mut lines);

    let snapshot = DiffSnapshot {
        tab_id,
        file_path,
        is_staged,
        lines,
        diff_syntax_lines: None,
        diff_syntax_notice: None,
    };

    perf_log!(
        "diff tab={} file={} staged={} lines={} took={}ms",
        tab_id,
        snapshot.file_path,
        snapshot.is_staged,
        snapshot.lines.len(),
        started.elapsed().as_millis()
    );

    snapshot
}

pub(crate) fn diff_line(line: gitterm::agentd::git::FileDiffLine) -> DiffLine {
    use gitterm::agentd::git::DiffLineKind;
    DiffLine {
        content: line.content,
        line_type: match line.kind {
            DiffLineKind::Context => DiffLineType::Context,
            DiffLineKind::Addition => DiffLineType::Addition,
            DiffLineKind::Deletion => DiffLineType::Deletion,
            DiffLineKind::Header => DiffLineType::Header,
        },
        old_line_num: line.old_line,
        new_line_num: line.new_line,
        inline_changes: None,
    }
}

pub(crate) fn collect_file_load(
    tab_id: usize,
    path: PathBuf,
    is_dark_theme: bool,
) -> FileLoadSnapshot {
    file_load_snapshot(gitterm_core::content::collect_file_load(
        tab_id,
        path,
        is_dark_theme,
    ))
}

/// Remote shaping delegates without consulting the local filesystem.
pub(crate) fn shape_source_file_load(
    tab_id: usize,
    path: PathBuf,
    content: Result<crate::source::SourceFileContent, String>,
    is_dark_theme: bool,
) -> FileLoadSnapshot {
    file_load_snapshot(gitterm_core::content::shape_source_file_load(
        tab_id,
        path,
        content,
        is_dark_theme,
    ))
}

fn file_load_snapshot(snapshot: gitterm_core::content::FileLoadSnapshot) -> FileLoadSnapshot {
    FileLoadSnapshot {
        tab_id: snapshot.tab_id,
        path: snapshot.path,
        file_content: snapshot.file_content,
        image_path: snapshot.image_path,
        webview_content: snapshot.webview_content,
        file_preview_notice: snapshot.file_preview_notice,
        syntax_highlight_lines: None,
        syntax_highlight_notice: None,
        file_signature: snapshot
            .file_signature
            .map(|signature| FileVersionSignature {
                modified_unix_nanos: signature.modified_unix_nanos,
                file_len: signature.file_len,
            }),
    }
}

pub(crate) fn collect_file_syntax_highlight(
    tab_id: usize,
    path: PathBuf,
    file_content: String,
    is_dark_theme: bool,
    file_signature: Option<FileVersionSignature>,
    max_lines: usize,
) -> FileSyntaxSnapshot {
    let started = Instant::now();
    let content_prefix = if max_lines == 0 {
        String::new()
    } else {
        file_content
            .lines()
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (syntax_highlight_lines, syntax_highlight_notice) =
        if content_prefix.trim().is_empty() || TabState::is_markdown_file(&path) {
            (None, None)
        } else {
            build_syntax_highlight_lines(&path, &content_prefix, is_dark_theme)
        };

    perf_log!(
        "syntax_load tab={} path={} bytes={} requested_lines={} highlighted_lines={} notice={} took={}ms",
        tab_id,
        path.display(),
        content_prefix.len(),
        max_lines,
        syntax_highlight_lines
            .as_ref()
            .map(|v| v.len())
            .unwrap_or(0),
        syntax_highlight_notice.is_some(),
        started.elapsed().as_millis()
    );

    FileSyntaxSnapshot {
        tab_id,
        path,
        syntax_highlight_lines,
        syntax_highlight_notice,
        file_signature,
    }
}

/// Preserve remote path tokens and source errors at the frontend/core boundary.
pub(crate) fn source_git_status_snapshot(
    tab_id: usize,
    fallback_root: PathBuf,
    result: Result<crate::source::SourceGitStatus, String>,
) -> GitStatusSnapshot {
    match result {
        Ok(status) => GitStatusSnapshot {
            tab_id,
            repo_path: PathBuf::from(status.root.display()),
            repo_name: status.repo_name,
            branch_name: status.branch_name,
            is_git_repo: status.is_git_repo,
            staged: status
                .staged
                .into_iter()
                .map(file_entry_from_source)
                .collect(),
            unstaged: status
                .unstaged
                .into_iter()
                .map(file_entry_from_source)
                .collect(),
            untracked: status
                .untracked
                .into_iter()
                .map(file_entry_from_source)
                .collect(),
            error: None,
        },
        Err(error) => GitStatusSnapshot {
            tab_id,
            repo_path: fallback_root,
            repo_name: String::new(),
            branch_name: String::new(),
            is_git_repo: false,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            error: Some(error),
        },
    }
}

fn file_entry_from_source(file: crate::source::SourceGitFile) -> FileEntry {
    FileEntry {
        path: file.path,
        status: file.status,
        is_staged: file.is_staged,
    }
}
