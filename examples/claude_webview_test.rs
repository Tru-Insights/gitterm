// Hour-2 spike: verify the Rust ↔ webview event pipe.
//
// Opens a native window with a wry WebView that renders a minimal chat UI.
// The user types a prompt, hits Cmd+Enter. The IPC handler forwards the prompt
// to a background tokio runtime, which spawns `claude --print --output-format
// stream-json` and streams each event line back to the main thread via an
// EventLoopProxy. The main thread calls `webview.evaluate_script(...)` to
// inject each event as JSON into the page, where JS renders it.
//
// Everything is deliberately isolated from gitterm's main app — this is
// throwaway proof-of-concept code. If it works and feels right, we'll design
// a real integration.
//
// Run with:
//   cargo run --example claude_webview_test

use std::process::Stdio;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use wry::http::Request;
use wry::WebViewBuilder;

#[derive(Debug)]
enum AppEvent {
    ClaudeLine(String),
    ClaudeDone,
    ClaudeError(String),
}

const HTML: &str = include_str!("claude_webview_test.html");

fn main() -> wry::Result<()> {
    let event_loop: EventLoop<AppEvent> =
        EventLoopBuilder::<AppEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("Claude webview spike")
        .with_inner_size(tao::dpi::LogicalSize::new(900.0, 700.0))
        .build(&event_loop)
        .expect("window");

    let proxy = event_loop.create_proxy();

    // Channel: main thread (IPC handler) → tokio thread (subprocess manager)
    let (input_tx, input_rx) =
        tokio::sync::mpsc::unbounded_channel::<String>();

    // Background tokio runtime, owns the input channel and subprocess lifecycle.
    {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(tokio_main(input_rx, proxy));
        });
    }

    // Build the webview. The IPC handler captures input_tx and forwards
    // user submissions to the tokio thread.
    let webview = WebViewBuilder::new()
        .with_html(HTML)
        .with_devtools(true)
        .with_ipc_handler(move |req: Request<String>| {
            let body = req.body();
            match serde_json::from_str::<serde_json::Value>(body) {
                Ok(v) => {
                    if v.get("type").and_then(|t| t.as_str()) == Some("submit") {
                        if let Some(text) =
                            v.get("text").and_then(|t| t.as_str())
                        {
                            if !text.trim().is_empty() {
                                let _ = input_tx.send(text.to_string());
                            }
                        }
                    }
                }
                Err(e) => eprintln!("[ipc] parse error: {} body={}", e, body),
            }
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(AppEvent::ClaudeLine(line)) => {
                // Embed the raw JSON line directly — the JS side will JSON.parse.
                // We have to base64-or-JSON-encode to pass safely; simplest is
                // to pass it as a JSON string argument that JS then parses.
                let script = format!(
                    "window.__appendEventLine({})",
                    serde_json::Value::String(line)
                );
                if let Err(e) = webview.evaluate_script(&script) {
                    eprintln!("[eval] error: {}", e);
                }
            }
            Event::UserEvent(AppEvent::ClaudeDone) => {
                let _ = webview.evaluate_script("window.__claudeDone()");
            }
            Event::UserEvent(AppEvent::ClaudeError(msg)) => {
                let script = format!(
                    "window.__claudeError({})",
                    serde_json::Value::String(msg)
                );
                let _ = webview.evaluate_script(&script);
            }
            _ => {}
        }
    });
}

async fn tokio_main(
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    proxy: EventLoopProxy<AppEvent>,
) {
    // Captured from the first `system:init` event and reused for --resume
    // on every subsequent turn so the conversation has context.
    let mut session_id: Option<String> = None;
    while let Some(prompt) = input_rx.recv().await {
        eprintln!(
            "[tokio] prompt (resume={:?}): {}",
            session_id.as_deref(),
            prompt
        );
        match run_claude(&prompt, &proxy, session_id.as_deref()).await {
            Ok(Some(sid)) if session_id.is_none() => session_id = Some(sid),
            Ok(_) => {}
            Err(e) => {
                eprintln!("[tokio] error: {}", e);
                let _ = proxy.send_event(AppEvent::ClaudeError(e.to_string()));
                let _ = proxy.send_event(AppEvent::ClaudeDone);
            }
        }
    }
}

async fn run_claude(
    prompt: &str,
    proxy: &EventLoopProxy<AppEvent>,
    resume_session: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "haiku".into());
    // Spike-only permission handling: non-interactive --print mode can't
    // prompt, so we pick a mode up-front. Default to bypassPermissions so
    // the agent can actually *do* things while we evaluate the experience.
    // Override: CLAUDE_PERMISSION_MODE=default|acceptEdits|bypassPermissions|plan
    let permission_mode = std::env::var("CLAUDE_PERMISSION_MODE")
        .unwrap_or_else(|_| "bypassPermissions".into());
    // Effort controls thinking budget. Opus is particularly selective about
    // when to think at default effort; bump to "high" or "max" to force it.
    let effort = std::env::var("CLAUDE_EFFORT").ok();
    eprintln!(
        "[tokio] spawning claude (model={} permission_mode={} effort={:?})",
        model, permission_mode, effort
    );

    let mut cmd = Command::new("claude");
    cmd.arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .arg("--permission-mode")
        .arg(&permission_mode)
        .arg("--model")
        .arg(&model);
    if let Some(e) = effort.as_deref() {
        cmd.arg("--effort").arg(e);
    }
    if let Some(sid) = resume_session {
        cmd.arg("--resume").arg(sid);
    }
    cmd.arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or("child stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut captured_session: Option<String> = None;
    while let Some(line) = lines.next_line().await? {
        // Capture session_id from the first `system:init` event so we can
        // --resume on subsequent turns.
        if captured_session.is_none() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let is_init = v.get("type").and_then(|t| t.as_str()) == Some("system")
                    && v.get("subtype").and_then(|t| t.as_str()) == Some("init");
                if is_init {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        captured_session = Some(sid.to_string());
                    }
                }
            }
        }
        if proxy
            .send_event(AppEvent::ClaudeLine(line))
            .is_err()
        {
            // Main thread is gone, bail out.
            break;
        }
    }
    let _ = child.wait().await?;
    let _ = proxy.send_event(AppEvent::ClaudeDone);
    Ok(captured_session)
}
