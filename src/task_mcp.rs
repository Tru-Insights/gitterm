//! Authenticated loopback MCP exposure for GitTerm's durable task control plane.
//!
//! The MCP server never opens the task store itself. Every operation crosses a
//! command bridge and is handled by the Iced application event loop, preserving
//! one owner for persistence, worktree provisioning, tabs, and UI state.

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
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
use serde_json::Value;
use std::{
    fmt, io,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const TASK_MCP_TOKEN_ENV: &str = "GITTERM_V5_TASK_MCP_TOKEN";
pub const TASK_MCP_URL_ENV: &str = "GITTERM_V5_TASK_MCP_URL";
const TASK_MCP_BASE_PORT: u16 = 25_030;
const TASK_MCP_PORTS_PER_INSTANCE: u16 = 10;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

pub type TaskControlSender = mpsc::UnboundedSender<TaskControlEnvelope>;
type TaskControlResult = Result<Value, String>;
type TaskControlResponseSender = oneshot::Sender<TaskControlResult>;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CreateTaskRequest {
    /// Short task title visible in GitTerm.
    pub title: String,
    /// Complete worker objective. This is persisted but not automatically typed
    /// into an arbitrary harness in the initial contract.
    pub objective: String,
    /// Absolute path inside the source repository from which to create a worktree.
    pub repository_path: PathBuf,
    /// Optional GitTerm workspace label. Defaults to the repository directory name.
    pub workspace_name: Option<String>,
    /// Optional Linear issue key such as TRU-106.
    pub issue_key: Option<String>,
    /// Branch, tag, or commit to use as the exact task base. Defaults to
    /// `develop` when it exists, then `main`, then the current branch.
    pub base_reference: Option<String>,
    /// Where the worker should stop. Defaults to implement_until_tests_pass.
    pub stopping_boundary: Option<TaskStoppingBoundary>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CreateTaskBatchRequest {
    /// Tasks are created sequentially. Every item returns an explicit result.
    pub tasks: Vec<CreateTaskRequest>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ListTasksRequest {
    /// Include archived tasks. Defaults to false.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetTaskRequest {
    pub task_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct LaunchTaskSessionRequest {
    pub task_id: String,
    /// Configured GitTerm agent-preset name, matched case-insensitively. Omit
    /// this value to launch a plain terminal.
    pub preset_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct UpdateTaskHandoffRequest {
    pub task_id: String,
    /// Concise current-state summary for the next harness or coordinator.
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    /// GitTerm task-session id writing this handoff, when known.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStoppingBoundary {
    PlanOnly,
    ImplementUntilTestsPass,
    PrepareDraftPr,
}

/// A harness-emitted session event crossing the notify bridge, e.g. a
/// Codex `agent-turn-complete`. Task identity comes from the notify URL
/// GitTerm injected at launch, never from the payload.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskSessionEventRequest {
    pub task_id: String,
    pub session_id: String,
    /// The notify payload's `type` field, e.g. "agent-turn-complete".
    pub event_type: String,
}

#[derive(Debug, Clone)]
pub enum TaskControlOperation {
    List(ListTasksRequest),
    Get(GetTaskRequest),
    Create(CreateTaskRequest),
    LaunchSession(LaunchTaskSessionRequest),
    UpdateHandoff(UpdateTaskHandoffRequest),
    SessionEvent(TaskSessionEventRequest),
}

#[derive(Clone)]
pub struct TaskControlReply {
    sender: Arc<Mutex<Option<TaskControlResponseSender>>>,
}

impl fmt::Debug for TaskControlReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskControlReply")
            .finish_non_exhaustive()
    }
}

impl TaskControlReply {
    fn new(sender: TaskControlResponseSender) -> Self {
        Self {
            sender: Arc::new(Mutex::new(Some(sender))),
        }
    }

    pub fn send(&self, result: TaskControlResult) {
        let sender = self.sender.lock().ok().and_then(|mut slot| slot.take());
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskControlEnvelope {
    pub operation: TaskControlOperation,
    pub reply: TaskControlReply,
}

pub struct TaskMcpConnection {
    endpoint: String,
    token: String,
    cancellation: CancellationToken,
}

impl TaskMcpConnection {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn terminal_environment(&self) -> [(String, String); 2] {
        [
            (TASK_MCP_URL_ENV.to_string(), self.endpoint.clone()),
            (TASK_MCP_TOKEN_ENV.to_string(), self.token.clone()),
        ]
    }

    pub fn codex_config_overrides(&self) -> [String; 4] {
        codex_config_overrides(&self.endpoint)
    }

    /// Plain HTTP endpoint for harness notify hooks, on the same
    /// authenticated loopback listener as the MCP service.
    pub fn notify_endpoint(&self) -> String {
        format!("{}/notify", self.endpoint.trim_end_matches("/mcp"))
    }
}

impl Drop for TaskMcpConnection {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub struct TaskMcpServer {
    listener: std::net::TcpListener,
    token: String,
    cancellation: CancellationToken,
    commands: TaskControlSender,
}

pub fn prepare(
    instance_id: &str,
    commands: TaskControlSender,
) -> io::Result<(TaskMcpConnection, TaskMcpServer)> {
    let listener = bind_v5_loopback_listener(instance_id)?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    // An external test driver that launches this instance may pre-share the
    // bearer secret through the app's own environment; whoever sets that env
    // already controls the process. Otherwise the token is random and lives
    // only in memory.
    let token = match std::env::var(TASK_MCP_TOKEN_ENV) {
        Ok(value) if !value.is_empty() => {
            eprintln!("GitTerm V5 task MCP is using the operator-supplied token from {TASK_MCP_TOKEN_ENV}");
            value
        }
        _ => format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
    };
    let cancellation = CancellationToken::new();
    Ok((
        TaskMcpConnection {
            endpoint,
            token: token.clone(),
            cancellation: cancellation.clone(),
        },
        TaskMcpServer {
            listener,
            token,
            cancellation,
            commands,
        },
    ))
}

fn task_mcp_port_range(instance_id: &str) -> std::ops::Range<u16> {
    let instance_slot = instance_id.parse::<u32>().unwrap_or(0).rem_euclid(100) as u16;
    let start = TASK_MCP_BASE_PORT + (instance_slot * TASK_MCP_PORTS_PER_INSTANCE);
    start..(start + TASK_MCP_PORTS_PER_INSTANCE)
}

fn bind_v5_loopback_listener(instance_id: &str) -> io::Result<std::net::TcpListener> {
    let mut last_error = None;
    for port in task_mcp_port_range(instance_id) {
        match std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "GitTerm V5 task MCP port range is empty",
        )
    }))
}

impl TaskMcpServer {
    pub async fn run(self) -> Result<(), String> {
        let listener = tokio::net::TcpListener::from_std(self.listener).map_err(|error| {
            format!("failed to activate the GitTerm V5 task MCP listener: {error}")
        })?;
        let notify_state = NotifyState {
            commands: self.commands.clone(),
        };
        let tools = TaskMcpTools::new(self.commands);
        let mcp_service: StreamableHttpService<TaskMcpTools, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok::<_, io::Error>(tools.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_json_response(true)
                    .with_cancellation_token(self.cancellation.child_token()),
            );
        let auth = Arc::new(BearerAuth { token: self.token });
        let app = Router::new()
            .nest_service("/mcp", mcp_service)
            .route("/notify", axum::routing::post(notify_session_event))
            .with_state(notify_state)
            .layer(middleware::from_fn_with_state(auth, authenticate));
        let shutdown = self.cancellation.cancelled_owned();
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|error| format!("GitTerm V5 task MCP server stopped: {error}"))
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
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

#[derive(Clone)]
struct NotifyState {
    commands: TaskControlSender,
}

#[derive(Deserialize)]
struct NotifyQuery {
    task_id: String,
    session_id: String,
}

/// Receives a harness notify payload (Codex `notify` runs a program with
/// one JSON argument; GitTerm's injected program POSTs it here). Task
/// identity is trusted from the injected URL, the payload only for its
/// `type` — everything else stays opaque.
async fn notify_session_event(
    State(state): State<NotifyState>,
    axum::extract::Query(query): axum::extract::Query<NotifyQuery>,
    body: String,
) -> Response {
    let event_type = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let request = TaskSessionEventRequest {
        task_id: query.task_id,
        session_id: query.session_id,
        event_type,
    };
    match dispatch_operation(&state.commands, TaskControlOperation::SessionEvent(request)).await {
        Ok(value) => (StatusCode::OK, axum::Json(value)).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

async fn dispatch_operation(
    commands: &TaskControlSender,
    operation: TaskControlOperation,
) -> Result<Value, String> {
    let (reply, response) = oneshot::channel();
    commands
        .send(TaskControlEnvelope {
            operation,
            reply: TaskControlReply::new(reply),
        })
        .map_err(|_| "GitTerm's task command bridge is unavailable".to_string())?;
    tokio::time::timeout(COMMAND_TIMEOUT, response)
        .await
        .map_err(|_| "GitTerm timed out while handling the task command".to_string())?
        .map_err(|_| "GitTerm closed the task command without a response".to_string())?
}

#[derive(Clone)]
struct TaskMcpTools {
    commands: TaskControlSender,
}

impl TaskMcpTools {
    fn new(commands: TaskControlSender) -> Self {
        Self { commands }
    }

    async fn dispatch(&self, operation: TaskControlOperation) -> Result<Value, String> {
        dispatch_operation(&self.commands, operation).await
    }
}

#[tool_router]
impl TaskMcpTools {
    #[tool(
        description = "List durable GitTerm tasks and their current state. Use this instead of reading tasks.json directly.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn task_list(&self, Parameters(request): Parameters<ListTasksRequest>) -> CallToolResult {
        task_result(self.dispatch(TaskControlOperation::List(request)).await)
    }

    #[tool(
        description = "Inspect one durable GitTerm task, including its prepared worktree and currently open child sessions.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn task_get(&self, Parameters(request): Parameters<GetTaskRequest>) -> CallToolResult {
        task_result(self.dispatch(TaskControlOperation::Get(request)).await)
    }

    #[tool(
        description = "Create one durable GitTerm task and prepare its isolated git worktree. This does not launch a worker or submit its objective.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn task_create(
        &self,
        Parameters(request): Parameters<CreateTaskRequest>,
    ) -> CallToolResult {
        task_result(self.dispatch(TaskControlOperation::Create(request)).await)
    }

    #[tool(
        description = "Create several durable GitTerm tasks sequentially. Results preserve input order and explicitly report partial failures.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn task_create_batch(
        &self,
        Parameters(request): Parameters<CreateTaskBatchRequest>,
    ) -> CallToolResult {
        if request.tasks.is_empty() {
            return tool_error("task_create_batch requires at least one task");
        }
        let mut results = Vec::with_capacity(request.tasks.len());
        for task in request.tasks {
            let title = task.title.clone();
            match self.dispatch(TaskControlOperation::Create(task)).await {
                Ok(value) => results.push(serde_json::json!({
                    "title": title,
                    "ok": true,
                    "task": value,
                })),
                Err(error) => results.push(serde_json::json!({
                    "title": title,
                    "ok": false,
                    "error": error,
                })),
            }
        }
        CallToolResult::structured(serde_json::json!({ "results": results }))
    }

    #[tool(
        description = "Open a configured agent preset or plain terminal as a child session in a prepared task worktree. The initial contract launches the harness but does not automatically submit the task objective.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn task_launch_session(
        &self,
        Parameters(request): Parameters<LaunchTaskSessionRequest>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::LaunchSession(request))
                .await,
        )
    }

    #[tool(
        description = "Persist a concise task handoff for the coordinator or a later harness. Record current state, durable decisions, next steps, and blockers; do not copy a full transcript.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn task_update_handoff(
        &self,
        Parameters(request): Parameters<UpdateTaskHandoffRequest>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::UpdateHandoff(request))
                .await,
        )
    }
}

#[tool_handler]
impl ServerHandler for TaskMcpTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gitterm-v5-tasks",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "These tools control GitTerm's durable tasks. A task is an isolated job with its own git worktree; a task session is one visible harness or terminal inside that task. Use task_create_batch for an approved set of independent issues, then task_launch_session for each worker. Task creation and session launch are distinct: launching currently opens the harness but does not type or submit the stored objective. Before stopping or handing work to another harness, use task_update_handoff to record a concise durable summary, decisions, next steps, and blockers. Never edit GitTerm's tasks.json or worktree registry directly.",
            )
    }
}

pub fn configure_codex_command(command: &str, endpoint: &str) -> String {
    let trimmed = command.trim();
    let executable_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let executable = &trimmed[..executable_end];
    let executable_name = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
    if executable_name != "codex" || trimmed.contains("mcp_servers.gitterm_tasks.url=") {
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

/// Append a Codex `notify` override so this task session's
/// agent-turn-complete events reach the live instance. The bearer token
/// is resolved from the terminal environment at event time and never
/// appears in the command line. Overrides any user-level `notify`
/// (Codex config holds a single value) for GitTerm task sessions only.
pub fn configure_codex_notify(
    command: &str,
    notify_endpoint: &str,
    task_id: &str,
    session_id: &str,
) -> String {
    let trimmed = command.trim();
    let executable_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let executable = &trimmed[..executable_end];
    let executable_name = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
    if executable_name != "codex" || trimmed.contains("notify=[") {
        return command.to_string();
    }
    let url = format!("{notify_endpoint}?task_id={task_id}&session_id={session_id}");
    // Codex appends the JSON payload as the program's last argument;
    // with `sh -c` and one placeholder arg it lands in `$1`.
    let script = format!(
        "curl -fsS -m 5 -X POST -H \"Authorization: Bearer ${TASK_MCP_TOKEN_ENV}\" -H \"Content-Type: application/json\" --data-binary \"$1\" \"{url}\" >/dev/null 2>&1 || true"
    );
    // JSON string escaping is valid TOML basic-string escaping for this
    // character set; the single quotes keep the value one shell word.
    let elements = ["/bin/sh", "-c", script.as_str(), "gitterm-codex-notify"]
        .iter()
        .map(|element| Value::String((*element).to_string()).to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{executable} --config 'notify=[{elements}]'{rest}",
        rest = &trimmed[executable_end..]
    )
}

fn codex_config_overrides(endpoint: &str) -> [String; 4] {
    [
        format!("mcp_servers.gitterm_tasks.url={endpoint}"),
        format!("mcp_servers.gitterm_tasks.bearer_token_env_var={TASK_MCP_TOKEN_ENV}"),
        "mcp_servers.gitterm_tasks.default_tools_approval_mode=writes".to_string(),
        "mcp_servers.gitterm_tasks.tool_timeout_sec=300".to_string(),
    ]
}

/// One zsh wrapper composes both GitTerm-owned MCP servers. It is safe when
/// either service is unavailable and never writes global Codex configuration.
pub fn codex_zsh_integration() -> String {
    format!(
        r#"if [[ ( -n "${{{browser_url}:-}}" && -n "${{{browser_token}:-}}" ) || ( -n "${{{task_url}:-}}" && -n "${{{task_token}:-}}" ) ]]; then
    codex() {{
        local arg
        for arg in "$@"; do
            if [[ "$arg" == mcp_servers.gitterm_browser.url=* || "$arg" == mcp_servers.gitterm_tasks.url=* ]]; then
                command codex "$@"
                return
            fi
        done
        local -a gitterm_mcp_args
        if [[ -n "${{{browser_url}:-}}" && -n "${{{browser_token}:-}}" ]]; then
            gitterm_mcp_args+=(
                --config "mcp_servers.gitterm_browser.url=${{{browser_url}}}"
                --config "mcp_servers.gitterm_browser.bearer_token_env_var={browser_token}"
                --config "mcp_servers.gitterm_browser.default_tools_approval_mode=writes"
                --config "mcp_servers.gitterm_browser.tool_timeout_sec=60"
            )
        fi
        if [[ -n "${{{task_url}:-}}" && -n "${{{task_token}:-}}" ]]; then
            gitterm_mcp_args+=(
                --config "mcp_servers.gitterm_tasks.url=${{{task_url}}}"
                --config "mcp_servers.gitterm_tasks.bearer_token_env_var={task_token}"
                --config "mcp_servers.gitterm_tasks.default_tools_approval_mode=writes"
                --config "mcp_servers.gitterm_tasks.tool_timeout_sec=300"
            )
        fi
        command codex "${{gitterm_mcp_args[@]}}" "$@"
    }}
fi"#,
        browser_url = crate::browser_mcp::BROWSER_MCP_URL_ENV,
        browser_token = crate::browser_mcp::BROWSER_MCP_TOKEN_ENV,
        task_url = TASK_MCP_URL_ENV,
        task_token = TASK_MCP_TOKEN_ENV,
    )
}

fn task_result(result: Result<Value, String>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => tool_error(error),
    }
}

fn tool_error(error: impl fmt::Display) -> CallToolResult {
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

    #[test]
    fn connection_uses_v5_task_range_and_memory_only_token_environment() {
        let (commands, _requests) = mpsc::unbounded_channel();
        let (connection, _server) = prepare("connection-test", commands).unwrap();
        let port = connection
            .endpoint()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/mcp")
            .parse::<u16>()
            .unwrap();
        assert!((TASK_MCP_BASE_PORT..26_030).contains(&port));
        assert!(!(14_030..15_030).contains(&port));
        let environment = connection.terminal_environment();
        assert_eq!(environment[0].0, TASK_MCP_URL_ENV);
        assert_eq!(environment[1].0, TASK_MCP_TOKEN_ENV);
        assert_eq!(environment[1].1.len(), 64);
        assert!(!connection
            .codex_config_overrides()
            .iter()
            .any(|value| value.contains(&environment[1].1)));
    }

    #[test]
    fn tool_surface_separates_read_and_write_operations() {
        let routes = TaskMcpTools::tool_router();
        assert_eq!(routes.list_all().len(), 6);
        for name in ["task_list", "task_get"] {
            assert_eq!(
                routes
                    .get(name)
                    .unwrap()
                    .annotations
                    .as_ref()
                    .and_then(|value| value.read_only_hint),
                Some(true)
            );
        }
        for name in [
            "task_create",
            "task_create_batch",
            "task_launch_session",
            "task_update_handoff",
        ] {
            assert_eq!(
                routes
                    .get(name)
                    .unwrap()
                    .annotations
                    .as_ref()
                    .and_then(|value| value.read_only_hint),
                Some(false)
            );
        }
    }

    #[test]
    fn codex_integration_composes_browser_and_task_servers() {
        let configured = configure_codex_command("codex resume --last", "http://127.0.0.1:1/mcp");
        assert!(configured.contains("mcp_servers.gitterm_tasks.url="));
        assert!(configured.ends_with(" resume --last"));
        assert_eq!(configure_codex_command("claude", "unused"), "claude");
        let integration = codex_zsh_integration();
        assert!(integration.contains("mcp_servers.gitterm_browser.url="));
        assert!(integration.contains("mcp_servers.gitterm_tasks.url="));
        assert!(integration.contains("gitterm_mcp_args"));
        assert!(!integration.contains("Bearer "));
    }

    #[test]
    fn codex_notify_injection_targets_codex_and_keeps_the_token_out_of_the_command() {
        let configured = configure_codex_notify(
            "codex resume abc123",
            "http://127.0.0.1:25030/notify",
            "task-1",
            "session-1",
        );
        assert!(configured.starts_with("codex --config 'notify=[\"/bin/sh\",\"-c\","));
        assert!(configured.ends_with("' resume abc123"));
        assert!(configured.contains("task_id=task-1&session_id=session-1"));
        // The token is read from the terminal environment at event time;
        // only the variable name may appear in the command line.
        assert!(configured.contains(&format!("${TASK_MCP_TOKEN_ENV}")));
        // Exactly the wrapping quote pair, so the zsh eval startup path
        // keeps the TOML override as one shell word.
        assert_eq!(configured.matches('\'').count(), 2);
        assert_eq!(
            configure_codex_notify(&configured, "http://127.0.0.1:25030/notify", "t", "s"),
            configured
        );
        assert_eq!(
            configure_codex_notify("claude --resume abc", "unused", "t", "s"),
            "claude --resume abc"
        );
        assert!(configure_codex_notify(
            "/opt/homebrew/bin/codex --model gpt-5",
            "http://127.0.0.1:25030/notify",
            "t",
            "s"
        )
        .starts_with("/opt/homebrew/bin/codex --config 'notify=["));
    }

    #[tokio::test]
    async fn notify_route_requires_auth_and_bridges_session_events() {
        let (commands, mut requests) = mpsc::unbounded_channel();
        let (connection, server) = prepare("notify-test", commands).unwrap();
        let url = format!(
            "{}?task_id=task-9&session_id=session-9",
            connection.notify_endpoint()
        );
        let token = connection.token.clone();
        let server_task = tokio::spawn(server.run());
        let client = hyper::Client::new();
        let payload = r#"{"type":"agent-turn-complete","turn-id":"t1"}"#;

        let unauthorized = client
            .request(
                hyper::Request::post(url.as_str())
                    .body(hyper::Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), hyper::StatusCode::UNAUTHORIZED);

        let responder = tokio::spawn(async move {
            let envelope = requests.recv().await.expect("missing bridged notify");
            match envelope.operation {
                TaskControlOperation::SessionEvent(request) => {
                    assert_eq!(request.task_id, "task-9");
                    assert_eq!(request.session_id, "session-9");
                    assert_eq!(request.event_type, "agent-turn-complete");
                }
                other => panic!("unexpected bridged operation: {other:?}"),
            }
            envelope.reply.send(Ok(serde_json::json!({ "ok": true })));
        });
        let authorized = client
            .request(
                hyper::Request::post(url.as_str())
                    .header("Authorization", format!("Bearer {token}"))
                    .body(hyper::Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), hyper::StatusCode::OK);
        responder.await.unwrap();

        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("task MCP server did not shut down")
            .expect("task MCP task panicked")
            .expect("task MCP server returned an error");
    }

    #[tokio::test]
    async fn endpoint_requires_auth_and_forwards_commands_to_the_app_bridge() {
        let (commands, mut requests) = mpsc::unbounded_channel();
        let (connection, server) = prepare("endpoint-test", commands).unwrap();
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

        let responder = tokio::spawn(async move {
            let envelope = requests.recv().await.expect("missing bridged request");
            assert!(matches!(envelope.operation, TaskControlOperation::List(_)));
            envelope.reply.send(Ok(serde_json::json!({ "tasks": [] })));
        });
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
        );
        let client = tokio::time::timeout(Duration::from_secs(5), ().serve(transport))
            .await
            .expect("authorized MCP initialization timed out")
            .expect("authorized MCP initialization failed");
        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 6);
        let result = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new("task_list").with_arguments(
                    serde_json::Map::from_iter([(
                        "include_archived".to_string(),
                        Value::Bool(false),
                    )]),
                ),
            )
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        responder.await.unwrap();
        client.cancel().await.unwrap();

        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("task MCP server did not shut down")
            .expect("task MCP task panicked")
            .expect("task MCP server returned an error");
    }
}
