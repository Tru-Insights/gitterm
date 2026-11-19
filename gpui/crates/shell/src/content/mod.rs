//! Leaf retained Viewer adapter. No shell registration, IPC, URL loading or Page stores.
//!
//! PR9 must own one coordinator per window, prepare documents off the UI thread,
//! route every cover (including popup menus and dock motion) through it before
//! drawing controls, drain native snapshot results and tick snapshot deadlines.
//! Render `backdrop()` in the pane, one `dim_layer()` above native content, then
//! controls. Recompute window-relative geometry before final cover close. Honor
//! `RestoreFocus` using the saved valid native focus target or active pane.
pub mod host;
pub mod overlay;

use std::path::{Path, PathBuf};

/// Captured origin; a completion must never consult a replacement active pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub workspace: u64,
    pub pane: u64,
    pub source: PathBuf,
}

#[derive(Debug)]
pub struct Document {
    html: String,
}

impl Document {
    /// Run on a worker. Keep core size limits, Markdown assets and optional
    /// Excalidraw validation; a failed/unsupported read is an explicit error.
    pub fn local_file(path: &Path, dark: bool) -> Result<Self, String> {
        let load = gitterm_core::content::collect_file_load(0, path.to_owned(), dark);
        load.webview_content
            .map(|html| Self { html })
            .ok_or_else(|| {
                format!(
                    "Cannot preview {}: {}",
                    path.display(),
                    load.file_preview_notice
                        .as_deref()
                        .unwrap_or("unreadable, invalid or unsupported retained content")
                )
            })
    }

    /// A local plans document, with the same relative-path and canonical-root
    /// checks as core plans routes. The captured root must change with workspace.
    /// No HTTP listener or ambient cwd/config lookup is started by this adapter.
    pub fn plan(root: &Path, relative: &str, dark: bool) -> Result<Self, String> {
        if !gitterm_core::plans::is_safe_relative_md_path(relative) {
            return Err(format!("Invalid plan path: {relative}"));
        }
        let root = root
            .canonicalize()
            .map_err(|e| format!("Cannot resolve plans root {}: {e}", root.display()))?;
        let path = root
            .join(relative)
            .canonicalize()
            .map_err(|e| format!("Cannot resolve plan {}: {e}", root.join(relative).display()))?;
        if !path.starts_with(&root) {
            return Err(format!("Plan escapes root: {relative}"));
        }
        Self::local_file(&path, dark)
    }

    pub fn html(&self) -> &str {
        &self.html
    }
}

#[cfg(test)]
mod tests;
