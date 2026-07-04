# Chats Panel — workspace-scoped session browser with resume-as-tab

**Mockup (interactive):** https://claude.ai/code/artifact/1299f08e-68ae-46a0-8c76-326dba0d341c
**Status:** planned · **Ticket:** [TRU-78](https://linear.app/truinsights/issue/TRU-78/chats-panel-workspace-scoped-session-browser-with-resume-as-tab)

## Problem

Every harness (claude, codex, pi) already persists every conversation as a
JSONL transcript keyed by session id + starting directory. Nothing is ever
lost when a tab closes — but there is no way to *find* a conversation again.
Concretely (2026-07-04 survey of this machine): 314 claude sessions across 47
directory slugs, 121 codex rollouts, pi sessions in the same shape. Worktrees
make it worse: claude keys sessions by exact cwd, so one repo's chats scatter
across `cree8-bun-api`, `-pr141`, `-hotfix`, `/private/tmp/...-surface-cleanup`,
and `--continue` in the main checkout never surfaces them.

## Design principles (user-confirmed via mockup)

1. **Workspace is the organizing principle, not the chat.** The panel is a
   sidebar tab (next to Git/Files/Agent/Plans) scoped to the active workspace
   by default. Chats are the workspace's memory, not a competing top level.
2. **"All workspaces" is an escape hatch**, for "I can't remember where that
   conversation happened." Groups there carry workspace attribution; scoped
   search offers "N matches in other workspaces" when local search misses.
3. **Resume follows the chat home.** Resuming from the All view switches to
   the owning workspace (reopening it if closed) and opens the tab there, in
   the conversation's recorded cwd.
4. **One conversation ⇒ at most one live tab (the registry rule).** A live
   chat shows "● open"; clicking focuses the existing tab — across workspaces
   if needed — never spawns a second process on the same session file.
5. **Read-only over harness files.** The index never writes or moves
   transcripts; resume is pure process-spawning. Harnesses stay untouched.

## Data model

### Session sources (backend adapters, config-driven like session_commands)

| backend | store | resume command |
|---|---|---|
| claude | `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` | `claude --resume <uuid>` (run in recorded cwd) |
| codex  | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `codex resume <id>` |
| pi     | `~/.pi/agent/sessions/<cwd-slug>/<ts>_<uuid>.jsonl` | `pi --resume <id>` |
| remote | agentd `sessions` capability via WorkspaceSource | `gitterm-agent attach` path (already exists) |

Adapter = { glob pattern, id extraction, metadata parse, resume command
template }. New backends are config, not code.

### Index entry (cached, built in background)

id, backend, cwd, repo-root (git common dir — collapses worktrees into one
repo group), branch (claude records gitBranch), title (claude summary line;
first user message fallback), mtime, size, message count, workspace
(derived: cwd under workspace root incl. worktrees; exact when GitTerm
spawned the tab and stamped it), flags: live / possibly-running / dead-cwd /
closed-workspace.

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

- Sidebar tab **Chats**: search box, scope toggle [This workspace | All
  workspaces], backend filter chips (claude/codex/pi/remote), list grouped by
  repo (worktree chats badge under their repo group).
- Row: title, backend dot, age, branch, badges (● open / ◐ possibly running /
  worktree / ⚠ dir gone).
- Main-pane preview on select: metadata header (backend, msgs, size, branch,
  age, cwd) + transcript tail (last N messages, parsed lazily) + primary
  action:
  - dormant → **Resume as Tab** (shows the exact command + cwd it will run)
  - live → **Go to Tab** (focuses, jumping workspaces if needed)
  - dead cwd → disabled + "Resume in <repo root> instead"
  - closed workspace → "Reopen workspace & resume"

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
- [ ] 4. Remote sessions through WorkspaceSource (subsumes the TRU-77
        "session reattach UI" slice; remote group looks identical to local).
- [ ] 5. Closed-workspace reopen-on-resume.
- [ ] 6. (Optional, later) per-tab composer input box that writes to the PTY —
        deprioritized; voice input covers most of this need.

## Explicitly out of scope

- No new chat rendering engine — resume always reopens the real harness TUI.
- No editing/moving/deleting transcript files from the panel (read-only v1).
- No replacement of the "+" new-session picker.
