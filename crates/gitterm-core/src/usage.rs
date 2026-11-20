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
//!   time; those are skipped (see `FORK_REPLAY_WINDOW` in the scanner). The model comes from
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
//! verified offline are seeded as `None`. `settings.json` may override or
//! add rows under `usage.pricing` (same shape; `null` marks a model
//! unpriced). A model with no priced row is shown as unpriced with its
//! tokens and never contributes a guessed cost.
//!
//! All scanning is blocking file I/O: run it off the UI thread. A
//! `UsageCache` keeps each file's parsed records keyed by (path, mtime,
//! size); a file that only grew is parsed from where the last scan stopped.
//! Reports and pricing are feature-independent; scanning and its cache require
//! `runtime`. The caller supplies transcript roots, calendar date and timezone.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(feature = "runtime")]
use chrono::Local;
use chrono::NaiveDate;
#[cfg(feature = "runtime")]
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::chats::ChatBackend;

#[cfg(feature = "runtime")]
mod scan;
#[cfg(feature = "runtime")]
pub use scan::{scan_usage_in, UsageCache};

/// Scan local harness transcripts for a window ending today in local time.
/// This performs blocking file I/O and should run on a background thread.
#[cfg(feature = "runtime")]
pub fn scan_usage_local(
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

    #[cfg(feature = "runtime")]
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

    /// Rates for a provider with no 1-hour cache tier: 1-hour writes bill
    /// at the plain cache-write rate.
    const fn flat(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        ModelPrice {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: None,
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
/// they show as unpriced until `usage.pricing` in settings.json gives them
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
    // OpenAI standard-tier list prices, collected online 2026-10-08 from
    // OpenAI's GPT-6.1 Sol launch post and two price trackers updated
    // 2026-10-06 (pricepertoken.com, cloudzero.com). Prompts over 272K
    // input tokens reprice the whole request (2x input, 1.5x output); not
    // modelled. OpenAI has no 1-hour cache tier.
    // Sol's cached-input rate is launch pricing, noted as promotional
    // through 2026-11-21; override in settings.json if it changes.
    ("gpt-6.1-sol", Some(ModelPrice::flat(2.0, 10.0, 0.10, 2.5))),
    (
        "gpt-6-luna",
        Some(ModelPrice::flat(0.10, 0.50, 0.01, 0.125)),
    ),
    ("gpt-6-astra", Some(ModelPrice::flat(10.0, 50.0, 1.0, 12.5))),
];

/// The `usage` object in settings.json:
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

#[cfg(feature = "runtime")]
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

/// Where the three harnesses keep their transcripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSources {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub pi_sessions: PathBuf,
}

impl UsageSources {
    pub fn under(home: &Path) -> Self {
        UsageSources {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: home.join(".codex").join("sessions"),
            pi_sessions: home.join(".pi").join("agent").join("sessions"),
        }
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
    #[cfg(feature = "runtime")]
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

/// What a repo row, and a repo filter, is keyed by: the main repo root
/// when resolved, else the recorded cwd; None for records with no cwd.
pub type RepoKey = Option<PathBuf>;

#[derive(Debug, Clone, PartialEq)]
pub struct RepoRow {
    /// Display name: the repo root's (or cwd's) last component.
    pub name: String,
    /// Main repo root when resolved, else the recorded cwd. The row's
    /// [`RepoKey`].
    pub path: RepoKey,
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
    /// The same view narrowed to each repo in `repos`, so a repo filter
    /// needs no rescan. Empty in a narrowed view.
    pub per_repo: HashMap<RepoKey, UsageView>,
}

impl UsageView {
    /// This view narrowed to `repo`, if it has usage in it.
    pub fn for_repo(&self, repo: &RepoKey) -> Option<&UsageView> {
        self.per_repo.get(repo)
    }
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

    /// [`Self::view`] narrowed to `repo` when given: totals, harness split,
    /// days and models count only that repo's responses. A repo with no
    /// usage in this population (say only headless runs, now hidden) gets
    /// an empty view over the window.
    pub fn view_for(&self, include_headless: bool, repo: Option<&RepoKey>) -> Cow<'_, UsageView> {
        let view = self.view(include_headless);
        match repo {
            None => Cow::Borrowed(view),
            Some(repo) => match view.for_repo(repo) {
                Some(narrowed) => Cow::Borrowed(narrowed),
                None => Cow::Owned(ViewBuilder::new(self.start, self.end).finish()),
            },
        }
    }
}

struct ViewBuilder {
    #[cfg(feature = "runtime")]
    start: NaiveDate,
    #[cfg(feature = "runtime")]
    end: NaiveDate,
    totals: Bucket,
    by_harness: [Bucket; 3],
    days: Vec<DayRow>,
    models: HashMap<(ChatBackend, String), (bool, Bucket)>,
    repos: HashMap<RepoKey, (String, bool, Bucket)>,
    /// One builder per repo, fed the same records (top level only).
    per_repo: HashMap<RepoKey, ViewBuilder>,
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
            #[cfg(feature = "runtime")]
            start,
            #[cfg(feature = "runtime")]
            end,
            totals: Bucket::default(),
            by_harness: [Bucket::default(); 3],
            days,
            models: HashMap::new(),
            repos: HashMap::new(),
            per_repo: HashMap::new(),
        }
    }

    #[cfg(feature = "runtime")]
    fn add(
        &mut self,
        backend: ChatBackend,
        date: NaiveDate,
        record: &UsageRecord,
        price: Option<ModelPrice>,
        repo: &(String, RepoKey, bool),
    ) {
        self.add_flat(backend, date, record, price, repo);
        let (start, end) = (self.start, self.end);
        self.per_repo
            .entry(repo.1.clone())
            .or_insert_with(|| ViewBuilder::new(start, end))
            .add_flat(backend, date, record, price, repo);
    }

    #[cfg(feature = "runtime")]
    fn add_flat(
        &mut self,
        backend: ChatBackend,
        date: NaiveDate,
        record: &UsageRecord,
        price: Option<ModelPrice>,
        repo: &(String, RepoKey, bool),
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
        let per_repo = self
            .per_repo
            .into_iter()
            .map(|(key, builder)| (key, builder.finish()))
            .collect();
        UsageView {
            totals: self.totals,
            by_harness: self.by_harness,
            days: self.days,
            models,
            repos,
            per_repo,
        }
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

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
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

    #[test]
    fn formats_are_compact() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(4_500_000), "4.5M");
        assert_eq!(format_cost(0.4234), "$0.42");
        assert_eq!(format_cost(1234.4), "$1,234");
    }
}

#[cfg(all(test, feature = "runtime"))]
#[path = "../../../tests/support/usage_parity.rs"]
mod parity;
