//! Git status collection shared by the desktop's local backend and the
//! remote agent, so both sources report identical status semantics.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use git2::{Repository, Status, StatusOptions};

pub use gitterm_core::status::{GitFileStatus, RepoStatus};

pub use gitterm_core::git_status::{collect_repo_status, git_command, status_char};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = git_command()
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    }

    #[test]
    fn collects_status_from_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "T"]);
        std::fs::write(dir.path().join("committed.txt"), "a").unwrap();
        git(dir.path(), &["add", "committed.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("committed.txt"), "changed").unwrap();
        std::fs::write(dir.path().join("staged.txt"), "s").unwrap();
        git(dir.path(), &["add", "staged.txt"]);
        std::fs::write(dir.path().join("untracked.txt"), "u").unwrap();

        let status = collect_repo_status(dir.path());
        assert!(status.is_git_repo);
        assert_eq!(status.branch_name, "main");
        assert_eq!(
            status
                .staged
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["staged.txt"]
        );
        assert_eq!(
            status
                .unstaged
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["committed.txt"]
        );
        assert_eq!(
            status
                .untracked
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["untracked.txt"]
        );
    }

    #[test]
    fn collects_diff_for_modified_and_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "T"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("a.txt"), "one\nchanged\n").unwrap();
        let lines = collect_file_diff(dir.path(), "a.txt", false, 100);
        assert!(lines
            .iter()
            .any(|l| l.kind == DiffLineKind::Deletion && l.content == "two"));
        assert!(lines
            .iter()
            .any(|l| l.kind == DiffLineKind::Addition && l.content == "changed"));

        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        let lines = collect_file_diff(dir.path(), "new.txt", false, 100);
        assert!(matches!(lines.first(), Some(l) if l.kind == DiffLineKind::Header));
        assert!(lines
            .iter()
            .any(|l| l.kind == DiffLineKind::Addition && l.content == "hello"));
        // A chat tab rooted below the repo root still gets the diff of a
        // repo-relative path (TRU-237 review F3).
        let sub = dir.path().join("notes");
        std::fs::create_dir_all(&sub).unwrap();
        let lines = collect_file_diff(&sub, "a.txt", false, 100);
        assert!(lines
            .iter()
            .any(|l| l.kind == DiffLineKind::Addition && l.content == "changed"));
        let lines = collect_file_diff(&sub, "new.txt", false, 100);
        assert!(lines
            .iter()
            .any(|l| l.kind == DiffLineKind::Addition && l.content == "hello"));
    }

    #[test]
    fn subdir_status_paths_select_the_matching_diff() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "T"]);
        let sub = dir.path().join("work");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "before\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        std::fs::write(sub.join("a.txt"), "after\n").unwrap();
        std::fs::write(sub.join("b.txt"), "inside work\n").unwrap();
        std::fs::write(sub.join("staged.txt"), "staged inside work\n").unwrap();
        git(dir.path(), &["add", "work/staged.txt"]);
        // A same-named root file must never be selected for work/b.txt.
        std::fs::write(dir.path().join("b.txt"), "at root\n").unwrap();

        let status = collect_repo_status(&sub);
        let root_status = collect_repo_status(dir.path());
        assert_eq!(status.staged, root_status.staged);
        assert_eq!(status.unstaged, root_status.unstaged);
        assert_eq!(status.untracked, root_status.untracked);
        for (entries, path, content) in [
            (&status.unstaged, "work/a.txt", "after"),
            (&status.untracked, "work/b.txt", "inside work"),
            (&status.staged, "work/staged.txt", "staged inside work"),
        ] {
            let file = entries.iter().find(|f| f.path == path).unwrap();
            let diff = collect_file_diff(&sub, &file.path, file.is_staged, 100);
            assert!(
                diff.iter()
                    .any(|line| line.kind == DiffLineKind::Addition && line.content == content),
                "{path}: {diff:?}"
            );
        }
    }

    #[test]
    fn non_repo_reports_not_git() {
        let dir = tempfile::tempdir().unwrap();
        let status = collect_repo_status(dir.path());
        assert!(!status.is_git_repo);
        assert!(status.staged.is_empty());
    }

    #[test]
    fn self_heals_to_toplevel_from_subdir() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let status = collect_repo_status(&sub);
        assert!(status.is_git_repo);
        let healed = std::fs::canonicalize(&status.root).unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(healed, expected);
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let output = git_command()
            .current_dir(root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn odd_names() -> Vec<&'static str> {
        if cfg!(windows) {
            vec!["space name.txt"]
        } else {
            vec![
                "double\"quote.txt",
                "line\nbreak.txt",
                "space name.txt",
                "tab\tfile.txt",
            ]
        }
    }

    #[test]
    fn cli_parser_and_missing_git_fallback_baseline() {
        const CHILD: &str = "GITTERM_V6_GIT_BASELINE_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let nested = root.join("nested/deep");
            let status = collect_repo_status(&nested);
            assert!(status.is_git_repo);
            // characterized: git2 fallback discovers the repo but retains the requested nested root/name.
            assert_eq!(status.root, nested);
            assert_eq!(status.repo_name, "deep");
            let detached = std::env::var_os("GITTERM_V6_GIT_BASELINE_DETACHED").is_some();
            assert_eq!(
                status.branch_name,
                if detached { "HEAD" } else { "fixture-branch" }
            );
            let paths: Vec<_> = status.untracked.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(paths, odd_names());
            // Renames are disabled in the fallback: separate add/delete entries.
            assert_eq!(
                status
                    .staged
                    .iter()
                    .map(|f| (f.path.as_str(), f.status.as_str()))
                    .collect::<Vec<_>>(),
                [("old.txt", "D"), ("renamed file.txt", "A")]
            );
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-qb", "fixture-branch"]);
        git(&root, &["config", "core.quotePath", "true"]);
        std::fs::write(root.join("old.txt"), "fixture\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "fixture"]);
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        git(&root, &["mv", "old.txt", "renamed file.txt"]);
        for path in odd_names() {
            std::fs::write(root.join(path), "fixture\n").unwrap();
        }
        for detached in [false, true] {
            if detached {
                git(&root, &["checkout", "--detach", "-q"]);
            }
            let status = collect_repo_status(&root.join("nested/deep"));
            assert!(status.is_git_repo);
            assert_eq!(
                std::fs::canonicalize(&status.root).unwrap(),
                std::fs::canonicalize(&root).unwrap()
            );
            assert_eq!(status.repo_name, "repo");
            // characterized: detached CLI HEAD keeps the initial "main" label, unlike fallback "HEAD".
            assert_eq!(
                status.branch_name,
                if detached { "main" } else { "fixture-branch" }
            );
            // characterized: line parser leaves C-quoted filenames escaped and truncates a spaced rename at the last space.
            assert_eq!(
                status
                    .untracked
                    .iter()
                    .map(|f| f.path.as_str())
                    .collect::<Vec<_>>(),
                if cfg!(windows) {
                    vec!["space name.txt"]
                } else {
                    vec![
                        "\"double\\\"quote.txt\"",
                        "\"line\\nbreak.txt\"",
                        "space name.txt",
                        "\"tab\\tfile.txt\"",
                    ]
                }
            );
            assert_eq!(
                status
                    .staged
                    .iter()
                    .map(|f| (f.path.as_str(), f.status.as_str()))
                    .collect::<Vec<_>>(),
                [("file.txt", "R")]
            );
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "agentd::git::baseline_tests::cli_parser_and_missing_git_fallback_baseline",
                    "--nocapture",
                ])
                .env(CHILD, &root)
                .env("PATH", fixture.path().join("missing-git-bin"));
            if detached {
                child.env("GITTERM_V6_GIT_BASELINE_DETACHED", "1");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let nonrepo = collect_repo_status(fixture.path());
        assert!(!nonrepo.is_git_repo);
        assert_eq!(nonrepo.branch_name, "main");
    }
}
