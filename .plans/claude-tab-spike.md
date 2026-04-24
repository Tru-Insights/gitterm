# Spike: Claude Tab Proof-of-Concept

**Purpose:** Prove the event pipe — subprocess ↔ Rust ↔ webview — works end-to-end at realistic sizes, before committing to building a real "Claude tab kind" in gitterm-v2.

**Status:** Not started. One day time box. Throwaway code.

## The Single Question

Can we stream `claude --output-format stream-json` events into a webview chat UI, accept input back, and stay responsive at realistic transcript sizes?

Yes → invest in the real feature.
No → document blockers, walk away.

## Key Findings from `src/webview.rs`

- `wry`-based, main-thread-only (Send/Sync constraint, `thread_local!` storage)
- Singleton: one `WebView` at a time stored in `WEBVIEW: RefCell<Option<WebView>>`
- Built as a child view with bounds — overlays the Iced window at x/y/w/h
- `set_visible(bool)` and `update_bounds()` already work
- **No IPC bridge exists.** Current use is one-way: `load_html(full_document)` → render. Fine for static markdown/mermaid, unusable for streaming (reloads page every call)

## What the Spike Must Add

- Replace/augment `update_content()` with `eval_js(script: &str)` — calls `webview.evaluate_script()`. This is the Rust→JS channel for streaming events in without reloading.
- Add `.with_ipc_handler(...)` at WebView construction — this is the JS→Rust channel for submitting user input back.
- Single skeleton HTML loaded once: `<div id="messages"></div><textarea id="input">`, plus a minimal JS module that:
  - Exposes `appendEvent(json)` for Rust to call via eval
  - Posts `submitInput(text)` via the IPC handler on Cmd+Enter

## Architecture Sketch

```
  ┌──────────────┐      spawn       ┌──────────────────────┐
  │   Main       │◄─────────────────│  claude process      │
  │   thread     │   stdout lines   │  --output-format     │
  │   (Iced app) │─────────────────►│   stream-json        │
  │              │   stdin writes   │                      │
  │              │                  └──────────────────────┘
  │              │
  │     Task::perform(async read)
  │     parse JSON line → Event::ClaudeEvent(json)
  │              │
  │              ▼
  │     main-thread handler
  │     webview::eval_js(&format!("appendEvent({})", json))
  │              │
  │              ▼
  │     ┌─────────────────┐
  │     │  wry WebView    │
  │     │  (child view)   │
  │     │                 │
  │     │  JS: parses     │
  │     │  event, appends │
  │     │  DOM node       │
  │     │                 │
  │     │  textarea submit│──ipc_handler──► Event::ClaudeInput(text)
  │     └─────────────────┘                         │
  │                                                 ▼
  │                                     write to subprocess stdin
```

## In Scope

- Standalone spike — not wired into tabs, workspaces, or persistence
- Runs as a throwaway window or a temporary `#[cfg(feature = "claude-spike")]` panel in gitterm
- Spawn `claude --output-format stream-json`, tokio async stdout reader
- Parse JSON events: text deltas, tool_use, tool_result, stop (ignore everything else)
- Render into the webview: append text deltas to active message block, render tool_use/tool_result as simple labeled blocks
- Textarea input → Cmd+Enter → submit via IPC → subprocess stdin + `\n`

## Out of Scope (don't drift)

- Tab integration. Spike does not become a "Claude tab kind" in this work.
- Persistence. Session disappears on process exit.
- Styling. Default system fonts, ugly is fine.
- Tool-call collapsibles, diff views, syntax highlighting, markdown rendering
- Attention system, workspaces, multi-session, session history
- Handling pi-harness, Claude Code CLI alternative modes, or anything other than `claude --output-format stream-json`
- Tearing down and re-using the webview for non-spike purposes

## Acceptance Criteria

**Go — invest in real Claude tab kind:**
- [ ] ~5 minute Claude session with ~10k lines streamed output stays responsive — no stutter during streaming, scroll remains smooth
- [ ] Realistic tool-use patterns (code gen, file edits, bash calls) parse without crashes
- [ ] Multi-turn works — follow-up message reaches subprocess, response streams back
- [ ] Nothing about the plumbing feels inherently fragile (IPC flaky, process deadlocks, webview state drift)

**No-go — walk away, revisit in a month or never:**
- [ ] Webview chokes at normal transcript sizes
- [ ] IPC between Rust and webview is a constant source of flakiness
- [ ] Subprocess stdin round-trip is fragile (stalls, buffers, process dies)

## Hour-1 Smallest Slice

Before touching webview.rs, prove the process layer works in isolation:

Standalone Rust binary (new file, probably `examples/claude_pipe_test.rs` or similar), not integrated with gitterm:
1. Spawn `claude --output-format stream-json`
2. Async read stdout line by line, parse JSON, pretty-print each event to terminal
3. Accept typed lines from terminal stdin → pipe to subprocess stdin + `\n`
4. Handle subprocess exit cleanly

If this works in an hour, move to the webview. If it doesn't, we've learned the first blocker without writing any HTML.

## File Touch List

**New:**
- `examples/claude_pipe_test.rs` (or similar) — hour-1 standalone binary
- `assets/claude_spike.html` (or inline `const STR`) — skeleton HTML + minimal JS

**Modified:**
- `src/webview.rs` — add `eval_js(script)`, extend builder with `.with_ipc_handler(...)`. Keep existing functions intact so markdown/excalidraw viewers still work.
- `src/main.rs` — temporary spike-only wiring behind `#[cfg(feature = "claude-spike")]` or a debug-only flag: event variants, hotkey to launch, subprocess handle storage. Nothing that bleeds into permanent architecture.

## Time Box

One day. Checkpoint at end:
- What works? What feels bad? What's the smallest possible permanent version?
- If "go," write a real plan for the Claude tab kind, informed by what the spike taught us.
- If "no-go," document the specific blockers in this file's "Blockers" section so future-you (or Claude) doesn't retry the same path blind.

---

# Checkpoint: 2026-04-23 — Verdict: GO

The spike ran significantly longer than one hour on day one and ended up proving more than just the pipe. What we have now is essentially a working throwaway v0 of the Claude tab — enough to confidently scope v1.

## What was actually built

### Files
- `examples/claude_pipe_test.rs` — hour-1 standalone binary (no UI). One-shot prompt via CLI arg. Spawns `claude --print --output-format stream-json --verbose`, async reads stdout line by line, parses JSON, pretty-prints by event type.
- `examples/claude_webview_test.rs` + `examples/claude_webview_test.html` — full interactive spike. tao top-level window + wry WebView (with devtools enabled for debugging). IPC handler receives submits and forwards to a background tokio thread. Background thread spawns `claude` and streams events back via `EventLoopProxy::send_event`. Main thread `evaluate_script`s each event into the page, JS renders it.

### Cargo changes
- `tokio`: added `"macros"` feature.
- `tao = "0.31"` in `[dev-dependencies]` (matches wry 0.48's own dev-dep version).

### Behavior the spike has end-to-end
- Streaming pipe with `--include-partial-messages` — text and thinking tokens appear as they're generated, not batched.
- Multi-turn via `--resume <session-id>` captured from the first `system:init` event. Context carries across turns correctly.
- Real tool execution via `--permission-mode bypassPermissions` (configurable via env var). Tool_use blocks show the tool name immediately on `content_block_start`; args build up live as `input_json_delta` streams; pretty-printed JSON once the block stops.
- Tool results extracted cleanly from `user` events — just the `content` field (file bytes, bash output, etc.), not the JSON wrapper. Errors render in an error style.
- Empty thinking blocks (opus signature-only thinking with no `thinking_delta`) are suppressed via lazy DOM creation: we only create the DOM block on first content-bearing delta.
- Activity bar between messages area and composer. States: `Starting` → `Thinking` → `Calling <ToolName>` → `Running <ToolName>` → `Thinking` (after tool result) → `Responding` → frozen `Done`. Shows elapsed time and running `↓ N tokens` count (from `message_delta.usage`). On `result`, freezes with final duration, tokens, and cost; clears on next submit.
- Env var knobs: `CLAUDE_MODEL` (default haiku), `CLAUDE_PERMISSION_MODE` (default bypassPermissions), `CLAUDE_EFFORT` (optional — maps to Claude Code's `--effort` flag; relevant for opus thinking budget).

## What was proven

- Subprocess + streaming + webview pipe works cleanly at scale. No flicker, no stutter, no deadlocks across sonnet and opus testing.
- `wry` 0.48 `evaluate_script` is the right Rust→JS channel; `with_ipc_handler` is the right JS→Rust channel. No third-party bridge needed.
- `EventLoopProxy` bridges the tokio-thread / main-thread boundary cleanly. Iced's task/subscription model should map equivalently when we integrate.
- Multi-turn via `--resume` worked, but see the scaling concern below — we will not ship this pattern.
- Real agent work with tool loops (Read, Grep, Bash, Edit) flows through the same stream. Tool output is already readable in the current spike.
- Opus is usable in the webview with the activity bar making silent-thinking phases feel alive instead of dead.

## What we discovered about opus thinking (important)

**Claude Code does not stream opus's thinking content.** This is not a bug in our spike — it's a product decision somewhere in the stack. Evidence:
- Opus emits `content_block_start` with `type: "thinking"` and `content_block_stop`, but the only deltas between them are `signature_delta` (crypto signature for API continuity). There are no `thinking_delta` events carrying readable thinking text.
- The official `claude` CLI's terminal UI shows the same pattern: an ambient spinner with elapsed time and token count (`✢ Gallivanting… (1m 22s · ↓ 4.1k tokens · almost done thinking)`) — no thinking content rendered.
- So an ambient indicator is the correct UI pattern here, not a cop-out. We're matching how the product itself handles it.

Still unconfirmed: whether `--effort max` changes this, whether the thinking is simply not emitted or emitted as `redacted_thinking`, whether there's a flag that forces it. Not blocking v1.

## What we deliberately did NOT build

- Zero integration with gitterm's `main.rs`, tab system, workspaces, or persistence. Spike is 100% standalone.
- No `--input-format stream-json` persistent subprocess. Every turn respawns `claude` (~1-2s startup + disk read of growing transcript). See below — this is now a v1 must.
- No permission prompt UX in the webview. We bypass entirely via `bypassPermissions` — spike-only; unshippable.
- No stop button, no session picker, no prompt history, no markdown rendering, no styling pass.

## What we learned that changes the v1 plan

### Persistent subprocess is a v1 must-have, not polish

Initial plan called `--input-format stream-json` a "nice polish." That was wrong. With `--resume`:
- Every submit respawns `claude`, reads the whole stored session from disk, rebuilds the system prompt, and re-hits the prompt-cache-creation path.
- Disk load grows linearly with conversation length. Startup wall-clock climbs with it.
- Prompt caching helps on identical prefixes but doesn't eliminate the respawn tax.
- For a user running sessions 12hr/day with 20+ turn conversations, this degrades noticeably over a single session.

Persistent subprocess via `--input-format stream-json` — one `claude` per tab, kept alive, user messages piped in as JSON — eliminates all of that. System prompt loaded once, context stays resident, each turn is just a message. **This is required for v1**, not optional.

### Process management is not a new problem

The user already runs multiple concurrent `claude` processes today — multiple repos × multiple terminal tabs × `claude` inside some of them. The Claude tab kind is a **different rendering surface for the same process pattern**, not a new scaling concern. MCP concurrency, memory per process, cleanup discipline: all already solved by how terminal tabs work today. The Claude tab just needs the same subprocess-cleanup discipline applied on tab close / workspace close / app exit.

### Permission UX is still the biggest v1 design problem

Spike bypassed permissions entirely. Real v1 must either:
- Render permission_request events from the stream as interactive approval rows (if Claude Code emits them in `--print` mode — needs confirmation), or
- Use `--allowedTools` to whitelist a specific toolset per tab (e.g., read-only agent, or full-capability trusted mode), configurable per tab, or
- Some combination.

This is the single design decision that most shapes how trustworthy and useful the feature feels. Worth its own mini-plan before implementation.

## Revised v1 punch list (priority order, not effort order)

1. **Persistent subprocess via `--input-format stream-json`.** Promoted from polish to must. Investigate the exact user-message JSON shape (spike hasn't wired this yet). One subprocess per Claude tab, spawned eager on tab create (lazy spawn is a later option). Clean shutdown on tab close / workspace close / app exit.
2. **Permission UX.** Design + implement. Probably a chat-row "Allow Read(foo.rs)? [Allow once] [Allow session] [Deny]" pattern. Answer round-trips via IPC.
3. **Extend `src/webview.rs`.** Add `evaluate_script` passthrough and an IPC handler on the existing child-view singleton webview. Keep markdown/excalidraw viewers working unchanged.
4. **"Claude tab kind" in the workspace/tab model.** Persistence, tab-strip glyph, creation via Cmd+Shift+T or "+" menu. Must coexist with terminal tabs. Cmd+1..9 still works across kinds.
5. **Port the spike's activity bar, tool_use rendering, tool_result extraction, empty-thinking suppression** into the real webview code. Most of this is already written in `examples/claude_webview_test.html`; it's transcription, not fresh design.
6. **Stop button.** Kill subprocess cleanly mid-stream. Resume a new turn cleanly afterward.
7. **Session persistence across app restart.** Store per-tab session_id in workspace JSON; resume subprocess with `--resume <id>` on tab reopen (one-time cost; after resume, operate in persistent-subprocess mode).
8. **Tool-call polish.** Collapsible tool cards, inline diffs for Edit, bash output preview with syntax highlighting. This is where the webview starts actually beating terminal Claude Code.
9. **Markdown rendering** for assistant text (pulldown-cmark is already a dep, same as the markdown module uses).
10. **Prompt history** (up-arrow in the textarea, tab-local).
11. **Styling pass** — Catppuccin theme, font consistency with the rest of gitterm.

Rough size: 2 weeks of focused work for items 1-7 (shippable v1 for personal use). Items 8-11 make it genuinely enjoyable.

## Current state

- Spike binaries (`examples/claude_pipe_test.rs`, `examples/claude_webview_test.{rs,html}`) compile cleanly and run well. Opus + tool use verified end-to-end with activity bar showing phase, time, tokens, cost.
- Cargo changes: `tokio` has `macros` feature; `tao = "0.31"` in `[dev-dependencies]`.
- No production code was touched (no changes to `src/`).
- API spend across the day's testing: estimated ~$0.40-0.60 (mostly opus with a single long multi-tool turn at $0.24).
- All spike code is disposable. None of it is on a path to production. The HTML and JS logic, however, encodes real design decisions (event dispatch, lazy DOM creation, activity state machine) that should be ported wholesale into the real implementation.

## Blockers / unknowns going into v1

- **Input wire format for `--input-format stream-json`.** Educated guess: `{"type":"user","message":{"role":"user","content":"..."}}` (symmetric with what we see on output). Verify with a small test before committing to the refactor.
- **Does `--print` emit permission_request events when `permissionMode` is not bypassed?** Determines whether the permission UX is "intercept existing events" or "different control path entirely."
- **Can we extract opus thinking via `--effort max` or another flag?** Not blocking — ambient indicator is fine — but worth one more investigation.
- **Clipboard keyboard shortcuts don't work in the wry WebView on macOS.** Specifically: right-click → Cut/Copy/Paste **does** work (WKWebView's built-in context menu is intact), but **Cmd+C / Cmd+V / Cmd+X / Cmd+A keyboard shortcuts do not**. This is the standard wry-on-macOS gotcha — the shortcuts require the app's menu to have first-responder Edit items bound to the standard AppKit selectors (`cut:`, `copy:`, `paste:`, `selectAll:`), and wry doesn't provide those by default. gitterm already uses `muda` for its menu so this integration point exists; real v1 needs to add an Edit menu (or Edit submenu) with these standard items. Not optional — a chat UI where Cmd+V doesn't paste a prompt is unacceptable at 12hr/day usage.

---

# Day 2 Checkpoint: 2026-04-24 — pi-harness spike, scope revised

This checkpoint supersedes the day-1 v1 scope. The day-1 material above is preserved as history; read this section for the current shape of v1.

## Why the scope changed

Yesterday's plan framed this as "Claude tab kind" with pi as a later possibility. User correction today: **the two harnesses are peers, not primary/secondary**, and pi usage is trending upward (more so with codex 5.5). That reframes v1 from "build one tab kind" to "build one pluggable agent-tab kind, wire two backends."

## What was built today

- `examples/pi_webview_test.rs` + `examples/pi_webview_test.html` — sibling spike to `claude_webview_test.{rs,html}`, same shape but drives `pi --print --mode json` against `openai-codex/gpt-5.4-mini` by default. Multi-turn via a fixed `--session <tmp path>` per process (different pattern from Claude's captured `--resume <id>`). Shared activity bar, tool rendering, DOM lifecycle — only the event parser is pi-specific. Env knobs: `PI_MODEL`, `PI_SESSION_PATH`, `PI_THINKING`.
- Event-shape discovery for pi: NDJSON on stdout. Top-level `type` is one of `session` / `agent_start` / `turn_start` / `message_start` / `message_end` / `message_update` / `extension_ui_request` / ... Assistant deltas arrive inside `message_update.assistantMessageEvent` with types like `thinking_start`, `thinking_delta`, `text_delta`, `tool_use_start`, `tool_use_input_delta` (names verified for thinking/text; tool variants inferred and handled defensively). Each delta also carries `partial` (the full message-so-far), convenient but unused — we accumulate per-index like we do for Claude.
- Claude spike (`examples/claude_webview_test.{rs,html}`) left untouched and still runs.

## What's new and important (vs. day-1 findings)

- **Codex 5.4 streams actual thinking content.** `thinking_delta` events carry real reasoning text. This is a noticeable UX contrast with opus via Claude Code (where thinking is signature-only / redacted). If you're on a slow think with codex, the user sees the reasoning unfold; with opus, the user sees only the ambient spinner. Not a reason to prefer one over the other — just different character.
- **pi has an active UI protocol (`extension_ui_request` events — `notify` / `setTitle` / `setStatus`).** The spike suppresses them; v1 should render them as first-class surface (title → tab label, status → tab header, notify → inline toast). Claude Code does not have an equivalent. This is the clearest place pi's shape is "richer" for a UI client.
- **pi is open source and the user has running customizations** (`~/.pi/agent/extensions/`, custom subagents, gitterm-aware memory). Constraints that apply to Claude (closed event format, permission prompts invisible in `--print`, fixed thinking emission policy) don't apply to pi — they can be changed by extending pi itself. This is strategic leverage the Claude side can never match.

## Codex 5.5 note
User reports codex 5.5 has shipped. Not in `pi --list-models` today (highest codex = 5.4). Likely requires `pi update` or a new pi release that knows about 5.5. Worth verifying before the next spike session so testing targets the current model.

## Revised architecture: one Agent tab kind, pluggable backends

Reframe: not "Claude tab kind" + "pi tab kind" as separate features. Build **one "Agent tab kind"** whose state carries a `backend: Claude | Pi | ...` discriminant.

What's shared across backends (≈85-90% of v1 work):
- webview + wry setup (`src/webview.rs` extension with `evaluate_script` + `with_ipc_handler`)
- HTML / CSS / JS chat renderer — block appending, lazy DOM, activity bar, tool_use cards, tool_result formatting, Done-state persistence, empty-thinking suppression
- tokio subprocess supervisor (spawn, stream stdout, forward events to main thread, graceful kill)
- IPC plumbing (JS submit → Rust → subprocess stdin)
- Tab-kind integration (workspace/tab model, persistence, tab strip glyph, creation shortcut, Cmd+1..9 routing)
- Stop button, prompt history, clipboard bindings, session-restore logic

What's per-backend (≈10-15%):
- Subprocess command + arg construction
- Raw-event parser that emits a canonical UI-event set (thinking_delta, text_delta, tool_use_start, tool_use_input_chunk, tool_use_done, tool_result, usage_update, ...)
- Session-continuity mechanism (Claude: captured `session_id` + `--resume`; pi: `--session <path>`)
- Permission strategy (Claude: `--permission-mode` + hope for the best in `--print` mode, OR surface permission_request events if they exist; pi: *extensible* — add explicit permission-request events to pi if needed)

Implementation shape for v1:
```rust
enum AgentBackend { Claude, Pi }

trait EventParser {
    fn parse_line(&mut self, line: &str) -> Vec<CanonicalEvent>;
}
// One parser impl per backend. The canonical event set is what the webview renders.
```

Tab creation: probably a "+" menu or picker that lists both backends, rather than separate Cmd+Shift+T / Cmd+Shift+P. Lets us grow to N backends without running out of shortcuts.

## Revised v1 punch list

Supersedes day-1 list. Priority order (not effort order).

1. **Extend `src/webview.rs`** to support `evaluate_script` + IPC handler on the existing child-view. No backend specifics yet. Keep markdown/excalidraw viewers unchanged.
2. **Agent tab kind in the workspace/tab model.** Tab state carries a `backend` enum. Persistence, tab-strip glyph (possibly different per backend), "+" menu with backend picker, Cmd+1..9 routing across kinds.
3. **Canonical-event contract + parsers.** Define the canonical `UiEvent` set the webview consumes. Write the Claude parser (most of it already exists in `examples/claude_webview_test.html` JS — port to Rust or keep in JS). Write the pi parser (most of it exists in `examples/pi_webview_test.html` JS). Both emit the same `UiEvent` sequence.
4. **Persistent subprocess per tab.** For Claude: `--input-format stream-json`. For pi: `--mode json --session <path>` (pi already supports sticky sessions via file path; persistent stdin mode may or may not be needed — test before deciding). One subprocess per agent tab, spawned eager on tab creation, killed on tab close / workspace close / app exit.
5. **Port the spike's UI**: activity bar, tool_use cards, tool_result extraction, empty-thinking suppression, Done-state persistence. Almost verbatim transcription from the spike HTML/JS.
6. **Permission UX.** Two-track:
   - Claude: investigate if `--print` emits permission_request events outside bypass mode; render them as approval rows if so; otherwise default to bypass for the spike user's trusted context.
   - pi: **extend pi** to emit explicit permission_request events in print/rpc mode and accept permission_response via stdin. This is the first example of "pi-side customization to be a better gitterm citizen." Defer actual pi customization until we've decided the protocol.
7. **Stop button** — SIGTERM (then SIGKILL) the subprocess; reset tab to ready state.
8. **Clipboard bindings** via muda Edit menu items (cut: / copy: / paste: / selectAll:).
9. **Session persistence across app restart.** Store the per-tab continuation anchor in workspace JSON (session_id for Claude, session file path for pi). Resume on tab reopen.
10. **`extension_ui_request` handling (pi-only)** — render `notify` as inline toasts, apply `setTitle` to the tab label, `setStatus` to the tab header.
11. **Tool-call polish** — collapsible cards, inline diffs for Edit, bash output preview, syntax highlighting.
12. **Markdown rendering** for assistant text.
13. **Prompt history** (up-arrow, tab-local).
14. **Styling pass** — Catppuccin theme, font/spacing alignment.

Rough size: **2.5 weeks** of focused work for items 1-9 (shippable v1 with both backends). Items 10-14 are polish.

## Parallel investment line: pi-side extensions

Separate from gitterm v1, worth tracking as an ongoing area:
- Design + implement a permission-event protocol in pi (feeds into item 6 above)
- Gitterm-aware skills / extensions (e.g. `/commit-with-context` that pulls from the active tab's working dir, `/diff-this-tab`)
- Whatever else becomes obvious once v1 ships and you use it daily

This is investment the Claude-Code backend can never benefit from. Worth naming as a first-class line of work rather than "stuff we might do with pi someday."

## Day-2 state

- `examples/pi_webview_test.{rs,html}` added. Compiles cleanly. Not yet driven end-to-end (user will run shortly).
- No other code changes from yesterday.
- Both spikes (`claude_webview_test`, `pi_webview_test`) run as independent `cargo run --example …` binaries.
- Total API spend across both days estimated ~$0.50-0.80 (mostly opus yesterday; minimal codex-mini today pre-run).

## Additional pi-specific unknowns

- **Tool-use event variant names.** The pi `assistantMessageEvent` delta types for tool-use args (`tool_use_input_delta` vs `tool_use_delta` vs other) weren't captured in today's exploratory run because the test prompt didn't invoke a tool. The spike parser handles both defensively; once a real tool call runs through it, confirm the actual name and tighten.
- **`message_end` vs `turn_end` for usage finalization.** Which event carries the final usage snapshot is assumed, not verified — the spike grabs usage from every `message_update` and freezes on stream exit, so it's robust regardless. Confirm when you drive it.
- **pi permissions behavior in `--print` mode.** pi's permission system is extensible by definition (it's your code), but the spike just bypasses whatever it does by default. Before item 6 above, decide what the permission-event protocol should look like — that's a design call, not a discovery.
- **rpc mode vs json mode.** Spike uses `--mode json` because `--mode rpc` didn't produce model output in `--print` (likely designed for interactive/persistent clients). Worth understanding what rpc mode wants so we can decide whether persistent-subprocess v1 should use rpc.

---

# Day 2 continued: UX polish complete — 2026-04-24

The morning checkpoint above captured "pi spike exists and works." The rest of day 2 was a sustained UX iteration pass that took the spike from "pipe works" to "genuinely pleasant to use daily." Everything below is reflected in the current `examples/pi_webview_test.{rs,html}` and needs to be ported into v1 integration code (except item 3 below, which was a bug fix — the port just uses the correct event names).

## What was added, feature by feature

1. **Stop button.** Submit button is now dual-mode — "Send" when idle, "Stop" (red) when streaming. Click while streaming sends a `{"type":"stop"}` IPC message; Rust side has a shared `StopSlot` (Arc<Mutex<Option<oneshot::Sender<()>>>>) and the tokio-thread `run_pi` uses `tokio::select!` on the oneshot receiver + stdout line reads, calling `child.start_kill()` on stop. Stop emits a dedicated `AppEvent::Stopped` → `window.__streamStopped()` that freezes the activity bar with a neutral "Stopped" label and grey dot — **not** an error block. User stops are first-class, not error states.

2. **Sticky scroll (state-based).** First attempt used per-mutation "wasAtBottom" checks; it failed under fast streaming because the user could never accumulate wheel distance past the threshold before the next delta scrolled them back to bottom. Second attempt (the one that works) tracks scroll direction via a scroll event listener: the moment the user scrolls UP, `autoFollowing = false`; scrolling back to near-bottom re-enables. `followBottom()` is a no-op when `autoFollowing` is false. This is the only reliable approach under streaming; per-mutation checks can't escape the threshold.

3. **Pi event-name corrections (bug fix, not a feature).** Before today, my parser assumed pi's delta events were `tool_use_start/input_delta/stop` — guessed from Claude Code's vocabulary. Actual pi names:
   - `toolcall_start` / `toolcall_delta` / `toolcall_end` (not `tool_use_*`; `_end` not `_stop`)
   - `thinking_start` / `thinking_delta` / `thinking_end` (not `_stop`)
   - `text_start` / `text_delta` / `text_end` (not `_stop`)
   - **Tool results arrive as a dedicated `role: "toolResult"` message, NOT as `role: "user"` with a tool_result content item** (that's Claude's shape). pi's toolResult message has `toolName`, `toolCallId`, `isError`, and `content: [{type: "text", text: ...}]` at the top level.
   - `tool_execution_start/update/end` events also fire between the Edit's `toolcall_end` and the `toolResult` `message_end` — currently ignored; could drive a more precise "Running X" activity label in v1.

4. **Tool-card auto-collapse with Edit/Write exemption.** Previous tool pair collapses when the next tool starts (keeps the transcript clean over long turns with many tool calls). Also collapses when the final text answer begins. **Edit and Write pairs never auto-collapse** because the diff view is the whole point of showing them — the user wants to see what changed. Collapsed blocks render a compact "tool: Name · args-summary · N lines" header and are clickable to toggle. Tool-specific arg summary logic: Read→path, Bash→command, Grep→pattern, etc.

5. **Collapsible tool_result blocks with tool-aware head/tail preview.** When `tool_result` content exceeds 30 lines, render a collapsed preview. Head preview (first 20 lines) for file-reading tools (Read, Grep, Glob, LS, Edit, Write). Tail preview (last 20 lines) for Bash/shell where summaries/errors are at the end. Expand button toggles to full; scroll position anchors to the block so toggling doesn't throw the user.

6. **Markdown rendering (marked.js via CDN).** Final answer text now renders as proper markdown: code fences, inline code pills, bulleted/numbered lists with correct indentation, headers, links, blockquotes, tables. Re-renders the full accumulated text on every `text_delta` (accept text-selection loss during streaming — selection survives after streaming ends). Thinking blocks stay as plain italic text, not markdown, because model raw thinking contains partial/mangled markdown that shouldn't be rendered.

7. **Syntax highlighting (highlight.js via CDN).** Fenced code blocks get full syntax highlighting. Tried jsdelivr's `/lib/highlight.min.js` first — it's core-only with no language modules. Switched to cdnjs's bundled build which includes ~40 common languages (Rust, TS/JS, Python, bash, JSON, CSS, HTML, YAML, SQL, Go, Java, C/C++, etc.). github-dark theme (close enough to Catppuccin for spike; swap to a Catppuccin hljs theme in v1 if you want exact palette match). Re-highlights on every text_delta — accepted cost; debounce or defer to `text_end` if perf becomes an issue in v1.

8. **CSS semantic tints.** Matches pi's terminal vocabulary: inline code pills use Catppuccin green (`--user: #a6e3a1`), headers use Catppuccin peach (`--tool: #fab387`). `pre code` (inside code fences) uses `color: unset` so hljs theme colors win inside fences while inline code stays green.

9. **Edit tool diff rendering.** When `toolcall_end` fires on an Edit call, we now render a diff view instead of raw JSON args. Uses `diff` library's `Diff.diffLines()` for LCS-based line-level diffing. Red removed lines, green added lines, dim context. Each hunk separated by `───` when multiple edits are in one Edit call (pi supports `{path, edits: [{oldText, newText}, ...]}` — array of hunks). Also handles Claude's `{file_path, old_string, new_string}` shape for cross-backend reuse. Write tool renders as all-additions with "(new file)" suffix.

10. **`@-mention` file picker.** Textarea triggers a popup on `@` at word-boundary. Popup shows files from the project (gathered via `git ls-files` at Rust startup, injected via `evaluate_script` as `window.__files`). Substring-match filter as user types. Arrow keys navigate, Enter/Tab inserts, Esc cancels, click also works. Max 30 items shown. Blur closes popup (after short delay so mousedown on items still works).

11. **Keyboard shortcuts via muda.** Added an Edit submenu with `PredefinedMenuItem::cut/copy/paste/select_all(None)` — these create NSMenuItems with standard AppKit selectors (`cut:`, `copy:`, `paste:`, `selectAll:`) which WKWebView's first responder knows how to handle. Also added an App submenu (macOS requires this as first submenu) with About/Hide/Quit. Cmd+C/V/X/A/Q now work throughout the webview. Held as `let _menu = build_app_menu()` in main() for lifetime.

12. **Enter-to-submit (was Cmd+Enter).** Changed default. Plain Enter submits; Shift+Enter inserts a newline. Matches terminal Claude Code / pi conventions so keystrokes feel the same across tools.

## Technical notes worth remembering for v1

- **highlight.js CDN URL matters.** Use `https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js` (bundled with common languages) — NOT jsdelivr's `/lib/highlight.min.js` which is core-only.
- **tao 0.31 `with_user_event` is on `EventLoopBuilder`, not `EventLoop`**. `EventLoopBuilder::<MyEvent>::with_user_event().build()`.
- **wry IPC handlers run on main thread on macOS.** Can't directly call `webview.evaluate_script` from inside the IPC handler (webview is !Send), but can take/fire a oneshot sender from a shared Arc<Mutex<...>> to signal the tokio runtime.
- **`muda::PredefinedMenuItem::*(None)`** creates items with the *default* system text + the right AppKit selector wired up. Don't override the text — the system translations are correct per locale.
- **Codex thinking streams real content.** Unlike opus via Claude Code (signature-only thinking, rendered via ambient spinner). Important UX character difference between backends.
- **Marked renders raw markdown through on every delta without errors.** Unclosed code fences render as open code blocks and close when the `` ``` `` arrives. No need for a "render only on text_end" fallback.
- **Diff library: `diff@5.1.0` on jsdelivr** — `Diff.diffLines(old, new)` returns `[{value, added, removed}]` where `value` includes a trailing `\n`. Split + drop trailing empty before rendering.
- **Collapsed tool_use blocks need click handlers attached at block-creation time**, not lazily. Attach once in `makeToolPair`; toggle via `pair.isCollapsed` state.
- **Sticky-scroll is state-based, not mutation-based.** Scroll listener updates `autoFollowing` flag; every DOM mutation calls `followBottom()` which is a no-op when the flag is false. Per-mutation `wasAtBottom()` checks fail under streaming.
- **tool_result `toolName` is on the toolResult message itself**, not inferred from context. Use it to pick head vs tail preview and for tool-pair summary strings.
- **`git ls-files` returns git-tracked files respecting .gitignore.** Perfect for the @-mention file list — no need for `walkdir` or `ignore` crate complexity.

## Third-party dependencies added to the spike

All CDN-loaded (no Cargo changes for these):
- `https://cdn.jsdelivr.net/npm/marked/marked.min.js` — markdown rendering
- `https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js` — syntax highlighting (bundled with common languages)
- `https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/styles/github-dark.min.css` — hljs theme
- `https://cdn.jsdelivr.net/npm/diff@5.1.0/dist/diff.min.js` — line-level diff for Edit rendering

For v1 integration these should likely be vendored into gitterm's asset bundle rather than CDN-loaded — the user must not need network for the Claude/pi tabs to be styled correctly. Minimal total weight is ~150KB.

Cargo changes (already in place from morning):
- `tokio` features include `"macros"`
- `tao = "0.31"` in `[dev-dependencies]`

## Files as of checkpoint

- `examples/claude_pipe_test.rs` — hour-1 no-UI Claude pipe verifier (unchanged today)
- `examples/claude_webview_test.rs` + `examples/claude_webview_test.html` — Claude spike (unchanged today; still on yesterday's UX pre-polish)
- `examples/pi_webview_test.rs` — full interactive pi spike with stop, muda menu, file list injection
- `examples/pi_webview_test.html` — UX-polished chat UI (markdown, syntax highlight, diff, auto-collapse, @-mention, sticky scroll, stop)

No `src/` changes. All spike code is still disposable.

## Deliberate decision: no port to claude_webview_test

Considered porting today's UX to the Claude spike for parity. Decided not to — porting would consume ~30 min of mechanical edit-copying that we'd throw away when v1 integration builds the real feature with both backends via the pluggable architecture. The Claude spike stays useful as a "yesterday's simpler version" reference if needed; the pi spike is the current high-water mark.

## Integration: what to do first

Phase A for the next session is **research + plan**, not code. Concretely:

1. Launch an `Explore` agent to map out the integration surface:
   - Current state of `src/webview.rs` after yesterday's changes (it shouldn't have changed but confirm)
   - How `src/main.rs` uses the webview (lifecycle — when it's built, shown, hidden, bounds-updated, and which tab kinds trigger it)
   - Where `TabState` lives, what fields it has today, how tab kinds are expressed (if at all)
   - Event enum — structure, dispatch, where a new variant set would slot in
   - Workspace + instance config schema — what needs to serialize for an agent tab kind
   - Existing patterns for non-terminal tab content (file viewer is the closest analog)
2. Write `.plans/agent-tab-integration.md` (new file, distinct from this spike plan) with concrete ordered steps for v1. First 3-4 steps scoped to be one-session-each.
3. **Smallest meaningful first code change**: extend `src/webview.rs` with a new `evaluate_script(script: &str)` passthrough and add IPC-handler support to the existing child-view webview — without breaking the markdown/excalidraw viewers that use it today. Small, verifiable, foundational.

See the "Revised v1 punch list" section above (in day-2 morning checkpoint) for the full ordered v1 plan. That list stands.
