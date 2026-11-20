use anyhow::{Context, Result, bail};
use std::{fs, path::Path};
// https://code.claude.com/docs/en/managed-settings#where-each-mechanism-stores-the-policy
// Refuse whole policy directories, including unknown future managed artifacts.
const LOCATIONS: &[&str] = &[
    "Library/Application Support/ClaudeCode",
    "Library/Managed Preferences",
    "Library/Preferences/com.anthropic.claudecode.plist",
    "etc/claude-code",
];
pub(super) fn check(root: &Path) -> Result<()> {
    for location in LOCATIONS {
        let path = root.join(location);
        match fs::symlink_metadata(&path) {
            Ok(_) => bail!(
                "Claude preview refuses managed settings location {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("inspect managed settings {}", path.display()));
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_managed_entry_including_dangling_symlinks_refuses() {
        let dir = tempfile::tempdir().unwrap();
        check(dir.path()).unwrap();
        for location in LOCATIONS {
            let path = dir.path().join(location);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(dir.path().join("missing"), &path).unwrap();
            assert!(check(dir.path()).is_err());
            fs::remove_file(path).unwrap();
        }
    }
}
