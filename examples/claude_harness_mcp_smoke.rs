// TRU-140: prove a native `ClaudeSession` (the chat-tab adapter) reaches
// GitTerm's task and browser MCP servers headless. Both servers run
// in-process on their V5 loopback ranges, attached exactly as the chat tab
// attaches them (`claude_mcp_server()`), so the bearer tokens travel only
// through the child environment. Claude must call `task_list` with every
// permission prompt denied (it is pre-approved) and name the browser tools.
// The wire log's `system/init` frame also lists both servers' status.
//
//   env -u GITTERM_V5_TASK_MCP_TOKEN cargo run --example claude_harness_mcp_smoke -- \
//       --workdir <empty dir> [--model haiku]
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
use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use gitterm::harness::{HarnessCommand, HarnessEvent, RuntimeDecision, TurnStatus};
use gitterm::task_mcp::{self, TaskControlOperation};
use tokio::sync::mpsc;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);
const SENTINEL: &str = "mcp-smoke-sentinel-7f3a";
// The chat tab's durable session_uid (TRU-142 S1): carried as `?caller=` on
// the task MCP URL and expected on every bridged call.
const CALLER: &str = "mcp-smoke-caller-5b21";

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let workdir = PathBuf::from(arg("--workdir").expect("--workdir <dir> is required"));
    std::fs::create_dir_all(&workdir).expect("create workdir");

    let (task_commands, mut task_requests) = mpsc::unbounded_channel();
    let (task, task_server) =
        task_mcp::prepare("mcp-smoke", task_commands).expect("reserve task MCP endpoint");
    tokio::spawn(task_server.run());
    let task_lists = Arc::new(AtomicUsize::new(0));
    let counted = task_lists.clone();
    let attributed_lists = Arc::new(AtomicUsize::new(0));
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

    let browser_dir = workdir.with_extension("browser");
    let (browser, browser_server) =
        browser_mcp::prepare(&browser_dir, "mcp-smoke").expect("reserve browser MCP endpoint");
    tokio::spawn(browser_server.run());
    println!(
        "task MCP {} / browser MCP {}",
        task.endpoint(),
        browser.endpoint()
    );

    let config = ClaudeSessionConfig {
        cwd: workdir.clone(),
        model: Some(arg("--model").unwrap_or_else(|| "haiku".into())),
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: Some(workdir.with_extension("wire")),
        mcp_servers: vec![
            task.claude_mcp_server(Some(CALLER)),
            browser.claude_mcp_server(),
        ],
    };
    println!("argv: {}", config.args().join(" "));
    let (session, mut events) = ClaudeSession::spawn(config);

    let prompt = "Call the mcp__gitterm_tasks__task_list tool once and reply with the title \
                  of each task it returns. Then, on one line, list the names of every tool \
                  you have whose name starts with mcp__gitterm_browser__. Do not call any \
                  other tool.";
    println!(">> {prompt}");
    session
        .send(HarnessCommand::SendUserMessage(prompt.into()))
        .expect("send prompt");

    let mut text = String::new();
    let mut prompts = 0;
    let status = loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, events.recv())
            .await
            .expect("timed out waiting for Claude")
            .expect("event channel closed");
        match ev {
            HarnessEvent::TextDelta(t) => text.push_str(&t),
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                prompts += 1;
                println!("[event] unexpected runtime request {kind:?}; denying");
                session
                    .send(HarnessCommand::Answer {
                        request_id,
                        decision: RuntimeDecision::Deny {
                            message: "the smoke test expects pre-approved MCP tools".into(),
                        },
                    })
                    .expect("deny");
            }
            HarnessEvent::TurnCompleted { status, .. } => break status,
            HarnessEvent::ProcessExited { code } => panic!("claude exited {code:?}"),
            other => println!(
                "[event] {}",
                serde_json::to_string(&other).unwrap_or_default()
            ),
        }
    };
    println!("\n<< {text}\n");
    drop(session);
    let lists = task_lists.load(Ordering::SeqCst);
    let attributed_lists = attributed_lists.load(Ordering::SeqCst);

    let checks = [
        ("turn completed", status == TurnStatus::Completed),
        ("task_list reached the task server", lists >= 1),
        (
            "every task_list carried the chat tab's caller",
            attributed_lists == lists,
        ),
        ("reply carries the task title", text.contains(SENTINEL)),
        (
            "reply names browser tools",
            // Models often drop the `mcp__gitterm_browser__` prefix when
            // listing; these names exist only on the browser server.
            text.contains("browser_target_diagnostics") && text.contains("browser_dom_outline"),
        ),
        ("no permission prompt for pre-approved tools", prompts == 0),
    ];
    let mut failed = false;
    for (name, ok) in checks {
        println!("{} {name}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    }
    if failed {
        std::process::exit(1);
    }
}
