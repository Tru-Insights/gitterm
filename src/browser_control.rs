//! Dedicated-profile Chrome control through the Chrome DevTools Protocol.
//!
//! This adapter deliberately has no dependency on GitTerm's singleton Wry
//! webview. Callers provide the V4 global config directory so browser state is
//! always rooted in the same isolated configuration tree.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use hyper::{body::to_bytes, Client, Uri};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use url::Url;

const PROFILE_DIR_NAME: &str = "browser-profile";
const DEVTOOLS_ACTIVE_PORT_FILE: &str = "DevToolsActivePort";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CDP_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CHROME_STDERR_BYTES: usize = 16_384;
const MAX_VISIBLE_TEXT_BYTES: usize = 100_000;
const MAX_INTERACTIVE_ELEMENTS: usize = 500;

/// Optional explicit Chrome executable for installations outside standard
/// platform locations.
pub const CHROME_PATH_ENV: &str = "GITTERM_V4_CHROME_PATH";

#[derive(Debug)]
pub struct BrowserControlError {
    message: String,
}

impl BrowserControlError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BrowserControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BrowserControlError {}

pub type Result<T> = std::result::Result<T, BrowserControlError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserState {
    Stopped,
    Running,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserStatus {
    pub state: BrowserState,
    pub profile_dir: PathBuf,
    pub devtools_port: Option<u16>,
    pub process_id: Option<u32>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavigationResult {
    pub url: String,
    pub frame_id: String,
    pub loader_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveElement {
    pub index: usize,
    pub tag: String,
    pub role: Option<String>,
    pub name: Option<String>,
    pub text: String,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshot {
    pub url: String,
    pub title: String,
    pub loading_state: String,
    pub visible_text: String,
    pub interactive_elements: Vec<InteractiveElement>,
    #[serde(skip_serializing)]
    pub screenshot_png: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct BrowserLaunchOptions {
    pub executable: Option<PathBuf>,
    pub startup_timeout: Duration,
}

impl Default for BrowserLaunchOptions {
    fn default() -> Self {
        Self {
            executable: None,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone)]
struct DevToolsEndpoint {
    port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevToolsTarget {
    #[serde(rename = "type")]
    kind: String,
    web_socket_debugger_url: Option<String>,
}

/// Owns one visible Chrome process and its dedicated GitTerm V4 profile.
///
/// Dropping the controller terminates the child process. Chrome is launched
/// without headless flags and its DevTools listener is bound to loopback on a
/// random OS-assigned port.
pub struct BrowserController {
    profile_dir: PathBuf,
    child: Option<Child>,
    endpoint: Option<DevToolsEndpoint>,
    last_exit: Option<String>,
    stderr_log: Arc<Mutex<Vec<u8>>>,
    stderr_task: Option<JoinHandle<()>>,
}

impl BrowserController {
    pub fn new(v4_global_config_dir: impl AsRef<Path>) -> Self {
        Self {
            profile_dir: browser_profile_dir(v4_global_config_dir.as_ref()),
            child: None,
            endpoint: None,
            last_exit: None,
            stderr_log: Arc::new(Mutex::new(Vec::new())),
            stderr_task: None,
        }
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    pub async fn launch(&mut self, options: BrowserLaunchOptions) -> Result<BrowserStatus> {
        let current = self.status()?;
        if current.state == BrowserState::Running {
            return Ok(current);
        }

        std::fs::create_dir_all(&self.profile_dir).map_err(|error| {
            BrowserControlError::new(format!(
                "failed to create V4 browser profile {}: {error}",
                self.profile_dir.display()
            ))
        })?;

        let active_port_path = self.profile_dir.join(DEVTOOLS_ACTIVE_PORT_FILE);
        if let Some(endpoint) = read_devtools_active_port(&active_port_path)? {
            if list_page_targets(endpoint.port).await.is_ok() {
                return Err(BrowserControlError::new(format!(
                    "the V4 browser profile {} is already controlled by another Chrome process on port {}",
                    self.profile_dir.display(),
                    endpoint.port
                )));
            }
            std::fs::remove_file(&active_port_path).map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to remove stale Chrome endpoint file {}: {error}",
                    active_port_path.display()
                ))
            })?;
        }

        let executable = resolve_chrome_executable(options.executable.as_deref())?;
        let args = chrome_launch_args(&self.profile_dir);
        let mut command = Command::new(&executable);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|error| {
            BrowserControlError::new(format!(
                "failed to launch Chrome at {} with V4 profile {}: {error}",
                executable.display(),
                self.profile_dir.display()
            ))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            BrowserControlError::new("failed to capture Chrome stderr for launch diagnostics")
        })?;
        self.start_stderr_capture(stderr).await;
        self.child = Some(child);
        self.last_exit = None;

        let deadline = Instant::now() + options.startup_timeout;
        loop {
            if let Some(endpoint) = read_devtools_active_port(&active_port_path)? {
                match list_page_targets(endpoint.port).await {
                    Ok(_) => {
                        self.endpoint = Some(endpoint);
                        return self.status();
                    }
                    Err(error) if Instant::now() < deadline => {
                        self.last_exit = Some(format!("waiting for DevTools: {error}"));
                    }
                    Err(error) => {
                        let stderr = self.terminate_child().await;
                        return Err(BrowserControlError::new(format!(
                            "Chrome published a DevTools endpoint that did not become ready: {error}{}",
                            stderr_suffix(&stderr)
                        )));
                    }
                }
            }

            if let Some(child) = self.child.as_mut() {
                if let Some(exit) = child.try_wait().map_err(|error| {
                    BrowserControlError::new(format!(
                        "failed to inspect the Chrome process launched with profile {}: {error}",
                        self.profile_dir.display()
                    ))
                })? {
                    let stderr = self.finish_stderr_capture().await;
                    self.child = None;
                    self.endpoint = None;
                    self.last_exit = Some(exit.to_string());
                    return Err(BrowserControlError::new(format!(
                        "Chrome exited before publishing its DevTools endpoint: {exit}{}",
                        stderr_suffix(&stderr)
                    )));
                }
            }

            if Instant::now() >= deadline {
                let stderr = self.terminate_child().await;
                return Err(BrowserControlError::new(format!(
                    "Chrome did not publish {} within {} seconds for profile {}{}",
                    DEVTOOLS_ACTIVE_PORT_FILE,
                    options.startup_timeout.as_secs(),
                    self.profile_dir.display(),
                    stderr_suffix(&stderr)
                )));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn status(&mut self) -> Result<BrowserStatus> {
        if let Some(child) = self.child.as_mut() {
            if let Some(exit) = child.try_wait().map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to inspect the Chrome process launched with profile {}: {error}",
                    self.profile_dir.display()
                ))
            })? {
                self.child = None;
                self.endpoint = None;
                self.last_exit = Some(exit.to_string());
            }
        }

        let (state, detail) = if self.child.is_some() && self.endpoint.is_some() {
            (BrowserState::Running, None)
        } else if let Some(exit) = &self.last_exit {
            (BrowserState::Exited, Some(exit.clone()))
        } else {
            (BrowserState::Stopped, None)
        };

        Ok(BrowserStatus {
            state,
            profile_dir: self.profile_dir.clone(),
            devtools_port: self.endpoint.as_ref().map(|endpoint| endpoint.port),
            process_id: self.child.as_ref().and_then(Child::id),
            detail,
        })
    }

    pub async fn navigate(&mut self, raw_url: &str) -> Result<NavigationResult> {
        let url = validate_navigation_url(raw_url)?;
        let endpoint = self.running_endpoint()?;
        let target = first_page_target(endpoint.port).await?;
        let websocket_url = target.web_socket_debugger_url.ok_or_else(|| {
            BrowserControlError::new("Chrome page target did not expose a WebSocket debugger URL")
        })?;
        let mut session = CdpSession::connect(&websocket_url).await?;
        let result = session
            .command("Page.navigate", json!({ "url": url.as_str() }))
            .await?;

        if let Some(error_text) = result.get("errorText").and_then(Value::as_str) {
            return Err(BrowserControlError::new(format!(
                "Chrome could not navigate to {}: {error_text}",
                url.as_str()
            )));
        }

        let frame_id = result
            .get("frameId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserControlError::new("Page.navigate omitted its frameId"))?
            .to_string();
        let loader_id = result
            .get("loaderId")
            .and_then(Value::as_str)
            .map(str::to_string);

        Ok(NavigationResult {
            url: url.into(),
            frame_id,
            loader_id,
        })
    }

    pub async fn snapshot(&mut self) -> Result<BrowserSnapshot> {
        let endpoint = self.running_endpoint()?;
        let target = first_page_target(endpoint.port).await?;
        let websocket_url = target.web_socket_debugger_url.ok_or_else(|| {
            BrowserControlError::new("Chrome page target did not expose a WebSocket debugger URL")
        })?;
        let mut session = CdpSession::connect(&websocket_url).await?;
        session.command("Page.enable", json!({})).await?;
        session.command("Runtime.enable", json!({})).await?;

        let page_state = session
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": snapshot_expression(),
                    "returnByValue": true,
                    "awaitPromise": true
                }),
            )
            .await?;
        let value = page_state
            .pointer("/result/value")
            .cloned()
            .ok_or_else(|| BrowserControlError::new("Runtime.evaluate omitted snapshot data"))?;
        let state: SnapshotState = serde_json::from_value(value).map_err(|error| {
            BrowserControlError::new(format!("Chrome returned malformed snapshot data: {error}"))
        })?;

        let screenshot = session
            .command(
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": false }),
            )
            .await?;
        let encoded_png = screenshot
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserControlError::new("Page.captureScreenshot omitted PNG data"))?;
        let screenshot_png = base64::engine::general_purpose::STANDARD
            .decode(encoded_png)
            .map_err(|error| {
                BrowserControlError::new(format!(
                    "Page.captureScreenshot returned invalid base64 PNG data: {error}"
                ))
            })?;

        Ok(BrowserSnapshot {
            url: state.url,
            title: state.title,
            loading_state: state.loading_state,
            visible_text: state.visible_text,
            interactive_elements: state.interactive_elements,
            screenshot_png,
        })
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await.map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to terminate Chrome using V4 profile {}: {error}",
                    self.profile_dir.display()
                ))
            })?;
            let _ = child.wait().await;
        }
        let _ = self.finish_stderr_capture().await;
        self.endpoint = None;
        self.last_exit = None;
        Ok(())
    }

    fn running_endpoint(&mut self) -> Result<DevToolsEndpoint> {
        let status = self.status()?;
        if status.state != BrowserState::Running {
            return Err(BrowserControlError::new(format!(
                "browser is not running for V4 profile {} (state: {:?})",
                self.profile_dir.display(),
                status.state
            )));
        }
        self.endpoint
            .clone()
            .ok_or_else(|| BrowserControlError::new("running browser has no DevTools endpoint"))
    }

    async fn terminate_child(&mut self) -> String {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.endpoint = None;
        self.finish_stderr_capture().await
    }

    async fn start_stderr_capture(&mut self, mut stderr: tokio::process::ChildStderr) {
        self.stderr_log.lock().await.clear();
        let log = Arc::clone(&self.stderr_log);
        self.stderr_task = Some(tokio::spawn(async move {
            let mut buffer = [0_u8; 2048];
            loop {
                let count = match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let mut captured = log.lock().await;
                captured.extend_from_slice(&buffer[..count]);
                if captured.len() > MAX_CHROME_STDERR_BYTES {
                    let excess = captured.len() - MAX_CHROME_STDERR_BYTES;
                    captured.drain(..excess);
                }
            }
        }));
    }

    async fn finish_stderr_capture(&mut self) -> String {
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        String::from_utf8_lossy(&self.stderr_log.lock().await)
            .trim()
            .to_string()
    }
}

fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; Chrome stderr: {stderr}")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotState {
    url: String,
    title: String,
    loading_state: String,
    visible_text: String,
    interactive_elements: Vec<InteractiveElement>,
}

struct CdpSession {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl CdpSession {
    async fn connect(websocket_url: &str) -> Result<Self> {
        let (socket, _) = timeout(CDP_COMMAND_TIMEOUT, connect_async(websocket_url))
            .await
            .map_err(|_| {
                BrowserControlError::new(format!(
                    "timed out connecting to Chrome DevTools at {websocket_url}"
                ))
            })?
            .map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to connect to Chrome DevTools at {websocket_url}: {error}"
                ))
            })?;
        Ok(Self { socket, next_id: 1 })
    }

    async fn command(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "id": id, "method": method, "params": params });
        self.socket
            .send(Message::Text(request.to_string()))
            .await
            .map_err(|error| {
                BrowserControlError::new(format!("failed to send CDP command {method}: {error}"))
            })?;

        let response = timeout(CDP_COMMAND_TIMEOUT, async {
            while let Some(message) = self.socket.next().await {
                let message = message.map_err(|error| {
                    BrowserControlError::new(format!(
                        "failed while reading CDP command {method}: {error}"
                    ))
                })?;
                let Message::Text(text) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text).map_err(|error| {
                    BrowserControlError::new(format!(
                        "Chrome returned malformed JSON for CDP command {method}: {error}"
                    ))
                })?;
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok(value);
                }
            }
            Err(BrowserControlError::new(format!(
                "Chrome closed the DevTools connection during CDP command {method}"
            )))
        })
        .await
        .map_err(|_| BrowserControlError::new(format!("CDP command {method} timed out")))??;

        if let Some(error) = response.get("error") {
            return Err(BrowserControlError::new(format!(
                "CDP command {method} failed: {error}"
            )));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| BrowserControlError::new(format!("CDP command {method} omitted result")))
    }
}

fn browser_profile_dir(v4_global_config_dir: &Path) -> PathBuf {
    v4_global_config_dir.join(PROFILE_DIR_NAME)
}

fn chrome_launch_args(profile_dir: &Path) -> Vec<String> {
    vec![
        format!("--user-data-dir={}", profile_dir.display()),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--remote-debugging-port=0".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-mode".to_string(),
        "--new-window".to_string(),
        "about:blank".to_string(),
    ]
}

fn resolve_chrome_executable(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return validate_chrome_executable(path, "browser launch option");
    }
    if let Some(raw) = env::var_os(CHROME_PATH_ENV) {
        if raw.is_empty() {
            return Err(BrowserControlError::new(format!(
                "{CHROME_PATH_ENV} is set but empty"
            )));
        }
        return validate_chrome_executable(Path::new(&raw), CHROME_PATH_ENV);
    }

    for path in platform_chrome_paths() {
        if path.is_file() {
            return Ok(path);
        }
    }
    for name in platform_chrome_commands() {
        if let Some(path) = executable_on_path(name) {
            return Ok(path);
        }
    }

    Err(BrowserControlError::new(format!(
        "Chrome was not found in standard platform locations or PATH; set {CHROME_PATH_ENV} to the Chrome executable"
    )))
}

fn validate_chrome_executable(path: &Path, source: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(BrowserControlError::new(format!(
            "Chrome path from {source} must be absolute: {}",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(BrowserControlError::new(format!(
            "Chrome path from {source} does not exist or is not a file: {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(target_os = "macos")]
fn platform_chrome_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    )];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
    }
    paths
}

#[cfg(target_os = "windows")]
fn platform_chrome_paths() -> Vec<PathBuf> {
    ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .map(|base| base.join("Google/Chrome/Application/chrome.exe"))
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_chrome_paths() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn platform_chrome_commands() -> &'static [&'static str] {
    &["chrome.exe"]
}

#[cfg(not(target_os = "windows"))]
fn platform_chrome_commands() -> &'static [&'static str] {
    &[
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ]
}

fn read_devtools_active_port(path: &Path) -> Result<Option<DevToolsEndpoint>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BrowserControlError::new(format!(
                "failed to read Chrome endpoint file {}: {error}",
                path.display()
            )))
        }
    };
    let mut lines = contents.lines();
    let port = lines
        .next()
        .ok_or_else(|| {
            BrowserControlError::new(format!("Chrome endpoint file {} is empty", path.display()))
        })?
        .parse::<u16>()
        .map_err(|error| {
            BrowserControlError::new(format!(
                "Chrome endpoint file {} has an invalid port: {error}",
                path.display()
            ))
        })?;
    let browser_path = lines.next().ok_or_else(|| {
        BrowserControlError::new(format!(
            "Chrome endpoint file {} omitted the browser WebSocket path",
            path.display()
        ))
    })?;
    if !browser_path.starts_with("/devtools/browser/") {
        return Err(BrowserControlError::new(format!(
            "Chrome endpoint file {} has an unexpected WebSocket path",
            path.display()
        )));
    }
    Ok(Some(DevToolsEndpoint { port }))
}

async fn list_page_targets(port: u16) -> Result<Vec<DevToolsTarget>> {
    let uri: Uri = format!("http://127.0.0.1:{port}/json/list")
        .parse()
        .map_err(|error| BrowserControlError::new(format!("invalid DevTools URL: {error}")))?;
    let response = timeout(CDP_COMMAND_TIMEOUT, Client::new().get(uri))
        .await
        .map_err(|_| BrowserControlError::new("timed out reading Chrome page targets"))?
        .map_err(|error| {
            BrowserControlError::new(format!(
                "failed to read Chrome page targets on 127.0.0.1:{port}: {error}"
            ))
        })?;
    if !response.status().is_success() {
        return Err(BrowserControlError::new(format!(
            "Chrome page target request returned HTTP {}",
            response.status()
        )));
    }
    let bytes = to_bytes(response.into_body()).await.map_err(|error| {
        BrowserControlError::new(format!("failed to read Chrome page target body: {error}"))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        BrowserControlError::new(format!("Chrome returned malformed page targets: {error}"))
    })
}

async fn first_page_target(port: u16) -> Result<DevToolsTarget> {
    list_page_targets(port)
        .await?
        .into_iter()
        .find(|target| target.kind == "page" && target.web_socket_debugger_url.is_some())
        .ok_or_else(|| BrowserControlError::new("Chrome has no controllable page target"))
}

fn validate_navigation_url(raw_url: &str) -> Result<Url> {
    let url = Url::parse(raw_url).map_err(|error| {
        BrowserControlError::new(format!("browser navigation URL is invalid: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BrowserControlError::new(format!(
            "browser navigation only permits http and https URLs, not {}",
            url.scheme()
        )));
    }
    Ok(url)
}

fn snapshot_expression() -> String {
    format!(
        r#"(() => {{
            const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim();
            const elements = Array.from(document.querySelectorAll(
                'a[href], button, input, select, textarea, [role], [tabindex]'
            )).filter((element) => {{
                const style = window.getComputedStyle(element);
                const rect = element.getBoundingClientRect();
                return style.visibility !== 'hidden' && style.display !== 'none' && rect.width > 0 && rect.height > 0;
            }}).slice(0, {MAX_INTERACTIVE_ELEMENTS}).map((element, index) => ({{
                index,
                tag: element.tagName.toLowerCase(),
                role: element.getAttribute('role'),
                name: element.getAttribute('aria-label') || element.getAttribute('name') || element.getAttribute('title'),
                text: normalize(element.innerText || element.value || element.alt || '').slice(0, 500),
                disabled: Boolean(element.disabled) || element.getAttribute('aria-disabled') === 'true'
            }}));
            return {{
                url: window.location.href,
                title: document.title,
                loadingState: document.readyState,
                visibleText: (document.body?.innerText || '').slice(0, {MAX_VISIBLE_TEXT_BYTES}),
                interactiveElements: elements
            }};
        }})()"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn browser_profile_is_nested_under_supplied_v4_config_root() {
        let root = Path::new("/tmp/test-gitterm-v4");
        assert_eq!(
            browser_profile_dir(root),
            PathBuf::from("/tmp/test-gitterm-v4/browser-profile")
        );
        assert_ne!(
            browser_profile_dir(root),
            PathBuf::from("/tmp/test-gitterm/browser-profile")
        );
    }

    #[test]
    fn launch_args_use_random_loopback_devtools_and_visible_dedicated_profile() {
        let profile = Path::new("/tmp/gitterm-v4/browser-profile");
        let args = chrome_launch_args(profile);
        assert!(args.contains(&format!("--user-data-dir={}", profile.display())));
        assert!(args.contains(&"--remote-debugging-address=127.0.0.1".to_string()));
        assert!(args.contains(&"--remote-debugging-port=0".to_string()));
        assert!(args.iter().all(|arg| !arg.contains("headless")));
        assert!(args
            .iter()
            .all(|arg| !arg.contains("gitterm/browser-profile")));
    }

    #[test]
    fn parses_chrome_devtools_active_port_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(DEVTOOLS_ACTIVE_PORT_FILE);
        fs::write(&path, "49152\n/devtools/browser/test-id\n").unwrap();
        let endpoint = read_devtools_active_port(&path).unwrap().unwrap();
        assert_eq!(endpoint.port, 49152);
    }

    #[test]
    fn rejects_malformed_chrome_devtools_active_port_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(DEVTOOLS_ACTIVE_PORT_FILE);
        fs::write(&path, "not-a-port\n/devtools/browser/test-id\n").unwrap();
        let error = read_devtools_active_port(&path).unwrap_err();
        assert!(error.to_string().contains("invalid port"));
    }

    #[test]
    fn navigation_rejects_non_http_schemes() {
        assert!(validate_navigation_url("https://localhost:3000").is_ok());
        assert!(validate_navigation_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_navigation_url("file:///etc/passwd").is_err());
        assert!(validate_navigation_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn snapshot_expression_includes_bounded_page_state() {
        let expression = snapshot_expression();
        assert!(expression.contains("visibleText"));
        assert!(expression.contains("interactiveElements"));
        assert!(expression.contains(&MAX_VISIBLE_TEXT_BYTES.to_string()));
        assert!(expression.contains(&MAX_INTERACTIVE_ELEMENTS.to_string()));
    }

    #[tokio::test]
    #[ignore = "launches a visible local Chrome instance"]
    async fn visible_chrome_status_navigate_snapshot_smoke() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let body = "<!doctype html><title>GitTerm browser smoke</title><main>browser-control-ready</main><button aria-label=\"Run smoke action\">Run</button>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let temp = tempdir().unwrap();
        let mut controller = BrowserController::new(temp.path().join("gitterm-v4"));
        let status = controller
            .launch(BrowserLaunchOptions::default())
            .await
            .unwrap();
        assert_eq!(status.state, BrowserState::Running);
        assert!(status.devtools_port.is_some());

        let url = format!("http://{address}/");
        controller.navigate(&url).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            let snapshot = controller.snapshot().await.unwrap();
            if snapshot.loading_state == "complete"
                && snapshot.visible_text.contains("browser-control-ready")
            {
                break snapshot;
            }
            assert!(Instant::now() < deadline, "page did not finish loading");
            sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(snapshot.url, url);
        assert_eq!(snapshot.title, "GitTerm browser smoke");
        assert!(snapshot.screenshot_png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(snapshot
            .interactive_elements
            .iter()
            .any(|element| element.name.as_deref() == Some("Run smoke action")));

        controller.disconnect().await.unwrap();
        assert_eq!(controller.status().unwrap().state, BrowserState::Stopped);
        server.await.unwrap();
    }
}
