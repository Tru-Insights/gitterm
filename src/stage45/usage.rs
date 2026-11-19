use super::*;
use chrono::{NaiveDate, Utc};
use gitterm::usage::*;
fn report_value(report: &UsageReport) -> Value {
    let bucket = |b: &Bucket| {
        json!({"responses":b.responses,"cost":b.cost,"unpriced":b.unpriced_tokens,
        "tokens":[b.tokens.uncached_input,b.tokens.cache_read,b.tokens.cache_write,b.tokens.cache_write_1h,b.tokens.output]})
    };
    json!({"all":bucket(&report.all.totals),"interactive":bucket(&report.interactive.totals),"headless":report.headless_responses,
        "harness":report.all.by_harness.iter().map(bucket).collect::<Vec<_>>(),"days":report.all.days.len(),
        "models":report.all.models.iter().map(|m|json!({"model":m.model,"priced":m.priced,"usage":bucket(&m.usage)})).collect::<Vec<_>>(),
        "stats":{"seen":report.stats.files_seen,"parsed":report.stats.files_parsed,"cached":report.stats.files_cached,"resumed":report.stats.files_resumed,"duplicates":report.stats.duplicates}})
}
#[test]
fn usage_read_cache_append_rewrite_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let sources = UsageSources {
        claude_projects: dir.path().join("claude"),
        codex_sessions: dir.path().join("codex"),
        pi_sessions: dir.path().join("pi"),
    };
    for (base, name) in [
        (&sources.claude_projects, "claude.jsonl"),
        (&sources.codex_sessions, "rollout-fixture.jsonl"),
        (&sources.pi_sessions, "pi.jsonl"),
    ] {
        std::fs::create_dir_all(base.join("fixture")).unwrap();
        std::fs::write(
            base.join("fixture").join(name),
            fixture(&format!("usage/{name}")),
        )
        .unwrap();
    }
    let mut cache = UsageCache::default();
    let pricing = Pricing::seeded();
    let today = NaiveDate::from_ymd_opt(2000, 1, 7).unwrap();
    let mut rows = Vec::new();
    for _ in 0..2 {
        rows.push(report_value(&scan_usage_in(
            &sources,
            UsageWindow::Days7,
            today,
            &Utc,
            &pricing,
            &mut cache,
        )));
    }
    let path = sources.claude_projects.join("fixture/claude.jsonl");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(fixture("usage/append.jsonl").as_bytes())
        .unwrap();
    rows.push(report_value(&scan_usage_in(
        &sources,
        UsageWindow::Days7,
        today,
        &Utc,
        &pricing,
        &mut cache,
    )));
    std::fs::write(&path, fixture("usage/append.jsonl")).unwrap();
    let report = scan_usage_in(
        &sources,
        UsageWindow::Days7,
        today,
        &Utc,
        &pricing,
        &mut cache,
    );
    rows.push(report_value(&report));
    let empty = report.view_for(false, Some(&Some(PathBuf::from("/fixture/absent"))));
    assert_eq!(empty.totals.responses, 0);
    assert_eq!(empty.days.len(), 7);
    golden("usage/read.trace.json", json!(rows));
}
#[test]
fn iced_usage_single_scan_and_stale_window_completion() {
    if isolated(
        "iced_usage_single_scan_and_stale_window_completion",
        "usage",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    let _trace = Trace::start();
    app.usage_scanning = true;
    let _ = app.request_usage_scan();
    assert!(app.usage_scanning);
    app.usage_window = UsageWindow::Days90;
    let empty = UsageSources {
        claude_projects: dir.path().join("empty-c"),
        codex_sessions: dir.path().join("empty-x"),
        pi_sessions: dir.path().join("empty-p"),
    };
    let report = scan_usage_in(
        &empty,
        UsageWindow::Days7,
        NaiveDate::from_ymd_opt(2000, 1, 7).unwrap(),
        &Utc,
        &Pricing::seeded(),
        &mut UsageCache::default(),
    );
    let _ = app.update(Event::UsageScanned(Ok(report)));
    let stale = json!({"report":format!("{:?}",app.usage_report.as_ref().unwrap().window),"selected":format!("{:?}",app.usage_window),"rescanning":app.usage_scanning});
    let _ = app.update(Event::UsageScanned(Err("synthetic scan failure".into())));
    golden(
        "usage/iced.trace.json",
        json!({"stale_completion":stale,"failure":{"scanning":app.usage_scanning,"error":app.usage_error,"old_report_retained":app.usage_report.is_some()}}),
    );
}

#[test]
fn usage_existing_negative_repo_cache_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("claude");
    std::fs::create_dir(&projects).unwrap();
    let repo = dir.path().join("repo");
    std::fs::write(
        projects.join("fixture.jsonl"),
        fixture("usage/claude.jsonl").replace("/fixture/repo", &repo.display().to_string()),
    )
    .unwrap();
    let sources = UsageSources {
        claude_projects: projects,
        codex_sessions: dir.path().join("no-codex"),
        pi_sessions: dir.path().join("no-pi"),
    };
    let mut cache = UsageCache::default();
    let today = NaiveDate::from_ymd_opt(2000, 1, 7).unwrap();
    let pricing = Pricing::seeded();
    let before = scan_usage_in(
        &sources,
        UsageWindow::Days7,
        today,
        &Utc,
        &pricing,
        &mut cache,
    );
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let cached = scan_usage_in(
        &sources,
        UsageWindow::Days7,
        today,
        &Utc,
        &pricing,
        &mut cache,
    );
    let fresh = scan_usage_in(
        &sources,
        UsageWindow::Days7,
        today,
        &Utc,
        &pricing,
        &mut UsageCache::default(),
    );
    golden(
        "usage/repo-cache.trace.json",
        json!({"before_resolved":before.all.repos[0].resolved,"cached_resolved":cached.all.repos[0].resolved,"fresh_resolved":fresh.all.repos[0].resolved}),
    );
}
