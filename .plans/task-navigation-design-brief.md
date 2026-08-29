# GitTerm V5 Workspace, Task, and Session Navigation

**Status:** Experience design brief

**Audience:** Product and interface design

**Purpose:** Explore alternative ways to organize GitTerm's workspaces, durable
tasks, and AI/terminal sessions. This brief defines the product model and the
jobs the navigation must support. It does not prescribe the current prototype
as the answer.

## The Product in One Paragraph

GitTerm is a macOS-first desktop workspace for repository work. It combines a
terminal multiplexer, Git and file views, and sessions running Claude, Codex,
Pi, or another configured AI harness. V5 adds durable **tasks**: bounded pieces
of work with their own branch and Git worktree. A user can begin in an ordinary
repository session, discuss a set of Linear issues with an AI coordinator,
create one task per approved issue, launch different AI harnesses to work on
them, monitor their state, and later return to any task or session without
confusing the task's lifetime with an open tab.

## Design Job

Help a developer coordinate several pieces of repository work without losing
the answers to four questions:

1. **Where am I?** Which machine, repository workspace, task, and session am I
   currently using?
2. **What exists?** Which tasks and sessions are available, including things
   that are not currently open?
3. **What is happening?** Which work is ready, running, waiting, failed, or
   complete?
4. **What will this action affect?** Am I switching a view, opening a session,
   stopping a process, archiving a task, or changing repositories?

The interface should feel like a focused development environment, not a
project-management application embedded inside one. Terminal and code space
remain valuable, so hierarchy must be legible without consuming excessive
screen area.

## Canonical Product Model

The domain hierarchy is:

```text
GitTerm
└── Workspace group (for example, This Mac or a remote source)
    └── Workspace (one logical repository/project context)
        ├── General sessions (ad hoc Claude, Codex, Pi, or terminal work)
        └── Tasks (durable objectives in this repository)
            └── Task
                ├── Overview and lifecycle
                ├── Session(s): Claude, Codex, Pi, terminal, or future harness
                ├── Execution target, branch, and managed worktree
                ├── Durable handoff and session history
                └── Future plan, review, and pull-request views
```

This hierarchy describes ownership and lifetime. It does not require each
level to be rendered as a permanent row of tabs. Workspace grouping describes
the existing navigator; a task session may later execute on a different machine
without turning that execution target into another workspace.

### Relationship rules

| Relationship | Rule |
|---|---|
| Workspace group to workspace | A local or remote source can expose many repository workspaces. |
| Workspace to task | A task belongs to exactly one logical repository workspace. A workspace can have many tasks. |
| Workspace to general session | A workspace can have many sessions that are not attached to a task. |
| Task to session | A task can have zero, one, or many sessions. A task session belongs to one task. |
| Session to harness | Each session chooses its own harness and model. The task is not permanently bound to Claude, Codex, Pi, or any one model. |
| Task to executor | Execution location is selected independently. Moving execution to a remote machine does not change task ownership or navigation. |
| Coordinator to task | One general session may initiate and monitor many tasks. This is provenance and coordination, not ownership. The tasks survive if the coordinator session closes. |
| Task to task | Tasks are siblings in the initial product, not a nested task tree. A task created while viewing another task may use that branch as a base, but it still belongs to the repository workspace. |

### Infrastructure is not navigation

Each task normally owns a Git branch and a managed worktree. The worktree is
where its sessions execute; it is not another user-created workspace. A managed
task worktree must therefore not appear as an extra repository in the bottom
workspace bar. The user may inspect its path as task metadata, but navigation
should continue to communicate:

```text
Repository workspace → task → session
```

rather than:

```text
Repository workspace → task worktree pretending to be another workspace
```

## Vocabulary

| Term | Meaning | Expected lifetime |
|---|---|---|
| **Workspace** | A repository/project context on a machine. | Long-lived and explicitly opened or closed by the user. |
| **Task** | A durable objective with issue, branch, worktree, lifecycle, and delivery state. | Until archived; independent of open views. |
| **Session** | One resumable conversation or terminal working in a workspace or task. | May be opened, closed, and resumed. |
| **Harness** | The runtime used by a session, such as Claude, Codex, or Pi. | Chosen per session. It may be shown as “Agent” in user-facing copy if clearer. |
| **Coordinator** | A role played by an ordinary session that creates or monitors tasks. | Not a special object and not tied to Pi, Codex, or Claude. |
| **Worktree** | Git isolation used to execute a task safely. | Owned by the task and usually secondary in the UI. |
| **Tab** | One possible UI representation of a session or view. | Ephemeral. It should not be used as the domain name for durable work. |

“Sprite” appears in early architecture notes as a friendly worker metaphor. It
does not add a separate object to this hierarchy and need not appear in the
navigation exploration unless it clarifies the experience.

## Primary Orchestration Scenario

This is the main scenario the design should make easy:

1. The user opens a repository workspace.
2. The user opens a general Codex, Claude, or Pi session. This session is acting
   as the coordinator, but remains an ordinary workspace session.
3. The user asks it to look up open Linear issues for the repository.
4. The user and coordinator discuss scope and choose which issues to address.
5. The coordinator creates one durable task for each approved issue. Task
   creation prepares a branch and worktree but does not move focus away from
   the conversation. The creation form currently prefers `develop` as the base
   branch, then `main`, while allowing an explicit alternative.
6. The new tasks become visibly available and initially may have **no worker
   session**. “Task ready” and “agent working” must not look equivalent.
7. The coordinator or user launches a worker session for each task, choosing
   Claude, Codex, Pi, or another harness independently.
8. The user continues the coordinating conversation while watching for
   progress, completion, failure, or a request for input across the tasks.
9. The user enters a task to inspect its overview, branch changes, and one or
   more sessions. They can close the view and return later without destroying
   the task.
10. The user can resume the same harness conversation or continue the same task
    with a different harness using a compact handoff.
11. Completed work proceeds to review, draft PR, and eventually explicit
    archive/cleanup.

The coordinator session does not “turn into” the tasks it creates. It remains
at the workspace level and may coordinate many task siblings.

## Important Secondary Scenarios

- **Ad hoc work:** Open a terminal or AI session that never becomes a task.
- **Direct task creation:** Create one task manually without first opening a
  coordinator session.
- **Empty task:** Reopen a prepared task that has no open session, understand
  its state, and choose a harness.
- **Several sessions in one task:** Use Codex for implementation, Claude for a
  review, or Pi for follow-up while preserving the same worktree and objective.
- **Resume:** Close a task session, find the durable task later, and resume the
  exact conversation when the harness supports it.
- **Cross-harness continuation:** Open a new harness in the same task with a
  handoff containing the objective, decisions, next steps, blockers, and
  relevant session references.
- **Attention:** Find tasks that require input or review without scanning every
  terminal.
- **Scale:** Remain understandable with multiple workspaces, roughly 10–20
  active tasks in one repository, and several sessions in a busy task.
- **Task created from task context:** Preserve any intentional base-branch
  relationship, but show the new task as a sibling under the repository rather
  than creating another navigational workspace or task nesting level.

## Durable Objects Versus Views

The design must communicate the difference between closing a view and changing
the underlying work:

| User action | Durable effect |
|---|---|
| Switch context or tab | None. Only focus changes. |
| Close a session view | The task remains; a resumable session record may remain. A live process may require a separate decision. |
| Stop a session | The process stops; the task, branch, worktree, and history remain. |
| Resume or continue | Reopen the prior conversation or create another session in the same task. |
| Archive a task | Remove it from active task views while retaining its record. |
| Delete a worktree | Separate guarded cleanup; never implied by closing or archiving. |

## System Capabilities and Honest States

The interaction design should not imply more automation than the system has:

- Task creation and session launch are separate operations.
- A newly prepared task may correctly show **Ready · 0 sessions**.
- Launching a harness process does not necessarily mean the task objective was
  delivered to it. The UI eventually needs to distinguish **session opened**,
  **brief delivered**, and **working** rather than collapsing them into one
  “started” state.
- Codex is the first automatically configured client of GitTerm's task-control
  tools. Claude, Pi, and other harnesses can still be selected as sessions, but
  equivalent automatic attachment and prompt delivery require explicit
  adapters.
- Compact handoffs are durable task data. Full transcripts remain owned by the
  native harness and are not copied into the task record.
- Multiple session views do not imply that multiple autonomous writers may
  safely mutate one task worktree simultaneously.

These are current implementation boundaries, not a request for the designer to
expose technical protocol names such as MCP or ACP in the primary interface.

## Existing Interface and Navigation Experiments

The current application provides these surfaces:

- A bottom bar grouped by machine, containing repository workspace chips.
- A large top area historically used for ordinary workspace session tabs.
- A left sidebar with perspectives such as Git, Files, Tasks, Agent, Chats, and
  Plans.
- A Tasks perspective that acts as a durable index and can group tasks across
  workspaces.
- A task overview and task-scoped session launcher.
- A bottom console and a large terminal/content canvas.

Several top-navigation approaches have already been tried:

1. A flat session tab strip, which did not express task ownership.
2. A conditional task mode that replaced ordinary tabs with a task header and
   child tabs, which made the hierarchy shift under the user.
3. A persistent two-tier experiment with workspace/tasks on one row and
   sessions on another. This made the levels more explicit but still consumes
   space and has unresolved relationships with the Tasks perspective.

The designer should treat all three as evidence, not constraints.

## Observed UX Problems

- A task, session, branch, and repository can all appear as similar tab-shaped
  labels, making their different lifetimes unclear.
- The top navigation has changed structure when entering a task, so the user
  cannot predict where ordinary sessions went.
- Showing two navigation levels only in some states makes the spatial model
  unstable; showing two at all times may be too heavy.
- The Tasks perspective and task chips can feel like duplicate ways to select
  the same thing without a clear distinction between “browse the index” and
  “enter this task.”
- Opaque task IDs or branch-derived names can dominate labels instead of the
  human task title or issue key.
- A task worktree has accidentally appeared in the bottom workspace bar,
  presenting implementation infrastructure as a peer repository.
- Creating a task with no worker session can look like an incomplete launch
  rather than a valid prepared state.
- A selected task in one region and an empty “select a task” state in another
  exposes ambiguity over which component owns selection.
- Session counts and statuses need concise, grammatical, visually distinct
  treatment rather than being embedded in long labels.
- It is not yet clear how a coordinator should see the tasks it just created
  while keeping its conversation in focus.

## Product Rules the Alternatives Must Preserve

1. The bottom workspace bar represents real repository/project workspaces by
   machine. Managed task worktrees never become peers there.
2. A task is durable and can have zero or many sessions. Closing a tab never
   silently deletes or archives it.
3. A session is either workspace-general or attached to exactly one task.
4. Harness and model are selected per session, not per task.
5. A coordinator is a role for any capable session, not a Pi-only or
   Codex-only mode.
6. Tasks created by one coordinator are not hidden inside that session. They
   remain discoverable after it closes and can be opened by another session.
7. Tasks are initially siblings under a repository. Base-branch or provenance
   relationships must not silently create navigation nesting.
8. Task state and session state must be distinct and truthful.
9. Human titles and issue keys are primary labels. Generated UUIDs, worktree
   paths, and branch names are supporting metadata.
10. Switching a task must not silently create or switch to another bottom-bar
    workspace.
11. Navigation must preserve useful terminal space and support keyboard-driven
    use.
12. Attention is a filtered answer to “what needs me?”; it is not a second task
    hierarchy.

## Experience Territories to Explore

These are starting points for alternatives, not requested solutions. A strong
proposal may combine or replace them.

### A. Persistent two-level navigation

Keep repository/task contexts at one level and show the selected context's
overview and sessions at a second level.

Questions to test:

- Can the two levels have sufficiently different visual grammar that they do
  not read as duplicate tab bars?
- How does the first row scale to 20 tasks without becoming an unreadable
  horizontal strip?
- Is the Tasks perspective still useful as an index, and is its role obvious?
- Can a coordinator keep focus while newly created tasks appear unobtrusively?

### B. Task index or command center as the primary organizer

Keep the top strip focused on open sessions. Make the Tasks perspective a
strong index/control center; selecting a task opens a task detail surface with
local session navigation.

Questions to test:

- Does this make durable tasks clearer or force too much travel through the
  sidebar?
- How does a user jump directly among three active tasks and their sessions?
- Can task detail coexist with the terminal rather than replacing it?
- How are open sessions visibly associated with their task in the top strip?

### C. Hierarchical or grouped session strip

Use one compact top region where sessions are visibly grouped beneath workspace
or task labels, potentially with collapsible groups, a task switcher, or a
popover for closed/inactive work.

Questions to test:

- Can grouping remain legible without nesting controls inside controls?
- What stays visible when task groups are collapsed?
- Does this preserve the speed and familiarity of ordinary tabs?
- How are durable tasks with zero open sessions represented?

### D. Coordinator workspace plus task activity rail

Treat the coordinator conversation as the working canvas and add a compact
task/activity rail or drawer that shows tasks it created alongside all other
repository tasks. Entering a task changes the canvas, while a clear return path
preserves the coordinator.

Questions to test:

- Does this overemphasize the coordinator even when tasks were created
  manually?
- Can the rail distinguish task state from worker-session state?
- Does the same model work when no coordinator session exists?
- How does it scale across several repository workspaces?

## States to Show in Each Proposal

Please demonstrate the hierarchy in more than the ideal “one task, one session”
state:

1. Repository workspace with three general sessions and no tasks.
2. Coordinator conversation after creating three issue-backed tasks without
   leaving the conversation.
3. One task prepared with zero sessions.
4. One task with two sessions using different harnesses.
5. Several tasks in ready, running, waiting-for-input, failed, and complete
   states.
6. A task session closed and later resumed.
7. A different harness continuing from a durable handoff.
8. Ten to twenty active tasks, including overflow behavior.
9. Two machines or several repository workspaces in the bottom bar.
10. A task created while another task is selected, with both shown as siblings
    under the same repository.

## Design Questions

- What is the stable visual anchor for “current repository,” “current task,”
  and “current session”?
- Which levels deserve persistent navigation, and which belong in a switcher,
  index, drawer, or breadcrumb?
- Is a task overview a peer of its sessions, a parent page, or a detail pane?
- What should the top-level `+` create, and how does its meaning change—or stay
  consistent—inside a task?
- Should there be an explicit **Create task from this session** action, and if
  so, does it merely capture provenance or attach/transfer the session?
- How should newly coordinator-created tasks appear without stealing focus?
- How does the user tell that a task is ready but has not launched or briefed a
  worker?
- How should status, attention, unread changes, and worker activity differ
  visually?
- What terminology should be visible to users: session, agent, harness, worker,
  run, or conversation?
- How should keyboard shortcuts switch workspaces, tasks, and sessions without
  creating a three-dimensional shortcut scheme?
- What is the fastest path from “this task needs me” to the exact session or
  review surface requiring attention?

## Requested Design Output

The most useful handoff back to engineering would include:

- Two or three meaningfully different low-fidelity experience alternatives,
  not only cosmetic variations of one tab layout.
- A walkthrough of the primary orchestration scenario for each alternative.
- Annotated hierarchy and interaction rules, including what persists when a
  view closes.
- The state examples listed above, especially zero-session tasks, multi-session
  tasks, task overflow, and attention.
- Proposed user-facing vocabulary and representative labels/copy.
- Desktop layouts at a normal working size and a constrained width.
- A recommendation with tradeoffs after comparing the alternatives.

## Evaluation Criteria

An alternative is promising if a user can quickly and correctly answer:

- Which repository am I in?
- Am I in general workspace work or a durable task?
- Which session and harness am I looking at?
- What other tasks exist, including those with no open session?
- Which tasks need attention, and why?
- Will closing this control close a view, stop work, or archive something?
- How do I return to the coordinating conversation?
- How do I resume this task with the same or a different harness?

The final experience should remain calm and predictable as the amount of work
grows. The signature behavior should be effortless movement between a
coordinating conversation and durable parallel tasks, with no ambiguity about
what is merely open, what is still running, and what will persist.
