# Usage extraction parity (TRU-274)

`report.json` was captured from the pre-extraction `src/usage.rs` using only
hand-written synthetic transcripts. `tests/support/usage_parity.rs` is shared
by the Iced facade and core and was run unchanged before and after the move.

The deterministic bytes include exact `f64::to_bits()` costs, all token/cache
tiers, unpriced tokens, per-model/harness/day/repo slices, pricing overrides,
all three windows and both interactive/headless populations. UTC timestamps
straddle both window edges and midnight. Scan timing and filesystem metadata
are omitted; append/rewrite/deduplication behavior has separate existing tests.

The original local `scan_usage` wrapper includes today's local calendar day.
The shared scanner accepts the date and timezone explicitly; these fixtures
pin UTC and preserve today's inclusivity. No prior-day cutoff is introduced.
Do not regenerate this golden as part of an extraction.
