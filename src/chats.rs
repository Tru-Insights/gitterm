//! Host facade for the read-only core chat index and previews.
//! Resume commands and task startup discovery remain host-owned.

pub use gitterm_core::chats::*;
pub(crate) use gitterm_core::find::{codex_message, codex_meta_is_subagent, resolve_repo_root};
pub use gitterm_core::find::{load_preview, load_snippet_preview, SNIPPET_TAIL_BYTES};
pub(crate) use gitterm_core::plan_limits::jsonl_files_under;

use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// Host-owned resume command construction for a chat backend.
pub trait ChatBackendResume {
    fn resume_command(&self, base: &str, id: &str) -> String;
}

impl ChatBackendResume for ChatBackend {
    /// Shell command that resumes conversation `id` using harness
    /// launch command `base`. Must run in the conversation's recorded
    /// cwd.
    fn resume_command(&self, base: &str, id: &str) -> String {
        match self {
            ChatBackend::Claude => format!("{base} --resume {id}"),
            ChatBackend::Codex => format!("{base} resume {id}"),
            // pi resumes a specific session via --session (`--resume`
            // is the interactive picker).
            ChatBackend::Pi => format!("{base} --session {id}"),
        }
    }
}

/// Host-owned resume command construction for an indexed conversation.
pub trait ChatEntryResume {
    fn resume_command(&self) -> String;
    fn resume_command_with(&self, base: &str) -> String;
}

impl ChatEntryResume for ChatIndexEntry {
    /// Shell command that resumes this conversation. Must run in the
    /// conversation's recorded cwd (or the rescue dir when cwd is dead).
    fn resume_command(&self) -> String {
        self.resume_command_with(self.backend.label())
    }
    /// Resume command with an explicit harness binary/launch command —
    /// remote machines resolve the binary through the remote's
    /// session_commands map (agent daemons run with a minimal PATH).
    fn resume_command_with(&self, base: &str) -> String {
        self.backend.resume_command(base, &self.id)
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Claude Code's home dir. Deliberately independent of GitTerm's config
/// dir override: the index is read-only over harness files and should
/// see the real ones on whichever machine this code runs (desktop or
/// agentd on a remote host).
fn claude_home_dir() -> PathBuf {
    home_dir().join(".claude")
}

/// Whether a Claude session transcript exists on this machine. Claude
/// Code writes `<projects>/<cwd-slug>/<session-id>.jsonl` on the first
/// message, so a preassigned session id may never materialize on disk.
pub fn claude_session_exists(session_id: &str) -> bool {
    claude_session_path(session_id).is_some()
}

/// The transcript file for a Claude session id, if it exists on this
/// machine. Looks across project slugs because the id alone does not say
/// which cwd the session ran in.
pub fn claude_session_path(session_id: &str) -> Option<PathBuf> {
    claude_session_path_under(&claude_home_dir().join("projects"), session_id)
}

fn claude_session_path_under(projects: &Path, session_id: &str) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&file_name))
        .find(|path| path.is_file())
}

/// Find a worker transcript by its recorded harness id, without reading
/// unrelated conversations. Blocking; `home` may be a fixture home.
pub(crate) fn task_conversation_path_under(
    home: &Path,
    conversation: &crate::tasks::HarnessConversationRef,
) -> Result<PathBuf, String> {
    use crate::tasks::HarnessConversationBackend as Backend;
    let id = &conversation.session_id;
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("invalid worker conversation id {id:?}"));
    }
    let (root, depth) = match conversation.backend {
        Backend::Claude => (home.join(".claude/projects"), 1),
        Backend::Codex => (home.join(".codex/sessions"), 3),
        Backend::Pi => return Err("worker PR scans do not support Pi transcripts".into()),
    };
    let expected = format!("{id}.jsonl");
    let suffix = format!("-{expected}");
    jsonl_files_under(&root, depth)
        .into_iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| match conversation.backend {
                    Backend::Codex => name.starts_with("rollout-") && name.ends_with(&suffix),
                    _ => name == expected,
                })
        })
        .ok_or_else(|| format!("no transcript for {id} under {}", root.display()))
}

/// Build the local index using the host's historical harness home.
pub fn build_local_index() -> Vec<ChatIndexEntry> {
    gitterm_core::find::build_local_index(&home_dir())
}

/// Look up subagent rollouts using the host's historical Codex sessions root.
pub fn codex_subagent_conversations(ids: &HashSet<String>) -> HashSet<String> {
    gitterm_core::find::codex_subagent_conversations_under(
        &home_dir().join(".codex").join("sessions"),
        ids,
    )
}

/// Launch-specific marker in the initial prompt binds a Codex file to its
/// GitTerm session even when several workers share the same checkout.
pub fn task_session_marker(session_id: &str) -> String {
    format!("GitTerm task session: {session_id}")
}

/// Probe the local-day rollout files around launch time. This is independent
/// of the Chats view/index and must run off the UI thread.
pub fn started_task_conversation(
    cwd: &Path,
    session: &crate::tasks::TaskSessionRecord,
    backend: crate::tasks::HarnessConversationBackend,
) -> Result<Option<crate::tasks::HarnessConversationRef>, String> {
    let home = dirs::home_dir()
        .ok_or_else(|| "Cannot locate home to verify worker startup".to_string())?;
    started_task_conversation_under(&home, cwd, session, backend)
}

fn started_task_conversation_under(
    home: &Path,
    cwd: &Path,
    session: &crate::tasks::TaskSessionRecord,
    backend: crate::tasks::HarnessConversationBackend,
) -> Result<Option<crate::tasks::HarnessConversationRef>, String> {
    started_task_conversation_under_local_date(
        home,
        cwd,
        session,
        backend,
        chrono::Local::now().date_naive(),
    )
}

fn started_task_conversation_under_local_date(
    home: &Path,
    cwd: &Path,
    session: &crate::tasks::TaskSessionRecord,
    backend: crate::tasks::HarnessConversationBackend,
    local_date: chrono::NaiveDate,
) -> Result<Option<crate::tasks::HarnessConversationRef>, String> {
    use crate::tasks::{HarnessConversationBackend as Backend, HarnessConversationRef};
    if let Some(conversation) = &session.conversation {
        if conversation.backend == Backend::Claude {
            return Ok(claude_session_path_under(
                &home.join(".claude/projects"),
                &conversation.session_id,
            )
            .map(|_| conversation.clone()));
        }
    }
    if backend != Backend::Codex {
        return Ok(None);
    }
    let started = chrono::DateTime::parse_from_rfc3339(&session.created_at).map_err(|error| {
        format!(
            "session {} has invalid start time: {error}",
            session.task_session_id
        )
    })?;
    let root = home.join(".codex/sessions");
    let dates = [
        local_date,
        local_date - chrono::Duration::days(1),
        local_date + chrono::Duration::days(1),
    ];
    for date in dates {
        let day = root.join(date.format("%Y/%m/%d").to_string());
        let files = match std::fs::read_dir(&day) {
            Ok(files) => files,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read startup sessions {}: {error}", day.display())),
        };
        for entry in files {
            let path = entry
                .map_err(|error| {
                    format!("read startup session entry in {}: {error}", day.display())
                })?
                .path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(id) = started_codex_session(&path, cwd, &session.task_session_id, started)?
            {
                return Ok(Some(HarnessConversationRef {
                    backend: Backend::Codex,
                    session_id: id,
                }));
            }
        }
    }
    Ok(None)
}

fn started_codex_session(
    path: &Path,
    cwd: &Path,
    task_session_id: &str,
    started: chrono::DateTime<chrono::FixedOffset>,
) -> Result<Option<String>, String> {
    // shortcut: bounded initial rollout scan; increase if launch instructions
    // exceed 2 MiB before the initial prompt is recorded.
    let file = std::fs::File::open(path)
        .map_err(|error| format!("read startup session {}: {error}", path.display()))?;
    let reader = BufReader::new(file.take(2 * 1024 * 1024));
    let marker = task_session_marker(task_session_id);
    let mut id = None;
    for line in reader.lines() {
        let line =
            line.map_err(|error| format!("read startup session {}: {error}", path.display()))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            let Some(payload) = value.get("payload") else {
                return Ok(None);
            };
            let stamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok());
            if codex_meta_is_subagent(payload)
                || payload
                    .get("originator")
                    .and_then(Value::as_str)
                    .is_some_and(|origin| origin.contains("exec"))
                || payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    != std::fs::canonicalize(cwd).ok()
                || stamp.is_none_or(|stamp| stamp < started)
            {
                return Ok(None);
            }
            id = payload
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if let Some((true, text)) = codex_message(&value) {
            if text.lines().any(|line| line == marker) {
                return Ok(id);
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod task_startup_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_startup_waits_for_its_exact_preassigned_transcript() {
        use crate::tasks::*;
        let temp = tempfile::tempdir().unwrap();
        let conversation = HarnessConversationRef {
            backend: HarnessConversationBackend::Claude,
            session_id: "fixture-id".into(),
        };
        let session = TaskSessionRecord {
            task_session_id: "worker".into(),
            label: "Claude".into(),
            harness: None,
            conversation: Some(conversation.clone()),
            objective_delivery: ObjectiveDeliveryState::Pending,
            launch_identity: None,
            created_at: "2026-11-05T10:00:00Z".into(),
            updated_at: "2026-11-05T10:00:00Z".into(),
        };
        let probe = || {
            started_task_conversation_under(
                temp.path(),
                temp.path(),
                &session,
                HarnessConversationBackend::Claude,
            )
            .unwrap()
        };
        assert!(probe().is_none());
        assert!(!crate::workers::startup_timed_out(29, probe().is_some()));
        assert!(crate::workers::startup_timed_out(30, probe().is_some()));
        let slug = temp.path().join(".claude/projects/fixture");
        std::fs::create_dir_all(&slug).unwrap();
        std::fs::write(slug.join("other-id.jsonl"), "{}").unwrap();
        assert!(probe().is_none());
        std::fs::write(slug.join("fixture-id.jsonl"), "{}").unwrap();
        assert_eq!(probe(), Some(conversation));
        assert!(!crate::workers::startup_timed_out(30, probe().is_some()));
    }

    #[test]
    fn codex_startup_searches_the_local_date_when_it_differs_from_utc() {
        use crate::tasks::*;
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let session = TaskSessionRecord {
            task_session_id: "worker-local-day".into(),
            label: "Codex".into(),
            harness: None,
            conversation: None,
            objective_delivery: ObjectiveDeliveryState::Pending,
            launch_identity: None,
            created_at: "2026-11-05T23:59:00Z".into(),
            updated_at: "2026-11-05T23:59:00Z".into(),
        };
        let utc_date = chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let local_date = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        assert_ne!(utc_date, local_date);
        let day = temp.path().join(".codex/sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        let meta = json!({"type":"session_meta", "timestamp":"2026-11-06T00:00:01Z", "payload":{
            "id":"conversation-local-day", "cwd":cwd, "source":"cli", "originator":"codex_cli_rs"}});
        let prompt = json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{
            "type":"input_text", "text":format!("Synthetic initial objective\n{}", task_session_marker("worker-local-day"))}]}});
        std::fs::write(
            day.join("rollout-local-day.jsonl"),
            format!("{meta}\n{prompt}\n"),
        )
        .unwrap();

        assert_eq!(
            started_task_conversation_under_local_date(
                temp.path(),
                &cwd,
                &session,
                HarnessConversationBackend::Codex,
                local_date,
            )
            .unwrap(),
            Some(HarnessConversationRef {
                backend: HarnessConversationBackend::Codex,
                session_id: "conversation-local-day".into(),
            })
        );
    }

    #[test]
    fn startup_file_requires_fresh_cwd_and_unique_initial_prompt_marker() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout.jsonl");
        let cwd = temp.path();
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-05T10:00:01Z").unwrap();
        // No file/session cannot count as started, even after the deadline.
        assert!(started_codex_session(&path, cwd, "worker-a", start).is_err());
        assert!(crate::workers::startup_timed_out(30, false));
        let meta = json!({"type":"session_meta", "timestamp":"2026-11-05T10:00:02Z", "payload":{
            "id":"conversation-a", "cwd":cwd, "source":"cli", "originator":"codex_cli_rs"}});
        let prompt = json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{
            "type":"input_text", "text":format!("Synthetic initial objective\n{}", task_session_marker("worker-a"))}]}});
        let write = |meta: &Value, prompt: &Value| {
            std::fs::write(&path, format!("{meta}\n{prompt}\n")).unwrap()
        };
        write(&meta, &prompt);
        let found = started_codex_session(&path, cwd, "worker-a", start).unwrap();
        assert_eq!(found.as_deref(), Some("conversation-a"));
        assert!(!crate::workers::startup_timed_out(30, found.is_some()));
        // Concurrent workers in one checkout must not claim one another.
        assert!(started_codex_session(&path, cwd, "worker-b", start)
            .unwrap()
            .is_none());
        assert!(
            started_codex_session(&path, Path::new("/other-checkout"), "worker-a", start)
                .unwrap()
                .is_none()
        );
        let mut old = meta.clone();
        old["timestamp"] = json!("2026-11-05T09:59:00Z");
        write(&old, &prompt);
        assert!(started_codex_session(&path, cwd, "worker-a", start)
            .unwrap()
            .is_none());
        let mut subagent = meta.clone();
        subagent["payload"]["source"] = json!({"subagent":"guardian"});
        write(&subagent, &prompt);
        assert!(started_codex_session(&path, cwd, "worker-a", start)
            .unwrap()
            .is_none());
        let mut assistant = prompt.clone();
        assistant["payload"]["role"] = json!("assistant");
        write(&meta, &assistant);
        assert!(started_codex_session(&path, cwd, "worker-a", start)
            .unwrap()
            .is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "scans the real ~/.claude of this machine; run manually with --ignored"]
    fn real_index_smoke() {
        let started = std::time::Instant::now();
        let entries = build_local_index();
        eprintln!("indexed {} chats in {:?}", entries.len(), started.elapsed());
        for entry in entries.iter().take(12) {
            eprintln!(
                "  {} | {:8} | {:24} | wt={} dead={} | {}",
                format_age(entry.mtime),
                entry.branch.as_deref().unwrap_or("-"),
                entry.group_name(),
                entry.is_worktree,
                entry.dead_cwd,
                entry.title,
            );
        }
    }

    #[test]
    fn resume_commands_remain_host_owned() {
        for (backend, id, expected) in [
            (ChatBackend::Claude, "fixture", "claude --resume fixture"),
            (ChatBackend::Codex, "019e-abc", "codex resume 019e-abc"),
            (ChatBackend::Pi, "019f-xyz", "pi --session 019f-xyz"),
        ] {
            let entry = ChatIndexEntry {
                id: id.into(),
                backend,
                path: PathBuf::new(),
                cwd: PathBuf::new(),
                repo_root: None,
                is_worktree: false,
                branch: None,
                title: String::new(),
                latest: None,
                mtime: std::time::UNIX_EPOCH,
                size: 0,
                dead_cwd: true,
                facts: None,
                root_id: None,
            };
            assert_eq!(entry.resume_command(), expected);
            assert_eq!(
                entry.resume_command_with("custom"),
                backend.resume_command("custom", id)
            );
        }
    }
}
