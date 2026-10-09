//! Usage panel (TRU-145): token usage and cost across Claude Code, Codex
//! and Pi, computed locally from the transcript files the Chats index
//! already scans. No network, no account APIs: cost is an estimate "if
//! billed at full API rate" from the pricing table below.
//!
//! Sources, as verified against this machine's transcripts (2026-10-08):
//!
//! - **Claude** (`~/.claude/projects/<slug>/<session>.jsonl`, plus Task
//!   subagents in `<slug>/<session>/subagents/agent-*.jsonl`): each
//!   `type: "assistant"` line carries `message.{id, model, usage}` and a
//!   top-level `timestamp`, `cwd` and `entrypoint`. `usage` has
//!   `input_tokens` (uncached), `cache_read_input_tokens`,
//!   `cache_creation_input_tokens` (split by TTL in
//!   `cache_creation.{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`)
//!   and `output_tokens`. One API message is written as several lines (one
//!   per content block, sharing `message.id`), and the usage can differ
//!   between them, so records are keyed by `message.id` and the last line
//!   wins. The same id can also appear in another file (a resumed or forked
//!   session copies history), so ids are deduped across files too.
//!   `model: "<synthetic>"` lines are harness-made error placeholders with
//!   zero usage and are dropped.
//! - **Codex** (`~/.codex/sessions/**/rollout-*.jsonl`): newer rollouts
//!   write one `type: "token_usage_record"` line per API response with
//!   `payload.{response_id, usage}`, `usage` being per response
//!   (`input_tokens` includes `cached_input_tokens` and
//!   `cache_write_input_tokens`; `output_tokens` includes
//!   `reasoning_output_tokens`). Older rollouts only have
//!   `event_msg`/`token_count` events whose `info.total_token_usage` is
//!   cumulative for the thread and `info.last_token_usage` is the last
//!   response's, and the same event is often written twice. So a token_count
//!   counts its `last_token_usage` only when `total_token_usage` differs
//!   from the previous event's, and once a file has token_usage_record
//!   lines its later token_count events are ignored. A forked rollout
//!   (`session_meta.payload.forked_from_id`) begins by replaying its
//!   parent's history, token_count events included, stamped at the fork
//!   time; those are skipped (see [`FORK_REPLAY_WINDOW`]). The model comes from
//!   the latest `turn_context.payload.model` (session_meta has none), the
//!   cwd from the first `session_meta.payload.cwd`.
//! - **Pi** (`~/.pi/agent/sessions/<slug>/*.jsonl`): `type: "message"`
//!   lines whose `message.role` is `assistant` carry `message.model` and
//!   `message.usage.{input, output, cacheRead, cacheWrite}` (input is
//!   uncached) per turn; the `type: "session"` line has the cwd.
//!
//! Headless runs count toward spend but are tagged so the panel can hide
//! them: Claude lines whose `entrypoint` starts with `sdk` other than
//! `sdk-ts` (GitTerm's own chat tabs), and Codex `exec` rollouts and
//! harness subagents (approval guardian, review) — Codex `thread_spawn`
//! subagents are part of a human session and stay interactive, as Claude's
//! Task subagents do. The Chats index skips all of these; this scanner
//! deliberately does not.
//!
//! Pricing policy: [`SEED_PRICES`] holds per-million list rates that were
//! verified at the time of writing; models whose rates could not be
//! verified offline are seeded as `None`. `config.json` may override or
//! add rows under `usage.pricing` (same shape; `null` marks a model
//! unpriced). A model with no priced row is shown as unpriced with its
//! tokens and never contributes a guessed cost.
//!
//! All scanning is blocking file I/O: run it off the UI thread. A
//! [`UsageCache`] keeps each file's parsed records keyed by (path, mtime,
//! size); a file that only grew is parsed from where the last scan stopped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chats::{self, ChatBackend};

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

/// The panel's time window, ending today (local time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UsageWindow {
    Days7,
    #[default]
    Days30,
    Days90,
}

impl UsageWindow {
    pub const ALL: [UsageWindow; 3] =
        [UsageWindow::Days7, UsageWindow::Days30, UsageWindow::Days90];

    pub fn days(self) -> u32 {
        match self {
            UsageWindow::Days7 => 7,
            UsageWindow::Days30 => 30,
            UsageWindow::Days90 => 90,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UsageWindow::Days7 => "7d",
            UsageWindow::Days30 => "30d",
            UsageWindow::Days90 => "90d",
        }
    }

    /// First day of the window that ends on `today` (inclusive).
    pub fn start(self, today: NaiveDate) -> NaiveDate {
        today - chrono::Days::new(u64::from(self.days() - 1))
    }
}

/// Token counts by billing category. `cache_write_1h` is the part of
/// `cache_write` written with the one-hour TTL (Claude only), which bills
/// at a higher rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_1h: u64,
    pub output: u64,
}

impl Tokens {
    /// Every token the model processed: input in all three forms plus
    /// output.
    pub fn processed(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write + self.output
    }

    /// Input in all forms (uncached, cache read, cache write).
    pub fn input(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write
    }

    fn add(&mut self, other: &Tokens) {
        self.uncached_input += other.uncached_input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cache_write_1h += other.cache_write_1h;
        self.output += other.output;
    }
}

/// Per-million-token list rates in USD.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// Cache writes (Claude: the five-minute TTL rate).
    pub cache_write: f64,
    /// One-hour-TTL cache writes. When absent, those tokens bill at
    /// `cache_write`; only Claude reports them separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
}

impl ModelPrice {
    const fn new(
        input: f64,
        output: f64,
        cache_read: f64,
        cache_write: f64,
        cache_write_1h: f64,
    ) -> Self {
        ModelPrice {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: Some(cache_write_1h),
        }
    }

    /// USD for `tokens` at these rates.
    pub fn cost(&self, tokens: &Tokens) -> f64 {
        let one_hour = tokens.cache_write_1h.min(tokens.cache_write);
        let five_minute = tokens.cache_write - one_hour;
        let per_million = tokens.uncached_input as f64 * self.input
            + tokens.cache_read as f64 * self.cache_read
            + five_minute as f64 * self.cache_write
            + one_hour as f64 * self.cache_write_1h.unwrap_or(self.cache_write)
            + tokens.output as f64 * self.output;
        per_million / 1_000_000.0
    }
}

/// Built-in rates, per million tokens. Anthropic rows are the first-party
/// API list prices as published on 2026-10-06 (input, output, cache read,
/// 5-minute cache write = 1.25x input, 1-hour cache write = 2x input).
/// `None` rows are models whose list price could not be verified offline:
/// they show as unpriced until `usage.pricing` in config.json gives them
/// a rate.
pub const SEED_PRICES: &[(&str, Option<ModelPrice>)] = &[
    (
        "claude-fable-5-1",
        Some(ModelPrice::new(10.0, 50.0, 0.25, 12.5, 20.0)),
    ),
    (
        "claude-fable-5",
        Some(ModelPrice::new(10.0, 50.0, 1.0, 12.5, 20.0)),
    ),
    (
        "claude-opus-5-5",
        Some(ModelPrice::new(4.0, 20.0, 0.2, 5.0, 8.0)),
    ),
    (
        "claude-opus-5",
        Some(ModelPrice::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ),
    (
        "claude-opus-4-8",
        Some(ModelPrice::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ),
    (
        "claude-opus-4-7",
        Some(ModelPrice::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ),
    (
        "claude-opus-4-6",
        Some(ModelPrice::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ),
    (
        "claude-sonnet-5-5",
        Some(ModelPrice::new(2.0, 10.0, 0.2, 2.5, 4.0)),
    ),
    (
        "claude-sonnet-5",
        Some(ModelPrice::new(2.0, 10.0, 0.2, 2.5, 4.0)),
    ),
    (
        "claude-sonnet-4-6",
        Some(ModelPrice::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ),
    (
        "claude-haiku-4-5",
        Some(ModelPrice::new(1.0, 5.0, 0.1, 1.25, 2.0)),
    ),
    // Haiku 5.5 bills $0.10 / $0.50 up to a 100K-token prompt and
    // $0.50 / $2.50 above it; its cache-read rate is not published per
    // model, and a single rate per category cannot express the tier.
    ("claude-haiku-5-5", None),
    // OpenAI list prices for these were not verifiable offline.
    ("gpt-6.1-sol", None),
    ("gpt-6-luna", None),
    ("gpt-6-astra", None),
];

/// The `usage` object in config.json:
///
/// ```json
/// "usage": { "pricing": {
///   "gpt-6-astra": { "input": 1.0, "output": 8.0, "cache_read": 0.1, "cache_write": 0.0 },
///   "claude-haiku-5-5": null
/// } }
/// ```
///
/// Rows replace the built-in row for the same model id; `null` marks a
/// model unpriced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageConfig {
    #[serde(default)]
    pub pricing: BTreeMap<String, Option<ModelPrice>>,
}

/// The rates in effect: [`SEED_PRICES`] with the config overrides on top.
#[derive(Debug, Clone, PartialEq)]
pub struct Pricing {
    rows: HashMap<String, Option<ModelPrice>>,
}

impl Pricing {
    pub fn seeded() -> Self {
        Pricing {
            rows: SEED_PRICES
                .iter()
                .map(|(model, price)| (model.to_string(), *price))
                .collect(),
        }
    }

    pub fn with_overrides(config: &UsageConfig) -> Self {
        let mut pricing = Self::seeded();
        for (model, price) in &config.pricing {
            pricing.rows.insert(model.clone(), *price);
        }
        pricing
    }

    /// The rate for `model`: its own row, else the row for the id without
    /// a trailing `-YYYYMMDD` snapshot date. None = unpriced.
    pub fn price_for(&self, model: &str) -> Option<ModelPrice> {
        if let Some(row) = self.rows.get(model) {
            return *row;
        }
        let (base, date) = model.rsplit_once('-')?;
        if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
            return self.rows.get(base).copied().flatten();
        }
        None
    }
}

/// One API response's usage.
#[derive(Debug, Clone, PartialEq)]
struct UsageRecord {
    at: DateTime<Utc>,
    model: String,
    tokens: Tokens,
    headless: bool,
    /// Identity across files (Claude message id, Codex response id); a
    /// key seen in an earlier file is not counted again.
    key: Option<String>,
}

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

/// Where the three harnesses keep their transcripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSources {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub pi_sessions: PathBuf,
}

impl UsageSources {
    /// This machine's harness directories. None when the home directory
    /// cannot be determined.
    pub fn local() -> Option<Self> {
        let home = dirs::home_dir()?;
        Some(Self::under(&home))
    }

    pub fn under(home: &Path) -> Self {
        UsageSources {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: home.join(".codex").join("sessions"),
            pi_sessions: home.join(".pi").join("agent").join("sessions"),
        }
    }

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
                chats::jsonl_files_under(root, depth)
                    .into_iter()
                    .map(|path| (path, backend)),
            );
        }
        out
    }
}

/// Totals for one slice of the report.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bucket {
    pub tokens: Tokens,
    /// USD at list rates, over priced models only.
    pub cost: f64,
    /// Processed tokens of models with no price.
    pub unpriced_tokens: u64,
    /// API responses counted.
    pub responses: u64,
}

impl Bucket {
    fn add(&mut self, tokens: &Tokens, price: Option<ModelPrice>) {
        self.tokens.add(tokens);
        match price {
            Some(price) => self.cost += price.cost(tokens),
            None => self.unpriced_tokens += tokens.processed(),
        }
        self.responses += 1;
    }
}

/// Index of a backend in [`ChatBackend::ALL`], for per-harness arrays.
pub fn harness_index(backend: ChatBackend) -> usize {
    match backend {
        ChatBackend::Claude => 0,
        ChatBackend::Codex => 1,
        ChatBackend::Pi => 2,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DayRow {
    pub date: NaiveDate,
    /// Indexed by [`harness_index`].
    pub by_harness: [Bucket; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub model: String,
    pub harness: ChatBackend,
    pub priced: bool,
    pub usage: Bucket,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoRow {
    /// Display name: the repo root's (or cwd's) last component.
    pub name: String,
    /// Main repo root when resolved, else the recorded cwd.
    pub path: Option<PathBuf>,
    /// False when the cwd is gone or not in a git repo.
    pub resolved: bool,
    pub usage: Bucket,
}

/// The report for one population (with or without headless runs).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageView {
    pub totals: Bucket,
    /// Indexed by [`harness_index`].
    pub by_harness: [Bucket; 3],
    /// One row per day of the window, oldest first.
    pub days: Vec<DayRow>,
    /// Costliest first, then by tokens.
    pub models: Vec<ModelRow>,
    /// Costliest first, then by tokens.
    pub repos: Vec<RepoRow>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanStats {
    pub files_seen: usize,
    /// Files skipped because their mtime predates the window.
    pub files_outside_window: usize,
    pub files_parsed: usize,
    /// Files that had grown and were parsed from their previous end.
    pub files_resumed: usize,
    pub files_cached: usize,
    pub files_unreadable: usize,
    /// Responses counted in the window (both populations).
    pub responses: u64,
    /// Responses dropped because another file already counted them.
    pub duplicates: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageReport {
    pub window: UsageWindow,
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// Everything, headless runs included.
    pub all: UsageView,
    /// Without headless runs.
    pub interactive: UsageView,
    pub headless_responses: u64,
    pub stats: ScanStats,
}

impl UsageReport {
    pub fn view(&self, include_headless: bool) -> &UsageView {
        if include_headless {
            &self.all
        } else {
            &self.interactive
        }
    }
}

/// Scan this machine's transcripts for `window` (ending today, local
/// time). Blocking; run on a background thread.
pub fn scan_usage(
    sources: &UsageSources,
    window: UsageWindow,
    pricing: &Pricing,
    cache: &mut UsageCache,
) -> UsageReport {
    scan_usage_in(
        sources,
        window,
        Local::now().date_naive(),
        &Local,
        pricing,
        cache,
    )
}

/// [`scan_usage`] with the calendar pinned: `today` and the time zone the
/// day buckets use.
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
    chats::resolve_repo_root(cwd).map(|(root, _)| root)
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

struct ViewBuilder {
    start: NaiveDate,
    totals: Bucket,
    by_harness: [Bucket; 3],
    days: Vec<DayRow>,
    models: HashMap<(ChatBackend, String), (bool, Bucket)>,
    repos: HashMap<Option<PathBuf>, (String, bool, Bucket)>,
}

impl ViewBuilder {
    fn new(start: NaiveDate, end: NaiveDate) -> Self {
        let days = start
            .iter_days()
            .take_while(|date| *date <= end)
            .map(|date| DayRow {
                date,
                by_harness: [Bucket::default(); 3],
            })
            .collect();
        ViewBuilder {
            start,
            totals: Bucket::default(),
            by_harness: [Bucket::default(); 3],
            days,
            models: HashMap::new(),
            repos: HashMap::new(),
        }
    }

    fn add(
        &mut self,
        backend: ChatBackend,
        date: NaiveDate,
        record: &UsageRecord,
        price: Option<ModelPrice>,
        repo: &(String, Option<PathBuf>, bool),
    ) {
        let harness = harness_index(backend);
        self.totals.add(&record.tokens, price);
        self.by_harness[harness].add(&record.tokens, price);
        let day = (date - self.start).num_days() as usize;
        self.days[day].by_harness[harness].add(&record.tokens, price);
        self.models
            .entry((backend, record.model.clone()))
            .or_insert_with(|| (price.is_some(), Bucket::default()))
            .1
            .add(&record.tokens, price);
        self.repos
            .entry(repo.1.clone())
            .or_insert_with(|| (repo.0.clone(), repo.2, Bucket::default()))
            .2
            .add(&record.tokens, price);
    }

    fn finish(self) -> UsageView {
        let by_spend = |a: &Bucket, b: &Bucket| {
            b.cost
                .total_cmp(&a.cost)
                .then(b.tokens.processed().cmp(&a.tokens.processed()))
        };
        let mut models: Vec<ModelRow> = self
            .models
            .into_iter()
            .map(|((harness, model), (priced, usage))| ModelRow {
                model,
                harness,
                priced,
                usage,
            })
            .collect();
        models.sort_by(|a, b| by_spend(&a.usage, &b.usage).then(a.model.cmp(&b.model)));
        let mut repos: Vec<RepoRow> = self
            .repos
            .into_iter()
            .map(|(path, (name, resolved, usage))| RepoRow {
                name,
                path,
                resolved,
                usage,
            })
            .collect();
        repos.sort_by(|a, b| by_spend(&a.usage, &b.usage).then(a.name.cmp(&b.name)));
        UsageView {
            totals: self.totals,
            by_harness: self.by_harness,
            days: self.days,
            models,
            repos,
        }
    }
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

/// Compact token count for tables ("950", "12.3K", "4.5M", "1.2B").
pub fn format_tokens(count: u64) -> String {
    let n = count as f64;
    if n >= 1e9 {
        format!("{:.1}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        count.to_string()
    }
}

/// Dollars for tables ("$0.42", "$12.30", "$1,234").
pub fn format_cost(usd: f64) -> String {
    if usd >= 1000.0 {
        let whole = usd.round() as u64;
        let digits = whole.to_string();
        let mut out = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        format!("${out}")
    } else {
        format!("${usd:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/usage");

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
        // C (headless) 0.003; Pi a1 0.0048. Codex and the unknown Claude
        // model are unpriced.
        assert!(close(all.totals.cost, 0.11254), "{}", all.totals.cost);
        assert_eq!(all.totals.responses, 8);
        assert_eq!(all.totals.unpriced_tokens, 100 + 1100 + 2200 + 330);
        assert_eq!(report.headless_responses, 1);
        assert!(close(report.interactive.totals.cost, 0.10954));
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
        assert!(!sol.priced);

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
        assert_eq!(names, ["alpha", "gamma", "beta"], "costliest first");
        assert!(all.repos.iter().all(|row| !row.resolved));
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
        assert_eq!(pricing.price_for("gpt-6-luna"), None, "seeded unpriced");
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

    #[test]
    fn one_hour_cache_writes_bill_at_their_own_rate() {
        let price = ModelPrice::new(4.0, 20.0, 0.2, 5.0, 8.0);
        let tokens = Tokens {
            cache_write: 1_000_000,
            cache_write_1h: 400_000,
            ..Tokens::default()
        };
        assert!(close(price.cost(&tokens), 0.6 * 5.0 + 0.4 * 8.0));
        let no_1h = ModelPrice {
            cache_write_1h: None,
            ..price
        };
        assert!(close(no_1h.cost(&tokens), 5.0));
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

    #[test]
    fn formats_are_compact() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(4_500_000), "4.5M");
        assert_eq!(format_cost(0.4234), "$0.42");
        assert_eq!(format_cost(1234.4), "$1,234");
    }

    #[test]
    #[ignore = "scans every transcript on this machine; run manually with --ignored --nocapture"]
    fn real_usage_90_day_scan() {
        let sources = UsageSources::local().expect("home dir");
        let pricing = Pricing::seeded();
        let mut cache = UsageCache::default();
        let cold = scan_usage(&sources, UsageWindow::Days90, &pricing, &mut cache);
        let warm = scan_usage(&sources, UsageWindow::Days90, &pricing, &mut cache);
        let print = |label: &str, report: &UsageReport| {
            let s = &report.stats;
            // Counts, totals and timings only: never transcript contents.
            eprintln!(
                "{label}: {} files ({} outside window, {} parsed, {} resumed, {} cached, {} unreadable), \
                 {} responses ({} duplicates, {} headless) in {:?}",
                s.files_seen,
                s.files_outside_window,
                s.files_parsed,
                s.files_resumed,
                s.files_cached,
                s.files_unreadable,
                s.responses,
                s.duplicates,
                report.headless_responses,
                s.elapsed,
            );
        };
        print("cold", &cold);
        print("warm", &warm);
        let all = &cold.all;
        eprintln!(
            "90d: {} est. at list rates, {} processed, {} unpriced tokens",
            format_cost(all.totals.cost),
            format_tokens(all.totals.tokens.processed()),
            format_tokens(all.totals.unpriced_tokens),
        );
        for backend in ChatBackend::ALL {
            let bucket = &all.by_harness[harness_index(backend)];
            eprintln!(
                "  {:<7} {:>10} {:>8} responses {:>8} processed",
                backend.label(),
                format_cost(bucket.cost),
                bucket.responses,
                format_tokens(bucket.tokens.processed())
            );
        }
        for row in &all.models {
            eprintln!(
                "  model {:<28} {:<6} {:>10} {:>8}",
                row.model,
                row.harness.label(),
                if row.priced {
                    format_cost(row.usage.cost)
                } else {
                    "unpriced".to_string()
                },
                format_tokens(row.usage.tokens.processed())
            );
        }
        eprintln!("  {} repos", all.repos.len());
        assert_eq!(warm.stats.files_parsed, 0);
    }
}
