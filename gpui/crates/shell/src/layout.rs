use crate::pane::PANEL_NAME;
use anyhow::{Context as _, Result, bail, ensure};
use gpui_component::dock::{DockAreaState, DockPlacement, PanelInfo, PanelState};
use std::{
    collections::HashSet,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
};

pub fn config_dir(override_dir: Option<OsString>, home: &Path) -> Result<PathBuf> {
    let dir = override_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config/gitterm-gpui"));
    ensure!(
        dir.is_absolute(),
        "GITTERM_GPUI_CONFIG_DIR must be absolute: {}",
        dir.display()
    );
    // Resolve existing ancestors too: a not-yet-created directory may live below a symlink.
    fn resolved(path: &Path) -> Result<PathBuf> {
        if path.exists() {
            return path
                .canonicalize()
                .with_context(|| format!("resolve {}", path.display()));
        }
        let parent = path.parent().context("config path has no parent")?;
        Ok(resolved(parent)?.join(path.file_name().context("config path has no name")?))
    }
    let actual = resolved(&dir)?;
    for name in ["gitterm-v3", "gitterm-v4", "gitterm-v5", "gitterm-v6"] {
        let legacy = resolved(&home.join(".config").join(name))?;
        ensure!(
            !actual.starts_with(&legacy),
            "GPUI config must be isolated from {}: {}",
            name,
            dir.display()
        );
    }
    Ok(actual)
}

pub struct LayoutStore {
    pub path: PathBuf,
    refused: Option<String>,
}
impl LayoutStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            refused: None,
        }
    }
    pub fn load(&mut self, known: &HashSet<String>) -> Result<Option<DockAreaState>> {
        let result = (|| -> Result<Option<DockAreaState>> {
            let bytes = match std::fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let state: DockAreaState = serde_json::from_slice(&bytes)?;
            validate(&state, known)?;
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
    pub fn save(&self, state: &DockAreaState) -> Result<()> {
        if let Some(reason) = &self.refused {
            bail!("refusing to overwrite {}: {reason}", self.path.display());
        }
        (|| -> Result<()> {
            let dir = self
                .path
                .parent()
                .context("layout has no parent directory")?;
            std::fs::create_dir_all(dir)?;
            let mut temp = tempfile::NamedTempFile::new_in(dir)?;
            temp.write_all(&serde_json::to_vec_pretty(state)?)?;
            temp.as_file().sync_all()?;
            temp.persist(&self.path)?;
            Ok(())
        })()
        .with_context(|| format!("save dock layout {}", self.path.display()))
    }
}

fn validate(state: &DockAreaState, known: &HashSet<String>) -> Result<()> {
    ensure!(
        state.version == Some(1),
        "unsupported dock layout version {:?}",
        state.version
    );
    fn visit(
        node: &PanelState,
        known: &HashSet<String>,
        seen: &mut HashSet<String>,
        depth: usize,
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
                ensure!(known.contains(&id), "unknown pane ID {id}");
                ensure!(seen.insert(id.clone()), "duplicate pane ID {id}");
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
            visit(child, known, seen, depth + 1)?;
        }
        Ok(())
    }
    let mut seen = HashSet::new();
    visit(&state.center, known, &mut seen, 0)?;
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
            visit(dock.panel(), known, &mut seen, 0)?;
        }
    }
    Ok(())
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
        assert!(validate(&state, &HashSet::new()).is_err());
        let leaf = state.bottom_dock.as_ref().unwrap().panel().children[0].clone();
        state.center.children = vec![leaf.clone(), leaf];
        state.center.info = PanelInfo::tabs(0);
        assert!(validate(&state, &HashSet::from(["session-11".into()])).is_err());
    }
    #[test]
    fn config_is_gpui_specific_and_rejects_v6_and_relative_paths() {
        let home = tempfile::tempdir().unwrap();
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
