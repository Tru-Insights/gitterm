//! Workspace sources.
//!
//! A workspace is backed by a source: the local filesystem or a remote
//! `gitterm-agent`. UI code (Files, Git, Plans, agent launchers) talks to
//! `WorkspaceSource` and renders what it returns. It must never branch on
//! which kind of source it has — a missing feature is expressed through
//! [`SourceCapabilities`], not location checks.
//!
//! Paths are opaque to the UI: it receives [`SourcePath`] tokens and hands
//! them back unchanged. All path semantics (parents, root confinement,
//! canonical form) live inside the source implementations. For remote
//! sources the agent's canonicalized paths are authoritative.

use std::path::PathBuf;

use gitterm::agentd::client::{RemoteAgentBackend, RemoteAgentClientConfig};

/// An opaque location token. UI code may display it and pass it back to the
/// source that produced it, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcePath {
    Local(PathBuf),
    Remote(String),
}

impl SourcePath {
    /// Human-readable form for headers/breadcrumbs and persistence.
    pub fn display(&self) -> String {
        match self {
            SourcePath::Local(path) => path.to_string_lossy().to_string(),
            SourcePath::Remote(path) => path.clone(),
        }
    }

    /// The local path, if this token came from a local source. Boundary code
    /// (file open/edit pipelines) uses this to enter local-only flows; UI
    /// rendering code should not.
    pub fn as_local(&self) -> Option<&std::path::Path> {
        match self {
            SourcePath::Local(path) => Some(path),
            SourcePath::Remote(_) => None,
        }
    }
}

/// What a source can do today. The UI gates affordances (open, edit, git
/// panel, session launchers) on these flags — never on the source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub open_file: bool,
    pub edit_file: bool,
    pub git_status: bool,
    pub sessions: bool,
}

impl SourceCapabilities {
    /// No capabilities — used when a workspace has no usable source (e.g.
    /// the legacy SSH prototype).
    pub fn none() -> Self {
        Self {
            open_file: false,
            edit_file: false,
            git_status: false,
            sessions: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDirEntry {
    pub name: String,
    pub path: SourcePath,
    pub is_dir: bool,
}

/// One directory listing, shaped identically for every source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDirListing {
    pub tab_id: usize,
    pub current_dir: SourcePath,
    /// Header/breadcrumb text, already formatted by the source.
    pub display_dir: String,
    /// `None` at the workspace root — drives the Up button with no path
    /// logic in the UI.
    pub parent: Option<SourcePath>,
    pub entries: Vec<SourceDirEntry>,
    pub error: Option<String>,
}

/// File-browser state for one tab, fed exclusively by [`WorkspaceSource`]
/// listings. The `seq` counter drops stale responses after rapid navigation
/// without comparing paths.
#[derive(Debug, Clone)]
pub struct FilesState {
    pub dir: SourcePath,
    pub display_dir: String,
    pub parent: Option<SourcePath>,
    pub entries: Vec<SourceDirEntry>,
    pub loading: bool,
    pub error: Option<String>,
    pub seq: u64,
}

impl FilesState {
    pub fn at(dir: SourcePath) -> Self {
        Self {
            display_dir: format!("{}/", dir.display()),
            dir,
            parent: None,
            entries: Vec::new(),
            loading: false,
            error: None,
            seq: 0,
        }
    }

    /// Point the browser at `dir` and mark it loading. Returns the request
    /// sequence number to thread through to [`FilesState::apply`].
    pub fn begin_request(&mut self, dir: SourcePath) -> u64 {
        self.dir = dir;
        self.loading = true;
        self.error = None;
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Apply a completed listing; ignored if a newer request superseded it.
    pub fn apply(&mut self, seq: u64, listing: SourceDirListing) {
        if seq != self.seq {
            return;
        }
        self.loading = false;
        self.dir = listing.current_dir;
        self.display_dir = listing.display_dir;
        self.parent = listing.parent;
        self.entries = listing.entries;
        self.error = listing.error;
    }
}

/// A workspace's source. Cheap to clone; async calls own everything they
/// need so they can run on any task.
#[derive(Debug, Clone)]
pub enum WorkspaceSource {
    Local {
        root: PathBuf,
    },
    RemoteAgent {
        workspace_id: String,
        root: String,
        client: RemoteAgentClientConfig,
    },
}

impl WorkspaceSource {
    pub fn capabilities(&self) -> SourceCapabilities {
        match self {
            WorkspaceSource::Local { .. } => SourceCapabilities {
                open_file: true,
                edit_file: true,
                git_status: true,
                sessions: true,
            },
            // Grows as the agent gains ReadFile (open), write ops (edit),
            // GitStatus, and the session runtime. Flipping a flag here is
            // the only change the UI needs.
            WorkspaceSource::RemoteAgent { .. } => SourceCapabilities {
                open_file: false,
                edit_file: false,
                git_status: false,
                sessions: false,
            },
        }
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
                    local_list_dir(tab_id, &root, dir, show_hidden)
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
                remote_list_dir(
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
}

fn local_list_dir(
    tab_id: usize,
    root: &std::path::Path,
    dir: PathBuf,
    show_hidden: bool,
) -> SourceDirListing {
    let parent = dir
        .parent()
        .filter(|p| p.starts_with(root))
        .map(|p| SourcePath::Local(p.to_path_buf()));
    let display_dir = local_display_dir(root, &dir);

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

/// Header text for a local directory: `root_name/rel/` inside the root,
/// `~/rel/` under the home directory, otherwise the absolute path.
fn local_display_dir(root: &std::path::Path, dir: &std::path::Path) -> String {
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string());
    if let Ok(rel) = dir.strip_prefix(root) {
        if rel.as_os_str().is_empty() {
            format!("{root_name}/")
        } else {
            format!("{root_name}/{}/", rel.display())
        }
    } else if let Some(rel) = dirs::home_dir().and_then(|home| dir.strip_prefix(home).ok()) {
        format!("~/{}/", rel.display())
    } else {
        format!("{}/", dir.display())
    }
}

/// Header text for a remote directory: `root_name/rel/` inside the root,
/// otherwise the full remote path.
fn remote_display_dir(root: &str, dir: &str) -> String {
    let root_trimmed = root.trim_end_matches('/');
    let root_name = root_trimmed.rsplit('/').next().unwrap_or(root_trimmed);
    match dir.strip_prefix(root_trimmed) {
        Some("") => format!("{root_name}/"),
        Some(rel) => format!("{root_name}/{}/", rel.trim_start_matches('/')),
        None => format!("{dir}/"),
    }
}

async fn remote_list_dir(
    tab_id: usize,
    client: RemoteAgentClientConfig,
    workspace_id: String,
    root: String,
    dir: String,
    show_hidden: bool,
) -> SourceDirListing {
    match RemoteAgentBackend::new(client)
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
fn remote_parent_within_root(current_dir: &str, root: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm::agentd::server::{is_authorized_metadata, GitTermAgentService};
    use std::sync::Arc;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;

    /// End-to-end: a real gitterm-agent gRPC server, driven through
    /// WorkspaceSource exactly as the Files UI drives it — listing, entry
    /// paths, parent computation, and navigation into a subdirectory.
    #[tokio::test]
    async fn remote_source_lists_and_navigates_against_real_agent() {
        use gitterm::agentd::protocol::v1::git_term_agent_server::GitTermAgentServer;

        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(repo.path().join("Cargo.toml"), "[package]\n").unwrap();
        let canonical_root = std::fs::canonicalize(repo.path())
            .unwrap()
            .to_string_lossy()
            .to_string();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_token = Arc::new("source-e2e".to_string());
        let service = GitTermAgentServer::with_interceptor(
            GitTermAgentService::new("e2e-agent".to_string()),
            move |request: tonic::Request<()>| {
                if is_authorized_metadata(request.metadata(), expected_token.as_str()) {
                    Ok(request)
                } else {
                    Err(tonic::Status::unauthenticated("bad token"))
                }
            },
        );
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        std::env::set_var("GITTERM_SOURCE_E2E_TOKEN", "source-e2e");
        let source = WorkspaceSource::RemoteAgent {
            workspace_id: "ws".to_string(),
            root: canonical_root.clone(),
            client: RemoteAgentClientConfig {
                remote_id: "e2e".to_string(),
                name: "e2e".to_string(),
                endpoint: format!("http://{addr}"),
                token_ref: "env:GITTERM_SOURCE_E2E_TOKEN".to_string(),
            },
        };

        // Root listing: dirs before files, no parent at root.
        let listing = source.list_dir(7, source.root(), false).await;
        assert_eq!(listing.error, None);
        assert_eq!(listing.parent, None);
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "Cargo.toml"]);

        // Navigate into src/ using the opaque entry path, as the UI does.
        let src_path = listing.entries[0].path.clone();
        let src_listing = source.list_dir(7, src_path, false).await;
        assert_eq!(src_listing.error, None);
        assert_eq!(
            src_listing.parent,
            Some(SourcePath::Remote(canonical_root.clone()))
        );
        assert!(src_listing.display_dir.ends_with("/src/"));
        let names: Vec<&str> = src_listing
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["main.rs"]);

        // A directory outside the root is refused by the agent and surfaces
        // as a visible error with a recovery parent, not a silent fallback.
        let outside = source
            .list_dir(7, SourcePath::Remote("/".to_string()), false)
            .await;
        assert!(outside.error.is_some());
        assert_eq!(outside.parent, Some(SourcePath::Remote(canonical_root)));

        server.abort();
    }

    #[test]
    fn remote_display_dir_is_root_relative() {
        assert_eq!(remote_display_dir("/home/u/repo", "/home/u/repo"), "repo/");
        assert_eq!(
            remote_display_dir("/home/u/repo", "/home/u/repo/src/tab"),
            "repo/src/tab/"
        );
        assert_eq!(
            remote_display_dir("/home/u/repo", "/elsewhere"),
            "/elsewhere/"
        );
    }

    #[test]
    fn remote_parent_stops_at_root() {
        assert_eq!(
            remote_parent_within_root("/home/user/repo/src", "/home/user/repo"),
            Some("/home/user/repo".to_string())
        );
        assert_eq!(
            remote_parent_within_root("/home/user/repo", "/home/user/repo"),
            None
        );
        assert_eq!(
            remote_parent_within_root("/home/user", "/home/user/repo"),
            None
        );
    }

    #[test]
    fn local_listing_reports_parent_within_root_only() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let at_root = local_list_dir(1, root.path(), root.path().to_path_buf(), false);
        assert_eq!(at_root.parent, None);
        assert!(at_root.error.is_none());

        let in_sub = local_list_dir(1, root.path(), sub, false);
        assert_eq!(
            in_sub.parent,
            Some(SourcePath::Local(root.path().to_path_buf()))
        );
    }

    #[test]
    fn local_listing_sorts_dirs_before_files_and_filters_hidden() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("z-dir")).unwrap();
        std::fs::create_dir(root.path().join("Alpha")).unwrap();
        std::fs::create_dir(root.path().join("node_modules")).unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("beta.txt"), "b").unwrap();
        std::fs::write(root.path().join("a.txt"), "a").unwrap();
        std::fs::write(root.path().join(".hidden"), "h").unwrap();

        let listing = local_list_dir(1, root.path(), root.path().to_path_buf(), false);
        let names: Vec<(String, bool)> = listing
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.is_dir))
            .collect();
        // Dirs first (case-insensitive sort), then files; node_modules,
        // target, and dotfiles filtered.
        assert_eq!(
            names,
            vec![
                ("Alpha".to_string(), true),
                ("z-dir".to_string(), true),
                ("a.txt".to_string(), false),
                ("beta.txt".to_string(), false),
            ]
        );
    }

    #[test]
    fn local_listing_shows_dotfiles_when_enabled() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".hidden"), "h").unwrap();

        let listing = local_list_dir(1, root.path(), root.path().to_path_buf(), true);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, ".hidden");
    }

    #[test]
    fn local_listing_of_empty_dir_is_empty_without_error() {
        let root = tempfile::tempdir().unwrap();
        let listing = local_list_dir(1, root.path(), root.path().to_path_buf(), false);
        assert!(listing.entries.is_empty());
        assert!(listing.error.is_none());
    }

    #[test]
    fn local_listing_of_unreadable_dir_reports_error() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let listing = local_list_dir(1, root.path(), missing, false);
        assert!(listing.entries.is_empty());
        assert!(listing.error.is_some());
    }

    #[tokio::test]
    async fn mismatched_source_path_is_a_loud_error() {
        let source = WorkspaceSource::Local {
            root: PathBuf::from("/tmp"),
        };
        let listing = source
            .list_dir(1, SourcePath::Remote("/elsewhere".to_string()), false)
            .await;
        assert!(listing.error.is_some());
    }
}
