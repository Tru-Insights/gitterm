//! Local retained Viewer documents and native overlay state.
//!
//! `window` owns the shell's surfaces, off-thread preparation, native result
//! draining, deadlines, selection and window-relative pane geometry. Component
//! popup/dialog state and native pickers cover the selected surface before paint.
//! There are no IPC handlers or Page identity/store capabilities. Document links
//! follow the Iced Viewer network/navigation policy.
mod actions;
pub mod host;
pub mod overlay;
mod session;
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
        if let Some(html) = load.webview_content {
            return Ok(Self { html });
        }
        if actions::is_rendered(relative) {
            return Err(format!(
                "Cannot preview {}: {}",
                relative.display(),
                load.file_preview_notice
                    .as_deref()
                    .unwrap_or("invalid or unsupported retained content")
            ));
        }
        let path = root.join(relative);
        // Core owns truncation and preview notices. Verify failed/invalid reads
        // explicitly because an empty core text payload can also be an empty file.
        std::fs::File::open(&path).map_err(|error| format!("Read {}: {error}", path.display()))?;
        if load.image_path.is_some() {
            return Err(format!(
                "Cannot preview {}: image preview unavailable",
                path.display()
            ));
        }
        if load.file_content.is_empty() && std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            return Err(format!(
                "Cannot preview {}: binary or invalid text",
                path.display()
            ));
        }
        let escape = |text: &str| {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('\"', "&quot;")
        };
        let (background, foreground) = if dark {
            ("#1e1e2e", "#cdd6f4")
        } else {
            ("#eff1f5", "#4c4f69")
        };
        Ok(Self {
            html: format!(
                "<!doctype html><html><meta charset=\"utf-8\"><body style=\"background:{background};color:{foreground};font:13px monospace\"><p>{}</p><pre>{}</pre></body></html>",
                escape(load.file_preview_notice.as_deref().unwrap_or("")),
                escape(&load.file_content)
            ),
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
mod interactions;
#[cfg(test)]
mod tests;
