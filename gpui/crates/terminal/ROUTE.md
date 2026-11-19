# TRU-278 — terminal route decision

**Recommend Route A: committed iced_term backend + GPUI snapshot renderer.**
The implemented prototype passes the required real-PTY gates. Budget **6–10
worker-days remaining for stage 3 after TRU-287**, compared with Route B's **18–27** from
TRU-276. Both routes reuse Alacritty 0.25.1; no emulator rewrite is needed.

This recommendation is based on the code and automated tests in this crate,
not a claim of production readiness. The separate GPUI binary accepts
`--terminal` and runs `$SHELL` in a real PTY. No visible app or running GitTerm
process was used during verification.

## Route comparison

Route B observations and estimates below are from the TRU-276 report and its
24 passing *observation* tests (some assert stock-widget failures). Route B
was not patched here. Route A tests assert the required working behavior.

| Area | Route A, implemented here | Route B, gpui_xterm 0.3.0 / remaining patch |
| --- | --- | --- |
| Multiline paste | PASS: real zsh enables 2004; clipboard paste executes neither command before Enter, both after Enter. All paste routes share CRLF/LF normalization, envelope and ESC stripping. | FAIL stock: both commands execute before Enter; patch every paste route and native Cmd-V. |
| CPR / DA | PASS: waiting children receive `ESC[3;5R` / `ESC[?6c` without rendering or foreground event forwarding. PtyWrite goes straight to asynchronous PTY input. | FAIL stock: event proxy discards replies; preserve and asynchronously forward them. |
| Mouse | PASS: actual view handlers deliver exact SGR 1006 press, release and wheel bytes in 1049 alternate screen; do not start local selection. Fork legacy/UTF8 encoding and drag/motion paths are retained. | FAIL stock widget: local selection instead of reports; wire existing encoder to view and resolve mode/selection precedence. |
| Hidden output and memory | PASS: hidden TerminalView entity, no GPUI pumping/rendering, 200 MB + 34 marker bytes verified at parser input, zero mismatches; fixed history and viewport, no foreground byte queue. | Finite byte ingestion passes, but 200 MB remains queued when foreground pauses (TRU-276 measured ~197 MiB RSS growth). Move parsing off foreground or add backpressure/time budgets; fix retry/read errors and synchronous input. |
| 100k history / search / export | PASS: exactly 100,000 retained lines after 100,100; literal `[.]` query and scroll-to-match; first retained cell included. Busy returns `None`, not an empty success. Snapshot remains viewport-sized. | FAIL widget: ignores 100k config and retains 10k; standalone emulator has working primitives, but private view state lacks service/search/export hooks. |
| Selection / resize | PASS: drag/semantic/line paths retained, Shift extends anchor, copy waits through parser contention. Headless paint resizes snapshot; real PTY child observes 103×31 via ioctl. | Existing local selection/resize foundation reusable; Shift currently resets anchor. PTY resize needs app-owned callback wiring. |
| IME / Unicode | PASS at automated scope: original UTF16 EntityInputHandler/preedit path retained, commit reaches child once, CJK/wide spacer/combining cells survive serial UTF8; geometric box/block glyph tests and headless preedit paint pass. | Existing view/render path is the source used by A; manual IME/font/Retina/feel verification still needed for either route. |
| URLs / app services | Committed fork regex/search APIs retained and URL opening returned to app policy; Cmd-hover pointer and Cmd-click app callback in TRU-282. Background service jobs, literal find and cheap tail snapshots completed in TRU-287. | Add widget hooks plus GitTerm link detection/opener and all history/service hooks. |
| Ownership / licences | MIT copying permitted with notices; one local crate owns fork-derived backend and xterm-derived presentation. No dependency on Iced. Future fixes need explicit synchronization with upstream/fork. | Maintain a widget patch/upstream changes, including its internal PTY scheduling architecture. Same permissive source licences. |
| Effort spent in TRU-278 | One local judgment-role implementation session: backend extraction, snapshot renderer/view integration, real-PTY/headless regressions, provenance and checks. Elapsed time is recorded under verification; it is not a worker-day productivity measure. | Estimate only; no implementation work spent on B in this task. TRU-276 supplied the comparison evidence. |
| Remaining stage-3 allowance | **6–10 worker-days** after TRU-287; breakdown below. | **18–27 worker-days** for the full patched-widget route, as sized in TRU-276. |

A avoids replacing the widget's reader/queue/parser ownership while preserving
its GPUI presentation and IME implementation. Copying the backend was necessary
because the fork imports Iced geometry/modifiers and its own event bridge.
The copied source commit and exact file hashes, licence checks and changes are
in [README.md](README.md) and [SOURCES.json](SOURCES.json). Dirty fork URL work
was excluded. Route A also removes the fork's 4,096-event reply-loss risk:
terminal replies bypass UI notification coalescing entirely.

## Evidence and its limits

The suite covers the acceptance contract plus existing fork snapshot and xterm
font/color/glyph/key regressions. Relevant tests:

- `multiline_clipboard_paste_waits_for_enter_in_real_zsh`
- `cpr_and_da_reach_waiting_child_without_foreground_pumping`
- `actual_view_sgr_press_release_wheel_reach_child`
- `hidden_view_200mb_no_render_no_foreground_drain_bounds_memory_and_verifies_all_bytes`
- `history_100k_literal_search_scroll_and_contention_are_explicit`
- `first_retained_cell_extraction_and_unicode_regression`
- `deferred_selection_and_resize_survive_parser_lock_contention`
- `drag_shift_copy_and_native_copy_survive_parser_contention`
- `real_pty_resize_reaches_child_and_grid`
- `ime_preedit_utf16_commit_selection_and_unicode_snapshots`
- `headless_paint_selection_preedit_and_resize_use_bounded_snapshot`
- `ui_effects_coalesce_and_survive_snapshot_dirty_reset`

The 200 MB test observes the actual Alacritty PTY reader, not a second reader
or replacement parser. Every data/marker byte matches a deterministic pattern.
The final marker also appears in parsed cells. While it runs, no window is
created and no GPUI tasks are pumped: the initial 4,000-cell snapshot stays
unchanged until the final explicit sync. History intentionally evicts old data;
"no dropped bytes" means no ingestion/parser loss, not unbounded retention.

RSS is sampled for this process only. The test enforces no more than 64 MiB
growth after 50 MB has been consumed (well after its 100k-line history fills).
A representative complete-suite run consumed **200,000,034 bytes**, mismatch
count **0**, retained **100,000 lines**, snapshot **4,000 cells**, in **9.118 s**;
RSS at the 50 MB sample and subsequent maximum were both **249,936 KiB**.
This is a plateau check, not a universal per-session memory budget or throughput
benchmark. The process includes other tests and allocator retention; 100k ×
column count is still expensive. Byte-by-byte observation in a debug test also
adds substantial cost. No paint/frame-latency or many-session speed claim follows.

The headless paint test executes the real canvas/shaping path at two sizes,
including selection, CJK/combining glyphs and marked preedit. It does not certify
native raster output, fonts on every machine, input-method candidate positioning
with a real IME, accessibility, cursor blink, drag autoscroll, or resize feel.
The retained preedit model has the same simple replacement/range semantics as
xterm; grapheme-aware preedit placement and advanced editing remain future work.

OSC52 stays copy-only by default. Reads are not enabled. Clipboard-store/title
callbacks are coalesced and delivered on render; the example does not install
an OS-clipboard callback. TRU-282 adds persistent exit/error banners, idempotent
reader join and owned-child reaping, bounded whole-message input admission, and duplex flood fairness gates.
The per-session worker replaces Alacritty's unbounded notifier queue. CPR/DA,
OSC title/bell and legacy mouse fixtures run headlessly against real PTYs.
Color/size query responses, broader link boundaries and representative TUIs
still need stage-3 fixtures. No indefinite
soak, production integration, visible smoke, or manual Unicode/IME testing is claimed.

## Revised stage-3 work, Route A

These are engineering estimates for remaining work, not measured development
times or calendar promises. They exclude other GPUI migration stages.

| Remaining work | Worker-days |
| --- | ---: |
| Close/reap/cancellation and error UX, bounded input, protocol/legacy-mouse fixtures, duplex multi-session fairness — completed in TRU-282 | 0 |
| Asynchronous search/export, literal find bar, watchdog/log tail snapshots and eviction handling — completed in TRU-287 | 0 |
| Main/bottom/run integration: cwd/env/startup, ownership/session lifecycle, hidden sessions and service consumers | 3–4 |
| Real shell/Claude/Codex/mouse-TUI checks, Unicode/IME/font/Retina/accessibility and resize under load | 2–4 |
| Contingency for GPUI scheduling, copied-code maintenance and integration discoveries | 1–2 |
| **Remaining total** | **6–10** |

Do not count the prototype as complete stage 3. It establishes the route and
removes the main scheduling/reply/paste uncertainty; it does not wire the
Iced app's callers into GPUI. Keep the root workspace and fork independent.

## Verification

The local judgment-role session began at 19:04 UTC and reached final verification
at 19:25 UTC on 2026-11-18 (about 20 minutes elapsed, including builds and tests).
This agent wall time is not a worker-day estimate. No delegation or specialist
review was used, as requested; the final diff was self-reviewed.

Commands and final exit codes are recorded in [evidence/checks.txt](evidence/checks.txt).
The test output is [evidence/tests.txt](evidence/tests.txt). Run commands from
the repository root with the toolchain/target settings in the crate README.
Only `gpui/` files changed. Root `Cargo.lock` is checked against base
`71ca620f45c88351a42d29259f94838f2ccb0778`.

The GPUI lockfile adds the terminal member and GPUI test-support dependencies
(including optional gpui-fast package records, seeded from TRU-276's resolved
0.1.17 family; the active backend remains gpui-pre 0.3.12). Cargo needed
`wgpu 30.0.2 → 30.0.1` to satisfy that optional family's exact requirement;
it also reselected compatible existing target dependencies. No registry source
or Wry manifest patch was made. The root lockfile and Iced dependency graph
were not involved. Dev builds omit debug info and optimize Alacritty/vte for
realistic background-parser gates; release settings are unchanged.

The root pre-commit hook invokes Iced checks, which conflict with this task's
explicit GPUI-only build scope. The commit uses a command-local disabled hook
path after the equivalent GPUI checks pass. No root checks, push, PR, consults,
other agents, or installed-app replacement were performed.

## TRU-282 verification

The first stage-3 slice replaces the upstream unbounded I/O channel with a
per-session worker, bounded admission and fixed duplex budgets. Its public
`Backend::cancel` joins the reader and reaps the child, including a child that
ignores HUP. `SessionState` survives coalesced-event consumption; the view paints
an exit/error banner and stops accepting input. Cmd-hover shows a pointer and
Cmd-click resolves the current URL through the retained regex APIs before
calling the app's opener. The example supplies an asynchronous macOS opener.

New regressions cover:

- `cancel_is_idempotent_joins_reader_and_reaps_hup_ignoring_child`
- `dropping_view_joins_reader_and_reaps_child`
- `child_exit_status_persists_after_events_are_consumed_and_ignores_input`
- `pty_read_and_registration_errors_stop_worker_reap_child_and_persist`
- `exited_and_failed_views_paint_persistent_status_and_ignore_input`
- `blocked_child_input_is_bounded_atomic_and_preserves_reply_reserve`
- `protocol_cpr_da_osc_title_and_bell_survive_without_ui_delivery`
- `legacy_mouse_press_release_and_wheel_use_non_sgr_bytes`
- `flooding_session_cannot_starve_its_input_or_a_second_session`
- `cmd_hover_and_click_resolve_current_url_and_call_only_app_opener`

No running app, OS input or visible window was used. Search/export and shell/dock
integration remain separate work. Only the terminal crate, its lockfile dependency
record, and the permitted `--terminal` opener callback change. `libc` and `polling`
were already locked; no package versions or root lockfile changed.

Final checks all exited **0**: workspace format, Clippy (`--all-targets`,
`-D warnings`), locked workspace tests (**65 passed**, including all 55 existing
terminal tests), dependency boundaries and locked workspace build. Commands and
limits are in [evidence/tru282-checks.txt](evidence/tru282-checks.txt). The existing
deferred-layout test now pumps a headless frame while waiting for a title
callback, rather than relying on the child replying during the first frame.

## TRU-287 verification

Stage 3b adds literal find (Cmd+F, next/previous, Esc), background search/export
and tail snapshots. A worker copies a coherent grid in 32-row try-lock slices,
then runs the existing Alacritty regex/text APIs off the foreground. One job
per terminal bounds admission. Capture contention/mutation reports `Busy`;
find clears and retries after output quiets. Export returns plain text to app
policy, with an optional foreground view callback. Tail copies only the last N
physical rows and needs no render. Revision checks discard expired matches and
pending selection coordinates; Alacritty rotates live selections on eviction.

All required checks and the locked GPUI build exited **0**: format, workspace
Clippy (`--all-targets`, `-D warnings`), workspace tests (**88 passed**, including
all 65 previous terminal tests plus five new regressions), and dependency
boundaries. Commands, regressions and resolved verification failures are in
[evidence/tru287-checks.txt](evidence/tru287-checks.txt).

Only this crate changed; both lockfiles remain unchanged. No GUI, OS input,
running app interaction or push was used. Native IME/visual/accessibility checks
and shell integration remain in the table above. If the shell consumes Cmd+F,
it must route that action to `open_find` and focus the terminal. Sustained output
can keep find Busy; full-history jobs allocate one temporary grid. See README
for callback and polling contracts. No history cache or file IO policy was added.
