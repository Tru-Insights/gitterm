//! Host facade for the shared local/remote source adapters.
use gitterm::agentd::client::{RemoteAgentBackend, RemoteAgentClientConfig};
pub use gitterm_core::content::source::*;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DesktopSourceHost;
impl gitterm_core::remote::source::SourceHost for DesktopSourceHost {
    type ClientConfig = RemoteAgentClientConfig;
    fn backend(config: Self::ClientConfig) -> gitterm_core::remote::client::RemoteAgentBackend {
        RemoteAgentBackend::new(config).into_core()
    }
    fn local_display_dir(root: &std::path::Path, dir: &std::path::Path) -> String {
        local_display_dir(root, dir)
    }
}
pub type WorkspaceSource = gitterm_core::remote::source::WorkspaceSource<DesktopSourceHost>;

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

#[cfg(test)]
use gitterm_core::remote::source::{remote_display_dir, remote_parent_within_root};
#[cfg(test)]
fn local_list_dir(
    tab_id: usize,
    root: &std::path::Path,
    dir: PathBuf,
    show_hidden: bool,
) -> SourceDirListing {
    gitterm_core::remote::source::local_list_dir::<DesktopSourceHost>(
        tab_id,
        root,
        dir,
        show_hidden,
    )
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // The test interceptor uses tonic's direct `Status` error contract.
mod tests {
    use super::*;
    use gitterm::agentd::server::{is_authorized_metadata, GitTermAgentService};
    use std::sync::Arc;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;

    /// End-to-end: a real gitterm-v6-agent gRPC server, driven through
    /// WorkspaceSource exactly as the Files UI drives it — listing, entry
    /// paths, parent computation, and navigation into a subdirectory.
    #[tokio::test]
    async fn remote_source_lists_and_navigates_against_real_agent() {
        use gitterm::agentd::protocol::v6::git_term_agent_server::GitTermAgentServer;

        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src").join("main.rs"), "fn main() {}").unwrap();
        // Hidden from the listing, but marks src/ as a repo over the wire.
        std::fs::create_dir(repo.path().join("src").join(".git")).unwrap();
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
        assert!(listing.entries[0].is_git_repo);
        assert!(!listing.entries[1].is_git_repo);

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

        // Read a file through the source, as the viewer pipeline does.
        let file_path = src_listing.entries[0].path.clone();
        let content = source.read_file(&file_path, 1_000_000).await.unwrap();
        assert_eq!(content.data, b"fn main() {}");
        assert_eq!(content.total_size, 12);
        assert!(!content.truncated);

        // A directory outside the root is refused by the agent and surfaces
        // as a visible error with a recovery parent, not a silent fallback.
        let outside = source
            .list_dir(7, SourcePath::Remote("/".to_string()), false)
            .await;
        assert!(outside.error.is_some());
        assert_eq!(outside.parent, Some(SourcePath::Remote(canonical_root)));

        server.abort();
    }

    #[tokio::test]
    async fn local_source_reads_files_with_truncation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("big.txt");
        std::fs::write(&path, vec![b'y'; 500]).unwrap();
        let source = WorkspaceSource::Local {
            root: root.path().to_path_buf(),
        };

        let full = source
            .read_file(&SourcePath::Local(path.clone()), 1_000)
            .await
            .unwrap();
        assert_eq!(full.data.len(), 500);
        assert!(!full.truncated);

        let capped = source
            .read_file(&SourcePath::Local(path), 100)
            .await
            .unwrap();
        assert_eq!(capped.data.len(), 100);
        assert_eq!(capped.total_size, 500);
        assert!(capped.truncated);
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
    fn local_listing_marks_git_repos() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("repo").join(".git")).unwrap();
        std::fs::create_dir(root.path().join("worktree")).unwrap();
        std::fs::write(root.path().join("worktree").join(".git"), "gitdir: x").unwrap();
        std::fs::create_dir(root.path().join("plain")).unwrap();

        let listing = local_list_dir(1, root.path(), root.path().to_path_buf(), false);
        let marks: Vec<(String, bool)> = listing
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.is_git_repo))
            .collect();
        assert_eq!(
            marks,
            vec![
                ("plain".to_string(), false),
                ("repo".to_string(), true),
                ("worktree".to_string(), true),
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

#[cfg(test)]
mod baseline_tests {
    use super::*;

    #[tokio::test]
    async fn opaque_remote_status_error_baseline() {
        // A deterministic backend failure before any connection or filesystem access.
        for (root, expected) in [
            (
                "Q:/opaque/repo",
                "remote git status for Q:/opaque/repo: token_ref must not be empty",
            ),
            (
                "/opaque/../repo",
                "remote git status for /opaque/../repo: token_ref must not be empty",
            ),
        ] {
            let source = WorkspaceSource::RemoteAgent {
                workspace_id: "fixture-workspace".into(),
                root: root.into(),
                client: RemoteAgentClientConfig {
                    remote_id: "fixture-remote".into(),
                    name: "Fixture".into(),
                    endpoint: "http://fixture.invalid:1".into(),
                    token_ref: String::new(),
                },
            };
            assert_eq!(source.root(), SourcePath::Remote(root.into()));
            assert!(source.capabilities().git_status);
            assert!(!source.capabilities().git_worktrees);
            assert_eq!(source.git_status().await.unwrap_err(), expected);
        }
    }
}

#[cfg(test)]
mod capability_boundary_tests {
    use super::*;

    #[tokio::test]
    async fn owns_is_variant_only_and_local_reads_and_listings_are_not_root_confined() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        std::fs::write(&path, "outside").unwrap();
        let local = WorkspaceSource::Local {
            root: root.path().into(),
        };
        assert!(local.owns(&SourcePath::Local(path.clone())));
        assert!(!local.owns(&SourcePath::Remote(path.display().to_string())));
        let content = local
            .read_file(&SourcePath::Local(path), 100)
            .await
            .unwrap();
        assert_eq!(content.data, b"outside");
        let listing = local
            .list_dir(1, SourcePath::Local(outside.path().into()), false)
            .await;
        assert_eq!(listing.error, None);
        assert_eq!(listing.entries.len(), 1);
        let remote = WorkspaceSource::RemoteAgent {
            workspace_id: "fixture-workspace".into(),
            root: "/fixture/root".into(),
            client: RemoteAgentClientConfig {
                remote_id: "fixture".into(),
                name: "Fixture".into(),
                endpoint: "http://fixture.invalid:1".into(),
                token_ref: String::new(),
            },
        };
        assert!(remote.owns(&SourcePath::Remote("/different/workspace/file".into())));
        assert!(!remote.owns(&SourcePath::Local(root.path().into())));
        assert!(local.capabilities().edit_file);
        assert!(!remote.capabilities().edit_file);
        assert!(remote.capabilities().open_file);
    }
}
