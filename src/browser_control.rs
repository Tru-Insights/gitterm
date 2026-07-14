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
const DEFAULT_PAGE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CHROME_STDERR_BYTES: usize = 16_384;
const MAX_VISIBLE_TEXT_BYTES: usize = 100_000;
const MAX_INTERACTIVE_ELEMENTS: usize = 500;
const MIN_VIEWPORT_DIMENSION: u32 = 200;
const MAX_VIEWPORT_WIDTH: u32 = 7_680;
const MAX_VIEWPORT_HEIGHT: u32 = 4_320;

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
    pub target_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavigationResult {
    pub url: String,
    pub frame_id: String,
    pub loader_id: Option<String>,
}

/// A strict locator for one visible page element.
///
/// Role and text locators are semantic and preferred. CSS remains available
/// as an explicit escape hatch. An action fails if the locator matches zero or
/// multiple visible elements so it cannot silently operate on the wrong node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserLocator {
    Role {
        role: String,
        name: Option<String>,
        #[serde(default)]
        exact: bool,
    },
    Text {
        text: String,
        #[serde(default)]
        exact: bool,
    },
    Css {
        selector: String,
    },
}

impl BrowserLocator {
    pub fn role(role: impl Into<String>, name: impl Into<String>) -> Self {
        Self::Role {
            role: role.into(),
            name: Some(name.into()),
            exact: true,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            exact: true,
        }
    }

    pub fn css(selector: impl Into<String>) -> Self {
        Self::Css {
            selector: selector.into(),
        }
    }

    fn validate(&self) -> Result<()> {
        let (kind, value) = match self {
            Self::Role { role, .. } => ("role", role),
            Self::Text { text, .. } => ("text", text),
            Self::Css { selector } => ("CSS selector", selector),
        };
        if value.trim().is_empty() {
            return Err(BrowserControlError::new(format!(
                "browser {kind} locator must not be empty"
            )));
        }
        if let Self::Role {
            name: Some(name), ..
        } = self
        {
            if name.trim().is_empty() {
                return Err(BrowserControlError::new(
                    "browser role locator name must not be empty when provided",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserActionTarget {
    pub tag: String,
    pub role: Option<String>,
    pub name: Option<String>,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

impl BrowserViewport {
    pub fn desktop(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            device_scale_factor: 1.0,
            mobile: false,
        }
    }

    pub fn mobile(width: u32, height: u32, device_scale_factor: f64) -> Self {
        Self {
            width,
            height,
            device_scale_factor,
            mobile: true,
        }
    }

    fn validate(self) -> Result<Self> {
        if !(MIN_VIEWPORT_DIMENSION..=MAX_VIEWPORT_WIDTH).contains(&self.width) {
            return Err(BrowserControlError::new(format!(
                "browser viewport width must be between {MIN_VIEWPORT_DIMENSION} and {MAX_VIEWPORT_WIDTH}, got {}",
                self.width
            )));
        }
        if !(MIN_VIEWPORT_DIMENSION..=MAX_VIEWPORT_HEIGHT).contains(&self.height) {
            return Err(BrowserControlError::new(format!(
                "browser viewport height must be between {MIN_VIEWPORT_DIMENSION} and {MAX_VIEWPORT_HEIGHT}, got {}",
                self.height
            )));
        }
        if !self.device_scale_factor.is_finite() || !(0.5..=4.0).contains(&self.device_scale_factor)
        {
            return Err(BrowserControlError::new(format!(
                "browser device scale factor must be between 0.5 and 4.0, got {}",
                self.device_scale_factor
            )));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Space,
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
    pub locator: BrowserLocator,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshot {
    pub url: String,
    pub title: String,
    pub loading_state: String,
    pub visible_text: String,
    pub interactive_elements: Vec<InteractiveElement>,
    pub viewport: BrowserViewport,
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
    id: String,
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
    active_target_id: Option<String>,
    active_viewport: Option<BrowserViewport>,
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
            active_target_id: None,
            active_viewport: None,
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
        self.active_target_id = None;
        self.active_viewport = None;
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
                    self.active_target_id = None;
                    self.active_viewport = None;
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
                self.active_target_id = None;
                self.active_viewport = None;
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
            target_id: self.active_target_id.clone(),
            detail,
        })
    }

    pub async fn navigate(&mut self, raw_url: &str) -> Result<NavigationResult> {
        let url = validate_navigation_url(raw_url)?;
        let mut session = self.page_session().await?;
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
        let mut session = self.page_session().await?;
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
            viewport: self.active_viewport.unwrap_or(state.viewport),
            screenshot_png,
        })
    }

    /// Click one strictly located visible element using CDP mouse events.
    pub async fn click(&mut self, locator: &BrowserLocator) -> Result<BrowserActionTarget> {
        let mut session = self.page_session().await?;
        let target = resolve_locator(&mut session, locator, LocatorPreparation::Point).await?;
        session
            .command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": target.x,
                    "y": target.y
                }),
            )
            .await?;
        session
            .command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "x": target.x,
                    "y": target.y,
                    "button": "left",
                    "buttons": 1,
                    "clickCount": 1
                }),
            )
            .await?;
        session
            .command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "x": target.x,
                    "y": target.y,
                    "button": "left",
                    "buttons": 0,
                    "clickCount": 1
                }),
            )
            .await?;
        Ok(target)
    }

    /// Replace the contents of one editable element and leave it focused.
    pub async fn type_text(
        &mut self,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionTarget> {
        let mut session = self.page_session().await?;
        let target =
            resolve_locator(&mut session, locator, LocatorPreparation::ReplaceText).await?;
        session
            .command("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(target)
    }

    /// Press one supported non-text key against the currently focused element.
    pub async fn press(&mut self, key: BrowserKey) -> Result<()> {
        let descriptor = key_descriptor(key);
        let mut session = self.page_session().await?;
        let mut key_down = json!({
            "type": if descriptor.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": descriptor.key,
            "code": descriptor.code,
            "windowsVirtualKeyCode": descriptor.virtual_key_code,
            "nativeVirtualKeyCode": descriptor.virtual_key_code
        });
        if let Some(text) = descriptor.text {
            key_down["text"] = Value::String(text.to_string());
            key_down["unmodifiedText"] = Value::String(text.to_string());
        }
        session.command("Input.dispatchKeyEvent", key_down).await?;
        session
            .command(
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp",
                    "key": descriptor.key,
                    "code": descriptor.code,
                    "windowsVirtualKeyCode": descriptor.virtual_key_code,
                    "nativeVirtualKeyCode": descriptor.virtual_key_code
                }),
            )
            .await?;
        Ok(())
    }

    /// Reload the current page and wait for a new document to finish loading.
    pub async fn reload(&mut self, ignore_cache: bool) -> Result<()> {
        let mut session = self.page_session().await?;
        session.command("Page.enable", json!({})).await?;
        session.command("Runtime.enable", json!({})).await?;
        let previous_time_origin = page_time_origin(&mut session).await?;
        session
            .command("Page.reload", json!({ "ignoreCache": ignore_cache }))
            .await?;
        wait_for_document_ready(
            &mut session,
            Some(previous_time_origin),
            DEFAULT_PAGE_WAIT_TIMEOUT,
        )
        .await
    }

    /// Apply responsive device metrics to the controlled page target.
    pub async fn resize(&mut self, viewport: BrowserViewport) -> Result<BrowserViewport> {
        let viewport = viewport.validate()?;
        let mut session = self.page_session().await?;
        session
            .command(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": viewport.width,
                    "height": viewport.height,
                    "deviceScaleFactor": viewport.device_scale_factor,
                    "mobile": viewport.mobile
                }),
            )
            .await?;
        self.active_viewport = Some(viewport);
        Ok(viewport)
    }

    /// Wait until the current document reports a complete loading state.
    pub async fn wait_for_ready(&mut self, max_wait: Duration) -> Result<()> {
        if max_wait.is_zero() {
            return Err(BrowserControlError::new(
                "browser readiness timeout must be greater than zero",
            ));
        }
        let mut session = self.page_session().await?;
        session.command("Runtime.enable", json!({})).await?;
        wait_for_document_ready(&mut session, None, max_wait).await
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
        self.active_target_id = None;
        self.active_viewport = None;
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

    async fn page_session(&mut self) -> Result<CdpSession> {
        let endpoint = self.running_endpoint()?;
        let targets = list_page_targets(endpoint.port).await?;
        let target = self
            .active_target_id
            .as_deref()
            .and_then(|active_id| {
                targets.iter().find(|target| {
                    target.id == active_id
                        && target.kind == "page"
                        && target.web_socket_debugger_url.is_some()
                })
            })
            .or_else(|| {
                targets.iter().find(|target| {
                    target.kind == "page" && target.web_socket_debugger_url.is_some()
                })
            })
            .ok_or_else(|| BrowserControlError::new("Chrome has no controllable page target"))?;
        if self.active_target_id.as_deref() != Some(target.id.as_str()) {
            self.active_target_id = Some(target.id.clone());
            self.active_viewport = None;
        }
        let websocket_url = target.web_socket_debugger_url.as_deref().ok_or_else(|| {
            BrowserControlError::new("Chrome page target did not expose a WebSocket debugger URL")
        })?;
        CdpSession::connect(websocket_url).await
    }

    async fn terminate_child(&mut self) -> String {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.endpoint = None;
        self.active_target_id = None;
        self.active_viewport = None;
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

/// Cloneable serialized access to a single browser controller.
///
/// Every operation holds the same async mutex for its complete CDP sequence,
/// preventing concurrent MCP/tool calls from interleaving input on the page.
#[derive(Clone)]
pub struct BrowserControlService {
    controller: Arc<Mutex<BrowserController>>,
}

impl BrowserControlService {
    pub fn new(v4_global_config_dir: impl AsRef<Path>) -> Self {
        Self {
            controller: Arc::new(Mutex::new(BrowserController::new(v4_global_config_dir))),
        }
    }

    pub async fn profile_dir(&self) -> PathBuf {
        self.controller.lock().await.profile_dir().to_path_buf()
    }

    pub async fn launch(&self, options: BrowserLaunchOptions) -> Result<BrowserStatus> {
        self.controller.lock().await.launch(options).await
    }

    pub async fn status(&self) -> Result<BrowserStatus> {
        self.controller.lock().await.status()
    }

    pub async fn navigate(&self, url: &str) -> Result<NavigationResult> {
        self.controller.lock().await.navigate(url).await
    }

    pub async fn snapshot(&self) -> Result<BrowserSnapshot> {
        self.controller.lock().await.snapshot().await
    }

    pub async fn click(&self, locator: &BrowserLocator) -> Result<BrowserActionTarget> {
        self.controller.lock().await.click(locator).await
    }

    pub async fn type_text(
        &self,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionTarget> {
        self.controller.lock().await.type_text(locator, text).await
    }

    pub async fn press(&self, key: BrowserKey) -> Result<()> {
        self.controller.lock().await.press(key).await
    }

    pub async fn reload(&self, ignore_cache: bool) -> Result<()> {
        self.controller.lock().await.reload(ignore_cache).await
    }

    pub async fn resize(&self, viewport: BrowserViewport) -> Result<BrowserViewport> {
        self.controller.lock().await.resize(viewport).await
    }

    pub async fn wait_for_ready(&self, max_wait: Duration) -> Result<()> {
        self.controller.lock().await.wait_for_ready(max_wait).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.controller.lock().await.disconnect().await
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
    viewport: BrowserViewport,
}

#[derive(Debug, Clone, Copy)]
enum LocatorPreparation {
    Point,
    ReplaceText,
}

impl LocatorPreparation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::ReplaceText => "replace_text",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocatorResolution {
    match_count: usize,
    target: Option<ResolvedActionTarget>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedActionTarget {
    tag: String,
    role: Option<String>,
    name: Option<String>,
    x: f64,
    y: f64,
    disabled: bool,
}

async fn resolve_locator(
    session: &mut CdpSession,
    locator: &BrowserLocator,
    preparation: LocatorPreparation,
) -> Result<BrowserActionTarget> {
    locator.validate()?;
    let expression = locator_expression(locator, preparation)?;
    let result = session
        .command(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true
            }),
        )
        .await?;
    let value = result
        .pointer("/result/value")
        .cloned()
        .ok_or_else(|| BrowserControlError::new("Chrome omitted locator resolution data"))?;
    let resolution: LocatorResolution = serde_json::from_value(value).map_err(|error| {
        BrowserControlError::new(format!("Chrome returned malformed locator data: {error}"))
    })?;

    if let Some(error) = resolution.error {
        return Err(BrowserControlError::new(format!(
            "browser locator could not be resolved: {error}"
        )));
    }
    if resolution.match_count == 0 {
        return Err(BrowserControlError::new(
            "browser locator matched no visible elements",
        ));
    }
    if resolution.match_count > 1 {
        return Err(BrowserControlError::new(format!(
            "browser locator is ambiguous: matched {} visible elements",
            resolution.match_count
        )));
    }
    let target = resolution.target.ok_or_else(|| {
        BrowserControlError::new("Chrome omitted the uniquely resolved browser target")
    })?;
    if target.disabled {
        return Err(BrowserControlError::new(format!(
            "browser locator resolved to a disabled {} element",
            target.tag
        )));
    }
    Ok(BrowserActionTarget {
        tag: target.tag,
        role: target.role,
        name: target.name,
        x: target.x,
        y: target.y,
    })
}

async fn page_time_origin(session: &mut CdpSession) -> Result<f64> {
    let result = session
        .command(
            "Runtime.evaluate",
            json!({
                "expression": "performance.timeOrigin",
                "returnByValue": true
            }),
        )
        .await?;
    result
        .pointer("/result/value")
        .and_then(Value::as_f64)
        .ok_or_else(|| BrowserControlError::new("Chrome omitted the page time origin"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentReadyState {
    loading_state: String,
    time_origin: f64,
}

async fn wait_for_document_ready(
    session: &mut CdpSession,
    previous_time_origin: Option<f64>,
    max_wait: Duration,
) -> Result<()> {
    let deadline = Instant::now() + max_wait;
    loop {
        let response = session
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": "({ loadingState: document.readyState, timeOrigin: performance.timeOrigin })",
                    "returnByValue": true
                }),
            )
            .await;
        let detail = match response {
            Ok(result) => {
                let value = result.pointer("/result/value").cloned().ok_or_else(|| {
                    BrowserControlError::new("Chrome omitted document readiness data")
                })?;
                let state: DocumentReadyState = serde_json::from_value(value).map_err(|error| {
                    BrowserControlError::new(format!(
                        "Chrome returned malformed document readiness data: {error}"
                    ))
                })?;
                let is_new_document = previous_time_origin
                    .map(|previous| state.time_origin != previous)
                    .unwrap_or(true);
                if state.loading_state == "complete" && is_new_document {
                    return Ok(());
                }
                format!("last loading state: {}", state.loading_state)
            }
            Err(error) => {
                // A navigation can briefly destroy the JavaScript execution
                // context. Retry within the explicit wait window and surface
                // the final CDP error if readiness never returns.
                format!("last CDP error: {error}")
            }
        };

        if Instant::now() >= deadline {
            return Err(BrowserControlError::new(format!(
                "page did not become ready within {} ms ({detail})",
                max_wait.as_millis()
            )));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

struct KeyDescriptor {
    key: &'static str,
    code: &'static str,
    virtual_key_code: u32,
    text: Option<&'static str>,
}

fn key_descriptor(key: BrowserKey) -> KeyDescriptor {
    match key {
        BrowserKey::Enter => KeyDescriptor {
            key: "Enter",
            code: "Enter",
            virtual_key_code: 13,
            text: Some("\r"),
        },
        BrowserKey::Tab => KeyDescriptor {
            key: "Tab",
            code: "Tab",
            virtual_key_code: 9,
            text: None,
        },
        BrowserKey::Escape => KeyDescriptor {
            key: "Escape",
            code: "Escape",
            virtual_key_code: 27,
            text: None,
        },
        BrowserKey::Backspace => KeyDescriptor {
            key: "Backspace",
            code: "Backspace",
            virtual_key_code: 8,
            text: None,
        },
        BrowserKey::Delete => KeyDescriptor {
            key: "Delete",
            code: "Delete",
            virtual_key_code: 46,
            text: None,
        },
        BrowserKey::ArrowUp => KeyDescriptor {
            key: "ArrowUp",
            code: "ArrowUp",
            virtual_key_code: 38,
            text: None,
        },
        BrowserKey::ArrowDown => KeyDescriptor {
            key: "ArrowDown",
            code: "ArrowDown",
            virtual_key_code: 40,
            text: None,
        },
        BrowserKey::ArrowLeft => KeyDescriptor {
            key: "ArrowLeft",
            code: "ArrowLeft",
            virtual_key_code: 37,
            text: None,
        },
        BrowserKey::ArrowRight => KeyDescriptor {
            key: "ArrowRight",
            code: "ArrowRight",
            virtual_key_code: 39,
            text: None,
        },
        BrowserKey::Home => KeyDescriptor {
            key: "Home",
            code: "Home",
            virtual_key_code: 36,
            text: None,
        },
        BrowserKey::End => KeyDescriptor {
            key: "End",
            code: "End",
            virtual_key_code: 35,
            text: None,
        },
        BrowserKey::PageUp => KeyDescriptor {
            key: "PageUp",
            code: "PageUp",
            virtual_key_code: 33,
            text: None,
        },
        BrowserKey::PageDown => KeyDescriptor {
            key: "PageDown",
            code: "PageDown",
            virtual_key_code: 34,
            text: None,
        },
        BrowserKey::Space => KeyDescriptor {
            key: " ",
            code: "Space",
            virtual_key_code: 32,
            text: Some(" "),
        },
    }
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

fn locator_expression(locator: &BrowserLocator, preparation: LocatorPreparation) -> Result<String> {
    let locator_json = serde_json::to_string(locator).map_err(|error| {
        BrowserControlError::new(format!("failed to serialize browser locator: {error}"))
    })?;
    let preparation_json = serde_json::to_string(preparation.as_str()).map_err(|error| {
        BrowserControlError::new(format!("failed to serialize browser action: {error}"))
    })?;
    Ok(r#"(() => {
        const locator = __LOCATOR__;
        const preparation = __PREPARATION__;
        const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim();
        const visible = (element) => {
            const style = window.getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.visibility !== 'hidden' && style.display !== 'none' && rect.width > 0 && rect.height > 0;
        };
        const roleFor = (element) => {
            const explicit = element.getAttribute('role');
            if (explicit) return explicit;
            const tag = element.tagName.toLowerCase();
            if (tag === 'a' && element.hasAttribute('href')) return 'link';
            if (tag === 'button') return 'button';
            if (tag === 'textarea') return 'textbox';
            if (tag === 'select') return element.multiple ? 'listbox' : 'combobox';
            if (tag === 'input') {
                const type = (element.type || 'text').toLowerCase();
                if (type === 'checkbox') return 'checkbox';
                if (type === 'radio') return 'radio';
                if (type === 'range') return 'slider';
                if (['button', 'submit', 'reset'].includes(type)) return 'button';
                if (type !== 'hidden') return 'textbox';
            }
            return null;
        };
        const nameFor = (element) => {
            const labelledBy = element.getAttribute('aria-labelledby');
            const labelledText = labelledBy ? labelledBy.split(/\s+/).map((id) => document.getElementById(id)?.innerText || '').join(' ') : '';
            const labelText = element.labels ? Array.from(element.labels).map((label) => label.innerText).join(' ') : '';
            return normalize(
                element.getAttribute('aria-label') || labelledText || labelText ||
                element.getAttribute('alt') || element.getAttribute('title') ||
                element.getAttribute('placeholder') || element.innerText || element.value
            ) || null;
        };
        const textMatches = (actual, expected, exact) => {
            const normalizedActual = normalize(actual);
            const normalizedExpected = normalize(expected);
            return exact
                ? normalizedActual === normalizedExpected
                : normalizedActual.toLocaleLowerCase().includes(normalizedExpected.toLocaleLowerCase());
        };

        let matches = [];
        try {
            if (locator.kind === 'css') {
                matches = Array.from(document.querySelectorAll(locator.selector));
            } else if (locator.kind === 'role') {
                matches = Array.from(document.querySelectorAll('a[href], button, input, select, textarea, [role], [tabindex]'))
                    .filter((element) => roleFor(element) === locator.role)
                    .filter((element) => locator.name === null || textMatches(nameFor(element), locator.name, locator.exact));
            } else if (locator.kind === 'text') {
                matches = Array.from(document.querySelectorAll('body *')).filter((element) => {
                    if (!textMatches(element.innerText, locator.text, locator.exact)) return false;
                    return !Array.from(element.children).some((child) => textMatches(child.innerText, locator.text, locator.exact));
                });
            } else {
                return { matchCount: 0, target: null, error: `unsupported locator kind: ${locator.kind}` };
            }
        } catch (error) {
            return { matchCount: 0, target: null, error: String(error) };
        }

        matches = Array.from(new Set(matches)).filter(visible);
        if (matches.length !== 1) {
            return { matchCount: matches.length, target: null, error: null };
        }

        const element = matches[0];
        const disabled = Boolean(element.disabled) || element.getAttribute('aria-disabled') === 'true';
        if (preparation === 'replace_text') {
            const tag = element.tagName.toLowerCase();
            const inputType = tag === 'input' ? (element.type || 'text').toLowerCase() : null;
            const textInput = tag === 'textarea' || (tag === 'input' && !['button', 'checkbox', 'file', 'hidden', 'radio', 'range', 'reset', 'submit'].includes(inputType));
            if (!textInput && !element.isContentEditable) {
                return { matchCount: 1, target: null, error: `matched ${tag} element is not editable` };
            }
            element.focus();
            if (textInput) {
                element.select();
            } else {
                const selection = window.getSelection();
                const range = document.createRange();
                range.selectNodeContents(element);
                selection.removeAllRanges();
                selection.addRange(range);
            }
        } else {
            element.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        }
        const rect = element.getBoundingClientRect();
        return {
            matchCount: 1,
            error: null,
            target: {
                tag: element.tagName.toLowerCase(),
                role: roleFor(element),
                name: nameFor(element),
                x: rect.left + (rect.width / 2),
                y: rect.top + (rect.height / 2),
                disabled
            }
        };
    })()"#
        .replace("__LOCATOR__", &locator_json)
        .replace("__PREPARATION__", &preparation_json))
}

fn snapshot_expression() -> String {
    r#"(() => {
            const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim();
            const roleFor = (element) => {
                const explicit = element.getAttribute('role');
                if (explicit) return explicit;
                const tag = element.tagName.toLowerCase();
                if (tag === 'a' && element.hasAttribute('href')) return 'link';
                if (tag === 'button') return 'button';
                if (tag === 'textarea') return 'textbox';
                if (tag === 'select') return element.multiple ? 'listbox' : 'combobox';
                if (tag === 'input') {
                    const type = (element.type || 'text').toLowerCase();
                    if (type === 'checkbox') return 'checkbox';
                    if (type === 'radio') return 'radio';
                    if (type === 'range') return 'slider';
                    if (['button', 'submit', 'reset'].includes(type)) return 'button';
                    if (type !== 'hidden') return 'textbox';
                }
                return null;
            };
            const nameFor = (element) => {
                const labelledBy = element.getAttribute('aria-labelledby');
                const labelledText = labelledBy ? labelledBy.split(/\s+/).map((id) => document.getElementById(id)?.innerText || '').join(' ') : '';
                const labelText = element.labels ? Array.from(element.labels).map((label) => label.innerText).join(' ') : '';
                return normalize(
                    element.getAttribute('aria-label') || labelledText || labelText ||
                    element.getAttribute('alt') || element.getAttribute('title') ||
                    element.getAttribute('placeholder') || element.innerText || element.value
                ) || null;
            };
            const selectorFor = (element) => {
                if (element.id) return `#${CSS.escape(element.id)}`;
                const parts = [];
                let current = element;
                while (current && current !== document.body) {
                    const tag = current.tagName.toLowerCase();
                    const siblings = current.parentElement ? Array.from(current.parentElement.children).filter((sibling) => sibling.tagName === current.tagName) : [];
                    const suffix = siblings.length > 1 ? `:nth-of-type(${siblings.indexOf(current) + 1})` : '';
                    parts.unshift(`${tag}${suffix}`);
                    current = current.parentElement;
                }
                return `body > ${parts.join(' > ')}`;
            };
            const elements = Array.from(document.querySelectorAll(
                'a[href], button, input, select, textarea, [role], [tabindex]'
            )).filter((element) => {
                const style = window.getComputedStyle(element);
                const rect = element.getBoundingClientRect();
                return style.visibility !== 'hidden' && style.display !== 'none' && rect.width > 0 && rect.height > 0;
            }).slice(0, __MAX_INTERACTIVE_ELEMENTS__).map((element, index) => {
                const role = roleFor(element);
                const name = nameFor(element);
                return {
                index,
                tag: element.tagName.toLowerCase(),
                role,
                name,
                text: normalize(element.innerText || element.value || element.alt || '').slice(0, 500),
                disabled: Boolean(element.disabled) || element.getAttribute('aria-disabled') === 'true',
                locator: role && name
                    ? { kind: 'role', role, name, exact: true }
                    : { kind: 'css', selector: selectorFor(element) }
                };
            });
            return {
                url: window.location.href,
                title: document.title,
                loadingState: document.readyState,
                visibleText: (document.body?.innerText || '').slice(0, __MAX_VISIBLE_TEXT_BYTES__),
                interactiveElements: elements,
                viewport: {
                    width: window.innerWidth,
                    height: window.innerHeight,
                    deviceScaleFactor: window.devicePixelRatio,
                    mobile: false
                }
            };
        })()"#
        .replace(
            "__MAX_INTERACTIVE_ELEMENTS__",
            &MAX_INTERACTIVE_ELEMENTS.to_string(),
        )
        .replace(
            "__MAX_VISIBLE_TEXT_BYTES__",
            &MAX_VISIBLE_TEXT_BYTES.to_string(),
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
        assert!(expression.contains("locator"));
        assert!(expression.contains("viewport"));
        assert!(expression.contains(&MAX_VISIBLE_TEXT_BYTES.to_string()));
        assert!(expression.contains(&MAX_INTERACTIVE_ELEMENTS.to_string()));
    }

    #[test]
    fn locators_validate_and_serialize_user_text_safely() {
        let locator = BrowserLocator::role("button", "Save \"quoted\" value");
        locator.validate().unwrap();
        let expression = locator_expression(&locator, LocatorPreparation::Point).unwrap();
        assert!(expression.contains(r#"Save \"quoted\" value"#));
        assert!(!expression.contains("__LOCATOR__"));

        let error = BrowserLocator::css("   ").validate().unwrap_err();
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn viewport_validation_bounds_responsive_metrics() {
        assert!(BrowserViewport::desktop(1_280, 720).validate().is_ok());
        assert!(BrowserViewport::mobile(390, 844, 3.0).validate().is_ok());
        assert!(BrowserViewport::desktop(199, 720).validate().is_err());
        assert!(BrowserViewport::mobile(390, 844, 5.0).validate().is_err());
    }

    #[test]
    fn key_descriptors_include_expected_cdp_values() {
        let enter = key_descriptor(BrowserKey::Enter);
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.virtual_key_code, 13);
        assert_eq!(enter.text, Some("\r"));

        let escape = key_descriptor(BrowserKey::Escape);
        assert_eq!(escape.code, "Escape");
        assert_eq!(escape.text, None);
    }

    #[tokio::test]
    #[ignore = "launches a visible local Chrome instance"]
    async fn visible_chrome_status_navigate_snapshot_smoke() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    let _ = socket.read(&mut request).await.unwrap();
                    let body = r#"<!doctype html>
                        <title>GitTerm browser smoke</title>
                        <main id="status">browser-control-ready</main>
                        <form onsubmit="event.preventDefault(); document.getElementById('status').textContent = 'submitted:' + document.getElementById('smoke-input').value">
                            <label for="smoke-input">Smoke input</label>
                            <input id="smoke-input" name="smokeInput">
                            <button type="button" aria-label="Run smoke action" onclick="document.getElementById('status').textContent = 'clicked'">Run</button>
                        </form>"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });

        let temp = tempdir().unwrap();
        let service = BrowserControlService::new(temp.path().join("gitterm-v4"));
        let status = service
            .launch(BrowserLaunchOptions::default())
            .await
            .unwrap();
        assert_eq!(status.state, BrowserState::Running);
        assert!(status.devtools_port.is_some());

        let url = format!("http://{address}/");
        service.navigate(&url).await.unwrap();
        service
            .wait_for_ready(Duration::from_secs(5))
            .await
            .unwrap();
        let target_id = service.status().await.unwrap().target_id.unwrap();
        let snapshot = service.snapshot().await.unwrap();
        assert_eq!(snapshot.url, url);
        assert_eq!(snapshot.title, "GitTerm browser smoke");
        assert!(snapshot.screenshot_png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let button = snapshot
            .interactive_elements
            .iter()
            .find(|element| element.name.as_deref() == Some("Run smoke action"))
            .unwrap();
        assert_eq!(button.role.as_deref(), Some("button"));
        assert_eq!(
            button.locator,
            BrowserLocator::role("button", "Run smoke action")
        );

        let mobile = BrowserViewport::mobile(390, 844, 3.0);
        service.resize(mobile).await.unwrap();
        assert_eq!(service.snapshot().await.unwrap().viewport, mobile);

        service.click(&button.locator).await.unwrap();
        assert!(service
            .snapshot()
            .await
            .unwrap()
            .visible_text
            .contains("clicked"));

        let input = BrowserLocator::role("textbox", "Smoke input");
        service.type_text(&input, "typed value").await.unwrap();
        let typed_snapshot = service.snapshot().await.unwrap();
        assert!(typed_snapshot
            .interactive_elements
            .iter()
            .any(|element| element.text == "typed value"));

        service.press(BrowserKey::Enter).await.unwrap();
        assert!(service
            .snapshot()
            .await
            .unwrap()
            .visible_text
            .contains("submitted:typed value"));

        service.reload(false).await.unwrap();
        assert_eq!(
            service.status().await.unwrap().target_id.as_deref(),
            Some(target_id.as_str())
        );
        assert!(service
            .snapshot()
            .await
            .unwrap()
            .visible_text
            .contains("browser-control-ready"));

        service.disconnect().await.unwrap();
        assert_eq!(service.status().await.unwrap().state, BrowserState::Stopped);
        server.abort();
    }
}
