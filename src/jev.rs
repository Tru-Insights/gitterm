//! TypeSafe System One ("Jev") client, and ranking of the Chats index by
//! relevance to what the user is doing (TRU-141).
//!
//! Jev is a decision model, not a chat model: it answers typed questions
//! about a state with probabilities. One plain HTTPS POST per call:
//!
//! ```text
//! POST https://api.typesafe.ai/v1/systemone
//! Authorization: Bearer $TYPESAFE_API_KEY
//! { state, model: "jev-latest", questions: { id: { type, instructions, criteria? } } }
//! -> { model, answers: { id: { type, choice?, probabilities?, confidence?,
//!      noul?, score? } }, usage: { input_tokens, output_tokens } }
//! ```
//!
//! The wire shapes, the 255-option limit, the 429 / 529 backoff and the
//! "key refused / unreachable" split mirror the working client in
//! cree8-video-editor (`lib/server/typesafe.ts`) and
//! <https://docs.typesafe.ai/api.md>.
//!
//! Nothing here runs on its own: no telemetry, no background calls. The
//! network is touched only when a caller invokes [`JevClient::decide`] or
//! [`rank_chats`]. The API key is read only from the environment, held in
//! memory, sent only as the Authorization header (marked sensitive), and
//! never logged, printed or written anywhere.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls;

use crate::chats::ChatPreviewMessage;

/// The System One endpoint.
pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
/// The model alias the reference client sends.
pub const MODEL: &str = "jev-latest";
/// The only place the key is read from.
pub const API_KEY_ENV: &str = "TYPESAFE_API_KEY";
/// TypeSafe's limit on a choice question's options.
pub const MAX_CHOICES: usize = 255;

/// Attempts on 429 / 529 before giving up (reference: MAX_ATTEMPTS).
const MAX_ATTEMPTS: u32 = 5;
/// First backoff; doubles per attempt (reference: BACKOFF_MS).
const BACKOFF_MS: u64 = 500;
/// Upper bound on a server-sent Retry-After, so a ranking the user is
/// waiting on can never stall for minutes.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per HTTP attempt: connect + TLS + request + response body.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Characters of an error response body kept for the error.
const BODY_EXCERPT_CHARS: usize = 300;

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug, Clone, PartialEq)]
pub enum JevError {
    /// TYPESAFE_API_KEY is not set (or empty).
    NoKey,
    /// TypeSafe answered 401 / 403: (status, body excerpt).
    KeyRefused(u16, String),
    /// DNS, connect, TLS or I/O failure, or a timeout.
    Unreachable(String),
    /// Still 429 / 529 after every retry: the last status.
    RateLimited(u16),
    /// A request we refuse to send, or a 2xx answer that does not have the
    /// documented shape.
    Protocol(String),
    /// Any other non-2xx: (status, body excerpt).
    Http(u16, String),
}

impl JevError {
    /// TypeSafe as a whole is unusable (no key, key refused, network down),
    /// as opposed to one call's own problem. Mirrors `typesafeDown()`: a
    /// caller ranking in batches stops instead of trying the next batch.
    pub fn is_down(&self) -> bool {
        matches!(
            self,
            JevError::NoKey | JevError::KeyRefused(..) | JevError::Unreachable(_)
        )
    }
}

impl fmt::Display for JevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JevError::NoKey => write!(f, "TypeSafe: {API_KEY_ENV} is not set"),
            JevError::KeyRefused(status, body) => {
                write!(f, "TypeSafe refused the key: HTTP {status}: {body}")
            }
            JevError::Unreachable(why) => write!(f, "TypeSafe is unreachable: {why}"),
            JevError::RateLimited(status) => write!(
                f,
                "TypeSafe is busy: HTTP {status} after {MAX_ATTEMPTS} attempts"
            ),
            JevError::Protocol(why) => write!(f, "TypeSafe protocol error: {why}"),
            JevError::Http(status, body) => write!(f, "TypeSafe HTTP {status}: {body}"),
        }
    }
}

impl std::error::Error for JevError {}

// ---------------------------------------------------------------------------
// Questions and the request body

#[derive(Debug, Clone, PartialEq)]
pub enum QuestionKind {
    /// Pick one option. `criteria`: option id -> what it means, sent in
    /// this order.
    Choice { criteria: Vec<(String, String)> },
    /// Yes / no as a probability of yes.
    Noul,
    /// A position on an ordered rubric, lowest anchor first.
    Score { criteria: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// The key under `questions` (and under `answers` in the response).
    pub id: String,
    pub instructions: String,
    pub kind: QuestionKind,
}

/// A JSON object whose keys keep the given order.
struct OrderedCriteria<'a>(&'a [(String, String)]);

impl Serialize for OrderedCriteria<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (id, meaning) in self.0 {
            map.serialize_entry(id, meaning)?;
        }
        map.end()
    }
}

/// One question in its wire form: `{type, instructions, criteria?}`.
struct WireQuestion<'a>(&'a Question);

impl Serialize for WireQuestion<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let q = self.0;
        let mut map = s.serialize_map(None)?;
        match &q.kind {
            QuestionKind::Choice { criteria } => {
                map.serialize_entry("type", "choice")?;
                map.serialize_entry("instructions", &q.instructions)?;
                map.serialize_entry("criteria", &OrderedCriteria(criteria))?;
            }
            QuestionKind::Noul => {
                map.serialize_entry("type", "noul")?;
                map.serialize_entry("instructions", &q.instructions)?;
            }
            QuestionKind::Score { criteria } => {
                map.serialize_entry("type", "score")?;
                map.serialize_entry("instructions", &q.instructions)?;
                map.serialize_entry("criteria", criteria)?;
            }
        }
        map.end()
    }
}

struct WireQuestions<'a>(&'a [Question]);

impl Serialize for WireQuestions<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for q in self.0 {
            map.serialize_entry(&q.id, &WireQuestion(q))?;
        }
        map.end()
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    state: &'a str,
    model: &'a str,
    questions: WireQuestions<'a>,
}

/// The JSON body for one call. Refuses (before any network) what TypeSafe
/// would reject: no questions, duplicate ids, an empty choice, more than
/// [`MAX_CHOICES`] options.
pub fn request_body(state: &str, questions: &[Question]) -> Result<Vec<u8>, JevError> {
    if questions.is_empty() {
        return Err(JevError::Protocol("no questions to ask".to_string()));
    }
    let mut seen = HashSet::new();
    for q in questions {
        if !seen.insert(q.id.as_str()) {
            return Err(JevError::Protocol(format!(
                "duplicate question id {}",
                q.id
            )));
        }
        if let QuestionKind::Choice { criteria } = &q.kind {
            if criteria.is_empty() {
                return Err(JevError::Protocol(format!("{} has no options", q.id)));
            }
            if criteria.len() > MAX_CHOICES {
                return Err(JevError::Protocol(format!(
                    "{} has {} options (TypeSafe takes {MAX_CHOICES})",
                    q.id,
                    criteria.len()
                )));
            }
        }
    }
    serde_json::to_vec(&WireRequest {
        state,
        model: MODEL,
        questions: WireQuestions(questions),
    })
    .map_err(|e| JevError::Protocol(format!("serialize request: {e}")))
}

// ---------------------------------------------------------------------------
// Answers

#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    /// The highest-probability option.
    pub choice: String,
    /// Option id -> probability.
    pub probabilities: HashMap<String, f64>,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Choice(ChoiceAnswer),
    /// Probability of yes.
    Noul(f64),
    /// Expected anchor index, 0 .. criteria.len() - 1.
    Score(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answers {
    /// The model that actually ran (e.g. "jev-1.13.0").
    pub model: String,
    pub answers: HashMap<String, Answer>,
    pub usage: Usage,
}

impl Answers {
    pub fn choice(&self, id: &str) -> Result<&ChoiceAnswer, JevError> {
        match self.answers.get(id) {
            Some(Answer::Choice(a)) => Ok(a),
            _ => Err(JevError::Protocol(format!("{id}: not a choice answer"))),
        }
    }

    pub fn noul(&self, id: &str) -> Result<f64, JevError> {
        match self.answers.get(id) {
            Some(Answer::Noul(p)) => Ok(*p),
            _ => Err(JevError::Protocol(format!("{id}: not a noul answer"))),
        }
    }

    pub fn score(&self, id: &str) -> Result<f64, JevError> {
        match self.answers.get(id) {
            Some(Answer::Score(s)) => Ok(*s),
            _ => Err(JevError::Protocol(format!("{id}: not a score answer"))),
        }
    }
}

fn protocol(what: &str, why: &str) -> JevError {
    JevError::Protocol(format!("{what}: {why}"))
}

fn num(v: Option<&Value>, what: &str) -> Result<f64, JevError> {
    match v.and_then(Value::as_f64) {
        Some(n) if n.is_finite() => Ok(n),
        _ => Err(protocol(what, "not a number")),
    }
}

fn count(v: Option<&Value>, what: &str) -> Result<u64, JevError> {
    let n = num(v, what)?;
    if n < 0.0 || n.fract() != 0.0 {
        return Err(protocol(what, "not a token count"));
    }
    Ok(n as u64)
}

/// Narrow one answer; the error names `what` (the question id).
pub fn parse_answer(v: &Value, what: &str) -> Result<Answer, JevError> {
    let Some(obj) = v.as_object() else {
        return Err(protocol(what, "not an object"));
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("choice") => {
            let Some(choice) = obj.get("choice").and_then(Value::as_str) else {
                return Err(protocol(what, "choice"));
            };
            let Some(raw) = obj.get("probabilities").and_then(Value::as_object) else {
                return Err(protocol(what, "probabilities not an object"));
            };
            let mut probabilities = HashMap::with_capacity(raw.len());
            for (k, p) in raw {
                probabilities.insert(
                    k.clone(),
                    num(Some(p), &format!("{what}: probabilities.{k}"))?,
                );
            }
            Ok(Answer::Choice(ChoiceAnswer {
                choice: choice.to_string(),
                probabilities,
                confidence: num(obj.get("confidence"), &format!("{what}: confidence"))?,
            }))
        }
        Some("noul") => Ok(Answer::Noul(num(
            obj.get("noul"),
            &format!("{what}: noul"),
        )?)),
        Some("score") => Ok(Answer::Score(num(
            obj.get("score"),
            &format!("{what}: score"),
        )?)),
        _ => Err(protocol(
            what,
            &format!("unknown type {}", obj.get("type").unwrap_or(&Value::Null)),
        )),
    }
}

/// Narrow a whole response; every asked question must be answered.
pub fn parse_response(v: &Value, asked: &[&str]) -> Result<Answers, JevError> {
    let Some(obj) = v.as_object() else {
        return Err(JevError::Protocol("response not an object".to_string()));
    };
    let Some(model) = obj.get("model").and_then(Value::as_str) else {
        return Err(JevError::Protocol("model".to_string()));
    };
    let Some(raw) = obj.get("answers").and_then(Value::as_object) else {
        return Err(JevError::Protocol("answers not an object".to_string()));
    };
    let Some(usage) = obj.get("usage").and_then(Value::as_object) else {
        return Err(JevError::Protocol("usage not an object".to_string()));
    };
    let mut answers = HashMap::with_capacity(asked.len());
    for id in asked {
        let Some(a) = raw.get(*id) else {
            return Err(JevError::Protocol(format!("no answer for {id}")));
        };
        answers.insert(id.to_string(), parse_answer(a, id)?);
    }
    Ok(Answers {
        model: model.to_string(),
        answers,
        usage: Usage {
            input_tokens: count(usage.get("input_tokens"), "usage.input_tokens")?,
            output_tokens: count(usage.get("output_tokens"), "usage.output_tokens")?,
        },
    })
}

// ---------------------------------------------------------------------------
// Client

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

/// Parses an endpoint URL (tests point the client at a local server).
#[cfg(test)]
fn parse_endpoint(raw: &str) -> Result<Endpoint, JevError> {
    let url =
        url::Url::parse(raw).map_err(|e| JevError::Protocol(format!("endpoint {raw}: {e}")))?;
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        other => {
            return Err(JevError::Protocol(format!(
                "endpoint {raw}: scheme {other}"
            )));
        }
    };
    let Some(host) = url.host_str() else {
        return Err(JevError::Protocol(format!("endpoint {raw}: no host")));
    };
    let Some(port) = url.port_or_known_default() else {
        return Err(JevError::Protocol(format!("endpoint {raw}: no port")));
    };
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    Ok(Endpoint {
        https,
        host: host.to_string(),
        port,
        path,
    })
}

/// One HTTP exchange's outcome, before status handling.
struct RawResponse {
    status: u16,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

/// Seconds from a Retry-After header (fractions allowed), capped.
fn retry_after(headers: &hyper::HeaderMap) -> Option<Duration> {
    let secs: f64 = headers
        .get(hyper::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs).min(MAX_RETRY_AFTER))
}

fn excerpt(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .take(BODY_EXCERPT_CHARS)
        .collect()
}

#[derive(Clone)]
pub struct JevClient {
    key: String,
    endpoint: Endpoint,
    connect_timeout: Duration,
    total_timeout: Duration,
    /// Built on the first HTTPS call (loads the OS root store once per
    /// client); an error is kept so every later call reports it.
    tls: Arc<OnceLock<Result<Arc<rustls::ClientConfig>, String>>>,
}

impl fmt::Debug for JevClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JevClient")
            .field("key", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl JevClient {
    /// A client when TYPESAFE_API_KEY is set and non-empty; None otherwise.
    /// The key is read from the environment only, never from a file.
    pub fn from_env() -> Option<JevClient> {
        let key = std::env::var(API_KEY_ENV).ok()?;
        if key.trim().is_empty() {
            return None;
        }
        Some(JevClient::with_endpoint_parsed(
            key,
            Endpoint {
                https: true,
                host: "api.typesafe.ai".to_string(),
                port: 443,
                path: "/v1/systemone".to_string(),
            },
        ))
    }

    fn with_endpoint_parsed(key: String, endpoint: Endpoint) -> JevClient {
        JevClient {
            key,
            endpoint,
            connect_timeout: CONNECT_TIMEOUT,
            total_timeout: TOTAL_TIMEOUT,
            tls: Arc::new(OnceLock::new()),
        }
    }

    /// A client for another endpoint (the tests' local fake server).
    #[cfg(test)]
    fn with_endpoint(key: &str, endpoint: &str) -> Result<JevClient, JevError> {
        Ok(JevClient::with_endpoint_parsed(
            key.to_string(),
            parse_endpoint(endpoint)?,
        ))
    }

    /// One System One call: `state` and `questions` in, every question's
    /// answer out. 429 / 529 are retried with backoff (Retry-After when
    /// sent, else 0.5 s doubling), up to 5 attempts.
    pub async fn decide(&self, state: &str, questions: &[Question]) -> Result<Answers, JevError> {
        if self.key.trim().is_empty() {
            return Err(JevError::NoKey);
        }
        let body = request_body(state, questions)?;
        let asked: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
        let mut attempt = 0u32;
        loop {
            let res = self.post_once(body.clone()).await?;
            if (200..300).contains(&res.status) {
                let v: Value = serde_json::from_slice(&res.body).map_err(|e| {
                    JevError::Protocol(format!(
                        "response is not JSON ({e}): {}",
                        excerpt(&res.body)
                    ))
                })?;
                return parse_response(&v, &asked);
            }
            let busy = res.status == 429 || res.status == 529;
            if busy && attempt + 1 < MAX_ATTEMPTS {
                let wait = res
                    .retry_after
                    .unwrap_or_else(|| Duration::from_millis(BACKOFF_MS << attempt));
                eprintln!(
                    "[jev] HTTP {}, retry {} in {} ms",
                    res.status,
                    attempt + 1,
                    wait.as_millis()
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                continue;
            }
            return Err(match res.status {
                401 | 403 => JevError::KeyRefused(res.status, excerpt(&res.body)),
                429 | 529 => JevError::RateLimited(res.status),
                status => JevError::Http(status, excerpt(&res.body)),
            });
        }
    }

    fn tls_config(&self) -> Result<Arc<rustls::ClientConfig>, JevError> {
        self.tls
            .get_or_init(build_tls_config)
            .clone()
            .map_err(JevError::Unreachable)
    }

    /// One POST: connect (5 s), then TLS + request + body, all within the
    /// total timeout (15 s).
    async fn post_once(&self, body: Vec<u8>) -> Result<RawResponse, JevError> {
        let ep = &self.endpoint;
        let total = self.total_timeout;
        match tokio::time::timeout(total, self.post_inner(body)).await {
            Ok(r) => r,
            Err(_) => Err(JevError::Unreachable(format!(
                "{}:{} did not answer within {} s",
                ep.host,
                ep.port,
                total.as_secs_f64()
            ))),
        }
    }

    async fn post_inner(&self, body: Vec<u8>) -> Result<RawResponse, JevError> {
        let ep = &self.endpoint;
        let target = format!("{}:{}", ep.host, ep.port);
        let tcp = match tokio::time::timeout(
            self.connect_timeout,
            TcpStream::connect((ep.host.as_str(), ep.port)),
        )
        .await
        {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(e)) => return Err(JevError::Unreachable(format!("connect {target}: {e}"))),
            Err(_) => {
                return Err(JevError::Unreachable(format!(
                    "connect {target}: timed out after {} s",
                    self.connect_timeout.as_secs_f64()
                )))
            }
        };
        tcp.set_nodelay(true)
            .map_err(|e| JevError::Unreachable(format!("{target}: set_nodelay: {e}")))?;

        let mut auth = hyper::header::HeaderValue::from_str(&format!("Bearer {}", self.key))
            .map_err(|_| {
                JevError::Protocol(format!("{API_KEY_ENV} is not a valid header value"))
            })?;
        auth.set_sensitive(true);
        let host_header = if (ep.https && ep.port == 443) || (!ep.https && ep.port == 80) {
            ep.host.clone()
        } else {
            target.clone()
        };
        let req = hyper::Request::post(ep.path.as_str())
            .header(hyper::header::HOST, host_header)
            .header(hyper::header::AUTHORIZATION, auth)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(hyper::header::CONTENT_LENGTH, body.len())
            .header(hyper::header::CONNECTION, "close")
            .body(hyper::Body::from(body))
            .map_err(|e| JevError::Protocol(format!("build request: {e}")))?;

        if ep.https {
            let config = self.tls_config()?;
            let name = rustls::pki_types::ServerName::try_from(ep.host.clone())
                .map_err(|e| JevError::Protocol(format!("server name {}: {e}", ep.host)))?;
            let connector = tokio_rustls::TlsConnector::from(config);
            let tls = match tokio::time::timeout(self.connect_timeout, connector.connect(name, tcp))
                .await
            {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => return Err(JevError::Unreachable(format!("TLS {target}: {e}"))),
                Err(_) => {
                    return Err(JevError::Unreachable(format!(
                        "TLS {target}: timed out after {} s",
                        self.connect_timeout.as_secs_f64()
                    )))
                }
            };
            exchange(tls, req, &target).await
        } else {
            exchange(tcp, req, &target).await
        }
    }
}

/// rustls on ring (the provider tonic already compiles) with the OS root
/// store. An empty root store is an error, not a silent fallback.
fn build_tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(format!(
            "no usable root certificates in the OS store ({} load errors{})",
            loaded.errors.len(),
            loaded
                .errors
                .first()
                .map(|e| format!(", first: {e}"))
                .unwrap_or_default()
        ));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS config: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// HTTP/1.1 over an established stream: send `req`, read the whole body.
async fn exchange<S>(
    io: S,
    req: hyper::Request<hyper::Body>,
    target: &str,
) -> Result<RawResponse, JevError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::handshake(io)
        .await
        .map_err(|e| JevError::Unreachable(format!("{target}: HTTP handshake: {e}")))?;
    // The connection driver's own failure surfaces through send_request /
    // the body read below, which is where it is reported.
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| JevError::Unreachable(format!("{target}: send: {e}")))?;
    let status = resp.status().as_u16();
    let retry_after = retry_after(resp.headers());
    let body = hyper::body::to_bytes(resp.into_body())
        .await
        .map_err(|e| JevError::Unreachable(format!("{target}: read body: {e}")))?;
    Ok(RawResponse {
        status,
        retry_after,
        body: body.to_vec(),
    })
}

// ---------------------------------------------------------------------------
// Ranking the Chats index

/// The question id of a ranking call.
pub const RANK_QUESTION_ID: &str = "relevant";
/// The extra option meaning "none of these conversations is about this".
pub const NONE_OPTION: &str = "none";
/// Below this confidence the caller keeps recency order. The entities
/// study (cree8-video-editor docs/studies/jev-vs-opus-entities.md) found
/// Jev's confidence calibrated: >= 0.8 agreed with Opus 79 %, < 0.5 was a
/// coin toss.
pub const DEFAULT_CONFIDENCE_GATE: f64 = 0.5;
/// Chats per call: the option limit less the "none" option.
pub const CHATS_PER_CALL: usize = MAX_CHOICES - 1;
/// Cap on one option's text.
///
/// Budget: a full call is 255 options x 300 chars, about 76 k chars or
/// ~19 k input tokens (at ~4 chars/token), which at Jev's $0.042 per
/// million input tokens is under a tenth of a cent, and output is free.
/// 300 chars fits a title (90), a home-relative path, branch and age
/// (~80) and a ~130-char tail of the conversation: enough to tell topics
/// apart, while keeping the request small enough to answer quickly.
pub const OPTION_MAX_CHARS: usize = 300;
const TITLE_MAX_CHARS: usize = 90;
const SNIPPET_ASK_CHARS: usize = 90;
const SNIPPET_REPLY_CHARS: usize = 60;
const CONTEXT_FIELD_MAX_CHARS: usize = 600;

/// One conversation offered to the ranker.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatCandidate {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    /// Compact relative age ("5m", "3h", "2d"), as the Chats panel shows.
    pub age: String,
    /// A bounded tail of the conversation ([`snippet_from_preview`]).
    pub snippet: String,
}

/// What the user is doing now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RankContext {
    pub workspace_dir: PathBuf,
    pub branch: Option<String>,
    /// The Chats panel's search text, if any.
    pub query: Option<String>,
    /// The prompt the user is writing or last sent, if any.
    pub recent_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedChat {
    pub id: String,
    pub probability: f64,
    /// 1 = most relevant.
    pub rank: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranking {
    /// Every candidate, most probable first; ties keep the input order
    /// (callers pass candidates newest first).
    pub chats: Vec<RankedChat>,
    /// Jev's confidence in the call whose batch holds the top chat.
    pub confidence: f64,
    /// That call picked "none of these" over every chat.
    pub chose_none: bool,
    /// System One calls made.
    pub calls: usize,
    pub usage: Usage,
}

impl Ranking {
    /// Whether to show Jev's order rather than recency.
    pub fn is_confident(&self, gate: f64) -> bool {
        !self.chats.is_empty() && !self.chose_none && self.confidence >= gate
    }

    /// The ids to show: Jev's order when confident at `gate`, otherwise the
    /// candidates' own (recency) order.
    pub fn ordered_ids(&self, gate: f64, by_recency: &[ChatCandidate]) -> Vec<String> {
        if self.is_confident(gate) {
            self.chats.iter().map(|c| c.id.clone()).collect()
        } else {
            by_recency.iter().map(|c| c.id.clone()).collect()
        }
    }
}

/// Whitespace collapsed to single spaces.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` chars, ending in "…" when cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A bounded tail of a conversation for its option text: the last thing
/// the user asked and the start of the last reply. The head of the
/// conversation is the chat's title, which the label carries already.
pub fn snippet_from_preview(messages: &[ChatPreviewMessage]) -> String {
    let last_ask = messages.iter().rev().find(|m| m.is_user);
    let last_reply = messages.iter().rev().find(|m| !m.is_user);
    let mut parts = Vec::new();
    if let Some(m) = last_ask {
        parts.push(format!(
            "asked: {}",
            clip(&one_line(&m.text), SNIPPET_ASK_CHARS)
        ));
    }
    if let Some(m) = last_reply {
        parts.push(format!(
            "reply: {}",
            clip(&one_line(&m.text), SNIPPET_REPLY_CHARS)
        ));
    }
    parts.join(" / ")
}

/// `path` with the home directory shown as "~".
fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|h| path.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// The option id for the `i`-th chat of a batch. Positional ids keep the
/// request small and are safe whatever characters a chat id has.
pub fn option_id(i: usize) -> String {
    format!("c{i}")
}

/// One chat's option text, at most [`OPTION_MAX_CHARS`]:
/// `title | ~/dir @ branch | age | asked: … / reply: …`.
pub fn option_label(c: &ChatCandidate, home: Option<&Path>) -> String {
    let mut label = clip(&one_line(&c.title), TITLE_MAX_CHARS);
    label.push_str(" | ");
    label.push_str(&display_path(&c.cwd, home));
    if let Some(b) = c.branch.as_deref().filter(|b| !b.is_empty()) {
        label.push_str(" @ ");
        label.push_str(b);
    }
    label.push_str(" | ");
    if c.age == "now" {
        label.push_str("active now");
    } else {
        label.push_str(&format!("{} ago", c.age));
    }
    let snippet = one_line(&c.snippet);
    if !snippet.is_empty() {
        label.push_str(" | ");
        label.push_str(&snippet);
    }
    clip(&label, OPTION_MAX_CHARS)
}

/// The state text: what the user is doing now.
pub fn rank_state(ctx: &RankContext, home: Option<&Path>) -> String {
    let mut lines = vec![
        "A developer in a terminal workspace wants to pick up an earlier AI coding conversation."
            .to_string(),
        format!("Workspace: {}", display_path(&ctx.workspace_dir, home)),
    ];
    if let Some(b) = ctx.branch.as_deref().filter(|b| !b.is_empty()) {
        lines.push(format!("Git branch: {b}"));
    }
    if let Some(q) = ctx.query.as_deref().map(one_line).filter(|q| !q.is_empty()) {
        lines.push(format!(
            "Searching the conversations for: {}",
            clip(&q, CONTEXT_FIELD_MAX_CHARS)
        ));
    }
    if let Some(p) = ctx
        .recent_prompt
        .as_deref()
        .map(one_line)
        .filter(|p| !p.is_empty())
    {
        lines.push(format!(
            "What they are asking now: {}",
            clip(&p, CONTEXT_FIELD_MAX_CHARS)
        ));
    }
    lines.join("\n")
}

const RANK_INSTRUCTIONS: &str = "Which earlier conversation is most relevant to what the developer is doing now (the workspace, branch, search text and current request in the state)? Prefer a conversation about the same task or topic; the same directory or branch alone is weaker evidence than the same subject. Choose \"none\" if no conversation is about this work.";

/// The one choice question for a batch of chats (plus "none").
pub fn rank_question(batch: &[ChatCandidate], home: Option<&Path>) -> Question {
    let mut criteria: Vec<(String, String)> = batch
        .iter()
        .enumerate()
        .map(|(i, c)| (option_id(i), option_label(c, home)))
        .collect();
    criteria.push((
        NONE_OPTION.to_string(),
        "None of these conversations is about the current work".to_string(),
    ));
    Question {
        id: RANK_QUESTION_ID.to_string(),
        instructions: RANK_INSTRUCTIONS.to_string(),
        kind: QuestionKind::Choice { criteria },
    }
}

/// One batch's answer, narrowed: each chat's probability in batch order.
/// An option missing from `probabilities` counts as 0 (TypeSafe's
/// probabilities sum to 1 over the options it reports); an option that
/// was not offered is a protocol error.
fn batch_probabilities(a: &ChoiceAnswer, batch_len: usize) -> Result<Vec<f64>, JevError> {
    let offered = |id: &str| {
        id == NONE_OPTION
            || id
                .strip_prefix('c')
                .and_then(|n| n.parse::<usize>().ok())
                .is_some_and(|n| n < batch_len && option_id(n) == id)
    };
    if !offered(&a.choice) {
        return Err(JevError::Protocol(format!(
            "{RANK_QUESTION_ID}: choice {:?} was not offered",
            a.choice
        )));
    }
    if let Some(bad) = a.probabilities.keys().find(|k| !offered(k)) {
        return Err(JevError::Protocol(format!(
            "{RANK_QUESTION_ID}: probability for {bad:?}, which was not offered"
        )));
    }
    Ok((0..batch_len)
        .map(|i| a.probabilities.get(&option_id(i)).copied().unwrap_or(0.0))
        .collect())
}

/// Rank `candidates` (newest first) by relevance to `ctx`: one choice
/// question per call, [`CHATS_PER_CALL`] chats per call, calls made one
/// after another, results merged by probability. The caller gates on
/// [`Ranking::is_confident`] and keeps recency order below the gate.
pub async fn rank_chats(
    client: &JevClient,
    ctx: &RankContext,
    candidates: &[ChatCandidate],
) -> Result<Ranking, JevError> {
    let home = dirs::home_dir();
    let home = home.as_deref();
    let state = rank_state(ctx, home);
    let mut scored: Vec<(usize, f64)> = Vec::with_capacity(candidates.len());
    let mut batches: Vec<(f64, bool)> = Vec::new();
    let mut usage = Usage::default();
    for (b, batch) in candidates.chunks(CHATS_PER_CALL).enumerate() {
        let question = rank_question(batch, home);
        let answers = client
            .decide(&state, std::slice::from_ref(&question))
            .await?;
        let a = answers.choice(RANK_QUESTION_ID)?;
        let probs = batch_probabilities(a, batch.len())?;
        let base = b * CHATS_PER_CALL;
        scored.extend(probs.into_iter().enumerate().map(|(i, p)| (base + i, p)));
        batches.push((a.confidence, a.choice == NONE_OPTION));
        usage.input_tokens += answers.usage.input_tokens;
        usage.output_tokens += answers.usage.output_tokens;
    }
    Ok(merge_ranking(candidates, scored, &batches, usage))
}

/// Merge per-batch scores: probability descending, input order on ties;
/// confidence and "none" come from the batch holding the top chat.
fn merge_ranking(
    candidates: &[ChatCandidate],
    mut scored: Vec<(usize, f64)>,
    batches: &[(f64, bool)],
    usage: Usage,
) -> Ranking {
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let (confidence, chose_none) = scored
        .first()
        .and_then(|(i, _)| batches.get(i / CHATS_PER_CALL).copied())
        .unwrap_or((0.0, false));
    Ranking {
        chats: scored
            .into_iter()
            .enumerate()
            .map(|(r, (i, p))| RankedChat {
                id: candidates[i].id.clone(),
                probability: p,
                rank: r + 1,
            })
            .collect(),
        confidence,
        chose_none,
        calls: batches.len(),
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use warp::Filter;

    const RECORDED: &str = include_str!("../tests/fixtures/jev/recorded_choice_noul.json");
    const RANK_BATCH_1: &str = include_str!("../tests/fixtures/jev/rank_batch_1.json");
    const RANK_BATCH_2: &str = include_str!("../tests/fixtures/jev/rank_batch_2.json");
    const RANK_REQUEST: &str = include_str!("../tests/fixtures/jev/rank_request.json");

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn reference_questions() -> Vec<Question> {
        vec![
            Question {
                id: "car".into(),
                instructions: "Which car?".into(),
                kind: QuestionKind::Choice {
                    criteria: vec![
                        ("a".into(), "#63".into()),
                        ("b".into(), "#7".into()),
                        ("none".into(), "nobody".into()),
                    ],
                },
            },
            Question {
                id: "onScreen".into(),
                instructions: "On screen?".into(),
                kind: QuestionKind::Noul,
            },
        ]
    }

    fn candidate(i: usize) -> ChatCandidate {
        ChatCandidate {
            id: format!("chat-{i}"),
            title: format!("Chat number {i}"),
            cwd: PathBuf::from("/Users/dev/GitRepo/gitterm-v5"),
            branch: Some("master".into()),
            age: format!("{i}m"),
            snippet: String::new(),
        }
    }

    // --- pure helpers -----------------------------------------------------

    #[test]
    fn default_endpoint_matches_the_constant() {
        let ep = parse_endpoint(ENDPOINT).unwrap();
        assert_eq!(
            ep,
            Endpoint {
                https: true,
                host: "api.typesafe.ai".into(),
                port: 443,
                path: "/v1/systemone".into(),
            }
        );
    }

    #[test]
    fn request_body_keeps_the_reference_shape_and_option_order() {
        let body = request_body("{}", &reference_questions()).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert_eq!(
            text,
            r##"{"state":"{}","model":"jev-latest","questions":{"car":{"type":"choice","instructions":"Which car?","criteria":{"a":"#63","b":"#7","none":"nobody"}},"onScreen":{"type":"noul","instructions":"On screen?"}}}"##
        );
    }

    #[test]
    fn request_body_refuses_more_than_255_options_before_any_call() {
        let criteria = (0..256)
            .map(|i| (format!("e{i}"), format!("#{i}")))
            .collect();
        let q = Question {
            id: "car".into(),
            instructions: "?".into(),
            kind: QuestionKind::Choice { criteria },
        };
        let err = request_body("{}", &[q]).unwrap_err();
        assert!(
            matches!(&err, JevError::Protocol(m) if m.contains("256 options")),
            "{err}"
        );
    }

    #[test]
    fn rank_question_matches_the_golden_request() {
        let batch: Vec<ChatCandidate> = vec![
            ChatCandidate {
                id: "1b0c".into(),
                title: "Wire the Chats panel search".into(),
                cwd: PathBuf::from("/home/dev/GitRepo/gitterm-v5"),
                branch: Some("tracey/tru-78-chats".into()),
                age: "2d".into(),
                snippet:
                    "asked: make search match branch names / reply: Done, matches_query now checks…"
                        .into(),
            },
            ChatCandidate {
                id: "77aa".into(),
                title: "Fix Windows CI".into(),
                cwd: PathBuf::from("/srv/ci"),
                branch: None,
                age: "now".into(),
                snippet: String::new(),
            },
        ];
        let ctx = RankContext {
            workspace_dir: PathBuf::from("/home/dev/GitRepo/gitterm-v5"),
            branch: Some("tracey/tru-141-jev".into()),
            query: Some("chats search".into()),
            recent_prompt: None,
        };
        let home = Some(Path::new("/home/dev"));
        let body = request_body(&rank_state(&ctx, home), &[rank_question(&batch, home)]).unwrap();
        let sent: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(sent, json(RANK_REQUEST));
    }

    #[test]
    fn option_label_is_capped_and_single_line() {
        let mut c = candidate(1);
        c.title = "word ".repeat(100);
        c.snippet = "line\n\n  ".repeat(200);
        let label = option_label(&c, None);
        assert_eq!(label.chars().count(), OPTION_MAX_CHARS);
        assert!(label.ends_with('…'));
        assert!(!label.contains('\n'));
        // The title is cut at its own budget so the path still shows.
        assert!(label.contains("| /Users/dev/GitRepo/gitterm-v5 @ master | 1m ago | "));
    }

    #[test]
    fn snippet_takes_the_last_ask_and_reply() {
        let m = |is_user: bool, t: &str| ChatPreviewMessage {
            is_user,
            text: t.into(),
        };
        let msgs = vec![
            m(true, "first ask"),
            m(false, "first reply"),
            m(true, "second\nask"),
            m(false, &"long reply ".repeat(20)),
        ];
        let s = snippet_from_preview(&msgs);
        assert!(
            s.starts_with("asked: second ask / reply: long reply"),
            "{s}"
        );
        assert!(
            s.chars().count()
                <= "asked: ".len() + SNIPPET_ASK_CHARS + " / reply: ".len() + SNIPPET_REPLY_CHARS
        );
        assert_eq!(snippet_from_preview(&[]), "");
    }

    #[test]
    fn display_path_uses_tilde_for_home() {
        let home = Some(Path::new("/Users/dev"));
        assert_eq!(display_path(Path::new("/Users/dev/x/y"), home), "~/x/y");
        assert_eq!(display_path(Path::new("/Users/dev"), home), "~");
        assert_eq!(display_path(Path::new("/srv/x"), home), "/srv/x");
    }

    // --- answer narrowing --------------------------------------------------

    #[test]
    fn parse_response_narrows_the_recorded_shape() {
        let r = parse_response(&json(RECORDED), &["car", "onScreen"]).unwrap();
        assert_eq!(r.model, "jev-1.13.0");
        let car = r.choice("car").unwrap();
        assert_eq!(car.choice, "a");
        assert_eq!(car.confidence, 0.92);
        assert_eq!(car.probabilities.get("a"), Some(&0.95));
        assert_eq!(car.probabilities.get("b"), Some(&0.0));
        assert_eq!(r.noul("onScreen").unwrap(), 0.82);
        assert!(r.noul("car").is_err());
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 385,
                output_tokens: 58
            }
        );
    }

    #[test]
    fn parse_response_rejects_missing_and_malformed_answers() {
        let err = parse_response(&json(RECORDED), &["car", "status"]).unwrap_err();
        assert_eq!(err, JevError::Protocol("no answer for status".into()));

        let mut v = json(RECORDED);
        v["answers"]["car"] = serde_json::json!({"type": "choice", "choice": "a"});
        let err = parse_response(&v, &["car"]).unwrap_err();
        assert_eq!(
            err,
            JevError::Protocol("car: probabilities not an object".into())
        );

        let mut v = json(RECORDED);
        v["answers"]["car"]["confidence"] = serde_json::json!("high");
        let err = parse_response(&v, &["car"]).unwrap_err();
        assert_eq!(
            err,
            JevError::Protocol("car: confidence: not a number".into())
        );

        let mut v = json(RECORDED);
        v["answers"]["car"]["probabilities"]["a"] = Value::Null;
        let err = parse_response(&v, &["car"]).unwrap_err();
        assert_eq!(
            err,
            JevError::Protocol("car: probabilities.a: not a number".into())
        );

        let mut v = json(RECORDED);
        v["answers"]["onScreen"]["type"] = serde_json::json!("verdict");
        let err = parse_response(&v, &["onScreen"]).unwrap_err();
        assert_eq!(
            err,
            JevError::Protocol("onScreen: unknown type \"verdict\"".into())
        );

        let mut v = json(RECORDED);
        v.as_object_mut().unwrap().remove("usage");
        assert_eq!(
            parse_response(&v, &["car"]).unwrap_err(),
            JevError::Protocol("usage not an object".into())
        );
    }

    #[test]
    fn batch_probabilities_rejects_options_that_were_not_offered() {
        let answer = |choice: &str, probs: &[(&str, f64)]| ChoiceAnswer {
            choice: choice.into(),
            probabilities: probs.iter().map(|(k, p)| (k.to_string(), *p)).collect(),
            confidence: 0.9,
        };
        assert_eq!(
            batch_probabilities(&answer("c1", &[("c1", 0.7), ("none", 0.3)]), 3).unwrap(),
            vec![0.0, 0.7, 0.0]
        );
        assert!(batch_probabilities(&answer("c3", &[("c3", 1.0)]), 3).is_err());
        assert!(batch_probabilities(&answer("c0", &[("c0", 0.5), ("c01", 0.5)]), 3).is_err());
        assert!(batch_probabilities(&answer("x", &[]), 3).is_err());
    }

    // --- confidence gate ---------------------------------------------------

    #[test]
    fn confidence_gate_falls_back_to_recency() {
        let cands: Vec<ChatCandidate> = (0..3).map(candidate).collect();
        let ranked = |confidence: f64, chose_none: bool| {
            merge_ranking(
                &cands,
                vec![(0, 0.1), (1, 0.2), (2, 0.6)],
                &[(confidence, chose_none)],
                Usage::default(),
            )
        };
        let jev_order = vec!["chat-2", "chat-1", "chat-0"];
        let recency = vec!["chat-0", "chat-1", "chat-2"];

        let r = ranked(0.8, false);
        assert!(r.is_confident(DEFAULT_CONFIDENCE_GATE));
        assert_eq!(r.ordered_ids(DEFAULT_CONFIDENCE_GATE, &cands), jev_order);
        assert_eq!(r.chats[0].rank, 1);

        let r = ranked(0.49, false);
        assert!(!r.is_confident(DEFAULT_CONFIDENCE_GATE));
        assert_eq!(r.ordered_ids(DEFAULT_CONFIDENCE_GATE, &cands), recency);

        let r = ranked(0.5, false);
        assert!(
            r.is_confident(DEFAULT_CONFIDENCE_GATE),
            "the gate is inclusive"
        );
        assert!(!r.is_confident(0.8));

        let r = ranked(0.95, true);
        assert!(
            !r.is_confident(DEFAULT_CONFIDENCE_GATE),
            "a confident none is no ranking"
        );
        assert_eq!(r.ordered_ids(DEFAULT_CONFIDENCE_GATE, &cands), recency);
    }

    // --- error mapping -----------------------------------------------------

    #[test]
    fn is_down_matches_typesafe_down() {
        assert!(JevError::NoKey.is_down());
        assert!(JevError::KeyRefused(401, String::new()).is_down());
        assert!(JevError::Unreachable("x".into()).is_down());
        assert!(
            !JevError::Http(422, "bad state".into()).is_down(),
            "one call's problem"
        );
        assert!(!JevError::RateLimited(429).is_down());
        assert!(!JevError::Protocol("x".into()).is_down());
    }

    #[test]
    fn debug_never_shows_the_key() {
        let c =
            JevClient::with_endpoint("sk-secret-value", "http://127.0.0.1:1/v1/systemone").unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("sk-secret-value"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    // --- fake server --------------------------------------------------------

    struct Canned {
        status: u16,
        retry_after: Option<&'static str>,
        body: String,
    }

    fn ok(body: &str) -> Canned {
        Canned {
            status: 200,
            retry_after: None,
            body: body.to_string(),
        }
    }

    fn status(code: u16, body: &str) -> Canned {
        Canned {
            status: code,
            retry_after: None,
            body: body.to_string(),
        }
    }

    fn busy(code: u16) -> Canned {
        Canned {
            status: code,
            retry_after: Some("0.01"),
            body: "busy".into(),
        }
    }

    struct Seen {
        auth: Option<String>,
        body: Value,
    }

    /// A local System One answering `responses` in turn; returns the
    /// endpoint and what each request carried.
    async fn fake_server(responses: Vec<Canned>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let route = warp::post()
            .and(warp::path!("v1" / "systemone"))
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::body::bytes())
            .map(move |auth: Option<String>, body: hyper::body::Bytes| {
                seen_in.lock().unwrap().push(Seen {
                    auth,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                let next = queue.lock().unwrap().pop_front();
                let Some(c) = next else {
                    return warp::http::Response::builder()
                        .status(500)
                        .body("unexpected request".to_string())
                        .unwrap();
                };
                let mut b = warp::http::Response::builder().status(c.status);
                if let Some(s) = c.retry_after {
                    b = b.header("retry-after", s);
                }
                b.body(c.body).unwrap()
            });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);
        (format!("http://{addr}/v1/systemone"), seen)
    }

    #[tokio::test]
    async fn decide_sends_the_reference_request() {
        let (url, seen) = fake_server(vec![ok(RECORDED)]).await;
        let client = JevClient::with_endpoint("test-key", &url).unwrap();
        let r = client
            .decide("a state", &reference_questions())
            .await
            .unwrap();
        assert_eq!(r.choice("car").unwrap().choice, "a");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].auth.as_deref(), Some("Bearer test-key"));
        assert_eq!(seen[0].body["model"], "jev-latest");
        assert_eq!(seen[0].body["state"], "a state");
        assert_eq!(seen[0].body["questions"]["car"]["type"], "choice");
    }

    #[tokio::test]
    async fn decide_backs_off_on_429_and_529_then_answers() {
        let (url, seen) = fake_server(vec![busy(429), busy(529), ok(RECORDED)]).await;
        let client = JevClient::with_endpoint("test-key", &url).unwrap();
        let r = client.decide("{}", &reference_questions()).await.unwrap();
        assert_eq!(r.noul("onScreen").unwrap(), 0.82);
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn decide_gives_up_after_five_busy_answers() {
        let (url, seen) = fake_server((0..5).map(|_| busy(529)).collect()).await;
        let client = JevClient::with_endpoint("test-key", &url).unwrap();
        let err = client
            .decide("{}", &reference_questions())
            .await
            .unwrap_err();
        assert_eq!(err, JevError::RateLimited(529));
        assert_eq!(seen.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn decide_maps_statuses_to_errors() {
        let (url, _) = fake_server(vec![
            status(401, "bad key"),
            status(403, "forbidden"),
            status(422, "questions.car.criteria: required"),
            status(500, &"x".repeat(1000)),
            ok("not json"),
            ok(r#"{"model":"jev-1.13.0","answers":{},"usage":{"input_tokens":1,"output_tokens":0}}"#),
        ])
        .await;
        let client = JevClient::with_endpoint("test-key", &url).unwrap();
        let q = reference_questions();

        let err = client.decide("{}", &q).await.unwrap_err();
        assert_eq!(err, JevError::KeyRefused(401, "bad key".into()));
        assert!(err.is_down());
        assert!(err.to_string().contains("refused the key"));

        let err = client.decide("{}", &q).await.unwrap_err();
        assert_eq!(err, JevError::KeyRefused(403, "forbidden".into()));

        let err = client.decide("{}", &q).await.unwrap_err();
        assert_eq!(
            err,
            JevError::Http(422, "questions.car.criteria: required".into())
        );
        assert!(!err.is_down());

        let err = client.decide("{}", &q).await.unwrap_err();
        assert!(matches!(&err, JevError::Http(500, body) if body.len() == BODY_EXCERPT_CHARS));

        let err = client.decide("{}", &q).await.unwrap_err();
        assert!(matches!(&err, JevError::Protocol(m) if m.starts_with("response is not JSON")));

        let err = client.decide("{}", &q).await.unwrap_err();
        assert_eq!(err, JevError::Protocol("no answer for car".into()));
    }

    #[tokio::test]
    async fn decide_reports_unreachable_and_no_key() {
        // A port that was just free: connection refused.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client =
            JevClient::with_endpoint("test-key", &format!("http://127.0.0.1:{port}/v1/systemone"))
                .unwrap();
        let err = client
            .decide("{}", &reference_questions())
            .await
            .unwrap_err();
        assert!(matches!(&err, JevError::Unreachable(_)), "{err}");
        assert!(err.is_down());

        let client = JevClient::with_endpoint("  ", "http://127.0.0.1:1/v1/systemone").unwrap();
        assert_eq!(
            client
                .decide("{}", &reference_questions())
                .await
                .unwrap_err(),
            JevError::NoKey
        );
    }

    #[tokio::test]
    async fn decide_times_out_as_unreachable() {
        let route = warp::post().and_then(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok::<_, warp::Rejection>("late")
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);
        let mut client =
            JevClient::with_endpoint("test-key", &format!("http://{addr}/v1/systemone")).unwrap();
        client.total_timeout = Duration::from_millis(200);
        let err = client
            .decide("{}", &reference_questions())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, JevError::Unreachable(m) if m.contains("did not answer")),
            "{err}"
        );
    }

    /// The real TLS path against api.typesafe.ai with a made-up key and a
    /// state that carries no user data. Ignored by default (network); run
    /// with `cargo test --lib jev -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_endpoint_refuses_a_made_up_key() {
        let mut client = JevClient::with_endpoint("not-a-real-key", ENDPOINT).unwrap();
        let err = client
            .decide("test", &reference_questions())
            .await
            .unwrap_err();
        assert!(matches!(&err, JevError::KeyRefused(401 | 403, _)), "{err}");
    }

    // --- ranking -------------------------------------------------------------

    #[tokio::test]
    async fn rank_chats_batches_over_255_and_merges_by_probability() {
        let (url, seen) = fake_server(vec![ok(RANK_BATCH_1), ok(RANK_BATCH_2)]).await;
        let client = JevClient::with_endpoint("test-key", &url).unwrap();
        let cands: Vec<ChatCandidate> = (0..300).map(candidate).collect();
        let ctx = RankContext {
            workspace_dir: PathBuf::from("/Users/dev/GitRepo/gitterm-v5"),
            branch: Some("master".into()),
            query: Some("windows build".into()),
            recent_prompt: None,
        };
        let r = rank_chats(&client, &ctx, &cands).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "300 chats take two calls");
        let options = |i: usize| {
            seen[i].body["questions"][RANK_QUESTION_ID]["criteria"]
                .as_object()
                .unwrap()
                .len()
        };
        assert_eq!(options(0), MAX_CHOICES, "254 chats + none");
        assert_eq!(options(1), 300 - CHATS_PER_CALL + 1);
        assert_eq!(seen[0].body["state"], seen[1].body["state"]);

        assert_eq!(r.calls, 2);
        assert_eq!(r.chats.len(), 300);
        // Batch 2 picked its c10 (chat 264) at 0.81, above batch 1's best
        // (c3, chat 3, at 0.6); its confidence is the one reported.
        let top: Vec<(&str, f64)> = r.chats[..4]
            .iter()
            .map(|c| (c.id.as_str(), c.probability))
            .collect();
        assert_eq!(
            top,
            vec![
                ("chat-264", 0.81),
                ("chat-3", 0.6),
                ("chat-0", 0.25),
                ("chat-265", 0.09)
            ]
        );
        assert_eq!(r.confidence, 0.77);
        assert!(!r.chose_none);
        assert!(r.is_confident(DEFAULT_CONFIDENCE_GATE));
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 19_000 + 3_500,
                output_tokens: 0
            }
        );
        // Unscored chats keep recency order after the scored ones.
        assert_eq!(r.chats[4].id, "chat-1");
        assert_eq!(r.chats.last().unwrap().rank, 300);
    }

    #[tokio::test]
    async fn rank_chats_stops_at_the_first_failed_batch() {
        let (url, seen) = fake_server(vec![status(401, "bad key")]).await;
        let client = JevClient::with_endpoint("test-key", &url).unwrap();
        let cands: Vec<ChatCandidate> = (0..300).map(candidate).collect();
        let err = rank_chats(&client, &RankContext::default(), &cands)
            .await
            .unwrap_err();
        assert!(err.is_down());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rank_chats_with_no_candidates_makes_no_call() {
        let client =
            JevClient::with_endpoint("test-key", "http://127.0.0.1:1/v1/systemone").unwrap();
        let r = rank_chats(&client, &RankContext::default(), &[])
            .await
            .unwrap();
        assert_eq!(r.calls, 0);
        assert!(r.chats.is_empty());
        assert!(!r.is_confident(0.0));
    }
}
