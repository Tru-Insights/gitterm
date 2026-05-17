# Terminal Backend Investigation Notes

Notes from investigating whether GitTerm should adopt tmux or cmux concepts, and what to improve in the current `iced_term` flow.

## Current GitTerm model

GitTerm already persists workspace and tab intent in:

```text
~/.config/gitterm/workspaces.json
```

That file records workspace metadata and tab launch intent, including:

- workspace name, directory, abbreviation, color, and environment variables
- tab current directory / repo directory
- terminal `startup_command`
- agent tab config
- bottom terminal directories
- active workspace index

On startup, GitTerm restores this state and recreates terminals/tabs. Agent presets already include resume commands such as:

```text
pi --resume
claude --resume
codex resume
gemini --resume
```

So GitTerm already has the main UX-level session/tab persistence that tmux is often used to provide.

## tmux assessment

tmux is not a terminal renderer. It is a multiplexer/session manager that still requires a terminal emulator in front of it. GitTerm would still need `iced_term`, Ghostty, Alacritty, or another renderer.

### What tmux would add

The main tmux benefit would be process/session durability outside GitTerm:

- shells/agents could survive GitTerm quitting, crashing, or recreating terminal widgets
- users could attach to the same session from another terminal app
- panes could be inspected or controlled externally with commands such as `capture-pane`, `send-keys`, and `list-panes`
- tmux would provide durable pane/window/session IDs and layouts
- background output could be captured without relying on GitTerm rendering that tab

### Why not adopt it now

For the current app, tmux likely adds more complexity than value:

- another abstraction layer over PTYs
- TERM, mouse, keybinding, clipboard, and truecolor edge cases
- duplicate concepts: GitTerm workspaces/tabs vs tmux sessions/windows/panes
- harder debugging when agent sessions behave oddly
- potential instability while restore/recreate flows are already evolving

Decision for now: **do not add tmux**. Revisit only if GitTerm needs exact live-process reattach after app crash/restart, or external attach/control from another terminal.

## cmux assessment

cmux is a native macOS terminal/orchestrator focused on AI coding agents. It is useful as product/design inspiration, but not a direct replacement for `iced_term`.

Observed characteristics:

- Swift/AppKit macOS app
- terminal rendering through libghostty / Metal
- Ghostty config compatibility
- vertical tabs / surfaces
- agent-oriented notification and sidebar model
- CLI/socket automation concepts

Reasons it is not a direct dependency candidate:

- GitTerm is Rust + Iced; cmux is Swift + AppKit
- cmux is app architecture, not a reusable Iced widget
- libghostty integration appears macOS-native and invasive
- cmux is GPL-3.0-or-later/commercial dual licensed, while GitTerm is MIT

Decision for now: **borrow ideas, not code**.

Useful ideas to revisit later:

- agent-aware notifications
- sidebar/session presentation
- surface/pane/session model
- socket/CLI automation model
- Ghostty-style config compatibility

## `iced_term` improvement backlog

These are likely higher-value than introducing tmux right now.

### 1. Avoid destructive terminal recreation for visual changes

GitTerm currently calls `recreate_terminals()` for font/theme changes. That drops and recreates `iced_term::Terminal` instances, which also drops the PTY process.

Prefer using existing `iced_term` commands where possible:

- `iced_term::Command::ChangeTheme(...)`
- `iced_term::Command::ChangeFont(...)`

Goal: changing theme/font should not kill shells or agent processes.

### 2. Preserve tab launch context when recreation is unavoidable

`recreate_terminals()` currently rebuilds terminals with:

```rust
startup_command: None
extra_env: &[]
```

That means recreation can turn an agent/preset tab into a plain shell and lose workspace env injection.

If terminal recreation is unavoidable, preserve:

- `tab.startup_command()`
- intended cwd/current dir
- workspace environment variables
- terminal title/startup metadata where useful

### 3. Add explicit terminal lifecycle state

Today terminal failure/exited states are fairly implicit. Add a lifecycle model such as:

- running
- exited
- crashed
- recreating
- failed to spawn

Then the UI can offer actions like:

- restart
- restart with same command
- open plain shell
- copy spawn details

### 4. Improve terminal spawn diagnostics

When a terminal fails to spawn or exits quickly, surface useful debugging details:

- program
- args
- cwd
- env overrides
- error message
- time started / time exited

This would help debug agent presets and workspace-specific env problems.

### 5. Persist/recover more terminal context

GitTerm already persists launch intent. It could also persist lightweight context such as:

- last known terminal title
- last known cwd from shell title integration
- startup command display label
- last exit status, if available
- start/exit timestamps

This would make restored tabs feel less anonymous and make failures easier to understand.

### 6. Improve background output and attention handling

Without tmux, GitTerm can still improve agent awareness through terminal output events:

- detect Claude/pi waiting prompts
- mark tabs needing attention
- show last N output lines in tab hover/sidebar
- route bell/notifications per tab
- support configurable regex attention rules

This likely fits GitTerm's purpose better than adopting tmux.

### 7. Decouple renderer settings from process settings in `iced_term`

Longer term, separate visual/runtime updates from PTY/process lifecycle more clearly:

- theme/font changes should be renderer-only
- scrollback/process/cwd/env changes may require backend changes
- GitTerm should be able to update visual settings without touching the PTY

## Recommended next work

When there is time to work on terminal robustness, start here:

1. Audit `recreate_terminals()` and replace full recreation with `ChangeTheme` / `ChangeFont` where possible.
2. If any recreation remains, preserve startup command, cwd, and workspace env.
3. Add visible terminal lifecycle/error state for failed or exited terminals.
4. Add better spawn diagnostics before considering external systems like tmux.
