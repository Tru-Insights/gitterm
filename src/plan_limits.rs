//! Plan limits (TRU-211): the Claude Code and Codex subscription windows
//! that `/usage` shows (5-hour and 7-day use, and when each resets), read
//! from data already on this machine. No network, no account APIs.
//!
//! Sources:
//!
//! - **Claude**: the `rate_limit_event` frames of a chat's stream-json
//!   output, `rate_limit_info.unifiedWindows.{five_hour, seven_day}` with
//!   `utilization` a 0–1 fraction and `resetsAt` Unix seconds, plus
//!   `status` (`allowed` when under the limit) and `isUsingOverage`. The
//!   parser hands the turn's last frame out on `TurnCompleted.rate_limit`
//!   ([`crate::harness::RateLimitReading`]); the latest from any chat wins.
//! - **Codex**: rollouts (`~/.codex/sessions/**/rollout-*.jsonl`) write
//!   `payload.rate_limits` on `event_msg` / `token_count` lines:
//!   `{limit_id, primary, secondary, credits, plan_type,
//!   rate_limit_reached_type}`, each window `{used_percent (0–100),
//!   window_minutes, resets_at (Unix seconds)}`. The newest rollout's last
//!   such line is current. Lines whose `limit_id` is not `codex` are a
//!   single model's own bucket (`codex_bengalfox`, "GPT-5.3-Codex-Spark")
//!   and are skipped, so they never stand in for the account's limit.
//!
//! Every field is optional on read: a window that is missing is unknown,
//! never 0%. Each reading keeps the time it was observed, and is shown as
//! current only for [`STALE_AFTER_SECS`]; after that it is "as of" that
//! time. A window whose reset time has passed has no meaningful % at all.
//! The last reading of each provider is kept in [`LIMITS_FILE`] in the
//! config dir, so the rail badge is not blank after a restart.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A reading older than this is shown "as of" its time, not as current.
pub const STALE_AFTER_SECS: i64 = 15 * 60;
pub const HISTORY_SAMPLE_SECS: i64 = 15 * 60;
pub const HISTORY_RETENTION_SECS: i64 = 8 * 24 * 60 * 60;
/// A window at or above this % is drawn in the warning colour.
pub const WARNING_PERCENT: f64 = 75.0;
/// A window at or above this % is drawn in the danger colour.
pub const DANGER_PERCENT: f64 = 90.0;
pub const WEEK_SECS: i64 = 168 * 3600;
pub const BURST_SECS: i64 = 5 * 3600;
pub const ELAPSED_FLOOR: f64 = 0.10;
pub const TARGET_PERCENT: f64 = 90.0;
pub const HOT_PACE: f64 = 100.0;
pub const HOT_AHEAD: f64 = 5.0;
pub const RECOVERED_PACE: f64 = 75.0;
pub const ALMOST_OUT_PERCENT: f64 = 95.0;
pub const STATE_DWELL_SECS: i64 = 6 * 3600;
pub const BUSY_PERCENT: f64 = 80.0;
pub const BUSY_CLEAR_PERCENT: f64 = 60.0;
pub const BURST_LIMIT_PERCENT: f64 = 100.0;
pub const BUSY_MIN_LEFT_SECS: i64 = 30 * 60;
pub const URGENT_RUNOUT_DAYS: f64 = 1.0;
pub const BALANCE_MARGIN: f64 = 10.0;
/// The last reading of each provider, in the V6 config dir.
pub const LIMITS_FILE: &str = "plan-limits.json";
/// Newest rollouts looked at for a `rate_limits` line before giving up.
const CODEX_MAX_FILES: usize = 8;
/// First tail read of a rollout; grown fourfold until a line is found.
const TAIL_START_BYTES: u64 = 256 * 1024;
/// Maximum bytes inspected at the end of one rollout.
const TAIL_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// The account-wide Codex limit; other ids are per-model buckets.
const CODEX_LIMIT_ID: &str = "codex";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
        }
    }
}

/// One usage window ("5h", "7d").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub label: String,
    /// 0–100 (may exceed 100 on overage). None: not reported.
    #[serde(default)]
    pub used_percent: Option<f64>,
    /// Unix seconds.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

impl LimitWindow {
    /// The % that still means something at `now`: None when unknown or
    /// once the window has reset since the reading.
    pub fn percent_at(&self, now: i64) -> Option<f64> {
        if self.has_reset(now) {
            return None;
        }
        self.used_percent
    }

    pub fn has_reset(&self, now: i64) -> bool {
        self.resets_at.is_some_and(|at| at <= now)
    }
}

/// A limit reached or overage in use: shown in words, in the danger
/// colour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertKind {
    LimitReached {
        #[serde(default)]
        which: Option<String>,
    },
    Overage,
    /// Claude `status` other than `allowed` or `rejected`, word for word.
    Status {
        status: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    #[serde(flatten)]
    pub kind: AlertKind,
    /// Unix seconds it holds until (the window's reset). None: it holds
    /// only while the reading is current.
    #[serde(default)]
    pub until: Option<i64>,
    /// Normalized duration label, independent of provider slot names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

impl Alert {
    pub fn text(&self) -> String {
        match &self.kind {
            AlertKind::LimitReached { which: Some(which) } => format!("Limit reached ({which})"),
            AlertKind::LimitReached { which: None } => "Limit reached".to_string(),
            AlertKind::Overage => "Using overage".to_string(),
            AlertKind::Status { status } => format!("Limit status: {status}"),
        }
    }

    /// The badge's word for it.
    pub fn short(&self) -> &'static str {
        match self.kind {
            AlertKind::Overage => "OVER",
            AlertKind::LimitReached { .. } | AlertKind::Status { .. } => "LIMIT",
        }
    }
}

/// One provider's reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLimits {
    pub provider: Provider,
    /// Unix seconds: when the Claude frame arrived, or the Codex line's
    /// timestamp (its file's mtime when the line has none).
    pub observed_at: i64,
    #[serde(default)]
    pub windows: Vec<LimitWindow>,
    #[serde(default)]
    pub alerts: Vec<Alert>,
    /// Codex `plan_type` ("pro").
    #[serde(default)]
    pub plan_type: Option<String>,
    /// Codex credits, as shown ("62,500", "unlimited", "none").
    #[serde(default)]
    pub credits: Option<String>,
    /// Legacy/remote Claude observations have unknown account provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSample {
    pub observed_at: i64,
    pub used_percent: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowHistory {
    pub provider: Provider,
    pub label: String,
    pub resets_at: Option<i64>,
    #[serde(default)]
    pub samples: Vec<UsageSample>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPin {
    pub provider: Provider,
    pub until: i64,
}

impl PlanLimits {
    pub fn is_stale(&self, now: i64) -> bool {
        is_stale(self.observed_at, now)
    }

    /// The highest window % that still means something at `now`.
    pub fn peak_percent(&self, now: i64) -> Option<f64> {
        self.windows
            .iter()
            .filter_map(|w| w.percent_at(now))
            .fold(None, |peak: Option<f64>, p| {
                Some(peak.map_or(p, |q| q.max(p)))
            })
    }

    /// The alerts that still hold at `now`.
    pub fn active_alerts(&self, now: i64) -> Vec<&Alert> {
        let current = !self.is_stale(now);
        self.alerts
            .iter()
            .filter(|alert| match alert.until {
                Some(until) => now < until,
                None => current,
            })
            .collect()
    }
}

pub fn is_stale(observed_at: i64, now: i64) -> bool {
    now - observed_at > STALE_AFTER_SECS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warning,
    Danger,
}

pub fn level(percent: f64) -> Level {
    if percent >= DANGER_PERCENT {
        Level::Danger
    } else if percent >= WARNING_PERCENT {
        Level::Warning
    } else {
        Level::Normal
    }
}

// ---------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------

/// The windows `/usage` shows first, with their labels; always listed,
/// unknown when the frame lacks them.
const CLAUDE_WINDOWS: [(&str, &str); 2] = [("five_hour", "5h"), ("seven_day", "7d")];

/// A `rate_limit_info` object read tolerantly: any field may be missing.
pub fn claude_limits(info: &Value, observed_at: i64) -> PlanLimits {
    let unified = info.get("unifiedWindows").and_then(Value::as_object);
    let window = |key: &str, label: String| {
        let raw = unified.and_then(|windows| windows.get(key));
        LimitWindow {
            label,
            used_percent: raw
                .and_then(|w| w.get("utilization"))
                .and_then(Value::as_f64)
                .map(|fraction| fraction * 100.0),
            resets_at: raw.and_then(|w| w.get("resetsAt")).and_then(Value::as_i64),
        }
    };
    let mut windows: Vec<LimitWindow> = CLAUDE_WINDOWS
        .iter()
        .map(|(key, label)| window(key, label.to_string()))
        .collect();
    if let Some(unified) = unified {
        for key in unified.keys() {
            if CLAUDE_WINDOWS.iter().any(|(known, _)| known == key) {
                continue;
            }
            let label = match key.strip_prefix("seven_day_") {
                Some(model) => format!("7d {model}"),
                None => key.clone(),
            };
            windows.push(window(key, label));
        }
    }
    let until = info.get("resetsAt").and_then(Value::as_i64);
    let alert_window = info
        .get("rateLimitType")
        .and_then(Value::as_str)
        .map(claude_window_label);
    let mut alerts = Vec::new();
    match info.get("status").and_then(Value::as_str) {
        None | Some("allowed") => {}
        Some("rejected") => alerts.push(Alert {
            window: alert_window.clone(),
            kind: AlertKind::LimitReached {
                which: info
                    .get("rateLimitType")
                    .and_then(Value::as_str)
                    .map(claude_window_label),
            },
            until,
        }),
        Some(status) => alerts.push(Alert {
            window: alert_window.clone(),
            kind: AlertKind::Status {
                status: status.to_string(),
            },
            until,
        }),
    }
    if info.get("isUsingOverage").and_then(Value::as_bool) == Some(true) {
        alerts.push(Alert {
            window: alert_window,
            kind: AlertKind::Overage,
            until,
        });
    }
    PlanLimits {
        provider: Provider::Claude,
        observed_at,
        windows,
        alerts,
        plan_type: None,
        credits: None,
        local: None,
    }
}

fn claude_window_label(key: &str) -> String {
    CLAUDE_WINDOWS
        .iter()
        .find(|(known, _)| *known == key)
        .map(|(_, label)| label.to_string())
        .unwrap_or_else(|| key.to_string())
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// "7d" for 10080 minutes, "5h" for 300, else whole hours or minutes.
pub fn window_label(minutes: u64) -> String {
    match minutes {
        10080 => "7d".to_string(),
        m if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// A `rate_limits` object, or None when it is not one or is a per-model
/// bucket rather than the account's limit.
pub fn codex_limits(rate_limits: &Value, observed_at: i64) -> Option<PlanLimits> {
    let object = rate_limits.as_object()?;
    if object.get("limit_id").and_then(Value::as_str) != Some(CODEX_LIMIT_ID) {
        return None;
    }
    let mut windows = Vec::new();
    for (key, fallback) in [("primary", "primary"), ("secondary", "secondary")] {
        let Some(raw) = object.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        windows.push(LimitWindow {
            label: raw
                .get("window_minutes")
                .and_then(Value::as_u64)
                .map(window_label)
                .unwrap_or_else(|| fallback.to_string()),
            used_percent: raw.get("used_percent").and_then(Value::as_f64),
            resets_at: raw.get("resets_at").and_then(Value::as_i64),
        });
    }
    let mut alerts = Vec::new();
    if let Some(reached) = object
        .get("rate_limit_reached_type")
        .and_then(Value::as_str)
    {
        alerts.push(Alert {
            window: object
                .get(reached)
                .and_then(|raw| raw.get("window_minutes"))
                .and_then(Value::as_u64)
                .map(window_label),
            kind: AlertKind::LimitReached {
                which: Some(reached.to_string()),
            },
            until: match reached {
                "primary" => object
                    .get("primary")
                    .and_then(|window| window.get("resets_at"))
                    .and_then(Value::as_i64),
                "secondary" => object
                    .get("secondary")
                    .and_then(|window| window.get("resets_at"))
                    .and_then(Value::as_i64),
                _ => None,
            },
        });
    }
    Some(PlanLimits {
        local: Some(true),
        provider: Provider::Codex,
        observed_at,
        windows,
        alerts,
        plan_type: object
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        credits: object.get("credits").and_then(codex_credits),
    })
}

fn codex_credits(credits: &Value) -> Option<String> {
    if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return Some("unlimited".to_string());
    }
    if credits.get("has_credits").and_then(Value::as_bool) == Some(false) {
        return Some("none".to_string());
    }
    let balance = credits.get("balance")?;
    let amount = balance
        .as_f64()
        .or_else(|| balance.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    Some(format_amount(amount))
}

/// "62,500" for whole amounts, two decimals otherwise.
fn format_amount(amount: f64) -> String {
    if amount.fract() != 0.0 {
        return format!("{amount:.2}");
    }
    let digits = format!("{:.0}", amount.abs());
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if amount < 0.0 {
        out.insert(0, '-');
    }
    out
}

/// The account-wide limits a rollout line carries, if it carries them.
fn codex_line_limits(line: &[u8], file_mtime: i64) -> Option<PlanLimits> {
    const MARKER: &[u8] = b"\"rate_limits\":{";
    if !line.windows(MARKER.len()).any(|w| w == MARKER) {
        return None;
    }
    let value: Value = serde_json::from_slice(line).ok()?;
    let rate_limits = value.pointer("/payload/rate_limits")?;
    let observed_at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|at| at.timestamp())
        .unwrap_or(file_mtime);
    codex_limits(rate_limits, observed_at)
}

/// The last account-wide `rate_limits` line of one rollout, read from the
/// end so a long session's file is not read whole.
pub fn last_codex_limits_in(path: &Path) -> std::io::Result<Option<PlanLimits>> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let len = meta.len();
    let mtime = meta.modified().map(unix_secs).unwrap_or(0);
    let mut window = TAIL_START_BYTES.min(len);
    loop {
        let start = len - window;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity(window as usize);
        (&mut file).take(window).read_to_end(&mut buf)?;
        let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
        if start > 0 && !lines.is_empty() {
            // Cut mid-line; it is read whole by the next, larger window.
            lines.remove(0);
        }
        if let Some(limits) = lines
            .iter()
            .rev()
            .find_map(|line| codex_line_limits(line, mtime))
        {
            return Ok(Some(limits));
        }
        if start == 0 || window >= TAIL_MAX_BYTES {
            return Ok(None);
        }
        window = window.saturating_mul(4).min(TAIL_MAX_BYTES).min(len);
    }
}

/// The current Codex limits: the newest observation in no more than the
/// newest [`CODEX_MAX_FILES`] rollouts (by mtime). Blocking; run it off the
/// UI thread. A rollout that cannot be read is logged and skipped.
pub fn latest_codex_limits(sessions_root: &Path) -> Option<PlanLimits> {
    let mut rollouts: Vec<(SystemTime, PathBuf)> =
        crate::chats::jsonl_files_under(sessions_root, 3)
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
            })
            .filter_map(|path| {
                let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
                Some((modified, path))
            })
            .collect();
    rollouts.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let mut newest: Option<PlanLimits> = None;
    for (_, path) in rollouts.into_iter().take(CODEX_MAX_FILES) {
        match last_codex_limits_in(&path) {
            Ok(Some(limits)) => {
                if newest
                    .as_ref()
                    .is_none_or(|current| limits.observed_at > current.observed_at)
                {
                    newest = Some(limits);
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("[plan-limits] cannot read {}: {error}", path.display()),
        }
    }
    newest
}

fn unix_secs(at: SystemTime) -> i64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The rail badge and the words around the numbers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    /// "14%", "≥92%" for a stale reading (use only grows inside a window,
    /// so an old % is a floor), or "LIMIT" / "OVER".
    pub text: String,
    pub level: Level,
}

/// The highest current % across `readings`, or an alert in words. None
/// when nothing still means something.
pub fn badge<'a>(readings: impl IntoIterator<Item = &'a PlanLimits>, now: i64) -> Option<Badge> {
    let mut peak: Option<(f64, bool)> = None;
    let mut alert: Option<&'static str> = None;
    for reading in readings {
        if let Some(first) = reading.active_alerts(now).first() {
            alert = alert.or(Some(first.short()));
        }
        if let Some(percent) = reading.peak_percent(now) {
            if peak.is_none_or(|(best, _)| percent > best) {
                peak = Some((percent, reading.is_stale(now)));
            }
        }
    }
    if let Some(word) = alert {
        return Some(Badge {
            text: word.to_string(),
            level: Level::Danger,
        });
    }
    let (percent, stale) = peak?;
    Some(Badge {
        text: format!("{}{}%", if stale { "≥" } else { "" }, percent.round()),
        level: level(percent),
    })
}

/// "resets in 2h 10m" within a day, "resets Thu 09:00" beyond it, "reset
/// Thu 09:00" once passed.
pub fn format_reset<Tz: TimeZone>(resets_at: i64, now: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let local = local_time(resets_at, tz);
    if resets_at <= now {
        return format!("reset {local}");
    }
    let left = resets_at - now;
    if left >= 24 * 3600 {
        return format!("resets {local}");
    }
    let hours = left / 3600;
    let minutes = ((left % 3600) + 59) / 60;
    match (hours, minutes) {
        (0, m) => format!("resets in {m}m"),
        (h, 60) => format!("resets in {}h", h + 1),
        (h, 0) => format!("resets in {h}h"),
        (h, m) => format!("resets in {h}h {m}m"),
    }
}

/// When a reading was taken: "14:05" today, "Tue 14:05" otherwise.
pub fn format_observed<Tz: TimeZone>(observed_at: i64, now: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let (Some(at), Some(today)) = (
        tz.timestamp_opt(observed_at, 0).single(),
        tz.timestamp_opt(now, 0).single(),
    ) else {
        return format!("@{observed_at}");
    };
    if at.date_naive() == today.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%a %H:%M").to_string()
    }
}

fn local_time<Tz: TimeZone>(at: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match tz.timestamp_opt(at, 0).single() {
        Some(local) => local.format("%a %H:%M").to_string(),
        None => format!("@{at}"),
    }
}

/// "updated 3m ago" for a current reading, "as of Tue 14:05" for a stale
/// one.
pub fn freshness<Tz: TimeZone>(reading: &PlanLimits, now: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if reading.is_stale(now) {
        return format!("as of {}", format_observed(reading.observed_at, now, tz));
    }
    match (now - reading.observed_at).max(0) / 60 {
        0 => "updated just now".to_string(),
        m => format!("updated {m}m ago"),
    }
}

/// One line per window for the rail tooltip: "Claude Code 5h  3% · resets
/// in 2h 10m", then the alerts and, when stale, "as of".
pub fn tooltip_lines<Tz: TimeZone>(reading: &PlanLimits, now: i64, tz: &Tz) -> Vec<String>
where
    Tz::Offset: std::fmt::Display,
{
    let name = reading.provider.label();
    let mut lines = Vec::new();
    for window in &reading.windows {
        let floor = if reading.is_stale(now) { "≥" } else { "" };
        let percent = match window.percent_at(now) {
            Some(p) => format!("{floor}{}%", p.round()),
            None if window.has_reset(now) => "no reading since".to_string(),
            None => "unknown".to_string(),
        };
        let reset = window
            .resets_at
            .map(|at| format!(" · {}", format_reset(at, now, tz)))
            .unwrap_or_default();
        lines.push(format!("{name} {}  {percent}{reset}", window.label));
    }
    for alert in reading.active_alerts(now) {
        lines.push(format!("{name}: {}", alert.text()));
    }
    if reading.is_stale(now) {
        lines.push(format!(
            "{name} as of {}",
            format_observed(reading.observed_at, now, tz)
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Shared routing policy (TRU-214). Evaluation is pure; the app owns saves/tells.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeeklyState {
    OnTrack,
    Hot,
    AlmostOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeeklyPolicy {
    pub state: WeeklyState,
    pub since: i64,
    pub resets_at: Option<i64>,
    #[serde(default)]
    hot_warned: bool,
    #[serde(default)]
    urgent_warned: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BurstPolicy {
    pub resets_at: Option<i64>,
    pub busy: bool,
    pub held: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderPolicy {
    pub weekly: Option<WeeklyPolicy>,
    #[serde(default)]
    pub burst: BurstPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingState {
    Balanced,
    LeanClaude,
    LeanCodex,
    HoldClaude,
    HoldCodex,
    Conserve,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicy {
    pub state: RoutingState,
    pub since: i64,
    /// Balanced target, retained while hot only warns. None uses the roles table.
    #[serde(default)]
    pub flexible_provider: Option<Provider>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyState {
    #[serde(default)]
    pub claude: ProviderPolicy,
    #[serde(default)]
    pub codex: ProviderPolicy,
    pub routing: Option<RoutingPolicy>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowReport {
    pub label: String,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    /// Seconds remaining; unknown when no reset was reported.
    pub resets_in: Option<i64>,
    pub elapsed_fraction: Option<f64>,
    pub pace: Option<f64>,
    /// Percentage points consumed over the latest 24 hours, when enough
    /// samples exist to estimate it.
    pub recent_24h_rate: Option<f64>,
    /// Projected use at reset from the latest 24-hour rate.
    pub projected_at_reset: Option<f64>,
    pub ahead: Option<f64>,
    pub days_left: Option<f64>,
    pub state: Option<WeeklyState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub plan_type: Option<String>,
    pub credits: Option<String>,
    pub observed_at: Option<i64>,
    pub stale: bool,
    pub windows: Vec<WindowReport>,
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutingReport {
    pub state: RoutingState,
    pub since: i64,
    /// Missing/stale weekly capacity or an active throttle/hold.
    pub constrained: bool,
    pub pinned: Option<Value>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyReport {
    pub claude: ProviderReport,
    pub codex: ProviderReport,
    pub routing: RoutingReport,
}

pub struct PolicyEvaluation {
    pub policy: PolicyState,
    pub pin: Option<RoutingPin>,
    pub report: PolicyReport,
    /// Already deduplicated per provider/window; the app fans out per chat.
    pub notices: Vec<String>,
}

impl PolicyEvaluation {
    /// Enforce at launch time as a queued request's reading may have changed.
    pub fn allows_start(
        &self,
        provider: Provider,
        parallel: bool,
        user_model_choice: bool,
    ) -> bool {
        if user_model_choice {
            return true;
        }
        let policy = match provider {
            Provider::Claude => &self.policy.claude,
            Provider::Codex => &self.policy.codex,
        };
        let (other_policy, other_report) = match provider {
            Provider::Claude => (&self.policy.codex, &self.report.codex),
            Provider::Codex => (&self.policy.claude, &self.report.claude),
        };
        let other_available =
            weekly_available(other_report) && !held(other_policy) && !other_report.busy;
        // A stale or missing reading says nothing about exhaustion, and only
        // a run writes a new one (Codex's rollout), so requiring a fresh
        // reading deadlocks every launch once a provider is idle (TRU-232).
        // Holds from the last reading still apply until their reset.
        !held(policy)
            && !(policy.burst.busy
                && (parallel
                    || (other_available
                        && !self
                            .pin
                            .as_ref()
                            .is_some_and(|pin| pin.provider == provider))))
    }
}

fn history_metrics(
    history: Option<&WindowHistory>,
    now: i64,
    resets_at: Option<i64>,
) -> (Option<f64>, Option<f64>) {
    let Some(history) = history else {
        return (None, None);
    };
    let cutoff = now - 24 * 60 * 60;
    let mut samples = history.samples.iter().filter(|sample| {
        sample.observed_at >= cutoff && sample.observed_at <= now && sample.used_percent.is_finite()
    });
    let Some(first) = samples.next() else {
        return (None, None);
    };
    let Some(last) = samples.next_back() else {
        return (None, None);
    };
    let elapsed = last.observed_at - first.observed_at;
    if elapsed <= 0 {
        return (None, None);
    }
    let rate = (last.used_percent - first.used_percent) * 86_400.0 / elapsed as f64;
    let projected = resets_at
        .filter(|at| *at > now)
        .map(|at| last.used_percent + (rate / 86_400.0) * (at - last.observed_at) as f64);
    (Some(rate), projected)
}

pub fn hot_pin_until_reset(report: &ProviderReport, now: i64) -> Option<i64> {
    report
        .windows
        .iter()
        .find(|window| window.label == "7d" && window.state == Some(WeeklyState::Hot))
        .and_then(|window| window.resets_at.filter(|at| *at > now))
}

pub fn hot_reset_pin(provider: Provider, report: &ProviderReport, now: i64) -> Option<RoutingPin> {
    hot_pin_until_reset(report, now).map(|until| RoutingPin {
        provider: match provider {
            Provider::Claude => Provider::Codex,
            Provider::Codex => Provider::Claude,
        },
        until,
    })
}

pub fn pin_change_notice(pin: Option<&RoutingPin>, now: i64, routing: &RoutingReport) -> String {
    let change = match pin {
        Some(pin) => format!(
            "pin to {} set until {} UTC.",
            pin.provider.label(),
            format_reset(pin.until, now, &chrono::Utc)
        ),
        None => "pin cleared.".to_string(),
    };
    format!("{} {change}", routing.reason)
}

fn window_report(
    window: &LimitWindow,
    fresh: bool,
    now: i64,
    history: Option<&WindowHistory>,
) -> WindowReport {
    let duration = match window.label.as_str() {
        "7d" => Some(WEEK_SECS),
        "5h" => Some(BURST_SECS),
        _ => None,
    };
    let used = window
        .percent_at(now)
        .filter(|p| p.is_finite() && *p >= 0.0);
    let (recent_24h_rate, projected_at_reset) = history_metrics(history, now, window.resets_at);
    let f = duration
        .zip(window.resets_at)
        .filter(|(_, at)| fresh && *at > now)
        .map(|(duration, at)| {
            (1.0 - (at - now) as f64 / duration as f64).clamp(ELAPSED_FLOOR, 1.0)
        });
    WindowReport {
        label: window.label.clone(),
        used_percent: used,
        resets_at: window.resets_at,
        resets_in: window.resets_at.map(|at| (at - now).max(0)),
        elapsed_fraction: f,
        pace: used.zip(f).map(|(used, f)| used / f),
        recent_24h_rate,
        projected_at_reset,
        ahead: used.zip(f).map(|(used, f)| used - TARGET_PERCENT * f),
        days_left: used
            .zip(f)
            .filter(|(used, _)| window.label == "7d" && *used > 0.0)
            .map(|(used, f)| (100.0 - used) * f * 7.0 / used),
        state: None,
    }
}

/// Unattributed limit/overage alerts are conservatively account-wide. A
/// verified 5h attribution is the only case that creates just a short hold.
fn alert_is_burst(alert: &Alert) -> bool {
    alert.window.as_deref() == Some("5h")
        || matches!(&alert.kind,
        AlertKind::LimitReached { which: Some(which) } if which == "5h" || which == "five_hour")
}

fn provider_evaluation(
    reading: Option<&PlanLimits>,
    histories: &[WindowHistory],
    previous: &ProviderPolicy,
    provider: Provider,
    now: i64,
) -> (ProviderPolicy, ProviderReport, Vec<String>, Option<String>) {
    let mut policy = previous.clone();
    let fresh = reading.is_some_and(|r| !r.is_stale(now));
    let mut windows = reading
        .map(|r| {
            r.windows
                .iter()
                .map(|w| {
                    let history = histories.iter().find(|h| {
                        h.provider == r.provider && h.label == w.label && h.resets_at == w.resets_at
                    });
                    window_report(w, fresh, now, history)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for label in ["7d", "5h"] {
        if !windows.iter().any(|w| w.label == label) {
            windows.push(window_report(
                &LimitWindow {
                    label: label.into(),
                    used_percent: None,
                    resets_at: None,
                },
                false,
                now,
                None,
            ));
        }
    }
    let weekly = windows
        .iter()
        .find(|w| w.label == "7d")
        .expect("weekly placeholder");
    let alerts = reading.map(|r| r.active_alerts(now)).unwrap_or_default();
    let weekly_alert = fresh
        && alerts.iter().any(|a| {
            !alert_is_burst(a)
                && matches!(a.kind, AlertKind::LimitReached { .. } | AlertKind::Overage)
        });
    let mut notices = Vec::new();
    let mut warning = None;
    let old_held = previous
        .weekly
        .as_ref()
        .is_some_and(|w| w.state == WeeklyState::AlmostOut);
    let usable_weekly =
        weekly.pace.is_some() || weekly.used_percent.is_some_and(|p| p >= ALMOST_OUT_PERCENT);
    let changed_anchor = policy.weekly.as_ref().is_some_and(|w| {
        w.resets_at
            .zip(weekly.resets_at)
            .is_some_and(|(old, new)| old != new)
    });
    // Learning the first reset time is not evidence that a window reset.
    if fresh {
        if let Some(state) = &mut policy.weekly {
            if state.resets_at.is_none() {
                state.resets_at = weekly.resets_at;
            }
        }
    }
    if fresh
        && !usable_weekly
        && !weekly_alert
        && weekly.resets_at.is_some_and(|at| at > now)
        && changed_anchor
    {
        policy.weekly = None;
        if old_held {
            notices.push(format!("{} weekly window reset; usage is unknown. Refresh readings before treating it as spare capacity.", provider.label()));
        }
    }
    // Never adopt a changed anchor from a stale/expired/unknown reading.
    if fresh && (usable_weekly || weekly_alert) {
        let new_window = policy.weekly.is_none() || changed_anchor;
        if new_window {
            policy.weekly = Some(WeeklyPolicy {
                state: WeeklyState::OnTrack,
                since: now,
                resets_at: weekly.resets_at,
                hot_warned: false,
                urgent_warned: false,
            });
        }
        if let Some(state) = &mut policy.weekly {
            let next = if weekly_alert
                || weekly.used_percent.is_some_and(|p| p >= ALMOST_OUT_PERCENT)
                || state.state == WeeklyState::AlmostOut
            {
                WeeklyState::AlmostOut
            } else if state.state == WeeklyState::Hot {
                if weekly.pace.is_some_and(|p| p <= RECOVERED_PACE)
                    && now - state.since >= STATE_DWELL_SECS
                {
                    WeeklyState::OnTrack
                } else {
                    WeeklyState::Hot
                }
            } else if weekly.pace.is_some_and(|p| p >= HOT_PACE)
                && weekly.ahead.is_some_and(|a| a >= HOT_AHEAD)
            {
                WeeklyState::Hot
            } else {
                WeeklyState::OnTrack
            };
            if state.state != next {
                state.state = next;
                state.since = now;
            }
            if old_held && new_window {
                notices.push(format!(
                    "{} weekly window reset; {}. Check current routing before starting work.",
                    provider.label(),
                    if next == WeeklyState::AlmostOut {
                        "the provider remains held"
                    } else {
                        "the provider recovered"
                    }
                ));
            }
            if state.state == WeeklyState::Hot {
                let urgent = weekly
                    .days_left
                    .is_some_and(|days| days <= URGENT_RUNOUT_DAYS);
                if let (true, Some(used), Some(left)) = (
                    !state.hot_warned || (urgent && !state.urgent_warned),
                    weekly.used_percent,
                    weekly.resets_in,
                ) {
                    let left = left as f64 / 86400.0;
                    let runout = weekly
                        .days_left
                        .map(|days| local_time(now + (days * 86400.0) as i64, &chrono::Utc))
                        .unwrap_or_else(|| "unknown".into());
                    let other = match provider {
                        Provider::Claude => "Codex",
                        Provider::Codex => "Claude",
                    };
                    let recent = weekly
                        .projected_at_reset
                        .filter(|projected| {
                            weekly
                                .pace
                                .is_some_and(|pace| (pace - projected).abs() > 20.0)
                        })
                        .map(|projected| {
                            format!(
                                "; pace {:.0}, last 24 h project {:.0}",
                                weekly.pace.unwrap_or_default(),
                                projected
                            )
                        })
                        .unwrap_or_default();
                    warning = Some(format!("{} {used:.0}% with {left:.1} days left; at this rate it runs out {runout} UTC{recent}. Want me to send work to {other} until it settles? Tell the coordinator to send flexible work there until reset.", provider.label()));
                    state.hot_warned = true;
                    state.urgent_warned |= urgent;
                }
            }
        }
    }
    let burst = windows
        .iter()
        .find(|w| w.label == "5h")
        .expect("burst placeholder");
    if policy.burst.resets_at.is_some_and(|at| at <= now) {
        policy.burst = BurstPolicy::default();
    }
    if fresh && burst.resets_at.is_some_and(|at| at > now) {
        if policy.burst.resets_at.is_some() && policy.burst.resets_at != burst.resets_at {
            policy.burst = BurstPolicy {
                resets_at: burst.resets_at,
                ..BurstPolicy::default()
            };
        }
        // Learning the first anchor does not establish a new burst window.
        policy.burst.resets_at = burst.resets_at;
        if let Some(used) = burst.used_percent {
            if used >= BUSY_PERCENT
                && burst
                    .resets_in
                    .is_some_and(|left| left > BUSY_MIN_LEFT_SECS)
            {
                policy.burst.busy = true;
            } else if used < BUSY_CLEAR_PERCENT {
                policy.burst.busy = false;
            }
            policy.burst.held |= used >= BURST_LIMIT_PERCENT;
        }
    }
    if fresh
        && burst
            .used_percent
            .is_some_and(|used| used >= BURST_LIMIT_PERCENT)
    {
        policy.burst.held = true;
        policy.burst.resets_at = burst.resets_at.or(policy.burst.resets_at);
    }
    for alert in alerts.iter().filter(|a| {
        fresh
            && alert_is_burst(a)
            && matches!(a.kind, AlertKind::LimitReached { .. } | AlertKind::Overage)
    }) {
        // A short hold with no reset cannot be released by guessing a time.
        policy.burst.held = true;
        policy.burst.resets_at = alert.until.or(burst.resets_at);
    }
    for window in &mut windows {
        if window.label == "7d" {
            window.state = policy.weekly.as_ref().map(|w| w.state);
        } else if window.label == "5h" && window.resets_at.is_none() && policy.burst.held {
            window.resets_at = policy.burst.resets_at;
            window.resets_in = policy.burst.resets_at.map(|at| (at - now).max(0));
        }
    }
    let report = ProviderReport {
        plan_type: reading.and_then(|r| r.plan_type.clone()),
        credits: reading.and_then(|r| r.credits.clone()),
        observed_at: reading.map(|r| r.observed_at),
        stale: !fresh,
        windows,
        busy: policy.burst.busy,
    };
    (policy, report, notices, warning)
}

fn held(policy: &ProviderPolicy) -> bool {
    policy.burst.held
        || policy
            .weekly
            .as_ref()
            .is_some_and(|w| w.state == WeeklyState::AlmostOut)
}

fn weekly_available(report: &ProviderReport) -> bool {
    !report.stale
        && report
            .windows
            .iter()
            .any(|w| w.label == "7d" && w.pace.is_some())
}

fn provider_summary(
    name: &str,
    report: &ProviderReport,
    policy: &ProviderPolicy,
    now: i64,
) -> String {
    let weekly_held = policy
        .weekly
        .as_ref()
        .is_some_and(|w| w.state == WeeklyState::AlmostOut);
    let label = if policy.burst.held && !weekly_held {
        "5h"
    } else {
        "7d"
    };
    let window = report.windows.iter().find(|w| w.label == label);
    let used = window
        .and_then(|w| w.used_percent)
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "unknown use".into());
    let held_reset = if label == "5h" {
        policy.burst.resets_at
    } else {
        policy.weekly.as_ref().and_then(|w| w.resets_at)
    };
    let reset = window.and_then(|w| w.resets_at).or(held_reset);
    let reset = reset
        .map(|at| format!("{} UTC", format_reset(at, now, &chrono::Utc)))
        .unwrap_or_else(|| "reset unknown".into());
    let pace = window
        .and_then(|w| w.pace)
        .map(|p| format!(", pace {p:.0}"))
        .unwrap_or_default();
    let recent = window
        .and_then(|w| w.projected_at_reset.zip(w.pace))
        .filter(|(projected, pace)| (projected - pace).abs() > 20.0)
        .map(|(projected, _)| format!(", last 24 h project {:.0}", projected))
        .unwrap_or_default();
    format!("{name} {used} ({label}, {reset}{pace}{recent})")
}

pub fn evaluate_policy(file: &PlanLimitsFile, now: i64) -> PolicyEvaluation {
    let previous = file.policy.clone().unwrap_or_default();
    // Remote/legacy Claude readings cannot establish local account capacity.
    let local_claude = file.claude.as_ref().filter(|r| r.local == Some(true));
    let (claude, claude_report, mut notices, claude_warning) = provider_evaluation(
        local_claude,
        &file.history,
        &previous.claude,
        Provider::Claude,
        now,
    );
    let (codex, codex_report, codex_notices, codex_warning) = provider_evaluation(
        file.codex.as_ref(),
        &file.history,
        &previous.codex,
        Provider::Codex,
        now,
    );
    notices.extend(codex_notices);
    let held_state = match (held(&claude), held(&codex)) {
        (true, true) => RoutingState::Conserve,
        (true, false) => RoutingState::HoldClaude,
        (false, true) => RoutingState::HoldCodex,
        (false, false) => RoutingState::Balanced,
    };
    let pin = file.pin.clone().filter(|pin| pin.until > now);
    let state = if held_state != RoutingState::Balanced {
        held_state
    } else {
        match pin.as_ref().map(|pin| pin.provider) {
            Some(Provider::Claude) => RoutingState::LeanClaude,
            Some(Provider::Codex) => RoutingState::LeanCodex,
            None => RoutingState::Balanced,
        }
    };
    let both_on_track = [&claude, &codex].iter().all(|p| {
        p.weekly
            .as_ref()
            .is_some_and(|w| w.state == WeeklyState::OnTrack)
    });
    let flexible_provider =
        if both_on_track && weekly_available(&claude_report) && weekly_available(&codex_report) {
            let ahead = |r: &ProviderReport| {
                r.windows
                    .iter()
                    .find(|w| w.label == "7d")
                    .and_then(|w| w.ahead)
            };
            match ahead(&claude_report).zip(ahead(&codex_report)) {
                Some((c, x)) if (c - x).abs() > BALANCE_MARGIN => Some(if c < x {
                    Provider::Claude
                } else {
                    Provider::Codex
                }),
                _ => None,
            }
        } else {
            previous.routing.as_ref().and_then(|r| r.flexible_provider)
        };
    let routing = RoutingPolicy {
        state,
        since: previous
            .routing
            .as_ref()
            .filter(|old| old.state == state)
            .map_or(now, |old| old.since),
        flexible_provider,
    };
    let available_claude =
        weekly_available(&claude_report) && !claude_report.busy && !held(&claude);
    let available_codex = weekly_available(&codex_report) && !codex_report.busy && !held(&codex);
    let constrained = !weekly_available(&claude_report)
        || !weekly_available(&codex_report)
        || claude_report.busy
        || codex_report.busy
        || state != RoutingState::Balanced;
    let c = provider_summary("Claude", &claude_report, &claude, now);
    let x = provider_summary("Codex", &codex_report, &codex, now);
    let both_hot = [&claude, &codex].iter().all(|p| {
        p.weekly
            .as_ref()
            .is_some_and(|w| w.state == WeeklyState::Hot)
    });
    if both_hot && (claude_warning.is_some() || codex_warning.is_some()) {
        let first = claude
            .weekly
            .as_ref()
            .and_then(|w| w.resets_at)
            .zip(codex.weekly.as_ref().and_then(|w| w.resets_at))
            .map(|(c, x)| if c <= x { "Claude" } else { "Codex" });
        let order = first
            .map(|name| format!("use {name} first because it resets sooner"))
            .unwrap_or_else(|| "verify reset times before choosing the order".into());
        notices.push(format!("Both plans are running hot: {c}; {x}. Want one priority task at a time, required reviews only, and {order}? Routing stays unchanged until you choose; credits or overage need your explicit choice."));
    } else {
        notices.extend(claude_warning.into_iter().chain(codex_warning));
    }
    let mut reason = match state {
        RoutingState::LeanClaude => format!("Routing: pinned to Claude until {} UTC.", pin.as_ref().map(|pin| format_reset(pin.until, now, &chrono::Utc)).unwrap_or_else(|| "cleared".into())),
        RoutingState::LeanCodex => format!("Routing: pinned to Codex until {} UTC.", pin.as_ref().map(|pin| format_reset(pin.until, now, &chrono::Utc)).unwrap_or_else(|| "cleared".into())),
        RoutingState::HoldClaude => format!("Routing: {c} → start nothing new on Claude. {} A Claude coordinator finishes its step, writes checkpoint and handoff, and says when Claude resets.", if available_codex { "Coding, reviews and consults to Codex until Claude recovers or resets." } else { "Codex has no verified spare capacity; defer flexible work until fresh readings permit it." }),
        RoutingState::HoldCodex => format!("Routing: {x} → start nothing new on Codex. {}", if available_claude { "Coding, reviews and consults to Claude until Codex recovers or resets." } else { "Claude has no verified spare capacity; defer flexible work until fresh readings permit it." }),
        RoutingState::Conserve => format!("Routing: {c}; {x} → conserve. Both providers are held: defer new work until a hold clears; then one priority task at a time, serial, required specialist reviews only. Credits or overage need the user's explicit choice."),
        RoutingState::Balanced => {
            let preference = if available_claude && available_codex {
                match flexible_provider {
                    Some(Provider::Claude) if both_on_track => "Flexible jobs to Claude, further behind its weekly line.",
                    Some(Provider::Codex) if both_on_track => "Flexible jobs to Codex, further behind its weekly line.",
                    Some(Provider::Claude) => "Flexible jobs to Claude; keep the balanced provider while hot usage only warns.",
                    Some(Provider::Codex) => "Flexible jobs to Codex; keep the balanced provider while hot usage only warns.",
                    None => "Use the roles table for flexible work.",
                }
            } else if available_claude { "Flexible jobs to Claude; Codex is busy or has unknown/stale weekly capacity." }
            else if available_codex { "Flexible jobs to Codex; Claude is busy or has unknown/stale weekly capacity." }
            else { "No verified spare capacity: refresh readings before starting flexible work." };
            format!("Routing: balanced. {preference}")
        }
    };
    if matches!(state, RoutingState::LeanClaude | RoutingState::LeanCodex) {
        if let Some(pin) = &pin {
            let busy = match pin.provider {
                Provider::Claude => claude_report.busy,
                Provider::Codex => codex_report.busy,
            };
            if busy {
                reason.push_str(&format!(" {} is busy in its 5h window: start no new parallel worker on it; the pin keeps flexible work on this provider, serially, until the throttle clears.", pin.provider.label()));
            }
        }
    }
    if previous
        .routing
        .as_ref()
        .is_some_and(|old| old.state != state)
        || (previous.routing.is_none() && state != RoutingState::Balanced)
    {
        notices.push(reason.clone());
    }
    PolicyEvaluation {
        policy: PolicyState {
            claude,
            codex,
            routing: Some(routing.clone()),
        },
        pin: pin.clone(),
        report: PolicyReport {
            claude: claude_report,
            codex: codex_report,
            routing: RoutingReport {
                state,
                since: routing.since,
                constrained,
                pinned: pin.as_ref().map(|pin| {
                    serde_json::json!({
                        "provider": pin.provider,
                        "until": pin.until,
                    })
                }),
                reason,
            },
        },
        notices,
    }
}

// ---------------------------------------------------------------------------
// The persisted last readings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanLimitsFile {
    #[serde(default)]
    pub claude: Option<PlanLimits>,
    #[serde(default)]
    pub codex: Option<PlanLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<WindowHistory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<RoutingPin>,
    /// Notices and their policy suppression flags are saved together so a
    /// busy coordinator can receive them after a restart.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub pending_notices: HashMap<String, Vec<String>>,
}

impl PlanLimitsFile {
    pub fn path_in(config_dir: &Path) -> PathBuf {
        config_dir.join(LIMITS_FILE)
    }

    /// The file at `path`; missing is empty, unreadable is logged and empty
    /// (left alone until the next save replaces it).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!(
                    "[plan-limits] {} is not a plan-limits file ({error}); starting empty",
                    path.display()
                );
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!(
                    "[plan-limits] cannot read {}: {error}; starting empty",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Blocking: run it off the UI thread.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        crate::merge_plan::write_atomically(path, &encoded)
    }

    pub fn get(&self, provider: Provider) -> Option<&PlanLimits> {
        match provider {
            Provider::Claude => self.claude.as_ref(),
            Provider::Codex => self.codex.as_ref(),
        }
    }

    pub fn prune_history(&mut self, now: i64) -> bool {
        let cutoff = now - HISTORY_RETENTION_SECS;
        let before_count = self.history.len();
        let before_samples = self
            .history
            .iter()
            .map(|history| history.samples.len())
            .sum::<usize>();
        for history in &mut self.history {
            history
                .samples
                .retain(|sample| sample.observed_at >= cutoff);
        }
        self.history.retain(|history| !history.samples.is_empty());
        before_count != self.history.len()
            || before_samples
                != self
                    .history
                    .iter()
                    .map(|history| history.samples.len())
                    .sum::<usize>()
    }

    /// Takes `reading` unless the one held was observed later. True when
    /// anything changed.
    pub fn absorb(&mut self, reading: PlanLimits) -> bool {
        let provider = reading.provider;
        let observed_at = reading.observed_at;
        let held = self.get(provider);
        if held.is_some_and(|held| held.observed_at > observed_at || *held == reading) {
            return false;
        }
        for window in &reading.windows {
            let Some(used_percent) = window.used_percent.filter(|value| value.is_finite()) else {
                continue;
            };
            self.record_sample(
                provider,
                &window.label,
                window.resets_at,
                observed_at,
                used_percent,
            );
        }
        match provider {
            Provider::Claude => self.claude = Some(reading),
            Provider::Codex => self.codex = Some(reading),
        }
        true
    }

    fn record_sample(
        &mut self,
        provider: Provider,
        label: &str,
        resets_at: Option<i64>,
        observed_at: i64,
        used_percent: f64,
    ) {
        let history = self
            .history
            .iter_mut()
            .find(|history| history.provider == provider && history.label == label);
        let history = match history {
            Some(history) if history.resets_at == resets_at => history,
            Some(history) => {
                *history = WindowHistory {
                    provider,
                    label: label.to_string(),
                    resets_at,
                    samples: Vec::new(),
                };
                history
            }
            None => {
                self.history.push(WindowHistory {
                    provider,
                    label: label.to_string(),
                    resets_at,
                    samples: Vec::new(),
                });
                self.history.last_mut().expect("just inserted history")
            }
        };
        if history
            .samples
            .last()
            .is_some_and(|sample| observed_at - sample.observed_at < HISTORY_SAMPLE_SECS)
        {
            return;
        }
        history.samples.push(UsageSample {
            observed_at,
            used_percent,
        });
        let cutoff = observed_at - HISTORY_RETENTION_SECS;
        history
            .samples
            .retain(|sample| sample.observed_at >= cutoff);
        if history.samples.len() > 768 {
            let excess = history.samples.len() - 768;
            history.samples.drain(..excess);
        }
    }

    pub fn readings(&self) -> impl Iterator<Item = &PlanLimits> {
        self.claude.iter().chain(self.codex.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    fn claude_info() -> Value {
        json!({
            "status": "allowed",
            "resetsAt": NOW + 7_800,
            "rateLimitType": "five_hour",
            "overageStatus": "allowed",
            "overageResetsAt": NOW + 900_000,
            "isUsingOverage": false,
            "unifiedWindows": {
                "five_hour": {"utilization": 0.42, "resetsAt": NOW + 7_800},
                "seven_day": {"utilization": 0.18, "resetsAt": NOW + 400_000}
            }
        })
    }

    #[test]
    fn claude_frame_reads_both_windows_as_percent() {
        let limits = claude_limits(&claude_info(), NOW);
        assert_eq!(limits.provider, Provider::Claude);
        assert_eq!(limits.observed_at, NOW);
        assert_eq!(limits.windows.len(), 2);
        assert_eq!(limits.windows[0].label, "5h");
        assert!((limits.windows[0].used_percent.unwrap() - 42.0).abs() < 1e-9);
        assert_eq!(limits.windows[0].resets_at, Some(NOW + 7_800));
        assert_eq!(limits.windows[1].label, "7d");
        assert!((limits.windows[1].used_percent.unwrap() - 18.0).abs() < 1e-9);
        assert!(limits.alerts.is_empty());
        assert!((limits.peak_percent(NOW).unwrap() - 42.0).abs() < 1e-9);
    }

    #[test]
    fn claude_missing_fields_are_unknown_never_zero() {
        // No unifiedWindows at all.
        let limits = claude_limits(&json!({"status": "allowed"}), NOW);
        assert_eq!(limits.windows.len(), 2);
        assert!(limits.windows.iter().all(|w| w.used_percent.is_none()));
        assert_eq!(limits.peak_percent(NOW), None);
        assert_eq!(badge([&limits], NOW), None);
        // One window missing, the other without utilization.
        let limits = claude_limits(
            &json!({"unifiedWindows": {"seven_day": {"resetsAt": NOW + 10}}}),
            NOW,
        );
        assert_eq!(limits.windows[0].used_percent, None);
        assert_eq!(limits.windows[0].resets_at, None);
        assert_eq!(limits.windows[1].used_percent, None);
        assert_eq!(limits.windows[1].resets_at, Some(NOW + 10));
        // An empty object parses, with no alerts.
        let limits = claude_limits(&json!({}), NOW);
        assert!(limits.alerts.is_empty());
        // An extra model window is listed after the two known ones.
        let limits = claude_limits(
            &json!({"unifiedWindows": {"seven_day_opus": {"utilization": 0.5}}}),
            NOW,
        );
        assert_eq!(limits.windows[2].label, "7d opus");
    }

    #[test]
    fn claude_overage_and_rejected_are_alerts() {
        let mut info = claude_info();
        info["isUsingOverage"] = json!(true);
        let limits = claude_limits(&info, NOW);
        assert_eq!(limits.alerts.len(), 1);
        assert_eq!(limits.alerts[0].kind, AlertKind::Overage);
        assert_eq!(limits.alerts[0].text(), "Using overage");
        assert_eq!(limits.alerts[0].until, Some(NOW + 7_800));
        let b = badge([&limits], NOW).unwrap();
        assert_eq!(b.text, "OVER");
        assert_eq!(b.level, Level::Danger);

        let mut info = claude_info();
        info["status"] = json!("rejected");
        let limits = claude_limits(&info, NOW);
        assert_eq!(limits.alerts[0].text(), "Limit reached (5h)");
        assert_eq!(badge([&limits], NOW).unwrap().text, "LIMIT");
        // It holds until the window resets, stale or not.
        assert_eq!(limits.active_alerts(NOW + 7_799).len(), 1);
        assert!(limits.active_alerts(NOW + 7_800).is_empty());

        let mut info = claude_info();
        info["status"] = json!("allowed_warning");
        let limits = claude_limits(&info, NOW);
        assert_eq!(limits.alerts[0].text(), "Limit status: allowed_warning");
        assert_eq!(badge([&limits], NOW).unwrap().level, Level::Danger);
    }

    #[test]
    fn alert_without_a_reset_holds_only_while_current() {
        let limits = claude_limits(&json!({"status": "rejected"}), NOW);
        assert_eq!(limits.active_alerts(NOW + STALE_AFTER_SECS).len(), 1);
        assert!(limits.active_alerts(NOW + STALE_AFTER_SECS + 1).is_empty());
    }

    fn codex_rate_limits() -> Value {
        json!({
            "limit_id": "codex",
            "limit_name": null,
            "primary": {"used_percent": 31.0, "window_minutes": 10080, "resets_at": NOW + 300_000},
            "secondary": null,
            "credits": {"has_credits": true, "unlimited": false, "balance": "1250.0000000000"},
            "individual_limit": null,
            "spend_control_reached": null,
            "plan_type": "plus",
            "rate_limit_reached_type": null
        })
    }

    #[test]
    fn codex_rate_limits_with_null_secondary() {
        let limits = codex_limits(&codex_rate_limits(), NOW).unwrap();
        assert_eq!(limits.provider, Provider::Codex);
        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].label, "7d");
        assert_eq!(limits.windows[0].used_percent, Some(31.0));
        assert_eq!(limits.windows[0].resets_at, Some(NOW + 300_000));
        assert_eq!(limits.plan_type.as_deref(), Some("plus"));
        assert_eq!(limits.credits.as_deref(), Some("1,250"));
        assert!(limits.alerts.is_empty());
    }

    #[test]
    fn codex_windows_are_labelled_from_minutes() {
        assert_eq!(window_label(10080), "7d");
        assert_eq!(window_label(300), "5h");
        assert_eq!(window_label(1440), "24h");
        assert_eq!(window_label(45), "45m");
        let mut raw = codex_rate_limits();
        raw["primary"]["window_minutes"] = json!(300);
        raw["secondary"] =
            json!({"used_percent": 2.5, "window_minutes": 10080, "resets_at": NOW + 9});
        let limits = codex_limits(&raw, NOW).unwrap();
        let labels: Vec<&str> = limits.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5h", "7d"]);
        // A window without minutes keeps its slot name.
        raw["primary"] = json!({"used_percent": 1.0});
        assert_eq!(codex_limits(&raw, NOW).unwrap().windows[0].label, "primary");
    }

    #[test]
    fn codex_reached_limit_is_an_alert_until_reset() {
        let mut raw = codex_rate_limits();
        raw["rate_limit_reached_type"] = json!("primary");
        let limits = codex_limits(&raw, NOW).unwrap();
        assert_eq!(limits.alerts[0].text(), "Limit reached (primary)");
        assert_eq!(limits.alerts[0].until, Some(NOW + 300_000));
        assert_eq!(badge([&limits], NOW).unwrap().text, "LIMIT");
    }

    #[test]
    fn codex_reached_limit_uses_the_named_window_reset() {
        let mut raw = codex_rate_limits();
        raw["primary"]["resets_at"] = json!(NOW + 500);
        raw["secondary"] = json!({
            "used_percent": 10.0,
            "window_minutes": 300,
            "resets_at": NOW + 100
        });
        raw["rate_limit_reached_type"] = json!("secondary");

        let limits = codex_limits(&raw, NOW).unwrap();
        assert_eq!(limits.alerts[0].until, Some(NOW + 100));
    }

    #[test]
    fn codex_credits_and_per_model_buckets() {
        let mut raw = codex_rate_limits();
        raw["credits"] = json!({"has_credits": false, "unlimited": false, "balance": "0"});
        assert_eq!(
            codex_limits(&raw, NOW).unwrap().credits.as_deref(),
            Some("none")
        );
        raw["credits"] = json!({"unlimited": true});
        assert_eq!(
            codex_limits(&raw, NOW).unwrap().credits.as_deref(),
            Some("unlimited")
        );
        raw["credits"] = json!({"has_credits": true, "balance": "12.5"});
        assert_eq!(
            codex_limits(&raw, NOW).unwrap().credits.as_deref(),
            Some("12.50")
        );
        raw["limit_id"] = json!("codex_othermodel");
        assert_eq!(codex_limits(&raw, NOW), None);
        raw.as_object_mut().unwrap().remove("limit_id");
        assert_eq!(codex_limits(&raw, NOW), None);
        for invalid in [json!(null), json!(42)] {
            raw["limit_id"] = invalid;
            assert_eq!(codex_limits(&raw, NOW), None);
        }
        assert_eq!(codex_limits(&Value::Null, NOW), None);
    }

    fn token_count_line(stamp: &str, limit_id: &str, used: f64) -> String {
        json!({
            "timestamp": stamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": null,
                "rate_limits": {
                    "limit_id": limit_id,
                    "primary": {"used_percent": used, "window_minutes": 10080, "resets_at": NOW + 1000},
                    "secondary": null,
                    "plan_type": "pro"
                }
            }
        })
        .to_string()
    }

    fn write_rollout(dir: &Path, name: &str, lines: &[String], age_secs: u64) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mtime = SystemTime::now() - std::time::Duration::from_secs(age_secs);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        path
    }

    #[test]
    fn newest_rollout_and_its_last_account_line_win() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026").join("11").join("04");
        std::fs::create_dir_all(&day).unwrap();
        let other = r#"{"timestamp":"2026-11-04T10:00:00Z","type":"response_item","payload":{}}"#;
        write_rollout(
            &day,
            "rollout-old.jsonl",
            &[token_count_line("2026-11-04T09:00:00Z", "codex", 90.0)],
            600,
        );
        write_rollout(
            &day,
            "rollout-new.jsonl",
            &[
                token_count_line("2026-11-04T10:00:00Z", "codex", 10.0),
                token_count_line("2026-11-04T10:05:00Z", "codex", 12.0),
                token_count_line("2026-11-04T10:06:00Z", "codex_othermodel", 70.0),
                other.to_string(),
            ],
            10,
        );
        // Not a rollout, newest of all: ignored.
        write_rollout(
            &day,
            "notes.jsonl",
            &[token_count_line("2026-11-04T11:00:00Z", "codex", 99.0)],
            0,
        );
        let limits = latest_codex_limits(tmp.path()).unwrap();
        assert_eq!(limits.windows[0].used_percent, Some(12.0));
        assert_eq!(
            limits.observed_at,
            DateTime::parse_from_rfc3339("2026-11-04T10:05:00Z")
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn a_newest_rollout_without_limits_falls_back_to_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        write_rollout(
            tmp.path(),
            "rollout-a.jsonl",
            &[token_count_line("2026-11-04T09:00:00Z", "codex", 55.0)],
            600,
        );
        write_rollout(
            tmp.path(),
            "rollout-b.jsonl",
            &[r#"{"type":"session_meta","payload":{}}"#.to_string()],
            1,
        );
        let limits = latest_codex_limits(tmp.path()).unwrap();
        assert_eq!(limits.windows[0].used_percent, Some(55.0));
        assert_eq!(latest_codex_limits(&tmp.path().join("missing")), None);
    }

    #[test]
    fn newest_observation_wins_across_bounded_rollouts() {
        let tmp = tempfile::tempdir().unwrap();
        write_rollout(
            tmp.path(),
            "rollout-newer-mtime.jsonl",
            &[token_count_line("2026-11-04T09:00:00Z", "codex", 10.0)],
            1,
        );
        write_rollout(
            tmp.path(),
            "rollout-older-mtime.jsonl",
            &[token_count_line("2026-11-04T10:00:00Z", "codex", 80.0)],
            600,
        );

        let limits = latest_codex_limits(tmp.path()).unwrap();
        assert_eq!(limits.windows[0].used_percent, Some(80.0));
        assert_eq!(
            limits.observed_at,
            DateTime::parse_from_rfc3339("2026-11-04T10:00:00Z")
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn tail_read_grows_past_a_long_trailing_line() {
        let tmp = tempfile::tempdir().unwrap();
        let filler = format!(
            r#"{{"type":"response_item","payload":{{"text":"{}"}}}}"#,
            "x".repeat(TAIL_START_BYTES as usize * 2)
        );
        let path = write_rollout(
            tmp.path(),
            "rollout-long.jsonl",
            &[
                token_count_line("2026-11-04T09:00:00Z", "codex", 7.0),
                filler,
            ],
            0,
        );
        let limits = last_codex_limits_in(&path).unwrap().unwrap();
        assert_eq!(limits.windows[0].used_percent, Some(7.0));
    }

    #[test]
    fn tail_read_skips_a_reading_before_the_byte_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        let filler = format!(
            r#"{{"type":"response_item","payload":{{"text":"{}"}}}}"#,
            "x".repeat(TAIL_MAX_BYTES as usize)
        );
        let path = write_rollout(
            tmp.path(),
            "rollout-too-large.jsonl",
            &[
                token_count_line("2026-11-04T09:00:00Z", "codex", 7.0),
                filler,
            ],
            0,
        );

        assert_eq!(last_codex_limits_in(&path).unwrap(), None);
    }

    #[test]
    fn a_line_without_a_timestamp_uses_the_file_mtime() {
        let line = br#"{"type":"event_msg","payload":{"rate_limits":{"limit_id":"codex","primary":{"used_percent":3.0}}}}"#;
        assert_eq!(codex_line_limits(line, 1234).unwrap().observed_at, 1234);
        assert_eq!(codex_line_limits(b"{\"rate_limits\":null}", 1), None);
    }

    #[test]
    fn thresholds() {
        assert_eq!(level(0.0), Level::Normal);
        assert_eq!(level(74.9), Level::Normal);
        assert_eq!(level(75.0), Level::Warning);
        assert_eq!(level(89.9), Level::Warning);
        assert_eq!(level(90.0), Level::Danger);
        assert_eq!(level(120.0), Level::Danger);
    }

    #[test]
    fn staleness_and_reset() {
        let limits = claude_limits(&claude_info(), NOW);
        assert!(!limits.is_stale(NOW + STALE_AFTER_SECS));
        assert!(limits.is_stale(NOW + STALE_AFTER_SECS + 1));
        // Current: the plain %.
        let b = badge([&limits], NOW + 60).unwrap();
        assert_eq!(b.text, "42%");
        assert_eq!(b.level, Level::Normal);
        // Stale: a floor, not a current value.
        assert_eq!(badge([&limits], NOW + 3_600).unwrap().text, "≥42%");
        // Once the 5h window resets its % means nothing; the 7d one is left.
        assert_eq!(limits.windows[0].percent_at(NOW + 7_800), None);
        assert_eq!(badge([&limits], NOW + 7_800).unwrap().text, "≥18%");
        // Both reset: no badge.
        assert_eq!(badge([&limits], NOW + 400_000), None);
    }

    #[test]
    fn badge_takes_the_highest_across_providers() {
        let claude = claude_limits(&claude_info(), NOW);
        let mut raw = codex_rate_limits();
        raw["primary"]["used_percent"] = json!(91.0);
        let codex = codex_limits(&raw, NOW - 3_600).unwrap();
        let b = badge([&claude, &codex], NOW).unwrap();
        assert_eq!(b.text, "≥91%");
        assert_eq!(b.level, Level::Danger);
        raw["primary"]["used_percent"] = json!(80.0);
        let codex = codex_limits(&raw, NOW).unwrap();
        let b = badge([&claude, &codex], NOW).unwrap();
        assert_eq!((b.text.as_str(), b.level), ("80%", Level::Warning));
        assert_eq!(badge(std::iter::empty::<&PlanLimits>(), NOW), None);
    }

    #[test]
    fn reset_and_freshness_words() {
        let tz = Utc;
        assert_eq!(format_reset(NOW + 7_800, NOW, &tz), "resets in 2h 10m");
        assert_eq!(format_reset(NOW + 600, NOW, &tz), "resets in 10m");
        assert_eq!(format_reset(NOW + 7_200, NOW, &tz), "resets in 2h");
        assert_eq!(format_reset(NOW + 7_170, NOW, &tz), "resets in 2h");
        // 1_800_000_000 is Fri 2027-01-15 08:00 UTC.
        assert_eq!(format_reset(NOW + 2 * 86_400, NOW, &tz), "resets Sun 08:00");
        assert_eq!(format_reset(NOW - 60, NOW, &tz), "reset Fri 07:59");
        let limits = claude_limits(&claude_info(), NOW);
        assert_eq!(freshness(&limits, NOW + 30, &tz), "updated just now");
        assert_eq!(freshness(&limits, NOW + 180, &tz), "updated 3m ago");
        assert_eq!(freshness(&limits, NOW + 3_600, &tz), "as of 08:00");
        assert_eq!(freshness(&limits, NOW + 86_400, &tz), "as of Fri 08:00");
        let lines = tooltip_lines(&limits, NOW + 3_600, &tz);
        assert_eq!(lines[0], "Claude Code 5h  ≥42% · resets in 1h 10m");
        let lines = tooltip_lines(&limits, NOW + 60, &tz);
        assert_eq!(
            lines,
            [
                "Claude Code 5h  42% · resets in 2h 9m",
                "Claude Code 7d  18% · resets Tue 23:06"
            ]
        );
        let lines = tooltip_lines(&limits, NOW + 8_000, &tz);
        assert_eq!(
            lines[0],
            "Claude Code 5h  no reading since · reset Fri 10:10"
        );
        assert_eq!(lines.last().unwrap(), "Claude Code as of 08:00");
    }

    #[test]
    fn persisted_file_round_trips_and_keeps_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(tmp.path());
        assert_eq!(PlanLimitsFile::load(&path), PlanLimitsFile::default());
        let mut file = PlanLimitsFile::default();
        let mut info = claude_info();
        info["status"] = json!("rejected");
        assert!(file.absorb(claude_limits(&info, NOW)));
        assert!(file.absorb(codex_limits(&codex_rate_limits(), NOW - 5).unwrap()));
        file.save(&path).unwrap();
        assert_eq!(PlanLimitsFile::load(&path), file);
        // An older reading does not replace a newer one; the same one is
        // no change.
        assert!(!file.absorb(claude_limits(&claude_info(), NOW - 1)));
        assert!(!file.absorb(file.claude.clone().unwrap()));
        assert!(file.absorb(claude_limits(&claude_info(), NOW + 1)));
        // A corrupt file loads empty.
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(PlanLimitsFile::load(&path), PlanLimitsFile::default());
        // Missing fields default.
        std::fs::write(&path, br#"{"codex":{"provider":"codex","observed_at":5}}"#).unwrap();
        let loaded = PlanLimitsFile::load(&path);
        assert_eq!(loaded.claude, None);
        assert!(loaded.codex.unwrap().windows.is_empty());
        assert!(PlanLimitsFile::load(&path).history.is_empty());
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    const NOW: i64 = 1_800_000_000;

    fn reading(provider: Provider, used: f64, left: i64) -> PlanLimits {
        PlanLimits {
            provider,
            observed_at: NOW,
            local: Some(true),
            windows: vec![LimitWindow {
                label: "7d".into(),
                used_percent: Some(used),
                resets_at: Some(NOW + left),
            }],
            alerts: vec![],
            plan_type: None,
            credits: None,
        }
    }
    fn step(file: &mut PlanLimitsFile, now: i64) -> PolicyEvaluation {
        let evaluation = evaluate_policy(file, now);
        file.policy = Some(evaluation.policy.clone());
        evaluation
    }
    fn fixture(used: f64, left: i64) -> PlanLimitsFile {
        let mut file = PlanLimitsFile::default();
        file.absorb(reading(Provider::Claude, used, left));
        file.absorb(reading(Provider::Codex, 10.0, 2 * 86400));
        file
    }

    #[test]
    fn first_burst_reset_anchor_preserves_hold_until_reset() {
        for provider in [Provider::Claude, Provider::Codex] {
            for used in [None, Some(1.0)] {
                let mut file = fixture(10.0, 2 * 86400);
                let reading = match provider {
                    Provider::Claude => file.claude.as_mut().unwrap(),
                    Provider::Codex => file.codex.as_mut().unwrap(),
                };
                reading.windows.push(LimitWindow {
                    label: "5h".into(),
                    used_percent: Some(100.0),
                    resets_at: None,
                });
                assert!(!step(&mut file, NOW).allows_start(provider, false, false));

                let reading = match provider {
                    Provider::Claude => file.claude.as_mut().unwrap(),
                    Provider::Codex => file.codex.as_mut().unwrap(),
                };
                reading.observed_at = NOW + 60;
                reading.windows[1].used_percent = used;
                reading.windows[1].resets_at = Some(NOW + 600);
                let held = step(&mut file, NOW + 60);
                assert_eq!(
                    held.report.routing.state,
                    match provider {
                        Provider::Claude => RoutingState::HoldClaude,
                        Provider::Codex => RoutingState::HoldCodex,
                    }
                );
                assert!(!held.allows_start(provider, false, false));
                let policy = match provider {
                    Provider::Claude => &held.policy.claude,
                    Provider::Codex => &held.policy.codex,
                };
                assert_eq!(policy.burst.resets_at, Some(NOW + 600));
                assert_eq!(
                    step(&mut file, NOW + 600).report.routing.state,
                    RoutingState::Balanced
                );
            }
        }
    }

    #[test]
    fn third_round_learning_reset_anchor_preserves_hold() {
        for used in [Some(1.0), None] {
            let mut file = fixture(10.0, 2 * 86400);
            file.claude.as_mut().unwrap().windows.clear();
            file.claude.as_mut().unwrap().alerts.push(Alert {
                kind: AlertKind::Overage,
                until: None,
                window: None,
            });
            assert_eq!(
                step(&mut file, NOW).report.routing.state,
                RoutingState::HoldClaude
            );
            let mut fresh = reading(Provider::Claude, 1.0, 2 * 86400);
            fresh.observed_at = NOW + 60;
            fresh.windows[0].used_percent = used;
            file.claude = Some(fresh);
            let e = step(&mut file, NOW + 60);
            assert_eq!(e.report.routing.state, RoutingState::HoldClaude);
            assert!(!e.allows_start(Provider::Claude, false, false));
            let weekly = e.policy.claude.weekly.unwrap();
            assert_eq!(weekly.resets_at, Some(NOW + 2 * 86400));
            assert_eq!(weekly.since, NOW);
            assert!(e.notices.is_empty());
            // Once known, a genuinely changed anchor releases the hold.
            file.claude.as_mut().unwrap().windows[0].resets_at = Some(NOW + WEEK_SECS);
            assert_eq!(
                step(&mut file, NOW + 60).report.routing.state,
                RoutingState::Balanced
            );
        }
    }

    #[test]
    fn third_round_hot_retains_balanced_provider_across_restart() {
        for target in [Provider::Claude, Provider::Codex] {
            let mut file = fixture(10.0, 2 * 86400);
            file.codex = Some(reading(Provider::Codex, 25.0, 5 * 86400));
            if target == Provider::Codex {
                std::mem::swap(&mut file.claude, &mut file.codex);
                file.claude.as_mut().unwrap().provider = Provider::Claude;
                file.codex.as_mut().unwrap().provider = Provider::Codex;
            }
            let phrase = if target == Provider::Claude {
                "Flexible jobs to Claude"
            } else {
                "Flexible jobs to Codex"
            };
            assert!(step(&mut file, NOW).report.routing.reason.contains(phrase));
            let hot = if target == Provider::Claude {
                file.codex.as_mut().unwrap()
            } else {
                file.claude.as_mut().unwrap()
            };
            hot.windows[0].used_percent = Some(35.0);
            hot.observed_at = NOW + 60;
            let e = step(&mut file, NOW + 60);
            assert_eq!(e.report.routing.state, RoutingState::Balanced);
            assert!(
                e.report.routing.reason.contains(phrase),
                "{}",
                e.report.routing.reason
            );
            // Even if the previous target becomes hotter and the ahead
            // comparison would now favor the other plan, hot cannot switch it.
            let preferred = if target == Provider::Claude {
                file.claude.as_mut().unwrap()
            } else {
                file.codex.as_mut().unwrap()
            };
            preferred.windows[0].used_percent = Some(90.0);
            preferred.observed_at = NOW + 60;
            assert!(step(&mut file, NOW + 60)
                .report
                .routing
                .reason
                .contains(phrase));
            let dir = tempfile::tempdir().unwrap();
            let path = PlanLimitsFile::path_in(dir.path());
            file.save(&path).unwrap();
            assert!(evaluate_policy(&PlanLimitsFile::load(&path), NOW + 60)
                .report
                .routing
                .reason
                .contains(phrase));
        }
    }

    #[test]
    fn third_round_busy_pin_keeps_target_and_prohibits_parallel_workers() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut file = fixture(10.0, 2 * 86400);
            file.pin = Some(RoutingPin {
                provider,
                until: NOW + 7200,
            });
            let busy = if provider == Provider::Claude {
                file.claude.as_mut().unwrap()
            } else {
                file.codex.as_mut().unwrap()
            };
            busy.windows.push(LimitWindow {
                label: "5h".into(),
                used_percent: Some(80.0),
                resets_at: Some(NOW + 7200),
            });
            let e = evaluate_policy(&file, NOW);
            assert_eq!(
                e.report.routing.state,
                if provider == Provider::Claude {
                    RoutingState::LeanClaude
                } else {
                    RoutingState::LeanCodex
                }
            );
            assert!(
                e.report
                    .routing
                    .reason
                    .contains("start no new parallel worker"),
                "{}",
                e.report.routing.reason
            );
            assert!(e.allows_start(provider, false, false));
            assert!(!e.allows_start(provider, true, false));
            assert!(e.allows_start(provider, true, true));
        }
    }

    #[test]
    fn policy_examples_and_early_week_guard() {
        for (used, left, pace, state) in [
            (85.0, 5 * 86400, 297.5, WeeklyState::Hot),
            (88.0, 2 * 86400, 123.2, WeeklyState::Hot),
            (8.0, WEEK_SECS - 12 * 3600, 80.0, WeeklyState::OnTrack),
            (24.0, WEEK_SECS - 86400, 168.0, WeeklyState::Hot),
        ] {
            let evaluation = evaluate_policy(&fixture(used, left), NOW);
            let window = &evaluation.report.claude.windows[0];
            assert!((window.pace.unwrap() - pace).abs() < 0.01);
            assert_eq!(window.state, Some(state));
            let f = window.elapsed_fraction.unwrap();
            assert!((window.ahead.unwrap() - (used - 90.0 * f)).abs() < 0.01);
            assert!((window.days_left.unwrap() - (100.0 - used) * f * 7.0 / used).abs() < 0.01);
            assert_eq!(evaluation.report.routing.state, RoutingState::Balanced);
        }
    }

    #[test]
    fn policy_hot_leave_requires_fresh_reading_and_six_hour_dwell() {
        let mut file = fixture(40.0, 5 * 86400);
        step(&mut file, NOW);
        for (elapsed, fresh, expected) in [
            (3600, true, WeeklyState::Hot),
            (STATE_DWELL_SECS, false, WeeklyState::Hot),
            (STATE_DWELL_SECS, true, WeeklyState::OnTrack),
        ] {
            let mut reading = reading(Provider::Claude, 10.0, 5 * 86400);
            reading.observed_at = if fresh { NOW + elapsed } else { NOW };
            file.claude = Some(reading);
            let e = step(&mut file, NOW + elapsed);
            assert_eq!(e.report.claude.windows[0].state, Some(expected));
        }
    }

    #[test]
    fn policy_almost_out_holds_until_fresh_new_window() {
        let mut file = fixture(95.0, 2 * 86400);
        let e = step(&mut file, NOW);
        assert_eq!(e.report.routing.state, RoutingState::HoldClaude);
        let since = e.report.routing.since;
        file.claude.as_mut().unwrap().windows[0].used_percent = Some(1.0);
        let e = step(&mut file, NOW + STATE_DWELL_SECS);
        assert_eq!(e.report.routing.state, RoutingState::HoldClaude);
        assert_eq!(e.report.routing.since, since);
        // Even a low fresh reading in the same week cannot release almost_out.
        file.claude.as_mut().unwrap().observed_at = NOW + STATE_DWELL_SECS;
        assert_eq!(
            step(&mut file, NOW + STATE_DWELL_SECS).report.routing.state,
            RoutingState::HoldClaude
        );
        file.claude = Some(reading(Provider::Claude, 1.0, WEEK_SECS));
        file.claude.as_mut().unwrap().observed_at = NOW + STATE_DWELL_SECS;
        let e = step(&mut file, NOW + STATE_DWELL_SECS);
        assert_eq!(e.report.routing.state, RoutingState::Balanced);
        assert!(e
            .notices
            .iter()
            .any(|line| line.contains("recovered") || line.contains("reset")));
    }

    #[test]
    fn policy_limit_alerts_and_conserve() {
        let mut file = fixture(30.0, 2 * 86400);
        file.claude.as_mut().unwrap().alerts.push(Alert {
            kind: AlertKind::Overage,
            until: None,
            window: None,
        });
        assert_eq!(
            step(&mut file, NOW).report.routing.state,
            RoutingState::HoldClaude
        );
        file.codex.as_mut().unwrap().windows[0].used_percent = Some(95.0);
        assert_eq!(
            step(&mut file, NOW).report.routing.state,
            RoutingState::Conserve
        );
    }

    #[test]
    fn policy_busy_hysteresis_and_short_hold_do_not_change_weekly_state() {
        let mut file = fixture(10.0, 2 * 86400);
        file.claude.as_mut().unwrap().windows.push(LimitWindow {
            label: "5h".into(),
            used_percent: Some(80.0),
            resets_at: Some(NOW + 3600),
        });
        assert!(step(&mut file, NOW).report.claude.busy);
        file.claude.as_mut().unwrap().windows[1].used_percent = Some(60.0);
        assert!(step(&mut file, NOW).report.claude.busy);
        file.claude.as_mut().unwrap().windows[1].used_percent = Some(59.0);
        assert!(!step(&mut file, NOW).report.claude.busy);
        file.claude.as_mut().unwrap().windows[1].used_percent = Some(80.0);
        file.claude.as_mut().unwrap().windows[1].resets_at = Some(NOW + 1800);
        assert!(!step(&mut file, NOW).report.claude.busy);
        file.claude.as_mut().unwrap().windows[1].used_percent = Some(100.0);
        let e = step(&mut file, NOW);
        assert_eq!(e.report.claude.windows[0].state, Some(WeeklyState::OnTrack));
        assert_eq!(e.report.routing.state, RoutingState::HoldClaude);
        // A burst hold expires at its reset, unlike a weekly hold.
        assert_eq!(
            step(&mut file, NOW + 1800).report.routing.state,
            RoutingState::Balanced
        );
    }

    #[test]
    fn policy_hot_warning_once_then_once_inside_24h() {
        let mut file = fixture(35.0, 5 * 86400);
        assert_eq!(step(&mut file, NOW).notices.len(), 1);
        assert!(step(&mut file, NOW).notices.is_empty());
        file.claude.as_mut().unwrap().windows[0].used_percent = Some(85.0);
        assert_eq!(step(&mut file, NOW).notices.len(), 1);
        assert!(step(&mut file, NOW).notices.is_empty());
        // Entry already inside 24 hours is a single notice, not two.
        assert_eq!(
            evaluate_policy(&fixture(85.0, 5 * 86400), NOW)
                .notices
                .len(),
            1
        );
    }

    #[test]
    fn policy_persistence_is_additive_and_preserves_dwell() {
        let dir = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(dir.path());
        let mut file = fixture(35.0, 5 * 86400);
        step(&mut file, NOW);
        file.save(&path).unwrap();
        let loaded = PlanLimitsFile::load(&path);
        assert_eq!(loaded, file);
        assert!(evaluate_policy(&loaded, NOW).notices.is_empty());
        let mut old = serde_json::to_value(&file).unwrap();
        old.as_object_mut().unwrap().remove("policy");
        let old: PlanLimitsFile = serde_json::from_value(old).unwrap();
        assert!(old.policy.is_none());
        assert_eq!(old.claude, file.claude);
    }

    #[test]
    fn policy_tool_shape_and_unknown_capacity() {
        let report = evaluate_policy(&fixture(35.0, 5 * 86400), NOW).report;
        let value = serde_json::to_value(report).unwrap();
        for key in [
            "plan_type",
            "credits",
            "observed_at",
            "stale",
            "windows",
            "busy",
        ] {
            assert!(value["claude"].get(key).is_some(), "{key}");
        }
        for key in [
            "label",
            "used_percent",
            "resets_at",
            "resets_in",
            "elapsed_fraction",
            "pace",
            "recent_24h_rate",
            "projected_at_reset",
            "ahead",
            "days_left",
            "state",
        ] {
            assert!(value["claude"]["windows"][0].get(key).is_some(), "{key}");
        }
        for key in ["state", "since", "constrained", "pinned", "reason"] {
            assert!(value["routing"].get(key).is_some(), "{key}");
        }
        assert!(value["routing"]["pinned"].is_null());
        assert!(
            evaluate_policy(&PlanLimitsFile::default(), NOW)
                .report
                .routing
                .constrained
        );
        let mut file = fixture(35.0, 5 * 86400);
        file.claude.as_mut().unwrap().windows.clear();
        assert!(evaluate_policy(&file, NOW).report.routing.constrained);
    }
    #[test]
    fn policy_exact_leave_threshold_and_dwell_boundary() {
        let mut file = fixture(40.0, 5 * 86400);
        step(&mut file, NOW);
        let reset = NOW + 5 * 86400;
        for (elapsed, pace, expected) in [
            (STATE_DWELL_SECS - 1, 75.0, WeeklyState::Hot),
            (STATE_DWELL_SECS, 75.01, WeeklyState::Hot),
            (STATE_DWELL_SECS, 75.0, WeeklyState::OnTrack),
        ] {
            let at = NOW + elapsed;
            let f = 1.0 - (reset - at) as f64 / WEEK_SECS as f64;
            let reading = file.claude.as_mut().unwrap();
            reading.observed_at = at;
            reading.windows[0].used_percent = Some(pace * f);
            assert_eq!(
                step(&mut file, at).policy.claude.weekly.unwrap().state,
                expected
            );
        }
    }

    #[test]
    fn policy_stale_changed_anchor_cannot_relax_and_fresh_reset_bypasses_dwell() {
        let mut file = fixture(95.0, 2 * 86400);
        step(&mut file, NOW);
        file.claude.as_mut().unwrap().windows[0] = LimitWindow {
            label: "7d".into(),
            used_percent: Some(1.0),
            resets_at: Some(NOW + WEEK_SECS),
        };
        let at = NOW + STALE_AFTER_SECS + 1;
        assert_eq!(
            step(&mut file, at).report.routing.state,
            RoutingState::HoldClaude
        );
        file.claude.as_mut().unwrap().observed_at = at;
        let e = step(&mut file, at);
        assert_eq!(e.report.routing.state, RoutingState::Balanced);
        assert_eq!(e.policy.claude.weekly.unwrap().since, at);
        assert!(e.notices.iter().any(|line| line.contains("reset")));
    }

    #[test]
    fn policy_alert_attribution_for_swapped_codex_slots_and_claude_overage() {
        for (minutes, expected) in [(300, WeeklyState::OnTrack), (10080, WeeklyState::AlmostOut)] {
            let raw = serde_json::json!({ "limit_id":"codex", "rate_limit_reached_type":"primary",
                "primary":{"used_percent":10.0,"window_minutes":minutes,"resets_at":NOW+3600},
                "secondary":{"used_percent":10.0,"window_minutes":10080,"resets_at":NOW+2*86400} });
            let mut file = fixture(10.0, 2 * 86400);
            file.codex = codex_limits(&raw, NOW);
            let e = step(&mut file, NOW);
            assert_eq!(e.policy.codex.weekly.unwrap().state, expected);
            assert_eq!(e.report.routing.state, RoutingState::HoldCodex);
        }
        let raw = serde_json::json!({"isUsingOverage":true,"rateLimitType":"five_hour","resetsAt":NOW+3600,
            "unifiedWindows":{"five_hour":{"utilization":0.2,"resetsAt":NOW+3600},"seven_day":{"utilization":0.1,"resetsAt":NOW+2*86400}}});
        let mut file = fixture(10.0, 2 * 86400);
        let mut reading = claude_limits(&raw, NOW);
        reading.local = Some(true);
        assert_eq!(reading.alerts[0].window.as_deref(), Some("5h"));
        file.claude = Some(reading);
        let e = step(&mut file, NOW);
        assert_eq!(e.policy.claude.weekly.unwrap().state, WeeklyState::OnTrack);
        assert_eq!(e.report.routing.state, RoutingState::HoldClaude);
    }

    #[test]
    fn policy_unknown_reset_still_holds_at_95_and_legacy_provenance_is_unknown() {
        let mut file = fixture(95.0, 2 * 86400);
        file.claude.as_mut().unwrap().windows[0].resets_at = None;
        assert_eq!(
            step(&mut file, NOW).report.routing.state,
            RoutingState::HoldClaude
        );
        let mut file = fixture(95.0, 2 * 86400);
        file.claude.as_mut().unwrap().local = None;
        let e = step(&mut file, NOW);
        assert_eq!(e.report.claude.observed_at, None);
        assert!(e.report.routing.constrained);
        assert_eq!(e.report.routing.state, RoutingState::Balanced);
    }

    #[test]
    fn policy_hot_warning_rearms_only_for_new_week() {
        let mut file = fixture(85.0, 5 * 86400);
        assert_eq!(step(&mut file, NOW).notices.len(), 1);
        file.claude.as_mut().unwrap().windows[0].resets_at = Some(NOW + 4 * 86400);
        assert_eq!(step(&mut file, NOW).notices.len(), 1);
        assert!(step(&mut file, NOW).notices.is_empty());
    }

    #[test]
    fn history_samples_are_spaced_bounded_and_reset_with_the_anchor() {
        let mut file = PlanLimitsFile::default();
        file.absorb(reading(Provider::Claude, 10.0, 5 * 86400));
        let first = &file.history[0].samples;
        assert_eq!(first.len(), 1);
        let mut too_soon = reading(Provider::Claude, 12.0, 5 * 86400);
        too_soon.observed_at = NOW + HISTORY_SAMPLE_SECS - 1;
        file.absorb(too_soon);
        assert_eq!(file.history[0].samples.len(), 1);
        let mut spaced = reading(Provider::Claude, 14.0, 5 * 86400);
        spaced.observed_at = NOW + HISTORY_SAMPLE_SECS;
        file.absorb(spaced);
        assert_eq!(file.history[0].samples.len(), 2);
        let mut new_anchor = reading(Provider::Claude, 3.0, WEEK_SECS);
        new_anchor.observed_at = NOW + 2 * HISTORY_SAMPLE_SECS;
        file.absorb(new_anchor);
        assert_eq!(file.history.len(), 1);
        assert_eq!(file.history[0].samples.len(), 1);
        for index in 1..=800 {
            file.record_sample(
                Provider::Claude,
                "7d",
                Some(NOW + WEEK_SECS),
                NOW + 2 * HISTORY_SAMPLE_SECS + index * HISTORY_SAMPLE_SECS,
                index as f64,
            );
        }
        assert_eq!(file.history[0].samples.len(), 768);
        let newest = file.history[0].samples.last().unwrap().observed_at;
        assert!(file.history[0].samples[0].observed_at >= newest - HISTORY_RETENTION_SECS);
        let old: PlanLimitsFile =
            serde_json::from_value(serde_json::json!({"claude": null})).unwrap();
        assert!(old.history.is_empty());
    }

    #[test]
    fn history_drives_recent_rate_projection_and_warning_detail() {
        let mut file = fixture(85.0, 2 * 86400);
        let resets_at = file.claude.as_ref().unwrap().windows[0].resets_at;
        file.history = vec![WindowHistory {
            provider: Provider::Claude,
            label: "7d".into(),
            resets_at,
            samples: vec![
                UsageSample {
                    observed_at: NOW - 86_400,
                    used_percent: 45.0,
                },
                UsageSample {
                    observed_at: NOW,
                    used_percent: 85.0,
                },
            ],
        }];
        let evaluation = evaluate_policy(&file, NOW);
        let weekly = &evaluation.report.claude.windows[0];
        assert!((weekly.recent_24h_rate.unwrap() - 40.0).abs() < 0.01);
        assert!((weekly.projected_at_reset.unwrap() - 165.0).abs() < 0.01);
        assert!(evaluation.notices[0].contains("pace 119, last 24 h project 165"));
    }

    #[test]
    fn pin_sets_expires_clears_and_almost_out_overrides_it() {
        let mut file = fixture(35.0, 5 * 86400);
        let pin = RoutingPin {
            provider: Provider::Codex,
            until: NOW + 3600,
        };
        file.pin = Some(pin.clone());
        let dir = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(dir.path());
        file.save(&path).unwrap();
        let mut file = PlanLimitsFile::load(&path);
        assert_eq!(file.pin, Some(pin.clone()));
        let active = evaluate_policy(&file, NOW);
        assert_eq!(active.report.routing.state, RoutingState::LeanCodex);
        assert_eq!(
            active.report.routing.pinned,
            Some(serde_json::json!({"provider":"codex","until":NOW+3600}))
        );
        assert_eq!(active.pin, Some(pin));
        assert_eq!(
            evaluate_policy(&file, NOW + 3600).report.routing.state,
            RoutingState::Balanced
        );
        file.pin = None;
        assert_eq!(
            evaluate_policy(&file, NOW).report.routing.state,
            RoutingState::Balanced
        );
        file.pin = Some(RoutingPin {
            provider: Provider::Claude,
            until: NOW + 3600,
        });
        file.claude.as_mut().unwrap().windows[0].used_percent = Some(95.0);
        assert_eq!(
            evaluate_policy(&file, NOW).report.routing.state,
            RoutingState::HoldClaude
        );
        file.claude.as_mut().unwrap().windows[0].used_percent = Some(35.0);
        assert!(pin_change_notice(
            file.pin.as_ref(),
            NOW,
            &evaluate_policy(&file, NOW).report.routing
        )
        .contains("pin to Claude Code set"));
        assert!(
            pin_change_notice(None, NOW, &evaluate_policy(&file, NOW).report.routing)
                .contains("pin cleared.")
        );
    }

    #[test]
    fn launch_boundary_enforces_both_providers_weekly_and_burst_holds() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut file = fixture(10.0, 2 * 86400);

            // A busy provider yields the next job to verified spare capacity;
            // the explicit user's model still wins.
            (match provider {
                Provider::Claude => file.claude.as_mut().unwrap(),
                Provider::Codex => file.codex.as_mut().unwrap(),
            })
            .windows
            .push(LimitWindow {
                label: "5h".into(),
                used_percent: Some(80.0),
                resets_at: Some(NOW + 3600),
            });
            let e = evaluate_policy(&file, NOW);
            assert!(!e.allows_start(provider, false, false));
            assert!(!e.allows_start(provider, true, false));
            assert!(e.allows_start(provider, true, true));
            (match provider {
                Provider::Claude => file.claude.as_mut().unwrap(),
                Provider::Codex => file.codex.as_mut().unwrap(),
            })
            .windows[1]
                .used_percent = Some(100.0);
            assert!(!evaluate_policy(&file, NOW).allows_start(provider, false, false));
            (match provider {
                Provider::Claude => file.claude.as_mut().unwrap(),
                Provider::Codex => file.codex.as_mut().unwrap(),
            })
            .windows[0]
                .used_percent = Some(95.0);
            let e = evaluate_policy(&file, NOW);
            assert!(!e.allows_start(provider, false, false));
            assert!(e.allows_start(provider, true, true));
        }
    }

    #[test]
    fn unknown_and_stale_capacity_start_but_a_stale_hold_still_holds() {
        // No reading at all, and an idle provider's stale one: the run
        // starts and writes the fresh reading itself (TRU-232).
        let empty = evaluate_policy(&PlanLimitsFile::default(), NOW);
        assert!(empty.allows_start(Provider::Codex, false, false));
        let file = fixture(10.0, 2 * 86400);
        let stale = evaluate_policy(&file, NOW + STALE_AFTER_SECS + 1);
        assert!(stale.allows_start(Provider::Codex, false, false));
        assert!(evaluate_policy(&file, NOW).allows_start(Provider::Codex, false, false));

        // A reading that said the 5h window was used up holds even once it
        // is stale, until a newer reading or its reset says otherwise.
        let mut file = fixture(10.0, 2 * 86400);
        file.codex.as_mut().unwrap().windows.push(LimitWindow {
            label: "5h".into(),
            used_percent: Some(100.0),
            resets_at: Some(NOW + 3 * 3600),
        });
        step(&mut file, NOW);
        let stale = step(&mut file, NOW + STALE_AFTER_SECS + 1);
        assert!(!stale.allows_start(Provider::Codex, false, false));
    }

    #[test]
    fn hot_reset_shortcut_sends_work_away_until_the_hot_plan_resets() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut file = fixture(35.0, 5 * 86400);
            if provider == Provider::Codex {
                file.codex = Some(reading(Provider::Codex, 35.0, 5 * 86400));
                file.claude = Some(reading(Provider::Claude, 10.0, 2 * 86400));
            }
            let report = evaluate_policy(&file, NOW).report;
            let hot = match provider {
                Provider::Claude => &report.claude,
                Provider::Codex => &report.codex,
            };
            let pin = hot_reset_pin(provider, hot, NOW).unwrap();
            assert_ne!(pin.provider, provider);
            assert_eq!(pin.until, NOW + 5 * 86400);
            file.pin = Some(pin);
            let expected = match provider {
                Provider::Claude => RoutingState::LeanCodex,
                Provider::Codex => RoutingState::LeanClaude,
            };
            assert_eq!(evaluate_policy(&file, NOW).report.routing.state, expected);
        }
    }

    #[test]
    fn one_click_reset_pin_is_available_only_while_weekly_hot() {
        let hot = evaluate_policy(&fixture(85.0, 5 * 86400), NOW)
            .report
            .claude;
        assert_eq!(hot.windows[0].state, Some(WeeklyState::Hot));
        assert_eq!(hot_pin_until_reset(&hot, NOW), Some(NOW + 5 * 86400));
        let normal = evaluate_policy(&fixture(10.0, 5 * 86400), NOW)
            .report
            .claude;
        assert_eq!(hot_pin_until_reset(&normal, NOW), None);
        let mut expired = hot;
        expired.windows[0].resets_at = Some(NOW);
        assert_eq!(hot_pin_until_reset(&expired, NOW), None);
    }
    #[test]
    fn policy_both_hot_warns_together_without_switching_and_balanced_margin_is_inclusive() {
        let mut file = fixture(35.0, 5 * 86400);
        file.codex = Some(reading(Provider::Codex, 50.0, 4 * 86400));
        let e = step(&mut file, NOW);
        assert_eq!(e.report.routing.state, RoutingState::Balanced);
        assert_eq!(e.notices.len(), 1);
        assert!(e.notices[0].contains("Both plans"));
        assert!(e.notices[0].contains("Codex first"));
        assert!(step(&mut file, NOW).notices.is_empty());
        let mut file = fixture(10.0, 2 * 86400);
        file.codex.as_mut().unwrap().windows[0].used_percent = Some(20.0);
        assert!(evaluate_policy(&file, NOW)
            .report
            .routing
            .reason
            .contains("roles table"));
        file.codex.as_mut().unwrap().windows[0].used_percent = Some(20.01);
        assert!(evaluate_policy(&file, NOW)
            .report
            .routing
            .reason
            .contains("Flexible jobs to Claude"));
    }
    #[test]
    fn policy_burst_limits_with_missing_reset_and_overlapping_holds() {
        let mut file = fixture(10.0, 2 * 86400);
        file.claude.as_mut().unwrap().windows.push(LimitWindow {
            label: "5h".into(),
            used_percent: Some(100.0),
            resets_at: None,
        });
        let e = step(&mut file, NOW);
        assert_eq!(e.report.routing.state, RoutingState::HoldClaude);
        assert!(e.report.routing.reason.contains("reset unknown"));
        let mut file = fixture(10.0, 2 * 86400);
        file.claude.as_mut().unwrap().alerts.push(Alert {
            kind: AlertKind::LimitReached {
                which: Some("5h".into()),
            },
            window: Some("5h".into()),
            until: Some(NOW + 3600),
        });
        let e = step(&mut file, NOW);
        assert_eq!(e.report.claude.windows[1].resets_in, Some(3600));
        assert!(e.report.routing.reason.contains("5h, resets in 1h"));
        let mut file = fixture(95.0, 2 * 86400);
        file.claude.as_mut().unwrap().windows.push(LimitWindow {
            label: "5h".into(),
            used_percent: Some(100.0),
            resets_at: Some(NOW + 3600),
        });
        let e = step(&mut file, NOW);
        assert!(e.report.routing.reason.contains("7d,"));
        assert_eq!(
            step(&mut file, NOW + 3600).report.routing.state,
            RoutingState::HoldClaude
        );
    }
    #[test]
    fn policy_a_fresh_new_anchor_discards_old_state_even_when_usage_is_unknown() {
        let mut file = fixture(95.0, 2 * 86400);
        step(&mut file, NOW);
        let window = &mut file.claude.as_mut().unwrap().windows[0];
        window.resets_at = Some(NOW + WEEK_SECS);
        window.used_percent = None;
        let e = step(&mut file, NOW);
        assert!(e.policy.claude.weekly.is_none());
        assert_eq!(e.report.routing.state, RoutingState::Balanced);
        assert!(e.report.routing.constrained);
        assert!(e
            .notices
            .iter()
            .any(|line| line.contains("usage is unknown")));
    }
}
