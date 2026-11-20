# TRU-274 C8b native pane

The local shell's More menu opens **New Claude chat** and **Claude preview
history**. The workspace owner retains each chat entity across dock changes.
Saved history is a `preview_chats` field in the existing guarded
`local-workspaces.json` catalog. Saved IDs/events only produce read-only views;
they never establish runtime authority or supply `--resume`. Catalog writes use
the existing serialized, atomic writer (concurrent instances retain its existing
last-writer-wins behavior).

## Activation status

**Real login probing and chat activation remain closed.** R2 plan consultation
`19f6332a-ee13-438a-9de6-856932c65c0e` identified an unresolved prerequisite in
the approved design: demonstrate a process-tree write boundary, including
settings, hooks, MCP and tool subprocesses. Provider paths and environment
isolation alone do not establish it. No production fake-executable switch or
capability bypass exists. General agents, workers/helpers, remote/phone, imported
sessions and all resume paths remain disabled. The probe executor and fresh
core `Session` launcher currently compile only in tests.

The fixed provider rule is `<validated GITTERM_GPUI_CONFIG_DIR>/providers/claude`
(or the shell's selected sample root). Relative, protected, overlapping and
aliased roots fail. Root/provider paths are checked again before private
creation or fake launch; workspace Git common directories are checked without
executing Git. The sole core change exposes the existing checked private-directory
creator. This is a filesystem guard, not a sandbox; concurrent path replacement
remains a known limitation.

The existing core `ProbeSpec` supplies a complete environment allowlist:
`CLAUDE_CONFIG_DIR`, `GITTERM_GPUI_CONFIG_DIR`, `GITTERM_V6_CONFIG_DIR`, `HOME`,
`XDG_CONFIG_HOME`, `PATH`, `TMPDIR`, `TMP`, `TEMP`. HOME/config/temp point inside
the preview, PATH is `/usr/bin:/bin`, and execution uses `env_clear`. Nothing
else is inherited, including all Anthropic/Claude API keys, OAuth/auth tokens,
helpers, cloud-provider credentials/toggles, parent session identities, proxies,
loader overrides or GitTerm MCP credentials. No credentials are copied, linked
or read from production. No GitTerm MCP servers are injected; that does not
claim to suppress MCP loaded from CLI settings.

Status uses the documented `auth status` subcommand, JSON by default. Collection
is bounded to 16 KiB and timed out. Only exit 0 + `authMethod: claude.ai` + an
exact matching `configDirectory` reports login; exit 1 + `none` reports signed
out; every other result is unavailable. Even a reported login denies activation.
The command shown is generated from that same environment, with shell quoting:

```sh
cd '<root>/child/neutral' && env -i 'CLAUDE_CONFIG_DIR=<root>/providers/claude' 'GITTERM_GPUI_CONFIG_DIR=<root>' 'GITTERM_V6_CONFIG_DIR=<root>/desktop' 'HOME=<root>/child/home' 'PATH=/usr/bin:/bin' 'TEMP=<root>/child/tmp' 'TMP=<root>/child/tmp' 'TMPDIR=<root>/child/tmp' 'XDG_CONFIG_HOME=<root>/child/config' '<resolved-absolute-claude>'
# Then /login
```

The pane labels sign-in as awaiting isolation review. It never runs this command.
The fake launch has no resume, Chrome, injected MCP, wire log, snapshots or
remember-permission writes. Sends, stop and answers go through `ChatAdapter`;
stream ingress is stamped before presentation and persistence. Channel acceptance
remains **delivery unconfirmed**. `apply_runtime_outcome` is the one C8a integration
point; no outcome types or write receipts were guessed.

## Coordinator checks before a real trial

1. Resolve/review the process-tree/settings/MCP boundary and demonstrate denied
   absolute writes, hooks and shell subprocesses against synthetic protected
   roots, plus allowed writes in a disposable independent clone. Keep host
   snapshot/helper execution disabled until covered.
2. Verify the installed CLI supports config-scoped authentication and the
   `configDirectory` status field (documented since v2.1.268). Verify no fallback
   to production login, helpers, Console/third-party auth or service endpoints.
3. Establish lifetime provider ownership before real execution. Current catalog
   locks serialize saves, not provider/session ownership. Never promote saved
   IDs or copied transcript basenames into live authority.
4. Only after those gates: sign in once using the displayed isolated command;
   inspect the returned root/method; create a fresh chat in the disposable clone.
   Check new ID/transcript placement, text/tools, permission allow/deny, questions,
   stop, dock/hide survival, close/shutdown/reap and read-only history after restart.
5. Compare synthetic production sentinels before/after. Do not launch/control the
   running GitTerm instance or share its writable provider/config directories.
6. Integrate C8a's actual correlated outcomes at the named hook; until then do
   not label channel acceptance as Written. Images, dictation, review/delegation,
   workers and remember permissions remain visibly unavailable.

## Automated verification

All commands use `scripts/cargo.sh`, `CARGO_TARGET_DIR=/private/tmp/gt-target-chatc8b`,
`RUST_TEST_THREADS=4`, `nice -n 10`, and `-j 4` for compilation. One build runs at
a time. The sandbox denies setpriority, but Cargo still runs successfully.

- GPUI `fmt --all -- --check`: exit 0.
- GPUI `clippy --workspace --locked --all-targets -- -D warnings`: exit 0.
- GPUI `test --workspace --locked`: exit 0 (245 tests). The initial sandbox run
  denied `/bin/ps` in an existing terminal test; the final headless rerun has
  process-inspection access. The stricter root check also required one existing
  test fixture to use concrete `/private/tmp` instead of the `/var` alias.
- `scripts/check-gpui-deps.sh`: exit 0.
- Root `cargo fmt --all -- --check`: exit 0.
- Root `cargo test -j 4 -p gitterm-core --features runtime`: exit 0 (616 unit
  tests and five integration tests passed; one existing unit test ignored).

No real Claude invocation, authentication, GUI launch or production-state access
was performed. No screenshot/manual live trial is claimed. The unrelated Iced
build/test pre-commit hook is replaced for this commit by the task's explicit
GPUI/core acceptance commands; repository hook configuration is not changed.
