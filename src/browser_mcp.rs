//! Authenticated loopback MCP exposure for the GitTerm-owned browser controller.

use crate::browser_control::{
    BrowserControlService, BrowserKey, BrowserLaunchOptions, BrowserLocator, BrowserViewport,
    BrowserWaitCondition,
};
use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine;
use rmcp::{
    handler::server::wrapper::Parameters,
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
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const BROWSER_MCP_TOKEN_ENV: &str = "GITTERM_V4_BROWSER_MCP_TOKEN";
pub const BROWSER_MCP_URL_ENV: &str = "GITTERM_V4_BROWSER_MCP_URL";
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;

/// Connection data retained by the app and inherited by local terminals.
///
/// The token is intentionally never serialized or included in `Debug` output.
pub struct BrowserMcpConnection {
    endpoint: String,
    token: String,
    cancellation: CancellationToken,
}

impl BrowserMcpConnection {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn terminal_environment(&self) -> [(String, String); 2] {
        [
            (BROWSER_MCP_URL_ENV.to_string(), self.endpoint.clone()),
            (BROWSER_MCP_TOKEN_ENV.to_string(), self.token.clone()),
        ]
    }

    /// Per-run Codex overrides avoid modifying global or project config files.
    pub fn codex_config_overrides(&self) -> [String; 4] {
        codex_config_overrides(&self.endpoint)
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
        configured.push_str(&value);
    }
    configured.push_str(&trimmed[executable_end..]);
    configured
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
    browser: BrowserControlService,
}

/// Reserve the random loopback endpoint synchronously so terminal environments
/// can be constructed before Iced starts polling asynchronous startup tasks.
pub fn prepare(
    v4_global_config_dir: impl AsRef<Path>,
) -> std::io::Result<(BrowserMcpConnection, BrowserMcpServer)> {
    let listener = std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let cancellation = CancellationToken::new();
    let browser = BrowserControlService::new(v4_global_config_dir);
    Ok((
        BrowserMcpConnection {
            endpoint,
            token: token.clone(),
            cancellation: cancellation.clone(),
        },
        BrowserMcpServer {
            listener,
            token,
            cancellation,
            browser,
        },
    ))
}

impl BrowserMcpServer {
    pub async fn run(self) -> Result<(), String> {
        let listener = tokio::net::TcpListener::from_std(self.listener).map_err(|error| {
            format!("failed to activate the GitTerm V4 browser MCP listener: {error}")
        })?;
        let tools = BrowserMcpTools::new(self.browser);
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
            .map_err(|error| format!("GitTerm V4 browser MCP server stopped: {error}"))
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
    browser: BrowserControlService,
}

impl BrowserMcpTools {
    fn new(browser: BrowserControlService) -> Self {
        Self { browser }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NavigateRequest {
    /// An absolute HTTP or HTTPS URL.
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LocatorRequest {
    /// A strict semantic or CSS locator that must match exactly one visible element.
    locator: BrowserLocator,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeRequest {
    /// A strict locator for one editable visible element.
    locator: BrowserLocator,
    /// Replacement text to enter.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressRequest {
    key: BrowserKey,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollRequest {
    #[serde(default)]
    delta_x: f64,
    delta_y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResizeRequest {
    viewport: BrowserViewport,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReloadRequest {
    #[serde(default)]
    ignore_cache: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitRequest {
    condition: BrowserWaitCondition,
    /// Maximum wait in milliseconds. Defaults to 10000 and is capped at 60000.
    timeout_ms: Option<u64>,
}

#[tool_router]
impl BrowserMcpTools {
    #[tool(
        description = "Report the GitTerm-owned browser process and controlled target status.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn browser_status(&self) -> CallToolResult {
        structured_browser_result(self.browser.status().await)
    }

    #[tool(
        description = "Open the visible Chrome instance using GitTerm V4's isolated browser profile.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_open(&self) -> CallToolResult {
        structured_browser_result(self.browser.launch(BrowserLaunchOptions::default()).await)
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
    ) -> CallToolResult {
        structured_browser_result(self.browser.navigate(&request.url).await)
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
    async fn browser_snapshot(&self) -> CallToolResult {
        match self.browser.snapshot().await {
            Ok(snapshot) => {
                let structured = match serde_json::to_value(&snapshot) {
                    Ok(value) => value,
                    Err(error) => {
                        return tool_error(format!(
                            "failed to serialize browser snapshot state: {error}"
                        ));
                    }
                };
                let text = serde_json::to_string_pretty(&structured)
                    .unwrap_or_else(|_| structured.to_string());
                let image =
                    base64::engine::general_purpose::STANDARD.encode(&snapshot.screenshot_png);
                let mut result = CallToolResult::success(vec![
                    ContentBlock::text(text),
                    ContentBlock::image(image, "image/png"),
                ]);
                result.structured_content = Some(structured);
                result
            }
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
    ) -> CallToolResult {
        structured_browser_result(self.browser.click(&request.locator).await)
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
    async fn browser_type(&self, Parameters(request): Parameters<TypeRequest>) -> CallToolResult {
        structured_browser_result(
            self.browser
                .type_text(&request.locator, &request.text)
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
    async fn browser_press(&self, Parameters(request): Parameters<PressRequest>) -> CallToolResult {
        structured_browser_result(self.browser.press(request.key).await)
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
    ) -> CallToolResult {
        structured_browser_result(self.browser.scroll(request.delta_x, request.delta_y).await)
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
    ) -> CallToolResult {
        structured_browser_result(self.browser.resize(request.viewport).await)
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
    ) -> CallToolResult {
        structured_browser_result(self.browser.reload(request.ignore_cache).await)
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
    ) -> CallToolResult {
        let timeout_ms = request.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        if timeout_ms == 0 || timeout_ms > MAX_WAIT_TIMEOUT_MS {
            return tool_error(format!(
                "browser wait timeout_ms must be between 1 and {MAX_WAIT_TIMEOUT_MS}"
            ));
        }
        structured_browser_result(
            self.browser
                .wait_for(&request.condition, Duration::from_millis(timeout_ms))
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
    async fn browser_console(&self) -> CallToolResult {
        match self.browser.diagnostics().await {
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
    async fn browser_network(&self) -> CallToolResult {
        match self.browser.diagnostics().await {
            Ok(diagnostics) => structured_value(diagnostics.failed_requests),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Immediately disconnect and terminate the GitTerm-owned visible Chrome process.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn browser_disconnect(&self) -> CallToolResult {
        structured_browser_result(self.browser.disconnect().await)
    }
}

#[tool_handler]
impl ServerHandler for BrowserMcpTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gitterm-v4-browser",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Control only the visible Chrome window owned by GitTerm V4. Call browser_open before page operations. Prefer semantic role or text locators from browser_snapshot; CSS is an explicit fallback. Browser actions are serialized. Read-only inspection tools are browser_status, browser_snapshot, browser_wait_for, browser_console, and browser_network. Never use these tools for passwords, cookies, authentication secrets, or unrestricted browser storage.",
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

fn structured_value<T: Serialize>(value: T) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => tool_error(format!("failed to serialize browser tool result: {error}")),
    }
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

    #[test]
    fn connection_uses_ephemeral_loopback_and_memory_only_token_environment() {
        let temp = tempdir().unwrap();
        let (connection, _server) = prepare(temp.path()).unwrap();
        assert!(connection.endpoint().starts_with("http://127.0.0.1:"));
        assert!(connection.endpoint().ends_with("/mcp"));
        let environment = connection.terminal_environment();
        assert_eq!(environment[0].0, BROWSER_MCP_URL_ENV);
        assert_eq!(environment[0].1, connection.endpoint());
        assert_eq!(environment[1].0, BROWSER_MCP_TOKEN_ENV);
        assert_eq!(environment[1].1.len(), 64);
        assert!(!connection
            .codex_config_overrides()
            .iter()
            .any(|value| value.contains(&environment[1].1)));
    }

    #[test]
    fn tool_surface_marks_inspection_tools_read_only() {
        let temp = tempdir().unwrap();
        let _tools = BrowserMcpTools::new(BrowserControlService::new(temp.path()));
        let routes = BrowserMcpTools::tool_router();
        let listed = routes.list_all();
        let find = |name: &str| routes.get(name).unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(listed.len(), 14);
        for name in [
            "browser_status",
            "browser_snapshot",
            "browser_wait_for",
            "browser_console",
            "browser_network",
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

    #[tokio::test]
    async fn endpoint_requires_bearer_token_and_serves_the_tool_contract() {
        let temp = tempdir().unwrap();
        let (connection, server) = prepare(temp.path()).unwrap();
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
        assert_eq!(tools.tools.len(), 14);
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "browser_snapshot"));
        client.cancel().await.unwrap();

        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("browser MCP server did not shut down")
            .expect("browser MCP task panicked")
            .expect("browser MCP server returned an error");
    }
}
