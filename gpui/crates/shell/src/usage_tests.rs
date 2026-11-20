use super::*;
use gitterm_core::usage::{DayRow, ModelRow, RepoRow, ScanStats, Tokens, UsageView};
use std::{collections::HashMap, path::PathBuf, time::Duration};

fn fixture_report() -> UsageReport {
    let day = "2026-11-18".parse().unwrap();
    let paid = Bucket {
        tokens: Tokens {
            uncached_input: 1_200,
            cache_read: 300,
            cache_write: 50,
            cache_write_1h: 0,
            output: 450,
        },
        cost: 0.42,
        unpriced_tokens: 0,
        responses: 2,
    };
    let free = Bucket {
        tokens: Tokens {
            uncached_input: 900,
            output: 100,
            ..Tokens::default()
        },
        cost: 0.0,
        unpriced_tokens: 1_000,
        responses: 1,
    };
    let mut by_harness = [Bucket::default(); 3];
    by_harness[0] = paid;
    by_harness[1] = free;
    let mut by_day = [Bucket::default(); 3];
    by_day[0] = paid;
    by_day[1] = free;
    let view = UsageView {
        totals: Bucket {
            tokens: Tokens {
                uncached_input: 2_100,
                cache_read: 300,
                cache_write: 50,
                cache_write_1h: 0,
                output: 550,
            },
            cost: 0.42,
            unpriced_tokens: 1_000,
            responses: 3,
        },
        by_harness,
        days: vec![DayRow {
            date: day,
            by_harness: by_day,
        }],
        models: vec![
            ModelRow {
                model: "fixture-paid-model".into(),
                harness: ChatBackend::Claude,
                priced: true,
                usage: paid,
            },
            ModelRow {
                model: "fixture-unpriced-model".into(),
                harness: ChatBackend::Codex,
                priced: false,
                usage: free,
            },
        ],
        repos: vec![RepoRow {
            name: "fixture-repo".into(),
            path: Some(PathBuf::from("/fixture/repo")),
            resolved: true,
            usage: paid,
        }],
        per_repo: HashMap::new(),
    };
    UsageReport {
        window: UsageWindow::Days7,
        start: day,
        end: day,
        all: view.clone(),
        interactive: view,
        headless_responses: 0,
        stats: ScanStats {
            files_seen: 2,
            files_outside_window: 0,
            responses: 3,
            elapsed: Duration::from_millis(1250),
            ..ScanStats::default()
        },
    }
}

#[test]
fn renders_core_report_totals_and_each_breakdown() {
    let page = UsagePage::from_report(fixture_report());
    let lines = page.display_lines();
    for expected in [
        "Window: 7d",
        "Estimated cost: $0.42",
        "Processed: 3.0K",
        "Cached input: 300",
        "Uncached input: 2.1K",
        "Cache writes: 50",
        "Output: 550",
        "+ 1.0K tokens on unpriced models",
        "By harness",
        "claude · $0.42 · 2.0K tokens",
        "codex · unpriced · 1.0K tokens",
        "By model",
        "fixture-paid-model · claude · $0.42 · 2.0K tokens",
        "2026-11-18 · claude $0.42 · 2.0K tokens · codex unpriced · 1.0K tokens · pi $0.00 · 0 tokens",
        "fixture-repo · $0.42 · 2.0K tokens",
        "2 transcripts · 3 responses · scanned in 1.2s",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "missing {expected:?}: {lines:#?}"
        );
    }
}

#[test]
fn unpriced_models_are_explicitly_marked() {
    let page = UsagePage::from_report(fixture_report());
    assert!(page.display_lines().iter().any(|line| {
        line == "fixture-unpriced-model (unpriced) · codex · unpriced · 1.0K tokens"
    }));
}

#[test]
fn empty_report_has_empty_table_states() {
    let mut report = fixture_report();
    report.all = empty_view();
    let lines = UsagePage::from_report(report).display_lines();
    assert_eq!(
        lines
            .iter()
            .filter(|line| *line == "No usage in this window.")
            .count(),
        2
    );
}

#[test]
fn scanning_and_error_states_are_renderable() {
    assert!(
        UsagePage::new(UsageState::Scanning)
            .display_lines()
            .contains(&"Reading transcripts…".into())
    );
    assert!(
        UsagePage::new(UsageState::Error("fixture failure".into()))
            .display_lines()
            .contains(&"Usage scan failed: fixture failure".into())
    );
}

#[test]
fn window_selection_produces_typed_intent() {
    assert_eq!(
        UsagePage::window_intent(UsageWindow::Days90),
        UsageIntent::SelectWindow(UsageWindow::Days90)
    );
}

fn empty_view() -> UsageView {
    UsageView {
        totals: Bucket::default(),
        by_harness: [Bucket::default(); 3],
        days: vec![],
        models: vec![],
        repos: vec![],
        per_repo: HashMap::new(),
    }
}
