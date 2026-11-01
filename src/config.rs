use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const APP_NAME: &str = "GitTerm V6";
pub const WINDOW_TITLE: &str = "GitTerm V6 Development";
pub const CONFIG_DIR_NAME: &str = "gitterm-v6";
pub const CONFIG_DIR_ENV: &str = "GITTERM_V6_CONFIG_DIR";
pub const INSTANCE_ID_ENV: &str = "GITTERM_V6_INSTANCE_ID";

/// The V5 runtime's config dir and its override variable. V6 only reads
/// them, once, to import V5 state on first launch (see `config_import`).
pub const V5_CONFIG_DIR_NAME: &str = "gitterm-v5";
pub const V5_CONFIG_DIR_ENV: &str = "GITTERM_V5_CONFIG_DIR";

// Global instance ID for this process
static INSTANCE_ID: OnceLock<String> = OnceLock::new();

// Config directory override, read once from GITTERM_V6_CONFIG_DIR
static CONFIG_DIR_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Get or generate the unique instance ID for this GitTerm process
pub fn instance_id() -> &'static str {
    INSTANCE_ID.get_or_init(|| {
        // Allow override via environment variable for testing
        std::env::var(INSTANCE_ID_ENV).unwrap_or_else(|_| std::process::id().to_string())
    })
}

/// The config directory override from GITTERM_V6_CONFIG_DIR, if set.
/// Dev/test instances set this so they can never read or write the
/// real ~/.config/gitterm-v6/* state of a running V6 instance.
pub fn config_dir_override() -> Option<&'static PathBuf> {
    CONFIG_DIR_OVERRIDE
        .get_or_init(|| resolve_config_dir_override(std::env::var_os(CONFIG_DIR_ENV)))
        .as_ref()
}

fn resolve_config_dir_override(raw: Option<std::ffi::OsString>) -> Option<PathBuf> {
    resolve_dir_override(CONFIG_DIR_ENV, raw)
}

/// Validate a config-dir override read from `env_name`: absolute and
/// non-empty, otherwise ignored with a log line.
pub fn resolve_dir_override(env_name: &str, raw: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let raw = raw?;
    if raw.is_empty() {
        eprintln!("{env_name} is set but empty; ignoring override");
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_relative() {
        eprintln!(
            "{env_name} is relative ({}); ignoring override",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// Get the shared (non-instance-specific) config directory
pub fn global_config_dir() -> PathBuf {
    if let Some(dir) = config_dir_override() {
        return dir.clone();
    }
    default_config_dir(&dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".config").join(CONFIG_DIR_NAME)
}

/// Get the base config directory for this instance
pub fn instance_config_dir() -> PathBuf {
    global_config_dir().join(format!("instance-{}", instance_id()))
}

/// The gh account each GitHub owner's repos run as (TRU-173), in the shared
/// config dir, not the per-instance `config.json`, so it survives restarts:
///
/// ```json
/// { "acme": "acme-dev", "acme-labs": "dev-personal" }
/// ```
///
/// The PR watch reads and the overview's Approve and Merge run with
/// `GH_TOKEN="$(gh auth token --user <user>)"` for that command only. An
/// owner with no entry uses gh's active account.
pub const GITHUB_ACCOUNTS_FILE: &str = "github-accounts.json";

pub fn load_github_accounts() -> BTreeMap<String, String> {
    load_github_accounts_from(&global_config_dir())
}

fn load_github_accounts_from(dir: &Path) -> BTreeMap<String, String> {
    let path = dir.join(GITHUB_ACCOUNTS_FILE);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            eprintln!("[config] cannot read {}: {error}", path.display());
            return BTreeMap::new();
        }
    };
    serde_json::from_str(&contents).unwrap_or_else(|error| {
        eprintln!("[config] ignoring {}: {error}", path.display());
        BTreeMap::new()
    })
}

/// Print instance info on startup
pub fn print_instance_info() {
    eprintln!("{APP_NAME} instance: {}", instance_id());
    eprintln!("Config directory: {}", instance_config_dir().display());
    eprintln!("Settings: {}", Config::settings_path().display());
    if let Some(dir) = config_dir_override() {
        eprintln!("Config dir override ({CONFIG_DIR_ENV}): {}", dir.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_accounts_file_parses_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_github_accounts_from(dir.path()).is_empty());
        std::fs::write(
            dir.path().join(GITHUB_ACCOUNTS_FILE),
            r#"{"acme":"acme-dev","acme-labs":"dev-personal"}"#,
        )
        .unwrap();
        let accounts = load_github_accounts_from(dir.path());
        assert_eq!(accounts.get("acme").map(String::as_str), Some("acme-dev"));
        assert_eq!(accounts.len(), 2);
        std::fs::write(dir.path().join(GITHUB_ACCOUNTS_FILE), "not json").unwrap();
        assert!(load_github_accounts_from(dir.path()).is_empty());
    }

    #[test]
    fn decisions_repo_is_additive_and_round_trips() {
        // An entry from before TRU-191 loads with none and writes no key.
        let old = r#"{
            "name": "example-work",
            "abbrev": "EW",
            "dir": "/tmp/work/example-work",
            "color": "blue",
            "tabs": [],
            "chats": ["s1"]
        }"#;
        let cfg: WorkspaceConfig = serde_json::from_str(old).unwrap();
        assert_eq!(cfg.decisions_repo, None);
        assert!(serde_json::to_value(&cfg)
            .unwrap()
            .get("decisions_repo")
            .is_none());
        let mut set = cfg.clone();
        set.decisions_repo = Some("/tmp/code/team-decisions".to_string());
        let back: WorkspaceConfig =
            serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
        assert_eq!(
            back.decisions_repo.as_deref(),
            Some("/tmp/code/team-decisions")
        );
        // A whole workspaces.json from before it loads too.
        let file: WorkspacesFile = serde_json::from_str(&format!(
            r#"{{"workspaces": [{old}], "active_workspace": 0}}"#
        ))
        .unwrap();
        assert_eq!(file.workspaces[0].decisions_repo, None);
    }

    #[test]
    fn workspace_config_loads_old_entries_and_round_trips_parking() {
        // A workspace closed before parking existed: `active:false`, no
        // `parked` or `done_at`. It loads unchanged and lists as parked.
        let old = r#"{
            "name": "acme-portal",
            "abbrev": "AP",
            "dir": "/tmp/acme/portal",
            "color": "blue",
            "tabs": [],
            "active": false
        }"#;
        let cfg: WorkspaceConfig = serde_json::from_str(old).unwrap();
        assert!(!cfg.active);
        assert_eq!(cfg.parked, None);
        assert_eq!(cfg.done_at, None);
        assert!(cfg.is_parked());
        // Writing it back adds no new keys.
        let written = serde_json::to_value(&cfg).unwrap();
        assert!(written.get("parked").is_none());
        assert!(written.get("done_at").is_none());
        // Guidelines are on unless the workspace switched them off (TRU-174).
        assert!(!cfg.guidelines_off);
        assert!(written.get("guidelines_off").is_none());
        let off: WorkspaceConfig = serde_json::from_str(&old.replace(
            r#""active": false"#,
            r#""active": false, "guidelines_off": true"#,
        ))
        .unwrap();
        assert!(off.guidelines_off);
        assert_eq!(
            serde_json::to_value(&off).unwrap()["guidelines_off"],
            serde_json::json!(true)
        );
        // Claude in Chrome is off unless the workspace switched it on
        // (TRU-181); off writes nothing.
        assert!(!cfg.claude_in_chrome);
        assert!(written.get("claude_in_chrome").is_none());
        let chrome: WorkspaceConfig = serde_json::from_str(&old.replace(
            r#""active": false"#,
            r#""active": false, "claude_in_chrome": true"#,
        ))
        .unwrap();
        assert!(chrome.claude_in_chrome);
        let json = serde_json::to_string(&chrome).unwrap();
        let back: WorkspaceConfig = serde_json::from_str(&json).unwrap();
        assert!(back.claude_in_chrome);
        assert_eq!(
            serde_json::to_value(&back).unwrap()["claude_in_chrome"],
            serde_json::json!(true)
        );

        // An open workspace is never on the Parked list.
        let open: WorkspaceConfig =
            serde_json::from_str(&old.replace(r#""active": false"#, r#""active": true"#)).unwrap();
        assert!(!open.is_parked());

        // Parked with a note, then done: both fields survive a round trip.
        let mut parked = cfg.clone();
        parked.parked = Some(ParkedInfo {
            note: "Waiting on PRJ-101 review".to_string(),
            parked_at: "2026-10-30T09:00:00+00:00".to_string(),
        });
        let json = serde_json::to_string(&parked).unwrap();
        let back: WorkspaceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.parked, parked.parked);
        assert!(back.is_parked());
        parked.done_at = Some("2026-10-31T09:00:00+00:00".to_string());
        let back: WorkspaceConfig =
            serde_json::from_str(&serde_json::to_string(&parked).unwrap()).unwrap();
        assert_eq!(back.done_at.as_deref(), Some("2026-10-31T09:00:00+00:00"));
        assert!(!back.is_parked());

        // A note-less parked entry (hand-edited) still loads.
        let bare: ParkedInfo =
            serde_json::from_str(r#"{"parked_at":"2026-10-30T09:00:00+00:00"}"#).unwrap();
        assert_eq!(bare.note, "");
    }

    /// Writes `instance-<id>/config.json` under `dir` with `contents`,
    /// modified `age_secs` seconds ago.
    fn write_instance_config(dir: &Path, id: &str, contents: &str, age_secs: u64) {
        let instance = dir.join(format!("instance-{id}"));
        std::fs::create_dir_all(&instance).unwrap();
        let path = instance.join(INSTANCE_CONFIG_FILE);
        std::fs::write(&path, contents).unwrap();
        let modified = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn settings_round_trip_through_the_shared_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            theme: "light".into(),
            terminal_font_size: 16.0,
            ..Config::default()
        };
        config.chat.default_model = "sonnet".into();
        config.chat.default_effort = Some("high".into());
        config.chat.remember_model("haiku");
        config.chat.gemini_model = "gemini-test".into();
        config.notifications.needs_you = false;
        config.review.default_reviewer = ReviewerKind::Codex;
        save_settings_to(dir.path(), &config).unwrap();

        let path = dir.path().join(SETTINGS_FILE);
        assert!(path.is_file());
        // No temporary file is left beside it.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [SETTINGS_FILE]);

        let loaded = load_settings_from(dir.path());
        assert_eq!(loaded.refused, None);
        let back = loaded.config;
        assert_eq!(back.theme, "light");
        assert_eq!(back.terminal_font_size, 16.0);
        assert_eq!(back.chat, config.chat);
        assert_eq!(back.notifications, config.notifications);
        assert_eq!(back.review, config.review);
        assert_eq!(back.policy, config.policy);
        assert_eq!(back.usage, config.usage);
        // The default worktree root is not pinned in the shared file.
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.get("task_worktree_root").is_none(), "{raw}");
        assert_eq!(back.task_worktree_root, default_task_worktree_root());

        // A hand-written file with only some settings loads with the rest
        // defaulted.
        std::fs::write(&path, r#"{"chat":{"default_model":"sonnet"}}"#).unwrap();
        let partial = load_settings_from(dir.path());
        assert_eq!(partial.refused, None);
        assert_eq!(partial.config.chat.default_model, "sonnet");
        assert_eq!(partial.config.theme, "dark");
        assert!(partial.config.notifications.needs_you);
    }

    #[test]
    fn carry_over_picks_the_newest_instance_with_settings_and_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        // The newest file has no settings (an instance that wrote nothing
        // of them), the next is malformed; the older one with settings wins
        // over the oldest.
        write_instance_config(dir.path(), "400", r#"{"theme":"light"}"#, 10);
        write_instance_config(dir.path(), "300", "not json", 20);
        write_instance_config(
            dir.path(),
            "200",
            r#"{"theme":"light","chat":{"default_model":"sonnet"},"notifications":{"needs_you":false}}"#,
            30,
        );
        write_instance_config(
            dir.path(),
            "100",
            r#"{"theme":"dark","chat":{"default_model":"haiku"}}"#,
            40,
        );
        // Not an instance dir: ignored.
        std::fs::create_dir_all(dir.path().join("worktrees")).unwrap();
        std::fs::write(
            dir.path().join("worktrees").join(INSTANCE_CONFIG_FILE),
            r#"{"theme":"dark","chat":{"default_model":"opus"}}"#,
        )
        .unwrap();

        let first = load_settings_from(dir.path());
        assert_eq!(first.refused, None);
        assert_eq!(first.config.chat.default_model, "sonnet");
        assert_eq!(first.config.theme, "light");
        assert!(!first.config.notifications.needs_you);
        // The shared file now holds them.
        let path = dir.path().join(SETTINGS_FILE);
        let written: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written.chat.default_model, "sonnet");

        // A newer instance config appearing later does not run it again:
        // the shared file wins.
        write_instance_config(
            dir.path(),
            "500",
            r#"{"theme":"dark","chat":{"default_model":"haiku"}}"#,
            0,
        );
        let second = load_settings_from(dir.path());
        assert_eq!(second.config.chat.default_model, "sonnet");
        assert_eq!(second.config.theme, "light");
    }

    #[test]
    fn carry_over_without_earlier_settings_writes_the_defaults_once() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_settings_from(dir.path());
        assert_eq!(loaded.refused, None);
        assert_eq!(loaded.config.chat, ChatDefaults::default());
        assert!(dir.path().join(SETTINGS_FILE).is_file());
        // An instance config found afterwards is not carried over.
        write_instance_config(
            dir.path(),
            "100",
            r#"{"theme":"light","chat":{"default_model":"haiku"}}"#,
            0,
        );
        let again = load_settings_from(dir.path());
        assert_eq!(again.config.chat, ChatDefaults::default());
        assert_eq!(again.config.theme, "dark");
    }

    #[test]
    fn a_malformed_shared_file_is_refused_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        let malformed = r#"{"chat": {"default_model": "sonnet",}"#;
        std::fs::write(&path, malformed).unwrap();
        // An instance config with settings is not carried over in its place.
        write_instance_config(
            dir.path(),
            "100",
            r#"{"theme":"light","chat":{"default_model":"haiku"}}"#,
            0,
        );

        let loaded = load_settings_from(dir.path());
        let reason = loaded.refused.expect("a malformed file is refused");
        assert!(reason.contains(SETTINGS_FILE), "{reason}");
        assert!(reason.contains("malformed"), "{reason}");
        // The run uses the defaults, not the instance config.
        assert_eq!(loaded.config.chat, ChatDefaults::default());
        assert_eq!(loaded.config.theme, "dark");
        // The file is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);

        // A well-formed file of the wrong shape is refused too.
        std::fs::write(&path, r#"{"chat": "sonnet"}"#).unwrap();
        assert!(load_settings_from(dir.path()).refused.is_some());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"chat": "sonnet"}"#
        );
    }

    #[test]
    fn test_instance_id_generation() {
        let id = instance_id();
        assert!(!id.is_empty());
        // Should be consistent across calls
        assert_eq!(instance_id(), id);
    }

    #[test]
    fn test_instance_config_dir() {
        let dir = instance_config_dir();
        assert!(dir.to_string_lossy().contains("instance-"));
        assert!(dir.to_string_lossy().contains(instance_id()));
    }

    #[test]
    fn default_config_dir_is_isolated_from_earlier_versions() {
        let home = PathBuf::from("/test-home");
        let path = default_config_dir(&home);
        assert_eq!(path, PathBuf::from("/test-home/.config/gitterm-v6"));
        assert_ne!(path, PathBuf::from("/test-home/.config/gitterm-v5"));
        assert_ne!(path, PathBuf::from("/test-home/.config/gitterm-v4"));
        assert_ne!(path, PathBuf::from("/test-home/.config/gitterm"));
    }

    #[test]
    fn runtime_identity_environment_is_v6_only() {
        assert_eq!(APP_NAME, "GitTerm V6");
        assert_eq!(WINDOW_TITLE, "GitTerm V6 Development");
        assert_eq!(CONFIG_DIR_ENV, "GITTERM_V6_CONFIG_DIR");
        assert_eq!(INSTANCE_ID_ENV, "GITTERM_V6_INSTANCE_ID");
        assert_ne!(CONFIG_DIR_ENV, V5_CONFIG_DIR_ENV);
        assert_ne!(INSTANCE_ID_ENV, "GITTERM_V5_INSTANCE_ID");
    }

    #[test]
    fn task_worktree_root_defaults_beneath_the_v6_config_root() {
        let root = PathBuf::from("/test-home/.config/gitterm-v6");
        assert_eq!(
            task_worktree_root_for_config_root(&root),
            PathBuf::from("/test-home/.config/gitterm-v6/worktrees")
        );

        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("task_worktree_root");
        let existing_config: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            existing_config.task_worktree_root,
            default_task_worktree_root()
        );
    }

    #[test]
    fn workspace_tab_task_link_is_optional_and_round_trips() {
        let existing: WorkspaceTabConfig =
            serde_json::from_value(serde_json::json!({ "dir": "/repo" })).unwrap();
        assert_eq!(existing.task_id, None);

        let linked: WorkspaceTabConfig = serde_json::from_value(serde_json::json!({
            "dir": "/worktrees/task-1",
            "task_id": "task-1"
        }))
        .unwrap();
        assert_eq!(linked.task_id.as_deref(), Some("task-1"));
        assert_eq!(linked.task_session_id, None);
        assert_eq!(
            serde_json::to_value(linked).unwrap()["task_id"],
            serde_json::json!("task-1")
        );

        let session: WorkspaceTabConfig = serde_json::from_value(serde_json::json!({
            "dir": "/worktrees/task-1",
            "task_id": "task-1",
            "task_session_id": "session-1"
        }))
        .unwrap();
        assert_eq!(session.task_session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn workspace_tab_session_uid_is_optional_and_round_trips() {
        let existing: WorkspaceTabConfig =
            serde_json::from_value(serde_json::json!({ "dir": "/repo" })).unwrap();
        assert_eq!(existing.session_uid, None);
        assert!(serde_json::to_value(&existing)
            .unwrap()
            .get("session_uid")
            .is_none());

        let tab: WorkspaceTabConfig = serde_json::from_value(serde_json::json!({
            "dir": "/repo",
            "session_uid": "7d1c2a4e-0b3f-4e8a-9c55-2f1e6d7a8b90"
        }))
        .unwrap();
        let encoded = serde_json::to_string(&tab).unwrap();
        let decoded: WorkspaceTabConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded.session_uid.as_deref(),
            Some("7d1c2a4e-0b3f-4e8a-9c55-2f1e6d7a8b90")
        );
    }

    /// A workspaces file as written before TRU-169: no `chats` anywhere.
    const OLD_WORKSPACES_JSON: &str = r#"{
      "workspaces": [
        {
          "name": "api-server",
          "abbrev": "AP",
          "dir": "/w/api-server",
          "color": "blue",
          "tabs": [{ "dir": "/w/api-server" }],
          "run_command": null,
          "bottom_terminals": [],
          "env": { "LINEAR_TEAM": "PRJ" }
        },
        {
          "name": "acme",
          "abbrev": "AC",
          "dir": "/w/acme",
          "color": "green",
          "tabs": [],
          "active": false
        }
      ],
      "active_workspace": 0
    }"#;

    #[test]
    fn workspace_owned_chats_are_additive_and_round_trip() {
        // An old file loads unchanged: no owned chats, other fields intact.
        let old: WorkspacesFile = serde_json::from_str(OLD_WORKSPACES_JSON).unwrap();
        assert_eq!(old.workspaces.len(), 2);
        assert!(old.workspaces.iter().all(|ws| ws.chats.is_empty()));
        assert!(old.workspaces.iter().all(|ws| !ws.claude_in_chrome));
        assert_eq!(old.workspaces[0].env["LINEAR_TEAM"], "PRJ");
        assert!(!old.workspaces[1].active);
        // Without chats nothing new is written.
        let encoded = serde_json::to_value(&old).unwrap();
        for ws in encoded["workspaces"].as_array().unwrap() {
            assert!(ws.get("chats").is_none(), "{ws}");
        }

        // With chats, through a file on disk and back.
        let mut owning = old.clone();
        owning.workspaces[1].name = "Cloud migration".into();
        owning.workspaces[1].chats = vec!["root-a".into(), "root-b".into()];
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workspaces.json");
        std::fs::write(&path, serde_json::to_string_pretty(&owning).unwrap()).unwrap();
        let read: WorkspacesFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.workspaces[1].name, "Cloud migration");
        assert_eq!(read.workspaces[1].chats, ["root-a", "root-b"]);
        assert!(read.workspaces[0].chats.is_empty());
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw["workspaces"][0].get("chats").is_none());
        assert_eq!(raw["workspaces"][1]["chats"][1], "root-b");
    }

    #[test]
    fn an_old_file_loads_as_one_repo_per_workspace_and_round_trips() {
        let mut file: WorkspacesFile = serde_json::from_str(OLD_WORKSPACES_JSON).unwrap();
        assert!(file.repos.is_empty());
        file.normalize_repos();

        // Each workspace lists its dir as home and only repo; each home
        // gets a repo entry from the workspace's own fields.
        for ws in &file.workspaces {
            assert_eq!(ws.home.as_deref(), Some(ws.dir.as_str()));
            assert_eq!(ws.repos, [ws.dir.clone()]);
            assert!(ws.env_overrides.is_empty());
        }
        assert_eq!(file.repos.len(), 2);
        let api = file
            .repo(&RepoKey::of_config("/w/api-server", None))
            .unwrap();
        assert_eq!(api.env["LINEAR_TEAM"], "PRJ");
        assert_eq!(api.run_command, None);
        // The env a tab gets is unchanged.
        assert_eq!(file.workspace_env("api-server", None)["LINEAR_TEAM"], "PRJ");
        assert!(file.workspace_env("acme", None).is_empty());
        assert!(file.workspace_env("unknown", None).is_empty());

        // Written and read back: the repo model survives, the old fields
        // are still there for an older build, and normalizing again
        // changes nothing.
        let json = serde_json::to_string_pretty(&file).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(raw["repos"][0]["dir"], "/w/api-server");
        assert_eq!(raw["workspaces"][0]["env"]["LINEAR_TEAM"], "PRJ");
        assert_eq!(raw["workspaces"][0]["home"], "/w/api-server");
        let mut read: WorkspacesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(read.repos, file.repos);
        read.normalize_repos();
        assert_eq!(read.repos, file.repos);
        assert_eq!(read.workspaces[0].repos, file.workspaces[0].repos);
        assert!(read.stale_env_copies().is_empty());

        // An older build reads the file, dropping what it doesn't know.
        let older: serde_json::Value = {
            let mut value = raw.clone();
            value.as_object_mut().unwrap().remove("repos");
            for ws in value["workspaces"].as_array_mut().unwrap() {
                let ws = ws.as_object_mut().unwrap();
                ws.remove("repos");
                ws.remove("home");
                ws.remove("env_overrides");
            }
            value
        };
        let mut again: WorkspacesFile = serde_json::from_value(older).unwrap();
        again.normalize_repos();
        assert_eq!(again.repos, file.repos);
    }

    #[test]
    fn workspaces_sharing_a_dir_in_an_old_file_share_one_repo() {
        let json = r#"{
          "workspaces": [
            { "name": "parked", "abbrev": "PA", "dir": "/w/shared", "color": "red",
              "tabs": [], "active": false, "run_command": "make parked",
              "env": { "TEAM": "PARKED" },
              "bottom_terminals": [{ "dir": "/w/shared/parked" }] },
            { "name": "first", "abbrev": "FI", "dir": "/w/shared", "color": "blue",
              "tabs": [], "run_command": "npm run dev",
              "env": { "TEAM": "PRJ", "ONLY_FIRST": "1" },
              "bottom_terminals": [{ "dir": "/w/shared" }] },
            { "name": "second", "abbrev": "SE", "dir": "/w/shared", "color": "green",
              "tabs": [], "run_command": "cargo run",
              "env": { "TEAM": "OPS" },
              "bottom_terminals": [{ "dir": "/w/shared/sub" }] }
          ],
          "active_workspace": 0
        }"#;
        let mut file: WorkspacesFile = serde_json::from_str(json).unwrap();
        file.normalize_repos();
        // One repo: the first open workspace wins its settings. Each
        // workspace keeps its own bottom terminals.
        assert_eq!(file.repos.len(), 1);
        let repo = &file.repos[0];
        assert_eq!(repo.run_command.as_deref(), Some("npm run dev"));
        assert!(repo.bottom_terminals.is_empty());
        let dirs = |idx: usize| -> Vec<&str> {
            file.workspaces[idx]
                .bottom_terminals
                .iter()
                .map(|bt| bt.dir.as_str())
                .collect()
        };
        assert_eq!(dirs(0), ["/w/shared/parked"]);
        assert_eq!(dirs(1), ["/w/shared"]);
        assert_eq!(dirs(2), ["/w/shared/sub"]);
        // The second's differing env became its overrides; each tab env
        // keeps what its workspace had (plus keys the repo adds).
        assert_eq!(file.workspaces[2].env_overrides["TEAM"], "OPS");
        assert_eq!(file.workspace_env("first", None)["TEAM"], "PRJ");
        assert_eq!(file.workspace_env("second", None)["TEAM"], "OPS");
        assert_eq!(file.workspace_env("parked", None)["TEAM"], "PARKED");
        assert_eq!(file.workspace_env("second", None)["ONLY_FIRST"], "1");
    }

    #[test]
    fn terminals_kept_on_repos_go_to_every_workspace_listing_them() {
        // As the build that kept bottom terminals on repos wrote it: each
        // workspace's own `bottom_terminals` is a copy of its home's.
        let json = r#"{
          "workspaces": [
            { "name": "one", "abbrev": "ON", "dir": "/w/shared", "color": "red",
              "tabs": [], "home": "/w/shared", "repos": ["/w/shared", "/w/api"],
              "bottom_terminals": [{ "dir": "/w/shared" }, { "dir": "/w/shared/sub" }] },
            { "name": "two", "abbrev": "TW", "dir": "/w/api", "color": "blue",
              "tabs": [], "home": "/w/api", "repos": ["/w/api", "/w/shared"],
              "bottom_terminals": [{ "dir": "/w/api" }] },
            { "name": "parked", "abbrev": "PA", "dir": "/w/shared", "color": "green",
              "tabs": [], "active": false, "home": "/w/shared", "repos": ["/w/shared"],
              "bottom_terminals": [{ "dir": "/w/shared" }, { "dir": "/w/shared/sub" }] },
            { "name": "solo", "abbrev": "SO", "dir": "/w/solo", "color": "teal",
              "tabs": [], "home": "/w/solo", "repos": ["/w/solo"],
              "bottom_terminals": [] }
          ],
          "repos": [
            { "dir": "/w/shared", "run_command": "npm run dev",
              "bottom_terminals": [{ "dir": "/w/shared" }, { "dir": "/w/shared/sub" }] },
            { "dir": "/w/api", "bottom_terminals": [{ "dir": "/w/api" }] },
            { "dir": "/w/solo", "bottom_terminals": [] }
          ],
          "active_workspace": 0
        }"#;
        let mut file: WorkspacesFile = serde_json::from_str(json).unwrap();
        file.normalize_repos();
        let dirs = |file: &WorkspacesFile, idx: usize| -> Vec<String> {
            file.workspaces[idx]
                .bottom_terminals
                .iter()
                .map(|bt| bt.dir.clone())
                .collect()
        };
        // Every terminal is kept; a repo listed by several workspaces gives
        // its terminals to each, home's first.
        assert_eq!(dirs(&file, 0), ["/w/shared", "/w/shared/sub", "/w/api"]);
        assert_eq!(dirs(&file, 1), ["/w/api", "/w/shared", "/w/shared/sub"]);
        assert_eq!(dirs(&file, 2), ["/w/shared", "/w/shared/sub"]);
        assert!(dirs(&file, 3).is_empty());
        // The repos keep their settings and no longer hold terminals.
        assert!(file
            .repos
            .iter()
            .all(|repo| repo.bottom_terminals.is_empty()));
        assert_eq!(file.repos[0].run_command.as_deref(), Some("npm run dev"));

        // Written back, terminals are only the workspaces'; loading that
        // again changes nothing.
        let json = serde_json::to_string_pretty(&file).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(raw["repos"][0].get("bottom_terminals").is_none());
        assert_eq!(raw["workspaces"][1]["bottom_terminals"][0]["dir"], "/w/api");
        let mut read: WorkspacesFile = serde_json::from_str(&json).unwrap();
        read.normalize_repos();
        for idx in 0..4 {
            assert_eq!(dirs(&read, idx), dirs(&file, idx));
        }
    }

    #[test]
    fn a_tab_gets_the_env_of_the_repo_holding_its_cwd_then_overrides() {
        let json = r#"{
          "workspaces": [
            { "name": "work", "abbrev": "WO", "dir": "/w/work", "color": "blue",
              "tabs": [], "home": "/w/work",
              "repos": ["/w/work", "/w/org-a/api", "/w/org-b/web", "/w/new"],
              "env": { "GH_TOKEN": "fake-home" },
              "env_overrides": { "LINEAR_TEAM": "PRJ" },
              "repos_added": ["/w/new"],
              "repos_removed": ["/w/gone"] }
          ],
          "repos": [
            { "dir": "/w/work", "env": { "GH_TOKEN": "fake-home" } },
            { "dir": "/w/org-a/api", "env": { "GH_TOKEN": "fake-a" } },
            { "dir": "/w/org-b/web", "env": { "GH_TOKEN": "fake-b", "LINEAR_TEAM": "OPS" } }
          ],
          "active_workspace": 0
        }"#;
        let mut file: WorkspacesFile = serde_json::from_str(json).unwrap();
        file.normalize_repos();
        let env = |cwd: &str| file.workspace_env("work", Some(Path::new(cwd)));
        assert_eq!(env("/w/org-a/api/src")["GH_TOKEN"], "fake-a");
        assert_eq!(env("/w/org-b/web")["GH_TOKEN"], "fake-b");
        // Overrides apply on top of every repo.
        assert_eq!(env("/w/org-b/web")["LINEAR_TEAM"], "PRJ");
        // Outside every listed repo: the home repo's env, as before repos.
        assert_eq!(env("/elsewhere")["GH_TOKEN"], "fake-home");
        assert_eq!(env("/w/work/notes")["GH_TOKEN"], "fake-home");
        // A listed repo with nothing stored has only the overrides.
        assert!(!env("/w/new/x").contains_key("GH_TOKEN"));
        assert_eq!(env("/w/new/x")["LINEAR_TEAM"], "PRJ");

        // Manual adds and removes round trip.
        let read: WorkspacesFile =
            serde_json::from_str(&serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(read.workspaces[0].repos_added, ["/w/new"]);
        assert_eq!(read.workspaces[0].repos_removed, ["/w/gone"]);
        assert_eq!(read.workspaces[0].repos[1], "/w/org-a/api");
    }

    #[test]
    fn env_layers_overrides_on_the_repo_env() {
        let env: HashMap<String, String> = [("A", "repo"), ("B", "repo")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let overrides: HashMap<String, String> = [("B", "ws"), ("C", "ws")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let layered = layered_env(&env, &overrides);
        assert_eq!(layered["A"], "repo");
        assert_eq!(layered["B"], "ws");
        assert_eq!(layered["C"], "ws");
        assert_eq!(env_difference(&layered, &env), overrides);
        assert!(env_difference(&env, &env).is_empty());
    }

    #[test]
    fn a_hand_edited_env_copy_is_reported_by_key() {
        let mut file: WorkspacesFile = serde_json::from_str(OLD_WORKSPACES_JSON).unwrap();
        file.normalize_repos();
        file.workspaces[0]
            .env
            .insert("FAKE_TOKEN".into(), "fake-value".into());
        assert_eq!(
            file.stale_env_copies(),
            [("api-server".to_string(), vec!["FAKE_TOKEN".to_string()])]
        );
        // The repo's env is what tabs get.
        assert!(!file
            .workspace_env("api-server", None)
            .contains_key("FAKE_TOKEN"));
    }

    #[test]
    fn remote_and_legacy_workspaces_key_their_repos_by_machine() {
        let remote = RepoKey::of_config(
            "/srv/app",
            Some(&WorkspaceLocationConfig::RemoteAgent {
                remote_id: "box".into(),
                workspace_id: "/srv/app".into(),
                root: "/srv/app".into(),
            }),
        );
        assert_ne!(remote, RepoKey::of_config("/srv/app", None));
        let legacy: WorkspaceConfig = serde_json::from_value(serde_json::json!({
            "name": "box:app", "abbrev": "BO", "dir": "/Users/me", "color": "blue",
            "tabs": [], "env": { "K": "v" },
            "remote_session": {
                "label": "box", "host_name": "box", "ssh_target": "box",
                "identity_file": "", "session_name": "app", "remote_dir": "/srv/app"
            }
        }))
        .unwrap();
        assert!(legacy.is_legacy_remote());
        let mut file = WorkspacesFile {
            workspaces: vec![legacy],
            active_workspace: 0,
            remote_sessions: Vec::new(),
            remote_agents: Vec::new(),
            collapsed_repo_groups: Vec::new(),
            repos: Vec::new(),
        };
        file.normalize_repos();
        // Never stored as a repo; its own env stays its source.
        assert!(file.repos.is_empty());
        assert_eq!(file.workspace_env("box:app", None)["K"], "v");
    }

    #[test]
    fn collapsed_repo_groups_are_additive_and_round_trip() {
        // An old file loads with every Repos zone expanded and writes
        // nothing new.
        let old: WorkspacesFile = serde_json::from_str(OLD_WORKSPACES_JSON).unwrap();
        assert!(old.collapsed_repo_groups.is_empty());
        let encoded = serde_json::to_value(&old).unwrap();
        assert!(encoded.get("collapsed_repo_groups").is_none(), "{encoded}");

        let mut collapsed = old.clone();
        collapsed.collapsed_repo_groups = vec!["local".into(), "agent:box-one".into()];
        let json = serde_json::to_string_pretty(&collapsed).unwrap();
        let read: WorkspacesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(read.collapsed_repo_groups, ["local", "agent:box-one"]);
        assert_eq!(read.workspaces.len(), 2);
        assert_eq!(read.active_workspace, 0);
    }

    #[test]
    fn test_resolve_config_dir_override_unset() {
        assert_eq!(resolve_config_dir_override(None), None);
    }

    #[test]
    fn test_resolve_config_dir_override_empty_ignored() {
        assert_eq!(resolve_config_dir_override(Some("".into())), None);
    }

    #[test]
    fn test_resolve_config_dir_override_relative_ignored() {
        assert_eq!(resolve_config_dir_override(Some("rel/path".into())), None);
    }

    #[test]
    fn test_resolve_config_dir_override_absolute() {
        assert_eq!(
            resolve_config_dir_override(Some("/tmp/gitterm-dev".into())),
            Some(PathBuf::from("/tmp/gitterm-dev"))
        );
    }

    #[test]
    fn review_defaults_to_an_opus_claude_subagent_and_old_configs_load() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            serialized["review"],
            serde_json::json!({
                "default_reviewer": "claude-subagent",
                "subagent_model": "opus",
                "max_concurrent_reviews": 2
            })
        );
        serialized.as_object_mut().unwrap().remove("review");
        let existing: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(existing.review, ReviewConfig::default());

        serialized["review"] = serde_json::json!({"default_reviewer": "codex"});
        let codex: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(codex.review.default_reviewer, ReviewerKind::Codex);
        assert_eq!(codex.review.subagent_model, "opus");
        assert_eq!(codex.review.codex_model, None);
        assert_eq!(codex.review.max_concurrent_reviews, 2);
    }

    #[test]
    fn the_worker_model_policy_is_seeded_and_old_configs_load() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            serialized["policy"],
            serde_json::json!({
                "claude": {"coordinator": "opus", "scoped": "sonnet", "judgment": "opus", "specialist": "fable"},
                "codex": {"coordinator": "gpt-6.1-sol", "scoped": "gpt-6-luna", "judgment": "gpt-6.1-sol", "specialist": "gpt-6-astra"}
            })
        );
        serialized.as_object_mut().unwrap().remove("policy");
        let existing: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(existing.policy, gitterm::workers::ModelPolicy::default());
    }

    #[test]
    fn usage_price_overrides_load_and_survive_a_save() {
        // An older config without `usage` has no overrides.
        let config: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(config.usage, gitterm::usage::UsageConfig::default());
        let config: Config = serde_json::from_str(
            r#"{"theme":"dark","usage":{"pricing":{
                "gpt-6-astra":{"input":1.0,"output":8.0,"cache_read":0.1,"cache_write":0.0},
                "claude-haiku-5-5":null}}}"#,
        )
        .unwrap();
        let astra = config.usage.pricing["gpt-6-astra"].unwrap();
        assert_eq!((astra.input, astra.output), (1.0, 8.0));
        assert_eq!(config.usage.pricing["claude-haiku-5-5"], None);
        let saved: Config = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(saved.usage, config.usage);
    }

    #[test]
    fn needs_you_notifications_default_on_and_can_be_switched_off() {
        // An older config without `notifications` notifies.
        let config: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(config.notifications.needs_you);
        let config: Config =
            serde_json::from_str(r#"{"theme":"dark","notifications":{}}"#).unwrap();
        assert!(config.notifications.needs_you);
        let config: Config =
            serde_json::from_str(r#"{"theme":"dark","notifications":{"needs_you":false}}"#)
                .unwrap();
        assert!(!config.notifications.needs_you);
        assert!(Config::default().notifications.needs_you);
    }

    #[test]
    fn new_chats_use_the_defaults_until_something_is_picked() {
        // An older config without `chat` gets the defaults.
        let config: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(config.chat, ChatDefaults::default());
        let mut chat = ChatDefaults {
            default_effort: Some("high".into()),
            ..ChatDefaults::default()
        };
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "opus".into(),
                effort: Some("high".into())
            }
        );
        // Picking a model keeps the effort a new chat would have had.
        chat.remember_model("sonnet");
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "sonnet".into(),
                effort: Some("high".into())
            }
        );
        chat.remember_effort(None);
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "sonnet".into(),
                effort: None
            }
        );
        // With remembering off, the configured defaults win again.
        chat.remember_last = false;
        assert_eq!(
            chat.new_chat_selection(),
            ChatSelection {
                model: "opus".into(),
                effort: Some("high".into())
            }
        );
        let json = serde_json::to_value(&chat).unwrap();
        assert_eq!(json["last"]["model"], "sonnet");
        let back: ChatDefaults = serde_json::from_value(json).unwrap();
        assert_eq!(back, chat);
    }

    #[test]
    fn acp_chat_models_default_and_parse() {
        let chat: ChatDefaults = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(chat.gemini_model, "gemini-4-argon");
        // The global Claude in Chrome switch of earlier builds is ignored
        // (it is per workspace now) and not written back.
        let earlier: ChatDefaults =
            serde_json::from_value(serde_json::json!({"claude_in_chrome": true})).unwrap();
        assert_eq!(earlier, ChatDefaults::default());
        assert!(serde_json::to_value(&earlier)
            .unwrap()
            .get("claude_in_chrome")
            .is_none());
        assert_eq!(chat.opencode_model, "");
        let chat: ChatDefaults = serde_json::from_value(serde_json::json!({
            "gemini_model": "gemini-3.8-flash",
            "opencode_model": "openrouter/google/gemini-3.6-flash",
        }))
        .unwrap();
        assert_eq!(chat.gemini_model, "gemini-3.8-flash");
        assert_eq!(chat.opencode_model, "openrouter/google/gemini-3.6-flash");
    }

    #[test]
    fn terminal_log_mirroring_is_opt_in() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        let object = serialized.as_object_mut().unwrap();
        object.remove("terminal_log_mirroring_enabled");
        // The retired setting controlled the whole local content server. It
        // must not silently opt existing users into expensive history scans.
        object.insert("log_server_enabled".into(), serde_json::Value::Bool(true));

        let existing_config: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert!(!existing_config.terminal_log_mirroring_enabled);

        serialized["terminal_log_mirroring_enabled"] = serde_json::Value::Bool(true);
        let explicitly_enabled: Config = serde_json::from_value(serialized).unwrap();
        assert!(explicitly_enabled.terminal_log_mirroring_enabled);
    }

    #[cfg(feature = "stt")]
    #[test]
    fn stt_defaults_enabled_for_existing_and_new_configs() {
        let mut serialized = serde_json::to_value(Config::default()).unwrap();
        serialized.as_object_mut().unwrap().remove("stt_enabled");

        let existing_config: Config = serde_json::from_value(serialized.clone()).unwrap();
        assert!(existing_config.stt_enabled);

        serialized["stt_enabled"] = serde_json::Value::Bool(false);
        let explicitly_disabled: Config = serde_json::from_value(serialized).unwrap();
        assert!(!explicitly_disabled.stt_enabled);
    }
}

/// Clean up this instance's config directory on exit
pub fn cleanup_instance_config() {
    let instance_dir = instance_config_dir();
    if instance_dir.exists() && instance_dir.to_string_lossy().contains(instance_id()) {
        let _ = std::fs::remove_dir_all(&instance_dir);
        eprintln!(
            "{APP_NAME} instance {} cleaned up config: {}",
            instance_id(),
            instance_dir.display()
        );
    }
}

// Default functions for serde
fn default_agent_color() -> WorkspaceColor {
    WorkspaceColor::Lavender
}

fn default_agent_presets() -> Vec<AgentPreset> {
    vec![
        AgentPreset {
            name: "Pi".to_string(),
            command: "pi".to_string(),
            resume_command: Some("pi --resume".to_string()),
            icon: "\u{03c0}".to_string(), // π
            color: WorkspaceColor::Pink,
        },
        AgentPreset {
            name: "Claude Code".to_string(),
            command: "claude".to_string(),
            resume_command: Some("claude --resume".to_string()),
            icon: "\u{276f}".to_string(),
            color: WorkspaceColor::Peach,
        },
        AgentPreset {
            name: "Codex".to_string(),
            command: "codex".to_string(),
            resume_command: Some("codex resume".to_string()),
            icon: "\u{2261}".to_string(),
            color: WorkspaceColor::Green,
        },
        AgentPreset {
            name: "Gemini".to_string(),
            command: "gemini".to_string(),
            resume_command: Some("gemini --resume".to_string()),
            icon: "G".to_string(),
            color: WorkspaceColor::Blue,
        },
    ]
}

pub fn default_task_worktree_root() -> PathBuf {
    task_worktree_root_for_config_root(&global_config_dir())
}

pub fn task_worktree_root_for_config_root(config_root: &Path) -> PathBuf {
    config_root.join("worktrees")
}

fn is_default_task_worktree_root(root: &Path) -> bool {
    *root == default_task_worktree_root()
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_terminal_font() -> f32 {
    14.0
}

fn default_ui_font() -> f32 {
    13.0
}

fn default_sidebar_width() -> f32 {
    280.0
}

fn default_scrollback_lines() -> usize {
    100_000
}

fn default_console_height() -> f32 {
    200.0
}

fn default_console_expanded() -> bool {
    true
}

fn default_remote_session_shell() -> String {
    "/bin/zsh".to_string()
}

fn default_remote_session_tmux_path() -> String {
    "/opt/homebrew/bin/tmux".to_string()
}

pub fn default_remote_sessions() -> Vec<RemoteSessionConfig> {
    Vec::new()
}

pub fn default_remote_agents() -> Vec<RemoteAgentConfig> {
    Vec::new()
}

#[cfg(feature = "stt")]
fn default_stt_enabled() -> bool {
    true
}

// Local task slots gate agent launches, not tabs or sessions. The number is
// a guard against a runaway dispatch fan-out rather than a throughput
// budget — the task system's value is worktree organization, so the cap
// should rarely be the thing a user bumps into (TRU-129).
fn default_max_concurrent_local_tasks() -> usize {
    4
}

// Persistent configuration: the shared `settings.json` (TRU-181), see
// `SETTINGS_FILE`. Every field is a setting the user expects to keep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_terminal_font")]
    pub terminal_font_size: f32,
    #[serde(default = "default_ui_font")]
    pub ui_font_size: f32,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: usize,
    // Legacy field for migration
    #[serde(default)]
    pub font_size: Option<f32>,
    /// Defaulted so a hand-written `settings.json` may leave it out.
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default = "default_console_height")]
    pub console_height: f32,
    #[serde(default = "default_console_expanded")]
    pub console_expanded: bool,
    /// Whether full terminal scrollback is mirrored into the local HTTP viewer.
    /// This is intentionally opt-in because collecting long histories is expensive.
    #[serde(default)]
    pub terminal_log_mirroring_enabled: bool,
    #[cfg(feature = "stt")]
    #[serde(default = "default_stt_enabled")]
    pub stt_enabled: bool,
    #[cfg(feature = "stt")]
    #[serde(default)]
    pub stt_model_path: Option<String>,
    #[serde(default = "default_agent_presets")]
    pub agent_presets: Vec<AgentPreset>,
    #[serde(default)]
    pub quick_commands: Vec<QuickCommand>,
    /// Written only when changed from the default: the shared
    /// `settings.json` must not pin one config dir's worktree root, so a
    /// copy of it in another (isolated) config dir keeps its own.
    #[serde(
        default = "default_task_worktree_root",
        skip_serializing_if = "is_default_task_worktree_root"
    )]
    pub task_worktree_root: PathBuf,
    /// How many local tasks may run agent sessions at once. Dispatching past
    /// the limit queues the task; capacity release starts the next in FIFO
    /// order. Zero is treated as 1 — a limit that can never start anything
    /// would strand every dispatch.
    #[serde(default = "default_max_concurrent_local_tasks")]
    pub max_concurrent_local_tasks: usize,
    /// Defaults for the chat tab's Review… button (TRU-142).
    #[serde(default)]
    pub review: ReviewConfig,
    /// Model and effort for new chats, and the last selection (TRU-143).
    #[serde(default)]
    pub chat: ChatDefaults,
    /// Which model each worker role runs on, per provider, for
    /// `delegate_task` (TRU-142 S6, TRU-144). Seeded with the routing table;
    /// see `gitterm::workers::ModelPolicy`.
    #[serde(default)]
    pub policy: gitterm::workers::ModelPolicy,
    /// Usage panel settings: per-model price overrides (TRU-145); see
    /// `gitterm::usage::UsageConfig`.
    #[serde(default)]
    pub usage: gitterm::usage::UsageConfig,
    /// macOS notifications (TRU-148).
    #[serde(default)]
    pub notifications: NotificationsConfig,
}

/// Which macOS notifications GitTerm posts (TRU-148). Edit the
/// `notifications` object in `settings.json`:
///
/// ```json
/// "notifications": { "needs_you": true }
/// ```
///
/// `needs_you`: notify when a tab that is not in front starts waiting on the
/// human (an input or approval request, a blocked worker, a task needing
/// input).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub needs_you: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { needs_you: true }
    }
}

fn default_chat_model() -> String {
    "opus".to_string()
}

/// Gemini's flagship model, the Gemini chat default (TRU-153).
fn default_gemini_model() -> String {
    "gemini-4-argon".to_string()
}

/// What a new chat tab starts with (TRU-143). The app has no settings
/// panel; edit the `chat` object in `settings.json`:
///
/// ```json
/// "chat": { "default_model": "opus", "default_effort": "high", "remember_last": true }
/// ```
///
/// `default_model` is a Claude model alias (`default` leaves the choice to
/// the user's Claude settings); `default_effort` is one of the CLI's effort
/// levels (`low`, `medium`, `high`, `xhigh`, `max`), or absent for the
/// model's own default. With `remember_last`, a new chat starts with the
/// model and effort last picked on any chat (`last`, written by the app)
/// instead of the defaults.
///
/// Claude in Chrome is per workspace (`WorkspaceConfig::claude_in_chrome`,
/// TRU-181); a `claude_in_chrome` key here, from an earlier build, is
/// ignored.
///
/// `gemini_model` and `opencode_model` are what new Gemini and OpenCode
/// chats start on (TRU-153): a model id as the agent lists it, such as
/// `gemini-4-argon` or `openrouter/google/gemini-3.6-flash`. An empty
/// `opencode_model` (the default) keeps the model OpenCode itself picks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatDefaults {
    #[serde(default = "default_chat_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default = "default_true")]
    pub remember_last: bool,
    /// The model and effort last picked on any chat. One global selection,
    /// not per workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<ChatSelection>,
    #[serde(default = "default_gemini_model")]
    pub gemini_model: String,
    #[serde(default)]
    pub opencode_model: String,
}

/// A chat's model and effort (`effort: None` is the model's default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSelection {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Default for ChatDefaults {
    fn default() -> Self {
        Self {
            default_model: default_chat_model(),
            default_effort: None,
            remember_last: true,
            last: None,
            gemini_model: default_gemini_model(),
            opencode_model: String::new(),
        }
    }
}

impl ChatDefaults {
    /// The configured defaults, ignoring any remembered selection.
    fn defaults(&self) -> ChatSelection {
        ChatSelection {
            model: self.default_model.clone(),
            effort: self.default_effort.clone(),
        }
    }

    /// What a new chat starts with: the last selection when remembering is
    /// on and one exists, else the configured defaults.
    pub fn new_chat_selection(&self) -> ChatSelection {
        match &self.last {
            Some(last) if self.remember_last => last.clone(),
            _ => self.defaults(),
        }
    }

    /// Record a model picked on a chat; the effort stays as the next new
    /// chat would have it.
    pub fn remember_model(&mut self, model: &str) {
        let mut selection = self.new_chat_selection();
        selection.model = model.to_string();
        self.last = Some(selection);
    }

    /// Record an effort picked on a chat.
    pub fn remember_effort(&mut self, effort: Option<&str>) {
        let mut selection = self.new_chat_selection();
        selection.effort = effort.map(str::to_string);
        self.last = Some(selection);
    }
}

/// Who reviews code when the chat tab's Review… button is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewerKind {
    /// A subagent spawned by the chat's own Claude through its Agent tool.
    #[default]
    ClaudeSubagent,
    /// An independent `codex exec review` run owned by GitTerm (TRU-142).
    Codex,
}

impl ReviewerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeSubagent => "claude-subagent",
            Self::Codex => "codex",
        }
    }
}

fn default_review_subagent_model() -> String {
    "opus".to_string()
}

/// Concurrent Codex runs (reviews and consults) before new requests queue
/// (TRU-142 decision D7).
pub const DEFAULT_MAX_CONCURRENT_REVIEWS: usize = 2;

fn default_max_concurrent_reviews() -> usize {
    DEFAULT_MAX_CONCURRENT_REVIEWS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewConfig {
    #[serde(default)]
    pub default_reviewer: ReviewerKind,
    /// The Agent tool `model` a Claude-subagent review runs on.
    #[serde(default = "default_review_subagent_model")]
    pub subagent_model: String,
    /// `codex -m <model>` for Codex reviews and consults; `None` keeps
    /// Codex's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_model: Option<String>,
    /// Codex runs (reviews and consults) that may run at once; further
    /// requests wait as `requested` and start when a slot frees. Zero is
    /// treated as 1.
    #[serde(default = "default_max_concurrent_reviews")]
    pub max_concurrent_reviews: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            default_reviewer: ReviewerKind::default(),
            subagent_model: default_review_subagent_model(),
            codex_model: None,
            max_concurrent_reviews: default_max_concurrent_reviews(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommand {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPreset {
    pub name: String,
    pub command: String,
    /// Command to resume the last session (e.g. "claude --resume", "codex resume")
    #[serde(default)]
    pub resume_command: Option<String>,
    #[serde(default)]
    pub icon: String,
    #[serde(default = "default_agent_color")]
    pub color: WorkspaceColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceColor {
    Lavender,
    Blue,
    Green,
    Peach,
    Pink,
    Yellow,
    Red,
    Teal,
}

impl WorkspaceColor {
    pub fn color(&self, theme: &crate::theme::AppTheme) -> iced::Color {
        use iced::color;
        match theme {
            crate::theme::AppTheme::Dark => match self {
                Self::Lavender => color!(0xb4befe),
                Self::Blue => color!(0x89b4fa),
                Self::Green => color!(0xa6e3a1),
                Self::Peach => color!(0xfab387),
                Self::Pink => color!(0xf5c2e7),
                Self::Yellow => color!(0xf9e2af),
                Self::Red => color!(0xf38ba8),
                Self::Teal => color!(0x94e2d5),
            },
            crate::theme::AppTheme::Light => match self {
                Self::Lavender => color!(0x7287fd),
                Self::Blue => color!(0x1e66f5),
                Self::Green => color!(0x40a02b),
                Self::Peach => color!(0xfe640b),
                Self::Pink => color!(0xea76cb),
                Self::Yellow => color!(0xdf8e1d),
                Self::Red => color!(0xd20f39),
                Self::Teal => color!(0x179299),
            },
        }
    }

    pub const ALL: [Self; 8] = [
        Self::Lavender,
        Self::Blue,
        Self::Green,
        Self::Peach,
        Self::Pink,
        Self::Yellow,
        Self::Red,
        Self::Teal,
    ];

    pub fn from_index(idx: usize) -> Self {
        Self::ALL[idx % Self::ALL.len()]
    }

    /// Pick the first color not already used by existing workspaces
    pub fn next_available(used: &[Self]) -> Self {
        Self::ALL
            .iter()
            .find(|c| !used.contains(c))
            .copied()
            .unwrap_or_else(|| Self::from_index(used.len()))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal_font_size: 14.0,
            ui_font_size: 13.0,
            sidebar_width: 280.0,
            scrollback_lines: 30_000,
            font_size: None,
            theme: "dark".to_string(),
            show_hidden: true,
            console_height: 200.0,
            console_expanded: true,
            terminal_log_mirroring_enabled: false,
            #[cfg(feature = "stt")]
            stt_enabled: true,
            #[cfg(feature = "stt")]
            stt_model_path: None,
            agent_presets: default_agent_presets(),
            quick_commands: Vec::new(),
            task_worktree_root: default_task_worktree_root(),
            max_concurrent_local_tasks: default_max_concurrent_local_tasks(),
            review: ReviewConfig::default(),
            chat: ChatDefaults::default(),
            policy: gitterm::workers::ModelPolicy::default(),
            usage: gitterm::usage::UsageConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }
}

/// The user's settings (TRU-181): every `Config` field, in the shared config
/// dir, so they survive restarts and every instance reads the same ones.
/// Before TRU-181 they lived in the per-instance `instance-<pid>/config.json`,
/// which starts empty on every launch; that file is no longer written (it
/// held no per-launch state). Follows the pattern of `github-accounts.json`
/// and `agent-guidelines.json`.
///
/// Carry-over: when this file is missing, it is built once from the newest
/// `instance-*/config.json` holding the settings (`CARRY_OVER_KEYS`), or
/// from the defaults when there is none, and written. This file wins after
/// that. A file that cannot be read or parsed is logged and never
/// overwritten: the run uses the defaults and saves are refused.
pub const SETTINGS_FILE: &str = "settings.json";

/// The per-instance config file settings lived in before TRU-181; read
/// only for the carry-over.
const INSTANCE_CONFIG_FILE: &str = "config.json";

/// The keys that mark an instance `config.json` as one a build with these
/// settings wrote; any one of them is enough.
const CARRY_OVER_KEYS: [&str; 4] = ["chat", "notifications", "usage", "policy"];

fn settings_log(msg: &str) {
    eprintln!("[settings] {msg}");
}

/// The settings a run starts with, and whether it may write them back.
#[derive(Debug)]
pub struct LoadedSettings {
    pub config: Config,
    /// Set when `settings.json` exists but cannot be read or parsed: the
    /// run uses the defaults and must not write the file.
    pub refused: Option<String>,
}

impl Config {
    pub fn settings_path() -> PathBuf {
        global_config_dir().join(SETTINGS_FILE)
    }

    /// `settings.json` from the shared config dir, carried over from the
    /// newest instance config the first time it is missing.
    pub fn load() -> LoadedSettings {
        load_settings_from(&global_config_dir())
    }

    /// Write `settings.json` atomically.
    pub fn save(&self) -> Result<(), String> {
        save_settings_to(&global_config_dir(), self)
    }
}

fn load_settings_from(dir: &Path) -> LoadedSettings {
    let path = dir.join(SETTINGS_FILE);
    let refuse = |reason: String| {
        let reason = format!(
            "{reason}; using the default settings for this run and not writing {}. \
             Fix or remove the file and restart",
            path.display()
        );
        settings_log(&reason);
        LoadedSettings {
            config: Config::default(),
            refused: Some(reason),
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<Config>(&contents) {
            Ok(config) => LoadedSettings {
                config,
                refused: None,
            },
            Err(error) => refuse(format!("{} is malformed: {error}", path.display())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => carry_over_settings(dir),
        Err(error) => refuse(format!("cannot read {}: {error}", path.display())),
    }
}

/// Builds `settings.json` the first time it is missing: from the newest
/// instance config holding the settings, else from the defaults. Writing
/// it makes this a one-time step; a failed write is logged and retried on
/// the next save.
fn carry_over_settings(dir: &Path) -> LoadedSettings {
    let config = match newest_instance_settings(dir) {
        Some((config, source)) => {
            settings_log(&format!(
                "carried the settings over from {}",
                source.display()
            ));
            config
        }
        None => {
            settings_log("no earlier settings found; starting from the defaults");
            Config::default()
        }
    };
    if let Err(error) = save_settings_to(dir, &config) {
        settings_log(&format!("cannot write the carried-over settings: {error}"));
    }
    LoadedSettings {
        config,
        refused: None,
    }
}

/// The settings in the most recently modified `instance-*/config.json`
/// under `dir` that holds any of `CARRY_OVER_KEYS`, with its path. Files
/// that cannot be read or parsed are logged and skipped.
fn newest_instance_settings(dir: &Path) -> Option<(Config, PathBuf)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            settings_log(&format!(
                "cannot list {} for earlier settings: {error}",
                dir.display()
            ));
            return None;
        }
    };
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("instance-"))
        .map(|entry| entry.path().join(INSTANCE_CONFIG_FILE))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    // Newest first; the path breaks ties so the pick is stable.
    candidates.sort_by(|a, b| b.cmp(a));
    candidates.into_iter().find_map(|(_, path)| {
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| settings_log(&format!("skipping {}: {error}", path.display())))
            .ok()?;
        let value: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|error| settings_log(&format!("skipping {}: {error}", path.display())))
            .ok()?;
        let has_settings = value
            .as_object()
            .is_some_and(|object| CARRY_OVER_KEYS.iter().any(|key| object.contains_key(*key)));
        if !has_settings {
            return None;
        }
        let config = serde_json::from_value::<Config>(value)
            .map_err(|error| settings_log(&format!("skipping {}: {error}", path.display())))
            .ok()?;
        Some((config, path))
    })
}

fn save_settings_to(dir: &Path, config: &Config) -> Result<(), String> {
    let path = dir.join(SETTINGS_FILE);
    let json = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    gitterm::merge_plan::write_atomically(&path, &json)
}

// Workspace persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesFile {
    pub workspaces: Vec<WorkspaceConfig>,
    pub active_workspace: usize,
    #[serde(default = "default_remote_sessions")]
    pub remote_sessions: Vec<RemoteSessionConfig>,
    #[serde(
        default = "default_remote_agents",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remote_agents: Vec<RemoteAgentConfig>,
    /// Machine group keys ("local", "agent:<id>", ...) whose `Repos` zone
    /// on the bottom bar is collapsed (TRU-175). Shared across instances
    /// and restarts; empty (every zone expanded) by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_repo_groups: Vec<String>,
    /// The repos workspaces list (TRU-180): a checkout's run command and
    /// env, shared by every workspace listing it. Files written before
    /// TRU-180 have none; `normalize_repos` derives them from each
    /// workspace's own fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoConfig>,
}

/// A repo as `workspaces.json` stores it (TRU-180). Identified by `dir` on
/// its machine (`location`; `None` is this Mac), see `RepoKey`. It is
/// open while an open workspace lists it; this entry persists while any
/// workspace does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<WorkspaceLocationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
    /// Environment for terminals and tabs running in this repo; a
    /// workspace's `env_overrides` apply on top.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Read only: the build that kept bottom terminals on repos wrote
    /// them here. `WorkspacesFile::normalize_repos` hands them to the
    /// workspaces listing the repo; terminals are each workspace's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bottom_terminals: Vec<BottomTerminalConfig>,
}

impl RepoConfig {
    pub fn key(&self) -> RepoKey {
        RepoKey::of_config(&self.dir, self.location.as_ref())
    }
}

/// Which checkout a repo is (TRU-180): its canonical dir on this Mac, or
/// its root on a remote machine. Two workspaces whose repos have the same
/// key share one repo.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepoKey {
    Local(PathBuf),
    RemoteAgent {
        remote_id: String,
        root: String,
    },
    /// A tmux-session workspace from before remote agents. Runtime only:
    /// its dir is a local stand-in, so it is never stored as a repo.
    LegacyRemote {
        session_name: String,
        root: String,
    },
}

impl RepoKey {
    /// A local checkout, by canonical path. A folder that cannot be
    /// resolved (gone, unreadable) keys by the path as given.
    pub fn local(dir: &Path) -> Self {
        RepoKey::Local(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
    }

    pub fn of_config(dir: &str, location: Option<&WorkspaceLocationConfig>) -> Self {
        match location {
            Some(WorkspaceLocationConfig::RemoteAgent { remote_id, .. }) => RepoKey::RemoteAgent {
                remote_id: remote_id.clone(),
                root: dir.to_string(),
            },
            Some(WorkspaceLocationConfig::Local { .. }) | None => RepoKey::local(Path::new(dir)),
        }
    }

    /// A checkout on this Mac.
    pub fn is_local(&self) -> bool {
        matches!(self, RepoKey::Local(_))
    }
}

/// `env` with `overrides` on top: what a tab, terminal or console in a
/// repo of a workspace runs with (TRU-180).
pub fn layered_env(
    env: &HashMap<String, String>,
    overrides: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut layered = env.clone();
    layered.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
    layered
}

/// The entries of `env` that `base` lacks or holds differently: what a
/// workspace joining a shared repo keeps as its overrides.
pub fn env_difference(
    env: &HashMap<String, String>,
    base: &HashMap<String, String>,
) -> HashMap<String, String> {
    env.iter()
        .filter(|(key, value)| base.get(*key) != Some(*value))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteSessionsFile {
    #[serde(default = "default_remote_sessions")]
    pub remote_sessions: Vec<RemoteSessionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteAgentsFile {
    #[serde(default = "default_remote_agents")]
    pub remote_agents: Vec<RemoteAgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionConfig {
    pub label: String,
    pub host_name: String,
    pub ssh_target: String,
    pub identity_file: String,
    #[serde(default = "default_remote_session_tmux_path")]
    pub tmux_path: String,
    pub session_name: String,
    pub remote_dir: String,
    #[serde(default = "default_remote_session_shell")]
    pub shell_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub auth: RemoteAgentAuthConfig,
    /// Shell command for plain sessions; run via `/bin/sh -lc` on the
    /// remote. Defaults to a login zsh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command: Option<String>,
    /// Launch commands per agent-preset name (lowercased), e.g.
    /// {"claude": "/Users/me/.local/bin/claude"}. Unlisted presets fall
    /// back to their lowercase name on the remote PATH.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub session_commands: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteAgentAuthConfig {
    Token { token_ref: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceLocationConfig {
    Local {
        root: String,
    },
    RemoteAgent {
        remote_id: String,
        workspace_id: String,
        root: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: String,
    pub abbrev: String,
    pub dir: String,
    pub color: WorkspaceColor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<WorkspaceLocationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_session: Option<RemoteSessionConfig>,
    /// Active tab index for this workspace. Older config files omit this; restore
    /// code falls back to the workspace-root tab when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<usize>,
    pub tabs: Vec<WorkspaceTabConfig>,
    #[serde(default)]
    pub run_command: Option<String>,
    #[serde(default)]
    pub bottom_terminals: Vec<BottomTerminalConfig>,
    /// Environment variables to inject into all terminal sessions in this workspace.
    /// Edit workspaces.json to add any vars without recompiling, e.g.:
    /// "env": { "LINEAR_WORKSPACE": "truinsights", "LINEAR_TEAM": "TRU", "GH_TOKEN": "..." }
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Whether this workspace is currently open. Closed workspaces are kept in
    /// workspaces.json so their env vars and settings are preserved for reopening.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Root chat session ids this workspace owns (TRU-169). A workspace
    /// that owns chats is a piece of work: its overview reads these chats
    /// and their children wherever they ran, and resuming one opens it
    /// here. Empty keeps the folder-scoped behaviour. A chat belongs to
    /// at most one workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chats: Vec<String>,
    /// Set when × parked this workspace (TRU-171): the note to come back
    /// to and when. A closed entry without it (closed before parking
    /// existed) lists as parked with an empty note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<ParkedInfo>,
    /// When the work was marked done (RFC 3339). A done workspace leaves
    /// the Parked list but stays in this file; its chats stay findable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<String>,
    /// Agents started in this workspace get no change guidelines
    /// (TRU-174). See `gitterm::guidelines`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guidelines_off: bool,
    /// Claude sessions started in this workspace get Claude in Chrome
    /// (`--chrome`); off (`--no-chrome`) by default, so agents use GitTerm's
    /// own browser tools (TRU-181). Read when a Claude process starts; a
    /// running one keeps what it started with.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claude_in_chrome: bool,
    /// The listed repo team-facing decisions go to (TRU-191), by dir; the
    /// work's agents are told it beside the work's docs folder. None by
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions_repo: Option<String>,
    /// The repos this workspace lists, by dir on its machine, home first
    /// (TRU-180). Their settings live in `WorkspacesFile::repos`. Files
    /// written before TRU-180 have none: `normalize_repos` makes `[dir]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// The home repo's dir, where new chats start; `dir` until set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Env applied on top of a repo's env for this workspace's tabs and
    /// terminals (TRU-180). `env` and `run_command` above are still
    /// written, from the home repo (env with these applied), so a build
    /// before TRU-180 reads them; `bottom_terminals` is the workspace's.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_overrides: HashMap<String, String>,
    /// Repos added by hand ("+ add repo"); listed whatever the chats touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos_added: Vec<String>,
    /// Repos removed by hand (×, or undoing an auto-add); never listed
    /// again by the chats touching them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos_removed: Vec<String>,
}

/// Why and when a workspace was parked (TRU-171).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedInfo {
    #[serde(default)]
    pub note: String,
    /// RFC 3339.
    pub parked_at: String,
}

impl WorkspaceConfig {
    /// On the Parked list: closed and not done.
    pub fn is_parked(&self) -> bool {
        !self.active && self.done_at.is_none()
    }

    /// The home repo's dir: `home`, else `dir` (files before TRU-180).
    pub fn home_dir(&self) -> &str {
        self.home.as_deref().unwrap_or(&self.dir)
    }

    /// A tmux-session workspace from before remote agents: its repo is
    /// never stored in the `repos` section (its dir is a local stand-in),
    /// so its own fields stay the source of its settings.
    pub fn is_legacy_remote(&self) -> bool {
        self.location.is_none() && self.remote_session.is_some()
    }

    /// The key of repo `dir` this workspace lists; `None` for a legacy
    /// remote workspace (see `is_legacy_remote`).
    pub fn repo_key(&self, dir: &str) -> Option<RepoKey> {
        (!self.is_legacy_remote()).then(|| RepoKey::of_config(dir, self.location.as_ref()))
    }

    /// The home repo's settings as this entry's own (pre-TRU-180) fields
    /// hold them. Its terminals stay the workspace's.
    pub fn own_home_repo(&self) -> RepoConfig {
        RepoConfig {
            dir: self.home_dir().to_string(),
            location: self.location.clone(),
            run_command: self.run_command.clone(),
            env: self.env.clone(),
            bottom_terminals: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTabConfig {
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<String>,
    /// Tab kind discriminator. `None` or `Some("terminal")` means a terminal tab
    /// (the implicit default — older `workspaces.json` files predate this field).
    /// `Some("agent")` means a Claude Code / pi agent tab; see `agent_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_kind: Option<String>,
    /// Backend config for `tab_kind: "agent"`. Required when `tab_kind == Some("agent")`;
    /// ignored otherwise. Schema: `{"backend": "pi"|"claude", ...backend-specific fields}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<crate::tab::AgentBackendConfig>,
    /// Harness conversation owned by this tab (claude session uuid) — set
    /// when the tab was spawned by the Chats panel resume flow or by a
    /// picker launch with a pre-assigned id. Keys the one-live-tab-per-
    /// conversation registry rule (TRU-78) and survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
    /// Durable task owned by this view. Closing the tab does not remove the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Durable identity for this child view inside a task. Several tabs may
    /// share one task id; this id distinguishes their sessions across restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_session_id: Option<String>,
    /// Durable caller identity of this tab (uuid v4). Every tab carries one
    /// on its task MCP URL (`?caller=<uid>`) so the task server can tell
    /// which tab made a call. Older files predate it; a missing value is
    /// generated when the tab is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottomTerminalConfig {
    pub dir: String,
}

impl WorkspacesFile {
    pub fn file_path() -> PathBuf {
        global_config_dir().join("workspaces.json")
    }

    /// Fills in the repo model (TRU-180) for entries written without it:
    /// each workspace lists its `dir` as home and only repo, and a home
    /// with no `repos` entry gets one from the workspace's own fields.
    /// Open workspaces go first, in file order, so the first open one
    /// sharing a dir wins its run command and env; a later one's
    /// differing env becomes its overrides. Entries already in `repos`
    /// are left as stored.
    ///
    /// Bottom terminals are each workspace's own `bottom_terminals`. A
    /// file from the build that kept them on repos (`repos[].bottom_terminals`)
    /// gives each workspace the terminals of every repo it lists, home
    /// first: a repo's terminals shared by several workspaces go to each.
    pub fn normalize_repos(&mut self) {
        let repo_terminals: Vec<(RepoKey, Vec<BottomTerminalConfig>)> = self
            .repos
            .iter_mut()
            .map(|repo| (repo.key(), std::mem::take(&mut repo.bottom_terminals)))
            .collect();
        let terminals_on_repos = repo_terminals.iter().any(|(_, t)| !t.is_empty());
        let mut created: Vec<RepoKey> = Vec::new();
        let order: Vec<usize> = (0..self.workspaces.len())
            .filter(|&i| self.workspaces[i].active)
            .chain((0..self.workspaces.len()).filter(|&i| !self.workspaces[i].active))
            .collect();
        for idx in order {
            let ws = &mut self.workspaces[idx];
            let home = ws.home_dir().to_string();
            ws.home = Some(home.clone());
            ws.repos.retain(|dir| *dir != home);
            ws.repos.insert(0, home.clone());
            let Some(key) = ws.repo_key(&home) else {
                continue;
            };
            match self.repos.iter_mut().find(|repo| repo.key() == key) {
                None => {
                    self.repos.push(ws.own_home_repo());
                    created.push(key);
                }
                Some(repo) if created.contains(&key) => {
                    for (k, v) in env_difference(&ws.env, &repo.env) {
                        ws.env_overrides.entry(k).or_insert(v);
                    }
                }
                Some(_) => {}
            }
        }
        if !terminals_on_repos {
            return;
        }
        for ws in &mut self.workspaces {
            let stored = |dir: &String| {
                let key = ws.repo_key(dir)?;
                repo_terminals.iter().find(|(k, _)| *k == key)
            };
            // A home the repo section never held (a legacy remote, an entry
            // an older build added) keeps the workspace's own terminals.
            if ws.repos.first().and_then(stored).is_none() {
                continue;
            }
            let terminals: Vec<BottomTerminalConfig> = ws
                .repos
                .iter()
                .filter_map(stored)
                .flat_map(|(_, terminals)| terminals.iter().cloned())
                .collect();
            ws.bottom_terminals = terminals;
        }
    }

    /// The stored repo with `key`.
    pub fn repo(&self, key: &RepoKey) -> Option<&RepoConfig> {
        self.repos.iter().find(|repo| repo.key() == *key)
    }

    /// The env a tab or terminal of workspace `name` running in `cwd`
    /// gets (TRU-180): the env of the deepest repo it lists containing
    /// `cwd`, else of its home repo (every tab got the workspace's env
    /// before repos), with the workspace's overrides on top. A legacy
    /// remote workspace, or a home with no stored repo, uses the
    /// workspace's own `env`. An unknown workspace gets none, as before.
    pub fn workspace_env(&self, name: &str, cwd: Option<&Path>) -> HashMap<String, String> {
        let Some(ws) = self.workspaces.iter().find(|ws| ws.name == name) else {
            return HashMap::new();
        };
        let home = ws.home_dir();
        let dir = cwd
            .filter(|_| !ws.is_legacy_remote())
            .and_then(|cwd| {
                let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
                ws.repos
                    .iter()
                    .filter(|dir| {
                        let dir = Path::new(dir.as_str());
                        let canonical =
                            std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                        cwd.starts_with(&canonical) || cwd.starts_with(dir)
                    })
                    .max_by_key(|dir| Path::new(dir.as_str()).components().count())
            })
            .map(String::as_str)
            .unwrap_or(home);
        let stored = |dir: &str| ws.repo_key(dir).and_then(|key| self.repo(&key));
        let base = match stored(dir) {
            Some(repo) => &repo.env,
            // A listed repo with nothing stored yet has no env of its own.
            None if dir != home => return ws.env_overrides.clone(),
            None => &ws.env,
        };
        layered_env(base, &ws.env_overrides)
    }

    /// Entries a TRU-180 build wrote whose `env` copy no longer matches
    /// what the repo model gives them (the copy was edited by hand), with
    /// the keys that differ. That copy is only for older builds: tabs get
    /// the repo's env and `env_overrides`, so edits belong there.
    pub fn stale_env_copies(&self) -> Vec<(String, Vec<String>)> {
        self.workspaces
            .iter()
            .filter(|ws| ws.home.is_some())
            .filter_map(|ws| {
                let repo = self.repo(&ws.repo_key(ws.home_dir())?)?;
                let layered = layered_env(&repo.env, &ws.env_overrides);
                let mut keys: Vec<String> = ws
                    .env
                    .keys()
                    .chain(layered.keys())
                    .filter(|key| ws.env.get(*key) != layered.get(*key))
                    .cloned()
                    .collect();
                keys.sort();
                keys.dedup();
                (!keys.is_empty()).then(|| (ws.name.clone(), keys))
            })
            .collect()
    }

    /// `workspaces.json` read fresh from disk with the repo model filled
    /// in; `None` when absent or unreadable, as `load`.
    pub fn load_normalized() -> Option<Self> {
        let mut file = Self::load()?;
        file.normalize_repos();
        Some(file)
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    pub fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

impl RemoteSessionsFile {
    pub fn file_path() -> PathBuf {
        global_config_dir().join("remotes.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    pub fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

impl RemoteAgentsFile {
    pub fn file_path() -> PathBuf {
        global_config_dir().join("remote-agents.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    pub fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
