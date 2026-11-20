use crate::pane::PANEL_NAME;
use anyhow::{Context as _, Result, bail, ensure};
use gpui_component::dock::{DockAreaState, DockPlacement, PanelInfo, PanelState};
#[cfg(test)]
use std::io::Write;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[cfg(test)]
use crate::preview::config_dir;

pub const WINDOW_WIDTH: f32 = 1400.;
pub const WINDOW_HEIGHT: f32 = 800.;
pub const SPINE_WIDTH: f32 = 36.;
pub const SESSION_HEIGHT: f32 = 36.;
pub const WORKSPACE_BAR_HEIGHT: f32 = 28.;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SidebarMode {
    #[default]
    Work,
    Files,
    Find,
    Git,
}

/// Stored alongside the retained pane inventory in the existing layout file.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RegionLayout {
    pub fonts: Option<crate::theme::FontSizes>,
    pub chat_font_size: Option<f32>,
    pub chat_width: Option<gitterm_core::settings::ChatWidth>,
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    pub sidebar_mode: SidebarMode,
    pub bottom_height: f32,
    pub bottom_visible: bool,
    pub bottom_opened: bool,
}
impl Default for RegionLayout {
    fn default() -> Self {
        Self {
            fonts: None,
            chat_font_size: None,
            chat_width: None,
            sidebar_width: 360.,
            sidebar_collapsed: false,
            sidebar_mode: SidebarMode::Work,
            bottom_height: 200.,
            bottom_visible: true,
            bottom_opened: false,
        }
    }
}
impl RegionLayout {
    pub fn resize_sidebar(&mut self, width: f32) {
        self.sidebar_width = width.clamp(150., 600.);
    }
    pub fn sidebar_size(&self) -> f32 {
        if self.sidebar_collapsed {
            36.
        } else {
            self.sidebar_width
        }
    }
    pub fn has_bottom(&self, explicit_terminal: bool) -> bool {
        (self.bottom_opened || explicit_terminal) && self.bottom_visible
    }
    fn validate(&mut self) -> Result<()> {
        ensure!(
            self.sidebar_width.is_finite() && self.bottom_height.is_finite(),
            "invalid region geometry"
        );
        self.resize_sidebar(self.sidebar_width);
        if let Some(fonts) = &mut self.fonts {
            fonts.ui = fonts.ui.clamp(10., 24.);
            fonts.terminal = fonts.terminal.clamp(10., 24.);
            ensure!(
                fonts.ui.is_finite() && fonts.terminal.is_finite(),
                "invalid font sizes"
            );
        }
        if let Some(size) = &mut self.chat_font_size {
            ensure!(size.is_finite(), "invalid chat font size");
            *size = size.clamp(11., 22.);
        }
        ensure!(self.bottom_height > 0., "invalid bottom panel height");
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalSession {
    pub id: u64,
    pub cwd: PathBuf,
}

pub struct LayoutStore {
    pub terminals: Option<Vec<TerminalSession>>,
    pub regions: RegionLayout,
    pub path: PathBuf,
    refused: Option<String>,
    guard: Option<std::sync::Arc<crate::preview::Writer>>,
}
impl LayoutStore {
    #[cfg(test)]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            terminals: None,
            regions: RegionLayout::default(),
            refused: None,
            guard: None,
        }
    }
    pub(crate) fn guarded(path: PathBuf, guard: std::sync::Arc<crate::preview::Writer>) -> Self {
        Self {
            path,
            terminals: None,
            regions: RegionLayout::default(),
            refused: None,
            guard: Some(guard),
        }
    }
    fn check(&self) -> Result<()> {
        if let Some(guard) = &self.guard {
            guard.preview().scoped(&self.path)?;
        }
        Ok(())
    }
    pub fn load_terminals(&mut self) -> Result<Option<Vec<TerminalSession>>> {
        let result = (|| -> Result<_> {
            self.check()?;
            let bytes = match std::fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let Some(value) = value.get("terminal_sessions") else {
                return Ok(None);
            };
            let mut sessions: Vec<TerminalSession> = serde_json::from_value(value.clone())?;
            let mut seen = HashSet::new();
            sessions.retain(|session| seen.insert(session.id));
            ensure!(
                sessions.iter().all(|s| s.cwd.is_absolute()),
                "terminal cwd must be absolute"
            );
            Ok(Some(sessions))
        })();
        if let Err(error) = &result {
            self.refused = Some(format!("{error:#}"));
        }
        result
    }
    pub fn load(&mut self, known: &HashSet<String>) -> Result<Option<DockAreaState>> {
        let result = (|| -> Result<Option<DockAreaState>> {
            self.check()?;
            let bytes = match std::fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            if let Some(reason) = &self.refused {
                bail!("{reason}");
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let mut regions: RegionLayout = value
                .get("regions")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            let has_regions = value.get("regions").is_some();
            let mut state: DockAreaState = serde_json::from_value(value)?;
            validate(&state, self.terminals.is_some())?;
            if !has_regions && let Some(bottom) = &state.bottom_dock {
                regions.bottom_height = f32::from(bottom.size());
                regions.bottom_visible = bottom.open();
            }
            regions.validate()?;
            self.regions = regions;
            let mut seen = HashSet::new();
            prune_unknown(&mut state.center, known, &mut seen);
            for dock in [
                &mut state.left_dock,
                &mut state.right_dock,
                &mut state.bottom_dock,
            ]
            .into_iter()
            .flatten()
            {
                let mut panel = dock.panel().clone();
                prune_unknown(&mut panel, known, &mut seen);
                *dock = gpui_component::dock::DockState::new(
                    panel,
                    dock.placement(),
                    dock.size(),
                    dock.open(),
                );
            }
            Ok(Some(state))
        })()
        .with_context(|| format!("load dock layout {}", self.path.display()));
        if let Err(error) = &result {
            self.refused = Some(format!("{error:#}"));
        }
        result
    }
    pub fn is_refused(&self) -> bool {
        self.refused.is_some()
    }
    fn bytes(&self, state: &DockAreaState) -> Result<Vec<u8>> {
        if let Some(reason) = &self.refused {
            bail!("refusing to overwrite {}: {reason}", self.path.display());
        }
        let mut value = serde_json::to_value(state)?;
        value["regions"] = serde_json::to_value(&self.regions)?;
        if let Some(terminals) = &self.terminals {
            value["terminal_sessions"] = serde_json::to_value(terminals)?;
        }
        Ok(serde_json::to_vec_pretty(&value)?)
    }
    pub(crate) fn save_async(&self, state: &DockAreaState) -> Result<crate::preview::SaveResult> {
        let bytes = self.bytes(state)?;
        if let Some(guard) = &self.guard {
            return guard.submit(self.path.clone(), bytes);
        }
        #[cfg(test)]
        {
            self.save(state)?;
            let (reply, receipt) = tokio::sync::oneshot::channel();
            let _ = reply.send(Ok(()));
            Ok(receipt)
        }
        #[cfg(not(test))]
        bail!("layout writer has no preview guard")
    }
    #[cfg(test)]
    pub fn save(&self, state: &DockAreaState) -> Result<()> {
        if let Some(reason) = &self.refused {
            bail!("refusing to overwrite {}: {reason}", self.path.display());
        }
        (|| -> Result<()> {
            let bytes = self.bytes(state)?;
            if let Some(guard) = &self.guard {
                guard.replace_layout(&self.path, &bytes)?;
            } else {
                #[cfg(not(test))]
                bail!("layout writer has no preview guard");
                #[cfg(test)]
                {
                    let dir = self.path.parent().context("layout has no parent")?;
                    std::fs::create_dir_all(dir)?;
                    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
                    temp.write_all(&bytes)?;
                    temp.as_file().sync_all()?;
                    temp.persist(&self.path)?;
                }
            }
            Ok(())
        })()
        .with_context(|| format!("save dock layout {}", self.path.display()))
    }
}

fn validate(state: &DockAreaState, recover_duplicates: bool) -> Result<()> {
    ensure!(
        state.version == Some(1),
        "unsupported dock layout version {:?}",
        state.version
    );
    fn visit(
        node: &PanelState,
        seen: &mut HashSet<String>,
        depth: usize,
        recover_duplicates: bool,
    ) -> Result<()> {
        ensure!(depth < 32, "dock layout nesting exceeds 32 levels");
        match &node.info {
            PanelInfo::Panel(value) => {
                ensure!(
                    node.panel_name == PANEL_NAME && node.children.is_empty(),
                    "unknown dock panel {}",
                    node.panel_name
                );
                let id: String = serde_json::from_value(value.clone())?;
                ensure!(
                    seen.insert(id.clone()) || recover_duplicates,
                    "duplicate pane ID {id}"
                );
            }
            PanelInfo::Stack { sizes, axis } => {
                ensure!(
                    *axis <= 1 && sizes.len() == node.children.len(),
                    "invalid split geometry"
                );
                ensure!(
                    sizes
                        .iter()
                        .all(|p| f32::from(*p).is_finite() && *p >= gpui::px(0.)),
                    "invalid split size"
                );
            }
            PanelInfo::Tabs { active_index } => {
                ensure!(
                    *active_index < node.children.len().max(1),
                    "invalid active dock tab"
                );
                ensure!(
                    node.children
                        .iter()
                        .all(|n| matches!(n.info, PanelInfo::Panel(_))),
                    "nested tab group"
                );
            }
        }
        for child in &node.children {
            visit(child, seen, depth + 1, recover_duplicates)?;
        }
        Ok(())
    }
    let mut seen = HashSet::new();
    visit(&state.center, &mut seen, 0, recover_duplicates)?;
    for (dock, placement) in [
        (&state.left_dock, DockPlacement::Left),
        (&state.right_dock, DockPlacement::Right),
        (&state.bottom_dock, DockPlacement::Bottom),
    ] {
        if let Some(dock) = dock {
            ensure!(
                dock.placement() == placement
                    && f32::from(dock.size()).is_finite()
                    && dock.size() > gpui::px(0.),
                "invalid dock geometry"
            );
            visit(dock.panel(), &mut seen, 0, recover_duplicates)?;
        }
    }
    Ok(())
}

/// Remove panes that no longer exist while retaining the remaining dock tree.
fn prune_unknown(
    node: &mut PanelState,
    known: &HashSet<String>,
    seen: &mut HashSet<String>,
) -> bool {
    if let PanelInfo::Panel(value) = &node.info {
        return value
            .as_str()
            .is_some_and(|id| known.contains(id) && seen.insert(id.to_string()));
    }
    let old_children = std::mem::take(&mut node.children);
    let mut kept = Vec::with_capacity(old_children.len());
    let mut kept_indices = Vec::with_capacity(old_children.len());
    for (index, mut child) in old_children.into_iter().enumerate() {
        if prune_unknown(&mut child, known, seen) {
            kept.push(child);
            kept_indices.push(index);
        }
    }
    match &mut node.info {
        PanelInfo::Stack { sizes, .. } => {
            let old_sizes = std::mem::take(sizes);
            *sizes = kept_indices
                .iter()
                .filter_map(|index| old_sizes.get(*index).copied())
                .collect();
        }
        PanelInfo::Tabs { active_index } => {
            *active_index = kept_indices
                .iter()
                .position(|index| index == active_index)
                .unwrap_or(0);
        }
        PanelInfo::Panel(_) => unreachable!(),
    }
    node.children = kept;
    true
}

pub fn pane_order(state: &DockAreaState) -> Vec<String> {
    fn visit(node: &PanelState, ids: &mut Vec<String>) {
        if let PanelInfo::Panel(value) = &node.info
            && let Some(id) = value.as_str()
        {
            ids.push(id.to_string());
        }
        for child in &node.children {
            visit(child, ids);
        }
    }
    let mut ids = vec![];
    visit(&state.center, &mut ids);
    for dock in [&state.left_dock, &state.right_dock, &state.bottom_dock]
        .into_iter()
        .flatten()
    {
        visit(dock.panel(), &mut ids);
    }
    ids
}

pub fn workspace_layout_key(path: &Path) -> String {
    format!("workspace-{:016x}", stable_path_hash(path))
}

pub fn stable_path_hash(path: &Path) -> u64 {
    // FNV-1a is used here because DefaultHasher does not promise stable output.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn workspace_layout_path(config_dir: &Path, workspace_key: &str) -> PathBuf {
    config_dir.join(format!("layout-{workspace_key}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::dock::DockState;
    fn fixture() -> DockAreaState {
        let leaf = PanelState {
            panel_name: PANEL_NAME.into(),
            children: vec![],
            info: PanelInfo::panel(serde_json::json!("session-11")),
        };
        DockAreaState {
            version: Some(1),
            center: PanelState {
                panel_name: "StackPanel".into(),
                children: vec![],
                info: PanelInfo::stack(vec![], gpui::Axis::Horizontal),
            },
            bottom_dock: Some(DockState::new(
                PanelState {
                    panel_name: "TabPanel".into(),
                    children: vec![leaf],
                    info: PanelInfo::tabs(0),
                },
                DockPlacement::Bottom,
                gpui::px(280.),
                false,
            )),
            ..Default::default()
        }
    }
    #[test]
    fn region_defaults_clamp_collapse_and_require_explicit_terminal() {
        let mut regions = RegionLayout::default();
        assert_eq!(regions.sidebar_size(), 360.);
        assert_eq!(regions.bottom_height, 200.);
        assert!(!regions.has_bottom(false));
        assert!(regions.has_bottom(true));
        regions.resize_sidebar(0.);
        assert_eq!(regions.sidebar_size(), 150.);
        regions.resize_sidebar(1000.);
        assert_eq!(regions.sidebar_size(), 600.);
        regions.sidebar_collapsed = true;
        assert_eq!(regions.sidebar_size(), 36.);
        regions.sidebar_collapsed = false;
        assert_eq!(regions.sidebar_size(), 600.);
    }
    #[test]
    fn region_settings_round_trip_in_existing_layout_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        let mut store = LayoutStore::new(path.clone());
        store.regions = RegionLayout {
            fonts: None,
            chat_font_size: None,
            chat_width: Some(gitterm_core::settings::ChatWidth::Narrow),
            sidebar_width: 492.,
            sidebar_collapsed: true,
            sidebar_mode: SidebarMode::Files,
            bottom_height: 321.,
            bottom_visible: false,
            bottom_opened: true,
        };
        store.save(&fixture()).unwrap();
        let mut restored = LayoutStore::new(path);
        restored
            .load(&HashSet::from(["session-11".into()]))
            .unwrap();
        assert_eq!(restored.regions, store.regions);
        let legacy = serde_json::to_vec(&fixture()).unwrap();
        std::fs::write(&restored.path, legacy).unwrap();
        restored
            .load(&HashSet::from(["session-11".into()]))
            .unwrap();
        assert_eq!(
            restored.regions,
            RegionLayout {
                bottom_height: 280.,
                bottom_visible: false,
                ..Default::default()
            }
        );
    }
    #[test]
    fn layout_round_trip_preserves_tabs_sizes_and_collapsed_dock() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LayoutStore::new(dir.path().join("layout.json"));
        assert!(store.load(&HashSet::new()).unwrap().is_none());
        let state = fixture();
        store.save(&state).unwrap();
        assert_eq!(
            store.load(&HashSet::from(["session-11".into()])).unwrap(),
            Some(state)
        );
    }
    #[test]
    fn corrupt_layout_reports_path_and_cannot_be_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        for bytes in [
            "{broken",
            r#"{"version":99,"center":{"panel_name":"","children":[],"info":{"panel":null}}}"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            let mut store = LayoutStore::new(path.clone());
            let error = store.load(&HashSet::new()).unwrap_err();
            assert!(format!("{error:#}").contains(path.to_str().unwrap()));
            assert!(store.save(&fixture()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
    }
    #[test]
    fn reject_unknown_or_duplicate_identity() {
        let mut state = fixture();
        assert!(validate(&state, false).is_ok());
        let leaf = state.bottom_dock.as_ref().unwrap().panel().children[0].clone();
        state.center.children = vec![leaf.clone(), leaf];
        state.center.info = PanelInfo::tabs(0);
        assert!(validate(&state, false).is_err());
    }
    #[test]
    fn unknown_panes_are_dropped_and_layout_remains_saveable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        let mut state = fixture();
        let known = PanelState {
            panel_name: PANEL_NAME.into(),
            children: vec![],
            info: PanelInfo::panel(serde_json::json!("session-12")),
        };
        let tabs = state.bottom_dock.as_mut().unwrap().panel().clone();
        let mut tabs = tabs;
        tabs.children.push(known);
        tabs.info = PanelInfo::tabs(1);
        state.bottom_dock = Some(DockState::new(
            tabs,
            DockPlacement::Bottom,
            gpui::px(280.),
            true,
        ));
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let mut store = LayoutStore::new(path.clone());
        let mut loaded = store
            .load(&HashSet::from(["session-12".into()]))
            .unwrap()
            .unwrap();
        let bottom = loaded.bottom_dock.as_mut().unwrap().panel();
        assert_eq!(bottom.children.len(), 1);
        assert_eq!(
            bottom.children[0].info,
            PanelInfo::panel(serde_json::json!("session-12"))
        );
        store.save(&loaded).unwrap();
        assert!(serde_json::from_slice::<DockAreaState>(&std::fs::read(path).unwrap()).is_ok());
    }
    #[test]
    fn workspace_layout_keys_are_stable_and_folder_specific() {
        let first = Path::new("/work/one");
        let second = Path::new("/work/two");
        assert_eq!(workspace_layout_key(first), workspace_layout_key(first));
        assert_ne!(workspace_layout_key(first), workspace_layout_key(second));
    }
    #[test]
    fn sample_and_local_layouts_and_local_folders_use_separate_files() {
        let config = tempfile::tempdir().unwrap();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let sample_dir = config.path().join("sample");
        let first_key = workspace_layout_key(first_dir.path());
        let second_key = workspace_layout_key(second_dir.path());
        let first_path = workspace_layout_path(config.path(), &first_key);
        let second_path = workspace_layout_path(config.path(), &second_key);
        let sample_path = workspace_layout_path(&sample_dir, "1");

        std::fs::write(config.path().join("layout-1.json"), "legacy").unwrap();
        let first = fixture();
        let mut second = fixture();
        second.bottom_dock = None;
        let known = HashSet::from(["session-11".into()]);
        for (path, state) in [
            (&first_path, &first),
            (&second_path, &second),
            (&sample_path, &first),
        ] {
            let store = LayoutStore::new(path.clone());
            store.save(state).unwrap();
        }

        assert_ne!(first_path, second_path);
        assert_ne!(first_path, sample_path);
        assert_ne!(second_path, sample_path);
        assert_eq!(
            std::fs::read_to_string(config.path().join("layout-1.json")).unwrap(),
            "legacy"
        );

        let mut first_store = LayoutStore::new(first_path);
        let mut second_store = LayoutStore::new(second_path);
        let mut sample_store = LayoutStore::new(sample_path);
        assert!(
            first_store
                .load(&known)
                .unwrap()
                .unwrap()
                .bottom_dock
                .is_some()
        );
        assert!(
            second_store
                .load(&known)
                .unwrap()
                .unwrap()
                .bottom_dock
                .is_none()
        );
        assert!(
            sample_store
                .load(&known)
                .unwrap()
                .unwrap()
                .bottom_dock
                .is_some()
        );
        // Removing the first folder does not change the second folder's key.
        assert_eq!(
            workspace_layout_path(config.path(), &second_key),
            workspace_layout_path(config.path(), &workspace_layout_key(second_dir.path()))
        );
    }
    #[test]
    fn config_is_gpui_specific_and_rejects_v6_and_relative_paths() {
        // Use the concrete macOS namespace; /var is itself a symlink alias.
        let home = tempfile::tempdir_in("/private/tmp").unwrap();
        assert_eq!(
            config_dir(None, home.path()).unwrap(),
            home.path()
                .canonicalize()
                .unwrap()
                .join(".config/gitterm-gpui")
        );
        assert!(config_dir(Some("relative".into()), home.path()).is_err());
        assert!(
            config_dir(
                Some(
                    home.path()
                        .join(".config/gitterm-v6/child")
                        .into_os_string()
                ),
                home.path()
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            let v6 = home.path().join(".config/gitterm-v6");
            std::fs::create_dir_all(&v6).unwrap();
            let alias = home.path().join("alias");
            std::os::unix::fs::symlink(v6, &alias).unwrap();
            assert!(config_dir(Some(alias.join("child").into_os_string()), home.path()).is_err());
        }
    }
}
