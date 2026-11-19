//! Git status DTOs. Collection, source conversion and polling stay in the host.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileStatus {
    pub path: String,
    pub status: String,
    pub is_staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    /// The repo root actually used; self-healed to `--show-toplevel` when
    /// the requested path was inside (not at) the repository root.
    pub root: PathBuf,
    pub repo_name: String,
    pub branch_name: String,
    pub is_git_repo: bool,
    pub staged: Vec<GitFileStatus>,
    pub unstaged: Vec<GitFileStatus>,
    pub untracked: Vec<GitFileStatus>,
}

#[derive(Debug, Clone)]
pub struct GitStatusSnapshot {
    pub tab_id: usize,
    pub repo_path: PathBuf,
    pub repo_name: String,
    pub branch_name: String,
    pub is_git_repo: bool,
    pub staged: Vec<GitFileStatus>,
    pub unstaged: Vec<GitFileStatus>,
    pub untracked: Vec<GitFileStatus>,
    /// Source-level failure (e.g. remote agent unreachable); shown in the
    /// Git sidebar instead of silently reporting a clean tree.
    pub error: Option<String>,
}
