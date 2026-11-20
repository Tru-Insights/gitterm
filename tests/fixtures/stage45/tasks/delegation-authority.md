# TRU-274 PR 17a: review/consult lifecycle authority

This is the coherent first part of row 17. The one task MCP receiver and the
one TaskAuthority store remain in Iced. Existing task/delegation fixtures and
C0/stage-1 goldens are unchanged. The new lifecycle trace was pinned through
the old Iced helpers before extraction and passes through the reducer adapter.

| Intent | State/decision | Effects executed by Iced |
| --- | --- | --- |
| Request (review/consult/rerun) | Capture catalog caller incarnation/roots; deduplicate pending rerun; bind newly authorized reruns to the live caller | Check checkout and capture Git/authorization-root facts off-thread, with opaque request ID |
| Checked | Consume request once; refuse changed catalog caller | Insert durable requested record; retain checked filesystem facts in memory; reply/anchor |
| Stored | Bind successful insertion to its caller and checked filesystem snapshot | None |
| Admit | Revalidate catalog caller; preserve policy/cap/store order; reserve before asynchronous preflight | Check checkout/Git/roots off-thread with a generation ticket, or record refusal |
| StartChecked | Consume the current ticket once; compare captured catalog/filesystem authority; recheck current policy/cap/worker occupancy | Start Codex stream, requeue a policy hold, or record refusal |
| Progress | Ignore stale/settled events; retain activity | Record thread identity and refresh card |
| Finished | Settle once before terminal IO; release reservation | Record completion/failure/time-limit outcome, attention and next admission |
| Dismiss | Settle before IO; remove activity and held sends | Abort exact delegation handle once, cancel if active, remove queued worker launch, persist dismissal |
| ManualSend | Compose bounded selection, suppress delivered/held duplicates | Hold/refresh, flush, or report error |
| Flush (tick/turn-end) | Prune stale parent associations and replaced reports; merge auto/manual sends; hold mid-turn | Refresh cards and synchronously submit one bounded batch |

Process launch/abort handles, filesystem checks, store writes, clocks, MCP
replies, notifications, attention and chat submission remain host effects.
Worker launch policy occupancy includes the reducer's reservations, including
preflight checks. No queued filesystem checks run on the UI thread. A settled
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

Code review `357d5042-28cf-406c-b0f2-c04a5e284374` requested three fixes:
F1 live caller rebinding for newly authorized reruns; F2 optional unavailable
unrelated repository roots; F3 asynchronous queued filesystem validation.
The follow-up uses catalog-only caller facts, checked filesystem snapshots,
and generation-bound preflights with a second policy/cap gate. Regression tests
cover these paths, cancelled/duplicate/superseded tickets, policy changes during
preflight and changed checkout authority. Final review IDs are in the handoff.
