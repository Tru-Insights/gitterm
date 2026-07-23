//! Dedicated-profile Chrome control through the Chrome DevTools Protocol.
//!
//! This adapter deliberately has no dependency on GitTerm's singleton Wry
//! webview. Callers provide the V4 global config directory so browser state is
//! always rooted in the same isolated configuration tree.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use hyper::{body::to_bytes, Client, Uri};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
use url::Url;

const PROFILE_DIR_NAME: &str = "browser-profile";
const DEVTOOLS_ACTIVE_PORT_FILE: &str = "DevToolsActivePort";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_CAPTURE_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const CDP_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_PAGE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const TARGET_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CHROME_STDERR_BYTES: usize = 16_384;
const MAX_VISIBLE_TEXT_BYTES: usize = 100_000;
const MAX_INTERACTIVE_ELEMENTS: usize = 500;
const MAX_CONSOLE_ERRORS: usize = 100;
const MAX_NETWORK_FAILURES: usize = 100;
const MAX_TRACKED_REQUESTS: usize = 512;
const MAX_DIAGNOSTIC_TEXT_CHARS: usize = 4_000;
const MAX_DIAGNOSTIC_URL_CHARS: usize = 2_048;
const MAX_DOM_OUTLINE_NODES: usize = 300;
const MAX_DOM_SCANNED_ELEMENTS: usize = 5_000;
const MAX_DOM_OUTLINE_DEPTH: usize = 16;
static NEXT_BROWSER_CONTROL_SERVICE_ID: AtomicU64 = AtomicU64::new(1);
const MAX_DOM_TEXT_CHARS: usize = 200;
const MAX_DOM_FIELD_CHARS: usize = 500;
const MAX_DOM_CLASSES: usize = 16;
const MAX_NODE_ATTRIBUTES: usize = 50;
const MAX_NODE_ATTRIBUTE_VALUE_CHARS: usize = 2_000;
const MAX_STYLE_VALUE_CHARS: usize = 2_000;
const MAX_OUTER_HTML_CHARS: usize = 10_000;
const MAX_NODE_REF_CHARS: usize = 128;
const MAX_LOCATOR_VALUE_CHARS: usize = 2_000;
const MAX_BROWSER_TARGET_NAME_CHARS: usize = 32;
const MAX_EVIDENCE_LABEL_CHARS: usize = 80;
const MAX_RETAINED_EVIDENCE: usize = 6;
const MAX_FULL_PAGE_CAPTURE_WIDTH: f64 = 7_680.0;
const MAX_FULL_PAGE_CAPTURE_HEIGHT: f64 = 20_000.0;
const MAX_FULL_PAGE_CAPTURE_PIXELS: f64 = 100_000_000.0;
const MIN_VIEWPORT_DIMENSION: u32 = 200;
const MAX_VIEWPORT_WIDTH: u32 = 7_680;
const MAX_VIEWPORT_HEIGHT: u32 = 4_320;
const BROWSER_PROFILE_NAME: &str = "GitTerm V4 Browser";
const DEFAULT_BROWSER_TARGET_NAME: &str = "primary";
// Catppuccin Mocha mauve (#cba6f7) encoded as Chrome's signed ARGB SkColor.
const BROWSER_PROFILE_COLOR: i64 = -3_430_665;

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
    pub active_target: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTargetInfo {
    pub name: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserOperation {
    Status,
    Open,
    Targets,
    TargetOpen,
    TargetFocus,
    TargetClose,
    Navigate,
    Snapshot,
    Click,
    Type,
    Press,
    Scroll,
    Resize,
    Reload,
    Wait,
    Console,
    Network,
    Dom,
    Styles,
    Disconnect,
}

impl BrowserOperation {
    pub fn active_label(self) -> &'static str {
        match self {
            Self::Status => "Checking status",
            Self::Open => "Opening browser",
            Self::Targets => "Listing browser targets",
            Self::TargetOpen => "Opening browser target",
            Self::TargetFocus => "Focusing browser target",
            Self::TargetClose => "Closing browser target",
            Self::Navigate => "Navigating",
            Self::Snapshot => "Capturing evidence",
            Self::Click => "Clicking",
            Self::Type => "Typing",
            Self::Press => "Pressing key",
            Self::Scroll => "Scrolling",
            Self::Resize => "Resizing",
            Self::Reload => "Reloading",
            Self::Wait => "Waiting",
            Self::Console => "Inspecting console",
            Self::Network => "Inspecting network",
            Self::Dom => "Inspecting page structure",
            Self::Styles => "Inspecting node styles",
            Self::Disconnect => "Disconnecting",
        }
    }

    pub fn completed_label(self) -> &'static str {
        match self {
            Self::Status => "Status checked",
            Self::Open => "Browser opened",
            Self::Targets => "Browser targets listed",
            Self::TargetOpen => "Browser target opened",
            Self::TargetFocus => "Browser target focused",
            Self::TargetClose => "Browser target closed",
            Self::Navigate => "Navigation complete",
            Self::Snapshot => "Evidence captured",
            Self::Click => "Click complete",
            Self::Type => "Typing complete",
            Self::Press => "Key pressed",
            Self::Scroll => "Scroll complete",
            Self::Resize => "Viewport resized",
            Self::Reload => "Reload complete",
            Self::Wait => "Wait complete",
            Self::Console => "Console inspected",
            Self::Network => "Network inspected",
            Self::Dom => "Page structure inspected",
            Self::Styles => "Node styles inspected",
            Self::Disconnect => "Browser disconnected",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrowserOperationActivity {
    pub id: u64,
    pub operation: BrowserOperation,
    pub started_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct BrowserOperationOutcome {
    pub operation: BrowserOperation,
    pub finished_at: SystemTime,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct BrowserEvidence {
    pub id: u64,
    pub target: String,
    pub label: String,
    pub capture_mode: BrowserCaptureMode,
    pub title: String,
    pub display_url: Option<String>,
    pub viewport: BrowserViewport,
    pub captured_at: SystemTime,
    pub console_error_count: usize,
    pub failed_request_count: usize,
    pub screenshot_png: Arc<Vec<u8>>,
}

impl fmt::Debug for BrowserEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserEvidence")
            .field("id", &self.id)
            .field("target", &self.target)
            .field("label", &self.label)
            .field("capture_mode", &self.capture_mode)
            .field("title", &self.title)
            .field("display_url", &self.display_url)
            .field("viewport", &self.viewport)
            .field("captured_at", &self.captured_at)
            .field("console_error_count", &self.console_error_count)
            .field("failed_request_count", &self.failed_request_count)
            .field("screenshot_png_bytes", &self.screenshot_png.len())
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct BrowserTelemetrySnapshot {
    pub revision: u64,
    pub active_agent_operation: Option<BrowserOperationActivity>,
    pub last_agent_operation: Option<BrowserOperationOutcome>,
    pub latest_evidence: Option<BrowserEvidence>,
    pub evidence: Vec<BrowserEvidence>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
        if value.chars().count() > MAX_LOCATOR_VALUE_CHARS {
            return Err(BrowserControlError::new(format!(
                "browser {kind} locator must not exceed {MAX_LOCATOR_VALUE_CHARS} characters"
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
            if name.chars().count() > MAX_LOCATOR_VALUE_CHARS {
                return Err(BrowserControlError::new(format!(
                    "browser role locator name must not exceed {MAX_LOCATOR_VALUE_CHARS} characters"
                )));
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConsoleError {
    pub source: String,
    pub text: String,
    pub url: Option<String>,
    pub line_number: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserNetworkFailure {
    pub url: String,
    pub method: Option<String>,
    pub status: Option<u16>,
    pub error_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDiagnostics {
    pub console_errors: Vec<BrowserConsoleError>,
    pub failed_requests: Vec<BrowserNetworkFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDomBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDomNode {
    pub node_ref: String,
    pub parent_ref: Option<String>,
    pub depth: usize,
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub bounds: BrowserDomBounds,
    pub disabled: bool,
    pub expanded: Option<String>,
    pub selected: Option<String>,
    pub checked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDomOutline {
    pub target: String,
    pub document_id: String,
    pub url: String,
    pub title: String,
    pub nodes: Vec<BrowserDomNode>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserNodeSelector {
    NodeRef { node_ref: String },
    Locator { locator: BrowserLocator },
}

impl BrowserNodeSelector {
    fn validate(&self) -> Result<()> {
        match self {
            Self::NodeRef { node_ref }
                if node_ref.trim().is_empty() || node_ref.chars().count() > MAX_NODE_REF_CHARS =>
            {
                Err(BrowserControlError::new(format!(
                    "browser DOM node_ref must contain 1 to {MAX_NODE_REF_CHARS} characters"
                )))
            }
            Self::NodeRef { .. } => Ok(()),
            Self::Locator { locator } => locator.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDomAttribute {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStyleProperty {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserNodeDetails {
    pub target: String,
    pub document_id: String,
    pub node_ref: String,
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub bounds: BrowserDomBounds,
    pub attributes: Vec<BrowserDomAttribute>,
    pub attributes_truncated: bool,
    pub styles: Vec<BrowserStyleProperty>,
    pub outer_html: String,
    pub outer_html_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserLoadingState {
    Loading,
    Interactive,
    Complete,
}

impl BrowserLoadingState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Interactive => "interactive",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserWaitCondition {
    Locator {
        locator: BrowserLocator,
    },
    Text {
        text: String,
        #[serde(default)]
        exact: bool,
    },
    Url {
        url: String,
        #[serde(default)]
        exact: bool,
    },
    LoadingState {
        state: BrowserLoadingState,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshot {
    pub target: String,
    pub capture_mode: BrowserCaptureMode,
    pub url: String,
    pub title: String,
    pub loading_state: String,
    pub visible_text: String,
    pub interactive_elements: Vec<InteractiveElement>,
    pub viewport: BrowserViewport,
    pub console_errors: Vec<BrowserConsoleError>,
    pub failed_requests: Vec<BrowserNetworkFailure>,
    #[serde(skip_serializing)]
    pub screenshot_png: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserCaptureMode {
    Viewport,
    FullPage,
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
    browser_websocket_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevToolsTarget {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    web_socket_debugger_url: Option<String>,
}

struct ManagedBrowserTarget {
    target_id: String,
    viewport: Option<BrowserViewport>,
    session: Option<CdpSession>,
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
    active_target_name: Option<String>,
    targets: HashMap<String, ManagedBrowserTarget>,
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
            active_target_name: None,
            targets: HashMap::new(),
            last_exit: None,
            stderr_log: Arc::new(Mutex::new(Vec::new())),
            stderr_task: None,
        }
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    pub async fn launch(&mut self, options: BrowserLaunchOptions) -> Result<BrowserStatus> {
        let current = self.status().await?;
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
                self.endpoint = Some(endpoint);
                self.last_exit = None;
                return self.status_snapshot();
            }
            std::fs::remove_file(&active_port_path).map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to remove stale Chrome endpoint file {}: {error}",
                    active_port_path.display()
                ))
            })?;
        }

        ensure_browser_profile_branding(&self.profile_dir)?;

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
        self.clear_targets();
        self.last_exit = None;

        let deadline = Instant::now() + options.startup_timeout;
        loop {
            if let Some(endpoint) = read_devtools_active_port(&active_port_path)? {
                match list_page_targets(endpoint.port).await {
                    Ok(_) => {
                        self.endpoint = Some(endpoint);
                        return self.status().await;
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
                    self.clear_targets();
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

    fn status_snapshot(&mut self) -> Result<BrowserStatus> {
        if let Some(child) = self.child.as_mut() {
            if let Some(exit) = child.try_wait().map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to inspect the Chrome process launched with profile {}: {error}",
                    self.profile_dir.display()
                ))
            })? {
                self.child = None;
                self.endpoint = None;
                self.clear_targets();
                self.last_exit = Some(exit.to_string());
            }
        }

        let (state, detail) = if self.endpoint.is_some() {
            (BrowserState::Running, None)
        } else if let Some(exit) = &self.last_exit {
            (BrowserState::Exited, Some(exit.clone()))
        } else {
            (BrowserState::Stopped, None)
        };

        let target_id = self
            .active_target_name
            .as_ref()
            .and_then(|name| self.targets.get(name))
            .map(|target| target.target_id.clone());
        Ok(BrowserStatus {
            state,
            profile_dir: self.profile_dir.clone(),
            devtools_port: self.endpoint.as_ref().map(|endpoint| endpoint.port),
            process_id: self.child.as_ref().and_then(Child::id),
            target_id,
            active_target: self.active_target_name.clone(),
            detail,
        })
    }

    /// Refresh process state and adopt a live DevTools endpoint left by
    /// another GitTerm V4 instance using the shared persistent profile.
    pub async fn status(&mut self) -> Result<BrowserStatus> {
        let _ = self.status_snapshot()?;
        if self.child.is_none() {
            let active_port_path = self.profile_dir.join(DEVTOOLS_ACTIVE_PORT_FILE);
            match read_devtools_active_port(&active_port_path)? {
                Some(endpoint) if list_page_targets(endpoint.port).await.is_ok() => {
                    self.endpoint = Some(endpoint);
                    self.last_exit = None;
                }
                _ => {
                    self.endpoint = None;
                    self.clear_targets();
                }
            }
        }
        self.status_snapshot()
    }

    pub async fn navigate(&mut self, raw_url: &str) -> Result<NavigationResult> {
        self.navigate_in(None, raw_url).await
    }

    pub async fn navigate_in(
        &mut self,
        target: Option<&str>,
        raw_url: &str,
    ) -> Result<NavigationResult> {
        let url = validate_navigation_url(raw_url)?;
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
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
        self.snapshot_in(None, BrowserCaptureMode::Viewport).await
    }

    pub async fn snapshot_in(
        &mut self,
        target: Option<&str>,
        capture_mode: BrowserCaptureMode,
    ) -> Result<BrowserSnapshot> {
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;

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

        let screenshot_params = match capture_mode {
            BrowserCaptureMode::Viewport => {
                json!({ "format": "png", "captureBeyondViewport": false })
            }
            BrowserCaptureMode::FullPage => {
                full_page_capture_params(session, state.viewport.device_scale_factor).await?
            }
        };
        let screenshot = session
            .command("Page.captureScreenshot", screenshot_params)
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
        let (console_errors, failed_requests) = session.diagnostics().await;
        let viewport = self
            .targets
            .get(&target_name)
            .and_then(|target| target.viewport)
            .unwrap_or(state.viewport);

        Ok(BrowserSnapshot {
            target: target_name,
            capture_mode,
            url: state.url,
            title: state.title,
            loading_state: state.loading_state,
            visible_text: state.visible_text,
            interactive_elements: state.interactive_elements,
            viewport,
            console_errors,
            failed_requests,
            screenshot_png,
        })
    }

    /// Click one strictly located visible element using CDP mouse events.
    pub async fn click(&mut self, locator: &BrowserLocator) -> Result<BrowserActionTarget> {
        self.click_in(None, locator).await
    }

    pub async fn click_in(
        &mut self,
        target: Option<&str>,
        locator: &BrowserLocator,
    ) -> Result<BrowserActionTarget> {
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
        let target = resolve_locator(session, locator, LocatorPreparation::Point).await?;
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
        self.type_text_in(None, locator, text).await
    }

    pub async fn type_text_in(
        &mut self,
        target: Option<&str>,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionTarget> {
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
        let target = resolve_locator(session, locator, LocatorPreparation::ReplaceText).await?;
        session
            .command("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(target)
    }

    /// Press one supported non-text key against the currently focused element.
    pub async fn press(&mut self, key: BrowserKey) -> Result<()> {
        self.press_in(None, key).await
    }

    pub async fn press_in(&mut self, target: Option<&str>, key: BrowserKey) -> Result<()> {
        let descriptor = key_descriptor(key);
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
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
        self.reload_in(None, ignore_cache).await
    }

    pub async fn reload_in(&mut self, target: Option<&str>, ignore_cache: bool) -> Result<()> {
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
        let previous_time_origin = page_time_origin(session).await?;
        session
            .command("Page.reload", json!({ "ignoreCache": ignore_cache }))
            .await?;
        wait_for_document_ready(
            session,
            Some(previous_time_origin),
            DEFAULT_PAGE_WAIT_TIMEOUT,
        )
        .await
    }

    /// Apply responsive device metrics to the controlled page target.
    pub async fn resize(&mut self, viewport: BrowserViewport) -> Result<BrowserViewport> {
        self.resize_in(None, viewport).await
    }

    pub async fn resize_in(
        &mut self,
        target: Option<&str>,
        viewport: BrowserViewport,
    ) -> Result<BrowserViewport> {
        let viewport = viewport.validate()?;
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
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
        self.targets
            .get_mut(&target_name)
            .ok_or_else(|| browser_target_not_found(&target_name))?
            .viewport = Some(viewport);
        Ok(viewport)
    }

    /// Scroll the page at the current viewport center using a CDP wheel event.
    pub async fn scroll(&mut self, delta_x: f64, delta_y: f64) -> Result<()> {
        self.scroll_in(None, delta_x, delta_y).await
    }

    pub async fn scroll_in(
        &mut self,
        target: Option<&str>,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<()> {
        const MAX_SCROLL_DELTA: f64 = 100_000.0;
        if !delta_x.is_finite()
            || !delta_y.is_finite()
            || delta_x.abs() > MAX_SCROLL_DELTA
            || delta_y.abs() > MAX_SCROLL_DELTA
        {
            return Err(BrowserControlError::new(format!(
                "browser scroll deltas must be finite and between -{MAX_SCROLL_DELTA} and {MAX_SCROLL_DELTA}"
            )));
        }
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
        let result = session
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": "({ x: window.innerWidth / 2, y: window.innerHeight / 2 })",
                    "returnByValue": true
                }),
            )
            .await?;
        let center: ViewportCenter = serde_json::from_value(
            result
                .pointer("/result/value")
                .cloned()
                .ok_or_else(|| BrowserControlError::new("Chrome omitted viewport center data"))?,
        )
        .map_err(|error| {
            BrowserControlError::new(format!(
                "Chrome returned malformed viewport center data: {error}"
            ))
        })?;
        session
            .command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": center.x,
                    "y": center.y,
                    "deltaX": delta_x,
                    "deltaY": delta_y
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn diagnostics(&mut self) -> Result<BrowserDiagnostics> {
        self.diagnostics_in(None).await
    }

    pub async fn diagnostics_in(&mut self, target: Option<&str>) -> Result<BrowserDiagnostics> {
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
        let (console_errors, failed_requests) = session.diagnostics().await;
        Ok(BrowserDiagnostics {
            console_errors,
            failed_requests,
        })
    }

    pub async fn dom_outline(&mut self) -> Result<BrowserDomOutline> {
        self.dom_outline_in(None).await
    }

    pub async fn dom_outline_in(&mut self, target: Option<&str>) -> Result<BrowserDomOutline> {
        let target_name = self.target_name(target)?;
        let expression = dom_outline_expression(&target_name)?;
        let session = self.page_session_named(&target_name).await?;
        let mut outline: BrowserDomOutline =
            evaluate_by_value(session, &expression, "DOM outline").await?;
        outline.url =
            sanitize_diagnostic_url(&outline.url).unwrap_or_else(|| "URL unavailable".to_string());
        Ok(outline)
    }

    pub async fn node_details(
        &mut self,
        selector: &BrowserNodeSelector,
    ) -> Result<BrowserNodeDetails> {
        self.node_details_in(None, selector).await
    }

    pub async fn node_details_in(
        &mut self,
        target: Option<&str>,
        selector: &BrowserNodeSelector,
    ) -> Result<BrowserNodeDetails> {
        selector.validate()?;
        let target_name = self.target_name(target)?;
        let expression = node_details_expression(&target_name, selector)?;
        let session = self.page_session_named(&target_name).await?;
        let evaluation: NodeDetailsEvaluation =
            evaluate_by_value(session, &expression, "DOM node details").await?;
        if let Some(error) = evaluation.error {
            return Err(BrowserControlError::new(error));
        }
        evaluation.details.ok_or_else(|| {
            BrowserControlError::new(format!(
                "browser node selector matched {} visible elements; expected exactly one",
                evaluation.match_count
            ))
        })
    }

    /// Wait for page state with a bounded, explicit timeout.
    pub async fn wait_for(
        &mut self,
        condition: &BrowserWaitCondition,
        max_wait: Duration,
    ) -> Result<()> {
        self.wait_for_in(None, condition, max_wait).await
    }

    pub async fn wait_for_in(
        &mut self,
        target: Option<&str>,
        condition: &BrowserWaitCondition,
        max_wait: Duration,
    ) -> Result<()> {
        if max_wait.is_zero() {
            return Err(BrowserControlError::new(
                "browser wait timeout must be greater than zero",
            ));
        }
        validate_wait_condition(condition)?;
        let target_name = self.target_name(target)?;
        let session = self.page_session_named(&target_name).await?;
        wait_for_condition(session, condition, max_wait).await
    }

    /// Wait until the current document reports a complete loading state.
    pub async fn wait_for_ready(&mut self, max_wait: Duration) -> Result<()> {
        self.wait_for_ready_in(None, max_wait).await
    }

    pub async fn wait_for_ready_in(
        &mut self,
        target: Option<&str>,
        max_wait: Duration,
    ) -> Result<()> {
        self.wait_for_in(
            target,
            &BrowserWaitCondition::LoadingState {
                state: BrowserLoadingState::Complete,
            },
            max_wait,
        )
        .await
    }

    /// Bring the managed page target to the front of its Chrome window.
    pub async fn focus(&mut self) -> Result<()> {
        let target = self.target_name(None)?;
        self.focus_target(&target).await
    }

    pub async fn open_target(&mut self, name: &str, raw_url: &str) -> Result<BrowserTargetInfo> {
        let name = validate_browser_target_name(name)?.to_string();
        let url = validate_navigation_url(raw_url)?;
        if self.targets.contains_key(&name) {
            return Err(BrowserControlError::new(format!(
                "browser target '{name}' already exists"
            )));
        }
        let session = self.browser_session().await?;
        let result = session
            .command("Target.createTarget", json!({ "url": url.as_str() }))
            .await?;
        let target_id = result
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserControlError::new("Target.createTarget omitted its targetId"))?
            .to_string();
        self.targets.insert(
            name.clone(),
            ManagedBrowserTarget {
                target_id: target_id.clone(),
                viewport: None,
                session: None,
            },
        );
        self.active_target_name = Some(name.clone());
        let deadline = Instant::now() + TARGET_STARTUP_TIMEOUT;
        loop {
            match self.focus_target(&name).await {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(25)).await,
                Err(error) => {
                    let cleanup = match session
                        .command("Target.closeTarget", json!({ "targetId": target_id }))
                        .await
                    {
                        Ok(result)
                            if result.get("success").and_then(Value::as_bool) == Some(true) =>
                        {
                            "the incomplete target was closed".to_string()
                        }
                        Ok(_) => "Chrome did not close the incomplete target".to_string(),
                        Err(cleanup_error) => {
                            format!("closing the incomplete target also failed: {cleanup_error}")
                        }
                    };
                    self.targets.remove(&name);
                    self.active_target_name = self.targets.keys().min().cloned();
                    return Err(BrowserControlError::new(format!(
                        "created browser target '{name}' but could not initialize it within {} seconds: {error}; {cleanup}",
                        TARGET_STARTUP_TIMEOUT.as_secs()
                    )));
                }
            }
        }
        Ok(BrowserTargetInfo {
            name,
            target_id,
            url: url.into(),
            title: String::new(),
            active: true,
        })
    }

    pub async fn targets(&mut self) -> Result<Vec<BrowserTargetInfo>> {
        let endpoint = self.running_endpoint()?;
        let mut live_targets = list_page_targets(endpoint.port).await?;
        self.targets.retain(|_, managed| {
            live_targets
                .iter()
                .any(|target| target.id == managed.target_id && target.kind == "page")
        });
        if self.targets.is_empty() {
            self.active_target_name = None;
            self.page_session_named(DEFAULT_BROWSER_TARGET_NAME).await?;
            live_targets = list_page_targets(endpoint.port).await?;
        } else if self
            .active_target_name
            .as_ref()
            .is_some_and(|name| !self.targets.contains_key(name))
        {
            self.active_target_name = self.targets.keys().min().cloned();
        }
        let active = self.active_target_name.as_deref();
        let mut result = self
            .targets
            .iter()
            .filter_map(|(name, managed)| {
                live_targets
                    .iter()
                    .find(|target| target.id == managed.target_id && target.kind == "page")
                    .map(|target| BrowserTargetInfo {
                        name: name.clone(),
                        target_id: target.id.clone(),
                        url: target.url.clone(),
                        title: target.title.clone(),
                        active: active == Some(name.as_str()),
                    })
            })
            .collect::<Vec<_>>();
        result.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(result)
    }

    pub async fn focus_target(&mut self, name: &str) -> Result<()> {
        let name = validate_browser_target_name(name)?.to_string();
        self.page_session_named(&name)
            .await?
            .command("Page.bringToFront", json!({}))
            .await?;
        self.active_target_name = Some(name);
        Ok(())
    }

    pub async fn close_target(&mut self, name: &str) -> Result<()> {
        let name = validate_browser_target_name(name)?.to_string();
        let target_id = self
            .targets
            .get(&name)
            .ok_or_else(|| browser_target_not_found(&name))?
            .target_id
            .clone();
        let session = self.browser_session().await?;
        let result = session
            .command("Target.closeTarget", json!({ "targetId": target_id }))
            .await?;
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(BrowserControlError::new(format!(
                "Chrome did not close browser target '{name}'"
            )));
        }
        self.targets.remove(&name);
        if self.active_target_name.as_deref() == Some(name.as_str()) {
            self.active_target_name = self.targets.keys().min().cloned();
        }
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.start_kill().map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to terminate Chrome using V4 profile {}: {error}",
                    self.profile_dir.display()
                ))
            })?;
            timeout(BROWSER_SHUTDOWN_TIMEOUT, child.wait())
                .await
                .map_err(|_| {
                    BrowserControlError::new(format!(
                        "Chrome using V4 profile {} did not exit within {} seconds",
                        self.profile_dir.display(),
                        BROWSER_SHUTDOWN_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|error| {
                    BrowserControlError::new(format!(
                        "failed while waiting for Chrome using V4 profile {} to exit: {error}",
                        self.profile_dir.display()
                    ))
                })?;
        } else if let Some(endpoint) = self.endpoint.clone() {
            let websocket_url = format!(
                "ws://127.0.0.1:{}{}",
                endpoint.port, endpoint.browser_websocket_path
            );
            let session = CdpSession::connect(&websocket_url).await.map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to connect to the existing V4 browser before disconnecting it: {error}"
                ))
            })?;
            if let Err(error) = session.command("Browser.close", json!({})).await {
                if list_page_targets(endpoint.port).await.is_ok() {
                    return Err(BrowserControlError::new(format!(
                        "failed to close the existing V4 browser on port {}: {error}",
                        endpoint.port
                    )));
                }
            }
            let deadline = Instant::now() + BROWSER_SHUTDOWN_TIMEOUT;
            while list_page_targets(endpoint.port).await.is_ok() {
                if Instant::now() >= deadline {
                    return Err(BrowserControlError::new(format!(
                        "existing V4 browser on port {} did not close within {} seconds",
                        endpoint.port,
                        BROWSER_SHUTDOWN_TIMEOUT.as_secs()
                    )));
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
        let _ = self.finish_stderr_capture().await;
        self.endpoint = None;
        self.clear_targets();
        self.last_exit = None;
        Ok(())
    }

    fn running_endpoint(&mut self) -> Result<DevToolsEndpoint> {
        let status = self.status_snapshot()?;
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

    fn target_name(&self, target: Option<&str>) -> Result<String> {
        match target {
            Some(name) => Ok(validate_browser_target_name(name)?.to_string()),
            None => Ok(self
                .active_target_name
                .clone()
                .unwrap_or_else(|| DEFAULT_BROWSER_TARGET_NAME.to_string())),
        }
    }

    async fn browser_session(&mut self) -> Result<CdpSession> {
        let endpoint = self.running_endpoint()?;
        let websocket_url = format!(
            "ws://127.0.0.1:{}{}",
            endpoint.port, endpoint.browser_websocket_path
        );
        CdpSession::connect(&websocket_url).await
    }

    async fn page_session_named(&mut self, name: &str) -> Result<&CdpSession> {
        let name = validate_browser_target_name(name)?.to_string();
        let endpoint = self.running_endpoint()?;
        let live_targets = list_page_targets(endpoint.port).await?;
        let mapped_target_id = self
            .targets
            .get(&name)
            .map(|target| target.target_id.clone());
        let target = mapped_target_id
            .as_deref()
            .and_then(|target_id| {
                live_targets.iter().find(|target| {
                    target.id == target_id
                        && target.kind == "page"
                        && target.web_socket_debugger_url.is_some()
                })
            })
            .or_else(|| {
                (name == DEFAULT_BROWSER_TARGET_NAME && mapped_target_id.is_none()).then(|| {
                    live_targets.iter().find(|target| {
                        target.kind == "page"
                            && target.web_socket_debugger_url.is_some()
                            && !self
                                .targets
                                .values()
                                .any(|managed| managed.target_id == target.id)
                    })
                })?
            })
            .cloned()
            .ok_or_else(|| browser_target_not_found(&name))?;
        let target_id = target.id.clone();
        let websocket_url = target.web_socket_debugger_url.clone().ok_or_else(|| {
            BrowserControlError::new("Chrome page target did not expose a WebSocket debugger URL")
        })?;
        let managed = self
            .targets
            .entry(name.clone())
            .or_insert_with(|| ManagedBrowserTarget {
                target_id: target_id.clone(),
                viewport: None,
                session: None,
            });
        let needs_session = managed.target_id != target_id
            || managed.session.as_ref().is_none_or(CdpSession::is_finished);
        if needs_session {
            let session = CdpSession::connect(&websocket_url).await?;
            session.command("Page.enable", json!({})).await?;
            session.command("Runtime.enable", json!({})).await?;
            session.command("Log.enable", json!({})).await?;
            session.command("Network.enable", json!({})).await?;
            managed.target_id = target_id;
            managed.viewport = None;
            managed.session = Some(session);
        }
        self.active_target_name = Some(name.clone());
        self.targets
            .get(&name)
            .and_then(|managed| managed.session.as_ref())
            .ok_or_else(|| {
                BrowserControlError::new("Chrome page target has no active DevTools session")
            })
    }

    fn clear_targets(&mut self) {
        self.active_target_name = None;
        self.targets.clear();
    }

    async fn terminate_child(&mut self) -> String {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.endpoint = None;
        self.clear_targets();
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
        if let Some(mut task) = self.stderr_task.take() {
            if timeout(STDERR_CAPTURE_SHUTDOWN_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
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
#[derive(Default)]
struct BrowserTelemetryState {
    next_activity_id: u64,
    next_evidence_id: u64,
    snapshot: BrowserTelemetrySnapshot,
}

#[derive(Clone)]
pub struct BrowserControlService {
    id: u64,
    controller: Arc<Mutex<BrowserController>>,
    telemetry: Arc<Mutex<BrowserTelemetryState>>,
    telemetry_revision: watch::Sender<u64>,
}

impl BrowserControlService {
    pub fn new(v4_global_config_dir: impl AsRef<Path>) -> Self {
        let (telemetry_revision, _) = watch::channel(0);
        Self {
            id: NEXT_BROWSER_CONTROL_SERVICE_ID.fetch_add(1, Ordering::Relaxed),
            controller: Arc::new(Mutex::new(BrowserController::new(v4_global_config_dir))),
            telemetry: Arc::new(Mutex::new(BrowserTelemetryState::default())),
            telemetry_revision,
        }
    }

    /// Subscribe to telemetry revisions. Receivers wake only when browser
    /// activity or retained evidence changes, avoiding UI polling while idle.
    pub fn telemetry_changes(&self) -> watch::Receiver<u64> {
        self.telemetry_revision.subscribe()
    }

    /// Stable identity for consumers that retain one telemetry subscription
    /// per service instance.
    pub fn telemetry_subscription_id(&self) -> u64 {
        self.id
    }

    pub async fn begin_agent_operation(&self, operation: BrowserOperation) -> u64 {
        let mut telemetry = self.telemetry.lock().await;
        telemetry.next_activity_id += 1;
        let id = telemetry.next_activity_id;
        telemetry.snapshot.revision += 1;
        telemetry.snapshot.active_agent_operation = Some(BrowserOperationActivity {
            id,
            operation,
            started_at: SystemTime::now(),
        });
        let revision = telemetry.snapshot.revision;
        drop(telemetry);
        self.telemetry_revision.send_replace(revision);
        id
    }

    pub async fn finish_agent_operation(&self, id: u64, error: Option<String>) {
        let mut telemetry = self.telemetry.lock().await;
        let Some(active) = telemetry.snapshot.active_agent_operation.as_ref() else {
            return;
        };
        if active.id != id {
            return;
        }
        let operation = active.operation;
        telemetry.snapshot.revision += 1;
        telemetry.snapshot.active_agent_operation = None;
        telemetry.snapshot.last_agent_operation = Some(BrowserOperationOutcome {
            operation,
            finished_at: SystemTime::now(),
            error,
        });
        let revision = telemetry.snapshot.revision;
        drop(telemetry);
        self.telemetry_revision.send_replace(revision);
    }

    pub async fn telemetry(&self) -> BrowserTelemetrySnapshot {
        self.telemetry.lock().await.snapshot.clone()
    }

    pub async fn profile_dir(&self) -> PathBuf {
        self.controller.lock().await.profile_dir().to_path_buf()
    }

    pub async fn launch(&self, options: BrowserLaunchOptions) -> Result<BrowserStatus> {
        self.controller.lock().await.launch(options).await
    }

    pub async fn status(&self) -> Result<BrowserStatus> {
        self.controller.lock().await.status().await
    }

    pub async fn navigate(&self, url: &str) -> Result<NavigationResult> {
        self.controller.lock().await.navigate(url).await
    }

    pub async fn navigate_in(&self, target: Option<&str>, url: &str) -> Result<NavigationResult> {
        self.controller.lock().await.navigate_in(target, url).await
    }

    pub async fn snapshot(&self) -> Result<BrowserSnapshot> {
        self.capture(None, "Snapshot", BrowserCaptureMode::Viewport)
            .await
    }

    pub async fn capture(
        &self,
        target: Option<&str>,
        label: &str,
        capture_mode: BrowserCaptureMode,
    ) -> Result<BrowserSnapshot> {
        let label = validate_evidence_label(label)?.to_string();
        let snapshot = self
            .controller
            .lock()
            .await
            .snapshot_in(target, capture_mode)
            .await?;
        let mut telemetry = self.telemetry.lock().await;
        telemetry.next_evidence_id += 1;
        let id = telemetry.next_evidence_id;
        telemetry.snapshot.revision += 1;
        let evidence = BrowserEvidence {
            id,
            target: snapshot.target.clone(),
            label,
            capture_mode: snapshot.capture_mode,
            title: snapshot.title.clone(),
            display_url: sanitize_diagnostic_url(&snapshot.url),
            viewport: snapshot.viewport,
            captured_at: SystemTime::now(),
            console_error_count: snapshot.console_errors.len(),
            failed_request_count: snapshot.failed_requests.len(),
            screenshot_png: Arc::new(snapshot.screenshot_png.clone()),
        };
        retain_browser_evidence(&mut telemetry.snapshot, evidence);
        let revision = telemetry.snapshot.revision;
        drop(telemetry);
        self.telemetry_revision.send_replace(revision);
        Ok(snapshot)
    }

    pub async fn targets(&self) -> Result<Vec<BrowserTargetInfo>> {
        self.controller.lock().await.targets().await
    }

    pub async fn open_target(&self, name: &str, url: &str) -> Result<BrowserTargetInfo> {
        self.controller.lock().await.open_target(name, url).await
    }

    pub async fn focus_target(&self, name: &str) -> Result<()> {
        self.controller.lock().await.focus_target(name).await
    }

    pub async fn close_target(&self, name: &str) -> Result<()> {
        self.controller.lock().await.close_target(name).await
    }

    pub async fn click(&self, locator: &BrowserLocator) -> Result<BrowserActionTarget> {
        self.controller.lock().await.click(locator).await
    }

    pub async fn click_in(
        &self,
        target: Option<&str>,
        locator: &BrowserLocator,
    ) -> Result<BrowserActionTarget> {
        self.controller.lock().await.click_in(target, locator).await
    }

    pub async fn type_text(
        &self,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionTarget> {
        self.controller.lock().await.type_text(locator, text).await
    }

    pub async fn type_text_in(
        &self,
        target: Option<&str>,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionTarget> {
        self.controller
            .lock()
            .await
            .type_text_in(target, locator, text)
            .await
    }

    pub async fn press(&self, key: BrowserKey) -> Result<()> {
        self.controller.lock().await.press(key).await
    }

    pub async fn press_in(&self, target: Option<&str>, key: BrowserKey) -> Result<()> {
        self.controller.lock().await.press_in(target, key).await
    }

    pub async fn reload(&self, ignore_cache: bool) -> Result<()> {
        self.controller.lock().await.reload(ignore_cache).await
    }

    pub async fn reload_in(&self, target: Option<&str>, ignore_cache: bool) -> Result<()> {
        self.controller
            .lock()
            .await
            .reload_in(target, ignore_cache)
            .await
    }

    pub async fn resize(&self, viewport: BrowserViewport) -> Result<BrowserViewport> {
        self.controller.lock().await.resize(viewport).await
    }

    pub async fn resize_in(
        &self,
        target: Option<&str>,
        viewport: BrowserViewport,
    ) -> Result<BrowserViewport> {
        self.controller
            .lock()
            .await
            .resize_in(target, viewport)
            .await
    }

    pub async fn scroll(&self, delta_x: f64, delta_y: f64) -> Result<()> {
        self.controller.lock().await.scroll(delta_x, delta_y).await
    }

    pub async fn scroll_in(&self, target: Option<&str>, delta_x: f64, delta_y: f64) -> Result<()> {
        self.controller
            .lock()
            .await
            .scroll_in(target, delta_x, delta_y)
            .await
    }

    pub async fn diagnostics(&self) -> Result<BrowserDiagnostics> {
        self.controller.lock().await.diagnostics().await
    }

    pub async fn diagnostics_in(&self, target: Option<&str>) -> Result<BrowserDiagnostics> {
        self.controller.lock().await.diagnostics_in(target).await
    }

    pub async fn dom_outline(&self) -> Result<BrowserDomOutline> {
        self.controller.lock().await.dom_outline().await
    }

    pub async fn dom_outline_in(&self, target: Option<&str>) -> Result<BrowserDomOutline> {
        self.controller.lock().await.dom_outline_in(target).await
    }

    pub async fn node_details(&self, selector: &BrowserNodeSelector) -> Result<BrowserNodeDetails> {
        self.controller.lock().await.node_details(selector).await
    }

    pub async fn node_details_in(
        &self,
        target: Option<&str>,
        selector: &BrowserNodeSelector,
    ) -> Result<BrowserNodeDetails> {
        self.controller
            .lock()
            .await
            .node_details_in(target, selector)
            .await
    }

    pub async fn wait_for_ready(&self, max_wait: Duration) -> Result<()> {
        self.controller.lock().await.wait_for_ready(max_wait).await
    }

    pub async fn wait_for_ready_in(&self, target: Option<&str>, max_wait: Duration) -> Result<()> {
        self.controller
            .lock()
            .await
            .wait_for_ready_in(target, max_wait)
            .await
    }

    pub async fn wait_for(
        &self,
        condition: &BrowserWaitCondition,
        max_wait: Duration,
    ) -> Result<()> {
        self.controller
            .lock()
            .await
            .wait_for(condition, max_wait)
            .await
    }

    pub async fn wait_for_in(
        &self,
        target: Option<&str>,
        condition: &BrowserWaitCondition,
        max_wait: Duration,
    ) -> Result<()> {
        self.controller
            .lock()
            .await
            .wait_for_in(target, condition, max_wait)
            .await
    }

    pub async fn focus(&self) -> Result<()> {
        self.controller.lock().await.focus().await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.controller.lock().await.disconnect().await
    }
}

fn retain_browser_evidence(snapshot: &mut BrowserTelemetrySnapshot, evidence: BrowserEvidence) {
    if let Some(index) = snapshot.evidence.iter().position(|retained| {
        retained.target == evidence.target
            && retained.label == evidence.label
            && retained.capture_mode == evidence.capture_mode
            && retained.viewport == evidence.viewport
    }) {
        snapshot.evidence[index] = evidence.clone();
    } else {
        snapshot.evidence.push(evidence.clone());
    }
    if snapshot.evidence.len() > MAX_RETAINED_EVIDENCE {
        if let Some((oldest, _)) = snapshot
            .evidence
            .iter()
            .enumerate()
            .min_by_key(|(_, retained)| retained.id)
        {
            snapshot.evidence.remove(oldest);
        }
    }
    snapshot.latest_evidence = Some(evidence);
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeDetailsEvaluation {
    error: Option<String>,
    match_count: usize,
    details: Option<BrowserNodeDetails>,
}

#[derive(Debug, Deserialize)]
struct ViewportCenter {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy)]
enum LocatorPreparation {
    ResolveOnly,
    Point,
    ReplaceText,
}

impl LocatorPreparation {
    fn as_str(self) -> &'static str {
        match self {
            Self::ResolveOnly => "resolve_only",
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
    session: &CdpSession,
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

async fn page_time_origin(session: &CdpSession) -> Result<f64> {
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
    session: &CdpSession,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitEvaluation {
    matched: bool,
    detail: String,
}

fn validate_wait_condition(condition: &BrowserWaitCondition) -> Result<()> {
    match condition {
        BrowserWaitCondition::Locator { locator } => locator.validate(),
        BrowserWaitCondition::Text { text, .. } => BrowserLocator::Text {
            text: text.clone(),
            exact: false,
        }
        .validate(),
        BrowserWaitCondition::Url { url, .. } if url.trim().is_empty() => Err(
            BrowserControlError::new("browser URL wait value must not be empty"),
        ),
        _ => Ok(()),
    }
}

async fn wait_for_condition(
    session: &CdpSession,
    condition: &BrowserWaitCondition,
    max_wait: Duration,
) -> Result<()> {
    let deadline = Instant::now() + max_wait;
    loop {
        let detail = match evaluate_wait_condition(session, condition).await {
            Ok(evaluation) if evaluation.matched => return Ok(()),
            Ok(evaluation) => evaluation.detail,
            Err(error) => format!("last CDP error: {error}"),
        };
        if Instant::now() >= deadline {
            return Err(BrowserControlError::new(format!(
                "browser wait condition was not met within {} ms ({detail})",
                max_wait.as_millis()
            )));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn evaluate_wait_condition(
    session: &CdpSession,
    condition: &BrowserWaitCondition,
) -> Result<WaitEvaluation> {
    if let BrowserWaitCondition::Locator { locator } = condition {
        return evaluate_locator_wait(session, locator).await;
    }
    if let BrowserWaitCondition::Text { text, exact } = condition {
        return evaluate_locator_wait(
            session,
            &BrowserLocator::Text {
                text: text.clone(),
                exact: *exact,
            },
        )
        .await;
    }

    let expression = match condition {
        BrowserWaitCondition::Url { url, exact } => {
            let expected = serde_json::to_string(url).map_err(|error| {
                BrowserControlError::new(format!("failed to serialize browser wait URL: {error}"))
            })?;
            format!(
                "(() => {{ const actual = window.location.href; const expected = {expected}; return {{ matched: {} ? actual === expected : actual.includes(expected), detail: actual }}; }})()",
                exact
            )
        }
        BrowserWaitCondition::LoadingState { state } => {
            let expected = serde_json::to_string(state.as_str()).map_err(|error| {
                BrowserControlError::new(format!(
                    "failed to serialize browser loading state: {error}"
                ))
            })?;
            format!(
                "(() => {{ const actual = document.readyState; return {{ matched: actual === {expected}, detail: `last loading state: ${{actual}}` }}; }})()"
            )
        }
        BrowserWaitCondition::Locator { .. } | BrowserWaitCondition::Text { .. } => {
            unreachable!("locator waits are handled before expression generation")
        }
    };
    let result = session
        .command(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true
            }),
        )
        .await?;
    let value = result
        .pointer("/result/value")
        .cloned()
        .ok_or_else(|| BrowserControlError::new("Chrome omitted browser wait condition data"))?;
    let mut evaluation: WaitEvaluation = serde_json::from_value(value).map_err(|error| {
        BrowserControlError::new(format!(
            "Chrome returned malformed browser wait condition data: {error}"
        ))
    })?;
    if matches!(condition, BrowserWaitCondition::Url { .. }) {
        evaluation.detail = sanitize_diagnostic_url(&evaluation.detail)
            .map(|url| format!("last URL: {url}"))
            .unwrap_or_else(|| "last URL was not an HTTP(S) page".to_string());
    }
    Ok(evaluation)
}

async fn evaluate_locator_wait(
    session: &CdpSession,
    locator: &BrowserLocator,
) -> Result<WaitEvaluation> {
    let expression = locator_expression(locator, LocatorPreparation::ResolveOnly)?;
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
        .ok_or_else(|| BrowserControlError::new("Chrome omitted browser locator wait data"))?;
    let resolution: LocatorResolution = serde_json::from_value(value).map_err(|error| {
        BrowserControlError::new(format!(
            "Chrome returned malformed browser locator wait data: {error}"
        ))
    })?;
    if let Some(error) = resolution.error {
        return Err(BrowserControlError::new(format!(
            "browser locator wait could not be evaluated: {error}"
        )));
    }
    Ok(WaitEvaluation {
        matched: resolution.match_count == 1,
        detail: format!(
            "last locator match count: {} (expected exactly 1)",
            resolution.match_count
        ),
    })
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

struct CdpRequest {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
}

struct PendingCdpCommand {
    method: String,
    reply: oneshot::Sender<Result<Value>>,
}

#[derive(Debug, Clone)]
struct TrackedRequest {
    url: String,
    method: String,
}

#[derive(Debug, Default)]
struct BrowserDiagnosticState {
    console_errors: VecDeque<BrowserConsoleError>,
    failed_requests: VecDeque<BrowserNetworkFailure>,
    requests: HashMap<String, TrackedRequest>,
    request_order: VecDeque<String>,
}

impl BrowserDiagnosticState {
    fn push_console_error(&mut self, error: BrowserConsoleError) {
        push_bounded(&mut self.console_errors, error, MAX_CONSOLE_ERRORS);
    }

    fn push_network_failure(&mut self, failure: BrowserNetworkFailure) {
        push_bounded(&mut self.failed_requests, failure, MAX_NETWORK_FAILURES);
    }

    fn track_request(&mut self, request_id: String, request: TrackedRequest) {
        if !self.requests.contains_key(&request_id) {
            self.request_order.push_back(request_id.clone());
        }
        self.requests.insert(request_id, request);
        while self.request_order.len() > MAX_TRACKED_REQUESTS {
            if let Some(expired) = self.request_order.pop_front() {
                self.requests.remove(&expired);
            }
        }
    }

    fn finish_request(&mut self, request_id: &str) -> Option<TrackedRequest> {
        self.requests.remove(request_id)
    }
}

struct CdpSession {
    requests: mpsc::UnboundedSender<CdpRequest>,
    diagnostics: Arc<Mutex<BrowserDiagnosticState>>,
    worker: JoinHandle<()>,
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
        let (requests, receiver) = mpsc::unbounded_channel();
        let diagnostics = Arc::new(Mutex::new(BrowserDiagnosticState::default()));
        let worker_diagnostics = Arc::clone(&diagnostics);
        let worker = tokio::spawn(async move {
            run_cdp_session(socket, receiver, worker_diagnostics).await;
        });
        Ok(Self {
            requests,
            diagnostics,
            worker,
        })
    }

    fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(CdpRequest {
                method: method.to_string(),
                params,
                reply,
            })
            .map_err(|_| {
                BrowserControlError::new(format!(
                    "Chrome DevTools connection closed before CDP command {method}"
                ))
            })?;
        timeout(CDP_COMMAND_TIMEOUT, response)
            .await
            .map_err(|_| BrowserControlError::new(format!("CDP command {method} timed out")))?
            .map_err(|_| {
                BrowserControlError::new(format!(
                    "Chrome DevTools connection closed during CDP command {method}"
                ))
            })?
    }

    async fn diagnostics(&self) -> (Vec<BrowserConsoleError>, Vec<BrowserNetworkFailure>) {
        let diagnostics = self.diagnostics.lock().await;
        (
            diagnostics.console_errors.iter().cloned().collect(),
            diagnostics.failed_requests.iter().cloned().collect(),
        )
    }
}

impl Drop for CdpSession {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn run_cdp_session<S>(
    mut socket: WebSocketStream<S>,
    mut requests: mpsc::UnboundedReceiver<CdpRequest>,
    diagnostics: Arc<Mutex<BrowserDiagnosticState>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut next_id = 1_u64;
    let mut pending = HashMap::<u64, PendingCdpCommand>::new();
    let terminal_error = loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else {
                    break "Chrome DevTools command channel closed".to_string();
                };
                let id = next_id;
                next_id += 1;
                let method = request.method.clone();
                let message = json!({ "id": id, "method": method, "params": request.params });
                if let Err(error) = socket.send(Message::Text(message.to_string())).await {
                    let detail = format!("failed to send CDP command {}: {error}", request.method);
                    let _ = request.reply.send(Err(BrowserControlError::new(detail.clone())));
                    break detail;
                }
                pending.insert(id, PendingCdpCommand { method: request.method, reply: request.reply });
            }
            message = socket.next() => {
                let Some(message) = message else {
                    break "Chrome closed the DevTools connection".to_string();
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => break format!("failed while reading Chrome DevTools: {error}"),
                };
                match message {
                    Message::Text(text) => {
                        let value: Value = match serde_json::from_str(&text) {
                            Ok(value) => value,
                            Err(error) => break format!("Chrome returned malformed CDP JSON: {error}"),
                        };
                        if let Some(id) = value.get("id").and_then(Value::as_u64) {
                            if let Some(command) = pending.remove(&id) {
                                let result = cdp_response_result(&command.method, &value);
                                let _ = command.reply.send(result);
                            }
                        } else if value.get("method").is_some() {
                            record_cdp_event(&diagnostics, &value).await;
                        }
                    }
                    Message::Ping(payload) => {
                        if let Err(error) = socket.send(Message::Pong(payload)).await {
                            break format!("failed to reply to Chrome DevTools ping: {error}");
                        }
                    }
                    Message::Close(_) => break "Chrome closed the DevTools connection".to_string(),
                    _ => {}
                }
            }
        }
    };

    for (_, command) in pending {
        let _ = command.reply.send(Err(BrowserControlError::new(format!(
            "CDP command {} failed: {terminal_error}",
            command.method
        ))));
    }
}

fn cdp_response_result(method: &str, response: &Value) -> Result<Value> {
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

async fn record_cdp_event(diagnostics: &Mutex<BrowserDiagnosticState>, event: &Value) {
    let Some(method) = event.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = event.get("params").unwrap_or(&Value::Null);
    let mut diagnostics = diagnostics.lock().await;
    match method {
        "Runtime.consoleAPICalled" => {
            let kind = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(kind, "error" | "assert") {
                return;
            }
            let text = params
                .get("args")
                .and_then(Value::as_array)
                .map(|arguments| {
                    arguments
                        .iter()
                        .filter_map(remote_object_text)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| kind.to_string());
            let frame = params
                .pointer("/stackTrace/callFrames/0")
                .unwrap_or(&Value::Null);
            diagnostics.push_console_error(BrowserConsoleError {
                source: "console".to_string(),
                text: bounded_text(&text),
                url: frame
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(sanitize_diagnostic_url),
                line_number: frame.get("lineNumber").and_then(Value::as_u64),
            });
        }
        "Runtime.exceptionThrown" => {
            let details = params.get("exceptionDetails").unwrap_or(&Value::Null);
            let text = details
                .get("exception")
                .and_then(remote_object_text)
                .or_else(|| {
                    details
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "uncaught exception".to_string());
            diagnostics.push_console_error(BrowserConsoleError {
                source: "exception".to_string(),
                text: bounded_text(&text),
                url: details
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(sanitize_diagnostic_url),
                line_number: details.get("lineNumber").and_then(Value::as_u64),
            });
        }
        "Log.entryAdded" => {
            let entry = params.get("entry").unwrap_or(&Value::Null);
            if entry.get("level").and_then(Value::as_str) != Some("error") {
                return;
            }
            diagnostics.push_console_error(BrowserConsoleError {
                source: entry
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                    .to_string(),
                text: bounded_text(
                    entry
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("browser log error"),
                ),
                url: entry
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(sanitize_diagnostic_url),
                line_number: entry.get("lineNumber").and_then(Value::as_u64),
            });
        }
        "Network.requestWillBeSent" => {
            let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
                return;
            };
            let request = params.get("request").unwrap_or(&Value::Null);
            let Some(url) = request
                .get("url")
                .and_then(Value::as_str)
                .and_then(sanitize_diagnostic_url)
            else {
                return;
            };
            diagnostics.track_request(
                request_id.to_string(),
                TrackedRequest {
                    url,
                    method: request
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or("GET")
                        .to_string(),
                },
            );
        }
        "Network.responseReceived" => {
            let Some(status) = params
                .pointer("/response/status")
                .and_then(Value::as_f64)
                .filter(|status| *status >= 400.0 && *status <= u16::MAX as f64)
                .map(|status| status as u16)
            else {
                return;
            };
            let request_id = params.get("requestId").and_then(Value::as_str);
            let tracked = request_id.and_then(|id| diagnostics.requests.get(id).cloned());
            let url = tracked
                .as_ref()
                .map(|request| request.url.clone())
                .or_else(|| {
                    params
                        .pointer("/response/url")
                        .and_then(Value::as_str)
                        .and_then(sanitize_diagnostic_url)
                });
            if let Some(url) = url {
                diagnostics.push_network_failure(BrowserNetworkFailure {
                    url,
                    method: tracked.map(|request| request.method),
                    status: Some(status),
                    error_text: params
                        .pointer("/response/statusText")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(bounded_text),
                });
            }
        }
        "Network.loadingFailed" => {
            let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
                return;
            };
            if let Some(request) = diagnostics.finish_request(request_id) {
                diagnostics.push_network_failure(BrowserNetworkFailure {
                    url: request.url,
                    method: Some(request.method),
                    status: None,
                    error_text: params
                        .get("errorText")
                        .and_then(Value::as_str)
                        .map(bounded_text),
                });
            }
        }
        "Network.loadingFinished" => {
            if let Some(request_id) = params.get("requestId").and_then(Value::as_str) {
                diagnostics.finish_request(request_id);
            }
        }
        _ => {}
    }
}

fn remote_object_text(object: &Value) -> Option<String> {
    if let Some(value) = object.get("value") {
        return match value {
            Value::String(text) => Some(text.clone()),
            Value::Null => Some("null".to_string()),
            value => Some(value.to_string()),
        };
    }
    object
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| object.get("unserializableValue").and_then(Value::as_str))
        .map(str::to_string)
}

fn sanitize_diagnostic_url(raw_url: &str) -> Option<String> {
    let mut url = Url::parse(raw_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(
        url.to_string()
            .chars()
            .take(MAX_DIAGNOSTIC_URL_CHARS)
            .collect(),
    )
}

fn bounded_text(text: &str) -> String {
    text.chars().take(MAX_DIAGNOSTIC_TEXT_CHARS).collect()
}

fn push_bounded<T>(items: &mut VecDeque<T>, item: T, capacity: usize) {
    if items.len() == capacity {
        items.pop_front();
    }
    items.push_back(item);
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

fn ensure_browser_profile_branding(profile_dir: &Path) -> Result<()> {
    let default_profile_dir = profile_dir.join("Default");
    std::fs::create_dir_all(&default_profile_dir).map_err(|error| {
        BrowserControlError::new(format!(
            "failed to create the branded V4 Chrome profile directory {}: {error}",
            default_profile_dir.display()
        ))
    })?;
    let preferences_path = default_profile_dir.join("Preferences");
    let mut preferences = match std::fs::read(&preferences_path) {
        Ok(contents) => serde_json::from_slice::<Value>(&contents).map_err(|error| {
            BrowserControlError::new(format!(
                "failed to parse V4 Chrome preferences {} before applying browser branding: {error}",
                preferences_path.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => {
            return Err(BrowserControlError::new(format!(
                "failed to read V4 Chrome preferences {} before applying browser branding: {error}",
                preferences_path.display()
            )))
        }
    };
    let root = preferences.as_object_mut().ok_or_else(|| {
        BrowserControlError::new(format!(
            "V4 Chrome preferences {} must contain a JSON object",
            preferences_path.display()
        ))
    })?;
    object_preference(root, "profile")?.insert(
        "name".to_string(),
        Value::String(BROWSER_PROFILE_NAME.to_string()),
    );
    let browser_theme = object_preference(object_preference(root, "browser")?, "theme")?;
    browser_theme.insert(
        "user_color2".to_string(),
        Value::Number(BROWSER_PROFILE_COLOR.into()),
    );
    browser_theme.insert("color_variant2".to_string(), Value::Number(1.into()));
    object_preference(object_preference(root, "extensions")?, "theme")?.insert(
        "id".to_string(),
        Value::String("user_color_theme_id".to_string()),
    );

    let temporary_path = default_profile_dir.join("Preferences.gitterm-v4.tmp");
    write_json_atomically(
        &preferences_path,
        &temporary_path,
        &preferences,
        "branded V4 Chrome preferences",
    )?;

    let local_state_path = profile_dir.join("Local State");
    let mut local_state = match std::fs::read(&local_state_path) {
        Ok(contents) => serde_json::from_slice::<Value>(&contents).map_err(|error| {
            BrowserControlError::new(format!(
                "failed to parse V4 Chrome Local State {} before applying browser branding: {error}",
                local_state_path.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => {
            return Err(BrowserControlError::new(format!(
                "failed to read V4 Chrome Local State {} before applying browser branding: {error}",
                local_state_path.display()
            )))
        }
    };
    let root = local_state.as_object_mut().ok_or_else(|| {
        BrowserControlError::new(format!(
            "V4 Chrome Local State {} must contain a JSON object",
            local_state_path.display()
        ))
    })?;
    let default_profile = object_preference(
        object_preference(object_preference(root, "profile")?, "info_cache")?,
        "Default",
    )?;
    default_profile.insert(
        "name".to_string(),
        Value::String(BROWSER_PROFILE_NAME.to_string()),
    );
    default_profile.insert("is_using_default_name".to_string(), Value::Bool(false));
    let temporary_local_state_path = profile_dir.join("Local State.gitterm-v4.tmp");
    write_json_atomically(
        &local_state_path,
        &temporary_local_state_path,
        &local_state,
        "branded V4 Chrome Local State",
    )?;
    Ok(())
}

fn write_json_atomically(
    destination: &Path,
    temporary: &Path,
    value: &Value,
    operation: &str,
) -> Result<()> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        BrowserControlError::new(format!(
            "failed to serialize {operation} {}: {error}",
            destination.display()
        ))
    })?;
    std::fs::write(temporary, encoded).map_err(|error| {
        BrowserControlError::new(format!(
            "failed to stage {operation} {}: {error}",
            temporary.display()
        ))
    })?;
    #[cfg(target_os = "windows")]
    if destination.exists() {
        std::fs::remove_file(destination).map_err(|error| {
            BrowserControlError::new(format!(
                "failed to replace {operation} {}: {error}",
                destination.display()
            ))
        })?;
    }
    std::fs::rename(temporary, destination).map_err(|error| {
        BrowserControlError::new(format!(
            "failed to install {operation} {}: {error}",
            destination.display()
        ))
    })?;
    Ok(())
}

fn object_preference<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    parent
        .entry(key.to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            BrowserControlError::new(format!(
                "V4 Chrome preference {key} must contain a JSON object"
            ))
        })
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
    Ok(Some(DevToolsEndpoint {
        port,
        browser_websocket_path: browser_path.to_string(),
    }))
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

fn validate_browser_target_name(name: &str) -> Result<&str> {
    if name.is_empty() || name.chars().count() > MAX_BROWSER_TARGET_NAME_CHARS {
        return Err(BrowserControlError::new(format!(
            "browser target names must contain 1 to {MAX_BROWSER_TARGET_NAME_CHARS} characters"
        )));
    }
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|character| character.is_ascii_lowercase())
        || !chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
    {
        return Err(BrowserControlError::new(
            "browser target names must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_'",
        ));
    }
    Ok(name)
}

fn validate_evidence_label(label: &str) -> Result<&str> {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_EVIDENCE_LABEL_CHARS {
        return Err(BrowserControlError::new(format!(
            "browser evidence labels must contain 1 to {MAX_EVIDENCE_LABEL_CHARS} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(BrowserControlError::new(
            "browser evidence labels cannot contain control characters",
        ));
    }
    Ok(trimmed)
}

fn browser_target_not_found(name: &str) -> BrowserControlError {
    BrowserControlError::new(format!(
        "browser target '{name}' does not exist; open it first or list the available targets"
    ))
}

async fn full_page_capture_params(session: &CdpSession, device_scale_factor: f64) -> Result<Value> {
    let metrics = session.command("Page.getLayoutMetrics", json!({})).await?;
    let width = metrics
        .pointer("/contentSize/width")
        .and_then(Value::as_f64)
        .ok_or_else(|| BrowserControlError::new("Page.getLayoutMetrics omitted content width"))?;
    let height = metrics
        .pointer("/contentSize/height")
        .and_then(Value::as_f64)
        .ok_or_else(|| BrowserControlError::new("Page.getLayoutMetrics omitted content height"))?;
    if !width.is_finite()
        || !height.is_finite()
        || !device_scale_factor.is_finite()
        || width <= 0.0
        || height <= 0.0
        || device_scale_factor <= 0.0
    {
        return Err(BrowserControlError::new(format!(
            "Chrome returned invalid full-page dimensions {width}x{height} at {device_scale_factor}x scale"
        )));
    }
    let encoded_pixels = width * height * device_scale_factor * device_scale_factor;
    if width > MAX_FULL_PAGE_CAPTURE_WIDTH
        || height > MAX_FULL_PAGE_CAPTURE_HEIGHT
        || encoded_pixels > MAX_FULL_PAGE_CAPTURE_PIXELS
    {
        return Err(BrowserControlError::new(format!(
            "full-page capture {width:.0}x{height:.0} exceeds the safe evidence limit"
        )));
    }
    Ok(json!({
        "format": "png",
        "captureBeyondViewport": true,
        "clip": { "x": 0, "y": 0, "width": width, "height": height, "scale": 1 }
    }))
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
        } else if (preparation === 'point') {
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

async fn evaluate_by_value<T: DeserializeOwned>(
    session: &CdpSession,
    expression: &str,
    operation: &str,
) -> Result<T> {
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
    if let Some(exception) = result
        .pointer("/exceptionDetails/exception/description")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .pointer("/exceptionDetails/text")
                .and_then(Value::as_str)
        })
    {
        return Err(BrowserControlError::new(format!(
            "Chrome failed while evaluating {operation}: {exception}"
        )));
    }
    let value = result
        .pointer("/result/value")
        .cloned()
        .ok_or_else(|| BrowserControlError::new(format!("Chrome omitted {operation} data")))?;
    serde_json::from_value(value).map_err(|error| {
        BrowserControlError::new(format!(
            "Chrome returned malformed {operation} data: {error}"
        ))
    })
}

fn dom_outline_expression(target: &str) -> Result<String> {
    let target = serde_json::to_string(target).map_err(|error| {
        BrowserControlError::new(format!("failed to serialize browser target name: {error}"))
    })?;
    Ok(r#"(() => {
        const target = __TARGET__;
        const maxNodes = __MAX_NODES__;
        const maxScanned = __MAX_SCANNED__;
        const maxDepth = __MAX_DEPTH__;
        const maxText = __MAX_TEXT__;
        const maxField = __MAX_FIELD__;
        const maxClasses = __MAX_CLASSES__;
        const registryLimit = maxNodes * 4;
        const registryKey = '__gittermV4DomInspectorRegistry';
        const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim();
        const roleFor = (element) => {
            const explicit = element.getAttribute('role');
            if (explicit) return explicit;
            const tag = element.tagName.toLowerCase();
            if (tag === 'a' && element.hasAttribute('href')) return 'link';
            if (tag === 'button') return 'button';
            if (tag === 'textarea') return 'textbox';
            if (tag === 'select') return element.multiple ? 'listbox' : 'combobox';
            if (/^h[1-6]$/.test(tag)) return 'heading';
            if (tag === 'nav') return 'navigation';
            if (tag === 'main') return 'main';
            if (tag === 'form') return 'form';
            if (tag === 'table') return 'table';
            if (tag === 'img') return 'img';
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
            return normalize(element.getAttribute('aria-label') || labelledText || labelText ||
                element.getAttribute('alt') || element.getAttribute('title') ||
                element.getAttribute('placeholder') || '') || null;
        };
        const directText = (element) => normalize(Array.from(element.childNodes)
            .filter((node) => node.nodeType === Node.TEXT_NODE)
            .map((node) => node.textContent || '').join(' '));
        const visible = (element) => {
            const style = window.getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.visibility !== 'hidden' && style.display !== 'none' &&
                parseFloat(style.opacity || '1') > 0 && rect.width > 0 && rect.height > 0;
        };
        const semanticTags = new Set(['body', 'main', 'header', 'nav', 'aside', 'section', 'article',
            'footer', 'form', 'dialog', 'details', 'summary', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
            'button', 'a', 'input', 'select', 'textarea', 'label', 'table', 'thead', 'tbody', 'tfoot',
            'tr', 'th', 'td', 'ul', 'ol', 'li', 'img', 'svg', 'figure', 'figcaption']);
        const meaningful = (element) => semanticTags.has(element.tagName.toLowerCase()) ||
            Boolean(element.id || element.getAttribute('role') || directText(element));
        let registry = globalThis[registryKey];
        if (!registry || registry.document !== document) {
            const documentId = globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random().toString(16).slice(2)}`;
            registry = { document, documentId, nextId: 1, byNode: new WeakMap(), byRef: new Map() };
            globalThis[registryKey] = registry;
        }
        const refFor = (element) => {
            const existing = registry.byNode.get(element);
            if (existing) return existing;
            if (registry.byRef.size >= registryLimit) return null;
            const nodeRef = `${registry.documentId}:node-${registry.nextId++}`;
            registry.byNode.set(element, nodeRef);
            registry.byRef.set(nodeRef, new WeakRef(element));
            return nodeRef;
        };
        const nodes = [];
        let truncated = false;
        let scanned = 0;
        const visit = (element, parentRef, depth, treeDepth) => {
            scanned += 1;
            if (scanned > maxScanned || treeDepth > maxDepth * 4) { truncated = true; return; }
            if (nodes.length >= maxNodes) { truncated = true; return; }
            if (depth > maxDepth) { truncated = true; return; }
            if (!visible(element)) return;
            let nextParent = parentRef;
            let nextDepth = depth;
            if (meaningful(element)) {
                const nodeRef = refFor(element);
                if (!nodeRef) { truncated = true; return; }
                const rect = element.getBoundingClientRect();
                const role = roleFor(element);
                const name = nameFor(element);
                const text = directText(element) || (['button', 'a', 'label', 'summary'].includes(element.tagName.toLowerCase()) ? normalize(element.innerText) : '');
                nodes.push({
                    nodeRef,
                    parentRef,
                    depth,
                    tag: element.tagName.toLowerCase(),
                    id: element.id ? element.id.slice(0, maxField) : null,
                    classes: Array.from(element.classList || []).slice(0, maxClasses).map((value) => value.slice(0, maxField)),
                    role: role ? role.slice(0, maxField) : null,
                    name: name ? name.slice(0, maxText) : null,
                    text: text ? text.slice(0, maxText) : null,
                    bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
                    disabled: Boolean(element.disabled) || element.getAttribute('aria-disabled') === 'true',
                    expanded: element.getAttribute('aria-expanded')?.slice(0, maxField) || null,
                    selected: element.getAttribute('aria-selected')?.slice(0, maxField) || null,
                    checked: element.getAttribute('aria-checked')?.slice(0, maxField) || null
                });
                nextParent = nodeRef;
                nextDepth = depth + 1;
            }
            for (const child of element.children) {
                visit(child, nextParent, nextDepth, treeDepth + 1);
                if (nodes.length >= maxNodes) { truncated = true; break; }
            }
        };
        if (document.body) visit(document.body, null, 0, 0);
        return { target, documentId: registry.documentId, url: window.location.href.slice(0, __MAX_URL__),
            title: document.title.slice(0, maxField), nodes, truncated };
    })()"#
        .replace("__TARGET__", &target)
        .replace("__MAX_NODES__", &MAX_DOM_OUTLINE_NODES.to_string())
        .replace("__MAX_SCANNED__", &MAX_DOM_SCANNED_ELEMENTS.to_string())
        .replace("__MAX_DEPTH__", &MAX_DOM_OUTLINE_DEPTH.to_string())
        .replace("__MAX_TEXT__", &MAX_DOM_TEXT_CHARS.to_string())
        .replace("__MAX_FIELD__", &MAX_DOM_FIELD_CHARS.to_string())
        .replace("__MAX_URL__", &MAX_DIAGNOSTIC_URL_CHARS.to_string())
        .replace("__MAX_CLASSES__", &MAX_DOM_CLASSES.to_string()))
}

fn node_details_expression(target: &str, selector: &BrowserNodeSelector) -> Result<String> {
    let target = serde_json::to_string(target).map_err(|error| {
        BrowserControlError::new(format!("failed to serialize browser target name: {error}"))
    })?;
    let selector = serde_json::to_string(selector).map_err(|error| {
        BrowserControlError::new(format!(
            "failed to serialize browser node selector: {error}"
        ))
    })?;
    Ok(r#"(() => {
        const target = __TARGET__;
        const selector = __SELECTOR__;
        const maxText = __MAX_TEXT__;
        const maxField = __MAX_FIELD__;
        const maxClasses = __MAX_CLASSES__;
        const maxAttributes = __MAX_ATTRIBUTES__;
        const maxAttributeValue = __MAX_ATTRIBUTE_VALUE__;
        const maxStyleValue = __MAX_STYLE_VALUE__;
        const maxOuterHtml = __MAX_OUTER_HTML__;
        const registryKey = '__gittermV4DomInspectorRegistry';
        const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim();
        const roleFor = (element) => {
            const explicit = element.getAttribute('role');
            if (explicit) return explicit;
            const tag = element.tagName.toLowerCase();
            if (tag === 'a' && element.hasAttribute('href')) return 'link';
            if (tag === 'button') return 'button';
            if (tag === 'textarea') return 'textbox';
            if (tag === 'select') return element.multiple ? 'listbox' : 'combobox';
            if (/^h[1-6]$/.test(tag)) return 'heading';
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
            return normalize(element.getAttribute('aria-label') || labelledText || labelText ||
                element.getAttribute('alt') || element.getAttribute('title') ||
                element.getAttribute('placeholder') || element.innerText || element.value) || null;
        };
        const visible = (element) => {
            const style = window.getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.visibility !== 'hidden' && style.display !== 'none' &&
                parseFloat(style.opacity || '1') > 0 && rect.width > 0 && rect.height > 0;
        };
        const textMatches = (actual, expected, exact) => {
            const left = normalize(actual);
            const right = normalize(expected);
            return exact ? left === right : left.toLocaleLowerCase().includes(right.toLocaleLowerCase());
        };
        let registry = globalThis[registryKey];
        if (!registry || registry.document !== document) {
            const documentId = globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random().toString(16).slice(2)}`;
            registry = { document, documentId, nextId: 1, byNode: new WeakMap(), byRef: new Map() };
            globalThis[registryKey] = registry;
        }
        const refFor = (element) => {
            const existing = registry.byNode.get(element);
            if (existing) return existing;
            const nodeRef = `${registry.documentId}:node-${registry.nextId++}`;
            registry.byNode.set(element, nodeRef);
            registry.byRef.set(nodeRef, new WeakRef(element));
            return nodeRef;
        };
        let matches = [];
        try {
            if (selector.kind === 'node_ref') {
                const element = registry.byRef.get(selector.node_ref)?.deref();
                if (!element || !element.isConnected) {
                    return { error: `DOM node reference '${selector.node_ref}' is stale or does not belong to the current document`, matchCount: 0, details: null };
                }
                matches = [element];
            } else if (selector.kind === 'locator') {
                const locator = selector.locator;
                if (locator.kind === 'css') {
                    matches = Array.from(document.querySelectorAll(locator.selector));
                } else if (locator.kind === 'role') {
                    matches = Array.from(document.querySelectorAll('a[href], button, input, select, textarea, [role], h1, h2, h3, h4, h5, h6'))
                        .filter((element) => roleFor(element) === locator.role)
                        .filter((element) => locator.name === null || textMatches(nameFor(element), locator.name, locator.exact));
                } else if (locator.kind === 'text') {
                    matches = Array.from(document.querySelectorAll('body *')).filter((element) => {
                        if (!textMatches(element.innerText, locator.text, locator.exact)) return false;
                        return !Array.from(element.children).some((child) => textMatches(child.innerText, locator.text, locator.exact));
                    });
                } else {
                    return { error: `unsupported locator kind: ${locator.kind}`, matchCount: 0, details: null };
                }
            } else {
                return { error: `unsupported node selector kind: ${selector.kind}`, matchCount: 0, details: null };
            }
        } catch (error) {
            return { error: String(error), matchCount: 0, details: null };
        }
        matches = Array.from(new Set(matches)).filter(visible);
        if (matches.length !== 1) return { error: null, matchCount: matches.length, details: null };
        const element = matches[0];
        const rect = element.getBoundingClientRect();
        const style = window.getComputedStyle(element);
        const styleNames = ['display', 'position', 'top', 'right', 'bottom', 'left', 'z-index',
            'box-sizing', 'overflow', 'overflow-x', 'overflow-y', 'flex-direction', 'flex-wrap',
            'flex-grow', 'flex-shrink', 'flex-basis', 'justify-content', 'align-items', 'align-content',
            'gap', 'row-gap', 'column-gap', 'grid-template-columns', 'grid-template-rows', 'grid-column',
            'grid-row', 'width', 'height', 'min-width', 'min-height', 'max-width', 'max-height',
            'margin-top', 'margin-right', 'margin-bottom', 'margin-left', 'padding-top', 'padding-right',
            'padding-bottom', 'padding-left', 'font-family', 'font-size', 'font-weight', 'font-style',
            'line-height', 'letter-spacing', 'text-align', 'text-decoration', 'color', 'background-color',
            'border-top-width', 'border-right-width', 'border-bottom-width', 'border-left-width',
            'border-top-color', 'border-right-color', 'border-bottom-color', 'border-left-color',
            'border-top-style', 'border-right-style', 'border-bottom-style', 'border-left-style',
            'border-radius', 'box-shadow', 'opacity', 'visibility', 'transform'];
        const sensitiveAttribute = /(^|[-_:])(password|passwd|secret|token|authorization|cookie|session|csrf|xsrf|api[-_]?key)([-_:]|$)/i;
        const urlAttributes = new Set(['href', 'src', 'action', 'formaction', 'poster', 'cite']);
        const sanitizeUrl = (value) => {
            try {
                const url = new URL(value, document.baseURI);
                if (!['http:', 'https:'].includes(url.protocol)) return '[non-http URL omitted]';
                url.username = '';
                url.password = '';
                url.search = '';
                url.hash = '';
                return url.toString();
            } catch (_) {
                return '[invalid URL omitted]';
            }
        };
        const sanitizeAttributeValue = (owner, attribute) => {
            const name = attribute.name.toLowerCase();
            const tag = owner.tagName.toLowerCase();
            const type = tag === 'input' ? (owner.getAttribute('type') || 'text').toLowerCase() : '';
            if (sensitiveAttribute.test(name) ||
                (name === 'value' && (tag === 'input' || tag === 'textarea')) ||
                (name === 'value' && type === 'hidden')) return '[redacted]';
            if (name === 'style') return '[inline style omitted; use computed styles]';
            if (name === 'srcset') return '[source set omitted]';
            if (urlAttributes.has(name)) return sanitizeUrl(attribute.value);
            return attribute.value;
        };
        const allAttributes = Array.from(element.attributes);
        const attributes = allAttributes.slice(0, maxAttributes).map((attribute) => ({
            name: attribute.name.slice(0, maxField),
            value: sanitizeAttributeValue(element, attribute).slice(0, maxAttributeValue)
        }));
        const styles = styleNames.map((name) => ({ name, value: style.getPropertyValue(name).slice(0, maxStyleValue) }));
        const clone = element.cloneNode(true);
        const originals = [element, ...element.querySelectorAll('*')];
        const clones = [clone, ...clone.querySelectorAll('*')];
        for (let index = 0; index < clones.length; index += 1) {
            const original = originals[index];
            const copy = clones[index];
            for (const attribute of Array.from(copy.attributes)) {
                const source = original.getAttributeNode(attribute.name) || attribute;
                copy.setAttribute(attribute.name, sanitizeAttributeValue(original, source));
            }
            const tag = copy.tagName.toLowerCase();
            const type = tag === 'input' ? (copy.getAttribute('type') || 'text').toLowerCase() : '';
            if (tag === 'textarea' || (tag === 'input' && ['password', 'hidden'].includes(type))) {
                copy.textContent = '[redacted]';
                copy.setAttribute('value', '[redacted]');
            }
        }
        const outer = clone.outerHTML || '';
        return { error: null, matchCount: 1, details: {
            target,
            documentId: registry.documentId,
            nodeRef: refFor(element),
            tag: element.tagName.toLowerCase(),
            id: element.id ? element.id.slice(0, maxField) : null,
            classes: Array.from(element.classList || []).slice(0, maxClasses).map((value) => value.slice(0, maxField)),
            role: roleFor(element)?.slice(0, maxField) || null,
            name: nameFor(element)?.slice(0, maxText) || null,
            text: element.matches('input[type="password" i], input[type="hidden" i]') ? null :
                normalize(element.innerText || element.value || '').slice(0, maxText) || null,
            bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
            attributes,
            attributesTruncated: allAttributes.length > maxAttributes,
            styles,
            outerHtml: outer.slice(0, maxOuterHtml),
            outerHtmlTruncated: outer.length > maxOuterHtml
        }};
    })()"#
        .replace("__TARGET__", &target)
        .replace("__SELECTOR__", &selector)
        .replace("__MAX_TEXT__", &MAX_DOM_TEXT_CHARS.to_string())
        .replace("__MAX_FIELD__", &MAX_DOM_FIELD_CHARS.to_string())
        .replace("__MAX_CLASSES__", &MAX_DOM_CLASSES.to_string())
        .replace("__MAX_ATTRIBUTES__", &MAX_NODE_ATTRIBUTES.to_string())
        .replace(
            "__MAX_ATTRIBUTE_VALUE__",
            &MAX_NODE_ATTRIBUTE_VALUE_CHARS.to_string(),
        )
        .replace("__MAX_STYLE_VALUE__", &MAX_STYLE_VALUE_CHARS.to_string())
        .replace("__MAX_OUTER_HTML__", &MAX_OUTER_HTML_CHARS.to_string()))
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
    fn browser_profile_branding_is_distinct_and_preserves_existing_preferences() {
        let temp = tempdir().unwrap();
        let profile_dir = temp.path().join("browser-profile");
        let default_dir = profile_dir.join("Default");
        fs::create_dir_all(&default_dir).unwrap();
        fs::write(
            default_dir.join("Preferences"),
            br#"{"keep":{"existing":true},"profile":{"avatar_index":26}}"#,
        )
        .unwrap();
        fs::write(
            profile_dir.join("Local State"),
            br#"{"keep":{"local":true},"profile":{"info_cache":{"Default":{"active_time":42}}}}"#,
        )
        .unwrap();

        ensure_browser_profile_branding(&profile_dir).unwrap();

        let preferences: Value =
            serde_json::from_slice(&fs::read(default_dir.join("Preferences")).unwrap()).unwrap();
        assert_eq!(preferences["keep"]["existing"], true);
        assert_eq!(preferences["profile"]["avatar_index"], 26);
        assert_eq!(preferences["profile"]["name"], BROWSER_PROFILE_NAME);
        assert_eq!(
            preferences["browser"]["theme"]["user_color2"],
            BROWSER_PROFILE_COLOR
        );
        assert_eq!(
            preferences["extensions"]["theme"]["id"],
            "user_color_theme_id"
        );
        let local_state: Value =
            serde_json::from_slice(&fs::read(profile_dir.join("Local State")).unwrap()).unwrap();
        assert_eq!(local_state["keep"]["local"], true);
        assert_eq!(
            local_state["profile"]["info_cache"]["Default"]["active_time"],
            42
        );
        assert_eq!(
            local_state["profile"]["info_cache"]["Default"]["name"],
            BROWSER_PROFILE_NAME
        );
        assert_eq!(
            local_state["profile"]["info_cache"]["Default"]["is_using_default_name"],
            false
        );
    }

    #[tokio::test]
    async fn agent_activity_tracks_active_and_failed_operations() {
        let temp = tempdir().unwrap();
        let service = BrowserControlService::new(temp.path());
        let mut telemetry_changes = service.telemetry_changes();
        assert_eq!(*telemetry_changes.borrow(), 0);

        let activity_id = service
            .begin_agent_operation(BrowserOperation::Navigate)
            .await;
        telemetry_changes.changed().await.unwrap();
        assert_eq!(*telemetry_changes.borrow_and_update(), 1);
        let active = service.telemetry().await.active_agent_operation.unwrap();
        assert_eq!(active.id, activity_id);
        assert_eq!(active.operation, BrowserOperation::Navigate);

        service
            .finish_agent_operation(activity_id, Some("navigation failed".to_string()))
            .await;
        telemetry_changes.changed().await.unwrap();
        assert_eq!(*telemetry_changes.borrow_and_update(), 2);
        let telemetry = service.telemetry().await;
        assert!(telemetry.active_agent_operation.is_none());
        let outcome = telemetry.last_agent_operation.unwrap();
        assert_eq!(outcome.operation, BrowserOperation::Navigate);
        assert_eq!(outcome.error.as_deref(), Some("navigation failed"));
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
    fn named_targets_and_evidence_labels_are_strict_and_generic() {
        for name in ["design", "implementation", "before", "after", "site-a"] {
            assert_eq!(validate_browser_target_name(name).unwrap(), name);
        }
        for name in ["", "Portal", "1target", "target space", "target/route"] {
            assert!(
                validate_browser_target_name(name).is_err(),
                "accepted {name:?}"
            );
        }
        assert_eq!(
            validate_evidence_label("  account settings / desktop  ").unwrap(),
            "account settings / desktop"
        );
        assert!(validate_evidence_label("\n").is_err());
        assert!(validate_evidence_label("route\u{0000}").is_err());
    }

    #[test]
    fn evidence_retention_replaces_matching_target_labels_and_stays_bounded() {
        let mut telemetry = BrowserTelemetrySnapshot::default();
        let evidence = |id: u64, target: String| BrowserEvidence {
            id,
            target,
            label: "account-settings".to_string(),
            capture_mode: BrowserCaptureMode::Viewport,
            title: "Settings".to_string(),
            display_url: Some("https://example.com/settings".to_string()),
            viewport: BrowserViewport::desktop(1_280, 720),
            captured_at: SystemTime::now(),
            console_error_count: 0,
            failed_request_count: 0,
            screenshot_png: Arc::new(vec![id as u8]),
        };
        for id in 1..=7 {
            retain_browser_evidence(&mut telemetry, evidence(id, format!("target-{id}")));
        }
        assert_eq!(telemetry.evidence.len(), MAX_RETAINED_EVIDENCE);
        assert!(telemetry.evidence.iter().all(|capture| capture.id != 1));

        retain_browser_evidence(&mut telemetry, evidence(8, "target-2".to_string()));
        assert_eq!(telemetry.evidence.len(), MAX_RETAINED_EVIDENCE);
        assert_eq!(
            telemetry
                .evidence
                .iter()
                .filter(|capture| capture.target == "target-2")
                .count(),
            1
        );
        assert_eq!(telemetry.latest_evidence.unwrap().id, 8);
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
    fn dom_inspection_expressions_are_bounded_and_serialize_selectors_safely() {
        let outline = dom_outline_expression("design").unwrap();
        assert!(outline.contains(&MAX_DOM_OUTLINE_NODES.to_string()));
        assert!(outline.contains(&MAX_DOM_OUTLINE_DEPTH.to_string()));
        assert!(outline.contains("documentId"));
        assert!(outline.contains("new WeakRef"));
        assert!(!outline.contains("__MAX_"));

        let selector = BrowserNodeSelector::Locator {
            locator: BrowserLocator::role("button", "Save \"quoted\" value"),
        };
        let details = node_details_expression("implementation", &selector).unwrap();
        assert!(details.contains(r#"Save \"quoted\" value"#));
        assert!(details.contains(&MAX_OUTER_HTML_CHARS.to_string()));
        assert!(details.contains("grid-template-columns"));
        assert!(!details.contains("__SELECTOR__"));
        assert!(!details.contains("__MAX_"));

        assert!(BrowserNodeSelector::NodeRef {
            node_ref: String::new()
        }
        .validate()
        .is_err());
        assert!(BrowserNodeSelector::Locator {
            locator: BrowserLocator::css("x".repeat(MAX_LOCATOR_VALUE_CHARS + 1))
        }
        .validate()
        .is_err());
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

    #[tokio::test]
    async fn scroll_rejects_unbounded_or_non_finite_deltas_before_browser_access() {
        let temp = tempdir().unwrap();
        let mut controller = BrowserController::new(temp.path());
        assert!(controller.scroll(f64::NAN, 0.0).await.is_err());
        assert!(controller.scroll(0.0, 100_001.0).await.is_err());
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

    #[test]
    fn diagnostic_urls_remove_credentials_query_and_fragment() {
        assert_eq!(
            sanitize_diagnostic_url(
                "https://user:password@example.com/private/path?token=secret#fragment"
            )
            .as_deref(),
            Some("https://example.com/private/path")
        );
        assert!(sanitize_diagnostic_url("data:text/plain,secret").is_none());
    }

    #[tokio::test]
    async fn cdp_events_capture_bounded_sanitized_failures() {
        let diagnostics = Mutex::new(BrowserDiagnosticState::default());
        for index in 0..=MAX_CONSOLE_ERRORS {
            record_cdp_event(
                &diagnostics,
                &json!({
                    "method": "Runtime.consoleAPICalled",
                    "params": {
                        "type": "error",
                        "args": [{ "value": format!("console-{index}") }]
                    }
                }),
            )
            .await;
        }
        record_cdp_event(
            &diagnostics,
            &json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "request-1",
                    "request": {
                        "url": "https://example.com/failure?token=secret",
                        "method": "POST"
                    }
                }
            }),
        )
        .await;
        record_cdp_event(
            &diagnostics,
            &json!({
                "method": "Network.responseReceived",
                "params": {
                    "requestId": "request-1",
                    "response": {
                        "url": "https://example.com/failure?token=secret",
                        "status": 503,
                        "statusText": "Unavailable"
                    }
                }
            }),
        )
        .await;

        let diagnostics = diagnostics.lock().await;
        assert_eq!(diagnostics.console_errors.len(), MAX_CONSOLE_ERRORS);
        assert_eq!(
            diagnostics.console_errors.front().unwrap().text,
            "console-1"
        );
        assert_eq!(diagnostics.failed_requests.len(), 1);
        assert_eq!(
            diagnostics.failed_requests[0],
            BrowserNetworkFailure {
                url: "https://example.com/failure".to_string(),
                method: Some("POST".to_string()),
                status: Some(503),
                error_text: Some("Unavailable".to_string()),
            }
        );
    }

    #[test]
    fn wait_conditions_reject_empty_values() {
        assert!(validate_wait_condition(&BrowserWaitCondition::Text {
            text: " ".to_string(),
            exact: false,
        })
        .is_err());
        assert!(validate_wait_condition(&BrowserWaitCondition::Url {
            url: String::new(),
            exact: true,
        })
        .is_err());
        assert!(
            validate_wait_condition(&BrowserWaitCondition::LoadingState {
                state: BrowserLoadingState::Complete,
            })
            .is_ok()
        );
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
                    let request_size = socket.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..request_size]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    if path.starts_with("/missing-resource") {
                        let body = "intentional browser smoke failure";
                        let response = format!(
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }
                    let body = r#"<!doctype html>
                        <title>GitTerm browser smoke</title>
                        <style>#status { display: grid; gap: 12px; padding: 8px; color: rgb(10, 20, 30); }</style>
                        <main id="status">browser-control-ready</main>
                        <form onsubmit="event.preventDefault(); document.getElementById('status').textContent = 'submitted:' + document.getElementById('smoke-input').value">
                            <input type="hidden" name="csrf-token" value="must-not-leak">
                            <label for="smoke-input">Smoke input</label>
                            <input id="smoke-input" name="smokeInput">
                            <button type="button" aria-label="Run smoke action" onclick="document.getElementById('status').textContent = 'clicked'">Run</button>
                        </form>
                        <script>
                            console.error('gitterm-console-smoke');
                            fetch('/missing-resource?token=must-not-leak');
                            window.addEventListener('wheel', () => {
                                document.getElementById('status').textContent = 'scrolled';
                            }, { once: true });
                            setTimeout(() => { throw new Error('gitterm-uncaught-smoke'); }, 25);
                            setTimeout(() => {
                                const delayed = document.createElement('p');
                                delayed.textContent = 'delayed-ready';
                                document.body.appendChild(delayed);
                            }, 150);
                        </script>"#;
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
        let config_root = temp.path().join("gitterm-v4");
        let service = BrowserControlService::new(&config_root);
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
        service
            .wait_for(
                &BrowserWaitCondition::Text {
                    text: "delayed-ready".to_string(),
                    exact: true,
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        service
            .wait_for(
                &BrowserWaitCondition::Url {
                    url: url.clone(),
                    exact: true,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        service.focus().await.unwrap();
        let target_id = service.status().await.unwrap().target_id.unwrap();
        let snapshot = service.snapshot().await.unwrap();
        assert_eq!(snapshot.url, url);
        assert_eq!(snapshot.title, "GitTerm browser smoke");
        assert!(snapshot.screenshot_png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(snapshot
            .console_errors
            .iter()
            .any(|error| error.text.contains("gitterm-console-smoke")));
        assert!(snapshot
            .console_errors
            .iter()
            .any(|error| error.text.contains("gitterm-uncaught-smoke")));
        assert!(snapshot.failed_requests.iter().any(|failure| {
            failure.url == format!("http://{address}/missing-resource")
                && failure.status == Some(503)
                && !failure.url.contains("token")
        }));
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
        service
            .wait_for(
                &BrowserWaitCondition::Locator {
                    locator: button.locator.clone(),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        service.scroll(0.0, 400.0).await.unwrap();
        service
            .wait_for(
                &BrowserWaitCondition::Text {
                    text: "scrolled".to_string(),
                    exact: true,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();

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

        let primary_capture = service
            .capture(
                Some(DEFAULT_BROWSER_TARGET_NAME),
                "smoke-route",
                BrowserCaptureMode::FullPage,
            )
            .await
            .unwrap();
        assert_eq!(primary_capture.target, DEFAULT_BROWSER_TARGET_NAME);
        assert_eq!(primary_capture.capture_mode, BrowserCaptureMode::FullPage);

        let outline = service
            .dom_outline_in(Some(DEFAULT_BROWSER_TARGET_NAME))
            .await
            .unwrap();
        assert_eq!(outline.target, DEFAULT_BROWSER_TARGET_NAME);
        assert!(!outline.document_id.is_empty());
        assert!(outline.nodes.len() <= MAX_DOM_OUTLINE_NODES);
        let status_node = outline
            .nodes
            .iter()
            .find(|node| node.id.as_deref() == Some("status"))
            .unwrap();
        assert_eq!(status_node.tag, "main");
        assert_eq!(status_node.role.as_deref(), Some("main"));
        let status_ref = status_node.node_ref.clone();
        let details = service
            .node_details_in(
                Some(DEFAULT_BROWSER_TARGET_NAME),
                &BrowserNodeSelector::NodeRef {
                    node_ref: status_ref.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(details.node_ref, status_ref);
        assert!(details.outer_html.contains("id=\"status\""));
        assert!(details
            .styles
            .iter()
            .any(|property| property.name == "display" && property.value == "grid"));
        assert!(details
            .styles
            .iter()
            .any(|property| property.name == "gap" && property.value == "12px"));
        let button_details = service
            .node_details_in(
                Some(DEFAULT_BROWSER_TARGET_NAME),
                &BrowserNodeSelector::Locator {
                    locator: BrowserLocator::role("button", "Run smoke action"),
                },
            )
            .await
            .unwrap();
        assert_eq!(button_details.tag, "button");
        assert_eq!(button_details.name.as_deref(), Some("Run smoke action"));
        let form_details = service
            .node_details_in(
                Some(DEFAULT_BROWSER_TARGET_NAME),
                &BrowserNodeSelector::Locator {
                    locator: BrowserLocator::css("form"),
                },
            )
            .await
            .unwrap();
        assert!(form_details.outer_html.contains("[redacted]"));
        assert!(!form_details.outer_html.contains("must-not-leak"));
        service
            .reload_in(Some(DEFAULT_BROWSER_TARGET_NAME), false)
            .await
            .unwrap();
        let stale_error = service
            .node_details_in(
                Some(DEFAULT_BROWSER_TARGET_NAME),
                &BrowserNodeSelector::NodeRef {
                    node_ref: status_ref,
                },
            )
            .await
            .unwrap_err();
        assert!(stale_error.to_string().contains("stale"));

        service.open_target("implementation", &url).await.unwrap();
        service
            .wait_for_ready_in(Some("implementation"), Duration::from_secs(5))
            .await
            .unwrap();
        service
            .resize_in(Some("implementation"), mobile)
            .await
            .unwrap();
        service
            .capture(
                Some("implementation"),
                "smoke-route",
                BrowserCaptureMode::FullPage,
            )
            .await
            .unwrap();
        let targets = service.targets().await.unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().any(|target| target.name == "primary"));
        assert!(targets
            .iter()
            .any(|target| target.name == "implementation" && target.active));

        let telemetry = service.telemetry().await;
        let evidence = telemetry.latest_evidence.as_ref().unwrap();
        assert_eq!(evidence.title, "GitTerm browser smoke");
        assert_eq!(evidence.display_url.as_deref(), Some(url.as_str()));
        assert_eq!(evidence.viewport, mobile);
        assert!(evidence.screenshot_png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let paired = telemetry
            .evidence
            .iter()
            .filter(|capture| capture.label == "smoke-route")
            .collect::<Vec<_>>();
        assert_eq!(paired.len(), 2);
        assert_ne!(paired[0].target, paired[1].target);

        service.close_target("implementation").await.unwrap();
        service
            .focus_target(DEFAULT_BROWSER_TARGET_NAME)
            .await
            .unwrap();

        let attached_service = BrowserControlService::new(&config_root);
        assert_eq!(
            attached_service.status().await.unwrap().state,
            BrowserState::Running
        );
        attached_service.focus().await.unwrap();
        attached_service.disconnect().await.unwrap();
        assert_eq!(
            attached_service.status().await.unwrap().state,
            BrowserState::Stopped
        );
        drop(service);
        server.abort();
    }
}
