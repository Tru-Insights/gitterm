//! Local retained Viewer documents and native overlay state.
//!
//! `window` owns the shell's surfaces, off-thread preparation, native result
//! draining, deadlines, selection and window-relative pane geometry. Component
//! popup/dialog state and native pickers cover the selected surface before paint.
//! There are no IPC handlers, URL loading or Page identity/store capabilities.
pub mod host;
pub mod overlay;
pub(crate) mod window;

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
    pub fn local_file(root: &Path, relative: &Path, dark: bool) -> Result<Self, String> {
        let load = gitterm_core::content::collect_contained_file_load(0, root, relative, dark)
            .map_err(|e| format!("Cannot preview {}: {e}", root.join(relative).display()))?;
        load.webview_content
            .map(|html| Self { html })
            .ok_or_else(|| {
                format!(
                    "Cannot preview {}: {}",
                    relative.display(),
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
        Self::local_file(root, Path::new(relative), dark)
    }

    pub fn html(&self) -> &str {
        &self.html
    }
}

#[cfg(test)]
mod tests;
