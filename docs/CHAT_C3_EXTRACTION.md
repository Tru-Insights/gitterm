# TRU-274 C3 extraction evidence

Source baseline: `36d39648cca42ee5f663c0d9b0ff6c209037f32d` (`origin/pr/chat-c1`).

The Claude parser and frame codecs now live in
`crates/gitterm-core/src/harness/claude_parser.rs`. Transcript file loading,
parsing, desktop timeline construction and phone replay payloads live in
`crates/gitterm-core/src/chat/history.rs`. Existing root paths are facades.
A nonserialized `HistoryEvent::{Harness, Other}` is mapped exhaustively to
the desktop's binary-only `AgentEvent`; it introduces no wire schema.

History also needs the existing delivery-card detection/payload and delegation
anchor rules. Those leaves moved with root reexports. The minimal HTML result
contract and validator moved into `core/chat/html_render.rs` so recognized visual
results keep their complete documents. Preview/IPC/navigation remain root.

Path discovery, App history scheduling, local read-error logging/empty-history
behavior, remote errors, binary summary conversion, load/live merging and all
process services remain root adapters. No transcript migration, cache, resume
ownership change or C5 safety fix is included.

## Verbatim comparison

Compared each source block against the pinned baseline, reversing only module
paths, the `HistoryEvent` name, three cross-crate codec visibility changes and
fixture include paths. Formatting normalization ignores whitespace and trailing
commas; string, raw-string and character literals are also compared exactly
after those path substitutions. All comparisons pass.

| Moved block | Original lines | Comparison |
| --- | ---: | --- |
| Parser and outgoing codecs | 851 | Identical after mechanical changes |
| Original parser/codec tests (27 tests) | 954 | Identical after import/fixture paths |
| Transcript load/parser helpers | 418 | Identical after HTML validator path |
| Original transcript tests (11 tests) | 348 | Identical after HTML contract path |
| Desktop timeline algorithm | 61 | Identical after neutral event/helper paths |
| Delegation anchor rules | 82 | Identical |
| Delivery detection and cards | 18 + 29 | Identical |
| Prompt/harness/subagent payloads | 40 | Identical after delivery helper path |
| Turn timing payload | 12 | Identical |
| Phone history projection | 27 | Identical |
| HTML result contracts/recognition and validation | 26 + 17 | Identical |

The retained root process/remote/stdin/stdout/snapshot/answer block is **byte for
byte identical**: 1,675 lines, 69,320 bytes, SHA-256
`09611f2105468f924f2515bd053f0ee5cd228f22b0f30a33d9fc25c1a8f76bbc`.
This includes the known failed-answer-write `RuntimeRequestResolved` before
`Error` ordering, explicitly deferred to C5.

`tests/native-chat/` and the C0 `claude/`, `history/` and `phone/` fixture
directories have no diff against the baseline. Their root integration-test
registrations remain at root. Core keeps the original leaf tests and adds two
focused tests for delivery/anchor/timestamp ordering, distinct desktop/phone
projections and strict HTML-result validation.

## Lock delta

Core adds direct `chrono = "0.4"` and `schemars = "1.0"` dependencies, already
present in both dependency graphs. The root `Cargo.lock` adds only these two
names to the `gitterm-core` dependency list. Regenerating through
`scripts/cargo.sh metadata --manifest-path gpui/Cargo.toml` adds `chrono`,
`schemars`, `serde` and `serde_json` to that same list in `gpui/Cargo.lock`;
the serde edges were missing from the C1 base lock. No package or version
changes occur. No other GPUI files change. Core's plain dependency tree has no
Iced, GPUI, Wry or Tokio dependency.

## Reviews

Independent `gpt-6-astra` R2 plan consultation:
`a54b0e9f-d4b2-4413-905b-819660a1c5f8`, clean source baseline above. It required
preserving delivery metadata and retaining C0 root integration registrations;
both are included. It accepted the neutral history-event adapter and minimal
HTML contract leaf. R3 remains conditional: existing schemas, files, payloads
and ownership are unchanged.

Independent `gpt-6-astra` R2 code review:
`cc38678d-151b-41b0-aa3b-5a2e92aeb055`, uncommitted extraction against the
same C1 baseline. Verdict: correct, no findings or blockers. The reviewer
confirmed the unchanged process block, facades, history projections, validation
and C0 registrations, independently ran core tests (exit 0), and inspected the
passing verification logs including the Excalidraw rerun. Reviewed code-diff
SHA-256 (excluding this evidence report):
`5bfb7f226cb297762403a4e8b79cdd9d7265c784ed3a7578be999b4f4142bab3`.
No code changes followed review. Residual coverage gaps are Windows and live
GUI/provider behavior; known C5 fixes remain deferred.

At final integration check, remote `v6` is
`df1cb354cd64a33e9ef42ab7b48be2c373478337` and does not contain C1. The branch
therefore remains based on the requested C1 head; merging `v6` is conditional
on C1 landing. No push is performed.

## Verification

Root commands use plain Homebrew Cargo 1.95.0, `-j 4` where applicable,
`CARGO_TARGET_DIR=/private/tmp/gt-target-chatc3`, `RUST_TEST_THREADS=4`, and an
absolute temporary `HOME` and `GITTERM_V6_CONFIG_DIR`. GPUI commands use the
repository's `scripts/cargo.sh` stable-toolchain wrapper. `CLAUDE_CONFIG_DIR` is
unset; provider paths therefore resolve under the temporary home. No GUI or
real provider process is launched.

| Final command | Exit |
| --- | ---: |
| `cargo fmt --all -- --check` | 0 |
| `cargo clippy -j 4 -- -D warnings` | 0 |
| `cargo clippy -j 4 --features excalidraw -- -D warnings` | 0 |
| `cargo test --workspace -j 4` | 0 |
| `cargo test -p gitterm-core -j 4` | 0 |
| `cargo test -p gitterm-core --no-default-features -j 4` | 0 |
| `cargo test -p gitterm-core --features runtime -j 4` | 0 |
| `cargo test --features excalidraw -j 4` (full rerun) | 0 |
| `cargo build -j 4` | 0 |
| `scripts/cargo.sh metadata --manifest-path gpui/Cargo.toml --locked` | 0 |
| `scripts/check-gpui-deps.sh` | 0 |
| `scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml -- --check` | 0 |

Workspace results: core 76 unit + 2 parity tests, root library 860 tests,
desktop binary 513 tests and integration 2 tests pass. Existing ignored tests
remain ignored; no new ignores are added. Standalone core without runtime
passes 69 unit + 2 parity tests; with runtime, 76 + 2.

The first isolated baseline run exited 101 because setting
`CLAUDE_CONFIG_DIR` conflicts with an existing trust test's expected launch
configuration. Removing that override preserves isolation through `HOME`;
the complete workspace suite then exits 0.

The additional Excalidraw test suite first exited 101 when the unchanged C0 Pi
fixture exceeded its five-second first-output timeout. The exact compiled
targeted test then passed unchanged (exit 0), and the complete
`cargo test --features excalidraw -j 4` rerun passed (exit 0). The test and its
fixture were not modified. This remains an intermittent fixture-timing risk.

The installed commit hook invokes the rustup wrapper for root checks. To keep
the explicitly requested plain-Cargo root toolchain, commit uses a per-command
hook override after running every equivalent root and GPUI gate above directly.
