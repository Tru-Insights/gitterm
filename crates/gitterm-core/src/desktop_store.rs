//! Serialized persistence authority for the two shared desktop documents.
//!
//! Construction and receipt waits perform blocking IO: use a background executor.
//! Clones submit to the same owner thread. Stop producers before awaiting `drain`
//! at shutdown; it acknowledges all earlier commands and retries dirty saves.
use crate::settings::DesktopSettings;
use crate::state_lock::StateLock;
use crate::workspace_config::WorkspacesFile;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

#[derive(Debug, Clone)]
pub enum LoadState<T> {
    Missing,
    Valid(T),
    Refused(String),
}

#[derive(Debug, Clone)]
pub struct FileState<T> {
    pub loaded: LoadState<T>,
    pub dirty: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub workspaces: FileState<WorkspacesFile>,
    pub settings: FileState<DesktopSettings>,
}

/// A queued command's acknowledgement. Dropping it does not cancel the command.
pub struct Receipt<T>(mpsc::Receiver<Result<T, String>>);
impl<T> Receipt<T> {
    pub fn wait(self) -> Result<T, String> {
        self.0
            .recv()
            .map_err(|_| "desktop persistence owner stopped before acknowledgement".to_string())?
    }
}

type Operation = Box<dyn FnOnce(&mut Snapshot, &Path) + Send>;
enum Command {
    Run(Operation),
    Stop,
}
struct Inner {
    root: PathBuf,
    sender: mpsc::Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(worker) = self.worker.get_mut().unwrap().take() {
            // All commands ahead of Stop finish before releasing the root lease.
            if worker.thread().id() != std::thread::current().id() && worker.join().is_err() {
                eprintln!(
                    "desktop persistence owner panicked: {}",
                    self.root.display()
                );
            }
        }
    }
}

/// The full documents are private to one thread, never rebuilt from a UI catalog.
#[derive(Clone)]
pub struct DesktopStore(Arc<Inner>);
impl DesktopStore {
    pub fn open_desktop() -> Result<Self, String> {
        Self::open(crate::config_paths::desktop_config_dir().map_err(|e| e.to_string())?)
    }

    pub fn open(root: &Path) -> Result<Self, String> {
        Self::with_lock(Arc::new(
            StateLock::acquire(root).map_err(|e| e.to_string())?,
        ))
    }

    /// Share the app lifetime's existing root lock. A second store using the
    /// same lease is refused too; consumers must clone the existing store.
    pub fn with_lock(lock: Arc<StateLock>) -> Result<Self, String> {
        let claim = Claim::new(lock.identity())?;
        let root = lock.root().to_path_buf();
        let snapshot = Snapshot {
            workspaces: load(&root.join("workspaces.json")),
            settings: load(&root.join("settings.json")),
        };
        let (sender, receiver) = mpsc::channel();
        let worker_root = root.clone();
        let worker = std::thread::Builder::new()
            .name("desktop-store".into())
            .spawn(move || {
                let (_lock, _claim) = (lock, claim);
                let mut snapshot = snapshot;
                while let Ok(Command::Run(operation)) = receiver.recv() {
                    operation(&mut snapshot, &worker_root);
                }
            })
            .map_err(|e| format!("start desktop persistence owner: {e}"))?;
        Ok(Self(Arc::new(Inner {
            root,
            sender,
            worker: Mutex::new(Some(worker)),
        })))
    }

    pub fn root(&self) -> &Path {
        &self.0.root
    }

    fn submit<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Snapshot, &Path) -> Result<T, String> + Send + 'static,
    ) -> Receipt<T> {
        let (sender, receiver) = mpsc::channel();
        // On send failure, dropping the boxed operation disconnects its reply.
        let _ = self
            .0
            .sender
            .send(Command::Run(Box::new(move |state, root| {
                let result = operation(state, root);
                let _ = sender.send(result);
            })));
        Receipt(receiver)
    }

    pub fn snapshot(&self) -> Receipt<Snapshot> {
        self.submit(|state, _| Ok(state.clone()))
    }

    /// A Missing file gains defaults only through an explicit mutation. A
    /// Refused file never gains defaults or write authority in this owner.
    pub fn edit_workspaces(
        &self,
        edit: impl FnOnce(&mut WorkspacesFile) + Send + 'static,
    ) -> Receipt<()> {
        self.submit(move |state, root| {
            mutate(&mut state.workspaces, edit)?;
            save(&mut state.workspaces, &root.join("workspaces.json"))
        })
    }

    /// Repo actions publish only after the atomic replacement is acknowledged.
    pub fn try_edit_workspaces(
        &self,
        edit: impl FnOnce(&mut WorkspacesFile) -> Result<(), String> + Send + 'static,
    ) -> Receipt<WorkspacesFile> {
        self.transact_workspaces(move |file| {
            edit(file)?;
            Ok(file.clone())
        })
    }

    /// Save a candidate before publishing it. A rejected edit or failed atomic
    /// replacement leaves both the owner snapshot and the previous file intact.
    pub fn transact_workspaces<T: Send + 'static>(
        &self,
        edit: impl FnOnce(&mut WorkspacesFile) -> Result<T, String> + Send + 'static,
    ) -> Receipt<T> {
        self.submit(move |state, root| {
            let mut candidate = match &state.workspaces.loaded {
                LoadState::Missing => WorkspacesFile::default(),
                LoadState::Valid(document) => document.clone(),
                LoadState::Refused(error) => return Err(error.clone()),
            };
            let result = edit(&mut candidate)?;
            let unchanged = match &state.workspaces.loaded {
                LoadState::Valid(previous) => serde_json::to_value(previous)
                    .and_then(|previous| {
                        serde_json::to_value(&candidate).map(|next| previous == next)
                    })
                    .map_err(|error| format!("compare workspace edit: {error}"))?,
                _ => false,
            };
            // No-op controls must preserve the original serialized bytes too.
            if unchanged && !state.workspaces.dirty {
                return Ok(result);
            }
            let mut pending = FileState {
                loaded: LoadState::Valid(candidate),
                dirty: true,
                error: None,
            };
            save(&mut pending, &root.join("workspaces.json"))?;
            state.workspaces = pending;
            Ok(result)
        })
    }

    pub fn edit_settings(
        &self,
        edit: impl FnOnce(&mut DesktopSettings) + Send + 'static,
    ) -> Receipt<()> {
        self.submit(move |state, root| {
            mutate(&mut state.settings, edit)?;
            // Iced omits the current root's default, even when explicitly set.
            if let LoadState::Valid(settings) = &mut state.settings.loaded {
                if settings.task_worktree_root.0.as_ref() == Some(&root.join("worktrees")) {
                    settings.task_worktree_root.0 = None;
                }
            }
            save(&mut state.settings, &root.join("settings.json"))
        })
    }

    pub fn save_workspaces(&self) -> Receipt<()> {
        self.submit(|state, root| save(&mut state.workspaces, &root.join("workspaces.json")))
    }

    pub fn save_settings(&self) -> Receipt<()> {
        self.submit(|state, root| save(&mut state.settings, &root.join("settings.json")))
    }

    pub fn drain(&self) -> Receipt<()> {
        self.submit(|state, root| {
            // Try both files even if one writer is blocked.
            let workspaces = save(&mut state.workspaces, &root.join("workspaces.json"));
            let settings = save(&mut state.settings, &root.join("settings.json"));
            let errors: Vec<_> = [workspaces, settings]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        })
    }
}

// StateLock excludes other processes. This registry excludes duplicate owners
// sharing the same app lease, using filesystem identity rather than path spelling.
fn claims() -> &'static Mutex<HashSet<(u64, u64)>> {
    static CLAIMS: OnceLock<Mutex<HashSet<(u64, u64)>>> = OnceLock::new();
    CLAIMS.get_or_init(Mutex::default)
}
struct Claim((u64, u64));
impl Claim {
    fn new(identity: (u64, u64)) -> Result<Self, String> {
        if !claims().lock().unwrap().insert(identity) {
            return Err("desktop persistence owner already exists for this state root".into());
        }
        Ok(Self(identity))
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        claims().lock().unwrap().remove(&self.0);
    }
}

fn load<T: DeserializeOwned>(path: &Path) -> FileState<T> {
    let loaded = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(document) => LoadState::Valid(document),
            Err(error) => LoadState::Refused(format!("parse {}: {error}", path.display())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
                    LoadState::Missing
                }
                _ => LoadState::Refused(format!("read {}: {error}", path.display())),
            }
        }
        Err(error) => LoadState::Refused(format!("read {}: {error}", path.display())),
    };
    let error = match &loaded {
        LoadState::Refused(error) => Some(error.clone()),
        _ => None,
    };
    FileState {
        loaded,
        dirty: false,
        error,
    }
}

fn mutate<T: Default>(state: &mut FileState<T>, edit: impl FnOnce(&mut T)) -> Result<(), String> {
    if let LoadState::Refused(error) = &state.loaded {
        return Err(error.clone());
    }
    if matches!(state.loaded, LoadState::Missing) {
        state.loaded = LoadState::Valid(T::default());
    }
    if let LoadState::Valid(document) = &mut state.loaded {
        // Mark before invoking user code so unwinding can never claim it saved.
        state.dirty = true;
        edit(document);
    }
    Ok(())
}

fn save<T: Serialize>(state: &mut FileState<T>, path: &Path) -> Result<(), String> {
    if let LoadState::Refused(error) = &state.loaded {
        return Err(error.clone());
    }
    // Iced uses randomized HashMaps. Avoid rewriting an unchanged document,
    // preserving its exact bytes rather than reordering environment keys.
    if !state.dirty {
        return Ok(());
    }
    let result = (|| {
        let LoadState::Valid(document) = &state.loaded else {
            return Err(format!("no document loaded for {}", path.display()));
        };
        let bytes = serde_json::to_vec_pretty(document)
            .map_err(|e| format!("encode {}: {e}", path.display()))?;
        crate::atomic_file::write_atomically(path, &bytes)
    })();
    match result {
        Ok(()) => {
            state.dirty = false;
            state.error = None;
            Ok(())
        }
        Err(error) => {
            state.error = Some(error.clone());
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn valid<T: std::fmt::Debug>(state: FileState<T>) -> T {
        match state.loaded {
            LoadState::Valid(document) => document,
            other => panic!("expected valid document: {other:?}"),
        }
    }

    #[test]
    fn missing_requires_an_edit_and_only_one_owner_can_use_a_root() {
        let temp = tempfile::tempdir().unwrap();
        let lease = Arc::new(StateLock::acquire(temp.path()).unwrap());
        let owner = DesktopStore::with_lock(lease.clone()).unwrap();
        assert!(DesktopStore::with_lock(lease.clone()).is_err());
        assert!(DesktopStore::open(temp.path()).is_err());
        assert!(matches!(
            owner.snapshot().wait().unwrap().settings.loaded,
            LoadState::Missing
        ));
        owner.drain().wait().unwrap();
        assert!(!temp.path().join("settings.json").exists());
        owner
            .edit_settings(|s| s.theme = "light".into())
            .wait()
            .unwrap();
        let snapshot = owner.snapshot().wait().unwrap();
        assert_eq!(valid(snapshot.settings).theme, "light");
        assert!(matches!(snapshot.workspaces.loaded, LoadState::Missing));
        drop(owner);
        DesktopStore::with_lock(lease).unwrap();
        assert!(!temp.path().join("profiles.json").exists());
        assert!(!temp.path().join("remote-agents.json").exists());
    }

    #[test]
    fn refused_files_never_gain_write_authority_but_other_files_can_save() {
        for file in ["workspaces.json", "settings.json"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(file);
            let bytes = b"{ malformed fixture";
            std::fs::write(&path, bytes).unwrap();
            let owner = DesktopStore::open(temp.path()).unwrap();
            let snapshot = owner.snapshot().wait().unwrap();
            if file == "workspaces.json" {
                assert!(matches!(snapshot.workspaces.loaded, LoadState::Refused(_)));
                assert!(owner
                    .edit_workspaces(|_| panic!("must not edit"))
                    .wait()
                    .is_err());
                assert!(owner.save_workspaces().wait().is_err());
                owner
                    .edit_settings(|s| s.theme = "light".into())
                    .wait()
                    .unwrap();
            } else {
                assert!(matches!(snapshot.settings.loaded, LoadState::Refused(_)));
                assert!(owner
                    .edit_settings(|_| panic!("must not edit"))
                    .wait()
                    .is_err());
                assert!(owner.save_settings().wait().is_err());
                owner
                    .edit_workspaces(|s| s.active_workspace = 7)
                    .wait()
                    .unwrap();
            }
            assert!(owner.drain().wait().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            // Repairing externally cannot silently re-authorize a refused owner.
            std::fs::write(&path, b"{}").unwrap();
            assert!(owner.drain().wait().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        }
    }

    #[test]
    fn unreadable_document_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("settings.json")).unwrap();
        let owner = DesktopStore::open(temp.path()).unwrap();
        assert!(matches!(
            owner.snapshot().wait().unwrap().settings.loaded,
            LoadState::Refused(_)
        ));
        assert!(owner
            .edit_settings(|_| panic!("must not edit"))
            .wait()
            .is_err());
        assert!(temp.path().join("settings.json").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_refused_instead_of_becoming_missing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::os::unix::fs::symlink(temp.path().join("absent"), &path).unwrap();
        let owner = DesktopStore::open(temp.path()).unwrap();
        assert!(matches!(
            owner.snapshot().wait().unwrap().settings.loaded,
            LoadState::Refused(_)
        ));
        assert!(owner
            .edit_settings(|_| panic!("must not edit"))
            .wait()
            .is_err());
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn failed_saves_keep_dirty_document_and_error_until_retry_succeeds() {
        for file in ["workspaces.json", "settings.json"] {
            let temp = tempfile::tempdir().unwrap();
            let owner = DesktopStore::open(temp.path()).unwrap();
            let path = temp.path().join(file);
            // Block atomic replacement after load; works even as a privileged user.
            std::fs::create_dir(&path).unwrap();
            let error = if file == "workspaces.json" {
                owner
                    .edit_workspaces(|s| s.active_workspace = 9)
                    .wait()
                    .unwrap_err()
            } else {
                owner
                    .edit_settings(|s| s.theme = "light".into())
                    .wait()
                    .unwrap_err()
            };
            assert!(error.contains(file), "{error}");
            let snapshot = owner.snapshot().wait().unwrap();
            if file == "workspaces.json" {
                assert!(snapshot.workspaces.dirty);
                assert_eq!(snapshot.workspaces.error, Some(error));
                assert_eq!(valid(snapshot.workspaces).active_workspace, 9);
            } else {
                assert!(snapshot.settings.dirty);
                assert_eq!(snapshot.settings.error, Some(error));
                assert_eq!(valid(snapshot.settings).theme, "light");
            }
            assert!(owner.drain().wait().is_err());
            std::fs::remove_dir(&path).unwrap();
            owner.drain().wait().unwrap();
            let snapshot = owner.snapshot().wait().unwrap();
            assert!(!snapshot.settings.dirty && snapshot.settings.error.is_none());
            assert!(!snapshot.workspaces.dirty && snapshot.workspaces.error.is_none());
            assert!(path.is_file());
        }
    }

    #[test]
    fn concurrent_mutations_are_serialized_without_lost_updates() {
        let temp = tempfile::tempdir().unwrap();
        let owner = DesktopStore::open(temp.path()).unwrap();
        let workers: Vec<_> = (0..12)
            .map(|_| {
                let owner = owner.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        owner
                            .edit_workspaces(|file| file.active_workspace += 1)
                            .wait()
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        owner.drain().wait().unwrap();
        assert_eq!(
            valid(owner.snapshot().wait().unwrap().workspaces).active_workspace,
            120
        );
        let saved: WorkspacesFile =
            serde_json::from_slice(&std::fs::read(temp.path().join("workspaces.json")).unwrap())
                .unwrap();
        assert_eq!(saved.active_workspace, 120);
    }

    #[test]
    fn last_handle_can_be_dropped_inside_a_queued_edit() {
        let temp = tempfile::tempdir().unwrap();
        let owner = DesktopStore::open(temp.path()).unwrap();
        let captured = owner.clone();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let saved = owner.edit_workspaces(move |file| {
            entered.send(()).unwrap();
            gate.recv().unwrap();
            drop(captured);
            file.active_workspace = 3;
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(owner);
        release.send(()).unwrap();
        saved.wait().unwrap();
        let file: WorkspacesFile =
            serde_json::from_slice(&std::fs::read(temp.path().join("workspaces.json")).unwrap())
                .unwrap();
        assert_eq!(file.active_workspace, 3);
    }

    // Serialization is part of an in-flight save. Gate it without relying on
    // disk speed, sleep-based scheduling, or adding a production writer hook.
    struct GatedDocument {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Serialize for GatedDocument {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            serializer.serialize_str("saved")
        }
    }

    #[test]
    fn drain_waits_for_in_flight_save_and_acknowledges_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let owner = DesktopStore::open(temp.path()).unwrap();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let save = owner.submit(move |_, root| {
            let mut file = FileState {
                loaded: LoadState::Valid(GatedDocument {
                    entered,
                    release: Mutex::new(gate),
                }),
                dirty: true,
                error: None,
            };
            super::save(&mut file, &root.join("workspaces.json"))
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let drain = owner.drain();
        let pending = matches!(drain.0.try_recv(), Err(mpsc::TryRecvError::Empty));
        let absent = !temp.path().join("workspaces.json").exists();
        release.send(()).unwrap();
        save.wait().unwrap();
        assert!(
            pending,
            "drain acknowledged before the in-flight save finished"
        );
        assert!(absent);
        drain.wait().unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("workspaces.json")).unwrap(),
            b"\"saved\""
        );
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;

    #[test]
    fn refused_candidate_leaves_file_and_snapshot_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let bytes = br#"{"workspaces":[],"active_workspace":7}"#;
        std::fs::write(root.path().join("workspaces.json"), bytes).unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        let result = store
            .transact_workspaces(|file| {
                file.active_workspace = 99;
                Err::<(), _>("cancelled candidate".into())
            })
            .wait();
        assert_eq!(result.unwrap_err(), "cancelled candidate");
        let snapshot = store.snapshot().wait().unwrap();
        assert!(!snapshot.workspaces.dirty);
        let LoadState::Valid(file) = snapshot.workspaces.loaded else {
            panic!("valid file")
        };
        assert_eq!(file.active_workspace, 7);
        store.drain().wait().unwrap();
        assert_eq!(
            std::fs::read(root.path().join("workspaces.json")).unwrap(),
            bytes
        );
    }
}
