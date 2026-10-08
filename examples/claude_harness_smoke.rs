// TRU-140 Phase B: drive `gitterm::harness::claude::ClaudeSession` (the
// library adapter the chat tab uses) through the acceptance flow without
// the UI: a text turn, a Bash permission prompt (allowed), an
// AskUserQuestion (answered), and an interrupted long turn.
//
//   cargo run --example claude_harness_smoke -- --workdir <empty dir> [--model haiku]
//
// Prints every HarnessEvent as JSON and exits 1 if a step fails.

use std::path::PathBuf;
use std::time::Duration;

use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::{
    HarnessCommand, HarnessEvent, RuntimeDecision, RuntimeRequestKind, TurnStatus,
};
use tokio::sync::mpsc::UnboundedReceiver;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn show(ev: &HarnessEvent) {
    match ev {
        HarnessEvent::TextDelta(_) | HarnessEvent::ItemInputDelta { .. } => {}
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let workdir = PathBuf::from(arg("--workdir").expect("--workdir <dir> is required"));
    std::fs::create_dir_all(&workdir).expect("create workdir");
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
