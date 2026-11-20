//! Workspace sources.
//!
//! A workspace is backed by a source: the local filesystem or a remote
//! `gitterm-v6-agent`. UI code (Files, Git, Plans, agent launchers) talks to
//! `WorkspaceSource` and renders what it returns. It must never branch on
//! which kind of source it has — a missing feature is expressed through
//! [`SourceCapabilities`], not location checks.
//!
//! Paths are opaque to the UI: it receives [`SourcePath`] tokens and hands
//! them back unchanged. All path semantics (parents, root confinement,
//! canonical form) live inside the source implementations. For remote
//! sources the agent's canonicalized paths are authoritative.

use std::path::PathBuf;

use super::client::RemoteAgentBackend;

/// Host bindings for credential lookup/version and local home-relative display.
/// No credential is resolved until an operation invokes the backend.
pub trait SourceHost: std::fmt::Debug + Clone {
    type ClientConfig: std::fmt::Debug + Clone;
    fn backend(config: Self::ClientConfig) -> RemoteAgentBackend;
    fn local_display_dir(root: &std::path::Path, dir: &std::path::Path) -> String;
}

pub use crate::content::source::{
    FilesState, SourceCapabilities, SourceDirEntry, SourceDirListing, SourceFileContent,
    SourceGitFile, SourceGitStatus, SourcePath,
};

/// A workspace's source. Cheap to clone; async calls own everything they
/// need so they can run on any task.
#[derive(Debug, Clone)]
pub enum WorkspaceSource<H: SourceHost> {
    Local {
        root: PathBuf,
    },
    RemoteAgent {
        workspace_id: String,
        root: String,
        client: H::ClientConfig,
    },
}

impl<H: SourceHost> WorkspaceSource<H> {
    pub fn capabilities(&self) -> SourceCapabilities {
        match self {
            WorkspaceSource::Local { .. } => SourceCapabilities {
                open_file: true,
                edit_file: true,
                git_status: true,
                git_diff: true,
                git_worktrees: true,
                sessions: true,
            },
            // Grows as the agent gains write ops (edit), GitStatus, and the
            // session runtime. Flipping a flag here is the only change the
            // UI needs.
            WorkspaceSource::RemoteAgent { .. } => SourceCapabilities {
                open_file: true,
                edit_file: false,
                git_status: true,
                git_diff: true,
                git_worktrees: false,
                sessions: true,
            },
        }
    }

    /// Whether a path token came from this source (Local paths belong to
    /// local sources, remote paths to remote ones).
    pub fn owns(&self, path: &SourcePath) -> bool {
        matches!(
            (self, path),
            (WorkspaceSource::Local { .. }, SourcePath::Local(_))
                | (WorkspaceSource::RemoteAgent { .. }, SourcePath::Remote(_))
        )
    }

    /// The directory a fresh Files view starts in.
    pub fn root(&self) -> SourcePath {
        match self {
            WorkspaceSource::Local { root } => SourcePath::Local(root.clone()),
            WorkspaceSource::RemoteAgent { root, .. } => SourcePath::Remote(root.clone()),
        }
    }

    pub async fn list_dir(
        &self,
        tab_id: usize,
        dir: SourcePath,
        show_hidden: bool,
    ) -> SourceDirListing {
        match (self, dir) {
            (WorkspaceSource::Local { root }, SourcePath::Local(dir)) => {
                let root = root.clone();
                let result = tokio::task::spawn_blocking(move || {
                    local_list_dir::<H>(tab_id, &root, dir, show_hidden)
                })
                .await;
                result.unwrap_or_else(|join_err| SourceDirListing {
                    tab_id,
                    display_dir: format!("{}/", self.root().display()),
                    current_dir: self.root(),
                    parent: None,
                    entries: Vec::new(),
                    error: Some(format!("directory listing task failed: {join_err}")),
                })
            }
            (
                WorkspaceSource::RemoteAgent {
                    workspace_id,
                    root,
                    client,
                },
                SourcePath::Remote(dir),
            ) => {
                remote_list_dir::<H>(
                    tab_id,
                    client.clone(),
                    workspace_id.clone(),
                    root.clone(),
                    dir,
                    show_hidden,
                )
                .await
            }
            // A token from one source handed to another is a programming
            // error; surface it loudly instead of guessing.
            (source, dir) => SourceDirListing {
                tab_id,
                display_dir: format!("{}/", source.root().display()),
                current_dir: source.root(),
                parent: None,
                entries: Vec::new(),
                error: Some(format!(
                    "internal error: path {} does not belong to this source",
                    dir.display()
                )),
            },
        }
    }

    /// Collect git status at this source's root. Errors carry user-facing
    /// context (endpoint/root), never a silent empty status.
    pub async fn git_status(&self) -> Result<SourceGitStatus, String> {
        match self {
            WorkspaceSource::Local { root } => {
                let root = root.clone();
                let status = tokio::task::spawn_blocking(move || {
                    crate::git_status::collect_repo_status(&root)
                })
                .await
                .map_err(|err| format!("git status task failed: {err}"))?;
                Ok(SourceGitStatus {
                    root: SourcePath::Local(status.root),
                    is_git_repo: status.is_git_repo,
                    repo_name: status.repo_name,
                    branch_name: status.branch_name,
                    staged: status.staged.into_iter().map(source_git_file).collect(),
                    unstaged: status.unstaged.into_iter().map(source_git_file).collect(),
                    untracked: status.untracked.into_iter().map(source_git_file).collect(),
                })
            }
            WorkspaceSource::RemoteAgent {
                workspace_id,
                root,
                client,
            } => {
                let status = H::backend(client.clone())
                    .git_status(workspace_id.clone(), root.clone())
                    .await
                    .map_err(|err| format!("remote git status for {root}: {err}"))?;
                let map = |file: super::client::RemoteAgentGitFile| SourceGitFile {
                    path: file.path,
                    status: file.status,
                    is_staged: file.is_staged,
                };
                Ok(SourceGitStatus {
                    root: SourcePath::Remote(status.root),
                    is_git_repo: status.is_git_repo,
                    repo_name: status.repo_name,
                    branch_name: status.branch_name,
                    staged: status.staged.into_iter().map(map).collect(),
                    unstaged: status.unstaged.into_iter().map(map).collect(),
                    untracked: status.untracked.into_iter().map(map).collect(),
                })
            }
        }
    }

    /// Diff one repo-relative file (from this source's git status). Lines
    /// come back unshaped; presentation (word diffs, syntax highlighting)
    /// happens on the desktop.
    pub async fn git_diff(
        &self,
        file_path: String,
        staged: bool,
    ) -> Result<Vec<crate::git_diff::FileDiffLine>, String> {
        const MAX_UNTRACKED_PREVIEW_LINES: usize = 3000;
        match self {
            WorkspaceSource::Local { root } => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || {
                    crate::git_diff::collect_file_diff(
                        &root,
                        &file_path,
                        staged,
                        MAX_UNTRACKED_PREVIEW_LINES,
                    )
                })
                .await
                .map_err(|err| format!("git diff task failed: {err}"))
            }
            WorkspaceSource::RemoteAgent {
                workspace_id,
                root,
                client,
            } => H::backend(client.clone())
                .git_diff(workspace_id.clone(), root.clone(), file_path, staged)
                .await
                .map(|diff| diff.lines)
                .map_err(|err| format!("remote git diff for {root}: {err}")),
        }
    }

    /// Read a file's bytes, capped at `max_bytes`. The path must have come
    /// from this source's listings.
    pub async fn read_file(
        &self,
        path: &SourcePath,
        max_bytes: u64,
    ) -> Result<SourceFileContent, String> {
        match (self, path) {
            (WorkspaceSource::Local { .. }, SourcePath::Local(path)) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    use std::io::Read as _;
                    let metadata = std::fs::metadata(&path)
                        .map_err(|err| format!("could not stat {}: {err}", path.display()))?;
                    let total_size = metadata.len();
                    let file = std::fs::File::open(&path)
                        .map_err(|err| format!("could not open {}: {err}", path.display()))?;
                    let mut data = Vec::new();
                    std::io::Read::take(file, max_bytes)
                        .read_to_end(&mut data)
                        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
                    Ok(SourceFileContent {
                        truncated: total_size > data.len() as u64,
                        total_size,
                        data,
                    })
                })
                .await
                .map_err(|err| format!("file read task failed: {err}"))?
            }
            (
                WorkspaceSource::RemoteAgent {
                    workspace_id,
                    root,
                    client,
                },
                SourcePath::Remote(path),
            ) => H::backend(client.clone())
                .read_file(workspace_id.clone(), root.clone(), path.clone(), max_bytes)
                .await
                .map(|content| SourceFileContent {
                    data: content.data,
                    total_size: content.total_size,
                    truncated: content.truncated,
                })
                .map_err(|err| err.to_string()),
            (_, path) => Err(format!(
                "internal error: path {} does not belong to this source",
                path.display()
            )),
        }
    }
}

pub fn local_list_dir<H: SourceHost>(
    tab_id: usize,
    root: &std::path::Path,
    dir: PathBuf,
    show_hidden: bool,
) -> SourceDirListing {
    let parent = dir
        .parent()
        .filter(|p| p.starts_with(root))
        .map(|p| SourcePath::Local(p.to_path_buf()));
    let display_dir = H::local_display_dir(root, &dir);

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            return SourceDirListing {
                tab_id,
                display_dir,
                current_dir: SourcePath::Local(dir.clone()),
                parent,
                entries: Vec::new(),
                error: Some(format!("could not read {}: {err}", dir.display())),
            };
        }
    };

    let mut dirs: Vec<SourceDirEntry> = Vec::new();
    let mut files: Vec<SourceDirEntry> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "node_modules" || name == "target" {
            continue;
        }
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = path.is_dir();
        let item = SourceDirEntry {
            name,
            is_git_repo: is_dir && path.join(".git").exists(),
            path: SourcePath::Local(path),
            is_dir,
        };
        if is_dir {
            dirs.push(item);
        } else {
            files.push(item);
        }
    }
    dirs.sort_by_key(|entry| entry.name.to_lowercase());
    files.sort_by_key(|entry| entry.name.to_lowercase());
    dirs.extend(files);

    SourceDirListing {
        tab_id,
        display_dir,
        current_dir: SourcePath::Local(dir),
        parent,
        entries: dirs,
        error: None,
    }
}

/// Header text for a remote directory: `root_name/rel/` inside the root,
/// otherwise the full remote path.
pub fn remote_display_dir(root: &str, dir: &str) -> String {
    let root_trimmed = root.trim_end_matches('/');
    let root_name = root_trimmed.rsplit('/').next().unwrap_or(root_trimmed);
    match dir.strip_prefix(root_trimmed) {
        Some("") => format!("{root_name}/"),
        Some(rel) => format!("{root_name}/{}/", rel.trim_start_matches('/')),
        None => format!("{dir}/"),
    }
}

async fn remote_list_dir<H: SourceHost>(
    tab_id: usize,
    client: H::ClientConfig,
    workspace_id: String,
    root: String,
    dir: String,
    show_hidden: bool,
) -> SourceDirListing {
    match H::backend(client)
        .list_dir(workspace_id, root.clone(), dir.clone(), show_hidden)
        .await
    {
        Ok(directory) => {
            // The agent canonicalizes and confines paths; its answer is
            // authoritative for both current_dir and root.
            let parent = remote_parent_within_root(&directory.current_dir, &directory.root)
                .map(SourcePath::Remote);
            SourceDirListing {
                tab_id,
                display_dir: remote_display_dir(&directory.root, &directory.current_dir),
                current_dir: SourcePath::Remote(directory.current_dir),
                parent,
                entries: directory
                    .entries
                    .into_iter()
                    .map(|entry| SourceDirEntry {
                        name: entry.name,
                        path: SourcePath::Remote(entry.path),
                        is_dir: entry.is_dir,
                        is_git_repo: entry.is_git_repo,
                    })
                    .collect(),
                error: None,
            }
        }
        Err(err) => SourceDirListing {
            tab_id,
            display_dir: remote_display_dir(&root, &dir),
            current_dir: SourcePath::Remote(dir),
            parent: Some(SourcePath::Remote(root)),
            entries: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

/// Parent of a canonical remote directory, staying at or under the canonical
/// root. Both inputs come from the same agent response, so plain string
/// handling is sound here — this is the one place remote path structure is
/// interpreted on the desktop.
pub fn remote_parent_within_root(current_dir: &str, root: &str) -> Option<String> {
    if current_dir == root {
        return None;
    }
    let trimmed = current_dir.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    let parent = if parent.is_empty() { "/" } else { parent };
    if parent.len() < root.trim_end_matches('/').len() {
        return None;
    }
    Some(parent.to_string())
}

fn source_git_file(status: crate::status::GitFileStatus) -> SourceGitFile {
    SourceGitFile {
        path: status.path,
        status: status.status,
        is_staged: status.is_staged,
    }
}
