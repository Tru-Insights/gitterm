//! Headless proof for TRU-142 S3b: a real Claude calls `review_request` on
//! GitTerm's task MCP, a real Codex reviews a scratch repo in the
//! background, and the result is read back through `delegation_get`.
//!
//! The task MCP runs in-process exactly as in the app; its bridge is a small
//! stand-in for the Iced update loop that uses the same pieces the app does
//! (`gitterm::delegations` rules, the real `TaskStore` in a scratch config
//! dir, `codex_runner::run`). Claude reaches the server through the per-tab
//! URL (`?caller=`), as a chat tab does. Spends one Claude turn (haiku by
//! default) and one Codex review.
//!
//!   env -u GITTERM_V5_TASK_MCP_TOKEN cargo run --example delegation_smoke -- \
//!       --workdir <scratch dir> [--model haiku]
//!
//! Exits 0 only if every check passes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gitterm::codex_runner::{self, CodexRunEvent};
use gitterm::delegations::{self, CallerTab};
use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::{HarnessCommand, HarnessEvent, RuntimeDecision, TurnStatus};
use gitterm::task_mcp::{self, TaskControlEnvelope, TaskControlOperation};
use gitterm::tasks::{Delegation, DelegationStatus, TaskStore};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::Value;
use tokio::sync::mpsc;

const CALLER: &str = "delegation-smoke-tab-4c1d";
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);
const REVIEW_TIMEOUT: Duration = Duration::from_secs(420);
const MAX_CONCURRENT_REVIEWS: usize = 2;

const BASE_STATS: &str = r#""""Small statistics helpers."""


def mean(values):
    """Arithmetic mean of a non-empty list."""
    return sum(values) / len(values)


def find_user(users, name):
    """Return the first user whose name matches, or None when nobody does."""
    for user in users:
        if user["name"] == name:
            return user
    return None
"#;

const CHANGED_STATS: &str = r#""""Small statistics helpers."""


def mean(values):
    """Arithmetic mean of a non-empty list."""
    return sum(values) / len(values)


def find_user(users, name):
    """Return the first user whose name matches, or None when nobody does."""
    matches = [user for user in users if user["name"] == name]
    return matches[0]
"#;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    let Some(workdir) = arg("--workdir").map(PathBuf::from) else {
        eprintln!("usage: delegation_smoke --workdir <scratch dir> [--model haiku]");
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

/// What the stand-in bridge observed.
#[derive(Default)]
struct BridgeLog {
    /// (tool, caller, time the bridge took to answer).
    calls: Vec<(String, Option<String>, Duration)>,
}

struct Bridge {
    store: Mutex<TaskStore>,
    running: Mutex<HashSet<String>>,
    config_root: PathBuf,
    repo: PathBuf,
    log: Mutex<BridgeLog>,
}

async fn smoke(workdir: &Path) -> Result<(), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("clock before epoch: {error}"))?
        .as_secs();
    let root = workdir.join(format!("delegation-smoke-{stamp}"));
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
        running: Mutex::new(HashSet::new()),
        config_root: config_root.clone(),
        repo: repo.clone(),
        log: Mutex::new(BridgeLog::default()),
    });

    let (commands, requests) = mpsc::unbounded_channel();
    let (task, server) =
        task_mcp::prepare("delegation-smoke", commands).map_err(|error| error.to_string())?;
    tokio::spawn(server.run());
    tokio::spawn(run_bridge(requests, bridge.clone()));
    println!("task MCP {}", task.endpoint());

    // One Claude turn: it must call review_request and come back at once.
    let config = ClaudeSessionConfig {
        cwd: repo.clone(),
        model: Some(arg("--model").unwrap_or_else(|| "haiku".into())),
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: Some(root.join("wire")),
        mcp_servers: vec![task.claude_mcp_server(Some(CALLER))],
    };
    let (session, mut events) = ClaudeSession::spawn(config);
    let prompt = "Call the mcp__gitterm_tasks__review_request tool exactly once with target \
                  \"uncommitted\" to ask Codex to review the uncommitted changes. Then reply \
                  with only the delegation_id it returned. Do not call any other tool and do \
                  not wait for the review.";
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

    let (request_calls, slowest_answer) = {
        let log = bridge.log.lock().map_err(|_| "bridge log poisoned")?;
        let calls: Vec<_> = log
            .calls
            .iter()
            .filter(|(tool, _, _)| tool == "review_request")
            .cloned()
            .collect();
        let slowest = calls.iter().map(|(_, _, took)| *took).max();
        (calls, slowest)
    };
    let delegation_id = {
        let store = bridge.store.lock().map_err(|_| "store poisoned")?;
        store
            .delegations_for_parent(CALLER)
            .first()
            .map(|delegation| delegation.delegation_id.clone())
            .ok_or("Claude's review_request created no delegation")?
    };
    println!(
        "delegation {delegation_id}; review_request answered in {:?}",
        slowest_answer
    );

    // Wait for the review through delegation_get, as a parent would.
    let [(url_var, url), (_, token)] = task.terminal_environment(Some(CALLER));
    debug_assert_eq!(url_var, task_mcp::TASK_MCP_URL_ENV);
    let client = ()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
        ))
        .await
        .map_err(|error| format!("connect to the task MCP: {error}"))?;
    let review_started = Instant::now();
    let record = loop {
        let record = call_tool(
            &client,
            "delegation_get",
            serde_json::json!({ "delegation_id": delegation_id }),
        )
        .await?;
        let state = record["status"]["state"]
            .as_str()
            .unwrap_or("?")
            .to_string();
        if state != "requested" && state != "running" {
            break record;
        }
        if review_started.elapsed() > REVIEW_TIMEOUT {
            return Err(format!(
                "the review is still {state} after {REVIEW_TIMEOUT:?}"
            ));
        }
        println!(
            "[poll] {state} after {:.0}s",
            review_started.elapsed().as_secs_f64()
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let review_secs = review_started.elapsed().as_secs_f64();
    println!("review settled after {review_secs:.1}s more");
    let list = call_tool(&client, "delegation_list", serde_json::json!({})).await?;
    client
        .cancel()
        .await
        .map_err(|error| format!("close the MCP client: {error}"))?;

    let findings = record["result"]["findings"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for finding in &findings {
        println!(
            "  {} [{}] {} — {}:{}",
            finding["id"].as_str().unwrap_or("?"),
            finding["severity"].as_str().unwrap_or("?"),
            finding["title"].as_str().unwrap_or("?"),
            finding["file"].as_str().unwrap_or("?"),
            finding["line_start"]
        );
    }
    println!("verdict {}", record["result"]["findings"]["verdict"]);
    println!("reviewed {}", record["result"]["reviewed"]);
    println!("log {}", record["log_path"]);
    let finding_in_repo = findings.iter().any(|finding| {
        finding["file"]
            .as_str()
            .is_some_and(|file| !file.starts_with('/') && repo.join(file).is_file())
    });
    let log_has_lines = record["log_path"]
        .as_str()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|log| log.lines().count() > 3);
    let listed_first = list["delegations"][0]["delegation_id"].as_str() == Some(&delegation_id);
    let thread_recorded = record["child"]["thread_id"].as_str().is_some();

    let checks = [
        ("Claude's turn completed", status == TurnStatus::Completed),
        (
            "no permission prompt for the pre-approved tools",
            prompts == 0,
        ),
        (
            "exactly one review_request reached GitTerm",
            request_calls.len() == 1,
        ),
        (
            "review_request carried the chat tab's caller",
            request_calls
                .iter()
                .all(|(_, caller, _)| caller.as_deref() == Some(CALLER)),
        ),
        (
            "review_request answered in under 1 s",
            slowest_answer.is_some_and(|took| took < Duration::from_secs(1)),
        ),
        (
            "Claude's reply names the delegation id",
            text.contains(&delegation_id),
        ),
        (
            "Claude's turn did not wait for the review",
            turn_secs < 90.0,
        ),
        (
            "the delegation completed",
            record["status"]["state"].as_str() == Some("completed"),
        ),
        ("the Codex thread id was recorded", thread_recorded),
        ("a finding names a file in the repo", finding_in_repo),
        ("the run log was written", log_has_lines),
        ("delegation_list lists it first", listed_first),
    ];
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

async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
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
        return Err(format!("{name} failed: {:?}", result.content));
    }
    result
        .structured_content
        .ok_or_else(|| format!("{name} returned no structured content"))
}

/// The stand-in for the app's `TaskControlRequested` handling of the four
/// delegation tools.
async fn run_bridge(
    mut requests: mpsc::UnboundedReceiver<TaskControlEnvelope>,
    bridge: Arc<Bridge>,
) {
    while let Some(envelope) = requests.recv().await {
        let received = Instant::now();
        let caller = envelope.caller.clone();
        let (tool, result) = match envelope.operation {
            TaskControlOperation::RequestReview(request) => (
                "review_request",
                request_review(&bridge, caller.as_deref(), &request).await,
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
        if let Ok(mut log) = bridge.log.lock() {
            log.calls
                .push((tool.to_string(), caller, received.elapsed()));
        }
        envelope.reply.send(result);
    }
}

async fn request_review(
    bridge: &Arc<Bridge>,
    caller: Option<&str>,
    request: &task_mcp::ReviewDelegationRequest,
) -> Result<Value, String> {
    let caller = caller.ok_or_else(|| task_mcp::missing_caller_error("review_request"))?;
    let tab = CallerTab {
        session_uid: caller.to_string(),
        chat_session_id: None,
        workspace: "delegation-smoke".to_string(),
        cwd: bridge.repo.clone(),
        remote: false,
    };
    let new = delegations::review_delegation(request, &tab, None, None)?;
    let cwd = tab.cwd.clone();
    tokio::task::spawn_blocking(move || delegations::check_checkout(&cwd))
        .await
        .map_err(|error| format!("checkout check failed: {error}"))??;
    let delegation = Delegation::new_requested(new, chrono::Utc::now().to_rfc3339());
    let delegation_id = delegation.delegation_id.clone();
    bridge
        .store
        .lock()
        .map_err(|_| "store poisoned".to_string())?
        .insert_delegation(delegation)
        .map_err(|error| error.to_string())?;
    start_queued(bridge);
    Ok(serde_json::json!({ "delegation_id": delegation_id, "status": "requested" }))
}

/// `App::start_queued_delegations` with tokio tasks instead of Iced tasks.
fn start_queued(bridge: &Arc<Bridge>) {
    let runs = {
        let (Ok(store), Ok(mut running)) = (bridge.store.lock(), bridge.running.lock()) else {
            eprintln!("[bridge] state poisoned; nothing started");
            return;
        };
        let ids = delegations::runs_to_start(store.delegations(), &running, MAX_CONCURRENT_REVIEWS);
        let mut runs = Vec::new();
        for id in ids {
            let Some(delegation) = store.delegation(&id) else {
                continue;
            };
            match delegations::codex_run(delegation, &bridge.config_root) {
                Ok(run) => {
                    running.insert(id.clone());
                    runs.push((id, run));
                }
                Err(error) => eprintln!("[bridge] {id} cannot run: {error}"),
            }
        }
        runs
    };
    for (id, run) in runs {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            let (progress, mut events) = mpsc::unbounded_channel();
            let watcher = {
                let bridge = bridge.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        match event {
                            CodexRunEvent::Started { thread_id } => {
                                println!("[codex] started thread {thread_id}");
                                if let Ok(mut store) = bridge.store.lock() {
                                    if let Err(error) = store.mark_delegation_started(
                                        &id,
                                        &thread_id,
                                        &chrono::Utc::now().to_rfc3339(),
                                    ) {
                                        eprintln!("[bridge] {error}");
                                    }
                                }
                            }
                            CodexRunEvent::Activity { description } => {
                                println!("[codex] {description}")
                            }
                            CodexRunEvent::Completed => println!("[codex] completed"),
                            CodexRunEvent::Failed { message } => {
                                println!("[codex] failed: {message}")
                            }
                        }
                    }
                })
            };
            let outcome = codex_runner::run(run, progress).await;
            if let Err(error) = watcher.await {
                eprintln!("[bridge] progress watcher panicked: {error}");
            }
            let now = chrono::Utc::now().to_rfc3339();
            if let Ok(mut store) = bridge.store.lock() {
                let stored = match outcome {
                    Ok(outcome) => store.complete_delegation(&id, outcome.result, &now),
                    Err(error) => store.set_delegation_status(
                        &id,
                        DelegationStatus::Failed {
                            message: error.to_string(),
                        },
                        &now,
                    ),
                };
                if let Err(error) = stored {
                    eprintln!("[bridge] {id}: {error}");
                }
            }
            if let Ok(mut running) = bridge.running.lock() {
                running.remove(&id);
            }
            start_queued(&bridge);
        });
    }
}

fn build_repo(repo: &Path) -> Result<(), String> {
    std::fs::create_dir_all(repo).map_err(|error| format!("create {}: {error}", repo.display()))?;
    write(
        &repo.join("README.md"),
        "# stats\n\nA tiny Python module of statistics helpers used to test code review tooling.\n",
    )?;
    write(&repo.join("stats.py"), BASE_STATS)?;
    git(repo, &["init", "-q", "-b", "main"])?;
    git(repo, &["add", "."])?;
    git(repo, &["commit", "-q", "-m", "Add stats helpers"])?;
    write(&repo.join("stats.py"), CHANGED_STATS)
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
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
