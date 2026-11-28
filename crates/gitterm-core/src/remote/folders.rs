//! Remote folder ownership and naming from Iced's folder picker handlers.
use crate::{
    remote::config::WorkspaceLocationConfig,
    workspace_config::{WorkspaceConfig, WorkspaceTabConfig, WorkspaceTarget, WorkspacesFile},
};
use std::path::Path;

/// Open Folder prefers the active containing workspace, then the deepest root.
pub fn folder_workspace<'a>(
    locations: impl IntoIterator<Item = (usize, Option<&'a WorkspaceLocationConfig>)>,
    active: usize,
    remote_id: &str,
    folder: &Path,
) -> Option<usize> {
    let candidates: Vec<_> = locations
        .into_iter()
        .filter_map(|(index, location)| match location {
            Some(WorkspaceLocationConfig::RemoteAgent {
                remote_id: id,
                root,
                ..
            }) if id == remote_id && folder.starts_with(root) => {
                Some((index, Path::new(root).components().count()))
            }
            _ => None,
        })
        .collect();
    candidates
        .iter()
        .find(|(index, _)| *index == active)
        .or_else(|| candidates.iter().max_by_key(|(_, depth)| depth))
        .map(|(index, _)| *index)
}

pub fn workspace_name(remote_name: &str, root: &str) -> String {
    let name = root
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("remote");
    format!("{remote_name}:{name}")
}

impl WorkspacesFile {
    /// Iced's create_remote_workspace_at / open_remote_folder_tab saved form.
    /// Browser tabs have no startup command; this never starts a remote process.
    pub fn open_remote_folder(
        &mut self,
        remote_id: &str,
        remote_name: &str,
        dir: &str,
        create: bool,
        active: usize,
        running: &[WorkspaceTarget],
    ) -> Result<WorkspaceConfig, String> {
        let folder = Path::new(dir);
        if !folder.is_absolute() {
            return Err("Remote folder must be absolute".into());
        }
        let existing = (!create)
            .then(|| {
                folder_workspace(
                    self.workspaces
                        .iter()
                        .enumerate()
                        .filter(|(_, w)| w.active || running.iter().any(|target| target.matches(w)))
                        .map(|(i, w)| (i, w.location.as_ref())),
                    active,
                    remote_id,
                    folder,
                )
            })
            .flatten();
        let closed = if create || existing.is_some() {
            None
        } else {
            self.workspaces.iter().position(|w| !w.active && matches!(&w.location, Some(WorkspaceLocationConfig::RemoteAgent { remote_id: id, root, .. }) if id == remote_id && folder.starts_with(root)))
        };
        let index = if let Some(index) = existing {
            index
        } else {
            let workspace = if let Some(index) = closed {
                let mut w = self.workspaces.remove(index);
                w.active = true;
                w.parked = None;
                w.done_at = None;
                w.tabs.clear();
                w.bottom_terminals.clear();
                w.home = Some(w.dir.clone());
                w.repos = vec![w.dir.clone()];
                w.repos_added.clear();
                w.repos_removed.clear();
                w
            } else {
                use crate::settings::WorkspaceColor as C;
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
                let name = if create {
                    workspace_name(remote_name, dir)
                } else {
                    folder
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| dir.into())
                };
                WorkspaceConfig {
                    extra: Default::default(),
                    legacy_env_base: None,
                    collapsed_issue_groups: crate::issues::IssueGroup::default_collapsed(),
                    abbrev: name.chars().take(2).collect::<String>().to_uppercase(),
                    name,
                    dir: dir.into(),
                    color,
                    location: Some(WorkspaceLocationConfig::RemoteAgent {
                        remote_id: remote_id.into(),
                        workspace_id: dir.into(),
                        root: dir.into(),
                    }),
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
                    repos: vec![dir.into()],
                    home: Some(dir.into()),
                    env_overrides: Default::default(),
                    repos_added: vec![],
                    repos_removed: vec![],
                }
            };
            let at = self
                .workspaces
                .iter()
                .position(|w| !w.active)
                .unwrap_or(self.workspaces.len());
            self.workspaces.insert(at, workspace);
            at
        };
        let workspace = &mut self.workspaces[index];
        workspace.active = true;
        workspace.parked = None;
        workspace.done_at = None;
        let root = match &workspace.location {
            Some(WorkspaceLocationConfig::RemoteAgent { root, .. }) => root.clone(),
            _ => unreachable!(),
        };
        workspace.tabs.push(WorkspaceTabConfig {
            extra: Default::default(),
            dir: dir.into(),
            repo_dir: Some(root),
            startup_command: None,
            tab_kind: None,
            agent_config: None,
            chat_session_id: None,
            task_id: None,
            task_session_id: None,
            worker_shutdown_refused: false,
            session_uid: Some(uuid::Uuid::new_v4().to_string()),
        });
        workspace.active_tab = Some(workspace.tabs.len() - 1);
        let target = WorkspaceTarget::of(workspace);
        let ranks: std::collections::HashMap<_, _> = self
            .remote_agents
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect();
        self.workspaces.sort_by_key(|w| {
            (
                !w.active,
                if !w.active {
                    0
                } else {
                    match &w.location {
                        Some(WorkspaceLocationConfig::RemoteAgent { remote_id, .. }) => {
                            1 + ranks.get(remote_id).copied().unwrap_or(usize::MAX / 2)
                        }
                        _ if w.remote_session.is_some() => usize::MAX / 2 + 2,
                        _ => 0,
                    }
                },
            )
        });
        let index = target.index(self)?;
        self.active_workspace = self.workspaces[..index].iter().filter(|w| w.active).count();
        self.normalize_repos();
        for workspace in self.workspaces.iter_mut().filter(|w| w.active) {
            if let Some(repo) = self
                .repos
                .iter()
                .find(|r| Some(r.key()) == workspace.repo_key(workspace.home_dir()))
            {
                workspace.run_command = repo.run_command.clone();
                workspace.env =
                    crate::workspace_config::layered_env(&repo.env, &workspace.env_overrides);
            }
        }
        Ok(self.workspaces[index].clone())
    }
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::*;
    use crate::desktop_store::DesktopStore;
    use serde_json::json;

    #[test]
    fn parity_live_remote_folder_saved_json_matches_iced_browser() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("workspaces.json"),
            br#"{"active_workspace":0,"future":true,"workspaces":[]}"#,
        )
        .unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        let workspace = store
            .transact_workspaces(|file| {
                file.open_remote_folder(
                    "placeholder",
                    "Example",
                    "/srv/project",
                    true,
                    usize::MAX,
                    &[],
                )
            })
            .wait()
            .unwrap();
        // Iced workspace_config(new_remote_agent_tab), with its fresh UUID.
        let expected = json!({"name":"Example:project", "abbrev":"EX", "dir":"/srv/project", "color":"lavender", "collapsed_issue_groups": crate::issues::IssueGroup::default_collapsed(),
            "location":{"kind":"remote_agent","remote_id":"placeholder","workspace_id":"/srv/project","root":"/srv/project"},
            "active_tab":0, "tabs":[{"dir":"/srv/project","repo_dir":"/srv/project","session_uid":workspace.tabs[0].session_uid}], "run_command":null,"bottom_terminals":[],"active":true,
            "repos":["/srv/project"],"home":"/srv/project"});
        assert_eq!(serde_json::to_value(&workspace).unwrap(), expected);
        let second = store
            .transact_workspaces(|file| {
                file.open_remote_folder("placeholder", "Example", "/srv/project/sub", false, 0, &[])
            })
            .wait()
            .unwrap();
        assert_eq!(second.tabs.len(), 2);
        assert_eq!(second.tabs[1].dir, "/srv/project/sub");
        assert_eq!(second.tabs[1].repo_dir.as_deref(), Some("/srv/project"));
        assert_eq!(second.active_tab, Some(1));
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("workspaces.json")).unwrap())
                .unwrap();
        assert_eq!(disk["future"], true);
        assert_eq!(disk["workspaces"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn parity_remote_folder_machine_identity_and_running_park_inventory() {
        let mut file = WorkspacesFile::default();
        let first = file
            .open_remote_folder("first", "First", "/srv/project", false, usize::MAX, &[])
            .unwrap();
        let second = file
            .open_remote_folder("second", "Second", "/srv/project", false, usize::MAX, &[])
            .unwrap();
        assert_ne!(first.location, second.location);
        assert_eq!(
            file.workspaces
                .iter()
                .filter(|w| w.active)
                .nth(file.active_workspace)
                .unwrap()
                .location,
            second.location
        );
        let index = WorkspaceTarget::of(&second).index(&file).unwrap();
        file.workspaces[index].active = false;
        file.workspaces[index].bottom_terminals.push(
            crate::workspace_config::BottomTerminalConfig {
                dir: "/srv/project".into(),
            },
        );
        let running = WorkspaceTarget::of(&file.workspaces[index]);
        let reopened = file
            .open_remote_folder(
                "second",
                "Second",
                "/srv/project/sub",
                false,
                usize::MAX,
                &[running],
            )
            .unwrap();
        assert_eq!(reopened.location, second.location);
        assert_eq!(reopened.tabs.len(), 2);
        assert_eq!(reopened.bottom_terminals.len(), 1);
        assert!(reopened.active && reopened.parked.is_none());
    }

    #[test]
    fn parity_remote_folder_keeps_closed_order_and_reopens_only_home() {
        let mut file = WorkspacesFile::default();
        file.open_remote_folder("placeholder", "Example", "/srv/closed", true, 0, &[])
            .unwrap();
        file.open_remote_folder("placeholder", "Example", "/srv/other", true, 0, &[])
            .unwrap();
        for w in &mut file.workspaces {
            w.active = false;
        }
        file.workspaces[1].location = None;
        let closed: Vec<_> = file.workspaces.iter().map(|w| w.name.clone()).collect();
        file.open_remote_folder("placeholder", "Example", "/srv/new", true, 0, &[])
            .unwrap();
        assert_eq!(
            file.workspaces
                .iter()
                .filter(|w| !w.active)
                .map(|w| w.name.clone())
                .collect::<Vec<_>>(),
            closed
        );
        let w = file
            .workspaces
            .iter_mut()
            .find(|w| w.dir == "/srv/closed")
            .unwrap();
        w.repos.push("/srv/extra".into());
        w.repos_added.push("/srv/extra".into());
        let reopened = file
            .open_remote_folder(
                "placeholder",
                "Example",
                "/srv/closed/sub",
                false,
                usize::MAX,
                &[],
            )
            .unwrap();
        assert_eq!(reopened.repos, ["/srv/closed"]);
        assert!(reopened.repos_added.is_empty());
    }

    #[test]
    fn parity_remote_folder_prefers_active_then_deepest_and_reopens_closed() {
        let mut file: WorkspacesFile =
            serde_json::from_value(json!({"active_workspace":0,"workspaces":[]})).unwrap();
        file.open_remote_folder("placeholder", "Example", "/srv", true, 0, &[])
            .unwrap();
        file.open_remote_folder("placeholder", "Example", "/srv/project", true, 0, &[])
            .unwrap();
        let chosen = file
            .open_remote_folder("placeholder", "Example", "/srv/project/sub", false, 0, &[])
            .unwrap();
        assert_eq!(chosen.dir, "/srv");
        let chosen = file
            .open_remote_folder(
                "placeholder",
                "Example",
                "/srv/project/sub",
                false,
                usize::MAX,
                &[],
            )
            .unwrap();
        assert_eq!(chosen.dir, "/srv/project");
        for w in &mut file.workspaces {
            w.active = false;
            w.extra.insert("future".into(), json!(17));
        }
        let chosen = file
            .open_remote_folder(
                "placeholder",
                "Example",
                "/srv/project/sub",
                false,
                usize::MAX,
                &[],
            )
            .unwrap();
        assert_eq!(chosen.dir, "/srv"); // first closed containing workspace, as Iced.
        assert_eq!(chosen.tabs.len(), 1);
        assert_eq!(chosen.tabs[0].dir, "/srv/project/sub");
        assert_eq!(chosen.extra["future"], 17);
    }
}
