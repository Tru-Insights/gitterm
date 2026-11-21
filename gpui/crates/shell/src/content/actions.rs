//! Viewer actions: clipboard reads run off-thread; external opens use the platform.
use crate::Shell;
use gpui::{App, ClipboardItem, Context};
use std::path::{Path, PathBuf};

pub(super) fn is_rendered(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "html" | "htm" | "excalidraw"
            )
        })
}

/// Paths handed to the platform opener, recorded instead under test so no
/// browser is ever launched.
#[cfg(test)]
pub(crate) static OPENED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

pub(super) fn open_file(path: &Path, cx: &App) -> Result<(), String> {
    #[cfg(test)]
    {
        _ = cx;
        OPENED.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
    #[cfg(not(test))]
    open_platform(path, cx)
}

#[cfg(not(test))]
fn open_platform(path: &Path, cx: &App) -> Result<(), String> {
    use objc2_foundation::{NSString, NSURL};
    let path = path
        .to_str()
        .ok_or_else(|| format!("Open {}: invalid path", path.display()))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let url = url
        .absoluteString()
        .ok_or_else(|| format!("Open {path}: invalid file URL"))?;
    cx.open_url(&url.to_string());
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Tests redirect the browser page into their own temp dir.
    pub(crate) static TEST_OUT_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Where the browser page is written: the temp dir (tests: their own dir).
fn browser_out_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_OUT_DIR.with(|dir| dir.borrow().clone()) {
        return dir;
    }
    std::env::temp_dir()
}

/// The page Iced opens in the browser, written to a temp file: the rendered
/// HTML for markdown/HTML/Excalidraw, the content server's `/file/{id}` page
/// for anything else. The `gitterm-gpui-` prefix never collides with Iced's.
/// As in Iced: rendered content missing opens the file itself; an empty raw
/// file opens nothing (`Ok(None)`).
pub(super) fn browser_page(
    root: &Path,
    relative: &Path,
    dark: bool,
    out_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let path = root.join(relative);
    let load = gitterm_core::content::collect_contained_file_load(0, root, relative, dark)
        .map_err(|e| format!("Open {} in browser: {e}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "preview".to_string());
    let (html, suffix) = if is_rendered(relative) {
        let Some(html) = load.webview_content else {
            return Ok(Some(path));
        };
        (html, "preview")
    } else {
        if load.file_content.is_empty() {
            return Ok(None);
        }
        let html =
            gitterm_core::file_page::file_page_html(&path.to_string_lossy(), &load.file_content);
        (html, "file")
    };
    let temp = out_dir.join(format!("gitterm-gpui-{stem}_{suffix}.html"));
    std::fs::write(&temp, html).map_err(|e| format!("Write {}: {e}", temp.display()))?;
    Ok(Some(temp))
}

fn full_text(root: &Path, relative: &Path) -> Result<String, String> {
    let path = root.join(relative);
    let read = || -> Result<String, std::io::Error> {
        let root = root.canonicalize()?;
        let path = path.canonicalize()?;
        if !path.starts_with(root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "outside viewer root",
            ));
        }
        std::fs::read_to_string(path)
    };
    read().map_err(|error| format!("Copy {}: {error}", path.display()))
}

impl Shell {
    pub(crate) fn editor_preview_notice(&self, cx: &mut Context<Self>) {
        self.viewer_error("Open in editor · not available in preview".into(), cx);
    }

    pub(super) fn viewer_error(&self, error: String, cx: &mut Context<Self>) {
        self.owner.update(cx, |owner, cx| {
            owner.error = Some(error);
            cx.notify();
        });
    }

    pub(super) fn open_viewer_in_browser(
        &self,
        root: PathBuf,
        relative: PathBuf,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        let out_dir = browser_out_dir();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { browser_page(&root, &relative, dark, &out_dir) })
                .await;
            _ = this.update(cx, |this, cx| {
                if let Err(error) =
                    result.and_then(|page| page.map_or(Ok(()), |page| open_file(&page, cx)))
                {
                    this.viewer_error(error, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn copy_viewer_file(
        &self,
        root: PathBuf,
        relative: PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { full_text(&root, &relative) })
                .await;
            _ = this.update(cx, |this, cx| match result {
                Ok(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Err(error) => this.viewer_error(error, cx),
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_text_rejects_missing_invalid_text_and_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside"), "private fixture").unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside"), root.path().join("link"))
            .unwrap();
        assert!(
            full_text(root.path(), Path::new("link"))
                .unwrap_err()
                .contains("outside viewer root")
        );
        std::fs::write(root.path().join("binary"), [0xff]).unwrap();
        assert!(full_text(root.path(), Path::new("binary")).is_err());
        assert!(
            full_text(root.path(), Path::new("missing"))
                .unwrap_err()
                .contains("missing")
        );
    }
}
