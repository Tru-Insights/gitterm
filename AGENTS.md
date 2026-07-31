# Repository Agent Instructions

This file is the canonical instruction source for this repository. It starts
from `agent-workflow-kit`; keep durable repo rules here, keep workflow facts in
`agent-workflow.config.json`, and keep personal/runtime state out of git.

If another local instruction file, generated wrapper, skill, command, memory
file, or checklist conflicts with this file, follow this file and call out the
conflict.

## Workflow Facts

- Project: `gitterm-v4`
- Default base branch: `v4`
- Protected branches: `v4`
- Issue provider: `linear`
- Issue key pattern: `[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}`
- Issue required in commits: `true`
- Issue required in PRs: `true`
- Draft PRs by default: `true`
- PR-ready attestation required: `true`

Configured commands:

- dev: `cargo run`
- lint: `cargo clippy -- -D warnings`
- check: `cargo fmt -- --check && cargo clippy -- -D warnings`
- test: `cargo test`
- build: `cargo build`

## Resume Checkpoints

The cross-agent checkpoint file is `.agents/checkpoint.md` in the current
project. Claude, Codex, and Pi should all use this same file.

At the start of a new session, resumed session, fork, or after compaction, check
whether `.agents/checkpoint.md` exists.

If it exists:

- Read `.agents/checkpoint.md` before doing substantive work.
- Use it as tactical orientation for branch state, recent work, hot files, and
  the immediate next step.
- Treat the user's current request and this file as higher priority than the
  checkpoint if they conflict.
- Mention the checkpoint only when it affects what you are about to do.

Do not create, modify, or delete `.agents/checkpoint.md` unless the user asks to
checkpoint or save progress. Do not write new checkpoints to harness-specific
paths such as `.codex/resume.md`, `.claude/resume.md`, or
`.plans/CHECKPOINT.md`.

## Code Safety Rules

- Read the existing working code before modifying it. Preserve behavior unless
  the requested change explicitly requires different behavior.
- Do not guess field names, response shapes, endpoint paths, status values,
  environment variables, or data contracts. Verify against existing code, docs,
  tests, or backend contracts.
- Do not add silent fallback logic for required values. Missing required data
  should fail loudly with useful context.
- Do not swallow errors. Every catch block must either rethrow or log useful
  operation/input context and surface an observable state when action is needed.
- Do not add caching unless explicitly requested and reviewed for invalidation,
  scope, and stale-data behavior.
- Keep edits scoped to the requested behavior. Avoid unrelated refactors,
  formatting churn, dependency changes, generated output, or broad rewrites.
- Do not run destructive production, data, git, or filesystem operations without
  explicit approval and a clear statement of what will be affected.

## Build, Test, And Development

Use the configured repo commands as the source of truth:

- dev: `cargo run`
- lint: `cargo clippy -- -D warnings`
- check: `cargo fmt -- --check && cargo clippy -- -D warnings`
- test: `cargo test`
- build: `cargo build`

If a command is missing, unavailable, or contradicted by repo-specific
instructions, stop and call out the conflict before guessing a package-manager
default.

Run focused tests for behavior changes. Broaden verification when the change
touches shared contracts, runtime boundaries, security, data migrations,
generated artifacts, or user-facing workflows.

## Skill And Command Routing

Canonical skill source lives in `.agents/skills`. Edit skills there rather than
in harness-specific global locations.

Repo-local commands should be thin wrappers around this file, `.agents/skills`,
and configured repo commands. Do not put repo workflow rules in global harness
commands.

Use the smallest command that matches the job:

- `/checkpoint` writes only `.agents/checkpoint.md`.
- `/done` writes a human-facing session log only, when configured locally.
- `/review` performs a quick local pre-commit review. It is not PR readiness.
- `/pr-ready` is the strict readiness gate. Current-branch mode is author
  preflight; explicit PR mode is required before requesting review, approving,
  marking ready, or merging an existing PR.
- `/pr` creates or updates a PR. Default to draft when `github.draftPrByDefault`
  is true.
- `/commit` creates focused commits following the issue policy below.

Avoid overlapping review commands for the same decision. If the question is "can
this PR request review or merge?", use `/pr-ready`.

## Commit Guidelines

Commit subjects should be short, imperative, and action-led.

When `issueTracking.requireIssueInCommit` is true, commit messages must include
an issue key matching `[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}`. If the issue cannot be
inferred from branch name, current PR, or recent context, ask the user to choose
or create an issue before committing.

Keep commits focused. Do not bundle unrelated cleanup, refactors, generated
changes, or dependency bumps with behavior fixes unless the user explicitly
approved that scope.

## Pull Request Guidelines

PRs should include:

- a clear summary
- linked issue keys when required by config or repo rules
- tests and verification performed
- screenshots or recordings for visual changes
- environment, migration, deployment, or rollout notes when relevant

Draft PRs are allowed for early CI or collaboration, but they must not request
review until `/pr-ready` accepts the current state.

## PR Submission Gate

Before opening a ready-for-review PR, requesting review, marking a draft PR
ready, approving, or merging, run `/pr-ready`.

For an existing PR, use explicit PR mode:

```text
/pr-ready --pr <number>
```

If `github.requirePrReadyBeforeReview` is true, the PR body must contain a
current-head readiness block before review or merge:

```markdown
## Review readiness

- [x] `/pr-ready --pr <number>` returned `ACCEPT-READY`
- Head SHA: <current 40-character PR head SHA>
- Reviewed at: <ISO-8601 UTC timestamp>
```

Any new commit makes the attestation stale and requires rerunning explicit PR
mode.

### Hard Blockers

Any one of these blocks review or merge:

- Missing required issue reference in the PR title, body, branch, commit, or
  linked issue.
- PR scope is not atomic.
- Base branch does not match the intended lane.
- Prior requested changes or unresolved review threads are not explicitly
  addressed.
- Behavior changed without targeted automated regression coverage, unless the
  repo explicitly permits manual-only verification and the risk is stated.
- Required verification commands, targeted tests, or build checks are failing,
  skipped, or only assumed.
- CI is failing, pending too long to judge, or attached to a stale head SHA.
- Submodule, generated, lockfile, dependency, runtime, or build config changes
  are unexplained.
- Repo ownership boundaries are crossed without the documented upstream change.
- Required data contracts are guessed instead of verified.
- Error handling swallows failures or omits useful operation/input context.
- Build/runtime risks are unverified after touching bundlers, SSR/client
  boundaries, dynamic imports, environment variables, assets, or production-only
  paths.
- Security basics regress: secrets, unsafe logs, injection risk, overbroad
  permissions, auth bypass, missing authorization boundary, or sensitive data
  exposure.
- The repo requires a current-head `/pr-ready --pr <number>` attestation and the
  PR body lacks one for the current head SHA.

### Required Review Posture

- Look for review blockers first, not style nits.
- Lead with concrete findings ordered by severity.
- Cite file and line evidence for each blocker.
- If no blockers remain, say that explicitly and still note residual risk or
  coverage gaps.
- Do not request review, approve, mark ready, or merge based on intent, PR
  summary, or manual notes alone.

## Memory And Preferences

Agent memory is private runtime state, not repository source of truth.

- Do not write repo rules, durable technical docs, backlog snapshots, or secrets
  to memory.
- Runtime memory belongs in ignored files such as `.agents/memory.md`.
- Personal repo workflow preferences belong in ignored files such as
  `.agents/preferences.md`.
- Promote shareable repo rules to this file and durable technical context to
  checked-in docs only when explicitly asked.
- Never store secrets, credentials, tokens, or connection strings in memory or
  preference files.

## Repo-Specific Rules

### Project

GitTerm V4 — a Rust desktop app: terminal multiplexer + git status viewer + file
explorer + agent (Claude Code / pi) host, built with the
[Iced](https://github.com/iced-rs/iced) v0.14 GUI framework. macOS-first;
Windows CI exists.

### Architecture

- **Single-file app** by convention: most logic lives in `src/main.rs` (~14k
  lines). Don't split into submodules unless the change is large and contained.
- **Supporting modules**: `src/log_server.rs` (warp localhost server),
  `src/plans_viewer.rs` (plans viewer routes), `src/markdown.rs`,
  `src/webview.rs` (singleton wry WebView), `src/services.rs`, `src/agent.rs`,
  `src/tab/` (TabKind enum + AgentSession), `src/events.rs`, `src/config.rs`,
  `src/theme.rs`.
- **Embedded WebView**: one wry `WebView` instance per app, repurposed for
  markdown viewer / Excalidraw / agent chat / plans viewer. See
  `webview::set_pending_content`, `set_pending_url`, `navigate_to_url`.
- **Terminal**: uses `iced_term` fork at `../iced_term_fork`. The Windows CI
  workflow clones from `https://github.com/Tru-Insights/iced_term.git` master
  fresh — push local fork changes before triggering Windows CI.
- **Theme**: Catppuccin Mocha (dark) / Latte (light) via
  `theme::AppTheme::{Dark, Light}` and `markdown::ThemeColors`.

### Persistence

- Config: `~/.config/gitterm-v4/config.json` (global) +
  `~/.config/gitterm-v4/instance-<pid>/config.json` (per-instance). Per-instance
  values override globals.
- Workspaces: `~/.config/gitterm-v4/workspaces.json`.
- All paths resolved via `dirs::home_dir()` — on Windows that's
  `%USERPROFILE%\.config\gitterm-v4\`.
- V4 must remain runtime-isolated from V3: do not reuse V3 config paths,
  bundle identifiers, app names, log-server port ranges, helper state, temporary
  artifact names, or future browser profiles.

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

- Debug: `cargo run` (the configured `dev` command). Log server defaults on.
- Voice is enabled by default (the `stt` default feature pulls `whisper-rs` +
  `cpal`). Use `--no-default-features` only for a build without voice.
- macOS bundle: `cargo bundle --release` produces
  `target/release/bundle/osx/GitTerm V4.app`. Install via `cp -R` to
  `/Applications/`.
- Excalidraw is an optional feature: add `--features excalidraw` to enable.

### CI Gates

`.github/workflows/*.yml` runs the configured `check`, `test`, and release
build with all features. Clippy is `-D warnings` — there is one justified
`#[allow(clippy::large_enum_variant)]` on `TabKind` (rationale in the source).
Don't "fix" it.

### Linear

Default team is **TRU** (Truinsights workspace), not the GIT (Git Term) team —
that's a deliberate user preference. Create issues with
`linear --workspace truinsights issue create --team TRU ...`.

### Design Roadmap

See `.plans/` for active design docs (the in-app **Plans** sidebar tab lists
them). Notable: `plans-viewer-integration.md` (the viewer itself),
`agent-tab-integration.md` (Claude Code / pi tab kind), and
`design/WORKSPACE_DESIGN.md` for the multi-phase workspace plan.
