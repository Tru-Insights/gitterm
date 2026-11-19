use std::path::PathBuf;

/// File bytes returned by the workspace source, plus enough
/// metadata to explain truncation to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileContent {
    pub data: Vec<u8>,
    /// Full size at the source; can exceed `data.len()` when truncated.
    pub total_size: u64,
    pub truncated: bool,
}

/// An opaque location token. UI code may display it and pass it back to the
/// source that produced it, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcePath {
    Local(PathBuf),
    Remote(String),
}

impl SourcePath {
    /// Human-readable form for headers/breadcrumbs and persistence.
    pub fn display(&self) -> String {
        match self {
            SourcePath::Local(path) => path.to_string_lossy().to_string(),
            SourcePath::Remote(path) => path.clone(),
        }
    }

    /// The local path, if this token came from a local source. Boundary code
    /// (file open/edit pipelines) uses this to enter local-only flows; UI
    /// rendering code should not.
    pub fn as_local(&self) -> Option<&std::path::Path> {
        match self {
            SourcePath::Local(path) => Some(path),
            SourcePath::Remote(_) => None,
        }
    }
}

/// What a source can do today. The UI gates affordances (open, edit, git
/// panel, session launchers) on these flags — never on the source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub open_file: bool,
    pub edit_file: bool,
    pub git_status: bool,
    pub git_diff: bool,
    pub git_worktrees: bool,
    pub sessions: bool,
}

impl SourceCapabilities {
    /// No capabilities — used when a workspace has no usable source (e.g.
    /// the legacy SSH prototype).
    pub fn none() -> Self {
        Self {
            open_file: false,
            edit_file: false,
            git_status: false,
            git_diff: false,
            git_worktrees: false,
            sessions: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDirEntry {
    pub name: String,
    pub path: SourcePath,
    pub is_dir: bool,
    /// A directory holding a `.git` entry. Remote agents that predate the
    /// field always report false.
    pub is_git_repo: bool,
}

/// One directory listing, shaped identically for every source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDirListing {
    pub tab_id: usize,
    pub current_dir: SourcePath,
    /// Header/breadcrumb text, already formatted by the source.
    pub display_dir: String,
    /// `None` at the workspace root — drives the Up button with no path
    /// logic in the UI.
    pub parent: Option<SourcePath>,
    pub entries: Vec<SourceDirEntry>,
    pub error: Option<String>,
}

/// File-browser state for one tab, fed exclusively by the workspace source
/// listings. The `seq` counter drops stale responses after rapid navigation
/// without comparing paths.
#[derive(Debug, Clone)]
pub struct FilesState {
    pub dir: SourcePath,
    pub display_dir: String,
    pub parent: Option<SourcePath>,
    pub entries: Vec<SourceDirEntry>,
    pub loading: bool,
    pub error: Option<String>,
    pub seq: u64,
}

impl FilesState {
    pub fn at(dir: SourcePath) -> Self {
        Self {
            display_dir: format!("{}/", dir.display()),
            dir,
            parent: None,
            entries: Vec::new(),
            loading: false,
            error: None,
            seq: 0,
        }
    }

    /// Point the browser at `dir` and mark it loading. Returns the request
    /// sequence number to thread through to [`FilesState::apply`].
    pub fn begin_request(&mut self, dir: SourcePath) -> u64 {
        self.dir = dir;
        self.loading = true;
        self.error = None;
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Apply a completed listing; ignored if a newer request superseded it.
    pub fn apply(&mut self, seq: u64, listing: SourceDirListing) {
        if seq != self.seq {
            return;
        }
        self.loading = false;
        self.dir = listing.current_dir;
        self.display_dir = listing.display_dir;
        self.parent = listing.parent;
        self.entries = listing.entries;
        self.error = listing.error;
    }
}

/// Git status shaped identically for every source. `root` is the canonical
/// repo root the source actually used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGitStatus {
    pub root: SourcePath,
    pub is_git_repo: bool,
    pub repo_name: String,
    pub branch_name: String,
    pub staged: Vec<SourceGitFile>,
    pub unstaged: Vec<SourceGitFile>,
    pub untracked: Vec<SourceGitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGitFile {
    pub path: String,
    pub status: String,
    pub is_staged: bool,
}
