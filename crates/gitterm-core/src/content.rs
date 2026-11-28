//! Frontend-independent retained-content DTOs and file rendering services.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

pub mod bar_hint;
#[cfg(feature = "excalidraw")]
pub mod excalidraw;
pub mod html_render;
pub mod markdown;
pub mod source;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct FileVersionSignature {
    pub modified_unix_nanos: u128,
    pub file_len: u64,
}

/// Iced's change signature: modified time in nanoseconds plus byte length.
pub fn file_version_signature(path: &Path) -> Option<FileVersionSignature> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileVersionSignature {
        modified_unix_nanos: metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
        file_len: metadata.len(),
    })
}

#[derive(Debug, Clone)]
pub struct FileLoadSnapshot {
    pub tab_id: usize,
    pub path: PathBuf,
    pub file_content: String,
    pub image_path: Option<PathBuf>,
    pub webview_content: Option<String>,
    pub file_preview_notice: Option<String>,
    pub file_signature: Option<FileVersionSignature>,
}

pub const MAX_INLINE_WEBVIEW_BYTES: u64 = 1_500_000;
pub const MAX_FULL_TEXT_LOAD_BYTES: u64 = 1_000_000;
pub const LARGE_TEXT_PREVIEW_BYTES: usize = 256 * 1024;
pub const LARGE_TEXT_PREVIEW_LINES: usize = 2000;

fn perf_enabled() -> bool {
    static PERF_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PERF_ENABLED.get_or_init(|| {
        std::env::var("GITTERM_PERF")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

macro_rules! perf_log {
    ($($arg:tt)*) => {{
        if perf_enabled() {
            eprintln!("[perf] {}", format_args!($($arg)*));
        }
    }};
}

pub mod syntax;

/// Root-bound local preview. Existing unrestricted Iced callers retain their
/// original reader; new frontends must supply a captured root and relative path.
pub fn collect_contained_file_load(
    tab_id: usize,
    root: &Path,
    relative: &Path,
    is_dark_theme: bool,
) -> Result<FileLoadSnapshot, crate::plans::ContainmentError> {
    use std::path::Component;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(crate::plans::ContainmentError::InvalidPath);
    }
    let path = crate::plans::resolve_contained(root, relative)?;
    Ok(collect_file_load(tab_id, path, is_dark_theme))
}

pub fn collect_file_load(tab_id: usize, path: PathBuf, is_dark_theme: bool) -> FileLoadSnapshot {
    let started = Instant::now();
    let mut snapshot = FileLoadSnapshot {
        tab_id,
        path: path.clone(),
        file_content: String::new(),
        image_path: None,
        webview_content: None,
        file_preview_notice: None,
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

    if is_markdown_file(&path) {
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
    } else if is_html_file(&path) {
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
    } else if is_image_file(&path) {
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

/// Shape a FileLoadSnapshot from bytes fetched via a WorkspaceSource so
/// remote files render through the exact same viewer pipeline as local
/// ones. `path` is the remote path as a display/extension token — nothing
/// here touches the local filesystem.
pub fn shape_source_file_load(
    tab_id: usize,
    path: PathBuf,
    content: Result<source::SourceFileContent, String>,
    is_dark_theme: bool,
) -> FileLoadSnapshot {
    let mut snapshot = FileLoadSnapshot {
        tab_id,
        path: path.clone(),
        file_content: String::new(),
        image_path: None,
        webview_content: None,
        file_preview_notice: None,
        file_signature: None,
    };

    let content = match content {
        Ok(content) => content,
        Err(err) => {
            snapshot.file_preview_notice = Some(format!("Could not load remote file: {err}"));
            return snapshot;
        }
    };
    let total_size = content.total_size;

    #[cfg(feature = "excalidraw")]
    if excalidraw::is_excalidraw_file(&path) {
        if total_size > MAX_INLINE_WEBVIEW_BYTES {
            snapshot.file_preview_notice = Some(format!(
                "Inline preview skipped for large Excalidraw file ({}).",
                format_bytes(total_size)
            ));
            return snapshot;
        }
        if let Ok(text) = String::from_utf8(content.data) {
            if excalidraw::validate_excalidraw(&text) {
                snapshot.webview_content =
                    Some(excalidraw::render_excalidraw_html(&text, is_dark_theme));
            }
        }
        return snapshot;
    }

    if is_markdown_file(&path) {
        if total_size > MAX_INLINE_WEBVIEW_BYTES {
            snapshot.file_preview_notice = Some(format!(
                "Inline preview skipped for large Markdown file ({}).",
                format_bytes(total_size)
            ));
            return snapshot;
        }
        let text = String::from_utf8_lossy(&content.data);
        snapshot.webview_content = Some(markdown::render_markdown_to_html(&text, is_dark_theme));
    } else if is_html_file(&path) {
        if total_size > MAX_INLINE_WEBVIEW_BYTES {
            snapshot.file_preview_notice = Some(format!(
                "Inline preview skipped for large HTML file ({}).",
                format_bytes(total_size)
            ));
            return snapshot;
        }
        snapshot.webview_content = Some(String::from_utf8_lossy(&content.data).to_string());
    } else if is_image_file(&path) {
        snapshot.file_preview_notice =
            Some("Image preview isn't supported for remote files yet.".to_string());
    } else if std::str::from_utf8(&content.data).is_err() {
        snapshot.file_preview_notice = Some(format!(
            "Binary file ({}) — no preview.",
            format_bytes(total_size)
        ));
    } else if total_size > MAX_FULL_TEXT_LOAD_BYTES {
        let cut = content.data.len().min(LARGE_TEXT_PREVIEW_BYTES);
        let text = String::from_utf8_lossy(&content.data[..cut]);
        snapshot.file_content = text
            .lines()
            .take(LARGE_TEXT_PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        snapshot.file_preview_notice = Some(format!(
            "Large file ({}): showing first {} lines (~{} KB).",
            format_bytes(total_size),
            LARGE_TEXT_PREVIEW_LINES,
            LARGE_TEXT_PREVIEW_BYTES / 1024
        ));
    } else {
        snapshot.file_content = String::from_utf8_lossy(&content.data).to_string();
        if content.truncated {
            snapshot.file_preview_notice = Some(format!(
                "Preview truncated: showing {} of {}.",
                format_bytes(snapshot.file_content.len() as u64),
                format_bytes(total_size)
            ));
        }
    }

    snapshot
}

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e: &std::ffi::OsStr| e.to_str())
        .map(|ext: &str| {
            matches!(
                ext.to_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
            )
        })
        .unwrap_or(false)
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e: &std::ffi::OsStr| e.to_str())
        .map(|ext: &str| matches!(ext.to_lowercase().as_str(), "md" | "markdown"))
        .unwrap_or(false)
}

fn is_html_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e: &std::ffi::OsStr| e.to_str())
        .map(|ext: &str| matches!(ext.to_lowercase().as_str(), "html" | "htm"))
        .unwrap_or(false)
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn read_text_preview(path: &Path, max_bytes: usize, max_lines: usize) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let bytes_read = file.read(&mut buf)?;
    buf.truncate(bytes_read);

    let text = String::from_utf8_lossy(&buf);
    let mut out = String::new();
    for (idx, line) in text.lines().take(max_lines).enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(path: PathBuf, data: &[u8], total_size: u64, truncated: bool) -> FileLoadSnapshot {
        shape_source_file_load(
            7,
            path,
            Ok(source::SourceFileContent {
                data: data.to_vec(),
                total_size,
                truncated,
            }),
            true,
        )
    }

    #[test]
    fn remote_shaping_uses_supplied_bytes_even_when_local_path_exists() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["same.txt", "same.html", "same.md", "same.png"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "local secret").unwrap();
            let snapshot = remote(path.clone(), b"remote bytes", 12, false);
            assert_eq!(snapshot.path, path);
            assert_eq!(snapshot.tab_id, 7);
            assert_eq!(snapshot.image_path, None);
            assert_eq!(snapshot.file_signature, None);
            assert!(!snapshot.file_content.contains("local secret"));
            assert!(!snapshot
                .webview_content
                .as_deref()
                .unwrap_or("")
                .contains("local secret"));
            if name.ends_with(".txt") {
                assert_eq!(snapshot.file_content, "remote bytes");
            } else if name.ends_with(".png") {
                assert_eq!(
                    snapshot.file_preview_notice.as_deref(),
                    Some("Image preview isn't supported for remote files yet.")
                );
            } else {
                assert!(snapshot.webview_content.unwrap().contains("remote bytes"));
            }
        }
        let snapshot =
            shape_source_file_load(1, dir.path().into(), Err("fixture error".into()), true);
        assert_eq!(
            snapshot.file_preview_notice.as_deref(),
            Some("Could not load remote file: fixture error")
        );
        assert_eq!(snapshot.file_signature, None);
    }

    #[test]
    fn inline_and_text_limits_keep_existing_local_and_remote_notices() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["doc.md", "page.HTML"] {
            let path = dir.path().join(name);
            // Sparse whitespace is valid UTF-8 without an enormous Markdown tree.
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(MAX_INLINE_WEBVIEW_BYTES + 1).unwrap();
            let local = collect_file_load(1, path.clone(), true);
            let kind = if name.ends_with("md") {
                "Markdown"
            } else {
                "HTML"
            };
            assert_eq!(
                local.file_preview_notice,
                Some(format!(
                    "Inline preview skipped for large {kind} file ({}). Click \"View in Browser\".",
                    format_bytes(MAX_INLINE_WEBVIEW_BYTES + 1)
                ))
            );
            let at_limit = remote(path.clone(), b"ok", MAX_INLINE_WEBVIEW_BYTES, false);
            assert!(at_limit.webview_content.is_some());
            let over = remote(path, b"ok", MAX_INLINE_WEBVIEW_BYTES + 1, false);
            assert_eq!(over.webview_content, None);
            assert_eq!(
                over.file_preview_notice,
                Some(format!(
                    "Inline preview skipped for large {kind} file ({}).",
                    format_bytes(MAX_INLINE_WEBVIEW_BYTES + 1)
                ))
            );
        }
        let path = dir.path().join("large.txt");
        std::fs::write(&path, vec![b'x'; MAX_FULL_TEXT_LOAD_BYTES as usize]).unwrap();
        assert_eq!(
            collect_file_load(1, path.clone(), true).file_preview_notice,
            None
        );
        std::fs::write(&path, vec![b'x'; MAX_FULL_TEXT_LOAD_BYTES as usize + 1]).unwrap();
        let local = collect_file_load(1, path.clone(), true);
        assert_eq!(local.file_content.len(), LARGE_TEXT_PREVIEW_BYTES);
        assert_eq!(
            local.file_signature.unwrap().file_len,
            MAX_FULL_TEXT_LOAD_BYTES + 1
        );
        let remote = remote(path, b"one\ntwo\n", MAX_FULL_TEXT_LOAD_BYTES + 1, true);
        assert_eq!(remote.file_content, "one\ntwo");
        assert_eq!(remote.file_preview_notice, local.file_preview_notice);
    }

    #[test]
    fn remote_binary_truncation_and_line_limits_are_preserved() {
        let binary = remote("opaque.txt".into(), &[0xff], 1, false);
        assert_eq!(
            binary.file_preview_notice.as_deref(),
            Some("Binary file (1 B) — no preview.")
        );
        let truncated = remote("opaque.txt".into(), b"abc", 10, true);
        assert_eq!(truncated.file_content, "abc");
        assert_eq!(
            truncated.file_preview_notice.as_deref(),
            Some("Preview truncated: showing 3 B of 10 B.")
        );
        let at_limit = remote("opaque.txt".into(), b"abc", MAX_FULL_TEXT_LOAD_BYTES, false);
        assert_eq!(at_limit.file_preview_notice, None);
        let lines = "x\n".repeat(LARGE_TEXT_PREVIEW_LINES + 1);
        let preview = remote(
            "opaque.txt".into(),
            lines.as_bytes(),
            MAX_FULL_TEXT_LOAD_BYTES + 1,
            true,
        );
        assert_eq!(
            preview.file_content.lines().count(),
            LARGE_TEXT_PREVIEW_LINES
        );
    }

    #[test]
    fn missing_local_and_image_paths_keep_original_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let missing = collect_file_load(1, dir.path().join("missing.txt"), true);
        assert_eq!(missing.file_signature, None);
        assert_eq!(missing.file_preview_notice, None);
        assert!(missing.file_content.is_empty());
        let path = dir.path().join("image.PNG");
        std::fs::write(&path, b"image bytes").unwrap();
        let image = collect_file_load(1, path.clone(), true);
        assert_eq!(image.image_path, Some(path));
        assert_eq!(image.file_signature.unwrap().file_len, 11);
    }

    #[test]
    fn excalidraw_classification_follows_the_feature() {
        let path = PathBuf::from("opaque.Excalidraw");
        let json = br#"{"type":"excalidraw","elements":[]}"#;
        let snapshot = remote(path.clone(), json, json.len() as u64, false);
        #[cfg(feature = "excalidraw")]
        {
            assert!(snapshot.webview_content.unwrap().contains("elements"));
            let invalid = remote(path.clone(), b"invalid", 7, false);
            assert!(invalid.webview_content.is_none());
            let over = remote(path, json, MAX_INLINE_WEBVIEW_BYTES + 1, false);
            assert_eq!(
                over.file_preview_notice,
                Some(format!(
                    "Inline preview skipped for large Excalidraw file ({}).",
                    format_bytes(MAX_INLINE_WEBVIEW_BYTES + 1)
                ))
            );
        }
        #[cfg(not(feature = "excalidraw"))]
        {
            assert!(snapshot.webview_content.is_none());
            assert_eq!(snapshot.file_content.as_bytes(), json);
        }
    }
}

#[cfg(test)]
mod containment_tests {
    use super::*;
    use crate::plans::ContainmentError;

    #[test]
    fn captured_roots_and_invalid_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let plans = workspace.path().join(".plans");
        let docs = workspace.path().join("docs");
        std::fs::create_dir(&plans).unwrap();
        std::fs::create_dir(&docs).unwrap();
        for (root, text) in [
            (workspace.path(), "Workspace"),
            (&plans, "Plans"),
            (&docs, "Docs"),
        ] {
            std::fs::write(root.join("guide.md"), format!("# {text}")).unwrap();
            let load = collect_contained_file_load(7, root, Path::new("guide.md"), true).unwrap();
            assert_eq!(load.tab_id, 7);
            assert!(load.webview_content.unwrap().contains(text));
        }
        for relative in [
            "../guide.md",
            "nested/../guide.md",
            "/guide.md",
            "./guide.md",
            "",
        ] {
            assert_eq!(
                collect_contained_file_load(0, workspace.path(), Path::new(relative), true)
                    .unwrap_err(),
                ContainmentError::InvalidPath
            );
        }
        let absolute_inside = workspace.path().join("guide.md");
        assert_eq!(
            collect_contained_file_load(0, workspace.path(), &absolute_inside, true).unwrap_err(),
            ContainmentError::InvalidPath
        );
        for relative in ["missing.md", "%2e%2e%2fguide.md", "%2e%2e/guide.md"] {
            assert!(
                collect_contained_file_load(0, workspace.path(), Path::new(relative), true)
                    .is_err()
            );
        }
        assert_eq!(
            collect_contained_file_load(
                0,
                &workspace.path().join("missing-root"),
                Path::new("guide.md"),
                true
            )
            .unwrap_err(),
            ContainmentError::RootMissing
        );
        assert_eq!(
            collect_contained_file_load(0, &plans, Path::new("../docs/guide.md"), true)
                .unwrap_err(),
            ContainmentError::InvalidPath
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_symlinks_cannot_cross_the_captured_root() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("guide.md"), "# Inside").unwrap();
        std::fs::write(outside.path().join("guide.md"), "# Outside").unwrap();
        symlink(
            root.path().join("guide.md"),
            root.path().join("internal.md"),
        )
        .unwrap();
        symlink(
            outside.path().join("guide.md"),
            root.path().join("external.md"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("external-dir")).unwrap();
        assert!(
            collect_contained_file_load(0, root.path(), Path::new("internal.md"), true)
                .unwrap()
                .webview_content
                .unwrap()
                .contains("Inside")
        );
        let plans = root.path().join(".plans");
        std::fs::create_dir(&plans).unwrap();
        symlink(
            root.path().join("guide.md"),
            plans.join("workspace-link.md"),
        )
        .unwrap();
        assert_eq!(
            collect_contained_file_load(0, &plans, Path::new("workspace-link.md"), true)
                .unwrap_err(),
            ContainmentError::InvalidPath
        );
        assert!(collect_contained_file_load(
            0,
            root.path(),
            Path::new(".plans/workspace-link.md"),
            true
        )
        .is_ok());
        for relative in ["external.md", "external-dir/guide.md"] {
            assert_eq!(
                collect_contained_file_load(0, root.path(), Path::new(relative), true).unwrap_err(),
                ContainmentError::InvalidPath
            );
        }
    }
}
