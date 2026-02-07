# Architecture

This document describes the internal architecture of GitTerm for contributors and anyone looking to understand the codebase.

## Overview

GitTerm is a native desktop application written in Rust using the [Iced](https://github.com/iced-rs/iced) GUI framework. It combines a full terminal emulator with Git integration, a file explorer, and a browser-accessible log server into a single app.

The entire application lives in a single crate (~3300 lines across 4 source files).

## Module Structure

| Module | Purpose |
|--------|---------|
| `src/main.rs` | Core application: UI layout, state management, event handling, git operations, terminal management |
| `src/webview.rs` | Embedded WebView (via `wry`) for rendering markdown and Mermaid diagrams |
| `src/log_server.rs` | HTTP server (via `warp`) on `localhost:3030` for browsing terminal output in a browser |
| `src/markdown.rs` | Markdown-to-HTML renderer (via `pulldown-cmark`) with Mermaid diagram support |
| `build.rs` | Windows-only build script for embedding the app icon via `winres` |

## Core Application (`main.rs`)

### State Model

The app follows Iced's Elm-inspired architecture: **State → View → Event → Update → State**.

- **`App`** holds global state: tabs, active tab index, theme, font sizes, sidebar width, config, and a handle to the log server's shared state.
- **`TabState`** holds per-tab state: the terminal instance, git status (staged/unstaged/untracked files), branch name, diff lines, file explorer state, file viewer state, search state, and the terminal title.
- **`Config`** is the persistent configuration, serialized to `~/.config/gitterm/config.json`.

### Event System

All user interactions and background events flow through the `Event` enum. Key categories:

- **Terminal events** — PTY output, title changes
- **Git events** — file selection, diff navigation
- **UI events** — tab switching, theme toggle, font size changes, sidebar mode toggle
- **File explorer events** — directory navigation, file viewing
- **Search events** — terminal scrollback search (Cmd+F)
- **Window events** — resize, divider dragging
- **Menu events** — native menu bar actions (polled every 50ms)

### Tab System

Each tab represents an independent workspace with:
- Its own PTY shell process
- Its own git repository context (auto-detected, updates when the terminal `cd`s to a different repo)
- Its own file explorer state
- Its own search state

Tabs are created by opening folders (via native file dialog or at startup with the home directory).

## Terminal Integration

The terminal emulator is provided by a custom fork of [`iced_term`](https://github.com/Harzu/iced_term), which wraps a PTY backend with an Iced widget.

### Shell Integration (Terminal-Sidebar Sync)

GitTerm injects shell hooks so the terminal title reflects the current working directory:

- **zsh**: Creates a custom `.zshrc` at `~/.config/gitterm/zsh/.zshrc` that installs a `precmd`/`chpwd` hook via `add-zsh-hook`. The `ZDOTDIR` environment variable redirects zsh to load this file before sourcing the user's normal config.
- **bash**: Sets `PROMPT_COMMAND` to emit an OSC title escape sequence with `$PWD`.
- **Windows**: No directory sync hooks currently; shells (PowerShell/cmd) are launched with inherited environment.

When the terminal title changes (via OSC escape codes), `TabState::extract_dir_from_title` parses the title to extract a directory path. This handles common formats:
- `~/path` or `/absolute/path`
- `~/path (extra)` — path with parenthetical info
- `dirname — zsh` — starship-style
- `user@host:~/path` — standard zsh/bash

If the parsed directory differs from the current one, the file explorer and git status are updated accordingly. If the new directory is inside a different git repository, the tab's repo context switches automatically.

## Git Integration

Git operations use `libgit2` (via the `git2` crate) and run synchronously on the main thread. Git status is polled every 5 seconds, but only when a diff is being viewed (to avoid unnecessary work).

### Diff Pipeline

1. **Status fetch** — `Repository::statuses()` categorizes files into staged, unstaged, and untracked.
2. **Diff generation** — `diff_tree_to_index` (staged) or `diff_index_to_workdir` (unstaged) produces patch-format diffs. Untracked files are read directly and shown as all-additions.
3. **Word-level highlighting** — After generating line-level diffs, `add_word_diffs()` pairs consecutive deletion/addition lines and runs `similar::TextDiff::from_words()` on each pair to produce inline change spans. These are rendered with distinct background colors in the diff view.

## Theming

GitTerm uses the [Catppuccin](https://github.com/catppuccin/catppuccin) color palette:
- **Dark mode**: Catppuccin Mocha
- **Light mode**: Catppuccin Latte

The `AppTheme` enum provides all color accessors used throughout the UI and terminal. Theme colors are also mirrored in `markdown::ThemeColors` for HTML rendering. The theme persists across sessions via the config file.

## WebView Module (`webview.rs`)

The WebView is used to render markdown files with full CSS styling and Mermaid diagram support. Due to `wry::WebView` not being `Send`/`Sync`, it must live on the main thread.

This is managed via `thread_local!` storage with a two-phase pattern:
1. **`set_pending_content`** — Stores HTML content and bounds to be rendered.
2. **`try_create_with_window`** — Called from the main thread with window access to create or update the WebView.

The WebView is shown/hidden as the user navigates between markdown files and other views.

## HTTP Log Server (`log_server.rs`)

A `warp` HTTP server runs on `localhost:3030` in a background Tokio task. It serves three routes:

| Route | Content |
|-------|---------|
| `GET /` | Index page listing all open tabs |
| `GET /tab/{id}` | Terminal output for a specific tab |
| `GET /file/{id}` | File content being viewed in a specific tab |

The server reads from `ServerState`, which holds `Arc<RwLock<HashMap>>` collections of terminal and file snapshots. The main app updates these snapshots on every tick (every 5 seconds). The browser pages include copy-to-clipboard, search, and refresh functionality.

## Markdown Rendering (`markdown.rs`)

Markdown is rendered to HTML using `pulldown-cmark` with GFM extensions (tables, strikethrough, task lists, footnotes). Before parsing, Mermaid code blocks are extracted and converted to `<pre class="mermaid">` tags so they're passed through to the HTML. If any Mermaid blocks are present, the Mermaid.js library is loaded from CDN.

The rendered HTML is a complete standalone document with theme-aware CSS.

## Platform Support

### macOS
- Native menu bar via `muda` (initialized after NSApp exists via `init_for_nsapp`)
- Shell integration via zsh/bash hooks
- App bundle creation via `scripts/bundle.sh`

### Windows
- App icon embedded via `winres` in `build.rs`
- PowerShell or cmd.exe as default shell (via `COMSPEC`)
- Full environment inherited from parent process
- Menu bar via `muda` (cross-platform fallback)

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `iced` | GUI framework (Elm architecture) |
| `iced_term` | Terminal emulator widget (custom fork at `../iced_term_fork`) |
| `git2` | libgit2 bindings for git operations |
| `wry` | WebView for markdown/Mermaid rendering |
| `muda` | Native menu bar (macOS + cross-platform) |
| `warp` | HTTP server for the log viewer |
| `pulldown-cmark` | Markdown parsing and HTML generation |
| `similar` | Text diffing for word-level diff highlights |
| `rfd` | Native file dialog (open folder) |
| `serde` / `serde_json` | Config serialization |
| `dirs` | Platform-appropriate config directories |
| `tokio` | Async runtime for warp and background tasks |
