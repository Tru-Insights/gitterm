# Claude Sidebar Tab — Design Doc

## Overview

Add a "Claude" tab to the left sidebar alongside the existing Git and Files tabs. This tab provides a structured tree view of all Claude Code configuration — skills, plugins, MCP servers, hooks, and settings — with the ability to click any item to view its source file in the main viewport.

## Mockup

See `design/claude-config-ui-mockup.html` for the interactive prototype.

## UI Design

### Sidebar Tab Bar

- Replaces the current sidebar button-style tab switcher with an underline-style tab bar (Git | Files | Claude)
- Active tab indicated by a mauve underline, consistent with the bottom panel tab treatment and workspace indicators
- Each tab has a small icon prefix

### Claude Tree View

Collapsible sections with item counts:

| Section     | Dot Color  | Data Source                                          |
|-------------|------------|------------------------------------------------------|
| Skills      | Green      | `~/.claude/commands/*.md` (user), `.claude/commands/*.md` (project), plugin skill files |
| Plugins     | Blue       | `~/.claude/settings.json` → `enabledPlugins`         |
| MCP Servers | Peach      | `.mcp.json` (project), `~/.claude/.mcp.json` (global) |
| Hooks       | Pink       | `~/.claude/settings.json` → `hooks`                  |
| Settings    | Lavender   | `~/.claude/settings.json` (various keys)             |

### Scope Badges

Each tree item can show a scope badge on the right:

- `USR` — user-global (from `~/.claude/`)
- `PRJ` — project-local (from `.claude/` in workspace dir)
- `PLG` — provided by a plugin

### File Viewing

Clicking a tree item opens the corresponding file in the main viewport using the existing file viewer infrastructure:

- **Skills**: opens the `.md` file, rendered as markdown
- **Plugins**: opens the plugin's config/manifest
- **MCP Servers**: opens `.mcp.json`
- **Hooks**: opens `settings.json` (scrolled to relevant section)
- **Settings**: opens `settings.json`

File viewer shows a path breadcrumb header with a close button. Closing returns to the terminal.

## Data Sources (all local filesystem, no subprocess)

### Skills

Merge from multiple sources:

1. **User global**: `~/.claude/commands/*.md` — scope `USR`
2. **Project local**: `{workspace_dir}/.claude/commands/*.md` — scope `PRJ`
3. **Plugin-provided**: read `~/.claude/settings.json` → `enabledPlugins`, scan plugin cache dirs for `skills/` folders — scope `PLG`

### Plugins

Read `~/.claude/settings.json` → `enabledPlugins` map. Each key is `name@source`. Installed plugin metadata lives in `~/.claude/plugins/`.

### MCP Servers

Parse JSON from:
- `{workspace_dir}/.mcp.json` (project-scoped)
- `~/.claude/.mcp.json` (global)

### Hooks

Read `~/.claude/settings.json` → `hooks` object. Keys are event names (PreCompact, Stop, Notification, etc.). Each contains an array of matchers with hook actions (command or prompt type).

### Settings

Read `~/.claude/settings.json` for top-level keys: `effortLevel`, `statusLine`, `permissions`, etc. Project-level overrides from `.claude/settings.local.json`.

## Implementation Notes

### Sidebar Tab Bar Refactor

The current sidebar likely uses buttons or a custom switcher. This should be refactored to a proper tab bar component with:
- Horizontal layout with equal-width tabs
- Underline indicator on active tab (2px mauve, matches bottom panel tabs)
- Click to switch sidebar content

### Tree Component

Reusable collapsible tree with:
- Section headers: chevron + label + count badge
- Items: dot + name + optional scope badge
- Active item: highlighted background + left border accent (mauve)
- Click handler that resolves to a file path and triggers the file viewer

### Scanning Logic

On workspace switch or sidebar tab activation:
1. Scan user-global `~/.claude/commands/` for `.md` files
2. Scan project `.claude/commands/` for `.md` files
3. Read `settings.json` for plugins, hooks, settings
4. Check for `.mcp.json` files
5. For enabled plugins, scan cache dirs for skill definitions
6. Merge and deduplicate (project overrides global for same-named skills)
7. Sort alphabetically within each section

### Refresh

Re-scan when:
- Switching to the Claude sidebar tab
- Switching workspaces (different project dir)
- Could add a manual refresh button in the section header
