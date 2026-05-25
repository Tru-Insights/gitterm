#[cfg(feature = "excalidraw")]
use crate::excalidraw;
use crate::markdown;
use crate::{
    add_word_diffs_to_lines, build_syntax_highlight_lines, format_bytes, read_text_preview,
    DiffLine, DiffLineType, DiffSnapshot, FileEntry, FileLoadSnapshot, FileSyntaxSnapshot,
    FileTreeEntry, FileTreeSnapshot, FileVersionSignature, GitStatusSnapshot, TabState,
    LARGE_TEXT_PREVIEW_BYTES, LARGE_TEXT_PREVIEW_LINES, MAX_FULL_TEXT_LOAD_BYTES,
    MAX_INLINE_WEBVIEW_BYTES,
};
use git2::{DiffOptions, Repository, Status, StatusOptions};
use std::path::PathBuf;
use std::time::{Instant, UNIX_EPOCH};

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

    let mut snapshot = GitStatusSnapshot {
        tab_id,
        repo_name: repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string()),
        repo_path: repo_path.clone(),
        branch_name: "main".to_string(),
        is_git_repo: false,
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: Vec::new(),
    };

    // Use native git CLI — faster than git2 because it uses fsmonitor,
    // split index, untracked cache, and other optimizations.
    //
    // Single command: `git status --porcelain=v2 --branch --no-renames --no-optional-locks`
    // This gives us branch info + file status in one process spawn.
    // --no-optional-locks avoids contention with other git processes (e.g. Claude Code).
    let cli_result = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ])
        .current_dir(&repo_path)
        .output();

    // If git binary isn't found, fall back to git2 library.
    // If git ran but returned non-zero, the directory isn't a git repo — don't bother with git2.
    let output = match cli_result {
        Ok(o) if o.status.success() => o,
        Ok(_) => return snapshot, // git ran, not a git repo
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return collect_git_status_git2(snapshot, &repo_path);
        }
        Err(_) => return snapshot,
    };

    snapshot.is_git_repo = true;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            let branch = rest.trim();
            if !branch.is_empty() && branch != "(detached)" {
                snapshot.branch_name = branch.to_string();
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            // Changed entries: "1 XY sub mH mI mW hH hI path"
            // or rename:       "2 XY sub mH mI mW hH hI X### path\torigPath"
            let bytes = line.as_bytes();
            if bytes.len() < 5 {
                continue;
            }
            let index_status = bytes[2];
            let worktree_status = bytes[3];

            // Path is the last space-separated field (field 9 for type 1, field 10 for type 2)
            // For type 1: split by space, take index 8+
            // For type 2: split by tab first to handle renames, then space
            let path = if line.starts_with("2 ") {
                // Rename: "2 XY ... X### path\torigPath"
                line.split('\t')
                    .next()
                    .and_then(|before_tab| before_tab.rsplit(' ').next())
                    .unwrap_or("")
                    .to_string()
            } else {
                // Regular: "1 XY ... path" — path is everything after 8th space
                let mut space_count = 0;
                let mut path_start = 0;
                for (i, b) in bytes.iter().enumerate() {
                    if *b == b' ' {
                        space_count += 1;
                        if space_count == 8 {
                            path_start = i + 1;
                            break;
                        }
                    }
                }
                if path_start > 0 && path_start < bytes.len() {
                    String::from_utf8_lossy(&bytes[path_start..]).to_string()
                } else {
                    continue;
                }
            };

            if path.is_empty() {
                continue;
            }

            // Staged changes (index column)
            match index_status {
                b'A' => snapshot.staged.push(FileEntry {
                    path: path.clone(),
                    status: "A".to_string(),
                    is_staged: true,
                }),
                b'M' => snapshot.staged.push(FileEntry {
                    path: path.clone(),
                    status: "M".to_string(),
                    is_staged: true,
                }),
                b'D' => snapshot.staged.push(FileEntry {
                    path: path.clone(),
                    status: "D".to_string(),
                    is_staged: true,
                }),
                b'R' => snapshot.staged.push(FileEntry {
                    path: path.clone(),
                    status: "R".to_string(),
                    is_staged: true,
                }),
                _ => {}
            }

            // Unstaged changes (worktree column)
            match worktree_status {
                b'M' => snapshot.unstaged.push(FileEntry {
                    path: path.clone(),
                    status: "M".to_string(),
                    is_staged: false,
                }),
                b'D' => snapshot.unstaged.push(FileEntry {
                    path: path.clone(),
                    status: "D".to_string(),
                    is_staged: false,
                }),
                _ => {}
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            snapshot.untracked.push(FileEntry {
                path: path.to_string(),
                status: "?".to_string(),
                is_staged: false,
            });
        }
        // Skip "u " (unmerged) and other header lines for now
    }

    // Self-heal repo path: check if .git exists at repo_path, otherwise discover root
    if !repo_path.join(".git").exists() {
        if let Ok(toplevel_output) = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel", "--no-optional-locks"])
            .current_dir(&repo_path)
            .output()
        {
            if toplevel_output.status.success() {
                let root = String::from_utf8_lossy(&toplevel_output.stdout)
                    .trim()
                    .to_string();
                let root_path = PathBuf::from(root);
                if root_path != repo_path {
                    snapshot.repo_path = root_path;
                    snapshot.repo_name = snapshot
                        .repo_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "repo".to_string());
                }
            }
        }
    }

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

/// Fallback git status collection using the git2 library, used when the `git` CLI is not found.
fn collect_git_status_git2(
    mut snapshot: GitStatusSnapshot,
    repo_path: &std::path::Path,
) -> GitStatusSnapshot {
    use crate::status_char;
    let Ok(repo) = Repository::open(repo_path).or_else(|_| Repository::discover(repo_path)) else {
        return snapshot;
    };

    snapshot.is_git_repo = true;

    if let Ok(head) = repo.head() {
        if let Some(name) = head.shorthand() {
            snapshot.branch_name = name.to_string();
        }
    }

    let mut opts = StatusOptions::new();
    opts.no_refresh(true)
        .include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true)
        .include_unmodified(false)
        .renames_head_to_index(false)
        .renames_index_to_workdir(false);

    if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
        for entry in statuses.iter() {
            let path = entry.path().unwrap_or("").to_string();
            let status = entry.status();

            if status.intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED,
            ) {
                snapshot.staged.push(FileEntry {
                    path: path.clone(),
                    status: status_char(status, true),
                    is_staged: true,
                });
            }
            if status.intersects(Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED) {
                snapshot.unstaged.push(FileEntry {
                    path: path.clone(),
                    status: status_char(status, false),
                    is_staged: false,
                });
            }
            if status.contains(Status::WT_NEW) {
                snapshot.untracked.push(FileEntry {
                    path,
                    status: "?".to_string(),
                    is_staged: false,
                });
            }
        }
    }

    snapshot
}

pub(crate) fn collect_file_tree(
    tab_id: usize,
    current_dir: PathBuf,
    show_hidden: bool,
) -> FileTreeSnapshot {
    let started = Instant::now();
    let mut dirs: Vec<FileTreeEntry> = Vec::new();
    let mut files: Vec<FileTreeEntry> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&current_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if name == "node_modules" || name == "target" {
                continue;
            }
            if !show_hidden && name.starts_with('.') {
                continue;
            }

            let is_dir = path.is_dir();
            let entry = FileTreeEntry { name, path, is_dir };
            if is_dir {
                dirs.push(entry);
            } else {
                files.push(entry);
            }
        }
    }

    dirs.sort_by_key(|a| a.name.to_lowercase());
    files.sort_by_key(|a| a.name.to_lowercase());
    dirs.extend(files);

    let snapshot = FileTreeSnapshot {
        tab_id,
        current_dir,
        entries: dirs,
    };

    perf_log!(
        "file_tree tab={} dir={} entries={} hidden={} took={}ms",
        tab_id,
        snapshot.current_dir.display(),
        snapshot.entries.len(),
        show_hidden,
        started.elapsed().as_millis()
    );

    snapshot
}

pub(crate) fn collect_diff(
    tab_id: usize,
    repo_path: PathBuf,
    file_path: String,
    is_staged: bool,
) -> DiffSnapshot {
    let started = Instant::now();
    let mut lines = Vec::new();
    let Ok(repo) = Repository::open(&repo_path) else {
        let snapshot = DiffSnapshot {
            tab_id,
            file_path,
            is_staged,
            lines,
            diff_syntax_lines: None,
            diff_syntax_notice: None,
        };
        perf_log!(
            "diff tab={} file={} staged={} lines={} took={}ms (repo open failed)",
            tab_id,
            snapshot.file_path,
            snapshot.is_staged,
            snapshot.lines.len(),
            started.elapsed().as_millis()
        );
        return snapshot;
    };

    // Use no_refresh + pathspec so status doesn't rewrite .git/index.lock
    // (would contend with concurrent git commands) and only inspects this file.
    let mut untracked_opts = StatusOptions::new();
    untracked_opts
        .no_refresh(true)
        .include_untracked(true)
        .pathspec(&file_path);
    let is_untracked = repo
        .statuses(Some(&mut untracked_opts))
        .ok()
        .map(|statuses| {
            statuses.iter().any(|e| {
                e.path() == Some(file_path.as_str()) && e.status().contains(Status::WT_NEW)
            })
        })
        .unwrap_or(false);

    if is_untracked {
        let full_path = repo_path.join(&file_path);
        if let Ok(content) = std::fs::read_to_string(&full_path) {
            let total_lines = content.lines().count();
            lines.push(DiffLine {
                content: format!("@@ -0,0 +1,{} @@ (new file)", total_lines),
                line_type: DiffLineType::Header,
                old_line_num: None,
                new_line_num: None,
                inline_changes: None,
            });
            for (i, line) in content
                .lines()
                .take(MAX_UNTRACKED_DIFF_PREVIEW_LINES)
                .enumerate()
            {
                lines.push(DiffLine {
                    content: line.to_string(),
                    line_type: DiffLineType::Addition,
                    old_line_num: None,
                    new_line_num: Some((i + 1) as u32),
                    inline_changes: None,
                });
            }
            if total_lines > MAX_UNTRACKED_DIFF_PREVIEW_LINES {
                lines.push(DiffLine {
                    content: format!(
                        "... truncated to first {} lines ({} total)",
                        MAX_UNTRACKED_DIFF_PREVIEW_LINES, total_lines
                    ),
                    line_type: DiffLineType::Header,
                    old_line_num: None,
                    new_line_num: None,
                    inline_changes: None,
                });
            }
        }
        let snapshot = DiffSnapshot {
            tab_id,
            file_path,
            is_staged,
            lines,
            diff_syntax_lines: None,
            diff_syntax_notice: None,
        };
        perf_log!(
            "diff tab={} file={} staged={} lines={} took={}ms (untracked preview)",
            tab_id,
            snapshot.file_path,
            snapshot.is_staged,
            snapshot.lines.len(),
            started.elapsed().as_millis()
        );
        return snapshot;
    }

    let mut diff_opts = DiffOptions::new();
    diff_opts.pathspec(&file_path);
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
                        lines.push(DiffLine {
                            content: format!(
                                "@@ -{},{} +{},{} @@",
                                h.old_start(),
                                h.old_lines(),
                                h.new_start(),
                                h.new_lines()
                            ),
                            line_type: DiffLineType::Header,
                            old_line_num: None,
                            new_line_num: None,
                            inline_changes: None,
                        });
                    }
                }
                '+' => lines.push(DiffLine {
                    content,
                    line_type: DiffLineType::Addition,
                    old_line_num: None,
                    new_line_num: line.new_lineno(),
                    inline_changes: None,
                }),
                '-' => lines.push(DiffLine {
                    content,
                    line_type: DiffLineType::Deletion,
                    old_line_num: line.old_lineno(),
                    new_line_num: None,
                    inline_changes: None,
                }),
                ' ' => lines.push(DiffLine {
                    content,
                    line_type: DiffLineType::Context,
                    old_line_num: line.old_lineno(),
                    new_line_num: line.new_lineno(),
                    inline_changes: None,
                }),
                _ => {}
            }
            true
        });
        add_word_diffs_to_lines(&mut lines);
    }

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

pub(crate) fn collect_file_load(
    tab_id: usize,
    path: PathBuf,
    is_dark_theme: bool,
) -> FileLoadSnapshot {
    let started = Instant::now();
    let mut snapshot = FileLoadSnapshot {
        tab_id,
        path: path.clone(),
        file_content: String::new(),
        image_path: None,
        webview_content: None,
        file_preview_notice: None,
        syntax_highlight_lines: None,
        syntax_highlight_notice: None,
        file_signature: None,
    };

    let file_metadata = std::fs::metadata(&path).ok();
    let file_size = file_metadata.as_ref().map(|m| m.len()).unwrap_or(0);
    snapshot.file_signature = file_metadata.as_ref().and_then(|metadata| {
        let modified_unix_nanos = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(FileVersionSignature {
            modified_unix_nanos,
            file_len: metadata.len(),
        })
    });

    #[cfg(feature = "excalidraw")]
    if excalidraw::is_excalidraw_file(&path) {
        if file_size > MAX_INLINE_WEBVIEW_BYTES {
            snapshot.file_preview_notice = Some(format!(
                "Inline preview skipped for large Excalidraw file ({}). Click \"View in Browser\".",
                format_bytes(file_size)
            ));
            perf_log!(
                "file_load tab={} path={} kind=excalidraw_skip size={}B text={}B webview={}B notice={} took={}ms",
                tab_id,
                path.display(),
                file_size,
                snapshot.file_content.len(),
                snapshot.webview_content.as_ref().map(|s| s.len()).unwrap_or(0),
                snapshot.file_preview_notice.is_some(),
                started.elapsed().as_millis()
            );
            return snapshot;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if excalidraw::validate_excalidraw(&content) {
                snapshot.webview_content =
                    Some(excalidraw::render_excalidraw_html(&content, is_dark_theme));
            }
        }
        perf_log!(
            "file_load tab={} path={} kind=excalidraw_inline size={}B text={}B webview={}B notice={} took={}ms",
            tab_id,
            path.display(),
            file_size,
            snapshot.file_content.len(),
            snapshot.webview_content.as_ref().map(|s| s.len()).unwrap_or(0),
            snapshot.file_preview_notice.is_some(),
            started.elapsed().as_millis()
        );
        return snapshot;
    }

    if TabState::is_markdown_file(&path) {
        if file_size > MAX_INLINE_WEBVIEW_BYTES {
            snapshot.file_preview_notice = Some(format!(
                "Inline preview skipped for large Markdown file ({}). Click \"View in Browser\".",
                format_bytes(file_size)
            ));
            perf_log!(
                "file_load tab={} path={} kind=markdown_skip size={}B text={}B webview={}B notice={} took={}ms",
                tab_id,
                path.display(),
                file_size,
                snapshot.file_content.len(),
                snapshot.webview_content.as_ref().map(|s| s.len()).unwrap_or(0),
                snapshot.file_preview_notice.is_some(),
                started.elapsed().as_millis()
            );
            return snapshot;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            snapshot.webview_content =
                Some(markdown::render_markdown_to_html(&content, is_dark_theme));
        }
    } else if TabState::is_html_file(&path) {
        if file_size > MAX_INLINE_WEBVIEW_BYTES {
            snapshot.file_preview_notice = Some(format!(
                "Inline preview skipped for large HTML file ({}). Click \"View in Browser\".",
                format_bytes(file_size)
            ));
            perf_log!(
                "file_load tab={} path={} kind=html_skip size={}B text={}B webview={}B notice={} took={}ms",
                tab_id,
                path.display(),
                file_size,
                snapshot.file_content.len(),
                snapshot.webview_content.as_ref().map(|s| s.len()).unwrap_or(0),
                snapshot.file_preview_notice.is_some(),
                started.elapsed().as_millis()
            );
            return snapshot;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            snapshot.webview_content = Some(content);
        }
    } else if TabState::is_image_file(&path) {
        snapshot.image_path = Some(path.clone());
    } else if file_size > MAX_FULL_TEXT_LOAD_BYTES {
        if let Ok(preview) =
            read_text_preview(&path, LARGE_TEXT_PREVIEW_BYTES, LARGE_TEXT_PREVIEW_LINES)
        {
            snapshot.file_content = preview;
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            snapshot.file_content = content;
        }
        snapshot.file_preview_notice = Some(format!(
            "Large file ({}): showing first {} lines (~{} KB).",
            format_bytes(file_size),
            LARGE_TEXT_PREVIEW_LINES,
            LARGE_TEXT_PREVIEW_BYTES / 1024
        ));
    } else if let Ok(content) = std::fs::read_to_string(&path) {
        snapshot.file_content = content;
    }

    let kind = if snapshot.image_path.is_some() {
        "image"
    } else if snapshot.webview_content.is_some() {
        "inline_webview"
    } else if snapshot.file_preview_notice.is_some() {
        "text_preview"
    } else {
        "text"
    };
    perf_log!(
        "file_load tab={} path={} kind={} size={}B text={}B webview={}B preview_notice={} syntax_notice={} took={}ms",
        tab_id,
        path.display(),
        kind,
        file_size,
        snapshot.file_content.len(),
        snapshot
            .webview_content
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0),
        snapshot.file_preview_notice.is_some(),
        false,
        started.elapsed().as_millis()
    );

    snapshot
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
