# GitTerm v2

A Rust terminal application with integrated git status viewer and file explorer, built with the [Iced](https://github.com/iced-rs/iced) GUI framework.

## Architecture

- **Single-file app**: `src/main.rs` (~3300 lines) contains the entire application
- **Supporting modules**: `src/log_server.rs`, `src/markdown.rs`, `src/webview.rs`
- **Terminal**: Uses `iced_term` fork at `../iced_term_fork`
- **Theme**: Catppuccin Mocha (dark) / Latte (light)

## Build & Run

```bash
cargo run          # Debug build
cargo build --release  # Release build
```

## Key Conventions

- Single-file architecture — all app logic lives in `main.rs`
- Catppuccin color theme with `AppTheme::Dark` / `AppTheme::Light`
- Config persisted to `~/.config/gitterm/config.json`
- Workspace state persisted to `~/.config/gitterm/workspaces.json`
- Iced `Task<Event>` pattern for async operations (file dialogs, etc.)

## Design Roadmap

See `design/WORKSPACE_DESIGN.md` for the multi-phase workspace system plan:
1. Workspace System (grouping tabs by project)
2. Attention System (detecting when Claude Code needs input)
3. Tab Overflow Handling
4. Console Panel (always-visible process runner)
