use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::model::ReviewFile;

/// The shared-config file holding viewed marks.
pub const VIEWED_FILE: &str = "review-viewed.json";

/// Whether the version shown was marked viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewedState {
    Unviewed,
    Viewed,
    /// Marked viewed, but the content changed since: shown expanded.
    ChangedSinceViewed,
}

/// `review-viewed.json`: canonical checkout -> file path -> content id of
/// the version viewed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewedMarks {
    #[serde(default)]
    pub checkouts: BTreeMap<String, BTreeMap<String, String>>,
}

fn checkout_key(checkout: &Path) -> String {
    checkout.to_string_lossy().into_owned()
}

impl ViewedMarks {
    /// Reads the marks from `dir`; none when the file is absent. A file that
    /// cannot be read or parsed is an error, so it is never overwritten.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(VIEWED_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| format!("cannot parse {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode viewed marks: {error}"))
    }

    pub fn state(&self, checkout: &Path, file: &ReviewFile) -> ViewedState {
        let Some(marked) = self
            .checkouts
            .get(&checkout_key(checkout))
            .and_then(|files| files.get(&file.path))
        else {
            return ViewedState::Unviewed;
        };
        if file.content_id.as_deref() == Some(marked.as_str()) {
            ViewedState::Viewed
        } else {
            ViewedState::ChangedSinceViewed
        }
    }

    /// Marks `file` viewed at its current content, or clears its mark.
    /// Returns false when a file without a content id is marked: there is
    /// nothing to compare a later version with.
    pub fn set(&mut self, checkout: &Path, file: &ReviewFile, viewed: bool) -> bool {
        let key = checkout_key(checkout);
        if !viewed {
            if let Some(files) = self.checkouts.get_mut(&key) {
                files.remove(&file.path);
                if files.is_empty() {
                    self.checkouts.remove(&key);
                }
            }
            return true;
        }
        let Some(id) = &file.content_id else {
            return false;
        };
        self.checkouts
            .entry(key)
            .or_default()
            .insert(file.path.clone(), id.clone());
        true
    }

    /// Drops the checkout's marks for files in no diff of it. Returns
    /// whether anything changed.
    pub fn prune(&mut self, checkout: &Path, live: &BTreeSet<String>) -> bool {
        let key = checkout_key(checkout);
        let Some(files) = self.checkouts.get_mut(&key) else {
            return false;
        };
        let before = files.len();
        files.retain(|path, _| live.contains(path));
        let changed = files.len() != before;
        if files.is_empty() {
            self.checkouts.remove(&key);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_keeps_exact_bytes_defaults_and_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VIEWED_FILE);
        let marks = ViewedMarks {
            checkouts: BTreeMap::from([(
                "/synthetic/checkout".to_string(),
                BTreeMap::from([
                    ("src/z.rs".to_string(), "deleted:old".to_string()),
                    ("src/a.rs".to_string(), "blob-a".to_string()),
                ]),
            )]),
        };
        let expected = br#"{
  "checkouts": {
    "/synthetic/checkout": {
      "src/a.rs": "blob-a",
      "src/z.rs": "deleted:old"
    }
  }
}"#;
        assert_eq!(marks.encode().unwrap(), expected);
        std::fs::write(&path, expected).unwrap();
        assert_eq!(ViewedMarks::load(dir.path()).unwrap(), marks);
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(
            ViewedMarks::load(dir.path()).unwrap(),
            ViewedMarks::default()
        );
        std::fs::write(&path, "not json").unwrap();
        assert!(ViewedMarks::load(dir.path())
            .unwrap_err()
            .starts_with("cannot parse "));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }
}
