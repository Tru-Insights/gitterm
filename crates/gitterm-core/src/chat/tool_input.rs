//! Shared path resolution and completed tool-input tracking. Home expansion is explicit.
use crate::chat::types::{HarnessEvent, ItemKind};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// `path` with its deepest existing ancestor canonicalized and `.` / `..`
/// folded lexically, including file targets that do not exist yet.
pub fn resolve_for_match(path: &Path) -> PathBuf {
    let path = normalize_lexically(path);
    let mut existing = path.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            let mut resolved = canonical;
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path,
        }
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Tools whose input names a file or directory.
const PATH_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Glob",
    "Grep",
];
const PATH_KEYS: &[&str] = &["file_path", "path", "notebook_path"];

fn is_watched_tool(name: &str) -> bool {
    name == "Bash" || PATH_TOOLS.contains(&name)
}

/// Absolute paths a tool call says the agent is working at. Relative paths
/// are ignored: the agent's cwd is not known here.
pub fn tool_call_candidates(name: &str, input: &Value, home: Option<&Path>) -> Vec<PathBuf> {
    if PATH_TOOLS.contains(&name) {
        return PATH_KEYS
            .iter()
            .filter_map(|key| input.get(*key).and_then(Value::as_str))
            .filter_map(|raw| absolute_path(raw, home))
            .collect();
    }
    if name == "Bash" {
        return input
            .get("command")
            .and_then(Value::as_str)
            .and_then(|command| bash_leading_dir(command, home))
            .into_iter()
            .collect();
    }
    Vec::new()
}

fn absolute_path(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let expanded = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    expanded
        .is_absolute()
        .then(|| normalize_lexically(&expanded))
}

/// The directory of a Bash command's leading `cd <path>` or
/// `git -C <path>`, when it is absolute (or `~`-relative).
pub fn bash_leading_dir(command: &str, home: Option<&Path>) -> Option<PathBuf> {
    let words = first_simple_command_words(command);
    let raw = match words.as_slice() {
        [cd, dir, ..] if cd == "cd" => dir,
        [git, flag, dir, ..] if git == "git" && flag == "-C" => dir,
        _ => return None,
    };
    absolute_path(raw, home)
}

/// Words of the first simple command, honouring single and double quotes
/// and stopping at the first unquoted `;`, `&`, `|` or newline.
fn first_simple_command_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.trim_start().chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        word.push(next);
                        in_word = true;
                    }
                }
                ';' | '&' | '|' | '\n' => break,
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    word.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Collects streamed tool-call inputs until they are complete.
///
/// The main agent's `ItemStarted` carries an empty input (`{}`); the real
/// input streams as `ItemInputDelta` fragments and is complete once the
/// tool's result (`ItemCompleted`) arrives. Subagent tool calls arrive
/// whole in `ItemStarted`.
#[derive(Debug, Default)]
pub struct ToolInputTracker {
    pending: HashMap<String, PendingCall>,
}

#[derive(Debug)]
struct PendingCall {
    name: String,
    json: String,
}

/// One tool call whose input is complete.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl ToolInputTracker {
    /// Feeds one harness event; returns candidate paths of a tool call whose
    /// input became complete with it.
    pub fn observe(&mut self, event: &HarnessEvent, home: Option<&Path>) -> Vec<PathBuf> {
        self.observe_calls(event, is_watched_tool)
            .map(|call| tool_call_candidates(&call.name, &call.input, home))
            .unwrap_or_default()
    }

    /// Feeds one harness event; returns the call of a tool `watch` accepts
    /// whose input became complete with it. Other tools are never buffered.
    pub fn observe_calls(
        &mut self,
        event: &HarnessEvent,
        watch: impl Fn(&str) -> bool,
    ) -> Option<CompletedToolCall> {
        match event {
            HarnessEvent::SubagentEvent { event, .. } => self.observe_calls(event, watch),
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, input },
            } => {
                if !watch(name) {
                    return None;
                }
                if input.as_object().is_some_and(|object| !object.is_empty()) {
                    return Some(CompletedToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
                self.pending.insert(
                    id.clone(),
                    PendingCall {
                        name: name.clone(),
                        json: String::new(),
                    },
                );
                None
            }
            HarnessEvent::ItemInputDelta { id, partial_json } => {
                if let Some(call) = self.pending.get_mut(id) {
                    call.json.push_str(partial_json);
                }
                None
            }
            HarnessEvent::ItemCompleted { id, .. } => {
                let call = self.pending.remove(id)?;
                if call.json.is_empty() {
                    return None;
                }
                match serde_json::from_str::<Value>(&call.json) {
                    Ok(input) => Some(CompletedToolCall {
                        id: id.clone(),
                        name: call.name,
                        input,
                    }),
                    Err(error) => {
                        eprintln!(
                            "[worktree-follow] {} call {id}: streamed input is not JSON ({error}); skipped",
                            call.name
                        );
                        None
                    }
                }
            }
            HarnessEvent::TurnCompleted { .. } | HarnessEvent::ProcessExited { .. } => {
                self.pending.clear();
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn path_tool_inputs_yield_absolute_candidates() {
        let home = Path::new("/Users/me");
        assert_eq!(
            tool_call_candidates(
                "Edit",
                &json!({"file_path": "/repos/app/./src/../x.rs"}),
                Some(home)
            ),
            vec![PathBuf::from("/repos/app/x.rs")]
        );
        assert_eq!(
            tool_call_candidates(
                "Grep",
                &json!({"pattern": "fn", "path": "~/GitRepo/x"}),
                Some(home)
            ),
            vec![PathBuf::from("/Users/me/GitRepo/x")]
        );
        assert_eq!(
            tool_call_candidates(
                "NotebookEdit",
                &json!({"notebook_path": "/n/a.ipynb"}),
                None
            ),
            vec![PathBuf::from("/n/a.ipynb")]
        );
        assert!(
            tool_call_candidates("Glob", &json!({"pattern": "**/*.rs", "path": "src"}), None)
                .is_empty()
        );
        assert!(tool_call_candidates("WebFetch", &json!({"url": "/x"}), None).is_empty());
    }

    #[test]
    fn bash_leading_cd_and_git_dash_c() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(
            bash_leading_dir("cd /repos/app-worktrees/feature && cargo test", home),
            Some(PathBuf::from("/repos/app-worktrees/feature"))
        );
        assert_eq!(
            bash_leading_dir("  cd \"/repos/my app\"; ls", home),
            Some(PathBuf::from("/repos/my app"))
        );
        assert_eq!(
            bash_leading_dir("cd ~/GitRepo/cree8-worktrees/x|cat", home),
            Some(PathBuf::from("/Users/me/GitRepo/cree8-worktrees/x"))
        );
        assert_eq!(
            bash_leading_dir(
                "git -C '/repos/app/.claude/worktrees/fix-login' status",
                home
            ),
            Some(PathBuf::from("/repos/app/.claude/worktrees/fix-login"))
        );
        assert_eq!(
            bash_leading_dir("cd /a/my\\ dir && make", home),
            Some(PathBuf::from("/a/my dir"))
        );
        // Not leading, relative, or not a cd / git -C.
        assert_eq!(bash_leading_dir("cargo test && cd /repos/x", home), None);
        assert_eq!(bash_leading_dir("cd src", home), None);
        assert_eq!(bash_leading_dir("git status", home), None);
        assert_eq!(bash_leading_dir("ls /repos/app", home), None);
        assert_eq!(bash_leading_dir("cd ~/x", None), None);
    }

    #[test]
    fn tracker_completes_streamed_main_agent_inputs() {
        let mut tracker = ToolInputTracker::default();
        let started = HarnessEvent::ItemStarted {
            id: "t1".to_string(),
            kind: ItemKind::ToolCall {
                name: "Bash".to_string(),
                input: json!({}),
            },
        };
        assert!(tracker.observe(&started, None).is_empty());
        for part in [
            r#"{"command": "cd /repos/"#,
            r#"app-worktrees/feature && ls"}"#,
        ] {
            let delta = HarnessEvent::ItemInputDelta {
                id: "t1".to_string(),
                partial_json: part.to_string(),
            };
            assert!(tracker.observe(&delta, None).is_empty());
        }
        let done = HarnessEvent::ItemCompleted {
            id: "t1".to_string(),
            output: String::new(),
            is_error: false,
        };
        assert_eq!(
            tracker.observe(&done, None),
            vec![PathBuf::from("/repos/app-worktrees/feature")]
        );
        // Completed once; a duplicate completion yields nothing.
        assert!(tracker.observe(&done, None).is_empty());
    }

    #[test]
    fn tracker_reads_whole_subagent_inputs_and_skips_unwatched_tools() {
        let mut tracker = ToolInputTracker::default();
        let sub = HarnessEvent::SubagentEvent {
            parent_tool_use_id: "agent-1".to_string(),
            event: Box::new(HarnessEvent::ItemStarted {
                id: "s1".to_string(),
                kind: ItemKind::ToolCall {
                    name: "Read".to_string(),
                    input: json!({"file_path": "/repos/app/.claude/worktrees/fix-login/a.rs"}),
                },
            }),
        };
        assert_eq!(
            tracker.observe(&sub, None),
            vec![PathBuf::from("/repos/app/.claude/worktrees/fix-login/a.rs")]
        );
        let other = HarnessEvent::ItemStarted {
            id: "w1".to_string(),
            kind: ItemKind::ToolCall {
                name: "WebSearch".to_string(),
                input: json!({}),
            },
        };
        assert!(tracker.observe(&other, None).is_empty());
        let delta = HarnessEvent::ItemInputDelta {
            id: "w1".to_string(),
            partial_json: "{\"query\":\"x\"}".to_string(),
        };
        assert!(tracker.observe(&delta, None).is_empty());
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn tracker_drops_pending_calls_when_the_turn_ends() {
        let mut tracker = ToolInputTracker::default();
        tracker.observe(
            &HarnessEvent::ItemStarted {
                id: "t1".to_string(),
                kind: ItemKind::ToolCall {
                    name: "Edit".to_string(),
                    input: json!({}),
                },
            },
            None,
        );
        assert_eq!(tracker.pending.len(), 1);
        tracker.observe(
            &HarnessEvent::TurnCompleted {
                status: crate::chat::types::TurnStatus::Interrupted,
                usage: Value::Null,
                cost_usd: None,
                rate_limit: None,
            },
            None,
        );
        assert!(tracker.pending.is_empty());
    }
}
