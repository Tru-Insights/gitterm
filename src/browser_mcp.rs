//! Authenticated loopback MCP exposure for the GitTerm-owned browser controller.

use crate::agent_browser::{
    BrowserDiff, BrowserDiffMode, BrowserNetworkCaptureAction, BrowserViewportRequest,
};
use crate::browser_control::{
    BrowserCaptureMode, BrowserControlError, BrowserControlService, BrowserKey,
    BrowserLaunchOptions, BrowserLocator, BrowserNodeSelector, BrowserOperation, BrowserSnapshot,
    BrowserViewport, BrowserWaitCondition,
};
use crate::browser_engine::{BrowserEngine, BrowserEngineKind};
use crate::harness::acp::AcpMcpServer;
use crate::harness::claude::ClaudeMcpServer;
use crate::task_mcp::{caller_endpoint, caller_from_query, TASK_MCP_CALLER_QUERY};
use axum::{
    body::Body,
    extract::State,
    http::{header, request::Parts, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine;
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::{
        streamable_http_server::{
            session::local::LocalSessionManager, tower::StreamableHttpService,
        },
        StreamableHttpServerConfig,
    },
    ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    io,
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const BROWSER_MCP_TOKEN_ENV: &str = "GITTERM_V5_BROWSER_MCP_TOKEN";
pub const BROWSER_MCP_URL_ENV: &str = "GITTERM_V5_BROWSER_MCP_URL";
const BROWSER_MCP_BASE_PORT: u16 = 24_030;
const BROWSER_MCP_PORTS_PER_INSTANCE: u16 = 10;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;

/// Connection data retained by the app and inherited by local terminals.
///
/// The token is intentionally never serialized or included in `Debug` output.
pub struct BrowserMcpConnection {
    endpoint: String,
    token: String,
    cancellation: CancellationToken,
    browser: BrowserControlService,
    engine: BrowserEngine,
}

impl BrowserMcpConnection {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The browser MCP variables for one terminal. Like the task server's,
    /// the URL carries the tab's caller identity (`?caller=<session_uid>`,
    /// TRU-176): browser targets belong to the chat that opened them, and a
    /// call without a caller is refused. The token is the same for every
    /// tab and lives only in the environment.
    pub fn terminal_environment(&self, caller: Option<&str>) -> [(String, String); 2] {
        [
            (
                BROWSER_MCP_URL_ENV.to_string(),
                caller_endpoint(&self.endpoint, caller),
            ),
            (BROWSER_MCP_TOKEN_ENV.to_string(), self.token.clone()),
        ]
    }

    /// Shared browser access for GitTerm UI controls.
    ///
    /// MCP tools and the UI clone this same service, whose controller mutex
    /// serializes complete browser operations.
    pub fn browser(&self) -> BrowserControlService {
        self.browser.clone()
    }

    /// The engine behind the browser tools (TRU-177), for the UI's own
    /// calls that must reach it too, such as releasing closed chats.
    pub fn engine(&self) -> BrowserEngine {
        self.engine.clone()
    }

    /// Per-run Codex overrides avoid modifying global or project config files.
    pub fn codex_config_overrides(&self) -> [String; 4] {
        codex_config_overrides(&self.endpoint)
    }

    /// The browser server for a natively spawned Claude session (chat
    /// tabs): the same config and read-only pre-approvals
    /// `configure_claude_command` injects into terminal launches, with the
    /// token carried in the child environment. `caller` is the chat tab's
    /// `session_uid`.
    pub fn claude_mcp_server(&self, caller: Option<&str>) -> ClaudeMcpServer {
        ClaudeMcpServer {
            config: claude_mcp_config(&caller_endpoint(&self.endpoint, caller)),
            allowed_tools: claude_allowed_tools(),
            env: self.terminal_environment(caller).into(),
        }
    }

    /// The browser server for an ACP chat (Gemini, OpenCode; TRU-153),
    /// with its bearer header carried literally in `session/new`.
    pub fn acp_mcp_server(&self, caller: Option<&str>) -> AcpMcpServer {
        AcpMcpServer {
            name: "gitterm_browser".to_string(),
            url: caller_endpoint(&self.endpoint, caller),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", self.token),
            )],
        }
    }
}

/// Add ephemeral MCP configuration immediately after the Codex executable so
/// subcommands such as `codex resume` continue to parse correctly.
pub fn configure_codex_command(command: &str, endpoint: &str) -> String {
    let trimmed = command.trim();
    let executable_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let executable = &trimmed[..executable_end];
    let executable_name = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
    if executable_name != "codex" || trimmed.contains("mcp_servers.gitterm_browser.url=") {
        return command.to_string();
    }

    let mut configured = executable.to_string();
    for value in codex_config_overrides(endpoint) {
        configured.push_str(" --config ");
        if value.contains(['?', '&', '*', '[']) {
            // A per-tab URL carries `?caller=…`; unquoted, zsh treats `?`
            // as a glob and aborts the launch with "no matches found".
            configured.push('\'');
            configured.push_str(&value);
            configured.push('\'');
        } else {
            configured.push_str(&value);
        }
    }
    configured.push_str(&trimmed[executable_end..]);
    configured
}

/// Attach the browser MCP server to whichever harness this launch command
/// starts. pi attaches through its `gitterm-mcp` extension (see
/// `pi-extensions/`); other commands pass through unchanged.
pub fn configure_browser_command(command: &str, endpoint: &str) -> String {
    let trimmed = command.trim();
    let executable = &trimmed[..trimmed.find(char::is_whitespace).unwrap_or(trimmed.len())];
    match executable.rsplit(['/', '\\']).next().unwrap_or(executable) {
        "codex" => configure_codex_command(command, endpoint),
        "claude" => configure_claude_command(command, endpoint),
        _ => command.to_string(),
    }
}

/// The browser tools Claude may call without a permission prompt: exactly the
/// inspection tools the server annotates `read_only_hint = true`. Mutating
/// tools (navigate, click, type, …) still prompt, mirroring Codex's
/// `default_tools_approval_mode=writes`. Kept in sync with both engines' tool
/// routers by `claude_allowed_tools_match_the_read_only_annotations`; the
/// agent-browser-only names are harmless when the gitterm engine runs.
const CLAUDE_ALLOWED_READ_TOOLS: [&str; 12] = [
    "browser_status",
    "browser_targets",
    "browser_snapshot",
    "browser_capture",
    "browser_wait_for",
    "browser_console",
    "browser_network",
    "browser_target_diagnostics",
    "browser_dom_outline",
    "browser_node_inspect",
    "browser_snapshot_refs",
    "browser_diff",
];

/// Claude Code: a second `--mcp-config='{…}'` alongside the task server's
/// (Claude merges repeated flags). Same rules as
/// `task_mcp::configure_claude_command`: the `=` form because both options
/// are variadic, and the bearer header expanded by Claude from the terminal
/// environment so the token never appears on the command line.
pub fn configure_claude_command(command: &str, endpoint: &str) -> String {
    let trimmed = command.trim();
    let executable_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let executable = &trimmed[..executable_end];
    let executable_name = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
    if executable_name != "claude" || command.contains("\"gitterm_browser\"") {
        return command.to_string();
    }
    let config = claude_mcp_config(endpoint).to_string();
    if config.contains('\'') {
        // Embedded single-quoted; GitTerm never produces such an endpoint.
        return command.to_string();
    }
    let allowed = claude_allowed_tools().join(",");
    format!(
        "{executable} --mcp-config='{config}' --allowedTools={allowed}{rest}",
        rest = &trimmed[executable_end..]
    )
}

fn claude_mcp_config(endpoint: &str) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "gitterm_browser": {
                "type": "http",
                "url": endpoint,
                "headers": {
                    "Authorization": format!("Bearer ${{{BROWSER_MCP_TOKEN_ENV}}}"),
                },
            },
        },
    })
}

fn claude_allowed_tools() -> Vec<String> {
    CLAUDE_ALLOWED_READ_TOOLS
        .map(|tool| format!("mcp__gitterm_browser__{tool}"))
        .to_vec()
}

/// Define a session-local zsh wrapper so Codex launched manually from a
/// GitTerm terminal receives the same ephemeral MCP configuration as a preset.
/// The bearer token itself remains only in the inherited environment.
pub fn codex_zsh_integration() -> String {
    format!(
        r#"if [[ -n "${{{url_env}:-}}" && -n "${{{token_env}:-}}" ]]; then
    codex() {{
        local arg
        for arg in "$@"; do
            if [[ "$arg" == mcp_servers.gitterm_browser.url=* ]]; then
                command codex "$@"
                return
            fi
        done
        command codex \
            --config "mcp_servers.gitterm_browser.url=${{{url_env}}}" \
            --config "mcp_servers.gitterm_browser.bearer_token_env_var={token_env}" \
            --config "mcp_servers.gitterm_browser.default_tools_approval_mode=writes" \
            --config "mcp_servers.gitterm_browser.tool_timeout_sec=60" \
            "$@"
    }}
fi"#,
        url_env = BROWSER_MCP_URL_ENV,
        token_env = BROWSER_MCP_TOKEN_ENV,
    )
}

fn codex_config_overrides(endpoint: &str) -> [String; 4] {
    [
        format!("mcp_servers.gitterm_browser.url={endpoint}"),
        format!("mcp_servers.gitterm_browser.bearer_token_env_var={BROWSER_MCP_TOKEN_ENV}"),
        "mcp_servers.gitterm_browser.default_tools_approval_mode=writes".to_string(),
        "mcp_servers.gitterm_browser.tool_timeout_sec=60".to_string(),
    ]
}

impl Drop for BrowserMcpConnection {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub struct BrowserMcpServer {
    listener: std::net::TcpListener,
    token: String,
    cancellation: CancellationToken,
    engine: BrowserEngine,
}

/// Reserve an endpoint from the V5-only loopback range synchronously so
/// terminal environments can be constructed before Iced starts polling
/// asynchronous startup tasks.
pub fn prepare(
    v5_global_config_dir: impl AsRef<Path>,
    instance_id: &str,
) -> std::io::Result<(BrowserMcpConnection, BrowserMcpServer)> {
    let listener = bind_v5_loopback_listener(instance_id)?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let cancellation = CancellationToken::new();
    let shared_config_dir = v5_global_config_dir.as_ref();
    let browser = BrowserControlService::new(shared_config_dir);
    // The engine choice is the shared config's browser.json (TRU-177),
    // read once: the tool list an agent sees depends on it.
    let engine = BrowserEngine::from_shared_config(shared_config_dir, browser.clone());
    eprintln!("GitTerm V5 browser engine: {}", engine.describe());
    Ok((
        BrowserMcpConnection {
            endpoint,
            token: token.clone(),
            cancellation: cancellation.clone(),
            browser,
            engine: engine.clone(),
        },
        BrowserMcpServer {
            listener,
            token,
            cancellation,
            engine,
        },
    ))
}

fn browser_mcp_port_range(instance_id: &str) -> std::ops::Range<u16> {
    let instance_slot = instance_id.parse::<u32>().unwrap_or(0).rem_euclid(100) as u16;
    let start = BROWSER_MCP_BASE_PORT + (instance_slot * BROWSER_MCP_PORTS_PER_INSTANCE);
    start..(start + BROWSER_MCP_PORTS_PER_INSTANCE)
}

fn bind_v5_loopback_listener(instance_id: &str) -> io::Result<std::net::TcpListener> {
    let mut last_error = None;
    for port in browser_mcp_port_range(instance_id) {
        match std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "GitTerm V5 browser MCP port range is empty",
        )
    }))
}

impl BrowserMcpServer {
    pub async fn run(self) -> Result<(), String> {
        let listener = tokio::net::TcpListener::from_std(self.listener).map_err(|error| {
            format!("failed to activate the GitTerm V5 browser MCP listener: {error}")
        })?;
        let tools = BrowserMcpTools::new(self.engine);
        let mcp_service: StreamableHttpService<BrowserMcpTools, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok::<_, std::io::Error>(tools.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_json_response(true)
                    .with_cancellation_token(self.cancellation.child_token()),
            );
        let auth = Arc::new(BearerAuth { token: self.token });
        let app = Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(middleware::from_fn_with_state(auth, authenticate));
        let shutdown = self.cancellation.cancelled_owned();
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|error| format!("GitTerm V5 browser MCP server stopped: {error}"))
    }
}

struct BearerAuth {
    token: String,
}

async fn authenticate(
    State(auth): State<Arc<BearerAuth>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token.as_bytes() == auth.token.as_bytes());
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Unauthorized",
        )
            .into_response()
    }
}

#[derive(Clone)]
struct BrowserMcpTools {
    engine: BrowserEngine,
    agent_operation_lock: Arc<Mutex<()>>,
    /// The tools this engine offers: the common set, plus the
    /// agent-browser-only tools when that engine runs (TRU-177).
    tool_router: ToolRouter<Self>,
}

impl BrowserMcpTools {
    fn new(engine: BrowserEngine) -> Self {
        let tool_router = match engine.kind() {
            Some(BrowserEngineKind::AgentBrowser) => {
                Self::tool_router() + Self::agent_browser_tool_router()
            }
            Some(BrowserEngineKind::Gitterm) | None => Self::tool_router(),
        };
        Self {
            engine,
            agent_operation_lock: Arc::new(Mutex::new(())),
            tool_router,
        }
    }

    /// Run one browser operation for the chat named on the HTTP request's
    /// URL (rmcp injects the request `Parts` into every call). Operations
    /// stay serialised across every caller; a call without a caller is
    /// refused before it reaches the browser.
    async fn run_agent_operation<T, Run, Operation>(
        &self,
        operation: BrowserOperation,
        parts: &Parts,
        run: Run,
    ) -> crate::browser_control::Result<T>
    where
        Run: FnOnce(BrowserEngine, String) -> Operation,
        Operation: Future<Output = crate::browser_control::Result<T>>,
    {
        let Some(caller) = caller_from_query(parts.uri.query()) else {
            return Err(BrowserControlError::new(missing_browser_caller_error()));
        };
        let _serial_operation = self.agent_operation_lock.lock().await;
        let service = self.engine.service();
        let activity_id = service.begin_agent_operation(operation).await;
        let result = run(self.engine.clone(), caller).await;
        let error = result.as_ref().err().map(ToString::to_string);
        service.finish_agent_operation(activity_id, error).await;
        result
    }
}

/// Why a browser tool refused a call that named no calling chat (TRU-176).
pub fn missing_browser_caller_error() -> String {
    format!(
        "GitTerm's browser tools need the calling GitTerm tab's identity, and this call has none. \
         Browser targets belong to the chat that opened them, so call them through the browser \
         MCP URL GitTerm gave that tab (it ends in ?{TASK_MCP_CALLER_QUERY}=<session id>)."
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NavigateRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// An absolute HTTP or HTTPS URL.
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LocatorRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// A strict semantic or CSS locator that must match exactly one visible element.
    locator: BrowserLocator,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// A strict locator for one editable visible element.
    locator: BrowserLocator,
    /// Replacement text to enter.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    key: BrowserKey,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    #[serde(default)]
    delta_x: f64,
    delta_y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResizeRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    viewport: BrowserViewport,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReloadRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    #[serde(default)]
    ignore_cache: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    condition: BrowserWaitCondition,
    /// Maximum wait in milliseconds. Defaults to 10000 and is capped at 60000.
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OpenTargetRequest {
    /// Stable lowercase name such as `design`, `implementation`, or `before`.
    name: String,
    /// Initial absolute HTTP or HTTPS URL.
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TargetRequest {
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CaptureRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// Short evidence purpose shared across captures that should be compared.
    label: String,
    /// Capture the complete document instead of only the current viewport.
    #[serde(default)]
    full_page: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DomOutlineRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NodeInspectRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// A document-scoped node reference from browser_dom_outline or a strict locator.
    selector: BrowserNodeSelector,
}

#[tool_router]
impl BrowserMcpTools {
    #[tool(
        description = "Report the GitTerm-owned browser process and this chat's active target.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn browser_status(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Status,
                &parts,
                |browser, caller| async move { browser.status_for(&caller).await },
            )
            .await,
        )
    }

    #[tool(
        description = "Open the visible Chrome instance using GitTerm V5's isolated browser profile.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_open(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Open,
                &parts,
                |browser, caller| async move {
                    browser.launch(BrowserLaunchOptions::default()).await?;
                    browser.status_for(&caller).await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "List the named page targets this chat opened, including its active target. Other chats' targets and the user's own tabs are not listed and cannot be used.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn browser_targets(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Targets,
                &parts,
                |browser, caller| async move { browser.targets(&caller).await },
            )
            .await,
        )
    }

    #[tool(
        description = "Open and name an independent page target, owned by this chat, for source/target, before/after, or other comparison workflows.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_target_open(
        &self,
        Parameters(request): Parameters<OpenTargetRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::TargetOpen,
                &parts,
                |browser, caller| async move {
                    browser
                        .open_target(&caller, &request.name, &request.url)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Focus one of this chat's named page targets and make it the default for this chat's calls that omit target.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn browser_target_focus(
        &self,
        Parameters(request): Parameters<TargetRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(BrowserOperation::TargetFocus, &parts, |browser, caller| async move {
                browser.focus_target(&caller, &request.target).await
            })
            .await,
        )
    }

    #[tool(
        description = "Close one of this chat's named page targets without disconnecting the remaining GitTerm browser targets.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_target_close(
        &self,
        Parameters(request): Parameters<TargetRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(BrowserOperation::TargetClose, &parts, |browser, caller| async move {
                browser.close_target(&caller, &request.target).await
            })
            .await,
        )
    }

    #[tool(
        description = "Navigate the controlled browser to an absolute HTTP or HTTPS URL.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_navigate(
        &self,
        Parameters(request): Parameters<NavigateRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Navigate,
                &parts,
                |browser, caller| async move {
                    browser
                        .navigate_in(&caller, request.target.as_deref(), &request.url)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Capture a PNG screenshot plus structured URL, title, page text, interactive elements, viewport, console errors, and failed requests.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_snapshot(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        match self
            .run_agent_operation(
                BrowserOperation::Snapshot,
                &parts,
                |browser, caller| async move { browser.snapshot(&caller).await },
            )
            .await
        {
            Ok(snapshot) => snapshot_result(snapshot),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Capture labeled viewport or full-page evidence for a named target and retain it in GitTerm's bounded memory-only comparison set.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_capture(
        &self,
        Parameters(request): Parameters<CaptureRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        let capture_mode = if request.full_page {
            BrowserCaptureMode::FullPage
        } else {
            BrowserCaptureMode::Viewport
        };
        match self
            .run_agent_operation(
                BrowserOperation::Snapshot,
                &parts,
                |browser, caller| async move {
                    browser
                        .capture(
                            &caller,
                            request.target.as_deref(),
                            &request.label,
                            capture_mode,
                        )
                        .await
                },
            )
            .await
        {
            Ok(snapshot) => snapshot_result(snapshot),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Click exactly one visible element resolved by a strict locator.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_click(
        &self,
        Parameters(request): Parameters<LocatorRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Click,
                &parts,
                |browser, caller| async move {
                    browser
                        .click_in(&caller, request.target.as_deref(), &request.locator)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Replace the contents of exactly one editable visible element.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_type(
        &self,
        Parameters(request): Parameters<TypeRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Type,
                &parts,
                |browser, caller| async move {
                    browser
                        .type_text_in(
                            &caller,
                            request.target.as_deref(),
                            &request.locator,
                            &request.text,
                        )
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Press one supported non-text key in the controlled page.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_press(
        &self,
        Parameters(request): Parameters<PressRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Press,
                &parts,
                |browser, caller| async move {
                    browser
                        .press_in(&caller, request.target.as_deref(), request.key)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Scroll the controlled page by bounded horizontal and vertical wheel deltas.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_scroll(
        &self,
        Parameters(request): Parameters<ScrollRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Scroll,
                &parts,
                |browser, caller| async move {
                    browser
                        .scroll_in(
                            &caller,
                            request.target.as_deref(),
                            request.delta_x,
                            request.delta_y,
                        )
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Apply bounded desktop or mobile viewport metrics to the controlled page.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_resize(
        &self,
        Parameters(request): Parameters<ResizeRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Resize,
                &parts,
                |browser, caller| async move {
                    browser
                        .resize_in(&caller, request.target.as_deref(), request.viewport)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Reload the controlled page and wait for its new document to finish loading.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_reload(
        &self,
        Parameters(request): Parameters<ReloadRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Reload,
                &parts,
                |browser, caller| async move {
                    browser
                        .reload_in(&caller, request.target.as_deref(), request.ignore_cache)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Wait deterministically for one locator, visible text, URL, or document loading state.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_wait_for(
        &self,
        Parameters(request): Parameters<WaitRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        let timeout_ms = request.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        if timeout_ms == 0 || timeout_ms > MAX_WAIT_TIMEOUT_MS {
            return tool_error(format!(
                "browser wait timeout_ms must be between 1 and {MAX_WAIT_TIMEOUT_MS}"
            ));
        }
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Wait,
                &parts,
                |browser, caller| async move {
                    browser
                        .wait_for_in(
                            &caller,
                            request.target.as_deref(),
                            &request.condition,
                            Duration::from_millis(timeout_ms),
                        )
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Return recent bounded browser console errors and uncaught exceptions.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_console(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        match self
            .run_agent_operation(
                BrowserOperation::Console,
                &parts,
                |browser, caller| async move { browser.diagnostics_in(&caller, None).await },
            )
            .await
        {
            Ok(diagnostics) => structured_value(diagnostics.console_errors),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Return recent bounded failed browser requests with sanitized URLs.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_network(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        match self
            .run_agent_operation(
                BrowserOperation::Network,
                &parts,
                |browser, caller| async move { browser.diagnostics_in(&caller, None).await },
            )
            .await
        {
            Ok(diagnostics) => structured_value(diagnostics.failed_requests),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Return isolated recent console errors and failed requests for one named browser target.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_target_diagnostics(
        &self,
        Parameters(request): Parameters<TargetRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Console,
                &parts,
                |browser, caller| async move {
                    browser.diagnostics_in(&caller, Some(&request.target)).await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Return a bounded structural outline of meaningful visible DOM nodes for one named target, with document-scoped node references, hierarchy, bounds, and accessibility state.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_dom_outline(
        &self,
        Parameters(request): Parameters<DomOutlineRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Dom,
                &parts,
                |browser, caller| async move {
                    browser
                        .dom_outline_in(&caller, request.target.as_deref())
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Inspect one DOM node by a document-scoped node reference or strict locator, returning bounded layout, computed styles, attributes, accessibility identity, and outerHTML.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_node_inspect(
        &self,
        Parameters(request): Parameters<NodeInspectRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Styles,
                &parts,
                |browser, caller| async move {
                    browser
                        .node_details_in(&caller, request.target.as_deref(), &request.selector)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Disconnect this chat from the GitTerm browser: close this chat's own page targets and drop their sessions. Chrome keeps running, and other chats' targets and the user's own tabs stay open; only the user can stop the browser, from GitTerm.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_disconnect(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Disconnect,
                &parts,
                |browser, caller| async move { browser.disconnect_owner(&caller).await },
            )
            .await,
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SnapshotRefsRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// Label of this chat's browser_capture evidence to compare against.
    baseline_label: String,
    /// The target that evidence was captured on. Defaults to `target`.
    baseline_target: Option<String>,
    mode: BrowserDiffMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NetworkCaptureRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    action: BrowserNetworkCaptureAction,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ViewportRequest {
    /// One of this chat's named browser targets. Omit to use its active target.
    target: Option<String>,
    /// Device preset such as "iPhone 15" or "Pixel 9"; or give width and height.
    device: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    device_scale_factor: Option<f64>,
}

/// Tools only the agent-browser engine offers (TRU-177). With the gitterm
/// engine they are not in the tool list at all.
#[tool_router(router = agent_browser_tool_router)]
impl BrowserMcpTools {
    #[tool(
        description = "Interactive accessibility tree with refs (e3) for click and type ref locators. Capped; says when cut.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_snapshot_refs(
        &self,
        Parameters(request): Parameters<SnapshotRefsRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Snapshot,
                &parts,
                |browser, caller| async move {
                    browser
                        .snapshot_refs_in(&caller, request.target.as_deref())
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Diff the page against this chat's labelled browser_capture evidence, as accessibility-tree text or pixels.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_diff(
        &self,
        Parameters(request): Parameters<DiffRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        match self
            .run_agent_operation(
                BrowserOperation::Snapshot,
                &parts,
                |browser, caller| async move {
                    browser
                        .diff_in(
                            &caller,
                            request.target.as_deref(),
                            &request.baseline_label,
                            request.baseline_target.as_deref(),
                            request.mode,
                        )
                        .await
                },
            )
            .await
        {
            Ok(diff) => diff_result(diff),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Start or stop recording a target's requests; stop returns method, URL, status, type and timing per request.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn browser_network_capture(
        &self,
        Parameters(request): Parameters<NetworkCaptureRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Network,
                &parts,
                |browser, caller| async move {
                    browser
                        .network_capture_in(&caller, request.target.as_deref(), request.action)
                        .await
                },
            )
            .await,
        )
    }

    #[tool(
        description = "Emulate a device preset or set the viewport size of a target.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_viewport(
        &self,
        Parameters(request): Parameters<ViewportRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        let viewport = BrowserViewportRequest {
            device: request.device,
            width: request.width,
            height: request.height,
            device_scale_factor: request.device_scale_factor,
        };
        structured_browser_result(
            self.run_agent_operation(
                BrowserOperation::Resize,
                &parts,
                |browser, caller| async move {
                    browser
                        .viewport_in(&caller, request.target.as_deref(), &viewport)
                        .await
                },
            )
            .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BrowserMcpTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gitterm-v5-browser",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "These are the GitTerm browser tools. When the user asks for the GitTerm or managed browser, use this server instead of Codex's bundled iab browser. Control only the visible Chrome window owned by GitTerm V5. Call browser_open before page operations. Page targets belong to the chat that opened them: names are private to this chat, a call that omits target uses this chat's active target (opening its default target on first use), and other chats' targets and the user's own tabs are out of reach. Use arbitrary named targets for source/target or before/after comparisons and pass target explicitly when more than one is open. Use the same evidence label across captures that should be compared. Use browser_dom_outline for bounded page structure, then browser_node_inspect with its document-scoped node_ref for layout and computed styles; node references become stale after navigation. Prefer semantic role or text locators; CSS is an explicit fallback. Browser actions are serialized. Evidence is bounded and memory-only. Never use these tools for passwords, cookies, authentication secrets, or unrestricted browser storage.",
            )
    }
}

fn structured_browser_result<T: Serialize>(
    result: crate::browser_control::Result<T>,
) -> CallToolResult {
    match result {
        Ok(value) => structured_value(value),
        Err(error) => tool_error(error),
    }
}

fn snapshot_result(snapshot: BrowserSnapshot) -> CallToolResult {
    let structured = match serialize_structured_content(&snapshot) {
        Ok(value) => value,
        Err(error) => {
            return tool_error(format!(
                "failed to serialize browser snapshot state: {error}"
            ));
        }
    };
    let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| structured.to_string());
    let image = base64::engine::general_purpose::STANDARD.encode(&snapshot.screenshot_png);
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(text),
        ContentBlock::image(image, "image/png"),
    ]);
    result.structured_content = Some(structured);
    result
}

fn diff_result(diff: BrowserDiff) -> CallToolResult {
    let structured = match serialize_structured_content(&diff) {
        Ok(value) => value,
        Err(error) => {
            return tool_error(format!("failed to serialize browser diff: {error}"));
        }
    };
    let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| structured.to_string());
    let mut content = vec![ContentBlock::text(text)];
    if let Some(png) = &diff.diff_png {
        content.push(ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(png),
            "image/png",
        ));
    }
    let mut result = CallToolResult::success(content);
    result.structured_content = Some(structured);
    result
}

fn structured_value<T: Serialize>(value: T) -> CallToolResult {
    match serialize_structured_content(value) {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => tool_error(format!("failed to serialize browser tool result: {error}")),
    }
}

fn serialize_structured_content<T: Serialize>(value: T) -> serde_json::Result<serde_json::Value> {
    let value = serde_json::to_value(value)?;
    Ok(match value {
        serde_json::Value::Object(_) => value,
        value => serde_json::json!({ "result": value }),
    })
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{
        transport::{
            streamable_http_client::StreamableHttpClientTransportConfig,
            StreamableHttpClientTransport,
        },
        ServiceExt,
    };
    use tempfile::tempdir;

    fn successful_structured_content<T: Serialize>(value: T) -> serde_json::Value {
        let result = structured_value(value);
        assert_eq!(result.is_error, Some(false));
        result
            .structured_content
            .expect("successful result must include structured content")
    }

    #[test]
    fn structured_content_preserves_objects() {
        let value = serde_json::json!({ "status": "ready" });
        assert_eq!(successful_structured_content(&value), value);
    }

    #[test]
    fn structured_content_wraps_arrays() {
        let value = serde_json::json!(["source", "target"]);
        assert_eq!(
            successful_structured_content(&value),
            serde_json::json!({ "result": value })
        );
    }

    #[test]
    fn structured_content_wraps_scalars() {
        assert_eq!(
            successful_structured_content(true),
            serde_json::json!({ "result": true })
        );
    }

    #[test]
    fn structured_content_wraps_unit_as_null() {
        assert_eq!(
            successful_structured_content(()),
            serde_json::json!({ "result": null })
        );
    }

    #[test]
    fn connection_uses_v5_loopback_range_and_memory_only_token_environment() {
        let temp = tempdir().unwrap();
        let (connection, _server) = prepare(temp.path(), "connection-test").unwrap();
        assert!(connection.endpoint().starts_with("http://127.0.0.1:"));
        assert!(connection.endpoint().ends_with("/mcp"));
        let port = connection
            .endpoint()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/mcp")
            .parse::<u16>()
            .unwrap();
        assert!((BROWSER_MCP_BASE_PORT..25_030).contains(&port));
        assert!(!(13_030..14_030).contains(&port));
        let caller = "7d1c2a4e-0b3f-4e8a-9c55-2f1e6d7a8b90";
        let caller_url = format!("{}?caller={caller}", connection.endpoint());
        let environment = connection.terminal_environment(Some(caller));
        assert_eq!(environment[0].0, BROWSER_MCP_URL_ENV);
        assert_eq!(environment[0].1, caller_url);
        assert_eq!(environment[1].0, BROWSER_MCP_TOKEN_ENV);
        assert_eq!(environment[1].1.len(), 64);
        assert!(!connection
            .codex_config_overrides()
            .iter()
            .any(|value| value.contains(&environment[1].1)));
        // Bottom-panel terminals name no caller: the bare endpoint.
        assert_eq!(
            connection.terminal_environment(None)[0].1,
            connection.endpoint()
        );

        // Chat tabs get the terminal launch's config and pre-approvals, with
        // the caller on the URL and the token only in the child environment.
        let server = connection.claude_mcp_server(Some(caller));
        let config = server.config.to_string();
        assert!(config.contains("\"gitterm_browser\""));
        assert!(config.contains(&caller_url));
        assert!(!config.contains(&environment[1].1));
        let terminal = configure_claude_command("claude", &caller_url);
        assert!(terminal.contains(&format!("--mcp-config='{config}'")));
        assert!(terminal.contains(&format!(
            "--allowedTools={}",
            server.allowed_tools.join(",")
        )));
        assert_eq!(server.env, environment.to_vec());
        let acp = connection.acp_mcp_server(Some(caller));
        assert_eq!(acp.url, caller_url);

        // Codex gets the per-tab URL quoted, or zsh would glob the `?`.
        let codex = configure_codex_command("codex", &caller_url);
        assert!(
            codex.contains(&format!("'mcp_servers.gitterm_browser.url={caller_url}'")),
            "{codex}"
        );
    }

    fn request_parts(uri: &str) -> Parts {
        Request::builder().uri(uri).body(()).unwrap().into_parts().0
    }

    fn error_text(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected a tool error");
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_call_without_a_caller_is_refused_before_the_browser() {
        let temp = tempdir().unwrap();
        let browser = BrowserControlService::new(temp.path());
        let tools = BrowserMcpTools::new(BrowserEngine::GitTerm(browser.clone()));
        for uri in ["/mcp", "/mcp?caller=", "/mcp?other=1"] {
            let result = tools.browser_targets(Extension(request_parts(uri))).await;
            assert_eq!(error_text(&result), missing_browser_caller_error(), "{uri}");
        }
        // Refused before it began: no agent activity was recorded.
        let telemetry = browser.telemetry().await;
        assert!(telemetry.last_agent_operation.is_none());
        assert!(telemetry.active_agent_operation.is_none());
    }

    #[tokio::test]
    async fn one_callers_disconnect_leaves_chrome_other_chats_and_the_users_tabs() {
        let devtools = crate::browser_control::test_support::FakeDevTools::start(&[
            "tab-a", "tab-b", "tab-user",
        ])
        .await;
        let temp = tempdir().unwrap();
        let browser = BrowserControlService::new(temp.path());
        browser.attach_test_devtools(devtools.port).await;
        browser
            .insert_test_target("chat-a", "primary", "tab-a")
            .await;
        browser
            .insert_test_target("chat-b", "primary", "tab-b")
            .await;
        let tools = BrowserMcpTools::new(BrowserEngine::GitTerm(browser));
        let chat_a = || Extension(request_parts("/mcp?caller=chat-a"));
        let chat_b = || Extension(request_parts("/mcp?caller=chat-b"));

        let result = tools.browser_disconnect(chat_a()).await;
        assert_eq!(result.is_error, Some(false));
        let outcome = result.structured_content.unwrap();
        assert_eq!(outcome["closed_targets"], serde_json::json!(["primary"]));
        assert_eq!(outcome["browser_running"], true);
        assert_eq!(
            *devtools.calls.lock().unwrap(),
            vec!["Target.closeTarget:tab-a".to_string()]
        );
        assert_eq!(
            *devtools.pages.lock().unwrap(),
            vec!["tab-b".to_string(), "tab-user".to_string()]
        );

        // chat-b still has its target; the user's tab is nobody's.
        let listed = tools.browser_targets(chat_b()).await;
        assert_eq!(listed.is_error, Some(false));
        let listed = listed.structured_content.unwrap();
        let listed = listed["result"].as_array().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["targetId"], "tab-b");
        assert_eq!(listed[0]["active"], true);
    }

    #[tokio::test]
    async fn two_callers_cannot_reach_each_others_targets() {
        let temp = tempdir().unwrap();
        let browser = BrowserControlService::new(temp.path());
        browser
            .insert_test_target("chat-a", "design", "tab-1")
            .await;
        let tools = BrowserMcpTools::new(BrowserEngine::GitTerm(browser));
        let chat_a = || Extension(request_parts("/mcp?caller=chat-a"));
        let chat_b = || Extension(request_parts("/mcp?caller=chat-b"));
        let design = || {
            Parameters(TargetRequest {
                target: "design".to_string(),
            })
        };
        let not_found = "browser target 'design' does not exist";

        // chat-b names chat-a's target: not found, as if it did not exist.
        for result in [
            tools.browser_target_close(design(), chat_b()).await,
            tools.browser_target_focus(design(), chat_b()).await,
            tools.browser_target_diagnostics(design(), chat_b()).await,
            tools
                .browser_navigate(
                    Parameters(NavigateRequest {
                        target: Some("design".to_string()),
                        url: "https://example.com/".to_string(),
                    }),
                    chat_b(),
                )
                .await,
        ] {
            let error = error_text(&result);
            assert!(error.starts_with(not_found), "{error}");
        }

        // chat-a reaches its own target; only the stopped browser fails it.
        let error = error_text(&tools.browser_target_close(design(), chat_a()).await);
        assert!(error.contains("browser is not running"), "{error}");

        // Each caller's status reports only its own active target.
        let status_a = tools.browser_status(chat_a()).await;
        assert_eq!(status_a.is_error, Some(false));
        let status_b = tools.browser_status(chat_b()).await;
        let status_b = status_b.structured_content.unwrap();
        assert_eq!(status_b["active_target"], serde_json::Value::Null);
        assert_eq!(status_b["target_id"], serde_json::Value::Null);
    }

    #[test]
    fn tool_surface_marks_inspection_tools_read_only() {
        let temp = tempdir().unwrap();
        let _tools = BrowserMcpTools::new(BrowserEngine::GitTerm(BrowserControlService::new(
            temp.path(),
        )));
        let routes = BrowserMcpTools::tool_router();
        let listed = routes.list_all();
        let find = |name: &str| routes.get(name).unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(listed.len(), 22);
        for name in [
            "browser_status",
            "browser_targets",
            "browser_snapshot",
            "browser_capture",
            "browser_wait_for",
            "browser_console",
            "browser_network",
            "browser_target_diagnostics",
            "browser_dom_outline",
            "browser_node_inspect",
        ] {
            assert_eq!(
                find(name)
                    .annotations
                    .as_ref()
                    .and_then(|value| value.read_only_hint),
                Some(true),
                "{name} must be read-only"
            );
        }
        for name in [
            "browser_open",
            "browser_target_open",
            "browser_target_focus",
            "browser_target_close",
            "browser_navigate",
            "browser_click",
            "browser_type",
            "browser_press",
            "browser_scroll",
            "browser_resize",
            "browser_reload",
            "browser_disconnect",
        ] {
            assert_eq!(
                find(name)
                    .annotations
                    .as_ref()
                    .and_then(|value| value.read_only_hint),
                Some(false),
                "{name} must be mutating"
            );
        }
    }

    #[test]
    fn only_the_agent_browser_engine_lists_its_tools_and_the_list_stays_small() {
        let temp = tempdir().unwrap();
        let listed = |engine: BrowserEngine| {
            let mut tools = BrowserMcpTools::new(engine).tool_router.list_all();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            tools
        };
        let gitterm = listed(BrowserEngine::GitTerm(BrowserControlService::new(
            temp.path(),
        )));
        let agent_browser = listed(BrowserEngine::from_config(
            temp.path(),
            &crate::browser_engine::BrowserEngineConfig {
                engine: BrowserEngineKind::AgentBrowser,
                agent_browser_path: Some(temp.path().join("agent-browser")),
            },
            BrowserControlService::new(temp.path()),
        ));
        let extras = [
            "browser_snapshot_refs",
            "browser_diff",
            "browser_network_capture",
            "browser_viewport",
        ];
        assert_eq!(gitterm.len(), 22);
        assert_eq!(agent_browser.len(), 26);
        assert!(!gitterm
            .iter()
            .any(|tool| extras.contains(&tool.name.as_ref())));
        // The common tools are byte-identical under both engines.
        let common: Vec<_> = agent_browser
            .iter()
            .filter(|tool| !extras.contains(&tool.name.as_ref()))
            .cloned()
            .collect();
        assert_eq!(
            serde_json::to_string(&common).unwrap(),
            serde_json::to_string(&gitterm).unwrap()
        );
        let gitterm_bytes = serde_json::to_vec(&gitterm).unwrap().len();
        let agent_browser_bytes = serde_json::to_vec(&agent_browser).unwrap().len();
        eprintln!(
            "browser tool list: gitterm engine {gitterm_bytes} bytes, agent-browser engine {agent_browser_bytes} bytes"
        );
        assert!(agent_browser_bytes < 40_000, "{agent_browser_bytes}");
    }

    #[test]
    fn codex_commands_receive_ephemeral_config_without_touching_other_agents() {
        let endpoint = "http://127.0.0.1:45678/mcp";
        let configured = configure_codex_command("codex resume --last", endpoint);
        assert!(configured.starts_with("codex --config "));
        assert!(configured.ends_with(" resume --last"));
        assert!(configured.contains(endpoint));
        assert!(configured.contains(BROWSER_MCP_TOKEN_ENV));
        assert_eq!(configure_codex_command("claude", endpoint), "claude");
        assert_eq!(configure_codex_command("pi", endpoint), "pi");
    }

    #[test]
    fn claude_commands_receive_browser_config_with_read_only_tools_pre_approved() {
        let endpoint = "http://127.0.0.1:45678/mcp";
        let configured =
            configure_claude_command("claude --resume abc \"$(cat '/b.md')\"", endpoint);
        assert!(
            configured.starts_with("claude --mcp-config='{"),
            "{configured}"
        );
        assert!(
            configured.ends_with(" --resume abc \"$(cat '/b.md')\""),
            "{configured}"
        );
        assert!(configured.contains("\"url\":\"http://127.0.0.1:45678/mcp\""));
        assert!(configured.contains("\"Authorization\":\"Bearer ${GITTERM_V5_BROWSER_MCP_TOKEN}\""));
        assert!(configured.contains("--allowedTools=mcp__gitterm_browser__browser_status,"));
        assert!(configured.contains("mcp__gitterm_browser__browser_node_inspect"));
        assert!(!configured.contains("mcp__gitterm_browser__browser_click"));
        assert!(!configured.contains("--allowedTools=mcp__gitterm_browser \""));
        assert_eq!(configure_claude_command(&configured, endpoint), configured);
        assert_eq!(configure_claude_command("codex", endpoint), "codex");
        assert_eq!(configure_claude_command("pi", endpoint), "pi");
    }

    #[test]
    fn claude_allowed_tools_match_the_read_only_annotations() {
        let routes = BrowserMcpTools::tool_router() + BrowserMcpTools::agent_browser_tool_router();
        let mut annotated: Vec<String> = routes
            .list_all()
            .into_iter()
            .filter(|tool| {
                tool.annotations
                    .as_ref()
                    .and_then(|value| value.read_only_hint)
                    == Some(true)
            })
            .map(|tool| tool.name.to_string())
            .collect();
        annotated.sort();
        let mut allowed: Vec<String> = CLAUDE_ALLOWED_READ_TOOLS
            .iter()
            .map(|tool| tool.to_string())
            .collect();
        allowed.sort();
        assert_eq!(allowed, annotated);
    }

    #[test]
    fn browser_command_configuration_dispatches_on_the_executable() {
        let endpoint = "http://127.0.0.1:45678/mcp";
        assert_eq!(
            configure_browser_command("codex resume --last", endpoint),
            configure_codex_command("codex resume --last", endpoint)
        );
        assert_eq!(
            configure_browser_command("claude", endpoint),
            configure_claude_command("claude", endpoint)
        );
        assert_eq!(
            configure_browser_command("pi --model x", endpoint),
            "pi --model x"
        );
        assert_eq!(configure_browser_command("", endpoint), "");
    }

    #[test]
    fn manual_codex_zsh_launches_receive_ephemeral_browser_config() {
        let integration = codex_zsh_integration();
        assert!(integration.contains("codex()"));
        assert!(integration.contains(BROWSER_MCP_URL_ENV));
        assert!(integration.contains(BROWSER_MCP_TOKEN_ENV));
        assert!(integration.contains("mcp_servers.gitterm_browser.url="));
        assert!(integration.contains("command codex"));
        assert!(!integration.contains("Bearer "));
    }

    #[tokio::test]
    async fn mcp_tools_publish_agent_operation_activity() {
        let temp = tempdir().unwrap();
        let browser = BrowserControlService::new(temp.path());
        let tools = BrowserMcpTools::new(BrowserEngine::GitTerm(browser.clone()));

        let result = tools
            .browser_status(Extension(request_parts("/mcp?caller=chat-a")))
            .await;
        assert!(!result.is_error.unwrap_or(false));
        let telemetry = browser.telemetry().await;
        assert!(telemetry.active_agent_operation.is_none());
        let outcome = telemetry.last_agent_operation.unwrap();
        assert_eq!(outcome.operation, BrowserOperation::Status);
        assert!(outcome.error.is_none());
    }

    #[tokio::test]
    async fn endpoint_requires_bearer_token_and_serves_the_tool_contract() {
        let temp = tempdir().unwrap();
        let (connection, server) = prepare(temp.path(), "endpoint-test").unwrap();
        let endpoint = connection.endpoint().to_string();
        let token = connection.token.clone();
        let server_task = tokio::spawn(server.run());

        let unauthorized = tokio::time::timeout(
            Duration::from_secs(5),
            ().serve(StreamableHttpClientTransport::from_uri(endpoint.clone())),
        )
        .await
        .expect("unauthorized MCP initialization timed out");
        assert!(unauthorized.is_err());

        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
        );
        let client = tokio::time::timeout(Duration::from_secs(5), ().serve(transport))
            .await
            .expect("authorized MCP initialization timed out")
            .expect("authorized MCP initialization failed");
        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 22);
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "browser_snapshot"));
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "browser_target_open"));
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "browser_capture"));
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "browser_dom_outline"));
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "browser_node_inspect"));
        client.cancel().await.unwrap();

        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("browser MCP server did not shut down")
            .expect("browser MCP task panicked")
            .expect("browser MCP server returned an error");
    }
}
