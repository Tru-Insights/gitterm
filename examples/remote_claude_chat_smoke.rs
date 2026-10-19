// TRU-152: drive a native Claude chat through a GitTerm agent on loopback,
// the way a chat tab in a remote workspace does. Starts an in-process agent
// on 127.0.0.1, connects with `ClaudeSession::spawn_remote`, runs one turn
// in a temp dir that needs a Bash permission prompt (answered over the
// relay), checks TurnCompleted and the file the command made, then fetches
// `ClaudeChatHistory` for the session and checks the turn is in it.
//
//   cargo run --example remote_claude_chat_smoke -- [--model haiku]
//
// Uses this machine's own Claude login. Prints PASS/FAIL per step and exits
// 1 if any step fails.

use std::time::Duration;

use gitterm::agentd::client::{RemoteAgentBackend, RemoteAgentClientConfig};
use gitterm::agentd::server::{self, AgentServerConfig};
use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::transcript::TranscriptEntry;
use gitterm::harness::{
    HarnessCommand, HarnessEvent, ItemKind, RuntimeDecision, RuntimeRequestKind, TurnStatus,
};
use tokio::sync::mpsc::UnboundedReceiver;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN_ENV: &str = "GITTERM_REMOTE_CHAT_SMOKE_TOKEN";
const PROBE_FILE: &str = "gitterm-remote-probe.txt";

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn show(ev: &HarnessEvent) {
    match ev {
        HarnessEvent::TextDelta(_)
        | HarnessEvent::ThinkingDelta(_)
        | HarnessEvent::ItemInputDelta { .. } => {}
        HarnessEvent::Ready { models, .. } => {
            println!("[event] ready ({} models)", models.len())
        }
        other => println!(
            "[event] {}",
            serde_json::to_string(other).unwrap_or_default()
        ),
    }
}

async fn next_event(events: &mut UnboundedReceiver<HarnessEvent>) -> Result<HarnessEvent, String> {
    let ev = tokio::time::timeout(STEP_TIMEOUT, events.recv())
        .await
        .map_err(|_| "timed out".to_string())?
        .ok_or("event channel closed")?;
    show(&ev);
    Ok(ev)
}

#[tokio::main]
async fn main() {
    let model = arg("--model").unwrap_or_else(|| "haiku".into());
    let workdir = tempfile::tempdir().expect("temp dir");
    let mut failures: Vec<String> = Vec::new();
    // A macro rather than a closure, so `failures` is only borrowed per
    // call and stays free for `finish`.
    macro_rules! check {
        ($name:expr, $ok:expr, $detail:expr $(,)?) => {
            record(&mut failures, $name, $ok, $detail)
        };
    }

    // A free loopback port for the in-process agent.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free loopback port");
    let token = uuid::Uuid::new_v4().to_string();
    std::env::set_var(TOKEN_ENV, &token);
    let agent = tokio::spawn(server::serve(AgentServerConfig {
        bind_addr: addr,
        auth_token: token,
        agent_name: "remote-chat-smoke".into(),
        claude_chat_limits: Default::default(),
    }));
    let client = RemoteAgentClientConfig {
        remote_id: "smoke".into(),
        name: "smoke agent".into(),
        endpoint: format!("http://{addr}"),
        token_ref: format!("env:{TOKEN_ENV}"),
    };

    // The agent needs a moment to bind; the handshake doubles as the wait.
    let backend = RemoteAgentBackend::new(client.clone());
    let mut handshake = Err("not tried".to_string());
    for _ in 0..50 {
        handshake = backend.handshake().await.map_err(|e| e.to_string());
        if handshake.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match &handshake {
        Ok(h) => check!(
            "handshake advertises claude_chat",
            h.capabilities.iter().any(|c| c == "claude_chat"),
            format!("capabilities={:?}", h.capabilities),
        ),
        Err(e) => {
            check!("handshake advertises claude_chat", false, e.clone());
            finish(failures);
        }
    }

    let cwd = workdir.path().canonicalize().expect("canonical temp dir");
    let (session, mut events) = ClaudeSession::spawn_remote(
        client,
        ClaudeSessionConfig {
            cwd: cwd.clone(),
            model: Some(model),
            permission_mode: "default".into(),
            effort: None,
            resume: None,
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: false,
        },
    );

    let prompt = format!("Use the Bash tool to run `touch {PROBE_FILE}`. Then reply with done.");
    println!("\n>> {prompt}");
    if let Err(e) = session.send(HarnessCommand::SendUserMessage(prompt.as_str().into())) {
        check!("send prompt", false, e);
        finish(failures);
    }
    let mut session_id: Option<String> = None;
    let mut permission_tool: Option<String> = None;
    let mut bash_ok = false;
    let outcome: Result<TurnStatus, String> = loop {
        let ev = match next_event(&mut events).await {
            Ok(ev) => ev,
            Err(e) => break Err(e),
        };
        match ev {
            HarnessEvent::TurnStarted {
                session_id: Some(id),
                ..
            } => session_id = Some(id),
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                let decision = match &kind {
                    RuntimeRequestKind::Permission { tool_name, .. } => {
                        permission_tool = Some(tool_name.clone());
                        RuntimeDecision::Allow {
                            updated_input: None,
                            remember: None,
                        }
                    }
                    RuntimeRequestKind::Question { .. } => RuntimeDecision::Deny {
                        message: "smoke test did not expect a question".into(),
                    },
                };
                println!(">> answer {request_id}: {decision:?}");
                if let Err(e) = session.send(HarnessCommand::Answer {
                    request_id,
                    decision,
                }) {
                    break Err(e);
                }
            }
            HarnessEvent::ItemCompleted { is_error, .. } if !is_error => bash_ok = true,
            HarnessEvent::ItemStarted {
                kind: ItemKind::ToolCall { name, .. },
                ..
            } => println!(">> tool call {name}"),
            HarnessEvent::TurnCompleted { status, .. } => break Ok(status),
            HarnessEvent::Error(e) => println!(">> error event: {e}"),
            HarnessEvent::ProcessExited { code } => break Err(format!("claude exited {code:?}")),
            _ => {}
        }
    };
    check!(
        "permission request relayed",
        permission_tool.as_deref() == Some("Bash"),
        format!("tool={permission_tool:?}"),
    );
    match &outcome {
        Ok(status) => check!(
            "turn completed",
            *status == TurnStatus::Completed && bash_ok,
            format!("status={status:?} tool ok={bash_ok}"),
        ),
        Err(e) => check!("turn completed", false, e.clone()),
    }
    check!(
        "command ran in the agent's cwd",
        cwd.join(PROBE_FILE).is_file(),
        cwd.join(PROBE_FILE).display().to_string(),
    );

    // The agent owns the chat (TRU-152): dropping the handle only
    // detaches, so stop it explicitly, as closing the tab does. Its exit
    // comes back.
    if let Err(e) = session.send(HarnessCommand::Shutdown) {
        check!("shutdown stops the remote process", false, e);
    }
    let mut exited = None;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(20), events.recv()).await {
        show(&ev);
        if let HarnessEvent::ProcessExited { code } = ev {
            exited = Some(code);
            break;
        }
    }
    drop(session);
    check!(
        "shutdown stops the remote process",
        exited.is_some(),
        format!("exit={exited:?}"),
    );

    match session_id {
        Some(id) => match backend.claude_chat_history(id.clone()).await {
            Ok(Some(timeline)) => {
                let has_prompt = timeline.iter().any(|timed| {
                    matches!(&timed.entry, TranscriptEntry::UserPrompt(text) if text.contains(PROBE_FILE))
                });
                let has_bash = timeline.iter().any(|timed| {
                    matches!(
                        &timed.entry,
                        TranscriptEntry::Harness(HarnessEvent::ItemStarted {
                            kind: ItemKind::ToolCall { name, .. },
                            ..
                        }) if name == "Bash"
                    )
                });
                check!(
                    "history has the turn",
                    has_prompt && has_bash,
                    format!(
                        "{} entries, prompt={has_prompt} bash={has_bash}",
                        timeline.len()
                    ),
                );
            }
            Ok(None) => check!("history has the turn", false, format!("{id}: not found")),
            Err(e) => check!("history has the turn", false, e.to_string()),
        },
        None => check!(
            "history has the turn",
            false,
            "no session id seen".to_string(),
        ),
    }
    match backend
        .claude_chat_history(uuid::Uuid::new_v4().to_string())
        .await
    {
        Ok(None) => check!(
            "history of an unknown session is not found",
            true,
            String::new(),
        ),
        other => check!(
            "history of an unknown session is not found",
            false,
            format!("{:?}", other.map(|t| t.map(|t| t.len()))),
        ),
    }

    agent.abort();
    finish(failures);
}

fn record(failures: &mut Vec<String>, name: &str, ok: bool, detail: String) {
    println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        failures.push(name.to_string());
    }
}

fn finish(failures: Vec<String>) -> ! {
    if failures.is_empty() {
        println!("\nALL PASS");
        std::process::exit(0);
    }
    println!("\nFAILED: {failures:?}");
    std::process::exit(1);
}
