//! Workspace settings shared by Iced's modal and GPUI's sheet.
use crate::{
    settings::WorkspaceColor,
    workspace_config::{layered_env, WorkspaceTarget, WorkspacesFile},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvEdit {
    Set { key: String, value: Option<String> },
    Profile(HashMap<String, String>),
}

impl EnvEdit {
    /// Iced edits an existing override first; otherwise the home repo's env.
    pub fn apply(
        self,
        overrides: &mut HashMap<String, String>,
        base: Option<&mut HashMap<String, String>>,
    ) {
        match self {
            Self::Set { key, value } => {
                let env = if overrides.contains_key(&key) {
                    Some(overrides)
                } else {
                    base
                };
                if let Some(env) = env {
                    match value {
                        Some(value) => {
                            env.insert(key, value);
                        }
                        None => {
                            env.remove(&key);
                        }
                    }
                }
            }
            Self::Profile(profile) => {
                overrides.clear();
                if let Some(base) = base {
                    *base = profile;
                }
            }
        }
    }
}

/// Iced trims both inputs and ignores an empty key. It does not validate names.
pub fn env_add(key: &str, value: &str) -> Option<EnvEdit> {
    let key = key.trim().to_string();
    (!key.is_empty()).then(|| EnvEdit::Set {
        key,
        value: Some(value.trim().to_string()),
    })
}

pub fn load_profiles(config_dir: &Path) -> Option<HashMap<String, HashMap<String, String>>> {
    #[derive(serde::Deserialize)]
    struct Profile {
        #[allow(dead_code)]
        description: Option<String>,
        env: HashMap<String, String>,
    }
    let contents = std::fs::read_to_string(config_dir.join("profiles.json")).ok()?;
    let profiles: HashMap<String, Profile> = serde_json::from_str(&contents).ok()?;
    Some(
        profiles
            .into_iter()
            .map(|(name, p)| (name, p.env))
            .collect(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Env(EnvEdit),
    Profile(String),
    GuidelinesEnabled(bool),
    GuidelinesRepo { repo: PathBuf, enabled: bool },
    GuidelinesWorkspace(bool),
    Chrome(bool),
    Decisions(Option<String>),
    Color(WorkspaceColor),
}

impl<C> WorkspacesFile<C> {
    pub fn edit_workspace_env(
        &mut self,
        target: &WorkspaceTarget,
        edit: EnvEdit,
    ) -> Result<(), String> {
        let index = target.index(self)?;
        self.normalize_repos();
        let ws = &mut self.workspaces[index];
        let key = ws.repo_key(ws.home_dir());
        if ws.is_legacy_remote() {
            let base = ws.legacy_env_base.get_or_insert_with(|| ws.env.clone());
            edit.apply(&mut ws.env_overrides, Some(base));
            ws.env = layered_env(base, &ws.env_overrides);
        } else {
            let base = self
                .repos
                .iter_mut()
                .find(|repo| Some(repo.key()) == key)
                .map(|repo| &mut repo.env);
            edit.apply(&mut ws.env_overrides, base);
            // Iced's workspace_config writes the effective home env for older builds.
            for ws in self
                .workspaces
                .iter_mut()
                .filter(|ws| ws.active && ws.repo_key(ws.home_dir()) == key)
            {
                if let Some(repo) = self.repos.iter().find(|repo| Some(repo.key()) == key) {
                    ws.env = layered_env(&repo.env, &ws.env_overrides);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_settings_env_matches_iced_override_and_profile_rules() {
        let mut base = HashMap::from([("KEY".into(), "base".into())]);
        let mut overrides = HashMap::from([("KEY".into(), "override".into())]);
        env_add(" KEY ", " edited ")
            .unwrap()
            .apply(&mut overrides, Some(&mut base));
        assert_eq!(overrides["KEY"], "edited");
        assert_eq!(base["KEY"], "base");
        EnvEdit::Set {
            key: "KEY".into(),
            value: None,
        }
        .apply(&mut overrides, Some(&mut base));
        assert!(overrides.is_empty());
        assert_eq!(layered_env(&base, &overrides)["KEY"], "base");
        EnvEdit::Profile(HashMap::new()).apply(&mut overrides, Some(&mut base));
        assert!(base.is_empty());
        assert!(env_add("  ", "ignored").is_none());
        // Existing Iced accepts names outside shell identifier syntax.
        assert!(env_add("NOT-A-NAME", "value").is_some());
    }
}

#[cfg(test)]
mod document_tests {
    use super::*;
    #[test]
    fn workspace_settings_env_updates_shared_home_copies_but_preserves_closed_work() {
        let mut file: WorkspacesFile = serde_json::from_value(serde_json::json!({
            "active_workspace":0,
            "repos":[{"dir":"/tmp/repo","env":{"KEY":"base"},"future":true}],
            "workspaces":[
                {"name":"Work","abbrev":"W","color":"blue","dir":"/tmp/repo","tabs":[],"env_overrides":{"KEY":"override"}},
                {"name":"Shared","abbrev":"S","color":"green","dir":"/tmp/repo","tabs":[],"env_overrides":{"OTHER":"keep"}},
                {"name":"Closed","abbrev":"C","color":"red","dir":"/tmp/repo","tabs":[],"active":false,"env":{"KEY":"closed"}}
            ]
        })).unwrap();
        let target = WorkspaceTarget::of(&file.workspaces[0]);
        file.edit_workspace_env(
            &target,
            EnvEdit::Set {
                key: "KEY".into(),
                value: None,
            },
        )
        .unwrap();
        assert_eq!(file.workspaces[0].env["KEY"], "base");
        assert!(file.workspaces[0].env_overrides.is_empty());
        file.edit_workspace_env(&target, env_add("KEY", "new").unwrap())
            .unwrap();
        assert_eq!(file.repos[0].env["KEY"], "new");
        assert_eq!(file.workspaces[0].env["KEY"], "new");
        assert_eq!(file.workspaces[1].env["KEY"], "new");
        assert_eq!(file.workspaces[1].env["OTHER"], "keep");
        assert_eq!(file.workspaces[2].env["KEY"], "closed");
        assert_eq!(file.repos[0].extra["future"], true);
        file.edit_workspace_env(&target, EnvEdit::Profile(HashMap::new()))
            .unwrap();
        assert!(file.repos[0].env.is_empty());
        assert!(file.workspaces[0].env.is_empty());
        assert_eq!(
            file.workspaces[1].env,
            HashMap::from([("OTHER".into(), "keep".into())])
        );
    }
}

#[cfg(all(test, feature = "runtime"))]
mod legacy_tests {
    use super::*;
    use crate::desktop_store::DesktopStore;

    #[test]
    fn workspace_settings_legacy_remote_keeps_base_across_acknowledged_saves() {
        let root = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "active_workspace":0,
            "workspaces":[{"name":"Remote fixture","abbrev":"RF","color":"blue",
                "dir":root.path().join("stand-in"),"tabs":[],
                "remote_session":{"label":"fixture","host_name":"fixture.invalid",
                    "ssh_target":"fixture.invalid","identity_file":"",
                    "session_name":"fixture","remote_dir":"/fixture"},
                "env":{"KEY":"old"},"env_overrides":{"KEY":"old"}}]
        });
        std::fs::write(root.path().join("workspaces.json"), input.to_string()).unwrap();
        let store = DesktopStore::open(root.path()).unwrap();
        let mut expected: WorkspacesFile = serde_json::from_value(input).unwrap();
        let target = WorkspaceTarget::of(&expected.workspaces[0]);
        expected.normalize_repos();
        // Iced set_workspace_env edits runtime overrides/home repo independently;
        // workspace_config emits their layered env, never feeding it back as base.
        let mut iced_home_env = expected.workspaces[0].env.clone();
        for edit in [
            env_add("KEY", "new").unwrap(),
            EnvEdit::Set {
                key: "KEY".into(),
                value: None,
            },
            EnvEdit::Profile(HashMap::from([("KEY".into(), "profile".into())])),
            env_add("KEY", "base edit").unwrap(),
            EnvEdit::Set {
                key: "KEY".into(),
                value: None,
            },
        ] {
            edit.clone().apply(
                &mut expected.workspaces[0].env_overrides,
                Some(&mut iced_home_env),
            );
            expected.workspaces[0].env =
                layered_env(&iced_home_env, &expected.workspaces[0].env_overrides);
            let acknowledged = store
                .edit_workspace_settings(target.clone(), Edit::Env(edit))
                .wait()
                .unwrap();
            assert_eq!(acknowledged.workspaces[0].env, expected.workspaces[0].env);
            assert_eq!(
                std::fs::read(root.path().join("workspaces.json")).unwrap(),
                serde_json::to_vec_pretty(&expected).unwrap()
            );
        }
    }
}
