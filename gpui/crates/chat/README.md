# Native chat interaction surface (TRU-288, parent TRU-274)

A fixture-driven native chat pane, independent of the root Iced/Wry package.
No harness processes, settings, home-directory transcript discovery, or
permission commands are started by this crate.

From the repository root:

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru288 scripts/cargo.sh build --manifest-path gpui/Cargo.toml --workspace --release --locked -j 4
/private/tmp/gt-target-tru288/release/gitterm-gpui --chat gpui/crates/chat/fixtures/showcase.jsonl
/private/tmp/gt-target-tru288/release/gitterm-gpui --chat gpui/crates/chat/fixtures/long-5000.jsonl
```

For a timed stream with interactive controls and intents printed to stderr:

```sh
/private/tmp/gt-target-tru288/release/gitterm-gpui --chat-live gpui/crates/chat/fixtures/showcase.jsonl
```

`--chat` remains a read-only recorded view. `run` selects live demo behavior
when invoked with `--chat-live`; the main entry changes only its fixture-mode
dispatch condition. The demo emits intents and never starts a harness or
executes a permission decision.

The viewer never reads the app's configuration, so it needs no configuration
seed. Any future integration that starts the full app must use an absolute,
isolated `GITTERM_V6_CONFIG_DIR`, as required by the repository instructions.
The ordinary no-argument stage-0 entry is preserved. Bad CLI arguments exit 2;
fixture loading errors exit 1 with the path and JSONL line number.

## Input and ownership

`events.rs` re-exports the canonical harness DTOs from
`gitterm_core::chat::types`. There is no native harness event mirror. Only the
unchanged fixture `Entry`/`Record` display envelopes remain local. The core
dependency enables no features; the reducer does not require `runtime`.

The loader accepts any of these JSONL line formats:

```json
{"type":"text_delta","data":"A normalized harness event."}
{"type":"user_prompt","data":"A transcript entry."}
{"at_ms":120,"entry":{"type":"harness","data":{"type":"text_delta","data":"A timed fixture entry."}}}
```

`at_ms` supplies recorded elapsed time, not a wall-clock timestamp. Missing
usage/time is labeled as unreported. Replay uses a fixed 35 ms event cadence
and the component's 280 ms stream fade, respecting its reduced-motion policy.

Two **fixture-only** `Entry` variants supply app-owned display context:
`display_decision {request_id, state: answered|denied, detail}` and
`display_delegation {id, title, state, body}`. These are not new harness events.
A real `runtime_request_resolved` alone says only that a request ended; it
never implies approval, denial, or an answer that was not recorded. Question
answers can be shown as the existing question-text → joined-label map, through the app adapter; this crate does not encode a harness protocol. Permission suggestions and ACP options
remain visible JSON passthrough.

`ChatModel` is a deterministic, disposable row projection with scoped tool/request lookup,
stream segmentation, and stable row IDs. Nested worker events cannot append to
the parent's assistant message. Unknown item references produce an explicit
incomplete-history error. Reset closes lookup scopes and adds a reset marker;
it does not reuse old row identities. The model is independent of rendered
rows. At most 96 recently visited rows retain rich-text and diff entities. Fold state
and card drafts/submissions survive rich-row eviction; rich-text selection on
evicted rows does not. Cross-row drag selection works across mounted rows through
gpui-component 0.9.1; it cannot span arbitrary unmounted/evicted history. Use
whole-reply Copy markdown for full-fidelity content after scrolling. Visited
rows own their rich-text selection and diff state; parsing completion remeasures the corresponding virtual row. No application data cache
is introduced. Replaying starts a fresh deterministic model and retains the
find query. Hiding a retained `ChatView` preserves its state.

## Behavior parity

References below use the migration plan's pinned `agent_chat.html` line numbers;
the current file has shifted. The parity target also includes TRU-256 find and
TRU-270 selected-text Markdown copy (the `pr/TRU-256` and `pr/TRU-270` history),
not just the older migration-plan audit.

| HTML behavior / reference | Status | Native behavior / boundary |
| --- | --- | --- |
| User and assistant messages; layout / `:863`, `:1003` | Done | Centered 960 px column, 15 px text, Catppuccin background/foreground/status colors; right-aligned user cards. |
| Markdown, tables, highlighted code / `:1003`, `:1090` | Done | Selectable component TextView with the `syntect` feature. Native table/code rendering. |
| Thinking / `:1319` | Done | Independent collapsible rows, including empty/redacted blocks. |
| Tool runs / `:1400`, `:1430` | Done | Stable-ID collapsible tool cards; input, streamed partial JSON, results and errors; running/done/error colors. |
| Edit, MultiEdit, Write / `:1519`, `:1580` | Done | Component Diff per replacement fragment; multiple edits supported. Original/result text is still selectable/searchable. No filesystem reads or invented full-file context. |
| Stream, replay, tail-follow / `:863`, `:945`, `:3501`, `:3731` | Done | Virtualized MessageScroller, model-stable IDs, retained folds, asynchronous remeasurement, Replay/Restart, stream fade, built-in jump-to-latest. Scroll-up suspends tail-follow. |
| Earlier history, reconnect, reset / `:945`, `:3501`, `:3731` | Display-only | All fixture history is in the model; history replay, reset markers, disconnect/reconnect errors and gap notice render. Live transport, fetching older history and repairing gaps are later. |
| Find in current chat / TRU-256 | Done | Cmd+F input, Enter/Cmd+G next, Cmd+Shift+G previous, Escape close; full model search including folded/unmounted rows; highlights and reveal; query retained during replay. Case-insensitive Unicode ranges remain valid byte boundaries. |
| Selection Cmd+C and right-click Copy | Done | Component plain-copy behavior retained, including outer-whitespace trimming; explicit context-menu handler uses the same policy. |
| Selected text Copy as Markdown / TRU-270 | Done | Right-click menu and Cmd+Shift+C use native selection Source format, preserving Markdown tables and code fences with language. Normal copy remains plain. Menu payloads are captured on opening, like HTML `:4427`, before popup focus changes; selection copy shows Copied for 1.2 seconds. |
| Whole-reply copy / TRU-274, `d7fe92b:1796`, `:1880` | Done | Assistant reply buttons copy complete model source as Markdown or plain text, independent of selection and prior row eviction; 1.2-second Copied feedback. |
| Code block Copy / `:1090` | Done | Copies code text only; per-code-block Copied feedback for 1.2 seconds (HTML `:1841`). |
| Question cards / `:3317` | Done | Single/multi choice, Other/free text, header and option descriptions, validated submit and dismiss. Answered/resolved cards are read-only. |
| Permission cards / `:3254`, `:3279` | Done | Allow / Allow always / Deny intents. Always passes the exact offered localSettings suggestion or ACP option. No invented rule or backend effect. |
| Workers / `:1671`, `:2668` | Display-only | Nested scope labels/indentation, worker lifecycle row, scoped child text/thinking/tools and usage. No worker roster/navigation commands. |
| Delegations / `:3048`, `:3087` | Display-only | Collapsible fixture/app delivery cards and results; no delivery or task commands. |
| Turn status / `:784`, `:2668` | Done | Recorded input/output tokens, elapsed time, optional cost; completed/interrupted/failed and explicit errors. |
| Review / `:2456`, `:2768` | Later | Review targets, findings, file opening, stale-head warnings, rerun/cancel/dismiss and held delivery require core/session integration. |
| Composer / `:1843`, `:1896` | Done | Per-view multiline draft; Enter sends, Shift+Enter inserts newline; visible read-only reason; Stop while a turn runs. Streaming Queue, withdraw by ID, slash completion and owner-fed status are provided by C11a below. Attachments, dictation and interactive model controls remain later. |
| HTML results / `:784`, `:3048` | Later | Text/code stays native; interactive HTML preview belongs in a retained Viewer surface. |

## Synthetic fixtures and verification

Both JSONL fixtures are generated by `python3 gpui/crates/chat/fixtures/generate.py`.
All prose/IDs are synthetic. Scenarios derive from the public synthetic
`tests/fixtures/claude`, `tests/fixtures/chats`, `tests/fixtures/acp`, and
`tests/fixtures/live_turns.js` contracts (uploader retries, partial Bash input,
question options, permission results, worker lifecycle). No real transcript or
private identity is used. The generator never reads outside its own directory.
`long-5000.jsonl` produces exactly 5,000 rows, with unique offscreen search markers.

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru288 scripts/cargo.sh test --manifest-path gpui/Cargo.toml --workspace --locked -j 4
CARGO_TARGET_DIR=/private/tmp/gt-target-tru288 scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --workspace --locked --all-targets -j 4 -- -D warnings
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml --all -- --check
scripts/check-gpui-deps.sh
```

The test-support features are enabled directly on `gpui-pre` and
`gpui-component` in dev-dependencies. Enabling the facade's platform-wide test
feature activates unnecessary optional backend resolution conflicts in the
pinned stack. The GPUI lockfile additions are test-harness dependencies; the
root lockfile is unchanged. `nice -n 10` was attempted, but this environment
denied `setpriority`, so subsequent commands omit it.

`virtual_5000_build_layout_find_and_tail_follow` enforces a **5 second budget**
for fixture parsing + model construction + initial GPUI virtual layout and
asserts fewer than 100 rich row entities are instantiated. The first measured
arm64 macOS debug run was **55.99 ms**; the final isolated measurement was
**63.83 ms**, against the same 5 second budget. This is a headless GPUI harness measure,
not a GPU frame-rate or physical trackpad measurement. Tests also exercise
find beyond the visible window, opening collapsed matches, scroll anchor
preservation, jump/tail-follow, plain/Markdown copy, native diff parsing and
incremental replay. No computer use, OS input, or running GitTerm process is
used. Live harness integration, physical rendering/VoiceOver, and selections
that span rows after their endpoints are unmounted remain later integration
coverage; native selection is tested on retained rendered content.

TRU-288 checks are run against the separate GPUI workspace with the locked
dependency graph, including all-target Clippy and headless tests. Root Iced
checks and physical GUI/VoiceOver testing are outside this slice.

## Adapter API and local state

C7 adds a feature-free `ChatAdapter` over the sole core `State::conversation`.
The shell owner connection belongs to C8. No process, real provider, GUI launch,
configuration discovery or transcript persistence is added here.

The owner installs its sole receiver with `begin_runtime(target)` once and
captures the returned stamp. Pane focus/show must never call it. Supply current
catalog facts at `admit`, then `execute` the opaque prepared command against the
current target. `Prepared::id()` correlates later I/O outcomes. Execution checks
full `accepts_runtime` again immediately before the synchronous channel send,
including deferred prompt sends and requests resolved after admission.

| Native action | Core intent/effect | Adapter handling |
| --- | --- | --- |
| Send text/images | `Submit` → `Submit(Submission)` | Send the **prepared** prompt, including its assigned ID. On channel acceptance only, apply `SubmissionAccepted` and record the echo once with the supplied host clock. |
| Allow/deny/question/dismiss | `Answer` → stamped `Send` | Validate the current canonical request kind and offered questionnaire; keep pending until an authoritative resolution. Remember is disabled until destination authority exists. |
| Stop | `Stop` → stamped `Send(Interrupt)` | Use current session authority; native adapter refuses the legacy task-handle path. |
| Receiver event | `Event` then `Record` | Gate the entire ingress with `accepts_stream` **before** observers/effects. Present ordered before/event/after markers; recover worker-start timing from the sole conversation. |
| History start | `BeginHistory` → `LoadHistory` | Return and retain the source `HistoryTicket` with the load ID. |
| History success/error | `HistoryLoaded` / `HistoryUnavailable` | Validate `accepts_history` for either outcome; surface accepted errors. Consume `ReplayHistory` even when revision is unchanged. |

`HistoryReplayed` receiver frames go through `complete_history` with the ticket
created for that source; they are deliberately refused by ordinary `receive`.
Convert their canonical transcript entries with `AgentEvent::from`. Duplicate
or stale history completions do nothing.

On macOS, install an `IntentCallback` returning a correlated `Receipt`. Deliver
adapter updates with `ChatView::present(receiver_stamp, adapter.state(), update,
window, cx)` and later receipts with `update_receipt`. Keep raw `append_record` /
`append_event` for fixture playback. History replacement reconciles retained
cards by request ID, scope and kind, rather than positional row ID. A new
runtime discards old card controls; stale projection stamps are refused.

| Receipt | Meaning and recovery |
| --- | --- |
| Prepared | Admitted locally; no delivery claim. |
| ChannelAccepted | Command channel accepted it; harness delivery remains unconfirmed. |
| Written | Correlated actual I/O succeeded; still awaiting the authoritative outcome. |
| Rejected | Definitive non-delivery. Draft and images remain; an open answer card unlocks for explicit retry. |
| Uncertain | Delivery may have occurred. Keep the draft/answer and block retry; never auto-resend. |
| Resolved | Runtime closed the request. It does **not** mean approved. Late rejection cannot reopen it. |

The composer retains text and image bytes until a write or terminal receipt, and clears
only the unchanged submitted draft, preserving newer edits. Images can enter
through `set_images`; image preparation/strip/paste remains C13. Receipt IDs
prevent late outcomes from modifying a newer attempt. Card answers remain
local intent labels, never fabricated recorded decisions.

**Live activation prerequisite:** the current core `Session::send` exposes
channel acceptance only; runner write failures surface as uncorrelated errors.
C7 defines and tests the synthetic `DeliveryOutcome` executor port. C8 must
supply trustworthy correlated outcomes before claiming harness write receipts;
never parse error strings to guess delivery. No wire enum or fixture changes
are used to carry app-local receipts. Remember dispatch remains unavailable
until a preview-owned destination is verified.

The test suite covers canonical showcase/C0 replay, stale ingress before
observers, full-binding checks before commands, host-clock echo, rejected
text/images, answer rejection/explicit retry, uncertain delivery, resolution
precedence, history source validation and replay without a revision change.
All test input is synthetic. GUI feel and live owner/provider integration are
outside C7.


## C11a composer (TRU-274)

`composer.rs` projects the owner's core state into the native composer. Idle
Send uses `Submit`; streaming Send is labeled Queue and uses `Queue`. Stop
interrupts the turn. The queue lists stable message IDs, retained prompt text,
and the reported pending/queued/started/completed/withdrawn/unsent/uncertain
state. Withdraw requests go through `Withdraw(message_id)` and remain pending
until the runtime responds. A start racing withdrawal or a provider refusal
shows the core's “Not withdrawn” reason without removing the prompt.

The adapter retains each Queue/Withdraw core correlation until the transport
outcome, including pre-send rejection and channel failure. Known-unsent and
uncertain prompts come from the core projection; exit never auto-resends them.
An uncertain submission blocks retry. An explicit retry of a known-unsent
queued delivery gets a fresh runtime message ID, while retaining the old unsent
entry and the caller identity for duplicate/uncertain-retry protection. Both Enter and the Send/Queue button
recheck pending requests and IME marked text; the adapter also checks pending
requests at admission and immediately before channel handoff. Draft/image
retention and matching receipt IDs preserve newer edits.

The slash list uses the session's `Ready.commands` names, descriptions and
argument hints. Up/Down chooses, Tab fills, Enter runs a command without
arguments or fills one with arguments, and Escape dismisses suggestions.
Shift+Enter keeps multiline input. Completion never intercepts IME marked
text, and slash input with arguments is sent unchanged through the normal gate.

Model, effort, mode and checkout are visibly disabled displays. Model/mode
values come from acknowledged core state; an unreported value stays labeled
“Not reported”. Effort distinguishes “Auto” from its reported applied level.
There is no UI setting/checkout dispatch. Interactive pickers await C10b-core's
typed provider acknowledgement/rejection contract. `ChangeMode` is a
rejection-only defensive intent: the preview controller rejects **every** value
before runtime dispatch with a visible receipt reason. FreshPreview remains in
its default launch mode. **Shift+Tab mode cycling is disabled**, including while
composing IME text.

Headless coverage exercises Enter/button and execution gates, IME, slash
completion, disabled mode dispatch, queue/start/withdraw races, channel failure,
task rebind and process-exit delivery states. No real Claude or GUI is launched.
For a later maintainer check in an already isolated preview:

- Confirm disabled model/effort/mode/checkout displays; “Auto” and applied effort
  remain distinct when reported. Shift+Tab never changes mode.
- Send while idle, then during a running turn: Send becomes Queue. Withdraw a
  queued ID; if it starts first, the refusal remains visible.
- Leave a request pending: Enter and the button retain the draft and send
  nothing. Commit an IME candidate without sending or changing mode.
- Complete a session-provided slash command with arrows/Tab/Enter, including an
  argument-taking command and a dismissed suggestion.
- Exit with queued input and verify the core-reported Unsent versus Uncertain
  labels and retained text. Neither path automatically resends.
