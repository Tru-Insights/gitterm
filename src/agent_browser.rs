//! agent-browser as an engine behind GitTerm's browser tools (TRU-177).
//!
//! agent-browser is an Apache-2.0 browser automation CLI. Its own MCP
//! server is not safe to hand to agents (its `eval` reads cookies, its
//! sessions can close each other's tabs, and its tool list is large), so
//! agents never talk to it: GitTerm's browser tools stay the front door and
//! this engine runs their operations through the CLI.
//!
//! - GitTerm launches and owns Chrome; the CLI attaches with `--cdp <port>`.
//! - Each target is one pinned agent-browser session. agent-browser's
//!   console, error and network buffers belong to a session and carry no
//!   tab identity, and a session that holds two tabs mixes both tabs'
//!   entries, so a session per target is what keeps a target's diagnostics
//!   its own. Sessions belong to the owning chat through GitTerm's target
//!   registry, the same TRU-176 registry the gitterm engine uses.
//! - Every command comes from [`AbCommand`], an allowlist. It has no
//!   variant that takes an agent's JavaScript, cookies, storage, auth or
//!   state. GitTerm's own JavaScript (outline, inspect, locators, waits)
//!   is the only script that runs. agent-browser's action policy is set to
//!   the same allowlist as a second guard.
//! - Commands go to the CLI as `batch` JSON on stdin, never as argv: the
//!   CLI reads flags anywhere in argv (typing `--headed` would set a flag),
//!   while a batch command's values are taken literally.
//! - The binary is pinned to [`PINNED_AGENT_BROWSER`]; the engine refuses
//!   to start on any other version.

use crate::browser_control::{
    action_target_from_resolution, bounded_text, browser_target_not_found, dom_outline_expression,
    executable_on_path, list_page_targets, locator_expression, node_details_expression,
    sanitize_diagnostic_url, snapshot_expression, snapshot_ref, validate_browser_target_name,
    validate_evidence_label, validate_navigation_url, validate_scroll_deltas,
    validate_wait_condition, wait_condition_predicate, BrowserCaptureMode, BrowserConsoleError,
    BrowserControlError, BrowserControlService, BrowserDiagnostics, BrowserDomOutline, BrowserKey,
    BrowserLaunchOptions, BrowserLocator, BrowserNetworkFailure, BrowserNodeDetails,
    BrowserNodeSelector, BrowserOwnerDisconnect, BrowserSnapshot, BrowserState, BrowserStatus,
    BrowserTargetInfo, BrowserTargetRegistry, BrowserViewport, BrowserWaitCondition,
    DevToolsTarget, LocatorPreparation, LocatorResolution, NavigationResult, NodeDetailsEvaluation,
    RegisteredTarget, Result, SnapshotState, TargetKey, ViewportCenter,
    DEFAULT_BROWSER_TARGET_NAME, MAX_CONSOLE_ERRORS, MAX_FULL_PAGE_CAPTURE_HEIGHT,
    MAX_FULL_PAGE_CAPTURE_PIXELS, MAX_FULL_PAGE_CAPTURE_WIDTH, MAX_NETWORK_FAILURES,
};
use crate::browser_engine::{
    BrowserActionOutcome, BrowserEngineConfig, BrowserRefAction, BROWSER_ENGINE_CONFIG_FILE,
};
use base64::Engine;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

/// The one agent-browser release this engine runs. agent-browser is pre-1.0
/// and changes weekly; an upgrade bumps this and must pass the contract
/// suite (`tests/agent_browser_contract.rs`) first.
pub const PINNED_AGENT_BROWSER: &str = "0.41.0";

/// The engine's directory under the shared config: sockets, its config and
/// action policy, and short-lived scratch files.
const STATE_DIR_NAME: &str = "agent-browser";
const SOCKET_DIR_NAME: &str = "s";
const SCRATCH_DIR_NAME: &str = "scratch";
const CLI_CONFIG_FILE: &str = "config.json";
const ACTION_POLICY_FILE: &str = "policy.json";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(90);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// A session's daemon exits after this long unused, so a chat GitTerm did
/// not get to clean up (a crash, say) leaves nothing running for long.
const SESSION_IDLE_TIMEOUT: &str = "30m";
const MAX_CLI_OUTPUT_CHARS: usize = 2_000;
const MAX_SNAPSHOT_REFS_CHARS: usize = 24_000;
const MAX_DIFF_CHARS: usize = 24_000;
/// Changed regions a pixel diff lists.
const MAX_DIFF_REGIONS: usize = 10;
/// Changed pixels closer than this many device pixels join one region.
const DIFF_REGION_CELL: u32 = 16;
/// Per-channel difference below which two pixels count as the same.
const DIFF_PIXEL_TOLERANCE: u8 = 8;
/// Larger screenshots are not decoded for regions, to bound memory.
const MAX_DIFF_REGION_PIXELS: u64 = 36_000_000;
const MAX_BASELINE_SNAPSHOT_CHARS: usize = 512_000;
const MAX_SCRATCH_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HAR_ENTRIES: usize = 200;
const MAX_DEVICE_NAME_CHARS: usize = 40;

/// agent-browser action names this engine runs. The action policy allows
/// these and denies everything else, cookies, storage, state and auth
/// included. Kept equal to [`AbCommand::action`]'s range by a unit test.
const ALLOWED_ACTIONS: [&str; 29] = [
    "launch",
    "navigate",
    "stream_status",
    "stream_disable",
    "close",
    "tab_switch",
    "tab_close",
    "evaluate",
    "waitforfunction",
    "click",
    "fill",
    "keyboard",
    "press",
    "mousemove",
    "mousedown",
    "mouseup",
    "wheel",
    "reload",
    "viewport",
    "device",
    "screenshot",
    "snapshot",
    "diff_snapshot",
    "diff_screenshot",
    "console",
    "errors",
    "requests",
    "har_start",
    "har_stop",
];

/// One allowlisted agent-browser command. GitTerm builds every command the
/// engine runs from this type; agents supply only validated values.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AbCommand {
    Open {
        url: String,
    },
    StreamStatus,
    StreamDisable,
    Close,
    TabSwitch {
        target_id: String,
    },
    TabClose {
        target_id: String,
    },
    TabCloseCurrent,
    /// GitTerm's own JavaScript, never an agent's.
    Eval {
        script: String,
    },
    /// Wait until GitTerm's own predicate holds.
    WaitFn {
        predicate: String,
        timeout_ms: u64,
    },
    ClickRef {
        reference: String,
    },
    FillRef {
        reference: String,
        text: String,
    },
    InsertText {
        text: String,
    },
    Press {
        key: &'static str,
    },
    MouseMove {
        x: f64,
        y: f64,
    },
    MouseDown,
    MouseUp,
    MouseWheel {
        delta_y: f64,
        delta_x: f64,
    },
    Reload,
    SetViewport {
        width: u32,
        height: u32,
        scale: f64,
    },
    SetDevice {
        name: String,
    },
    Screenshot {
        path: PathBuf,
        full_page: bool,
    },
    Snapshot {
        interactive: bool,
    },
    DiffSnapshot {
        baseline: PathBuf,
    },
    DiffScreenshot {
        baseline: PathBuf,
        output: PathBuf,
        full_page: bool,
    },
    Console,
    Errors,
    NetworkRequests,
    HarStart,
    HarStop {
        path: PathBuf,
    },
}

impl AbCommand {
    /// The command's words, one batch row.
    pub(crate) fn args(&self) -> Vec<String> {
        let words: Vec<String> = match self {
            Self::Open { url } => vec!["open".into(), url.clone()],
            Self::StreamStatus => vec!["stream".into(), "status".into()],
            Self::StreamDisable => vec!["stream".into(), "disable".into()],
            Self::Close => vec!["close".into()],
            Self::TabSwitch { target_id } => vec!["tab".into(), target_id.clone()],
            Self::TabClose { target_id } => vec!["tab".into(), "close".into(), target_id.clone()],
            Self::TabCloseCurrent => vec!["tab".into(), "close".into()],
            Self::Eval { script } => vec![
                "eval".into(),
                "-b".into(),
                base64::engine::general_purpose::STANDARD.encode(script),
            ],
            Self::WaitFn {
                predicate,
                timeout_ms,
            } => vec![
                "wait".into(),
                "--fn".into(),
                predicate.clone(),
                "--timeout".into(),
                timeout_ms.to_string(),
            ],
            Self::ClickRef { reference } => vec!["click".into(), format!("@{reference}")],
            Self::FillRef { reference, text } => {
                vec!["fill".into(), format!("@{reference}"), text.clone()]
            }
            Self::InsertText { text } => {
                vec!["keyboard".into(), "inserttext".into(), text.clone()]
            }
            Self::Press { key } => vec!["press".into(), (*key).into()],
            Self::MouseMove { x, y } => {
                vec!["mouse".into(), "move".into(), pixels(*x), pixels(*y)]
            }
            Self::MouseDown => vec!["mouse".into(), "down".into(), "left".into()],
            Self::MouseUp => vec!["mouse".into(), "up".into(), "left".into()],
            Self::MouseWheel { delta_y, delta_x } => vec![
                "mouse".into(),
                "wheel".into(),
                pixels(*delta_y),
                pixels(*delta_x),
            ],
            Self::Reload => vec!["reload".into()],
            Self::SetViewport {
                width,
                height,
                scale,
            } => vec![
                "set".into(),
                "viewport".into(),
                width.to_string(),
                height.to_string(),
                number(*scale),
            ],
            Self::SetDevice { name } => vec!["set".into(), "device".into(), name.clone()],
            Self::Screenshot { path, full_page } => {
                let mut words = vec!["screenshot".to_string()];
                if *full_page {
                    words.push("--full".into());
                }
                words.push(path.display().to_string());
                words
            }
            Self::Snapshot { interactive } => {
                let mut words = vec!["snapshot".to_string()];
                if *interactive {
                    words.push("-i".into());
                }
                words
            }
            Self::DiffSnapshot { baseline } => vec![
                "diff".into(),
                "snapshot".into(),
                "--baseline".into(),
                baseline.display().to_string(),
            ],
            Self::DiffScreenshot {
                baseline,
                output,
                full_page,
            } => {
                let mut words = vec![
                    "diff".to_string(),
                    "screenshot".into(),
                    "--baseline".into(),
                    baseline.display().to_string(),
                    "-o".into(),
                    output.display().to_string(),
                ];
                if *full_page {
                    words.push("--full".into());
                }
                words
            }
            Self::Console => vec!["console".into()],
            Self::Errors => vec!["errors".into()],
            Self::NetworkRequests => vec!["network".into(), "requests".into()],
            Self::HarStart => vec!["network".into(), "har".into(), "start".into()],
            Self::HarStop { path } => vec![
                "network".into(),
                "har".into(),
                "stop".into(),
                path.display().to_string(),
            ],
        };
        words
    }

    /// The action name agent-browser's policy checks for this command.
    pub(crate) fn action(&self) -> &'static str {
        match self {
            Self::Open { .. } => "navigate",
            Self::StreamStatus => "stream_status",
            Self::StreamDisable => "stream_disable",
            Self::Close => "close",
            Self::TabSwitch { .. } => "tab_switch",
            Self::TabClose { .. } | Self::TabCloseCurrent => "tab_close",
            Self::Eval { .. } => "evaluate",
            Self::WaitFn { .. } => "waitforfunction",
            Self::ClickRef { .. } => "click",
            Self::FillRef { .. } => "fill",
            Self::InsertText { .. } => "keyboard",
            Self::Press { .. } => "press",
            Self::MouseMove { .. } => "mousemove",
            Self::MouseDown => "mousedown",
            Self::MouseUp => "mouseup",
            Self::MouseWheel { .. } => "wheel",
            Self::Reload => "reload",
            Self::SetViewport { .. } => "viewport",
            Self::SetDevice { .. } => "device",
            Self::Screenshot { .. } => "screenshot",
            Self::Snapshot { .. } => "snapshot",
            Self::DiffSnapshot { .. } => "diff_snapshot",
            Self::DiffScreenshot { .. } => "diff_screenshot",
            Self::Console => "console",
            Self::Errors => "errors",
            Self::NetworkRequests => "requests",
            Self::HarStart => "har_start",
            Self::HarStop { .. } => "har_stop",
        }
    }
}

/// A finite number as agent-browser reads one.
fn number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// A whole number of CSS pixels: 0.41.0's `mouse` commands reject
/// fractions ("Missing arguments"). Callers have bounded the values.
fn pixels(value: f64) -> String {
    format!("{}", value.round() as i64)
}

/// The batch's stdin: one row of words per command.
pub(crate) fn batch_stdin(commands: &[AbCommand]) -> String {
    Value::Array(
        commands
            .iter()
            .map(|command| Value::Array(command.args().into_iter().map(Value::String).collect()))
            .collect(),
    )
    .to_string()
}

fn action_allowed(action: &str) -> bool {
    ALLOWED_ACTIONS.contains(&action)
}

/// The action policy: the allowlist, everything else denied.
pub(crate) fn action_policy() -> Value {
    serde_json::json!({ "default": "deny", "allow": ALLOWED_ACTIONS })
}

/// Why one agent-browser command failed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AbFailure {
    pub(crate) message: String,
    pub(crate) code: Option<String>,
}

impl AbFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    /// The session's pinned tab no longer exists.
    pub(crate) fn tab_gone(&self) -> bool {
        self.code.as_deref() == Some("tab_gone") || self.message.starts_with("tab_gone:")
    }
}

/// Each command's `data` from `batch --json --bail --omit-command` output,
/// or the first command's failure. A batch the CLI refused as a whole
/// prints one `{ success: false, error }` object instead of rows.
pub(crate) fn parse_batch_output(
    stdout: &str,
    expected_rows: usize,
) -> std::result::Result<Vec<Value>, AbFailure> {
    let parsed: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
        AbFailure::new(format!(
            "agent-browser printed output that is not JSON ({error}): {}",
            bounded_chars(stdout.trim(), MAX_CLI_OUTPUT_CHARS)
        ))
    })?;
    let rows = match parsed {
        Value::Array(rows) => rows,
        Value::Object(object) => {
            return Err(row_failure(&Value::Object(object)));
        }
        other => {
            return Err(AbFailure::new(format!(
                "agent-browser printed unexpected JSON: {}",
                bounded_chars(&other.to_string(), MAX_CLI_OUTPUT_CHARS)
            )));
        }
    };
    let mut results = Vec::with_capacity(rows.len());
    for row in rows {
        if row.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(row_failure(&row));
        }
        results.push(row.get("result").cloned().unwrap_or(Value::Null));
    }
    if results.len() != expected_rows {
        return Err(AbFailure::new(format!(
            "agent-browser ran {} of {expected_rows} commands without reporting a failure",
            results.len()
        )));
    }
    Ok(results)
}

/// Whether the batch's leading `stream status` found streaming on, read
/// even when a later command failed.
pub(crate) fn batch_found_streaming(stdout: &str) -> bool {
    serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .and_then(|parsed| parsed.get(0).cloned())
        .filter(|row| row.get("success").and_then(Value::as_bool) == Some(true))
        .and_then(|row| row.pointer("/result/enabled").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn row_failure(row: &Value) -> AbFailure {
    AbFailure {
        message: row
            .get("error")
            .and_then(Value::as_str)
            .map(|error| bounded_chars(error, MAX_CLI_OUTPUT_CHARS))
            .unwrap_or_else(|| "agent-browser reported a failure without a message".to_string()),
        code: row.get("code").and_then(Value::as_str).map(str::to_string),
    }
}

/// `agent-browser --version` prints `agent-browser 0.41.0`.
pub(crate) fn parse_version(stdout: &str) -> Option<&str> {
    stdout
        .trim()
        .strip_prefix("agent-browser ")
        .map(str::trim)
        .filter(|version| !version.is_empty())
}

fn bounded_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// `text` cut to at most `max` characters at a line boundary when one is
/// near, with whether it was cut.
fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_string(), false);
    }
    let cut: String = text.chars().take(max).collect();
    let cut = match cut.rfind('\n') {
        Some(index) if index > max / 2 => cut[..index].to_string(),
        _ => cut,
    };
    (cut, true)
}

/// `console --json`: the error and assert messages, as the gitterm engine
/// reports them. agent-browser 0.41.0 records no source URL or line.
pub(crate) fn parse_console_errors(data: &Value) -> Vec<BrowserConsoleError> {
    let mut errors: Vec<BrowserConsoleError> = data
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| {
            matches!(
                message.get("type").and_then(Value::as_str),
                Some("error" | "assert")
            )
        })
        .map(|message| BrowserConsoleError {
            source: "console".to_string(),
            text: bounded_text(
                message
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("console error"),
            ),
            url: None,
            line_number: None,
        })
        .collect();
    keep_last(&mut errors, MAX_CONSOLE_ERRORS);
    errors
}

/// `errors --json`: uncaught page exceptions. (0.41.0's text mode prints a
/// blank line for these, so the engine only ever reads JSON.)
pub(crate) fn parse_page_errors(data: &Value) -> Vec<BrowserConsoleError> {
    let mut errors: Vec<BrowserConsoleError> = data
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|error| BrowserConsoleError {
            source: "exception".to_string(),
            text: bounded_text(
                error
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("uncaught exception"),
            ),
            url: error
                .get("url")
                .and_then(Value::as_str)
                .and_then(sanitize_diagnostic_url),
            line_number: error.get("line").and_then(Value::as_u64),
        })
        .collect();
    keep_last(&mut errors, MAX_CONSOLE_ERRORS);
    errors
}

/// `network requests --json`: the requests that failed. A status of 400 or
/// more failed; a request with no status got no response (it failed, was
/// blocked, or is still in flight; 0.41.0 does not say which). Headers,
/// which can carry cookies and tokens, are never read.
pub(crate) fn parse_failed_requests(data: &Value) -> Vec<BrowserNetworkFailure> {
    let mut failures: Vec<BrowserNetworkFailure> = data
        .get("requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|request| {
            let url = request
                .get("url")
                .and_then(Value::as_str)
                .and_then(sanitize_diagnostic_url)?;
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string);
            match request.get("status").and_then(Value::as_u64) {
                Some(status) if status >= 400 => Some(BrowserNetworkFailure {
                    url,
                    method,
                    status: u16::try_from(status).ok(),
                    error_text: None,
                }),
                Some(_) => None,
                None => Some(BrowserNetworkFailure {
                    url,
                    method,
                    status: None,
                    error_text: Some(
                        "no response recorded (failed, blocked, or still pending)".to_string(),
                    ),
                }),
            }
        })
        .collect();
    keep_last(&mut failures, MAX_NETWORK_FAILURES);
    failures
}

fn keep_last<T>(items: &mut Vec<T>, max: usize) {
    if items.len() > max {
        items.drain(..items.len() - max);
    }
}

/// One request of a HAR capture, without headers, cookies or bodies.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowserHarEntry {
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub mime_type: Option<String>,
    pub resource_type: Option<String>,
    pub time_ms: Option<f64>,
    pub response_bytes: Option<i64>,
}

/// A HAR file reduced to what an agent may see: method, sanitized URL,
/// status, type, timing and size per request. Headers, cookies, query
/// strings and bodies are dropped.
pub(crate) fn summarize_har(har: &Value) -> (Vec<BrowserHarEntry>, usize) {
    let entries = har
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let summaries = entries
        .iter()
        .filter_map(|entry| {
            let url = entry
                .pointer("/request/url")
                .and_then(Value::as_str)
                .and_then(sanitize_diagnostic_url)?;
            Some(BrowserHarEntry {
                method: entry
                    .pointer("/request/method")
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_string(),
                url,
                status: entry
                    .pointer("/response/status")
                    .and_then(Value::as_u64)
                    .filter(|status| *status > 0)
                    .and_then(|status| u16::try_from(status).ok()),
                mime_type: entry
                    .pointer("/response/content/mimeType")
                    .and_then(Value::as_str)
                    .filter(|mime| !mime.is_empty())
                    .map(|mime| bounded_chars(mime, 100)),
                resource_type: entry
                    .get("_resourceType")
                    .and_then(Value::as_str)
                    .map(|kind| bounded_chars(kind, 40)),
                time_ms: entry.get("time").and_then(Value::as_f64),
                response_bytes: entry.pointer("/response/bodySize").and_then(Value::as_i64),
            })
        })
        .take(MAX_HAR_ENTRIES)
        .collect();
    (summaries, entries.len())
}

/// browser_snapshot_refs: agent-browser's interactive accessibility tree.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowserSnapshotRefs {
    pub target: String,
    /// Set when the call stored this page state as diff evidence under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_label: Option<String>,
    pub url: Option<String>,
    /// One line per element; its `[ref=eN]` works as a `ref` locator.
    pub snapshot: String,
    pub ref_count: usize,
    pub total_chars: usize,
    /// The snapshot was longer than the cap and is cut.
    pub truncated: bool,
}

/// The role and name of one ref in a target's latest snapshot.
#[derive(Debug, Clone, PartialEq)]
struct SnapshotRefEntry {
    role: Option<String>,
    name: Option<String>,
}

fn parse_snapshot_refs(data: &Value) -> HashMap<String, SnapshotRefEntry> {
    data.get("refs")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(reference, entry)| {
            let field = |name: &str| {
                entry
                    .get(name)
                    .and_then(Value::as_str)
                    .map(|value| bounded_chars(value, 200))
            };
            (
                reference.clone(),
                SnapshotRefEntry {
                    role: field("role"),
                    name: field("name"),
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDiffMode {
    /// Accessibility-tree text diff.
    Snapshot,
    /// Pixel diff.
    Screenshot,
}

/// One changed area of a pixel diff, in page CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BrowserDiffRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// browser_diff: the current page against a labelled baseline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowserDiff {
    pub target: String,
    pub baseline_target: String,
    pub baseline_label: String,
    pub mode: BrowserDiffMode,
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removals: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unchanged: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mismatch_percentage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub different_pixels: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pixels: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension_mismatch: Option<Value>,
    /// Pixel diff: where the page changed, largest first, at most
    /// `MAX_DIFF_REGIONS`, in page CSS pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regions: Option<Vec<BrowserDiffRegion>>,
    /// Pixel diff: how many regions changed in all; more than `regions` lists
    /// when the list is capped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_count: Option<usize>,
    /// Pixel diff: why `regions` is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regions_unavailable: Option<String>,
    /// The pixel diff image, returned as image content.
    #[serde(skip)]
    pub diff_png: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNetworkCaptureAction {
    Start,
    Stop,
}

/// browser_network_capture: a HAR recording of one target's requests.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowserNetworkCapture {
    pub target: String,
    pub recording: bool,
    pub entries: Vec<BrowserHarEntry>,
    pub entry_count: usize,
    pub truncated: bool,
}

/// browser_viewport: a named device preset, or a size.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrowserViewportRequest {
    pub device: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub device_scale_factor: Option<f64>,
}

/// What the engine keeps per target besides the registry's ownership.
struct AbTarget {
    target_id: String,
    session: String,
    viewport: Option<BrowserViewport>,
    refs: HashMap<String, SnapshotRefEntry>,
    har_recording: bool,
}

impl RegisteredTarget for AbTarget {
    fn target_id(&self) -> &str {
        &self.target_id
    }
}

#[derive(Default)]
struct AbState {
    /// The binary's version was checked and the state directory prepared.
    started: bool,
    next_session: u64,
    registry: BrowserTargetRegistry<AbTarget>,
    /// Accessibility snapshots captured with evidence, by evidence id, for
    /// snapshot diffs. Pruned with the evidence set; memory only.
    baselines: HashMap<u64, Arc<String>>,
}

/// The page one operation runs on.
struct AbPage {
    port: u16,
    session: String,
    target_id: String,
}

/// A scratch file the CLI writes or reads, removed when dropped.
struct ScratchFile(PathBuf);

impl ScratchFile {
    fn new(state_dir: &Path, extension: &str) -> Self {
        Self(
            state_dir
                .join(SCRATCH_DIR_NAME)
                .join(format!("{}.{extension}", Uuid::new_v4().simple())),
        )
    }

    fn read(&self, what: &str) -> Result<Vec<u8>> {
        let size = std::fs::metadata(&self.0)
            .map_err(|error| {
                BrowserControlError::new(format!(
                    "agent-browser did not write the {what} to {}: {error}",
                    self.0.display()
                ))
            })?
            .len();
        if size > MAX_SCRATCH_FILE_BYTES {
            return Err(BrowserControlError::new(format!(
                "the {what} agent-browser wrote is {size} bytes, over GitTerm's {MAX_SCRATCH_FILE_BYTES}-byte limit"
            )));
        }
        std::fs::read(&self.0).map_err(|error| {
            BrowserControlError::new(format!(
                "could not read the {what} at {}: {error}",
                self.0.display()
            ))
        })
    }

    fn write(&self, bytes: &[u8], what: &str) -> Result<()> {
        std::fs::write(&self.0, bytes).map_err(|error| {
            BrowserControlError::new(format!(
                "could not write the {what} to {}: {error}",
                self.0.display()
            ))
        })
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "agent-browser engine could not remove scratch file {}: {error}",
                self.0.display()
            ),
        }
    }
}

/// The agent-browser engine. One per GitTerm process; operations are
/// serialized by its state lock (and, for agents, by the browser tools).
pub struct AgentBrowserEngine {
    browser: BrowserControlService,
    /// The binary to run, or why none could be named.
    binary: std::result::Result<PathBuf, String>,
    config_file: PathBuf,
    state_dir: PathBuf,
    /// Distinguishes this process's sessions from any a previous GitTerm
    /// left idling, so a session name is never reused across restarts.
    session_prefix: String,
    state: Mutex<AbState>,
}

impl AgentBrowserEngine {
    pub fn new(
        shared_config_dir: &Path,
        config: &BrowserEngineConfig,
        browser: BrowserControlService,
    ) -> Self {
        let config_file = shared_config_dir.join(BROWSER_ENGINE_CONFIG_FILE);
        let binary = match &config.agent_browser_path {
            Some(path) => Ok(path.clone()),
            None => executable_on_path("agent-browser").ok_or_else(|| {
                format!(
                    "{} names no agent_browser_path and no agent-browser is on PATH",
                    config_file.display()
                )
            }),
        };
        Self {
            browser,
            binary,
            config_file,
            state_dir: shared_config_dir.join(STATE_DIR_NAME),
            session_prefix: format!("gt{}", &Uuid::new_v4().simple().to_string()[..8]),
            state: Mutex::new(AbState::default()),
        }
    }

    pub fn describe(&self) -> String {
        match &self.binary {
            Ok(path) => format!(
                "agent-browser {PINNED_AGENT_BROWSER} (pinned) at {}",
                path.display()
            ),
            Err(reason) => format!("agent-browser (unavailable: {reason})"),
        }
    }

    pub fn service(&self) -> &BrowserControlService {
        &self.browser
    }

    // ----- engine start -------------------------------------------------

    /// Engine start: refuse anything but the pinned binary, then prepare
    /// the state directory. Never falls back to the gitterm engine.
    async fn ensure_started(&self, state: &mut AbState) -> Result<()> {
        if state.started {
            return Ok(());
        }
        let binary = self.binary.as_ref().map_err(|reason| {
            BrowserControlError::new(format!(
                "the agent-browser engine cannot start: {reason}. GitTerm pins agent-browser {PINNED_AGENT_BROWSER}; install that release and set agent_browser_path in {}",
                self.config_file.display()
            ))
        })?;
        let found = binary_version(binary).await?;
        if found != PINNED_AGENT_BROWSER {
            return Err(BrowserControlError::new(format!(
                "the agent-browser engine refuses {}: it is agent-browser {found}, and GitTerm pins {PINNED_AGENT_BROWSER}. Install agent-browser {PINNED_AGENT_BROWSER} and point agent_browser_path in {} at it",
                binary.display(),
                self.config_file.display()
            )));
        }
        self.prepare_state_dir()?;
        state.started = true;
        Ok(())
    }

    fn prepare_state_dir(&self) -> Result<()> {
        for dir in [
            self.state_dir.clone(),
            self.state_dir.join(SOCKET_DIR_NAME),
            self.state_dir.join(SCRATCH_DIR_NAME),
        ] {
            std::fs::create_dir_all(&dir).map_err(|error| {
                BrowserControlError::new(format!(
                    "could not create the agent-browser engine directory {}: {error}",
                    dir.display()
                ))
            })?;
            restrict_to_owner(&dir)?;
        }
        let write = |name: &str, contents: String| {
            let path = self.state_dir.join(name);
            std::fs::write(&path, contents).map_err(|error| {
                BrowserControlError::new(format!(
                    "could not write the agent-browser engine file {}: {error}",
                    path.display()
                ))
            })
        };
        // An empty config file: with --config, agent-browser reads neither
        // ~/.agent-browser/config.json nor a project's agent-browser.json.
        write(CLI_CONFIG_FILE, "{}\n".to_string())?;
        write(ACTION_POLICY_FILE, format!("{:#}\n", action_policy()))
    }

    // ----- running the CLI ----------------------------------------------

    fn global_args(&self, port: Option<u16>, session: &str) -> Vec<String> {
        let mut args = vec![
            "--config".to_string(),
            self.state_dir.join(CLI_CONFIG_FILE).display().to_string(),
            "--action-policy".to_string(),
            self.state_dir
                .join(ACTION_POLICY_FILE)
                .display()
                .to_string(),
        ];
        if let Some(port) = port {
            args.push("--cdp".to_string());
            args.push(port.to_string());
        }
        args.extend(
            [
                "--session",
                session,
                "--pin-tab",
                "--idle-timeout",
                SESSION_IDLE_TIMEOUT,
                "--json",
                "batch",
                "--bail",
                "--omit-command",
            ]
            .map(str::to_string),
        );
        args
    }

    /// Run `commands` in `session` as one batch.
    async fn run(
        &self,
        port: Option<u16>,
        session: &str,
        commands: &[AbCommand],
    ) -> std::result::Result<Vec<Value>, AbFailure> {
        if let Some(command) = commands
            .iter()
            .find(|command| !action_allowed(command.action()))
        {
            return Err(AbFailure::new(format!(
                "GitTerm does not run agent-browser action '{}'",
                command.action()
            )));
        }
        // Every batch first reads the streaming state: a session daemon
        // serves a loopback WebSocket stream by default, and serves it again
        // whenever it restarts (after an idle exit, say). Found on, it is
        // switched off right after this batch. (`stream disable` itself
        // fails when streaming is already off, so it cannot lead.)
        let mut batch = Vec::with_capacity(commands.len() + 1);
        batch.push(AbCommand::StreamStatus);
        batch.extend_from_slice(commands);
        let stdout = self.exec_batch(port, session, &batch).await?;
        if batch_found_streaming(&stdout) {
            let disabled = self
                .exec_batch(port, session, &[AbCommand::StreamDisable])
                .await
                .and_then(|stdout| parse_batch_output(&stdout, 1));
            if let Err(failure) = disabled {
                return Err(AbFailure {
                    message: format!(
                        "could not switch off session {session}'s stream server: {}",
                        failure.message
                    ),
                    code: failure.code,
                });
            }
        }
        let mut rows = parse_batch_output(&stdout, batch.len())?;
        rows.remove(0);
        Ok(rows)
    }

    /// One CLI process running `batch`, returning what it printed.
    async fn exec_batch(
        &self,
        port: Option<u16>,
        session: &str,
        batch: &[AbCommand],
    ) -> std::result::Result<String, AbFailure> {
        let binary = self
            .binary
            .as_ref()
            .map_err(|reason| AbFailure::new(reason.clone()))?;
        let mut command = cli_command(binary);
        command
            .env(
                "AGENT_BROWSER_SOCKET_DIR",
                self.state_dir.join(SOCKET_DIR_NAME),
            )
            .current_dir(&self.state_dir)
            .args(self.global_args(port, session))
            .stdin(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            AbFailure::new(format!(
                "could not run agent-browser at {}: {error}",
                binary.display()
            ))
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(batch_stdin(batch).as_bytes())
                .await
                .map_err(|error| {
                    AbFailure::new(format!("could not send commands to agent-browser: {error}"))
                })?;
        }
        let output = tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| {
                AbFailure::new(format!(
                    "agent-browser did not finish within {} seconds",
                    PROCESS_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| AbFailure::new(format!("agent-browser failed to run: {error}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.trim().is_empty() {
            return Err(AbFailure::new(format!(
                "agent-browser exited ({}) without output: {}",
                output.status,
                bounded_chars(
                    String::from_utf8_lossy(&output.stderr).trim(),
                    MAX_CLI_OUTPUT_CHARS
                )
            )));
        }
        Ok(stdout)
    }

    /// Run `commands` on `key`'s page. A pinned tab that is gone forgets
    /// the target, as the gitterm engine forgets a closed tab.
    async fn run_page(
        &self,
        state: &mut AbState,
        key: &TargetKey,
        page: &AbPage,
        commands: &[AbCommand],
        what: &str,
    ) -> Result<Vec<Value>> {
        match self.run(Some(page.port), &page.session, commands).await {
            Ok(rows) => Ok(rows),
            Err(failure) if failure.tab_gone() => {
                if let Some(target) = state.registry.remove(key) {
                    self.stop_session(None, &target.session).await;
                }
                Err(browser_target_not_found(&key.name))
            }
            Err(failure) => Err(BrowserControlError::new(format!(
                "agent-browser could not {what} in browser target '{}': {}",
                key.name, failure.message
            ))),
        }
    }

    /// Stop a session's daemon, logging (not hiding) a failure: the caller
    /// has already let the target go.
    async fn stop_session(&self, port: Option<u16>, session: &str) {
        if let Err(failure) = self.run(port, session, &[AbCommand::Close]).await {
            eprintln!(
                "agent-browser engine could not stop session {session}: {}",
                failure.message
            );
        }
    }

    // ----- targets ------------------------------------------------------

    /// Start the engine, find Chrome, and forget targets whose tab closed.
    async fn prepare(&self, state: &mut AbState) -> Result<(u16, Vec<DevToolsTarget>)> {
        self.ensure_started(state).await?;
        let port = self.browser.running_devtools_port().await?;
        let live = list_page_targets(port).await?;
        let stale: Vec<TargetKey> = state
            .registry
            .targets
            .iter()
            .filter(|(_, target)| {
                !live
                    .iter()
                    .any(|page| page.id == target.target_id && page.kind == "page")
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(target) = state.registry.remove(&key) {
                self.stop_session(Some(port), &target.session).await;
            }
        }
        Ok((port, live))
    }

    /// The page of `key`, opening the owner's default target on first use.
    /// Any other name the owner has not opened (another owner's included)
    /// is not found before Chrome or agent-browser is consulted.
    async fn page(&self, state: &mut AbState, key: &TargetKey) -> Result<AbPage> {
        if state.registry.get(key).is_none() && key.name != DEFAULT_BROWSER_TARGET_NAME {
            return Err(browser_target_not_found(&key.name));
        }
        let (port, _) = self.prepare(state).await?;
        if state.registry.get(key).is_none() {
            if key.name != DEFAULT_BROWSER_TARGET_NAME {
                return Err(browser_target_not_found(&key.name));
            }
            let (session, target_id) = self.open_session(state, port, "about:blank").await?;
            state.registry.targets.insert(
                key.clone(),
                AbTarget {
                    target_id,
                    session,
                    viewport: None,
                    refs: HashMap::new(),
                    har_recording: false,
                },
            );
        }
        state.registry.mark_active(key);
        let target = state
            .registry
            .get(key)
            .ok_or_else(|| browser_target_not_found(&key.name))?;
        Ok(AbPage {
            port,
            session: target.session.clone(),
            target_id: target.target_id.clone(),
        })
    }

    /// A new pinned session on a new tab showing `url`. agent-browser opens
    /// the tab itself when the session starts.
    async fn open_session(
        &self,
        state: &mut AbState,
        port: u16,
        url: &str,
    ) -> Result<(String, String)> {
        state.next_session += 1;
        let session = format!("{}-{}", self.session_prefix, state.next_session);
        let opened = self
            .run(
                Some(port),
                &session,
                &[AbCommand::Open {
                    url: url.to_string(),
                }],
            )
            .await
            .and_then(|rows| {
                rows[0]
                    .get("targetId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| AbFailure::new("agent-browser open omitted its targetId"))
            });
        match opened {
            Ok(target_id) => Ok((session, target_id)),
            Err(failure) => {
                let cleanup = match self
                    .run(Some(port), &session, &[AbCommand::TabCloseCurrent])
                    .await
                {
                    Ok(_) => "its tab was closed".to_string(),
                    Err(error) => format!("its tab may remain open ({})", error.message),
                };
                self.stop_session(Some(port), &session).await;
                Err(BrowserControlError::new(format!(
                    "agent-browser could not open a page at {url}: {}; {cleanup}",
                    failure.message
                )))
            }
        }
    }

    pub async fn launch(&self, options: BrowserLaunchOptions) -> Result<BrowserStatus> {
        let status = self.browser.launch(options).await?;
        self.ensure_started(&mut *self.state.lock().await).await?;
        Ok(status)
    }

    pub async fn status_for(&self, owner: &str) -> Result<BrowserStatus> {
        let mut status = self.browser.status().await?;
        let state = self.state.lock().await;
        let key = state.registry.key(owner, None)?;
        let managed = state.registry.get(&key);
        status.target_id = managed.map(|target| target.target_id.clone());
        status.active_target = managed.map(|_| key.name);
        Ok(status)
    }

    pub async fn targets(&self, owner: &str) -> Result<Vec<BrowserTargetInfo>> {
        let mut state = self.state.lock().await;
        let default_key = state
            .registry
            .key(owner, Some(DEFAULT_BROWSER_TARGET_NAME))?;
        let (port, mut live) = self.prepare(&mut state).await?;
        if !state.registry.owns_any(owner) {
            self.page(&mut state, &default_key).await?;
            live = list_page_targets(port).await?;
        }
        Ok(state.registry.infos(owner, &live))
    }

    pub async fn open_target(
        &self,
        owner: &str,
        name: &str,
        raw_url: &str,
    ) -> Result<BrowserTargetInfo> {
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, Some(name))?;
        let url = validate_navigation_url(raw_url)?;
        if state.registry.get(&key).is_some() {
            return Err(BrowserControlError::new(format!(
                "browser target '{}' already exists",
                key.name
            )));
        }
        let (port, _) = self.prepare(&mut state).await?;
        let (session, target_id) = self.open_session(&mut state, port, url.as_str()).await?;
        state.registry.targets.insert(
            key.clone(),
            AbTarget {
                target_id: target_id.clone(),
                session: session.clone(),
                viewport: None,
                refs: HashMap::new(),
                har_recording: false,
            },
        );
        state.registry.mark_active(&key);
        let page = AbPage {
            port,
            session,
            target_id: target_id.clone(),
        };
        self.run_page(
            &mut state,
            &key,
            &page,
            &[AbCommand::TabSwitch {
                target_id: target_id.clone(),
            }],
            "bring the new target forward",
        )
        .await?;
        Ok(BrowserTargetInfo {
            name: key.name,
            target_id,
            url: url.into(),
            title: String::new(),
            active: true,
        })
    }

    pub async fn focus_target(&self, owner: &str, name: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, Some(name))?;
        let page = self.page(&mut state, &key).await?;
        let target_id = page.target_id.clone();
        self.run_page(
            &mut state,
            &key,
            &page,
            &[AbCommand::TabSwitch { target_id }],
            "focus",
        )
        .await?;
        Ok(())
    }

    pub async fn close_target(&self, owner: &str, name: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, Some(name))?;
        let target = state
            .registry
            .get(&key)
            .ok_or_else(|| browser_target_not_found(&key.name))?;
        let (session, target_id) = (target.session.clone(), target.target_id.clone());
        self.ensure_started(&mut state).await?;
        let port = self.browser.running_devtools_port().await?;
        self.run(Some(port), &session, &[AbCommand::TabClose { target_id }])
            .await
            .map_err(|failure| {
                BrowserControlError::new(format!(
                    "agent-browser did not close browser target '{}': {}",
                    key.name, failure.message
                ))
            })?;
        state.registry.remove(&key);
        self.stop_session(Some(port), &session).await;
        Ok(())
    }

    /// An agent's disconnect (TRU-176): close the caller's own targets and
    /// stop their sessions; Chrome, other chats and the user's tabs stay.
    pub async fn disconnect_owner(&self, owner: &str) -> Result<BrowserOwnerDisconnect> {
        let mut state = self.state.lock().await;
        state.registry.key(owner, None)?;
        let browser_running = self.browser.status().await?.state == BrowserState::Running;
        let port = if browser_running && state.started {
            Some(self.prepare(&mut state).await?.0)
        } else {
            None
        };
        let mut keys = state.registry.owned_keys(owner);
        keys.sort_by(|left, right| left.name.cmp(&right.name));
        let mut closed_targets = Vec::new();
        let mut failures = Vec::new();
        for key in keys {
            let Some(target) = state.registry.remove(&key) else {
                continue;
            };
            if port.is_some() {
                match self
                    .run(
                        port,
                        &target.session,
                        &[AbCommand::TabClose {
                            target_id: target.target_id.clone(),
                        }],
                    )
                    .await
                {
                    Ok(_) => closed_targets.push(key.name),
                    Err(failure) => failures.push(format!("'{}': {}", key.name, failure.message)),
                }
            } else {
                closed_targets.push(key.name);
            }
            self.stop_session(port, &target.session).await;
        }
        if failures.is_empty() {
            Ok(BrowserOwnerDisconnect {
                closed_targets,
                browser_running,
            })
        } else {
            Err(BrowserControlError::new(format!(
                "disconnected this chat from the browser, but could not close its targets {}; they stay open as the user's tabs",
                failures.join("; ")
            )))
        }
    }

    /// Chats that closed give up their targets (TRU-176). Their tabs stay
    /// open as the user's; their sessions stop.
    pub async fn release_owners(&self, owners: &HashSet<String>) -> Result<()> {
        let released = self.state.lock().await.registry.release(owners);
        let port = if released.is_empty() {
            None
        } else {
            self.browser.running_devtools_port().await.ok()
        };
        let mut failures = Vec::new();
        for (key, target) in released {
            if let Err(failure) = self.run(port, &target.session, &[AbCommand::Close]).await {
                failures.push(format!("'{}': {}", key.name, failure.message));
            }
        }
        if let Err(error) = self.browser.release_owners(owners).await {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(BrowserControlError::new(format!(
                "released the closed chats' browser targets, but could not stop their agent-browser sessions {}",
                failures.join("; ")
            )))
        }
    }

    // ----- page operations ----------------------------------------------

    pub async fn navigate_in(
        &self,
        owner: &str,
        target: Option<&str>,
        raw_url: &str,
    ) -> Result<NavigationResult> {
        let url = validate_navigation_url(raw_url)?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        self.run_page(
            &mut state,
            &key,
            &page,
            &[AbCommand::Open {
                url: url.to_string(),
            }],
            "navigate",
        )
        .await?;
        if let Some(target) = state.registry.targets.get_mut(&key) {
            target.refs.clear();
        }
        // A page target's main frame id is its target id.
        Ok(NavigationResult {
            url: url.into(),
            frame_id: page.target_id,
            loader_id: None,
        })
    }

    pub async fn capture(
        &self,
        owner: &str,
        target: Option<&str>,
        label: &str,
        capture_mode: BrowserCaptureMode,
    ) -> Result<BrowserSnapshot> {
        let label = validate_evidence_label(label)?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        self.capture_labelled(&mut state, owner, &key, &page, label, capture_mode)
            .await
    }

    /// A labelled capture of an already-resolved page, retained as evidence.
    /// browser_capture and a labelled browser_snapshot_refs both land here.
    async fn capture_labelled(
        &self,
        state: &mut AbState,
        owner: &str,
        key: &TargetKey,
        page: &AbPage,
        label: &str,
        capture_mode: BrowserCaptureMode,
    ) -> Result<BrowserSnapshot> {
        let full_page = capture_mode == BrowserCaptureMode::FullPage;
        let mut reads = vec![AbCommand::Eval {
            script: snapshot_expression(),
        }];
        if full_page {
            reads.push(AbCommand::Eval {
                script: FULL_PAGE_SIZE_EXPRESSION.to_string(),
            });
        }
        let rows = self
            .run_page(state, key, page, &reads, "read the page")
            .await?;
        let page_state: SnapshotState = eval_value(&rows[0], "snapshot")?;
        if full_page {
            check_full_page_size(&eval_value(&rows[1], "full-page size")?)?;
        }
        let shot = ScratchFile::new(&self.state_dir, "png");
        let rows = self
            .run_page(
                state,
                key,
                page,
                &[
                    AbCommand::Screenshot {
                        path: shot.0.clone(),
                        full_page,
                    },
                    AbCommand::Snapshot { interactive: false },
                    AbCommand::Console,
                    AbCommand::Errors,
                    AbCommand::NetworkRequests,
                ],
                "capture",
            )
            .await?;
        let screenshot_png = shot.read("screenshot")?;
        let accessibility = rows[1]
            .get("snapshot")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut console_errors = parse_console_errors(&rows[2]);
        console_errors.extend(parse_page_errors(&rows[3]));
        keep_last(&mut console_errors, MAX_CONSOLE_ERRORS);
        let viewport = state
            .registry
            .get(key)
            .and_then(|target| target.viewport)
            .unwrap_or(page_state.viewport);
        let snapshot = BrowserSnapshot {
            target: key.name.clone(),
            capture_mode,
            url: page_state.url,
            title: page_state.title,
            loading_state: page_state.loading_state,
            visible_text: page_state.visible_text,
            interactive_elements: page_state.interactive_elements,
            viewport,
            console_errors,
            failed_requests: parse_failed_requests(&rows[4]),
            screenshot_png,
        };
        self.record_capture(state, owner, label, &snapshot, accessibility)
            .await;
        Ok(snapshot)
    }

    /// Retain a labelled capture as evidence, with its accessibility text as
    /// the snapshot-diff baseline when small enough. Pruned with the evidence
    /// set.
    async fn record_capture(
        &self,
        state: &mut AbState,
        owner: &str,
        label: &str,
        snapshot: &BrowserSnapshot,
        accessibility: &str,
    ) {
        let evidence_id = self.browser.record_evidence(owner, label, snapshot).await;
        if accessibility.chars().count() <= MAX_BASELINE_SNAPSHOT_CHARS {
            state
                .baselines
                .insert(evidence_id, Arc::new(accessibility.to_string()));
        }
        let retained = self.browser.retained_evidence_ids().await;
        state.baselines.retain(|id, _| retained.contains(id));
    }

    pub async fn click_in(
        &self,
        owner: &str,
        target: Option<&str>,
        locator: &BrowserLocator,
    ) -> Result<BrowserActionOutcome> {
        locator.validate()?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        if let BrowserLocator::Ref { reference } = locator {
            let reference = snapshot_ref(reference)?.to_string();
            let action = known_ref(&state, &key, &reference)?;
            self.run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::ClickRef { reference }],
                "click",
            )
            .await?;
            return Ok(BrowserActionOutcome::Ref(action));
        }
        let element = self
            .resolve_locator(&mut state, &key, &page, locator, LocatorPreparation::Point)
            .await?;
        self.run_page(
            &mut state,
            &key,
            &page,
            &[
                AbCommand::MouseMove {
                    x: element.x,
                    y: element.y,
                },
                AbCommand::MouseDown,
                AbCommand::MouseUp,
            ],
            "click",
        )
        .await?;
        Ok(BrowserActionOutcome::Element(element))
    }

    pub async fn type_text_in(
        &self,
        owner: &str,
        target: Option<&str>,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionOutcome> {
        locator.validate()?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        if let BrowserLocator::Ref { reference } = locator {
            let reference = snapshot_ref(reference)?.to_string();
            let action = known_ref(&state, &key, &reference)?;
            self.run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::FillRef {
                    reference,
                    text: text.to_string(),
                }],
                "type",
            )
            .await?;
            return Ok(BrowserActionOutcome::Ref(action));
        }
        // GitTerm's locator focuses the one editable element and selects its
        // contents; the inserted text then replaces them.
        let element = self
            .resolve_locator(
                &mut state,
                &key,
                &page,
                locator,
                LocatorPreparation::ReplaceText,
            )
            .await?;
        self.run_page(
            &mut state,
            &key,
            &page,
            &[AbCommand::InsertText {
                text: text.to_string(),
            }],
            "type",
        )
        .await?;
        Ok(BrowserActionOutcome::Element(element))
    }

    async fn resolve_locator(
        &self,
        state: &mut AbState,
        key: &TargetKey,
        page: &AbPage,
        locator: &BrowserLocator,
        preparation: LocatorPreparation,
    ) -> Result<crate::browser_control::BrowserActionTarget> {
        let script = locator_expression(locator, preparation)?;
        let rows = self
            .run_page(
                state,
                key,
                page,
                &[AbCommand::Eval { script }],
                "resolve the locator",
            )
            .await?;
        let resolution: LocatorResolution = eval_value(&rows[0], "locator resolution")?;
        action_target_from_resolution(resolution)
    }

    pub async fn press_in(&self, owner: &str, target: Option<&str>, key: BrowserKey) -> Result<()> {
        let mut state = self.state.lock().await;
        let target_key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &target_key).await?;
        self.run_page(
            &mut state,
            &target_key,
            &page,
            &[AbCommand::Press { key: key_name(key) }],
            "press a key",
        )
        .await?;
        Ok(())
    }

    /// Scroll with a wheel event at the viewport centre, as the gitterm
    /// engine does.
    pub async fn scroll_in(
        &self,
        owner: &str,
        target: Option<&str>,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<()> {
        validate_scroll_deltas(delta_x, delta_y)?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        let rows = self
            .run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::Eval {
                    script: "({ x: window.innerWidth / 2, y: window.innerHeight / 2 })".to_string(),
                }],
                "find the viewport centre",
            )
            .await?;
        let center: ViewportCenter = eval_value(&rows[0], "viewport centre")?;
        self.run_page(
            &mut state,
            &key,
            &page,
            &[
                AbCommand::MouseMove {
                    x: center.x,
                    y: center.y,
                },
                AbCommand::MouseWheel { delta_y, delta_x },
            ],
            "scroll",
        )
        .await?;
        Ok(())
    }

    pub async fn resize_in(
        &self,
        owner: &str,
        target: Option<&str>,
        viewport: BrowserViewport,
    ) -> Result<BrowserViewport> {
        let viewport = viewport.validate()?;
        if viewport.mobile {
            return Err(BrowserControlError::new(
                "the agent-browser engine emulates mobile devices through browser_viewport's device presets; browser_resize sets desktop metrics (mobile: false) only",
            ));
        }
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        let rows = self
            .run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::SetViewport {
                    width: viewport.width,
                    height: viewport.height,
                    scale: viewport.device_scale_factor,
                }],
                "resize",
            )
            .await?;
        let applied = parse_viewport(&rows[0]).unwrap_or(viewport);
        if let Some(target) = state.registry.targets.get_mut(&key) {
            target.viewport = Some(applied);
        }
        Ok(applied)
    }

    pub async fn reload_in(
        &self,
        owner: &str,
        target: Option<&str>,
        ignore_cache: bool,
    ) -> Result<()> {
        if ignore_cache {
            return Err(BrowserControlError::new(
                "agent-browser 0.41.0 cannot reload without the cache; reload with ignore_cache false",
            ));
        }
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        self.run_page(&mut state, &key, &page, &[AbCommand::Reload], "reload")
            .await?;
        if let Some(target) = state.registry.targets.get_mut(&key) {
            target.refs.clear();
        }
        Ok(())
    }

    pub async fn wait_for_in(
        &self,
        owner: &str,
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
        let predicate = wait_condition_predicate(condition)?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        let timeout_ms = u64::try_from(max_wait.as_millis()).unwrap_or(u64::MAX);
        match self
            .run(
                Some(page.port),
                &page.session,
                &[AbCommand::WaitFn {
                    predicate,
                    timeout_ms,
                }],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(failure) if failure.tab_gone() => {
                if let Some(target) = state.registry.remove(&key) {
                    self.stop_session(None, &target.session).await;
                }
                Err(browser_target_not_found(&key.name))
            }
            Err(failure) => Err(BrowserControlError::new(format!(
                "browser wait condition was not met within {timeout_ms} ms (agent-browser: {})",
                failure.message
            ))),
        }
    }

    pub async fn diagnostics_in(
        &self,
        owner: &str,
        target: Option<&str>,
    ) -> Result<BrowserDiagnostics> {
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        let rows = self
            .run_page(
                &mut state,
                &key,
                &page,
                &[
                    AbCommand::Console,
                    AbCommand::Errors,
                    AbCommand::NetworkRequests,
                ],
                "read diagnostics",
            )
            .await?;
        let mut console_errors = parse_console_errors(&rows[0]);
        console_errors.extend(parse_page_errors(&rows[1]));
        keep_last(&mut console_errors, MAX_CONSOLE_ERRORS);
        Ok(BrowserDiagnostics {
            console_errors,
            failed_requests: parse_failed_requests(&rows[2]),
        })
    }

    /// GitTerm's own outline JavaScript, run through the engine's eval, so
    /// the output matches the gitterm engine's exactly.
    pub async fn dom_outline_in(
        &self,
        owner: &str,
        target: Option<&str>,
    ) -> Result<BrowserDomOutline> {
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let script = dom_outline_expression(&key.name)?;
        let page = self.page(&mut state, &key).await?;
        let rows = self
            .run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::Eval { script }],
                "outline the DOM",
            )
            .await?;
        let mut outline: BrowserDomOutline = eval_value(&rows[0], "DOM outline")?;
        outline.url =
            sanitize_diagnostic_url(&outline.url).unwrap_or_else(|| "URL unavailable".to_string());
        Ok(outline)
    }

    pub async fn node_details_in(
        &self,
        owner: &str,
        target: Option<&str>,
        selector: &BrowserNodeSelector,
    ) -> Result<BrowserNodeDetails> {
        selector.validate()?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let script = node_details_expression(&key.name, selector)?;
        let page = self.page(&mut state, &key).await?;
        let rows = self
            .run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::Eval { script }],
                "inspect the node",
            )
            .await?;
        let evaluation: NodeDetailsEvaluation = eval_value(&rows[0], "DOM node details")?;
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

    // ----- agent-browser-only tools -------------------------------------

    pub async fn snapshot_refs_in(
        &self,
        owner: &str,
        target: Option<&str>,
        label: Option<&str>,
    ) -> Result<BrowserSnapshotRefs> {
        let label = label.map(validate_evidence_label).transpose()?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        // A label retains this page state as a diff baseline, exactly as a
        // labelled browser_capture does (viewport screenshot included). It
        // runs first so the interactive snapshot below is the last one taken
        // and its refs are the live ones.
        let evidence_label = match label {
            Some(label) => {
                self.capture_labelled(
                    &mut state,
                    owner,
                    &key,
                    &page,
                    label,
                    BrowserCaptureMode::Viewport,
                )
                .await?;
                Some(label.to_string())
            }
            None => None,
        };
        let rows = self
            .run_page(
                &mut state,
                &key,
                &page,
                &[AbCommand::Snapshot { interactive: true }],
                "snapshot",
            )
            .await?;
        let data = &rows[0];
        let full = data
            .get("snapshot")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let refs = parse_snapshot_refs(data);
        let ref_count = refs.len();
        if let Some(target) = state.registry.targets.get_mut(&key) {
            target.refs = refs;
        }
        let total_chars = full.chars().count();
        let (snapshot, truncated) = cap_text(full, MAX_SNAPSHOT_REFS_CHARS);
        Ok(BrowserSnapshotRefs {
            target: key.name,
            evidence_label,
            url: data
                .get("origin")
                .and_then(Value::as_str)
                .and_then(sanitize_diagnostic_url),
            snapshot,
            ref_count,
            total_chars,
            truncated,
        })
    }

    pub async fn diff_in(
        &self,
        owner: &str,
        target: Option<&str>,
        baseline_label: &str,
        baseline_target: Option<&str>,
        mode: BrowserDiffMode,
    ) -> Result<BrowserDiff> {
        let label = validate_evidence_label(baseline_label)?.to_string();
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let baseline_target = match baseline_target {
            Some(name) => validate_browser_target_name(name)?.to_string(),
            None => key.name.clone(),
        };
        let evidence = self
            .browser
            .owned_evidence(owner, &baseline_target, &label)
            .await
            .ok_or_else(|| {
                BrowserControlError::new(format!(
                    "this chat has no evidence labelled '{label}' for browser target '{baseline_target}'; capture the baseline with browser_capture first"
                ))
            })?;
        let page = self.page(&mut state, &key).await?;
        let mut diff = BrowserDiff {
            target: key.name.clone(),
            baseline_target,
            baseline_label: label,
            mode,
            changed: false,
            additions: None,
            removals: None,
            unchanged: None,
            diff: None,
            diff_truncated: None,
            mismatch_percentage: None,
            different_pixels: None,
            total_pixels: None,
            dimension_mismatch: None,
            regions: None,
            region_count: None,
            regions_unavailable: None,
            diff_png: None,
        };
        match mode {
            BrowserDiffMode::Snapshot => {
                let text = state.baselines.get(&evidence.id).cloned().ok_or_else(|| {
                    BrowserControlError::new(format!(
                        "evidence '{}' has no accessibility snapshot to diff against (it was captured by the gitterm engine, or its snapshot was too large); capture it again",
                        diff.baseline_label
                    ))
                })?;
                let baseline = ScratchFile::new(&self.state_dir, "txt");
                baseline.write(text.as_bytes(), "snapshot baseline")?;
                let rows = self
                    .run_page(
                        &mut state,
                        &key,
                        &page,
                        &[AbCommand::DiffSnapshot {
                            baseline: baseline.0.clone(),
                        }],
                        "diff the snapshot",
                    )
                    .await?;
                apply_snapshot_diff(&mut diff, &rows[0]);
            }
            BrowserDiffMode::Screenshot => {
                let baseline = ScratchFile::new(&self.state_dir, "png");
                baseline.write(&evidence.screenshot_png, "screenshot baseline")?;
                let output = ScratchFile::new(&self.state_dir, "png");
                let current = ScratchFile::new(&self.state_dir, "png");
                let full_page = evidence.capture_mode == BrowserCaptureMode::FullPage;
                let rows = self
                    .run_page(
                        &mut state,
                        &key,
                        &page,
                        &[
                            AbCommand::DiffScreenshot {
                                baseline: baseline.0.clone(),
                                output: output.0.clone(),
                                full_page,
                            },
                            AbCommand::Screenshot {
                                path: current.0.clone(),
                                full_page,
                            },
                        ],
                        "diff the screenshot",
                    )
                    .await?;
                apply_screenshot_diff(&mut diff, &rows[0]);
                if diff.changed && diff.dimension_mismatch.is_none() {
                    let current_png = current.read("current screenshot")?;
                    match changed_regions(
                        &evidence.screenshot_png,
                        &current_png,
                        evidence.viewport.device_scale_factor,
                    ) {
                        Ok((regions, count)) => {
                            diff.regions = Some(regions);
                            diff.region_count = Some(count);
                        }
                        Err(reason) => diff.regions_unavailable = Some(reason),
                    }
                } else if diff.changed {
                    diff.regions_unavailable = Some(
                        "the screenshots differ in size, so there is no pixel-for-pixel comparison"
                            .into(),
                    );
                } else {
                    diff.regions = Some(Vec::new());
                    diff.region_count = Some(0);
                }
                if output.0.exists() {
                    diff.diff_png = Some(output.read("diff image")?);
                }
            }
        }
        Ok(diff)
    }

    pub async fn network_capture_in(
        &self,
        owner: &str,
        target: Option<&str>,
        action: BrowserNetworkCaptureAction,
    ) -> Result<BrowserNetworkCapture> {
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        let recording = state
            .registry
            .get(&key)
            .is_some_and(|target| target.har_recording);
        match action {
            BrowserNetworkCaptureAction::Start => {
                if recording {
                    return Err(BrowserControlError::new(format!(
                        "browser target '{}' is already recording; stop it first",
                        key.name
                    )));
                }
                self.run_page(
                    &mut state,
                    &key,
                    &page,
                    &[AbCommand::HarStart],
                    "start recording",
                )
                .await?;
                if let Some(target) = state.registry.targets.get_mut(&key) {
                    target.har_recording = true;
                }
                Ok(BrowserNetworkCapture {
                    target: key.name,
                    recording: true,
                    entries: Vec::new(),
                    entry_count: 0,
                    truncated: false,
                })
            }
            BrowserNetworkCaptureAction::Stop => {
                if !recording {
                    return Err(BrowserControlError::new(format!(
                        "browser target '{}' is not recording; start a capture first",
                        key.name
                    )));
                }
                let har = ScratchFile::new(&self.state_dir, "har");
                let stopped = self
                    .run_page(
                        &mut state,
                        &key,
                        &page,
                        &[AbCommand::HarStop {
                            path: har.0.clone(),
                        }],
                        "stop recording",
                    )
                    .await;
                if let Some(target) = state.registry.targets.get_mut(&key) {
                    target.har_recording = false;
                }
                stopped?;
                let bytes = har.read("HAR")?;
                let parsed: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    BrowserControlError::new(format!("agent-browser wrote an invalid HAR: {error}"))
                })?;
                let (entries, entry_count) = summarize_har(&parsed);
                Ok(BrowserNetworkCapture {
                    target: key.name,
                    recording: false,
                    truncated: entry_count > entries.len(),
                    entries,
                    entry_count,
                })
            }
        }
    }

    pub async fn viewport_in(
        &self,
        owner: &str,
        target: Option<&str>,
        request: &BrowserViewportRequest,
    ) -> Result<BrowserViewport> {
        let command = viewport_command(request)?;
        let mut state = self.state.lock().await;
        let key = state.registry.key(owner, target)?;
        let page = self.page(&mut state, &key).await?;
        let rows = self
            .run_page(&mut state, &key, &page, &[command], "set the viewport")
            .await?;
        let applied = parse_viewport(&rows[0]).ok_or_else(|| {
            BrowserControlError::new("agent-browser did not report the viewport it applied")
        })?;
        if let Some(target) = state.registry.targets.get_mut(&key) {
            target.viewport = Some(applied);
        }
        Ok(applied)
    }

    /// Test seam: claim a target for `owner` without Chrome or the CLI.
    #[cfg(test)]
    pub(crate) async fn insert_test_target(&self, owner: &str, name: &str, target_id: &str) {
        let mut state = self.state.lock().await;
        let key = TargetKey::new(owner, name);
        state.registry.targets.insert(
            key.clone(),
            AbTarget {
                target_id: target_id.to_string(),
                session: format!("{}-test", self.session_prefix),
                viewport: None,
                refs: HashMap::new(),
                har_recording: false,
            },
        );
        state.registry.mark_active(&key);
    }
}

/// The CLI with none of the caller's `AGENT_BROWSER_*` settings, so only
/// GitTerm's flags configure it.
fn cli_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("AGENT_BROWSER_") {
            command.env_remove(name);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn binary_version(binary: &Path) -> Result<String> {
    if !binary.is_file() {
        return Err(BrowserControlError::new(format!(
            "the agent-browser engine cannot start: there is no agent-browser binary at {}. GitTerm pins agent-browser {PINNED_AGENT_BROWSER}",
            binary.display()
        )));
    }
    let mut command = cli_command(binary);
    command.arg("--version");
    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            BrowserControlError::new(format!(
                "{} --version did not answer within {} seconds",
                binary.display(),
                VERSION_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| {
            BrowserControlError::new(format!(
                "could not run {} --version: {error}",
                binary.display()
            ))
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_version(&stdout) {
        Some(version) if output.status.success() => Ok(version.to_string()),
        _ => Err(BrowserControlError::new(format!(
            "{} --version did not report an agent-browser version (exit {}): {}",
            binary.display(),
            output.status,
            bounded_chars(
                format!(
                    "{}{}",
                    stdout.trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .as_str(),
                MAX_CLI_OUTPUT_CHARS
            )
        ))),
    }
}

#[cfg(unix)]
fn restrict_to_owner(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        BrowserControlError::new(format!(
            "could not restrict {} to its owner: {error}",
            dir.display()
        ))
    })
}

#[cfg(not(unix))]
fn restrict_to_owner(_dir: &Path) -> Result<()> {
    Ok(())
}

/// The value an `eval` returned.
fn eval_value<T: DeserializeOwned>(data: &Value, what: &str) -> Result<T> {
    let value = data.get("result").cloned().ok_or_else(|| {
        BrowserControlError::new(format!("agent-browser eval omitted the {what} data"))
    })?;
    serde_json::from_value(value).map_err(|error| {
        BrowserControlError::new(format!("the page returned malformed {what} data: {error}"))
    })
}

/// The document's full size, checked against the gitterm engine's limits
/// before a full-page screenshot.
const FULL_PAGE_SIZE_EXPRESSION: &str = "(() => { const root = document.documentElement; const body = document.body; return { width: Math.max(root.scrollWidth, body ? body.scrollWidth : 0), height: Math.max(root.scrollHeight, body ? body.scrollHeight : 0), scale: window.devicePixelRatio || 1 }; })()";

#[derive(Debug, Deserialize)]
struct PageSize {
    width: f64,
    height: f64,
    scale: f64,
}

fn check_full_page_size(size: &PageSize) -> Result<()> {
    let PageSize {
        width,
        height,
        scale,
    } = *size;
    if !width.is_finite()
        || !height.is_finite()
        || !scale.is_finite()
        || width <= 0.0
        || height <= 0.0
        || scale <= 0.0
    {
        return Err(BrowserControlError::new(format!(
            "the page reported invalid full-page dimensions {width}x{height} at {scale}x scale"
        )));
    }
    if width > MAX_FULL_PAGE_CAPTURE_WIDTH
        || height > MAX_FULL_PAGE_CAPTURE_HEIGHT
        || width * height * scale * scale > MAX_FULL_PAGE_CAPTURE_PIXELS
    {
        return Err(BrowserControlError::new(format!(
            "full-page capture {width:.0}x{height:.0} exceeds the safe evidence limit"
        )));
    }
    Ok(())
}

/// A ref from the target's latest browser_snapshot_refs.
fn known_ref(state: &AbState, key: &TargetKey, reference: &str) -> Result<BrowserRefAction> {
    let entry = state
        .registry
        .get(key)
        .and_then(|target| target.refs.get(reference))
        .ok_or_else(|| {
            BrowserControlError::new(format!(
                "ref {reference} is not in browser target '{}''s latest browser_snapshot_refs output; take a fresh one",
                key.name
            ))
        })?;
    Ok(BrowserRefAction {
        reference: reference.to_string(),
        role: entry.role.clone(),
        name: entry.name.clone(),
    })
}

fn key_name(key: BrowserKey) -> &'static str {
    match key {
        BrowserKey::Enter => "Enter",
        BrowserKey::Tab => "Tab",
        BrowserKey::Escape => "Escape",
        BrowserKey::Backspace => "Backspace",
        BrowserKey::Delete => "Delete",
        BrowserKey::ArrowUp => "ArrowUp",
        BrowserKey::ArrowDown => "ArrowDown",
        BrowserKey::ArrowLeft => "ArrowLeft",
        BrowserKey::ArrowRight => "ArrowRight",
        BrowserKey::Home => "Home",
        BrowserKey::End => "End",
        BrowserKey::PageUp => "PageUp",
        BrowserKey::PageDown => "PageDown",
        BrowserKey::Space => "Space",
    }
}

/// `set viewport` / `set device` report the metrics they applied.
fn parse_viewport(data: &Value) -> Option<BrowserViewport> {
    Some(BrowserViewport {
        width: u32::try_from(data.get("width")?.as_u64()?).ok()?,
        height: u32::try_from(data.get("height")?.as_u64()?).ok()?,
        device_scale_factor: data.get("deviceScaleFactor")?.as_f64()?,
        mobile: data.get("mobile")?.as_bool()?,
    })
}

/// browser_viewport's command: a device preset by name, or a size.
pub(crate) fn viewport_command(request: &BrowserViewportRequest) -> Result<AbCommand> {
    match (&request.device, request.width, request.height) {
        (Some(device), None, None) if request.device_scale_factor.is_none() => {
            let name = device.trim();
            let valid = !name.is_empty()
                && name.chars().count() <= MAX_DEVICE_NAME_CHARS
                && name.starts_with(|c: char| c.is_ascii_alphanumeric())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ');
            if !valid {
                return Err(BrowserControlError::new(format!(
                    "browser viewport device must be a device name such as \"iPhone 15\" or \"Pixel 9\" (letters, digits and spaces, up to {MAX_DEVICE_NAME_CHARS}), got {device:?}"
                )));
            }
            Ok(AbCommand::SetDevice {
                name: name.to_string(),
            })
        }
        (None, Some(width), Some(height)) => {
            let viewport = BrowserViewport {
                width,
                height,
                device_scale_factor: request.device_scale_factor.unwrap_or(1.0),
                mobile: false,
            }
            .validate()?;
            Ok(AbCommand::SetViewport {
                width: viewport.width,
                height: viewport.height,
                scale: viewport.device_scale_factor,
            })
        }
        _ => Err(BrowserControlError::new(
            "browser viewport takes either a device name, or a width and height (with an optional device_scale_factor)",
        )),
    }
}

fn apply_snapshot_diff(diff: &mut BrowserDiff, data: &Value) {
    diff.changed = data
        .get("changed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    diff.additions = data.get("additions").and_then(Value::as_u64);
    diff.removals = data.get("removals").and_then(Value::as_u64);
    diff.unchanged = data.get("unchanged").and_then(Value::as_u64);
    let (text, truncated) = cap_text(
        data.get("diff").and_then(Value::as_str).unwrap_or_default(),
        MAX_DIFF_CHARS,
    );
    diff.diff = Some(text);
    diff.diff_truncated = Some(truncated);
}

fn apply_screenshot_diff(diff: &mut BrowserDiff, data: &Value) {
    diff.changed = data.get("match").and_then(Value::as_bool) == Some(false);
    diff.mismatch_percentage = data.get("mismatchPercentage").and_then(Value::as_f64);
    diff.different_pixels = data.get("differentPixels").and_then(Value::as_u64);
    diff.total_pixels = data.get("totalPixels").and_then(Value::as_u64);
    diff.dimension_mismatch = data
        .get("dimensionMismatch")
        .filter(|value| !value.is_null())
        .cloned();
}

/// Where two same-size PNG screenshots differ: nearby changed pixels merge
/// into regions (bounding boxes in page CSS pixels, `scale` device pixels to
/// one), the largest first, capped at `MAX_DIFF_REGIONS`. Also returns how
/// many regions there are in all. Memory only; nothing is written.
fn changed_regions(
    baseline_png: &[u8],
    current_png: &[u8],
    scale: f64,
) -> std::result::Result<(Vec<BrowserDiffRegion>, usize), String> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let decode = |bytes: &[u8], what: &str| {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .map(|decoded| decoded.into_rgba8())
            .map_err(|error| format!("could not decode the {what} screenshot: {error}"))
    };
    let (bw, bh) = png_dimensions(baseline_png, "baseline")?;
    let (cw, ch) = png_dimensions(current_png, "current")?;
    if (bw, bh) != (cw, ch) {
        return Err(format!(
            "the screenshots differ in size ({bw}x{bh} against {cw}x{ch})"
        ));
    }
    if u64::from(bw) * u64::from(bh) > MAX_DIFF_REGION_PIXELS {
        return Err(format!(
            "the screenshots are too large ({bw}x{bh}) to locate regions"
        ));
    }
    let before = decode(baseline_png, "baseline")?;
    let after = decode(current_png, "current")?;

    let cell = DIFF_REGION_CELL;
    let cols = bw.div_ceil(cell) as usize;
    let rows = bh.div_ceil(cell) as usize;
    // Per cell: the changed pixels' bounds (x0, y0, x1, y1), if any.
    let mut cells: Vec<Option<(u32, u32, u32, u32)>> = vec![None; cols * rows];
    for (x, y, a) in before.enumerate_pixels() {
        let b = after.get_pixel(x, y);
        let changed =
            a.0.iter()
                .zip(b.0.iter())
                .any(|(p, q)| p.abs_diff(*q) > DIFF_PIXEL_TOLERANCE);
        if changed {
            let slot = &mut cells[(y / cell) as usize * cols + (x / cell) as usize];
            *slot = Some(match *slot {
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                None => (x, y, x, y),
            });
        }
    }

    // Changed cells that touch (8 neighbours) are one region.
    let mut seen = vec![false; cells.len()];
    let mut regions: Vec<(u32, u32, u32, u32)> = Vec::new();
    for start in 0..cells.len() {
        if seen[start] || cells[start].is_none() {
            continue;
        }
        seen[start] = true;
        let mut bounds = cells[start].unwrap_or_default();
        let mut stack = vec![start];
        while let Some(index) = stack.pop() {
            let (cx, cy) = (index % cols, index / cols);
            for ny in cy.saturating_sub(1)..=(cy + 1).min(rows - 1) {
                for nx in cx.saturating_sub(1)..=(cx + 1).min(cols - 1) {
                    let next = ny * cols + nx;
                    if let (false, Some(cell_bounds)) = (seen[next], cells[next]) {
                        seen[next] = true;
                        bounds = (
                            bounds.0.min(cell_bounds.0),
                            bounds.1.min(cell_bounds.1),
                            bounds.2.max(cell_bounds.2),
                            bounds.3.max(cell_bounds.3),
                        );
                        stack.push(next);
                    }
                }
            }
        }
        regions.push(bounds);
    }

    let count = regions.len();
    let mut boxes: Vec<BrowserDiffRegion> = regions
        .into_iter()
        .map(|(x0, y0, x1, y1)| {
            let left = (f64::from(x0) / scale).floor() as u32;
            let top = (f64::from(y0) / scale).floor() as u32;
            let right = (f64::from(x1 + 1) / scale).ceil() as u32;
            let bottom = (f64::from(y1 + 1) / scale).ceil() as u32;
            BrowserDiffRegion {
                x: left,
                y: top,
                width: right.saturating_sub(left).max(1),
                height: bottom.saturating_sub(top).max(1),
            }
        })
        .collect();
    boxes.sort_by_key(|region| {
        (
            std::cmp::Reverse(u64::from(region.width) * u64::from(region.height)),
            region.y,
            region.x,
        )
    });
    boxes.truncate(MAX_DIFF_REGIONS);
    Ok((boxes, count))
}

fn png_dimensions(bytes: &[u8], what: &str) -> std::result::Result<(u32, u32), String> {
    image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png)
        .into_dimensions()
        .map_err(|error| format!("could not read the {what} screenshot: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Fixtures recorded from agent-browser 0.41.0 attached to Chrome 156
    // (neutral local test pages; lifecycle blobs trimmed).
    const BATCH_OPEN_AND_STREAM: &str = r#"[{"error":null,"index":0,"result":{"targetId":"6DC82394E6AE3613A31756D5A65F00FB","title":"Fixture Page","url":"http://127.0.0.1:18765/index.html"},"success":true},{"error":null,"index":1,"result":{"disabled":true},"success":true}]"#;
    const BATCH_TAB_GONE: &str = r#"[{"code":"tab_gone","error":"tab_gone: bound tab is gone (target 61EDD514B2011983462908D0BB07BC54, last url http://127.0.0.1:18765/index.html). Run `agent-browser tab new <url>` to bind a new tab, or `agent-browser tab list` to pick an existing one","index":0,"result":{"lastUrl":"http://127.0.0.1:18765/index.html","targetId":"61EDD514B2011983462908D0BB07BC54"},"success":false}]"#;
    const BATCH_DENIED: &str = r#"[{"error":"Action 'cookies_get' denied by policy: Action 'cookies_get' is not in the allow list","index":0,"result":null,"success":false}]"#;
    const POLICY_LOAD_FAILURE: &str = r#"{"error":"Action 'launch' denied by policy: action policy failed to load: Invalid policy JSON","success":false}"#;
    const CONSOLE: &str = r#"{"messages":[{"args":[{"type":"string","value":"hello from button"}],"text":"hello from button","type":"log"},{"args":[{"type":"string","value":"fixture console error"}],"text":"fixture console error","type":"error"},{"args":[{"type":"string","value":"assert msg"}],"text":"assert msg","type":"assert"},{"args":[{"type":"string","value":"warned"}],"text":"warned","type":"warning"}]}"#;
    const ERRORS: &str = r#"{"errors":[{"column":480,"line":4,"text":"Error: boom from page\n    at http://127.0.0.1:18765/index.html:5:481","url":null}]}"#;
    const REQUESTS: &str = r#"{"requests":[{"headers":{"Cookie":"session=secret","User-Agent":"Mozilla/5.0"},"method":"GET","mimeType":"text/html","requestId":"DDE5CB2F4D0A6813B0FEBD97A3ECCAFD","resourceType":"Document","responseHeaders":{"Set-Cookie":"session=secret"},"status":200,"timestamp":1793471914656,"url":"http://127.0.0.1:18765/index.html"},{"headers":{"Referer":"http://127.0.0.1:18765/index.html"},"method":"GET","mimeType":"text/html","requestId":"28845.3","resourceType":"Fetch","responseHeaders":{"Content-Length":"460"},"status":404,"timestamp":1793471550526,"url":"http://127.0.0.1:18765/missing.json?token=abc"},{"headers":{"Referer":"http://127.0.0.1:18765/"},"method":"GET","requestId":"30407.2","resourceType":"Fetch","timestamp":1793471914670,"url":"http://127.0.0.1:1/refused"}]}"#;
    const SNAPSHOT_INTERACTIVE: &str = r#"{"origin":"http://127.0.0.1:18765/index.html","refs":{"e1":{"name":"Fixture heading","role":"heading"},"e2":{"name":"Name","role":"textbox"},"e3":{"name":"Go","role":"button"},"e4":{"name":"Other page","role":"link"}},"removedRefs":[],"snapshot":"- heading \"Fixture heading\" [level=1, ref=e1]\n- textbox \"Name\" [ref=e2]\n- button \"Go\" [ref=e3]\n- link \"Other page\" [ref=e4]"}"#;
    const DIFF_SNAPSHOT: &str = r#"{"additions":1,"changed":true,"diff":"--- before\n+++ after\n@@ -1,4 +1,4 @@\n - heading \"Fixture heading\" [level=1]\n-- textbox \"Name\"\n+- textbox \"Name\": Lin\n - button \"Go\"\n","removals":1,"unchanged":3}"#;
    const DIFF_SCREENSHOT: &str = r#"{"diffPath":"/tmp/d.png","differentPixels":831,"dimensionMismatch":null,"match":false,"mismatchPercentage":0.16247477828351556,"totalPixels":511464}"#;
    const SET_DEVICE: &str =
        r#"{"device":"iPhone 12","deviceScaleFactor":3.0,"height":844,"mobile":true,"width":390}"#;
    const HAR: &str = r#"{"log":{"creator":{"name":"agent-browser","version":"0.41.0"},"entries":[{"_resourceType":"Fetch","request":{"cookies":[{"name":"session","value":"secret"}],"headers":[{"name":"Authorization","value":"Bearer secret"}],"method":"GET","url":"http://127.0.0.1:18765/missing.json?token=abc"},"response":{"bodySize":645,"content":{"mimeType":"text/html","size":645,"text":"<html>body</html>"},"headers":[{"name":"Set-Cookie","value":"session=secret"}],"status":404},"time":3.5}]}}"#;

    #[test]
    fn version_output_is_parsed_and_the_pin_is_exact() {
        assert_eq!(parse_version("agent-browser 0.41.0\n"), Some("0.41.0"));
        assert_eq!(parse_version("agent-browser 0.19.0"), Some("0.19.0"));
        assert_eq!(parse_version("something else"), None);
        assert_eq!(parse_version("agent-browser "), None);
        assert_eq!(PINNED_AGENT_BROWSER, "0.41.0");
    }

    #[test]
    fn batch_output_yields_each_commands_data_or_the_first_failure() {
        let rows = parse_batch_output(BATCH_OPEN_AND_STREAM, 2).unwrap();
        assert_eq!(rows[0]["targetId"], "6DC82394E6AE3613A31756D5A65F00FB");
        assert_eq!(rows[1]["disabled"], true);
        assert!(parse_batch_output(BATCH_OPEN_AND_STREAM, 3).is_err());

        let gone = parse_batch_output(BATCH_TAB_GONE, 1).unwrap_err();
        assert!(gone.tab_gone(), "{gone:?}");
        let denied = parse_batch_output(BATCH_DENIED, 1).unwrap_err();
        assert!(!denied.tab_gone());
        assert!(denied.message.contains("denied by policy"));
        let refused = parse_batch_output(POLICY_LOAD_FAILURE, 1).unwrap_err();
        assert!(refused.message.contains("policy failed to load"));
        let garbage = parse_batch_output("✗ something", 1).unwrap_err();
        assert!(garbage.message.contains("not JSON"));
    }

    #[test]
    fn commands_are_built_as_literal_batch_rows() {
        // Agent text rides in the batch's JSON, where `--headed` is text,
        // not the global flag it would be in argv.
        let stdin = batch_stdin(&[
            AbCommand::FillRef {
                reference: "e2".into(),
                text: "--headed --cdp 1".into(),
            },
            AbCommand::InsertText { text: "-i".into() },
            AbCommand::Press { key: "Space" },
            AbCommand::MouseMove { x: 400.0, y: 12.5 },
            AbCommand::MouseWheel {
                delta_y: -300.0,
                delta_x: 0.0,
            },
            AbCommand::SetViewport {
                width: 800,
                height: 600,
                scale: 2.0,
            },
            AbCommand::WaitFn {
                predicate: "true".into(),
                timeout_ms: 500,
            },
        ]);
        let rows: Vec<Vec<String>> = serde_json::from_str(&stdin).unwrap();
        assert_eq!(rows[0], ["fill", "@e2", "--headed --cdp 1"]);
        assert_eq!(rows[1], ["keyboard", "inserttext", "-i"]);
        assert_eq!(rows[2], ["press", "Space"]);
        assert_eq!(rows[3], ["mouse", "move", "400", "13"]);
        assert_eq!(rows[4], ["mouse", "wheel", "-300", "0"]);
        assert_eq!(rows[5], ["set", "viewport", "800", "600", "2"]);
        assert_eq!(rows[6], ["wait", "--fn", "true", "--timeout", "500"]);

        let eval = AbCommand::Eval {
            script: "document.title".into(),
        }
        .args();
        assert_eq!(eval[..2], ["eval", "-b"]);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&eval[2])
                .unwrap(),
            b"document.title"
        );
        assert_eq!(
            AbCommand::Screenshot {
                path: "/s/a.png".into(),
                full_page: true
            }
            .args(),
            ["screenshot", "--full", "/s/a.png"]
        );
        assert_eq!(
            AbCommand::DiffScreenshot {
                baseline: "/s/b.png".into(),
                output: "/s/d.png".into(),
                full_page: false
            }
            .args(),
            [
                "diff",
                "screenshot",
                "--baseline",
                "/s/b.png",
                "-o",
                "/s/d.png"
            ]
        );
    }

    #[test]
    fn global_args_attach_pin_and_guard_every_session() {
        let temp = tempdir().unwrap();
        let engine = AgentBrowserEngine::new(
            temp.path(),
            &BrowserEngineConfig {
                engine: crate::browser_engine::BrowserEngineKind::AgentBrowser,
                agent_browser_path: Some("/opt/agent-browser".into()),
            },
            BrowserControlService::new(temp.path()),
        );
        let args = engine.global_args(Some(9333), "gtabc-1");
        let state = temp.path().join(STATE_DIR_NAME);
        assert_eq!(
            args,
            [
                "--config".to_string(),
                state.join(CLI_CONFIG_FILE).display().to_string(),
                "--action-policy".to_string(),
                state.join(ACTION_POLICY_FILE).display().to_string(),
                "--cdp".to_string(),
                "9333".to_string(),
                "--session".to_string(),
                "gtabc-1".to_string(),
                "--pin-tab".to_string(),
                "--idle-timeout".to_string(),
                SESSION_IDLE_TIMEOUT.to_string(),
                "--json".to_string(),
                "batch".to_string(),
                "--bail".to_string(),
                "--omit-command".to_string(),
            ]
        );
        assert!(!engine
            .global_args(None, "gtabc-1")
            .contains(&"--cdp".to_string()));
    }

    #[test]
    fn the_action_policy_is_exactly_the_command_allowlist() {
        let policy = action_policy();
        assert_eq!(policy["default"], "deny");
        let mut allowed: Vec<String> = policy["allow"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        allowed.sort();
        let commands = [
            AbCommand::Open { url: String::new() },
            AbCommand::StreamStatus,
            AbCommand::StreamDisable,
            AbCommand::Close,
            AbCommand::TabSwitch {
                target_id: String::new(),
            },
            AbCommand::TabClose {
                target_id: String::new(),
            },
            AbCommand::TabCloseCurrent,
            AbCommand::Eval {
                script: String::new(),
            },
            AbCommand::WaitFn {
                predicate: String::new(),
                timeout_ms: 1,
            },
            AbCommand::ClickRef {
                reference: String::new(),
            },
            AbCommand::FillRef {
                reference: String::new(),
                text: String::new(),
            },
            AbCommand::InsertText {
                text: String::new(),
            },
            AbCommand::Press { key: "Enter" },
            AbCommand::MouseMove { x: 0.0, y: 0.0 },
            AbCommand::MouseDown,
            AbCommand::MouseUp,
            AbCommand::MouseWheel {
                delta_y: 0.0,
                delta_x: 0.0,
            },
            AbCommand::Reload,
            AbCommand::SetViewport {
                width: 1,
                height: 1,
                scale: 1.0,
            },
            AbCommand::SetDevice {
                name: String::new(),
            },
            AbCommand::Screenshot {
                path: PathBuf::new(),
                full_page: false,
            },
            AbCommand::Snapshot { interactive: true },
            AbCommand::DiffSnapshot {
                baseline: PathBuf::new(),
            },
            AbCommand::DiffScreenshot {
                baseline: PathBuf::new(),
                output: PathBuf::new(),
                full_page: false,
            },
            AbCommand::Console,
            AbCommand::Errors,
            AbCommand::NetworkRequests,
            AbCommand::HarStart,
            AbCommand::HarStop {
                path: PathBuf::new(),
            },
        ];
        let mut used: Vec<String> = commands
            .iter()
            .map(|command| command.action().to_string())
            .collect();
        // "launch" is the attach step agent-browser checks before any command.
        used.push("launch".to_string());
        used.sort();
        used.dedup();
        assert_eq!(allowed, used);
        for forbidden in [
            "cookies_get",
            "cookies_set",
            "storage_get",
            "state_save",
            "auth_login",
            "clipboard",
            "connect",
            "download",
            "upload",
        ] {
            assert!(
                !allowed.iter().any(|action| action == forbidden),
                "{forbidden}"
            );
        }
    }

    #[test]
    fn diagnostics_keep_errors_and_failures_and_never_headers() {
        let console = parse_console_errors(&serde_json::from_str(CONSOLE).unwrap());
        assert_eq!(
            console
                .iter()
                .map(|error| error.text.as_str())
                .collect::<Vec<_>>(),
            ["fixture console error", "assert msg"]
        );
        assert!(console.iter().all(|error| error.source == "console"));

        let errors = parse_page_errors(&serde_json::from_str(ERRORS).unwrap());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].source, "exception");
        assert!(errors[0].text.starts_with("Error: boom from page"));
        assert_eq!(errors[0].line_number, Some(4));

        let failures = parse_failed_requests(&serde_json::from_str(REQUESTS).unwrap());
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].url, "http://127.0.0.1:18765/missing.json");
        assert_eq!(failures[0].status, Some(404));
        assert_eq!(failures[1].url, "http://127.0.0.1:1/refused");
        assert_eq!(failures[1].status, None);
        let serialized = serde_json::to_string(&failures).unwrap();
        assert!(!serialized.contains("secret") && !serialized.contains("token"));
    }

    #[test]
    fn har_summaries_drop_headers_cookies_queries_and_bodies() {
        let (entries, count) = summarize_har(&serde_json::from_str(HAR).unwrap());
        assert_eq!(count, 1);
        assert_eq!(
            entries,
            [BrowserHarEntry {
                method: "GET".into(),
                url: "http://127.0.0.1:18765/missing.json".into(),
                status: Some(404),
                mime_type: Some("text/html".into()),
                resource_type: Some("Fetch".into()),
                time_ms: Some(3.5),
                response_bytes: Some(645),
            }]
        );
        let serialized = serde_json::to_string(&entries).unwrap();
        assert!(!serialized.contains("secret") && !serialized.contains("body"));
    }

    #[test]
    fn snapshot_refs_and_diffs_are_parsed_and_capped() {
        let data: Value = serde_json::from_str(SNAPSHOT_INTERACTIVE).unwrap();
        let refs = parse_snapshot_refs(&data);
        assert_eq!(refs.len(), 4);
        assert_eq!(refs["e3"].role.as_deref(), Some("button"));
        assert_eq!(refs["e3"].name.as_deref(), Some("Go"));

        let long = "- button \"x\" [ref=e1]\n".repeat(2_000);
        let (cut, truncated) = cap_text(&long, 1_000);
        assert!(truncated && cut.chars().count() <= 1_000 && !cut.ends_with('\n'));
        assert_eq!(cap_text("short", 1_000), ("short".to_string(), false));

        let mut diff = BrowserDiff {
            target: "primary".into(),
            baseline_target: "primary".into(),
            baseline_label: "before".into(),
            mode: BrowserDiffMode::Snapshot,
            changed: false,
            additions: None,
            removals: None,
            unchanged: None,
            diff: None,
            diff_truncated: None,
            mismatch_percentage: None,
            different_pixels: None,
            total_pixels: None,
            dimension_mismatch: None,
            regions: None,
            region_count: None,
            regions_unavailable: None,
            diff_png: None,
        };
        apply_snapshot_diff(&mut diff, &serde_json::from_str(DIFF_SNAPSHOT).unwrap());
        assert!(diff.changed);
        assert_eq!(
            (diff.additions, diff.removals, diff.unchanged),
            (Some(1), Some(1), Some(3))
        );
        assert!(diff
            .diff
            .as_deref()
            .unwrap()
            .contains("+- textbox \"Name\": Lin"));
        apply_screenshot_diff(&mut diff, &serde_json::from_str(DIFF_SCREENSHOT).unwrap());
        assert_eq!(diff.different_pixels, Some(831));
        assert_eq!(diff.total_pixels, Some(511_464));
        assert!(diff.dimension_mismatch.is_none());
    }

    #[test]
    fn viewport_requests_are_a_device_or_a_size() {
        assert_eq!(
            viewport_command(&BrowserViewportRequest {
                device: Some("iPhone 15".into()),
                ..Default::default()
            })
            .unwrap(),
            AbCommand::SetDevice {
                name: "iPhone 15".into()
            }
        );
        assert_eq!(
            viewport_command(&BrowserViewportRequest {
                width: Some(390),
                height: Some(844),
                device_scale_factor: Some(3.0),
                ..Default::default()
            })
            .unwrap(),
            AbCommand::SetViewport {
                width: 390,
                height: 844,
                scale: 3.0
            }
        );
        for invalid in [
            BrowserViewportRequest::default(),
            BrowserViewportRequest {
                device: Some("--headed".into()),
                ..Default::default()
            },
            BrowserViewportRequest {
                device: Some("iPhone 15".into()),
                width: Some(400),
                ..Default::default()
            },
            BrowserViewportRequest {
                width: Some(10),
                height: Some(10),
                ..Default::default()
            },
        ] {
            assert!(viewport_command(&invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            parse_viewport(&serde_json::from_str(SET_DEVICE).unwrap()),
            Some(BrowserViewport::mobile(390, 844, 3.0))
        );
    }

    fn engine_with_binary(dir: &Path, binary: PathBuf) -> AgentBrowserEngine {
        AgentBrowserEngine::new(
            dir,
            &BrowserEngineConfig {
                engine: crate::browser_engine::BrowserEngineKind::AgentBrowser,
                agent_browser_path: Some(binary),
            },
            BrowserControlService::new(dir),
        )
    }

    #[tokio::test]
    async fn a_missing_binary_is_refused_naming_the_path_and_the_pin() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("no-such-agent-browser");
        let engine = engine_with_binary(temp.path(), missing.clone());
        let error = engine
            .ensure_started(&mut *engine.state.lock().await)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains(PINNED_AGENT_BROWSER), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn another_version_is_refused_naming_the_path_and_the_pin() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let fake = temp.path().join("fake-agent-browser");
        std::fs::write(&fake, "#!/bin/sh\necho 'agent-browser 0.40.2'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let engine = engine_with_binary(temp.path(), fake.clone());
        let error = engine
            .ensure_started(&mut *engine.state.lock().await)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&fake.display().to_string()), "{error}");
        assert!(
            error.contains("0.40.2") && error.contains(PINNED_AGENT_BROWSER),
            "{error}"
        );
        assert!(!engine.state.lock().await.started);

        // The pinned version starts and lays out a private state directory.
        std::fs::write(&fake, "#!/bin/sh\necho 'agent-browser 0.41.0'\n").unwrap();
        engine
            .ensure_started(&mut *engine.state.lock().await)
            .await
            .unwrap();
        let state = temp.path().join(STATE_DIR_NAME);
        let policy: Value =
            serde_json::from_str(&std::fs::read_to_string(state.join(ACTION_POLICY_FILE)).unwrap())
                .unwrap();
        assert_eq!(policy, action_policy());
        assert_eq!(
            std::fs::read_to_string(state.join(CLI_CONFIG_FILE))
                .unwrap()
                .trim(),
            "{}"
        );
        let mode = std::fs::metadata(state.join(SOCKET_DIR_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn another_owners_target_is_not_found_before_the_engine_starts() {
        let temp = tempdir().unwrap();
        let engine = engine_with_binary(temp.path(), temp.path().join("absent"));
        engine.insert_test_target("chat-a", "design", "tab-1").await;
        let not_found = "browser target 'design' does not exist";
        for error in [
            engine.close_target("chat-b", "design").await.unwrap_err(),
            engine.focus_target("chat-b", "design").await.unwrap_err(),
            engine
                .diagnostics_in("chat-b", Some("design"))
                .await
                .unwrap_err(),
            engine
                .navigate_in("chat-b", Some("design"), "https://example.com/")
                .await
                .unwrap_err(),
            engine
                .click_in(
                    "chat-b",
                    Some("design"),
                    &BrowserLocator::Ref {
                        reference: "e1".into(),
                    },
                )
                .await
                .unwrap_err(),
        ] {
            assert!(error.to_string().starts_with(not_found), "{error}");
        }
        // chat-a's own target reaches the engine start, which refuses.
        let error = engine.focus_target("chat-a", "design").await.unwrap_err();
        assert!(
            error.to_string().contains("no agent-browser binary"),
            "{error}"
        );
        let status = engine.status_for("chat-b").await.unwrap();
        assert!(status.active_target.is_none() && status.target_id.is_none());
        let status = engine.status_for("chat-a").await.unwrap();
        assert_eq!(status.active_target.as_deref(), Some("design"));
    }

    fn solid_png(width: u32, height: u32, paint: &[(u32, u32, u32, u32)]) -> Vec<u8> {
        let mut img =
            image::RgbaImage::from_pixel(width, height, image::Rgba([255, 255, 255, 255]));
        for &(x, y, w, h) in paint {
            for py in y..y + h {
                for px in x..x + w {
                    img.put_pixel(px, py, image::Rgba([200, 0, 0, 255]));
                }
            }
        }
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn identical_screenshots_have_no_regions() {
        let a = solid_png(100, 80, &[(10, 10, 5, 5)]);
        let (regions, count) = changed_regions(&a, &a, 1.0).unwrap();
        assert!(regions.is_empty());
        assert_eq!(count, 0);
    }

    #[test]
    fn changed_regions_are_boxed_merged_and_largest_first() {
        let base = solid_png(200, 100, &[]);
        // Two blocks 4px apart merge; a distant small one stays apart.
        let now = solid_png(
            200,
            100,
            &[(10, 10, 10, 10), (24, 10, 10, 10), (150, 60, 3, 3)],
        );
        let (regions, count) = changed_regions(&base, &now, 1.0).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            regions,
            [
                BrowserDiffRegion {
                    x: 10,
                    y: 10,
                    width: 24,
                    height: 10
                },
                BrowserDiffRegion {
                    x: 150,
                    y: 60,
                    width: 3,
                    height: 3
                },
            ]
        );
    }

    #[test]
    fn region_boxes_are_in_css_pixels_and_capped() {
        // 2x scale: device pixels halve into CSS pixels.
        let base = solid_png(200, 100, &[]);
        let now = solid_png(200, 100, &[(20, 20, 40, 20)]);
        let (regions, _) = changed_regions(&base, &now, 2.0).unwrap();
        assert_eq!(
            regions,
            [BrowserDiffRegion {
                x: 10,
                y: 10,
                width: 20,
                height: 10
            }]
        );

        // Twelve far-apart changes: all counted, ten listed, biggest first.
        let paint: Vec<_> = (0..12).map(|i| (i * 40, 0, 1 + i, 1)).collect();
        let wide = solid_png(500, 20, &[]);
        let now = solid_png(500, 20, &paint);
        let (regions, count) = changed_regions(&wide, &now, 1.0).unwrap();
        assert_eq!(count, 12);
        assert_eq!(regions.len(), MAX_DIFF_REGIONS);
        assert_eq!(regions[0].width, 12);
        assert!(regions
            .windows(2)
            .all(|pair| pair[0].width >= pair[1].width));
    }

    #[test]
    fn mismatched_screenshot_sizes_give_no_regions() {
        let error =
            changed_regions(&solid_png(10, 10, &[]), &solid_png(12, 10, &[]), 1.0).unwrap_err();
        assert!(error.contains("differ in size"), "{error}");
    }

    #[tokio::test]
    async fn a_labelled_capture_is_a_diff_baseline_and_an_unlabelled_read_is_not() {
        let temp = tempdir().unwrap();
        let engine = engine_with_binary(temp.path(), temp.path().join("unused"));
        let snapshot = BrowserSnapshot {
            target: "primary".into(),
            capture_mode: BrowserCaptureMode::Viewport,
            url: "https://example.com/".into(),
            title: "Example".into(),
            loading_state: "complete".into(),
            visible_text: String::new(),
            interactive_elements: Vec::new(),
            viewport: BrowserViewport::desktop(100, 80),
            console_errors: Vec::new(),
            failed_requests: Vec::new(),
            screenshot_png: solid_png(100, 80, &[]),
        };
        // What a labelled browser_snapshot_refs stores (capture_labelled ends here).
        let mut state = engine.state.lock().await;
        engine
            .record_capture(&mut state, "chat-1", "before", &snapshot, "- button \"Go\"")
            .await;
        let evidence = engine
            .browser
            .owned_evidence("chat-1", "primary", "before")
            .await
            .expect("labelled evidence is retained");
        assert_eq!(
            state.baselines.get(&evidence.id).map(|text| text.as_str()),
            Some("- button \"Go\"")
        );
        assert_eq!(*evidence.screenshot_png, snapshot.screenshot_png);
        // Another chat, or an unlabelled read, finds nothing.
        assert!(engine
            .browser
            .owned_evidence("chat-2", "primary", "before")
            .await
            .is_none());
        assert!(engine
            .browser
            .owned_evidence("chat-1", "primary", "after")
            .await
            .is_none());
    }
}
