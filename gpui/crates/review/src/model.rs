use gitterm_core::status::GitStatusSnapshot;
use std::{collections::BTreeMap, sync::Arc};

/// A path can have different staged and unstaged patches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Staged,
    Unstaged,
    Untracked,
}
impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Self::Staged => "Staged",
            Self::Unstaged => "Unstaged",
            Self::Untracked => "Untracked",
        }
    }
}
pub type FileKey = (Group, String);
pub type Patches = BTreeMap<FileKey, Arc<str>>;

/// Match the Iced review's initial per-file text limits; explicit expansion is unrestricted.
pub const LARGE_LINES: usize = 2_000;
pub const LARGE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub struct ReviewFile {
    pub key: FileKey,
    pub status: String,
    pub patch: Arc<str>,
    pub viewed: bool,
    pub expanded: bool,
}
impl ReviewFile {
    pub fn is_binary(&self) -> bool {
        self.patch
            .lines()
            .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
    }
    pub fn is_large(&self) -> bool {
        self.patch.len() > LARGE_BYTES
            || self.patch.lines().take(LARGE_LINES + 1).count() > LARGE_LINES
    }
    pub fn collapsed(&self) -> bool {
        self.is_large() && !self.expanded && !self.is_binary()
    }
    pub fn title(&self) -> String {
        let old = self
            .patch
            .lines()
            .find_map(|s| s.strip_prefix("rename from "));
        let new = self
            .patch
            .lines()
            .find_map(|s| s.strip_prefix("rename to "));
        match (old, new) {
            (Some(old), Some(new)) => format!("{old} → {new}"),
            _ => self.key.1.clone(),
        }
    }
}

#[derive(Default)]
pub struct ReviewModel {
    pub files: Vec<ReviewFile>,
    pub selected: Option<usize>,
    pub error: Option<String>,
}
impl ReviewModel {
    pub fn new(snapshot: &GitStatusSnapshot, patches: &Patches) -> Result<Self, String> {
        let mut model = Self::default();
        model.replace(snapshot, patches)?;
        Ok(model)
    }
    /// Atomic replacement: missing patches leave the current review intact. Exact patch
    /// equality preserves viewed/expansion state; any change clears both for that entry.
    pub fn replace(
        &mut self,
        snapshot: &GitStatusSnapshot,
        patches: &Patches,
    ) -> Result<(), String> {
        let selected = self.selected.map(|ix| self.files[ix].key.clone());
        let mut files = Vec::new();
        for (group, statuses) in [
            (Group::Staged, &snapshot.staged),
            (Group::Unstaged, &snapshot.unstaged),
            (Group::Untracked, &snapshot.untracked),
        ] {
            for status in statuses {
                let key = (group, status.path.clone());
                if files.iter().any(|file: &ReviewFile| file.key == key) {
                    return Err(format!(
                        "duplicate {} patch for {}",
                        group.label(),
                        status.path
                    ));
                }
                let patch = patches.get(&key).ok_or_else(|| {
                    format!("missing {} patch for {}", group.label(), status.path)
                })?;
                let previous = self
                    .files
                    .iter()
                    .find(|file| file.key == key && file.patch == *patch);
                files.push(ReviewFile {
                    key,
                    status: status.status.clone(),
                    patch: patch.clone(),
                    viewed: previous.is_some_and(|file| file.viewed),
                    expanded: previous.is_some_and(|file| file.expanded),
                });
            }
        }
        self.selected = selected
            .and_then(|key| files.iter().position(|file| file.key == key))
            .or_else(|| (!files.is_empty()).then_some(0));
        self.files = files;
        self.error = snapshot.error.clone();
        Ok(())
    }
    pub fn navigate(&mut self, previous: bool) {
        if let Some(index) = self.selected {
            self.selected = Some(if previous {
                index.saturating_sub(1)
            } else {
                (index + 1).min(self.files.len() - 1)
            });
        }
    }
}

#[cfg(test)]
mod tests;
