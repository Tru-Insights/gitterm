# Repository Agent Instructions

This file is the canonical instruction source for this repository. The repo's
own rules come first; the agent kit section at the end is rendered from
`agent-workflow.config.json` (commands, base branch, issue keys, workflow
skills and change guidelines). Keep personal/runtime state out of git.

If another local instruction file, generated wrapper, skill, command, memory
file, or checklist conflicts with this file, follow this file and call out the
conflict.

## Code Safety Rules

These add to the kit's change guidelines below.

- Do not add caching unless explicitly requested and reviewed for invalidation,
  scope, and stale-data behavior.
- Do not run destructive production, data, git, or filesystem operations without
  explicit approval and a clear statement of what will be affected.

## Pull Request Guidelines

PRs should include:

- a clear summary
- linked issue keys
- tests and verification performed
- screenshots or recordings for visual changes
- environment, migration, deployment, or rollout notes when relevant

Draft PRs are allowed for early CI or collaboration, but they must not request
review until the `pr-ready` skill reports Ready.

### Hard Blockers

In addition to the `pr-ready` skill's checks, any one of these blocks review or
merge:

- Missing issue reference in the PR title, body, branch, commit, or linked
  issue.
- PR scope is not atomic.
- Prior requested changes or unresolved review threads are not explicitly
  addressed.
- Required verification commands, targeted tests, or build checks are failing,
  skipped, or only assumed.
- CI is failing, pending too long to judge, or attached to a stale head SHA.
- Submodule, generated, lockfile, dependency, runtime, or build config changes
  are unexplained.
- Repo ownership boundaries are crossed without the documented upstream change.
- Required data contracts are guessed instead of verified.
- Build/runtime risks are unverified after touching bundlers, SSR/client
  boundaries, dynamic imports, environment variables, assets, or production-only
  paths.
- Security basics regress: secrets, unsafe logs, injection risk, overbroad
  permissions, auth bypass, missing authorization boundary, or sensitive data
  exposure.

### Required Review Posture

- Look for review blockers first, not style nits.
- Lead with concrete findings ordered by severity.
- Cite file and line evidence for each blocker.
- If no blockers remain, say that explicitly and still note residual risk or
  coverage gaps.
- Do not request review, approve, mark ready, or merge based on intent, PR
  summary, or manual notes alone.

## Repo-Specific Rules

### Project

GitTerm V6 — a Rust desktop app: terminal multiplexer + git status viewer + file
explorer + agent (Claude Code / pi) host, built with the
[Iced](https://github.com/iced-rs/iced) v0.14 GUI framework. macOS-first;
Windows CI exists.

### Architecture

- **Single-file app** by convention: most logic lives in `src/main.rs`. Don't
  split into submodules unless the change is large and contained.
- **Supporting modules**: `src/log_server.rs` (warp localhost server),
  `src/plans_viewer.rs` (plans viewer routes), `src/markdown.rs`,
  `src/webview.rs` (wry WebView surfaces), `src/services.rs`,
  `src/tab/` (TabKind enum + AgentSession), `src/events.rs`, `src/config.rs`,
  `src/theme.rs`.
- **Embedded WebView**: wry child `WebView` surfaces keyed by
  `webview::WebviewSurface`. `Agent(tab_id)` is one agent tab's chat page (its
  own IPC handler installed at construction; the page holds its tab id via
  `window.__setTabId`). Each chat tab keeps its own page alive, hidden, while
  another tab, a file or the plans viewer is shown, and keeps receiving the
  tab's events, so switching back is show/hide with scroll, tool cards and
  composer draft intact. At most `MAX_AGENT_PAGES` (4) pages live per process
  (`App::agent_pages`, most recently shown first, `promote_agent_page`); the
  least recently shown is destroyed and rebuilt and replayed from
  `AgentSession.conversation` (the source of truth) on its next show, via
  `AgentWebviewCreated`. A closed tab's page is destroyed
  (`prune_agent_pages`). `Viewer` shows markdown / HTML / Excalidraw or the
  plans viewer. At most one surface is visible, Viewer over the agent page
  (`visible_webview_surface` in `main.rs`). See
  `webview::set_pending_content`, `set_pending_url`, `navigate_to_url`.
- **Terminal**: uses `iced_term` fork at `../iced_term_fork`. The Windows CI
  workflow clones `Tru-Insights/iced_term` master fresh —
  push local fork changes before triggering Windows CI.
- **Theme**: Catppuccin Mocha (dark) / Latte (light) via
  `theme::AppTheme::{Dark, Light}` and `markdown::ThemeColors`.

### Persistence

- Settings: `~/.config/gitterm-v6/settings.json`, shared by every instance and
  kept across restarts (TRU-181); see `config::SETTINGS_FILE`. It is built
  once from the newest `instance-<pid>/config.json` (where settings lived
  before) when missing. A malformed file is logged and never overwritten.
  `instance-<pid>/` holds per-launch state only and is removed on exit.
- Workspaces: `~/.config/gitterm-v6/workspaces.json`.
- All paths resolved via `dirs::home_dir()` — on Windows that's
  `%USERPROFILE%\.config\gitterm-v6\`.
- V6 must remain runtime-isolated from V5, V4 and V3: do not reuse earlier config
  paths, bundle identifiers, app and binary names, port ranges, env var names,
  agent proto package, agent config dir/service label, helper state, temporary
  artifact names, or browser profiles. The one sanctioned read of V5 state is
  the first-launch import (`src/config_import.rs`), which copies from
  `~/.config/gitterm-v5` (or `GITTERM_V5_CONFIG_DIR`) and never writes there.
- **A second instance must never share the real config dir.** The user may run
  an installed earlier version at the same time. `workspaces.json`,
  `settings.json`, `tasks.json`,
  `profiles.json`, `remote-agents.json`, `browser-profile/`, `worktrees/` and
  `task-briefs/` are shared across instances and each instance writes back its whole view
  (last writer wins), so a dev or test launch against `~/.config/gitterm-v6`
  can erase or resurrect tabs, tasks and chat sessions in the running app and
  contend for the Chrome profile lock. Every dev/test launch, by an agent or
  in a try-it checklist, sets `GITTERM_V6_CONFIG_DIR` to an absolute isolated
  directory, seeded with copies of the files above when the test needs real
  workspaces:

  ```
  D=$HOME/.config/gitterm-v6-<purpose>
  mkdir -p "$D" && cp ~/.config/gitterm-v6/{workspaces.json,tasks.json,profiles.json} "$D"/ 2>/dev/null
  GITTERM_V6_CONFIG_DIR="$D" cargo run
  ```

  An isolated instance skips the V5 import unless `GITTERM_V5_CONFIG_DIR`
  also points at a fixture dir, so it never reads the real V5 state.

  Never kill, signal, or replace the bundle of the running instance; prefer a
  headless example over driving the GUI to verify a change.

- Workers never launch `/Applications/Google Chrome.app`. For page checks,
  use the page-test harness or GitTerm's browser tools.

### Workspaces

- `App` holds `Vec<Workspace>` + `active_workspace_idx`. `Workspace` =
  `{name, abbrev, dir, color, tabs, active_tab}`.
- `Ctrl+1-9` switches workspace; `Cmd+1-9` switches tab within the active
  workspace.
- When the active workspace changes, call `sync_plans_dir()` so the plans
  viewer's `.plans/` resolution stays in sync.

### Iced Conventions

- `Task<Event>` for async work (file dialogs, command spawning, etc.). Don't
  block in `update()`.
- Window-coordinate WebView bounds: see `App::calculate_webview_bounds()`. The
  top 40px is reserved for headers (file viewer, plans viewer); the bottom
  reserves the console panel + workspace bar.

### Build & Bundling

- Debug: `cargo run`. Log server defaults on.
- Voice is enabled by default (the `stt` default feature pulls `whisper-rs` +
  `cpal`). Use `--no-default-features` only for a build without voice.
- macOS bundle: `cargo bundle --release` produces
  `target/release/bundle/osx/GitTerm V6.app`. The custom bundle script produces
  `target/GitTerm V6.app`. Install via `cp -R` to
  `/Applications/`.
- Excalidraw is an optional feature: add `--features excalidraw` to enable.

### CI Gates

`.github/workflows/*.yml` runs formatting, Clippy with and without the
`excalidraw` feature, tests with and without `excalidraw`, release builds, and
the macOS bundle script on their configured platforms. The pre-commit hook
(`.githooks/pre-commit`) runs Clippy, formatting, and `cargo test`. Clippy is
`-D warnings` — there is one justified
`#[allow(clippy::large_enum_variant)]` on `TabKind` (rationale in the source).
Don't "fix" it.

### Linear

Issues go to the `TRU` team in the `truinsights` Linear workspace; commit
messages need a `TRU-` key. Pass the workspace explicitly when using the Linear
CLI: `linear --workspace truinsights issue create --team TRU ...`.

### Design Roadmap

See `.plans/` for active design docs (the in-app **Work** page's Documents section lists
them). Notable: `plans-viewer-integration.md` (the viewer itself),
`agent-tab-integration.md` (Claude Code / pi tab kind), and
`design/WORKSPACE_DESIGN.md` for the multi-phase workspace plan.

<!-- agent-kit:begin v0.2.0 sha256:0807aa126676618b404fd7ebf21cc040bf21116aec2ee8f1c8f4bb4bc0f9a9b5 -->

# Agent instructions (agent kit)

This section is rendered by the agent kit from `agent-workflow.config.json`.
Change the config and re-run the kit's install rather than editing it here; a
hand edit stops the next install until it is resolved.

## Project facts

- Project: gitterm-v6
- Base branch: `v6`
- Every commit message contains an issue key matching `TRU-[0-9]+`.
- Protected branches: `v4`, `v5`, `v6`.

## Commands

Use these, not a package-manager default. Report their real exit codes.

| Task | Command |
| ---- | ------- |
| Build | `cargo build` |
| Test | `cargo test` |
| Lint | `cargo clippy -- -D warnings` |
| Format check | `cargo fmt -- --check` |

## Workflow skills

Skills live in `.agents/skills/<name>/SKILL.md` (Claude Code reads them through `.claude/skills/<name>/SKILL.md`):
`build`, `test`, `lint`, `checkpoint`, `commit`, `pr`, `pr-ready`. Use the matching skill
for those tasks.

## Change guidelines

These apply to every change you make. Where this repository's own rules
outside this section conflict with them, the repository's rules win.

### Before you write

- Read the task and the code it touches before changing anything.
- List every place the change must reach: callers, tests, fixtures, config,
  exports. That is the scope. Extra features are not.
- Check what the change could break for users: data it could destroy or
  expose, callers that stop working.
- Never invent API endpoints, field names, response shapes, data contracts
  or tool commands. Verify them from code, docs or the command's help.

### The smallest complete change

Take the first option that fully works:

1. Does it need to exist? Skip features, options and flexibility nobody
   asked for, and name what you skipped in one line.
2. Is it already in this codebase (a helper, component, pattern)? Use it the
   way the surrounding code does.
3. Does the standard library or platform do it? Use that, unless the project
   has its own.
4. Does an installed dependency do it? Use it. Never add a dependency for a
   few lines.
5. Otherwise, write the minimum code that works.

- Be lazy about the solution, never about the change: finish every part the
  task needs, including the callers, tests and fixtures it breaks.
- No abstraction, wrapper, option, config or "for later" code nobody asked
  for. Deleting beats adding. Keep the structure, layers and conventions the
  codebase already has. Prefer focused edits over rewrites.
- Preserve behaviour unless the task requires a change.
- Bug fix: grep every caller of the function you touch, then fix the root
  cause once, in the shared code.
- Code you move or merge keeps its error handling and validation.
- Between options of equal size, take the one that is correct on edge cases.
- Comment only the why the code cannot show.
- A shortcut with a known limit gets a comment:
  `shortcut: <the limit>, <when to upgrade>`.

### Never cut

- Validation at trust boundaries.
- Error handling that prevents data loss. Do not swallow errors; surface
  them with the operation and input that failed.
- Silent fallbacks for required values: fail loudly instead.
- Security and secrets handling, and accessibility.
- Anything the user asked for.

### Done means checked

- New non-trivial logic (a branch, a loop, a parser, money, security, data
  writes, a bug fix) leaves one small test that fails when it breaks.
  Trivial changes need none.
- Run the repository's own checks with their real exit codes. Never claim
  success without that evidence; if a check cannot run, say why.
- Review your own diff before saying the work is done.
- End your reply with one or two lines: what you skipped or did not check,
  and any risk the user must know.

Adapted in part from ponytail (MIT, https://github.com/dietrichgebert/ponytail).
<!-- agent-kit:end -->
