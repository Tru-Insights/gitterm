# TRU-140 Phase A: Claude Code control protocol from Rust

Status: gate passed. The probe (`examples/claude_control_probe.rs`) drives
one long-lived `claude` process over stream-json in both directions from
Rust with no Node in the loop. All nine probe scenarios pass against
`claude 2.1.294` with `--model haiku`.

Sources: `@anthropic-ai/claude-agent-sdk` 0.3.293 (`sdk.mjs`, `sdk.d.ts` and
`sdk-tools.d.ts`, read but never executed), `claude --help`, and the probe's
own wire logs. Where this doc says "SDK does X", the behaviour comes from
`sdk.mjs`. Where it says "observed", the probe saw it on the wire.

## How to run

```sh
cargo run --example claude_control_probe -- --workdir <fresh empty dir> \
    [--model haiku] [--scenarios 0,1,2,6,3,4,7,5]
```

Logs go to `<workdir>-logs/`:

- `stdout-N.jsonl`: every CLI line, verbatim
- `stdin-N.jsonl`: every line the probe wrote
- `stderr-N.log`
- `summary.txt`: the per-scenario notes

The process exits with code 1 if any scenario fails. A full run takes
about 19 s.

## CLI invocation

The SDK builds its arguments in `ProcessTransport` (`sdk.mjs` ~1139596). It
always passes:

```
--output-format stream-json --verbose --input-format stream-json
```

It does not pass `--print`/`-p`. Even so, the CLI runs headless and reads
stdin, as observed. Depending on the options set, the SDK adds the following
flags:

| SDK option | CLI argument |
|---|---|
| `canUseTool` callback | `--permission-prompt-tool stdio` (mutually exclusive with `permissionPromptToolName`) |
| `model` | `--model <m>` |
| `permissionMode` | `--permission-mode <m>` |
| `resume` | `--resume=<id>` (single argv, `=` form) |
| `continue` | `--continue` |
| `sessionId` | `--session-id=<uuid>` |
| `forkSession` | `--fork-session` |
| `includePartialMessages` | `--include-partial-messages` |
| `settingSources` | `--setting-sources=<a,b>` (omitted when unset, so the CLI loads all sources) |
| `allowedTools` / `disallowedTools` | `--allowedTools a,b` / `--disallowedTools a,b` |
| `mcpServers` (non-SDK servers) | `--mcp-config '{"mcpServers":{...}}'` |
| `persistSession: false` | `--no-session-persistence` |
| `maxTurns`, `maxBudgetUsd`, `effort`, `fallbackModel`, `thinking`, `additionalDirectories` | `--max-turns`, `--max-budget-usd`, `--effort`, `--fallback-model`, `--thinking`/`--max-thinking-tokens`, `--add-dir` |
| `permissionPrompts` | `--permission-prompts host\|none` |

The SDK also sets these environment variables:

- `CLAUDE_CODE_ENTRYPOINT=sdk-ts`, only when the variable is unset.
- `CLAUDE_CODE_SDK_READS_SESSION_STATE=1`, which turns on the
  `system/session_state_changed` frames.
- `CLAUDE_AGENT_SDK_VERSION`.
- It deletes `NODE_OPTIONS`.

The probe runs this command:

```
claude --output-format stream-json --verbose --input-format stream-json \
  --model haiku --permission-prompt-tool stdio --permission-mode default \
  --include-partial-messages --setting-sources=project,local [--resume=<id>]
```

It also removes the parent session's `CLAUDECODE` and `CLAUDE_CODE_*` vars,
then sets the two SDK env vars.

`--permission-prompt-tool stdio` is the switch that matters. Without it, a
prompt-worthy tool call is auto-denied in-band, and no `can_use_tool` is ever
sent. A test without the flag produced this tool_result with
`is_error:true`: "touch in '…/probe-noflag.txt' needs approval…".

## Wire shapes (confirmed)

Every frame is a single JSON line terminated by `\n`.

### Host to CLI: `initialize` (optional; see gotchas)

```json
{"request_id":"probe_2_1","type":"control_request","request":{"subtype":"initialize","hooks":{}}}
```

The SDK sends more fields: `hooks` (callback ids), `sdkMcpServers`,
`systemPrompt`, `appendSystemPrompt`, `agents`, `jsonSchema`,
`supportedDialogKinds`, `title`, `skills`, `plugins` and others. All of them
are optional. The SDK's `request_id` is `Math.random().toString(36)`. Any
string unique among in-flight requests works.

Reply (observed, trimmed):

```json
{"type":"control_response","response":{"subtype":"success","request_id":"probe_2_1",
 "response":{"commands":[...],"agents":[...],"models":[{"value":"haiku",...},...],"account":{...},
  "output_style":"default","available_output_styles":[...],"current_permission_mode":"default",
  "session_state":"idle","capabilities":["ui_surface_v1"],"hooks_applied":true,"pid":51308,
  "fast_mode_state":"off", ...},
 "pending_permission_requests":[],"pending_user_dialog_requests":[]}}
```

`pending_permission_requests` re-delivers in-flight `can_use_tool` frames
when a client re-initializes. The SDK feeds them back through its normal
handler.

### Host to CLI: user turn

This is what the SDK writes for a string prompt (`mT` in `sdk.mjs`):

```json
{"type":"user","session_id":"","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly: ready"}]},"parent_tool_use_id":null}
```

`session_id` can be `""` and the CLI fills it in. Optional fields come from
`SDKUserMessage`: `uuid`, `priority` (`now`/`next`/`later`), `shouldQuery`,
`client_composed`, `timestamp`.

### CLI to host: `can_use_tool`

Observed frame (Bash, default mode):

```json
{"type":"control_request","request_id":"82b8b40a-e477-4b5a-939c-dfcfc57bff0a","request":{
  "subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash",
  "input":{"command":"touch probe-ok.txt","description":"Create empty probe-ok.txt in the working directory"},
  "description":"Create empty probe-ok.txt in the working directory",
  "permission_suggestions":[
    {"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch probe-ok.txt"}],"behavior":"allow","destination":"localSettings"},
    {"type":"addDirectories","directories":["<cwd>"],"destination":"session"},
    {"type":"setMode","mode":"acceptEdits","destination":"session"}],
  "blocked_path":"<cwd>/probe-ok.txt","tool_use_id":"toolu_01UHyU5T8MWmPsRzNsisuAcm"}}
```

The CLI chooses this `request_id` as a UUID. Other fields the request can
carry (from `sdk.d.ts`, not seen in these runs): `decision_reason`,
`decision_reason_type`, `classifier_approvable`,
`suppress_always_allow_rule`, `default_to_no`, `matched_ask_rule`, `title`,
`agent_id`, `mcp_server`, `requires_user_interaction`.

### Host to CLI: permission answer

The SDK spreads the callback's `PermissionResult` into the response and adds
`toolUseID` (`{...r, toolUseID: request.tool_use_id}`).

```json
{"type":"control_response","response":{"subtype":"success","request_id":"82b8b40a-…",
 "response":{"behavior":"allow","toolUseID":"toolu_01UH…","updatedInput":{"command":"touch probe-ok.txt","description":"…"}}}}
```

```json
{"type":"control_response","response":{"subtype":"success","request_id":"…",
 "response":{"behavior":"deny","message":"PROBE-DENY: the user declined this command","toolUseID":"toolu_…"}}}
```

The model receives a deny as a tool_result:

```json
{"type":"tool_result","content":"PROBE-DENY: the user declined this command","is_error":true,"tool_use_id":"toolu_…"}
```

Allow can also carry
`"updatedPermissions":[<PermissionUpdate>...]`. Deny can carry
`"interrupt":true`.

`PermissionUpdate` is one of these, each with a `destination` of
`userSettings`, `projectSettings`, `localSettings`, `session` or `cliArg`:

- `addRules`, `replaceRules` or `removeRules`, with `{rules:[{toolName, ruleContent?}], behavior}`
- `setMode` `{mode}`
- `addDirectories` or `removeDirectories` `{directories}`

A failed handler answers like this. The SDK sends it when a callback throws
or for an unknown subtype:

```json
{"type":"control_response","response":{"subtype":"error","request_id":"…","error":"Unsupported control request subtype: foo"}}
```

### AskUserQuestion

This tool arrives as an ordinary `can_use_tool` with
`"requires_user_interaction":true` and no suggestions:

```json
{"request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","display_name":"AskUserQuestion",
 "input":{"questions":[{"question":"Do you prefer red or blue?","header":"Colour","multiSelect":false,
   "options":[{"label":"Red","description":"Pick red"},{"label":"Blue","description":"Pick blue"}]}]},
 "requires_user_interaction":true,"tool_use_id":"toolu_01B8…"},"request_id":"1f3498c6-…","type":"control_request"}
```

The answer is `allow` with `updatedInput = {questions, answers}`. `answers`
maps question text to option label, and multi-select labels are
comma-separated (`AskUserQuestionInput.answers` in `sdk-tools.d.ts`).

```json
{"behavior":"allow","toolUseID":"toolu_01B8…","updatedInput":{"questions":[…unchanged…],"answers":{"Do you prefer red or blue?":"Blue"}}}
```

The resulting tool_result content is `Your questions have been answered: "Do
you prefer red or blue?"="Blue". You can now continue with these answers in
mind.`

### Host to CLI: interrupt, set_permission_mode, set_model

All of these are observed.

| Request | Response |
|---|---|
| `{"subtype":"interrupt"}` (optional `cancel_queued:true`) | `{"subtype":"success","request_id":…,"response":{"still_queued":[]}}` |
| `{"subtype":"set_permission_mode","mode":"plan"}` | `{"subtype":"success",…,"response":{"mode":"plan"}}` |
| `{"subtype":"set_permission_mode","mode":"not-a-mode"}` | `{"subtype":"error",…,"error":"Cannot set permission mode: must be one of acceptEdits, auto, bypassPermissions, default, dontAsk, plan","error_code":"invalid_mode"}` |
| `{"subtype":"set_model","model":"haiku"}` (null or `"default"` resets) | `{"subtype":"success",…}` (no payload) |
| unknown subtype | `{"subtype":"error",…,"error":"Unsupported control request subtype: no_such_subtype"}` |

After an interrupt the CLI sends these frames, in this order:

1. The interrupt receipt (`control_response`).
2. `assistant` with the partial text.
3. `user` with the text `[Request interrupted by user]`.
4. `stream_event` `message_stop`.
5. `result` with `"subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming"`.

The process stays alive, and the next user turn works normally.

### Other CLI-to-host control subtypes

How the SDK handles each one (`processControlRequest`):

| Subtype | SDK behaviour |
|---|---|
| `can_use_tool` | Callback, or an error if no callback is set |
| `hook_callback` | Runs the hook registered via `initialize.hooks`. The CLI only sends it for registered callback ids. |
| `mcp_message` | Routes to an in-process SDK MCP server. Errors if the server is unknown. |
| `elicitation` | Callback, otherwise answers `{"action":"decline"}` |
| `request_user_dialog` | Callback, otherwise left unanswered on purpose: an error is discarded and the CLI cancels at its deadline. The CLI sends it only for kinds declared in `initialize.supportedDialogKinds`. |
| `remote_tool_call`, `remote_plumbing_call`, `remote_tools_probe`, `remote_tools_reannounce` | Left unanswered ("for the machine serving this session's tools") |
| `oauth_token_refresh`, `host_auth_token_refresh`, `remote_control_work_secret`, `ui_*` | Callback, otherwise an error |
| anything else | Error: `Unsupported control request subtype: …` |

The SDK also handles these frame types:

- `control_cancel_request {request_id}`: the CLI withdraws a pending request,
  and the SDK aborts that handler.
- `keep_alive`: ignored.
- `transcript_mirror`: session-store only.

None of the other subtypes appeared in the probe runs. The probe reproduces
the SDK's default behaviour for each one.

## Scenario results (final run)

The run used `claude 2.1.294`. The `haiku` alias resolved to
`claude-haiku-5-5`.

| # | Scenario | Result | Evidence | Latency |
|---|---|---|---|---|
| 0 | `can_use_tool` without sending `initialize` | PASS | Bash `can_use_tool` arrived, was allowed and ran | first delta 1133 ms (cold) |
| 1 | initialize, then "Reply with exactly: ready" | PASS | init reply `success`; `system/init` gave session_id; text deltas; `result` "ready" | spawn→init reply 654 ms; spawn→system/init 693 ms; prompt→first delta 864 ms |
| 2a | Bash allow with updatedInput | PASS | `can_use_tool` Bash; allow; tool_result `is_error:false`; file created | first delta 619 ms |
| 2b | Bash deny | PASS | tool_result `is_error:true` with the deny text; model quoted "PROBE-DENY: …" | first delta 457 ms |
| 6 | Suggestions plus a `localSettings` updatedPermissions | PASS | Three suggestions (see above). Echoing the `addRules`/`localSettings` one wrote `<cwd>/.claude/settings.local.json` `{"permissions":{"allow":["Bash(touch probe-six.txt)"]}}`. A repeat of the same command ran with **no prompt**. | 445 / 482 ms |
| 3 | AskUserQuestion | PASS | `can_use_tool` tool_name AskUserQuestion; answered `{questions, answers}`; result "You picked blue." | first delta 571 ms |
| 4 | Interrupt after 2 s of text deltas, then a new turn | PASS | Receipt `{"still_queued":[]}`; result `error_during_execution`/`aborted_streaming` mid-count; the next turn returned "after-interrupt" | interrupt 3283 ms after prompt; result about 5 ms after the receipt |
| 7 | set_permission_mode / set_model / error subtype | PASS | See the table above | under 10 ms each |
| 5 | Exit, respawn with `--resume=<id>` | PASS | Same session_id. Asked for the first touched file, it answered "probe-ok.txt", with no permission prompt. | spawn→init reply 600 ms; spawn→system/init 602 ms; first delta 524 ms |

The probe used Bash `touch` instead of the brief's `echo probe-ok` (see
gotchas). The brief's resume question became "What was the FIRST file name I
asked you to create with touch?".

Cost: the final run cost about $0.010. A process's `total_cost_usd` is
cumulative: $0.0025 for proc 1, $0.0073 for proc 2, and about $0.0004 for the
resumed turn. All four probe runs together, plus the handshake-only checks,
came to well under $0.05.

## Gotchas

1. **`initialize` is not required for `can_use_tool`.** Scenario 0 got a
   prompt without one. Two things are required:
   - `--permission-prompt-tool stdio`
   - stdin stays open. The CLI exits once stdin closes and the turn ends.

   The docs' "hook workaround" exists for the Python SDK, whose stdin closes
   after a string prompt. A host that keeps the pipe open does not need it.
   `initialize` is still worth sending: it returns `models`, `commands`,
   `agents`, the account, `current_permission_mode`, `session_state` and
   `pending_permission_requests`, and it is how hooks, SDK MCP servers,
   `systemPrompt` and `supportedDialogKinds` get registered.
2. **`system/init` arrives only after the first user message, then again on
   every turn.** The probe saw 10 of them in one process. Do not treat it as
   a once-per-process handshake. For "ready", use the `initialize` reply.
3. **Pass `--permission-mode` explicitly.** This machine's
   `~/.claude/settings.json` has `defaultMode: auto`. Without the flag the
   session starts in `auto`, and the classifier answers instead of the host.
   `--permission-mode manual` is accepted and reports as `default`.
4. **Read-only commands never prompt.** In default mode, with user settings
   excluded, `echo …` ran with no `can_use_tool`. User settings also allow
   `Bash(echo:*)`. That is why the probe uses `touch`.
5. **The SDK's default `systemPrompt` is `""`.** With no `systemPrompt`
   option, `initialize` carries `systemPrompt: [""]`, which replaces Claude
   Code's prompt. The probe omits the field and keeps the CLI default. GitTerm
   should omit it too, unless it wants the bare-SDK behaviour.
6. **Isolate the child from the parent session.** When GitTerm (or the probe)
   runs inside a Claude Code session, `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT=cli`,
   `CLAUDE_CODE_SESSION_ID` and `CLAUDE_CODE_MESSAGING_*` are inherited. The
   SDK only fills `CLAUDE_CODE_ENTRYPOINT` when it is unset. The probe strips
   all of these.
7. **`total_cost_usd` and `modelUsage` are cumulative per process.** They
   appear to carry over across `--resume`. An interrupted turn added $0 and
   reported zero `usage`, so the counter under-reports aborted streaming.
8. **Interrupt timing.** Haiku emits thinking deltas first, sparsely, and then
   streams text fast. Time an interrupt from the first *text* delta. The
   receipt comes before the `result` (receipt first, `result` about 5 ms
   later in the final run).
9. **Interleaving.** `control_response` frames for host requests can
   interleave with turn frames. Match them by `request_id`, never by order.
10. **`--setting-sources=project,local` in the probe.** It isolates the probe
    from user hooks (SessionStart, Stop, Notification) and user allow rules.
    The `localSettings` permission write still works and applies within the
    same process. GitTerm proper should probably load all sources, which is
    the SDK default, so the user's rules apply.

## Open questions

- **Entrypoint value.** Should GitTerm set `CLAUDE_CODE_ENTRYPOINT=sdk-ts` as
  the SDK does, or its own value? The probe mirrors `sdk-ts`. Unknown which
  CLI behaviours depend on it.
- **`supportedDialogKinds` / `request_user_dialog`.** Which dialog kinds exist,
  and should the chat UI render any? The probe did not declare any, so none
  were sent.
- **Re-initialize semantics.** After a GitTerm restart, the
  `pending_permission_requests` re-delivery on re-`initialize` was not
  exercised. Re-attaching to a live process is not possible with a plain
  pipe anyway.
- **`--resume` while another process holds the same session.** Not tested.
- **Permission suggestions for other tools.** Edit/Write suggestions (e.g.
  `setMode acceptEdits`), and suggestions under `auto` mode with
  `decision_reason_type`, were not exercised.
- **`--include-hook-events`, `--replay-user-messages` and
  `--permission-prompts none`.** Not exercised. They may matter for the chat
  view.
- **Tokio `time` feature.** The example uses `tokio::time::timeout`. The
  crate's own `tokio` features do not list `time`; it is enabled through
  feature unification (warp, hyper, tonic). The app code should add `time`
  explicitly if it relies on it.
