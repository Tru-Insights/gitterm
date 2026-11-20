//! Host facade for shared usage accounting and explicit-source scans.

pub use gitterm_core::usage::*;

use chrono::{Local, NaiveDate, TimeZone};
use std::path::{Path, PathBuf};

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

    fn core_sources(&self) -> gitterm_core::usage::UsageSources {
        gitterm_core::usage::UsageSources {
            claude_projects: self.claude_projects.clone(),
            codex_sessions: self.codex_sessions.clone(),
            pi_sessions: self.pi_sessions.clone(),
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

/// Explicit-calendar scan through the shared service.
pub fn scan_usage_in<Tz: TimeZone>(
    sources: &UsageSources,
    window: UsageWindow,
    today: NaiveDate,
    tz: &Tz,
    pricing: &Pricing,
    cache: &mut UsageCache,
) -> UsageReport {
    gitterm_core::usage::scan_usage_in(&sources.core_sources(), window, today, tz, pricing, cache)
}

#[cfg(test)]
#[path = "../tests/support/usage_parity.rs"]
mod parity;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chats::ChatBackend;

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
