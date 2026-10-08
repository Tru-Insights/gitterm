//! Authenticated loopback MCP exposure for GitTerm's durable task control plane.
//!
//! The MCP server never opens the task store itself. Every operation crosses a
//! command bridge and is handled by the Iced application event loop, preserving
//! one owner for persistence, worktree provisioning, tabs, and UI state.

use crate::harness::claude::ClaudeMcpServer;
use axum::{
    body::Body,
    extract::State,
    http::{header, request::Parts, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use rmcp::{
    handler::server::{tool::Extension, wrapper::Parameters},
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
/// Query parameter on a tab's task MCP URL naming the calling tab's durable
/// `session_uid`. The bearer token authenticates; this only attributes.
pub const TASK_MCP_CALLER_QUERY: &str = "caller";
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
    /// GitTerm task-session id writing this handoff, when known. Omit it to
    /// attribute the handoff to the calling GitTerm tab.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStoppingBoundary {
    PlanOnly,
    ImplementUntilTestsPass,
    PrepareDraftPr,
}

/// What `review_request` reviews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTargetKind {
    /// Staged, unstaged and untracked changes against HEAD.
    Uncommitted,
    /// The merge base with `base_ref` against the working tree, so it
    /// includes uncommitted changes (Codex `--base`).
    Base,
    /// One commit, named by `commit`.
    Commit,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ReviewDelegationRequest {
    /// `uncommitted`, `base` (needs `base_ref`) or `commit` (needs `commit`).
    pub target: ReviewTargetKind,
    /// Branch or ref to diff against, for target `base`.
    pub base_ref: Option<String>,
    /// Commit SHA (7 to 40 hex characters), for target `commit`.
    pub commit: Option<String>,
    /// Optional extra instructions for the reviewer.
    pub focus: Option<String>,
    /// Only `codex` is supported; omit it for Codex. For a Claude review,
    /// spawn a review subagent with your own Agent tool instead.
    pub reviewer: Option<String>,
    /// Codex model (`codex -m`). Omit it for GitTerm's configured Codex model.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ConsultDelegationRequest {
    /// The full question or brief for the consultant: context, what to
    /// decide, and the answer shape you want. It runs read-only in your
    /// checkout and cannot see this conversation.
    pub brief: String,
    /// Codex model (`codex -m`). Omit it for GitTerm's configured Codex model.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetDelegationRequest {
    pub delegation_id: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ListDelegationsRequest {
    /// Only delegations in this state: requested, running, completed,
    /// failed, interrupted or cancelled.
    pub status: Option<String>,
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
    /// Delegation tools (TRU-142). The bridge only receives these with a
    /// caller: the tool handlers refuse calls without one.
    RequestReview(ReviewDelegationRequest),
    RequestConsult(ConsultDelegationRequest),
    GetDelegation(GetDelegationRequest),
    ListDelegations(ListDelegationsRequest),
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
    /// The calling tab's `session_uid`, read from the `caller` query of the
    /// MCP URL GitTerm handed that tab. `None` for calls from a URL without
    /// it (bottom-panel terminals, external clients, the notify route).
    pub caller: Option<String>,
    pub reply: TaskControlReply,
}

/// The task MCP URL handed to one tab: the shared endpoint plus
/// `?caller=<session_uid>`. Without a caller the bare endpoint is returned.
pub fn caller_endpoint(endpoint: &str, caller: Option<&str>) -> String {
    match caller.filter(|caller| !caller.is_empty()) {
        Some(caller) => {
            let encoded: String = url::form_urlencoded::byte_serialize(caller.as_bytes()).collect();
            format!("{endpoint}?{TASK_MCP_CALLER_QUERY}={encoded}")
        }
        None => endpoint.to_string(),
    }
}

/// The caller named by a request URI's `caller` query, if any.
fn caller_from_query(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(key, _)| key == TASK_MCP_CALLER_QUERY)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
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

    /// The task MCP variables for one tab's terminal. The URL carries the
    /// tab's caller identity; the token is the same for every tab and lives
    /// only in the environment.
    pub fn terminal_environment(&self, caller: Option<&str>) -> [(String, String); 2] {
        [
            (
                TASK_MCP_URL_ENV.to_string(),
                caller_endpoint(&self.endpoint, caller),
            ),
            (TASK_MCP_TOKEN_ENV.to_string(), self.token.clone()),
        ]
    }

    pub fn codex_config_overrides(&self) -> [String; 4] {
        codex_config_overrides(&self.endpoint)
    }

    /// The task server for a natively spawned Claude session (chat tabs):
    /// the same config and pre-approval `configure_claude_command` injects
    /// into terminal launches, with the token carried in the child
    /// environment. `caller` is the chat tab's `session_uid`.
    pub fn claude_mcp_server(&self, caller: Option<&str>) -> ClaudeMcpServer {
        ClaudeMcpServer {
            config: claude_mcp_config(&caller_endpoint(&self.endpoint, caller)),
            allowed_tools: vec![CLAUDE_ALLOWED_TOOLS.to_string()],
            env: self.terminal_environment(caller).into(),
        }
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
    match dispatch_operation(
        &state.commands,
        TaskControlOperation::SessionEvent(request),
        None,
    )
    .await
    {
        Ok(value) => (StatusCode::OK, axum::Json(value)).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

async fn dispatch_operation(
    commands: &TaskControlSender,
    operation: TaskControlOperation,
    caller: Option<String>,
) -> Result<Value, String> {
    let (reply, response) = oneshot::channel();
    commands
        .send(TaskControlEnvelope {
            operation,
            caller,
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

    /// Bridge one operation, attributed to the caller named on the HTTP
    /// request's URL (rmcp injects the request `Parts` into every call).
    async fn dispatch(
        &self,
        operation: TaskControlOperation,
        parts: &Parts,
    ) -> Result<Value, String> {
        dispatch_operation(
            &self.commands,
            operation,
            caller_from_query(parts.uri.query()),
        )
        .await
    }

    /// Like `dispatch`, for tools that act on behalf of the calling tab:
    /// a call without a caller is refused before it reaches GitTerm.
    async fn dispatch_for_caller(
        &self,
        tool: &str,
        operation: TaskControlOperation,
        parts: &Parts,
    ) -> CallToolResult {
        let Some(caller) = caller_from_query(parts.uri.query()) else {
            return tool_error(missing_caller_error(tool));
        };
        task_result(dispatch_operation(&self.commands, operation, Some(caller)).await)
    }
}

/// Why a delegation tool refused a call that named no calling tab.
pub fn missing_caller_error(tool: &str) -> String {
    format!(
        "{tool} needs the calling GitTerm tab's identity, and this call has none. Delegations \
         report back to the tab that asked, so call it through the task MCP URL GitTerm gave \
         that tab (it ends in ?{TASK_MCP_CALLER_QUERY}=<session id>)."
    )
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
    async fn task_list(
        &self,
        Parameters(request): Parameters<ListTasksRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::List(request), &parts)
                .await,
        )
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
    async fn task_get(
        &self,
        Parameters(request): Parameters<GetTaskRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::Get(request), &parts)
                .await,
        )
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
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::Create(request), &parts)
                .await,
        )
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
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        if request.tasks.is_empty() {
            return tool_error("task_create_batch requires at least one task");
        }
        let mut results = Vec::with_capacity(request.tasks.len());
        for task in request.tasks {
            let title = task.title.clone();
            match self
                .dispatch(TaskControlOperation::Create(task), &parts)
                .await
            {
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
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::LaunchSession(request), &parts)
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
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        task_result(
            self.dispatch(TaskControlOperation::UpdateHandoff(request), &parts)
                .await,
        )
    }

    #[tool(
        description = "Ask Codex to review code in your GitTerm tab's checkout, in the background. Returns {delegation_id} at once; the review takes a minute or more. Do not wait or poll for it: end your turn. The findings appear as a card in the requesting chat tab, and the human sends the ones they want back to you. Read the full result any time with delegation_get. Targets: uncommitted, base (with base_ref; includes uncommitted changes) or commit (with commit). Only reviewer \"codex\" is supported; for a Claude review, spawn a review subagent with your own Agent tool.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn review_request(
        &self,
        Parameters(request): Parameters<ReviewDelegationRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        self.dispatch_for_caller(
            "review_request",
            TaskControlOperation::RequestReview(request),
            &parts,
        )
        .await
    }

    #[tool(
        description = "Ask Codex for advice (brainstorming, architecture, investigation) on a self-contained brief. It runs read-only in your GitTerm tab's checkout, in the background, and cannot see this conversation. Returns {delegation_id} at once; do not wait or poll for it. The answer appears as a card in the requesting chat tab; read it with delegation_get.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn consult_request(
        &self,
        Parameters(request): Parameters<ConsultDelegationRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        self.dispatch_for_caller(
            "consult_request",
            TaskControlOperation::RequestConsult(request),
            &parts,
        )
        .await
    }

    #[tool(
        description = "Read one delegation (a Codex review or consult) with its status and, once completed, its result: review findings with ids F1, F2, ..., severity, file and lines, or the consult's answer.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn delegation_get(
        &self,
        Parameters(request): Parameters<GetDelegationRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        self.dispatch_for_caller(
            "delegation_get",
            TaskControlOperation::GetDelegation(request),
            &parts,
        )
        .await
    }

    #[tool(
        description = "List the delegations your GitTerm tab requested, newest first, optionally only those in one status (requested, running, completed, failed, interrupted, cancelled).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn delegation_list(
        &self,
        Parameters(request): Parameters<ListDelegationsRequest>,
        Extension(parts): Extension<Parts>,
    ) -> CallToolResult {
        self.dispatch_for_caller(
            "delegation_list",
            TaskControlOperation::ListDelegations(request),
            &parts,
        )
        .await
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
                "These tools control GitTerm's durable tasks. A task is an isolated job with its own git worktree; a task session is one visible harness or terminal inside that task. Use task_create_batch for an approved set of independent issues, then task_launch_session for each worker. Task creation and session launch are distinct: launching an agent preset opens the harness with the stored objective and latest handoff delivered as its initial prompt; a plain terminal session receives nothing. Before stopping or handing work to another harness, use task_update_handoff to record a concise durable summary, decisions, next steps, and blockers. Never edit GitTerm's tasks.json or worktree registry directly. For a second opinion from Codex, review_request (code review) and consult_request (advice) run in the background and return a delegation id at once; do not wait for them, the result reaches the requesting chat tab and delegation_get reads it.",
            )
    }
}

/// Split a launch command into its executable, the executable's basename and
/// the untouched remainder (leading whitespace included).
fn split_executable(command: &str) -> (&str, &str, &str) {
    let trimmed = command.trim();
    let executable_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let executable = &trimmed[..executable_end];
    let executable_name = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
    (executable, executable_name, &trimmed[executable_end..])
}

/// Attach the task MCP server to whichever harness this launch command
/// starts. Codex and Claude get per-process configuration; pi discovers the
/// endpoint from the terminal environment through its `gitterm-mcp`
/// extension (see `pi-extensions/`). Other commands pass through unchanged.
pub fn configure_task_command(command: &str, endpoint: &str) -> String {
    match split_executable(command).1 {
        "codex" => configure_codex_command(command, endpoint),
        "claude" => configure_claude_command(command, endpoint),
        _ => command.to_string(),
    }
}

pub fn configure_codex_command(command: &str, endpoint: &str) -> String {
    let (executable, executable_name, rest) = split_executable(command);
    if executable_name != "codex" || command.contains("mcp_servers.gitterm_tasks.url=") {
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
    configured.push_str(rest);
    configured
}

/// Claude Code takes ad-hoc servers through `--mcp-config`. The `=` form is
/// deliberate: the option is variadic, and the space form would swallow the
/// positional brief that task launches append. The bearer header uses
/// Claude's own `${VAR}` expansion so the token never appears in the command
/// line or the persisted startup command. The server's tools are
/// pre-approved (`mcp__gitterm_tasks` covers every tool it serves): they are
/// GitTerm-owned, and a task session must be able to record handoffs and
/// launch workers without a human answering permission prompts.
pub fn configure_claude_command(command: &str, endpoint: &str) -> String {
    let (executable, executable_name, rest) = split_executable(command);
    if executable_name != "claude" || command.contains("\"gitterm_tasks\"") {
        return command.to_string();
    }
    let config = claude_mcp_config(endpoint).to_string();
    if config.contains('\'') {
        // The JSON is embedded single-quoted; an endpoint that would break
        // that quoting is not one GitTerm produces, so leave the launch alone.
        return command.to_string();
    }
    format!("{executable} --mcp-config='{config}' --allowedTools={CLAUDE_ALLOWED_TOOLS}{rest}")
}

const CLAUDE_ALLOWED_TOOLS: &str = "mcp__gitterm_tasks";

fn claude_mcp_config(endpoint: &str) -> Value {
    serde_json::json!({
        "mcpServers": {
            "gitterm_tasks": {
                "type": "http",
                "url": endpoint,
                "headers": {
                    "Authorization": format!("Bearer ${{{TASK_MCP_TOKEN_ENV}}}"),
                },
            },
        },
    })
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
    let (executable, executable_name, rest) = split_executable(command);
    if executable_name != "codex" || command.contains("notify=[") {
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
    format!("{executable} --config 'notify=[{elements}]'{rest}")
}

/// Task tools are approved outright (Claude gets the equivalent
/// `--allowedTools`): a task session must record handoffs and launch workers
/// without a human answering prompts. The browser server keeps `writes`.
fn codex_config_overrides(endpoint: &str) -> [String; 4] {
    [
        format!("mcp_servers.gitterm_tasks.url={endpoint}"),
        format!("mcp_servers.gitterm_tasks.bearer_token_env_var={TASK_MCP_TOKEN_ENV}"),
        "mcp_servers.gitterm_tasks.default_tools_approval_mode=approve".to_string(),
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
                --config "mcp_servers.gitterm_tasks.default_tools_approval_mode=approve"
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
        let environment = connection.terminal_environment(None);
        assert_eq!(environment[0].0, TASK_MCP_URL_ENV);
        assert_eq!(environment[0].1, connection.endpoint());
        assert_eq!(environment[1].0, TASK_MCP_TOKEN_ENV);
        assert_eq!(environment[1].1.len(), 64);
        assert!(!connection
            .codex_config_overrides()
            .iter()
            .any(|value| value.contains(&environment[1].1)));

        // Chat tabs get the terminal launch's config and pre-approvals, with
        // the token only in the child environment.
        let server = connection.claude_mcp_server(None);
        let config = server.config.to_string();
        assert!(config.contains("\"gitterm_tasks\""));
        assert!(!config.contains(&environment[1].1));
        let terminal = configure_claude_command("claude", connection.endpoint());
        assert!(terminal.contains(&format!("--mcp-config='{config}'")));
        assert!(terminal.contains(&format!(
            "--allowedTools={}",
            server.allowed_tools.join(",")
        )));
        assert_eq!(server.env, environment.to_vec());
    }

    #[test]
    fn tool_surface_separates_read_and_write_operations() {
        let routes = TaskMcpTools::tool_router();
        assert_eq!(routes.list_all().len(), 10);
        for name in ["task_list", "task_get", "delegation_get", "delegation_list"] {
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
            "review_request",
            "consult_request",
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
    fn claude_injection_uses_single_value_mcp_config_and_keeps_the_token_out_of_the_command() {
        let endpoint = "http://127.0.0.1:25031/mcp";
        let configured =
            configure_claude_command("claude --resume abc \"$(cat '/b.md')\"", endpoint);
        assert!(
            configured.starts_with("claude --mcp-config='{"),
            "{configured}"
        );
        assert!(
            configured
                .ends_with("}' --allowedTools=mcp__gitterm_tasks --resume abc \"$(cat '/b.md')\""),
            "{configured}"
        );
        assert!(!configured.contains("--mcp-config '"));
        assert!(configured.contains("\"url\":\"http://127.0.0.1:25031/mcp\""));
        assert!(configured.contains("\"Authorization\":\"Bearer ${GITTERM_V5_TASK_MCP_TOKEN}\""));
        assert!(!configured.contains("secret"));
        assert_eq!(configure_claude_command(&configured, endpoint), configured);
        assert_eq!(configure_claude_command("codex", endpoint), "codex");
        assert_eq!(
            configure_claude_command("pi 'hello'", endpoint),
            "pi 'hello'"
        );
        assert_eq!(
            configure_claude_command("/opt/bin/claude", endpoint),
            format!(
                "/opt/bin/claude --mcp-config='{}' --allowedTools=mcp__gitterm_tasks",
                claude_mcp_config(endpoint)
            )
        );
    }

    #[test]
    fn every_injection_path_carries_the_caller_and_keeps_the_token_in_the_environment() {
        let (commands, _requests) = mpsc::unbounded_channel();
        let (connection, _server) = prepare("caller-injection-test", commands).unwrap();
        let caller = "7d1c2a4e-0b3f-4e8a-9c55-2f1e6d7a8b90";
        let expected_url = format!("{}?caller={caller}", connection.endpoint());
        let token = connection.token.clone();

        // Terminal tabs: the per-tab environment.
        let environment = connection.terminal_environment(Some(caller));
        assert_eq!(
            environment[0],
            (TASK_MCP_URL_ENV.to_string(), expected_url.clone())
        );
        assert_eq!(
            environment[1],
            (TASK_MCP_TOKEN_ENV.to_string(), token.clone())
        );

        // Claude and Codex terminal launches read the URL from that
        // environment (`build_terminal_settings`).
        let claude = configure_task_command("claude", &environment[0].1);
        assert!(
            claude.contains(&format!("\"url\":\"{expected_url}\"")),
            "{claude}"
        );
        assert!(!claude.contains(&token));
        let codex = configure_task_command("codex resume --last", &environment[0].1);
        // Quoted: zsh would glob the bare `?` and abort the launch.
        assert!(
            codex.contains(&format!(
                " --config 'mcp_servers.gitterm_tasks.url={expected_url}' "
            )),
            "{codex}"
        );
        assert!(codex.ends_with(" resume --last"), "{codex}");
        assert!(!codex.contains(&token));

        // Native chat tabs: the spawned Claude's MCP config.
        let server = connection.claude_mcp_server(Some(caller));
        assert_eq!(
            server.config["mcpServers"]["gitterm_tasks"]["url"],
            Value::String(expected_url.clone())
        );
        assert!(!server.config.to_string().contains(&token));
        assert_eq!(server.env, environment.to_vec());

        // No caller: the bare endpoint, unchanged launch shapes.
        assert_eq!(
            caller_endpoint(connection.endpoint(), None),
            connection.endpoint()
        );
        assert_eq!(
            caller_endpoint(connection.endpoint(), Some("")),
            connection.endpoint()
        );
        assert!(!configure_codex_command("codex", connection.endpoint()).contains('\''));
        assert_eq!(
            caller_from_query(Some("caller=abc")),
            Some("abc".to_string())
        );
        assert_eq!(
            caller_from_query(Some("other=1&caller=a%20b")),
            Some("a b".to_string())
        );
        assert_eq!(caller_from_query(Some("caller=")), None);
        assert_eq!(caller_from_query(None), None);
    }

    #[test]
    fn task_command_configuration_dispatches_on_the_executable() {
        let endpoint = "http://127.0.0.1:25031/mcp";
        assert_eq!(
            configure_task_command("codex resume --last", endpoint),
            configure_codex_command("codex resume --last", endpoint)
        );
        assert_eq!(
            configure_task_command("claude", endpoint),
            configure_claude_command("claude", endpoint)
        );
        assert_eq!(
            configure_task_command("pi --model x", endpoint),
            "pi --model x"
        );
        assert_eq!(configure_task_command("", endpoint), "");
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
            envelope.caller
        });
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
        );
        let client = tokio::time::timeout(Duration::from_secs(5), ().serve(transport))
            .await
            .expect("authorized MCP initialization timed out")
            .expect("authorized MCP initialization failed");
        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 10);
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
        assert_eq!(responder.await.unwrap(), None);
        client.cancel().await.unwrap();

        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("task MCP server did not shut down")
            .expect("task MCP task panicked")
            .expect("task MCP server returned an error");
    }

    #[tokio::test]
    async fn caller_query_reaches_the_bridged_envelope() {
        let (commands, mut requests) = mpsc::unbounded_channel();
        let (connection, server) = prepare("caller-test", commands).unwrap();
        let endpoint = caller_endpoint(connection.endpoint(), Some("abc"));
        let token = connection.token.clone();
        let server_task = tokio::spawn(server.run());

        let responder = tokio::spawn(async move {
            let mut callers = Vec::new();
            for _ in 0..2 {
                let envelope = requests.recv().await.expect("missing bridged request");
                let operation = match &envelope.operation {
                    TaskControlOperation::List(_) => "list",
                    TaskControlOperation::UpdateHandoff(_) => "handoff",
                    other => panic!("unexpected bridged operation: {other:?}"),
                };
                callers.push((operation, envelope.caller.clone()));
                envelope.reply.send(Ok(serde_json::json!({ "tasks": [] })));
            }
            callers
        });
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
        );
        let client = tokio::time::timeout(Duration::from_secs(5), ().serve(transport))
            .await
            .expect("MCP initialization timed out")
            .expect("MCP initialization failed");
        let list = client
            .call_tool(rmcp::model::CallToolRequestParams::new("task_list"))
            .await
            .unwrap();
        assert!(!list.is_error.unwrap_or(false));
        let handoff = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new("task_update_handoff").with_arguments(
                    serde_json::Map::from_iter([
                        ("task_id".to_string(), Value::from("task-1")),
                        ("summary".to_string(), Value::from("done")),
                    ]),
                ),
            )
            .await
            .unwrap();
        assert!(!handoff.is_error.unwrap_or(false));
        assert_eq!(
            responder.await.unwrap(),
            vec![
                ("list", Some("abc".to_string())),
                ("handoff", Some("abc".to_string())),
            ]
        );
        client.cancel().await.unwrap();

        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("task MCP server did not shut down")
            .expect("task MCP task panicked")
            .expect("task MCP server returned an error");
    }

    fn call(name: &str, arguments: Value) -> rmcp::model::CallToolRequestParams {
        let Value::Object(arguments) = arguments else {
            panic!("arguments must be an object");
        };
        rmcp::model::CallToolRequestParams::new(name.to_string()).with_arguments(arguments)
    }

    fn structured(result: &CallToolResult) -> Value {
        assert!(
            !result.is_error.unwrap_or(false),
            "tool failed: {:?}",
            result.content
        );
        result
            .structured_content
            .clone()
            .expect("structured result")
    }

    fn error_text(result: &CallToolResult) -> String {
        assert!(
            result.is_error.unwrap_or(false),
            "expected an error: {result:?}"
        );
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A fake GitTerm bridge for the delegation tools: a real task store in a
    /// temporary directory and the same pure rules the app uses, but no
    /// Codex runner, so a request stays `requested`.
    fn spawn_delegation_bridge(
        mut requests: mpsc::UnboundedReceiver<TaskControlEnvelope>,
        store_dir: PathBuf,
        bridged: Arc<Mutex<Vec<(String, Option<String>)>>>,
    ) -> tokio::task::JoinHandle<()> {
        use crate::delegations::{self, CallerTab};
        use crate::tasks::{Delegation, TaskStore};
        tokio::spawn(async move {
            let mut store = TaskStore::load(TaskStore::path_for_config_root(&store_dir)).unwrap();
            let mut clock = 0;
            while let Some(envelope) = requests.recv().await {
                let caller = envelope.caller.clone();
                let tab = CallerTab {
                    session_uid: caller.clone().unwrap_or_default(),
                    chat_session_id: None,
                    workspace: "scratch".to_string(),
                    cwd: store_dir.clone(),
                    remote: false,
                };
                clock += 1;
                let now = format!("2026-10-08T10:00:{clock:02}Z");
                let (name, result) = match envelope.operation {
                    TaskControlOperation::RequestReview(request) => (
                        "review_request",
                        delegations::review_delegation(&request, &tab, None, None).and_then(
                            |new| {
                                let delegation = Delegation::new_requested(new, now);
                                let id = delegation.delegation_id.clone();
                                store
                                    .insert_delegation(delegation)
                                    .map_err(|error| error.to_string())?;
                                Ok(serde_json::json!({ "delegation_id": id }))
                            },
                        ),
                    ),
                    TaskControlOperation::RequestConsult(request) => (
                        "consult_request",
                        delegations::consult_delegation(&request, &tab, None, None).and_then(
                            |new| {
                                let delegation = Delegation::new_requested(new, now);
                                let id = delegation.delegation_id.clone();
                                store
                                    .insert_delegation(delegation)
                                    .map_err(|error| error.to_string())?;
                                Ok(serde_json::json!({ "delegation_id": id }))
                            },
                        ),
                    ),
                    TaskControlOperation::GetDelegation(request) => (
                        "delegation_get",
                        store
                            .delegation(&request.delegation_id)
                            .ok_or_else(|| {
                                format!("delegation {} does not exist", request.delegation_id)
                            })
                            .and_then(|delegation| {
                                delegations::delegation_record(delegation, &store_dir)
                            }),
                    ),
                    TaskControlOperation::ListDelegations(request) => (
                        "delegation_list",
                        delegations::delegation_list(
                            &store.delegations_for_parent(&tab.session_uid),
                            request.status.as_deref(),
                        ),
                    ),
                    other => panic!("unexpected bridged operation: {other:?}"),
                };
                bridged.lock().unwrap().push((name.to_string(), caller));
                envelope.reply.send(result);
            }
        })
    }

    #[tokio::test]
    async fn delegation_tools_need_a_caller_return_ids_at_once_and_list_newest_first() {
        let (commands, requests) = mpsc::unbounded_channel();
        let (connection, server) = prepare("delegation-test", commands).unwrap();
        let token = connection.token.clone();
        let server_task = tokio::spawn(server.run());
        let store_dir = tempfile::tempdir().unwrap();
        let bridged = Arc::new(Mutex::new(Vec::new()));
        let bridge =
            spawn_delegation_bridge(requests, store_dir.path().to_path_buf(), bridged.clone());

        // Without a caller every delegation tool is refused before the bridge.
        let anonymous = tokio::time::timeout(
            Duration::from_secs(5),
            ().serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(connection.endpoint().to_string())
                    .auth_header(token.clone()),
            )),
        )
        .await
        .expect("MCP initialization timed out")
        .expect("MCP initialization failed");
        for (name, arguments) in [
            (
                "review_request",
                serde_json::json!({ "target": "uncommitted" }),
            ),
            (
                "consult_request",
                serde_json::json!({ "brief": "Which way?" }),
            ),
            (
                "delegation_get",
                serde_json::json!({ "delegation_id": "x" }),
            ),
            ("delegation_list", serde_json::json!({})),
        ] {
            let result = anonymous.call_tool(call(name, arguments)).await.unwrap();
            let text = error_text(&result);
            assert!(
                text.contains(&format!("{name} needs the calling GitTerm tab's identity")),
                "{text}"
            );
            assert!(text.contains("?caller="), "{text}");
        }
        assert!(bridged.lock().unwrap().is_empty());
        anonymous.cancel().await.unwrap();

        let client = tokio::time::timeout(
            Duration::from_secs(5),
            ().serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(caller_endpoint(
                    connection.endpoint(),
                    Some("tab-a"),
                ))
                .auth_header(token),
            )),
        )
        .await
        .expect("MCP initialization timed out")
        .expect("MCP initialization failed");

        // The id comes back without waiting for any review.
        let started = std::time::Instant::now();
        let first = structured(
            &client
                .call_tool(call(
                    "review_request",
                    serde_json::json!({ "target": "base", "base_ref": "v5", "focus": "webview lifecycle" }),
                ))
                .await
                .unwrap(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        let first_id = first["delegation_id"].as_str().unwrap().to_string();
        let second = structured(
            &client
                .call_tool(call(
                    "consult_request",
                    serde_json::json!({ "brief": "Split main.rs?" }),
                ))
                .await
                .unwrap(),
        );
        let second_id = second["delegation_id"].as_str().unwrap().to_string();
        assert_ne!(first_id, second_id);

        // Request validation errors come back as tool errors.
        let bad = client
            .call_tool(call(
                "review_request",
                serde_json::json!({ "target": "commit" }),
            ))
            .await
            .unwrap();
        assert!(error_text(&bad).contains("needs commit"));

        let record = structured(
            &client
                .call_tool(call(
                    "delegation_get",
                    serde_json::json!({ "delegation_id": first_id }),
                ))
                .await
                .unwrap(),
        );
        assert_eq!(record["kind"], "review");
        assert_eq!(record["status"]["state"], "requested");
        assert_eq!(record["parent"]["session_uid"], "tab-a");
        assert_eq!(record["target"]["mode"]["reference"], "v5");
        assert!(record["log_path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("delegations/{first_id}.jsonl")));

        let list = structured(
            &client
                .call_tool(call("delegation_list", serde_json::json!({})))
                .await
                .unwrap(),
        );
        let ids: Vec<&str> = list["delegations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["delegation_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [second_id.as_str(), first_id.as_str()]);
        let requested = structured(
            &client
                .call_tool(call(
                    "delegation_list",
                    serde_json::json!({ "status": "requested" }),
                ))
                .await
                .unwrap(),
        );
        assert_eq!(requested["delegations"].as_array().unwrap().len(), 2);
        let none = structured(
            &client
                .call_tool(call(
                    "delegation_list",
                    serde_json::json!({ "status": "completed" }),
                ))
                .await
                .unwrap(),
        );
        assert!(none["delegations"].as_array().unwrap().is_empty());

        assert!(bridged
            .lock()
            .unwrap()
            .iter()
            .all(|(_, caller)| caller.as_deref() == Some("tab-a")));
        client.cancel().await.unwrap();
        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("task MCP server did not shut down")
            .expect("task MCP task panicked")
            .expect("task MCP server returned an error");
        bridge.abort();
    }
}
