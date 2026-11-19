//! Owner-scoped review refresh policy; independent of the dock and executor.
use crate::workspace_status::LocalWorkspace;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotKey {
    root: std::path::PathBuf,
    files: Vec<(usize, String, String, bool)>,
}
impl SnapshotKey {
    pub fn new(workspace: &LocalWorkspace) -> Self {
        let status = &workspace.status;
        let mut files: Vec<_> = [&status.staged, &status.unstaged, &status.untracked]
            .into_iter()
            .enumerate()
            .flat_map(|(group, files)| {
                files.iter().map(move |file| {
                    (
                        group,
                        file.path.clone(),
                        file.status.clone(),
                        file.is_staged,
                    )
                })
            })
            .collect();
        files.sort();
        Self {
            root: workspace.root.clone(),
            files,
        }
    }
}

#[derive(Default)]
pub(crate) struct ReviewReload {
    pub visible: bool,
    last_loaded: Option<SnapshotKey>,
    running: bool,
    pending: bool,
}
impl ReviewReload {
    /// True means start one load. Clicks may request an initial load or retry.
    pub fn request(&mut self, key: SnapshotKey, force: bool) -> bool {
        self.pending |= force;
        if !self.visible || (!self.pending && self.last_loaded.as_ref() == Some(&key)) {
            return false;
        }
        if self.running {
            self.pending = true;
            return false;
        }
        self.running = true;
        self.pending = false;
        // Record the status that triggered this attempt, including failed loads;
        // errors are retried by clicking rather than on every maintenance tick.
        self.last_loaded = Some(key);
        true
    }
    pub fn finish(&mut self) {
        self.running = false;
    }
}

#[cfg(test)]
mod tests;
