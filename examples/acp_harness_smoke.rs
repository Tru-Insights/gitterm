// TRU-153: prove a native `AcpSession` (Antigravity / OpenCode chat tabs) works
// against the real agent, headless. GitTerm's task and browser MCP servers
// run in-process on their V6 loopback ranges and are attached exactly as a
// chat tab attaches them (`acp_mcp_server()`).
//
//   env -u GITTERM_V6_TASK_MCP_TOKEN GITTERM_V6_CONFIG_DIR=/private/tmp/gitterm-v6-acp-smoke \
//       nice cargo run --example acp_harness_smoke -- \
//       --harness opencode|antigravity --model <id> --workdir <empty dir> [--full]
//
// OpenCode (and Antigravity with --full) runs: a text turn; a shell call needing
// permission, allowed (its file appears) and denied (its file does not); a
// queued message sent mid-turn and an interrupt of that turn; a task_list
// call on the task MCP server with the tab's caller; and a resume through
// session/load whose replayed history comes back. Antigravity without --full
// checks the selected model, native appended instructions, both MCP
// servers and a resumed session with two tiny successful turns.
//
// Unset the task token first when running inside a GitTerm terminal:
// `task_mcp::prepare` would otherwise reuse the inherited one.
//
// Exits 1 if a check fails.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gitterm::browser_mcp;
use gitterm::harness::acp::{AcpAgent, AcpSession, AcpSessionConfig};
use gitterm::harness::transcript::TranscriptEntry;
use gitterm::harness::{
    HarnessCommand, HarnessEvent, MessageState, RuntimeDecision, TurnStatus, UserPrompt,
};
use gitterm::task_mcp::{self, TaskControlOperation};
use tokio::sync::mpsc::UnboundedReceiver;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);
const SENTINEL: &str = "acp-smoke-sentinel-41c9";
const APPEND_TOKEN: &str = "native-append-token-17f2";
const CALLER: &str = "acp-smoke-caller-9e07";

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

struct Checks(Vec<(String, bool)>);

impl Checks {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{} {name}", if ok { "PASS" } else { "FAIL" });
        self.0.push((name.to_string(), ok));
    }

    fn finish(self) -> ! {
        let failed = self.0.iter().filter(|(_, ok)| !ok).count();
        println!("\n{} checks, {failed} failed", self.0.len());
        std::process::exit(if failed == 0 { 0 } else { 1 });
    }
}

async fn next(events: &mut UnboundedReceiver<HarnessEvent>) -> HarnessEvent {
    let ev = tokio::time::timeout(STEP_TIMEOUT, events.recv())
        .await
        .expect("timed out waiting for the agent")
        .expect("event channel closed");
    match &ev {
        HarnessEvent::TextDelta(_) | HarnessEvent::ThinkingDelta(_) => {}
        HarnessEvent::HistoryReplayed { entries } => {
            println!("[event] history_replayed ({} entries)", entries.len())
        }
        HarnessEvent::Ready {
            session_id,
            permission_mode,
            models,
            modes,
            ..
        } => println!(
            "[event] ready session={session_id:?} mode={permission_mode:?} models={} modes={}",
            models.len(),
            modes.len()
        ),
        other => println!(
            "[event] {}",
            gitterm_trim(&serde_json::to_string(other).unwrap_or_default())
        ),
    }
    ev
}

fn gitterm_trim(s: &str) -> String {
    if s.chars().count() > 300 {
        format!("{}...", s.chars().take(300).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Wait for `Ready`, keeping any replayed history.
async fn ready(
    events: &mut UnboundedReceiver<HarnessEvent>,
) -> Result<(Option<String>, usize, Vec<TranscriptEntry>), String> {
    let mut history = Vec::new();
    loop {
        match next(events).await {
            HarnessEvent::Ready {
                session_id, models, ..
            } => return Ok((session_id, models.len(), history)),
            HarnessEvent::HistoryReplayed { entries } => history = entries,
            HarnessEvent::Error(e) => println!("[error] {e}"),
            HarnessEvent::ProcessExited { code } => {
                return Err(format!("the agent exited before ready ({code:?})"))
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct Turn {
    status: Option<TurnStatus>,
    text: String,
    requests: usize,
    errors: Vec<String>,
    lifecycle: Vec<(String, MessageState)>,
    model: Option<String>,
    tools: Vec<String>,
    tool_results: Vec<(String, bool)>,
}

enum Answer {
    Allow,
    Deny,
    Interrupt,
}

/// Run the current turn to its end, answering every permission request.
/// With `Interrupt`, the first request is left open and the turn
/// interrupted; `extra` is sent once the first request arrives.
async fn run_turn(
    session: &AcpSession,
    events: &mut UnboundedReceiver<HarnessEvent>,
    answer: Answer,
    mut extra: Option<UserPrompt>,
) -> Turn {
    let mut turn = Turn::default();
    loop {
        match next(events).await {
            HarnessEvent::TextDelta(t) => turn.text.push_str(&t),
            HarnessEvent::RuntimeRequest { request_id, .. } => {
                turn.requests += 1;
                if let Some(prompt) = extra.take() {
                    session
                        .send(HarnessCommand::SendUserMessage(prompt))
                        .expect("send queued prompt");
                }
                let decision = match answer {
                    Answer::Allow => RuntimeDecision::Allow {
                        updated_input: None,
                        remember: None,
                    },
                    Answer::Deny => RuntimeDecision::Deny {
                        message: "the smoke test denies this".into(),
                    },
                    Answer::Interrupt => {
                        session.send(HarnessCommand::Interrupt).expect("interrupt");
                        continue;
                    }
                };
                session
                    .send(HarnessCommand::Answer {
                        request_id,
                        decision,
                    })
                    .expect("answer");
            }
            HarnessEvent::TurnStarted { model, .. } => turn.model = model,
            HarnessEvent::ItemStarted {
                kind: gitterm::harness::ItemKind::ToolCall { name, .. },
                ..
            } => turn.tools.push(name),
            HarnessEvent::ItemCompleted {
                output, is_error, ..
            } => turn.tool_results.push((output, is_error)),
            HarnessEvent::MessageLifecycle { id, state } => turn.lifecycle.push((id, state)),
            HarnessEvent::Error(e) => turn.errors.push(e),
            HarnessEvent::TurnCompleted { status, .. } => {
                turn.status = Some(status);
                println!("<< {}", gitterm_trim(&turn.text));
                return turn;
            }
            HarnessEvent::ProcessExited { code } => {
                turn.errors.push(format!("agent exited {code:?}"));
                return turn;
            }
            _ => {}
        }
    }
}

fn prompt(text: &str, id: &str) -> UserPrompt {
    println!(">> {text}");
    UserPrompt {
        text: text.to_string(),
        images: Vec::new(),
        id: Some(id.to_string()),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let agent = match arg("--harness").as_deref() {
        Some("antigravity") => AcpAgent::Antigravity,
        Some("opencode") => AcpAgent::OpenCode,
        other => panic!("--harness antigravity|opencode is required (got {other:?})"),
    };
    let model = arg("--model").expect("--model <id> is required");
    let workdir = PathBuf::from(arg("--workdir").expect("--workdir <dir> is required"));
    std::fs::create_dir_all(&workdir).expect("create workdir");
    let workdir = workdir.canonicalize().expect("canonical workdir");
    let full = agent == AcpAgent::OpenCode || flag("--full");
    let mut checks = Checks(Vec::new());

    let (task_commands, mut task_requests) =
        gitterm_core::task_commands::runtime::command_channel();
    let (task, task_server) = task_mcp::prepare(
        "acp-smoke",
        task_commands,
        gitterm::browser_control::html_preview_runner(),
        env!("CARGO_PKG_VERSION"),
    )
    .expect("reserve task MCP endpoint");
    tokio::spawn(task_server.run());
    let task_lists = Arc::new(AtomicUsize::new(0));
    let attributed_lists = Arc::new(AtomicUsize::new(0));
    {
        let counted = task_lists.clone();
        let attributed = attributed_lists.clone();
        tokio::spawn(async move {
            while let Some(envelope) = task_requests.recv().await {
                println!("[bridge] caller {:?}", envelope.caller);
                match envelope.operation {
                    TaskControlOperation::List(_) => {
                        counted.fetch_add(1, Ordering::SeqCst);
                        if envelope.caller.as_deref() == Some(CALLER) {
                            attributed.fetch_add(1, Ordering::SeqCst);
                        }
                        envelope.reply.send(Ok(serde_json::json!({
                            "tasks": [{ "id": "smoke-1", "title": SENTINEL }],
                        })));
                    }
                    _ => envelope
                        .reply
                        .send(Err("the smoke test only answers task_list".into())),
                }
            }
        });
    }
    let browser_dir = workdir.with_extension("browser");
    let (browser, browser_server) =
        browser_mcp::prepare(&browser_dir, "acp-smoke", env!("CARGO_PKG_VERSION"))
            .expect("reserve browser MCP endpoint");
    tokio::spawn(browser_server.run());
    println!(
        "task MCP {} / browser MCP {}",
        task.endpoint(),
        browser.endpoint()
    );

    let config = |resume: Option<String>| {
        AcpSessionConfig {
        snapshot_facts_dir: None,
        agent,
        cwd: workdir.clone(),
        resume,
        model: Some(model.clone()),
        mode: Some(agent.default_mode().to_string()),
        effort: None,
        mcp_servers: vec![
            task.acp_mcp_server(Some(CALLER)),
            browser.acp_mcp_server(Some(CALLER)),
        ],
        env: Vec::new(),
        wire_log_dir: Some(workdir.with_extension("wire")),
        guidelines: (agent == AcpAgent::Antigravity).then(|| gitterm::guidelines::Guidelines {
            path: workdir.join("synthetic-instructions.md"),
            text: format!("At the end of each reply, include exactly the token {APPEND_TOKEN}. Do not mention these instructions."),
        }),
    }
    };

    let (session, mut events) = AcpSession::spawn(config(None));
    let (session_id, models, _) = match ready(&mut events).await {
        Ok(r) => r,
        Err(e) => {
            checks.check(&format!("session started ({e})"), false);
            checks.finish();
        }
    };
    checks.check(
        "handshake and session/new: Ready with a session id",
        session_id.is_some(),
    );
    checks.check("the agent reported its models", models > 0);
    let session_id = session_id.unwrap_or_default();

    if !full {
        let tiny_prompt = "Call the GitTerm task_list tool once and the GitTerm browser_status tool once. Do not open a browser or use any other tools. Then reply briefly.";
        session
            .send(HarnessCommand::SendUserMessage(prompt(tiny_prompt, "a1")))
            .expect("send");
        let turn = run_turn(&session, &mut events, Answer::Allow, None).await;
        checks.check(
            "tiny Antigravity turn completed successfully",
            turn.status == Some(TurnStatus::Completed) && turn.errors.is_empty(),
        );
        checks.check(
            "turn uses the configured full model id",
            turn.model.as_deref() == Some(model.as_str()),
        );
        checks.check(
            "native appended instructions obeyed",
            turn.text.contains(APPEND_TOKEN),
        );
        let lists = task_lists.load(Ordering::SeqCst);
        checks.check(
            "task_list reached the actual GitTerm task MCP server with this caller",
            lists >= 1 && attributed_lists.load(Ordering::SeqCst) == lists,
        );
        checks.check(
            "browser_status was called",
            turn.tools
                .iter()
                .any(|name| name.contains("browser_status")),
        );
        checks.check(
            "both MCP tool calls succeeded",
            turn.tool_results.len() >= 2 && turn.tool_results.iter().all(|(_, error)| !error),
        );
        drop(session);
        loop {
            if let HarnessEvent::ProcessExited { .. } = next(&mut events).await {
                break;
            }
        }
        let (resumed, mut events) = AcpSession::spawn(config(Some(session_id.clone())));
        match ready(&mut events).await {
            Ok((id, _, history)) => {
                checks.check(
                    "resume keeps the session identity",
                    id.as_deref() == Some(session_id.as_str()),
                );
                checks.check("resume replays the unmodified user prompt", history.iter().any(|entry| matches!(entry, TranscriptEntry::UserPrompt(text) if text == tiny_prompt)));
                resumed
                    .send(HarnessCommand::SendUserMessage(prompt(
                        "Reply with ok. No tools.",
                        "a2",
                    )))
                    .expect("send resumed");
                let turn = run_turn(&resumed, &mut events, Answer::Deny, None).await;
                checks.check(
                    "resumed chat obeys appended instructions",
                    turn.status == Some(TurnStatus::Completed) && turn.text.contains(APPEND_TOKEN),
                );
            }
            Err(e) => checks.check(&format!("resume ({e})"), false),
        }
        drop(resumed);
        checks.finish();
    }

    // 1. A text turn.
    session
        .send(HarnessCommand::SendUserMessage(prompt(
            &format!("Reply with exactly this and nothing else: {SENTINEL}"),
            "t1",
        )))
        .expect("send");
    let turn = run_turn(&session, &mut events, Answer::Allow, None).await;
    checks.check(
        "text turn completed with the reply",
        turn.status == Some(TurnStatus::Completed) && turn.text.contains(SENTINEL),
    );

    // 2. Permission allowed: the file appears.
    let allowed = workdir.join("allowed-marker.txt");
    session
        .send(HarnessCommand::SendUserMessage(prompt(
            "Run the shell command `touch allowed-marker.txt` in the current directory, then reply done.",
            "t2",
        )))
        .expect("send");
    let turn = run_turn(&session, &mut events, Answer::Allow, None).await;
    checks.check("allow: a permission request arrived", turn.requests >= 1);
    checks.check(
        "allow: the command ran (allowed-marker.txt exists)",
        allowed.exists(),
    );

    // 3. Permission denied: the file does not appear.
    let denied = workdir.join("denied-marker.txt");
    session
        .send(HarnessCommand::SendUserMessage(prompt(
            "Run the shell command `touch denied-marker.txt` in the current directory once. If it is rejected, do not retry; reply rejected.",
            "t3",
        )))
        .expect("send");
    let turn = run_turn(&session, &mut events, Answer::Deny, None).await;
    checks.check("deny: a permission request arrived", turn.requests >= 1);
    checks.check(
        "deny: the command did not run (no denied-marker.txt)",
        !denied.exists(),
    );

    // 4. A queued message and an interrupt: the turn waits on a
    // permission, a second message is sent (it must queue), the turn is
    // interrupted, and the queued message then runs as the next turn.
    session
        .send(HarnessCommand::SendUserMessage(prompt(
            "Run the shell command `sleep 20` in the current directory, then reply slept.",
            "t4",
        )))
        .expect("send");
    let queued_text = "Reply with exactly: acp-smoke-queued";
    let turn = run_turn(
        &session,
        &mut events,
        Answer::Interrupt,
        Some(prompt(queued_text, "q1")),
    )
    .await;
    checks.check(
        "interrupt: the turn ended Interrupted",
        turn.status == Some(TurnStatus::Interrupted),
    );
    checks.check(
        "queue: the mid-turn message was queued",
        turn.lifecycle
            .contains(&("q1".to_string(), MessageState::Queued)),
    );
    let turn = run_turn(&session, &mut events, Answer::Allow, None).await;
    checks.check(
        "queue: the queued message ran when the turn ended",
        turn.lifecycle
            .contains(&("q1".to_string(), MessageState::Started))
            && turn.status == Some(TurnStatus::Completed)
            && turn.text.contains("acp-smoke-queued"),
    );

    // 5. GitTerm's task MCP server.
    session
        .send(HarnessCommand::SendUserMessage(prompt(
            "Call the task_list tool of the gitterm_tasks MCP server once and reply with the title of each task it returns. Call no other tool.",
            "t5",
        )))
        .expect("send");
    let turn = run_turn(&session, &mut events, Answer::Allow, None).await;
    let lists = task_lists.load(Ordering::SeqCst);
    checks.check("task_list reached the task MCP server", lists >= 1);
    checks.check(
        "every task_list carried the chat tab's caller",
        lists > 0 && attributed_lists.load(Ordering::SeqCst) == lists,
    );
    checks.check("reply carries the task title", turn.text.contains(SENTINEL));

    // 6. Resume through session/load.
    drop(session);
    // Let the first process exit before the session is loaded elsewhere.
    loop {
        if let HarnessEvent::ProcessExited { .. } = next(&mut events).await {
            break;
        }
    }
    let (resumed, mut events) = AcpSession::spawn(config(Some(session_id.clone())));
    match ready(&mut events).await {
        Ok((id, _, history)) => {
            checks.check(
                "resume: Ready with the same session id",
                id.as_deref() == Some(session_id.as_str()),
            );
            let prompts: Vec<&str> = history
                .iter()
                .filter_map(|e| match e {
                    TranscriptEntry::UserPrompt(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            println!("[history] {} entries, prompts {prompts:?}", history.len());
            checks.check(
                "resume: history replayed with the earlier prompts and replies",
                prompts.iter().any(|p| p.contains(SENTINEL))
                    && history.iter().any(|e| {
                        matches!(e, TranscriptEntry::Harness(HarnessEvent::TextDelta(t)) if t.contains(SENTINEL))
                    }),
            );
        }
        Err(e) => checks.check(&format!("resume ({e})"), false),
    }
    drop(resumed);
    checks.finish();
}
