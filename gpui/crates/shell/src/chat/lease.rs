//! Persistent inode, exclusive kernel lock. Marker contents never grant ownership.
use anyhow::{Context, Result, ensure};
use gitterm_core::preview::Preview;
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

pub(super) struct ProviderLease {
    file: File,
    path: std::path::PathBuf,
}
impl ProviderLease {
    pub fn claim(preview: &Preview) -> Result<Self> {
        let path = preview.provider().join(".writer");
        preview.scoped(&path)?;
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                check(&path)?;
                OpenOptions::new().read(true).write(true).open(&path)?
            }
            Err(e) => return Err(e.into()),
        };
        file.try_lock().context("Claude provider root already owned by another preview; close and drain that chat first")?;
        let lease = Self { file, path };
        lease.validate(preview)?;
        Ok(lease)
    }
    pub fn validate(&self, preview: &Preview) -> Result<()> {
        preview.scoped(&self.path)?;
        let actual = check(&self.path)?;
        let opened = self.file.metadata()?;
        ensure!(
            actual.dev() == opened.dev() && actual.ino() == opened.ino(),
            "Claude provider lock replaced"
        );
        Ok(())
    }
}
fn check(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o077 == 0,
        "Claude provider lock must be a private, unaliased regular file"
    );
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_stale_marker_has_no_authority_and_aliases_refuse() {
        let (_dir, preview) = super::super::tests::fixture();
        let provider =
            super::super::provider::Provider::new(preview.clone(), Path::new("/fake/claude"))
                .unwrap();
        provider.prepare().unwrap();
        let path = preview.provider().join(".writer");
        let first = ProviderLease::claim(&preview).unwrap();
        assert!(ProviderLease::claim(&preview).is_err());
        drop(first);
        fs::write(&path, "stale pid 1 is never signalled").unwrap();
        drop(ProviderLease::claim(&preview).unwrap());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(preview.root().join("missing"), &path).unwrap();
        assert!(ProviderLease::claim(&preview).is_err());
        fs::remove_file(&path).unwrap();
        let other = preview.provider().join("other");
        fs::write(&other, "").unwrap();
        fs::hard_link(&other, &path).unwrap();
        assert!(ProviderLease::claim(&preview).is_err());
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    #[test]
    fn independent_process_observes_the_same_kernel_lease() {
        let (_dir, preview) = super::super::tests::fixture();
        super::super::provider::Provider::new(preview.clone(), Path::new("/fake/claude"))
            .unwrap()
            .prepare()
            .unwrap();
        let check = |held: bool| {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::lease::process_tests::lease_child"])
                .env("GITTERM_C8_TEST_ROOT", preview.root())
                .env("GITTERM_C8_TEST_HELD", if held { "yes" } else { "no" })
                .status()
                .unwrap();
            assert!(status.success());
        };
        let lease = ProviderLease::claim(&preview).unwrap();
        check(true);
        drop(lease);
        check(false);
    }
    #[test]
    fn lease_child() {
        let Some(root) = std::env::var_os("GITTERM_C8_TEST_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let parent = root.parent().unwrap();
        let preview = Preview::validate(
            &root,
            gitterm_core::preview::ProtectedRoots::new(
                &parent.join("host"),
                [parent.join("production")],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            ProviderLease::claim(&preview).is_err(),
            std::env::var("GITTERM_C8_TEST_HELD").unwrap() == "yes"
        );
    }
}
