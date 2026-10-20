//! The agent's phone web app (TRU-151).
//!
//! A small HTTP server beside the gRPC one, on the agent's host at the
//! next port up (`--phone-addr` overrides, `off` disables). Plain HTTP:
//! the tailnet's WireGuard already encrypts it, and every URL the pages use
//! is relative, so `tailscale serve` can front it with HTTPS later.
//!
//! Reachable without a device token: the three pages (`/`, `/pair`,
//! `/chat`, which carry no data) and `POST /api/pair` (which needs a valid
//! one-time code). Everything else needs `Authorization: Bearer <device
//! token>`, or, on the WebSocket, the token in the first message (browsers
//! cannot set headers on a WebSocket, and a token in the URL would be
//! logged by proxies).
//!
//! Live chats travel over a WebSocket rather than server-sent events: it
//! carries the token in its first frame instead of the URL, and a phone's
//! commands ride the same socket the way the desktop's gRPC stream carries
//! them. After its hello, a phone paired with act scope may send exactly
//! three commands (`PhoneCommand`): a message, an answer to a permission
//! prompt or question, and an interrupt. It can never stop the chat,
//! change its model or mode, or start one. Every message from a view-scope
//! phone, and anything else from an act-scope one, is refused here,
//! server-side, and never reaches Claude.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use warp::http::{header, Response, StatusCode};
use warp::hyper::Body;
use warp::ws::{Message, WebSocket};
use warp::Filter;

use super::claude_chats::{Chat, ChatActivity, ChatAttachment, ClaudeChatRegistry, Gap, SeqEvent};
use super::phones::{PairError, PhoneInfo, PhoneRegistry, Throttle};
use crate::harness::{HarnessCommand, RuntimeDecision, UserPrompt};

/// Where the phone server listens and keeps its paired phones.
#[derive(Debug, Clone)]
pub struct PhoneServerConfig {
    pub bind_addr: SocketAddr,
    /// The agent's config dir; `phones.json` lives here.
    pub store_dir: PathBuf,
    /// Asked for explicitly (`--phone-addr`): failing to start is an error
    /// rather than a logged "phone access disabled".
    pub required: bool,
}

/// Recent conversations listed beside the running chats.
const RECENT_CHATS: usize = 30;
/// A socket must say hello this soon after it opens.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Pings keep mobile networks from dropping an idle socket and find
/// phones that went away without closing it.
const SOCKET_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Longest message text a phone may send, in bytes.
const MAX_PHONE_PROMPT_BYTES: usize = 64 * 1024;
/// Largest socket message: room for a prompt at its cap once JSON-escaped,
/// or an answer carrying an edited tool input. Anything larger fails the
/// socket.
const MAX_SOCKET_MESSAGE_BYTES: usize = 256 * 1024;
/// Longest runtime request id a phone may name.
const MAX_REQUEST_ID_CHARS: usize = 256;
const MAX_PAIR_BODY_BYTES: u64 = 4 * 1024;

/// WebSocket close codes the pages act on.
const CLOSE_UNAUTHORIZED: u16 = 4401;
const CLOSE_REVOKED: u16 = 4403;
const CLOSE_NOT_RUNNING: u16 = 4404;
const CLOSE_GAP: u16 = 4409;
const CLOSE_THROTTLED: u16 = 4429;

const INDEX_HTML: &str = include_str!("../../assets/phone/index.html");
const PAIR_HTML: &str = include_str!("../../assets/phone/pair.html");
const CHAT_HEAD: &str = include_str!("../../assets/phone/chat_head.html");
const CHAT_SHIM: &str = include_str!("../../assets/phone/chat_shim.html");

/// The desktop chat page made a phone page: a viewport and mobile styles
/// in its head, and the socket shim that feeds `__appendEvent` before its
/// closing body tag.
pub fn phone_chat_html() -> Result<String, String> {
    let page = crate::chat_page::agent_chat_html();
    let head = page
        .find("</head>")
        .ok_or("the chat page has no </head> to add the phone styles to")?;
    let body = page
        .rfind("</body>")
        .ok_or("the chat page has no </body> to add the phone shim to")?;
    let mut html = String::with_capacity(page.len() + CHAT_HEAD.len() + CHAT_SHIM.len());
    html.push_str(&page[..head]);
    html.push_str(CHAT_HEAD);
    html.push_str(&page[head..body]);
    html.push_str(CHAT_SHIM);
    html.push_str(&page[body..]);
    Ok(html)
}

#[derive(Clone)]
pub struct PhoneState {
    agent_name: String,
    phones: Arc<PhoneRegistry>,
    claude_chats: Arc<ClaudeChatRegistry>,
    throttle: Arc<Mutex<Throttle>>,
    chat_html: warp::hyper::body::Bytes,
}

impl PhoneState {
    pub fn new(
        agent_name: String,
        phones: Arc<PhoneRegistry>,
        claude_chats: Arc<ClaudeChatRegistry>,
    ) -> Result<Self, String> {
        Ok(Self {
            agent_name,
            phones,
            claude_chats,
            throttle: Arc::new(Mutex::new(Throttle::default())),
            chat_html: phone_chat_html()?.into(),
        })
    }

    fn throttle(&self) -> std::sync::MutexGuard<'_, Throttle> {
        self.throttle.lock().expect("phone throttle poisoned")
    }

    /// `Err(wait)` while `ip` is held back for earlier failures.
    fn throttled(&self, ip: IpAddr) -> Result<(), Duration> {
        self.throttle().check(ip, Instant::now())
    }

    fn failed(&self, ip: IpAddr) {
        self.throttle().record_failure(ip, Instant::now());
    }

    fn succeeded(&self, ip: IpAddr) {
        self.throttle().record_success(ip);
    }

    /// The phone a request's bearer token belongs to.
    fn authorize(&self, authorization: Option<&str>, ip: IpAddr) -> Result<PhoneInfo, ApiError> {
        if let Err(wait) = self.throttled(ip) {
            return Err(ApiError::Throttled(wait));
        }
        let Some(token) = authorization.and_then(|value| value.strip_prefix("Bearer ")) else {
            return Err(ApiError::Unauthorized("this phone is not paired"));
        };
        self.check_token(token, ip).ok_or(ApiError::Unauthorized(
            "this phone is not paired (or was revoked)",
        ))
    }

    fn check_token(&self, token: &str, ip: IpAddr) -> Option<PhoneInfo> {
        match self.phones.authenticate(token) {
            Some(phone) => {
                self.succeeded(ip);
                Some(phone)
            }
            None => {
                eprintln!("[phone] {ip}: a request carried an unknown device token");
                self.failed(ip);
                None
            }
        }
    }
}

enum ApiError {
    Unauthorized(&'static str),
    Throttled(Duration),
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl ApiError {
    fn into_response(self) -> Response<Body> {
        match self {
            Self::Unauthorized(message) => {
                json_response(StatusCode::UNAUTHORIZED, &json!({ "error": message }))
            }
            Self::Throttled(wait) => {
                let secs = wait.as_secs().max(1);
                let mut response = json_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    &json!({ "error": format!("too many failed attempts; try again in {secs}s") }),
                );
                if let Ok(value) = secs.to_string().parse() {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
                response
            }
            Self::BadRequest(message) => {
                json_response(StatusCode::BAD_REQUEST, &json!({ "error": message }))
            }
            Self::NotFound(message) => {
                json_response(StatusCode::NOT_FOUND, &json!({ "error": message }))
            }
            Self::Internal(message) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": message }),
            ),
        }
    }
}

/// Headers every response carries: no caching of data, no framing, no
/// sniffing, no referrers, and a CSP that keeps rendered agent output from
/// loading anything off the page.
fn secure(mut response: Response<Body>) -> Response<Body> {
    let headers = response.headers_mut();
    let pairs = [
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             img-src data:; connect-src 'self' ws: wss:; base-uri 'none'; form-action 'none'; \
             frame-ancestors 'none'",
        ),
    ];
    for (name, value) in pairs {
        headers.insert(name, header::HeaderValue::from_static(value));
    }
    response
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    secure(response)
}

fn html_response(html: impl Into<Body>) -> Response<Body> {
    let mut response = Response::new(html.into());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    secure(response)
}

fn client_ip(remote: Option<SocketAddr>) -> IpAddr {
    remote
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// Every route of the phone server.
pub fn routes(
    state: PhoneState,
) -> impl Filter<Extract = (Response<Body>,), Error = warp::Rejection> + Clone {
    let with_state = warp::any().map(move || state.clone());
    let remote = warp::addr::remote();
    let auth = warp::header::optional::<String>("authorization");

    let index = warp::get()
        .and(warp::path::end())
        .map(|| html_response(INDEX_HTML));
    let pair_page = warp::get()
        .and(warp::path!("pair"))
        .map(|| html_response(PAIR_HTML));
    let chat_page = warp::get()
        .and(warp::path!("chat"))
        .and(with_state.clone())
        .map(|state: PhoneState| html_response(state.chat_html));

    let pair = warp::post()
        .and(warp::path!("api" / "pair"))
        .and(warp::body::content_length_limit(MAX_PAIR_BODY_BYTES))
        .and(warp::body::bytes())
        .and(remote)
        .and(with_state.clone())
        .and_then(handle_pair);
    let chats = warp::get()
        .and(warp::path!("api" / "chats"))
        .and(auth)
        .and(remote)
        .and(with_state.clone())
        .and_then(handle_chats);
    let history = warp::get()
        .and(warp::path!("api" / "history"))
        .and(warp::query::<HistoryQuery>())
        .and(auth)
        .and(remote)
        .and(with_state.clone())
        .and_then(handle_history);
    let socket = warp::path!("api" / "ws")
        .and(warp::ws())
        .and(remote)
        .and(with_state)
        .map(
            |ws: warp::ws::Ws, remote: Option<SocketAddr>, state: PhoneState| {
                let ip = client_ip(remote);
                ws.max_message_size(MAX_SOCKET_MESSAGE_BYTES)
                    .max_frame_size(MAX_SOCKET_MESSAGE_BYTES)
                    .on_upgrade(move |socket| phone_socket(state, socket, ip))
            },
        )
        .map(warp::reply::Reply::into_response);

    index
        .or(pair_page)
        .unify()
        .or(chat_page)
        .unify()
        .or(pair)
        .unify()
        .or(chats)
        .unify()
        .or(history)
        .unify()
        .or(socket)
        .unify()
}

/// Bind the phone server and run it on its own task. Returns the address
/// it listens on.
pub fn spawn(state: PhoneState, bind_addr: SocketAddr) -> Result<SocketAddr, String> {
    let (addr, server) = warp::serve(routes(state))
        .try_bind_ephemeral(bind_addr)
        .map_err(|err| format!("could not listen on {bind_addr}: {err}"))?;
    tokio::spawn(server);
    Ok(addr)
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
}

async fn handle_pair(
    body: warp::hyper::body::Bytes,
    remote: Option<SocketAddr>,
    state: PhoneState,
) -> Result<Response<Body>, Infallible> {
    let ip = client_ip(remote);
    if let Err(wait) = state.throttled(ip) {
        return Ok(ApiError::Throttled(wait).into_response());
    }
    let Ok(body) = serde_json::from_slice::<PairBody>(&body) else {
        return Ok(ApiError::BadRequest("expected {\"code\": \"...\"}".into()).into_response());
    };
    match state.phones.redeem(body.code.trim()) {
        Ok(paired) => {
            state.succeeded(ip);
            eprintln!(
                "[phone] {ip}: paired phone {:?} ({}, {})",
                paired.phone.name,
                paired.phone.id,
                paired.phone.scope.label()
            );
            Ok(json_response(
                StatusCode::OK,
                &json!({
                    "token": paired.token,
                    "phone": phone_json(&paired.phone),
                    "agent": state.agent_name,
                }),
            ))
        }
        Err(PairError::InvalidCode) => {
            eprintln!("[phone] {ip}: a pairing attempt named no valid code");
            state.failed(ip);
            Ok(ApiError::Unauthorized(
                "this pairing code is not valid; it may have been used or expired",
            )
            .into_response())
        }
        Err(PairError::Storage(err)) => {
            eprintln!("[phone] {ip}: a pairing could not be saved: {err}");
            Ok(
                ApiError::Internal(format!("the agent could not save the pairing: {err}"))
                    .into_response(),
            )
        }
    }
}

fn phone_json(phone: &PhoneInfo) -> Value {
    json!({
        "id": phone.id,
        "name": phone.name,
        "scope": phone.scope.label(),
    })
}

fn activity_label(activity: ChatActivity) -> &'static str {
    match activity {
        ChatActivity::Idle => "idle",
        ChatActivity::TurnRunning => "turn_running",
        ChatActivity::AwaitingInput => "awaiting_input",
    }
}

fn unix_secs(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn handle_chats(
    authorization: Option<String>,
    remote: Option<SocketAddr>,
    state: PhoneState,
) -> Result<Response<Body>, Infallible> {
    let ip = client_ip(remote);
    let phone = match state.authorize(authorization.as_deref(), ip) {
        Ok(phone) => phone,
        Err(err) => return Ok(err.into_response()),
    };
    let running = state.claude_chats.list();
    let listing = tokio::task::spawn_blocking(move || chats_listing(running)).await;
    match listing {
        Ok((running, recent)) => Ok(json_response(
            StatusCode::OK,
            &json!({
                "agent": state.agent_name,
                "phone": phone_json(&phone),
                "running": running,
                "recent": recent,
            }),
        )),
        Err(err) => {
            eprintln!("[phone] {}: the chat listing failed: {err}", phone.id);
            Ok(ApiError::Internal(format!("the chat listing failed: {err}")).into_response())
        }
    }
}

/// The running chats (newest activity first) with titles and repos from the
/// chats index, and the most recent Claude conversations. Blocking.
fn chats_listing(running: Vec<super::claude_chats::ChatInfo>) -> (Vec<Value>, Vec<Value>) {
    let index = crate::chats::build_local_index();
    let by_session = |session: &str| index.iter().find(|entry| entry.id == session);
    let mut running_rows: Vec<(u64, Value)> = running
        .into_iter()
        .map(|chat| {
            let entry = chat.claude_session_id.as_deref().and_then(by_session);
            let repo = match entry {
                Some(entry) => entry.group_name(),
                None => repo_name_for(std::path::Path::new(&chat.cwd)),
            };
            let row = json!({
                "chat_id": chat.chat_id,
                "claude_session_id": chat.claude_session_id,
                "state": activity_label(chat.activity),
                "cwd": chat.cwd,
                "repo": repo,
                "title": entry.map(|entry| entry.title.clone()),
                "created_unix_secs": chat.created_unix_secs,
                "last_activity_unix_secs": chat.last_activity_unix_secs,
                "attached_clients": chat.attached_clients,
            });
            (chat.last_activity_unix_secs, row)
        })
        .collect();
    running_rows.sort_by_key(|(last, _)| std::cmp::Reverse(*last));
    let running_ids: Vec<Option<(String, String)>> = running_rows
        .iter()
        .map(|(_, row)| {
            Some((
                row["claude_session_id"].as_str()?.to_string(),
                row["chat_id"].as_str()?.to_string(),
            ))
        })
        .collect();
    let recent = index
        .iter()
        .filter(|entry| entry.backend == crate::chats::ChatBackend::Claude)
        .take(RECENT_CHATS)
        .map(|entry| {
            let running_chat = running_ids
                .iter()
                .flatten()
                .find(|(session, _)| *session == entry.id)
                .map(|(_, chat)| chat.clone());
            json!({
                "session_id": entry.id,
                "title": entry.title,
                "cwd": entry.cwd.to_string_lossy(),
                "repo": entry.group_name(),
                "branch": entry.branch,
                "mtime_unix_secs": unix_secs(entry.mtime),
                "running_chat_id": running_chat,
            })
        })
        .collect();
    (
        running_rows.into_iter().map(|(_, row)| row).collect(),
        recent,
    )
}

/// The repo a cwd belongs to (its main checkout's name), or the cwd's own
/// name when it is not in one.
fn repo_name_for(cwd: &std::path::Path) -> String {
    let root = crate::chats::resolve_repo_root(cwd).map(|(root, _)| root);
    let named = root.as_deref().unwrap_or(cwd);
    named
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| named.to_string_lossy().into_owned())
}

#[derive(Deserialize)]
struct HistoryQuery {
    session: String,
}

/// A Claude session id names a transcript file: plain characters only.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn handle_history(
    query: HistoryQuery,
    authorization: Option<String>,
    remote: Option<SocketAddr>,
    state: PhoneState,
) -> Result<Response<Body>, Infallible> {
    let ip = client_ip(remote);
    let phone = match state.authorize(authorization.as_deref(), ip) {
        Ok(phone) => phone,
        Err(err) => return Ok(err.into_response()),
    };
    if !valid_session_id(&query.session) {
        return Ok(ApiError::BadRequest("invalid session id".into()).into_response());
    }
    let session = query.session.clone();
    let loaded = tokio::task::spawn_blocking(move || history_events(&session)).await;
    match loaded {
        Ok(Ok(Some(events))) => Ok(json_response(
            StatusCode::OK,
            &json!({ "session_id": query.session, "events": events }),
        )),
        Ok(Ok(None)) => Ok(ApiError::NotFound(format!(
            "no transcript for Claude session {}",
            query.session
        ))
        .into_response()),
        Ok(Err(err)) => {
            eprintln!(
                "[phone] {}: history of {} failed: {err}",
                phone.id, query.session
            );
            Ok(ApiError::Internal(err).into_response())
        }
        Err(err) => {
            eprintln!(
                "[phone] {}: history task for {} failed: {err}",
                phone.id, query.session
            );
            Ok(ApiError::Internal(format!("history task failed: {err}")).into_response())
        }
    }
}

/// A Claude session's transcript as chat page payloads; `None` when this
/// machine has no transcript for it. Blocking.
fn history_events(session_id: &str) -> Result<Option<Vec<Value>>, String> {
    let Some(path) = crate::chats::claude_session_path(session_id) else {
        return Ok(None);
    };
    let entries = crate::harness::transcript::load_claude_history(&path)
        .map_err(|err| format!("could not read transcript {}: {err}", path.display()))?;
    Ok(Some(crate::chat_page::history_payloads(entries)))
}

/// A socket's first message.
#[derive(Deserialize)]
struct SocketHello {
    #[serde(rename = "type")]
    kind: String,
    token: String,
    chat_id: String,
    /// The last seq this page saw of the chat (a reconnect).
    #[serde(default)]
    after_seq: Option<u64>,
}

type SocketSink = futures_util::stream::SplitSink<WebSocket, Message>;

async fn send_json(sink: &mut SocketSink, value: Value) -> Result<(), ()> {
    send(sink, Message::text(value.to_string())).await
}

/// Send one message; `Err` when the phone is gone.
async fn send(sink: &mut SocketSink, message: Message) -> Result<(), ()> {
    sink.send(message).await.map_err(|_| ())
}

async fn close_with(sink: &mut SocketSink, notice: Value, code: u16, reason: &'static str) {
    let _ = send_json(sink, notice).await;
    let _ = sink.send(Message::close_with(code, reason)).await;
    let _ = sink.close().await;
}

/// One relayed event as the page receives it: the seq, and the payload
/// `__appendEvent` takes. Built around the event's stored JSON so nothing
/// is decoded again.
fn event_frame(event: &SeqEvent, redelivered: bool) -> String {
    format!(
        r#"{{"type":"event","seq":{},"redelivered":{},"event":{{"kind":"harness","event":{}}}}}"#,
        event.seq, redelivered, event.json
    )
}

/// A phone viewing a running chat: hello (token, chat, last seq seen),
/// then `opened`, the history when the page has none to continue from,
/// the redelivered standing events, any replay, and live events, until
/// the chat exits, the phone goes, or the phone is revoked.
async fn phone_socket(state: PhoneState, socket: WebSocket, ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(message))) if message.is_text() => {
            serde_json::from_slice::<SocketHello>(message.as_bytes()).ok()
        }
        Ok(Some(Ok(_))) | Ok(Some(Err(_))) | Ok(None) => None,
        Err(_) => {
            eprintln!("[phone] {ip}: a socket sent no hello within {HELLO_TIMEOUT:?}");
            None
        }
    };
    let Some(hello) = hello.filter(|hello| hello.kind == "hello") else {
        return close_with(
            &mut sink,
            json!({"type": "error", "message": "expected a hello first"}),
            CLOSE_UNAUTHORIZED,
            "no hello",
        )
        .await;
    };
    if let Err(wait) = state.throttled(ip) {
        return close_with(
            &mut sink,
            json!({"type": "error", "message": format!("too many failed attempts; try again in {}s", wait.as_secs().max(1))}),
            CLOSE_THROTTLED,
            "throttled",
        )
        .await;
    }
    let Some(phone) = state.check_token(&hello.token, ip) else {
        return close_with(
            &mut sink,
            json!({"type": "unauthorized", "message": "this phone is not paired (or was revoked)"}),
            CLOSE_UNAUTHORIZED,
            "unauthorized",
        )
        .await;
    };
    let revoked = state.phones.revocation(&phone.id);
    let who = format!("{} ({})", phone.name, phone.id);

    let attachment = if valid_session_id(&hello.chat_id) {
        state.claude_chats.attach(&hello.chat_id, hello.after_seq)
    } else {
        None
    };
    let Some(mut attachment) = attachment else {
        return close_with(
            &mut sink,
            json!({"type": "not_running", "chat_id": hello.chat_id}),
            CLOSE_NOT_RUNNING,
            "not running",
        )
        .await;
    };
    let chat_id = attachment.opened.chat_id.clone();
    eprintln!(
        "[phone] {who}: viewing chat {chat_id} from {ip} (after seq {:?})",
        hello.after_seq
    );

    match relay(
        &mut sink,
        &mut stream,
        &mut attachment,
        &revoked,
        &phone,
        hello.after_seq,
    )
    .await
    {
        Ok(Ended::Exited) => {
            let _ = send_json(&mut sink, json!({"type": "exited"})).await;
            let _ = sink.send(Message::close_with(1000u16, "chat exited")).await;
            eprintln!("[phone] {who}: chat {chat_id} exited; socket closed");
        }
        Ok(Ended::Revoked) => {
            close_with(
                &mut sink,
                json!({"type": "revoked"}),
                CLOSE_REVOKED,
                "revoked",
            )
            .await;
            eprintln!("[phone] {who}: revoked; closed its socket on chat {chat_id}");
        }
        Ok(Ended::Gap) => {
            close_with(&mut sink, json!({"type": "gap"}), CLOSE_GAP, "gap").await;
            eprintln!(
                "[phone] {who}: fell behind chat {chat_id}'s buffer; closed so it reattaches"
            );
        }
        Ok(Ended::PhoneLeft) => {
            eprintln!("[phone] {who}: stopped viewing chat {chat_id}");
        }
        Err(why) => eprintln!("[phone] {who}: chat {chat_id}: {why}; detaching"),
    }
}

enum Ended {
    Exited,
    Revoked,
    Gap,
    PhoneLeft,
}

async fn relay(
    sink: &mut SocketSink,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    attachment: &mut ChatAttachment,
    revoked: &tokio_util::sync::CancellationToken,
    phone: &PhoneInfo,
    after_seq: Option<u64>,
) -> Result<Ended, String> {
    let gone = |_| "the phone went away".to_string();
    let opened = attachment.opened.clone();
    send_json(
        sink,
        json!({
            "type": "opened",
            "chat_id": opened.chat_id,
            "claude_session_id": opened.claude_session_id,
            "head_seq": opened.head_seq,
            "gap": opened.gap,
            "scope": phone.scope.label(),
        }),
    )
    .await
    .map_err(gone)?;

    // A page with nothing to continue from (a first view, or a gap) gets
    // the history; live events follow it.
    if after_seq.is_none() || opened.gap {
        let events = match opened.claude_session_id.clone() {
            Some(session) => {
                match tokio::task::spawn_blocking(move || history_events(&session)).await {
                    Ok(Ok(events)) => events.unwrap_or_default(),
                    Ok(Err(err)) => {
                        eprintln!(
                            "[phone] {}: chat {}: history unavailable: {err}",
                            phone.id, opened.chat_id
                        );
                        Vec::new()
                    }
                    Err(err) => {
                        eprintln!(
                            "[phone] {}: chat {}: history task failed: {err}",
                            phone.id, opened.chat_id
                        );
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        send_json(sink, json!({"type": "history", "events": events}))
            .await
            .map_err(gone)?;
    }

    for event in std::mem::take(&mut attachment.redelivered) {
        send(sink, Message::text(event_frame(&event, true)))
            .await
            .map_err(gone)?;
    }
    let mut position = 0;
    for event in std::mem::take(&mut attachment.replay) {
        position = event.seq;
        send(sink, Message::text(event_frame(&event, false)))
            .await
            .map_err(gone)?;
        if event.exited {
            return Ok(Ended::Exited);
        }
    }
    position = position.max(attachment.position);

    let mut ping = tokio::time::interval(SOCKET_PING_INTERVAL);
    ping.tick().await;
    loop {
        tokio::select! {
            _ = revoked.cancelled() => return Ok(Ended::Revoked),
            _ = ping.tick() => {
                send(sink, Message::ping(Vec::new())).await.map_err(gone)?;
            }
            incoming = stream.next() => {
                let message = match incoming {
                    None => return Ok(Ended::PhoneLeft),
                    Some(Err(err)) => return Err(format!("socket failed ({err})")),
                    Some(Ok(message)) => message,
                };
                if message.is_close() {
                    return Ok(Ended::PhoneLeft);
                }
                if message.is_ping() || message.is_pong() {
                    continue;
                }
                // Anything else is an attempt to act on the chat.
                if let Some(reply) = act(phone, &attachment.chat, &message) {
                    send_json(sink, reply).await.map_err(gone)?;
                }
            }
            live = attachment.live.recv() => {
                let events = match live {
                    Ok(event) => vec![event],
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        match attachment.chat.events_after(position) {
                            Ok(events) => events,
                            Err(Gap) => return Ok(Ended::Gap),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Ok(Ended::Exited);
                    }
                };
                for event in events {
                    if event.seq <= position {
                        continue;
                    }
                    position = event.seq;
                    send(sink, Message::text(event_frame(&event, false)))
                        .await
                        .map_err(gone)?;
                    if event.exited {
                        return Ok(Ended::Exited);
                    }
                }
            }
        }
    }
}

/// The commands an act-scope phone may send on its socket.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PhoneCommand {
    /// A user message. `id` (a UUID the page makes) becomes the message's
    /// `UserPrompt::id`, so the page knows its own echo.
    Send { id: String, text: String },
    /// An answer to a runtime request, in the chat page's own
    /// `RuntimeDecision` shape.
    Answer {
        request_id: String,
        decision: RuntimeDecision,
    },
    /// Stop the running turn (not the chat).
    Interrupt,
}

impl PhoneCommand {
    fn kind(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::Answer { .. } => "answer",
            Self::Interrupt => "interrupt",
        }
    }
}

/// Why a phone's message did not reach Claude, as the frame the phone
/// gets: `refused` (not allowed), `invalid` (malformed or too large) or
/// `failed` (allowed, but the chat could not take it). Carries the
/// message's `id` or `request_id` when it had one.
fn rejection(kind: &str, message: impl Into<String>, about: Option<(&str, &str)>) -> Value {
    let mut frame = json!({"type": kind, "message": message.into()});
    if let Some((key, value)) = about {
        frame[key] = json!(value);
    }
    frame
}

/// Read and check one message from a phone: the command it asks for, or
/// the frame that turns it down. Never panics on any input.
fn phone_command(phone: &PhoneInfo, chat: &Chat, message: &Message) -> Result<PhoneCommand, Value> {
    if let Err(refusal) = phone.scope.may_send_commands() {
        return Err(rejection("refused", refusal, None));
    }
    let Ok(text) = message.to_str() else {
        return Err(rejection(
            "invalid",
            "commands are JSON text messages",
            None,
        ));
    };
    let value: Value = serde_json::from_str(text)
        .map_err(|err| rejection("invalid", format!("a command must be JSON ({err})"), None))?;
    match value.get("type").and_then(Value::as_str) {
        Some("send" | "answer" | "interrupt") => {}
        Some(other) => {
            return Err(rejection(
                "refused",
                format!(
                    "a phone can send a message, answer a prompt or stop the turn; {other:?} is not allowed"
                ),
                None,
            ))
        }
        None => return Err(rejection("invalid", "a command needs a \"type\"", None)),
    }
    let command: PhoneCommand = serde_json::from_value(value)
        .map_err(|err| rejection("invalid", format!("malformed command: {err}"), None))?;
    match &command {
        PhoneCommand::Send { id, text } => {
            let about = Some(("id", id.as_str()));
            if uuid::Uuid::parse_str(id).is_err() {
                return Err(rejection("invalid", "a message id must be a UUID", about));
            }
            if text.trim().is_empty() {
                return Err(rejection("invalid", "the message is empty", about));
            }
            if text.len() > MAX_PHONE_PROMPT_BYTES {
                return Err(rejection(
                    "invalid",
                    format!(
                        "the message is {} KiB; a phone can send at most {} KiB",
                        text.len().div_ceil(1024),
                        MAX_PHONE_PROMPT_BYTES / 1024
                    ),
                    about,
                ));
            }
        }
        PhoneCommand::Answer { request_id, .. } => {
            if request_id.is_empty() || request_id.chars().count() > MAX_REQUEST_ID_CHARS {
                return Err(rejection("invalid", "invalid request id", None));
            }
            // Answered already (here or on another client), or ended with
            // its turn: say so to this phone alone rather than have Claude
            // report it to every client.
            if !chat.awaits_answer(request_id) {
                return Err(rejection(
                    "refused",
                    "that prompt was already answered",
                    Some(("request_id", request_id)),
                ));
            }
        }
        PhoneCommand::Interrupt => {}
    }
    Ok(command)
}

/// Act on one message from a phone: pass an allowed command to the chat,
/// or refuse it. `Some` is the frame to send the phone back; an accepted
/// command needs none (its effects arrive as chat events).
fn act(phone: &PhoneInfo, chat: &Chat, message: &Message) -> Option<Value> {
    let command = match phone_command(phone, chat, message) {
        Ok(command) => command,
        Err(frame) => {
            eprintln!(
                "[phone] {}: {} a message on chat {} ({} scope): {}",
                phone.id,
                frame["type"].as_str().unwrap_or("refused"),
                chat.chat_id(),
                phone.scope.label(),
                frame["message"].as_str().unwrap_or("")
            );
            return Some(frame);
        }
    };
    let kind = command.kind();
    // Logged without the message text.
    let (harness_command, about) = match command {
        PhoneCommand::Send { id, text } => {
            eprintln!(
                "[phone] {}: send {id} ({} bytes) on chat {}",
                phone.id,
                text.len(),
                chat.chat_id()
            );
            let about = ("id", id.clone());
            let prompt = UserPrompt {
                text,
                images: Vec::new(),
                id: Some(id),
            };
            (HarnessCommand::SendUserMessage(prompt), Some(about))
        }
        PhoneCommand::Answer {
            request_id,
            decision,
        } => {
            eprintln!(
                "[phone] {}: answer {request_id} on chat {}",
                phone.id,
                chat.chat_id()
            );
            let about = ("request_id", request_id.clone());
            (
                HarnessCommand::Answer {
                    request_id,
                    decision,
                },
                Some(about),
            )
        }
        PhoneCommand::Interrupt => {
            eprintln!("[phone] {}: interrupt on chat {}", phone.id, chat.chat_id());
            (HarnessCommand::Interrupt, None)
        }
    };
    match chat.send(harness_command) {
        Ok(()) => None,
        Err(err) => {
            eprintln!(
                "[phone] {}: chat {} could not take a {kind}: {err}",
                phone.id,
                chat.chat_id()
            );
            Some(rejection(
                "failed",
                format!("the chat could not take that: {err}"),
                about.as_ref().map(|(key, value)| (*key, value.as_str())),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::claude_chats::{ChatLimits, ChatProcess};
    use super::super::phones::PhoneScope;
    use super::*;
    use crate::harness::claude::ClaudeSessionConfig;
    use crate::harness::{HarnessCommand, HarnessEvent};
    use tokio::sync::mpsc;

    struct FakeProcess(mpsc::UnboundedSender<HarnessCommand>);

    impl ChatProcess for FakeProcess {
        fn send(&self, command: HarnessCommand) -> Result<(), String> {
            self.0.send(command).map_err(|_| "fake gone".to_string())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        state: PhoneState,
        phones: Arc<PhoneRegistry>,
        chats: Arc<ClaudeChatRegistry>,
        /// Events into the fake chat, and the commands it received.
        events: mpsc::UnboundedSender<HarnessEvent>,
        commands: mpsc::UnboundedReceiver<HarnessCommand>,
        chat_id: String,
        /// Holds the chat open, as a desktop client would.
        _desktop: ChatAttachment,
    }

    /// A phone app over a registry running one fake chat.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let phones = PhoneRegistry::load(dir.path()).unwrap();
        let fakes: Arc<Mutex<Vec<FakeChannels>>> = Arc::default();
        type FakeChannels = (
            mpsc::UnboundedSender<HarnessEvent>,
            mpsc::UnboundedReceiver<HarnessCommand>,
        );
        let chats = {
            let fakes = fakes.clone();
            ClaudeChatRegistry::with_parts(
                ChatLimits::default(),
                Arc::new(move |_config| {
                    let (event_tx, event_rx) = mpsc::unbounded_channel();
                    let (command_tx, command_rx) = mpsc::unbounded_channel();
                    fakes.lock().unwrap().push((event_tx, command_rx));
                    (
                        Box::new(FakeProcess(command_tx)) as Box<dyn ChatProcess>,
                        event_rx,
                    )
                }),
                Arc::new(std::time::SystemTime::now),
            )
        };
        let desktop = chats.start_or_attach(
            ClaudeSessionConfig {
                cwd: dir.path().to_path_buf(),
                model: None,
                permission_mode: "default".into(),
                effort: None,
                resume: None,
                wire_log_dir: None,
                mcp_servers: Vec::new(),
                chrome: false,
            },
            None,
        );
        let (events, commands) = fakes.lock().unwrap().pop().unwrap();
        let state = PhoneState::new("test-agent".into(), phones.clone(), chats.clone()).unwrap();
        Fixture {
            _dir: dir,
            state,
            phones,
            chats,
            events,
            commands,
            chat_id: desktop.opened.chat_id.clone(),
            _desktop: desktop,
        }
    }

    fn paired_token(phones: &PhoneRegistry, scope: PhoneScope) -> (String, String) {
        let pairing = phones.create_pairing("phone", scope).unwrap();
        let paired = phones.redeem(&pairing.code).unwrap();
        (paired.token, paired.phone.id)
    }

    /// A socket that said hello to the fixture's chat with a phone of
    /// `scope`, past its `opened` and `history` frames.
    async fn open_socket(fx: &Fixture, scope: PhoneScope) -> warp::test::WsClient {
        let (token, _) = paired_token(&fx.phones, scope);
        let mut client = warp::test::ws()
            .path("/api/ws")
            .handshake(routes(fx.state.clone()))
            .await
            .unwrap();
        client
            .send_text(json!({"type": "hello", "token": token, "chat_id": fx.chat_id}).to_string())
            .await;
        let opened = recv_json(&mut client).await;
        assert_eq!(opened["type"], "opened");
        assert_eq!(opened["scope"], scope.label());
        assert_eq!(recv_json(&mut client).await["type"], "history");
        client
    }

    async fn next_command(fx: &mut Fixture) -> HarnessCommand {
        tokio::time::timeout(Duration::from_secs(5), fx.commands.recv())
            .await
            .expect("a command in time")
            .expect("the fake process is alive")
    }

    /// The next relayed harness event on the socket.
    async fn next_event(client: &mut warp::test::WsClient) -> Value {
        let frame = recv_json(client).await;
        assert_eq!(frame["type"], "event", "{frame}");
        frame["event"]["event"].clone()
    }

    async fn recv_json(client: &mut warp::test::WsClient) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(5), client.recv())
            .await
            .expect("a socket message in time")
            .expect("an open socket");
        serde_json::from_str(message.to_str().expect("a text message")).unwrap()
    }

    #[tokio::test]
    async fn pairing_and_tokens_over_http() {
        let fx = fixture();
        let routes = routes(fx.state.clone());
        for page in ["/", "/pair", "/chat"] {
            let response = warp::test::request().path(page).reply(&routes).await;
            assert_eq!(response.status(), 200, "{page}");
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
        let response = warp::test::request()
            .path("/api/chats")
            .reply(&routes)
            .await;
        assert_eq!(response.status(), 401);

        let pairing = fx.phones.create_pairing("phone", PhoneScope::View).unwrap();
        let pair = |code: &str| {
            warp::test::request()
                .method("POST")
                .path("/api/pair")
                .json(&json!({ "code": code }))
        };
        let response = pair(&pairing.code).reply(&routes).await;
        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(body["phone"]["scope"], "view");
        let token = body["token"].as_str().unwrap().to_string();
        assert!(fx.phones.authenticate(&token).is_some());
        assert_eq!(pair(&pairing.code).reply(&routes).await.status(), 401);

        let history = |bearer: &str| {
            warp::test::request()
                .path("/api/history?session=no-such-session-0000")
                .header("authorization", format!("Bearer {bearer}"))
        };
        assert_eq!(history(&token).reply(&routes).await.status(), 404);
        assert_eq!(history("wrong").reply(&routes).await.status(), 401);
        let bad_id = warp::test::request()
            .path("/api/history?session=..%2Fetc")
            .header("authorization", format!("Bearer {token}"))
            .reply(&routes)
            .await;
        assert_eq!(bad_id.status(), 400);
    }

    #[tokio::test]
    async fn a_viewing_socket_gets_events_and_cannot_send() {
        let mut fx = fixture();
        let (token, phone_id) = paired_token(&fx.phones, PhoneScope::View);
        fx.events
            .send(HarnessEvent::TextDelta("before".into()))
            .unwrap();
        let mut client = warp::test::ws()
            .path("/api/ws")
            .handshake(routes(fx.state.clone()))
            .await
            .unwrap();
        client
            .send_text(
                json!({"type": "hello", "token": token, "chat_id": fx.chat_id, "after_seq": null})
                    .to_string(),
            )
            .await;
        let opened = recv_json(&mut client).await;
        assert_eq!(opened["type"], "opened");
        assert_eq!(opened["scope"], "view");
        assert_eq!(recv_json(&mut client).await["type"], "history");

        let head = fx.chats.list()[0].attached_clients;
        assert_eq!(head, 2, "the phone attaches like a desktop client");
        fx.events
            .send(HarnessEvent::TextDelta("live".into()))
            .unwrap();
        let live = recv_json(&mut client).await;
        assert_eq!(live["type"], "event");
        assert_eq!(live["event"]["kind"], "harness");
        assert_eq!(live["event"]["event"]["data"], "live");
        let first_seq = live["seq"].as_u64().unwrap();
        assert!(first_seq >= 1);

        // Anything the phone sends is refused and never reaches Claude,
        // the act commands included.
        for message in [
            json!({"type": "interrupt"}).to_string(),
            json!({"type": "send", "id": uuid::Uuid::new_v4().to_string(), "text": "hi"})
                .to_string(),
            json!({"type": "command", "command": {"type": "interrupt"}}).to_string(),
            "not even json".to_string(),
        ] {
            client.send_text(message).await;
            assert_eq!(recv_json(&mut client).await["type"], "refused");
        }
        assert!(fx.commands.try_recv().is_err());

        // Revoking closes the socket.
        assert_eq!(fx.phones.revoke(&phone_id), Ok(true));
        assert_eq!(recv_json(&mut client).await["type"], "revoked");
        let closed = tokio::time::timeout(Duration::from_secs(5), client.recv())
            .await
            .unwrap();
        // A close frame, or the socket already gone.
        assert!(match closed {
            Ok(message) => message.is_close(),
            Err(_) => true,
        });
    }

    #[tokio::test]
    async fn an_acting_socket_sends_answers_and_interrupts() {
        let mut fx = fixture();
        let mut client = open_socket(&fx, PhoneScope::Act).await;

        // A message reaches Claude with the page's id and no images, and
        // comes back to the phone as the chat's echo.
        let id = uuid::Uuid::new_v4().to_string();
        client
            .send_text(json!({"type": "send", "id": id, "text": "run the tests"}).to_string())
            .await;
        assert_eq!(
            next_command(&mut fx).await,
            HarnessCommand::SendUserMessage(UserPrompt {
                text: "run the tests".into(),
                images: Vec::new(),
                id: Some(id.clone()),
            })
        );
        let echo = next_event(&mut client).await;
        assert_eq!(echo["type"], "user_message_echo");
        assert_eq!(echo["data"]["id"], id.as_str());
        assert_eq!(echo["data"]["text"], "run the tests");
        assert_eq!(echo["data"]["image_count"], 0);

        // An answer, in the chat page's decision shape, to a prompt that
        // is waiting.
        fx.events
            .send(crate::harness::HarnessEvent::RuntimeRequest {
                request_id: "r1".into(),
                tool_use_id: "t1".into(),
                kind: crate::harness::RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: json!({"command": "cargo test"}),
                    suggestions: Vec::new(),
                    decision_reason: None,
                },
            })
            .unwrap();
        assert_eq!(next_event(&mut client).await["type"], "runtime_request");
        client
            .send_text(
                json!({"type": "answer", "request_id": "r1", "decision": {"kind": "allow"}})
                    .to_string(),
            )
            .await;
        assert_eq!(
            next_command(&mut fx).await,
            HarnessCommand::Answer {
                request_id: "r1".into(),
                decision: RuntimeDecision::Allow {
                    updated_input: None,
                    remember: None,
                },
            }
        );
        let deny = json!({"type": "answer", "request_id": "r1",
            "decision": {"kind": "deny", "message": "not now"}});
        client.send_text(deny.to_string()).await;
        assert_eq!(
            next_command(&mut fx).await,
            HarnessCommand::Answer {
                request_id: "r1".into(),
                decision: RuntimeDecision::Deny {
                    message: "not now".into()
                },
            }
        );
        // Once resolved (by any client), a late answer goes no further.
        fx.events
            .send(crate::harness::HarnessEvent::RuntimeRequestResolved {
                request_id: "r1".into(),
            })
            .unwrap();
        assert_eq!(
            next_event(&mut client).await["type"],
            "runtime_request_resolved"
        );
        client.send_text(deny.to_string()).await;
        let late = recv_json(&mut client).await;
        assert_eq!(late["type"], "refused");
        assert_eq!(late["request_id"], "r1");

        client
            .send_text(json!({"type": "interrupt"}).to_string())
            .await;
        assert_eq!(next_command(&mut fx).await, HarnessCommand::Interrupt);
        assert!(fx.commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn malformed_oversized_or_forbidden_commands_never_reach_claude() {
        let mut fx = fixture();
        let mut client = open_socket(&fx, PhoneScope::Act).await;
        let id = uuid::Uuid::new_v4().to_string();
        let cases = [
            ("not json".to_string(), "invalid"),
            (json!({"text": "no type"}).to_string(), "invalid"),
            (
                json!({"type": "send", "text": "no id"}).to_string(),
                "invalid",
            ),
            (json!({"type": "send", "id": id}).to_string(), "invalid"),
            (
                json!({"type": "send", "id": "not-a-uuid", "text": "hi"}).to_string(),
                "invalid",
            ),
            (
                json!({"type": "send", "id": id, "text": "  \n "}).to_string(),
                "invalid",
            ),
            (
                json!({"type": "send", "id": id, "text": "x".repeat(MAX_PHONE_PROMPT_BYTES + 1)})
                    .to_string(),
                "invalid",
            ),
            (
                json!({"type": "answer", "request_id": "r1", "decision": {"kind": "maybe"}})
                    .to_string(),
                "invalid",
            ),
            (
                json!({"type": "answer", "request_id": "nobody-asked",
                    "decision": {"kind": "allow"}})
                .to_string(),
                "refused",
            ),
            // Never from a phone: stopping the chat, its model or mode,
            // withdrawing, or a raw harness command.
            (json!({"type": "shutdown"}).to_string(), "refused"),
            (
                json!({"type": "set_model", "data": "opus"}).to_string(),
                "refused",
            ),
            (
                json!({"type": "set_permission_mode", "data": "bypassPermissions"}).to_string(),
                "refused",
            ),
            (
                json!({"type": "send_user_message", "data": {"text": "hi", "images": []}})
                    .to_string(),
                "refused",
            ),
        ];
        for (message, expected) in cases {
            client.send_text(message.clone()).await;
            let reply = recv_json(&mut client).await;
            assert_eq!(reply["type"], expected, "{message:.80}: {reply}");
            assert!(reply["message"].as_str().is_some_and(|m| !m.is_empty()));
        }
        // The oversized message is named by its id, so the page can mark
        // it unsent.
        client
            .send_text(
                json!({"type": "send", "id": id, "text": "x".repeat(MAX_PHONE_PROMPT_BYTES + 1)})
                    .to_string(),
            )
            .await;
        assert_eq!(recv_json(&mut client).await["id"], id.as_str());
        client.send(Message::binary(b"{}".to_vec())).await;
        assert_eq!(recv_json(&mut client).await["type"], "invalid");
        assert!(fx.commands.try_recv().is_err());

        // A message past the socket's cap ends the socket instead.
        client
            .send_text("x".repeat(MAX_SOCKET_MESSAGE_BYTES + 1))
            .await;
        let closed = tokio::time::timeout(Duration::from_secs(5), client.recv())
            .await
            .unwrap();
        assert!(match closed {
            Ok(message) => message.is_close(),
            Err(_) => true,
        });
        assert!(fx.commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_socket_needs_a_valid_token_and_a_running_chat() {
        let fx = fixture();
        let (token, _) = paired_token(&fx.phones, PhoneScope::View);
        let hello = |token: &str, chat: &str| {
            json!({"type": "hello", "token": token, "chat_id": chat}).to_string()
        };

        let mut client = warp::test::ws()
            .path("/api/ws")
            .handshake(routes(fx.state.clone()))
            .await
            .unwrap();
        client.send_text(hello("wrong", &fx.chat_id)).await;
        assert_eq!(recv_json(&mut client).await["type"], "unauthorized");

        let mut client = warp::test::ws()
            .path("/api/ws")
            .handshake(routes(fx.state.clone()))
            .await
            .unwrap();
        client
            .send_text(hello(&token, "00000000-0000-0000-0000-000000000000"))
            .await;
        assert_eq!(recv_json(&mut client).await["type"], "not_running");
    }

    #[test]
    fn the_phone_chat_page_wraps_the_desktop_page() {
        let html = phone_chat_html().unwrap();
        assert!(html.contains("window.__appendEvent = function (event)"));
        assert!(html.contains(r#"name="viewport""#));
        // The shim comes after the page script that defines __appendEvent.
        let page_script = html.find("window.__appendEvent = function").unwrap();
        let shim = html.find("api/ws").unwrap();
        assert!(shim > page_script);
        assert!(html.trim_end().ends_with("</html>"));
    }

    #[test]
    fn event_frames_are_json_the_page_reads() {
        let frame = event_frame(
            &SeqEvent {
                seq: 7,
                json: r#"{"type":"text_delta","data":"hi"}"#.into(),
                exited: false,
            },
            true,
        );
        let value: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["type"], "event");
        assert_eq!(value["seq"], 7);
        assert_eq!(value["redelivered"], true);
        assert_eq!(value["event"]["kind"], "harness");
        assert_eq!(value["event"]["event"]["data"], "hi");
    }

    #[test]
    fn session_ids_are_plain_names() {
        assert!(valid_session_id("0b9f-abc_1"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../x"));
        assert!(!valid_session_id("a/b"));
    }
}
