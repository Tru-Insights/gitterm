# TRU-274 C2 preview boundary

C2 adds a guarded foundation for new chat services while preserving today's
stage 1–3 GPUI app. Terminals (new and restored), Git status jobs, review/diff,
`--terminal` and `--review-repo` keep their existing execution authority. These
legacy routes are not confined by C2 and can run normal shell/Git configuration.
Standalone terminal/review routes do not compose preview roots or
validate provider overrides. Saved workspace admission keeps its existing error
handling, including a saved folder that has since been removed. The independent
C2 Git-metadata validator still executes no Git commands; it is not an admission
gate for legacy workspaces.

Before shell persistence, `GITTERM_GPUI_CONFIG_DIR` (absolute) or
`~/.config/gitterm-gpui` supplies the selected root; `--sample` selects its
`sample/` child. Existing `local-workspaces.json`, numeric `layout-<n>.json` and
`layout-workspace-<16hex>.json` stay directly in that selected root. There is no
new `layout/` subfolder, migration or import. New-only namespaces remain in
`desktop/`, `history/`, `artifacts/snapshots/`, and `providers/claude/` (transcripts
in `projects/`). Iced's paths and compatibility behavior are unchanged.

Every catalog/layout save retains the private authority's validation of
conventional V3–V6/provider roots and active `GITTERM_V6_CONFIG_DIR`,
`GITTERM_V5_CONFIG_DIR`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` overrides. It rejects
overlaps in either direction, traversal, dangling/looping links, aliases at
concrete destinations, and ancestors marked with `.gitterm-protected`. Missing
tails resolve through existing ancestors. Only the established direct-child
layout/catalog filenames have write authority; history/provider files and
unrelated JSON do not. Private temporary files are synced and atomically
replaced, so imported hard links are replaced without modifying their source.
Existing root/parent permissions are preserved, including old `0755` config
roots. Missing parents are created privately, with concurrent creation checked.

Each app context has an ordered background save queue. An OS-exclusive lease
on the regular `.writer` file surrounds each save; UI callbacks never wait for
that lock. A second instance starts and serializes its saves through the same
lock. This preserves atomic last-writer-wins behavior, not merging between stale
in-memory layouts/catalogs. The marker file is never truncated, rewritten or
unlinked: its contents and copied PIDs convey no authority. Process exit/crash
releases the kernel lock automatically, so an old regular marker needs no manual
recovery. Symlinked/hard-linked/replaced lock files fail closed. Unix device/inode
identity is checked after acquisition and before replacement; unsupported
platform identity fails closed. Quit rejects new submissions, drains queued
saves and closes shared writer authority even if store handles survive. Save
failures are surfaced in the shell; late writes fail closed.

This is a filesystem guard, **not a sandbox**. Concurrent filesystem replacement
can race checks and rename. No process-tree confinement has been demonstrated.
Only the new pieces remain closed: agents, delegated workers/helpers, snapshot
capture, real Claude probing/session activation, history mutation and resume,
and generic process restoration. Restoring an existing terminal is normal
terminal behavior, not generic agent/process resume. Fixture rendering remains
available. No real Claude probe runs, and no imported history has mutation or
resume authority.

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

Headless shell/backend regression tests use the production capability policy,
including guarded app-store tests for terminal restore, status dispatch and review
loading. No trusted-fixture capability bypass exists. Unguarded fixture-store
constructors remain test-only and do not exist in production.
