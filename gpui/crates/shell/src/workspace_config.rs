//! Iced workspace reads and isolated preview metadata persistence.
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
    pub(crate) iced: Option<IcedCatalog>,
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
            iced: None,
            chats: vec![],
            parked: vec![],
            path: config.join("local-workspaces.json"),
            refused: false,
            guard: None,
        }
    }
    pub(crate) fn guarded(guard: std::sync::Arc<crate::preview::Writer>) -> Self {
        Self {
            iced: None,
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
            folders: if self.iced.is_some() {
                vec![]
            } else {
                folders.to_vec()
            },
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

/// Explicit read authority only. This reader never enters the preview writer or
/// claims an Iced lease, including when the source is absent or half-written.
#[derive(Default)]
pub(crate) struct IcedCatalog {
    pub source: PathBuf,
    pub repos: Vec<gitterm_core::workspace_config::RepoConfig>,
    pub workspaces: Vec<gitterm_core::workspace_config::WorkspaceConfig>,
    pub active: usize,
    pub repos_on_bar: bool,
}
impl IcedCatalog {
    pub fn read(config: &Path) -> Result<Self> {
        let path = config.join("workspaces.json");
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read Iced workspaces {}", path.display()))?;
        let mut file: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parse Iced workspaces {}", path.display()))?;
        // Normalize legacy repo records in memory, just as Iced does on restore.
        file.normalize_repos();
        let mut open: Vec<_> = file
            .workspaces
            .into_iter()
            .filter(|w| w.active)
            .enumerate()
            .collect();
        let selected = file.active_workspace.min(open.len().saturating_sub(1));
        // Iced normalizes machines before building the bar, stably within each.
        open.sort_by_key(|(_, w)| match &w.location {
            Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent {
                remote_id,
                ..
            }) => {
                1 + file
                    .remote_agents
                    .iter()
                    .position(|a| a.id == *remote_id)
                    .unwrap_or(usize::MAX / 2)
            }
            _ if w.remote_session.is_some() && w.location.is_none() => usize::MAX / 2 + 1,
            _ => 0,
        });
        let active = open.iter().position(|(i, _)| *i == selected).unwrap_or(0);
        Ok(Self {
            source: config.to_owned(),
            repos: file.repos,
            workspaces: open.into_iter().map(|(_, w)| w).collect(),
            active,
            repos_on_bar: !file.collapsed_repo_groups.iter().any(|g| g == "local"),
        })
    }
    pub fn bar_layout(
        &self,
        active: Option<crate::data::WorkspaceId>,
    ) -> gitterm_core::workspace::BarLayout {
        use gitterm_core::workspace::{BarChip, bar_layout};
        let chips: Vec<_> = self
            .workspaces
            .iter()
            .map(|w| BarChip {
                group: iced_group(w),
                work: gitterm_core::work::catalog::is_work(&w.chats, false),
                parked: false,
            })
            .collect();
        let active = active
            .and_then(|id| id.0.checked_sub(1))
            .map(|i| i as usize)
            .unwrap_or(self.active);
        bar_layout(&chips, active, self.repos_on_bar)
    }
    pub fn visible_order(&self, active: Option<crate::data::WorkspaceId>) -> Vec<usize> {
        gitterm_core::workspace::visible_order(&self.bar_layout(active))
    }
}
fn iced_group(w: &gitterm_core::workspace_config::WorkspaceConfig) -> String {
    match &w.location {
        Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent {
            remote_id,
            ..
        }) => format!("agent:{remote_id}"),
        _ if w.location.is_none() && w.remote_session.is_some() => {
            format!("ssh:{}", w.remote_session.as_ref().unwrap().session_name)
        }
        _ => "local".into(),
    }
}
pub(crate) fn iced_layout_key(w: &gitterm_core::workspace_config::WorkspaceConfig) -> String {
    // A named piece of work owns a layout even when another work shares its home.
    let identity = serde_json::to_string(&(&w.name, &w.dir, &w.location)).expect("string identity");
    crate::layout::workspace_layout_key(Path::new(&identity))
}

pub(crate) fn iced_config_dir() -> PathBuf {
    gitterm_core::config_paths::desktop_config_dir()
        .expect("resolve read-only Iced config root")
        .to_path_buf()
}

/// Later Live startup shares its lifetime lease with the full-document owner.
/// Preview never calls this constructor and retains read-only Iced readers.
#[allow(dead_code)] // Live wiring is the next slice.
pub(crate) fn live_desktop_store(
    lock: std::sync::Arc<gitterm_core::state_lock::StateLock>,
) -> Result<gitterm_core::desktop_store::DesktopStore> {
    gitterm_core::desktop_store::DesktopStore::with_lock(lock).map_err(anyhow::Error::msg)
}

/// Iced builds its index from harness transcripts, independently of its config
/// override. Only the facts directory comes from the resolved Iced catalog.
/// Both core operations are read-only; catch-up/backfill writers are never run.
pub(crate) fn read_iced_chat_index(
    config: &Path,
    home: &Path,
) -> Vec<gitterm_core::chats::ChatIndexEntry> {
    let mut entries = gitterm_core::find::build_local_index(home);
    gitterm_core::chat_facts::attach_facts(
        &mut entries,
        &gitterm_core::chat_facts::facts_dir(config),
    );
    entries
}

// Keep host-owned usage data opaque while parsing the shared settings schema.
pub(crate) type IcedSettings = gitterm_core::settings::Settings<
    gitterm_core::settings::WorkspaceColor,
    serde_json::Value,
    PathBuf,
>;

pub(crate) fn load_iced_settings() -> IcedSettings {
    load_iced_settings_from(&iced_config_dir())
}

/// Read authority only: no migration, directory creation, lease or write-back.
pub(crate) fn load_iced_settings_from(config: &Path) -> IcedSettings {
    let path = config.join("settings.json");
    let result = (|| -> Result<IcedSettings> {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read Iced settings {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse Iced settings {}", path.display()))
    })();
    result.unwrap_or_else(|error| {
        eprintln!("{error:#}; using Iced defaults");
        IcedSettings::default()
    })
}

pub(crate) fn load_iced_workspaces(
    store: &mut WorkspaceStore,
) -> crate::workspace_status::WorkspaceStatus {
    load_iced_workspaces_from(store, &iced_config_dir())
}
pub(crate) fn load_iced_workspaces_from(
    store: &mut WorkspaceStore,
    config: &Path,
) -> crate::workspace_status::WorkspaceStatus {
    // Only preview chat/park metadata comes from this store. Its old folder list
    // must never become a fallback catalog when Iced's file is missing or invalid.
    if let Err(error) = store.load() {
        eprintln!("Load preview metadata: {error:#}");
    }
    let catalog = IcedCatalog::read(config).unwrap_or_else(|error| {
        eprintln!("{error:#}");
        IcedCatalog::default()
    });
    let model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
    store.iced = Some(catalog);
    model
}

#[cfg(test)]
mod iced_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn settings_missing_or_invalid_use_iced_defaults_without_writing() {
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("absent");
        let expected = serde_json::to_value(IcedSettings::default()).unwrap();
        assert_eq!(
            serde_json::to_value(load_iced_settings_from(&missing)).unwrap(),
            expected
        );
        assert!(!missing.exists());
        for invalid in [b"{".as_slice(), br#"{"ui_font_size":"invalid"}"#] {
            let path = fixture.path().join("settings.json");
            std::fs::write(&path, invalid).unwrap();
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(
                serde_json::to_value(load_iced_settings_from(fixture.path())).unwrap(),
                expected
            );
            assert_eq!(std::fs::read(&path).unwrap(), invalid);
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                modified
            );
        }
    }

    #[test]
    fn settings_load_preserves_source_bytes_entries_and_mtimes() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("settings.json");
        let bytes = br#"{ "theme": "light", "ui_font_size": 17, "terminal_font_size": 19,
            "usage": {"pricing": {"fixture": null}}, "future": true }"#;
        std::fs::write(&path, bytes).unwrap();
        let sentinel = fixture.path().join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
        let before = (
            modified(fixture.path()),
            modified(&path),
            modified(&sentinel),
        );
        let settings = load_iced_settings_from(fixture.path());
        assert_eq!(settings.theme, "light");
        assert_eq!(settings.ui_font_size, 17.);
        assert_eq!(settings.terminal_font_size, 19.);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 2);
        assert_eq!(
            (
                modified(fixture.path()),
                modified(&path),
                modified(&sentinel)
            ),
            before
        );
    }

    fn fixture(iced: &Path) {
        std::fs::create_dir_all(iced).unwrap();
        std::fs::write(iced.join("sentinel"), b"keep exact bytes").unwrap();
        let entry = |name: &str, active: bool, chats: Vec<&str>| {
            serde_json::json!({
                "name": name, "abbrev":"FX", "dir":"/fixture/shared", "color":"blue", "active":active,
                "repos":["/fixture/api", "/fixture/shared"], "home":"/fixture/api", "chats":chats, "active_tab":1,
                "tabs":[{"dir":"/fixture/shared", "startup_command":"must-never-run", "session_uid":format!("{name}-terminal")},
                        {"dir":"/fixture/api", "tab_kind":"agent", "agent_config":{"backend":"claude", "model":"default"}, "chat_session_id":"chat-1", "session_uid":format!("{name}-agent")}]
            })
        };
        std::fs::write(iced.join("workspaces.json"), serde_json::to_vec(&serde_json::json!({
            "active_workspace":1, "workspaces":[entry("Folder", true, vec![]), entry("Closed", false, vec![]), entry("Named work", true, vec!["chat-1", "chat-2"]) ]
        })).unwrap()).unwrap();
    }
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                (
                    path.file_name().unwrap().into(),
                    std::fs::read(path).unwrap(),
                )
            })
            .collect()
    }
    #[test]
    fn reads_named_work_same_folder_tabs_selection_and_iced_bar_order() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let before = snapshot(dir.path());
        let catalog = IcedCatalog::read(dir.path()).unwrap();
        assert_eq!(
            catalog
                .workspaces
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["Folder", "Named work"]
        );
        assert_eq!(catalog.active, 1); // Saved index addresses OPEN entries.
        assert_eq!(catalog.visible_order(None), [1, 0]);
        assert_eq!(
            catalog.workspaces[1].repos,
            ["/fixture/api", "/fixture/shared"]
        );
        let model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
        assert_eq!(model.workspaces.len(), 2);
        assert_eq!(model.workspaces[1].folder, PathBuf::from("/fixture/shared"));
        assert_eq!(model.active(), Some(crate::data::WorkspaceId(2)));
        assert_ne!(
            iced_layout_key(&catalog.workspaces[0]),
            iced_layout_key(&catalog.workspaces[1])
        );
        let sessions = crate::data::iced_sessions(&catalog.workspaces[1]);
        assert_eq!(sessions.len(), 2); // Owned chats are not extra open tabs.
        assert_eq!(catalog.workspaces[1].chats, ["chat-1", "chat-2"]);
        assert_eq!(sessions[1].title, "chat-1");
        assert_ne!(sessions[0].id, sessions[1].id);
        assert_eq!(snapshot(dir.path()), before);
    }
    #[test]
    fn source_and_all_entries_unchanged_after_preview_layout_and_metadata_saves() {
        use gitterm_core::preview::{Preview, ProtectedRoots};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let iced = dir.path().join("iced");
        fixture(&iced);
        let before = snapshot(&iced);
        let protected = ProtectedRoots::new(&dir.path().join("home"), [iced.clone()]).unwrap();
        let roots = Arc::new(Preview::validate(&dir.path().join("preview"), protected).unwrap());
        let writer = Arc::new(crate::preview::Writer::new(roots.claim_writer().unwrap()));
        let mut store = WorkspaceStore::guarded(writer.clone());
        let model = load_iced_workspaces_from(&mut store, &iced);
        assert_eq!(model.workspaces.len(), 2);
        writer
            .replace_layout(&roots.layout().join("layout-1.json"), b"{}")
            .unwrap();
        store
            .save_async(&model.folders())
            .unwrap()
            .blocking_recv()
            .unwrap()
            .unwrap();
        assert!(
            writer
                .submit(iced.join("workspaces.json"), b"bad".to_vec())
                .is_err()
        );
        assert!(roots.scoped(&iced.join("new-file")).is_err());
        assert!(
            Preview::validate(
                &iced,
                ProtectedRoots::new(&dir.path().join("home"), [iced.clone()]).unwrap()
            )
            .is_err()
        );
        assert_eq!(snapshot(&iced), before);
    }
    #[test]
    fn missing_or_partial_source_is_empty_despite_saved_local_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::new(&dir.path().join("preview"));
        store.save(&[dir.path().join("stale-folder")]).unwrap();
        let source = dir.path().join("missing-iced");
        assert!(
            load_iced_workspaces_from(&mut store, &source)
                .workspaces
                .is_empty()
        );
        assert!(!source.exists());
        std::fs::create_dir(&source).unwrap();
        for bytes in ["{", "null", r#"{"workspaces":[]}"#] {
            std::fs::write(source.join("workspaces.json"), bytes).unwrap();
            let before = snapshot(&source);
            assert!(
                load_iced_workspaces_from(&mut store, &source)
                    .workspaces
                    .is_empty()
            );
            assert_eq!(snapshot(&source), before);
        }
    }
    #[test]
    fn env_override_reads_fixture() {
        const CHILD: &str = "GITTERM_PSRC_FIXTURE_CHILD";
        if let Some(preview) = std::env::var_os(CHILD) {
            let mut store = WorkspaceStore::new(Path::new(&preview));
            let model = load_iced_workspaces(&mut store);
            assert_eq!(model.workspaces.len(), 2);
            assert_eq!(
                store.iced.as_ref().unwrap().workspaces[1].name,
                "Named work"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let iced = dir.path().join("iced");
        fixture(&iced);
        let before = snapshot(&iced);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_config::iced_tests::env_override_reads_fixture",
                "--nocapture",
            ])
            .env(CHILD, dir.path().join("preview"))
            .env("GITTERM_V6_CONFIG_DIR", &iced)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(snapshot(&iced), before);
    }
}

#[cfg(test)]
mod iced_projection_tests {
    use super::*;
    #[test]
    fn remote_group_order_collapsed_repos_and_invalid_saved_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, r#"{"active_workspace":999,"collapsed_repo_groups":["local"],"workspaces":[
            {"name":"Remote","abbrev":"R","dir":"opaque:remote","color":"blue","location":{"kind":"remote_agent","remote_id":"r","workspace_id":"w","root":"opaque:remote"},"tabs":[]},
            {"name":"Folder","abbrev":"F","dir":"/fixture/folder","color":"blue","tabs":[]},
            {"name":"Work","abbrev":"W","dir":"/fixture/work","color":"blue","chats":["c"],"active_tab":99,"tabs":[{"dir":"/fixture/other"},{"dir":"/fixture/work"}]}
        ]}"#).unwrap();
        let catalog = IcedCatalog::read(dir.path()).unwrap();
        assert_eq!(
            catalog
                .workspaces
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["Folder", "Work", "Remote"]
        );
        assert_eq!(catalog.active, 1);
        assert_eq!(catalog.visible_order(None), [1]);
        assert_eq!(
            catalog.visible_order(Some(crate::data::WorkspaceId(3))),
            [1, 2]
        );
        assert_eq!(crate::data::iced_active_tab(&catalog.workspaces[1]), 1);
        let mut model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
        model.dispatch(
            crate::workspace_status::Intent::Activate(crate::data::WorkspaceId(3)),
            std::time::Instant::now(),
        );
        assert!(
            model
                .dispatch(
                    crate::workspace_status::Intent::Opened,
                    std::time::Instant::now()
                )
                .is_empty()
        );
        assert!(model.workspaces[2].remote);
    }
    #[gpui::test]
    fn imported_panes_keep_selection_without_terminal_or_agent_construction(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        use std::rc::Rc;
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::preview::tests::guarded_store(&dir.path().join("preview"));
        let path = dir.path().join("iced");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("workspaces.json"), r#"{"active_workspace":1,"workspaces":[
            {"name":"First","abbrev":"F","dir":"/fixture/same","color":"blue","tabs":[]},
            {"name":"Selected work","abbrev":"S","dir":"/fixture/same","color":"green","chats":["owned-chat"],"active_tab":1,"tabs":[{"dir":"/fixture/same","startup_command":"MUST NOT RUN"},{"dir":"/fixture/same","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"owned-chat"}]}
        ]}"#).unwrap();
        let model = load_iced_workspaces_from(&mut store, &path);
        let layout = store.guard.as_ref().unwrap().preview().layout();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            let data = Rc::new(crate::data::LocalData(owner.clone()));
            crate::Shell::with_owner(data, owner, &layout, true, window, cx).unwrap()
        });
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(
                shell.active_workspace(cx),
                Some(crate::data::WorkspaceId(2))
            );
            let active = shell.active(cx);
            assert_eq!(active.active_session, Some(active.sessions[1].id));
            assert!(crate::pane_id_visible(
                active,
                &crate::session_pane_id(active.sessions[1].id),
                cx
            ));
            assert!(!crate::pane_id_visible(
                active,
                &crate::session_pane_id(active.sessions[0].id),
                cx
            ));
            for session in &active.sessions {
                let pane = active.panes[&crate::session_pane_id(session.id)].read(cx);
                assert!(matches!(
                    pane.content,
                    crate::pane::PaneContent::IcedSession(_)
                ));
                assert!(pane.session.is_none());
            }
            assert!(shell.owner.read(cx).chats.is_empty());
        });
    }
}

#[cfg(test)]
mod preview_terminal_restore_test {
    use super::*;
    #[gpui::test]
    fn imported_work_restores_and_resaves_only_its_preview_terminal_inventory(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        use std::rc::Rc;
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::preview::tests::guarded_store(&dir.path().join("preview"));
        let source = dir.path().join("iced");
        std::fs::create_dir(&source).unwrap();
        let bytes = br#"{"active_workspace":0,"workspaces":[{"name":"Work","abbrev":"W","dir":"/fixture/work","color":"blue","tabs":[{"dir":"/fixture/work","startup_command":"MUST NOT RUN"}]}]}"#;
        std::fs::write(source.join("workspaces.json"), bytes).unwrap();
        let model = load_iced_workspaces_from(&mut store, &source);
        let layout = store.guard.as_ref().unwrap().preview().layout();
        let key = iced_layout_key(&store.iced.as_ref().unwrap().workspaces[0]);
        let path = crate::layout::workspace_layout_path(&layout, &key);
        let mut saved = crate::layout::LayoutStore::guarded(
            path.clone(),
            store.guard.as_ref().unwrap().clone(),
        );
        saved.terminals = Some(vec![crate::layout::TerminalSession {
            id: 777,
            cwd: dir.path().canonicalize().unwrap(),
            startup_command: None,
            task_id: None,
            task_session_id: None,
        }]);
        let state = gpui_component::dock::DockAreaState {
            version: Some(1),
            center: gpui_component::dock::PanelState {
                panel_name: "StackPanel".into(),
                children: vec![],
                info: gpui_component::dock::PanelInfo::stack(vec![], gpui::Axis::Horizontal),
            },
            ..Default::default()
        };
        saved.save(&state).unwrap();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
            crate::Shell::with_owner(
                Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                &layout,
                true,
                window,
                cx,
            )
            .unwrap()
        });
        cx.read(|cx| {
            let state = shell.read(cx).active(cx);
            assert_eq!(state.sessions.len(), 2);
            let imported = state.panes[&crate::session_pane_id(state.sessions[0].id)].read(cx);
            assert!(matches!(
                imported.content,
                crate::pane::PaneContent::IcedSession(_)
            ));
            assert!(imported.session.is_none());
            assert!(matches!(
                state.panes["session-777"].read(cx).content,
                crate::pane::PaneContent::Terminal(_)
            ));
            assert_eq!(state.store.terminals.as_ref().unwrap()[0].id, 777);
            state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
        });
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["terminal_sessions"][0]["id"], 777);
        assert_eq!(
            std::fs::read(source.join("workspaces.json")).unwrap(),
            bytes
        );
    }
}

#[cfg(test)]
mod live_constructor_tests {
    use super::*;

    #[test]
    fn live_constructor_owns_full_document_and_leaves_preview_projection_read_only() {
        use gitterm_core::desktop_store::LoadState;
        let temp = tempfile::tempdir().unwrap();
        let bytes = br#"{"active_workspace":1,"workspaces":[
            {"name":"Open","abbrev":"O","dir":"/fixture/open","color":"blue","tabs":[]},
            {"name":"Parked","abbrev":"P","dir":"/fixture/parked","color":"red","active":false,"tabs":[]}
        ]}"#;
        std::fs::write(temp.path().join("workspaces.json"), bytes).unwrap();
        let projection = IcedCatalog::read(temp.path()).unwrap();
        assert_eq!(projection.workspaces.len(), 1);
        let lease =
            std::sync::Arc::new(gitterm_core::state_lock::StateLock::acquire(temp.path()).unwrap());
        let store = live_desktop_store(lease).unwrap();
        let LoadState::Valid(file) = store.snapshot().wait().unwrap().workspaces.loaded else {
            panic!("valid document")
        };
        assert_eq!(file.workspaces.len(), 2);
        assert_eq!(file.active_workspace, 1);
        store.drain().wait().unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("workspaces.json")).unwrap(),
            bytes
        );
    }
}
