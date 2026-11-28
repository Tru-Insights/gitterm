//! Live admission finishes before publishing a recovery marker or service owner.
use anyhow::{Context, Result, ensure};
use gitterm_core::{
    backup,
    desktop_store::{DesktopStore, LoadState, Snapshot},
    preview::service::ServiceContext,
};
use serde::{Deserialize, Serialize};
use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
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
#[cfg(test)]
fn admit(root: &Path, prefs: &Path, backup_parent: &Path) -> Result<Admission> {
    admit_with_progress(root, prefs, backup_parent, None)
}
fn admit_with_progress(
    root: &Path,
    prefs: &Path,
    backup_parent: &Path,
    progress: Option<&Arc<ActivationProgress>>,
) -> Result<Admission> {
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
    let old = old.filter(|a| a.root == root);
    let (backup, created) = if let Some(recorded) = old {
        backup::verify(&recorded.backup, root).context("verify recorded Live backup")?;
        (recorded.backup, None)
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
        let created = backup::create_with_progress(root, backup_parent, true, |a, b, c, d| {
            if let Some(progress) = progress {
                *progress.counts.lock().unwrap() = (a, b, c, d);
                if progress.phase.load(Ordering::SeqCst) == CANCELLED {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        })
        .context("create verified Live backup")?;
        (created.path().to_owned(), Some(created))
    };
    let activation = Activation {
        root: root.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        backup,
    };
    finish_activation(progress, created, || {
        record(&prefs, &activation).context("record GPUI activation after verified backup")
    })?;
    Ok(Admission {
        checked,
        store,
        snapshot,
        activation,
    })
}
fn finish_activation(
    progress: Option<&Arc<ActivationProgress>>,
    created: Option<backup::CreatedBackup>,
    record: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // Atomically order cancellation and commit without holding a progress lock
    // across cleanup or the activation record's potentially slow full fsyncs.
    if progress.is_some_and(|progress| {
        progress
            .phase
            .compare_exchange(COPYING, COMMITTING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
    }) {
        if let Some(created) = created {
            let path = created.path().to_owned();
            created
                .remove()
                .with_context(|| format!("remove cancelled backup {}", path.display()))?;
        }
        anyhow::bail!("V6 backup cancelled");
    }
    // A record failure can happen after rename (e.g. directory fsync failure).
    // Retain a completed backup in that case, so published prefs cannot dangle.
    record()
}
pub(crate) fn prepare() -> Result<Admission> {
    prepare_with_progress(None)
}
fn prepare_with_progress(progress: Option<&Arc<ActivationProgress>>) -> Result<Admission> {
    let root = selected_root()?;
    let backup_parent = root
        .parent()
        .context("V6 root has no parent")?
        .join("gitterm-v6-backups");
    admit_with_progress(&root, &prefs_path()?, &backup_parent, progress)
}
const SUCCESS: &str =
    "V6 state is backed up and activated. Quit and reopen GitTerm GPUI to use it.";
const QUIT_BACKUP: &str =
    "A V6 backup is in progress. Quit anyway? The partial backup will be removed.";

#[derive(Default)]
struct ActivationProgress {
    counts: Mutex<(u64, u64, u64, u64)>,
    phase: AtomicU8,
}
const COPYING: u8 = 0;
const CANCELLED: u8 = 1;
const COMMITTING: u8 = 2;
impl ActivationProgress {
    fn request_cancel(&self) {
        let _ = self
            .phase
            .compare_exchange(COPYING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst);
    }
    fn text(&self) -> Option<String> {
        self.counts
            .try_lock()
            .ok()
            .map(|counts| progress_text(*counts))
    }
}
#[derive(Clone)]
struct RunningActivation {
    progress: Arc<ActivationProgress>,
    window: gpui::WindowHandle<ActivationMessage>,
}
#[derive(Default)]
struct ActivationState {
    running: Option<RunningActivation>,
    prompt_pending: bool,
    quit_confirmed: bool,
}
impl gpui::Global for ActivationState {}

fn progress_text((done, total, bytes, size): (u64, u64, u64, u64)) -> String {
    format!(
        "Backing up V6 state… {done} of {total} files ({:.2} of {:.2} GB)\n\nDon't quit GitTerm GPUI until this finishes.",
        bytes as f64 / 1_000_000_000.0,
        size as f64 / 1_000_000_000.0,
    )
}
fn open_progress(cx: &mut gpui::App) -> Result<gpui::WindowHandle<ActivationMessage>> {
    use gpui::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(520.), px(180.)),
                cx,
            ))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Use V6 state".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            window.on_window_should_close(cx, |_, cx| {
                if cx
                    .try_global::<ActivationState>()
                    .is_some_and(|state| state.running.is_some())
                {
                    crate::lifetime::quit(cx);
                    false
                } else {
                    true
                }
            });
            cx.new(|_| ActivationMessage(progress_text((0, 0, 0, 0))))
        },
    )?;
    cx.activate(true);
    Ok(window)
}

pub(crate) fn install_activation(cx: &mut gpui::App) {
    cx.set_global(ActivationState::default());
    cx.on_action(|_: &crate::app_menus::UseV6State, cx| {
        if let Some(running) = cx.global::<ActivationState>().running.clone() {
            let _ = running
                .window
                .update(cx, |_, window, _| window.activate_window());
            return;
        }
        // A failed window must never leave another silent activation running.
        let window = match open_progress(cx) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("V6 activation failed: show progress window: {error:#}");
                return;
            }
        };
        let progress = Arc::new(ActivationProgress::default());
        cx.global_mut::<ActivationState>().running = Some(RunningActivation {
            progress: progress.clone(),
            window,
        });
        let observed = progress.clone();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let running = cx.update(|cx| {
                    if !cx
                        .global::<ActivationState>()
                        .running
                        .as_ref()
                        .is_some_and(|run| Arc::ptr_eq(&run.progress, &observed))
                    {
                        return false;
                    }
                    let Some(text) = observed.text() else {
                        return true;
                    };
                    let _ = window.update(cx, |view, _, cx| {
                        view.0 = text;
                        cx.notify();
                    });
                    true
                });
                if !running {
                    break;
                }
            }
        })
        .detach();
        let work = cx.background_executor().spawn(async move {
            drop(prepare_with_progress(Some(&progress))?);
            Ok::<_, anyhow::Error>(())
        });
        cx.spawn(async move |cx| {
            let message = match work.await {
                Ok(()) => SUCCESS.to_string(),
                Err(error) => format!("V6 activation failed: {error:#}"),
            };
            eprintln!("{message}");
            // Let an already-open quit sheet resolve before showing the outcome.
            while cx.update(|cx| cx.global::<ActivationState>().prompt_pending) {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
            }
            cx.update(|cx| {
                let state = cx.global_mut::<ActivationState>();
                state.running = None; // The worker has dropped the V6 lease.
                if state.quit_confirmed {
                    crate::lifetime::quit(cx);
                    return;
                }
                let answer = window.update(cx, |view, window, cx| {
                    view.0 = message.clone();
                    cx.notify();
                    window.prompt(
                        gpui::PromptLevel::Info,
                        "Use V6 state",
                        Some(&message),
                        &["OK"],
                        cx,
                    )
                });
                match answer {
                    Ok(answer) => cx
                        .spawn(async move |cx| {
                            let _ = answer.await;
                            let _ = window.update(cx, |_, window, _| window.remove_window());
                        })
                        .detach(),
                    Err(error) => eprintln!("show GPUI activation result: {error:#}"),
                }
            });
        })
        .detach();
    });
}

/// Called only from lifetime::quit, before shutdown starts. Keep the event loop
/// alive until the worker acknowledges cancellation, cleans up and drops its lock.
pub(crate) fn defer_quit_for_backup(cx: &mut gpui::App) -> bool {
    let Some(running) = cx
        .try_global::<ActivationState>()
        .and_then(|state| state.running.clone())
    else {
        return false;
    };
    let state = cx.global_mut::<ActivationState>();
    if state.prompt_pending || state.quit_confirmed {
        return true;
    }
    state.prompt_pending = true;
    let answer = running.window.update(cx, |_, window, cx| {
        window.activate_window();
        window.prompt(
            gpui::PromptLevel::Warning,
            QUIT_BACKUP,
            None,
            &["Cancel", "Quit Anyway"],
            cx,
        )
    });
    match answer {
        Ok(answer) => cx
            .spawn(async move |cx| {
                let quit = answer.await == Ok(1);
                cx.update(|cx| {
                    let state = cx.global_mut::<ActivationState>();
                    state.prompt_pending = false;
                    if quit {
                        if let Some(running) = &state.running {
                            running.progress.request_cancel();
                            state.quit_confirmed = true;
                        } else {
                            // Completion raced the native prompt: use normal shutdown.
                            crate::lifetime::quit(cx);
                        }
                    }
                });
            })
            .detach(),
        Err(error) => {
            cx.global_mut::<ActivationState>().prompt_pending = false;
            eprintln!("show V6 backup quit prompt: {error:#}");
        }
    }
    true
}
struct ActivationMessage(String);
impl gpui::Render for ActivationMessage {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::prelude::*;
        let palette = crate::theme::Palette::current(cx);
        gpui::div()
            .size_full()
            .p_4()
            .bg(gpui::rgb(palette.base))
            .text_color(gpui::rgb(palette.text))
            .font_family(crate::theme::LABEL_FONT)
            .text_size(gpui::px(crate::theme::UI_FONT_SIZE))
            .child(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn quit_during_backup_prompts_and_cancel_keeps_running(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let progress = Arc::new(ActivationProgress::default());
        cx.update(|cx| {
            let window = open_progress(cx).unwrap();
            cx.set_global(ActivationState {
                running: Some(RunningActivation {
                    progress: progress.clone(),
                    window,
                }),
                ..Default::default()
            });
            crate::install_app_menus(cx);
            cx.dispatch_action(&crate::app_menus::Quit);
        });
        assert!(cx.has_pending_prompt());
        assert_eq!(cx.pending_prompt().unwrap().0, QUIT_BACKUP);
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(progress.phase.load(Ordering::SeqCst), COPYING);
        cx.update(|cx| {
            assert!(cx.global::<ActivationState>().running.is_some());
            assert!(!cx.global::<ActivationState>().prompt_pending);
            crate::lifetime::quit(cx);
        });
        assert!(cx.has_pending_prompt());
        let _held_counts = progress.counts.lock().unwrap();
        assert!(progress.text().is_none());
        cx.simulate_prompt_answer("Quit Anyway");
        cx.run_until_parked();
        assert_eq!(progress.phase.load(Ordering::SeqCst), CANCELLED);
        cx.update(|cx| {
            // Shutdown waits for the backup worker to acknowledge and clean up.
            assert!(cx.global::<ActivationState>().running.is_some());
            assert!(cx.global::<ActivationState>().quit_confirmed);
        });
    }

    #[test]
    fn slow_activation_record_leaves_progress_readable_and_orders_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let prefs = base.join("gpui/live-activation.json");
        let progress = Arc::new(ActivationProgress::default());
        let observed = progress.clone();
        let (began, begun) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            finish_activation(Some(&progress), None, || {
                // Simulate slow filesystem work at the record's full fsync.
                assert!(progress.counts.try_lock().is_ok());
                began.send(()).unwrap();
                released
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                record(
                    &prefs,
                    &Activation {
                        root: base.join("v6"),
                        app_version: "fixture".into(),
                        backup: base.join("backup"),
                    },
                )
            })
        });
        begun
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(observed.text().is_some());
        observed.request_cancel();
        assert_eq!(observed.phase.load(Ordering::SeqCst), COMMITTING);
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();

        let root = temp.path().join("v6");
        let backups = temp.path().join("backups");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&backups).unwrap();
        let created = backup::create_with_progress(&root, &backups, true, |_, _, _, _| {
            ControlFlow::Continue(())
        })
        .unwrap();
        let progress = Arc::new(ActivationProgress::default());
        progress.request_cancel();
        let error = finish_activation(Some(&progress), Some(created), || {
            panic!("record after cancellation")
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(std::fs::read_dir(backups).unwrap().count(), 0);
        assert!(progress.text().is_some());
    }

    #[test]
    fn cancelled_activation_does_not_record_prefs_and_releases_v6_lock() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("settings.json"), b"{}").unwrap();
        let prefs = temp.path().join("gpui/live-activation.json");
        let backups = temp.path().join("backups");
        let progress = Arc::new(ActivationProgress {
            phase: AtomicU8::new(CANCELLED),
            ..Default::default()
        });
        let error = admit_with_progress(&root, &prefs, &backups, Some(&progress))
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("cancelled"));
        assert!(!prefs.exists());
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 0);
        assert_eq!(std::fs::read(root.join("settings.json")).unwrap(), b"{}");
        gitterm_core::state_lock::StateLock::acquire(&root).unwrap();
    }

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
