# TRU-274 Stage 5 PR 25b-core

Baseline head: `106280320a1027ddc162470bd63525503b06da6f`.
`tests/remote_connection_handlers.rs` includes the existing binary's private
handlers in a headless integration-test crate. Its three new tests inspect and
drop Iced tasks, never poll them. No credential resolution or network is used.
The imported binary's existing tests also run in this integration test crate.

Pinned before reducer implementation: defaults do not replace existing status;
non-forced connects deduplicate Connecting/Connected; forced connects replace
the handshake and clear last_connected; missing config produces an error;
completion uses saved name/endpoint, records timestamps, preserves capability
order/duplicates, passes home through unchanged, retains last_connected on a
failure completion, and never closes another remote's form. A follow-on baseline test pins matching
picker notification on success/failure and ignores another remote's completion. The existing
remote leaf parity tests pin wire requests and path/source behavior.

## 25b-main wiring inventory (baseline line anchors)

Direct connection arms:
- `Event::RemoteAgentConnect(remote_id)` — `src/main.rs:30564`.
- `Event::RemoteAgentHandshakeLoaded(snapshot)` — `src/main.rs:30567`.
- `Event::MachineReconnect(key)` — `src/main.rs:31490`.
- `Event::WorkspaceSelect(idx)` — 32703 (activation/non-forced connect).
- `Event::SlideScrolled(viewport)` — 32832 (workspace activation/connect).
- `Event::FolderPickerMachine(remote_id)` — 33142.
- `Event::RemoteWorkspaceBrowse(remote_id)` — 33139.
- `Event::RemoteChatIndexLoaded(remote_id, result)` — `src/main.rs:31969`,
  successful index requests a non-forced handshake.

Form adapter arms (credential handling stays host-owned):
- `Event::RemoteConnectOpen` — 32959.
- `Event::RemoteConnectClose` — 32966.
- `Event::RemoteConnectNameChanged(value)` — 32970.
- `Event::RemoteConnectEndpointChanged(value)` — 32975.
- `Event::RemoteConnectTokenChanged(value)` — 32980.
- `Event::RemoteConnectSubmit` — 32985.

Helpers/consumers to migrate with these arms: `request_remote_agent_handshake`
(18041), `ensure_active_remote_agent_connection` (23010),
`ensure_remote_agent_connection` (23023),
`register_remote_agent_connection_defaults` (23068), and startup call (21859).
Picker `open_folder_picker` (non-forced connect at 18226),
`folder_picker_home` (18135), `folder_picker_home_arrived` (18293), and
`submit_remote_connect_form` (22925) use the same connection owner.
Capability consumers: `remote_runs_claude_chat` (11457),
`remote_serves_phones` (11466), `remote_owns_claude_chats` (11518).
Successful completion refreshes files/git only if the active workspace owns
that remote. Every accepted completion notifies the matching picker, and
requests running chats (the helper gates on persistent-chat capability).
Close only the connect form whose saved remote id matches a successful reply.
Do not migrate legacy SSH `RefreshRemoteSessions` / `RemoteSessionsLoaded` or
session attach, phone pairing, chat streaming, or folder listing reducers here.

## Identity boundary

The existing protocol has no server UUID/key/fingerprint. Handshake `remote_id`
and `endpoint` are copied from client config, not reported by the server.
The reducer can bind only that exact configured identity; it cannot detect a
server replaced behind an unchanged endpoint. It must not treat agent name,
version, home, or capabilities as machine identity. Stronger server identity
requires a separate protocol and persistence design.

## New reducer and later integration requirements

`remote::state::State` accepts `Register`, `Ensure`, `HandshakeFinished`,
`Disconnect`, and `Remove`. It emits `Render`, stamped `Handshake`, stamped
`Completed { connected }`, or a safe `Rejected` reason. `Completed` is the
single adapter hook for the four completion behaviors listed above; it is
emitted on accepted success/failure only. Check `is_current` before delayed
host effect dispatch; use `accepts` for pending handshake completion admission.

The core owner intentionally tightens unsafe behavior without changing Iced:
old/duplicate/foreign completions are ignored, a configured id is pinned to its
endpoint for the owner's lifetime (even across Remove), and missing config is
a typed `NotConfigured` rejection rather than a placeholder connection with an
empty endpoint. Register conflicts leave the original connection and pending
attempt unchanged. A mismatched reply also leaves the valid attempt outstanding.
Force reconnect clears old capabilities/home until a new accepted handshake.
Protocol version, unknown capabilities, order and duplicates pass through.
Timestamps are supplied by the host; retry/reset behavior follows the baseline.

Before 25b-main, resolve the existing form's same-id endpoint-edit behavior
(`submit_remote_connect_form`, 22953). Do not weaken endpoint pinning to make
that retry succeed: a different configured machine requires a new saved id.
A host executing Handshake must compare the current config's id AND exact
endpoint with its effect stamp before resolving credentials. Never feed raw
transport/resolver errors to the reducer or logs: map them to `HandshakeFailed`.
`Identity::new` refuses URL userinfo, queries and fragments; labels/ids must come
from the host's non-secret saved identity fields, never the form's token input.
The reducer has no client config, token reference, credential resolver, transport
handle, local-path conversion, persistence, or logging.

Mechanical leaf edit: the unchanged `RemoteAgentHandshake` DTO moved from
`remote/client.rs` to the unconditional `remote/state.rs`, with a compatibility
re-export from `remote::client`. `remote/mod.rs` registers the module. Existing
host/source imports continue to compile; no facade, main, chat, work, GPUI,
manifest or lockfile edits are required.

R2 plan review: `8a985453-53ee-4d18-b313-e215783098a2` (GPT-6 Astra).
Its P2 corrections were applied before implementation: the completion hook
includes connect-form/picker/active-source/running-chat responsibilities; reply
protocol is passed through and unstructured failures use a safe generic type.

## Verification before commit

All exited 0 on rustup stable 1.99.1, using the isolated target directory,
4 build jobs, 4 test threads and disposable config roots:

- `cargo fmt --all -- --check`
- `cargo clippy -j 4 -- -D warnings`
- `cargo test -j 4 --workspace`
- `cargo test -j 4 -p gitterm-core --features runtime`
- `cargo test -j 4 -p gitterm-core --no-default-features`
- `cargo build -j 4`
- Clippy and workspace tests with `--features excalidraw`
- GPUI format check, `scripts/check-gpui-deps.sh`, `tests/check-gpui-deps.sh`

Root and GPUI `metadata --locked --format-version 1` also exited 0. The first
two handler pin tests passed before reducer implementation; the added picker
pin and all eight reducer tests pass in the final workspace/feature runs.
Existing ignored live tests were not enabled. No GUI or real remote was used.
`nice -n 10` was invoked; the sandbox denied its priority adjustment while
allowing the commands to run. Initial doc-lint, picker field-name and atomic
API deprecation failures were corrected before these successful final runs.
