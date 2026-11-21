# TRU-274 fresh local Claude preview

## Gate and activation

`chat/provider.rs::Provider::start` is the narrow C2 opening: a new local Claude
session in the selected root of an independent clone. The generic C2 `Agent`,
Worker, Helper, Resume, RestoreProcess and remote/phone routes remain closed.
History is read only and never supplies a resume ID. There is no production
fake-executable switch or environment bypass.

`chat/auth.rs::AUTH_STATUS_SCHEMA` is verified against the coordinator's isolated
Claude Code **2.1.327** `claude auth status --json` probe (`unverified: false`).
Launch requires `loggedIn: true`, `authMethod: "claude.ai"`,
`apiProvider: "firstParty"`, `subscriptionType: "pro"` or `"max"`, and
`configDirectory` equal to the isolated provider root. Team, Enterprise, unknown
or missing plans, other authentication methods/providers and signed-out status
refuse. Personal accounts have organization fields: `orgId` and `orgName` do
not determine account policy.

The complete signed-in schema also requires string `email`, `orgId`, `orgName`,
`planTier`, `projectsDirectory` and `loginExpiresAt`, plus boolean
`storedClaudeAiLogin`, `analyticsDisabled` and `consoleProfileSignInOffered`.
These fields are type-checked only; their values are never logged, retained or
included in errors. No org field must be empty. The existing exact-key rule
remains: **unknown or missing keys refuse**, as do incorrect field types.
The observed signed-out shape (including null `loginExpiresAt` and absent
account/plan fields) refuses. Synthetic fixtures cover both shapes and Pro/Max
policy. `Field::PersonalOnly(EmptyIndicator)` was replaced with explicit provider
validation and informational boolean/string types because empty org indicators
cannot express personal subscription policy. The test-only `FIXTURE_SCHEMA`
remains unavailable to app/environment selection. The only mechanical follow-on
outside the chat/doc scope is `tests/claude-process/preview_outcomes.py`: its
synthetic auth response now supplies the fixture provider field.

Every start holds the provider lease before running the checks. On the session
thread, under the unchanged nine-variable environment, it runs bounded, timed
`claude --help`, then `claude auth status --json`. Failed commands, malformed or
oversized JSON and uncertain account status refuse before a chat is spawned.
The help must advertise `--restricted`, `--strict-mcp-config`, `--safe-mode` and
`--permission-mode`; `default` or its documented `manual` alias must be listed.
The literal argument remains `default`.
[CLI reference](https://code.claude.com/docs/en/cli-reference#cli-flags).

Before help/auth and again before launch, any entry (including dangling symlinks)
or inspection failure at these local managed-policy locations refuses:

- `/Library/Application Support/ClaudeCode/` (covers `managed-settings.json`,
  `managed-settings.d/` and `managed-mcp.json`);
- `/Library/Managed Preferences/` (conservative whole-directory refusal);
- `/Library/Preferences/com.anthropic.claudecode.plist` (preferences domain);
- `/etc/claude-code/` (Linux system directory; conservatively also checked on macOS).

Source: [managed policy locations](https://code.claude.com/docs/en/managed-settings#where-each-mechanism-stores-the-policy).
`chat/managed.rs` accepts a private temp-root parameter for tests; production
always uses `/`. Safe mode preserves managed hooks. The accepted boundary is
therefore **personal Pro/Max only plus no local managed policy**, together with
restricted tools. Team/Enterprise server-delivered policy is refused by the
auth gate. No claim is made that safe mode alone disables managed hooks.

## Launch and writer boundary

Exact preview argv (after the resolved absolute Claude executable):

```text
--output-format stream-json --verbose --input-format stream-json --permission-prompt-tool stdio --include-partial-messages --no-chrome --permission-mode default --restricted --strict-mcp-config --safe-mode
```

When the preview task host (H5b B4, `task_host.rs`) is running, the chat
launches with `LaunchFlags::PreviewTools` instead, and two arguments sit
between `--permission-mode default` and the fence (H5b B6):

```text
--mcp-config={"mcpServers":{"gitterm_tasks":{"headers":{"Authorization":"Bearer ${GITTERM_V6_TASK_MCP_TOKEN}"},"type":"http","url":"http://127.0.0.1:<port>/mcp?caller=<session uid>"}}} --allowedTools=mcp__gitterm_tasks
```

The caller is the pane's `session_uid`, fixed in `ChatPane::new` before the
gate runs. The token is only in the child environment
(`GITTERM_V6_TASK_MCP_TOKEN`, beside `GITTERM_V6_TASK_MCP_URL`), added to the
fixed environment above; a key that would replace a fixed one refuses the
launch. Only the task server is attached: the browser server waits for its
server-side preview gate (B7). Without a running host the chat launches with
the plain argv above and logs why once.

Core `harness/claude.rs::LaunchFlags::FreshPreview` supplies only the three fence
flags and validates the fixed session config before any guideline writes.
`LaunchFlags::Standard` is the default; Iced's argv is byte-identical. Both are
pinned by tests. No bypass flag, `--add-dir`, `--bare`, tools override, MCP config
other than the task server, or resume can enter a preview launch. Host writers stay disabled: snapshot
capture/`git add`, snapshot publication and close maintenance (`None` facts root
plus no-op recorder), generated guideline/context files and reinjection, wire
logs, injected browser MCP, Chrome, worker/delegation/helper launch,
remember-permission persistence, remote/phone and resume.

The coordinator accepts that `--restricted` removes Bash/code-running tools and
confines file tools to the working directory, strict MCP loads no implicit
servers, and safe mode suppresses user/project customizations. With managed
writers excluded and host helpers disabled, the remaining CLI file tools are
the writer boundary. The coordinator must demonstrate it with the real probe
before using a chat. No optional deny-rule JSON is added.

The clone validator runs no Git. It refuses plain/nested folders, linked
worktrees, `.git` symlinks, shared common directories/registered worktrees,
object alternates and aliased/missing core metadata, protected roots and overlap
with app/provider state. Use an exclusively owned disposable `git clone
--no-local`. This does not prove clone provenance, exclude every hardlink, or
prevent concurrent path replacement; keep all trial data exclusively owned.

`chat/lease.rs::ProviderLease` holds an exclusive nonblocking OS lock on the
persistent `<provider>/.writer` inode. It checks private mode, regular file,
single link and opened/path device+inode identity after locking. A second chat
or preview instance refuses. The file is never unlinked. Marker/PID contents
are never authority and no PID is signalled: the kernel releases a dead owner's
lock, so stale markers need no PID-based recovery. A shared lease is held by
both the no-op recorder and `OwnedRuntime`. Close transfers the latter into
`RuntimeShutdown`; it drops only after `Session::completed()` joins the runtime
thread through process reaping and cleanup. Quit drains that queue synchronously
before GPUI's timeout. Tests cover second-owner refusal, stale marker reuse,
symlink/hardlink refusal and retention through slow shutdown/drain.

## Delivery outcomes

Production `ChatPane::new` calls `Provider::start` → `Session::spawn_runtime` and
`install_runtime`. `Controller::dispatch` maps each C7 `Prepared::id()` to a
session-local `CommandCorrelation` and calls `send_correlated`. The one receiver
stamps and gates **all** ingress before pane mutation, then passes ordered
`RuntimeEvent::CommandOutcome` through `apply_runtime_outcome` →
`ChatAdapter::delivery` → exact receipt. Labels are Sending, Written, Rejected
and Uncertain. Written means stdin delivery, not provider acknowledgement.
Resolved answers remain Resolved when a later Written event arrives. Outcomes
are local, never persisted to provider JSONL or phone payloads. Fake children
cover send, permission denial, question response, interrupt, rejected writes
and death during a partial write.

## Coordinator-only sign-in, schema probe and confinement probe

These commands are for the coordinator **after review**, never run by the
worker. Close all preview chats and probe processes first. Use the default
non-sample preview root below; launch the GPUI shell with this same explicit
`GITTERM_GPUI_CONFIG_DIR` later. On macOS only, the child home's
`Library/Keychains` links to the process user's `~/Library/Keychains` so Security
can find the login keychain. All other child-home paths stay substitute, and
`CLAUDE_CONFIG_DIR` still selects the preview's separate credential entry.
Use a personal Pro/Max account; never select a Team/Enterprise organization.
Never click "Reset To Defaults" in a macOS "Keychain Not Found" dialog.

```sh
P="$HOME/.config/gitterm-gpui"
CLAUDE="$HOME/.local/bin/claude"  # resolved installed CLI on the coordinator host
C="$HOME/gitterm-chat-c8-disposable-clone"  # independent, exclusively owned clone
REPO="$PWD"                  # this checkout, before changing directory
# If C does not exist, create it once: git clone --no-local "$REPO" "$C"
umask 077
if [ "$(uname -s)" = Darwin ]; then
  # Run in the parent environment, before HOME is substituted. Never overwrite.
  python3 - "$P/child/home" <<'PY' || exit 1
from pathlib import Path
import sys
library = Path(sys.argv[1]) / "Library"
if not library.is_absolute() or library.resolve() != library:
    raise SystemExit(f"Refusing aliased directory or ancestor: {library}")
library.mkdir(mode=0o700, parents=True, exist_ok=True)
if library.is_symlink() or not library.is_dir() or library.stat().st_mode & 0o077:
    raise SystemExit(f"Refusing aliased or non-private directory: {library}")
link = library / "Keychains"
target = Path.home() / "Library/Keychains"
try:
    link.symlink_to(target, target_is_directory=True)
except FileExistsError:
    if not link.is_symlink() or link.readlink() != target:
        raise SystemExit(f"Refusing existing keychain path: {link}")
PY
fi
mkdir -p "$P/providers/claude" "$P/child/neutral" "$P/child/home" "$P/child/config" "$P/child/tmp"
cd "$P/child/neutral"
# (a) One-time isolated sign-in. Then /login; exit the CLI after signing in.
env -i "CLAUDE_CONFIG_DIR=$P/providers/claude" "GITTERM_GPUI_CONFIG_DIR=$P" \
  "GITTERM_V6_CONFIG_DIR=$P/desktop" "HOME=$P/child/home" "XDG_CONFIG_HOME=$P/child/config" \
  PATH=/usr/bin:/bin "TMPDIR=$P/child/tmp" "TMP=$P/child/tmp" "TEMP=$P/child/tmp" \
  "$CLAUDE" --restricted --strict-mcp-config --safe-mode --permission-mode default
# (b) Inspect locally. Do not paste private output into commits or public reports.
env -i "CLAUDE_CONFIG_DIR=$P/providers/claude" "GITTERM_GPUI_CONFIG_DIR=$P" \
  "GITTERM_V6_CONFIG_DIR=$P/desktop" "HOME=$P/child/home" "XDG_CONFIG_HOME=$P/child/config" \
  PATH=/usr/bin:/bin "TMPDIR=$P/child/tmp" "TMP=$P/child/tmp" "TEMP=$P/child/tmp" \
  "$CLAUDE" auth status --json
# Schema is verified for 2.1.327; changed/unknown fields still refuse.
# Re-verify the complete schema and review before supporting a different shape.
# (c) Run only after confirming personal auth and absence of managed policy.
cd "$REPO"
scripts/preview-claude-confinement-probe.sh "$P/providers/claude" "$C"
```

These commands keep the complete allowlist: `CLAUDE_CONFIG_DIR`,
`GITTERM_GPUI_CONFIG_DIR`, `GITTERM_V6_CONFIG_DIR`, `HOME`, `XDG_CONFIG_HOME`,
`PATH`, `TMPDIR`, `TMP`, `TEMP`. No inherited API/OAuth tokens, helpers, parent
session IDs, cloud-provider toggles, proxies, loader overrides or MCP credentials.
The provider path is literally `$HOME/.config/gitterm-gpui/providers/claude` for
these commands; the app rule is `<validated selected preview root>/providers/claude`.

The existing tool-layer probe uses the same roots/env and fence, with the sole
probe-only mode `acceptEdits`, plus `-p --output-format stream-json --verbose
--permission-prompts none <case-prompt>`. It is a coordinator diagnostic, not an
app activation receipt or cached authorization. It checks synthetic and literal
protected sentinels, outside-directory symlink, clone `.claude/settings.json`,
`.git/config`, missing shell tools and a positive in-clone edit. Actual Write
input and authoritative correlated permission denials must agree with independent
disk checks. Auth/model/parse/IO failures are inconclusive failures. Cleanup
removes only owned fixtures; escaped sentinels remain evidence. Offline test:
`python3 -B tests/preview_claude_confinement_probe_test.py`.

Before daily use, verify fresh ID/transcript placement, text/tools, allow/deny,
questions, stop, hide/dock retention, close/shutdown/reap and read-only history
after restart, with unchanged production sentinels. Resume, workers, images,
dictation, remember and remote remain unavailable. No real CLI session, login,
GUI, production-state inspection, push or merge is part of worker verification.

## Review and verification record

R2 approach consultation `a5f8b0f9-f9d3-4427-a96e-713678343768` timed out without
findings; coordinator explicitly waived a rerun and resolved scope/account
choices. R2 code review `3991a38c-e37c-4d36-aa7c-d89276abddf3`
(`gpt-6-astra`, high) confirmed this task worktree, clean commit `1309d39`,
and the full diff against `767df25`. Verdict: correct, no findings or concrete
blockers in the gates, lease lifecycle, outcome ordering or public-data handling.
The reviewer inspected verification logs without rerunning tests. No review
fixes were requested; the live confinement trial remains a coordinator
prerequisite. The subsequent 2.1.327 schema verification is recorded above; the
new activation-gate review is recorded in the durable task handoff. GitTerm
refused the explicit checkout argument; the request without checkout ran in the
correct cwd/head as confirmed by
`delegation_get`. Prior C8-confine
review `9f3fe814-9fae-4fa2-92fc-7a9f8bab6ac2` closed its four probe corrections;
it did not approve this new activation gate.

Mechanical edits outside the provider/shell scope: one default `Launch.flags`
entry in core `chat/runtime_tests.rs`, and the two Iced `src/harness/claude.rs`
Launch literals, as explicitly authorized. No other core chat or main.rs edits.

Continuation verification uses `scripts/cargo.sh`,
`CARGO_TARGET_DIR=/private/tmp/gt-target-chatc8o`, `RUST_TEST_THREADS=4`,
`nice -n 10`, `-j 4`, one build at a time. Exit 0:

- Root `fmt --all -- --check`, `clippy -j 4 -- -D warnings`,
  `test -j 4 --workspace`, `test -j 4 -p gitterm-core --features runtime`,
  and `build -j 4`.
- GPUI `fmt --all -- --check`, `clippy --workspace --locked --all-targets -j 4
  -- -D warnings`, and `test --workspace --locked -j 4` (275 tests plus doc tests).
- `scripts/check-gpui-deps.sh`, `python3 -B
  tests/preview_claude_confinement_probe_test.py` (14 tests), `git diff --check`.

Sandbox artifacts: `nice` cannot set priority in sandboxed commands, but Cargo
runs; the initial headless GPUI tests could not execute `/bin/ps` in two existing
reaping tests. The complete authorized headless rerun passes; no test was
skipped or weakened for that restriction. Existing root test warnings and
upstream `block` future-compatibility warning are unchanged. Optional feature,
release/bundle and live/manual checks are outside this task's acceptance set.
