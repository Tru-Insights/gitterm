// Plans viewer warp routes.
//
// Serves:
//   GET /plans               -> JSON { plans: [{ name, title }, ...] }
//   GET /plans/raw/{name}.md -> raw markdown bytes
//   GET /plans/viewer        -> embedded HTML viewer (reads ?plan= and ?theme=)
//
// Source-of-truth invariant: this module reads `.md` files only. It never
// writes. Path traversal is rejected before any filesystem access.
//
// Plans directory comes from `ServerState::plans_dir`, which the app pushes
// to whenever the active workspace changes. If unset, list+raw return empty
// / 404 — we never fall back to cwd because cwd is `/` for a bundled .app.

use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use warp::filters::BoxedFilter;
use warp::{Filter, Rejection};

use crate::log_server::ServerState;

#[derive(Serialize)]
struct PlanEntry {
    name: String,
    title: String,
}

#[derive(Serialize)]
struct PlansList {
    plans: Vec<PlanEntry>,
}

fn plans_dir(state: &ServerState) -> Option<PathBuf> {
    state.plans_dir.read().ok().and_then(|p| p.clone())
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.ends_with(".md")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.starts_with('.')
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

pub fn routes(state: ServerState) -> BoxedFilter<(warp::reply::Response,)> {
    let with_state = warp::any().map(move || state.clone());

    let list = warp::path!("plans")
        .and(warp::get())
        .and(with_state.clone())
        .and_then(handle_list);

    let raw = warp::path!("plans" / "raw" / String)
        .and(warp::get())
        .and(with_state)
        .and_then(handle_raw);

    let viewer = warp::path!("plans" / "viewer")
        .and(warp::get())
        .and_then(handle_viewer);

    list.or(viewer).unify().or(raw).unify().boxed()
}

async fn handle_viewer() -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    Ok(
        warp::reply::with_header(VIEWER_HTML, "content-type", "text/html; charset=utf-8")
            .into_response(),
    )
}

async fn handle_list(state: ServerState) -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    let Some(dir) = plans_dir(&state) else {
        return Ok(warp::reply::json(&PlansList { plans: Vec::new() }).into_response());
    };
    let mut entries = Vec::new();
    if let Ok(read_dir) = fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_safe_name(name) {
                continue;
            }
            let content = fs::read_to_string(&path).unwrap_or_default();
            let title = extract_title(&content, name);
            entries.push(PlanEntry {
                name: name.to_string(),
                title,
            });
        }
    }
    entries.sort_by_key(|a| a.title.to_lowercase());
    Ok(warp::reply::json(&PlansList { plans: entries }).into_response())
}

async fn handle_raw(name: String, state: ServerState) -> Result<warp::reply::Response, Rejection> {
    use warp::Reply;
    if !is_safe_name(&name) {
        return Ok(warp::reply::with_status(
            "invalid plan name",
            warp::http::StatusCode::BAD_REQUEST,
        )
        .into_response());
    }
    let Some(dir) = plans_dir(&state) else {
        return Ok(warp::reply::with_status(
            "no active plans dir",
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response());
    };
    let Ok(canon_dir) = dir.canonicalize() else {
        return Ok(warp::reply::with_status(
            "plans dir missing",
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response());
    };
    let candidate = dir.join(&name);
    let Ok(canon_path) = candidate.canonicalize() else {
        return Ok(
            warp::reply::with_status("plan not found", warp::http::StatusCode::NOT_FOUND)
                .into_response(),
        );
    };
    // Defense in depth: even if is_safe_name passed, refuse anything that
    // canonicalizes outside the plans dir (e.g. symlink shenanigans).
    if !canon_path.starts_with(&canon_dir) {
        return Ok(warp::reply::with_status(
            "invalid plan path",
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
        Err(_) => Ok(
            warp::reply::with_status("plan not found", warp::http::StatusCode::NOT_FOUND)
                .into_response(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_accepts_normal_md() {
        assert!(is_safe_name("foo.md"));
        assert!(is_safe_name("agent-tab-integration.md"));
    }

    #[test]
    fn safe_name_rejects_traversal() {
        assert!(!is_safe_name("../foo.md"));
        assert!(!is_safe_name("foo/bar.md"));
        assert!(!is_safe_name("foo\\bar.md"));
        assert!(!is_safe_name(".hidden.md"));
        assert!(!is_safe_name(""));
    }

    #[test]
    fn safe_name_requires_md_suffix() {
        assert!(!is_safe_name("foo"));
        assert!(!is_safe_name("foo.txt"));
        assert!(!is_safe_name("foo.md.exe"));
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
