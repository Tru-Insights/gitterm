//! Guarded persistence composition; new C2 services have no execution authority.
use anyhow::{Context, Result};
use gitterm_core::preview::{Effect, Preview, ProtectedRoots, WriterLease};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct PreviewContext {
    pub roots: Arc<Preview>,
    writer: Arc<Writer>,
}
impl PreviewContext {
    pub(crate) fn writer(&self) -> Arc<Writer> {
        self.writer.clone()
    }
    pub(crate) fn release_on_quit(&self, cx: &gpui::App) {
        let writer = self.writer();
        cx.on_app_quit(move |_| {
            // GPUI 0.3.12 starts its 200ms future timeout AFTER invoking these
            // callbacks. Drain synchronously here, before returning the future:
            // the independent disk thread needs no App borrow or UI polling.
            // Only final termination waits; ordinary saves/startup never do.
            let result = writer.drain();
            if let Err(error) = result {
                eprintln!(
                    "release preview writer {}: {error:#}",
                    writer.preview().root().display()
                );
            }
            std::future::ready(())
        })
        .detach();
    }
    pub fn from_environment(sample: bool) -> Result<Self> {
        Self::from_roots(roots_from_environment(sample)?)
    }
    pub(crate) fn from_roots(roots: Arc<Preview>) -> Result<Self> {
        let writer = Writer::new(roots.claim_writer().context("open guarded GPUI writer")?);
        Ok(Self {
            roots,
            writer: Arc::new(writer),
        })
    }
}
/// Read-only routes compose the same validated roots without creating any files.
pub fn roots_from_environment(sample: bool) -> Result<Arc<Preview>> {
    let home = dirs::home_dir().context("resolve home for preview protection")?;
    let others = ["GITTERM_V5_CONFIG_DIR", "CLAUDE_CONFIG_DIR", "CODEX_HOME"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
    let overrides = protected_overrides(std::env::var_os("GITTERM_V6_CONFIG_DIR"), others);
    let protected = ProtectedRoots::new(&home, overrides)?;
    let root = config_dir(std::env::var_os("GITTERM_GPUI_CONFIG_DIR"), &home)?;
    let root = if sample { root.join("sample") } else { root };
    let roots = Arc::new(Preview::validate(&root, protected)?);
    anyhow::ensure!(
        roots.root() == root,
        "preview root is aliased: {}",
        root.display()
    );
    Ok(roots)
}
fn protected_overrides(
    v6: Option<OsString>,
    others: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut roots: Vec<_> = others.into_iter().collect();
    // Ignore exactly the overrides Iced ignores. The default V6 root stays
    // protected by ProtectedRoots even when no valid override was supplied.
    roots.extend(gitterm_core::config_paths::resolve_dir_override(
        "GITTERM_V6_CONFIG_DIR",
        v6,
    ));
    roots
}
pub fn config_dir(override_dir: Option<OsString>, home: &Path) -> Result<PathBuf> {
    let root = override_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config/gitterm-gpui"));
    let validated = Preview::validate(&root, ProtectedRoots::new(home, [])?)?;
    anyhow::ensure!(
        validated.root() == root,
        "preview root is aliased: {}",
        root.display()
    );
    Ok(root)
}
/// Stage 1–3 terminal/Git behavior predates C2; every new-service effect stays closed.
pub fn executable(preview: Option<&Preview>, effect: Effect, target: &Path) -> Result<()> {
    if matches!(effect, Effect::Terminal | Effect::Git) {
        return Ok(());
    }
    let Some(preview) = preview else {
        anyhow::bail!("no preview context for {effect:?}: {}", target.display());
    };
    preview.capabilities().authorize(effect, target)?;
    Ok(())
}
/// One FIFO disk queue per app context. Only its thread waits for the OS lease;
/// UI callbacks enqueue snapshots and report completion errors asynchronously.
pub(crate) struct Writer {
    core: Arc<WriterLease>,
    sender: std::sync::Mutex<Option<std::sync::mpsc::Sender<SaveCommand>>>,
    completion: Arc<WriterCompletion>,
}
type WriterCompletion = (
    std::sync::Mutex<Option<std::result::Result<(), String>>>,
    std::sync::Condvar,
);
enum SaveCommand {
    Replace(
        PathBuf,
        Vec<u8>,
        tokio::sync::oneshot::Sender<std::io::Result<()>>,
    ),
    Close(tokio::sync::oneshot::Sender<std::io::Result<()>>),
}
pub(crate) type SaveResult = tokio::sync::oneshot::Receiver<std::io::Result<()>>;
pub(crate) async fn finish(result: Result<SaveResult>) -> Result<()> {
    result?
        .await
        .context("GPUI save queue stopped before completion")??;
    Ok(())
}
impl Writer {
    pub(crate) fn new(core: WriterLease) -> Self {
        let core = Arc::new(core);
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = core.clone();
        let completion = Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
        let finished = completion.clone();
        std::thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    SaveCommand::Replace(path, bytes, reply) => {
                        let result = worker.replace_layout(&path, &bytes).map_err(|error| {
                            std::io::Error::new(
                                error.kind(),
                                format!("save {}: {error}", path.display()),
                            )
                        });
                        // Quit may drain while UI completion tasks cannot run.
                        if let Err(error) = &result {
                            eprintln!("{error}");
                        }
                        let _ = reply.send(result);
                    }
                    SaveCommand::Close(reply) => {
                        let result = worker.close();
                        *finished.0.lock().unwrap() =
                            Some(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                        let _ = reply.send(result);
                        finished.1.notify_all();
                        return;
                    }
                }
            }
            *finished.0.lock().unwrap() = Some(worker.close().map_err(|error| error.to_string()));
            finished.1.notify_all();
        });
        Self {
            core,
            sender: std::sync::Mutex::new(Some(sender)),
            completion,
        }
    }
    pub(crate) fn preview(&self) -> &Arc<Preview> {
        self.core.preview()
    }
    pub(crate) fn submit(&self, path: PathBuf, bytes: Vec<u8>) -> Result<SaveResult> {
        self.preview().scoped(&path)?;
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let sender = self
            .sender
            .lock()
            .map_err(|_| anyhow::anyhow!("GPUI save queue poisoned"))?;
        sender
            .as_ref()
            .context("GPUI writer is closed")?
            .send(SaveCommand::Replace(path, bytes, reply))
            .context("GPUI save queue stopped")?;
        Ok(receipt)
    }
    /// Headless tests only. Production saves use submit/finish.
    #[cfg(test)]
    pub(crate) fn replace_layout(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.submit(path.to_path_buf(), bytes.to_vec())?
            .blocking_recv()??;
        Ok(())
    }
    pub(crate) fn drain(&self) -> Result<()> {
        self.close()?;
        let (done, _) = self
            .completion
            .1
            .wait_timeout_while(
                self.completion.0.lock().unwrap(),
                crate::lifetime::DRAIN_TIMEOUT,
                |done| done.is_none(),
            )
            .unwrap();
        done.as_ref()
            .context("shutdown phase preview-writer: timeout")?
            .clone()
            .map_err(anyhow::Error::msg)
    }
    pub(crate) fn close(&self) -> Result<SaveResult> {
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let mut sender = self
            .sender
            .lock()
            .map_err(|_| anyhow::anyhow!("GPUI save queue poisoned"))?;
        if let Some(sender) = sender.take() {
            sender
                .send(SaveCommand::Close(reply))
                .context("GPUI save queue stopped")?;
        } else {
            let _ = reply.send(Ok(()));
        }
        Ok(receipt)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{layout::LayoutStore, workspace_config::WorkspaceStore};
    pub(crate) fn guarded_store(root: &Path) -> WorkspaceStore {
        let protected =
            ProtectedRoots::new(&root.parent().unwrap().join("fixture-home"), []).unwrap();
        let roots = Arc::new(Preview::validate(root, protected).unwrap());
        WorkspaceStore::guarded(Arc::new(Writer::new(roots.claim_writer().unwrap())))
    }
    fn fixture() -> (tempfile::TempDir, Arc<Writer>) {
        let dir = tempfile::tempdir().unwrap();
        let protected = ProtectedRoots::new(&dir.path().join("host"), []).unwrap();
        let roots = Arc::new(Preview::validate(&dir.path().join("preview"), protected).unwrap());
        let writer = Arc::new(Writer::new(roots.claim_writer().unwrap()));
        (dir, writer)
    }
    #[test]
    fn executable_dispatch_reports_attempted_target_and_never_calls_effect() {
        let (_dir, writer) = fixture();
        let target = writer.preview().history().join("copied-pid-123");
        let mut executed = vec![];
        for effect in [
            Effect::Snapshot,
            Effect::Agent,
            Effect::Worker,
            Effect::Helper,
            Effect::Resume,
            Effect::MutateHistory,
            Effect::RestoreProcess,
        ] {
            let result = executable(Some(writer.preview()), effect, &target)
                .map(|_| executed.push((effect, target.clone())));
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains(target.to_str().unwrap())
            );
        }
        assert!(executed.is_empty());
    }
    #[test]
    fn existing_catalog_and_layout_load_and_save_in_place_including_sample() {
        for sample in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = if sample {
                dir.path().join("config/sample")
            } else {
                dir.path().join("config")
            };
            std::fs::create_dir_all(&root).unwrap();
            let folder = dir.path().join("workspace");
            std::fs::create_dir(&folder).unwrap();
            std::fs::write(
                root.join("local-workspaces.json"),
                serde_json::to_vec(&serde_json::json!({"version":1,"folders":[folder]})).unwrap(),
            )
            .unwrap();
            let root = root.canonicalize().unwrap();
            let path = root.join("layout-1.json");
            let state = gpui_component::dock::DockAreaState {
                version: Some(1),
                center: gpui_component::dock::PanelState {
                    panel_name: "StackPanel".into(),
                    children: vec![],
                    info: gpui_component::dock::PanelInfo::stack(vec![], gpui::Axis::Horizontal),
                },
                ..Default::default()
            };
            std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
            let roots = Arc::new(
                Preview::validate(
                    &root,
                    ProtectedRoots::new(&dir.path().join("host"), []).unwrap(),
                )
                .unwrap(),
            );
            assert_eq!(roots.layout(), root.canonicalize().unwrap());
            let writer = Arc::new(Writer::new(roots.claim_writer().unwrap()));
            let mut folders = WorkspaceStore::guarded(writer.clone());
            assert_eq!(folders.load().unwrap(), vec![folder.clone()]);
            std::fs::remove_dir(&folder).unwrap();
            let (model, save) = crate::load_local_workspaces(&mut folders, vec![]).unwrap();
            assert_eq!(model.folders(), vec![folder]);
            save.blocking_recv().unwrap().unwrap();
            folders.save(&[]).unwrap();
            assert!(folders.load().unwrap().is_empty());
            let mut layout = LayoutStore::guarded(path.clone(), writer);
            // An empty dock is sufficient for the file contract; use its known ids.
            let known = std::collections::HashSet::new();
            assert!(layout.load(&known).unwrap().is_some());
            layout.save(&state).unwrap();
            assert!(path.is_file());
            assert!(!root.join("layout").exists());
        }
    }
    #[test]
    fn startup_catalog_save_does_not_block_app_composition_on_another_writer() {
        let (dir, writer) = fixture();
        let folder = dir.path().join("workspace");
        std::fs::create_dir(&folder).unwrap();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(writer.preview().root().join(".writer"))
            .unwrap();
        blocker.lock().unwrap();
        let mut folders = WorkspaceStore::guarded(writer.clone());
        let (send, receive) = std::sync::mpsc::channel();
        let composed = std::thread::spawn(move || {
            send.send(crate::load_local_workspaces(&mut folders, vec![folder]))
                .unwrap();
        });
        let (model, mut saved) = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("startup blocked on writer lease")
            .unwrap();
        assert_eq!(model.folders().len(), 1);
        assert!(matches!(
            saved.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        blocker.unlock().unwrap();
        saved.blocking_recv().unwrap().unwrap();
        composed.join().unwrap();
        assert_eq!(
            WorkspaceStore::guarded(writer).load().unwrap(),
            model.folders()
        );
    }
    #[test]
    fn queue_never_waits_on_ui_orders_saves_reports_errors_and_drains_on_quit() {
        let (_dir, writer) = fixture();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(writer.preview().root().join(".writer"))
            .unwrap();
        blocker.lock().unwrap();
        let path = writer.preview().root().join("local-workspaces.json");
        let folders = WorkspaceStore::guarded(writer.clone());
        let mut first = folders
            .save_async(&[writer.preview().root().join("workspace")])
            .unwrap();
        let layout = LayoutStore::guarded(
            writer.preview().root().join("layout-1.json"),
            writer.clone(),
        );
        let dock_save = layout.save_async(&Default::default()).unwrap();
        let bad = writer
            .submit(
                writer.preview().root().join("unrelated.json"),
                b"bad".to_vec(),
            )
            .unwrap();
        let last = folders.save_async(&[]).unwrap();
        let mut close = writer.close().unwrap();
        assert!(matches!(
            first.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            close.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(writer.submit(path.clone(), vec![]).is_err());
        blocker.unlock().unwrap();
        first.blocking_recv().unwrap().unwrap();
        dock_save.blocking_recv().unwrap().unwrap();
        assert!(bad.blocking_recv().unwrap().is_err());
        last.blocking_recv().unwrap().unwrap();
        close.blocking_recv().unwrap().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["folders"], serde_json::json!([]));
        assert!(
            writer
                .core
                .replace_layout(&writer.preview().root().join("layout-2.json"), b"late")
                .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn both_store_writers_refuse_replaced_destinations_and_preserve_sources() {
        let (dir, writer) = fixture();
        let protected = dir.path().join("host/.config/gitterm-v6");
        std::fs::create_dir_all(&protected).unwrap();
        let sentinel = protected.join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let mut folders = WorkspaceStore::guarded(writer.clone());
        folders.save(&[]).unwrap();
        let path = writer.preview().layout().join("local-workspaces.json");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&sentinel, &path).unwrap();
        assert!(folders.load().is_err());
        assert!(folders.save(&[]).is_err());
        let path = writer
            .preview()
            .layout()
            .join("layout-workspace-0123456789abcdef.json");
        let layout = LayoutStore::guarded(path.clone(), writer);
        std::os::unix::fs::symlink(&sentinel, &path).unwrap();
        assert!(layout.save(&Default::default()).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    }
    #[gpui::test]
    fn startup_catalog_delayed_failure_reaches_owner(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        cx.background_executor.allow_parking();
        let (_dir, writer) = fixture();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(writer.preview().root().join(".writer"))
            .unwrap();
        blocker.lock().unwrap();
        let mut folders = WorkspaceStore::guarded(writer.clone());
        let (model, save) = crate::load_local_workspaces(&mut folders, vec![]).unwrap();
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(folders), cx));
        owner.update(cx, |owner, cx| owner.watch_save(save, cx));
        std::fs::write(
            writer.preview().root().join(".gitterm-protected"),
            b"fixture",
        )
        .unwrap();
        blocker.unlock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            if owner.read_with(cx, |owner, _| owner.error.is_some()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "startup save error not delivered"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        owner.read_with(cx, |owner, _| {
            let error = owner.error.as_ref().unwrap();
            assert!(error.contains("local-workspaces.json"), "{error}");
            assert!(error.contains("protected sentinel"), "{error}");
        });
        assert!(
            !writer
                .preview()
                .root()
                .join("local-workspaces.json")
                .exists()
        );
    }
    #[gpui::test]
    fn quit_drains_beyond_gpui_timeout_before_later_observers(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let (_dir, writer) = fixture();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(writer.preview().root().join(".writer"))
            .unwrap();
        blocker.lock().unwrap();
        let path = writer.preview().root().join("layout-1.json");
        let saved = writer
            .submit(path.clone(), b"must persist before quit".to_vec())
            .unwrap();
        let (begin, started) = std::sync::mpsc::channel();
        let released = std::thread::spawn(move || {
            started.recv().unwrap();
            std::thread::sleep(gpui::SHUTDOWN_TIMEOUT + std::time::Duration::from_millis(100));
            blocker.unlock().unwrap();
        });
        let preview = PreviewContext {
            roots: writer.preview().clone(),
            writer: writer.clone(),
        };
        let app = cx.new_app();
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let checked = observed.clone();
        let surviving = writer.clone();
        app.update(|cx| {
            cx.on_app_quit(move |_| {
                begin.send(()).unwrap();
                std::future::ready(())
            })
            .detach();
            preview.release_on_quit(cx);
            cx.on_app_quit(move |_| {
                assert_eq!(std::fs::read(&path).unwrap(), b"must persist before quit");
                assert!(surviving.core.replace_layout(&path, b"late").is_err());
                checked.set(true);
                std::future::ready(())
            })
            .detach();
        });
        let start = std::time::Instant::now();
        app.update(|app| app.shutdown());
        assert!(start.elapsed() > gpui::SHUTDOWN_TIMEOUT);
        assert!(observed.get());
        saved.blocking_recv().unwrap().unwrap();
        released.join().unwrap();
    }
    #[gpui::test]
    fn shutdown_releases_lease_while_store_handles_are_still_alive(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let (_dir, writer) = fixture();
        let preview = PreviewContext {
            roots: writer.preview().clone(),
            writer: writer.clone(),
        };
        let app = cx.new_app();
        app.update(|cx| preview.release_on_quit(cx));
        app.quit();
        assert!(
            writer
                .replace_layout(&preview.roots.layout().join("layout-2.json"), b"bad")
                .is_err()
        );
        let next = preview.roots.claim_writer().unwrap();
        drop(preview);
        drop(writer);
        next.replace_layout(&next.preview().layout().join("layout-3.json"), b"good")
            .unwrap();
    }
    #[gpui::test]
    fn normal_startup_restores_existing_terminal_sessions(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        use std::{rc::Rc, time::Instant};
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let (dir, writer) = fixture();
        let folder = dir.path().join("workspace");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        let mut model = crate::workspace_status::WorkspaceStatus::default();
        model.add(folder.clone(), Instant::now());
        let key = crate::layout::workspace_layout_key(&folder);
        writer
            .replace_layout(
                &crate::layout::workspace_layout_path(&writer.preview().layout(), &key),
                &serde_json::to_vec(
                    &serde_json::json!({"terminal_sessions":[{"id":999,"cwd":folder}]}),
                )
                .unwrap(),
            )
            .unwrap();
        let layout = writer.preview().layout();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| {
                crate::WorkspaceOwner::new(model, Some(WorkspaceStore::guarded(writer)), cx)
            });
            let data = Rc::new(crate::data::LocalData(owner.clone()));
            crate::Shell::with_owner(data, owner, &layout, true, window, cx).unwrap()
        });
        cx.run_until_parked();
        cx.read(|cx| {
            let shell = shell.read(cx);
            let state = shell.active(cx);
            assert_eq!(state.sessions[0].id.0, 999);
            let terminal = state.panes.get("session-999").unwrap().read(cx);
            assert!(matches!(
                terminal.content,
                crate::pane::PaneContent::Terminal(_)
            ));
            assert!(
                !shell
                    .owner
                    .read(cx)
                    .error
                    .as_ref()
                    .is_some_and(|e| e.contains("preview disables"))
            );
        });
    }
}

#[cfg(test)]
mod iced_override_tests {
    use super::*;
    #[test]
    fn invalid_v6_overrides_fall_back_while_default_and_valid_override_stay_protected() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("fixture-home");
        for raw in [None, Some(OsString::new()), Some("relative".into())] {
            let source = gitterm_core::config_paths::iced_config_dir(raw.clone(), &home);
            let protected = ProtectedRoots::new(&home, protected_overrides(raw, [])).unwrap();
            let preview = Preview::validate(&dir.path().join("preview"), protected).unwrap();
            assert_eq!(source, home.join(".config/gitterm-v6"));
            assert!(preview.scoped(&source.join("workspaces.json")).is_err());
        }
        let source = dir.path().join("isolated-iced");
        let raw = Some(source.as_os_str().to_owned());
        let protected = ProtectedRoots::new(&home, protected_overrides(raw, [])).unwrap();
        assert!(Preview::validate(&source, protected.clone()).is_err());
        let preview = Preview::validate(&dir.path().join("preview"), protected).unwrap();
        assert!(preview.scoped(&source.join("workspaces.json")).is_err());
        assert!(!source.exists());
    }
}
