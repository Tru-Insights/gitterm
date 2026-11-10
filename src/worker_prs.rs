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
        uid_to_chat: &HashMap<String, String>,
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
                parent: delegation
                    .parent
                    .chat_session_id
                    .clone()
                    .or_else(|| uid_to_chat.get(&delegation.parent.session_uid).cloned()),
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

/// Adapters pair actual shell inputs with their output, including Codex's
/// completed command events. All formats use the live facts recogniser.
fn scan_transcript(
    path: &Path,
    backend: HarnessConversationBackend,
) -> std::io::Result<Vec<PrRef>> {
    let mut prs = scan_transcript_file(path, backend)?;
    if backend != HarnessConversationBackend::Claude {
        return Ok(prs);
    }
    let children = path.with_extension("").join("subagents");
    let read = match std::fs::read_dir(&children) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(prs),
        Err(error) => return Err(error),
    };
    let mut files = read
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    files.retain(|file| file.extension().and_then(|ext| ext.to_str()) == Some("jsonl"));
    files.sort();
    let mut seen: HashSet<_> = prs.iter().map(|pr| pr.key().id()).collect();
    for file in files {
        let child_prs = scan_transcript_file(&file, backend).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", file.display()),
            )
        })?;
        for pr in child_prs {
            if seen.insert(pr.key().id()) {
                prs.push(pr);
            }
        }
    }
    Ok(prs)
}

fn scan_transcript_file(
    path: &Path,
    backend: HarnessConversationBackend,
) -> std::io::Result<Vec<PrRef>> {
    let mut calls: HashMap<String, String> = HashMap::new();
    let mut prs = Vec::new();
    let mut seen = HashSet::new();
    let mut record_prs = |created: Vec<PrRef>| {
        for pr in created {
            if seen.insert(pr.key().id()) {
                prs.push(pr);
            }
        }
    };
    let mut invalid_lines = 0;
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                invalid_lines += 1;
                continue;
            }
        };
        match backend {
            HarnessConversationBackend::Codex => {
                let payload = &value["payload"];
                if value["type"] == "event_msg" && payload["type"] == "item_completed" {
                    let item = &payload["item"];
                    if item["type"] == "CommandExecution" && item["exit_code"].as_i64() == Some(0) {
                        if let (Some(argv), Some(output)) = (
                            item["command"].as_array(),
                            item["aggregated_output"].as_str(),
                        ) {
                            if argv_creates_pr(argv) {
                                record_prs(crate::merge_plan::pr_urls_in(output));
                            }
                        }
                    }
                }
                if value["type"] != "response_item" {
                    continue;
                }
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
                                        shell_script(argv).map(str::to_owned)
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
                            if codex_output_succeeded(output) {
                                record_prs(chat_facts::created_prs(&command, output));
                            }
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
                                if block["is_error"].as_bool() != Some(true) {
                                    record_prs(chat_facts::created_prs(
                                        &command,
                                        &chat_facts::tool_result_text(&block["content"]),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            HarnessConversationBackend::Pi => {}
        }
    }
    if invalid_lines > 0 {
        eprintln!(
            "[worker-prs] {}: skipped {invalid_lines} invalid JSON lines",
            path.display()
        );
    }
    Ok(prs)
}

/// Only a shell's command-string argument is shell syntax. Other argv
/// vectors must name gh and its subcommands as separate literal arguments.
fn argv_creates_pr(argv: &[Value]) -> bool {
    if let Some(script) = shell_script(argv) {
        return chat_facts::is_gh_pr_create(script);
    }
    argv.first()
        .and_then(Value::as_str)
        .and_then(|exe| exe.rsplit('/').next())
        == Some("gh")
        && argv.get(1).and_then(Value::as_str) == Some("pr")
        && argv.get(2).and_then(Value::as_str) == Some("create")
}

fn shell_script(argv: &[Value]) -> Option<&str> {
    let executable = argv.first()?.as_str()?.rsplit('/').next()?;
    if matches!(executable, "bash" | "sh" | "zsh" | "dash" | "ksh")
        && matches!(argv.get(1)?.as_str()?, "-c" | "-lc" | "-cl")
    {
        argv.get(2)?.as_str()
    } else {
        None
    }
}

/// Shell outputs use JSON metadata in older Codex versions and status headers
/// in newer ones. Plain legacy outputs have no reported exit code.
fn codex_output_succeeded(output: &str) -> bool {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        if let Some(code) = value
            .get("exit_code")
            .or_else(|| value["metadata"].get("exit_code"))
        {
            return code.as_i64() == Some(0);
        }
    }
    output
        .lines()
        .take_while(|line| !matches!(*line, "Output:" | "Final output:"))
        .all(|line| {
            if line.starts_with("Process running with session ID ") {
                return false;
            }
            line.strip_prefix("Process exited with code ")
                .or_else(|| line.strip_prefix("Exit code: "))
                .is_none_or(|code| code.trim().parse::<i64>() == Ok(0))
        })
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

    const CREATE_SCRIPT: &str = r#"python3 - <<'PY'
from pathlib import Path
Path('/tmp/example-pr-body.md').write_text('Update documentation.\n')
PY
GH_TOKEN="$(gh auth token --user example-user)" gh pr create --draft --base develop --title "Update docs" --body-file /tmp/example-pr-body.md
"#;

    fn command_execution(exit_code: Value) -> Value {
        json!({
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "CommandExecution",
                    "command": ["/bin/zsh", "-lc", CREATE_SCRIPT],
                    "aggregated_output": "Creating draft pull request\nhttps://github.com/example/docs/pull/181\n",
                    "exit_code": exit_code,
                    "status": "completed"
                }
            }
        })
    }

    #[test]
    fn codex_command_execution_records_code_mode_create_and_dedupes_function_output() {
        let temp = tempfile::tempdir().unwrap();
        let mut lines = vec![
            json!({"type":"response_item","payload":{
                "type":"custom_tool_call", "name":"exec", "call_id":"code-mode",
                "input": format!("await tools.exec_command({{cmd: {}}});", json!(CREATE_SCRIPT))
            }}),
            command_execution(json!(0)),
            command_execution(json!(0)),
        ];
        let path = transcript(temp.path(), HarnessConversationBackend::Codex, &lines);
        let prs = scan_transcript(&path, HarnessConversationBackend::Codex).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].key().id(), "example/docs#181");

        lines.extend([
            call("duplicate", CREATE_SCRIPT),
            output("duplicate", "https://github.com/Example/Docs/pull/181"),
            call("other", "gh pr create --title Other --body Update"),
            output("other", "https://github.com/example/app/pull/182"),
        ]);
        transcript(temp.path(), HarnessConversationBackend::Codex, &lines);
        let prs = scan_transcript(&path, HarnessConversationBackend::Codex).unwrap();
        assert_eq!(
            prs.iter().map(|pr| pr.key().id()).collect::<Vec<_>>(),
            ["example/docs#181", "example/app#182"]
        );
    }

    #[test]
    fn review_regression_codex_command_argv_only_interprets_shell_scripts() {
        let temp = tempfile::tempdir().unwrap();
        for (argv, expected) in [
            (
                json!([
                    "printf",
                    "%s\n",
                    "gh pr create https://github.com/fake/repo/pull/42"
                ]),
                0,
            ),
            (json!(["printf", "-c", "gh pr create"]), 0),
            (json!(["/opt/homebrew/bin/gh", "pr", "create", "--fill"]), 1),
            (json!(["gh", "pr create"]), 0),
            (
                json!(["/bin/bash", "-c", "gh pr create --fill", "argv0"]),
                1,
            ),
            (json!(["sh", "-lc", "gh pr create --fill"]), 1),
        ] {
            let mut event = command_execution(json!(0));
            event["payload"]["item"]["command"] = argv.clone();
            let path = transcript(temp.path(), HarnessConversationBackend::Codex, &[event]);
            assert_eq!(
                scan_transcript(&path, HarnessConversationBackend::Codex)
                    .unwrap()
                    .len(),
                expected,
                "{argv}"
            );
        }
    }

    #[test]
    fn review_regression_claude_worker_scans_child_transcripts_before_marking_complete() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let facts_path = parent(&config);
        let root = transcript(temp.path(), HarnessConversationBackend::Claude, &[]);
        let children = root.with_extension("").join("subagents");
        std::fs::create_dir_all(&children).unwrap();
        let child = children.join("agent-create.jsonl");
        let lines = [
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"create","input":{"command":"gh pr create --fill"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"create","content":"https://github.com/example/docs/pull/20"}]}}),
        ];
        let worker = worker(HarnessConversationBackend::Claude);
        let marker = config.join("worker-pr-scans").join(&worker.delegation_id);
        let due = || {
            Scans::default().take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now())
        };
        std::fs::write(&child, b"\xff").unwrap();
        assert!(scan_one(&due()[0], &config, temp.path()).is_err());
        assert!(!marker.exists());
        std::fs::write(
            &child,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        assert_eq!(run(due(), &config, temp.path())[0].1.len(), 1);
        let facts = load_facts(&facts_path).unwrap().unwrap();
        assert_eq!(facts.prs[1].key().id(), "example/docs#20");
        assert_eq!(facts.live_created_prs, ["example/docs#20"]);
        assert!(marker.is_file());
    }

    #[test]
    fn codex_command_execution_requires_successful_exit() {
        let temp = tempfile::tempdir().unwrap();
        let mut missing_exit = command_execution(json!(0));
        missing_exit["payload"]["item"]
            .as_object_mut()
            .unwrap()
            .remove("exit_code");
        let path = transcript(
            temp.path(),
            HarnessConversationBackend::Codex,
            &[
                command_execution(json!(1)),
                command_execution(Value::Null),
                missing_exit,
            ],
        );
        assert!(scan_transcript(&path, HarnessConversationBackend::Codex)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn worker_creates_require_an_executable_command_in_both_adapters() {
        let temp = tempfile::tempdir().unwrap();
        for backend in [
            HarnessConversationBackend::Codex,
            HarnessConversationBackend::Claude,
        ] {
            let mut lines = Vec::new();
            for (i, command) in [
                "echo gh pr create https://github.com/example/docs/pull/20",
                "printf '%s' 'gh pr create'",
                "'gh pr create'",
                "cat <<'EOF'\ngh pr create\nEOF",
                "# gh pr create\necho output",
            ]
            .iter()
            .enumerate()
            {
                let id = format!("mention-{i}");
                match backend {
                    HarnessConversationBackend::Codex => {
                        lines.extend([call(&id, command), output(&id, "https://github.com/example/docs/pull/20")]);
                        let mut event = command_execution(json!(0));
                        event["payload"]["item"]["command"] = json!(["/bin/zsh", "-lc", command]);
                        lines.push(event);
                    }
                    HarnessConversationBackend::Claude => lines.extend([
                        json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":id,"input":{"command":command}}]}}),
                        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":id,"content":"https://github.com/example/docs/pull/20"}]}}),
                    ]),
                    HarnessConversationBackend::Pi => unreachable!(),
                }
            }
            let path = transcript(temp.path(), backend, &lines);
            assert!(
                scan_transcript(&path, backend).unwrap().is_empty(),
                "{backend:?}"
            );
        }
    }

    #[test]
    fn codex_function_outputs_require_success_when_an_exit_code_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let url = "https://github.com/example/docs/pull/20";
        for text in [
            format!("Process exited with code 1\nFinal output:\n{url}"),
            format!("Exit code: 2\nOutput:\n{url}"),
            format!("Process running with session ID 123\nOutput:\n{url}"),
            json!({"exit_code":1,"output":url}).to_string(),
            json!({"metadata":{"exit_code":1},"output":url}).to_string(),
            json!({"metadata":{"exit_code":null},"output":url}).to_string(),
        ] {
            let path = transcript(
                temp.path(),
                HarnessConversationBackend::Codex,
                &[call("create", "gh pr create"), output("create", &text)],
            );
            assert!(
                scan_transcript(&path, HarnessConversationBackend::Codex)
                    .unwrap()
                    .is_empty(),
                "{text}"
            );
        }
        for text in [
            format!("Process exited with code 0\nFinal output:\n{url}"),
            format!("Exit code: 0\nOutput:\n{url}\nExit code: 1"),
            json!({"exit_code":0,"output":url}).to_string(),
            json!({"metadata":{"exit_code":0},"output":url}).to_string(),
        ] {
            let path = transcript(
                temp.path(),
                HarnessConversationBackend::Codex,
                &[call("create", "gh pr create"), output("create", &text)],
            );
            assert_eq!(
                scan_transcript(&path, HarnessConversationBackend::Codex)
                    .unwrap()
                    .len(),
                1,
                "{text}"
            );
        }
    }

    #[test]
    fn claude_error_results_do_not_record_prs() {
        let temp = tempfile::tempdir().unwrap();
        let path = transcript(
            temp.path(),
            HarnessConversationBackend::Claude,
            &[
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"create","input":{"command":"gh pr create"}}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"create","is_error":true,"content":"https://github.com/example/docs/pull/20"}]}}),
            ],
        );
        assert!(scan_transcript(&path, HarnessConversationBackend::Claude)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn blocked_worker_remains_eligible_after_completing_in_the_same_process() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let facts = parent(&config);
        let mut worker = worker(HarnessConversationBackend::Codex);
        worker.status = DelegationStatus::Blocked;
        transcript(
            temp.path(),
            HarnessConversationBackend::Codex,
            &[
                call("create", "gh pr create"),
                output("create", "https://github.com/example/docs/pull/20"),
            ],
        );
        let mut scans = Scans::default();
        let due = scans.take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now());
        run(due, &config, temp.path());
        assert!(!scans.attempted.contains(&worker.delegation_id));
        assert!(!config
            .join("worker-pr-scans")
            .join(&worker.delegation_id)
            .exists());
        scans.running = false;
        worker.status = DelegationStatus::Completed;
        let due = scans.take_due(&[worker], &[], &HashMap::new(), now());
        assert_eq!(run(due, &config, temp.path())[0].1.len(), 1);
        assert_eq!(load_facts(&facts).unwrap().unwrap().prs.len(), 2);
    }

    #[test]
    fn worker_parent_falls_back_to_tab_identity_and_explicit_chat_wins() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let facts = parent(&config);
        transcript(
            temp.path(),
            HarnessConversationBackend::Codex,
            &[
                call("create", "gh pr create"),
                output("create", "https://github.com/example/docs/pull/20"),
            ],
        );
        let mut worker = worker(HarnessConversationBackend::Codex);
        let uid_to_chat = HashMap::from([("tab-parent".into(), "parent-chat".into())]);
        worker.parent.chat_session_id = None;
        let due =
            Scans::default().take_due(std::slice::from_ref(&worker), &[], &uid_to_chat, now());
        assert_eq!(due[0].parent.as_deref(), Some("parent-chat"));
        assert_eq!(run(due, &config, temp.path())[0].1.len(), 1);
        assert_eq!(load_facts(&facts).unwrap().unwrap().prs.len(), 2);
        worker.parent.chat_session_id = Some("explicit-chat".into());
        let due = Scans::default().take_due(&[worker], &[], &uid_to_chat, now());
        assert_eq!(due[0].parent.as_deref(), Some("explicit-chat"));
    }

    #[test]
    fn malformed_json_lines_and_partial_tail_do_not_discard_valid_prs() {
        let temp = tempfile::tempdir().unwrap();
        for (backend, lines) in [
            (
                HarnessConversationBackend::Codex,
                vec![
                    call("create", "gh pr create"),
                    output("create", "https://github.com/example/docs/pull/181"),
                ],
            ),
            (
                HarnessConversationBackend::Claude,
                vec![
                    json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"create","input":{"command":"gh pr create"}}]}}),
                    json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"create","content":"https://github.com/example/docs/pull/181"}]}}),
                ],
            ),
        ] {
            let path = transcript(temp.path(), backend, &[]);
            std::fs::write(
                &path,
                format!("{}\ninvalid json\n{}\n{{\"type\":", lines[0], lines[1]),
            )
            .unwrap();
            let prs = scan_transcript(&path, backend).unwrap();
            assert_eq!(prs.len(), 1);
            assert_eq!(prs[0].key().id(), "example/docs#181");
        }
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
        let due = scans.take_due(
            std::slice::from_ref(&worker),
            &tasks,
            &HashMap::new(),
            now(),
        );
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
            .take_due(
                std::slice::from_ref(&worker),
                &tasks,
                &HashMap::new(),
                now()
            )
            .is_empty());
        // Even across launches, no transcript read (invalid UTF-8 would fail)
        // and no write of the marker or facts occurs on a second pass.
        std::fs::write(&source, b"\xff").unwrap();
        let marker = config.join("worker-pr-scans").join(&worker.delegation_id);
        let marked_at = std::fs::metadata(&marker).unwrap().modified().unwrap();
        let due = Scans::default().take_due(&[worker], &tasks, &HashMap::new(), now());
        assert_eq!(due.len(), 1);
        // Unlike `run`, this propagates a read error, so the assertion fails
        // if the invalid transcript is ever read again.
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
        let due = Scans::default().take_due(&[worker], &[], &HashMap::new(), now());
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
        let due = Scans::default().take_due(&[worker], &[], &HashMap::new(), now());
        assert_eq!(run(due, &config, temp.path())[0].1.len(), 1);
        assert_eq!(load_facts(&path).unwrap().unwrap().prs.len(), 2);
    }

    #[test]
    fn terminal_outcomes_only_within_fourteen_days_are_eligible() {
        let mut worker = worker(HarnessConversationBackend::Codex);
        for status in [
            DelegationStatus::Completed,
            DelegationStatus::Cancelled,
            DelegationStatus::Failed {
                message: "exited".into(),
            },
        ] {
            worker.status = status;
            assert_eq!(
                Scans::default()
                    .take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now())
                    .len(),
                1
            );
        }
        worker.updated_at = "2026-10-24T10:00:00Z".into();
        assert!(Scans::default()
            .take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now())
            .is_empty());
        worker.updated_at = "2026-11-08T10:00:00Z".into();
        for status in [
            DelegationStatus::Running,
            DelegationStatus::Requested,
            DelegationStatus::Interrupted,
            DelegationStatus::Blocked,
        ] {
            worker.status = status;
            assert!(Scans::default()
                .take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now())
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
            .write_all(b"\n\xff\n")
            .unwrap();
        let worker = worker(HarnessConversationBackend::Codex);
        let mut scans = Scans::default();
        let due = scans.take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now());
        assert!(run(due, &config, temp.path()).is_empty());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(!config
            .join("worker-pr-scans")
            .join(&worker.delegation_id)
            .exists());
        scans.running = false;
        assert!(scans
            .take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now())
            .is_empty());
        assert_eq!(
            Scans::default()
                .take_due(&[worker], &[], &HashMap::new(), now())
                .len(),
            1
        );
    }

    #[test]
    fn parent_without_facts_is_logged_and_skipped_once_without_reading_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let worker = worker(HarnessConversationBackend::Codex);
        let due =
            Scans::default().take_due(std::slice::from_ref(&worker), &[], &HashMap::new(), now());
        assert!(run(due, &config, temp.path()).is_empty());
        assert!(config
            .join("worker-pr-scans")
            .join(&worker.delegation_id)
            .exists());
        assert!(!facts_dir(&config).exists());
        let due = Scans::default().take_due(&[worker], &[], &HashMap::new(), now());
        assert!(run(due, &config, temp.path()).is_empty());
    }
}
