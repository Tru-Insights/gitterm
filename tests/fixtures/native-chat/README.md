# TRU-274: chat C0 characterization

Baseline: `f6df7cae36ac8af2ef758b5619a86ef5f56a6972`. See [CALLERS.md](CALLERS.md) for pinned owner/consumer locations and remaining seams.

Everything in this directory is **synthetic**, composed for this task. No captured provider stream, transcript, account, credential, private name or real image is included. `QQ==` is one illustrative byte, not a screenshot. The device-token string is an inert fixture literal. No real agent, GUI or provider endpoint is needed. Production behavior is unchanged; source additions are test registration and test-only executable overrides.

| Directory | Inputs and behavior pinned |
| --- | --- |
| `claude/` | Stream-json text/thinking/input fragments, permission suggestions/reason, questions, nested child output, image result marker, failure and malformed tail; initialization/resume arguments, actual stdin frames, deny/question answer, interrupt, graceful shutdown, grace-period kill and reap; closed-stdin answer failure; model/mode/effort acknowledgements and rejections; local close vs remote detach; compact session replay and real headless Iced update trace. |
| `acp/` | Typed update mapper, text/thinking/tool/image output, offered permission options and answers, unsupported questions/subagents, malformed tail; initialize/new/load/prompt/answer/cancel/shutdown/forced kill/reap; history before Ready; host queue/withdraw order and refusal. |
| `pi/` | Actual per-turn argv and `--session` continuity; raw forwarded JSON versus filtered streaming/tool frames; thinking/tool/image content in turn-end; opaque subagent/permission/question/error records, malformed raw tail; stop, dropped receiver, nonzero exit and reap. Pi has no initialize/answer/shutdown wire protocol. |
| `phone/` | Actual normalized event envelopes, optional rate limits and image-count echo, accepted text/answer/interrupt commands and exact refusals; view/act scope; sequence, reconnect, standing redelivery, buffer gap and after-head repair via a fake registry. Hello decoding includes legacy missing `after_seq`; the existing fake-socket test now pins opened/history/live/refused/revoked frames. |
| `history/` | Synthetic Claude file load, timed/untimed and serde replay, image markers, metadata/sidechain filtering, early tool result, subagent completion and partial tail; result character cap; per-backend resume/history/queue eligibility. ACP replay is in `acp/resume*`; Pi deliberately has no restored timeline loader here. |

Tests live in `tests/native-chat/`, included as child modules of the original implementation so they exercise private code without widening runtime APIs. The fake executables run only in unique temporary directories. Claude uses the existing explicit-program seam; ACP/Pi overrides are thread-local and exist only under `cfg(test)`, without changing process-global PATH. Process tests require Unix and Python 3; portable mapper/JSON/session tests still compile on Windows. Every fake peer has a 20-second watchdog. Only temporary cwd, generated request IDs and wall-clock markers are normalized; semantic event order and fields remain exact.

The Iced trace calls real `App::update_event`: Pi submit to a fake input channel, stop, pending request order, answer without a process, resolution, history prepended to live data, duplicate ACP replay suppression, confirmed model/mode/effort, exit with pending input, last-tab close refusal and close with a sibling. Its subprocess sets a fresh `GITTERM_V6_CONFIG_DIR` before any config access. Returned Iced tasks are not run. No `src/main.rs` edits are needed. Native process-write dispatch coverage and retained socket checks are described precisely in CALLERS.md.

## Current oddities (preserved)

- Claude emits `RuntimeRequestResolved` **before** surfacing a failed answer write. `failed-answer.json` pins a real broken stdin pipe; resolution is not proof of successful delivery. ACP's write-then-resolve ordering has the same limitation in source. No retry/validation fixes belong in C0.
- Claude suppresses subagent stream deltas and uses complete assistant blocks, so those deltas do not become parent text. Claude's malformed tail becomes `Unparsed`; ACP logs/ignores it; Pi forwards it as a JSON string.
- ACP has no structured question or nested subagent update protocol. A Task tool remains an ordinary tool; unknown update kinds are ignored. Images become `[image]` markers on replay/display, and image capability gates outgoing blocks. Deny reason/edited input cannot be represented by the permission-option protocol.
- Pi drops `message_start`, `message_update`, `message_end`, `tool_execution_start` and `tool_execution_end`. It does not produce the unused typed Pi variants. A nonzero child status still produces `done`; dropping the stop sender also triggers the stop select arm. There is no native mid-turn answer/image input or transcript timeline load in this path.
- Claude history can emit an early tool result twice: once where encountered, again after its tool start. The fixture retains that ordering. Ordinary results are capped at 20,000 Unicode characters plus a truncation suffix; images restore markers, not bytes. File-read errors are returned by the parser API.
- `record` coalesces fragments and increments revision but does not independently reduce App state/pending requests. History load prepends before buffered live events; repeated ACP replay outside Loading is dropped. A ProcessExited event alone can leave pending requests until resolution events arrive. The last tab cannot be closed.
- A local handle drop sends Shutdown. A remote handle drop detaches; explicit close sends Shutdown first. Phone detach likewise leaves the registry-owned fake process alive. No auto-resend or protocol improvements are added.

## Preview feature inventory

Checked means inspected in the **existing HTML preview**, not implemented or visually verified in GPUI. References are baseline `assets/agent_chat.html` line numbers unless noted.

- [x] Literal user prompts; assistant Markdown, tables, highlighted code and code copy (1782–1855).
- [x] Streamed/redacted thinking, partial tool JSON, outputs and error cards (1858–2165).
- [x] Completed-turn folds, consecutive tool-run groups, exposed running/error/waiting activity; anchor-preserving folds (1450–1756).
- [x] Earlier history, retained offscreen rows, tail-follow/Latest, replay/reset and late updates (913–1038, 4249–4301).
- [x] Cmd+F literal case-insensitive search, count/next/previous, tool-output toggle, folded/earlier matches and Escape restoration (1080–1427).
- [x] Selection/plain copy, normal editing shortcuts, context copy and selected Copy as Markdown / Cmd+Shift+C (4320–4529).
- [x] Multiline autosized composer; Enter/Shift+Enter/IME; pending/send/stop gates; immediate draft/attachment clearing after submit (2335–2377, 2500).
- [x] Queue markers, withdraw, already-started refusal, unsent on exit (3943–4000).
- [x] Session slash commands/skills and keyboard completion (2526–2609).
- [x] Model aliases/default/context variant; effort Auto/applied/supported levels; acknowledged picker state (2722–2915).
- [x] Permission/mode picker and Shift+Tab; checkout choice/open/locking and guidelines indicator (728, 2614–2664, 2844–2936).
- [x] Ordered pending permissions; salient input/reason, allow-once, offered remember/ACP options and deny reason (3753–3815, 3895).
- [x] Multiple questions, headers/descriptions, single/multi-select, Other/free text, required answers and dismiss (3816–3894).
- [x] Paste/drop/remove/image-only input, thumbnails, 1568px downscale, JPEG .85/transparent PNG, four-image/5MiB caps (2378–2498, 4167).
- [x] Dictation insertion hook without send; host Ctrl+Space capture/status is in main.rs (4306; host behavior is not browser-test coverage).
- [x] Elapsed/token/cost activity; interrupted/failed/error/reset/restart states (2300–2334, 4012–4136).
- [x] Subagent/worker nesting, roster, status and reveal; external Codex activity projection (2166, 3413–3751).
- [x] Delegation progress/result/handoff, open/rerun/dismiss, selected findings, held/delivered state and stale-head warnings (3209–3403). Host cancel routing is outside the HTML.
- [x] Review/Consult form, reviewer/model, uncommitted/base/commit, focus/brief, cancel/keyboard submit (2978–3127).
- [x] Edit/MultiEdit/Write diffs, file/review navigation and external URL actions (1847, 2014–2128).
- [x] Recognized interactive HTML results, expansion and theme/open behavior (834–874).
- [x] Theme, font size, chat width, focus and host keyboard routing (817–843, 4472–4529).
- [x] Remote notice/echo/image counts and ACP capabilities/read-only legacy Gemini; raw Pi turn rendering (backend-fed hooks; 3972–4230).
- [x] V5 continuation offer/error (877–905); no legacy migration or live continuation is tested here.
- [x] Whole-reply copy remains a parity requirement in the design; the current HTML has code/selection copy but no separate per-reply action menu. Preserve this distinction when planning native work.

## Verification

Use plain cargo with `CARGO_TARGET_DIR=/private/tmp/gt-target-chatc0`, `RUST_TEST_THREADS=4`, `CARGO_BUILD_JOBS=4`, `CARGO_NET_OFFLINE=true` and an absolute temporary `GITTERM_V6_CONFIG_DIR` plus a temporary test-process `HOME` (existing workspace scanners use HOME directly). Keep cargo/rustup locations explicit when compiling with that environment. Do not point tests at installed app/provider roots. The existing workspace suite includes fake loopback transport tests; the new phone registry tests open no sockets.

```sh
cargo test --workspace -j 4 native_chat
cargo fmt --all -- --check
cargo clippy -j 4 -- -D warnings
cargo clippy -j 4 --features excalidraw -- -D warnings
cargo test --workspace -j 4
cargo build -j 4
```

Golden comparisons never rewrite fixtures. To change a baseline, inspect the input, implementation and resulting JSON together, and review the expected-file edit explicitly. GUI feel, browser layout, accessibility/IME, live agents, production-root sentinels and platform packaging are later gates, not claims of this characterization.
