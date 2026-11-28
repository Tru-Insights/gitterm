//! ACP stdio session loop shared unchanged by the desktop hosts.
use super::*;
use crate::chat::{runtime::EventSink, types::HarnessCommand};
use crate::harness::acp_launch::*;
use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const STDERR_TAIL_LINES: usize = 20;
// session/load replay can arrive after its response; wait for quiet as Iced did.
const HISTORY_QUIET: Duration = Duration::from_millis(400);
/// A request GitTerm sent and is waiting on.
#[derive(Debug)]
enum Call {
    Initialize,
    NewSession,
    LoadSession,
    /// Applying the configured model/mode/effort before `Ready`.
    Setup(Setting, Option<String>),
    /// A change the human asked for.
    Change(Setting, Option<String>),
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initializing,
    Creating,
    /// `session/load` answered; its replay may still be arriving.
    History,
    Setup,
    Ready,
    /// The session could not be created; the process is being stopped.
    Failed,
}

struct PendingPermission {
    rpc_id: Value,
    options: Vec<Value>,
}

struct WireLog {
    stdin: std::fs::File,
    stdout: std::fs::File,
}

impl WireLog {
    fn open(dir: &Path, agent: AcpAgent, pid: u32) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create wire log dir {}: {e}", dir.display()))?;
        let open = |name: String| {
            let path = dir.join(name);
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("open wire log {}: {e}", path.display()))
        };
        let agent = agent.program();
        Ok(Self {
            stdin: open(format!("acp-{agent}-{pid}-stdin.jsonl"))?,
            stdout: open(format!("acp-{agent}-{pid}-stdout.jsonl"))?,
        })
    }
}

struct Session {
    snapshots: Box<dyn crate::harness::claude::SnapshotRecorder>,
    config: AcpSessionConfig,
    stdin: Option<ChildStdin>,
    wire: Option<WireLog>,
    events: EventSink,
    pid: u32,
    next_id: u64,
    calls: HashMap<u64, Call>,
    phase: Phase,
    session_id: Option<String>,
    settings: SessionSettings,
    mapper: UpdateMapper,
    history: HistoryBuilder,
    history_deadline: Option<tokio::time::Instant>,
    setup: VecDeque<(Setting, Option<String>)>,
    load_session: bool,
    http_mcp: bool,
    images: bool,
    queue: PromptQueue,
    in_turn: bool,
    /// The id of the prompt the running turn answers.
    turn_message: Option<String>,
    interrupted: bool,
    permissions: HashMap<String, PendingPermission>,
    next_request: u64,
    logged_methods: HashSet<String>,
}

impl Session {
    fn emit(&self, ev: HarnessEvent) {
        // The receiver is gone only when the tab is; nothing to report to.
        self.events.emit(ev);
    }

    fn error(&self, msg: String) {
        acp_log(&msg);
        self.emit(HarnessEvent::Error(msg));
    }

    async fn write(&mut self, frame: Value) -> Result<(), String> {
        if let Some(wire) = self.wire.as_mut() {
            if let Err(e) = writeln!(wire.stdin, "{}", redact_frame(&frame)) {
                acp_log(&format!("wire log write failed: {e}"));
            }
        }
        let stdin = self.stdin.as_mut().ok_or("the agent's stdin is closed")?;
        let mut bytes = frame.to_string().into_bytes();
        bytes.push(b'\n');
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| format!("write to the agent's stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush the agent's stdin: {e}"))
    }

    async fn call(&mut self, method: &str, params: Value, call: Call) -> Result<(), String> {
        self.next_id += 1;
        let id = self.next_id;
        acp_log(&format!("-> {method} ({id}) {call:?}"));
        self.calls.insert(id, call);
        self.write(request_frame(id, method, params)).await
    }

    fn mcp_servers(&self) -> Vec<Value> {
        if self.config.mcp_servers.is_empty() {
            return Vec::new();
        }
        if !self.http_mcp {
            self.error(format!(
                "{} does not take HTTP MCP servers, so GitTerm's task and browser tools are not \
                 attached to this chat.",
                self.config.agent.label()
            ));
            return Vec::new();
        }
        self.config
            .mcp_servers
            .iter()
            .map(|server| server.to_acp(self.config.agent))
            .collect()
    }

    async fn create_session(&mut self) -> Result<(), String> {
        self.phase = Phase::Creating;
        let mut params = session_params(&self.config, self.mcp_servers());
        match self.config.resume.clone() {
            Some(id) if self.load_session => {
                params["sessionId"] = json!(id);
                self.call("session/load", params, Call::LoadSession).await
            }
            resume => {
                if let Some(id) = resume {
                    self.error(format!(
                        "{} cannot resume sessions; session {id} starts over as a new session.",
                        self.config.agent.label()
                    ));
                }
                self.call("session/new", params, Call::NewSession).await
            }
        }
    }

    /// Queue the configured settings the agent does not already have.
    async fn begin_setup(&mut self) -> Result<(), String> {
        self.phase = Phase::Setup;
        let wanted = [
            (Setting::Model, self.config.model.clone()),
            (Setting::Mode, self.config.mode.clone()),
            (Setting::Effort, self.config.effort.clone()),
        ];
        for (setting, value) in wanted {
            let Some(value) = value else { continue };
            if self.settings.current(setting).as_deref() == Some(value.as_str()) {
                continue;
            }
            if !self.settings.supports(setting) {
                acp_log(&format!(
                    "{} has no {} setting; ignoring configured {value:?}",
                    self.config.agent.label(),
                    setting.label()
                ));
                continue;
            }
            self.setup.push_back((setting, Some(value)));
        }
        self.next_setup().await
    }

    async fn next_setup(&mut self) -> Result<(), String> {
        while let Some((setting, value)) = self.setup.pop_front() {
            let session_id = self.session_id.clone().unwrap_or_default();
            match self
                .settings
                .change_request(&session_id, setting, value.as_deref())
            {
                Ok((method, params)) => {
                    return self.call(method, params, Call::Setup(setting, value)).await;
                }
                Err(e) => self.error(format!(
                    "Could not start on {} {}: {e}",
                    setting.label(),
                    value.as_deref().unwrap_or("default")
                )),
            }
        }
        self.ready().await
    }

    async fn ready(&mut self) -> Result<(), String> {
        self.phase = Phase::Ready;
        let session_id = self.session_id.clone();
        acp_log(&format!(
            "{} session {} ready (model {:?}, mode {:?}, effort {:?})",
            self.config.agent.label(),
            session_id.as_deref().unwrap_or("?"),
            self.settings.current(Setting::Model),
            self.settings.current(Setting::Mode),
            self.settings.current(Setting::Effort),
        ));
        self.emit(HarnessEvent::Ready {
            session_id,
            permission_mode: self.settings.current(Setting::Mode),
            models: self.settings.page_models(),
            modes: self.settings.page_modes(),
            commands: Vec::new(),
        });
        // The tab and page start from the tab's config; where the agent
        // ended up elsewhere (a rejected setting, or no model configured),
        // they follow the agent.
        if let Some(current) = self.settings.current(Setting::Model) {
            if self.config.model.as_deref() != Some(current.as_str()) {
                self.emit(HarnessEvent::ModelChanged(current));
            }
        }
        if let (Some(_), Some(current)) = (&self.config.mode, self.settings.current(Setting::Mode))
        {
            if self.config.mode.as_deref() != Some(current.as_str()) {
                self.emit(HarnessEvent::PermissionModeChanged(current));
            }
        }
        if self.settings.supports(Setting::Effort)
            && self.config.effort != self.settings.current(Setting::Effort)
        {
            self.emit(HarnessEvent::EffortChanged {
                effort: self.settings.current(Setting::Effort),
                applied: None,
            });
        }
        self.pump_queue().await
    }

    async fn pump_queue(&mut self) -> Result<(), String> {
        if self.phase != Phase::Ready || self.in_turn {
            return Ok(());
        }
        let Some(prompt) = self.queue.pop_next() else {
            return Ok(());
        };
        self.send_prompt(prompt).await
    }

    async fn send_prompt(&mut self, prompt: UserPrompt) -> Result<(), String> {
        let session_id = self.session_id.clone().unwrap_or_default();
        self.snapshots.bind(&session_id).await;
        self.snapshots.begin().await;
        if !prompt.images.is_empty() && !self.images {
            self.error(format!(
                "{} takes no images; this message went without its {} image(s).",
                self.config.agent.label(),
                prompt.images.len()
            ));
        }
        acp_log(&format!(
            "prompt ({} chars, {} images, id {:?})",
            prompt.text.len(),
            prompt.images.len(),
            prompt.id
        ));
        let blocks = prompt_blocks(&prompt, self.images);
        self.in_turn = true;
        self.interrupted = false;
        self.turn_message = prompt.id.clone();
        if let Some(id) = &prompt.id {
            self.emit(HarnessEvent::MessageLifecycle {
                id: id.clone(),
                state: MessageState::Started,
            });
        }
        self.emit(HarnessEvent::TurnStarted {
            session_id: Some(session_id.clone()),
            model: self.settings.current(Setting::Model),
        });
        let result = self
            .call(
                "session/prompt",
                json!({"sessionId": session_id, "prompt": blocks}),
                Call::Prompt,
            )
            .await;
        if let Err(e) = &result {
            self.end_turn(TurnStatus::Failed(e.clone()), Value::Null)
                .await;
        }
        result
    }

    fn resolve_permissions(&mut self) {
        let ids: Vec<String> = self.permissions.drain().map(|(id, _)| id).collect();
        for request_id in ids {
            self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
        }
    }

    async fn end_turn(&mut self, status: TurnStatus, usage: Value) {
        self.snapshots.end().await;
        acp_log(&format!(
            "turn completed: {status:?} cost={:?}",
            self.mapper.cost_usd
        ));
        self.in_turn = false;
        self.interrupted = false;
        self.resolve_permissions();
        self.emit(HarnessEvent::TurnCompleted {
            status,
            usage,
            cost_usd: self.mapper.cost_usd,
            rate_limit: None,
        });
        if let Some(id) = self.turn_message.take() {
            self.emit(HarnessEvent::MessageLifecycle {
                id,
                state: MessageState::Completed,
            });
        }
    }

    async fn finish_history(&mut self) -> Result<(), String> {
        self.history_deadline = None;
        let entries = std::mem::take(&mut self.history).finish();
        acp_log(&format!(
            "session {} replayed {} history entries",
            self.session_id.as_deref().unwrap_or("?"),
            entries.len()
        ));
        self.emit(HarnessEvent::HistoryReplayed { entries });
        self.begin_setup().await
    }

    /// Stop after a failure that leaves no usable session.
    fn fail(&mut self, msg: String) {
        self.error(msg);
        self.phase = Phase::Failed;
        self.stdin = None;
    }

    async fn on_response(&mut self, id: u64, msg: &Value) -> Result<(), String> {
        let Some(call) = self.calls.remove(&id) else {
            acp_log(&format!("response to unknown request {id}"));
            return Ok(());
        };
        let error = msg.get("error").filter(|e| !e.is_null());
        let result = &msg["result"];
        match call {
            Call::Initialize => {
                if let Some(error) = error {
                    self.fail(format!(
                        "{} refused the ACP handshake: {}",
                        self.config.agent.label(),
                        rpc_error_text(error)
                    ));
                    return Ok(());
                }
                let caps = &result["agentCapabilities"];
                self.load_session = caps["loadSession"] == true;
                self.http_mcp = supports_http_mcp(self.config.agent, result);
                if self.config.agent == AcpAgent::Antigravity
                    && self.config.guidelines.is_some()
                    && !supports_system_prompt_append(result)
                {
                    self.fail("Antigravity does not advertise systemPrompt.append; GitTerm instructions cannot be attached. Update agy and try again.".into());
                    return Ok(());
                }
                self.images = caps["promptCapabilities"]["image"] == true;
                acp_log(&format!(
                    "initialized {} {} (protocol {}, loadSession {}, http MCP {}, images {})",
                    result["agentInfo"]["name"].as_str().unwrap_or("?"),
                    result["agentInfo"]["version"].as_str().unwrap_or("?"),
                    result["protocolVersion"],
                    self.load_session,
                    self.http_mcp,
                    self.images
                ));
                self.create_session().await
            }
            Call::NewSession => {
                if let Some(error) = error {
                    let text = rpc_error_text(error);
                    let msg = if error["code"].as_i64() == Some(ERR_AUTH_REQUIRED) {
                        format!(
                            "{} could not start a session: {text}{}",
                            self.config.agent.label(),
                            auth_hint(self.config.agent)
                        )
                    } else {
                        format!(
                            "{} could not start a session: {text}",
                            self.config.agent.label()
                        )
                    };
                    self.fail(msg);
                    return Ok(());
                }
                let Some(id) = result["sessionId"].as_str() else {
                    self.fail(format!(
                        "{} answered session/new without a sessionId",
                        self.config.agent.label()
                    ));
                    return Ok(());
                };
                self.session_id = Some(id.to_string());
                self.settings = SessionSettings::for_agent(self.config.agent, result);
                self.begin_setup().await
            }
            Call::LoadSession => {
                if let Some(error) = error {
                    let resume = self.config.resume.take().unwrap_or_default();
                    self.error(format!(
                        "Could not resume {} session {resume}: {}. Starting a new session.",
                        self.config.agent.label(),
                        rpc_error_text(error)
                    ));
                    self.history = HistoryBuilder::default();
                    return self.create_session().await;
                }
                self.session_id = self.config.resume.clone();
                self.settings = SessionSettings::for_agent(self.config.agent, result);
                self.phase = Phase::History;
                self.history_deadline = Some(tokio::time::Instant::now() + HISTORY_QUIET);
                Ok(())
            }
            Call::Setup(setting, value) | Call::Change(setting, value) if error.is_some() => {
                let during_setup = self.phase == Phase::Setup;
                self.error(format!(
                    "Could not change the {} to {}: {}",
                    setting.label(),
                    value.as_deref().unwrap_or("default"),
                    rpc_error_text(error.unwrap_or(&Value::Null))
                ));
                if during_setup {
                    return self.next_setup().await;
                }
                Ok(())
            }
            Call::Setup(setting, value) | Call::Change(setting, value) => {
                let before = self.settings.clone();
                match result["configOptions"].as_array() {
                    Some(options) => self.settings.set_config_options(options.clone()),
                    None => {
                        if let Some(value) = &value {
                            self.settings.set_current(setting, value);
                        }
                    }
                }
                if self.config.agent == AcpAgent::Antigravity && setting == Setting::Model {
                    if let Some(value) = &value {
                        self.settings.set_current(setting, value);
                    }
                }
                if self.phase == Phase::Setup {
                    return self.next_setup().await;
                }
                // Confirm what was asked for; a set_config_option reply
                // can also move another setting (a new model's efforts).
                let mut changes = settings_changes(&before, &self.settings);
                let confirmed = match setting {
                    Setting::Model => value.clone().map(HarnessEvent::ModelChanged),
                    Setting::Mode => value.clone().map(HarnessEvent::PermissionModeChanged),
                    Setting::Effort => Some(HarnessEvent::EffortChanged {
                        effort: value.clone(),
                        applied: None,
                    }),
                };
                if let Some(confirmed) = confirmed {
                    if !changes.contains(&confirmed) {
                        changes.retain(|c| {
                            std::mem::discriminant(c) != std::mem::discriminant(&confirmed)
                        });
                        changes.push(confirmed);
                    }
                }
                for change in changes {
                    self.emit(change);
                }
                Ok(())
            }
            Call::Prompt => {
                let (status, usage) = prompt_reply(msg, self.interrupted);
                self.end_turn(status, usage).await;
                self.pump_queue().await
            }
        }
    }

    async fn on_agent_request(&mut self, msg: &Value) -> Result<(), String> {
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg["id"].clone();
        if method != "session/request_permission" {
            acp_log(&format!(
                "agent called {method:?}, which GitTerm does not offer"
            ));
            return self
                .write(error_frame(
                    &id,
                    ERR_METHOD_NOT_FOUND,
                    &format!("GitTerm does not implement {method}"),
                ))
                .await;
        }
        let params = &msg["params"];
        let (items, tool_use_id, kind) = self.mapper.permission(params);
        for item in items {
            self.emit(item);
        }
        self.next_request += 1;
        let request_id = format!("acp_{}_{}", self.pid, self.next_request);
        acp_log(&format!(
            "permission request {request_id} for {tool_use_id}: {}",
            trim_for_log(&serde_json::to_string(&kind).unwrap_or_default(), 300)
        ));
        self.permissions.insert(
            request_id.clone(),
            PendingPermission {
                rpc_id: id,
                options: params["options"].as_array().cloned().unwrap_or_default(),
            },
        );
        self.emit(HarnessEvent::RuntimeRequest {
            request_id,
            tool_use_id,
            kind,
        });
        Ok(())
    }

    fn on_update(&mut self, params: &Value) {
        let update = &params["update"];
        for mapped in self.mapper.map(update) {
            if self.phase == Phase::History || self.phase == Phase::Creating {
                match mapped {
                    Mapped::Event(ev) => {
                        self.history.event(ev);
                        self.extend_history();
                    }
                    Mapped::UserChunk { message_id, text } => {
                        self.history.user_chunk(message_id, &text);
                        self.extend_history();
                    }
                    Mapped::Mode(mode) => self.settings.set_current(Setting::Mode, &mode),
                    Mapped::ConfigOptions(options) => self.settings.set_config_options(options),
                }
                continue;
            }
            match mapped {
                Mapped::Event(ev) => {
                    self.snapshots.observe(&ev);
                    self.emit(ev);
                }
                Mapped::UserChunk { .. } => {}
                Mapped::Mode(mode) => {
                    if self.settings.current(Setting::Mode).as_deref() != Some(mode.as_str()) {
                        self.settings.set_current(Setting::Mode, &mode);
                        self.emit(HarnessEvent::PermissionModeChanged(mode));
                    }
                }
                Mapped::ConfigOptions(options) => {
                    let before = self.settings.clone();
                    self.settings.set_config_options(options);
                    if self.phase == Phase::Ready {
                        for change in settings_changes(&before, &self.settings) {
                            self.emit(change);
                        }
                    }
                }
            }
        }
    }

    fn extend_history(&mut self) {
        if self.history_deadline.is_some() {
            self.history_deadline = Some(tokio::time::Instant::now() + HISTORY_QUIET);
        }
    }

    async fn on_line(&mut self, line: &str) -> Result<(), String> {
        if let Some(wire) = self.wire.as_mut() {
            if let Err(e) = writeln!(wire.stdout, "{line}") {
                acp_log(&format!("wire log write failed: {e}"));
            }
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                acp_log(&format!("non-JSON stdout: {}", trim_for_log(line, 200)));
                return Ok(());
            }
        };
        let has_method = msg["method"].is_string();
        let has_id = msg.get("id").is_some_and(|id| !id.is_null());
        match (has_method, has_id) {
            (true, true) => self.on_agent_request(&msg).await,
            (true, false) => {
                let method = msg["method"].as_str().unwrap_or("");
                if method == "session/update" {
                    self.on_update(&msg["params"]);
                } else if self.logged_methods.insert(method.to_string()) {
                    acp_log(&format!(
                        "ignoring notification {method:?} (logged once per session)"
                    ));
                }
                Ok(())
            }
            (false, true) => match msg["id"].as_u64() {
                Some(id) => self.on_response(id, &msg).await,
                None => {
                    acp_log(&format!("response with a foreign id {}", msg["id"]));
                    Ok(())
                }
            },
            (false, false) => {
                acp_log(&format!("unrecognized frame: {}", trim_for_log(line, 200)));
                Ok(())
            }
        }
    }

    async fn on_command(&mut self, cmd: HarnessCommand) -> Result<(), String> {
        match cmd {
            HarnessCommand::SendUserMessage(prompt) => {
                if self.phase == Phase::Failed {
                    return Err(format!(
                        "Not sent: the {} session did not start.",
                        self.config.agent.label()
                    ));
                }
                let idle = self.phase == Phase::Ready && !self.in_turn;
                let (now, events) = self.queue.submit(prompt, idle);
                for ev in events {
                    self.emit(ev);
                }
                match now {
                    Some(prompt) => self.send_prompt(prompt).await,
                    None => {
                        acp_log(&format!("queued a prompt ({} waiting)", self.queue.len()));
                        Ok(())
                    }
                }
            }
            HarnessCommand::WithdrawQueuedMessage(id) => {
                let ev = self.queue.withdraw(&id);
                acp_log(&format!("withdraw {id}: {ev:?}"));
                self.emit(ev);
                Ok(())
            }
            HarnessCommand::Answer {
                request_id,
                decision,
            } => {
                let Some(pending) = self.permissions.remove(&request_id) else {
                    return Err(format!(
                        "no pending permission request {request_id} (already answered or withdrawn)"
                    ));
                };
                match permission_outcome(&decision, &pending.options) {
                    Ok(outcome) => {
                        acp_log(&format!("answer {request_id}: {outcome}"));
                        let written = self.write(response_frame(&pending.rpc_id, outcome)).await;
                        self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                        written
                    }
                    Err(e) => {
                        // Keep it answerable; the page can pick again.
                        self.permissions.insert(request_id, pending);
                        Err(e)
                    }
                }
            }
            HarnessCommand::Interrupt => {
                if !self.in_turn {
                    acp_log("interrupt with no turn running; nothing to cancel");
                    return Ok(());
                }
                acp_log("session/cancel");
                self.interrupted = true;
                let session_id = self.session_id.clone().unwrap_or_default();
                let result = self
                    .write(notification_frame(
                        "session/cancel",
                        json!({"sessionId": session_id}),
                    ))
                    .await;
                // The spec has the client answer every open permission
                // request of the cancelled turn with `cancelled`.
                let open: Vec<(String, PendingPermission)> = self.permissions.drain().collect();
                for (request_id, pending) in open {
                    self.write(response_frame(
                        &pending.rpc_id,
                        json!({"outcome": {"outcome": "cancelled"}}),
                    ))
                    .await?;
                    self.emit(HarnessEvent::RuntimeRequestResolved { request_id });
                }
                result
            }
            HarnessCommand::SetPermissionMode(mode) => self.change(Setting::Mode, Some(mode)).await,
            HarnessCommand::SetModel(model) => self.change(Setting::Model, Some(model)).await,
            HarnessCommand::SetEffort(effort) => self.change(Setting::Effort, effort).await,
            HarnessCommand::SetWorkspaceContext(_) => {
                Err("Workspace SessionStart context is only supported by Claude".into())
            }
            HarnessCommand::Shutdown => {
                acp_log(&format!("shutdown requested (pid {})", self.pid));
                self.stdin = None;
                Ok(())
            }
        }
    }

    async fn change(&mut self, setting: Setting, value: Option<String>) -> Result<(), String> {
        if self.phase != Phase::Ready {
            return Err(format!(
                "The {} session is still starting; change the {} once it is ready.",
                self.config.agent.label(),
                setting.label()
            ));
        }
        let session_id = self.session_id.clone().unwrap_or_default();
        let (method, params) = self
            .settings
            .change_request(&session_id, setting, value.as_deref())
            .map_err(|e| {
                format!(
                    "Could not change the {} to {}: {e}",
                    setting.label(),
                    value.as_deref().unwrap_or("default")
                )
            })?;
        self.call(method, params, Call::Change(setting, value))
            .await
    }
}

pub(crate) async fn run_session(
    launch: Launch,
    mut commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: EventSink,
) {
    let Launch {
        client_version,
        config,
        program,
        snapshots,
    } = launch;
    let label = config.agent.label();
    let fail = |msg: String| {
        acp_log(&msg);
        events.emit(HarnessEvent::Error(msg));
        events.emit(HarnessEvent::ProcessExited { code: None });
    };
    if config.agent == AcpAgent::Gemini {
        return fail(GEMINI_REPLACED_NOTE.into());
    }
    let agent_env = prepare_agent_env(&config);
    let args = config.args();
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("PATH", crate::harness::local_launch::child_path());
    // Secrets (MCP tokens) travel only through the
    // environment and stdin.
    cmd.envs(agent_env.iter().map(|(k, v)| (k, v)));
    cmd.envs(config.env.iter().map(|(k, v)| (k, v)));
    acp_log(&format!(
        "spawn: {} {} (cwd {}, env {:?})",
        program.display(),
        args.join(" "),
        config.cwd.display(),
        agent_env
            .iter()
            .chain(config.env.iter())
            .map(|(k, _)| k)
            .collect::<Vec<_>>()
    ));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return fail(format!(
                "failed to start {} in {}: {e}",
                program.display(),
                config.cwd.display()
            ))
        }
    };
    let pid = child.id().unwrap_or(0);
    let (Some(stdout), Some(stderr), Some(stdin)) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take())
    else {
        return fail(format!("the {label} child is missing a stdio pipe"));
    };
    let wire = config.wire_log_dir.as_deref().and_then(|dir| {
        match WireLog::open(dir, config.agent, pid) {
            Ok(w) => Some(w),
            Err(e) => {
                acp_log(&e);
                None
            }
        }
    });
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let stderr_reader = {
        let tail = stderr_tail.clone();
        let agent = config.agent.program();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        acp_log(&format!(
                            "{agent} stderr[{pid}]: {}",
                            trim_for_log(&line, 400)
                        ));
                        if let Ok(mut t) = tail.lock() {
                            if t.len() == STDERR_TAIL_LINES {
                                t.pop_front();
                            }
                            t.push_back(line);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        acp_log(&format!("{agent} stderr[{pid}] read failed: {e}"));
                        break;
                    }
                }
            }
        })
    };

    let mut session = Session {
        snapshots,
        config,
        stdin: Some(stdin),
        wire,
        events,
        pid,
        next_id: 0,
        calls: HashMap::new(),
        phase: Phase::Initializing,
        session_id: None,
        settings: SessionSettings::default(),
        mapper: UpdateMapper::new(),
        history: HistoryBuilder::default(),
        history_deadline: None,
        setup: VecDeque::new(),
        load_session: false,
        http_mcp: false,
        images: false,
        queue: PromptQueue::default(),
        in_turn: false,
        turn_message: None,
        interrupted: false,
        permissions: HashMap::new(),
        next_request: 0,
        logged_methods: HashSet::new(),
    };
    if let Err(e) = session
        .call(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {"name": "gitterm", "version": client_version},
            }),
            Call::Initialize,
        )
        .await
    {
        session.error(e);
    }

    let mut lines = BufReader::new(stdout).lines();
    let mut shutting_down = false;
    let mut kill_deadline: Option<tokio::time::Instant> = None;
    loop {
        if session.stdin.is_none() && kill_deadline.is_none() {
            // Shutdown, a dropped handle or a failed start: the agent gets
            // a grace period to exit on its closed stdin.
            shutting_down = true;
            kill_deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
        }
        let history_deadline = session.history_deadline;
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        session.error(format!("read {label} stdout: {e}"));
                        break;
                    }
                };
                if let Err(e) = session.on_line(&line).await {
                    session.error(e);
                }
            }
            cmd = commands.recv(), if !shutting_down => {
                let Some(cmd) = cmd else {
                    // Handle dropped without an explicit Shutdown.
                    session.stdin = None;
                    continue;
                };
                if let Err(e) = session.on_command(cmd).await {
                    session.error(e);
                }
            }
            _ = tokio::time::sleep_until(history_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if history_deadline.is_some() => {
                if let Err(e) = session.finish_history().await {
                    session.error(e);
                }
            }
            _ = tokio::time::sleep_until(kill_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if kill_deadline.is_some() => {
                acp_log(&format!("{label} {pid} did not exit after stdin closed; killing"));
                if let Err(e) = child.start_kill() {
                    acp_log(&format!("kill {label} {pid}: {e}"));
                }
                break;
            }
        }
    }

    let code = match child.wait().await {
        Ok(status) => status.code(),
        Err(e) => {
            acp_log(&format!("wait for {label} {pid}: {e}"));
            None
        }
    };
    if let Err(error) = stderr_reader.await {
        acp_log(&format!("{label} stderr reader: {error}"));
    }
    acp_log(&format!("{label} {pid} exited with {code:?}"));
    let failed_start = session.phase == Phase::Failed;
    if !shutting_down && !failed_start && code != Some(0) {
        let tail = stderr_tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        session.emit(HarnessEvent::Error(if tail.is_empty() {
            format!("{label} exited unexpectedly (code {code:?})")
        } else {
            format!("{label} exited unexpectedly (code {code:?}):\n{tail}")
        }));
    }
    if session.in_turn {
        session
            .end_turn(
                TurnStatus::Failed(format!("{label} exited mid-turn")),
                Value::Null,
            )
            .await;
    }
    session.snapshots.end().await;
    session.resolve_permissions();
    session.emit(HarnessEvent::ProcessExited { code });
}
