// Markdown document viewer warp routes.
//
// Serves:
//   GET /plans               -> JSON { plans: [{ name, title }, ...] }
//   GET /plans/raw?path=foo.md -> raw markdown bytes
//   GET /plans/viewer        -> embedded HTML viewer (reads ?plan= and ?theme=)
//   GET /docs                -> JSON { plans: [{ name, title }, ...] }
//   GET /docs/raw?path=foo.md  -> raw markdown bytes
//   GET /docs/viewer         -> embedded HTML viewer (reads ?plan= and ?theme=)
//
// Source-of-truth invariant: this module reads `.md` files only. It never
// writes. Path traversal is rejected before any filesystem access.
//
// Directories come from `DocumentRoots`, which the app pushes to whenever the
// active workspace changes. If unset, list+raw return empty / 404 — we never
// fall back to cwd because cwd is `/` for a bundled .app.

#[cfg(feature = "runtime")]
use std::collections::HashMap;
#[cfg(feature = "runtime")]
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;
#[cfg(feature = "runtime")]
use warp::filters::BoxedFilter;
#[cfg(feature = "runtime")]
use warp::{Filter, Rejection};

use std::sync::{Arc, RwLock};

/// Shared roots are read for every request, including workspace switches.
#[derive(Clone)]
pub struct DocumentRoots {
    pub plans_dir: Arc<RwLock<Option<PathBuf>>>,
    pub docs_dir: Arc<RwLock<Option<PathBuf>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentSource {
    Plans,
    Docs,
}

impl DocumentSource {
    pub fn route_segment(self) -> &'static str {
        match self {
            Self::Plans => "plans",
            Self::Docs => "docs",
        }
    }
}

#[derive(Serialize)]
pub struct PlanEntry {
    pub name: String,
    pub title: String,
}

#[derive(Serialize)]
pub struct PlansList {
    pub plans: Vec<PlanEntry>,
}

#[cfg(feature = "runtime")]
fn source_dir(state: &DocumentRoots, source: DocumentSource) -> Option<PathBuf> {
    match source {
        DocumentSource::Plans => state.plans_dir.read().ok().and_then(|p| p.clone()),
        DocumentSource::Docs => state.docs_dir.read().ok().and_then(|p| p.clone()),
    }
}

pub fn is_safe_relative_md_path(path: &str) -> bool {
    !path.is_empty()
        && path.ends_with(".md")
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment != "_archive"
                && !segment.starts_with('.')
                && !segment.contains('\\')
        })
}

/// Failure categories preserve the raw routes' existing status/body contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentError {
    InvalidPath,
    RootMissing,
    NotFound,
}

impl std::fmt::Display for ContainmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "invalid document path",
            Self::RootMissing => "document dir missing",
            Self::NotFound => "document not found",
        })
    }
}
impl std::error::Error for ContainmentError {}

/// Resolve an existing candidate beneath a captured root, including symlink containment.
/// This accepts filesystem paths, not URLs: percent escapes are never decoded.
/// This extracts the raw routes' canonical check unchanged. Callers must apply
/// their lexical policy first: plans use `is_safe_relative_md_path`, while the
/// root-bound content reader requires a nonempty relative path without parents.
/// Like the raw routes, this checks canonical paths at resolution time; it does
/// not defend against a concurrent filesystem writer replacing path components.
pub fn resolve_contained(root: &Path, relative: &Path) -> Result<PathBuf, ContainmentError> {
    let canon_root = root
        .canonicalize()
        .map_err(|_| ContainmentError::RootMissing)?;
    let canon_path = root
        .join(relative)
        .canonicalize()
        .map_err(|_| ContainmentError::NotFound)?;
    if !canon_path.starts_with(&canon_root) {
        return Err(ContainmentError::InvalidPath);
    }
    Ok(canon_path)
}

pub fn extract_title(md: &str, fallback: &str) -> String {
    for line in md.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("# ") {
            return rest.trim().to_string();
        }
    }
    fallback.trim_end_matches(".md").to_string()
}

#[cfg(feature = "runtime")]
const VIEWER_HTML: &str = include_str!("../../../assets/plans-viewer.html");

#[cfg(feature = "runtime")]
fn collect_markdown_entries(root: &Path, dir: &Path, entries: &mut Vec<PlanEntry>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') || file_name == "_archive" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_markdown_entries(root, &path, entries);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };
        let name = rel_path.to_string_lossy().replace('\\', "/");
        if !is_safe_relative_md_path(&name) {
            continue;
        }
        let content = fs::read_to_string(&path).unwrap_or_default();
        let title = extract_title(&content, &name);
        entries.push(PlanEntry { name, title });
    }
}

/// The same read-only inventory used by the HTTP routes and embedded desktop viewer.
#[cfg(feature = "runtime")]
pub fn list_documents(dir: &Path) -> PlansList {
    let mut entries = Vec::new();
    collect_markdown_entries(dir, dir, &mut entries);
    entries.sort_by_key(|a| a.title.to_lowercase());
    PlansList { plans: entries }
}

#[cfg(feature = "runtime")]
pub fn routes(state: DocumentRoots) -> BoxedFilter<(warp::reply::Response,)> {
    let with_state = warp::any().map(move || state.clone());

    let list_plans = warp::path!("plans")
        .and(warp::get())
        .map(|| DocumentSource::Plans)
        .and(with_state.clone())
        .and_then(handle_list);

    let raw_plans = warp::path!("plans" / "raw")
        .and(warp::get())
        .map(|| DocumentSource::Plans)
        .and(warp::query::<HashMap<String, String>>())
        .and(with_state.clone())
        .and_then(handle_raw);

    let viewer_plans = warp::path!("plans" / "viewer")
        .and(warp::get())
        .and_then(handle_viewer);

    let list_docs = warp::path!("docs")
        .and(warp::get())
        .map(|| DocumentSource::Docs)
        .and(with_state.clone())
        .and_then(handle_list);

    let raw_docs = warp::path!("docs" / "raw")
        .and(warp::get())
        .map(|| DocumentSource::Docs)
        .and(warp::query::<HashMap<String, String>>())
        .and(with_state)
        .and_then(handle_raw);

    let viewer_docs = warp::path!("docs" / "viewer")
        .and(warp::get())
        .and_then(handle_viewer);

    list_plans
        .or(viewer_plans)
        .unify()
        .or(raw_plans)
        .unify()
        .or(list_docs)
        .unify()
        .or(viewer_docs)
        .unify()
        .or(raw_docs)
        .unify()
        .boxed()
}

#[cfg(feature = "runtime")]
async fn handle_viewer() -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    Ok(warp::reply::with_header(
        VIEWER_HTML.replacen(
            crate::content::bar_hint::BAR_HINT_SLOT,
            &crate::content::bar_hint::bar_hint_script("var(--text)", "var(--bg)"),
            1,
        ),
        "content-type",
        "text/html; charset=utf-8",
    )
    .into_response())
}

#[cfg(feature = "runtime")]
async fn handle_list(
    source: DocumentSource,
    state: DocumentRoots,
) -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    let Some(dir) = source_dir(&state, source) else {
        return Ok(warp::reply::json(&PlansList { plans: Vec::new() }).into_response());
    };
    Ok(warp::reply::json(&list_documents(&dir)).into_response())
}

#[cfg(feature = "runtime")]
async fn handle_raw(
    source: DocumentSource,
    query: HashMap<String, String>,
    state: DocumentRoots,
) -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    let Some(path) = query.get("path") else {
        return Ok(warp::reply::with_status(
            "missing document path",
            warp::http::StatusCode::BAD_REQUEST,
        )
        .into_response());
    };
    if !is_safe_relative_md_path(path) {
        return Ok(warp::reply::with_status(
            "invalid document path",
            warp::http::StatusCode::BAD_REQUEST,
        )
        .into_response());
    }
    let Some(dir) = source_dir(&state, source) else {
        return Ok(warp::reply::with_status(
            "no active document dir",
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response());
    };
    let canon_path = match resolve_contained(&dir, Path::new(path)) {
        Ok(path) => path,
        Err(error) => {
            let status = match error {
                ContainmentError::InvalidPath => warp::http::StatusCode::BAD_REQUEST,
                ContainmentError::RootMissing | ContainmentError::NotFound => {
                    warp::http::StatusCode::NOT_FOUND
                }
            };
            return Ok(warp::reply::with_status(error.to_string(), status).into_response());
        }
    };
    match fs::read(&canon_path) {
        Ok(bytes) => {
            Ok(
                warp::reply::with_header(bytes, "content-type", "text/markdown; charset=utf-8")
                    .into_response(),
            )
        }
        Err(_) => Ok(warp::reply::with_status(
            "document not found",
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_relative_md_path_accepts_normal_md() {
        assert!(is_safe_relative_md_path("foo.md"));
        assert!(is_safe_relative_md_path("agent-tab-integration.md"));
    }

    #[test]
    fn safe_relative_md_path_accepts_nested_md() {
        assert!(is_safe_relative_md_path("guides/setup.md"));
        assert!(is_safe_relative_md_path("guides/deep/setup.md"));
    }

    #[test]
    fn safe_relative_md_path_rejects_traversal() {
        assert!(!is_safe_relative_md_path("../foo.md"));
        assert!(!is_safe_relative_md_path("foo/../bar.md"));
        assert!(!is_safe_relative_md_path("/foo.md"));
        assert!(!is_safe_relative_md_path("foo\\bar.md"));
        assert!(!is_safe_relative_md_path(".hidden.md"));
        assert!(!is_safe_relative_md_path("foo/.hidden.md"));
        assert!(!is_safe_relative_md_path("_archive/old.md"));
        assert!(!is_safe_relative_md_path("foo/_archive/old.md"));
        assert!(!is_safe_relative_md_path("foo//bar.md"));
        assert!(!is_safe_relative_md_path(""));
    }

    #[test]
    fn safe_relative_md_path_requires_md_suffix() {
        assert!(!is_safe_relative_md_path("foo"));
        assert!(!is_safe_relative_md_path("foo.txt"));
        assert!(!is_safe_relative_md_path("foo.md.exe"));
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn collect_markdown_entries_walks_nested_md_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("root.md"), "# Root\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "# Ignore\n").unwrap();

        let nested_dir = dir.path().join("guides").join("deep");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("setup.md"), "# Setup\n").unwrap();

        let hidden_dir = dir.path().join(".hidden");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(hidden_dir.join("ignored.md"), "# Hidden\n").unwrap();

        let archive_dir = dir.path().join("_archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join("old.md"), "# Old\n").unwrap();

        let mut entries = Vec::new();
        collect_markdown_entries(dir.path(), dir.path(), &mut entries);
        entries.sort_by_key(|entry| entry.name.clone());

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        let titles: Vec<_> = entries.iter().map(|entry| entry.title.as_str()).collect();
        assert_eq!(names, vec!["guides/deep/setup.md", "root.md"]);
        assert_eq!(titles, vec!["Setup", "Root"]);
    }

    #[test]
    fn extract_title_finds_first_h1() {
        let md = "Some text\n# The Title\n## Sub\nbody";
        assert_eq!(extract_title(md, "fallback.md"), "The Title");
    }

    #[test]
    fn extract_title_falls_back_to_filename() {
        let md = "no heading here\njust text";
        assert_eq!(extract_title(md, "my-plan.md"), "my-plan");
    }

    #[test]
    fn extract_title_ignores_h2_and_lower() {
        let md = "## Not the title\n# Actual Title";
        assert_eq!(extract_title(md, "x.md"), "Actual Title");
    }
}

#[cfg(all(test, feature = "runtime"))]
mod route_tests {
    use super::*;
    use warp::http::StatusCode;

    fn roots(plans: Option<PathBuf>, docs: Option<PathBuf>) -> DocumentRoots {
        DocumentRoots {
            plans_dir: Arc::new(RwLock::new(plans)),
            docs_dir: Arc::new(RwLock::new(docs)),
        }
    }

    async fn assert_document(
        filter: &BoxedFilter<(warp::reply::Response,)>,
        segment: &str,
        content: Option<&str>,
    ) {
        let list = warp::test::request()
            .path(&format!("/{segment}"))
            .reply(filter)
            .await;
        assert_eq!(list.status(), StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(list.body()).unwrap();
        let raw = warp::test::request()
            .path(&format!("/{segment}/raw?path=guide.md"))
            .reply(filter)
            .await;
        if let Some(content) = content {
            assert_eq!(
                list,
                serde_json::json!({"plans":[{"name":"guide.md", "title":content}]})
            );
            assert_eq!(raw.status(), StatusCode::OK);
            assert_eq!(raw.body().as_ref(), format!("# {content}").as_bytes());
        } else {
            assert_eq!(list, serde_json::json!({"plans":[]}));
            assert_eq!(raw.status(), StatusCode::NOT_FOUND);
            assert_eq!(raw.body(), "no active document dir");
        }
    }

    #[tokio::test]
    async fn one_filter_tracks_independent_plans_and_docs_a_b_none_switches() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::write(a.path().join("guide.md"), "# A").unwrap();
        fs::write(b.path().join("guide.md"), "# B").unwrap();
        let state = roots(Some(a.path().into()), Some(b.path().into()));
        let filter = routes(state.clone());
        assert_document(&filter, "plans", Some("A")).await;
        assert_document(&filter, "docs", Some("B")).await;
        *state.plans_dir.write().unwrap() = Some(b.path().into());
        assert_document(&filter, "plans", Some("B")).await;
        assert_document(&filter, "docs", Some("B")).await;
        *state.plans_dir.write().unwrap() = None;
        assert_document(&filter, "plans", None).await;
        assert_document(&filter, "docs", Some("B")).await;
        *state.docs_dir.write().unwrap() = Some(a.path().into());
        assert_document(&filter, "docs", Some("A")).await;
        assert_document(&filter, "plans", None).await;
        *state.docs_dir.write().unwrap() = Some(b.path().into());
        assert_document(&filter, "docs", Some("B")).await;
        *state.docs_dir.write().unwrap() = None;
        assert_document(&filter, "docs", None).await;
    }

    #[tokio::test]
    async fn route_validation_and_missing_roots_preserve_exact_statuses() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/guide.md"), "# Nested").unwrap();
        let state = roots(Some(dir.path().into()), Some(dir.path().into()));
        let filter = routes(state.clone());
        for segment in ["plans", "docs"] {
            for (query, status, body) in [
                ("", StatusCode::BAD_REQUEST, "missing document path"),
                (
                    "?path=%2e%2e%2foutside.md",
                    StatusCode::BAD_REQUEST,
                    "invalid document path",
                ),
                (
                    "?path=nested%2f..%2fguide.md",
                    StatusCode::BAD_REQUEST,
                    "invalid document path",
                ),
                (
                    "?path=%5coutside.md",
                    StatusCode::BAD_REQUEST,
                    "invalid document path",
                ),
                (
                    "?path=_archive/old.md",
                    StatusCode::BAD_REQUEST,
                    "invalid document path",
                ),
                (
                    "?path=.hidden.md",
                    StatusCode::BAD_REQUEST,
                    "invalid document path",
                ),
                (
                    "?path=file.txt",
                    StatusCode::BAD_REQUEST,
                    "invalid document path",
                ),
                (
                    "?path=missing.md",
                    StatusCode::NOT_FOUND,
                    "document not found",
                ),
                ("?path=nested/guide.md", StatusCode::OK, "# Nested"),
            ] {
                let reply = warp::test::request()
                    .path(&format!("/{segment}/raw{query}"))
                    .reply(&filter)
                    .await;
                assert_eq!(reply.status(), status, "{segment}{query}");
                assert_eq!(reply.body(), body, "{segment}{query}");
            }
            let reply = warp::test::request()
                .path(&format!("/{segment}/viewer"))
                .reply(&filter)
                .await;
            assert_eq!(reply.status(), StatusCode::OK);
            assert_eq!(reply.headers()["content-type"], "text/html; charset=utf-8");
            assert!(!std::str::from_utf8(reply.body())
                .unwrap()
                .contains(crate::content::bar_hint::BAR_HINT_SLOT));
        }
        for handle in [&state.plans_dir, &state.docs_dir] {
            *handle.write().unwrap() = Some(dir.path().join("missing-root"));
        }
        for segment in ["plans", "docs"] {
            let reply = warp::test::request()
                .path(&format!("/{segment}/raw?path=guide.md"))
                .reply(&filter)
                .await;
            assert_eq!(reply.status(), StatusCode::NOT_FOUND);
            assert_eq!(reply.body(), "document dir missing");
            let reply = warp::test::request()
                .path(&format!("/{segment}"))
                .reply(&filter)
                .await;
            assert_eq!(reply.status(), StatusCode::OK);
            assert_eq!(reply.body(), r#"{"plans":[]}"#);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_symlinks_are_excluded_from_listing_and_refused_by_raw_routes() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "# Outside").unwrap();
        fs::write(dir.path().join("good.md"), "# Good").unwrap();
        symlink(outside.path().join("secret.md"), dir.path().join("file.md")).unwrap();
        symlink(outside.path(), dir.path().join("directory")).unwrap();
        // Internal file links are also absent from listings but remain readable.
        symlink(dir.path().join("good.md"), dir.path().join("internal.md")).unwrap();
        let filter = routes(roots(Some(dir.path().into()), Some(dir.path().into())));
        for segment in ["plans", "docs"] {
            let list = warp::test::request()
                .path(&format!("/{segment}"))
                .reply(&filter)
                .await;
            assert_eq!(list.status(), StatusCode::OK);
            assert_eq!(
                list.body(),
                r#"{"plans":[{"name":"good.md","title":"Good"}]}"#
            );
            for path in ["file.md", "directory/secret.md"] {
                let reply = warp::test::request()
                    .path(&format!("/{segment}/raw?path={path}"))
                    .reply(&filter)
                    .await;
                assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
                assert_eq!(reply.body(), "invalid document path");
            }
            let reply = warp::test::request()
                .path(&format!("/{segment}/raw?path=internal.md"))
                .reply(&filter)
                .await;
            assert_eq!(reply.status(), StatusCode::OK);
            assert_eq!(reply.body(), "# Good");
        }
    }
}
