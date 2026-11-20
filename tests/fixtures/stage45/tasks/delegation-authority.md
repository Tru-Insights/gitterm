# TRU-274 PR 17a: review/consult lifecycle authority

This is the coherent first part of row 17. The one task MCP receiver and the
one TaskAuthority store remain in Iced. Existing task/delegation fixtures and
C0/stage-1 goldens are unchanged. The new lifecycle trace was pinned through
the old Iced helpers before extraction and passes through the reducer adapter.

| Intent | State/decision | Effects executed by Iced |
| --- | --- | --- |
| Request (review/consult/rerun) | Capture caller incarnation, repository and authorization roots; deduplicate pending rerun | Check checkout off-thread, with opaque request ID |
| Checked | Consume request once; refuse changed caller | Insert durable requested record, then reply/anchor |
| Stored | Bind successful insertion to its original caller | None |
| Admit | Revalidate queued caller; preserve policy/cap/store order and reserve before launch | Start Codex stream, or record refusal |
| Progress | Ignore stale/settled events; retain activity | Record thread identity and refresh card |
| Finished | Settle once before terminal IO; release reservation | Record completion/failure/time-limit outcome, attention and next admission |
| Dismiss | Settle before IO; remove activity and held sends | Abort exact delegation handle once, cancel if active, remove queued worker launch, persist dismissal |
| ManualSend | Compose bounded selection, suppress delivered/held duplicates | Hold/refresh, flush, or report error |
| Flush (tick/turn-end) | Prune stale parent associations and replaced reports; merge auto/manual sends; hold mid-turn | Refresh cards and synchronously submit one bounded batch |

Process launch/abort handles, filesystem checks, store writes, clocks, MCP
replies, notifications, attention and chat submission remain host effects.
Worker launch policy occupancy includes the reducer's reservations. A settled
ID cannot auto-restart even if started/terminal/cancellation saves fail. A
rerun uses a new ID and policy admission. Durable delivery receipts retain the
existing in-process retry behavior; crash-proof exactly-once delivery is not
claimed. Existing Codex timeout process-group handling is unchanged.

The reducer is under the same `runtime` feature as PR 16's delegation services.
The default-free core build remains supported.

## Routed Iced paths

- MCP review/consult requests and page review/consult/rerun requests use the
  request/check/insert/admission path.
- DelegationChecked, DelegationProgress, DelegationFinished,
  DelegationSendRequested and DelegationDismissRequested.
- Limits-driven queue pumping, tick auto delivery and turn-end held delivery.
- Activity/card readers, worker-close held-state reader, worker policy occupancy.

## 17b: remaining row-17 work

- Implement-worker request/lifecycle reduction after the existing
  TaskAuthority worker-preparation effects (do not replace those arms).
- Hosted task-session handoff completion/blocking, parent wake/attention and
  final-worker close coordination.
- Worker deadline scheduling/expiry/teardown by delegation + task-session
  identity; retain existing exact-child safeguards and pins.
- Remaining task-MCP operations in TaskControlRequested, including
  delegation_get/delegation_list projections and task handoff dispatch,
  through the shared command/reducer bridge, preserving reply/identity rules.
- Unified worker cancel/rerun invariants and adapter tests for those moved
  paths. Existing worker behavior/pins remain exercised in 17a's full suite.

## Review evidence

Pre-implementation R1/R2 consultation: `8ce46366-2608-4d21-a3da-0f7c017c6d39`,
confirmed this task checkout and HEAD `503ede5`. Its caller incarnation,
reservation/save-failure, union accounting, synchronous delivery and shared
held-state reader requirements are reflected above. Code review IDs and final
command exit codes belong in the durable task handoff.
