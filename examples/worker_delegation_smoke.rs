//! Headless proof for TRU-142 S6: a real Claude calls `delegate_task` on
//! GitTerm's task MCP, a task with a real git worktree is created in a
//! scratch repository, a worker is "launched", a second MCP client posing as
//! the worker's tab records `status: done`, and the parent reads the
//! completed handoff back through `delegation_get`.
//!
//! The task MCP runs in-process exactly as in the app. Its bridge is a small
//! stand-in for the Iced update loop that uses the same library pieces the
//! app does: `gitterm::workers::resolve_worker` and the model policy, the
//! real `TaskStore` in a scratch config dir, `task_worktree`'s create path,
//! `gitterm::delegations::{worker_delegation, report_back_section,
//! record_handoff}`. The launch is simulated: headless there is no PTY, so
//! the bridge records the task session and writes the brief instead of
//! opening a terminal tab. Spends one Claude turn (haiku by default).
//!
//!   env -u GITTERM_V5_TASK_MCP_TOKEN cargo run --example worker_delegation_smoke -- \
//!       --workdir <scratch dir> [--model haiku]
//!
//! Exits 0 only if every check passes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gitterm::delegations::{self, CallerTab};
use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::{HarnessCommand, HarnessEvent, RuntimeDecision, TurnStatus};
use gitterm::task_mcp::{self, TaskControlEnvelope, TaskControlOperation, TaskStoppingBoundary};
use gitterm::task_worktree::{
    prepare_task_worktree_blocking, resolve_task_preparation_blocking, PrepareTaskWorktreeRequest,
    DEFAULT_TASK_BASE,
};
use gitterm::tasks::{
    Delegation, ExecutorTarget, IssueProvider, IssueReference, NewTaskRecord,
    ObjectiveDeliveryState, StoppingBoundary, TaskHandoff, TaskLifecycle, TaskRecord,
    TaskSessionRecord, TaskStore, TaskWorktree, TaskWorktreeState, WorkspaceIdentity,
    WorkspaceLocationIdentity,
};
use gitterm::workers::{resolve_worker, ModelPolicy, PresetRef};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::Value;
use tokio::sync::mpsc;

const PARENT: &str = "worker-smoke-chat-tab-7e2a";
const WORKER: &str = "worker-smoke-worker-tab-91bf";
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    let Some(workdir) = arg("--workdir").map(PathBuf::from) else {
        eprintln!("usage: worker_delegation_smoke --workdir <scratch dir> [--model haiku]");
        return ExitCode::from(2);
    };
    let started = Instant::now();
    let result = smoke(&workdir).await;
    println!("total {:.1}s", started.elapsed().as_secs_f64());
    match result {
        Ok(()) => {
            println!("SMOKE OK");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("SMOKE FAILED: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Bridge {
    store: Mutex<TaskStore>,
    config_root: PathBuf,
    repo: PathBuf,
    /// Tab session_uid -> (task id, task session id) the tab hosts, as the
    /// app's tabs do after a launch.
    hosted: Mutex<HashMap<String, (String, String)>>,
    /// (tool, caller, time the bridge took to answer).
    calls: Mutex<Vec<(String, Option<String>, Duration)>>,
    briefs: Mutex<Vec<PathBuf>>,
}

async fn smoke(workdir: &Path) -> Result<(), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("clock before epoch: {error}"))?
        .as_secs();
    let root = workdir.join(format!("worker-smoke-{stamp}"));
    let repo = root.join("repo");
    let config_root = root.join("config");
    build_repo(&repo)?;
    let repo = repo
        .canonicalize()
        .map_err(|error| format!("canonicalize {}: {error}", repo.display()))?;
    println!(
        "scratch repo {} / config {}",
        repo.display(),
        config_root.display()
    );
    let store = TaskStore::load(TaskStore::path_for_config_root(&config_root))
        .map_err(|error| error.to_string())?;
    let bridge = Arc::new(Bridge {
        store: Mutex::new(store),
        config_root: config_root.clone(),
        repo: repo.clone(),
        hosted: Mutex::new(HashMap::new()),
        calls: Mutex::new(Vec::new()),
        briefs: Mutex::new(Vec::new()),
    });

    let (commands, requests) = mpsc::unbounded_channel();
    let (task, server) =
        task_mcp::prepare("worker-smoke", commands).map_err(|error| error.to_string())?;
    tokio::spawn(server.run());
    tokio::spawn(run_bridge(requests, bridge.clone()));
    println!("task MCP {}", task.endpoint());

    // One Claude turn in the parent chat: it must call delegate_task.
    let config = ClaudeSessionConfig {
        cwd: repo.clone(),
        model: Some(arg("--model").unwrap_or_else(|| "haiku".into())),
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: Some(root.join("wire")),
        mcp_servers: vec![task.claude_mcp_server(Some(PARENT))],
    };
    let (session, mut events) = ClaudeSession::spawn(config);
    let prompt = "Hand the find_user fix to a Codex worker: call the \
                  mcp__gitterm_tasks__delegate_task tool exactly once with title \"Fix find_user\", \
                  objective \"find_user in stats.py must return None when no user matches; add a \
                  test.\", issue_key \"TRU-150\", stopping_boundary \"implement_until_tests_pass\" \
                  and role \"scoped\". Then reply with only the delegation_id and branch it \
                  returned. Do not call any other tool and do not wait for the worker.";
    println!(">> {prompt}");
    let turn_started = Instant::now();
    session
        .send(HarnessCommand::SendUserMessage(prompt.into()))
        .map_err(|error| format!("send prompt: {error}"))?;
    let mut text = String::new();
    let mut prompts = 0;
    let status = loop {
        let event = tokio::time::timeout(CLAUDE_TIMEOUT, events.recv())
            .await
            .map_err(|_| "timed out waiting for Claude".to_string())?
            .ok_or("Claude's event channel closed")?;
        match event {
            HarnessEvent::TextDelta(delta) => text.push_str(&delta),
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                prompts += 1;
                println!("[claude] unexpected permission prompt {kind:?}; denying");
                session
                    .send(HarnessCommand::Answer {
                        request_id,
                        decision: RuntimeDecision::Deny {
                            message: "the smoke expects pre-approved task tools".into(),
                        },
                    })
                    .map_err(|error| format!("deny: {error}"))?;
            }
            HarnessEvent::ItemStarted { kind, .. } => println!("[claude] tool {kind:?}"),
            HarnessEvent::TurnCompleted { status, .. } => break status,
            HarnessEvent::ProcessExited { code } => {
                return Err(format!("claude exited {code:?}"));
            }
            _ => {}
        }
    };
    let turn_secs = turn_started.elapsed().as_secs_f64();
    drop(session);
    println!("<< {text}");
    println!("Claude turn took {turn_secs:.1}s");

    let delegate_calls: Vec<_> = bridge
        .calls
        .lock()
        .map_err(|_| "calls poisoned")?
        .iter()
        .filter(|(tool, _, _)| tool == "delegate_task")
        .cloned()
        .collect();
    let (delegation_id, task_id, branch) = {
        let store = bridge.store.lock().map_err(|_| "store poisoned")?;
        let delegation = store
            .delegations_for_parent(PARENT)
            .first()
            .map(|delegation| (*delegation).clone())
            .ok_or("Claude's delegate_task created no delegation")?;
        let gitterm::tasks::DelegationChild::TaskSession { task_id, .. } = &delegation.child else {
            return Err("the delegation has no task-session worker".into());
        };
        let task = store.get(task_id).ok_or("the worker task is missing")?;
        (
            delegation.delegation_id.clone(),
            task_id.clone(),
            task.branch.clone(),
        )
    };
    println!("delegation {delegation_id} task {task_id} branch {branch}");

    let connect = |caller: &'static str| {
        let [(_, url), (_, token)] = task.terminal_environment(Some(caller));
        async move {
            ().serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
            ))
            .await
            .map_err(|error| format!("connect to the task MCP as {caller}: {error}"))
        }
    };
    // The parent's Claude cannot complete its own worker's delegation.
    let parent = connect(PARENT).await?;
    let parent_refused = call_tool_raw(
        &parent,
        "task_update_handoff",
        serde_json::json!({ "task_id": task_id, "summary": "done?", "status": "done" }),
    )
    .await?;
    // The worker's tab reports progress, then done.
    let worker = connect(WORKER).await?;
    let progress = call_tool(
        &worker,
        "task_update_handoff",
        serde_json::json!({ "task_id": task_id, "summary": "Wrote a failing test for find_user" }),
    )
    .await?;
    let progress_state = delegation_state(&bridge, &delegation_id)?;
    let done = call_tool(
        &worker,
        "task_update_handoff",
        serde_json::json!({
            "task_id": task_id,
            "summary": "find_user returns None when nobody matches; tests pass",
            "decisions": ["Kept the loop; no list comprehension"],
            "next_steps": ["Review the branch"],
            "status": "done",
        }),
    )
    .await?;
    worker
        .cancel()
        .await
        .map_err(|error| format!("close the worker client: {error}"))?;
    let record = call_tool(
        &parent,
        "delegation_get",
        serde_json::json!({ "delegation_id": delegation_id }),
    )
    .await?;
    let list = call_tool(&parent, "delegation_list", serde_json::json!({})).await?;
    parent
        .cancel()
        .await
        .map_err(|error| format!("close the parent client: {error}"))?;
    println!(
        "delegation_get: {}",
        serde_json::to_string_pretty(&record).unwrap_or_default()
    );

    let brief = bridge
        .briefs
        .lock()
        .map_err(|_| "briefs poisoned")?
        .first()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let worktree = record_worktree(&bridge, &task_id)?;
    let checks = [
        ("Claude's turn completed", status == TurnStatus::Completed),
        (
            "no permission prompt for the pre-approved tools",
            prompts == 0,
        ),
        (
            "exactly one delegate_task reached GitTerm",
            delegate_calls.len() == 1,
        ),
        (
            "delegate_task carried the chat tab's caller",
            delegate_calls
                .iter()
                .all(|(_, caller, _)| caller.as_deref() == Some(PARENT)),
        ),
        (
            "Claude's reply names the delegation id",
            text.contains(&delegation_id),
        ),
        (
            "the worker task has a real git worktree on its branch",
            worktree.as_ref().is_some_and(|path| {
                path.join("stats.py").is_file() && current_branch(path).as_deref() == Some(&branch)
            }),
        ),
        (
            "the worker brief ends with Report back naming the delegation",
            brief.contains("Report back:")
                && brief.contains(&format!("GitTerm delegation {delegation_id}")),
        ),
        (
            "the parent cannot report done for its worker",
            parent_refused
                .as_ref()
                .err()
                .is_some_and(|error| error.contains("only the worker's own GitTerm tab")),
        ),
        (
            "progress left the delegation running",
            progress["delegation_id"].is_null() && progress_state == "running",
        ),
        (
            "done named the delegation",
            done["delegation_id"].as_str() == Some(delegation_id.as_str()),
        ),
        (
            "delegation_get shows it completed",
            record["status"]["state"].as_str() == Some("completed"),
        ),
        (
            "the completed handoff is the worker's report",
            record["result"]["handoff"]["summary"].as_str()
                == Some("find_user returns None when nobody matches; tests pass")
                && record["result"]["handoff"]["next_steps"][0].as_str()
                    == Some("Review the branch"),
        ),
        (
            "the worker ran on the policy's scoped Codex model",
            record["child"]["preset_name"].as_str() == Some("Codex")
                && record["child"]["model"].as_str() == Some("gpt-6-luna")
                && record["child"]["role"].as_str() == Some("scoped"),
        ),
        (
            "delegation_list lists it first",
            list["delegations"][0]["delegation_id"].as_str() == Some(&delegation_id),
        ),
    ];
    if let Some((_, _, took)) = delegate_calls.first() {
        println!("delegate_task answered in {took:?} (worktree creation included)");
    }
    let mut failed = Vec::new();
    for (name, ok) in checks {
        println!("{} {name}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failed.push(name);
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("failed: {}", failed.join("; ")))
    }
}

fn delegation_state(bridge: &Bridge, delegation_id: &str) -> Result<String, String> {
    let store = bridge.store.lock().map_err(|_| "store poisoned")?;
    Ok(store
        .delegation(delegation_id)
        .map(|delegation| delegation.status.label().to_string())
        .unwrap_or_default())
}

fn record_worktree(bridge: &Bridge, task_id: &str) -> Result<Option<PathBuf>, String> {
    let store = bridge.store.lock().map_err(|_| "store poisoned")?;
    Ok(store
        .get(task_id)
        .and_then(|task| task.worktree.path.clone()))
}

async fn call_tool_raw(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> Result<Result<Value, String>, String> {
    let Value::Object(arguments) = arguments else {
        return Err("tool arguments must be an object".to_string());
    };
    let result = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new(name.to_string()).with_arguments(arguments),
        )
        .await
        .map_err(|error| format!("{name}: {error}"))?;
    if result.is_error.unwrap_or(false) {
        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Err(text));
    }
    result
        .structured_content
        .map(Ok)
        .ok_or_else(|| format!("{name} returned no structured content"))
}

async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    call_tool_raw(client, name, arguments)
        .await?
        .map_err(|error| format!("{name} failed: {error}"))
}

/// The stand-in for the app's `TaskControlRequested` handling.
async fn run_bridge(
    mut requests: mpsc::UnboundedReceiver<TaskControlEnvelope>,
    bridge: Arc<Bridge>,
) {
    while let Some(envelope) = requests.recv().await {
        let received = Instant::now();
        let caller = envelope.caller.clone();
        let (tool, result) = match envelope.operation {
            TaskControlOperation::DelegateTask(request) => (
                "delegate_task",
                delegate_task(&bridge, caller.as_deref(), request).await,
            ),
            TaskControlOperation::UpdateHandoff(request) => (
                "task_update_handoff",
                update_handoff(&bridge, caller.as_deref(), request),
            ),
            TaskControlOperation::GetDelegation(request) => ("delegation_get", {
                bridge
                    .store
                    .lock()
                    .map_err(|_| "store poisoned".to_string())
                    .and_then(|store| {
                        store
                            .delegation(&request.delegation_id)
                            .ok_or_else(|| {
                                format!("delegation {} does not exist", request.delegation_id)
                            })
                            .and_then(|delegation| {
                                delegations::delegation_record(delegation, &bridge.config_root)
                            })
                    })
            }),
            TaskControlOperation::ListDelegations(request) => ("delegation_list", {
                bridge
                    .store
                    .lock()
                    .map_err(|_| "store poisoned".to_string())
                    .and_then(|store| {
                        delegations::delegation_list(
                            &store.delegations_for_parent(caller.as_deref().unwrap_or_default()),
                            request.status.as_deref(),
                        )
                    })
            }),
            other => (
                "other",
                Err(format!("the smoke bridge does not handle {other:?}")),
            ),
        };
        if let Ok(mut calls) = bridge.calls.lock() {
            calls.push((tool.to_string(), caller, received.elapsed()));
        }
        envelope.reply.send(result);
    }
}

/// `App::delegate_task_requested` + `start_delegated_worker`, with the
/// worktree created by the same `task_worktree` functions and the terminal
/// launch simulated.
async fn delegate_task(
    bridge: &Arc<Bridge>,
    caller: Option<&str>,
    request: task_mcp::DelegateTaskRequest,
) -> Result<Value, String> {
    let caller = caller.ok_or_else(|| task_mcp::missing_caller_error("delegate_task"))?;
    let tab = CallerTab {
        session_uid: caller.to_string(),
        chat_session_id: None,
        workspace: "worker-smoke".to_string(),
        cwd: bridge.repo.clone(),
        remote: false,
    };
    // The default presets' Claude Code and Codex entries.
    let presets = [
        PresetRef {
            name: "Pi",
            command: "pi",
        },
        PresetRef {
            name: "Claude Code",
            command: "claude",
        },
        PresetRef {
            name: "Codex",
            command: "codex",
        },
    ];
    let choice = resolve_worker(
        &presets,
        request.preset_name.as_deref(),
        request.role,
        request.model.as_deref(),
        &ModelPolicy::default(),
    )?;
    let task_id = uuid::Uuid::new_v4().to_string();
    let issue_key = request
        .issue_key
        .clone()
        .map(|key| key.trim().to_uppercase());
    let prepare = PrepareTaskWorktreeRequest {
        repository_path: bridge.repo.clone(),
        worktree_root: bridge.config_root.join("worktrees"),
        task_id: task_id.clone(),
        title: request.title.clone(),
        issue_key: issue_key.clone(),
        base_reference: request
            .base_reference
            .clone()
            .unwrap_or_else(|| DEFAULT_TASK_BASE.to_string()),
    };
    let (resolved, prepared) = tokio::task::spawn_blocking(move || {
        let resolved = resolve_task_preparation_blocking(&prepare).map_err(|e| e.to_string())?;
        let prepared = prepare_task_worktree_blocking(&prepare).map_err(|e| e.to_string())?;
        Ok::<_, String>((resolved, prepared))
    })
    .await
    .map_err(|error| format!("worktree preparation panicked: {error}"))??;
    let now = chrono::Utc::now().to_rfc3339();
    let mut record = TaskRecord::new_draft(
        NewTaskRecord {
            task_id: task_id.clone(),
            title: request.title.trim().to_string(),
            objective: request.objective.trim().to_string(),
            workspace: WorkspaceIdentity {
                name: "worker-smoke".to_string(),
                location: WorkspaceLocationIdentity::Local {
                    directory: resolved.repository.top_level.clone(),
                },
            },
            repository: resolved.repository.identity.clone(),
            issue: issue_key.map(|key| IssueReference {
                provider: IssueProvider::Linear,
                key,
                url: None,
            }),
            base: resolved.base.clone(),
            branch: resolved.proposal.branch.clone(),
            executor: ExecutorTarget::Local,
            harness: None,
            stopping_boundary: match request.stopping_boundary {
                Some(TaskStoppingBoundary::PlanOnly) => StoppingBoundary::PlanOnly,
                Some(TaskStoppingBoundary::PrepareDraftPr) => StoppingBoundary::PrepareDraftPr,
                _ => StoppingBoundary::ImplementUntilTestsPass,
            },
        },
        now.clone(),
    );
    record.lifecycle = TaskLifecycle::Preparing;
    record.worktree = TaskWorktree {
        state: TaskWorktreeState::Preparing,
        path: None,
    };
    let mut store = bridge.store.lock().map_err(|_| "store poisoned")?;
    store.insert(record).map_err(|error| error.to_string())?;
    store
        .complete_worktree_preparation(&task_id, prepared.into(), &now)
        .map_err(|error| error.to_string())?;
    let task = store.get(&task_id).cloned().ok_or("task vanished")?;
    let delegation = Delegation::new_requested(
        delegations::worker_delegation(&tab, &task_id, &task.title, &task.objective, &choice)?,
        now.clone(),
    );
    let delegation_id = delegation.delegation_id.clone();
    store
        .insert_delegation(delegation)
        .map_err(|error| error.to_string())?;

    // Simulated launch: no PTY headless. Record the session, write the
    // brief the terminal would get, and let the worker tab host it.
    let session_id = uuid::Uuid::new_v4().to_string();
    let brief_dir = bridge.config_root.join("task-briefs");
    std::fs::create_dir_all(&brief_dir)
        .map_err(|error| format!("create {}: {error}", brief_dir.display()))?;
    let brief_path = brief_dir.join(format!("{session_id}.md"));
    std::fs::write(
        &brief_path,
        format!(
            "Continue GitTerm task {task_id}: {}\n\nObjective:\n{}\n\n{}",
            task.title,
            task.objective,
            delegations::report_back_section(&delegation_id, &task_id)
        ),
    )
    .map_err(|error| format!("write {}: {error}", brief_path.display()))?;
    store
        .upsert_session(
            &task_id,
            TaskSessionRecord {
                task_session_id: session_id.clone(),
                label: choice.preset_name.clone(),
                harness: None,
                conversation: None,
                objective_delivery: ObjectiveDeliveryState::Delivered,
                created_at: now.clone(),
                updated_at: now.clone(),
            },
            &now,
        )
        .map_err(|error| error.to_string())?;
    store
        .attach_worker_session(&delegation_id, &session_id, None, &now)
        .map_err(|error| error.to_string())?;
    store
        .record_lifecycle_signal(&task_id, TaskLifecycle::Running, None, &now)
        .map_err(|error| error.to_string())?;
    drop(store);
    bridge
        .hosted
        .lock()
        .map_err(|_| "hosted poisoned")?
        .insert(WORKER.to_string(), (task_id.clone(), session_id.clone()));
    bridge
        .briefs
        .lock()
        .map_err(|_| "briefs poisoned")?
        .push(brief_path);
    println!(
        "[bridge] simulated launch of {} for task {task_id}",
        choice.preset_name
    );
    Ok(serde_json::json!({
        "delegation_id": delegation_id,
        "task_id": task_id,
        "task_session_id": session_id,
        "worktree_path": task.worktree.path,
        "branch": task.branch,
        "preset_name": choice.preset_name,
        "model": choice.model,
        "role": choice.role.label(),
        "queued": false,
    }))
}

/// `App::update_handoff_requested`: the hosted session comes from the
/// calling tab, never from the request.
fn update_handoff(
    bridge: &Arc<Bridge>,
    caller: Option<&str>,
    request: task_mcp::UpdateTaskHandoffRequest,
) -> Result<Value, String> {
    let hosted_session = caller
        .and_then(|caller| bridge.hosted.lock().ok()?.get(caller).cloned())
        .filter(|(task_id, _)| *task_id == request.task_id)
        .map(|(_, session)| session);
    let now = chrono::Utc::now().to_rfc3339();
    let mut store = bridge.store.lock().map_err(|_| "store poisoned")?;
    let delegation = delegations::record_handoff(
        &mut store,
        &request.task_id,
        TaskHandoff {
            summary: request.summary.trim().to_string(),
            decisions: request.decisions,
            next_steps: request.next_steps,
            blockers: request.blockers,
            updated_by_session_id: hosted_session.clone().or(caller.map(str::to_string)),
            updated_at: now.clone(),
        },
        request.status.unwrap_or_default(),
        hosted_session.as_deref(),
        &now,
    )?;
    Ok(serde_json::json!({ "task_id": request.task_id, "delegation_id": delegation }))
}

const STATS: &str = r#""""Small statistics helpers."""


def find_user(users, name):
    """Return the first user whose name matches, or None when nobody does."""
    matches = [user for user in users if user["name"] == name]
    return matches[0]
"#;

fn build_repo(repo: &Path) -> Result<(), String> {
    std::fs::create_dir_all(repo).map_err(|error| format!("create {}: {error}", repo.display()))?;
    std::fs::write(repo.join("stats.py"), STATS)
        .map_err(|error| format!("write stats.py: {error}"))?;
    git(repo, &["init", "-q", "-b", "main"])?;
    git(repo, &["add", "."])?;
    git(repo, &["commit", "-q", "-m", "Add stats helpers"])
}

fn current_branch(path: &Path) -> Option<String> {
    let output = gitterm::agentd::git::git_command()
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git(repo: &Path, args: &[&str]) -> Result<(), String> {
    // `git_command` clears the repository variables a hook exports.
    let mut command: Command = gitterm::agentd::git::git_command();
    for (var, value) in [
        ("GIT_AUTHOR_NAME", "Smoke"),
        ("GIT_AUTHOR_EMAIL", "smoke@example.com"),
        ("GIT_COMMITTER_NAME", "Smoke"),
        ("GIT_COMMITTER_EMAIL", "smoke@example.com"),
    ] {
        command.env(var, value);
    }
    let output = command
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("run git {args:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}
