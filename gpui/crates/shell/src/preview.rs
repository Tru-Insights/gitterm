//! Composition before any GPUI store is loaded. No V6 fallback, import or child.
use anyhow::{Context, Result};
use gitterm_core::preview::{Effect, Preview, ProtectedRoots, WriterLease};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct PreviewContext {
    pub roots: Arc<Preview>,
    writer: Arc<WriterLease>,
}
impl PreviewContext {
    pub(crate) fn writer(&self) -> Arc<WriterLease> {
        self.writer.clone()
    }
    pub(crate) fn release_on_quit(&self, cx: &gpui::App) {
        let writer = self.writer();
        cx.on_app_quit(move |_| {
            let writer = writer.clone();
            async move {
                if let Err(error) = writer.close() {
                    eprintln!(
                        "release preview writer {}: {error}",
                        writer.preview().root().display()
                    );
                }
            }
        })
        .detach();
    }
    pub fn from_environment(sample: bool) -> Result<Self> {
        let roots = roots_from_environment(sample)?;
        let writer = roots
            .claim_writer()
            .context("claim exclusive preview writer (stale leases require explicit recovery)")?;
        Ok(Self {
            roots,
            writer: Arc::new(writer),
        })
    }
}
/// Read-only routes compose the same validated roots without creating any files.
pub fn roots_from_environment(sample: bool) -> Result<Arc<Preview>> {
    let home = dirs::home_dir().context("resolve home for preview protection")?;
    let overrides = [
        "GITTERM_V6_CONFIG_DIR",
        "GITTERM_V5_CONFIG_DIR",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from);
    let protected = ProtectedRoots::new(&home, overrides)?;
    let root = config_dir(std::env::var_os("GITTERM_GPUI_CONFIG_DIR"), &home)?;
    let root = if sample { root.join("sample") } else { root };
    let roots = Arc::new(Preview::validate(&root, protected)?);
    Ok(roots)
}
pub fn config_dir(override_dir: Option<OsString>, home: &Path) -> Result<PathBuf> {
    let root = override_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config/gitterm-gpui"));
    Ok(Preview::validate(&root, ProtectedRoots::new(home, [])?)?
        .root()
        .to_path_buf())
}
/// There is deliberately no enabling production branch. The private test switch
/// lets legacy headless backend tests exercise trusted temporary repositories.
pub fn executable(preview: Option<&Preview>, effect: Effect, target: &Path) -> Result<()> {
    #[cfg(test)]
    if TRUSTED_FIXTURE.with(std::cell::Cell::get) {
        return Ok(());
    }
    let Some(preview) = preview else {
        anyhow::bail!("no preview context for {effect:?}: {}", target.display());
    };
    preview.capabilities().authorize(effect, target)?;
    Ok(())
}
#[cfg(test)]
thread_local! { static TRUSTED_FIXTURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) fn trusted_fixture() {
    TRUSTED_FIXTURE.with(|allowed| allowed.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{layout::LayoutStore, workspace_config::WorkspaceStore};
    fn fixture() -> (tempfile::TempDir, Arc<WriterLease>) {
        let dir = tempfile::tempdir().unwrap();
        let protected = ProtectedRoots::new(&dir.path().join("host"), []).unwrap();
        let roots = Arc::new(Preview::validate(&dir.path().join("preview"), protected).unwrap());
        let writer = Arc::new(roots.claim_writer().unwrap());
        (dir, writer)
    }
    #[test]
    fn executable_dispatch_reports_attempted_target_and_never_calls_effect() {
        let (_dir, writer) = fixture();
        let target = writer.preview().history().join("copied-pid-123");
        let mut executed = vec![];
        for effect in [
            Effect::Terminal,
            Effect::Git,
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
        let path = writer.preview().layout().join("layout-workspace.json");
        let layout = LayoutStore::guarded(path.clone(), writer);
        std::os::unix::fs::symlink(&sentinel, &path).unwrap();
        assert!(layout.save(&Default::default()).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    }
    #[gpui::test]
    fn shutdown_releases_lease_while_store_handles_are_still_alive(cx: &mut gpui::TestAppContext) {
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
                .replace_layout(&preview.roots.layout().join("late.json"), b"bad")
                .is_err()
        );
        let next = preview.roots.claim_writer().unwrap();
        drop(preview);
        drop(writer);
        assert!(next.preview().claim_writer().is_err());
    }
    #[gpui::test]
    fn startup_restore_keeps_copied_terminals_inert(cx: &mut gpui::TestAppContext) {
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
            let crate::pane::PaneContent::Error(error) = &terminal.content else {
                panic!("restored terminal launched");
            };
            assert!(error.contains(folder.to_str().unwrap()));
            assert!(error.contains("Terminal"));
            assert!(
                shell
                    .owner
                    .read(cx)
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains(folder.to_str().unwrap()))
            );
        });
    }
}
