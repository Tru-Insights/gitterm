//! What the shell lends the native review: where marks live, the turn snapshot
//! source, the active chat and the file opener. A default host is in-memory only.
use gitterm_core::turn_snapshots::SnapshotRoots;
use gpui::App;
use std::{path::PathBuf, rc::Rc, sync::Arc};

pub type SaveMarks = Arc<dyn Fn(Vec<u8>) -> Result<(), String> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ReviewHost {
    /// Iced's shared config dir; its `review-viewed.json` is only ever read.
    pub iced_marks_dir: Option<PathBuf>,
    /// The preview state root's own marks, read and merged over Iced's.
    pub preview_marks_dir: Option<PathBuf>,
    /// Writes the encoded marks into the preview state root, nowhere else.
    pub save_marks: Option<SaveMarks>,
    /// Iced's turn snapshot store, read-only. None leaves turns unavailable.
    pub snapshot_roots: Option<SnapshotRoots>,
    /// The chat whose turns Turns lists.
    pub active_chat: Option<String>,
    /// Opens a worktree file the way the shell opens a file from the file tree.
    pub open_file: Option<Rc<dyn Fn(PathBuf, &mut App)>>,
}
