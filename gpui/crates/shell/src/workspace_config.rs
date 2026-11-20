//! Explicit GPUI folders only; no import from the Iced workspace store.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Folders {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    preview_chats: Vec<crate::chat::SavedChat>,
    version: u32,
    folders: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parked_workspaces: Vec<crate::park::SavedPark>,
}

pub fn local_folder(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve folder {}", path.display()))?;
    ensure!(
        path.is_dir(),
        "workspace is not a folder: {}",
        path.display()
    );
    Ok(path)
}

pub struct WorkspaceStore {
    pub(crate) chats: Vec<crate::chat::SavedChat>,
    pub(crate) parked: Vec<crate::park::SavedPark>,
    path: PathBuf,
    refused: bool,
    pub(crate) guard: Option<std::sync::Arc<crate::preview::Writer>>,
}
impl WorkspaceStore {
    #[cfg(test)]
    pub fn new(config: &Path) -> Self {
        Self {
            chats: vec![],
            parked: vec![],
            path: config.join("local-workspaces.json"),
            refused: false,
            guard: None,
        }
    }
    pub(crate) fn guarded(guard: std::sync::Arc<crate::preview::Writer>) -> Self {
        Self {
            chats: vec![],
            parked: vec![],
            path: guard.preview().layout().join("local-workspaces.json"),
            refused: false,
            guard: Some(guard),
        }
    }
    pub fn load(&mut self) -> Result<Vec<PathBuf>> {
        let result = (|| {
            if let Some(guard) = &self.guard {
                guard.preview().scoped(&self.path)?;
            }
            let bytes = match std::fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(e.into()),
            };
            let stored: Folders = serde_json::from_slice(&bytes)?;
            ensure!(
                stored.version == 1,
                "unsupported folders version {}",
                stored.version
            );
            let mut seen = std::collections::HashSet::new();
            for path in &stored.folders {
                ensure!(
                    path.is_absolute(),
                    "workspace path must be absolute: {}",
                    path.display()
                );
                ensure!(
                    seen.insert(path),
                    "duplicate workspace path: {}",
                    path.display()
                );
            }
            crate::park::validate_saved(&stored.parked_workspaces)?;
            for chat in &stored.preview_chats {
                chat.validate()?;
            }
            self.chats = stored.preview_chats;
            self.parked = stored.parked_workspaces;
            Ok(stored.folders)
        })()
        .with_context(|| format!("load local folders {}", self.path.display()));
        self.refused = result.is_err();
        result
    }
    fn bytes(&self, folders: &[PathBuf]) -> Result<Vec<u8>> {
        ensure!(
            !self.refused,
            "refusing to overwrite invalid local folders file"
        );
        Ok(serde_json::to_vec_pretty(&Folders {
            preview_chats: self.chats.clone(),
            version: 1,
            folders: folders.to_vec(),
            parked_workspaces: self.parked.clone(),
        })?)
    }
    pub(crate) fn save_async(&self, folders: &[PathBuf]) -> Result<crate::preview::SaveResult> {
        let bytes = self.bytes(folders)?;
        if let Some(guard) = &self.guard {
            return guard.submit(self.path.clone(), bytes);
        }
        #[cfg(test)]
        {
            self.save(folders)?;
            let (reply, receipt) = tokio::sync::oneshot::channel();
            let _ = reply.send(Ok(()));
            Ok(receipt)
        }
        #[cfg(not(test))]
        anyhow::bail!("workspace writer has no preview guard")
    }
    #[cfg(test)]
    pub fn save(&self, folders: &[PathBuf]) -> Result<()> {
        (|| -> Result<()> {
            ensure!(
                !self.refused,
                "refusing to overwrite invalid local folders file"
            );
            let bytes = self.bytes(folders)?;
            if let Some(guard) = &self.guard {
                guard.replace_layout(&self.path, &bytes)?;
            } else {
                #[cfg(not(test))]
                anyhow::bail!("workspace writer has no preview guard");
                #[cfg(test)]
                {
                    let dir = self.path.parent().context("folders file has no parent")?;
                    std::fs::create_dir_all(dir)?;
                    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
                    temp.write_all(&bytes)?;
                    temp.as_file().sync_all()?;
                    temp.persist(&self.path)?;
                }
            }
            Ok(())
        })()
        .with_context(|| format!("save local folders {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folders_roundtrip_and_invalid_files_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::new(dir.path());
        assert!(store.load().unwrap().is_empty());
        let folder = local_folder(dir.path()).unwrap();
        store.save(std::slice::from_ref(&folder)).unwrap();
        assert_eq!(store.load().unwrap(), [folder]);
        for invalid in [
            "{",
            r#"{"version":2,"folders":[]}"#,
            r#"{"version":1,"folders":["relative"]}"#,
            r#"{"version":1,"folders":["/fixture","/fixture"]}"#,
        ] {
            std::fs::write(&store.path, invalid).unwrap();
            assert!(store.load().is_err());
            assert!(store.save(&[]).is_err());
            assert_eq!(std::fs::read_to_string(&store.path).unwrap(), invalid);
        }
        assert!(local_folder(&store.path).is_err());
    }
}
