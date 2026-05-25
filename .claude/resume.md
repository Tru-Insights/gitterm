# Resume: gitterm-v2

**Checkpointed:** 2026-04-24 (end of day)
**Branch:** master

## Just Did
Full UX polish pass on the pi-harness webview spike. Spike is now genuinely pleasant to use day-to-day (codex 5.4 backend). Features added: stop button with proper Stopped state, state-based sticky scroll, tool-card auto-collapse with Edit/Write exemption, markdown rendering via marked, syntax highlighting via highlight.js (cdnjs bundled build), CSS semantic tints (green inline code, peach headers), Edit-tool diff rendering (supports both pi's `{path, edits: []}` shape and Claude's `{file_path, old_string, new_string}`), @-mention file picker driven by `git ls-files`, macOS keyboard shortcuts via a muda Edit menu, and Enter-to-submit matching terminal conventions.

Also caught several assumed-wrong event names during testing: pi uses `toolcall_start/delta/end` (not `tool_use_*`), `_end` not `_stop` suffixes, and tool results arrive as a dedicated `role: "toolResult"` message (not `role: "user"` with a tool_result content item, which is Claude's shape). Fixed.

Spike verdict: **GO** (already declared yesterday), and the spike now demonstrates a v1-ready UX vision. Spike is isolated — no `src/` changes, all work is in `examples/pi_webview_test.{rs,html}`, `examples/claude_webview_test.{rs,html}`, and `examples/claude_pipe_test.rs`. Spike code is being committed to git so it's preserved as a reference during integration but clearly separated.

## Immediate Next Step
**Start integration** (see `.plans/claude-tab-spike.md` for the full revised v1 plan in the "Day 2 morning checkpoint" section plus the day-2-late section at the bottom).

Next session should begin with **Phase A: research + plan**, not code:

1. Launch an `Explore` agent to map the integration surface:
   - Current state of `src/webview.rs` (the singleton wry child-view used by markdown + excalidraw viewers)
   - How `src/main.rs` uses the webview today — lifecycle, threading, bounds updates
   - `TabState` struct location and fields; how tab kinds are expressed today
   - Event enum structure; where a new variant set for agent tabs would slot in
   - Workspace config schema for persistence
   - Existing patterns for non-terminal tab content (file viewer is the closest analog)
2. Write `.plans/agent-tab-integration.md` with concrete ordered steps, first 3-4 scoped to one-session-each with acceptance criteria per step
3. Execute the first concrete change: extend `src/webview.rs` with `evaluate_script` + `with_ipc_handler` — smallest meaningful foundational primitive — without breaking markdown/excalidraw viewers.

Then checkpoint, review diff, commit, move on.

## Key Files
- **`.plans/claude-tab-spike.md`** — the master plan. Day-1 checkpoint (yesterday's Claude spike), day-2 morning checkpoint (pi spike added, architecture reframe, revised v1 punch list), day-2 late checkpoint (UX polish details + integration phase-A guidance). Read end-to-end in fresh sessions.
- `examples/pi_webview_test.{rs,html}` — high-water-mark pi spike. Run with `cargo run --example pi_webview_test`. Env: `PI_MODEL` (default `openai-codex/gpt-5.4`), `PI_SESSION_PATH`, `PI_THINKING`.
- `examples/claude_webview_test.{rs,html}` — yesterday's Claude spike, UX pre-polish. Run with `cargo run --example claude_webview_test`. Env: `CLAUDE_MODEL`, `CLAUDE_PERMISSION_MODE`, `CLAUDE_EFFORT`.
- `examples/claude_pipe_test.rs` — hour-1 no-UI pipe verifier.
- `src/webview.rs` — existing singleton wry child-view (149 lines as of yesterday's check). v1 integration extends it; don't rewrite.
- `Cargo.toml` — `tokio` now has `"macros"` feature; `tao = "0.31"` added to `[dev-dependencies]`.

## State
- Spike work is isolated in `examples/`. No `src/` changes.
- Pre-existing working-tree modifications unrelated to this work (workflows, assets, docs, icons) still present, left alone.
- Spike files are being committed as an atomic unit so integration work can start from a clean slate.
- Branch is `master`. If integration work becomes big, worth creating a feature branch.

## Critical "don't break this" list for integration
- Markdown viewer (uses `src/webview.rs` singleton today — extension must not regress)
- Excalidraw viewer (same)
- Terminal tabs (iced_term backed, separate from agent tabs)
- Workspace persistence at `~/.config/gitterm/instance-{pid}/workspaces.json`
- Git status polling (freeze fix from 2026-04-23 — TRU-27 — must stay working)
- FairMutex try_lock_unfair patterns in the iced_term_fork (never use blocking lock)

## Integration v1 scope summary (full detail in plan file)
One **Agent tab kind** with pluggable backends (Claude Code + pi). Shared ~85-90% across backends: webview, chat UI, activity bar, tool rendering, diff view, @-mentions, keyboard menu. Per-backend: subprocess command, event parser, session-continuity mechanism, permission strategy. Estimated 2.5 weeks for items 1-9 on the punch list; items 10-14 are polish.

## Critical unknowns going into v1 (all documented in the plan file)
- Clipboard keyboard shortcuts require a muda Edit menu with standard AppKit selectors (already done in the spike; needs to be folded into gitterm's existing muda menu without breaking existing items).
- Claude `--input-format stream-json` input wire format — guess `{"type":"user","message":{"role":"user","content":"..."}}`; verify with small test before refactoring to persistent subprocess.
- Whether Claude Code's `--print` emits `permission_request` events with non-bypass permission modes — determines the permission UX path for the Claude backend.
- pi permission UX — this is a pi-side design decision since pi is open source and the user owns it. Design the event protocol before implementing.
- `tool_execution_start/update/end` pi events — not handled in spike; could drive a more precise "Running" activity label in v1.
- Whether `--effort max` changes opus thinking emission (low-pri; ambient indicator is fine regardless).

## Context
Two-day session: yesterday built the Claude-Code webview spike and proved the architecture (streaming tokens, tool use, session resume, activity bar with elapsed/tokens/cost, opus silent-thinking handled with ambient indicator matching Claude Code's CLI). Today built the pi-harness parallel spike plus a sustained UX polish pass that lifted it to v1-ready quality. The strategic reframe today: pi and Claude are peers (user goes between them; pi trending up with codex 5.5). The integration therefore targets **one Agent tab kind with pluggable backends** rather than two tab kinds. pi being open source + user-customized is first-class leverage for v1+ (custom permission events, gitterm-aware skills) that the Claude backend can never match.
