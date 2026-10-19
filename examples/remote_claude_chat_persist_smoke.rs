// TRU-152 slice 2: remote Claude chats outlive the connection. Starts
// in-process agents on 127.0.0.1 and drives real `claude` (haiku) through
// them:
//
//   1. a turn raises a Bash permission prompt; the client drops before
//      answering; the chat keeps running (detached, awaiting input);
//   2. a reattach after the prompt's seq gets it redelivered;
//   3. a second client starting with the same resume id attaches to the
//      same chat (one `claude` child process);
//   4. answering from one client completes the turn, and both clients see
//      the same events;
//   5. an explicit Shutdown stops the process;
//   6. on an agent with a 4-event buffer, a reattach far behind is a gap;
//   7. `ClaudeSession::spawn_remote` through a loopback proxy that is cut
//      mid-turn reports RemoteDisconnected, reconnects on its own, replays
//      what it missed (RemoteReconnected, no gap) and finishes the turn;
//      dropping that handle leaves the chat running.
//
//   cargo run --example remote_claude_chat_persist_smoke -- [--model haiku]
//
// Uses this machine's own Claude login. Prints PASS/FAIL per step and exits
// 1 if any step fails.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gitterm::agentd::claude_chats::ChatLimits;
use gitterm::agentd::client::{
    ClaudeChatFrame, ClaudeChatFrameStream, ClaudeChatOpen, ClaudeChatOpened, RemoteAgentBackend,
    RemoteAgentClientConfig, RunningClaudeChatState,
};
use gitterm::agentd::server::{self, AgentServerConfig};
use gitterm::harness::claude::{ClaudeSession, ClaudeSessionConfig, RemoteClaudeChatStart};
use gitterm::harness::{HarnessCommand, HarnessEvent, RuntimeDecision, TurnStatus};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN_ENV: &str = "GITTERM_REMOTE_CHAT_PERSIST_SMOKE_TOKEN";
const PROBE_FILE: &str = "gitterm-persist-probe.txt";

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// One raw `ClaudeChat` attachment, the way the desktop's handle uses it.
struct RawClient {
    name: &'static str,
    commands: mpsc::UnboundedSender<String>,
    stream: ClaudeChatFrameStream,
}

/// A relayed event as a client saw it.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    seq: u64,
    redelivered: bool,
    json: String,
    event: HarnessEvent,
}

impl RawClient {
    async fn open(
        name: &'static str,
        backend: &RemoteAgentBackend,
        open: ClaudeChatOpen,
    ) -> Result<(Self, ClaudeChatOpened), String> {
        let (commands, rx) = mpsc::unbounded_channel();
        let mut stream = backend
            .claude_chat_attach(open, rx)
            .await
            .map_err(|e| e.to_string())?;
        let first = tokio::time::timeout(STEP_TIMEOUT, stream.next())
            .await
            .map_err(|_| "timed out waiting for opened".to_string())?;
        match first {
            Some(Ok(ClaudeChatFrame::Opened(opened))) => {
                println!("[{name}] opened {opened:?}");
                Ok((
                    Self {
                        name,
                        commands,
                        stream,
                    },
                    opened,
                ))
            }
            other => Err(format!("first frame was not opened: {other:?}")),
        }
    }

    fn send(&self, command: &HarnessCommand) -> Result<(), String> {
        self.commands
            .send(serde_json::to_string(command).map_err(|e| e.to_string())?)
            .map_err(|_| "command channel closed".to_string())
    }

    /// The next event, or `None` when the stream ended.
    async fn next(&mut self) -> Result<Option<Seen>, String> {
        let frame = tokio::time::timeout(STEP_TIMEOUT, self.stream.next())
            .await
            .map_err(|_| format!("[{}] timed out", self.name))?;
        match frame {
            None => Ok(None),
            Some(Err(e)) => Err(format!("[{}] stream failed: {e}", self.name)),
            Some(Ok(ClaudeChatFrame::Opened(opened))) => Err(format!(
                "[{}] unexpected second opened: {opened:?}",
                self.name
            )),
            Some(Ok(ClaudeChatFrame::Event {
                json,
                seq,
                redelivered,
            })) => {
                let event: HarnessEvent = serde_json::from_str(&json).map_err(|e| e.to_string())?;
                show(self.name, seq, redelivered, &event);
                Ok(Some(Seen {
                    seq,
                    redelivered,
                    json,
                    event,
                }))
            }
        }
    }

    /// Events until (and including) the first one `stop` accepts.
    async fn until(&mut self, stop: impl Fn(&Seen) -> bool) -> Result<Vec<Seen>, String> {
        let mut seen = Vec::new();
        loop {
            let Some(event) = self.next().await? else {
                return Err(format!("[{}] stream ended early", self.name));
            };
            let done = stop(&event);
            seen.push(event);
            if done {
                return Ok(seen);
            }
        }
    }
}

fn show(who: &str, seq: u64, redelivered: bool, ev: &HarnessEvent) {
    let mark = if redelivered { " (redelivered)" } else { "" };
    match ev {
        HarnessEvent::TextDelta(_)
        | HarnessEvent::ThinkingDelta(_)
        | HarnessEvent::ItemInputDelta { .. } => {}
        HarnessEvent::Ready { models, .. } => {
            println!("[{who}] #{seq}{mark} ready ({} models)", models.len())
        }
        other => println!(
            "[{who}] #{seq}{mark} {}",
            serde_json::to_string(other).unwrap_or_default()
        ),
    }
}

/// `claude` processes this smoke process started (the in-process agents
/// spawn them as direct children).
fn claude_children() -> Vec<String> {
    let me = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,args="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?;
            let ppid = parts.next()?;
            let args: Vec<&str> = parts.collect();
            (ppid == me && args.contains(&"stream-json"))
                .then(|| format!("{pid} {}", args.join(" ")))
        })
        .collect()
}

async fn start_agent(name: &str, limits: ChatLimits, token: &str) -> SocketAddr {
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free loopback port");
    tokio::spawn(server::serve(AgentServerConfig {
        bind_addr: addr,
        auth_token: token.to_string(),
        agent_name: name.to_string(),
        claude_chat_limits: limits,
    }));
    addr
}

fn client_for(name: &str, addr: SocketAddr) -> RemoteAgentClientConfig {
    RemoteAgentClientConfig {
        remote_id: name.into(),
        name: name.into(),
        endpoint: format!("http://{addr}"),
        token_ref: format!("env:{TOKEN_ENV}"),
    }
}

async fn wait_for_handshake(backend: &RemoteAgentBackend) -> Result<Vec<String>, String> {
    let mut last = String::new();
    for _ in 0..50 {
        match backend.handshake().await {
            Ok(h) => return Ok(h.capabilities),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last)
}

fn session_config(
    cwd: &std::path::Path,
    model: &str,
    resume: Option<String>,
) -> ClaudeSessionConfig {
    ClaudeSessionConfig {
        cwd: cwd.to_path_buf(),
        model: Some(model.to_string()),
        permission_mode: "default".into(),
        effort: None,
        resume,
        wire_log_dir: None,
        mcp_servers: Vec::new(),
        chrome: false,
    }
}

fn start_json(config: &ClaudeSessionConfig) -> String {
    serde_json::to_string(&RemoteClaudeChatStart::from_config(config).expect("remote start"))
        .expect("start json")
}

/// A loopback TCP proxy whose connections can be cut, standing in for a
/// network that goes away.
struct Proxy {
    addr: SocketAddr,
    up: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Proxy {
    async fn start(target: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy port");
        let addr = listener.local_addr().expect("proxy addr");
        let up = Arc::new(AtomicBool::new(true));
        let connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        {
            let up = up.clone();
            let connections = connections.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    if !up.load(Ordering::SeqCst) {
                        drop(inbound);
                        continue;
                    }
                    connections.lock().unwrap().push(tokio::spawn(async move {
                        let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }));
                }
            });
        }
        Self {
            addr,
            up,
            connections,
        }
    }

    fn cut(&self) {
        self.up.store(false, Ordering::SeqCst);
        for connection in self.connections.lock().unwrap().drain(..) {
            connection.abort();
        }
    }

    fn restore(&self) {
        self.up.store(true, Ordering::SeqCst);
    }
}

#[tokio::main]
async fn main() {
    let model = arg("--model").unwrap_or_else(|| "haiku".into());
    let mut failures: Vec<String> = Vec::new();
    macro_rules! check {
        ($name:expr, $ok:expr, $detail:expr $(,)?) => {
            record(&mut failures, $name, $ok, $detail)
        };
    }
    macro_rules! must {
        ($name:expr, $result:expr) => {
            match $result {
                Ok(value) => value,
                Err(e) => {
                    check!($name, false, e.to_string());
                    finish(failures);
                }
            }
        };
    }

    let token = uuid::Uuid::new_v4().to_string();
    std::env::set_var(TOKEN_ENV, &token);
    let main_addr = start_agent("persist-smoke", ChatLimits::default(), &token).await;
    // The test knob: a buffer that holds only the last 4 events.
    let tiny_addr = start_agent(
        "persist-smoke-tiny-buffer",
        ChatLimits {
            buffer_max_events: 4,
            ..ChatLimits::default()
        },
        &token,
    )
    .await;
    let backend = RemoteAgentBackend::new(client_for("persist-smoke", main_addr));
    let tiny = RemoteAgentBackend::new(client_for("persist-smoke-tiny", tiny_addr));
    let capabilities = must!("handshake", wait_for_handshake(&backend).await);
    check!(
        "handshake advertises claude_chat_persistent",
        capabilities.iter().any(|c| c == "claude_chat_persistent"),
        format!("{capabilities:?}"),
    );
    must!(
        "tiny-buffer agent handshake",
        wait_for_handshake(&tiny).await
    );

    let workdir = tempfile::tempdir().expect("temp dir");
    let cwd = workdir.path().canonicalize().expect("canonical temp dir");

    // 1. A turn that stops on a Bash permission prompt.
    let fresh = start_json(&session_config(&cwd, &model, None));
    let (a, opened_a) = must!(
        "client A starts a chat",
        RawClient::open("A", &backend, ClaudeChatOpen::Start { start_json: fresh }).await
    );
    check!(
        "a fresh start is not an attach",
        !opened_a.attached && opened_a.head_seq == 0,
        format!("{opened_a:?}"),
    );
    let chat_id = opened_a.chat_id.clone();
    let mut a = a;
    let prompt = format!("Use the Bash tool to run `touch {PROBE_FILE}`. Then reply with done.");
    println!("\n>> {prompt}");
    must!(
        "send prompt",
        a.send(&HarnessCommand::SendUserMessage(prompt.as_str().into()))
    );
    let seen_a = must!(
        "permission prompt raised",
        a.until(|s| matches!(s.event, HarnessEvent::RuntimeRequest { .. }))
            .await
    );
    let session_id = seen_a.iter().find_map(|s| match &s.event {
        HarnessEvent::TurnStarted {
            session_id: Some(id),
            ..
        } => Some(id.clone()),
        _ => None,
    });
    let request = seen_a.last().expect("the request");
    let (request_id, request_seq) = match &request.event {
        HarnessEvent::RuntimeRequest { request_id, .. } => (request_id.clone(), request.seq),
        _ => unreachable!(),
    };
    check!(
        "permission prompt raised",
        session_id.is_some(),
        format!("request {request_id} at seq {request_seq}, session {session_id:?}"),
    );
    let Some(session_id) = session_id else {
        finish(failures);
    };

    // The client goes away without answering.
    drop(a);
    let mut detached = None;
    for _ in 0..100 {
        let chats = must!("list chats", backend.list_claude_chats().await);
        if chats.len() == 1 && chats[0].attached_clients == 0 {
            detached = Some(chats[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check!(
        "dropping the client only detaches",
        detached.as_ref().is_some_and(|chat| chat.chat_id == chat_id
            && chat.claude_session_id == session_id
            && chat.state == RunningClaudeChatState::AwaitingInput),
        format!("{detached:?}"),
    );
    let children = claude_children();
    check!(
        "its claude keeps running",
        children.len() == 1,
        format!("{children:?}"),
    );

    // 2. Back after the prompt's seq: the prompt is redelivered.
    let (mut b, opened_b) = must!(
        "client B reattaches",
        RawClient::open(
            "B",
            &backend,
            ClaudeChatOpen::Chat {
                chat_id: chat_id.clone(),
                after_seq: Some(request_seq),
            },
        )
        .await
    );
    let redelivered_b = must!(
        "client B gets the open prompt",
        b.until(|s| matches!(&s.event, HarnessEvent::RuntimeRequest { .. }))
            .await
    );
    let prompt_b = redelivered_b.last().expect("the request");
    check!(
        "pending prompt re-delivered on reattach",
        opened_b.attached
            && !opened_b.gap
            && prompt_b.redelivered
            && matches!(&prompt_b.event, HarnessEvent::RuntimeRequest { request_id: id, .. } if *id == request_id),
        format!(
            "opened {opened_b:?}; prompt seq {} redelivered {}",
            prompt_b.seq, prompt_b.redelivered
        ),
    );

    // 3. A start for the same conversation joins it.
    let resume = start_json(&session_config(&cwd, &model, Some(session_id.clone())));
    let (mut c, opened_c) = must!(
        "client C starts with the resume id",
        RawClient::open("C", &backend, ClaudeChatOpen::Start { start_json: resume }).await
    );
    let redelivered_c = must!(
        "client C gets the open prompt",
        c.until(|s| matches!(&s.event, HarnessEvent::RuntimeRequest { .. }))
            .await
    );
    let chats = must!("list chats", backend.list_claude_chats().await);
    let children = claude_children();
    check!(
        "a start with the same resume id attaches (one process)",
        opened_c.attached
            && opened_c.chat_id == chat_id
            && redelivered_c.last().is_some_and(|s| s.redelivered)
            && chats.len() == 1
            && chats[0].attached_clients == 2
            && children.len() == 1,
        format!(
            "opened {opened_c:?}; ListClaudeChats {} chat(s), {} attached; claude children {children:?}",
            chats.len(),
            chats.first().map(|chat| chat.attached_clients).unwrap_or(0)
        ),
    );

    // 4. B answers; both see the turn through.
    println!(">> B answers {request_id}: allow");
    must!(
        "answer",
        b.send(&HarnessCommand::Answer {
            request_id: request_id.clone(),
            decision: RuntimeDecision::Allow {
                updated_input: None,
                remember: None,
            },
        })
    );
    let is_turn_end = |s: &Seen| matches!(s.event, HarnessEvent::TurnCompleted { .. });
    let rest_b = must!("B sees the turn end", b.until(is_turn_end).await);
    let rest_c = must!("C sees the turn end", c.until(is_turn_end).await);
    let status = rest_b.last().and_then(|s| match &s.event {
        HarnessEvent::TurnCompleted { status, .. } => Some(status.clone()),
        _ => None,
    });
    check!(
        "answering completes the turn",
        status == Some(TurnStatus::Completed) && cwd.join(PROBE_FILE).is_file(),
        format!(
            "status {status:?}; {} exists: {}",
            PROBE_FILE,
            cwd.join(PROBE_FILE).is_file()
        ),
    );
    let live = |seen: &[Seen]| -> Vec<(u64, String)> {
        seen.iter()
            .filter(|s| !s.redelivered && s.seq != 0)
            .map(|s| (s.seq, s.json.clone()))
            .collect()
    };
    let (live_b, live_c) = (live(&rest_b), live(&rest_c));
    check!(
        "a second concurrent client sees the same events",
        !live_b.is_empty() && live_b == live_c,
        format!(
            "B {} events (seq {:?}..{:?}), C {} events",
            live_b.len(),
            live_b.first().map(|e| e.0),
            live_b.last().map(|e| e.0),
            live_c.len()
        ),
    );

    // 5. An explicit Shutdown (from C) stops it for everyone.
    must!("shutdown", c.send(&HarnessCommand::Shutdown));
    let exit_b = must!(
        "B sees the exit",
        b.until(|s| matches!(s.event, HarnessEvent::ProcessExited { .. }))
            .await
    );
    let exit_c = must!(
        "C sees the exit",
        c.until(|s| matches!(s.event, HarnessEvent::ProcessExited { .. }))
            .await
    );
    let ended = matches!(b.next().await, Ok(None)) && matches!(c.next().await, Ok(None));
    let mut gone = false;
    for _ in 0..100 {
        if must!("list chats", backend.list_claude_chats().await).is_empty()
            && claude_children().is_empty()
        {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check!(
        "explicit Shutdown stops the process",
        gone && ended && exit_b.last().map(|s| s.seq) == exit_c.last().map(|s| s.seq),
        format!(
            "exit seq B {:?} C {:?}; streams ended {ended}; registry and claude children empty {gone}",
            exit_b.last().map(|s| s.seq),
            exit_c.last().map(|s| s.seq)
        ),
    );
    match RawClient::open(
        "late",
        &backend,
        ClaudeChatOpen::Chat {
            chat_id: chat_id.clone(),
            after_seq: Some(1),
        },
    )
    .await
    {
        Err(e) => check!(
            "a stopped chat cannot be attached",
            e.contains("no running"),
            e
        ),
        Ok((_, opened)) => check!(
            "a stopped chat cannot be attached",
            false,
            format!("{opened:?}")
        ),
    }

    // 6. A reattach past a 4-event buffer is a gap.
    let gap_dir = tempfile::tempdir().expect("temp dir");
    let gap_cwd = gap_dir.path().canonicalize().expect("canonical temp dir");
    let (mut d, opened_d) = must!(
        "client D starts on the tiny-buffer agent",
        RawClient::open(
            "D",
            &tiny,
            ClaudeChatOpen::Start {
                start_json: start_json(&session_config(&gap_cwd, &model, None)),
            },
        )
        .await
    );
    // Long enough to stream well past 4 events.
    must!(
        "send prompt",
        d.send(&HarnessCommand::SendUserMessage(
            "Count from 1 to 30, one number per line, digits only, nothing else.".into()
        ))
    );
    let seen_d = must!("D's turn ends", d.until(is_turn_end).await);
    let head = seen_d.last().map(|s| s.seq).unwrap_or(0);
    drop(d);
    let (mut e, opened_e) = must!(
        "client E reattaches after seq 1",
        RawClient::open(
            "E",
            &tiny,
            ClaudeChatOpen::Chat {
                chat_id: opened_d.chat_id.clone(),
                after_seq: Some(1),
            },
        )
        .await
    );
    check!(
        "a forced gap is reported",
        opened_e.gap && opened_e.head_seq >= head && head > 5,
        format!("{head} events in the turn; reattach after 1: {opened_e:?}"),
    );
    let (_, opened_ok) = must!(
        "client F reattaches at the head",
        RawClient::open(
            "F",
            &tiny,
            ClaudeChatOpen::Chat {
                chat_id: opened_d.chat_id.clone(),
                after_seq: Some(opened_e.head_seq),
            },
        )
        .await
    );
    check!(
        "a reattach within the buffer is not a gap",
        !opened_ok.gap,
        format!("{opened_ok:?}"),
    );
    must!("shutdown", e.send(&HarnessCommand::Shutdown));
    must!(
        "E sees the exit",
        e.until(|s| matches!(s.event, HarnessEvent::ProcessExited { .. }))
            .await
    );

    // 7. The desktop handle through a network that drops mid-turn.
    let proxy = Proxy::start(main_addr).await;
    let link_dir = tempfile::tempdir().expect("temp dir");
    let link_cwd = link_dir.path().canonicalize().expect("canonical temp dir");
    let (session, mut events) = ClaudeSession::spawn_remote(
        client_for("proxied-mini", proxy.addr),
        session_config(&link_cwd, &model, None),
    );
    let count = "Count from 1 to 40, one number per line, digits only, nothing else.";
    println!("\n>> {count}");
    must!(
        "send prompt",
        session.send(HarnessCommand::SendUserMessage(count.into()))
    );
    let mut text = String::new();
    let mut disconnected = None;
    let mut reconnected = None;
    let mut status = None;
    let mut cut = false;
    // Until the turn has ended and, once cut, the handle is back.
    while status.is_none() || (cut && reconnected.is_none()) {
        let ev = match tokio::time::timeout(STEP_TIMEOUT, events.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => {
                println!("[handle] timed out");
                break;
            }
        };
        show("handle", 0, false, &ev);
        match ev {
            HarnessEvent::TextDelta(delta) => {
                text.push_str(&delta);
                if !cut {
                    cut = true;
                    println!(">> cutting the network");
                    proxy.cut();
                    let restore = proxy.up.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        println!(">> restoring the network");
                        restore.store(true, Ordering::SeqCst);
                    });
                }
            }
            HarnessEvent::RemoteDisconnected { machine, error } => {
                disconnected = Some(format!("{machine}: {error}"))
            }
            HarnessEvent::RemoteReconnected {
                machine,
                history_gap,
            } => reconnected = Some((machine, history_gap)),
            HarnessEvent::TurnCompleted { status: s, .. } => status = Some(s),
            HarnessEvent::ProcessExited { code } => {
                println!("[handle] unexpected exit {code:?}");
                break;
            }
            _ => {}
        }
    }
    proxy.restore();
    let numbers: Vec<u32> = text
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    check!(
        "a lost connection is a notice, not an exit",
        disconnected.is_some(),
        format!("{disconnected:?}"),
    );
    check!(
        "the handle reconnects and replays what it missed",
        reconnected == Some(("proxied-mini".to_string(), false))
            && status == Some(TurnStatus::Completed)
            && numbers == (1..=40).collect::<Vec<u32>>(),
        format!(
            "reconnected {reconnected:?}; turn {status:?}; text has {} numbers, in order and once each: {}",
            numbers.len(),
            numbers == (1..=40).collect::<Vec<u32>>()
        ),
    );

    // Dropping the handle detaches; the chat keeps running.
    drop(session);
    let mut still_running = None;
    for _ in 0..100 {
        let chats = must!("list chats", backend.list_claude_chats().await);
        if chats.len() == 1 && chats[0].attached_clients == 0 {
            still_running = Some(chats[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check!(
        "dropping the handle leaves the chat running",
        still_running.is_some() && claude_children().len() == 1,
        format!("{still_running:?}; claude children {:?}", claude_children()),
    );
    let Some(chat) = still_running else {
        finish(failures);
    };

    // 8. A tab restored on the next launch: attach to the listed chat,
    //    carry on in it, then close it (Shutdown).
    let restored_config = session_config(&link_cwd, &model, Some(chat.claude_session_id.clone()));
    let (restored, mut events) = ClaudeSession::attach_remote(
        client_for("proxied-mini", proxy.addr),
        restored_config.clone(),
        chat.chat_id.clone(),
    );
    must!(
        "send prompt",
        restored.send(HarnessCommand::SendUserMessage(
            "Reply with the single word back.".into()
        ))
    );
    let mut seen = Vec::new();
    let mut exit = None;
    let mut shutdown_sent = false;
    loop {
        let ev = match tokio::time::timeout(STEP_TIMEOUT, events.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) | Err(_) => break,
        };
        show("restored", 0, false, &ev);
        match &ev {
            HarnessEvent::TurnCompleted { .. } if !shutdown_sent => {
                shutdown_sent = true;
                // While attached: still the one process.
                seen.push(format!("children {}", claude_children().len()));
                must!("shutdown", restored.send(HarnessCommand::Shutdown));
            }
            HarnessEvent::ProcessExited { code } => {
                exit = Some(*code);
                break;
            }
            _ => {}
        }
        seen.push(match &ev {
            HarnessEvent::Ready { .. } => "ready".to_string(),
            HarnessEvent::TurnStarted { session_id, .. } => format!("turn {session_id:?}"),
            HarnessEvent::TurnCompleted { status, .. } => format!("completed {status:?}"),
            _ => continue,
        });
    }
    drop(restored);
    check!(
        "a restored tab reattaches to its running chat (no second claude)",
        seen.first().map(String::as_str) == Some("ready")
            && seen.contains(&format!("turn {:?}", Some(&chat.claude_session_id)))
            && seen.contains(&"children 1".to_string())
            && seen.contains(&format!("completed {:?}", TurnStatus::Completed)),
        format!("{seen:?}"),
    );
    let mut gone = false;
    for _ in 0..100 {
        if must!("list chats", backend.list_claude_chats().await).is_empty()
            && claude_children().is_empty()
        {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check!(
        "closing the tab (Shutdown) stops it",
        exit.is_some() && gone,
        format!("exit {exit:?}; registry and claude children empty {gone}"),
    );

    // A listed chat that stopped meanwhile: the attach says so and starts
    // nothing.
    let (late, mut events) = ClaudeSession::attach_remote(
        client_for("proxied-mini", proxy.addr),
        restored_config,
        chat.chat_id,
    );
    let mut said = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(STEP_TIMEOUT, events.recv()).await {
        show("late", 0, false, &ev);
        let exited = matches!(ev, HarnessEvent::ProcessExited { .. });
        said.push(ev);
        if exited {
            break;
        }
    }
    drop(late);
    check!(
        "attaching to a stopped chat starts nothing",
        matches!(said.first(), Some(HarnessEvent::Error(e)) if e.contains("no longer running"))
            && matches!(
                said.last(),
                Some(HarnessEvent::ProcessExited { code: None })
            )
            && claude_children().is_empty(),
        format!("{said:?}"),
    );

    finish(failures);
}

fn record(failures: &mut Vec<String>, name: &str, ok: bool, detail: String) {
    println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        failures.push(name.to_string());
    }
}

fn finish(failures: Vec<String>) -> ! {
    if failures.is_empty() {
        println!("\nALL PASS");
        std::process::exit(0);
    }
    println!("\nFAILED: {failures:?}");
    std::process::exit(1);
}
