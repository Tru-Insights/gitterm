// pi-harness webview spike — parallel to claude_webview_test, same UX shape
// but drives `pi --print --mode json` instead of `claude`.
//
// Multi-turn: pi uses a session file (`--session <path>`). We point every
// subprocess at the same tmp file for the lifetime of the window, so each
// submit continues the prior conversation.
//
// Run with:
//   cargo run --example pi_webview_test
//
// Optional env:
//   PI_MODEL           (default: openai-codex/gpt-5.4-mini)
//   PI_SESSION_PATH    (default: /tmp/pi-spike-session-<pid>.jsonl)
//   PI_THINKING        (off|minimal|low|medium|high|xhigh — passed as --thinking)

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use muda::{Menu, PredefinedMenuItem, Submenu};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use wry::http::Request;
use wry::WebViewBuilder;

// Shared stop-signal slot between the IPC handler and the active turn.
// Each turn creates a fresh oneshot pair and parks the sender here; the IPC
// handler takes-and-fires it when the user clicks Stop. Cleared at turn end.
type StopSlot = Arc<Mutex<Option<oneshot::Sender<()>>>>;

#[derive(Debug)]
enum AppEvent {
    // Provider-neutral names; both spikes share the signaling shape.
    Line(String),
    Done,
    Error(String),
    Stopped,
}

const HTML: &str = include_str!("pi_webview_test.html");

fn main() -> wry::Result<()> {
    let event_loop: EventLoop<AppEvent> = EventLoopBuilder::<AppEvent>::with_user_event().build();

    // Install an Edit menu with the standard AppKit selectors so Cmd+C / Cmd+V
    // / Cmd+X / Cmd+A work inside the WKWebView. Held for the life of the
    // program so the NSMenu doesn't get dropped.
    let _menu = build_app_menu();

    let window = WindowBuilder::new()
        .with_title("pi-harness webview spike")
        .with_inner_size(tao::dpi::LogicalSize::new(900.0, 700.0))
        .build(&event_loop)
        .expect("window");

    let proxy = event_loop.create_proxy();

    // Channel: main thread (IPC handler) → tokio thread (subprocess manager)
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Stop-signal slot shared between IPC handler and tokio runtime.
    let stop_slot: StopSlot = Arc::new(Mutex::new(None));

    // Background tokio runtime, owns the input channel and subprocess lifecycle.
    {
        let proxy = proxy.clone();
        let stop_slot = stop_slot.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(tokio_main(input_rx, proxy, stop_slot));
        });
    }

    // Gather the repo's file list once at startup so @-mentions in the
    // textarea can autocomplete paths. Uses `git ls-files` (fast, respects
    // .gitignore). If we're not in a git repo or git isn't available, the
    // list is just empty — the popup will be empty but not broken.
    let file_list_js = gather_file_list_as_js();

    let webview = WebViewBuilder::new()
        .with_html(HTML)
        .with_devtools(true)
        .with_ipc_handler(move |req: Request<String>| {
            let body = req.body();
            match serde_json::from_str::<serde_json::Value>(body) {
                Ok(v) => match v.get("type").and_then(|t| t.as_str()) {
                    Some("submit") => {
                        if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                            if !text.trim().is_empty() {
                                let _ = input_tx.send(text.to_string());
                            }
                        }
                    }
                    Some("stop") => {
                        // Take the current turn's stop sender and fire it. If
                        // nothing's running the slot is None and this no-ops.
                        if let Some(tx) = stop_slot.lock().ok().and_then(|mut g| g.take()) {
                            let _ = tx.send(());
                        }
                    }
                    _ => {}
                },
                Err(e) => eprintln!("[ipc] parse error: {} body={}", e, body),
            }
        })
        .build(&window)?;

    // Push the file list into the page after the webview is constructed.
    // wry queues evaluate_script until the HTML is ready, so this is safe
    // even though DOMContentLoaded hasn't fired yet.
    let _ = webview.evaluate_script(&format!("window.__files = {};", file_list_js));

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(AppEvent::Line(line)) => {
                let script = format!(
                    "window.__appendEventLine({})",
                    serde_json::Value::String(line)
                );
                if let Err(e) = webview.evaluate_script(&script) {
                    eprintln!("[eval] error: {}", e);
                }
            }
            Event::UserEvent(AppEvent::Done) => {
                let _ = webview.evaluate_script("window.__streamDone()");
            }
            Event::UserEvent(AppEvent::Stopped) => {
                let _ = webview.evaluate_script("window.__streamStopped()");
            }
            Event::UserEvent(AppEvent::Error(msg)) => {
                let script = format!("window.__streamError({})", serde_json::Value::String(msg));
                let _ = webview.evaluate_script(&script);
            }
            _ => {}
        }
    });
}

fn gather_file_list_as_js() -> String {
    // Shell out to `git ls-files`. On any error we return an empty JSON array.
    let output = std::process::Command::new("git")
        .arg("ls-files")
        .stderr(Stdio::null())
        .output();
    let files: Vec<String> = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    };
    serde_json::to_string(&files).unwrap_or_else(|_| "[]".to_string())
}

fn build_app_menu() -> Menu {
    let menu = Menu::new();

    // macOS: first submenu is the application menu. Needed for the menu bar
    // to render correctly and for the standard Hide/Quit shortcuts to work.
    #[cfg(target_os = "macos")]
    {
        let app_menu = Submenu::new("pi-spike", true);
        let _ = app_menu.append_items(&[
            &PredefinedMenuItem::about(None, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ]);
        let _ = menu.append(&app_menu);
    }

    // Edit menu with standard AppKit selectors. These route Cmd+C / Cmd+V /
    // Cmd+X / Cmd+A to the first responder — in our case the WKWebView,
    // which knows how to handle them.
    let edit_menu = Submenu::new("Edit", true);
    let _ = edit_menu.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ]);
    let _ = menu.append(&edit_menu);

    #[cfg(target_os = "macos")]
    menu.init_for_nsapp();

    menu
}

async fn tokio_main(
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    proxy: EventLoopProxy<AppEvent>,
    stop_slot: StopSlot,
) {
    // pi uses a session file for multi-turn continuity. Pick a stable path
    // per process so every submit in this window continues the same session.
    let session_path = std::env::var("PI_SESSION_PATH")
        .unwrap_or_else(|_| format!("/tmp/pi-spike-session-{}.jsonl", std::process::id()));
    eprintln!("[tokio] session file: {}", session_path);

    while let Some(prompt) = input_rx.recv().await {
        eprintln!("[tokio] prompt: {}", prompt);

        // Fresh oneshot for this turn; park the sender in the shared slot
        // so the IPC handler can fire it.
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        if let Ok(mut g) = stop_slot.lock() {
            *g = Some(stop_tx);
        }

        let result = run_pi(&prompt, &proxy, &session_path, stop_rx).await;

        // Clear the slot regardless of how the turn ended. If stop was fired
        // the sender is already taken; if not, we drop it here.
        if let Ok(mut g) = stop_slot.lock() {
            g.take();
        }

        if let Err(e) = result {
            eprintln!("[tokio] error: {}", e);
            let _ = proxy.send_event(AppEvent::Error(e.to_string()));
            let _ = proxy.send_event(AppEvent::Done);
        }
    }
}

async fn run_pi(
    prompt: &str,
    proxy: &EventLoopProxy<AppEvent>,
    session_path: &str,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let model = std::env::var("PI_MODEL").unwrap_or_else(|_| "openai-codex/gpt-5.4".into());
    let thinking = std::env::var("PI_THINKING").ok();
    eprintln!(
        "[tokio] spawning pi (model={} thinking={:?})",
        model, thinking
    );

    let mut cmd = Command::new("pi");
    cmd.arg("--print")
        .arg("--mode")
        .arg("json")
        .arg("--model")
        .arg(&model)
        .arg("--session")
        .arg(session_path);
    if let Some(t) = thinking.as_deref() {
        cmd.arg("--thinking").arg(t);
    }
    cmd.arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or("child stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut stopped = false;
    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                match maybe_line {
                    Ok(Some(line)) => {
                        if proxy.send_event(AppEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("[tokio] stdout read error: {}", e);
                        break;
                    }
                }
            }
            _ = &mut stop_rx => {
                eprintln!("[tokio] stop requested — killing subprocess");
                stopped = true;
                let _ = child.start_kill();
                break;
            }
        }
    }

    // Always wait so the child reaps cleanly — avoids orphan / zombie.
    let _ = child.wait().await;
    if stopped {
        let _ = proxy.send_event(AppEvent::Stopped);
    }
    let _ = proxy.send_event(AppEvent::Done);
    Ok(())
}
