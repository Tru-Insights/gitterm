use super::*;

#[test]
fn fixture_usage_scan_runs_off_thread_and_returns_report() {
    let fixture = tempfile::tempdir().unwrap();
    let sources = gitterm_core::usage::UsageSources::under(fixture.path());
    for path in [
        &sources.claude_projects,
        &sources.codex_sessions,
        &sources.pi_sessions,
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let fixture_transcript = sources.claude_projects.join("-fixture/session.jsonl");
    std::fs::create_dir_all(fixture_transcript.parent().unwrap()).unwrap();
    std::fs::write(fixture_transcript, "{}\n").unwrap();
    let caller = std::thread::current().id();
    let (scanner, report) = std::thread::spawn(move || {
        let scanner = std::thread::current().id();
        let report = gitterm_core::usage::scan_usage_local(
            &sources,
            UsageWindow::Days7,
            &gitterm_core::usage::Pricing::seeded(),
            &mut gitterm_core::usage::UsageCache::default(),
        );
        (scanner, report)
    })
    .join()
    .unwrap();

    assert_ne!(scanner, caller);
    assert_eq!(report.window, UsageWindow::Days7);
    assert_eq!(report.stats.files_seen, 1);
}

#[test]
fn stale_usage_scan_window_requests_a_follow_up() {
    assert_eq!(
        follow_up_window(UsageWindow::Days7, UsageWindow::Days90),
        Some(UsageWindow::Days90)
    );
    assert_eq!(
        follow_up_window(UsageWindow::Days90, UsageWindow::Days90),
        None
    );
}
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

fn fixture_input() -> UsageInput {
    let now = 1_800_000_000;
    let mut plan_limits = PlanLimitsFile::default();
    let mut claude = plan_limits::claude_limits(
        &serde_json::json!({
            "unifiedWindows": {
                "five_hour": {"utilization": 0.25, "resetsAt": now + 3600},
                "seven_day": {"utilization": 0.35, "resetsAt": now + 5 * 86400}
            }
        }),
        now,
    );
    claude.local = Some(true);
    plan_limits.absorb(claude);
    plan_limits.absorb(plan_limits::codex_limits(&serde_json::json!({
        "limit_id": "codex", "plan_type": "pro", "credits": {"balance": 62500},
        "primary": {"used_percent": 12.0, "window_minutes": 300, "resets_at": now + 7200},
        "secondary": {"used_percent": 20.0, "window_minutes": 10080, "resets_at": now + 6 * 86400}
    }), now).unwrap());
    UsageInput {
        report: Some(Box::new(fixture_report())),
        window: UsageWindow::Days7,
        plan_limits,
        now,
        ..UsageInput::default()
    }
}

#[test]
fn fixture_matches_iced_section_order_and_labels() {
    let lines = UsagePage::from_input(fixture_input()).display_lines();
    let mut previous = 0;
    for expected in [
        "Usage",
        "Plan limits",
        "Claude Code",
        "Codex",
        "Estimated cost: $0.42",
        "Processed: 3.0K",
        "Cached input: 300",
        "Uncached input: 2.1K",
        "Cache writes: 50",
        "Output: 550",
        "Daily cost",
        "By model",
        "Model · Cost · Share · Tokens",
        "By repo",
        "Repo · Cost · Share · Tokens",
        "2 transcripts · 3 responses · scanned in 1.2s",
    ] {
        let index = lines
            .iter()
            .position(|line| line == expected)
            .unwrap_or_else(|| panic!("missing {expected:?}: {lines:#?}"));
        assert!(index >= previous, "{expected} out of order: {lines:#?}");
        previous = index;
    }
    for expected in [
        "All repos",
        "7d",
        "30d",
        "90d",
        "Cost",
        "Tokens",
        "headless shown",
        "refresh",
        "5h · 25% · resets in 1h",
        "5h · 12% · resets in 2h",
        "by day",
        "Plan: pro  ·  Credits: 62,500",
        "7 days at full API list rates",
        "+ 1.0K tokens on unpriced models",
        "peak $0.42 / day",
        "Nov 18",
        "claude $0.42",
        "codex unpriced",
        "pi $0.00",
        "fixture-paid-model · $0.42 · 100% · 2.0K",
        "fixture-unpriced-model · unpriced · — · 1.0K",
        "fixture-repo · $0.42 · 100% · 2.0K",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "missing {expected:?}: {lines:#?}"
        );
    }
    // Iced uses the daily chart's legend for harnesses, and the core's row order.
    assert!(
        !lines
            .iter()
            .any(|line| line == "By harness" || line == "By day")
    );
}

#[test]
fn empty_report_has_both_empty_tables_and_unknown_plan_readings() {
    let mut report = fixture_report();
    report.all = empty_view();
    let lines = UsagePage::from_report(report).display_lines();
    assert_eq!(lines.iter().filter(|line| *line == EMPTY).count(), 2);
    for provider in [Provider::Claude, Provider::Codex] {
        assert!(lines.contains(&empty_limits(provider).into()));
    }
    assert!(lines.contains(&"Estimated cost: $0.00".into()));
    assert!(lines.contains(&"peak $0.00 / day".into()));
}

#[test]
fn initial_and_failed_scans_show_plan_limits_without_success_fixtures() {
    for state in [
        UsageState::Scanning,
        UsageState::Error("scan failed".into()),
    ] {
        let lines = UsagePage::new(state).display_lines();
        assert!(lines.contains(&"Plan limits".into()));
        assert!(lines.contains(&"Reading transcripts…".into()));
        assert!(!lines.iter().any(|line| line.contains("Estimated cost")));
        assert!(!lines.join(" ").to_lowercase().contains("fixture"));
        assert!(!lines.join(" ").to_lowercase().contains("sample"));
    }
    assert!(
        UsagePage::new(UsageState::Error("scan failed".into()))
            .display_lines()
            .contains(&"Usage scan failed: scan failed".into())
    );
    let source = include_str!("usage.rs");
    assert!(
        !source.contains("fixture_report")
            && !source.contains("fixture-")
            && !source.contains("sample-success")
    );
}

#[test]
fn repo_and_headless_filters_use_core_views_and_keep_all_repo_rows() {
    let mut input = fixture_input();
    let report = input.report.as_mut().unwrap();
    report.headless_responses = 1;
    report.interactive = empty_view();
    let key = Some(PathBuf::from("/fixture/repo"));
    let mut narrowed = empty_view();
    narrowed.totals = report.all.by_harness[0];
    narrowed.models = vec![report.all.models[0].clone()];
    report.all.per_repo.insert(key.clone(), narrowed);
    let mut page = UsagePage::from_input(input);
    page.select(&UsageIntent::SelectRepo(Some((
        key.clone(),
        "fixture-repo".into(),
    ))));
    let lines = page.display_lines();
    assert!(lines.contains(&"Processed: 2.0K".into()));
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("fixture-unpriced-model"))
    );
    assert!(lines.contains(&"fixture-repo · $0.42 · 100% · 2.0K".into()));
    assert_eq!(
        page.repo_intent(key.clone(), "fixture-repo".into()),
        UsageIntent::SelectRepo(None)
    );
    page.select(&UsageIntent::ToggleHeadless);
    let lines = page.display_lines();
    assert!(lines.contains(&"Processed: 0".into()));
    assert!(lines.contains(&"headless hidden".into()));
    assert!(lines.contains(&"1 headless responses (SDK, exec, harness subagents) hidden".into()));
    assert_eq!(lines.iter().filter(|line| *line == EMPTY).count(), 2);
    page.select(&UsageIntent::SelectRepo(None));
    assert!(page.display_lines().contains(&"All repos".into()));
}

#[test]
fn tokens_mode_changes_chart_legend_and_share_without_changing_costs_or_sort() {
    let mut page = UsagePage::from_input(fixture_input());
    page.select(&UsageIntent::ChartTokens(true));
    let lines = page.display_lines();
    for expected in [
        "Daily tokens",
        "peak 3.0K / day",
        "claude 2.0K",
        "codex 1.0K",
        "fixture-paid-model · $0.42 · 67% · 2.0K",
        "fixture-unpriced-model · unpriced · 33% · 1.0K",
    ] {
        assert!(
            lines.contains(&expected.into()),
            "missing {expected}: {lines:#?}"
        );
    }
    assert!(
        lines
            .iter()
            .position(|l| l.starts_with("fixture-paid-model"))
            .unwrap()
            < lines
                .iter()
                .position(|l| l.starts_with("fixture-unpriced-model"))
                .unwrap()
    );
    assert_eq!(
        chart_peak(&page.input.report.as_ref().unwrap().all.days, true),
        3000.
    );
}

#[test]
fn refresh_and_scan_errors_keep_previous_report_and_plan_limits() {
    let mut input = fixture_input();
    input.scanning = true;
    input.error = Some("read failed".into());
    let mut page = UsagePage::from_input(input);
    let before = page.input.clone();
    page.select(&UsageIntent::Refresh);
    assert_eq!(page.input, before);
    let lines = page.display_lines();
    for expected in [
        "scanning…",
        "Usage scan failed: read failed",
        "Plan limits",
        "Estimated cost: $0.42",
    ] {
        assert!(lines.contains(&expected.into()));
    }
    page.set_input(UsageInput::default());
    assert!(
        page.display_lines()
            .contains(&"Reading transcripts…".into())
    );
}

#[test]
fn plan_snapshot_is_read_only_and_distinguishes_fresh_stale_and_reset_windows() {
    let mut input = fixture_input();
    let before = input.plan_limits.clone();
    let snapshots = input.plan_limits.usage_snapshot(input.now);
    assert_eq!(input.plan_limits, before);
    assert_eq!(snapshots[0].freshness.as_deref(), Some("updated just now"));
    assert_eq!(snapshots[0].windows[0].percent, Some(25.));
    assert_eq!(snapshots[0].windows[0].daily.unwrap()[6], Some(25.));
    let stale = input.plan_limits.usage_snapshot(input.now + 1000);
    assert!(
        stale[0]
            .freshness
            .as_ref()
            .unwrap()
            .ends_with("may be higher now")
    );
    assert_eq!(stale[0].windows[0].figure, "≥25%");
    assert!(stale[0].windows[1].figure.starts_with('≈'));
    let reset = input.plan_limits.usage_snapshot(input.now + 3600);
    assert_eq!(reset[0].windows[0].figure, "–");
    assert_eq!(reset[0].windows[0].percent, None);
    assert!(reset[0].windows[0].reset.ends_with("; no reading since"));
    // Future observations and histories for a different reset are not daily use.
    input.plan_limits.history[0]
        .samples
        .push(plan_limits::UsageSample {
            observed_at: input.now + 100,
            used_percent: 99.,
        });
    assert_eq!(
        input.plan_limits.usage_snapshot(input.now)[0].windows[0]
            .daily
            .unwrap()[6],
        Some(25.)
    );
}

#[test]
fn selection_intents_do_not_change_plan_limits_or_start_io() {
    let mut page = UsagePage::from_input(fixture_input());
    let limits = page.input.plan_limits.clone();
    let intent = UsagePage::window_intent(UsageWindow::Days90);
    assert_eq!(intent, UsageIntent::SelectWindow(UsageWindow::Days90));
    page.select(&intent);
    assert_eq!(page.input.window, UsageWindow::Days90);
    page.select(&UsageIntent::PinChanged(Some(RoutingPin {
        provider: Provider::Codex,
        until: page.input.now + 3600,
    })));
    assert_eq!(page.input.plan_limits, limits);
    // A no-cwd repo is selectable, distinct from the absence of a filter.
    page.select(&UsageIntent::SelectRepo(Some((None, "(no cwd)".into()))));
    assert_eq!(
        page.repo_intent(None, "(no cwd)".into()),
        UsageIntent::SelectRepo(None)
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
