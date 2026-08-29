# GitTerm V5 Task and Sprite Implementation Plan

**Status:** Slices 0-6 complete (Slice 6 delivery/publish/transcripts: TRU-117/118/119); Slice 7 Mac mini executor next

**Captured:** 2026-08-19

**Architecture:** `task-sprite-architecture.md`

## Delivery Strategy

Build the task foundation in small, usable slices. Local worktrees are the
first executor. Mac mini dispatch follows only after the local task lifecycle
is useful in daily work.

This work uses the isolated `v5` development lane at `../gitterm-v5`, based on
merged V4 commit `f1c3c4c`. The attention-inbox work is already included in
that base. The work is tracked by parent issue TRU-96 with slice-level child
issues; Slice 0 is TRU-100. Commits and PRs must continue to include their slice
issue key.

Each slice must preserve ordinary workspaces, tabs, Chats, and remote sessions.
Task support is additive; ad hoc terminals and agent conversations remain valid.

## Slice 0 - Create the Isolated V5 Lane

**Goal:** Make V5 safe to build and run alongside the V4 daily driver before
adding task behavior.

Tasks:

- [x] Select merged V4 commit `f1c3c4c` as the exact base.
- [x] Create the local `v5` branch and `../gitterm-v5` worktree.
- [x] Rename the application/package/binary and macOS bundle identity to V5.
- [x] Introduce V5-only roots for config, workspaces, future tasks, tokens,
  profiles, browser state, logs, helper state, and temporary artifacts.
- [x] Allocate V5-only log-server/browser-MCP ports.
- [x] Rename and isolate the remote helper as `gitterm-v5-agent`, including config
  root, token/service identity, and default port.
- [x] Make the window title unmistakably V5 development.
- [x] Update V5's repository instructions and build documentation without changing
  V4's checked-out runtime state.
- [x] Add regression tests proving V5 cannot resolve V4 config/runtime paths.

Assigned identities:

| Surface | V5 identity |
|---|---|
| Package / desktop / helper binaries | `gitterm-v5` / `gitterm-v5-agent` |
| App / bundle | `GitTerm V5` / `com.cree8.gitterm.v5` |
| Desktop state root | `~/.config/gitterm-v5` |
| Helper state / service / token identity | `~/.config/gitterm-v5-agent` / `com.cree8.gitterm.v5.agent` / `gitterm-v5-agent` |
| Log server / browser MCP / task MCP | `localhost:23030-24029` / `localhost:24030-25029` / `localhost:25030-26029` |
| Remote helper endpoint | `127.0.0.1:8787` |
| Remote helper wire namespace | `gitterm.agent.v5.GitTermAgent` |
| Browser / temporary state | `<desktop-root>/browser-profile` / `gitterm-v5-*` |

Acceptance:

- V4 and V5 can run simultaneously without sharing or interrupting state.
- Building V5 cannot overwrite the V4 application binary or helper.
- Connecting a V5 remote host cannot attach to or stop V4 agent sessions.
- Fresh V5 starts with no V4 workspaces, browser profile, or tokens.

## Slice 1 - Confirm UX and Contracts

**Goal:** Resolve the few choices that materially affect persistence and UI
before code begins.

Tasks:

- [x] Review `task-sprite-architecture.md` with the user.
- [x] Choose the all-tasks surface placement: dedicated Tasks sidebar mode.
- [x] Choose the configurable default task-worktree root:
  `~/.config/gitterm-v5/worktrees`.
- [x] Choose the initial branch-name template:
  `task/<issue-or-task-id>-<slug>`.
- [x] Confirm the first supported child-tab launch paths:
  - terminal-backed configured harnesses;
  - native Claude/Pi agent tabs where appropriate;
  - named shell/test profiles deferred until local task execution works.
- [x] Default the stopping boundary to **Implement until tests pass**, with
  **Plan only** and **Prepare a draft PR** as initial alternatives.
- [x] Keep archive/cleanup manual and prompt before closing a live task child tab.
- [x] Confirm the visible hierarchy is Workspace -> Task -> child tabs. Harness
  and model selection belong to each child session, not permanently to the task.
- [x] Create the Linear issue breakdown and use focused implementation branches
  based on `v5` when a slice is ready to commit and publish.

Acceptance:

- The architecture has no unresolved decision that changes the persisted
  schema or first user flow.
- The implementation branch contains none of the unrelated current worktree
  changes.

## Slice 2 - Task Domain and Durable Store

**Goal:** Persist and reconcile tasks without creating worktrees or launching
processes.

Likely files:

```text
src/tasks.rs                 # domain types and store
src/config.rs                # config-root path helper only if needed
src/main.rs                  # App registry/load wiring
```

Tasks:

- [x] Add versioned `TaskRecord`, `TaskExecutionAttempt`, lifecycle, executor, issue,
  repository, worktree, harness, and attention types.
- [x] Store task metadata separately from `workspaces.json`.
- [x] Use atomic replacement for writes; report serialization, write, and corrupt
  file failures with the exact path and operation.
- [x] Serialize durable transitions only, not terminal output or high-frequency
  progress events.
- [x] Load tasks during app startup without opening their workspaces or spawning
  processes.
- [x] Reconcile persisted `preparing`/`running` local attempts to an explicit
  interrupted/resumable state after app restart.
- [x] Add create/update/archive APIs with one active attempt per task.

Tests:

- empty/missing store;
- round-trip every variant;
- schema-version rejection or migration behavior;
- corrupt file surfaces an observable error without overwriting it;
- atomic save leaves the prior valid file on failure;
- startup reconciliation never reports a dead local process as running;
- unrelated workspace persistence remains unchanged.

Exit gate:

- Tasks survive restart and can be enumerated independently of open tabs and
  workspaces.

## Slice 3 - Safe Local Worktree Provisioning

**Goal:** Prepare and clean up isolated task worktrees without launching an
agent.

Likely files:

```text
src/task_worktree.rs         # contained git/worktree boundary
src/tasks.rs
src/main.rs
```

Tasks:

- [x] Resolve repository top-level, common directory, origin URL when present,
  current branch, and exact base commit.
- [x] Generate a unique task branch and worktree path from validated task metadata.
- [x] Run `git worktree add` with structured arguments off the Iced update thread.
- [x] Reject collisions, non-repositories, missing base commits, and unsafe paths
  with actionable context.
- [x] Roll back only artifacts created by the failed preparation attempt.
- [x] Record the exact base commit and final canonical worktree path after success.
- [x] Add guarded cleanup inspection: running process, dirty state, ahead/unpushed
  commits, existing PR, and worktree registration.
- [x] Do not implement automatic destructive cleanup.

Tests using temporary Git repositories:

- create from branch and detached commit;
- unique parallel tasks from the same repository;
- existing branch/path collisions;
- partial-failure rollback;
- dirty/unpushed cleanup refusal;
- paths containing spaces;
- main checkout remains untouched.

Exit gate:

- A task can move `draft -> preparing -> ready` with a valid isolated branch and
  worktree, and failures leave no ambiguous repository state.

## Slice 4 - Minimal Tasks UI and Task Child Tabs

**Goal:** Create a task from the active workspace, enter its nested context,
open one or more task-scoped sessions, and return to them later.

Tasks:

- [x] Add the chosen all-tasks surface with cross-workspace grouping and basic
  filters.
- [x] Add **New Task** from the active workspace.
- [x] Task sheet fields: objective/title, optional issue, base ref, proposed
  branch/worktree, stopping boundary, and local executor. It is harness-neutral.
- [x] Create the task and worktree asynchronously with visible preparation steps.
- [x] Add `task_id` and durable task-session identity to persisted tab configuration.
- [x] Enter a task context whose top bar shows only that task's child tabs plus a
  task-scoped `+` launcher and an explicit back-to-workspace action.
- [x] Launch configured terminal-backed harnesses and plain terminals as sibling
  child tabs in the task worktree. Each launch chooses its own harness.
- [x] Focus/restore by task-session identity; do not enforce one tab per task.
- [x] If no child tab is open, show task details and an invitation to add the first
  agent or terminal.
- [x] Keep ordinary non-task tabs unchanged.

Tests:

- task-tab association persists through workspace save/restore;
- multiple child tabs for one task persist and remain independently addressable;
- focusing a known session never duplicates that session;
- closing one child view retains the task, worktree, and sibling views;
- task creation failures surface without opening a misleading tab;
- local and remote workspace identities cannot be confused.

Manual verification:

- Create one task, add Claude/Codex/Pi or terminal child tabs, switch between the
  task and ordinary workspace tabs, then return to each live child session.

Exit gate:

- The first end-to-end local workflow is usable without Docker or GitHub-hosted
  execution.

## Slice 4A - Generic Agent Task Control (TRU-106)

**Goal:** Let a coordinator session use the same durable task and worktree
workflow as the UI without making GitTerm depend on one harness.

Tasks:

- [x] Add a V5-isolated authenticated task-control MCP endpoint.
- [x] Route every operation through the Iced event loop; do not give the MCP
  server independent ownership of `tasks.json` or visible tabs.
- [x] Expose `task_list`, `task_get`, `task_create`, `task_create_batch`, and
  `task_launch_session`.
- [x] Keep batch creation sequential and return explicit per-item partial
  failures.
- [x] Create agent-requested tasks in the background without stealing focus
  from the coordinator session.
- [x] Inject ephemeral task MCP configuration into Codex presets and manual
  Codex launches without modifying persisted user/project configuration.
- [x] Verify a real coordinator creates a task and launches a configured child
  session through the MCP endpoint.

Boundaries:

- Session launch opens the selected harness or terminal but does not claim the
  stored objective was submitted. Automatic prompt delivery requires a tested
  adapter or a standard session transport such as ACP.
- Pi, Claude, and other harnesses inherit connection environment variables but
  need their own MCP/session adapter before GitTerm can advertise automatic
  attachment.
- Cross-harness handoff, messaging, waiting, lifecycle automation, and writer
  leases remain later slices built on this control plane.

Exit gate:

- A generic Codex coordinator can create independent background tasks and open
  their worker sessions, while the same GitTerm-owned command bridge remains
  transport-neutral for later harness adapters.

## Slice 4B - Task Conversation Continuity and Handoffs

**Goal:** Let task work survive closed tabs and move between harnesses without
copying native transcripts into GitTerm.

Tasks:

- [x] Persist independent child-session history on each task, separate from the
  single-writer execution-attempt lifecycle.
- [x] Link task sessions to native Claude, Codex, and Pi conversation IDs found
  by the existing Chats index in the task worktree.
- [x] Pre-assign Claude conversation IDs at launch and persist a resume command
  so restart reconnects instead of recreating the session.
- [x] Show resumable conversation history in task details and focus an existing
  tab rather than duplicating a live conversation.
- [x] Replace the generic add-session action with **Continue with…**; copy a
  structured task briefing before opening the harness picker.
- [x] Add `task_update_handoff` so coordinators and compatible workers can
  persist a compact summary, decisions, next steps, and blockers.
- [x] Keep session history and handoffs additive so existing `tasks.json`
  records load with empty defaults.

Boundaries:

- Same-harness continuation resumes the exact native conversation. A different
  harness starts a new conversation in the same worktree and receives the
  copied task briefing; arbitrary terminal presets still do not have a safe
  automatic prompt-submission contract.
- Native transcripts remain owned by Claude, Codex, and Pi. GitTerm stores only
  stable references and compact handoffs.
- The local Chats index supplies conversation discovery. Remote resume remains
  part of the remote-executor work.

Exit gate:

- A task with no open child tabs can resume a prior local harness conversation,
  or continue in a different configured harness with durable handoff context.

## Slice 5 - Lifecycle, Progress, and Attention

**Goal:** Make autonomous work understandable and controllable, correcting the
opacity observed in Kandev.

Tasks:

- Map terminal-title and native agent events into task lifecycle and attention
  updates without removing the existing tab attention adapters.
- Track current phase, last meaningful update, current raw action, elapsed time,
  last activity, changed-file count, and verification summary.
- Show **Go to Session**, **Stop**, **Resume**, **Review Changes**, and **Archive**
  according to state.
- Feed task attention into the existing cross-workspace Attention view.
- Keep `completed unread` until the task/session has actually been visited.
- Surface unknown/stale state rather than inferring progress from elapsed time.
- Persist lifecycle transitions and compact summaries, not every event.
- Add a modest configurable local concurrency limit and a queued state.

Tests:

- lifecycle transition table, including invalid transitions;
- attention priority and clearing behavior;
- terminal and native-agent event adapters produce the same task semantics;
- queue starts the next task only after capacity is released;
- restart reconciliation produces interrupted/resumable attention;
- progress updates do not cause hot-loop persistence or redraw work.

Exit gate:

- A user can tell what every task is doing, what changed, and what needs action
  without interpreting raw shell commands.

## Slice 6 - Review, Publish, and Cleanup Boundaries

**Goal:** Carry a completed task safely into the existing review workflow.

Tasks:

- Show task-scoped git status and diff using the task worktree.
- Record commits and discovered/published PR URLs without making GitHub the
  execution host.
- Provide explicit actions to push and open a draft PR; do not auto-publish.
- Link the task to the required Linear issue and warn before a commit/PR flow
  if the issue policy cannot be satisfied.
- Route readiness through `/pr-ready`; never label a task review-ready from
  local intent alone.
- Add guarded archive and worktree-cleanup actions.
- Preserve harness transcript references after worktree cleanup so the work is
  still auditable.

Tests:

- task diff is always scoped to its worktree;
- cleanup refuses running, dirty, or unpushed work;
- archive does not imply cleanup;
- stale PR/readiness state is visibly stale after a new commit;
- no action pushes, opens a PR, or removes files without explicit invocation.

Exit gate:

- A local task can travel from objective to reviewed draft PR with explicit
  human gates and recoverable cleanup.

## Slice 7 - Mac Mini Executor

**Goal:** Dispatch an on-demand task from the laptop to the mini and monitor it
through `gitterm-v4-agent`.

Prerequisites:

- Local task lifecycle has been used enough to stabilize the task contract.
- The deployed `agentd` includes the natural-exit file-descriptor cleanup fix.
- Remote host pairing and session attach are healthy.

Protocol work:

- Add protocol capabilities for repository cache/fetch, task worktree prepare,
  inspect, and guarded cleanup.
- Associate an `agentd` session with a desktop task ID and execution-attempt ID.
- Return typed preparation phases and errors.
- Make additive protobuf changes and capability-gate older agents.
- Do not expose shell command strings as the worktree protocol.

Execution flow:

- Resolve remote URL and exact pushed commit on the laptop.
- Refuse dispatch when the commit is not fetchable from the configured origin.
- On the mini, clone once into a cache or reuse a verified cache, fetch origin,
  and verify repository identity.
- Create a fresh per-task worktree and branch when mutation is allowed.
- Start the configured remote session through the existing agentd PTY runtime.
- Stream typed task/session state plus attachable terminal output.
- Continue running through desktop disconnect; reconnect by task/session ID.
- On `agentd` restart, report interrupted/resumable unless the session can be
  proven live.

Tests:

- first clone versus cached fetch;
- wrong origin and missing commit rejection;
- disconnect/reconnect while the process continues;
- duplicate dispatch idempotency;
- agent capability/version mismatch;
- remote cleanup guards;
- laptop and mini paths remain source-native and never cross-resolve;
- soak-sized output stays bounded and reattach remains responsive.

Manual verification:

- From GitTerm on the laptop, dispatch a bounded read-only test to the mini,
  disconnect the desktop, reconnect, inspect progress, and receive the result.
- Dispatch a mutating agent task, review its remote worktree changes, then push
  a draft branch only after explicit approval.

Exit gate:

- The mini can perform unattended work without timers or an open laptop, and
  GitTerm presents the same task lifecycle used locally.

## Slice 8 - Named Run Profiles and Scheduling

**Goal:** Generalize the proven task executor for repeatable operations such as
soak and headed tests.

Tasks:

- Add versioned run-profile configuration: setup, command, timeout,
  environment/secret references, success criteria, and retained artifacts.
- Allow manual dispatch from a repository or task.
- Integrate existing scheduled soak behavior with the same profile rather than
  maintaining a second execution path.
- Add timers only after manual runs are observable and reliable.
- Keep agent-created runs within explicit profile permissions and concurrency
  limits.

Exit gate:

- Manual and scheduled runs share one preparation, monitoring, and result
  contract.

## Verification for Every Slice

At minimum:

```text
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test
cargo build
```

Add focused tests for the slice before broad verification. Any protocol change
must also build and test `gitterm-v4-agent`. Task work should not require a
sibling `iced_term_fork` change; if an unrelated terminal change becomes
necessary, follow the existing fork-first Windows CI rule explicitly.

Visual slices require screenshots or recordings in their PRs. Existing
performance constraints remain binding: no repository scanning, transcript
parsing, status persistence, or terminal synchronization in Iced view/update
hot paths.

## Recommended First Implementation Batch

Start with Slices 0-3 only:

1. Create and verify the isolated V5 lane.
2. Resolve the initial UX/storage choices.
3. Add the durable task store.
4. Add safe local worktree provisioning.

Do not begin process-detachment, mini dispatch, Docker, GitHub execution, or run
profiles in that batch. Once the foundation and worktree tests are solid, build
the smallest usable task-to-tab flow in Slice 4 and use it before widening the
system.
