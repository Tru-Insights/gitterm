//! Viewer actions: clipboard reads run off-thread; external opens use the platform.
use crate::Shell;
use gpui::{App, ClipboardItem, Context};
use objc2_foundation::{NSString, NSURL};
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

pub(super) fn open_file(path: &Path, cx: &App) -> Result<(), String> {
    // Open the original file: preview actions must not write a temporary rendition.
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
