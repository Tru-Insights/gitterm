# Resume: gitterm-v2

**Last checkpoint:** 2026-02-12

## Project Context

This is a fork of `gitterm` (the user's daily-driver terminal app) created specifically to build the **workspace system** and related features. The original repo at `/Users/t.trewin/GitRepo/gitterm` must remain untouched — the user runs that version all day.

## What Was Decided

Full design spec is at `design/WORKSPACE_DESIGN.md` with HTML mockup at `design/workspace-mockup.html`.

Four features to build:
1. **Workspace System** — Left rail (48px), 2-letter abbreviation buttons, `Ctrl+1/2/3` to switch, tabs grouped by project
2. **Attention System** — Detect `*` in terminal title, amber tab styling, badges on workspace rail, `Ctrl+backtick` to jump to next attention tab
3. **Console Panel** — Always-visible bottom panel for workspace run command (dev server), not a tab. Start/stop/restart controls, auto-expand on error, `Cmd+J` toggle
4. **Tab Overflow** — Scrollable tab bar with overflow indicator showing hidden tab count + attention count

## Current Codebase Notes

- **Single file architecture**: `src/main.rs` (~3300 lines) contains almost everything
- **Key structs**: `App` holds `Vec<TabState>` + `active_tab` index — this needs to become `Vec<Workspace>`
- **Tab bar**: `view_tab_bar()` at line ~2062, simple `Row` with no scroll/overflow handling
- **Terminal titles**: Captured via `ChangeTitle` event at line ~1358, stored in `tab.terminal_title`
- **Config**: Persisted to `~/.config/gitterm/config.json`, loaded on startup
- **Log server**: `src/log_server.rs` — HTTP server at :3030 for terminal output
- **Markdown**: `src/markdown.rs` — markdown rendering with Mermaid support
- **Theme**: Catppuccin Mocha (dark) / Latte (light), colors defined as methods on `AppTheme`

## To Resume

Start with **Phase 1: Foundation** from the design doc:
1. Refactor `App` to hold `Vec<Workspace>` wrapping the existing `Vec<TabState>`
2. Add workspace rail UI
3. Workspace switching
4. Persistence

Read `design/WORKSPACE_DESIGN.md` first for the full spec.
