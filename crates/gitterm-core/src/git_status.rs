//! Shared local status collector, including the characterized CLI/git2 fallback.

use crate::status::{GitFileStatus, RepoStatus};
use git2::{Repository, Status, StatusOptions};
use std::path::{Path, PathBuf};

/// Build a `git` Command with repo-discovery env scrubbed. When this
/// process is spawned from a git hook, git exports GIT_DIR / GIT_WORK_TREE /
/// GIT_INDEX_FILE pointing at the hook's repo; inheriting them would
/// redirect every command away from the `current_dir` it is aimed at.
pub fn git_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string())
}

/// Collect git status for a repository path.
///
/// Prefers the native git CLI (`status --porcelain=v2 --branch`) because it
/// benefits from fsmonitor/untracked-cache and takes no optional locks;
/// falls back to git2 when no git binary is installed.
pub fn collect_repo_status(repo_path: &Path) -> RepoStatus {
    collect_repo_status_with(repo_path, StatusReadOptions::default())
}

/// How a status read talks to git.
#[derive(Debug, Clone, Copy)]
pub struct StatusReadOptions {
    /// `false` runs git with `-c core.fsmonitor=false`, so a configured
    /// fsmonitor hook or daemon never starts and nothing is written under
    /// `.git`. The default (`true`) keeps git's own configuration.
    pub fsmonitor: bool,
}

impl Default for StatusReadOptions {
    fn default() -> Self {
        Self { fsmonitor: true }
    }
}

/// `collect_repo_status` with explicit options.
///
/// The git2 fallback never runs an fsmonitor hook: libgit2 does not implement
/// `core.fsmonitor`, so the option needs no handling there.
pub fn collect_repo_status_with(repo_path: &Path, options: StatusReadOptions) -> RepoStatus {
    let mut status = RepoStatus {
        root: repo_path.to_path_buf(),
        repo_name: dir_name(repo_path),
        branch_name: "main".to_string(),
        is_git_repo: false,
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: Vec::new(),
    };

    // Diff paths and the git2 fallback use the repo root, even when a
    // chat's cwd is a subfolder.
    let mut cmd = git_command();
    cmd.args(["--no-optional-locks", "-c", "status.relativePaths=false"]);
    if !options.fsmonitor {
        cmd.args(["-c", "core.fsmonitor=false"]);
    }
    let cli_result = cmd
        .args(["status", "--porcelain=v2", "--branch"])
        .current_dir(repo_path)
        .output();

    let output = match cli_result {
        Ok(o) if o.status.success() => o,
        Ok(_) => return status, // git ran, not a git repo
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return collect_repo_status_git2(status, repo_path);
        }
        Err(_) => return status,
    };

    status.is_git_repo = true;
    parse_porcelain_v2(&String::from_utf8_lossy(&output.stdout), &mut status);

    // Self-heal repo path: if .git isn't here, discover the real toplevel.
    if !repo_path.join(".git").exists() {
        // NOTE: --no-optional-locks is a git-level flag and must precede the
        // subcommand; after `rev-parse` it gets echoed into stdout and
        // corrupts the discovered root.
        if let Ok(toplevel_output) = git_command()
            .args(["--no-optional-locks", "rev-parse", "--show-toplevel"])
            .current_dir(repo_path)
            .output()
        {
            if toplevel_output.status.success() {
                let root = String::from_utf8_lossy(&toplevel_output.stdout)
                    .trim()
                    .to_string();
                let root_path = PathBuf::from(root);
                if root_path != status.root {
                    status.repo_name = dir_name(&root_path);
                    status.root = root_path;
                }
            }
        }
    }

    status
}

fn parse_porcelain_v2(stdout: &str, status: &mut RepoStatus) {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            let branch = rest.trim();
            if !branch.is_empty() && branch != "(detached)" {
                status.branch_name = branch.to_string();
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

            let path = if line.starts_with("2 ") {
                line.split('\t')
                    .next()
                    .and_then(|before_tab| before_tab.rsplit(' ').next())
                    .unwrap_or("")
                    .to_string()
            } else {
                // Regular: path is everything after the 8th space
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

            if matches!(index_status, b'A' | b'M' | b'D' | b'R') {
                status.staged.push(GitFileStatus {
                    path: path.clone(),
                    status: (index_status as char).to_string(),
                    is_staged: true,
                });
            }
            if matches!(worktree_status, b'M' | b'D') {
                status.unstaged.push(GitFileStatus {
                    path: path.clone(),
                    status: (worktree_status as char).to_string(),
                    is_staged: false,
                });
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            status.untracked.push(GitFileStatus {
                path: path.to_string(),
                status: "?".to_string(),
                is_staged: false,
            });
        }
        // Skip "u " (unmerged) and other header lines for now.
    }
}

pub fn status_char(status: Status, staged: bool) -> String {
    if staged {
        if status.contains(Status::INDEX_NEW) {
            "A".to_string()
        } else if status.contains(Status::INDEX_MODIFIED) {
            "M".to_string()
        } else if status.contains(Status::INDEX_DELETED) {
            "D".to_string()
        } else if status.contains(Status::INDEX_RENAMED) {
            "R".to_string()
        } else {
            "?".to_string()
        }
    } else if status.contains(Status::WT_MODIFIED) {
        "M".to_string()
    } else if status.contains(Status::WT_DELETED) {
        "D".to_string()
    } else if status.contains(Status::WT_RENAMED) {
        "R".to_string()
    } else {
        "?".to_string()
    }
}

fn collect_repo_status_git2(mut status: RepoStatus, repo_path: &Path) -> RepoStatus {
    let Ok(repo) = Repository::open(repo_path).or_else(|_| Repository::discover(repo_path)) else {
        return status;
    };

    status.is_git_repo = true;

    if let Ok(head) = repo.head() {
        if let Some(name) = head.shorthand() {
            status.branch_name = name.to_string();
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
            let entry_status = entry.status();

            if entry_status.intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED,
            ) {
                status.staged.push(GitFileStatus {
                    path: path.clone(),
                    status: status_char(entry_status, true),
                    is_staged: true,
                });
            }
            if entry_status
                .intersects(Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED)
            {
                status.unstaged.push(GitFileStatus {
                    path: path.clone(),
                    status: status_char(entry_status, false),
                    is_staged: false,
                });
            }
            if entry_status.contains(Status::WT_NEW) {
                status.untracked.push(GitFileStatus {
                    path,
                    status: "?".to_string(),
                    is_staged: false,
                });
            }
        }
    }

    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = git_command().args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out);
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "n\n").unwrap();
        dir
    }

    #[test]
    fn fsmonitor_off_matches_default_output_without_fsmonitor() {
        let dir = repo();
        let on = collect_repo_status(dir.path());
        let off = collect_repo_status_with(dir.path(), StatusReadOptions { fsmonitor: false });
        assert!(on.is_git_repo);
        assert_eq!(on.branch_name, off.branch_name);
        assert_eq!(on.root, off.root);
        let paths = |v: &[GitFileStatus]| v.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&on.unstaged), paths(&off.unstaged));
        assert_eq!(paths(&on.untracked), paths(&off.untracked));
        assert_eq!(paths(&on.staged), paths(&off.staged));
        assert_eq!(paths(&off.unstaged), ["a.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn fsmonitor_off_never_runs_the_configured_hook() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let hooks = tempfile::tempdir().unwrap();
        let marker = hooks.path().join("marker");
        let hook = hooks.path().join("hook.sh");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho ran >> '{}'\nprintf '\\0'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            dir.path(),
            &["config", "core.fsmonitor", hook.to_str().unwrap()],
        );

        collect_repo_status_with(dir.path(), StatusReadOptions { fsmonitor: false });
        assert!(!marker.exists(), "fsmonitor hook ran with fsmonitor off");
        assert!(!dir.path().join(".git/fsmonitor--daemon").exists());

        // Control: the plain collector still runs the hook, so the check above
        // would catch a regression.
        collect_repo_status(dir.path());
        assert!(marker.exists(), "default collector should run the hook");
    }
}
