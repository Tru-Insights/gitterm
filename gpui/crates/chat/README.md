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

`events.rs` mirrors the serialized display subset of `HarnessEvent`,
`ItemKind`, `RuntimeRequestKind`, `TurnStatus`, `MessageState`, and
`TranscriptEntry` (`Entry::UserPrompt` / `Entry::Harness`) from
`src/harness/mod.rs` and `src/harness/transcript.rs` at `ab542113`.
These neutral event types should move to `gitterm-core` in stage 1; the chat
model, search state, fixture envelopes, and GPUI entities stay here. `Ready`
is retained as a JSON payload because its composer capabilities are outside
this viewer. Additional fields such as rate-limit readings are ignored by
serde; unknown event types fail rather than silently dropping history.

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

`ChatModel` is deterministic and owns the timeline, scoped tool/request lookup,
stream segmentation, and stable row IDs. Nested worker events cannot append to
the parent's assistant message. Unknown item references produce an explicit
incomplete-history error. Reset closes lookup scopes and adds a reset marker;
it does not reuse old row identities. The model is independent of rendered
rows. At most 96 recently visited rows retain rich-text and diff entities. Fold state
and card drafts/submissions survive rich-row eviction; rich-text selection on
evicted rows does not. Visited rows own their rich-text selection and diff state; parsing
completion remeasures the corresponding virtual row. No application data cache
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
| Selected text Copy as Markdown / TRU-270 | Done | Right-click menu and Cmd+Shift+C use native selection Source format, preserving Markdown tables and code fences with language. Normal copy remains plain. |
| Code block Copy / `:1090` | Done | Copies code text only; per-code-block Copied feedback for one second. |
| Question cards / `:3317` | Done | Single/multi choice, Other/free text, header and option descriptions, validated submit and dismiss. Answered/resolved cards are read-only. |
| Permission cards / `:3254`, `:3279` | Done | Allow / Allow always / Deny intents. Always passes the exact offered localSettings suggestion or ACP option. No invented rule or backend effect. |
| Workers / `:1671`, `:2668` | Display-only | Nested scope labels/indentation, worker lifecycle row, scoped child text/thinking/tools and usage. No worker roster/navigation commands. |
| Delegations / `:3048`, `:3087` | Display-only | Collapsible fixture/app delivery cards and results; no delivery or task commands. |
| Turn status / `:784`, `:2668` | Done | Recorded input/output tokens, elapsed time, optional cost; completed/interrupted/failed and explicit errors. |
| Review / `:2456`, `:2768` | Later | Review targets, findings, file opening, stale-head warnings, rerun/cancel/dismiss and held delivery require core/session integration. |
| Composer / `:1843`, `:1896` | Done | Per-view multiline draft; Enter sends, Shift+Enter inserts newline; visible read-only reason; Stop while a turn runs. Queue/steer/withdraw, slash commands, attachments, dictation and model controls remain later. |
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

On macOS, create `ChatView::new(records, window, cx)`, install an app callback
with `set_intent_callback(Rc::new(|intent, app| { /* deliver intent */ }), cx)`,
and enable interaction with `set_read_only(None, cx)`. To archive the view,
call `set_read_only(Some(reason), cx)`: the draft survives, all mutation
controls disable, and the reason remains visible. Without a callback, controls
stay disabled. Worker requests remain read-only until a session adapter owns
worker routing.

`append_event(HarnessEvent, cx)` accepts normalized events one at a time;
`append_record(&Record, cx)` additionally accepts timestamps and transcript
entries. Text deltas, tool starts/completions and turn completion use the same
model path as fixture loading and replay. Ingestion alone mutates transcript
rows. Scroll-up preserves the reader anchor for both added rows and in-place
text deltas. New content then exposes **New messages · Jump to latest**;
jumping resumes tail-follow. Rich text uses the component's 280 ms stream fade
and reduced-motion policy.

`ChatIntent` has typed `Send`, `Stop`, `Permission`, `AnswerQuestion` and
`DismissQuestion` variants. Permission choices are `Allow`, `AllowAlways`
(with the exact offered suggestion), and `Deny`. Always is only offered when
a localSettings rule or ACP option exists, matching the HTML. Question answers
retain question text, selected labels in fixture order, and optional trimmed
free text. The adapter can encode the existing question-text → joined-label map.
Malformed questionnaires show an error and cannot submit; all questions need
a selected option or nonempty free text.

The only optimistic state is local: Send clears the draft after emitting;
card submission immediately locks that card and shows **Submitted locally**
with its answer. Stop waits for a turn-end event. Neither send nor card actions
append an echo, decision, or resolution to the transcript. The callback must
arrange authoritative events; no backend acknowledgment is inferred. Replacing
the callback preserves local submissions. Replay resets transcript/card state
but retains the composer draft and find query. A future adapter may need a
rejection/retry API; this fixture slice deliberately provides no guessed
acknowledgment protocol.

Headless GPUI tests exercise composer Enter/Shift+Enter, empty send, retained
drafts, read-only and stop intents, each permission decision, questionnaire
single/multi/free text and validation, read-only/resolved/answered/worker card
guards, follow versus jump on text deltas, and bounded rich-row retention.

Verified for TRU-288 on arm64 macOS: formatting, locked workspace all-target
Clippy (`-D warnings`), locked workspace tests (**16 chat + 7 shell + 65 terminal**),
locked workspace build, and dependency boundary check each exited **0**. Both
lockfiles remain unchanged. No GUI or running app was used.
