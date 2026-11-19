//! Display name for a checkout path.

use std::path::Path;

/// A repo's short name: its folder name, else the whole path.
pub fn repo_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}
