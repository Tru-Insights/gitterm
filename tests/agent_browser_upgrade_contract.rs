//! The agent-browser engine's contract (TRU-177): GitTerm's browser tools,
//! called through the authenticated MCP front door, against the pinned
//! agent-browser binary attached to a throwaway Chrome.
//!
//! Run this first on any agent-browser upgrade, after bumping
//! `PINNED_AGENT_BROWSER`:
//!
//! ```text
//! cargo test --test agent_browser_upgrade_contract -- --ignored --nocapture
//! ```
//!
//! It launches its own headless Chrome on a spare loopback port with a
//! temporary profile, serves neutral pages from a local server, and never
//! touches the user's Chrome, GitTerm, or `~/.config/gitterm*`.
//! `GITTERM_AGENT_BROWSER_PATH` names the binary (default
//! `~/.local/agent-browser/agent-browser`); `GITTERM_V6_CHROME_PATH` names
//! Chrome (default: the macOS install).

use gitterm::agent_browser::PINNED_AGENT_BROWSER;
use gitterm::browser_mcp::{self, BrowserMcpConnection};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const INDEX: &str = r#"<!doctype html><html><head><title>Fixture Page</title></head><body>
<h1 id="heading">Fixture heading</h1>
<p id="status">idle</p>
<label for="name">Name</label><input id="name" type="text">
<button id="go" onclick="document.getElementById('status').textContent='clicked '+document.getElementById('name').value;console.error('fixture console error');fetch('missing.json');setTimeout(()=>{throw new Error('boom from page')},0)">Go</button>
<a href="other.html">Other page</a>
<div style="height:3000px"></div>
</body></html>"#;
const OTHER: &str = r#"<!doctype html><html><head><title>Other Page</title></head><body><h1>Other</h1><button id="noise" onclick="console.error('other tab error');fetch('gone.json')">Noise</button></body></html>"#;

type Client = RunningService<RoleClient, ()>;

/// Stops any agent-browser daemon still running in the engine's socket
/// directory when the test ends, passing or not, by the pid files the
/// daemons write there.
struct DaemonSweep(PathBuf);

impl Drop for DaemonSweep {
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.0) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "pid") {
                if let Ok(pid) = std::fs::read_to_string(&path) {
                    let pid = pid.trim();
                    let is_agent_browser = !pid.is_empty()
                        && pid.bytes().all(|byte| byte.is_ascii_digit())
                        && Command::new("ps")
                            .args(["-p", pid, "-o", "comm="])
                            .output()
                            .is_ok_and(|output| {
                                String::from_utf8_lossy(&output.stdout).contains("agent-browser")
                            });
                    if is_agent_browser {
                        eprintln!("stopping leftover agent-browser daemon {pid}");
                        let _ = Command::new("kill").arg(pid).status();
                    }
                }
            }
        }
    }
}

/// The throwaway Chrome, killed with its profile when dropped.
struct TempChrome {
    child: Child,
    port: u16,
}

impl Drop for TempChrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spare_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn launch_chrome(profile: &Path) -> TempChrome {
    let chrome = std::env::var_os("GITTERM_V6_CHROME_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        });
    let port = spare_port();
    let child = Command::new(&chrome)
        .args([
            "--headless=new".to_string(),
            format!("--user-data-dir={}", profile.display()),
            "--remote-debugging-address=127.0.0.1".to_string(),
            format!("--remote-debugging-port={port}"),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "about:blank".to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("could not launch Chrome at {}: {error}", chrome.display()));
    let chrome = TempChrome { child, port };
    // GitTerm adopts a running Chrome through its profile's
    // DevToolsActivePort, as it adopts one another instance launched.
    // Headless Chrome with a fixed port does not write that file, so the
    // test writes it from the browser's own /json/version.
    let deadline = Instant::now() + Duration::from_secs(20);
    let browser_path = loop {
        let output = Command::new("curl")
            .args(["-s", &format!("http://127.0.0.1:{port}/json/version")])
            .output()
            .unwrap();
        if let Ok(version) = serde_json::from_slice::<Value>(&output.stdout) {
            let url = version["webSocketDebuggerUrl"]
                .as_str()
                .unwrap()
                .to_string();
            break url[url.find("/devtools/browser/").unwrap()..].to_string();
        }
        assert!(
            Instant::now() < deadline,
            "Chrome's DevTools endpoint never answered"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    std::fs::write(
        profile.join("DevToolsActivePort"),
        format!("{port}\n{browser_path}\n"),
    )
    .unwrap();
    chrome
}

/// A minimal static server for the fixture pages.
async fn serve_pages() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let line = String::from_utf8_lossy(&request);
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let path = path.split('?').next().unwrap_or("/").to_string();
                let (status, body) = match path.as_str() {
                    "/index.html" => ("200 OK", INDEX),
                    "/other.html" => ("200 OK", OTHER),
                    _ => ("404 Not Found", "not found"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (port, task)
}

async fn client_for(connection: &BrowserMcpConnection, caller: &str) -> Client {
    let [(_, url), (_, token)] = connection.terminal_environment(Some(caller));
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .expect("MCP initialization timed out")
        .expect("MCP initialization failed")
}

async fn call(client: &Client, tool: &str, arguments: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(tool.to_string());
    if let Value::Object(arguments) = arguments {
        params = params.with_arguments(arguments);
    }
    tokio::time::timeout(Duration::from_secs(120), client.call_tool(params))
        .await
        .unwrap_or_else(|_| panic!("{tool} timed out"))
        .unwrap_or_else(|error| panic!("{tool} failed at the protocol level: {error}"))
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The structured result of a call that must succeed.
async fn ok(client: &Client, tool: &str, arguments: Value) -> Value {
    let result = call(client, tool, arguments.clone()).await;
    assert_eq!(
        result.is_error,
        Some(false),
        "{tool} {arguments} failed: {}",
        text_of(&result)
    );
    result.structured_content.unwrap_or(Value::Null)
}

/// The error text of a call that must fail.
async fn refused(client: &Client, tool: &str, arguments: Value) -> String {
    let result = call(client, tool, arguments.clone()).await;
    assert_eq!(
        result.is_error,
        Some(true),
        "{tool} {arguments} should have failed: {}",
        text_of(&result)
    );
    text_of(&result)
}

fn page_ids(port: u16) -> Vec<String> {
    let output = Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/json/list")])
        .output()
        .unwrap();
    let pages: Value = serde_json::from_slice(&output.stdout).unwrap();
    pages
        .as_array()
        .unwrap()
        .iter()
        .filter(|page| page["type"] == "page")
        .map(|page| page["id"].as_str().unwrap().to_string())
        .collect()
}

fn sockets(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.ends_with(".sock"))
                .collect()
        })
        .unwrap_or_default()
}

/// The binary run directly in one of the engine's sessions, outside the
/// front door, with the engine's flags (different launch flags would make
/// agent-browser restart the session's daemon).
fn agent_browser_direct(
    binary: &Path,
    state: &Path,
    port: u16,
    session: &str,
    command: &[&str],
) -> Value {
    let output = Command::new(binary)
        .env("AGENT_BROWSER_SOCKET_DIR", state.join("s"))
        .current_dir(state)
        .args(["--config", &state.join("config.json").display().to_string()])
        .args([
            "--action-policy",
            &state.join("policy.json").display().to_string(),
        ])
        .args(["--cdp", &port.to_string(), "--session", session])
        .args(["--pin-tab", "--idle-timeout", "30m", "--json"])
        .args(command)
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "agent-browser printed non-JSON: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// The outline nodes without the per-document references, which differ
/// between any two documents.
fn outline_shape(outline: &Value) -> Value {
    let mut nodes = outline["nodes"].clone();
    for node in nodes.as_array_mut().unwrap() {
        let node = node.as_object_mut().unwrap();
        node.remove("nodeRef");
        node.remove("parentRef");
    }
    nodes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "launches Chrome and the pinned agent-browser; run on every agent-browser upgrade"]
async fn agent_browser_engine_honours_the_front_door_contract() {
    let binary = std::env::var_os("GITTERM_AGENT_BROWSER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap()
                .join(".local/agent-browser/agent-browser")
        });
    assert!(
        binary.is_file(),
        "no agent-browser at {}; set GITTERM_AGENT_BROWSER_PATH to the pinned {PINNED_AGENT_BROWSER} binary",
        binary.display()
    );
    // Short paths: agent-browser's Unix sockets live under the config dir.
    let config = tempfile::Builder::new()
        .prefix("gtab")
        .tempdir_in("/tmp")
        .unwrap();
    let gitterm_config = tempfile::Builder::new()
        .prefix("gtgt")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::write(
        config.path().join("browser.json"),
        json!({ "engine": "agent-browser", "agent_browser_path": binary }).to_string(),
    )
    .unwrap();
    let state = config.path().join("agent-browser");
    let _sweep = DaemonSweep(state.join("s"));
    let chrome = launch_chrome(&config.path().join("browser-profile"));
    let (site_port, site) = serve_pages().await;
    let index = format!("http://127.0.0.1:{site_port}/index.html");
    let other = format!("http://127.0.0.1:{site_port}/other.html");

    let (connection, server) = browser_mcp::prepare(config.path(), "contract").unwrap();
    let server = tokio::spawn(server.run());
    let a = client_for(&connection, "chat-a").await;
    let b = client_for(&connection, "chat-b").await;

    // The tool list: the common tools plus the four agent-browser tools,
    // and nothing that runs script or reaches cookies or storage.
    let tools = a.list_tools(Default::default()).await.unwrap().tools;
    let names: HashSet<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    assert_eq!(tools.len(), 26, "{names:?}");
    for extra in [
        "browser_snapshot_refs",
        "browser_diff",
        "browser_network_capture",
        "browser_viewport",
    ] {
        assert!(names.contains(extra), "{extra}");
    }
    for name in &names {
        for forbidden in ["eval", "cookie", "storage", "script", "state"] {
            assert!(!name.contains(forbidden), "{name}");
        }
    }
    let tool_list_bytes = serde_json::to_vec(&tools).unwrap().len();
    println!(
        "agent-browser engine tool list: {} tools, {tool_list_bytes} bytes",
        tools.len()
    );

    // Open: GitTerm adopts the Chrome it finds in its profile; the engine
    // starts on the pinned binary.
    let status = ok(&a, "browser_open", json!({})).await;
    assert_eq!(status["state"], "running");
    assert_eq!(status["devtools_port"], chrome.port);

    // Navigate opens chat-a's default target; type and click by GitTerm's
    // strict semantic locators; agent text reaches the page literally.
    let navigated = ok(&a, "browser_navigate", json!({ "url": index })).await;
    assert_eq!(navigated["url"], index);
    let targets = ok(&a, "browser_targets", json!({})).await;
    let targets = targets["result"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["name"], "primary");
    assert_eq!(targets[0]["active"], true);
    let primary_id = targets[0]["targetId"].as_str().unwrap().to_string();
    let typed = ok(
        &a,
        "browser_type",
        json!({ "locator": { "kind": "role", "role": "textbox", "name": "Name" }, "text": "--headed Ada" }),
    )
    .await;
    assert_eq!(typed["tag"], "input");
    let clicked = ok(
        &a,
        "browser_click",
        json!({ "locator": { "kind": "role", "role": "button", "name": "Go", "exact": true } }),
    )
    .await;
    assert_eq!(clicked["tag"], "button");
    ok(
        &a,
        "browser_wait_for",
        json!({ "condition": { "kind": "text", "text": "clicked --headed Ada", "exact": true }, "timeout_ms": 5000 }),
    )
    .await;
    let wait_error = refused(
        &a,
        "browser_wait_for",
        json!({ "condition": { "kind": "text", "text": "never shown" }, "timeout_ms": 300 }),
    )
    .await;
    assert!(wait_error.contains("not met within 300 ms"), "{wait_error}");
    ok(
        &a,
        "browser_wait_for",
        json!({ "condition": { "kind": "loading_state", "state": "complete" } }),
    )
    .await;
    let ambiguous = refused(
        &a,
        "browser_click",
        json!({ "locator": { "kind": "css", "selector": "body *" } }),
    )
    .await;
    assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
    ok(&a, "browser_press", json!({ "key": "tab" })).await;
    ok(
        &a,
        "browser_wait_for",
        json!({ "condition": { "kind": "url", "url": "/index.html" } }),
    )
    .await;

    // Console, page errors and failed requests, from chat-a's own tab.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let console = ok(&a, "browser_console", json!({})).await;
    let console = console["result"].to_string();
    assert!(console.contains("fixture console error"), "{console}");
    assert!(console.contains("boom from page"), "{console}");
    let network = ok(&a, "browser_network", json!({})).await;
    let network = network["result"].to_string();
    assert!(
        network.contains("/missing.json") && network.contains("404"),
        "{network}"
    );
    assert!(!network.to_lowercase().contains("user-agent"), "{network}");

    // Scroll moves the page: the heading's box rises out of view.
    ok(&a, "browser_scroll", json!({ "delta_y": 400 })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let heading = ok(
        &a,
        "browser_node_inspect",
        json!({ "selector": { "kind": "locator", "locator": { "kind": "css", "selector": "#heading" } } }),
    )
    .await;
    assert!(heading["bounds"]["y"].as_f64().unwrap() < 0.0, "{heading}");

    // Outline and inspect: GitTerm's own JavaScript through the engine's
    // eval gives the gitterm engine's output. Compare against the gitterm
    // engine on the same Chrome, the same page and the same viewport.
    std::fs::create_dir_all(gitterm_config.path().join("browser-profile")).unwrap();
    std::fs::copy(
        config.path().join("browser-profile/DevToolsActivePort"),
        gitterm_config
            .path()
            .join("browser-profile/DevToolsActivePort"),
    )
    .unwrap();
    let (gitterm_connection, gitterm_server) =
        browser_mcp::prepare(gitterm_config.path(), "contract-gitterm").unwrap();
    let gitterm_server = tokio::spawn(gitterm_server.run());
    let g = client_for(&gitterm_connection, "chat-g").await;
    assert_eq!(
        g.list_tools(Default::default()).await.unwrap().tools.len(),
        22
    );
    ok(&g, "browser_open", json!({})).await;
    let desktop = json!({ "viewport": { "width": 1000, "height": 700, "deviceScaleFactor": 1.0, "mobile": false } });
    ok(&g, "browser_resize", desktop.clone()).await;
    ok(&g, "browser_navigate", json!({ "url": index })).await;
    ok(
        &g,
        "browser_wait_for",
        json!({ "condition": { "kind": "loading_state", "state": "complete" } }),
    )
    .await;
    ok(&a, "browser_resize", desktop.clone()).await;
    ok(&a, "browser_navigate", json!({ "url": index })).await;
    let outline = ok(&a, "browser_dom_outline", json!({})).await;
    let gitterm_outline = ok(&g, "browser_dom_outline", json!({})).await;
    assert_eq!(outline["title"], "Fixture Page");
    assert_eq!(outline["url"], gitterm_outline["url"]);
    assert_eq!(outline["title"], gitterm_outline["title"]);
    assert_eq!(outline["truncated"], gitterm_outline["truncated"]);
    assert_eq!(outline_shape(&outline), outline_shape(&gitterm_outline));
    let node_ref = outline["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["tag"] == "h1")
        .unwrap()["nodeRef"]
        .clone();
    let by_ref = ok(
        &a,
        "browser_node_inspect",
        json!({ "selector": { "kind": "node_ref", "node_ref": node_ref } }),
    )
    .await;
    assert_eq!(by_ref["tag"], "h1");
    let inspect_selector = json!({ "selector": { "kind": "locator", "locator": { "kind": "css", "selector": "#go" } } });
    let mut inspected = ok(&a, "browser_node_inspect", inspect_selector.clone()).await;
    let mut gitterm_inspected = ok(&g, "browser_node_inspect", inspect_selector).await;
    for details in [&mut inspected, &mut gitterm_inspected] {
        let details = details.as_object_mut().unwrap();
        details.remove("documentId");
        details.remove("nodeRef");
    }
    assert_eq!(inspected, gitterm_inspected);
    ok(&g, "browser_disconnect", json!({})).await;
    g.cancel().await.unwrap();
    drop(gitterm_connection);
    gitterm_server.await.unwrap().unwrap();

    // Capture keeps labelled evidence and returns the screenshot.
    let capture = call(&a, "browser_capture", json!({ "label": "before" })).await;
    assert_eq!(capture.is_error, Some(false), "{}", text_of(&capture));
    assert!(capture
        .content
        .iter()
        .any(|content| content.as_image().is_some()));
    let captured = capture.structured_content.unwrap();
    assert_eq!(captured["target"], "primary");
    assert_eq!(captured["title"], "Fixture Page");
    let evidence = connection.browser().telemetry().await.evidence;
    assert!(evidence
        .iter()
        .any(|evidence| evidence.owner == "chat-a" && evidence.label == "before"));
    let snapshot = call(&a, "browser_snapshot", json!({})).await;
    assert_eq!(snapshot.is_error, Some(false), "{}", text_of(&snapshot));
    let full = ok(
        &a,
        "browser_capture",
        json!({ "label": "full", "full_page": true }),
    )
    .await;
    assert_eq!(full["captureMode"], "full_page");

    // Refs from browser_snapshot_refs work for click and type.
    let refs = ok(&a, "browser_snapshot_refs", json!({})).await;
    assert_eq!(refs["truncated"], false);
    let tree = refs["snapshot"].as_str().unwrap();
    let ref_of = |role: &str| {
        let line = tree
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("- {role}")))
            .unwrap_or_else(|| panic!("no {role} in {tree}"));
        let start = line.find("ref=").unwrap() + 4;
        line[start..]
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap()
            .to_string()
    };
    let (textbox, button) = (ref_of("textbox"), ref_of("button"));
    let typed = ok(
        &a,
        "browser_type",
        json!({ "locator": { "kind": "ref", "ref": textbox }, "text": "Lin" }),
    )
    .await;
    assert_eq!(typed["ref"], textbox);
    assert_eq!(typed["role"], "textbox");
    ok(
        &a,
        "browser_click",
        json!({ "locator": { "kind": "ref", "ref": format!("@{button}") } }),
    )
    .await;
    ok(
        &a,
        "browser_wait_for",
        json!({ "condition": { "kind": "text", "text": "clicked Lin" } }),
    )
    .await;
    let stale = refused(
        &a,
        "browser_click",
        json!({ "locator": { "kind": "ref", "ref": "e999" } }),
    )
    .await;
    assert!(stale.contains("latest browser_snapshot_refs"), "{stale}");

    // Diffs against the labelled baseline from chat-a's own evidence.
    let diff = ok(
        &a,
        "browser_diff",
        json!({ "baseline_label": "before", "mode": "snapshot" }),
    )
    .await;
    assert_eq!(diff["changed"], true, "{diff}");
    assert!(diff["diff"].as_str().unwrap().contains("Lin"), "{diff}");
    let pixel_diff = call(
        &a,
        "browser_diff",
        json!({ "baseline_label": "before", "mode": "screenshot" }),
    )
    .await;
    assert_eq!(pixel_diff.is_error, Some(false), "{}", text_of(&pixel_diff));
    let pixel = pixel_diff.structured_content.clone().unwrap();
    assert!(
        pixel["totalPixels"].is_null() && pixel["total_pixels"].as_u64().unwrap() > 0,
        "{pixel}"
    );
    assert!(pixel_diff
        .content
        .iter()
        .any(|content| content.as_image().is_some()));
    let no_baseline = refused(
        &b,
        "browser_diff",
        json!({ "baseline_label": "before", "mode": "snapshot" }),
    )
    .await;
    assert!(
        no_baseline.contains("no evidence labelled 'before'"),
        "{no_baseline}"
    );

    // HAR capture of chat-a's own tab, reduced to safe fields.
    ok(&a, "browser_network_capture", json!({ "action": "start" })).await;
    ok(
        &a,
        "browser_click",
        json!({ "locator": { "kind": "css", "selector": "#go" } }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let har = ok(&a, "browser_network_capture", json!({ "action": "stop" })).await;
    let har_text = har.to_string();
    assert!(har_text.contains("/missing.json"), "{har_text}");
    for leaked in ["headers", "cookies", "User-Agent", "\"text\""] {
        assert!(!har_text.contains(leaked), "{leaked} in {har_text}");
    }
    let not_recording = refused(&a, "browser_network_capture", json!({ "action": "stop" })).await;
    assert!(not_recording.contains("not recording"), "{not_recording}");

    // Viewport presets and sizes; reload.
    let phone = ok(&a, "browser_viewport", json!({ "device": "iPhone 15" })).await;
    assert_eq!(phone["mobile"], true, "{phone}");
    let sized = ok(
        &a,
        "browser_viewport",
        json!({ "width": 900, "height": 700 }),
    )
    .await;
    assert_eq!(
        (sized["width"].as_u64(), sized["height"].as_u64()),
        (Some(900), Some(700))
    );
    let mobile_resize = refused(
        &a,
        "browser_resize",
        json!({ "viewport": { "width": 390, "height": 844, "deviceScaleFactor": 3.0, "mobile": true } }),
    )
    .await;
    assert!(
        mobile_resize.contains("browser_viewport"),
        "{mobile_resize}"
    );
    ok(&a, "browser_reload", json!({})).await;
    let no_cache = refused(&a, "browser_reload", json!({ "ignore_cache": true })).await;
    assert!(no_cache.contains("cache"), "{no_cache}");

    // A second named target has its own diagnostics: entries from one tab
    // never appear in another's.
    let opened = ok(
        &a,
        "browser_target_open",
        json!({ "name": "other", "url": other }),
    )
    .await;
    let other_id = opened["targetId"].as_str().unwrap().to_string();
    assert_ne!(other_id, primary_id);
    ok(
        &a,
        "browser_click",
        json!({ "target": "other", "locator": { "kind": "css", "selector": "#noise" } }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let other_diagnostics = ok(
        &a,
        "browser_target_diagnostics",
        json!({ "target": "other" }),
    )
    .await;
    let other_diagnostics = other_diagnostics.to_string();
    assert!(
        other_diagnostics.contains("other tab error"),
        "{other_diagnostics}"
    );
    assert!(
        other_diagnostics.contains("/gone.json"),
        "{other_diagnostics}"
    );
    assert!(
        !other_diagnostics.contains("fixture console error"),
        "{other_diagnostics}"
    );
    let primary_diagnostics = ok(
        &a,
        "browser_target_diagnostics",
        json!({ "target": "primary" }),
    )
    .await;
    let primary_diagnostics = primary_diagnostics.to_string();
    assert!(
        primary_diagnostics.contains("fixture console error"),
        "{primary_diagnostics}"
    );
    assert!(
        !primary_diagnostics.contains("other tab error"),
        "{primary_diagnostics}"
    );
    assert!(
        !primary_diagnostics.contains("/gone.json"),
        "{primary_diagnostics}"
    );

    // chat-b cannot reach chat-a's targets, by name or otherwise, and its
    // own first page operation opens its own tab with clean diagnostics.
    let not_found = "browser target 'other' does not exist";
    for (tool, arguments) in [
        ("browser_target_close", json!({ "target": "other" })),
        ("browser_target_focus", json!({ "target": "other" })),
        ("browser_target_diagnostics", json!({ "target": "other" })),
        (
            "browser_navigate",
            json!({ "target": "other", "url": index }),
        ),
        ("browser_snapshot_refs", json!({ "target": "other" })),
    ] {
        let error = refused(&b, tool, arguments).await;
        assert!(error.starts_with(not_found), "{tool}: {error}");
    }
    let b_targets = ok(&b, "browser_targets", json!({})).await;
    let b_targets = b_targets["result"].as_array().unwrap();
    assert_eq!(b_targets.len(), 1);
    let b_primary = b_targets[0]["targetId"].as_str().unwrap().to_string();
    assert!(b_primary != primary_id && b_primary != other_id);
    ok(&b, "browser_navigate", json!({ "url": other })).await;
    let b_console = ok(&b, "browser_console", json!({})).await;
    assert_eq!(b_console["result"], json!([]), "{b_console}");
    assert!(page_ids(chrome.port).contains(&other_id));

    // Focus, then close: the tab leaves Chrome and its session stops.
    ok(&a, "browser_target_focus", json!({ "target": "primary" })).await;
    let sessions_before = sockets(&state.join("s")).len();
    ok(&a, "browser_target_close", json!({ "target": "other" })).await;
    assert!(!page_ids(chrome.port).contains(&other_id));
    assert_eq!(sockets(&state.join("s")).len(), sessions_before - 1);

    // A tab closed behind the engine's back is forgotten, not replaced.
    let external = ok(
        &a,
        "browser_target_open",
        json!({ "name": "external", "url": index }),
    )
    .await;
    let external_id = external["targetId"].as_str().unwrap().to_string();
    Command::new("curl")
        .args([
            "-s",
            &format!("http://127.0.0.1:{}/json/close/{external_id}", chrome.port),
        ])
        .output()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let gone = refused(
        &a,
        "browser_navigate",
        json!({ "target": "external", "url": index }),
    )
    .await;
    assert!(
        gone.starts_with("browser target 'external' does not exist"),
        "{gone}"
    );

    // Every session runs under GitTerm's policy, streams nothing, and the
    // policy denies what the engine never does.
    let live_sessions: Vec<String> = sockets(&state.join("s"))
        .into_iter()
        .map(|name| name.trim_end_matches(".sock").to_string())
        .collect();
    assert_eq!(live_sessions.len(), 2, "{live_sessions:?}");
    for session in &live_sessions {
        for (command, action) in [
            (&["cookies", "get"][..], "cookies_get"),
            (&["storage", "local"][..], "storage_get"),
            (
                &["state", "save", "/tmp/never-written.json"][..],
                "state_save",
            ),
            (&["clipboard", "read"][..], "clipboard"),
            (&["eval", "document.cookie"][..], ""),
        ] {
            let denied = agent_browser_direct(&binary, &state, chrome.port, session, command);
            if action.is_empty() {
                // GitTerm's own outline and inspect need eval, so the policy
                // allows it; no front-door tool takes script (see above).
                assert_eq!(denied["success"], true, "{denied}");
                continue;
            }
            assert_eq!(denied["success"], false, "{command:?}: {denied}");
            let error = denied["error"].as_str().unwrap();
            assert!(
                error.contains(&format!("Action '{action}' denied by policy")),
                "{error}"
            );
            assert!(
                !denied.to_string().contains("restartedBackground\":true"),
                "{denied}"
            );
        }
    }
    // No session daemon listens on TCP: streaming is off in every session.
    for session in &live_sessions {
        let pid = std::fs::read_to_string(state.join("s").join(format!("{session}.pid")))
            .unwrap()
            .trim()
            .to_string();
        let listening = Command::new("lsof")
            .args(["-nP", "-a", "-p", &pid, "-iTCP", "-sTCP:LISTEN"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&listening.stdout).trim().is_empty(),
            "{session} (pid {pid}) listens: {}",
            String::from_utf8_lossy(&listening.stdout)
        );
    }

    // Disconnect closes only chat-a's targets; releasing chat-b (the UI's
    // path when its tab closes) leaves its page open as the user's.
    let disconnected = ok(&a, "browser_disconnect", json!({})).await;
    assert_eq!(disconnected["closed_targets"], json!(["primary"]));
    assert_eq!(disconnected["browser_running"], true);
    assert!(!page_ids(chrome.port).contains(&primary_id));
    assert!(page_ids(chrome.port).contains(&b_primary));
    connection
        .engine()
        .release_owners(&HashSet::from(["chat-b".to_string()]))
        .await
        .unwrap();
    assert!(page_ids(chrome.port).contains(&b_primary));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sockets(&state.join("s")).is_empty() {
        assert!(
            Instant::now() < deadline,
            "agent-browser sessions left running: {:?}",
            sockets(&state.join("s"))
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let scratch: Vec<_> = std::fs::read_dir(state.join("scratch")).unwrap().collect();
    assert!(scratch.is_empty(), "scratch files left behind: {scratch:?}");

    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
    drop(connection);
    server.await.unwrap().unwrap();
    site.abort();
    drop(chrome);
}

#[tokio::test]
#[ignore = "runs the pinned agent-browser binary; run on every agent-browser upgrade"]
async fn the_installed_binary_is_the_pinned_version() {
    let binary = std::env::var_os("GITTERM_AGENT_BROWSER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap()
                .join(".local/agent-browser/agent-browser")
        });
    let output = Command::new(&binary).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("agent-browser {PINNED_AGENT_BROWSER}")
    );
}
