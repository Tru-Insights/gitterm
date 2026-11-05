//! Isolated native sandbox checks. Never starts or connects to GitTerm.
//! GITTERM_V6_CONFIG_DIR=/absolute/tmp/path cargo run --example html_render_webkit_smoke

#[allow(dead_code)]
#[path = "../src/webview.rs"]
mod webview;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = std::env::var("GITTERM_V6_CONFIG_DIR")?;
    if !std::path::Path::new(&config).is_absolute() {
        return Err("smoke requires an absolute isolated config dir".into());
    }
    let mut event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("GitTerm HTML sandbox smoke")
        .with_visible(false)
        .build(&event_loop)?;
    let proxy = event_loop.create_proxy();
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let network_address = listener.local_addr()?.to_string();
    let mut page = gitterm::chat_page::agent_chat_html();
    page = page.replace(
        "</body>",
        &format!(
            "<script>window.__testMode='webkit';</script><script>{}</script></body>",
            include_str!("../tests/html_render_ui.js").replace("127.0.0.1:1", &network_address)
        ),
    );
    let unexpected = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&unexpected);
    let surface = webview::WebviewSurface::Agent(1);
    webview::set_pending_content_with_ipc(
        surface,
        page,
        (0., 0., 1000., 900.),
        Some(Box::new(move |body| {
            let value: serde_json::Value = serde_json::from_str(&body).expect("valid test IPC");
            if value["type"] == "test_report" {
                let _ = proxy.send_event(value["result"].to_string());
            } else if !(value["type"] == "hostkey" && value["key"] == "Escape") {
                observed.lock().unwrap().push(body);
            }
        })),
    );
    webview::try_create_with_window(surface, &window)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut report = None;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(deadline);
        if let Event::UserEvent(result) = event {
            report = Some(result);
            *flow = ControlFlow::Exit;
        } else if Instant::now() >= deadline {
            *flow = ControlFlow::Exit;
        }
    });
    let report = report.ok_or("native HTML smoke timed out")?;
    println!("{report}");
    let value: serde_json::Value = serde_json::from_str(&report)?;
    assert_eq!(
        value["failures"].as_array().unwrap().len(),
        0,
        "native UI checks"
    );
    assert!(
        listener.accept().is_err(),
        "sandboxed content reached the network"
    );
    assert!(
        unexpected.lock().unwrap().is_empty(),
        "untrusted IPC reached host: {:?}",
        unexpected.lock().unwrap()
    );
    println!("Native WebKit sandbox/UI checks passed; raw and forged IPC reached no application handler.");
    Ok(())
}
