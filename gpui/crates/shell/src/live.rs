//! Live admission finishes before publishing a recovery marker or service owner.
use anyhow::{Context, Result, ensure};
use gitterm_core::{
    backup,
    desktop_store::{DesktopStore, LoadState, Snapshot},
    preview::service::ServiceContext,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Activation {
    pub root: PathBuf,
    pub app_version: String,
    pub backup: PathBuf,
}
fn prefs_path() -> Result<PathBuf> {
    // Include overridden protected roots and the final file, not just its
    // parent: an activation symlink must never read or replace V6/provider state.
    let roots = crate::preview::roots_from_environment(false)?;
    Ok(roots.scoped(&roots.root().join("live-activation.json"))?)
}
pub(crate) fn read_activation(path: &Path) -> Result<Option<Activation>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parse GPUI activation {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read GPUI activation {}", path.display()))
        }
    }
}
fn record(path: &Path, activation: &Activation) -> Result<()> {
    use std::io::Write as _;
    let parent = path.parent().context("GPUI preferences have no parent")?;
    ensure!(
        gitterm_core::state_lock::resolve_root(path)? == path,
        "GPUI activation path changed: {}",
        path.display()
    );
    std::fs::create_dir_all(parent)?;
    ensure!(
        gitterm_core::state_lock::resolve_root(path)? == path,
        "GPUI activation path changed: {}",
        path.display()
    );
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(activation)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn installed_executable(exe: &Path) -> bool {
    let Some(macos) = exe.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    macos.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && contents.parent().is_some_and(|bundle| {
            bundle
                .extension()
                .is_some_and(|extension| extension == "app")
        })
}
pub fn launch_mode(args: &mut Vec<std::ffi::OsString>) -> Result<crate::launch::LaunchMode> {
    gitterm_core::worker_process::handle_recorder(args);
    // Cargo/debug and unbundled launches always default to Preview.
    let installed = installed_executable(&std::env::current_exe()?);
    let activated = if installed && !args.iter().any(|a| a == "--live" || a == "--preview") {
        let activation = read_activation(&prefs_path()?)?;
        if let Some(activation) = &activation {
            ensure!(
                activation.root == selected_root()?,
                "V6 state root differs from the GPUI activation; start in Preview and activate the selected root again"
            );
        }
        activation.is_some()
    } else {
        false
    };
    Ok(crate::launch::resolve_mode(args, installed, activated))
}
fn validate_override(raw: Option<std::ffi::OsString>) -> Result<()> {
    if let Some(raw) = raw {
        ensure!(
            !raw.is_empty() && Path::new(&raw).is_absolute(),
            "GITTERM_V6_CONFIG_DIR must be a non-empty absolute path; refusing Live startup"
        );
    }
    Ok(())
}
fn selected_root() -> Result<PathBuf> {
    validate_override(std::env::var_os("GITTERM_V6_CONFIG_DIR"))?;
    Ok(gitterm_core::config_paths::desktop_config_dir()?.to_path_buf())
}
pub(crate) struct Admission {
    checked: crate::lifetime::CheckedLifetime,
    pub store: DesktopStore,
    pub snapshot: Snapshot,
    pub activation: Activation,
}
impl Admission {
    pub(crate) fn publish(
        self,
    ) -> Result<(
        crate::lifetime::Lifetime,
        DesktopStore,
        Snapshot,
        Arc<ServiceContext>,
    )> {
        let lifetime = self.checked.publish()?;
        let services = Arc::new(
            ServiceContext::production(&self.activation.root, std::process::id().to_string())?
                .for_gpui_live(),
        );
        Ok((lifetime, self.store, self.snapshot, services))
    }
}
fn validated(store: &DesktopStore) -> Result<Snapshot> {
    let snapshot = store.snapshot().wait().map_err(anyhow::Error::msg)?;
    if let LoadState::Refused(error) = &snapshot.workspaces.loaded {
        anyhow::bail!("Live workspaces refused: {error}");
    }
    if let LoadState::Refused(error) = &snapshot.settings.loaded {
        anyhow::bail!("Live settings refused: {error}");
    }
    Ok(snapshot)
}
fn admit(root: &Path, prefs: &Path, backup_parent: &Path) -> Result<Admission> {
    let checked = crate::lifetime::Lifetime::check_live(root)?;
    let root = checked.lock.root();
    let prefs = gitterm_core::state_lock::resolve_root(prefs)?;
    ensure!(
        !prefs.starts_with(root) && !root.starts_with(&prefs),
        "GPUI activation preferences overlap the V6 root"
    );
    let store = crate::workspace_config::live_desktop_store(checked.lock.clone())?;
    let snapshot = validated(&store)?;
    // Validate tasks before backup or services; load has no migration/write step.
    gitterm_core::tasks::TaskStore::load(root.join("tasks.json"))?;
    let old = read_activation(&prefs)?;
    let backup = if let Some(recorded) = old.filter(|a| a.root == root) {
        backup::verify(&recorded.backup, root).context("verify recorded Live backup")?;
        recorded.backup
    } else {
        if !backup_parent.exists() {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(backup_parent)
                .context("create Live backup directory")?;
        }
        backup::create(root, backup_parent, true).context("create verified Live backup")?
    };
    let activation = Activation {
        root: root.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        backup,
    };
    record(&prefs, &activation).context("record GPUI activation after verified backup")?;
    Ok(Admission {
        checked,
        store,
        snapshot,
        activation,
    })
}
pub(crate) fn prepare() -> Result<Admission> {
    let root = selected_root()?;
    let backup_parent = root
        .parent()
        .context("V6 root has no parent")?
        .join("gitterm-v6-backups");
    admit(&root, &prefs_path()?, &backup_parent)
}
pub(crate) fn activate() -> Result<()> {
    drop(prepare()?);
    Ok(())
}

pub(crate) fn install_activation(cx: &mut gpui::App) {
    use gpui::AppContext as _;
    cx.on_action(|_: &crate::app_menus::UseV6State, cx| {
        let work = cx.background_executor().spawn(async { activate() });
        cx.spawn(async move |cx| {
            let message = match work.await {
                Ok(()) => "V6 state is backed up and activated. Restart GitTerm GPUI to use it."
                    .to_string(),
                Err(error) => format!("V6 activation failed: {error:#}"),
            };
            cx.update(|cx| {
                let opened = cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                    let answer = window.prompt(
                        gpui::PromptLevel::Info,
                        "Use V6 state",
                        Some(&message),
                        &["OK"],
                        cx,
                    );
                    cx.spawn(async move |_| {
                        let _ = answer.await;
                    })
                    .detach();
                    cx.new(|_| ActivationMessage(message))
                });
                if let Err(error) = opened {
                    eprintln!("show GPUI activation result: {error:#}");
                }
            });
        })
        .detach();
    });
}
struct ActivationMessage(String);
impl gpui::Render for ActivationMessage {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::prelude::*;
        gpui::div().p_4().child(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_a_bundled_executable_uses_remembered_activation() {
        assert!(installed_executable(Path::new(
            "/Applications/GitTerm GPUI.app/Contents/MacOS/gitterm-gpui"
        )));
        for exe in [
            "/repo/target/debug/gitterm-gpui",
            "/repo.app/target/release/gitterm-gpui",
            "/repo/Contents/MacOS/gitterm-gpui",
        ] {
            assert!(!installed_executable(Path::new(exe)));
        }
    }
    #[test]
    fn activation_cannot_write_inside_v6_or_through_a_swapped_prefs_parent() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("v6");
        std::fs::create_dir(&root).unwrap();
        assert!(
            admit(
                &root,
                &root.join("live-activation.json"),
                &base.join("backups")
            )
            .is_err()
        );
        assert!(!root.join("live-activation.json").exists());
        assert!(!base.join("backups").exists());
        let prefs_dir = base.join("gpui");
        std::fs::create_dir(&prefs_dir).unwrap();
        let prefs = prefs_dir.join("live-activation.json");
        std::fs::remove_dir(&prefs_dir).unwrap();
        std::os::unix::fs::symlink(&root, &prefs_dir).unwrap();
        let activation = Activation {
            root: root.clone(),
            app_version: "fixture".into(),
            backup: base.join("backup"),
        };
        assert!(record(&prefs, &activation).is_err());
        assert!(!root.join("live-activation.json").exists());
    }
    #[test]
    fn invalid_override_child() {
        if std::env::var_os("GITTERM_GPUI_INVALID_LIVE_TEST").is_none() {
            return;
        }
        let error = prepare().err().unwrap().to_string();
        assert!(error.contains("non-empty absolute path"), "{error}");
        let home = dirs::home_dir().unwrap();
        assert!(!home.join(".config").exists());
    }
    #[test]
    fn invalid_override_startup_never_reads_or_creates_default_state() {
        let temp = tempfile::tempdir().unwrap();
        for raw in ["", "relative"] {
            let home = temp
                .path()
                .join(if raw.is_empty() { "empty" } else { "relative" });
            std::fs::create_dir(&home).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "live::tests::invalid_override_child",
                    "--nocapture",
                ])
                .env("HOME", &home)
                .env("GITTERM_V6_CONFIG_DIR", raw)
                .env("GITTERM_GPUI_CONFIG_DIR", home.join("gpui"))
                .env("GITTERM_GPUI_INVALID_LIVE_TEST", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
        }
    }
    #[test]
    fn invalid_explicit_override_never_falls_back() {
        for raw in ["", "relative", "../v6"] {
            assert!(validate_override(Some(raw.into())).is_err());
        }
        assert!(validate_override(None).is_ok());
        assert!(validate_override(Some("/tmp/fixture/v6".into())).is_ok());
    }
    #[test]
    fn validation_and_verified_backup_precede_marker_and_activation_records_original_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        std::fs::create_dir(&root).unwrap();
        let prefs = temp.path().join("gpui/live-activation.json");
        let backups = temp.path().join("backups");
        let path = root.join("workspaces.json");
        std::fs::write(&path, b"{").unwrap();
        let error = admit(&root, &prefs, &backups).err().unwrap().to_string();
        assert!(error.contains("workspaces refused"), "{error}");
        assert!(!prefs.exists());
        assert!(!backups.exists());
        assert!(!root.join(".gpui-shutdown-pending").exists());
        let bytes = b"{  \"workspaces\": [], \"active_workspace\": 0, \"future\": true }\n";
        std::fs::write(&path, bytes).unwrap();
        let admission = admit(&root, &prefs, &backups).unwrap();
        assert!(!root.join(".gpui-shutdown-pending").exists());
        let activation = read_activation(&prefs).unwrap().unwrap();
        assert_eq!(activation, admission.activation);
        let manifest = backup::verify(&activation.backup, &root).unwrap();
        assert!(
            !manifest
                .entries
                .iter()
                .any(|e| e.path == Path::new(".gpui-shutdown-pending"))
        );
        assert_eq!(
            std::fs::read(activation.backup.join("state/workspaces.json")).unwrap(),
            bytes
        );
        let (lifetime, store, _, services) = admission.publish().unwrap();
        assert!(root.join(".gpui-shutdown-pending").exists());
        assert!(!services.is_preview());
        assert!(
            services
                .runner(
                    gitterm_core::preview::service::RunnerEffect::RestoreSession,
                    None
                )
                .is_err()
        );
        drop(store);
        lifetime.finish().unwrap();
        drop(admit(&root, &prefs, &backups).unwrap());
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn busy_live_root_names_the_locked_path_without_activation_or_backup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("v6");
        let held = gitterm_core::state_lock::StateLock::acquire(&root).unwrap();
        let sentinel = root.join("settings.json");
        std::fs::write(&sentinel, b"{}").unwrap();
        let before = std::fs::read_dir(&root).unwrap().count();
        let prefs = temp.path().join("gpui/live-activation.json");
        let backups = temp.path().join("backups");
        let error = admit(&root, &prefs, &backups).err().unwrap().to_string();
        assert!(
            error.contains("already in use") && error.contains(root.to_str().unwrap()),
            "{error}"
        );
        assert!(!prefs.exists());
        assert!(!backups.exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), before);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"{}");
        drop(held);
    }

    #[test]
    fn backup_failure_aborts_without_marker_or_prefs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        std::fs::create_dir(&root).unwrap();
        let prefs = temp.path().join("gpui/live-activation.json");
        let invalid = temp.path().join("invalid");
        std::fs::write(&invalid, b"file").unwrap();
        let marker = root.join(".gpui-shutdown-pending");
        std::fs::write(&marker, b"prior shutdown evidence").unwrap();
        assert!(admit(&root, &prefs, &invalid).is_err());
        assert!(!prefs.exists());
        assert_eq!(std::fs::read(&marker).unwrap(), b"prior shutdown evidence");
    }
}
