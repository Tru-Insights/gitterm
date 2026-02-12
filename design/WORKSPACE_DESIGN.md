# GitTerm v2 — Workspace System Design

## The Problem: The Agentic Split

Modern dev workflows split context across three apps per project:

| App | What Lives There |
|-----|-----------------|
| **Terminal** | Dev server, git tab, Claude Code / AI assistant |
| **IDE** | Workspace / file viewing |
| **Browser** | localhost preview, GitHub |

Multiply by 3 projects and you have 9+ windows across 3 apps. Context doesn't travel together — switching from Project A to Project B means mentally reassembling your workspace.

GitTerm already solves much of this by combining terminal + git + file viewing. The next step is solving the **multi-project** and **attention management** problems.

## The Core Workflow

Every workspace is assumed to be running **Claude Code** as the primary terminal session. The sidebar handles git and file browsing — no need for a dedicated git tab. A dev server runs in the console panel — no need for a dedicated dev server tab.

**Per workspace:**
- **Terminal** = Claude Code (the main and usually only tab)
- **Sidebar** = git status, staging, diffs, file explorer
- **Console panel** = dev server / build watch process

This means most workspaces are effectively **single-tab**. With 3 projects open, you have 3 Claude Code sessions, 3 sidebars, and 3 console panels — all instantly switchable via the workspace rail.

## What We're Building

Four interconnected features:

1. **Workspace System** — Group tabs by project, switch between projects instantly
2. **Attention System** — Make it impossible to miss when a Claude Code session (or any tab) needs input
3. **Tab Overflow Handling** — Fix the current wrapping bug when tabs exceed window width
4. **Console Panel** — Always-visible process runner (dev server) that doesn't waste a tab

## 1. Workspace System

### Layout

```
┌──┬──────────────────────────────────────────┐
│  │ [Claude Code]  [+]                        │
│W1│──────────────────────────────────────────│
│  │ sidebar │ terminal (Claude Code)          │
│W2│ git/    │                                 │
│  │ files   │                                 │
│W3│──────────────────────────────────────────│
│  │ ▼ cargo run  ● running    ↑ 2h   [↻] [■] │
│+ │ 14:22:04 Server listening on :3030        │
└──┴──────────────────────────────────────────┘
```

The typical workspace has a single tab (Claude Code), with git/files in the sidebar and the dev server in the console panel. Additional tabs can be added when needed but most workflows are single-tab.

- **Workspace rail**: 48px wide column on the far left
- **Each workspace button**: 36x36px, shows 2-letter abbreviation, color accent
- **Active workspace**: highlighted background + left accent bar (inset box-shadow)
- **Add button**: dashed border `+` below the divider

### Data Model

```rust
struct Workspace {
    name: String,          // "gitterm"
    abbrev: String,        // "GT"
    dir: PathBuf,          // root directory
    color: WorkspaceColor, // lavender, green, peach, etc.
    run_command: Option<String>, // "cargo run"
    tabs: Vec<TabState>,
    active_tab: usize,
    process: Option<ProcessState>,
}

// App now holds workspaces instead of flat tabs
struct App {
    workspaces: Vec<Workspace>,
    active_workspace: usize,
    // ... existing fields
}
```

### Persistence

Saved to `~/.config/gitterm/workspaces.json`:

```json
{
  "workspaces": [
    {
      "name": "gitterm",
      "abbrev": "GT",
      "dir": "/Users/t/GitRepo/gitterm",
      "color": "lavender",
      "run": "cargo run",
      "tabs": [
        { "dir": "/Users/t/GitRepo/gitterm" },
        { "dir": "/Users/t/GitRepo/gitterm" }
      ]
    }
  ],
  "active_workspace": 0
}
```

Terminal state is NOT saved — just working directories. Tabs are recreated on launch.

### Workspace Creation Flow

1. User clicks `+` on workspace rail → folder picker opens
2. GitTerm auto-detects run command from the directory:
   - `package.json` with "dev" script → `npm run dev`
   - `package.json` with "start" script → `npm start`
   - `Cargo.toml` → `cargo run`
   - `docker-compose.yml` → `docker compose up`
   - Otherwise → no run command (user can set manually)
3. Workspace created with name derived from folder name
4. First tab opens in that directory

### Migration / Default Behavior

On first launch after update: all existing tabs become a single "Default" workspace. Zero friction. Users opt into multi-workspace when ready.

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+1/2/3...` | Switch workspace by position |
| `Ctrl+Tab` | Cycle to next workspace |
| `Ctrl+Shift+Tab` | Cycle to previous workspace |
| `Cmd+Shift+N` | New workspace (folder picker) |
| `Cmd+Shift+W` | Close current workspace |

Note: `Ctrl` for workspaces, `Cmd` for tabs — two levels of navigation, two modifier keys.

## 2. Attention System

### The Problem

Claude Code sets a `*` in the terminal title when waiting for input. With 6+ tabs across multiple workspaces, scanning for `*` is a "Where's Waldo" situation.

### Three Tab States

| State | Visual | Trigger |
|-------|--------|---------|
| **Idle** | Gray dot | No process running in tab |
| **Running** | Green dot (subtle) | Process actively running |
| **Needs Attention** | Pulsing amber dot + amber background + amber border | Terminal title contains `*` |

### Detection

On `ChangeTitle` event from the terminal:
- Check if title starts with `*` → set `tab.needs_attention = true`
- Clear attention when: tab becomes active AND user sends any keypress to terminal

### Attention Bubbles Up

Workspace buttons show a badge with count of attention-needing tabs:
```
┌────┐
│ FE │
│  2 │  ← amber badge: 2 tabs need input
└────┘
```

Two badge colors:
- **Amber** = Claude Code / tab needs input
- **Red** = workspace process crashed

### The Killer Shortcut

`Ctrl+backtick` — Jump to the next tab needing attention across ALL workspaces. Round-robin cycling. Doesn't matter where you are — one keystroke takes you to the next CC session waiting for input.

### Implementation in Iced

Iced doesn't natively support CSS animations. Two approaches for the pulsing dot:
1. `Subscription::every(Duration::from_millis(500))` to toggle `attention_bright` bool, alternating between two amber shades
2. Simpler: static amber background (still highly visible without animation)

## 3. Tab Overflow Handling

### Current Problem

Tab bar is a `Row` that grows indefinitely. When tabs exceed window width, they wrap in an odd way.

### Solution

- Make tab bar a horizontal `Scrollable(Row)`
- When tabs overflow, show an overflow indicator at the end: `+2 more ● 1`
  - `+2 more` = count of hidden tabs
  - `● 1` = count of hidden tabs needing attention (in amber)
- Clicking the overflow indicator could show a dropdown/list of all tabs

### Natural Mitigation

With most workspaces being single-tab (Claude Code), overflow should be extremely rare. But the fix handles edge cases when users add extra tabs.

## 4. Console Panel

### Concept

Every workspace needs a "run" process (dev server, build watch, etc). Currently this wastes a full terminal tab. The console panel is a dedicated, always-visible panel at the bottom of the window that shows process output without consuming a tab.

### Layout

The console panel sits at the bottom, below the terminal area, spanning the full width (excluding workspace rail):

```
Row[
  workspace_rail,
  Column[
    tab_bar,
    content_area,     // sidebar + terminal (flex: 1)
    console_panel     // fixed/resizable height
  ]
]
```

### Console Panel Structure

```
┌─────────────────────────────────────────────────────────┐
│ ▼  ●  cargo run  — target/debug/gitterm  ↑ 2h  [⌀][↻][■] │  ← header (32px)
│ 14:22:01  Compiling gitterm v0.1.0                       │  ← scrollable output
│ 14:22:03  Finished dev in 2.1s                           │
│ 14:22:04  Server listening on http://localhost:3030       │
└─────────────────────────────────────────────────────────┘
```

### Header Elements

| Element | Description |
|---------|-------------|
| Chevron (▼/▶) | Click to expand/collapse |
| Status dot | Green (running), red (error/crashed), gray (stopped) |
| Process name | The run command |
| Process detail | Binary name or "exited 101" |
| Uptime | "↑ 2h 14m" or "died 30s ago" |
| Clear button (⌀) | Clear output buffer |
| Restart button (↻) | Restart the process |
| Stop button (■) | Stop the process (swaps to ▶ play when stopped) |

### Console States

1. **Healthy / Running**: Green dot, normal output, minimal visual weight
2. **Error / Crashed**: Red dot (pulsing), red-tinted header background, auto-expands if collapsed, shows error output
3. **Collapsed**: Just the 32px header bar — status dot + name + uptime. Maximum terminal space.
4. **No Process Configured**: Shows "No run command configured" + gear button to set one

### Key Behaviors

- **Belongs to workspace, not tab**: Switching tabs doesn't change the console. Switching workspaces shows that workspace's process.
- **Always visible**: You see build errors instantly regardless of which tab you're in
- **Resizable**: Drag handle between content area and console panel. Height persisted in config.
- **Auto-expand on error**: If process crashes, console expands automatically to show the error
- **Auto-collapse on success**: After restart, if process starts successfully, auto-collapse back

### First-Time Startup Flow

1. Create workspace → auto-detect run command
2. Console shows: `● cargo run (detected)  [▶ Start]`
3. User clicks Start → process runs
4. Next time workspace activates → auto-starts (configurable)

### Process Lifecycle

- **Start**: When workspace is activated (if auto-start enabled) or user clicks play
- **Background**: Process keeps running when you switch to another workspace
- **Stop**: User clicks stop, or process crashes
- **Quit**: All processes gracefully terminated (SIGTERM → SIGKILL after 5s)
- **Relaunch**: Processes restart automatically for each workspace

### Output Buffer

- Last 1000 lines per process (configurable)
- Timestamped for context
- Stderr lines highlighted in red
- Error detection by exit code or stderr patterns

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+J` | Toggle console expand/collapse |
| `Cmd+Shift+R` | Restart the process |

### Tab Reduction

Before (traditional terminal): 3 tabs per project (dev server + git + claude code)
After (GitTerm): **1 tab** (claude code) + sidebar (git/files) + console panel (dev server)

With 3 workspaces, that's going from 9 tabs to 3 — one Claude Code session per project, everything else handled by the surrounding UI.

## Implementation Order (Suggested)

### Phase 1: Foundation
1. Refactor `App` to hold `Vec<Workspace>` instead of flat `Vec<TabState>`
2. Workspace rail UI (static, single workspace initially)
3. Workspace switching (Ctrl+1/2/3)
4. Workspace persistence (save/load workspaces.json)

### Phase 2: Attention
5. Add `needs_attention` flag to TabState
6. Detect `*` in terminal title → set flag
7. Attention tab styling (amber background/border/dot)
8. Attention badges on workspace rail
9. `Ctrl+backtick` jump-to-attention shortcut

### Phase 3: Console Panel
10. Console panel UI (header + scrollable output)
11. Process spawning (tokio::process::Command with piped stdout/stderr)
12. Process lifecycle (start/stop/restart)
13. Auto-detection of run commands
14. Error state + auto-expand behavior
15. Resize handle + persisted height

### Phase 4: Polish
16. Tab overflow handling (scrollable tab bar + overflow indicator)
17. Workspace creation flow (folder picker + auto-detect)
18. Migration logic (existing config → default workspace)
19. Workspace tooltips on hover

## Design Assets

- HTML mockup: `design/workspace-mockup.html`
- Screenshot reference: The "agentic split" diagram showing the Terminal/IDE/Browser problem

## Open Questions

- Should workspace rail show a tiny process status dot below the abbreviation?
- Should there be a "Save current tabs as workspace" command in addition to folder picker?
- Tab movement between workspaces — keyboard command or drag?
- Multiple run commands per workspace? (e.g., dev server + watch mode)
