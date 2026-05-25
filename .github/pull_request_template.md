<!--
Repo rules: see AGENTS.md. PR readiness gates are enforced via
`/pr-ready --pr <number>` per agent-workflow.config.json.
-->

## Summary

<!-- 1–3 bullets: what changed and why. Link the Linear issue (TRU-NN). -->

-

## Linked Issue

<!-- Required when issueTracking.requireIssueInPr is true. -->

- TRU-

## Test Plan

<!-- Bullet checklist of how reviewers / CI verify this PR. -->

- [ ]
- [ ]

## Verification Run

<!-- Configured verification.beforePrReady: check, test, build. -->

- [ ] `cargo fmt -- --check && cargo clippy -- -D warnings`
- [ ] `cargo test`
- [ ] `cargo build --release --features stt`

## Notes

<!-- Migrations, env vars, screenshots, follow-ups, rollout concerns. -->

## Review readiness

<!--
Filled in by `/pr-ready --pr <number>` once all blockers are clear.
Any new commit after this block lands stales the attestation and requires
rerunning explicit PR mode.
-->

- [ ] `/pr-ready --pr <number>` returned `ACCEPT-READY`
- Head SHA: <current 40-character PR head SHA>
- Reviewed at: <ISO-8601 UTC timestamp>
