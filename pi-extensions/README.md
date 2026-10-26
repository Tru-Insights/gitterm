# pi extensions for GitTerm

GitTerm attaches its MCP servers to Codex and Claude Code per process
(`task_mcp::configure_task_command`). pi has no MCP client of its own, so it
gets the same servers through the `gitterm-mcp` extension here, which reads
the `GITTERM_V5_TASK_MCP_*` / `GITTERM_V5_BROWSER_MCP_*` variables GitTerm
exports into every terminal and registers the tools via `pi-mcp-adapter`.
Outside GitTerm the variables are absent and the extension does nothing.

## Install (global, once)

```sh
cp -R pi-extensions/gitterm-mcp ~/.pi/agent/extensions/
cp pi-extensions/gitterm-mcp.ts ~/.pi/agent/extensions/
(cd ~/.pi/agent/extensions/gitterm-mcp && npm install && npm run check)
```

pi discovers `~/.pi/agent/extensions/*.ts` automatically. Verify inside a
GitTerm task session with `/tools` — `task_get`, `task_update_handoff`, etc.
should be listed.

This supersedes the older hand-maintained `gitterm-browser` extension, which
only knew the V4 browser variables.

## Skills

`skills/resume-foreign-session/` continues work started in another harness
(Codex, Claude Code, or an earlier pi session) after a provider outage or a
deliberate switch: a finder script locates recent transcripts for the current
directory, and the skill distills them (objective, decisions, state, next
step) instead of replaying them. Install:

```sh
cp -R pi-extensions/skills/resume-foreign-session ~/.pi/agent/skills/
```

Invoke in pi with `/resume-foreign-session` (or just describe the takeover;
the skill description triggers on it).
