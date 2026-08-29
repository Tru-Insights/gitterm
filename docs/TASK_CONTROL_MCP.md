# GitTerm V5 Task Control MCP

TRU-106 adds the first agent-facing control surface for GitTerm's durable task
model. It is a transport adapter into the application, not a second task
service.

## Ownership boundary

The MCP server never opens or writes `tasks.json`. A tool request crosses an
in-memory command channel into the Iced update loop. The application then uses
the same task store, worktree preparation, workspace, and tab code as the UI and
answers the request after the operation succeeds or fails.

This keeps one serialization order for task mutations and prevents MCP state
from diverging from the visible application.

## Security and runtime isolation

- GitTerm reserves a V5-only loopback endpoint in ports `25030-26029`.
- Every process receives a random 64-character bearer token held in memory.
- The URL and token are inherited only by local GitTerm terminals through
  `GITTERM_V5_TASK_MCP_URL` and `GITTERM_V5_TASK_MCP_TOKEN`.
- Codex receives ephemeral per-process MCP overrides. GitTerm does not modify
  global or repository Codex configuration.
- The task server has a separate endpoint and token from the browser MCP.
- Remote sessions never receive the local endpoint.

## Initial tools

| Tool | Behavior |
|---|---|
| `task_list` | Return durable task state, open child sessions, and configured preset names. |
| `task_get` | Return one task plus its currently open child sessions. |
| `task_create` | Validate metadata, persist the task, and prepare its worktree asynchronously. |
| `task_create_batch` | Create tasks sequentially and preserve explicit partial failures. |
| `task_launch_session` | Open a configured preset or plain terminal in a ready task worktree. |
| `task_update_handoff` | Persist the latest summary, decisions, next steps, and blockers for another harness or the coordinator. |

Tool responses use durable task IDs and task-session IDs. Agent-created tasks
and sessions are background operations and do not move the user's active tab.
Task records also retain resumable Claude, Codex, and Pi conversation references
discovered in the task worktree. The task detail view uses those references to
resume the exact harness conversation after its tab has been closed.

## Prompt and harness boundary

`task_launch_session` launches a process; it does not submit the stored task
objective. Its response includes `objective_submitted: false`. GitTerm cannot
safely type a prompt into an arbitrary configured command without knowing that
harness's readiness, input framing, permission behavior, and failure semantics.

Codex is the first automatically configured MCP client. Other local harnesses
inherit the endpoint environment, but automatic attachment and prompt delivery
require an explicit adapter. ACP is a promising common GitTerm-to-agent session
transport; MCP remains the agent-to-GitTerm task-control surface.

## Handoff boundary

`task_update_handoff` stores a compact handoff in the task record rather than
copying harness transcripts. **Continue with…** copies a briefing containing
the objective, worktree, branch, current durable handoff, and prior conversation
references before opening the harness picker. A same-harness continuation can
resume the native conversation; a different harness receives the briefing and
works against the same task worktree.

## Intended extensions

The same command bridge can later support lifecycle control and mediated
cross-harness communication:

- `task_send_message`
- `task_wait`
- durable artifacts and structured worker results
- stop, resume, archive, and writer-lease operations

Handoffs should be persisted as compact packets containing the objective,
approved decisions, artifacts, branch state, verification results, and open
questions. GitTerm should deliver those packets through ACP or a harness adapter
rather than making harness processes connect directly to each other.
