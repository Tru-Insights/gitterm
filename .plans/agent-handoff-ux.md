# Agent handoff UX: ask for a review, get findings back where you work

Status: design, not started. Branch context: TRU-140 (`tracey/tru-140-claude-control-protocol-spike`, PR #43).
Goal: from the chat tab she is working in, Tracey asks another agent for a code review (or hands off an
implementation slice), keeps working, is told when the result lands, reads it, and acts on it from the same chat.
Constraint already decided: handoffs are summaries, not transcript copies
(`task_handoff_contains_durable_context_without_transcript_copying`, src/main.rs).

Authored by an Opus design pass on 2026-10-08 from a read-only audit of the repo; line numbers are as of
commit ce922c6.

## 1. Current-state audit

### Task MCP surface (src/task_mcp.rs)
Server `gitterm-v5-tasks`. HTTP endpoint `http://127.0.0.1:<25030+>/mcp`, bearer auth (`authenticate`, :327).
The token is `GITTERM_V5_TASK_MCP_TOKEN`. Every call crosses a command bridge into the Iced loop
(`dispatch_operation`, :380, 300 s `COMMAND_TIMEOUT`, :46). There are six tools (`tool_router`, :413-534):

| Tool | Input (exact fields) | Notes |
|---|---|---|
| `task_list` (:423) | `include_archived: bool` (default false) | also returns `available_presets` |
| `task_get` (:436) | `task_id` | adds live `open_sessions` (`task_control_value`, main.rs:7128) |
| `task_create` (:449) | `title, objective, repository_path, workspace_name?, issue_key?, base_reference?, stopping_boundary? (plan_only / implement_until_tests_pass / prepare_draft_pr)` | prepares a worktree, launches nothing |
| `task_create_batch` (:465) | `tasks: [CreateTaskRequest]` | sequential, per-item ok/error |
| `task_launch_session` (:500) | `task_id, preset_name?` | terminal preset or plain terminal only |
| `task_update_handoff` (:519) | `task_id, summary, decisions[], next_steps[], blockers[], session_id?` | overwrites the single `TaskRecord.handoff` |

There is also a plain HTTP route `POST /notify?task_id=&session_id=` (:307, `notify_session_event` :355).
It reads only the payload `type`.

**Gap: the server has no caller identity.** A tool call cannot tell which tab or session made it.
Coordinator provenance is always recorded with `session_id: None` (main.rs:14149-14157).
`task_update_handoff.session_id` is optional, and no brief tells the worker its session id
(`task_handoff_prompt`, main.rs:2606). rmcp 2.2 does inject `http::request::Parts` into tool
context (`rmcp-2.2.0/src/transport/streamable_http_server/tower.rs:491-541`), so a per-session URL query or
header is readable without changing transports.

### Tasks, sessions, handoffs (src/tasks.rs)
- `TaskRecord` (:27): one worktree and branch per task; `handoff: Option<TaskHandoff>` (:52) is
  latest-wins; `sessions: Vec<TaskSessionRecord>` (:61).
- `TaskSessionRecord` (:281): `task_session_id`, label, `harness`, `conversation`
  (`HarnessConversationRef` backend + id, :275), `objective_delivery`.
- `TaskHandoff` (:306): `summary, decisions, next_steps, blockers, updated_by_session_id, updated_at`.
  `TaskStore::update_handoff` (:932) rejects an empty summary and does nothing else: no attention, no notification.
- `HarnessKind` (:253) has `NativeClaude`, but `try_launch_task_child` (main.rs:7836) only launches
  terminal presets. A task cannot start a native chat tab.
- `TaskAttentionReason` (:388) includes `CompletedUnread` and `ReadyForReview`, which feed the Needs-you inbox.

### Launch, briefs, notify (src/main.rs)
- `try_launch_task_child` (:7836): refuses remote workspaces (:7893); applies a concurrency
  queue; writes the brief to `<config>/task-briefs/<session>.md` (`write_task_brief` :2714) and
  passes it as the positional prompt (`command_with_initial_prompt` :2731); records a `TaskSessionRecord`.
- Codex notify: `codex_task_notify_command` (:7819) → `configure_codex_notify` (task_mcp.rs:623)
  injects `notify=[sh -c curl … /notify?task_id&session_id]`. The handler `SessionEvent` (main.rs:11518)
  acts only on `agent-turn-complete`: it sets the tab to `AwaitingInput` and `HumanInputRequired`.
- The `UpdateHandoff` handler (main.rs:11455) validates the session id when one is given, stores the handoff
  and replies. It wakes nobody.
- Task MCP env reaches terminals per tab at creation (`create_tab_for_workspace`,
  main.rs:10250-10255). This is the natural place for per-tab caller identity.

### Native Claude chat tab (TRU-140)
- Spawn and send: `Event::AgentSubmitPrompt(tab_id, prompt)` (main.rs:12476) spawns `ClaudeSession`
  lazily, attaching the task and browser MCP (`claude_mcp_server()`, task_mcp.rs:202; main.rs:12492-12503), then
  `HarnessCommand::SendUserMessage` (harness/mod.rs:161; claude.rs:984 writes the stdin frame at once).
  Sending mid-turn was not tested (TRU-140 plan, interrupt `still_queued` hints that the CLI queues).
- Task linking for chat tabs happens only by cwd: `anchor_open_sessions_to_task` (main.rs:6403) or the
  worktree migration (main.rs:8222). `TabState.task_id` / `task_session_id` / `chat_session_id` are
  persisted in `WorkspaceTabConfig` (config.rs:627-655).
- Page model: `AgentSession.conversation` is the source of truth. It is replayed with `__replay`
  (main.rs:289) and live events are appended with `__appendEvent` (main.rs:273). After a restart the timeline is
  rebuilt from the Claude transcript (`harness/transcript.rs:35`), so GitTerm-only cards are lost unless
  they are stored elsewhere.

### Transcript readers
- `chats::claude_session_path(id)` (chats.rs:677) finds `~/.claude/projects/*/<id>.jsonl`.
  `build_local_index` (:733) covers Claude, Codex and Pi. `load_preview` (:771) reads a tail.
- `transcript::parse_claude_transcript` (transcript.rs:42) works on Claude only and caps results at 20k chars
  (`MAX_RESULT_CHARS`, :23).

### Presets and Codex
- Default presets (config.rs:236) are Pi, Claude Code, Codex and Gemini. `AgentPreset` has `{name, command, resume_command, icon, color}`.
- Local `codex-cli 0.161.0` has `codex exec review [--uncommitted | --base <BRANCH> | --commit <SHA>]`,
  `--json` (JSONL events), `-o <file>` (last message), `--output-schema <file>` (support for this together
  with `review` is unverified), `--ephemeral`, and `codex queue --thread <id> --message <text>`.

## 2. User journeys

### A. Review from the chat she is working in (headline)
1. She is in a Claude chat tab on `tracey/tru-140…` (her own checkout, not a task). She types
   "get Codex to review this branch against v5, focus on webview lifecycle", or clicks **Review…** in
   the composer toolbar and picks Codex / "branch vs v5" / focus text.
2. Claude calls `review_request`. It is pre-approved, so there is no prompt. A GitTerm card appears under the tool
   card: **Codex review · branch vs v5 @ ce922c6 · running 0:12**. Claude says it requested review
   `D-7` and will pick the findings up when they land. The turn ends.
3. She keeps chatting and editing in the same tab. Only the card's timer moves.
4. Codex finishes. The card flips to **Review ready · 5 findings (1 P1, 3 P2, 1 P3) · reviewed at ce922c6**.
   If the branch moved since, it adds "branch has moved since". If she is on another tab, this tab gets a
   "Review ready" dot and the Needs-you inbox gets a row.
5. She expands the card. Each finding shows severity, title, `file:line` (click opens the file or diff
   viewer over the chat) and the body. She can untick findings she disagrees with.
6. **Send to Claude** posts one bounded user message ("Codex review D-7, 4 selected findings: F1 [P1]
   src/main.rs:289 …; full record: delegation_get D-7"). If Claude is mid-turn the button reads
   "Queued, sends when Claude finishes".
7. Claude fixes and replies per finding id. "Re-run the review" creates `D-8` with `previous: D-7`, so the
   reviewer's brief lists the findings already reported.

### B. Hand an implementation slice to a worker
1. In the chat she says: "hand the Excalidraw export fix to a Codex worker, TRU-150, stop when tests pass."
2. Claude calls `delegate_task`. GitTerm creates the task and worktree and launches the Codex preset in a background
   tab (focus stays in the chat). The brief ends with **Report back**: call `task_update_handoff` with
   `status: done|blocked`. The card shows **Worker · Codex · TRU-150 · running**, mirroring the task lifecycle.
3. If the worker waits for input (Codex notify), the card shows **Worker needs you · Open worker tab**.
4. The worker records `status: done`. The card shows the summary, decisions, next steps and blockers. **Send to Claude**
   posts it, and Claude can follow with `review_request` targeting the worker's branch.

Terminal parents (Claude or pi in a terminal) get the same flow minus the card. The tab gets an attention badge,
the inbox row has **Copy for agent**, and the agent can fetch the result itself with `delegation_get` / `delegation_list`.

## 3. Design options

### Option A: everything is a task
A review becomes an auto-created task ("Review: <branch>") and its result is the task handoff.
- Pros: reuses the rail, inbox, lifecycle, history and `task_update_handoff` unchanged.
- Cons: provisions a branch and worktree for a read-only job; a worktree at HEAD **cannot see uncommitted
  work**; fills the rail with review tasks; findings get flattened into `decisions` strings; one
  latest-wins handoff per task collides when a task has both a worker and a reviewer; still no parent
  identity, so nothing to wake. This is the ceremony she called awkward.

### Option B: delegations over the task control plane (recommended)
A **delegation** is a parent-addressed request with a typed result, stored next to tasks.
```
Delegation { delegation_id, kind: review | implement,
  parent: { session_uid, chat_session_id?, workspace, cwd },
  child:  { runner: codex_review | task_session, task_id?, task_session_id?, conversation? },
  brief, target?: { mode: uncommitted | base(ref) | commit(sha), focus? }, previous?: id,
  status: requested | running | completed | failed | interrupted | cancelled,
  result?: { handoff: TaskHandoff, findings?: ReviewFindings, reviewed: { head, dirty, target } },
  created_at, updated_at, delivered_at? }
ReviewFindings { verdict: correct|needs_changes|unknown, summary,
  findings: [{ id: "F1", severity: P0..P3, title, body, file, line_start, line_end, confidence? }],
  reviewer: { harness, model?, conversation_id? }, structured: bool }
```
- **Protocol:** new tools `review_request`, `delegate_task`, `delegation_get`, `delegation_list`.
  `task_update_handoff` gains an optional `status: progress|done|blocked` (default `progress`).
  Caller identity comes from `?caller=<session_uid>` on the per-tab MCP URL; handlers read it from `Parts`
  and the bridge envelope gains `caller: Option<String>`.
- **Persistence:** `TaskStoreDocument.delegations` (serde default, additive like 4B). It uses the same atomic writer
  and single owner. `reconcile_after_restart` marks `running` as `interrupted`.
- **Review runner:** GitTerm spawns `codex exec review <target> --json` in the parent cwd as a
  background process (not a terminal tab, so a restart never re-runs it), logs to
  `<config>/delegations/<id>.jsonl`, parses the structured review into `ReviewFindings`, and falls back to
  the last message as `summary` with `structured: false`.
- **UI surface:** a card in the chat page (new `{"type":"delegation"}` payload in `agent_chat.html`),
  re-inserted on replay from the store because it is not in the Claude transcript. Elsewhere it uses tab attention
  and an inbox row.
- **Failure modes:** codex missing or logged out → `failed` with the stderr tail and "Open in terminal";
  unparseable output → unstructured fallback; parent tab closed → result waits and reappears when
  that `chat_session_id` is reopened (or in the inbox); app restart → `interrupted` + **Re-run**; branch
  moved → stale banner (line numbers may drift); a send while streaming waits for `TurnCompleted`.
- **Restarts:** parent identity survives through the persisted `session_uid` and `chat_session_id`. In-flight reviews do not.
- **Remote:** v1 refuses with an explicit error (matching main.rs:7893). The record carries cwd and
  workspace so a later agentd runner can execute `codex exec review` remotely.
- **Harness-agnostic:** caller identity, the delegation store, the MCP tools, the Codex runner and parser,
  the findings contract, attention and inbox, and `delegation_get` pull. **Claude-chat-only:** the timeline card,
  Send-to-Claude through `AgentSubmitPrompt`, and queue-until-turn-complete.
  **Later adapters:** Codex parents through `codex queue --thread` (needs the Codex conversation id);
  pi through its extension.

### Option C: pointer + pull only, optionally blocking
Add `session_handoff_get` and `session_transcript_slice`. The parent pulls when told to, or
`review_request` blocks until the review is done.
- Pros: smallest change; nothing new to deliver.
- Cons: nobody is told when findings land (fails "get told"); a blocking tool call freezes the parent's turn
  for minutes and hits the 300 s bridge timeout; transcript slicing is Claude-only today and pulls
  toward transcript copying. Useful as a **drill-down** tool on top of B (S7), not as the design.

### Evaluation of the floated ideas
1. Pointer + pull: keep `delegation_get` as the pointer (summary and findings first). Transcript slices
   are an optional S7 and are bounded (last N turns, char cap, MAX_RESULT_CHARS-style truncation).
2. Wake the parent: yes, but deliver through a card with a one-click send rather than an unconditional user message.
   Unattended injection spends tokens and can start edits while she is typing. Auto-send is a toggle (D1).
   Codex notify stays the waiting-for-input signal for workers.
3. Codex review preset: yes, as a GitTerm-run headless `codex exec review`, not a terminal preset.
   The structured findings come from Codex's review mode, so the reviewer does not need to call MCP.

## 4. Recommendation and sliced plan

Take **Option B**. Each slice ships on its own. Every dev or manual check uses an isolated
`GITTERM_V5_CONFIG_DIR` (AGENTS.md).

**S0: pin the Codex review output (spike, read-only).** In a scratch repo with a planted bug, run
`codex exec review --uncommitted --json -o last.md`, `--base <ref>` and `--commit <sha>`, and record the JSONL.
Identify the structured review event and its fields, check whether `--output-schema` composes with
`review`, and check the default sandbox for `exec review`.
*Accept:* fixtures under `tests/fixtures/codex/review-*.jsonl` plus a note in this file naming the event and
fields the parser will use. Do not write the parser against guessed field names (AGENTS.md).

**S1: caller identity.** Add a persisted `session_uid` per tab (new optional field in `WorkspaceTabConfig`,
generated when missing). The per-tab task MCP URL becomes `…/mcp?caller=<uid>`: terminals through the per-tab
env (main.rs:10250), chat tabs through `claude_mcp_server(caller)`, Claude terminal launches through
`configure_claude_command`. Handlers take `Extension<http::request::Parts>`. `task_create` records
`TaskCreator.session_id`. `task_update_handoff` infers `session_id` from the caller when omitted.
*Accept (headless):* a task_mcp.rs test connects with `?caller=abc`, calls `task_list`, and asserts that
the bridged envelope carries `caller == "abc"`. Existing MCP tests still pass. A config round-trip test covers `session_uid`.

**S2: delegation store.** `Delegation` / `ReviewFindings` types in tasks.rs, `TaskStoreDocument.delegations`,
insert/update/complete/list-by-parent, validation, and reconcile to `interrupted`.
*Accept (headless):* an old tasks.json without `delegations` loads; round-trip; reconcile turns `running` into
`interrupted`; completing twice is an error, not a silent overwrite.

**S3: review runner + `review_request` / `delegation_get` / `delegation_list`.** `review_request
{target: uncommitted|base|commit, base_ref?, commit?, focus?, reviewer?: "codex", previous?}` returns
`{delegation_id}` in under 1 s. It resolves cwd from the caller's tab and errors for remote workspaces or a non-git cwd.
The runner records the HEAD and dirty fingerprint at start, streams the JSONL to a log, parses it with the S0 contract,
completes the delegation, and raises "Review ready" attention on the parent tab plus an inbox row.
The review case is now usable from any harness: Claude can call `delegation_get` when told to.
*Accept:* parser unit tests on the S0 fixtures (structured and fallback); MCP test that
`review_request` returns an id without waiting; `cargo run --example review_delegation_smoke --
--workdir <scratch repo>` drives the real codex and asserts at least one finding whose file is in the repo.

**S4: chat-tab delivery (Claude-chat-only).** Add the delegation card to `agent_chat.html` (running timer, findings
list, `file:line` opens the viewer, finding checkboxes, Send to Claude, Dismiss), injected on `__replay` from
the store. The bounded message composer (≤ 8 KB, finding ids, `delegation_get` pointer) sends through
`Event::AgentSubmitPrompt`. If the tab is streaming, the message is held on the tab and flushed on `TurnCompleted`.
`delivered_at` is stamped so a replay or restart never sends it twice.
*Accept:* unit tests for the composer (size cap, ids, severity counts) and for the hold-and-flush rule as a
pure function. Manual script: isolated config → chat tab in a scratch repo → "ask Codex to review
uncommitted changes" → card appears → findings land → Send → Claude replies citing F-ids → restart →
card is still present and marked sent.

**S5: Review… affordance.** A composer toolbar button and popover (reviewer, target, focus) in the chat page,
plus a command-palette entry for terminal tabs, all calling the same S3 path. Do not reuse `/review` in the
composer, because Claude Code already has a built-in `/review`.
*Accept:* unit test for the target→codex flags mapping; manual: button → card, with no agent turn spent.

**S6: implementation workers.** `delegate_task {title, objective, preset_name?, issue_key?,
base_reference?, stopping_boundary?}` = create + launch (existing paths) + a `kind: implement` delegation.
The brief adds the delegation id and the Report-back contract. `task_update_handoff` with `status: done|blocked`
from the child's caller completes or flags the delegation (snapshotting the handoff, so the latest-wins
`TaskRecord.handoff` cannot lose it) and wakes the parent. `progress` only updates.
The card mirrors task lifecycle and Codex notify waiting.
*Accept:* MCP test with a fake bridge (create → launch → handoff done → parent delegation
completed); manual script with a Claude worker in an isolated config dir.

**S7 (optional, decision D8): bounded transcript pull.** `delegation_transcript_slice {delegation_id,
last_turns ≤ 5, max_chars ≤ 20k}`, Claude children only (transcript.rs). Codex and Pi readers come later.
*Accept:* unit test on a fixture transcript: limits respected, no tool outputs above the cap.

**S8 (later):** Codex parent wake through `codex queue --thread`, pi wake through its extension, a remote runner through agentd,
and a Claude reviewer (chat tab plus a `review_submit_findings` tool mapping onto the same contract).

### Decisions needed from Tracey
- **D1** Delivery default: card + one-click Send (recommended) or auto-send when the chat is idle.
- **D2** Default review target: what Codex compares against. Its target flags are mutually exclusive: uncommitted changes, or the branch against its base, or a specific commit.
- **D3** Where reviews run: the live checkout, read-only, with a fingerprint and stale banner (recommended), or a snapshot worktree.
- **D4** Where requests appear: the parent tab plus the Needs-you inbox (recommended), or the Tasks rail too.
- **D5** Storage: delegations inside tasks.json (recommended; same last-writer-wins caveat across instances) or a new file.
- **D6** Pre-approval: `delegate_task` inherits the `mcp__gitterm_tasks` allowlist, so an agent can start
  a worker with no prompt. Is that OK?
- **D7** Review limits: a cap on concurrent reviews (separate from `max_concurrent_local_tasks`) and the Codex model.
- **D8** Transcript slices: keep S7 or drop it.

## 5. T3 Code: what we copy and what we don't
There is no local T3 checkout, so this compares against the pattern as described. Confirm before S4.
**Copy:** the request and its result live in the thread you are working in, with no separate task
ceremony; a provider-neutral orchestration layer with per-provider adapters (already mirrored in
`harness/mod.rs` as thread/turn/item/runtime-request); results render as structured timeline
items you act on in place.
**Don't copy:** carrying full conversation context between providers (we pass summaries and findings,
with bounded pull only on request); one worktree per thread (reviews run in place, only implementation work
gets a worktree); a server or web architecture (GitTerm stays the single owner through the MCP bridge).

## 6. Addendum 2026-10-08: in-process subagents and a default reviewer

Tracey: "when I am in Claude Code I can ask you to use Opus and you spin up a sub-agent. It would be nice to
say that in the chat, and maybe have a default for code reviews."

- **Subagents already work in the chat tab.** The tab runs the real CLI, so Claude's Agent tool (with a
  model override) is available and its report returns into the same conversation. Gap: the page drops
  `parent_tool_use_id` frames (`from_subagent` in `harness/claude.rs`), so only the parent's tool card is
  visible. **S4b:** render subagent activity as a nested card under the parent's Agent tool card
  (name, model, running/done, last text), fed by the frames the parser currently discards.
- **D9 Default reviewer.** `review.default_reviewer` setting: `claude-subagent` (in-process Agent tool
  with a review prompt and a model, e.g. Opus; result lands in the chat by itself, no runner or
  delegation record needed) or `codex` (Option B runner, independent model). The Review… button and
  `review_request` use the default unless the request names a reviewer. Recommended order: ship the
  Claude-subagent reviewer first (a prompt template plus the Review… button), then Codex via S3/S4.

## 7. Decisions taken (2026-10-08, ticket TRU-142)

Tracey took the recommended defaults for D1-D9: card with one-click Send (auto-send later as a toggle);
default target branch-vs-base when the branch has commits, else uncommitted; reviews run in the live checkout
with a stale banner; requests show in the parent tab and the Needs-you inbox; delegations stored in tasks.json;
`delegate_task` pre-approved like the other task tools; cap of 2 concurrent reviews and Codex's default model;
transcript slices (S7) dropped until summaries prove insufficient; default reviewer is a Claude subagent on Opus,
Codex second.

Build order: **R1** Claude-subagent reviewer (review prompt template, Review… button, nested subagent cards),
then S0, S1, S2, S3, S4, S5, S6 as written above. Branch `tracey/tru-142-agent-handoff-ux` off the TRU-140 spike.

## 8. Codex review output contract (S0, verified 2026-10-08)

**Versions and setup.** `codex-cli 0.161.0`, default model from `~/.codex/config.toml` (the model name is not in
the events). Scratch Python repo: `main` with one commit, branch `feature` with one commit planting two
off-by-ones (`moving_average`, `last_n`), plus an uncommitted change that indexes `matches[0]` unchecked.
Each run in its own copy of that repo, five in parallel, stdout/stderr/exit code captured separately:
```
codex exec review --uncommitted --json -o last-uncommitted.md                 rc 0, 40 s, 1 finding
codex exec review --base main --json -o last-base.md                          rc 0, 55 s, 3 findings
codex exec review --commit <feature sha> --json -o last-commit.md             rc 0, 43 s, 2 findings
codex exec review --uncommitted --output-schema schema.json --json -o ...     rc 0, 33 s, 1 finding
codex exec review --uncommitted --ephemeral --json -o ...                     rc 0, 55 s, 2 findings
codex exec review --uncommitted --json -o ...   (docstring-only change)       rc 0, 17 s, 0 findings
```
Fixtures in `tests/fixtures/codex/`: `review-{uncommitted,base,commit,schema,ephemeral,clean}.jsonl` (stdout, 10
lines each, 12-17 KB), `review-{uncommitted,base,commit,clean}.last.md` (the `-o` file) and
`review-{uncommitted,base,commit,clean}.rollout.jsonl` (5-line excerpt of the session file, see below). Scratch paths
are `/scratch/repo`; `ls` owner and git author are anonymised; nothing else was edited.

**Headline: `--json` stdout has no structured review event.** Every run emits the same ten lines:
```
{"type":"thread.started","thread_id":"01a11c9d-b01b-77f1-8ae8-36be47f37050"}
{"type":"turn.started"}
item.started / item.completed  {"item":{"id":"item_0","type":"command_execution","command":"/bin/zsh -lc '...'",
                                "aggregated_output":"...","exit_code":0,"status":"completed"}}   (x3, the model's own shell)
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"<rendered review>"}}
{"type":"turn.completed","usage":{"input_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0,
                                  "output_tokens":0,"reasoning_output_tokens":0}}
```
The findings exist only as text in the final `agent_message`, rendered as follows (`—` is U+2014, from `review-base.last.md`):
```
<overall_explanation>\n\nFull review comments:\n\n
- [P2] Include the final complete moving-average window — /scratch/repo/stats.py:9-9\n  <body>\n\n
- [P2] Return None when no user matches — /scratch/repo/stats.py:18-18\n  <body>\n\n ...
```
With one finding, the heading reads `Review comment:` instead of `Full review comments:`. With no findings the text is
only the explanation (`review-clean.last.md`: "The only change expands the module docstring in stats.py. It does not
alter behavior or introduce any actionable issues."). The `-o` file was byte-identical to the last
`agent_message.text` in all 8 runs (6 above plus 2 sandbox probes; no trailing newline).

**The structured review is in the session rollout, not on stdout.** `~/.codex/sessions/YYYY/MM/DD/rollout-<local
start time>-<thread_id>.jsonl` holds `event_msg` → `payload.type:"item_completed"` → `payload.item`:
```
{"type":"EnteredReviewMode","id":"…","target":{"type":"baseBranch","branch":"main"},"user_facing_hint":"changes against 'main'"}
   target variants seen: {"type":"uncommittedChanges"}, {"type":"commit","sha":"09889ca6…","title":null}
{"type":"ExitedReviewMode","id":"…","review_output":{
   "findings":[{"title":"[P2] Return None when no user matches","body":"If `users` is empty …",
                "confidence_score":1.0,"priority":2,
                "code_location":{"absolute_file_path":"/scratch/repo/stats.py","line_range":{"start":18,"end":18}}}],
   "overall_correctness":"patch is incorrect",          ("patch is correct" in the clean run)
   "overall_explanation":"Direct execution confirms …","overall_confidence_score":1.0}}
```
With `--ephemeral`, no rollout file is written, so this source is gone. The rollout's `session_meta` line carries
`creator_user_id` / `creator_account_id`; the fixtures keep only the review items, `AgentMessage`,
`task_started` and `task_complete`. This is Codex's internal persistence format: the item names moved to PascalCase
`item_completed` in this version, so it is not a public contract.

**Flags and options.**
- `--output-schema` composes syntactically with `review` (rc 0) but has **no observable effect**: the final
  message is the same rendered text, not JSON matching the schema. Do not use it.
- `[PROMPT]` and a target flag are mutually exclusive (`--uncommitted x` → clap error, rc 2).
- `--base main` diffs the merge-base **against the working tree** (`git diff 08254e1…` with no second side). It
  reported the uncommitted bug alongside the branch bugs, so base mode means branch plus dirty changes.
- File references: `absolute_file_path` / the text path is the canonical absolute path (`/private/tmp/...`, not
  `/tmp`). Lines are 1-based inclusive ranges, usually single lines. For `--uncommitted` and `--base`, line numbers
  refer to the working tree. For `--commit`, they refer to **the commit's version** of the file: `last_n` was
  reported at line 25 while the working tree has it at line 23.
- Severity: `priority` is an integer 0-3 in the rollout. In the text it appears only as the `[Pn] ` title prefix, which
  the model writes; all 11 findings seen used it. Confidence and overall correctness appear only in the rollout.
- Token usage: `turn.completed.usage` was **all zeros** in every run, and the rollout has no token-count event. Do not
  report review cost from it.
- stderr: four of the six main runs logged one or two benign `ERROR codex_models_manager::manager: failed to refresh
  available models: request timed out` lines with rc 0. Do not treat non-empty stderr as failure.

**Sandbox and repo writes.** `exec review` has no `-s/--sandbox` flag, and neither the stdout events nor
`session_meta` report a sandbox. Two custom-prompt probes (`codex exec review "<run touch probe.txt; touch
../outside.txt>"`, one with `-c 'projects."<repo>".trust_level="trusted"'`) got `Operation not permitted` for both
writes. The model's git calls also failed to create `/tmp` cache files. The effective sandbox is read-only, whether
or not the project is trusted (the override's effect is not confirmed separately).
`git status --porcelain --ignored` was identical before and after all runs, so review mode wrote nothing to the
repo. Codex itself writes the `-o` file (outside the sandbox, any path) and the rollout file (unless `--ephemeral`).

**Incidental, do not parse:** the `command_execution` items. They are the model's own exploration and depend on
the user's setup; here `~/.codex/AGENTS.md` made every review start by reading `~/.agents/model-workflow.md`, and
that text appears in `item_0` of the fixtures. Also ignore `item_N` ids, the usage numbers, the stderr
log lines, the wording of the explanation, and whether the title carries a `[Pn]` prefix.

**Parser recommendation for S3.**
1. Spawn `codex exec review <target> --json -o <config>/delegations/<id>.last.md` without `--ephemeral` or
   `--output-schema`. Record `thread.started.thread_id` as `reviewer.conversation_id`. Success means
   `turn.completed` was seen and the exit code was 0. Neither `turn.failed` nor `error` events appeared in these runs:
   treat any other terminal state as `failed`, with the stderr tail.
2. Final text = the last `item.completed` with `item.type == "agent_message"` → `item.text`. Fall back to the `-o`
   file if the stream was cut.
3. Structured findings, preferred source: locate `rollout-*-<thread_id>.jsonl` under `$CODEX_HOME/sessions` (or
   `~/.codex`), take the last `payload.item.type == "ExitedReviewMode"` and map:
   `priority` → `P{n}`, `title` (strip a leading `[Pn] `), `body`, `code_location.absolute_file_path` (made
   repo-relative after canonicalising both sides), `line_range.start/end`, `confidence_score`. Map
   `overall_correctness` `"patch is correct"`/`"patch is incorrect"` → `correct`/`needs_changes`, and anything else →
   `unknown`. `overall_explanation` → `summary`. Set `structured: true`.
4. Fallback when the rollout is missing or unreadable: parse the final text. Split on
   `\n\n(Review comment|Full review comments):\n\n`; the summary is the part before the split. Each finding header matches
   `^- \[P([0-3])\] (.+) — (/.+):(\d+)-(\d+)$`, and the body is the following lines minus their 2-space indent.
   Verdict: `needs_changes` if at least one finding, else `unknown`. Set `structured: true` only if every
   bullet parsed.
5. Last resort: the whole text as `summary`, with `findings: []` and `structured: false`.

Unit tests should cover (3) with `review-*.rollout.jsonl`, (4) with `review-*.last.md` (1 finding, several,
none), and the stream reading with `review-*.jsonl`. The S3 parser must not depend on any other field of the stdout
items.
