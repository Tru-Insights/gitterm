// TRU-140 Phase B: drive `gitterm::harness::claude::ClaudeSession` (the
// library adapter the chat tab uses) through the acceptance flow without
// the UI: a permission mode cycle (acceptEdits, plan, auto, default), a text
// turn, a Bash permission prompt (allowed), an AskUserQuestion (answered),
// and an interrupted long turn.
//
//   cargo run --example claude_harness_smoke -- --workdir <empty dir> [--model haiku]
//       [--scenario all|permission-mode|review|model] [--reviewer-model haiku]
//
// `--scenario permission-mode` runs only the mode switch (no model turns).
// `--scenario model` (TRU-143) switches the model to sonnet after Ready
// (`set_model`), pins the effort to low and back to auto
// (`apply_flag_settings` then `get_settings`), and checks that the next turn
// runs on sonnet.
// `--scenario review` (TRU-142 R1) builds a scratch git repo under the
// workdir with a planted bug in an uncommitted change, sends the Review…
// prompt (`gitterm::review::review_prompt`), and checks that the subagent's
// activity arrives as nested `SubagentEvent`s under the parent's Agent tool
// call and that the relayed report has F1 with a `file:line` in the repo.
// Prints every HarnessEvent as JSON and exits 1 if a step fails.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::{
    HarnessCommand, HarnessEvent, ItemKind, RuntimeDecision, RuntimeRequestKind, TurnStatus,
};
use gitterm::review::{review_prompt, ReviewRequest, ReviewTarget};
use tokio::sync::mpsc::UnboundedReceiver;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);
/// A review turn spawns a subagent that reads the diff; give it longer.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(600);

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn show(ev: &HarnessEvent) {
    match ev {
        HarnessEvent::TextDelta(_) | HarnessEvent::ItemInputDelta { .. } => {}
        HarnessEvent::SubagentEvent { event, .. }
            if matches!(
                **event,
                HarnessEvent::TextDelta(_)
                    | HarnessEvent::ThinkingDelta(_)
                    | HarnessEvent::ItemInputDelta { .. }
            ) => {}
        HarnessEvent::Ready {
            session_id,
            permission_mode,
            models,
        } => println!(
            "[event] ready session_id={session_id:?} mode={permission_mode:?} models={}",
            models.len()
        ),
        other => println!(
            "[event] {}",
            serde_json::to_string(other).unwrap_or_default()
        ),
    }
}

/// Runs one turn to completion, answering runtime requests with `answer`.
async fn turn(
    session: &ClaudeSession,
    events: &mut UnboundedReceiver<HarnessEvent>,
    prompt: &str,
    mut answer: impl FnMut(&RuntimeRequestKind) -> RuntimeDecision,
    interrupt_after_text: Option<usize>,
) -> Result<(TurnStatus, String, usize), String> {
    println!("\n>> {prompt}");
    session.send(HarnessCommand::SendUserMessage(prompt.to_string()))?;
    let mut text = String::new();
    let mut requests = 0;
    let mut interrupted = false;
    loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, events.recv())
            .await
            .map_err(|_| "timed out".to_string())?
            .ok_or("event channel closed")?;
        show(&ev);
        match ev {
            HarnessEvent::TextDelta(t) => {
                text.push_str(&t);
                if let Some(n) = interrupt_after_text {
                    if !interrupted && text.len() >= n {
                        interrupted = true;
                        println!(">> interrupt after {} chars", text.len());
                        session.send(HarnessCommand::Interrupt)?;
                    }
                }
            }
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                requests += 1;
                let decision = answer(&kind);
                println!(">> answer {request_id}: {decision:?}");
                session.send(HarnessCommand::Answer {
                    request_id,
                    decision,
                })?;
            }
            HarnessEvent::TurnCompleted { status, .. } => return Ok((status, text, requests)),
            HarnessEvent::ProcessExited { code } => return Err(format!("claude exited {code:?}")),
            _ => {}
        }
    }
}

/// Waits for the next event matching `pick`, printing everything on the way.
async fn wait_for<T>(
    events: &mut UnboundedReceiver<HarnessEvent>,
    mut pick: impl FnMut(&HarnessEvent) -> Option<T>,
) -> Result<T, String> {
    loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, events.recv())
            .await
            .map_err(|_| "timed out".to_string())?
            .ok_or("event channel closed")?;
        show(&ev);
        if let Some(found) = pick(&ev) {
            return Ok(found);
        }
        match ev {
            HarnessEvent::Error(e) => return Err(e),
            HarnessEvent::ProcessExited { code } => return Err(format!("claude exited {code:?}")),
            _ => {}
        }
    }
}

/// Switches the session's permission mode and waits for the confirmation.
async fn set_mode(
    session: &ClaudeSession,
    events: &mut UnboundedReceiver<HarnessEvent>,
    mode: &str,
) -> Result<String, String> {
    println!("\n>> set permission mode {mode}");
    session.send(HarnessCommand::SetPermissionMode(mode.to_string()))?;
    wait_for(events, |ev| match ev {
        HarnessEvent::PermissionModeChanged(m) => Some(m.clone()),
        _ => None,
    })
    .await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let workdir = PathBuf::from(arg("--workdir").expect("--workdir <dir> is required"));
    std::fs::create_dir_all(&workdir).expect("create workdir");
    let scenario = arg("--scenario").unwrap_or_else(|| "all".into());
    if !["all", "permission-mode", "review", "model"].contains(&scenario.as_str()) {
        eprintln!(
            "unknown --scenario {scenario:?} (expected all, permission-mode, review or model)"
        );
        std::process::exit(2);
    }
    if scenario == "review" {
        review_scenario(&workdir).await;
        return;
    }
    let (session, mut events) = ClaudeSession::spawn(ClaudeSessionConfig {
        cwd: workdir.clone(),
        model: Some(arg("--model").unwrap_or_else(|| "haiku".into())),
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: Some(workdir.with_extension("wire")),
        mcp_servers: Vec::new(),
    });
    let mut failures = Vec::new();
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures.push(name.to_string());
        }
    };
    let deny_all = |_: &RuntimeRequestKind| RuntimeDecision::Deny {
        message: "smoke test did not expect a prompt".into(),
    };

    // The handshake reply arrives without a prompt; switch modes after it.
    match wait_for(&mut events, |ev| match ev {
        HarnessEvent::Ready {
            permission_mode, ..
        } => Some(permission_mode.clone()),
        _ => None,
    })
    .await
    {
        Ok(mode) => check(
            "ready",
            mode.as_deref() == Some("default"),
            format!("mode={mode:?}"),
        ),
        Err(e) => check("ready", false, e),
    }
    if scenario == "model" {
        model_scenario(&session, &mut events, &mut check).await;
        finish(session, events, failures).await;
        return;
    }
    // The chat page's Shift+Tab cycle, ending back where it started.
    for mode in ["acceptEdits", "plan", "auto", "default"] {
        match set_mode(&session, &mut events, mode).await {
            Ok(confirmed) => check(
                &format!("permission mode {mode}"),
                confirmed == mode,
                format!("confirmed={confirmed:?}"),
            ),
            Err(e) => check(&format!("permission mode {mode}"), false, e),
        }
    }
    if scenario == "permission-mode" {
        finish(session, events, failures).await;
        return;
    }

    match turn(
        &session,
        &mut events,
        "Reply with exactly: ready",
        deny_all,
        None,
    )
    .await
    {
        Ok((status, text, _)) => check(
            "text turn",
            status == TurnStatus::Completed && text.trim() == "ready",
            format!("{status:?} {text:?}"),
        ),
        Err(e) => check("text turn", false, e),
    }

    let allow = |kind: &RuntimeRequestKind| match kind {
        RuntimeRequestKind::Permission { .. } => RuntimeDecision::Allow {
            updated_input: None,
            remember: None,
        },
        RuntimeRequestKind::Question { .. } => RuntimeDecision::Deny {
            message: "unexpected question".into(),
        },
    };
    match turn(
        &session,
        &mut events,
        "Use the Bash tool to run `touch gitterm-probe.txt`. Then reply with done.",
        allow,
        None,
    )
    .await
    {
        Ok((status, _, requests)) => check(
            "bash permission",
            status == TurnStatus::Completed
                && requests >= 1
                && workdir.join("gitterm-probe.txt").exists(),
            format!("{status:?} requests={requests}"),
        ),
        Err(e) => check("bash permission", false, e),
    }

    let pick_blue = |kind: &RuntimeRequestKind| match kind {
        RuntimeRequestKind::Question { questions } => RuntimeDecision::AnswerQuestion {
            answers: questions
                .as_array()
                .into_iter()
                .flatten()
                .map(|q| {
                    (
                        q["question"].as_str().unwrap_or("").to_string(),
                        "Blue".to_string(),
                    )
                })
                .collect(),
        },
        RuntimeRequestKind::Permission { .. } => RuntimeDecision::Deny {
            message: "unexpected permission prompt".into(),
        },
    };
    match turn(
        &session,
        &mut events,
        "Use the AskUserQuestion tool to ask me whether I prefer red or blue (options Red and Blue). Then tell me which colour I picked.",
        pick_blue,
        None,
    )
    .await
    {
        Ok((status, text, requests)) => check(
            "question",
            status == TurnStatus::Completed && requests == 1 && text.to_lowercase().contains("blue"),
            format!("{status:?} requests={requests} {:?}", text.trim()),
        ),
        Err(e) => check("question", false, e),
    }

    match turn(
        &session,
        &mut events,
        "Count from 1 to 500, writing each number out in English words, one per line. Do not use any tools.",
        deny_all,
        Some(200),
    )
    .await
    {
        Ok((status, _, _)) => check("interrupt", status == TurnStatus::Interrupted, format!("{status:?}")),
        Err(e) => check("interrupt", false, e),
    }

    match turn(
        &session,
        &mut events,
        "Reply with exactly: after-interrupt",
        deny_all,
        None,
    )
    .await
    {
        Ok((status, text, _)) => check(
            "turn after interrupt",
            status == TurnStatus::Completed && text.contains("after-interrupt"),
            format!("{status:?} {text:?}"),
        ),
        Err(e) => check("turn after interrupt", false, e),
    }

    finish(session, events, failures).await;
}

/// The composer's model and effort chips against the live CLI (TRU-143).
async fn model_scenario(
    session: &ClaudeSession,
    events: &mut UnboundedReceiver<HarnessEvent>,
    check: &mut impl FnMut(&str, bool, String),
) {
    println!("\n>> set model sonnet");
    let changed = async {
        session.send(HarnessCommand::SetModel("sonnet".into()))?;
        wait_for(events, |ev| match ev {
            HarnessEvent::ModelChanged(m) => Some(m.clone()),
            _ => None,
        })
        .await
    }
    .await;
    match changed {
        Ok(model) => check(
            "model sonnet",
            model == "sonnet",
            format!("confirmed={model:?}"),
        ),
        Err(e) => check("model sonnet", false, e),
    }
    for effort in [Some("low"), None] {
        println!("\n>> set effort {effort:?}");
        let changed = async {
            session.send(HarnessCommand::SetEffort(effort.map(str::to_string)))?;
            wait_for(events, |ev| match ev {
                HarnessEvent::EffortChanged { effort, applied } => {
                    Some((effort.clone(), applied.clone()))
                }
                _ => None,
            })
            .await
        }
        .await;
        let name = format!("effort {}", effort.unwrap_or("auto"));
        match changed {
            Ok((pinned, applied)) => check(
                &name,
                pinned.as_deref() == effort && (effort.is_none() || applied.as_deref() == effort),
                format!("effort={pinned:?} applied={applied:?}"),
            ),
            Err(e) => check(&name, false, e),
        }
    }
    println!("\n>> Reply with exactly: ok");
    let model = async {
        session.send(HarnessCommand::SendUserMessage(
            "Reply with exactly: ok".into(),
        ))?;
        let model = wait_for(events, |ev| match ev {
            HarnessEvent::TurnStarted { model, .. } => Some(model.clone()),
            _ => None,
        })
        .await?;
        wait_for(events, |ev| match ev {
            HarnessEvent::TurnCompleted { .. } => Some(()),
            _ => None,
        })
        .await?;
        Ok::<_, String>(model)
    }
    .await;
    match model {
        Ok(model) => check(
            "turn on sonnet",
            model.as_deref().is_some_and(|m| m.contains("sonnet")),
            format!("model={model:?}"),
        ),
        Err(e) => check("turn on sonnet", false, e),
    }
}

/// Shuts the session down, drains its last events, and sets the exit code.
async fn finish(
    session: ClaudeSession,
    mut events: UnboundedReceiver<HarnessEvent>,
    failures: Vec<String>,
) {
    drop(session);
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(10), events.recv()).await {
        show(&ev);
    }
    if failures.is_empty() {
        println!("\nALL PASS");
    } else {
        println!("\nFAILED: {failures:?}");
        std::process::exit(1);
    }
}

/// The scratch file with the planted bug, before and after the change.
const PLANTED_FILE: &str = "calc.py";
const PLANTED_ORIGINAL: &str = "def average(values):
    \"\"\"Return the arithmetic mean of a non-empty list of numbers.\"\"\"
    total = 0
    for v in values:
        total += v
    return total / len(values)
";
/// The uncommitted change skips the first value but still divides by the
/// full length.
const PLANTED_CHANGE: &str = "def average(values):
    \"\"\"Return the arithmetic mean of a non-empty list of numbers.\"\"\"
    total = 0
    for v in values[1:]:
        total += v
    return total / len(values)
";

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A fresh repo with one commit and the planted bug left uncommitted.
fn plant_review_repo(workdir: &Path) -> PathBuf {
    let repo = workdir.join("review-repo");
    if repo.exists() {
        eprintln!(
            "{} already exists; pass a fresh --workdir for the review scenario",
            repo.display()
        );
        std::process::exit(2);
    }
    std::fs::create_dir_all(&repo).expect("create review repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "smoke@example.com"]);
    git(&repo, &["config", "user.name", "smoke"]);
    std::fs::write(repo.join(PLANTED_FILE), PLANTED_ORIGINAL).expect("write original");
    git(&repo, &["add", PLANTED_FILE]);
    git(&repo, &["commit", "-q", "-m", "Add average"]);
    std::fs::write(repo.join(PLANTED_FILE), PLANTED_CHANGE).expect("plant the bug");
    repo
}

/// Read-only tools the reviewer may use; anything else is denied, which
/// also proves the brief's "do not edit" holds.
fn review_answer(kind: &RuntimeRequestKind) -> RuntimeDecision {
    let allow = RuntimeDecision::Allow {
        updated_input: None,
        remember: None,
    };
    match kind {
        RuntimeRequestKind::Permission {
            tool_name, input, ..
        } => match tool_name.as_str() {
            "Read" | "Grep" | "Glob" | "LS" | "Agent" | "Task" => allow,
            "Bash" => {
                let command = input["command"].as_str().unwrap_or("");
                let read_only = [
                    "git status",
                    "git diff",
                    "git ls-files",
                    "git log",
                    "git show",
                    "cat ",
                    "ls",
                ]
                .iter()
                .any(|prefix| command.trim_start().starts_with(prefix));
                if read_only {
                    allow
                } else {
                    RuntimeDecision::Deny {
                        message: format!(
                            "review smoke: only read-only git commands, not {command:?}"
                        ),
                    }
                }
            }
            other => RuntimeDecision::Deny {
                message: format!("review smoke: {other} is not a read-only tool"),
            },
        },
        RuntimeRequestKind::Question { .. } => RuntimeDecision::Deny {
            message: "review smoke: no questions expected".into(),
        },
    }
}

/// `path:line` references in `text` whose path is a file in `repo`.
fn repo_file_lines(text: &str, repo: &Path) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "`*()[],".contains(c))
        .filter_map(|word| {
            let (path, rest) = word.split_once(':')?;
            let line: String = rest.chars().take_while(char::is_ascii_digit).collect();
            (!path.is_empty() && !line.is_empty() && repo.join(path).is_file())
                .then(|| format!("{path}:{line}"))
        })
        .collect()
}

async fn review_scenario(workdir: &Path) {
    let started = Instant::now();
    let repo = plant_review_repo(workdir);
    let model = arg("--model").unwrap_or_else(|| "haiku".into());
    let reviewer_model = arg("--reviewer-model").unwrap_or_else(|| "haiku".into());
    let (session, mut events) = ClaudeSession::spawn(ClaudeSessionConfig {
        cwd: repo.clone(),
        model: Some(model.clone()),
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: Some(workdir.join("review-wire")),
        mcp_servers: Vec::new(),
    });
    let mut failures = Vec::new();
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures.push(name.to_string());
        }
    };

    match wait_for(&mut events, |ev| {
        matches!(ev, HarnessEvent::Ready { .. }).then_some(())
    })
    .await
    {
        Ok(()) => check(
            "ready",
            true,
            format!("parent model {model}, reviewer {reviewer_model}"),
        ),
        Err(e) => check("ready", false, e),
    }

    let prompt = review_prompt(&ReviewRequest {
        target: ReviewTarget::Uncommitted,
        focus: None,
        model: reviewer_model.clone(),
    })
    .expect("review prompt builds");
    println!("\n>> review prompt ({} chars)", prompt.len());
    if let Err(e) = session.send(HarnessCommand::SendUserMessage(prompt)) {
        check("send review prompt", false, e);
    }

    // Parent tool calls by id -> (name, accumulated input JSON).
    let mut parent_tools: HashMap<String, (String, String)> = HashMap::new();
    let mut parent_text = String::new();
    let mut nested: Vec<(String, HarnessEvent)> = Vec::new();
    // Subagents that reported task_started and have not finished. A
    // background subagent outlives the turn that spawned it; Claude then
    // starts a follow-up turn on its own, so wait for that one too.
    let mut running: Vec<String> = Vec::new();
    let mut turns = 0;
    let status = loop {
        let ev = match tokio::time::timeout(REVIEW_TIMEOUT, events.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => break Err("event channel closed".to_string()),
            Err(_) => break Err("timed out".to_string()),
        };
        show(&ev);
        match ev {
            HarnessEvent::TextDelta(t) => parent_text.push_str(&t),
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, .. },
            } => {
                parent_tools.insert(id, (name, String::new()));
            }
            HarnessEvent::ItemInputDelta { id, partial_json } => {
                if let Some((_, input)) = parent_tools.get_mut(&id) {
                    input.push_str(&partial_json);
                }
            }
            HarnessEvent::SubagentEvent {
                parent_tool_use_id,
                event,
            } => {
                match *event {
                    HarnessEvent::TurnStarted { .. } => running.push(parent_tool_use_id.clone()),
                    HarnessEvent::TurnCompleted { .. } => {
                        running.retain(|id| *id != parent_tool_use_id)
                    }
                    _ => {}
                }
                nested.push((parent_tool_use_id, *event));
            }
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                let decision = review_answer(&kind);
                println!(">> answer {request_id}: {decision:?}");
                if let Err(e) = session.send(HarnessCommand::Answer {
                    request_id,
                    decision,
                }) {
                    break Err(e);
                }
            }
            HarnessEvent::TurnCompleted { status, .. } => {
                turns += 1;
                if running.is_empty() || status != TurnStatus::Completed {
                    break Ok(status);
                }
                println!(
                    ">> turn ended with {} subagent(s) still running; waiting",
                    running.len()
                );
            }
            HarnessEvent::ProcessExited { code } => break Err(format!("claude exited {code:?}")),
            _ => {}
        }
    };
    match &status {
        Ok(s) => check(
            "review turn",
            *s == TurnStatus::Completed,
            format!("{s:?} after {turns} turn(s) (1 means the subagent ran in the foreground)"),
        ),
        Err(e) => check("review turn", false, e.clone()),
    }

    let agent_calls: Vec<(&String, &(String, String))> = parent_tools
        .iter()
        .filter(|(_, (name, _))| name == "Agent" || name == "Task")
        .collect();
    let agent_models: Vec<String> = agent_calls
        .iter()
        .map(|(_, (_, input))| {
            serde_json::from_str::<serde_json::Value>(input)
                .ok()
                .and_then(|v| v["model"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("<no model in {input}>"))
        })
        .collect();
    check(
        "one Agent call with the reviewer model",
        agent_calls.len() == 1 && agent_models == [reviewer_model.clone()],
        format!("{} call(s), models {agent_models:?}", agent_calls.len()),
    );
    let orphans = nested
        .iter()
        .filter(|(parent, _)| !agent_calls.iter().any(|(id, _)| *id == parent))
        .count();
    let nested_tools: Vec<String> = nested
        .iter()
        .filter_map(|(_, ev)| match ev {
            HarnessEvent::ItemStarted {
                kind: ItemKind::ToolCall { name, .. },
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let nested_completed = nested
        .iter()
        .filter(|(_, ev)| matches!(ev, HarnessEvent::ItemCompleted { .. }))
        .count();
    let nested_text: String = nested
        .iter()
        .filter_map(|(_, ev)| match ev {
            HarnessEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    check(
        "subagent events nest under the Agent call",
        !nested.is_empty() && orphans == 0,
        format!(
            "{} nested events, {orphans} without a parent Agent call",
            nested.len()
        ),
    );
    check(
        "nested events parse into tool calls and text",
        !nested_tools.is_empty() && nested_completed > 0 && !nested_text.trim().is_empty(),
        format!(
            "tools {nested_tools:?}, {nested_completed} completed, {} chars of text",
            nested_text.len()
        ),
    );
    let refs = repo_file_lines(&parent_text, &repo);
    check(
        "report has F1 and a file:line in the repo",
        parent_text.contains("F1") && !refs.is_empty(),
        format!("refs {refs:?}"),
    );
    let after = std::fs::read_to_string(repo.join(PLANTED_FILE)).unwrap_or_default();
    check(
        "reviewer edited nothing",
        after == PLANTED_CHANGE,
        format!("{PLANTED_FILE} unchanged: {}", after == PLANTED_CHANGE),
    );
    println!("\n--- parent reply ---\n{}\n---", parent_text.trim());
    println!("elapsed {:.1}s", started.elapsed().as_secs_f64());
    finish(session, events, failures).await;
}
