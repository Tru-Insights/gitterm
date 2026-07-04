# Chats Panel — workspace-scoped session browser with resume-as-tab

**Mockup (interactive, v2 — machine axis):** https://claude.ai/code/artifact/1299f08e-68ae-46a0-8c76-326dba0d341c
**Status:** in progress (slice 0 done) · **Ticket:** [TRU-78](https://linear.app/truinsights/issue/TRU-78/chats-panel-workspace-scoped-session-browser-with-resume-as-tab)

**Decisions (2026-07-04):** one workspace = one machine (a workspace never
spans machines; same repo on two machines = two workspaces). All configured
remotes are always visible, grouped by machine, marked reachable or not.

## Problem

Every harness (claude, codex, pi) already persists every conversation as a
JSONL transcript keyed by session id + starting directory. Nothing is ever
lost when a tab closes — but there is no way to *find* a conversation again.
Concretely (2026-07-04 survey of this machine): 314 claude sessions across 47
directory slugs, 121 codex rollouts, pi sessions in the same shape. Worktrees
make it worse: claude keys sessions by exact cwd, so one repo's chats scatter
across `cree8-bun-api`, `-pr141`, `-hotfix`, `/private/tmp/...-surface-cleanup`,
and `--continue` in the main checkout never surfaces them.

## Design principles (user-confirmed via mockup, v2 2026-07-04)

1. **Machine is the top axis; workspace is the day-to-day organizing
   principle.** The hierarchy is machine → workspace → chats. Every
   workspace is bound to exactly one machine (local or a remote host —
   already true in code: `WorkspaceSource` is `Local | RemoteAgent`).
   The same repo checked out on two machines is two workspaces, two
   separate chat groups — repo groups never merge across machines.
2. **A session's identity is (machine, backend, cwd, session-id).**
   Backend (claude/codex/pi) says which harness; machine says where the
   transcript lives — and resume can only happen there. "Remote" is a
   transport, not a backend: a remote machine has its own claude/codex/pi
   sessions, reached through agentd instead of the local filesystem.
3. **Scope is three rings: This workspace → This machine → Everywhere.**
   The panel defaults to the active workspace; "This machine" is the active
   workspace's machine; "Everywhere" groups machine → repo with workspace
   attribution. Scoped search misses widen ring by ring ("N matches
   elsewhere on this machine" / "N on other machines").
4. **Connected remotes are always visible.** Every configured remote shows
   in the workspace bar/rail grouped under its machine, marked reachable or
   unreachable. An unreachable machine keeps its place and its cached chat
   index; resume is disabled until it reconnects. (Otherwise remotes have
   no way to be visible at all — user-confirmed.)
5. **Resume follows the chat home.** Resuming from a wider scope switches
   to the owning workspace (reopening it if closed) on the owning machine
   and opens the tab there, in the conversation's recorded cwd. Local →
   spawn in recorded cwd; remote → agentd session + attach.
6. **One conversation ⇒ at most one live tab (the registry rule),** keyed
   on the full (machine, session-id). A live chat shows "● open"; clicking
   focuses the existing tab — across workspaces and machines — never a
   second process on the same session file.
7. **Read-only over harness files.** The index never writes or moves
   transcripts; resume is pure process-spawning. Harnesses stay untouched.

## Data model

### Session sources: machine × backend, not backend-with-remote-bolted-on

Backend adapters (config-driven like session_commands):

| backend | store (on the session's machine) | resume command |
|---|---|---|
| claude | `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` | `claude --resume <uuid>` (run in recorded cwd) |
| codex  | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `codex resume <id>` |
| pi     | `~/.pi/agent/sessions/<cwd-slug>/<ts>_<uuid>.jsonl` | `pi --resume <id>` |

Adapter = { glob pattern, id extraction, metadata parse, resume command
template }. New backends are config, not code.

Machines are the other axis. The **local machine** runs adapters against the
local filesystem. A **remote machine** exposes the same adapter data through
agentd (list/stat/tail of transcript files via the `sessions`/file
capabilities); resume there means starting an agentd session running the
adapter's resume command and attaching the tab to it. The remote chat index
is cached locally so Everywhere search works instantly and unreachable
machines still show their (stale-marked) chats.

### Index entry (cached, built in background)

id, **machine**, backend, cwd, repo-root (git common dir — collapses
worktrees into one repo group *within a machine*, never across machines),
branch (claude records gitBranch), title (claude summary line; first user
message fallback), mtime, size, message count, workspace (derived: cwd under
workspace root incl. worktrees, on the same machine; exact when GitTerm
spawned the tab and stamped it), flags: live / possibly-running / dead-cwd /
closed-workspace / machine-unreachable (index cached).

Headless sessions (claude entrypoint `sdk*` — `claude -p`/SDK runs such as
hook-spawned push reviews and standup writers) are excluded from the index:
they're one-shot automation, not conversations to go back to. On this
machine that's 165 of 215 session files (2026-07-04). Could become an
opt-in filter chip if ever needed. Missing entrypoint = interactive (old
transcripts stay visible).

### Tab↔session registry

- `WorkspaceTabConfig.agent_config` already carries backend config for
  `tab_kind: "agent"`; extend with `session_id` (+ workspace stamp implicit).
- New sessions launched from the "+" picker get a **pre-assigned id**
  (`claude --session-id <uuid>`; codex/pi equivalents) so the mapping is
  exact from birth. The "+" picker flow itself is unchanged.
- Blind spot: sessions the user starts by hand in a bare terminal. Heuristic:
  transcript file actively growing → "◐ possibly running", confirm before
  resume. Never block.

## UI

- Sidebar tab **Chats**: search box, scope toggle [This workspace | This
  machine | Everywhere], backend filter chips (claude/codex/pi), list grouped
  by repo (worktree chats badge under their repo group). Everywhere scope
  adds machine sections (this mac / each remote, with ● connected /
  ○ unreachable status) containing the repo groups.
- Row: title, backend dot, age, branch, badges (● open / ◐ possibly running /
  worktree / ⚠ dir gone).
- Workspace bar / rail: workspaces grouped by machine — local first, then
  each configured remote under its machine label, always visible with
  reachable/unreachable state.
- Main-pane preview on select: metadata header (backend, msgs, size, branch,
  age, cwd) + transcript tail (last N messages, parsed lazily) + primary
  action:
  - dormant → **Resume as Tab** (shows the exact command + cwd it will run;
    remote chats note the agentd session + machine)
  - live → **Go to Tab** (focuses, jumping workspaces/machines if needed)
  - dead cwd → disabled + "Resume in <repo root> instead"
  - closed workspace → "Reopen workspace & resume"
  - machine unreachable → disabled + "shown from cached index; resume when
    <machine> reconnects"

## Performance constraints (12–15 h/day daily driver)

- Never parse full transcripts for the index: first line + tail + stat only.
- Index built/refreshed on a background task; refresh on panel open and via
  cheap mtime polling or fs watcher. Zero work when panel closed.
- No filesystem access in update()/view() hot paths. Preview tail parsing is
  on-demand per selected chat, off-thread.

## Development lane: GitTerm v3

This is v3 work. It lives on the long-lived **`v3` branch**, developed in the
**`../gitterm-v3` worktree** — never in the `gitterm-v2` checkout, which stays
on `master` as the stable daily driver.

- v3 PRs target `v3`, branch names `tracey/tru-78-<slug>`. `master` stays the
  lane for v2 fixes; merge `master` → `v3` regularly so v3 never drifts.
- The worktree has its own `target/`, so v3 builds can never overwrite the
  daily driver's `target/release/gitterm`.
- **Slice 0 (first commit on v3): config isolation.** `GITTERM_CONFIG_DIR`
  env var (and/or `--config-dir` flag) overriding `global_config_dir()` +
  `instance_config_dir()`, and the window title marked "GitTerm v3-dev" when
  the override is active. Every v3 test instance runs with its own config dir
  — it must be impossible for a dev instance to read or write
  `~/.config/gitterm/*` (today's session proved shared config is how dev
  instances interrupt real work: workspace restore spawning tabs, pre-guard
  saves clobbering remote-agents.json).
- v3 graduates by merging to `master` only when it has replaced the daily
  driver in real use.

## Slices

- [ ] 0. Config isolation for dev instances (`GITTERM_CONFIG_DIR` +
        v3-dev window title). Prerequisite for all live testing.
- [ ] 1. Local claude index + workspace-scoped Chats sidebar tab (list,
        search, scope toggle, repo grouping, preview pane). Read-only.
- [ ] 2. Resume-as-tab + registry rule: spawn in recorded cwd, live badges,
        Go-to-Tab focus (cross-workspace jump), pre-assigned session ids for
        new picker-launched sessions, possibly-running heuristic, dead-cwd
        rescue.
- [ ] 3. Codex + pi adapters via config.
- [ ] 4. Remote machines: run the same adapters over agentd (list/stat/tail
        via WorkspaceSource `sessions` capability), machine sections in
        Everywhere scope, locally cached remote index with unreachable
        state, resume = agentd session + attach (subsumes the TRU-77
        "session reattach UI" slice).
- [ ] 4b. Workspace bar/rail grouped by machine, remotes always visible
        with reachable/unreachable state (own mockup before build — this
        touches the whole rail, not just Chats).
- [ ] 5. Closed-workspace reopen-on-resume.
- [ ] 6. (Optional, later) per-tab composer input box that writes to the PTY —
        deprioritized; voice input covers most of this need.

## Explicitly out of scope

- No new chat rendering engine — resume always reopens the real harness TUI.
- No editing/moving/deleting transcript files from the panel (read-only v1).
- No replacement of the "+" new-session picker.
