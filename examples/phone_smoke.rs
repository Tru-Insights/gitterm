// TRU-151 stage 1: the agent's phone web app, end to end on loopback.
// Starts an in-process agent on 127.0.0.1 with its phone app on the next
// port and a throwaway config dir, then:
//
//   1. the handshake advertises "phone"; the pages load without a token;
//   2. a pairing made over gRPC is redeemed over HTTP, and only once;
//   3. GET /api/chats answers 200 with the device token and 401 without;
//   4. a real Claude chat (haiku) started the way the desktop starts one is
//      viewed from the phone socket: opened, history, live events with
//      seqs through the turn's end;
//   5. a viewing socket that tries to send a message is refused, and
//      Claude starts no turn for it;
//   6. the chat shows in /api/chats and its history loads over HTTP;
//   7. revoking the phone closes its socket and its token answers 401;
//   8. repeated bad tokens from one address are throttled (429).
//
//   cargo run --example phone_smoke -- [--model haiku] [--hold SECS]
//
// `--hold SECS` keeps the agent, a chat and a fresh pairing URL up for that
// long at the end (for loading the pages in a browser). Uses this
// machine's own Claude login. Prints PASS/FAIL per step and exits 1 if any
// step fails.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use gitterm::agentd::client::{
    ClaudeChatFrame, ClaudeChatFrameStream, ClaudeChatOpen, RemoteAgentBackend,
    RemoteAgentClientConfig,
};
use gitterm::agentd::phone_http::PhoneServerConfig;
use gitterm::agentd::server::{self, AgentServerConfig};
use gitterm::harness::claude::{ClaudeSessionConfig, RemoteClaudeChatStart};
use gitterm::harness::{HarnessCommand, HarnessEvent};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const STEP_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN_ENV: &str = "GITTERM_PHONE_SMOKE_TOKEN";

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free loopback port")
}

/// A plain HTTP request to the phone app: status and body.
async fn http(
    method: &str,
    url: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Result<(u16, String), String> {
    let mut request = hyper::Request::builder().method(method).uri(url);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            hyper::Body::from(value.to_string())
        }
        None => hyper::Body::empty(),
    };
    let request = request.body(body).map_err(|e| e.to_string())?;
    let response = hyper::Client::new()
        .request(request)
        .await
        .map_err(|e| format!("{method} {url}: {e}"))?;
    let status = response.status().as_u16();
    let bytes = hyper::body::to_bytes(response.into_body())
        .await
        .map_err(|e| e.to_string())?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

type PhoneSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the phone socket and say hello.
async fn phone_socket(
    phone_base: &str,
    token: &str,
    chat_id: &str,
    after_seq: Option<u64>,
) -> Result<PhoneSocket, String> {
    let url = format!("{}/api/ws", phone_base.replacen("http://", "ws://", 1));
    let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;
    socket
        .send(Message::Text(
            json!({"type": "hello", "token": token, "chat_id": chat_id, "after_seq": after_seq})
                .to_string(),
        ))
        .await
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// The next JSON frame from the phone socket; `None` once it closed.
async fn next_frame(socket: &mut PhoneSocket) -> Result<Option<Value>, String> {
    loop {
        let message = tokio::time::timeout(STEP_TIMEOUT, socket.next())
            .await
            .map_err(|_| "timed out waiting for the phone socket".to_string())?;
        match message {
            None => return Ok(None),
            Some(Err(e)) => return Err(format!("phone socket failed: {e}")),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| e.to_string())
            }
            Some(Ok(Message::Close(frame))) => {
                println!("[phone] socket closed: {frame:?}");
                return Ok(None);
            }
            Some(Ok(_)) => continue,
        }
    }
}

/// Print a phone frame briefly (deltas are skipped).
fn show_frame(frame: &Value) {
    match frame["type"].as_str() {
        Some("event") => {
            let event = &frame["event"]["event"];
            let kind = event["type"].as_str().unwrap_or("?");
            if !matches!(kind, "text_delta" | "thinking_delta" | "item_input_delta") {
                println!(
                    "[phone] #{}{} {kind}",
                    frame["seq"],
                    if frame["redelivered"] == true {
                        " (redelivered)"
                    } else {
                        ""
                    }
                );
            }
        }
        Some("history") => println!(
            "[phone] history ({} events)",
            frame["events"].as_array().map_or(0, Vec::len)
        ),
        _ => println!("[phone] {frame}"),
    }
}

/// The desktop's side: a ClaudeChat attachment and its command channel.
struct Desktop {
    commands: mpsc::UnboundedSender<String>,
    stream: ClaudeChatFrameStream,
}

impl Desktop {
    fn send(&self, command: &HarnessCommand) -> Result<(), String> {
        self.commands
            .send(serde_json::to_string(command).map_err(|e| e.to_string())?)
            .map_err(|_| "command channel closed".to_string())
    }

    /// The next event within `wait`; `Ok(None)` when none came.
    async fn next_within(&mut self, wait: Duration) -> Result<Option<HarnessEvent>, String> {
        match tokio::time::timeout(wait, self.stream.next()).await {
            Err(_) => Ok(None),
            Ok(None) => Err("the desktop stream ended".into()),
            Ok(Some(Err(e))) => Err(format!("desktop stream failed: {e}")),
            Ok(Some(Ok(ClaudeChatFrame::Event { json, .. }))) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| e.to_string()),
            Ok(Some(Ok(ClaudeChatFrame::Opened(opened)))) => {
                Err(format!("unexpected second opened: {opened:?}"))
            }
        }
    }

    async fn until(&mut self, stop: impl Fn(&HarnessEvent) -> bool) -> Result<(), String> {
        loop {
            match self.next_within(STEP_TIMEOUT).await? {
                Some(event) if stop(&event) => return Ok(()),
                Some(_) => {}
                None => return Err("timed out".into()),
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let model = arg("--model").unwrap_or_else(|| "haiku".into());
    let hold = arg("--hold").and_then(|secs| secs.parse::<u64>().ok());
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

    let store = tempfile::tempdir().expect("temp config dir");
    let token = uuid::Uuid::new_v4().to_string();
    std::env::set_var(TOKEN_ENV, &token);
    let agent_addr = free_port();
    let phone_addr = free_port();
    tokio::spawn(server::serve(AgentServerConfig {
        bind_addr: agent_addr,
        auth_token: token.clone(),
        agent_name: "phone-smoke".into(),
        claude_chat_limits: Default::default(),
        phone: Some(PhoneServerConfig {
            bind_addr: phone_addr,
            store_dir: store.path().to_path_buf(),
            required: true,
        }),
    }));
    let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
        remote_id: "phone-smoke".into(),
        name: "phone-smoke".into(),
        endpoint: format!("http://{agent_addr}"),
        token_ref: format!("env:{TOKEN_ENV}"),
    });
    let phone_base = format!("http://{phone_addr}");

    // 1. Handshake and pages.
    let mut capabilities = Err("no handshake".to_string());
    for _ in 0..50 {
        match backend.handshake().await {
            Ok(h) => {
                capabilities = Ok(h.capabilities);
                break;
            }
            Err(e) => capabilities = Err(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let capabilities = must!("handshake", capabilities);
    check!(
        "handshake advertises phone",
        capabilities.iter().any(|c| c == "phone"),
        format!("{capabilities:?}"),
    );
    for page in ["/", "/pair", "/chat"] {
        let (status, body) = must!(
            "page loads",
            http("GET", &format!("{phone_base}{page}"), None, None).await
        );
        check!(
            &format!("page {page} loads without a token"),
            status == 200 && body.contains("<html"),
            format!("{status}, {} bytes", body.len()),
        );
    }
    let (status, _) = must!(
        "chats without token",
        http("GET", &format!("{phone_base}/api/chats"), None, None).await
    );
    check!(
        "GET /api/chats without a token is 401",
        status == 401,
        status.to_string()
    );

    // 2. Pair.
    let pairing = must!(
        "create pairing",
        backend
            .create_phone_pairing("Smoke iPhone".into(), "view".into())
            .await
    );
    let code = pairing
        .pairing_url
        .split_once('#')
        .map(|(_, code)| code.to_string())
        .unwrap_or_default();
    check!(
        "CreatePhonePairing returns a /pair#code URL on the phone port",
        pairing
            .pairing_url
            .starts_with(&format!("{phone_base}/pair#"))
            && code.len() >= 22
            && pairing.scope == "view",
        format!(
            "{}/pair#<{} chars>, scope {}, expires {}",
            phone_base,
            code.len(),
            pairing.scope,
            pairing.expires_unix_secs
        ),
    );
    let (status, body) = must!(
        "pair",
        http(
            "POST",
            &format!("{phone_base}/api/pair"),
            None,
            Some(json!({"code": code}))
        )
        .await
    );
    let paired: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let device_token = paired["token"].as_str().unwrap_or_default().to_string();
    let phone_id = paired["phone"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    check!(
        "POST /api/pair redeems the code",
        status == 200 && device_token.len() >= 43 && paired["phone"]["scope"] == "view",
        format!(
            "{status}, phone {} ({}), token <{} chars>",
            paired["phone"]["name"],
            phone_id,
            device_token.len()
        ),
    );
    let (status, _) = must!(
        "pair again",
        http(
            "POST",
            &format!("{phone_base}/api/pair"),
            None,
            Some(json!({"code": code}))
        )
        .await
    );
    check!("reusing the code fails", status == 401, status.to_string());
    let stored = std::fs::read_to_string(store.path().join("phones.json")).unwrap_or_default();
    check!(
        "phones.json holds no token or code",
        !stored.is_empty() && !stored.contains(&device_token) && !stored.contains(&code),
        format!("{} bytes", stored.len()),
    );

    // 3. Authenticated listing.
    let (status, _) = must!(
        "chats with token",
        http(
            "GET",
            &format!("{phone_base}/api/chats"),
            Some(&device_token),
            None
        )
        .await
    );
    check!(
        "GET /api/chats with the device token is 200",
        status == 200,
        status.to_string()
    );
    let (status, _) = must!(
        "chats with the agent token",
        http(
            "GET",
            &format!("{phone_base}/api/chats"),
            Some(&token),
            None
        )
        .await
    );
    check!(
        "the agent's own token is not a device token (401)",
        status == 401,
        status.to_string()
    );

    // 4. A real chat, started the way the desktop starts one.
    let workdir = tempfile::tempdir().expect("temp dir");
    let cwd = workdir.path().canonicalize().expect("canonical temp dir");
    let config = ClaudeSessionConfig {
        cwd: cwd.clone(),
        model: Some(model.clone()),
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: None,
        mcp_servers: Vec::new(),
        chrome: false,
    };
    let start_json =
        serde_json::to_string(&RemoteClaudeChatStart::from_config(&config).expect("remote start"))
            .expect("start json");
    let (commands, rx) = mpsc::unbounded_channel();
    let mut stream = must!(
        "desktop starts a chat",
        backend
            .claude_chat_attach(ClaudeChatOpen::Start { start_json }, rx)
            .await
            .map_err(|e| e.to_string())
    );
    let chat_id = match tokio::time::timeout(STEP_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(ClaudeChatFrame::Opened(opened)))) => opened.chat_id,
        other => {
            check!(
                "desktop starts a chat",
                false,
                format!("first frame: {other:?}")
            );
            finish(failures);
        }
    };
    let mut desktop = Desktop { commands, stream };
    println!("desktop opened chat {chat_id}");

    let mut socket = must!(
        "phone socket",
        phone_socket(&phone_base, &device_token, &chat_id, None).await
    );
    let opened = must!("phone opened", next_frame(&mut socket).await);
    let opened = opened.unwrap_or(Value::Null);
    show_frame(&opened);
    check!(
        "the phone socket opens the running chat in view scope",
        opened["type"] == "opened" && opened["chat_id"] == chat_id && opened["scope"] == "view",
        opened.to_string(),
    );
    let history = must!("phone history", next_frame(&mut socket).await).unwrap_or(Value::Null);
    show_frame(&history);
    check!(
        "a first view gets the history first",
        history["type"] == "history",
        history["type"].to_string(),
    );

    must!(
        "desktop sends a prompt",
        desktop.send(&HarnessCommand::SendUserMessage(
            "Reply with exactly one word: pong".into()
        ))
    );
    let mut seqs: Vec<u64> = Vec::new();
    let mut kinds: Vec<String> = Vec::new();
    let mut session_id = String::new();
    loop {
        let Some(frame) = must!("phone live events", next_frame(&mut socket).await) else {
            check!("phone live events", false, "socket closed early".into());
            finish(failures);
        };
        show_frame(&frame);
        if frame["type"] != "event" {
            continue;
        }
        let event = &frame["event"]["event"];
        if frame["redelivered"] != true {
            seqs.push(frame["seq"].as_u64().unwrap_or(0));
        }
        let kind = event["type"].as_str().unwrap_or_default().to_string();
        if kind == "turn_started" {
            if let Some(id) = event["data"]["session_id"].as_str() {
                session_id = id.to_string();
            }
        }
        let done = kind == "turn_completed";
        kinds.push(kind);
        if done {
            break;
        }
    }
    let increasing = seqs.windows(2).all(|pair| pair[1] == pair[0] + 1);
    check!(
        "the phone sees the turn live with consecutive seqs",
        !seqs.is_empty()
            && increasing
            && kinds.iter().any(|k| k == "turn_started")
            && kinds.iter().any(|k| k == "text_delta"),
        format!(
            "seqs {}..={} ({} events), session {session_id}",
            seqs.first().unwrap_or(&0),
            seqs.last().unwrap_or(&0),
            seqs.len()
        ),
    );
    must!(
        "desktop sees the turn end",
        desktop
            .until(|e| matches!(e, HarnessEvent::TurnCompleted { .. }))
            .await
    );

    // 5. A viewing socket cannot send.
    must!(
        "phone tries to send",
        socket
            .send(Message::Text(
                json!({"type": "command", "command": {"type": "send_user_message", "data": {"text": "Say hacked", "images": [], "id": null}}})
                    .to_string()
            ))
            .await
            .map_err(|e| e.to_string())
    );
    let refused = must!("phone refused", next_frame(&mut socket).await).unwrap_or(Value::Null);
    show_frame(&refused);
    check!(
        "a view-scope socket's command is refused",
        refused["type"] == "refused",
        refused.to_string(),
    );
    let mut started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        match must!(
            "desktop watches",
            desktop.next_within(Duration::from_millis(500)).await
        ) {
            Some(HarnessEvent::TurnStarted { .. }) => {
                started = true;
                break;
            }
            Some(_) | None => {}
        }
    }
    check!(
        "Claude starts no turn for the refused command",
        !started,
        "no turn_started within 8s".into(),
    );

    // 6. Listing and history.
    let (status, body) = must!(
        "chats while running",
        http(
            "GET",
            &format!("{phone_base}/api/chats"),
            Some(&device_token),
            None
        )
        .await
    );
    let listing: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let running = listing["running"]
        .as_array()
        .and_then(|chats| chats.iter().find(|chat| chat["chat_id"] == chat_id))
        .cloned()
        .unwrap_or(Value::Null);
    check!(
        "GET /api/chats lists the running chat",
        status == 200 && running["claude_session_id"] == session_id,
        format!(
            "state {}, repo {}, title {}, {} recent",
            running["state"],
            running["repo"],
            running["title"],
            listing["recent"].as_array().map_or(0, Vec::len)
        ),
    );
    let (status, body) = must!(
        "history",
        http(
            "GET",
            &format!("{phone_base}/api/history?session={session_id}"),
            Some(&device_token),
            None
        )
        .await
    );
    let history: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let events = history["events"].as_array().cloned().unwrap_or_default();
    check!(
        "GET /api/history returns page payloads",
        status == 200
            && events
                .iter()
                .any(|e| e["type"] == "user_prompt"
                    && e["text"] == "Reply with exactly one word: pong"),
        format!("{status}, {} events", events.len()),
    );

    // 7. Revoke.
    let revoked = must!(
        "revoke",
        backend
            .revoke_phone(phone_id.clone())
            .await
            .map_err(|e| e.to_string())
    );
    let mut closed_with_notice = false;
    loop {
        match must!("socket after revoke", next_frame(&mut socket).await) {
            Some(frame) => {
                show_frame(&frame);
                if frame["type"] == "revoked" {
                    closed_with_notice = true;
                }
            }
            None => break,
        }
    }
    check!(
        "revoking closes the phone's socket",
        revoked && closed_with_notice,
        format!("revoked {revoked}, revoked notice then close {closed_with_notice}"),
    );
    let (status, _) = must!(
        "chats after revoke",
        http(
            "GET",
            &format!("{phone_base}/api/chats"),
            Some(&device_token),
            None
        )
        .await
    );
    check!(
        "GET /api/chats with the revoked token is 401",
        status == 401,
        status.to_string()
    );
    let phones = must!(
        "list phones",
        backend.list_phones().await.map_err(|e| e.to_string())
    );
    check!(
        "ListPhones no longer lists it",
        phones.iter().all(|phone| phone.id != phone_id),
        format!("{} phones", phones.len()),
    );

    if let Some(secs) = hold {
        let pairing = must!(
            "hold pairing",
            backend
                .create_phone_pairing("Browser check".into(), "view".into())
                .await
        );
        println!("\nHOLD {secs}s");
        println!("PAIR_URL {}", pairing.pairing_url);
        println!("CHAT_ID {chat_id}");
        println!("SESSION_ID {session_id}");
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    // 8. Throttle (last: it holds back 127.0.0.1). A good pairing first
    // clears the address; then five failures pass through as 401 and the
    // fifth makes the address wait, so the next attempts are 429.
    let pairing = must!(
        "throttle baseline pairing",
        backend
            .create_phone_pairing("Throttle check".into(), "view".into())
            .await
    );
    let code = pairing
        .pairing_url
        .split_once('#')
        .map(|(_, code)| code.to_string())
        .unwrap_or_default();
    let (status, _) = must!(
        "throttle baseline pair",
        http(
            "POST",
            &format!("{phone_base}/api/pair"),
            None,
            Some(json!({"code": code}))
        )
        .await
    );
    let mut statuses = vec![status];
    for _ in 0..7 {
        let (status, _) = must!(
            "bad token",
            http(
                "GET",
                &format!("{phone_base}/api/chats"),
                Some("not-a-token"),
                None
            )
            .await
        );
        statuses.push(status);
    }
    check!(
        "repeated bad tokens are throttled",
        statuses == [200, 401, 401, 401, 401, 401, 429, 429],
        format!("{statuses:?}"),
    );

    let _ = desktop.send(&HarnessCommand::Shutdown);
    tokio::time::sleep(Duration::from_millis(500)).await;
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
