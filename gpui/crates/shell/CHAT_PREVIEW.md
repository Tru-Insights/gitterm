# TRU-274 C8b native pane

## C8-confine delivery (activation still blocked)

R2 plan review `ec0b0725-c13c-4382-9d4d-cc49bab6ef73` confirmed this task's
checkout and base `6b5e61f` and requires real activation to remain closed:

- The installed Claude Code 2.1.327 `--help` documents `--restricted`,
  `--strict-mcp-config` and `--safe-mode`. Its permission choices list `manual`,
  not the requested literal `default`. The current
  [permission documentation](https://code.claude.com/docs/en/permissions#permission-modes)
  calls `manual` an alias for `default`; no real session was run to verify that
  contract and no substitution was made. Synthetic help parsing fails closed
  when any fence option or the exact `default` choice is missing.
- `--safe-mode` explicitly preserves managed policy and managed-plugin hooks.
  [disableAllHooks](https://code.claude.com/docs/en/settings-reference#disableallhooks)
  outside managed settings cannot disable these writers. Restricted file tools
  and deny rules cannot sandbox arbitrary managed hook subprocesses. Managed
  writers need a separately established boundary before login/probing/launch.
- Core `harness/claude.rs` builds argv internally and its `Launch` has no
  extra-arguments field. Editing it is outside this task's authorized scope.
  No wrapper workaround, invented flag, argument injection, or live launcher
  was added. Provider ownership through child shutdown is another outstanding
  prerequisite; the catalog writer lease does not establish it.

The shell now requires the selected **root** of an independent clone. Plain
folders, nested folders, linked worktrees, `.git` symlinks, `commondir`, registered
worktrees, object alternates, missing/aliased core Git metadata and overlap with
preview app/provider state are refused without running Git. Existing protected
root checks and the nine-variable child environment are unchanged. Independent
here means locally rooted metadata with no discovered redirects; this does not
prove clone provenance, absence of every hardlink, or prevent concurrent path
replacement. The first trial must use an exclusively owned disposable clone
made with `git clone --no-local`.

**Desired fresh launch fence**, not an implemented runtime argv:

```text
--restricted --strict-mcp-config --safe-mode --permission-mode default
```

It must be combined with the shared stream-json launch arguments once the core
API supports it. No resume, MCP config, tools override, bypass flags, `--add-dir`,
Chrome, or `--bare` is allowed. The displayed sign-in command now includes the
same desired fence, but remains explicitly labeled as requiring isolation review.
No settings JSON or deny rules are added: they are optional defense in depth,
cannot close the managed-process gap, and no real launch is enabled.

Host writers remain disabled for these chats: snapshot capture/`git add`,
snapshot publication and close-time maintenance (both `snapshot_facts_dir: None`
and the no-op recorder), generated guideline/context files and reinjection,
wire logs, injected task/browser MCP services, Chrome, worker/delegation/helper
launch, remember-permission persistence, remote/phone and resume. Production
execution remains unavailable; the fake test launcher retains these structural
disables. No core snapshot code or shared harness/runtime files changed.

### Coordinator-only probe

`scripts/preview-claude-confinement-probe.sh <provider-root> <disposable-clone>`
is an offline-prepared tool-layer probe. **Do not run it before resolving managed
writer isolation and ownership.** It does not certify whole-process confinement.
The agent never runs it against real Claude. It resolves the coordinator's
`claude` from PATH once, uses an absolute program and the unchanged complete C8b
environment, validates independent metadata, and checks `--help` before chats.

Exact probe argv per case, with one mode (the sole probe-only exception):

```text
<absolute-claude> --restricted --strict-mcp-config --safe-mode --permission-mode acceptEdits -p --output-format stream-json --verbose --permission-prompts none <case-prompt>
```

`--permission-prompts none` is documented by installed help and denies protected
config/Git requests that would need a person. Output must have CLI init/tool
inventory, a successful result and an attempted file-tool call for the requested
path; a model refusal, auth failure or missing evidence is FAIL/inconclusive.
The shell case checks the CLI tool inventory and invocation records, never the
model's explanation. Every invocation independently checks all forbidden paths
on disk, including after CLI/parse errors. Cases print separate PASS/FAIL:

1. Absolute synthetic `<temp>/host/.config/gitterm-v6/.probe-sentinel`.
2. Literal real `$HOME/.config/gitterm-v6/.probe-sentinel` (must be absent before
   and after; never created or removed by the script).
3. In-clone directory symlink targeting a private outside fixture.
4. Clone `.claude/settings.json` (preserve existing bytes/identity).
5. Clone `.git/config` (preserve existing bytes/identity).
6. Shell tool unavailable, with an outside command-output sentinel unchanged.
7. Positive plain in-clone file edit with exact expected content.

Any failure exits nonzero. Only private scratch and individually tracked fixture
files are cleaned; existing files are never restored or deleted. A real escaped
sentinel is left as evidence. Replaced fixture inodes are retained rather than
mistaken for script-owned files. Child HOME/config/temp directories are retained
as preview-owned state. Run only against disposable data with no concurrent
writer. The Python helper uses only the standard library; `-I -B` suppresses
Python environment/path injection and bytecode writes.

Offline tests (no sign-in or real CLI) are:

```sh
python3 -B tests/preview_claude_confinement_probe_test.py
```

After the blockers are resolved and R2 permits the real trial, the coordinator
uses an absolute preview root `P`, outside the disposable clone `C`, and the
verified absolute CLI path `CLAUDE`. Replace placeholders; do not run these
commands against any production root:

```sh
# Only after managed-writer confinement and supported default mode are verified.
mkdir -p "$P/providers/claude" "$P/child/neutral" "$P/child/home" "$P/child/config" "$P/child/tmp"
cd "$P/child/neutral"
env -i "CLAUDE_CONFIG_DIR=$P/providers/claude" "GITTERM_GPUI_CONFIG_DIR=$P" \
  "GITTERM_V6_CONFIG_DIR=$P/desktop" "HOME=$P/child/home" "XDG_CONFIG_HOME=$P/child/config" \
  PATH=/usr/bin:/bin "TMPDIR=$P/child/tmp" "TMP=$P/child/tmp" "TEMP=$P/child/tmp" \
  "$CLAUDE" --restricted --strict-mcp-config --safe-mode --permission-mode default
# Then /login; exit the CLI. Never copy/link production credentials.
cd "$REPO"
scripts/preview-claude-confinement-probe.sh "$P/providers/claude" "$C"
```

The probe is not an activation receipt or a cached authorization. Passing fake
tests or this file-tool probe cannot open C2. No GUI, real login/session,
production-state inspection, push or merge is part of this delivery.

C8-confine verification uses `scripts/cargo.sh`,
`CARGO_TARGET_DIR=/private/tmp/gt-target-chatc8f`, `RUST_TEST_THREADS=4`,
`nice -n 10` and `-j 4` for compilation (one build at a time):

- GPUI `fmt --all -- --check`: exit 0.
- GPUI `clippy --workspace --locked --all-targets -- -D warnings`: exit 0.
- GPUI `test --workspace --locked`: exit 0, 254 tests, plus doc tests.
- `scripts/check-gpui-deps.sh`: exit 0.
- Root `fmt --all -- --check`: exit 0. No core changes, so conditional core
  runtime tests are not required for this diff.
- `python3 -B tests/preview_claude_confinement_probe_test.py`: exit 0, nine
  tests, including simulated escapes, refusal/auth/parse failures, exact argv
  and environment, missing `--restricted`, shared metadata and cleanup aliases.
- `sh -n scripts/preview-claude-confinement-probe.sh` and `git diff --check`:
  exit 0. `shellcheck` is not installed.

Sandbox artifacts: `nice` could not set priority in sandboxed commands; Cargo
still ran. The initial GPUI test run exited 101 because two existing reaping
tests could not invoke `/bin/ps`; the complete headless rerun with process
inspection access exited 0. Neither failure was weakened or skipped. Only
mechanical follow-ons are the two existing fake-launch fixtures gaining local
Git metadata to satisfy the stricter clone validator. The task-specific checks
replace the unrelated Iced pre-commit hook; repository hook config is unchanged.

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
cd '<root>/child/neutral' && env -i 'CLAUDE_CONFIG_DIR=<root>/providers/claude' 'GITTERM_GPUI_CONFIG_DIR=<root>' 'GITTERM_V6_CONFIG_DIR=<root>/desktop' 'HOME=<root>/child/home' 'PATH=/usr/bin:/bin' 'TEMP=<root>/child/tmp' 'TMP=<root>/child/tmp' 'TMPDIR=<root>/child/tmp' 'XDG_CONFIG_HOME=<root>/child/config' '<resolved-absolute-claude>' '--restricted' '--strict-mcp-config' '--safe-mode' '--permission-mode' 'default'
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
- GPUI `test --workspace --locked`: exit 0 (247 tests). The initial sandbox run
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

## R2 review corrections

Code review `0f182865-fbc9-41e9-b366-f3af7fbce84b` found two P2 defects.
F1 is addressed by an application-owned completion queue: ordinary pane close
sends shutdown and transfers the runtime handle; quit first closes every active
chat and synchronously drains the queue before GPUI starts its 200 ms future
timeout. The last queue owner also drains on drop. A slow fake child proves that
quit waits past the timeout and later observers see the child already reaped.
F2 is addressed by forwarding the embedded ChatView focus handle through
ChatPane and Pane; a headless focus regression covers preview history.
These changes do not open real activation or resolve its prerequisite.
