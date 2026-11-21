//! The agent configuration the Agent sidebar lists: skills, plugins, MCP
//! servers, hooks and settings, read from the agent's home config directory
//! and a workspace. Reading only; nothing here writes.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    User,
    Project,
}

impl ConfigScope {
    /// The Agent sidebar's scope badge.
    pub fn badge(self) -> &'static str {
        match self {
            ConfigScope::User => "USR",
            ConfigScope::Project => "PRJ",
        }
    }
}

/// One skill, plugin, server, hook or setting, and the file that defines it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfigItem {
    pub name: String,
    pub file_path: PathBuf,
    pub scope: ConfigScope,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    pub skills: Vec<AgentConfigItem>,
    pub plugins: Vec<AgentConfigItem>,
    pub mcp_servers: Vec<AgentConfigItem>,
    pub hooks: Vec<AgentConfigItem>,
    pub settings: Vec<AgentConfigItem>,
}

/// The sidebar's sections in order: (label, key).
pub const SECTIONS: [(&str, &str); 5] = [
    ("Skills", "skills"),
    ("Plugins", "plugins"),
    ("MCP Servers", "mcp_servers"),
    ("Hooks", "hooks"),
    ("Settings", "settings"),
];

impl AgentConfig {
    /// The items of the section `key` names (see [`SECTIONS`]).
    pub fn section(&self, key: &str) -> &[AgentConfigItem] {
        match key {
            "skills" => &self.skills,
            "plugins" => &self.plugins,
            "mcp_servers" => &self.mcp_servers,
            "hooks" => &self.hooks,
            "settings" => &self.settings,
            _ => &[],
        }
    }

    /// Read `agent_home` (the user's `~/.claude`) and `workspace_dir`.
    /// Missing or malformed files contribute nothing.
    pub fn load(agent_home: &Path, workspace_dir: &Path) -> Self {
        let mut config = Self::default();

        // --- Skills ---
        // Project skills first (they override user skills with same name)
        let mut skill_names: HashSet<String> = HashSet::new();
        for (dir, scope) in [
            (
                workspace_dir.join(".claude").join("commands"),
                ConfigScope::Project,
            ),
            (agent_home.join("commands"), ConfigScope::User),
        ] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if scope == ConfigScope::User && skill_names.contains(&name) {
                        continue;
                    }
                    skill_names.insert(name.clone());
                    config.skills.push(AgentConfigItem {
                        name,
                        file_path: path,
                        scope,
                    });
                }
            }
        }

        // --- Read settings.json ---
        let settings_path = agent_home.join("settings.json");
        let settings_json = read_json(&settings_path);
        let keys = |value: Option<&serde_json::Value>| -> Vec<String> {
            value
                .and_then(|v| v.as_object())
                .map(|object| object.keys().cloned().collect())
                .unwrap_or_default()
        };
        let items = |names: Vec<String>, file: &Path, scope: ConfigScope| {
            let file = file.to_path_buf();
            names
                .into_iter()
                .map(|name| AgentConfigItem {
                    name,
                    file_path: file.clone(),
                    scope,
                })
                .collect::<Vec<_>>()
        };

        // --- Plugins ---
        let settings = settings_json.as_ref();
        config.plugins.extend(items(
            keys(settings.and_then(|s| s.get("enabledPlugins"))),
            &settings_path,
            ConfigScope::User,
        ));

        // --- MCP Servers ---
        // Project .mcp.json, then the global one in the agent home.
        for (path, scope) in [
            (workspace_dir.join(".mcp.json"), ConfigScope::Project),
            (agent_home.join(".mcp.json"), ConfigScope::User),
        ] {
            let json = read_json(&path);
            config.mcp_servers.extend(items(
                keys(json.as_ref().and_then(|j| j.get("mcpServers"))),
                &path,
                scope,
            ));
        }

        // --- Hooks ---
        config.hooks.extend(items(
            keys(settings.and_then(|s| s.get("hooks"))),
            &settings_path,
            ConfigScope::User,
        ));

        // --- Settings ---
        // User settings (top-level keys, excluding plugins/hooks which have their own sections)
        let excluded = ["enabledPlugins", "hooks"];
        config.settings.extend(items(
            keys(settings)
                .into_iter()
                .filter(|key| !excluded.contains(&key.as_str()))
                .collect(),
            &settings_path,
            ConfigScope::User,
        ));
        // Project settings
        let project_settings_path = workspace_dir.join(".claude").join("settings.local.json");
        let project_settings = read_json(&project_settings_path);
        config.settings.extend(items(
            keys(project_settings.as_ref()),
            &project_settings_path,
            ConfigScope::Project,
        ));

        for section in [
            &mut config.skills,
            &mut config.plugins,
            &mut config.mcp_servers,
            &mut config.hooks,
            &mut config.settings,
        ] {
            section.sort_by_key(|a| a.name.to_lowercase());
        }
        config
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_each_section_with_project_overrides_and_sorted_names() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let (home, work) = (home.path(), work.path());
        std::fs::create_dir_all(home.join("commands")).unwrap();
        std::fs::create_dir_all(work.join(".claude/commands")).unwrap();
        std::fs::write(home.join("commands/Zeta.md"), "z").unwrap();
        std::fs::write(home.join("commands/shared.md"), "user").unwrap();
        std::fs::write(home.join("commands/notes.txt"), "not a skill").unwrap();
        std::fs::write(work.join(".claude/commands/shared.md"), "project").unwrap();
        std::fs::write(work.join(".claude/commands/alpha.md"), "a").unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"model":"x","enabledPlugins":{"p1":true},"hooks":{"Stop":[]},"Theme":"dark"}"#,
        )
        .unwrap();
        std::fs::write(home.join(".mcp.json"), r#"{"mcpServers":{"global":{}}}"#).unwrap();
        std::fs::write(work.join(".mcp.json"), r#"{"mcpServers":{"local":{}}}"#).unwrap();
        std::fs::write(
            work.join(".claude/settings.local.json"),
            r#"{"permissions":{}}"#,
        )
        .unwrap();
        std::fs::write(work.join("broken.json"), "{").unwrap();

        let config = AgentConfig::load(home, work);
        let names = |items: &[AgentConfigItem]| -> Vec<String> {
            items.iter().map(|item| item.name.clone()).collect()
        };
        assert_eq!(names(&config.skills), ["alpha", "shared", "Zeta"]);
        let shared = &config.skills[1];
        assert_eq!(shared.scope, ConfigScope::Project);
        assert_eq!(shared.file_path, work.join(".claude/commands/shared.md"));
        assert_eq!(names(&config.plugins), ["p1"]);
        assert_eq!(names(&config.mcp_servers), ["global", "local"]);
        assert_eq!(config.mcp_servers[1].file_path, work.join(".mcp.json"));
        assert_eq!(names(&config.hooks), ["Stop"]);
        assert_eq!(names(&config.settings), ["model", "permissions", "Theme"]);
        assert_eq!(config.settings[1].scope, ConfigScope::Project);
        for (_, key) in SECTIONS {
            assert!(!config.section(key).is_empty(), "{key}");
        }
        assert!(config.section("other").is_empty());
    }

    #[test]
    fn missing_or_malformed_files_list_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("settings.json"), "not json").unwrap();
        assert_eq!(
            AgentConfig::load(home.path(), &home.path().join("missing")),
            AgentConfig::default()
        );
    }
}
