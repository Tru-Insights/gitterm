//! Local retained Viewer documents and native overlay state.
//!
//! `window` owns the shell's surfaces, off-thread preparation, native result
//! draining, deadlines, selection and window-relative pane geometry. Component
//! popup/dialog state and native pickers cover the selected surface before paint.
//! A local message can only request viewer dismissal. Document links follow the
//! Iced Viewer network/navigation policy; a remote Page has its own login store and policy.
pub(crate) mod actions;
pub mod host;
pub mod overlay;
mod session;
pub(crate) mod visual;
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
    text: String,
    signature: Option<gitterm_core::content::FileVersionSignature>,
    /// A remote HTTP(S) Page, loaded by URL instead of `html`.
    page_url: Option<String>,
}

impl Document {
    /// Run on a worker. Keep core size limits, Markdown assets and optional
    /// Excalidraw validation; a failed/unsupported read is an explicit error.
    pub fn local_file(root: &Path, relative: &Path, dark: bool) -> Result<Self, String> {
        Self::local_file_with_font(root, relative, dark, crate::theme::UI_FONT_SIZE)
    }

    pub(super) fn local_file_with_font(
        root: &Path,
        relative: &Path,
        dark: bool,
        font: f32,
    ) -> Result<Self, String> {
        let load = gitterm_core::content::collect_contained_file_load(0, root, relative, dark)
            .map_err(|e| format!("Cannot preview {}: {e}", root.join(relative).display()))?;
        if let Some(html) = load.webview_content {
            return Ok(Self {
                html,
                text: load.file_content,
                signature: load.file_signature,
                page_url: None,
            });
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
        if let Some(path) = &load.image_path {
            use base64::Engine as _;
            let bytes =
                std::fs::read(path).map_err(|error| format!("Read {}: {error}", path.display()))?;
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mime = match ext.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "ico" => "image/x-icon",
                "png" => "image/png",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                _ => unreachable!("core classifies supported image formats"),
            };
            let background = if dark { "#1e1e2e" } else { "#eff1f5" };
            return Ok(Self {
                html: format!(
                    "<!doctype html><html><meta charset=\"utf-8\"><body style=\"margin:0;padding:16px;background:{background};text-align:center\"><img alt=\"Image preview\" style=\"max-width:100%;object-fit:contain\" src=\"data:{mime};base64,{}\"></body></html>",
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ),
                text: String::new(),
                signature: load.file_signature,
                page_url: None,
            });
        }
        if load.file_content.is_empty() && std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            return Err(format!(
                "Cannot preview {}: binary or invalid text",
                path.display()
            ));
        }
        let escape = gitterm_core::file_page::html_escape;
        let (background, foreground, muted) = if dark {
            ("#1e1e2e", "#cdd6f4", "#9399b2")
        } else {
            ("#eff1f5", "#4c4f69", "#7c7f93")
        };
        let (highlighted, syntax_notice) =
            gitterm_core::content::syntax::build_syntax_highlight_lines(
                &path,
                &load.file_content,
                dark,
            );
        let mut body = String::new();
        for notice in [
            load.file_preview_notice.as_deref(),
            syntax_notice.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            body.push_str(&format!("<p>{}</p>", escape(notice)));
        }
        let total_lines = load.file_content.lines().count();
        let limit = gitterm_core::content::syntax::MAX_FILE_VIEW_RENDER_LINES;
        if total_lines > limit {
            body.push_str(&format!("<p>Rendering first {limit} of {total_lines} lines ({} hidden) for performance.</p>", total_lines - limit));
        }
        body.push_str("<div class=\"source\">");
        for (i, line) in load.file_content.lines().take(limit).enumerate() {
            body.push_str(&format!(
                "<div class=\"line\"><span class=\"line-number\">{:4}</span> ",
                i + 1
            ));
            if let Some(line) = highlighted.as_ref().and_then(|lines| lines.get(i)) {
                for segment in &line.segments {
                    let [r, g, b] = segment.color;
                    body.push_str(&format!(
                        "<span style=\"color:#{r:02x}{g:02x}{b:02x}\">{}</span>",
                        escape(&segment.text)
                    ));
                }
            } else {
                body.push_str(&escape(if line.is_empty() { " " } else { line }));
            }
            body.push_str("</div>");
        }
        if total_lines > limit {
            body.push_str(&format!(
                "<p>... {} additional lines not rendered</p>",
                total_lines - limit
            ));
        }
        if load.file_content.is_empty() {
            body.push_str("(empty file)");
        }
        body.push_str("</div>");
        Ok(Self {
            html: format!(
                "<!doctype html><html><meta charset=\"utf-8\"><style>body{{margin:0;background:{background};color:{foreground};font:{font}px/1.3 monospace}}.source{{padding:8px}}.line{{white-space:pre;padding:1px 4px;min-width:max-content}}.line-number{{color:{muted};user-select:none}}p{{padding:6px 10px;margin:0}}span{{font-family:monospace}}</style><body>{body}</body></html>"
            ),
            text: load.file_content,
            signature: load.file_signature,
            page_url: None,
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

    pub(crate) fn plans_viewer(html: String) -> Self {
        Self {
            html,
            text: String::new(),
            signature: None,
            page_url: None,
        }
    }

    /// A remote Page. Only HTTP(S) URLs are accepted.
    pub fn page(url: &str) -> Result<Self, String> {
        gitterm_core::page_store::browser_url(url)?;
        Ok(Self {
            html: String::new(),
            text: String::new(),
            signature: None,
            page_url: Some(url.to_owned()),
        })
    }

    pub fn html(&self) -> &str {
        &self.html
    }

    pub fn page_url(&self) -> Option<&str> {
        self.page_url.as_deref()
    }
}

#[cfg(test)]
mod interactions;
#[cfg(test)]
mod tests;
