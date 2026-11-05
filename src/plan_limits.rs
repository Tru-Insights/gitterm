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

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A reading older than this is shown "as of" its time, not as current.
pub const STALE_AFTER_SECS: i64 = 15 * 60;
/// A window at or above this % is drawn in the warning colour.
pub const WARNING_PERCENT: f64 = 75.0;
/// A window at or above this % is drawn in the danger colour.
pub const DANGER_PERCENT: f64 = 90.0;
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
    let mut alerts = Vec::new();
    match info.get("status").and_then(Value::as_str) {
        None | Some("allowed") => {}
        Some("rejected") => alerts.push(Alert {
            kind: AlertKind::LimitReached {
                which: info
                    .get("rateLimitType")
                    .and_then(Value::as_str)
                    .map(claude_window_label),
            },
            until,
        }),
        Some(status) => alerts.push(Alert {
            kind: AlertKind::Status {
                status: status.to_string(),
            },
            until,
        }),
    }
    if info.get("isUsingOverage").and_then(Value::as_bool) == Some(true) {
        alerts.push(Alert {
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
// The persisted last readings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanLimitsFile {
    #[serde(default)]
    pub claude: Option<PlanLimits>,
    #[serde(default)]
    pub codex: Option<PlanLimits>,
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

    /// Takes `reading` unless the one held was observed later. True when
    /// anything changed.
    pub fn absorb(&mut self, reading: PlanLimits) -> bool {
        let slot = match reading.provider {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
        };
        match slot {
            Some(held) if held.observed_at > reading.observed_at || *held == reading => false,
            _ => {
                *slot = Some(reading);
                true
            }
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
    }
}
