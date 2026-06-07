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
// Directories come from `ServerState`, which the app pushes to whenever the
// active workspace changes. If unset, list+raw return empty / 404 — we never
// fall back to cwd because cwd is `/` for a bundled .app.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use warp::filters::BoxedFilter;
use warp::{Filter, Rejection};

use crate::log_server::ServerState;

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
struct PlanEntry {
    name: String,
    title: String,
}

#[derive(Serialize)]
struct PlansList {
    plans: Vec<PlanEntry>,
}

fn source_dir(state: &ServerState, source: DocumentSource) -> Option<PathBuf> {
    match source {
        DocumentSource::Plans => state.plans_dir.read().ok().and_then(|p| p.clone()),
        DocumentSource::Docs => state.docs_dir.read().ok().and_then(|p| p.clone()),
    }
}

fn is_safe_relative_md_path(path: &str) -> bool {
    !path.is_empty()
        && path.ends_with(".md")
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.starts_with('.')
                && !segment.contains('\\')
        })
}

fn extract_title(md: &str, fallback: &str) -> String {
    for line in md.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("# ") {
            return rest.trim().to_string();
        }
    }
    fallback.trim_end_matches(".md").to_string()
}

const VIEWER_HTML: &str = include_str!("../assets/plans-viewer.html");

fn collect_markdown_entries(root: &Path, dir: &Path, entries: &mut Vec<PlanEntry>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
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

pub fn routes(state: ServerState) -> BoxedFilter<(warp::reply::Response,)> {
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

async fn handle_viewer() -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    Ok(
        warp::reply::with_header(VIEWER_HTML, "content-type", "text/html; charset=utf-8")
            .into_response(),
    )
}

async fn handle_list(
    source: DocumentSource,
    state: ServerState,
) -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    let Some(dir) = source_dir(&state, source) else {
        return Ok(warp::reply::json(&PlansList { plans: Vec::new() }).into_response());
    };
    let mut entries = Vec::new();
    collect_markdown_entries(&dir, &dir, &mut entries);
    entries.sort_by_key(|a| a.title.to_lowercase());
    Ok(warp::reply::json(&PlansList { plans: entries }).into_response())
}

async fn handle_raw(
    source: DocumentSource,
    query: HashMap<String, String>,
    state: ServerState,
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
    let Ok(canon_dir) = dir.canonicalize() else {
        return Ok(warp::reply::with_status(
            "document dir missing",
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response());
    };
    let candidate = dir.join(path);
    let Ok(canon_path) = candidate.canonicalize() else {
        return Ok(warp::reply::with_status(
            "document not found",
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response());
    };
    // Defense in depth: even if validation passed, refuse anything that
    // canonicalizes outside the source dir (e.g. symlinks).
    if !canon_path.starts_with(&canon_dir) {
        return Ok(warp::reply::with_status(
            "invalid document path",
            warp::http::StatusCode::BAD_REQUEST,
        )
        .into_response());
    }
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
        assert!(!is_safe_relative_md_path("foo//bar.md"));
        assert!(!is_safe_relative_md_path(""));
    }

    #[test]
    fn safe_relative_md_path_requires_md_suffix() {
        assert!(!is_safe_relative_md_path("foo"));
        assert!(!is_safe_relative_md_path("foo.txt"));
        assert!(!is_safe_relative_md_path("foo.md.exe"));
    }

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
