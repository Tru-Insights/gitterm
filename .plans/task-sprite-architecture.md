# GitTerm V5 Task and Sprite Architecture

**Status:** Draft for discussion

**Captured:** 2026-08-19

**Related:** `chats-panel.md`, `remote-gitterm-agent-architecture.md`

## Intent

GitTerm should make it easy to delegate bounded repository work without losing
the visibility and direct control of its existing workspace-and-tab model.

The initial goal is deliberately local:

1. Create a durable task from the active repository or an issue.
2. Give the task its own branch and Git worktree.
3. Start Claude, Codex, Pi, Cursor, or another configured harness in that
   worktree.
4. Keep the task visible across workspaces, with useful progress and attention
   state.
5. Review the actual session and changes in GitTerm before publishing or
   running `/pr-ready`.

The next executor will be the Mac mini through an isolated
`gitterm-v5-agent`. From a laptop repository, the user should be able to
dispatch an exact pushed commit
to the mini, have it prepare a clean worktree, run an agent or named test
profile, and monitor or reattach to that run while it continues independently
of the laptop connection.

## V5 Development Lane

This is large enough to be GitTerm V5. It introduces a durable task model,
managed worktrees, execution attempts, cross-workspace task navigation, and an
executor contract that later extends `agentd`. Those changes should not be
developed in the V4 daily-driver runtime.

V5 is an evolutionary branch from V4, not a rewrite:

```text
v4                         stable daily driver and bug-fix lane
  \
   v5                      task/Sprite development lane
    -> ../gitterm-v5       isolated development worktree
```

V5 must be runtime-isolated before task implementation starts:

- application name, binary/package name, and bundle identifier;
- config, workspaces, tasks, tokens, profiles, and browser profile roots;
- log-server, browser-MCP, and other local port ranges;
- helper state, temporary files, service labels, and launch artifacts;
- `gitterm-v5-agent` config, token, service, and default endpoint on the mini;
- build output through the V5 worktree's own `target/`.

V4 should remain usable throughout V5 development. Intentional V4 fixes can be
merged forward into V5 regularly. V5 must never read, migrate, overwrite, or
stop V4 runtime state as a side effect of development.

## Product Decisions

### Tasks are durable work; tabs are views

A task is the durable unit of delegated work. It owns the objective, repository
identity, worktree, branch, execution state, and delivery links. A tab is one
way to observe or steer the task.

Closing or switching a tab must not mean deleting the task. Initially, a local
process may stop when GitTerm exits, but the task, worktree, branch, and harness
session remain resumable. A remote `agentd` session continues while the desktop
is disconnected.

### Sprite is the worker metaphor, not another MVP record

"Sprite" is GitTerm product language for an autonomous worker assigned to a
task. It was chosen as a more appropriate metaphor than "minion."

For the first implementation, Sprite is not a separate top-level navigation
object or persistence table. The assigned harness/session is the task's worker.
If multi-worker tasks become real, a durable worker identity can be introduced
from evidence rather than assumed now.

Fly.io's `sprites.dev` is unrelated infrastructure. GitTerm does not need Fly
for this design. If supported someday, it would be one executor runtime and
would be labeled clearly (for example, "Fly Cloud") to avoid "Sprite running
in a Sprite" ambiguity.

### Executor location is independent of the task

The task model must not hardcode local paths or GitHub-hosted execution.

```text
Task
  -> executor: This Mac | RemoteAgent(remote_id)
      -> worktree
          -> harness session or named run profile
```

The first executor is `This Mac`. The second is the configured Mac mini. Docker,
GitHub-hosted runners, and cloud sandboxes are optional future executors, not
initial dependencies.

### GitHub is the collaboration and verification plane

GitHub provides issue/branch/commit/PR/check state. It is not the default host
for long-running agent work.

Routine work runs in local or mini worktrees. GitHub Actions remains the
independent CI gate and can later provide burst execution when useful. This
avoids turning inexpensive occasional hosted runs into an always-on compute
cost.

### Worktrees are the default isolation boundary

One active task gets one dedicated branch and one dedicated worktree. This
isolates source changes without the overhead and behavioral differences of a
container.

Worktrees do not isolate ports, databases, temporary directories, global tool
state, credentials, or machine resources. Task execution therefore also needs:

- unique task IDs exposed to child processes;
- optional per-task port and temporary-directory allocation;
- bounded concurrency;
- explicit permission and credential profiles;
- clear warnings for shared external state;
- Docker only when stronger isolation is actually required.

## Mental Model

| Concept | Meaning | Lifetime |
|---|---|---|
| Workspace | A project context on one machine | Long-lived |
| Task | A bounded objective and its delivery lifecycle | Until archived |
| Sprite | Human-facing name for the assigned autonomous worker | One execution at a time initially |
| Execution attempt | One launch or resume of a task's worker | Until completion, failure, or stop |
| Tab | A view into a terminal, agent session, task, document, or browser | Open/close freely |
| Executor | The machine/runtime that performs an attempt | Selected per attempt |

Execution attempts are an internal durability concept. The normal interface
should say "Run," "Resume," or "Retry," not force the user to manage attempt
records.

## Primary User Experience

### Create and run a local task

From an active workspace, the user chooses **New Task** or asks an agent to
create one. The launch sheet shows:

- title/objective;
- optional Linear or GitHub issue;
- repository and exact base ref;
- proposed branch and worktree;
- harness/profile and model;
- executor (`This Mac` initially);
- stopping boundary, such as plan only, implement until tests pass, or prepare
  a draft PR.

Starting the task creates the worktree, opens or focuses its linked tab, and
starts the selected harness there. The new task appears in the cross-workspace
task view immediately.

### Observe progress without reading raw commands

The Kandev experiment demonstrated that "agent running" plus a stream of shell
commands is not enough. Every running task should expose:

- current phase or plan step;
- latest meaningful agent update;
- current command/tool action on demand;
- elapsed time and last activity time;
- changed-file summary;
- verification state;
- branch, worktree, harness/model, machine, and stopping boundary;
- an unambiguous **Go to Session** action.

Raw terminal and tool output remain available as detail, not as the only
progress explanation.

### Attention is a projection, not a second task system

The existing cross-workspace Attention view should consume task state alongside
tab attention. It answers "what needs me now?" and remains a filtered view.

The Tasks view answers "what exists and what is it doing?" It includes running,
quiet, completed, stopped, and archived work. Selecting a task focuses its live
tab or opens its task detail if no tab is attached.

Initial attention reasons:

- human input or permission required;
- execution failed;
- completed but unread;
- changes ready for review;
- remote host unavailable while action is required.

### Stop, resume, and cleanup are distinct

- **Detach/close view:** leave the durable task intact.
- **Stop:** stop the active process but retain task, worktree, branch, and
  harness session.
- **Resume:** launch or reconnect using the retained task context.
- **Archive:** remove the task from active views while retaining its record.
- **Delete worktree:** a separate guarded cleanup action.

Worktree deletion must refuse or require explicit confirmation when changes are
uncommitted, commits are unpushed, or a process is still running. Deleting a
task must never silently delete repository work.

## Task Record

The exact Rust types may evolve, but the durable contract should contain:

```text
TaskRecord
  schema_version
  task_id
  title
  objective
  created_at / updated_at
  workspace identity
  repository identity (local common-dir identity and remote URL when known)
  optional issue reference
  exact base ref and base commit
  task branch
  worktree location (source-native path)
  executor target
  harness/profile selection
  stopping boundary
  lifecycle state
  attention state
  active/latest execution attempt
  harness conversation/session reference
  changed-file and verification summary
  optional commit and PR references
  archive state
```

An execution attempt records the executor, start/end time, session reference,
result, and failure context. Only one attempt may actively mutate a task
worktree at a time.

The initial lifecycle vocabulary should remain explicit and small:

```text
draft -> preparing -> ready -> queued -> running
                                      -> waiting_for_input
                                      -> completed
                                      -> failed
                                      -> stopped
                                      -> interrupted
any inactive state -> archived
```

`completed` means the worker stopped successfully; it does not mean the changes
are correct, reviewed, pushed, or ready to merge. Verification, publication,
and `/pr-ready` state are separate evidence carried by the task.

Task metadata should live separately from `workspaces.json`; restoring a
workspace must not be required to enumerate tasks. The first store can be a
small versioned JSON document under the V5 config root, written atomically and
only on durable transitions. Harness transcripts and terminal output remain in
their existing stores rather than being duplicated into the task registry.

## Local Execution

Local execution reuses existing GitTerm capabilities:

- Git worktree discovery already exists.
- Terminal tabs already launch Claude, Codex, Pi, Cursor, and custom commands.
- Native agent tabs currently support Claude and Pi with structured events.
- Chat indexing already locates and resumes harness conversations.
- Attention already accepts terminal-title and native agent events.

A task must therefore link to either a terminal-backed harness tab or a native
agent tab. It must not require every harness to adopt the native agent-tab
protocol before tasks can ship.

Local process survival is staged:

1. First release: work continues while its GitTerm tab/app session is alive;
   task/worktree/conversation survive restart and can be resumed.
2. Later, if needed: move local task processes behind an app-independent
   supervisor or local `agentd` so they continue while GitTerm is closed.

## Mac Mini Dispatch

The Mac mini is the second executor and evolves the existing
`gitterm-v4-agent`/`WorkspaceSource::RemoteAgent` direction into an isolated
`gitterm-v5-agent` runtime.

Example request:

> Run the GitTerm soak test on the Mac mini against the current pushed `v4`
> commit for four hours and notify me if it slows down.

Expected flow:

1. Desktop resolves the active repository's remote URL and exact commit.
2. Dispatch refuses an unreachable/unpushed commit instead of silently using
   an older ref.
3. The mini maintains a repository cache, fetches origin, and verifies the
   requested commit.
4. The mini creates a fresh task worktree and, for mutating work, a unique task
   branch.
5. `agentd` starts the configured harness or named run profile in that
   worktree.
6. GitTerm streams status/output and can detach/reconnect by task/session ID.
7. The mini retains the worktree and session result until an explicit cleanup
   policy applies.

Named run profiles make repeatable operations such as soak tests and headed
end-to-end tests explicit. A profile defines setup, command, timeout,
environment/secret references, success criteria, and retained artifacts. It
can later be invoked manually, by a timer, or by an agent without duplicating
execution machinery.

The existing `agentd` PTY registry already keeps sessions alive through desktop
disconnects and replays bounded output on attach. It does not currently survive
an `agentd` restart; task metadata and harness session references must make that
limitation visible and support a clean resume rather than claiming continuous
process survival.

## Repository and Security Rules

- Every task records the exact base commit used.
- Remote dispatch requires that commit to be fetchable from the configured
  origin in the first implementation.
- Git commands use structured arguments, never shell interpolation.
- No task automatically pushes, opens a PR, merges, deploys, or deletes a
  worktree without the corresponding user-approved boundary.
- Credentials are referenced through existing config/secret mechanisms and
  are not copied into task records.
- Executor profiles are allowlisted and display their effective machine,
  command, environment, and permissions before launch.
- Concurrent mutation of the same worktree is rejected.
- Startup reconciliation reports stale or interrupted state; it does not guess
  that a task is still running.

## Non-Goals for the First Release

- Docker or Fly.io execution.
- GitHub Actions as the primary agent runtime.
- Multi-agent task graphs or autonomous swarms.
- Automatic issue selection or unsupervised backlog consumption.
- Automatic push, PR creation, merge, or deployment.
- Replacing the Chats panel or normal ad hoc terminal/agent tabs.
- Keeping local processes alive after the entire GitTerm application exits.
- Building a second Kanban/workspace hierarchy like Kandev.
- Destabilizing or silently migrating the V4 daily driver while V5 is under
  development.

## Initial Product Decisions

1. Tasks get a dedicated sidebar mode. Attention remains the filtered
   "what needs me now?" projection instead of becoming the task registry.
2. Managed worktrees default to `~/.config/gitterm-v5/worktrees`. The root is
   globally configurable in the first release and stored as a source-native
   path.
3. Branches use `task/<issue-or-task-id>-<slug>`. A linked issue key wins over
   the generated task ID; uniqueness checks may add a short suffix rather than
   silently reusing an existing branch.
4. The launch sheet requires an objective and offers stopping-boundary presets.
   The default is **Implement until tests pass**; the initial alternatives are
   **Plan only** and **Prepare a draft PR**.
5. Completed tasks archive and clean up manually. Merge detection may suggest
   cleanup later, but never performs it automatically.
6. Closing a live task tab prompts the user to keep the view open or stop the
   run. V5 will not hide a live process behind a closed tab while process
   ownership still lives in `TabState`.
7. The first launch paths are configured terminal-backed harnesses plus the
   existing native Claude and Pi tabs where their session contracts fit.
   Named shell/test profiles remain deferred until local task execution works.
