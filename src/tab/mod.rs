//! Tab content kinds.
//!
//! Today there are two variants: `Terminal` (with an optional file viewer overlay)
//! and `Agent` (Claude Code or pi-backed conversational tab — data shape only at
//! this step; subprocess plumbing lands in Step 3).
//!
//! `TabState` itself lives in `main.rs` because it's tightly coupled to many in-binary
//! helpers (file load, syntax highlight, git status, claude config). Only the tab-kind
//! data structures live here.

mod agent;

// Re-exports. Some are used immediately (config persistence, TabKind variants); others
// (AgentEvent, AgentSessionState, AgentBackend) are surfaced now so the type lives in
// crate::tab::*, but won't be referenced from main.rs until Steps 3 and 4 wire them up.
#[allow(unused_imports)]
pub(crate) use agent::{
    AgentBackend, AgentBackendConfig, AgentEvent, AgentSession, AgentSessionState,
};

use std::path::PathBuf;
use std::time::Instant;

use iced::widget::image;

use crate::FileVersionSignature;
use crate::SyntaxHighlightLine;
use crate::agent as agent_log;

/// File viewer state attached to a Terminal tab while the user is viewing a file.
/// Closing the file (Back / Close button) drops this back to None and reveals the terminal.
pub(crate) struct FileViewerOverlay {
    pub(crate) path: PathBuf,
    pub(crate) file_content: String,
    pub(crate) image_handle: Option<image::Handle>,
    /// Rendered HTML for markdown / excalidraw / .html files (driven through the wry webview).
    pub(crate) webview_content: Option<String>,
    /// Optional notice shown in the viewer (e.g. large-file preview mode).
    pub(crate) preview_notice: Option<String>,
    pub(crate) syntax_highlight_lines: Option<Vec<SyntaxHighlightLine>>,
    pub(crate) syntax_highlight_notice: Option<String>,
    pub(crate) syntax_highlight_in_progress: bool,
    pub(crate) syntax_highlight_requested_lines: usize,
    pub(crate) loaded_signature: Option<FileVersionSignature>,
    pub(crate) load_in_progress: bool,
    pub(crate) load_started_at: Option<Instant>,
}

impl FileViewerOverlay {
    pub(crate) fn for_path(path: PathBuf) -> Self {
        Self {
            path,
            file_content: String::new(),
            image_handle: None,
            webview_content: None,
            preview_notice: None,
            syntax_highlight_lines: None,
            syntax_highlight_notice: None,
            syntax_highlight_in_progress: false,
            syntax_highlight_requested_lines: 0,
            loaded_signature: None,
            load_in_progress: false,
            load_started_at: None,
        }
    }
}

/// Per-terminal-tab state. A terminal tab can optionally have a file viewer overlay
/// open on top of its terminal (modal-style); closing the overlay restores the terminal view.
pub(crate) struct TerminalTab {
    pub(crate) terminal: Option<iced_term::Terminal>,
    /// Title set by the shell/programs via OSC escape codes.
    pub(crate) terminal_title: Option<String>,
    /// Optional command to run after shell init (e.g. "claude" for Claude Code tabs).
    pub(crate) startup_command: Option<String>,
    /// Modal file viewer overlay sitting on top of the terminal.
    pub(crate) file_viewer: Option<FileViewerOverlay>,
    /// Debounce: most-recent `ViewFile` request this tab received, to suppress double-clicks.
    pub(crate) last_view_request_path: Option<PathBuf>,
    pub(crate) last_view_request_at: Option<Instant>,
}

impl TerminalTab {
    pub(crate) fn new() -> Self {
        Self {
            terminal: None,
            terminal_title: None,
            startup_command: None,
            file_viewer: None,
            last_view_request_path: None,
            last_view_request_at: None,
        }
    }
}

/// Tab content kind. Terminal tabs run a shell (with an optional file viewer overlay);
/// Agent tabs host a Claude Code or pi conversation in a wry webview.
pub(crate) enum TabKind {
    Terminal(TerminalTab),
    Agent(AgentSession),
}

/// Cross-cutting agent-activity sidebar state. Lives on every tab regardless of kind because
/// any tab can have its sidebar set to `SidebarMode::Agent` and view captures from this repo.
/// Note: this is the *captured-log viewer* (reads on-disk JSONL captures) — it predates
/// the live agent tab kind in `TabKind::Agent` and is separate state.
#[derive(Default)]
pub(crate) struct AgentActivityState {
    pub(crate) activity: Option<agent_log::AgentActivity>,
    pub(crate) loading: bool,
    pub(crate) selected_capture_idx: Option<usize>,
    pub(crate) conversation: Option<agent_log::Conversation>,
}
