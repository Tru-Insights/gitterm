# TRU-274 PR15b mutation pin (Iced before integration)

Sources: `src/main.rs` named handlers below, `src/task_mcp.rs` tool DTOs,
`core/tasks/store.rs`, and `core/task_worktree.rs`. These are synthetic cases.

- `pending_task_from_control`: trim title/objective; reject empty with `title
  is required` / `objective is required`. Repository path must be absolute.
  Trim/uppercase issue and discard blank; predicate is 2–10 uppercase ASCII
  letters/digits, first a letter, dash, 1–7 digits (zero allowed). Error example
  uses TRU-106. Blank/omitted base selects DEFAULT_TASK_BASE. Managed worktree
  paths map to their workspace directory before choosing the label.
- `TaskCreateSubmit`: title/objective/base required (capitalized messages),
  issue is trimmed but NOT uppercased; error example uses TRU-105. Generated
  task branches are checked by `task_worktree::validate_branch`: task/ prefix,
  no further slash, then Git check-ref-format --branch. Base resolution is Git
  rev-parse plus existing default order (develop, main, current), not a new
  syntax validator. Persisted record validation is deliberately weaker:
  nonempty title/objective/base/branch; it accepts adopted branches and records
  with no issue. Do not impose create branch syntax on stored/adopted tasks.
- `TaskMetadataResolved` saves Preparing before preparing the worktree;
  `TaskWorktreePrepared` saves Ready before reply/worker launch. Save failure
  leaves the old state and must not acknowledge success or start a worker.
- `update_handoff_requested` trims summary (required), trims list entries and
  drops blanks. Explicit writer session must belong to this task; otherwise
  use caller's hosted task session if known to the store, else caller UID,
  else no writer. Explicit writer is attribution, never completion authority.
  Done/blocked with an open worker requires the caller-hosted worker session;
  no open worker accepts a plain task handoff. `record_handoff` checks that
  authority before writing. Later delegation-save failure reports partial save.
- `TaskArchive` closes/archives the durable task without deleting history or
  worktree. Store lifecycle transitions refuse Preparing, Queued, Running and
  WaitingForInput; Draft, Ready and outcomes can archive, and Archived is
  idempotent.
- `close_task_tab` uses session::State::reduce against full runtime target:
  stale target is ignored; live task asks confirmation; confirmed closure is
  allowed even for the last tab (host creates replacement). Closing a live
  session signals Stopped only after the tab is removed and other live task
  sessions are aggregated. Sessions/conversations stay in durable history.
- `TaskWorktreeDeleteOpen/Confirm/Completed`: absent task, absent worktree or
  remote workspace refuse inspection. Active lifecycle OR open tabs enters
  cleanup context as process_running. Confirm requires a successful inspection;
  ActiveProcess, UncommittedChanges, UnpushedCommits block. ExistingPullRequest
  alone does not block. Missing/unregistered path clears record only; otherwise
  remove without force. Clear durable worktree only after successful removal;
  sessions and conversation references survive. No task-record deletion verb.
- `request_task_publish` asks confirmation for no linked issue. After confirm,
  missing task/path or already busy silently does nothing. There is no separate
  archived/lifecycle gate. Push uses stored branch; draft PR uses existing
  pr_base_branch and draft_pr_copy. Completion refreshes delivery, then replies
  to UI; do not add lifecycle or branch restrictions here.
- `try_launch_task_child`: missing task/path, non-Ready worktree/missing directory,
  removed preset/CLI without task tools or remote workspace refuse. MCP preset
  lookup is case-insensitive and trims name; error quotes original name and
  lists configured presets. Terminal bypasses agent policy/capacity. Agent
  launch queues at max(1, limit) or policy hold, except an existing holder can
  launch when not held; a held holder errors with policy reason. Existing queue
  entry is reused. UI-origin bypasses policy, explicit model choice overrides
  policy but not concurrency. Store transition checks still apply; no new
  blanket archived gate. Queue persistence precedes queue acknowledgement.
- `delegate_task_requested`, `delegate_task_targets_checked`,
  `delegate_task_in_checkout`, `start_delegated_worker`: source is the caller's
  open local tab, never active tab fallback. Existing checkout reuse refuses
  archived task, non-blocked open worker, or a live non-final session (blocked
  idle worker exception). Checkout/add_dirs membership is checked off-thread
  against caller home/repos/worktrees. Current legacy completion retains the
  caller snapshot (pinned by legacy_worker_preparation_retains_stale_caller_snapshot).
  New core completion must additionally recheck live caller/runtime incarnation,
  hosted task/chat session, source workspace/repository/checkout and destination
  checkout/repository, including changed workspace membership. It grants no IO
  permission from a caller-typed session id.

## Later main.rs integration (this PR does not wire these)

- `pending_task_from_control` and `TaskCreateSubmit`: shared create validation;
  `TaskMetadataResolved` / `TaskWorktreePrepared`: ordered persistence effects.
- `update_handoff_requested`: shared handoff decision, followed by existing
  record_handoff save/reply behavior; keep worker_reported UI updates.
- `TaskArchive`: shared Close/archive effect.
- `close_task_tab` / `TaskTabStopAndClose`: shared close effect; host still drops
  PTY/chat handles, aggregates lifecycle and moves focus.
- `TaskWorktreeDeleteOpen`, `TaskWorktreeDeleteConfirm`,
  `TaskWorktreeDeleteCompleted`: inspect/remove/clear effects.
- `request_task_publish`, `start_task_publish`, `TaskPublishCompleted`:
  confirm/publish effects and delivery persistence.
- `TaskControlOperation::LaunchSession`, `launch_task_child`,
  `try_launch_task_child`, `promote_queued_tasks`: shared launch decision with
  current directory/preset/policy/slot facts, retaining host process/brief work.
- `delegate_task_requested`: capture the core worker authority before async
  target checking. Recheck it at `DelegateTaskTargetsChecked` /
  `delegate_task_targets_checked`, `TaskMetadataResolved`, `TaskWorktreePrepared`
  and `start_delegated_worker`; checkout reuse must also check just before
  `delegate_task_in_checkout` writes. The future adapter must collect facts by
  captured UID, not active tab or the old PendingWorker.caller snapshot.

Root `src/tasks.rs` already re-exports core tasks (new validators included).
`src/task_mcp.rs` has DTO/transport only; its create/handoff validation is in
main.rs, so it is deliberately unchanged to preserve dispatch/save ordering.

## Core effect sequencing and R2 plan resolution

R2 plan review: `e37086c3-d13f-4640-8d4a-c4cabde221fe` (GPT-6 Astra).
The reducer retains pending preparation state, not another writable store:
BeginWorkerPreparation before async checks; CheckWorkerPreparation after target
checks and before metadata/adoption writes; BindWorkerPreparation after metadata
save with the exact expected preparation; WorkerPrepared before the store write;
WorkerPreparationSaved after the real store result, with freshly projected facts.
Cancellation/timeout/owner teardown must cancel the pending preparation. A caller
or target refusal consumes that operation. Duplicate completions cannot write or
launch. A prepared worktree left after refusal is reported for explicit cleanup;
there is no automatic destructive rollback from an authority refusal.

Each command produces one effect, never an unconditional save/start/reply batch.
PersistHandoff executes existing delegations::record_handoff; its partial-save
error is returned unchanged, with no worker-report notification on error.
PersistWorkerPreparation must complete before WorkerPreparationSaved can emit
StartWorker. StartWorker delegates insertion, provisional process/session launch
and attachment to the existing runtime service: insertion failure prevents
launch, attachment failure stops the provisional worker. These runtime operations
are not reimplemented by this core-only slice. QueueSession and ArchiveTask also
require actual store success before acknowledging. RemoveWorktree means
force=false and permits clear_worktree only after actual removal success.

Public validators: tasks::{validate_task_title, validate_task_objective,
validate_base_reference, is_linear_issue_key, validate_task,
validate_task_transition}; runtime tasks::validate_branch re-exports the unchanged
Git-backed task_worktree validator. HandoffStatus and cleanup DTOs now live in the
ungated model, with their old service-module paths re-exported unchanged.

Pinned main.rs line references at the starting revision:

| Boundary | Sites |
| --- | --- |
| Create/preparation | pending_task_from_control:13309; TaskCreateSubmit:28657; TaskMetadataResolved:28715; TaskWorktreePrepared:28810 |
| Worker authority | delegate_task_requested:18917; delegate_task_targets_checked:18997; delegate_task_in_checkout:19071; start_delegated_worker:19292 |
| Handoff | update_handoff_requested:19409 |
| Close/archive | close_task_tab:12242; TaskArchive:29180; TaskTabStopAndClose:29711 |
| Launch | promote_queued_tasks:13689; try_launch_task_child:13766; LaunchSession dispatch:24187 |
| Publish | request_task_publish:13617; start_task_publish:13638; TaskPublishConfirm:29142; TaskPublishCompleted:29151 |
| Delete | TaskWorktreeDeleteOpen:29293; TaskWorktreeDeleteConfirm:29362; TaskWorktreeDeleteCompleted:29417 |

R2 code review `1469ae86-a92a-4d53-952e-06dd576b9ea5`, F1: ordinary
preparation failures must emit PersistWorkerPreparationFailure after source and
target checks. The adapter runs existing fail_worktree_preparation before
returning the preparation error. Preserve the expected path only when the error
contains "rollback requires attention" and fresh Path::exists facts include it.
A failure save cannot emit StartWorker; an authority refusal emits no write.
Regression tests cover ordinary failure, existing/missing rollback paths
(including a file), actual failure-save errors, and stale source/target refusal.
