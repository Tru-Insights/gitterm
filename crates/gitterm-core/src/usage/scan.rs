//! Blocking transcript scan with explicit source roots and calendar inputs.

use super::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::time::{Instant, SystemTime};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

/// Bytes kept from the end of the parsed part of a file, compared before
/// a grown file is parsed incrementally: a mismatch means the file was
/// rewritten, not appended to, and it is parsed again from the start.
const RESUME_CHECK_BYTES: usize = 64;
/// A forked Codex rollout starts by replaying its parent's history,
/// token_count events included, all stamped within a moment of the fork's
/// session_meta (at most 0.33 s on this machine); the fork's own first
/// response lands seconds later (4.4 s at the earliest). Token counts this
/// close to the fork are the parent's and are not counted again.
const FORK_REPLAY_WINDOW: chrono::TimeDelta = chrono::TimeDelta::seconds(2);
/// How far into a Codex line the cheap type sniff looks.
const CODEX_SNIFF_BYTES: usize = 200;

/// Parser state that must survive between incremental reads of a file.
#[derive(Debug, Clone, Default)]
struct ParseState {
    /// Claude: message id -> index in `records` (later lines replace).
    claude_ids: HashMap<String, usize>,
    /// Codex: model of the current turn.
    codex_model: Option<String>,
    codex_meta_seen: bool,
    /// Codex: this thread's id, from its session_meta.
    codex_thread_id: Option<String>,
    /// Codex: when a forked rollout was created; the parent history it
    /// replays is stamped within [`FORK_REPLAY_WINDOW`] of this.
    codex_forked_at: Option<DateTime<Utc>>,
    codex_headless: bool,
    /// Codex: the last token_count total, to drop repeated events.
    codex_prev_total: Option<CodexUsage>,
    /// Codex: once true, token_count events are ignored.
    codex_has_records: bool,
    codex_response_ids: HashSet<String>,
    /// Pi: line ids already counted.
    pi_ids: HashSet<String>,
}

/// One transcript as parsed so far.
#[derive(Debug, Clone)]
struct FileUsage {
    backend: ChatBackend,
    mtime: SystemTime,
    size: u64,
    /// Bytes consumed: everything up to the end of the last complete line.
    offset: u64,
    /// The last bytes before `offset`, for the append-only check.
    tail: Vec<u8>,
    cwd: Option<PathBuf>,
    records: Vec<UsageRecord>,
    state: ParseState,
}

impl FileUsage {
    fn new(backend: ChatBackend) -> Self {
        FileUsage {
            backend,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            offset: 0,
            tail: Vec::new(),
            cwd: None,
            records: Vec::new(),
            state: ParseState::default(),
        }
    }
}

/// Parsed transcripts and resolved repo roots, reused across scans.
#[derive(Debug, Default)]
pub struct UsageCache {
    files: HashMap<PathBuf, FileUsage>,
    /// cwd -> main repo root (None: gone or not a git repo).
    repo_roots: HashMap<PathBuf, Option<PathBuf>>,
}

impl UsageSources {
    /// Every transcript, with its backend. Claude: slug/session.jsonl and
    /// slug/session/subagents/agent-*.jsonl; Codex: YYYY/MM/DD/rollout
    /// (older rollouts sit at the top level); Pi: slug/session.jsonl.
    fn files(&self) -> Vec<(PathBuf, ChatBackend)> {
        let mut out = Vec::new();
        for (root, depth, backend) in [
            (&self.claude_projects, 3, ChatBackend::Claude),
            (&self.codex_sessions, 3, ChatBackend::Codex),
            (&self.pi_sessions, 1, ChatBackend::Pi),
        ] {
            out.extend(
                crate::plan_limits::jsonl_files_under(root, depth)
                    .into_iter()
                    .map(|path| (path, backend)),
            );
        }
        out
    }
}

/// Scan with the calendar pinned: `today` and the time zone the day buckets use.
pub fn scan_usage_in<Tz: TimeZone>(
    sources: &UsageSources,
    window: UsageWindow,
    today: NaiveDate,
    tz: &Tz,
    pricing: &Pricing,
    cache: &mut UsageCache,
) -> UsageReport {
    let started = Instant::now();
    let start = window.start(today);
    let mut stats = ScanStats::default();
    let files = sources.files();
    stats.files_seen = files.len();

    let present: HashSet<&PathBuf> = files.iter().map(|(path, _)| path).collect();
    cache.files.retain(|path, _| present.contains(path));

    let mut in_window: Vec<PathBuf> = Vec::new();
    for (path, backend) in &files {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) => {
                eprintln!("[usage] cannot stat {}: {err}", path.display());
                stats.files_unreadable += 1;
                continue;
            }
        };
        let mtime = match meta.modified() {
            Ok(mtime) => mtime,
            Err(err) => {
                eprintln!("[usage] no mtime for {}: {err}", path.display());
                stats.files_unreadable += 1;
                continue;
            }
        };
        // A file last written before the window holds nothing in it.
        if DateTime::<Utc>::from(mtime).with_timezone(tz).date_naive() < start {
            stats.files_outside_window += 1;
            continue;
        }
        match refresh_file(cache, path, *backend, mtime, meta.len()) {
            Ok(Refresh::Cached) => stats.files_cached += 1,
            Ok(Refresh::Resumed) => stats.files_resumed += 1,
            Ok(Refresh::Parsed) => stats.files_parsed += 1,
            Err(err) => {
                eprintln!("[usage] cannot read {}: {err}", path.display());
                cache.files.remove(path);
                stats.files_unreadable += 1;
                continue;
            }
        }
        in_window.push(path.clone());
    }
    // Stable order so cross-file dedupe keeps the same copy every scan.
    in_window.sort();

    for path in &in_window {
        let Some(cwd) = cache.files.get(path).and_then(|file| file.cwd.clone()) else {
            continue;
        };
        cache
            .repo_roots
            .entry(cwd.clone())
            .or_insert_with(|| resolve_root(&cwd));
    }

    let mut all = ViewBuilder::new(start, today);
    let mut interactive = ViewBuilder::new(start, today);
    let mut seen_keys: HashSet<&str> = HashSet::new();
    let mut prices: HashMap<&str, Option<ModelPrice>> = HashMap::new();
    let mut headless_responses = 0;
    for path in &in_window {
        let file = &cache.files[path];
        let repo = repo_key(file.cwd.as_deref(), &cache.repo_roots);
        for record in &file.records {
            if record.tokens.processed() == 0 {
                continue;
            }
            let date = record.at.with_timezone(tz).date_naive();
            if date < start || date > today {
                continue;
            }
            if let Some(key) = &record.key {
                if !seen_keys.insert(key.as_str()) {
                    stats.duplicates += 1;
                    continue;
                }
            }
            let price = *prices
                .entry(record.model.as_str())
                .or_insert_with(|| pricing.price_for(&record.model));
            stats.responses += 1;
            all.add(file.backend, date, record, price, &repo);
            if record.headless {
                headless_responses += 1;
            } else {
                interactive.add(file.backend, date, record, price, &repo);
            }
        }
    }

    stats.elapsed = started.elapsed();
    UsageReport {
        window,
        start,
        end: today,
        all: all.finish(),
        interactive: interactive.finish(),
        headless_responses,
        stats,
    }
}

fn resolve_root(cwd: &Path) -> Option<PathBuf> {
    if !cwd.exists() {
        return None;
    }
    crate::find::resolve_repo_root(cwd).map(|(root, _)| root)
}

/// (display name, path, resolved) for a file's cwd.
fn repo_key(
    cwd: Option<&Path>,
    roots: &HashMap<PathBuf, Option<PathBuf>>,
) -> (String, Option<PathBuf>, bool) {
    let Some(cwd) = cwd else {
        return ("(no cwd)".to_string(), None, false);
    };
    let (path, resolved) = match roots.get(cwd).cloned().flatten() {
        Some(root) => (root, true),
        None => (cwd.to_path_buf(), false),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    (name, Some(path), resolved)
}

enum Refresh {
    Cached,
    Resumed,
    Parsed,
}

/// Bring the cached parse of `path` up to date with its (mtime, size).
fn refresh_file(
    cache: &mut UsageCache,
    path: &Path,
    backend: ChatBackend,
    mtime: SystemTime,
    size: u64,
) -> std::io::Result<Refresh> {
    if let Some(file) = cache.files.get_mut(path) {
        if file.backend == backend && file.mtime == mtime && file.size == size {
            return Ok(Refresh::Cached);
        }
        if file.backend == backend && size >= file.offset && still_prefix(path, file)? {
            read_lines(path, file)?;
            file.mtime = mtime;
            file.size = size;
            return Ok(Refresh::Resumed);
        }
    }
    let mut file = FileUsage::new(backend);
    read_lines(path, &mut file)?;
    file.mtime = mtime;
    file.size = size;
    cache.files.insert(path.to_path_buf(), file);
    Ok(Refresh::Parsed)
}

/// Whether the bytes parsed last time are still where they were (the file
/// was appended to rather than rewritten).
fn still_prefix(path: &Path, file: &FileUsage) -> std::io::Result<bool> {
    if file.tail.is_empty() {
        return Ok(file.offset == 0);
    }
    let mut handle = File::open(path)?;
    handle.seek(SeekFrom::Start(file.offset - file.tail.len() as u64))?;
    let mut buf = vec![0u8; file.tail.len()];
    match handle.read_exact(&mut buf) {
        Ok(()) => Ok(buf == file.tail),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

/// Parse complete lines from `file.offset` to the end. A trailing line
/// without its newline is still being written; it is left for next time.
fn read_lines(path: &Path, file: &mut FileUsage) -> std::io::Result<()> {
    let mut handle = File::open(path)?;
    handle.seek(SeekFrom::Start(file.offset))?;
    let mut reader = BufReader::with_capacity(256 * 1024, handle);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        file.offset += read as u64;
        let keep = line.len().min(RESUME_CHECK_BYTES);
        file.tail.clear();
        file.tail.extend_from_slice(&line[line.len() - keep..]);
        let body = &line[..line.len() - 1];
        match file.backend {
            ChatBackend::Claude => claude_line(file, body),
            ChatBackend::Codex => codex_line(file, body),
            ChatBackend::Pi => pi_line(file, body),
        }
    }
    Ok(())
}

fn parse_timestamp(raw: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw?)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    std::str::from_utf8(haystack).is_ok_and(|text| text.contains(needle))
}

// ---- Claude ----

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    entrypoint: Option<String>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<ClaudeCacheCreation>,
}

#[derive(Deserialize)]
struct ClaudeCacheCreation {
    ephemeral_1h_input_tokens: Option<u64>,
}

/// Entrypoint GitTerm's native chat tabs run under: interactive, though
/// it is an SDK entrypoint (see chats.rs).
const SDK_TS_ENTRYPOINT: &str = "sdk-ts";

fn claude_line(file: &mut FileUsage, line: &[u8]) {
    // Most bytes are user/tool lines without usage; skip them unparsed.
    if !contains(line, "\"usage\"") {
        return;
    }
    let Ok(parsed) = serde_json::from_slice::<ClaudeLine>(line) else {
        return;
    };
    if parsed.kind.as_deref() != Some("assistant") {
        return;
    }
    if file.cwd.is_none() {
        file.cwd = parsed.cwd.as_deref().map(PathBuf::from);
    }
    let Some(message) = parsed.message else {
        return;
    };
    let (Some(usage), Some(model)) = (message.usage, message.model) else {
        return;
    };
    if model == "<synthetic>" {
        return;
    }
    let Some(at) = parse_timestamp(parsed.timestamp.as_deref()) else {
        return;
    };
    let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
    let tokens = Tokens {
        uncached_input: usage.input_tokens.unwrap_or(0),
        cache_read: usage.cache_read_input_tokens.unwrap_or(0),
        cache_write,
        cache_write_1h: usage
            .cache_creation
            .and_then(|c| c.ephemeral_1h_input_tokens)
            .unwrap_or(0)
            .min(cache_write),
        output: usage.output_tokens.unwrap_or(0),
    };
    let headless = parsed
        .entrypoint
        .as_deref()
        .is_some_and(|entry| entry.starts_with("sdk") && entry != SDK_TS_ENTRYPOINT);
    let record = UsageRecord {
        at,
        model,
        tokens,
        headless,
        key: message.id.as_ref().map(|id| format!("claude:{id}")),
    };
    // Streamed partials share the message id: the last line wins.
    if let Some(id) = message.id {
        if let Some(&idx) = file.state.claude_ids.get(&id) {
            file.records[idx] = record;
            return;
        }
        file.state.claude_ids.insert(id, file.records.len());
    }
    // Zero-usage records keep their slot (a later partial may fill it)
    // and are skipped when aggregating.
    file.records.push(record);
}

// ---- Codex ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

impl CodexUsage {
    fn tokens(&self) -> Tokens {
        Tokens {
            uncached_input: self
                .input_tokens
                .saturating_sub(self.cached_input_tokens)
                .saturating_sub(self.cache_write_input_tokens),
            cache_read: self.cached_input_tokens,
            cache_write: self.cache_write_input_tokens,
            cache_write_1h: 0,
            output: self.output_tokens,
        }
    }
}

#[derive(Deserialize)]
struct CodexLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<Value>,
}

#[derive(Debug, PartialEq, Eq)]
enum CodexSniff {
    /// A line kind the scanner reads.
    Wanted,
    /// A line kind the scanner ignores.
    Skip,
    /// The type was not in the sniffed prefix; parse to find out.
    Unknown,
}

/// Classify a rollout line from its first bytes (`{"timestamp":…,"type":…,
/// "payload":{"type":…`) without parsing the whole, often huge, line.
fn sniff_codex(line: &[u8]) -> CodexSniff {
    let head = &line[..line.len().min(CODEX_SNIFF_BYTES)];
    let Some((kind, after)) = string_after(head, b"\"type\":\"") else {
        return CodexSniff::Unknown;
    };
    // Only the top-level type counts, and rollouts write it before the
    // payload; any other key order is left to the full parse.
    if find(head, b"\"payload\"").is_some_and(|at| at < after) {
        return CodexSniff::Unknown;
    }
    match kind {
        b"session_meta" | b"turn_context" | b"token_usage_record" => CodexSniff::Wanted,
        b"event_msg" => match string_after(&head[after..], b"\"payload\":{\"type\":\"") {
            Some((b"token_count", _)) => CodexSniff::Wanted,
            Some(_) => CodexSniff::Skip,
            None => CodexSniff::Unknown,
        },
        _ => CodexSniff::Skip,
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The string value right after `marker` in `hay`, and the index just
/// past it. None when the marker or the closing quote is not in `hay`.
fn string_after<'a>(hay: &'a [u8], marker: &[u8]) -> Option<(&'a [u8], usize)> {
    let start = find(hay, marker)? + marker.len();
    let len = hay[start..].iter().position(|&b| b == b'"')?;
    Some((&hay[start..start + len], start + len + 1))
}

fn codex_line(file: &mut FileUsage, line: &[u8]) {
    if sniff_codex(line) == CodexSniff::Skip {
        return;
    }
    let Ok(parsed) = serde_json::from_slice::<CodexLine>(line) else {
        return;
    };
    let Some(payload) = parsed.payload else {
        return;
    };
    let state = &mut file.state;
    match parsed.kind.as_deref() {
        Some("session_meta") => {
            // A forked rollout repeats the parent's meta after its own;
            // the first one is this thread's.
            if state.codex_meta_seen {
                return;
            }
            state.codex_meta_seen = true;
            file.cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            state.codex_headless = codex_meta_is_headless(&payload);
            state.codex_thread_id = payload
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
            if payload
                .get("forked_from_id")
                .is_some_and(|id| !id.is_null())
            {
                state.codex_forked_at = parse_timestamp(parsed.timestamp.as_deref());
            }
        }
        Some("turn_context") => {
            if let Some(model) = payload.get("model").and_then(Value::as_str) {
                state.codex_model = Some(model.to_string());
            }
        }
        Some("token_usage_record") => {
            state.codex_has_records = true;
            let Some(response_id) = payload.get("response_id").and_then(Value::as_str) else {
                return;
            };
            // Only this thread's responses (a fork's replay of its parent
            // would carry the parent's thread id).
            let thread = payload.get("thread_id").and_then(Value::as_str);
            if thread.is_some_and(|thread| {
                state
                    .codex_thread_id
                    .as_deref()
                    .is_some_and(|own| own != thread)
            }) {
                return;
            }
            if !state.codex_response_ids.insert(response_id.to_string()) {
                return;
            }
            let Some(usage) = payload
                .get("usage")
                .and_then(|u| CodexUsage::deserialize(u).ok())
            else {
                return;
            };
            let Some(at) = parse_timestamp(parsed.timestamp.as_deref()) else {
                return;
            };
            let key = Some(format!("codex:{response_id}"));
            push_codex(file, at, usage, key);
        }
        Some("event_msg") => {
            if payload.get("type").and_then(Value::as_str) != Some("token_count")
                || state.codex_has_records
            {
                return;
            }
            let Some(info) = payload.get("info").filter(|info| !info.is_null()) else {
                return;
            };
            let total = info
                .get("total_token_usage")
                .and_then(|u| CodexUsage::deserialize(u).ok());
            let last = info
                .get("last_token_usage")
                .and_then(|u| CodexUsage::deserialize(u).ok());
            let (Some(total), Some(last)) = (total, last) else {
                return;
            };
            let Some(at) = parse_timestamp(parsed.timestamp.as_deref()) else {
                return;
            };
            if state
                .codex_forked_at
                .is_some_and(|forked| at - forked < FORK_REPLAY_WINDOW)
            {
                return;
            }
            // The same event is often written twice; an unchanged running
            // total means no new response.
            if state.codex_prev_total == Some(total) {
                return;
            }
            state.codex_prev_total = Some(total);
            push_codex(file, at, last, None);
        }
        _ => {}
    }
}

fn push_codex(file: &mut FileUsage, at: DateTime<Utc>, usage: CodexUsage, key: Option<String>) {
    let tokens = usage.tokens();
    if tokens.processed() == 0 {
        return;
    }
    file.records.push(UsageRecord {
        at,
        model: file
            .state
            .codex_model
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        tokens,
        headless: file.state.codex_headless,
        key,
    });
}

/// `codex exec` runs and harness subagents (approval guardian, review)
/// are headless; `thread_spawn` subagents belong to a human session.
fn codex_meta_is_headless(payload: &Value) -> bool {
    let exec = payload
        .get("originator")
        .and_then(Value::as_str)
        .is_some_and(|originator| originator.contains("exec"));
    let harness_subagent = payload
        .get("source")
        .and_then(|source| source.get("subagent"))
        .is_some_and(|subagent| subagent.get("thread_spawn").is_none());
    exec || harness_subagent
}

// ---- Pi ----

#[derive(Deserialize)]
struct PiLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    message: Option<PiMessage>,
}

#[derive(Deserialize)]
struct PiMessage {
    role: Option<String>,
    model: Option<String>,
    usage: Option<PiUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiUsage {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

fn pi_line(file: &mut FileUsage, line: &[u8]) {
    let Ok(parsed) = serde_json::from_slice::<PiLine>(line) else {
        return;
    };
    match parsed.kind.as_deref() {
        Some("session") if file.cwd.is_none() => {
            file.cwd = parsed.cwd.map(PathBuf::from);
        }
        Some("message") => {
            let Some(message) = parsed.message else {
                return;
            };
            if message.role.as_deref() != Some("assistant") {
                return;
            }
            let (Some(usage), Some(model)) = (message.usage, message.model) else {
                return;
            };
            if let Some(id) = &parsed.id {
                if !file.state.pi_ids.insert(id.clone()) {
                    return;
                }
            }
            let Some(at) = parse_timestamp(parsed.timestamp.as_deref()) else {
                return;
            };
            let tokens = Tokens {
                uncached_input: usage.input.unwrap_or(0),
                cache_read: usage.cache_read.unwrap_or(0),
                cache_write: usage.cache_write.unwrap_or(0),
                cache_write_1h: 0,
                output: usage.output.unwrap_or(0),
            };
            if tokens.processed() == 0 {
                return;
            }
            file.records.push(UsageRecord {
                at,
                model,
                tokens,
                headless: false,
                key: None,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/usage");

    fn fixture_sources() -> UsageSources {
        let base = Path::new(FIXTURES);
        UsageSources {
            claude_projects: base.join("claude").join("projects"),
            codex_sessions: base.join("codex").join("sessions"),
            pi_sessions: base.join("pi").join("sessions"),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 7).unwrap()
    }

    fn scan(sources: &UsageSources, window: UsageWindow, cache: &mut UsageCache) -> UsageReport {
        scan_usage_in(sources, window, today(), &Utc, &Pricing::seeded(), cache)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn model<'a>(view: &'a UsageView, name: &str) -> &'a ModelRow {
        view.models
            .iter()
            .find(|row| row.model == name)
            .unwrap_or_else(|| panic!("no row for {name}"))
    }

    #[test]
    fn fixtures_cover_every_backend_at_list_rates() {
        let report = scan(
            &fixture_sources(),
            UsageWindow::Days7,
            &mut UsageCache::default(),
        );
        let all = &report.all;
        assert_eq!(report.start, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        assert_eq!(all.days.len(), 7);
        // Claude A (last partial wins) 0.01124 + B (1h writes) 0.0935 +
        // C (headless) 0.003; Pi a1 0.0048; Codex astra 0.0078 + sol
        // 0.00372 + luna 0.000045. Only the unknown Claude model is
        // unpriced.
        assert!(close(all.totals.cost, 0.124105), "{}", all.totals.cost);
        assert_eq!(all.totals.responses, 8);
        assert_eq!(all.totals.unpriced_tokens, 100);
        assert_eq!(report.headless_responses, 1);
        assert!(close(report.interactive.totals.cost, 0.121105));
        assert_eq!(report.interactive.totals.responses, 7);

        let opus = model(all, "claude-opus-5-5");
        assert_eq!(opus.usage.tokens.output, 500, "the final partial's usage");
        assert_eq!(opus.usage.responses, 1);
        assert!(opus.priced);
        let fable = model(all, "claude-fable-5-1");
        assert_eq!(fable.usage.tokens.cache_write_1h, 2000);
        assert!(close(fable.usage.cost, 0.0935));
        assert!(!model(all, "claude-mystery-9").priced);
        assert!(all.models.iter().all(|row| row.model != "<synthetic>"));

        let astra = model(all, "gpt-6-astra");
        assert_eq!(astra.harness, ChatBackend::Codex);
        assert_eq!(
            astra.usage.responses, 1,
            "the repeated token_count is one response"
        );
        assert_eq!(
            astra.usage.tokens,
            Tokens {
                uncached_input: 200,
                cache_read: 800,
                cache_write: 0,
                cache_write_1h: 0,
                output: 100
            }
        );
        let sol = model(all, "gpt-6.1-sol");
        assert_eq!(
            sol.usage.tokens.uncached_input, 800,
            "last_token_usage, not the total"
        );
        assert_eq!(sol.usage.tokens.cache_read, 1200);
        assert!(sol.priced);
        assert!(close(sol.usage.cost, 0.00372), "{}", sol.usage.cost);
        assert!(close(astra.usage.cost, 0.0078), "{}", astra.usage.cost);

        let pi_sonnet = all
            .models
            .iter()
            .find(|row| row.harness == ChatBackend::Pi && row.model == "claude-sonnet-5-5")
            .unwrap();
        assert_eq!(
            pi_sonnet.usage.responses, 1,
            "duplicate pi line ids count once"
        );
        assert!(close(pi_sonnet.usage.cost, 0.0048));

        let claude = &all.by_harness[harness_index(ChatBackend::Claude)];
        assert_eq!(claude.responses, 4);
        let oct6 = &all.days[5];
        assert_eq!(oct6.date, NaiveDate::from_ymd_opt(2026, 10, 6).unwrap());
        assert!(close(oct6.by_harness[0].cost, 0.0935 + 0.003));
        assert_eq!(all.days[2].by_harness[2].responses, 1, "pi a1 on Oct 3");

        let names: Vec<&str> = all.repos.iter().map(|row| row.name.as_str()).collect();
        // beta is the Codex-only repo; priced Codex now puts it ahead of gamma.
        assert_eq!(names, ["alpha", "beta", "gamma"], "costliest first");
        assert!(all.repos.iter().all(|row| !row.resolved));
    }

    #[test]
    fn a_repo_filter_narrows_totals_days_and_models_to_that_repo() {
        let report = scan(
            &fixture_sources(),
            UsageWindow::Days7,
            &mut UsageCache::default(),
        );
        for include_headless in [true, false] {
            let view = report.view(include_headless);
            let mut responses = 0;
            let mut cost = 0.0;
            for repo in &view.repos {
                let narrowed = report.view_for(include_headless, Some(&repo.path));
                // The narrowed totals are the repo row, and every slice of
                // the narrowed view adds back up to them.
                assert_eq!(narrowed.totals, repo.usage, "{}", repo.name);
                let day_responses: u64 = narrowed
                    .days
                    .iter()
                    .flat_map(|day| day.by_harness.iter())
                    .map(|bucket| bucket.responses)
                    .sum();
                assert_eq!(day_responses, repo.usage.responses);
                let model_responses: u64 =
                    narrowed.models.iter().map(|row| row.usage.responses).sum();
                assert_eq!(model_responses, repo.usage.responses);
                let model_cost: f64 = narrowed.models.iter().map(|row| row.usage.cost).sum();
                assert!(close(model_cost, repo.usage.cost));
                assert_eq!(narrowed.days.len(), view.days.len());
                assert_eq!(narrowed.repos.len(), 1);
                assert_eq!(narrowed.repos[0].path, repo.path);
                assert!(narrowed.per_repo.is_empty());
                responses += narrowed.totals.responses;
                cost += narrowed.totals.cost;
            }
            assert_eq!(responses, view.totals.responses);
            assert!(close(cost, view.totals.cost));
        }

        // beta is the Codex rollout: only its models, only on Codex.
        let beta = report
            .all
            .repos
            .iter()
            .find(|row| row.name == "beta")
            .unwrap();
        let narrowed = report.view_for(true, Some(&beta.path));
        let mut models: Vec<&str> = narrowed
            .models
            .iter()
            .map(|row| row.model.as_str())
            .collect();
        models.sort();
        assert_eq!(models, ["gpt-6-astra", "gpt-6.1-sol"]);
        assert_eq!(
            narrowed.by_harness[harness_index(ChatBackend::Claude)].responses,
            0
        );
        assert_eq!(
            narrowed.by_harness[harness_index(ChatBackend::Pi)].responses,
            0
        );
        assert_eq!(
            narrowed.by_harness[harness_index(ChatBackend::Codex)],
            narrowed.totals
        );

        // alpha's headless run drops out of the interactive population.
        let alpha_key = report
            .all
            .repos
            .iter()
            .find(|row| row.name == "alpha")
            .unwrap()
            .path
            .clone();
        let all_alpha = report.view_for(true, Some(&alpha_key));
        let interactive_alpha = report.view_for(false, Some(&alpha_key));
        assert_eq!(
            all_alpha.totals.responses,
            interactive_alpha.totals.responses + 1
        );

        // No filter is the unfiltered view; a repo with no usage in the
        // population is an empty view over the whole window.
        assert_eq!(*report.view_for(true, None), report.all);
        let nowhere = report.view_for(true, Some(&Some(PathBuf::from("/nowhere"))));
        assert_eq!(nowhere.totals, Bucket::default());
        assert_eq!(nowhere.days.len(), 7);
        assert!(nowhere.models.is_empty() && nowhere.repos.is_empty());
    }

    #[test]
    fn the_window_bounds_the_records() {
        let mut cache = UsageCache::default();
        let week = scan(&fixture_sources(), UsageWindow::Days7, &mut cache);
        let month = scan(&fixture_sources(), UsageWindow::Days30, &mut cache);
        assert_eq!(month.all.days.len(), 30);
        assert_eq!(month.all.totals.responses, week.all.totals.responses + 1);
        assert!(close(month.all.totals.cost - week.all.totals.cost, 0.024));
        assert_eq!(
            month.stats.files_cached, 3,
            "a window change re-reads nothing"
        );
    }

    #[test]
    fn pricing_overrides_replace_seed_rows_and_null_unprices() {
        let config: UsageConfig = serde_json::from_str(
            r#"{"pricing": {
                "gpt-6-astra": {"input": 1.0, "output": 10.0, "cache_read": 0.1, "cache_write": 0.0},
                "claude-opus-5-5": null
            }}"#,
        )
        .unwrap();
        let pricing = Pricing::with_overrides(&config);
        let astra = pricing.price_for("gpt-6-astra").unwrap();
        assert_eq!(astra.cache_write_1h, None);
        assert_eq!(pricing.price_for("claude-opus-5-5"), None);
        assert_eq!(
            pricing.price_for("claude-haiku-5-5"),
            None,
            "seeded unpriced"
        );
        assert_eq!(pricing.price_for("never-heard-of-it"), None);
        assert_eq!(
            pricing.price_for("claude-haiku-4-5-20251001"),
            pricing.price_for("claude-haiku-4-5"),
            "snapshot dates fall back to the base id"
        );
        assert!(pricing.price_for("claude-haiku-4-5").is_some());

        let report = scan_usage_in(
            &fixture_sources(),
            UsageWindow::Days7,
            today(),
            &Utc,
            &pricing,
            &mut UsageCache::default(),
        );
        // astra: 200 * 1 + 800 * 0.1 + 100 * 10 = 1280 per million.
        assert!(close(model(&report.all, "gpt-6-astra").usage.cost, 0.00128));
        assert!(!model(&report.all, "claude-opus-5-5").priced);

        let round_trip: UsageConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(round_trip, config);
        assert_eq!(
            serde_json::from_str::<UsageConfig>("{}").unwrap(),
            UsageConfig::default()
        );
    }

    fn write_lines(path: &Path, lines: &[String]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn claude_line(id: &str, ts: &str, output: u64, entrypoint: &str) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": ts, "cwd": "/nonexistent/synthetic/delta",
            "entrypoint": entrypoint, "isSidechain": false,
            "message": {"id": id, "model": "claude-opus-5-5", "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 1, "output_tokens": output,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        })
        .to_string()
    }

    fn temp_sources(dir: &Path) -> UsageSources {
        UsageSources::under(dir)
    }

    #[test]
    fn cache_skips_unchanged_files_and_resumes_grown_ones() {
        let dir = tempfile::tempdir().unwrap();
        let sources = temp_sources(dir.path());
        let path = sources.claude_projects.join("-slug").join("s.jsonl");
        write_lines(
            &path,
            &[claude_line("m1", "2026-10-06T10:00:00Z", 10, "cli")],
        );
        let mut cache = UsageCache::default();

        let cold = scan(&sources, UsageWindow::Days7, &mut cache);
        assert_eq!(
            (cold.stats.files_parsed, cold.all.totals.tokens.output),
            (1, 10)
        );
        let warm = scan(&sources, UsageWindow::Days7, &mut cache);
        assert_eq!((warm.stats.files_cached, warm.stats.files_parsed), (1, 0));
        assert_eq!(warm.all, cold.all);

        // A line still being written is not consumed until it ends.
        let next = claude_line("m2", "2026-10-07T10:00:00Z", 5, "cli");
        let (head, rest) = next.split_at(20);
        append(&path, head);
        let partial = scan(&sources, UsageWindow::Days7, &mut cache);
        assert_eq!(partial.stats.files_resumed, 1);
        assert_eq!(partial.all.totals.responses, 1);
        append(&path, &format!("{rest}\n"));
        // A later partial of m1 replaces its record, even across reads.
        append(
            &path,
            &format!("{}\n", claude_line("m1", "2026-10-06T10:00:01Z", 30, "cli")),
        );
        let grown = scan(&sources, UsageWindow::Days7, &mut cache);
        assert_eq!(grown.stats.files_resumed, 1);
        assert_eq!(grown.all.totals.responses, 2);
        assert_eq!(grown.all.totals.tokens.output, 30 + 5);

        // A rewritten (shorter) file is parsed from scratch.
        write_lines(
            &path,
            &[claude_line("m9", "2026-10-06T10:00:00Z", 7, "cli")],
        );
        let rewritten = scan(&sources, UsageWindow::Days7, &mut cache);
        assert_eq!(rewritten.stats.files_parsed, 1);
        assert_eq!(rewritten.all.totals.tokens.output, 7);
    }

    #[test]
    fn claude_subagents_count_and_copied_messages_count_once() {
        let dir = tempfile::tempdir().unwrap();
        let sources = temp_sources(dir.path());
        let slug = sources.claude_projects.join("-slug");
        let main = claude_line("m1", "2026-10-06T10:00:00Z", 10, "cli");
        write_lines(&slug.join("s.jsonl"), std::slice::from_ref(&main));
        // A resumed session's new transcript repeats the earlier message.
        write_lines(
            &slug.join("s2.jsonl"),
            &[main, claude_line("m2", "2026-10-06T11:00:00Z", 20, "cli")],
        );
        write_lines(
            &slug.join("s").join("subagents").join("agent-a1.jsonl"),
            &[claude_line("m3", "2026-10-06T10:30:00Z", 40, "cli")],
        );
        write_lines(
            &slug.join("review.jsonl"),
            &[
                claude_line("m4", "2026-10-06T12:00:00Z", 80, "sdk-cli"),
                claude_line("m5", "2026-10-06T12:00:00Z", 160, "sdk-ts"),
            ],
        );
        let report = scan(&sources, UsageWindow::Days7, &mut UsageCache::default());
        assert_eq!(report.all.totals.tokens.output, 10 + 20 + 40 + 80 + 160);
        assert_eq!(report.stats.duplicates, 1);
        assert_eq!(
            report.headless_responses, 1,
            "sdk-cli only; sdk-ts is a chat tab"
        );
        assert_eq!(report.interactive.totals.tokens.output, 10 + 20 + 40 + 160);
    }

    fn codex_usage(input: u64, cached: u64, output: u64) -> Value {
        serde_json::json!({"input_tokens": input, "cached_input_tokens": cached,
            "cache_write_input_tokens": 0, "output_tokens": output,
            "reasoning_output_tokens": 0, "total_tokens": input + output})
    }

    fn codex_meta(originator: &str, source: Value) -> String {
        serde_json::json!({"timestamp": "2026-10-06T08:00:00Z", "type": "session_meta",
            "payload": {"id": "t", "cwd": "/nonexistent/synthetic/eps",
                "originator": originator, "source": source}})
        .to_string()
    }

    fn codex_turn(model: &str) -> String {
        serde_json::json!({"timestamp": "2026-10-06T08:00:01Z", "type": "turn_context",
            "payload": {"model": model}})
        .to_string()
    }

    fn codex_count(total: Value, last: Value) -> String {
        codex_count_at("2026-10-06T08:00:02Z", total, last)
    }

    fn codex_count_at(ts: &str, total: Value, last: Value) -> String {
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "token_count",
                "info": {"total_token_usage": total, "last_token_usage": last}}})
        .to_string()
    }

    fn codex_record(response: &str, usage: Value) -> String {
        serde_json::json!({"timestamp": "2026-10-06T08:00:03Z", "ordinal": 4,
            "type": "token_usage_record",
            "payload": {"thread_id": "t", "turn_id": "u", "response_id": response,
                "usage": usage, "turn_token_usage": usage, "thread_token_usage": usage}})
        .to_string()
    }

    #[test]
    fn codex_records_win_over_token_counts_and_forks_count_their_own_turns() {
        let dir = tempfile::tempdir().unwrap();
        let sources = temp_sources(dir.path());
        let day = sources.codex_sessions.join("2026").join("10").join("06");
        // A forked thread replays its parent's meta and history, token
        // counts included, at the fork time; only its own later turns count.
        let mut fork_meta: Value = serde_json::from_str(&codex_meta(
            "codex-tui",
            serde_json::json!({"subagent": {"thread_spawn": {"parent_thread_id": "p"}}}),
        ))
        .unwrap();
        fork_meta["payload"]["forked_from_id"] = serde_json::json!("p");
        write_lines(
            &day.join("rollout-fork.jsonl"),
            &[
                fork_meta.to_string(),
                codex_meta("codex-tui", serde_json::json!("cli")),
                codex_turn("gpt-6-astra"),
                codex_count_at(
                    "2026-10-06T08:00:00.300Z",
                    codex_usage(90_000, 0, 9_000),
                    codex_usage(5_000, 0, 500),
                ),
                codex_count_at(
                    "2026-10-06T08:00:09Z",
                    codex_usage(90_100, 0, 9_010),
                    codex_usage(100, 0, 10),
                ),
            ],
        );
        // A newer rollout: per-response records, then a token_count that
        // repeats them and must be ignored.
        write_lines(
            &day.join("rollout-records.jsonl"),
            &[
                codex_meta("codex_exec", serde_json::json!("exec")),
                codex_turn("gpt-6-luna"),
                codex_record("r1", codex_usage(1_000, 600, 50)),
                codex_record("r1", codex_usage(1_000, 600, 50)),
                codex_record("r2", codex_usage(2_000, 1_500, 70)),
                codex_count(
                    codex_usage(3_000, 2_100, 120),
                    codex_usage(2_000, 1_500, 70),
                ),
            ],
        );
        write_lines(
            &day.join("rollout-guardian.jsonl"),
            &[
                codex_meta(
                    "codex-tui",
                    serde_json::json!({"subagent": {"other": "guardian"}}),
                ),
                codex_turn("codex-auto-review"),
                codex_count(codex_usage(10, 0, 1), codex_usage(10, 0, 1)),
            ],
        );
        let report = scan(&sources, UsageWindow::Days7, &mut UsageCache::default());
        let all = &report.all;
        let astra = model(all, "gpt-6-astra");
        assert_eq!(astra.usage.tokens.output, 10);
        let luna = model(all, "gpt-6-luna");
        assert_eq!(luna.usage.responses, 2);
        assert_eq!(luna.usage.tokens.uncached_input, 400 + 500);
        assert_eq!(luna.usage.tokens.cache_read, 600 + 1_500);
        assert_eq!(report.headless_responses, 3, "exec + guardian");
        assert_eq!(
            report.interactive.totals.responses, 1,
            "thread_spawn stays interactive"
        );
    }

    #[test]
    fn codex_sniff_reads_only_the_line_head() {
        let line = |kind: &str, payload: &str| {
            format!(
                r#"{{"timestamp":"2026-10-06T08:00:00Z","ordinal":7,"type":"{kind}","payload":{payload}}}"#
            )
        };
        let wanted = [
            line("event_msg", r#"{"type":"token_count","info":null}"#),
            line("token_usage_record", r#"{"usage":{}}"#),
            line("turn_context", r#"{"model":"m"}"#),
            line("session_meta", r#"{"id":"t"}"#),
        ];
        for text in &wanted {
            assert_eq!(sniff_codex(text.as_bytes()), CodexSniff::Wanted, "{text}");
        }
        let skipped = [
            line("response_item", r#"{"type":"message"}"#),
            line("event_msg", r#"{"type":"agent_message"}"#),
        ];
        for text in &skipped {
            assert_eq!(sniff_codex(text.as_bytes()), CodexSniff::Skip, "{text}");
        }
        // Payload first (or a type past the sniffed head): parse instead.
        let reordered = r#"{"payload":{"type":"token_count"},"type":"event_msg"}"#;
        assert_eq!(sniff_codex(reordered.as_bytes()), CodexSniff::Unknown);
        let late = format!(r#"{{"blob":"{}","type":"turn_context"}}"#, "x".repeat(400));
        assert_eq!(sniff_codex(late.as_bytes()), CodexSniff::Unknown);
    }

    #[test]
    fn files_last_written_before_the_window_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let sources = temp_sources(dir.path());
        let path = sources.claude_projects.join("-slug").join("old.jsonl");
        write_lines(
            &path,
            &[claude_line("m1", "2026-08-01T10:00:00Z", 10, "cli")],
        );
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_000_000); // 2026-07
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let report = scan(&sources, UsageWindow::Days7, &mut UsageCache::default());
        assert_eq!(report.stats.files_outside_window, 1);
        assert_eq!(report.stats.files_parsed, 0);
    }
}
