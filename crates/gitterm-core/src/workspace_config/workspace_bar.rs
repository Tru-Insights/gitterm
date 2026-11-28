//! Iced's bottom-bar mutations, applied to the desktop owner's candidate.
use super::*;

impl WorkspacesFile {
    /// `ToggleReposZone`: the saved set is serialized in sorted order by Iced.
    pub fn toggle_repos_zone(&mut self, key: &str) {
        if self.collapsed_repo_groups.iter().any(|group| group == key) {
            self.collapsed_repo_groups.retain(|group| group != key);
        } else {
            self.collapsed_repo_groups.push(key.into());
        }
        self.collapsed_repo_groups.sort();
        self.collapsed_repo_groups.dedup();
    }

    /// `RemoteWorkspaceOpen` / `open_remote_workspace`: select an existing
    /// session, reuse a closed entry's settings, or create its empty home.
    /// Opening the home does not itself attach or start a shell.
    pub fn open_remote_workspace(
        &mut self,
        session_name: &str,
        local_home: &Path,
    ) -> Result<WorkspaceConfig, String> {
        let definition = self
            .remote_sessions
            .iter()
            .find(|s| s.session_name == session_name);
        let name = definition.map(|s| format!("{}:{}", s.host_name, s.label));
        let matches = |w: &WorkspaceConfig| {
            w.remote_session
                .as_ref()
                .is_some_and(|s| s.session_name == session_name)
                || (!w.active && name.as_ref() == Some(&w.name))
        };
        let indices: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| matches(w))
            .map(|(i, _)| i)
            .collect();
        if indices.len() > 1 {
            return Err(format!(
                "Remote session {session_name} has several saved workspaces"
            ));
        }
        if let Some(&index) = indices.first() {
            if self.workspaces[index].active {
                self.active_workspace =
                    self.workspaces[..index].iter().filter(|w| w.active).count();
                return Ok(self.workspaces[index].clone());
            }
        }
        let saved = indices.first().map(|&i| self.workspaces[i].clone());
        let session = saved
            .as_ref()
            .and_then(|w| w.remote_session.clone())
            .or_else(|| definition.cloned())
            .ok_or_else(|| format!("Remote session {session_name} is no longer configured"))?;
        let mut workspace = if let Some(saved) = saved {
            saved
        } else {
            use WorkspaceColor as C;
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
            let used: Vec<_> = self
                .workspaces
                .iter()
                .filter(|w| w.active)
                .map(|w| w.color)
                .collect();
            let color = colors
                .iter()
                .copied()
                .find(|c| !used.contains(c))
                .unwrap_or(colors[used.len() % colors.len()]);
            let name = format!("{}:{}", session.host_name, session.label);
            let dir = local_home.to_string_lossy().into_owned();
            WorkspaceConfig {
                extra: Default::default(),
                legacy_env_base: None,
                collapsed_issue_groups: crate::issues::IssueGroup::default_collapsed(),
                abbrev: name.chars().take(2).collect::<String>().to_uppercase(),
                name,
                dir,
                color,
                location: None,
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
                decisions_repo: None,
                repos: vec![],
                home: None,
                env_overrides: Default::default(),
                repos_added: vec![],
                repos_removed: vec![],
            }
        };
        workspace.remote_session = Some(session);
        workspace.active = true;
        workspace.parked = None;
        workspace.done_at = None;
        // Iced reopens the home, retaining settings but not stopped terminals.
        workspace.tabs.clear();
        workspace.active_tab = Some(0);
        workspace.bottom_terminals.clear();
        workspace.home = Some(workspace.dir.clone());
        workspace.repos = vec![workspace.dir.clone()];
        if let Some(&index) = indices.first() {
            self.workspaces.remove(index);
        }
        self.active_workspace = self.workspaces.iter().filter(|w| w.active).count();
        let at = self
            .workspaces
            .iter()
            .position(|w| !w.active)
            .unwrap_or(self.workspaces.len());
        self.workspaces.insert(at, workspace.clone());
        Ok(workspace)
    }
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::*;
    use crate::desktop_store::DesktopStore;

    fn file() -> WorkspacesFile {
        serde_json::from_value(serde_json::json!({
            "workspaces":[{"name":"Work","abbrev":"W","dir":"/tmp/work","color":"blue","tabs":[]}],
            "active_workspace":0,
            "remote_sessions":[{"label":"app","host_name":"box","ssh_target":"dev@box.example","identity_file":"/tmp/key","session_name":"box-app","remote_dir":"/srv/app"}],
            "future":{"keep":true}
        })).unwrap()
    }

    #[test]
    fn workspace_bar_store_saves_visibility_and_legacy_home_without_touching_other_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workspaces.json");
        let before = file();
        std::fs::write(&path, serde_json::to_vec(&before).unwrap()).unwrap();
        std::fs::write(root.path().join("settings.json"), b"{ \"future\": 2 }\n").unwrap();
        let owner = DesktopStore::open(root.path()).unwrap();
        let saved = owner
            .try_edit_workspaces(|file| {
                file.toggle_repos_zone("local");
                Ok(())
            })
            .wait()
            .unwrap();
        assert_eq!(saved.collapsed_repo_groups, ["local"]);
        let remote = owner
            .transact_workspaces(|file| {
                file.open_remote_workspace("box-app", Path::new("/tmp/home"))
            })
            .wait()
            .unwrap();
        assert_eq!(remote.name, "box:app");
        assert_eq!(remote.dir, "/tmp/home");
        assert!(remote.tabs.is_empty() && remote.location.is_none());
        let saved: WorkspacesFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.workspaces.len(), 2);
        assert_eq!(saved.active_workspace, 1);
        assert_eq!(saved.extra, before.extra);
        assert_eq!(
            serde_json::to_value(&saved.workspaces[0]).unwrap(),
            serde_json::to_value(&before.workspaces[0]).unwrap()
        );
        assert_eq!(
            std::fs::read(root.path().join("settings.json")).unwrap(),
            b"{ \"future\": 2 }\n"
        );
        let bytes = std::fs::read(&path).unwrap();
        owner
            .transact_workspaces(|file| {
                file.open_remote_workspace("box-app", Path::new("/tmp/other-home"))
            })
            .wait()
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn workspace_bar_missing_or_ambiguous_remote_refuses_without_mutation() {
        let mut file = file();
        let before = serde_json::to_value(&file).unwrap();
        assert!(file
            .open_remote_workspace("missing", Path::new("/tmp/home"))
            .is_err());
        assert_eq!(serde_json::to_value(&file).unwrap(), before);
        let remote = file
            .open_remote_workspace("box-app", Path::new("/tmp/home"))
            .unwrap();
        file.workspaces.push(remote);
        let before = serde_json::to_value(&file).unwrap();
        assert!(file
            .open_remote_workspace("box-app", Path::new("/tmp/home"))
            .unwrap_err()
            .contains("several"));
        assert_eq!(serde_json::to_value(&file).unwrap(), before);
    }
}
