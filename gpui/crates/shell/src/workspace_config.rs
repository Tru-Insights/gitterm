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
    pub closed: Vec<gitterm_core::workspace_config::WorkspaceConfig>,
    pub claims: std::collections::HashSet<usize>,
    pub order: Vec<usize>,
    pub active: usize,
    pub repos_on_bar: bool,
}
impl IcedCatalog {
    pub fn read(config: &Path) -> Result<Self> {
        let path = config.join("workspaces.json");
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read Iced workspaces {}", path.display()))?;
        let file: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parse Iced workspaces {}", path.display()))?;
        Ok(Self::from_document(config, file))
    }
    pub(crate) fn from_document(
        config: &Path,
        mut file: gitterm_core::workspace_config::WorkspacesFile,
    ) -> Self {
        // Normalize legacy repo records in memory, just as Iced does on restore.
        file.normalize_repos();
        let closed = file
            .workspaces
            .iter()
            .filter(|w| !w.active)
            .cloned()
            .collect();
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
        Self {
            source: config.to_owned(),
            closed,
            claims: Default::default(),
            order: (0..open.len()).collect(),
            repos: file.repos,
            workspaces: open.into_iter().map(|(_, w)| w).collect(),
            active,
            repos_on_bar: !file.collapsed_repo_groups.iter().any(|g| g == "local"),
        }
    }
    pub fn bar_layout(
        &self,
        active: Option<crate::data::WorkspaceId>,
    ) -> gitterm_core::workspace::BarLayout {
        use gitterm_core::workspace::{BarChip, bar_layout};
        let order = self.ordered_indices();
        let chips: Vec<_> = order
            .iter()
            .map(|&index| (index, &self.workspaces[index]))
            .map(|(index, w)| BarChip {
                group: iced_group(w),
                work: gitterm_core::work::catalog::is_work(&w.chats, self.claims.contains(&index)),
                parked: !w.active,
            })
            .collect();
        let active = active
            .and_then(|id| id.0.checked_sub(1))
            .map(|i| i as usize)
            .unwrap_or(self.active);
        let active = order.iter().position(|&index| index == active).unwrap_or(0);
        bar_layout(&chips, active, self.repos_on_bar)
    }
    pub(crate) fn ordered_indices(&self) -> Vec<usize> {
        if self.order.is_empty() {
            (0..self.workspaces.len()).collect()
        } else {
            self.order.clone()
        }
    }
    pub fn visible_order(&self, active: Option<crate::data::WorkspaceId>) -> Vec<usize> {
        let order = self.ordered_indices();
        gitterm_core::workspace::visible_order(&self.bar_layout(active))
            .into_iter()
            .map(|index| order[index])
            .collect()
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
pub(crate) type IcedSettings = gitterm_core::settings::SettingsWithVoice<
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
            "usage": {"pricing": {"fixture": null}}, "stt_enabled": false, "future": true }"#;
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
        assert!(!settings.stt_enabled);
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
            session_uid: None,
            chat_session_id: None,
            bottom: false,
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

/// Installed only after Live admission; Preview has no catalog write authority.
#[derive(Clone)]
pub(crate) struct LiveWorkspaces(pub gitterm_core::desktop_store::DesktopStore);
impl gpui::Global for LiveWorkspaces {}

#[derive(Clone)]
pub(crate) enum LiveWorkspaceAction {
    Create {
        name: String,
        start: gitterm_core::work::catalog::WorkStart,
        home: PathBuf,
    },
    Park {
        active_tab: Option<usize>,
        tabs: Vec<gitterm_core::workspace_config::WorkspaceTabConfig>,
        bottom_terminals: Vec<gitterm_core::workspace_config::BottomTerminalConfig>,
        key: gitterm_core::park::Key,
        info: gitterm_core::park::Info,
        done: bool,
    },
    Resume {
        key: gitterm_core::park::Key,
        reopen: bool,
        open: Vec<gitterm_core::park::Key>,
    },
    Done(gitterm_core::park::Key),
    Return {
        key: gitterm_core::park::Key,
        ordinal: usize,
    },
}

pub(crate) struct LiveWorkspaceSaved {
    pub repos: Vec<gitterm_core::workspace_config::RepoConfig>,
    pub workspace: gitterm_core::workspace_config::WorkspaceConfig,
    pub closed: Vec<gitterm_core::workspace_config::WorkspaceConfig>,
}

/// Runs on a background executor; DesktopStore owns the complete document and
/// publishes no candidate on a failed save. UI callers apply effects only on Ok.
pub(crate) fn save_live_workspace(
    store: &gitterm_core::desktop_store::DesktopStore,
    action: LiveWorkspaceAction,
    active: Option<gitterm_core::park::Key>,
) -> Result<LiveWorkspaceSaved, String> {
    use gitterm_core::{park, workspace_config::ParkedInfo};
    let created = if let LiveWorkspaceAction::Create { name, home, .. } = &action {
        Some(
            gitterm_core::workspace_config::create_work_folder(&home.join("Work"), name).map_err(
                |error| {
                    format!(
                        "cannot create a folder in {}: {error}",
                        home.join("Work").display()
                    )
                },
            )?,
        )
    } else {
        None
    };
    let folder = created.clone();
    let result = store
        .transact_workspaces(move |file| {
            let previous_active = active.as_ref().and_then(|key| file.workspaces.iter().filter(|w| w.active).position(|w| key.matches(w))).unwrap_or(file.active_workspace);
            let returning_at = if let LiveWorkspaceAction::Return { ordinal, .. } = &action { Some(*ordinal) } else { None };
            let target = match &action {
                LiveWorkspaceAction::Create { .. } => None,
                LiveWorkspaceAction::Park { key, .. } | LiveWorkspaceAction::Resume { key, .. } | LiveWorkspaceAction::Done(key) | LiveWorkspaceAction::Return { key, .. } => Some(key),
            };
            if let Some(key) = target && file.workspaces.iter().filter(|w| key.matches(w)).count() > 1 {
                return Err(format!("Several saved workspaces are named {} in {}; this workspace cannot be changed in Live.", key.name, key.dir));
            }
            if let Some(workspace) = target.and_then(|key| file.workspaces.iter().find(|w| key.matches(w)))
                && (workspace.is_legacy_remote() || matches!(workspace.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. }))) {
                return Err("Remote workspaces cannot be changed in Live yet.".into());
            }
            let mut workspace = match action {
                LiveWorkspaceAction::Create { name, start, .. } => {
                    use gitterm_core::settings::WorkspaceColor as C;
                    let used: Vec<_> = file
                        .workspaces
                        .iter()
                        .filter(|w| w.active)
                        .map(|w| w.color)
                        .collect();
                    let colors = [
                        C::Lavender,
                        C::Blue,
                        C::Green,
                        C::Peach,
                        C::Pink,
                        C::Yellow,
                        C::Red,
                        C::Teal,
                    ];
                    let color = colors
                        .iter()
                        .copied()
                        .find(|c| !used.contains(c))
                        .unwrap_or(colors[used.len() % colors.len()]);
                    let dir = folder.as_ref().expect("created folder");
                    let mut workspace = new_work_config(&name, dir, color, start);
                    workspace.tabs.push(crate::chat::resume::tab(
                        dir,
                        crate::sessions::fresh_target().session_uid,
                        None,
                    ));
                    workspace
                }
                LiveWorkspaceAction::Park {
                    key,
                    info,
                    done,
                    tabs,
                    bottom_terminals,
                    active_tab,
                } => {
                    let index = file
                        .workspaces
                        .iter()
                        .position(|w| key.matches(w) && (w.active || w.is_parked()))
                        .ok_or_else(|| format!("Workspace {} is no longer open", key.name))?;
                    let mut workspace = park::parked_config(
                        file.workspaces.remove(index),
                        ParkedInfo {
                            note: info.note,
                            parked_at: info.parked_at,
                        },
                    );
                    let mut tabs = tabs;
                    // A provider acknowledgement can reach the serialized owner
                    // after the UI captured its park request. Keep that metadata
                    // and fields this desktop cannot render.
                    for tab in &mut tabs {
                        if tab.session_uid.is_some()
                            && let Some(confirmed) = workspace.tabs.iter().find(|saved| saved.session_uid == tab.session_uid) {
                            let mut extra = confirmed.extra.clone();
                            extra.extend(tab.extra.clone());
                            tab.extra = extra;
                            if confirmed.chat_session_id.is_some()
                                && (tab.chat_session_id.is_none() || tab.chat_session_id == confirmed.chat_session_id) {
                                tab.chat_session_id = confirmed.chat_session_id.clone();
                                tab.agent_config = confirmed.agent_config.clone();
                                tab.repo_dir = confirmed.repo_dir.clone();
                            }
                        }
                    }
                    workspace.tabs = tabs;
                    workspace.bottom_terminals = bottom_terminals;
                    workspace.active_tab = active_tab;
                    if done {
                        workspace.done_at = Some(chrono::Utc::now().to_rfc3339());
                    }
                    workspace
                }
                LiveWorkspaceAction::Resume { key, reopen, open } => {
                    if reopen {
                        park::reopen_done(&mut file.workspaces, &key);
                    }
                    let index = file
                        .workspaces
                        .iter()
                        .position(|w| key.matches(w) && w.is_parked())
                        .ok_or_else(|| format!("Workspace {} is no longer parked", key.name))?;
                    let mut workspace = file.workspaces.remove(index);
                    // Reuse Iced's conversation ownership and active-tab mapping.
                    let state = park::State {
                        open: file.workspaces.iter().filter(|w| w.active || open.iter().any(|key| key.matches(w))).map(|w| park::OpenWorkspace {
                            target: park::Target { uid: 0, tabs: vec![] },
                            key: park::Key { name: w.name.clone(), dir: w.dir.clone() },
                            running: None, remote_id: None,
                            conversations: w.tabs.iter().filter_map(|t| t.chat_session_id.clone()).collect(),
                        }).collect(),
                        closed: vec![],
                    };
                    let park::Effect::FilteredRestore { indices, active_tab } = state.reduce(park::Intent::FilterRestore {
                        remote_id: None, tabs: workspace.tabs.iter().map(|t| t.session_identity()).collect(), active_tab: workspace.active_tab,
                    }) else { unreachable!("filter restore effect") };
                    workspace.tabs = workspace.tabs.into_iter().enumerate().filter_map(|(i,t)| indices.contains(&i).then_some(t)).collect();
                    workspace.active_tab = active_tab;
                    if workspace.tabs.is_empty() {
                        workspace.tabs.push(gitterm_core::workspace_config::WorkspaceTabConfig {
                            extra: Default::default(),
                            dir: workspace.dir.clone(), repo_dir: Some(workspace.dir.clone()), startup_command: None, tab_kind: None, agent_config: None, chat_session_id: None, task_id: None, task_session_id: None, worker_shutdown_refused: false, session_uid: None,
                        });
                        workspace.active_tab = Some(0);
                    }
                    for tab in &mut workspace.tabs { if tab.session_uid.is_none() { tab.session_uid = Some(crate::sessions::fresh_target().session_uid); } }
                    workspace.active = true;
                    workspace.parked = None;
                    workspace.done_at = None;
                    workspace
                }
                LiveWorkspaceAction::Return { key, .. } => {
                    let index = file.workspaces.iter().position(|w| key.matches(w) && w.is_parked()).ok_or_else(|| format!("Workspace {} is no longer parked", key.name))?;
                    let mut workspace = file.workspaces.remove(index);
                    workspace.active = true;
                    workspace.parked = None;
                    workspace
                }
                LiveWorkspaceAction::Done(key) => {
                    if !park::finish_parked(
                        &mut file.workspaces,
                        &key,
                        chrono::Utc::now().to_rfc3339(),
                    ) {
                        return Err(format!("Workspace {} is no longer parked", key.name));
                    }
                    file.workspaces
                        .iter()
                        .find(|w| key.matches(w) && !w.active && w.done_at.is_some())
                        .unwrap()
                        .clone()
                }
            };
            // Mirror Iced's open-then-closed full-document order. Running parks are
            // saved closed too; their runtime ownership stays exclusively in Shell.
            let key = park::Key {
                name: workspace.name.clone(),
                dir: workspace.dir.clone(),
            };
            if !file.workspaces.iter().any(|w| {
                key.matches(w) && w.done_at == workspace.done_at && w.active == workspace.active
            }) {
                file.workspaces.push(workspace.clone());
            }
            file.workspaces.sort_by_key(|w| !w.active);
            if let Some(ordinal) = returning_at {
                let index = file.workspaces.iter().position(|w| w.active && key.matches(w)).expect("returned workspace");
                let returned = file.workspaces.remove(index);
                let at = ordinal.min(file.workspaces.iter().filter(|w| w.active).count());
                file.workspaces.insert(at, returned);
            }
            if let Some(selected) = active.as_ref().or(workspace.active.then_some(&key)) {
                file.active_workspace = file
                    .workspaces
                    .iter()
                    .filter(|w| w.active)
                    .position(|w| selected.matches(w))
                    .unwrap_or_else(|| {
                        previous_active.min(
                            file.workspaces
                                .iter()
                                .filter(|w| w.active)
                                .count()
                                .saturating_sub(1),
                        )
                    });
            }
            file.normalize_repos();
            // Return the normalized wire DTO, including all tabs/settings/notes.
            if let Some(saved) = file
                .workspaces
                .iter()
                .find(|w| key.matches(w) && w.active == workspace.active)
            {
                workspace = saved.clone();
            }
            Ok(LiveWorkspaceSaved {
                workspace,
                repos: file.repos.clone(),
                closed: file
                    .workspaces
                    .iter()
                    .filter(|w| !w.active)
                    .cloned()
                    .collect(),
            })
        })
        .wait();
    if result.is_err()
        && let Some(created) = created
    {
        // Only our newly-created, still empty folder can be removed on failure.
        // Never remove user files which appeared while the save was in flight.
        if let Err(error) = std::fs::remove_dir(&created) {
            eprintln!("Remove unsaved workspace {}: {error}", created.display());
        }
    }
    result
}

fn new_work_config(
    name: &str,
    dir: &Path,
    color: gitterm_core::settings::WorkspaceColor,
    start: gitterm_core::work::catalog::WorkStart,
) -> gitterm_core::workspace_config::WorkspaceConfig {
    use gitterm_core::workspace_config::WorkspaceConfig;
    let dir = dir.to_string_lossy().into_owned();
    let added: Vec<String> = start
        .repos
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    WorkspaceConfig {
        extra: Default::default(),
        collapsed_issue_groups: gitterm_core::issues::IssueGroup::default_collapsed(),
        name: name.into(),
        abbrev: name.chars().take(2).collect::<String>().to_uppercase(),
        dir: dir.clone(),
        color,
        location: Some(
            gitterm_core::remote::config::WorkspaceLocationConfig::Local { root: dir.clone() },
        ),
        remote_session: None,
        active_tab: Some(0),
        tabs: vec![],
        run_command: None,
        bottom_terminals: vec![],
        env: Default::default(),
        active: true,
        chats: vec![],
        parked: None,
        done_at: None,
        guidelines_off: false,
        claude_in_chrome: false,
        decisions_repo: start
            .decisions_repo
            .map(|p| p.to_string_lossy().into_owned()),
        repos: std::iter::once(dir.clone())
            .chain(added.iter().filter(|r| **r != dir).cloned())
            .collect(),
        home: Some(dir),
        env_overrides: Default::default(),
        repos_added: added,
        repos_removed: vec![],
    }
}

#[cfg(test)]
mod live_workspace_tests {
    use super::*;
    use gitterm_core::{
        desktop_store::{DesktopStore, LoadState},
        park::{Info, Key},
        workspace_config::WorkspacesFile,
    };
    fn isolated(name: &str, test: impl FnOnce(&Path, &Path)) {
        if std::env::var("GITTERM_WORKSPACE_CONTRACT").as_deref() == Ok(name) {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            let root = PathBuf::from(std::env::var_os("GITTERM_V6_CONFIG_DIR").unwrap());
            assert!(root.is_absolute() && home.is_absolute());
            test(&root, &home);
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("desktop");
        let home = fixture.path().join("home");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&home).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("workspace_config::live_workspace_tests::{name}"),
                "--nocapture",
            ])
            .env("GITTERM_WORKSPACE_CONTRACT", name)
            .env("HOME", home)
            .env("GITTERM_V6_CONFIG_DIR", root)
            .status()
            .unwrap();
        assert!(status.success());
    }
    fn fixture(root: &Path) -> Key {
        // The original Iced format, before repo normalization and park notes.
        std::fs::write(root.join("workspaces.json"), br#"{"active_workspace":0,"workspaces":[
            {"name":"Open","abbrev":"O","dir":"/fixture/open","color":"blue","tabs":[]},
            {"name":"Old work","abbrev":"OW","dir":"/fixture/old","color":"green","active":false,"chats":["owned"],"tabs":[{"dir":"/fixture/repo","tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"owned"}],"env":{"KEEP":"yes"}}
        ]}"#).unwrap();
        Key {
            name: "Old work".into(),
            dir: "/fixture/old".into(),
        }
    }
    fn document(root: &Path) -> WorkspacesFile {
        serde_json::from_slice(&std::fs::read(root.join("workspaces.json")).unwrap()).unwrap()
    }
    #[test]
    fn old_iced_park_round_trip() {
        isolated("old_iced_park_round_trip", |root, _| {
            let key = fixture(root);
            let store = DesktopStore::open(root).unwrap();
            let old = document(root).workspaces[1].clone();
            let resumed = save_live_workspace(
                &store,
                LiveWorkspaceAction::Resume {
                    key: key.clone(),
                    reopen: false,
                    open: vec![],
                },
                None,
            )
            .unwrap();
            assert!(resumed.workspace.active);
            assert_eq!(resumed.workspace.env, old.env);
            let resumed_uid = resumed.workspace.tabs[0].session_uid.clone();
            assert!(resumed_uid.as_ref().is_some_and(|uid| !uid.is_empty()));
            // Iced assigns a caller id when an old tab predates that field.
            let mut expected_tabs = old.tabs.clone();
            expected_tabs[0].session_uid = resumed_uid.clone();
            assert_eq!(
                serde_json::to_value(&resumed.workspace.tabs).unwrap(),
                serde_json::to_value(&expected_tabs).unwrap()
            );
            let info = Info::new("  come back here  ", "2026-11-27T20:00:00Z".into());
            let parked = save_live_workspace(
                &store,
                LiveWorkspaceAction::Park {
                    active_tab: resumed.workspace.active_tab,
                    key: key.clone(),
                    info: info.clone(),
                    done: false,
                    bottom_terminals: resumed.workspace.bottom_terminals,
                    tabs: resumed.workspace.tabs,
                },
                None,
            )
            .unwrap();
            assert!(!parked.workspace.active);
            assert_eq!(parked.workspace.parked.unwrap().note, "come back here");
            drop(store);
            let restarted = DesktopStore::open(root).unwrap();
            let done =
                save_live_workspace(&restarted, LiveWorkspaceAction::Done(key.clone()), None)
                    .unwrap();
            assert!(done.workspace.done_at.is_some());
            let reopened = save_live_workspace(
                &restarted,
                LiveWorkspaceAction::Resume {
                    key,
                    reopen: true,
                    open: vec![],
                },
                None,
            )
            .unwrap();
            assert!(reopened.workspace.active && reopened.workspace.done_at.is_none());
            assert_eq!(reopened.workspace.chats, ["owned"]);
            assert_eq!(reopened.workspace.tabs.len(), 1);
            assert_eq!(reopened.workspace.tabs[0].session_uid, resumed_uid);
        });
    }
    #[test]
    fn cancel_creates_no_folder_or_document() {
        isolated("cancel_creates_no_folder_or_document", |root, home| {
            // Cancel emits no catalog mutation, even after editing the draft.
            let mut sheet = gitterm_core::work::catalog::NewWorkSheet {
                name: "Cancelled work".into(),
                ..Default::default()
            };
            sheet.use_picked_folder(home.join("repo"));
            let store = DesktopStore::open(root).unwrap();
            drop(sheet);
            store.drain().wait().unwrap();
            assert!(!root.join("workspaces.json").exists());
            assert!(!home.join("Work").exists());
        });
    }
    #[test]
    fn failed_save_keeps_old_file_and_owner_snapshot() {
        isolated(
            "failed_save_keeps_old_file_and_owner_snapshot",
            |root, _| {
                use std::os::unix::fs::PermissionsExt;
                let key = fixture(root);
                let bytes = std::fs::read(root.join("workspaces.json")).unwrap();
                let store = DesktopStore::open(root).unwrap();
                std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555)).unwrap();
                let result = save_live_workspace(
                    &store,
                    LiveWorkspaceAction::Resume {
                        key,
                        reopen: false,
                        open: vec![],
                    },
                    None,
                );
                std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
                assert!(result.is_err());
                assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), bytes);
                let snapshot = store.snapshot().wait().unwrap();
                assert!(!snapshot.workspaces.dirty);
                let LoadState::Valid(file) = snapshot.workspaces.loaded else {
                    panic!("valid")
                };
                assert!(!file.workspaces[1].active);
                store.drain().wait().unwrap();
                assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), bytes);
            },
        );
    }
    #[test]
    fn create_plans_root_and_bidirectional_wire_contract() {
        isolated(
            "create_plans_root_and_bidirectional_wire_contract",
            |root, home| {
                fixture(root);
                let store = DesktopStore::open(root).unwrap();
                let repo = home.join("repo");
                let created = save_live_workspace(
                    &store,
                    LiveWorkspaceAction::Create {
                        name: "Cloud migration".into(),
                        home: home.into(),
                        start: gitterm_core::work::catalog::WorkStart::new(
                            vec![repo.clone()],
                            Some(repo.clone()),
                        )
                        .unwrap(),
                    },
                    None,
                )
                .unwrap();
                assert_eq!(
                    created.workspace.dir,
                    home.join("Work/cloud-migration").to_string_lossy()
                );
                assert_eq!(
                    created.workspace.decisions_repo,
                    Some(repo.to_string_lossy().into_owned())
                );
                assert_eq!(created.workspace.repos_added, [repo.to_string_lossy()]);
                let (plans, docs) = gitterm_core::workspace_config::document_roots(Path::new(
                    &created.workspace.dir,
                ));
                assert_eq!(plans, home.join("Work/cloud-migration/.plans"));
                assert_eq!(docs, home.join("Work/cloud-migration/docs"));
                // GPUI reads Iced's shared DTO, and Iced's reader accepts GPUI's write.
                let iced: WorkspacesFile = document(root);
                let gpui = IcedCatalog::read(root).unwrap();
                let saved = gpui
                    .workspaces
                    .iter()
                    .find(|w| w.name == "Cloud migration")
                    .unwrap();
                let from_iced = iced
                    .workspaces
                    .iter()
                    .find(|w| w.name == "Cloud migration")
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(saved).unwrap(),
                    serde_json::to_value(from_iced).unwrap()
                );
                assert_eq!(iced.active_workspace, 1);
                drop(store);
                std::fs::write(
                    root.join("workspaces.json"),
                    serde_json::to_vec_pretty(&iced).unwrap(),
                )
                .unwrap();
                assert_eq!(IcedCatalog::read(root).unwrap().workspaces[1].tabs.len(), 1);
            },
        );
    }
    #[test]
    fn resume_skips_a_chat_open_in_a_running_park() {
        isolated("resume_skips_a_chat_open_in_a_running_park", |root, _| {
            let key = fixture(root);
            let mut file = document(root);
            let tab = file.workspaces[1].tabs[0].clone();
            file.workspaces[0].active = false;
            file.workspaces[0].tabs.push(tab);
            let running = Key {
                name: file.workspaces[0].name.clone(),
                dir: file.workspaces[0].dir.clone(),
            };
            std::fs::write(
                root.join("workspaces.json"),
                serde_json::to_vec(&file).unwrap(),
            )
            .unwrap();
            let store = DesktopStore::open(root).unwrap();
            let restored = save_live_workspace(
                &store,
                LiveWorkspaceAction::Resume {
                    key,
                    reopen: false,
                    open: vec![running],
                },
                None,
            )
            .unwrap();
            assert_eq!(restored.workspace.chats, ["owned"]);
            assert!(
                restored
                    .workspace
                    .tabs
                    .iter()
                    .all(|tab| tab.chat_session_id.is_none())
            );
            assert_eq!(restored.workspace.tabs.len(), 1); // Iced's default shell after filtering.
        });
    }
    #[test]
    fn ambiguous_legacy_identity_never_changes_the_wrong_workspace() {
        isolated(
            "ambiguous_legacy_identity_never_changes_the_wrong_workspace",
            |root, _| {
                let key = fixture(root);
                let mut file = document(root);
                file.workspaces.push(file.workspaces[1].clone());
                let bytes = serde_json::to_vec(&file).unwrap();
                std::fs::write(root.join("workspaces.json"), &bytes).unwrap();
                let store = DesktopStore::open(root).unwrap();
                assert!(
                    save_live_workspace(
                        &store,
                        LiveWorkspaceAction::Resume {
                            key,
                            reopen: false,
                            open: vec![]
                        },
                        None
                    )
                    .is_err()
                );
                assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), bytes);
                store.drain().wait().unwrap();
                assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), bytes);
            },
        );
    }
    #[test]
    fn park_keeps_a_provider_identity_confirmed_after_ui_capture() {
        isolated(
            "park_keeps_a_provider_identity_confirmed_after_ui_capture",
            |root, _| {
                let key = fixture(root);
                let mut file = document(root);
                file.workspaces[1].active = true;
                file.workspaces[1].tabs[0].session_uid = Some("caller".into());
                let mut captured = file.workspaces[1].tabs.clone();
                captured[0].chat_session_id = None;
                std::fs::write(
                    root.join("workspaces.json"),
                    serde_json::to_vec(&file).unwrap(),
                )
                .unwrap();
                let store = DesktopStore::open(root).unwrap();
                let saved = save_live_workspace(
                    &store,
                    LiveWorkspaceAction::Park {
                        key,
                        info: Info::new("note", "2026-11-27T00:00:00Z".into()),
                        done: false,
                        active_tab: Some(0),
                        bottom_terminals: file.workspaces[1].bottom_terminals.clone(),
                        tabs: captured,
                    },
                    None,
                )
                .unwrap();
                assert_eq!(
                    saved.workspace.tabs[0].chat_session_id.as_deref(),
                    Some("owned")
                );
            },
        );
    }
}
