# Remote GitTerm Agent Architecture

**Status:** Planned. Architecture decision captured 2026-07-03.

## Decision

Remote workspaces should be backed by a long-running `gitterm-agent` process on
the remote machine. GitTerm desktop should not mirror local behavior by adding
one-off SSH/tmux branches throughout the UI.

The durable direction is:

```text
GitTerm desktop
  -> WorkspaceBackend abstraction
      -> LocalBackend
      -> RemoteAgentBackend
          -> gRPC over HTTP/2
              -> reachable endpoint (Tailscale recommended, not required)
                  -> gitterm-agent on the remote machine
                      -> filesystem
                      -> git
                      -> session runtime for shell/codex/claude/pi/custom commands
```

Tailscale is the recommended reachability layer for personal machines, but the
agent must not depend on Tailscale. Any secure path that makes the endpoint
reachable should work: Tailscale, LAN, WireGuard, private VPC, SSH tunnel,
reverse proxy, or localhost for development.

SSH should remain useful for bootstrap/install/emergency repair, not the normal
runtime API.

## Why

The current remote prototype proved feasibility, but it also shows the danger:
remote support quickly turns into scattered checks like `if remote { ... }` in
Git, Files, Agent, terminal, workspace persistence, and tab activation paths.

That will not scale to the actual product shape:

- Remote workspaces need to mirror local behavior, not expose a smaller special
  mode.
- Local supports multiple shell/codex/claude/pi/custom sessions; remote should
  too.
- Files, Git status, branches, worktrees, diffs, file preview, and agent
  launchers should all work through the same UI surfaces.
- Parsing terminal/tmux output in the desktop app would be fragile and would
  leak session implementation details across the codebase.

The remote agent makes "local vs remote" an implementation detail of a backend,
not a condition spread throughout the UI.

## Transport Decision

Use gRPC over HTTP/2 between GitTerm desktop and `gitterm-agent`.

Recommended transport stack:

```text
gRPC / protobuf contracts
HTTP/2 streaming
Tailscale or another private network for reachability
Bearer pairing token for app-level authorization
```

Reasons:

- Bidirectional streaming is first-class, which maps well to terminal attach.
- Server-side streaming maps well to watches for filesystem, git, and session
  status changes.
- HTTP/2 multiplexing avoids inventing stream IDs over raw sockets/WebSockets.
- Protobuf gives stable typed contracts instead of ad hoc JSON shapes.
- Reconnect behavior is clear: reconnect, handshake, list sessions, reattach.
- Transport is portable because the endpoint is just a network endpoint.

Security should be layered:

- Network reachability is not authorization.
- Store a per-remote pairing token in macOS Keychain on the desktop.
- Store only a token hash or equivalent secret material in the agent config.
- Send the token as gRPC metadata, e.g. `authorization: Bearer <token>`.
- Bind agent to a configured interface/address. For personal use, a Tailscale
  address or localhost tunnel is preferred over `0.0.0.0`.
- Support TLS for non-private networks. For Tailscale/private-only use, h2c plus
  the app token can be acceptable initially, but the protocol should not assume
  cleartext.

## Tailscale Policy

Tailscale is a recommended deployment path, not a dependency.

GitTerm config should store an endpoint, not a Tailscale-specific object:

```json
{
  "name": "Mac mini",
  "kind": "agent",
  "endpoint": "https://traceys-mac-mini.tailnet-name.ts.net:7777",
  "auth": {
    "type": "token",
    "token_ref": "keychain:gitterm/mac-mini"
  }
}
```

Other valid endpoints:

```text
http://127.0.0.1:7777              # dev / SSH tunnel
http://192.168.1.20:7777           # LAN
https://mac-mini.example.net:7777  # TLS/reverse-proxy path
```

The setup UI may detect or suggest a Tailscale/MagicDNS address, but the runtime
client should only need `endpoint + auth`.

## Remote Agent Responsibilities

The remote agent owns remote state and exposes structured APIs.

Initial responsibilities:

- Handshake/version/capability reporting.
- Workspace discovery and configured workspace roots.
- Directory listing.
- File read for preview.
- Git status, branches, worktrees, and diffs.
- Session catalog for shell/codex/claude/pi/custom commands.
- Start/resume/stop session.
- Terminal attach streaming.
- Event watch stream for session/file/git status changes.

Later responsibilities:

- File write/save.
- File operations: rename, delete, create folder/file.
- Remote plans/docs listing and preview.
- Agent transcript indexing/search.
- Remote command palette actions.
- Agent self-update.

## Session Runtime

The desktop should never parse tmux output. The agent may choose its internal
session implementation.

Preferred API-level model:

```text
SessionKind = Shell | Codex | Claude | Pi | Custom(command)
SessionId = opaque stable id assigned by agent
WorkspaceId = opaque stable id assigned by agent
```

Important: support multiple sessions of every kind. The model should not have
one hardcoded Codex session and one hardcoded Claude session.

Recommended implementation approach:

1. Make the protocol independent of tmux.
2. Implement agent-owned PTY/session supervision first if it is straightforward:
   the agent is the durable process host, so laptop disconnects do not matter.
3. Use tmux behind the agent only if needed for crash/restart survival, external
   terminal attach, or lower-risk process persistence.
4. If tmux is used, all tmux parsing/control stays inside `gitterm-agent`; the
   desktop still receives typed session events and terminal bytes.

This keeps the future path open: the agent can swap from tmux to direct PTY or
vice versa without changing the desktop UI.

## Desktop Model

Introduce an explicit workspace location/backend model:

```rust
enum WorkspaceLocation {
    Local {
        root: PathBuf,
    },
    RemoteAgent {
        remote_id: RemoteId,
        workspace_id: WorkspaceId,
        root: String,
    },
}
```

GitTerm UI code should operate through a backend facade:

```rust
trait WorkspaceBackend {
    fn list_dir(&self, request: ListDirRequest) -> Task<Event>;
    fn read_file(&self, request: ReadFileRequest) -> Task<Event>;
    fn git_status(&self, request: GitStatusRequest) -> Task<Event>;
    fn git_worktrees(&self, request: GitWorktreesRequest) -> Task<Event>;
    fn list_sessions(&self, request: ListSessionsRequest) -> Task<Event>;
    fn start_session(&self, request: StartSessionRequest) -> Task<Event>;
    fn attach_terminal(&self, request: AttachTerminalRequest) -> Task<Event>;
}
```

The exact Rust shape can differ because `Task<Event>` and async trait ergonomics
matter, but the boundary is the important part: Files/Git/Agent/session UI asks
the active workspace backend to do work. It does not decide how local or remote
work is performed.

## Protocol Sketch

Use protobuf definitions under a stable location such as:

```text
proto/gitterm/agent/v1/agent.proto
```

Initial service shape:

```protobuf
service GitTermAgent {
  rpc Handshake(HandshakeRequest) returns (HandshakeResponse);

  rpc ListWorkspaces(ListWorkspacesRequest) returns (ListWorkspacesResponse);

  rpc ListDir(ListDirRequest) returns (ListDirResponse);
  rpc ReadFile(ReadFileRequest) returns (stream FileChunk);

  rpc GitStatus(GitStatusRequest) returns (GitStatusResponse);
  rpc GitWorktrees(GitWorktreesRequest) returns (GitWorktreesResponse);
  rpc GitDiff(GitDiffRequest) returns (GitDiffResponse);

  rpc ListSessions(ListSessionsRequest) returns (ListSessionsResponse);
  rpc StartSession(StartSessionRequest) returns (Session);
  rpc StopSession(StopSessionRequest) returns (StopSessionResponse);
  rpc AttachTerminal(stream TerminalInput) returns (stream TerminalOutput);

  rpc Watch(WatchRequest) returns (stream AgentEvent);
}
```

Contract principles:

- Use opaque IDs for remote workspaces and sessions.
- Include protocol version and capability flags in handshake.
- Include enough error context to show useful UI messages.
- Keep paths remote-native strings in the protocol; only the local backend uses
  `PathBuf`.
- Avoid exposing tmux concepts in public API names.
- Use chunks for file content and terminal streams.
- Watch stream events should be typed, not string logs.

## Suggested File Layout

Likely first layout inside this repo:

```text
proto/gitterm/agent/v1/agent.proto
src/remote/
  mod.rs
  backend.rs          # WorkspaceBackend facade and shared request/result types
  local.rs            # LocalBackend wraps current filesystem/git/session behavior
  client.rs           # RemoteAgentBackend gRPC client
  protocol.rs         # generated/re-exported protobuf types or adapters
src/bin/gitterm-agent.rs
src/agentd/
  mod.rs
  server.rs           # gRPC service implementation
  filesystem.rs
  git.rs
  sessions.rs
  config.rs
```

This repo has a single-file `src/main.rs` convention for the desktop app. The
remote agent is a separate binary with a separate ownership boundary, so it is
reasonable to put agent/backend code in modules instead of adding thousands more
lines to `main.rs`.

## Current Prototype State To Avoid Deepening

Current in-progress remote support is useful as learning/prototype code:

- `RemoteSessionConfig` / `RemoteSessionsFile` in `src/config.rs`.
- Remote workspace entries and `Remote` sidebar mode in `src/main.rs`.
- SSH/tmux session listing in `src/services.rs`.
- Remote shell/codex/claude attach commands in `src/main.rs`.

But do not keep adding remote Files/Git/Agent feature work by layering more
direct SSH calls into the desktop app. That path will create the complexity the
agent architecture is meant to avoid.

Before continuing feature work, decide whether to keep the prototype behind a
temporary flag or migrate it into the new backend abstraction.

## Phased Task Plan

Each phase should leave the app buildable. Prefer one phase per commit or PR.

### Phase 0 - Stabilize And Document The Current Prototype

Goal: capture decisions and avoid accidental deepening of the SSH/tmux desktop
prototype.

- [x] Confirm current branch and dirty files.
- [x] Keep the remote placeholder behavior that prevents local filesystem/Git
      leakage in remote workspaces.
- [x] Add this architecture plan.
- [x] Add a tactical checkpoint pointing to this plan.
- [x] Do not add remote Files/Git over SSH in desktop before Phase 1.

Acceptance:

- [x] `cargo fmt -- --check`
- [x] `cargo clippy -- -D warnings`
- [ ] User can still open existing remote workspace prototype.

### Phase 1 - Define Remote Model And Config

Goal: introduce the durable data model without changing behavior.

- [x] Add `WorkspaceLocation` to runtime workspace state.
- [x] Add persistent remote-agent config separate from old
      `RemoteSessionConfig`.
- [x] Add endpoint/auth token reference fields.
- [x] Add workspace identity fields for remote workspaces.
- [x] Migrate or adapt current remote prototype config into the new shape.
- [x] Ensure local workspaces persist exactly as before.
- [x] Add backend-shaped Git/Files request types and a local-first
      `WorkspaceBackendRef` facade.

Acceptance:

- [x] Existing local workspace JSON loads and saves without churn.
- [x] Existing remote prototype can be represented as `RemoteAgent` or
      explicitly marked legacy.
- [x] No Files/Git behavior change yet.
- [x] Existing local Git/Files request helpers dispatch through the backend
      facade while preserving their public call shape.

### Phase 2 - Add Protocol Contract And Agent Skeleton

Goal: create the standalone `gitterm-agent` binary and prove desktop-to-agent
handshake.

- [x] Add protobuf contract and codegen.
- [x] Add `src/bin/gitterm-agent.rs`.
- [x] Implement `Handshake`.
- [x] Implement token auth interceptor/middleware.
- [x] Add local config path for the agent, e.g.
      `~/.config/gitterm-agent/config.json`.
- [x] Bind to configured host/port.
- [x] Add a simple CLI mode: `gitterm-agent serve`.

Acceptance:

- [x] `cargo run --bin gitterm-agent -- serve` starts locally.
- [x] A test client can call `Handshake`.
- [x] Bad/missing token is rejected.

### Phase 3 - Add Desktop RemoteAgentBackend Client

Goal: desktop can connect to agent and list remote workspaces/sessions through
typed API, but UI can remain simple.

- [x] Add `RemoteAgentBackend` client wrapper.
- [x] Add connection state: disconnected, connecting, connected, error.
- [x] Add handshake on remote workspace activation.
- [x] Add useful error display in remote workspace home/sidebar.
- [x] Keep current remote prototype available only as needed during transition.

Acceptance:

- [x] Selecting a remote workspace connects to the agent endpoint.
- [x] Connection failure shows endpoint/error, not local filesystem fallback.
- [x] Reconnect works after agent restart.

### Phase 4 - Backend-Driven Files

Goal: same Files panel works for local and remote through the backend facade.

- [x] Extract local file tree request/result into backend-shaped types.
- [x] Implement agent `ListDir`.
- [x] Route Files tab through active workspace backend.
- [x] Support folder navigation.
- [x] Add loading/error states for file tree.
- [x] Implement `ReadFile` for preview after listing works.
- [x] Keep write/edit actions disabled for remote until explicitly implemented.

Acceptance:

- [x] Local Files behavior unchanged.
- [x] Remote Files shows actual remote repo files after deploying the updated
      agent binary.
- [x] Navigating remote directories never reads local filesystem.
- [x] Clicking remote file can preview content once `ReadFile` lands.

### Phase 5 - Backend-Driven Git

Goal: same Git panel works for local and remote.

- [x] Extract git status request/result into backend-shaped types.
- [x] Implement agent `GitStatus`.
- [ ] Implement agent `GitWorktrees`.
- [x] Implement agent `GitDiff`.
- [x] Route Git tab through active workspace backend.
- [x] Preserve current branch/worktree UI (worktrees stay local-only until
      the worktrees RPC lands).

Acceptance:

- [x] Local Git behavior unchanged.
- [x] Remote Git shows status for remote repo.
- [ ] Remote branches/worktrees do not show local branches/worktrees.
- [x] Errors include remote path and command context.

### Phase 6 - Session Runtime And Terminal Streaming

Goal: remote shell/codex/claude/pi/custom sessions are first-class and
multi-instance.

**Decision (2026-07-03):** direct agent-owned PTY supervision, no tmux. The
agent is the durable host: sessions keep running when the desktop
disconnects; an output ring buffer replays recent scrollback on reattach.
(Sessions do not survive an agent restart — tmux-behind-agent remains the
upgrade path if that matters later.)

**Desktop attach:** a `gitterm-agent attach` CLI bridges stdio to the
AttachTerminal stream in raw mode. Desktop terminal tabs run that CLI as
their startup command inside the existing local iced_term machinery — no
terminal-emulator changes, and the CLI works from any terminal.

- [x] Define `SessionKind` and `Session` protocol messages.
- [x] Implement agent session registry.
- [x] Implement `ListSessions`.
- [x] Implement `StartSession`.
- [x] Implement `StopSession`.
- [x] Implement bidirectional `AttachTerminal`.
- [x] Decide direct PTY vs tmux-behind-agent for the first implementation.
- [x] Update desktop tab creation so remote sessions behave like local terminal
      tabs (attach CLI inside a normal local tab).

Acceptance:

- [x] Multiple remote shell sessions can run at once.
- [x] Multiple remote Codex sessions can run at once.
- [x] Multiple remote Claude sessions can run at once.
- [ ] Pi is modeled as a peer session kind, not a later special case.
- [x] Laptop disconnect/reconnect does not stop remote sessions while agent is
      running.

### Phase 7 - Remote Agent Tabs

Goal: the existing local agent-tab direction works against remote sessions too.

- [x] Ensure Codex/Claude/Pi launch commands are configured per remote
      (`session_commands` in remote-agents.json).
- [ ] Represent agent sessions with the same session model as shell sessions.
- [ ] Feed remote agent stream events to the same UI surfaces as local agent
      tabs where possible.
- [ ] Avoid separate "remote codex" and "local codex" UI implementations.

Acceptance:

- [x] User can create remote Codex/Claude/Pi sessions from a remote workspace.
- [x] The existing option-click "+" launcher works identically in remote
      workspaces for every configured harness (Claude, Codex, pi, custom
      commands), driven by backend session capabilities — no separate remote
      launcher UI and no remote conditionals in the launcher code.
- [ ] Multiple sessions are supported.
- [ ] Session status survives desktop reconnect.

### Phase 8 - Install/Bootstrap Flow

Goal: make setup repeatable.

- [ ] Add installer command or UI flow that uses SSH only for bootstrap.
- [ ] Copy/build/install `gitterm-agent` on remote machine.
- [ ] Generate/pair token.
- [ ] Install macOS LaunchAgent for user-level service.
- [ ] Start/restart service.
- [ ] Detect and suggest endpoint, including Tailscale/MagicDNS when present.
- [ ] Store desktop token in Keychain.
- [ ] Remote workspace creation flow: after pairing, the user browses the
      remote machine's directories (agent `ListDir`) with the same picker UX
      as local workspace creation and selects a workspace root. No typing
      remote paths by hand.

Acceptance:

- [ ] Fresh Mac mini can be configured from GitTerm.
- [ ] Runtime connection uses endpoint, not SSH.
- [ ] Tailscale is suggested if available but not required.

### Phase 9 - Remove Or Retire Legacy SSH/Tmux Desktop Prototype

Goal: eliminate duplicate remote paths once agent backend covers the behavior.

- [ ] Remove direct SSH session listing from desktop if no longer needed.
- [ ] Remove hardcoded remote shell/codex/claude attach commands.
- [ ] Migrate legacy `remotes.json` if needed.
- [ ] Keep only agent endpoint config for normal runtime.

Acceptance:

- [ ] No remote Files/Git/session feature depends on ad hoc SSH calls in
      desktop update/view code.
- [ ] UI code routes through `WorkspaceBackend`.

## Immediate Next Step

Finish Phase 4 by implementing agent `ReadFile` and routing remote file preview
through the workspace backend facade. Do not add direct SSH file reads to the
desktop app.

Do not implement remote file listing over SSH as the next step. That would move
the codebase in the wrong architectural direction.

## Open Questions

- Should the first agent session runtime be direct PTY supervision or tmux
  behind the agent?
- Should the first gRPC server run h2c on a private endpoint, or TLS from day
  one?
- Should the agent live in this repo as `src/bin/gitterm-agent.rs`, or become a
  separate crate/workspace member?
- How much remote file editing should v1 include versus read-only preview?
- Should remote workspaces be discovered by the agent, configured in desktop,
  or both?

## Non-Goals For The First Agent Milestone

- Public internet exposure.
- Multi-user permissions.
- Enterprise SSO.
- Remote file write operations.
- Replacing all local terminal/agent code.
- Requiring Tailscale.
- Making SSH the long-term runtime API.
