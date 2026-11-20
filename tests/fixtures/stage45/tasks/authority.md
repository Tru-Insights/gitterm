# TRU-274 PR15a task authority and ingress

This is the approved read-first split of stage-5 PR15. `TaskAuthority` owns the
single startup-loaded task store. `task_list` and `task_get` use its reducer;
Iced adds current session and preset facts synchronously in the same update.
Create, launch, handoff, notify, delegation and UI mutations borrow this same
store through `store()` / `store_mut()`. They have not been extracted into
mutation reducers. There is no second store or task-MCP server.

The process ingress remains take-once. The runtime owns reply channels;
reducer commands/effects carry process-local opaque reply IDs. Admission is
FIFO; deferred replies may finish out of order. First reply wins, final reply
handle drop releases the waiter, and timeout/cancellation frees reply storage
without retracting accepted work. Owner and receiver closure release pending
replies even when deferred handles survive. The 300-second timeout and error
strings are unchanged.

Evidence:

- Existing stage45 task/store/session/identity goldens and the stage-1 baseline
  retain their contents. Their constructors now borrow the shared authority.
- `tools.wire.json` and `src/stage45/task_mcp.rs` pin task tool request defaults,
  bridge attribution, response wrapping and sequential batch partial errors.
  These transport replies are synthetic, not persistence evidence.
- `src/stage45/task_authority.rs` exercises actual Iced dispatch for read
  projections/errors, notify, create validation, deferred preparation and save
  failure, handoff attribution refusal, abandoned waiters and owner shutdown.
- `task_commands::runtime::tests` exercises timeout, cancelled waiters, FIFO,
  independent reply order, clone races, last-handle drop, receiver/owner close,
  repeated close and late replies. `tasks::state::tests` verifies read filtering,
  order, errors, shared adapter writes and absence of read saves.
- MCP DTOs, schemas, tool registrations, bearer/caller handling, task model,
  store serializer, launch policy and the delivery receipt exception are
  unchanged. No dependency or lockfile change is required.

Independent R1/R2 plan review and R3 compatibility audit:
`d832612b-d248-4884-ba71-5c9353ab08b9` (`gpt-6-astra`, base `2261642`). Its
reply-lifetime, synchronous-projection and single-store conditions are covered
above. Code review and final command results are recorded in the task handoff.

PR15b must extract task mutations and their completions before PR17. The plan
review identified an existing stale caller/checkout snapshot after asynchronous
worker preparation. `legacy_worker_preparation_retains_stale_caller_snapshot`
characterizes closed-tab and changed-checkout cases with an invalid preset, so
no process starts. Fresh caller/session/checkout validation is a required R2
gate for PR15b, not a claim made by this slice. PR17 retains responsibility for
delegation/worker reducers, dispatch, delivery and deadline teardown over this
same authority and bridge. Full PR15/C17 completion is not claimed.
