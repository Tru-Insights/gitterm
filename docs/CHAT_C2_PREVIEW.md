# TRU-274 C2 preview boundary

C2 provides a closed foundation, not live sessions. GPUI composes its preview
before any store load/save. `GITTERM_GPUI_CONFIG_DIR` (absolute) or the separate
GPUI default supplies the root. Layout/catalog files live in `layout/`, desktop
state in `desktop/`, imported history in `history/`, snapshots in
`artifacts/snapshots/`, and Claude's namespace in `providers/claude/` (transcripts
in its `projects/`). Iced retains its existing compatibility behavior; no Iced
root discovery/runner was moved, and GPUI does not consume those ambient services.

The private authority validates conventional V3–V6/provider roots and active
`GITTERM_V6_CONFIG_DIR`, `GITTERM_V5_CONFIG_DIR`, `CLAUDE_CONFIG_DIR` and
`CODEX_HOME` overrides. It rejects overlaps in either direction, traversal,
dangling/looping links, and ancestors marked with `.gitterm-protected`.
Missing tails are resolved through existing ancestors. Concrete layout/catalog
writes are revalidated, use private temporary files and atomic replacement, and
require an exclusive writer lease. Imported hard links are replaced, not modified
in place. Stale `.writer` leases require explicit recovery; no PID is trusted or
signalled. Imported history has no mutation or resume authority.

This is a filesystem guard, **not a sandbox**. Concurrent filesystem replacement
can race checks and rename. No process-tree confinement has been demonstrated.
Consequently terminals (including restored sessions and the standalone CLI),
Git status/review commands, snapshot capture, agents, workers, helpers, process
restoration, history mutation and resume remain disabled. Fixture rendering and
explicitly scoped filesystem reads remain available. Git metadata validation
never runs Git, and acceptance does not authorize Git execution: repository
configuration may still name executable helpers. No real Claude probe runs.

## Upstream contracts checked for the specification

Verified 2026-11-19 against official Claude Code documentation:

- [Authentication](https://code.claude.com/docs/en/authentication):
  `CLAUDE_CONFIG_DIR` scopes ordinary configuration/history and claude.ai/API-key
  sign-ins. **Keyless Console sign-ins are outside that isolation.**
- [CLI reference](https://code.claude.com/docs/en/cli-reference): `auth status`
  produces JSON by default, exit 0 logged in / 1 not logged in. `configDirectory`
  requires v2.1.268+. `authMethod` has a documented finite set. The narrow parser
  only recognizes `claude.ai` and `none`; mismatches, unsupported methods,
  malformed/oversized output, exit inconsistency and timeout mean unavailable.
  Only documented `authMethod`, `configDirectory` and exit status are required;
  no undocumented login Boolean is guessed.
- The same reference documents `--setting-sources`, `--strict-mcp-config` and
  `--mcp-config`. C2 does **not** assume their interaction with auth subcommands
  or managed policy. The spec contains only `auth status`; no MCP credentials or
  endpoints are inherited. File/managed MCP initialization has no proven isolation,
  and real execution is unavailable. Before any real probe, verify the installed
  version and confine its entire process tree.
- [Settings](https://code.claude.com/docs/en/settings): `--settings '{}'` does not
  disable file settings, so it is not used. Managed policy remains applicable.

The child specification has an absolute executable, `env_clear`, a controlled
`PATH=/usr/bin:/bin`, isolated `HOME`, `XDG_CONFIG_HOME`, `TMPDIR`/`TMP`/`TEMP`,
explicit app/provider roots and a neutral cwd. No credential, proxy, loader,
helper or MCP environment variables are inherited. All returned probe states,
including a reported login, deny activation. There is no login/logout/update,
repair, fallback runner, credential copy or transcript rewrite.

Headless legacy shell/backend regression tests explicitly opt into a private
`cfg(test)` trusted-fixture switch. Closed-preview tests do not opt in. That
switch and unguarded fixture-store constructors do not exist in production.
