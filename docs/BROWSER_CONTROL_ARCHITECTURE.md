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

## Acceptance Scenario

From a Codex terminal in GitTerm, start a local application, ask Codex to open
its detected localhost URL, resize to desktop and mobile widths, inspect the
rendered page, click through the workflow, identify a console or network error,
edit the application, reload, and verify the fix while the user watches the same
visible Chrome window.
