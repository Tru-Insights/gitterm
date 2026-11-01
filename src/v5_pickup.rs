//! Picking up a GitTerm V5 conversation in an imported chat tab (TRU-183).
//!
//! The first-launch import (`config_import`) unbinds every tab from its V5
//! conversation, so V5 and V6 never resume one Claude session together.
//! An imported chat tab therefore opens empty. This module finds the V5
//! conversations such a tab could continue: the ones V5's `workspaces.json`
//! holds in a workspace with the tab's dir that no V6 tab is bound to,
//! likeliest first. The V5 dir is only ever read.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config_import;
use crate::tab::AgentBackendConfig;

pub const LOG_PREFIX: &str = "V5 conversation pickup";

/// What kind of V5 tab held a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V5Origin {
    /// A native Claude chat tab.
    Chat,
    /// A terminal tab running `claude --resume <id>`.
    Terminal,
}

/// One conversation a V5 tab was bound to.
#[derive(Debug, Clone, PartialEq)]
pub struct V5Conversation {
    pub session_id: String,
    /// The tab's index among the V5 workspace's tabs the import keeps, so
    /// it lines up with the imported V6 tab's index.
    pub position: usize,
    /// The chat tab's backend config, normalized through V6's own type;
    /// `None` for terminal tabs and configs V6 cannot read.
    pub agent_config: Option<Value>,
    pub origin: V5Origin,
}

/// A V5 workspace's dir and the conversations its tabs held.
#[derive(Debug, Clone, PartialEq)]
pub struct V5Workspace {
    pub dir: PathBuf,
    pub conversations: Vec<V5Conversation>,
}

/// The V6 chat tab asking: its workspace's dir, its index there, and its
/// backend config.
#[derive(Debug, Clone)]
pub struct TabQuery {
    pub dir: PathBuf,
    pub position: usize,
    pub agent_config: Option<Value>,
}

/// A V6 backend config as compared with V5's.
pub fn config_value(config: &AgentBackendConfig) -> Option<Value> {
    serde_json::to_value(config).ok()
}

/// Read V5's `workspaces.json` in `v5_dir`. `Err` says why there is
/// nothing to offer: no file, unreadable, or not a workspaces file.
pub fn read_v5_workspaces(v5_dir: &Path) -> Result<Vec<V5Workspace>, String> {
    let path = v5_dir.join("workspaces.json");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no V5 workspaces file at {}", path.display()));
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let value: Value = serde_json::from_str(&contents)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    let workspaces = value
        .get("workspaces")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{} has no workspaces list", path.display()))?;
    Ok(workspaces
        .iter()
        .filter_map(|workspace| {
            let dir = workspace.get("dir").and_then(Value::as_str)?;
            let tabs = workspace.get("tabs").and_then(Value::as_array)?;
            let conversations = tabs
                .iter()
                .filter(|tab| config_import::import_keeps_tab(tab, v5_dir))
                .enumerate()
                .filter_map(|(position, tab)| tab_conversation(tab, position))
                .collect();
            Some(V5Workspace {
                dir: config_import::expand_home(dir),
                conversations,
            })
        })
        .collect())
}

/// The conversation a V5 tab was bound to: a Claude chat tab's
/// `chat_session_id`, or a terminal's `claude … --resume <id>`.
fn tab_conversation(tab: &Value, position: usize) -> Option<V5Conversation> {
    let chat_session_id = tab
        .get("chat_session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if tab.get("tab_kind").and_then(Value::as_str) == Some("agent") {
        let config = tab.get("agent_config")?;
        if config.get("backend").and_then(Value::as_str) != Some("claude") {
            return None;
        }
        let agent_config = serde_json::from_value::<AgentBackendConfig>(config.clone())
            .ok()
            .as_ref()
            .and_then(config_value);
        return Some(V5Conversation {
            session_id: chat_session_id?.to_string(),
            position,
            agent_config,
            origin: V5Origin::Chat,
        });
    }
    let resumed = tab
        .get("startup_command")
        .and_then(Value::as_str)
        .and_then(claude_resume_id)?;
    Some(V5Conversation {
        session_id: resumed,
        position,
        agent_config: None,
        origin: V5Origin::Terminal,
    })
}

/// The session id of a plain `claude … --resume <id>` command. A quoted
/// command (an embedded brief) is not inspected.
pub fn claude_resume_id(command: &str) -> Option<String> {
    if command.contains(['"', '\'']) {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.first() != Some(&"claude") {
        return None;
    }
    let idx = words.iter().position(|word| *word == "--resume")?;
    words
        .get(idx + 1)
        .filter(|id| !id.starts_with('-'))
        .map(|id| id.to_string())
}

/// Whether two workspace dirs name the same folder.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The V5 conversations `tab` could continue: held in a V5 workspace with
/// the tab's dir and bound to no V6 tab (`bound`, by session id), each
/// once. The likeliest come first: same position and same config, then
/// same config, then same position, then chat tabs before terminals, then
/// V5's order.
pub fn candidates<'a>(
    v5: &'a [V5Workspace],
    tab: &TabQuery,
    bound: &HashMap<String, String>,
) -> Vec<&'a V5Conversation> {
    let mut seen = std::collections::HashSet::new();
    let mut found: Vec<&V5Conversation> = v5
        .iter()
        .filter(|workspace| same_dir(&workspace.dir, &tab.dir))
        .flat_map(|workspace| &workspace.conversations)
        .filter(|conversation| !bound.contains_key(&conversation.session_id))
        .filter(|conversation| seen.insert(conversation.session_id.as_str()))
        .collect();
    let rank = |conversation: &V5Conversation| {
        let same_config =
            conversation.agent_config.is_some() && conversation.agent_config == tab.agent_config;
        let same_position = conversation.position == tab.position;
        (
            !(same_config && same_position),
            !same_config,
            !same_position,
            conversation.origin != V5Origin::Chat,
        )
    };
    found.sort_by_key(|conversation| rank(conversation));
    found
}

/// Why `session_id` cannot be bound to a tab: another V6 tab has it
/// (`bound`: session id to the workspace holding that tab), or its
/// transcript is gone. The text is shown to the human.
pub fn bind_refusal(
    session_id: &str,
    bound: &HashMap<String, String>,
    transcript_exists: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(workspace) = bound.get(session_id) {
        return Some(format!(
            "Another tab in {workspace} already has this conversation. Switch to that tab instead."
        ));
    }
    if !transcript_exists(session_id) {
        return Some(
            "This conversation's transcript no longer exists, so it cannot be continued."
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_config(model: &str) -> Value {
        json!({"backend": "claude", "model": model, "permission_mode": "default"})
    }

    fn normalized(config: Value) -> Option<Value> {
        serde_json::from_value::<AgentBackendConfig>(config)
            .ok()
            .as_ref()
            .and_then(config_value)
    }

    /// A V5 dir whose workspaces file holds `/work/repo` with two chat
    /// tabs, a resumed terminal, a plain terminal and a task tab, and
    /// `/work/other` with one chat.
    fn v5_fixture(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("workspaces.json"),
            json!({
                "active_workspace": 0,
                "workspaces": [
                    {
                        "name": "Repo", "abbrev": "RE", "dir": "/work/repo", "color": "Blue",
                        "tabs": [
                            {"dir": "/work/repo", "task_id": "task-1", "chat_session_id": "task-chat"},
                            {"dir": "/work/repo", "tab_kind": "agent",
                             "agent_config": claude_config("opus"), "chat_session_id": "chat-a"},
                            {"dir": "/work/repo", "startup_command": "claude --model sonnet --resume term-b"},
                            {"dir": "/work/repo"},
                            {"dir": "/work/repo", "tab_kind": "agent",
                             "agent_config": claude_config("sonnet"), "chat_session_id": "chat-c"},
                            {"dir": "/work/repo", "tab_kind": "agent",
                             "agent_config": {"backend": "pi", "model": "m"}, "chat_session_id": "pi-d"}
                        ]
                    },
                    {
                        "name": "Other", "abbrev": "OT", "dir": "/work/other", "color": "Green",
                        "tabs": [
                            {"dir": "/work/other", "tab_kind": "agent",
                             "agent_config": claude_config("opus"), "chat_session_id": "chat-e"}
                        ]
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
    }

    fn query(dir: &str, position: usize, model: &str) -> TabQuery {
        TabQuery {
            dir: PathBuf::from(dir),
            position,
            agent_config: normalized(claude_config(model)),
        }
    }

    fn ids(found: &[&V5Conversation]) -> Vec<String> {
        found.iter().map(|c| c.session_id.clone()).collect()
    }

    #[test]
    fn reads_chat_and_resumed_terminal_tabs_at_their_imported_positions() {
        let temp = tempfile::tempdir().unwrap();
        v5_fixture(temp.path());
        let v5 = read_v5_workspaces(temp.path()).unwrap();
        assert_eq!(v5.len(), 2);
        let repo = &v5[0];
        assert_eq!(repo.dir, PathBuf::from("/work/repo"));
        // The task tab is dropped by the import, so positions start after it;
        // the pi chat and the plain terminal hold nothing to continue.
        let found: Vec<(&str, usize, V5Origin)> = repo
            .conversations
            .iter()
            .map(|c| (c.session_id.as_str(), c.position, c.origin))
            .collect();
        assert_eq!(
            found,
            vec![
                ("chat-a", 0, V5Origin::Chat),
                ("term-b", 1, V5Origin::Terminal),
                ("chat-c", 3, V5Origin::Chat),
            ]
        );
    }

    #[test]
    fn candidates_match_the_dir_and_rank_position_and_config_first() {
        let temp = tempfile::tempdir().unwrap();
        v5_fixture(temp.path());
        let v5 = read_v5_workspaces(temp.path()).unwrap();
        let bound = HashMap::new();

        // Position 3 with the sonnet config is chat-c's own slot.
        assert_eq!(
            ids(&candidates(&v5, &query("/work/repo", 3, "sonnet"), &bound)),
            vec!["chat-c", "chat-a", "term-b"]
        );
        // Position 0, opus: chat-a's slot.
        assert_eq!(
            ids(&candidates(&v5, &query("/work/repo", 0, "opus"), &bound)),
            vec!["chat-a", "chat-c", "term-b"]
        );
        // Same config beats same position alone.
        assert_eq!(
            ids(&candidates(&v5, &query("/work/repo", 1, "sonnet"), &bound)),
            vec!["chat-c", "term-b", "chat-a"]
        );
        // A trailing slash names the same dir.
        assert_eq!(
            ids(&candidates(&v5, &query("/work/repo/", 0, "opus"), &bound)),
            vec!["chat-a", "chat-c", "term-b"]
        );
        assert_eq!(
            ids(&candidates(&v5, &query("/work/other", 0, "opus"), &bound)),
            vec!["chat-e"]
        );
    }

    #[test]
    fn conversations_bound_in_v6_are_skipped() {
        let temp = tempfile::tempdir().unwrap();
        v5_fixture(temp.path());
        let v5 = read_v5_workspaces(temp.path()).unwrap();
        let bound = HashMap::from([
            ("chat-a".to_string(), "Repo".to_string()),
            ("term-b".to_string(), "Elsewhere".to_string()),
        ]);
        assert_eq!(
            ids(&candidates(&v5, &query("/work/repo", 0, "opus"), &bound)),
            vec!["chat-c"]
        );
        let all_bound = HashMap::from([("chat-e".to_string(), "Other".to_string())]);
        assert!(candidates(&v5, &query("/work/other", 0, "opus"), &all_bound).is_empty());
    }

    #[test]
    fn a_fresh_v6_workspace_with_no_v5_match_offers_nothing() {
        let temp = tempfile::tempdir().unwrap();
        v5_fixture(temp.path());
        let v5 = read_v5_workspaces(temp.path()).unwrap();
        assert!(candidates(&v5, &query("/work/new-in-v6", 0, "opus"), &HashMap::new()).is_empty());
    }

    #[test]
    fn a_missing_or_malformed_v5_file_offers_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let missing = read_v5_workspaces(temp.path()).unwrap_err();
        assert!(missing.contains("no V5 workspaces file"), "{missing}");

        fs::write(temp.path().join("workspaces.json"), "{ not json").unwrap();
        let malformed = read_v5_workspaces(temp.path()).unwrap_err();
        assert!(malformed.contains("not valid JSON"), "{malformed}");

        fs::write(temp.path().join("workspaces.json"), r#"{"tabs": []}"#).unwrap();
        let wrong_shape = read_v5_workspaces(temp.path()).unwrap_err();
        assert!(wrong_shape.contains("no workspaces list"), "{wrong_shape}");
    }

    #[test]
    fn binding_refuses_a_duplicate_or_a_missing_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let transcripts = temp.path().join("transcripts");
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(transcripts.join("chat-a.jsonl"), "{}\n").unwrap();
        let exists = |id: &str| transcripts.join(format!("{id}.jsonl")).is_file();

        let bound = HashMap::from([("chat-c".to_string(), "Repo".to_string())]);
        assert_eq!(bind_refusal("chat-a", &bound, exists), None);
        let duplicate = bind_refusal("chat-c", &bound, exists).unwrap();
        assert!(duplicate.contains("Another tab in Repo"), "{duplicate}");
        let missing = bind_refusal("chat-gone", &bound, exists).unwrap();
        assert!(missing.contains("no longer exists"), "{missing}");
    }

    #[test]
    fn the_v5_dir_is_byte_identical_after_every_operation() {
        let temp = tempfile::tempdir().unwrap();
        v5_fixture(temp.path());
        let file = temp.path().join("workspaces.json");
        let before = fs::read(&file).unwrap();
        let listing = |dir: &Path| {
            let mut names: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let names_before = listing(temp.path());

        let v5 = read_v5_workspaces(temp.path()).unwrap();
        let found = candidates(&v5, &query("/work/repo", 0, "opus"), &HashMap::new());
        for conversation in found {
            let _ = bind_refusal(&conversation.session_id, &HashMap::new(), |_| true);
        }
        assert_eq!(fs::read(&file).unwrap(), before);
        assert_eq!(listing(temp.path()), names_before);
    }

    #[test]
    fn resume_ids_come_from_plain_claude_commands_only() {
        assert_eq!(claude_resume_id("claude --resume abc"), Some("abc".into()));
        assert_eq!(
            claude_resume_id("claude --model opus --resume abc --verbose"),
            Some("abc".into())
        );
        assert_eq!(claude_resume_id("claude"), None);
        assert_eq!(claude_resume_id("codex resume abc"), None);
        assert_eq!(claude_resume_id("claude --resume"), None);
        assert_eq!(claude_resume_id("claude --resume --verbose"), None);
        assert_eq!(claude_resume_id("claude 'brief' --resume abc"), None);
    }
}
