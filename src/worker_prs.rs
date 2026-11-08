//! PRs created by delegated workers, attributed to the chat that started them.
//! All transcript and facts I/O runs on a blocking thread. Empty per-delegation
//! marker files under `<config>/worker-pr-scans/` leave existing schemas intact.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::chat_facts;
use crate::merge_plan::PrRef;
use crate::tasks::{
    Delegation, DelegationChild, DelegationStatus, HarnessConversationBackend,
    HarnessConversationRef, TaskRecord,
};

#[derive(Debug, Clone)]
pub struct Scan {
    delegation_id: String,
    parent: Option<String>,
    conversation: Option<HarnessConversationRef>,
}

/// One catch-up per run plus newly finished workers. Failed reads are tried
/// again next launch; the in-memory set keeps errors from repeating each tick.
#[derive(Default)]
pub struct Scans {
    attempted: HashSet<String>,
    pub running: bool,
}

impl Scans {
    pub fn take_due(
        &mut self,
        delegations: &[Delegation],
        tasks: &[TaskRecord],
        now: DateTime<Utc>,
    ) -> Vec<Scan> {
        if self.running {
            return Vec::new();
        }
        let mut scans = Vec::new();
        for delegation in delegations {
            let DelegationChild::TaskSession {
                task_id,
                task_session_id,
                conversation,
                ..
            } = &delegation.child
            else {
                continue;
            };
            if !matches!(
                delegation.status,
                DelegationStatus::Completed
                    | DelegationStatus::Blocked
                    | DelegationStatus::Cancelled
                    | DelegationStatus::Failed { .. }
            ) || self.attempted.contains(&delegation.delegation_id)
            {
                continue;
            }
            let finished = match DateTime::parse_from_rfc3339(&delegation.updated_at) {
                Ok(at) => at.with_timezone(&Utc),
                Err(error) => {
                    eprintln!(
                        "[worker-prs] delegation {}: invalid finish time: {error}",
                        delegation.delegation_id
                    );
                    self.attempted.insert(delegation.delegation_id.clone());
                    continue;
                }
            };
            if finished < now - Duration::days(14) {
                continue;
            }
            // The task session is authoritative: the delegation's copied
            // conversation may predate Codex's recorded startup identity.
            let recorded = tasks
                .iter()
                .find(|task| task.task_id == *task_id)
                .and_then(|task| {
                    task.sessions
                        .iter()
                        .find(|session| Some(&session.task_session_id) == task_session_id.as_ref())
                })
                .and_then(|session| session.conversation.clone())
                .or_else(|| conversation.clone());
            scans.push(Scan {
                delegation_id: delegation.delegation_id.clone(),
                parent: delegation.parent.chat_session_id.clone(),
                conversation: recorded,
            });
            self.attempted.insert(delegation.delegation_id.clone());
        }
        self.running = !scans.is_empty();
        scans
    }
}

pub type RecordedPrs = Vec<(String, Vec<PrRef>)>;

/// Run serially off the UI thread. Only successful scans/skips get a durable
/// marker; unreadable transcripts or facts leave disk state unchanged.
pub fn run(scans: Vec<Scan>, config: &Path, home: &Path) -> RecordedPrs {
    let mut recorded = Vec::new();
    for scan in scans {
        match scan_one(&scan, config, home) {
            Ok(Some(prs)) => recorded.push(prs),
            Ok(None) => {}
            Err(error) => eprintln!("[worker-prs] delegation {}: {error}", scan.delegation_id),
        }
    }
    recorded
}

fn scan_one(
    scan: &Scan,
    config: &Path,
    home: &Path,
) -> Result<Option<(String, Vec<PrRef>)>, String> {
    if scan.delegation_id.is_empty()
        || !scan
            .delegation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid delegation id for scan marker".into());
    }
    let marker = config.join("worker-pr-scans").join(&scan.delegation_id);
    if marker
        .try_exists()
        .map_err(|error| format!("cannot read {}: {error}", marker.display()))?
    {
        return Ok(None);
    }
    let facts_dir = chat_facts::facts_dir(config);
    let has_parent = match &scan.parent {
        Some(parent) => chat_facts::record_worker_prs(&facts_dir, parent, &[])?.is_some(),
        None => false,
    };
    if !has_parent {
        eprintln!(
            "[worker-prs] delegation {}: parent has no saved chat facts; skipping",
            scan.delegation_id
        );
        mark_scanned(&marker)?;
        return Ok(None);
    }
    let conversation = scan
        .conversation
        .as_ref()
        .ok_or("task session has no recorded conversation id")?;
    let transcript = crate::chats::task_conversation_path_under(home, conversation)?;
    let prs = scan_transcript(&transcript, conversation.backend)
        .map_err(|error| format!("cannot read {}: {error}", transcript.display()))?;
    let parent = scan.parent.as_ref().expect("checked parent above");
    let added = chat_facts::record_worker_prs(&facts_dir, parent, &prs)?.ok_or_else(|| {
        format!(
            "parent {parent} facts disappeared during scan of {}",
            transcript.display()
        )
    })?;
    mark_scanned(&marker)?;
    Ok(Some((parent.clone(), added)))
}

fn mark_scanned(marker: &Path) -> Result<(), String> {
    std::fs::create_dir_all(marker.parent().expect("marker directory"))
        .and_then(|()| std::fs::write(marker, b""))
        .map_err(|error| format!("cannot write {}: {error}", marker.display()))
}

/// Adapters retain only actual shell inputs keyed by call id. Both formats
/// use `created_prs`, the live facts recogniser, when their output arrives.
fn scan_transcript(
    path: &Path,
    backend: HarnessConversationBackend,
) -> std::io::Result<Vec<PrRef>> {
    let mut calls: HashMap<String, String> = HashMap::new();
    let mut prs = Vec::new();
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        let value: Value = serde_json::from_str(&line)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        match backend {
            HarnessConversationBackend::Codex => {
                if value["type"] != "response_item" {
                    continue;
                }
                let payload = &value["payload"];
                let Some(id) = payload["call_id"].as_str() else {
                    continue;
                };
                match payload["type"].as_str() {
                    Some("function_call") => {
                        let args: Option<Value> = payload["arguments"]
                            .as_str()
                            .and_then(|args| serde_json::from_str(args).ok());
                        let command =
                            args.as_ref()
                                .and_then(|args| match payload["name"].as_str()? {
                                    "exec_command" => args["cmd"].as_str().map(str::to_owned),
                                    "shell_command" => args["command"].as_str().map(str::to_owned),
                                    "shell" => {
                                        let argv = args["command"].as_array()?;
                                        match argv.as_slice() {
                                            [_, flag, script]
                                                if matches!(flag.as_str(), Some("-lc" | "-c")) =>
                                            {
                                                script.as_str().map(str::to_owned)
                                            }
                                            _ => None,
                                        }
                                    }
                                    _ => None,
                                });
                        if let Some(command) = command {
                            calls.insert(id.to_owned(), command);
                        }
                    }
                    Some("function_call_output") => {
                        if let (Some(command), Some(output)) =
                            (calls.remove(id), payload["output"].as_str())
                        {
                            prs.extend(chat_facts::created_prs(&command, output));
                        }
                    }
                    _ => {}
                }
            }
            HarnessConversationBackend::Claude => {
                let Some(blocks) = value["message"]["content"].as_array() else {
                    continue;
                };
                for block in blocks {
                    match (value["type"].as_str(), block["type"].as_str()) {
                        (Some("assistant"), Some("tool_use")) if block["name"] == "Bash" => {
                            if let (Some(id), Some(command)) =
                                (block["id"].as_str(), block["input"]["command"].as_str())
                            {
                                calls.insert(id.to_owned(), command.to_owned());
                            }
                        }
                        (Some("user"), Some("tool_result")) => {
                            if let Some(command) = block["tool_use_id"]
                                .as_str()
                                .and_then(|id| calls.remove(id))
                            {
                                prs.extend(chat_facts::created_prs(
                                    &command,
                                    &chat_facts::tool_result_text(&block["content"]),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            HarnessConversationBackend::Pi => {}
        }
    }
    Ok(prs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_facts::{facts_dir, facts_path, load_facts, write_facts, ChatFacts};
    use crate::chats::ChatBackend;
    use crate::tasks::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn worker(backend: HarnessConversationBackend) -> Delegation {
        let mut worker = Delegation::new_requested(
            NewDelegation {
                kind: DelegationKind::Implement,
                parent: DelegationParent {
                    session_uid: "tab-parent".into(),
                    chat_session_id: Some("parent-chat".into()),
                    workspace: "workspace-a".into(),
                    cwd: PathBuf::from("/repos/app"),
                },
                child: DelegationChild::TaskSession {
                    task_id: "task-worker".into(),
                    task_session_id: Some("worker-session".into()),
                    conversation: Some(HarnessConversationRef {
                        backend,
                        session_id: "worker-chat".into(),
                    }),
                    preset_name: None,
                    model: None,
                    role: None,
                },
                brief: "Create a docs PR in another repo".into(),
                target: None,
                previous: None,
                options: Default::default(),
            },
            "2026-11-08T10:00:00Z",
        );
        worker.status = DelegationStatus::Completed;
        worker
    }

    fn task(conversation: &str) -> TaskRecord {
        let mut task = TaskRecord::new_draft(
            NewTaskRecord {
                task_id: "task-worker".into(),
                title: "Worker".into(),
                objective: "Docs".into(),
                workspace: WorkspaceIdentity {
                    name: "workspace-a".into(),
                    location: WorkspaceLocationIdentity::Local {
                        directory: PathBuf::from("/repos/app"),
                    },
                },
                repository: RepositoryIdentity {
                    common_dir: PathBuf::from("/repos/app/.git"),
                    remote_url: None,
                },
                issue: None,
                base: GitBase {
                    reference: "v6".into(),
                    commit: "a".repeat(40),
                },
                branch: "task/worker".into(),
                executor: ExecutorTarget::Local,
                harness: None,
                stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            },
            "2026-11-08T10:00:00Z",
        );
        task.sessions.push(TaskSessionRecord {
            task_session_id: "worker-session".into(),
            label: "Codex".into(),
            harness: None,
            conversation: Some(HarnessConversationRef {
                backend: HarnessConversationBackend::Codex,
                session_id: conversation.into(),
            }),
            objective_delivery: Default::default(),
            launch_identity: None,
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
        });
        task
    }

    fn now() -> DateTime<Utc> {
        "2026-11-08T11:00:00Z".parse().unwrap()
    }

    fn parent(config: &Path) -> PathBuf {
        let path = facts_path(&facts_dir(config), ChatBackend::Claude, "parent-chat").unwrap();
        let mut facts = ChatFacts::default();
        facts.add_pr(
            crate::merge_plan::pr_urls_in("https://github.com/example/app/pull/10").remove(0),
        );
        facts.add_issue_key("TEST-42");
        write_facts(&path, &facts).unwrap();
        path
    }

    fn transcript(home: &Path, backend: HarnessConversationBackend, lines: &[Value]) -> PathBuf {
        let relative = match backend {
            HarnessConversationBackend::Codex => {
                ".codex/sessions/2026/11/08/rollout-2026-11-08T10-00-00-worker-chat.jsonl"
            }
            _ => ".claude/projects/repo-slug/worker-chat.jsonl",
        };
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        path
    }

    fn call(id: &str, command: &str) -> Value {
        json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":id,"arguments":json!({"cmd":command}).to_string()}})
    }

    fn output(id: &str, text: &str) -> Value {
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":id,"output":text}})
    }

    #[test]
    fn codex_worker_records_other_repo_pr_on_parent_and_is_scanned_once() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let path = parent(&config);
        let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let source = transcript(temp.path(), HarnessConversationBackend::Codex, &[
            call("create", "cd /repos/docs && gh pr create --head docs/update --title Docs --body Update"),
            output("unrelated", "https://github.com/example/unrelated/pull/99"),
            output("create", "https://github.com/example/docs/pull/20\nhttps://github.com/example/docs/pull/20"),
        ]);
        let mut worker = worker(HarnessConversationBackend::Codex);
        // The task session's latest identity takes precedence over the copy
        // attached to the delegation when the worker originally launched.
        if let DelegationChild::TaskSession { conversation, .. } = &mut worker.child {
            conversation.as_mut().unwrap().session_id = "stale-chat".into();
        }
        let tasks = [task("worker-chat")];
        let mut scans = Scans::default();
        let due = scans.take_due(std::slice::from_ref(&worker), &tasks, now());
        let result = run(due, &config, temp.path());
        assert_eq!(result[0].0, "parent-chat");
        assert_eq!(result[0].1.len(), 1);
        let facts = load_facts(&path).unwrap().unwrap();
        assert_eq!(facts.prs.len(), 2);
        assert_eq!(facts.prs[1].key().id(), "example/docs#20");
        assert_eq!(facts.issue_keys, ["TEST-42"]);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime
        );
        assert!(config
            .join("worker-pr-scans")
            .join(&worker.delegation_id)
            .is_file());
        scans.running = false;
        assert!(scans
            .take_due(std::slice::from_ref(&worker), &tasks, now())
            .is_empty());
        // Even across launches, no transcript read (invalid input would fail
        // parsing) and no write of the marker or facts occurs on a second pass.
        std::fs::write(&source, "invalid json").unwrap();
        let marker = config.join("worker-pr-scans").join(&worker.delegation_id);
        let marked_at = std::fs::metadata(&marker).unwrap().modified().unwrap();
        let due = Scans::default().take_due(&[worker], &tasks, now());
        assert_eq!(due.len(), 1);
        // Unlike `run`, this propagates a parsing error, so the assertion
        // fails if the invalid transcript is ever read again.
        assert!(scan_one(&due[0], &config, temp.path()).unwrap().is_none());
        assert_eq!(
            std::fs::metadata(marker).unwrap().modified().unwrap(),
            marked_at
        );
        assert_eq!(load_facts(&path).unwrap().unwrap(), facts);
    }

    #[test]
    fn codex_url_mentions_grep_echo_and_unpaired_output_are_not_creates() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let facts_path = parent(&config);
        let before = std::fs::read(&facts_path).unwrap();
        let path = transcript(
            temp.path(),
            HarnessConversationBackend::Codex,
            &[
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"https://github.com/example/docs/pull/20"}]}}),
                call("grep", "rg 'pull/20' docs"),
                output("grep", "https://github.com/example/docs/pull/20"),
                call("echo", "echo https://github.com/example/docs/pull/20"),
                output("echo", "https://github.com/example/docs/pull/20"),
                output("unknown", "https://github.com/example/docs/pull/20"),
                call("unfinished", "gh pr create --title Docs --body Update"),
            ],
        );
        assert!(scan_transcript(&path, HarnessConversationBackend::Codex)
            .unwrap()
            .is_empty());
        let worker = worker(HarnessConversationBackend::Codex);
        let due = Scans::default().take_due(&[worker], &[], now());
        let result = run(due, &config, temp.path());
        assert!(result[0].1.is_empty());
        assert_eq!(std::fs::read(facts_path).unwrap(), before);
    }

    #[test]
    fn claude_worker_transcript_records_real_create_results() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let path = parent(&config);
        transcript(
            temp.path(),
            HarnessConversationBackend::Claude,
            &[
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"create","input":{"command":"cd /repos/docs && gh pr create --title Docs --body Update"}}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"create","content":[{"type":"text","text":"https://github.com/example/docs/pull/20"}]}]}}),
            ],
        );
        let worker = worker(HarnessConversationBackend::Claude);
        let due = Scans::default().take_due(&[worker], &[], now());
        assert_eq!(run(due, &config, temp.path())[0].1.len(), 1);
        assert_eq!(load_facts(&path).unwrap().unwrap().prs.len(), 2);
    }

    #[test]
    fn terminal_outcomes_only_within_fourteen_days_are_eligible() {
        let mut worker = worker(HarnessConversationBackend::Codex);
        for status in [
            DelegationStatus::Completed,
            DelegationStatus::Blocked,
            DelegationStatus::Cancelled,
            DelegationStatus::Failed {
                message: "exited".into(),
            },
        ] {
            worker.status = status;
            assert_eq!(
                Scans::default()
                    .take_due(std::slice::from_ref(&worker), &[], now())
                    .len(),
                1
            );
        }
        worker.updated_at = "2026-10-24T10:00:00Z".into();
        assert!(Scans::default()
            .take_due(std::slice::from_ref(&worker), &[], now())
            .is_empty());
        worker.updated_at = "2026-11-08T10:00:00Z".into();
        for status in [
            DelegationStatus::Running,
            DelegationStatus::Requested,
            DelegationStatus::Interrupted,
        ] {
            worker.status = status;
            assert!(Scans::default()
                .take_due(std::slice::from_ref(&worker), &[], now())
                .is_empty());
        }
    }

    #[test]
    fn read_errors_leave_facts_and_marker_unchanged_and_retry_next_run() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let path = parent(&config);
        let before = std::fs::read(&path).unwrap();
        let source = transcript(
            temp.path(),
            HarnessConversationBackend::Codex,
            &[
                call("create", "gh pr create"),
                output("create", "https://github.com/example/docs/pull/20"),
            ],
        );
        // Fail after reading a valid create, ensuring partial results do not
        // make it to disk.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"\ninvalid\n")
            .unwrap();
        let worker = worker(HarnessConversationBackend::Codex);
        let mut scans = Scans::default();
        let due = scans.take_due(std::slice::from_ref(&worker), &[], now());
        assert!(run(due, &config, temp.path()).is_empty());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(!config
            .join("worker-pr-scans")
            .join(&worker.delegation_id)
            .exists());
        scans.running = false;
        assert!(scans
            .take_due(std::slice::from_ref(&worker), &[], now())
            .is_empty());
        assert_eq!(Scans::default().take_due(&[worker], &[], now()).len(), 1);
    }

    #[test]
    fn parent_without_facts_is_logged_and_skipped_once_without_reading_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let worker = worker(HarnessConversationBackend::Codex);
        let due = Scans::default().take_due(std::slice::from_ref(&worker), &[], now());
        assert!(run(due, &config, temp.path()).is_empty());
        assert!(config
            .join("worker-pr-scans")
            .join(&worker.delegation_id)
            .exists());
        assert!(!facts_dir(&config).exists());
        let due = Scans::default().take_due(&[worker], &[], now());
        assert!(run(due, &config, temp.path()).is_empty());
    }
}
