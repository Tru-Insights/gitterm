// Synthetic-only report contract, run unchanged before and after extraction.
use super::*;
use chrono::{NaiveDate, Utc};
use serde_json::{json, Value};

fn bucket_value(b: &Bucket) -> Value {
    json!({"tokens": [b.tokens.uncached_input, b.tokens.cache_read,
        b.tokens.cache_write, b.tokens.cache_write_1h, b.tokens.output],
        "cost_bits": b.cost.to_bits(), "cost": format_cost(b.cost),
        "unpriced": b.unpriced_tokens, "responses": b.responses})
}

fn view_value(v: &UsageView) -> Value {
    json!({"totals": bucket_value(&v.totals),
        "harness": v.by_harness.iter().map(bucket_value).collect::<Vec<_>>(),
        "days": v.days.iter().filter(|d| d.by_harness.iter().any(|b| b.responses > 0))
            .map(|d| json!({"date": d.date, "harness": d.by_harness.iter().map(bucket_value).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "day_count": v.days.len(),
        "models": v.models.iter().map(|m| json!({"model": m.model,
            "harness": m.harness.label(), "priced": m.priced, "usage": bucket_value(&m.usage)})).collect::<Vec<_>>(),
        "repos": v.repos.iter().map(|r| json!({"path": r.path, "name": r.name,
            "resolved": r.resolved, "usage": bucket_value(&r.usage),
            "view": v.for_repo(&r.path).map(view_value)})).collect::<Vec<_>>()})
}

#[test]
fn synthetic_report_bytes_and_utc_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let sources = UsageSources::under(dir.path());
    for (root, lines) in [
        (&sources.claude_projects, include_str!("../fixtures/usage/claude/projects/-nonexistent-synthetic-alpha/s-claude.jsonl")),
        (&sources.codex_sessions, include_str!("../fixtures/usage/codex/sessions/2026/10/05/rollout-2026-10-05T08-00-00-synthetic.jsonl")),
        (&sources.pi_sessions, include_str!("../fixtures/usage/pi/sessions/--nonexistent-synthetic-gamma--/s-pi.jsonl")),
    ] {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("fixture.jsonl"), lines).unwrap();
    }
    std::fs::write(
        sources.codex_sessions.join("rollout-headless.jsonl"),
        include_str!("../fixtures/stage45/usage/rollout-fixture.jsonl")
            .replace("2000-01-03", "2026-10-07")
            .replace("/fixture/repo", "/nonexistent/synthetic/headless"),
    )
    .unwrap();
    // Both edges of the 7-day UTC window, and both sides of midnight.
    let mut boundaries = String::new();
    for (i, at) in [
        "2026-09-30T23:59:59.999Z",
        "2026-10-01T00:00:00Z",
        "2026-10-06T23:59:59.999Z",
        "2026-10-07T00:00:00Z",
        "2026-10-07T23:59:59.999Z",
        "2026-10-08T00:00:00Z",
    ]
    .iter()
    .enumerate()
    {
        let line = json!({"type":"assistant", "timestamp":at, "entrypoint":"sdk-cli",
            "message":{"id":format!("boundary-{i}"), "model":"claude-opus-5-5",
                "usage":{"input_tokens":i+1, "output_tokens":10, "cache_read_input_tokens":20,
                    "cache_creation_input_tokens":30, "cache_creation":{"ephemeral_1h_input_tokens":10}}}});
        boundaries.push_str(&format!("{line}\n"));
    }
    std::fs::write(sources.claude_projects.join("boundaries.jsonl"), boundaries).unwrap();
    let today = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
    let mut rows = Vec::new();
    let mut cache = UsageCache::default();
    for config in [UsageConfig::default(), serde_json::from_str(r#"{"pricing":{"claude-opus-5-5":null,"claude-mystery-9":{"input":1.0,"output":2.0,"cache_read":0.5,"cache_write":1.25}}}"#).unwrap()] {
        for window in UsageWindow::ALL {
            let report = scan_usage_in(&sources, window, today, &Utc, &Pricing::with_overrides(&config), &mut cache);
            // Explicitly pin today's inclusivity; no UTC midnight rounding changes.
            if window == UsageWindow::Days7 {
                assert_eq!(report.headless_responses, 6);
                assert_eq!(report.all.totals.responses, 13);
            }
            rows.push(json!({"window":window.label(), "start":report.start, "end":report.end,
                "all":view_value(&report.all), "interactive":view_value(&report.interactive),
                "headless":report.headless_responses}));
        }
    }
    let bytes = serde_json::to_vec(&rows).unwrap();
    assert_eq!(
        bytes,
        include_bytes!("../fixtures/usage-parity/report.json")
    );
}

#[test]
fn fixed_offset_midnight_keeps_existing_day_buckets() {
    let dir = tempfile::tempdir().unwrap();
    let sources = UsageSources::under(dir.path());
    std::fs::create_dir_all(&sources.claude_projects).unwrap();
    let lines = [
        "2026-09-30T21:59:59Z",
        "2026-09-30T22:00:00Z",
        "2026-10-06T21:59:59Z",
        "2026-10-06T22:00:00Z",
        "2026-10-07T21:59:59Z",
        "2026-10-07T22:00:00Z",
    ]
    .iter()
    .enumerate()
    .map(|(i, at)| {
        format!(
            "{}\n",
            json!({"type":"assistant", "timestamp":at,
        "message":{"id":format!("offset-{i}"),"model":"synthetic-unpriced",
            "usage":{"output_tokens":10}}})
        )
    })
    .collect::<String>();
    std::fs::write(sources.claude_projects.join("offset.jsonl"), lines).unwrap();
    let report = scan_usage_in(
        &sources,
        UsageWindow::Days7,
        NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
        &chrono::FixedOffset::east_opt(2 * 3600).unwrap(),
        &Pricing::seeded(),
        &mut UsageCache::default(),
    );
    assert_eq!(report.all.totals.unpriced_tokens, 40);
    assert_eq!(report.all.totals.responses, 4);
    assert_eq!(
        report
            .all
            .days
            .iter()
            .map(|d| d.by_harness[0].tokens.output)
            .collect::<Vec<_>>(),
        [10, 0, 0, 0, 0, 10, 20]
    );
}
