# GitTerm V4 Browser Control Architecture

## Goal

Give Codex CLI sessions running inside GitTerm a visible browser they can inspect
and operate while debugging local applications. The user and Codex must be able
to see the same page, click around, reproduce responsive bugs, inspect browser
diagnostics, change code, reload, and verify the result.

## Product Boundary

GitTerm V4 remains the host application and Codex remains the terminal agent.
This work does not adopt T3 Code, depend on the Codex desktop application, or
replace GitTerm's interface.

The existing singleton Wry WebView remains responsible for GitTerm-owned
surfaces such as agent chat, Markdown, Excalidraw, and the plans viewer. On
macOS Wry uses WebKit, so it is not the browser automation target and must not
be expanded into one for the first implementation.

## Proposed Architecture

```text
Codex CLI in a GitTerm terminal
              |
              | MCP
              v
GitTerm V4 browser-control service
              |
              | Chrome DevTools Protocol
              v
Visible Chrome window using a dedicated GitTerm V4 profile
```

GitTerm owns the browser lifecycle and connection state. The first version uses
a separate visible Chrome window because embedding Chromium into Iced would add
substantial CEF-style complexity without improving the debugging loop.

The browser controller must sit behind an adapter boundary. A later adapter may
connect through a Chrome extension when controlling explicitly authorized tabs
in the user's normal Chrome profile becomes a requirement. The dedicated-profile
adapter is the default because it is safer and more deterministic.

## T3 Code Lessons

T3 Code demonstrates a useful pattern without providing a drop-in component:

- expose focused browser operations through an authenticated MCP server;
- route tool calls through a browser broker rather than coupling tools directly
  to a window implementation;
- use Chromium debugging commands for input, console, network, accessibility,
  screenshots, and recording;
- prefer semantic/Playwright-style locators over screen coordinates;
- return screenshots together with structured page state;
- track whether the human or agent currently controls the page and interrupt an
  agent action when human input invalidates it.

Relevant upstream reference files:

- <https://github.com/pingdotgg/t3code/blob/main/apps/server/src/mcp/toolkits/preview/tools.ts>
- <https://github.com/pingdotgg/t3code/blob/main/apps/server/src/mcp/PreviewAutomationBroker.ts>
- <https://github.com/pingdotgg/t3code/blob/main/apps/desktop/src/preview/Manager.ts>
- <https://github.com/pingdotgg/t3code/blob/main/apps/server/src/provider/Layers/CodexAdapter.ts>

## Initial MCP Tool Surface

The first usable slice should provide:

- `browser_status`
- `browser_open`
- `browser_navigate`
- `browser_snapshot`
- `browser_click`
- `browser_type`
- `browser_press`
- `browser_scroll`
- `browser_resize`
- `browser_reload`
- `browser_wait_for`
- `browser_console`
- `browser_network`

`browser_snapshot` should combine a PNG screenshot with URL, title, loading
state, visible text, interactive elements, accessibility information, recent
console errors, and recent failed network requests. Locators are preferred;
coordinates remain an explicit fallback.

## Isolation And Security

Browser automation must preserve the V4/V3 boundary established by the V4
worktree:

- store the browser profile under the V4 config root, never under V3 or the
  user's normal Chrome profile;
- use a V4-specific local endpoint and random per-process credential;
- bind control endpoints to loopback only;
- do not expose cookies, passwords, or unrestricted browser storage as tools;
- annotate mutating MCP tools accurately and keep read-only inspection tools
  separate;
- show when the agent controls the browser and provide an immediate disconnect;
- serialize actions per tab and treat human input as an interruption boundary;
- never silently fall back to a personal Chrome profile.

## Delivery Phases

1. Launch a visible Chrome instance with a persistent V4 profile and establish a
   reliable DevTools connection.
2. Implement status, navigation, snapshot, click, type, reload, and responsive
   viewport controls.
3. Add console and network diagnostics plus deterministic waiting.
4. Wire the browser tools into Codex CLI through an authenticated local MCP
   endpoint managed by GitTerm.
5. Add GitTerm UI for browser status, open/focus/disconnect, and evidence
   screenshots or recordings.
6. Evaluate an optional extension adapter only after the dedicated-profile
   workflow has been used on real projects.

## Phase 4 Integration

GitTerm reserves an ephemeral `127.0.0.1` Streamable HTTP endpoint during app
startup and protects every MCP request with a random per-process bearer token.
The token remains in memory and is inherited only by local GitTerm terminals;
it is not logged or written to a config file.

When GitTerm launches a local Codex preset, it adds per-run `--config`
overrides for the MCP URL, bearer-token environment variable, write-tool
approval policy, and tool timeout. The persisted preset command remains
unchanged, and GitTerm does not edit user or project Codex configuration.
Remote-agent sessions do not receive the local browser endpoint.

GitTerm's generated zsh integration applies the same overrides when a user
starts `codex` manually from a local GitTerm terminal. The wrapper exists only
inside that terminal environment and reads the endpoint and token from inherited
environment variables, so the bearer token is still never written to disk.

The MCP server exposes the initial tool surface plus
`browser_disconnect`. Read-only annotations are limited to status, snapshot,
wait, console, and network inspection; browser lifecycle and interaction tools
are annotated as mutating.

## Phase 5 Integration

GitTerm's workspace bar reads shared in-memory browser telemetry so agent MCP
operations are visible while they run and briefly after they complete. MCP tool
calls are serialized before publishing activity, keeping the displayed
operation aligned with the browser controller's action order.

A successful browser snapshot retains only the latest PNG and its evidence
metadata in process memory. The in-app evidence viewer shows the page title,
sanitized URL, viewport, capture age, and diagnostic counts. Evidence is not
written to disk or restored across app launches; recording, replay, and a
persistent evidence history remain out of scope.

## Acceptance Scenario

From a Codex terminal in GitTerm, start a local application, ask Codex to open
its detected localhost URL, resize to desktop and mobile widths, inspect the
rendered page, click through the workflow, identify a console or network error,
edit the application, reload, and verify the fix while the user watches the same
visible Chrome window.
