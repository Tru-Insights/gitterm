# GitTerm V5 Task Navigation — Design Decision

**Status:** Direction chosen — Task Rail + task switcher

**Decides:** `.plans/task-navigation-design-brief.md`

**Chosen:** 2026-08-19

**Exploration record:** three directions were mocked and compared (Task Rail,
Grouped Strip, Command Deck). The full exploration with mockups lives in the
"GitTerm Task Navigation" artifact; this document records the winning design
in enough detail to implement.

## Decision

Adopt the **Task Rail** as the backbone of task navigation, with the
**Command Deck's fuzzy switcher** grafted on as the keyboard path:

1. Delete the tier-1 context chip row. One top strip remains: context stamp +
   the current context's session tabs + a context-aware add button.
2. Promote the Tasks sidebar perspective to a pinnable **rail**: the single
   task list in the product, living beside the canvas, collapsing into the
   spine as status micro-dots.
3. Add a **⌘T task switcher** (fuzzy palette over General + tasks), which is
   also the fix for the currently missing task keyboard story.

Rejected: **Grouped Strip** (re-imports the tab-shaped-task problem — durable
caps sitting beside ephemeral view tabs — and is blind to zero-session tasks)
and **Command Deck** alone (no ambient fleet state; the coordinator has to
poll for progress). Both failed the same way: they bunch objects with
different lifetimes into one region. The Rail gives each lifetime its own
home:

| Lifetime | Home | Shape |
|---|---|---|
| Workspace (long-lived) | Bottom workspace bar | Chip, grouped by machine |
| Task (durable) | Rail | Two-line row — never a tab |
| Session (resumable) | Top strip, within its context | Tab with agent glyph |
| View (ephemeral) | Top strip | Tab; closing loses nothing |

## Layout

```text
┌──┬──────────────────────────────────────────────────────────────────────┐
│sp│ [TASK·TRU-104 ⌇ Durable task store][Overview][✦ Claude ⌘1][⌁ Codex ⌘2]│
│in├──────────────┬───────────────────────────────────────────────────────┤
│e │ rail (224px, │                                                       │
│◈ │ pinnable,    │                     canvas                            │
│••│ collapsible) │                                                       │
│  ├──────────────┴───────────────────────────────────────────────────────┤
│  │ [THIS MAC] [gitterm-v5] [gitterm-v4] … [⚡ n] [?]                     │
└──┴──────────────────────────────────────────────────────────────────────┘
```

The strip is **one ~33px horizontal row** — never two stacked rows. The
stamp is a compact two-line text block (9px kicker over 12px value, as the
shipped tier-2 stamp already renders) sitting inline at the row's left,
followed by Overview, the session tabs, the add button, and the branch
readout at the right edge. Top chrome drops from 66px (two tiers) to ~33px.

## The rail

Content, top to bottom:

- Header: current repo name + `+ New task`.
- **General** row: the workspace's non-task sessions (`General · 3 sessions`).
  Selecting it returns to workspace context.
- `TASKS · n` section with filter pills: `All` / `Needs you · n` /
  `Archived` (filter, never a section).
- One two-line row per non-archived task in this repository workspace:
  - Line 1: status dot + issue key + title. Tasks without an issue show the
    title with a `local` badge. Branch, worktree path, and UUIDs never appear
    in rows — overview only.
  - Line 2: honest state copy (ladder below), optionally trailing agent
    glyphs for open sessions (`✦⌁`).

Sort: needs-you first, then active (`TaskLifecycle::is_active()`), then
ready, then done/failed, each bucket by `updated_at` descending.

New-task treatment: rows created by a coordinator (or manually) slide in with
a quiet highlight ring keyed off store insert time; the ring decays after the
row is first focused or 60s. **Focus never moves** — new tasks appear in a
region focus does not occupy.

Collapse: the rail collapses into the existing 34px spine. The ◈ icon stacks
up to three status micro-dots (worst state first) so a needs-you peach dot
reaches peripheral vision at zero width cost. Auto-collapse under ~1200px
window width. Intermediate width (~150px): rows shed the title, keep
dot + issue key.

Selection: rail rows fire the existing `Event::TaskSelected` /
`enter_task_context()`. With the chip row gone, the rail is the only task
selector, dissolving the "which component owns selection" ambiguity.

## The strip

- **Stamp** (shipped, kept) is the "where am I" anchor:
  `WORKSPACE / Sessions` or `TASK · TRU-104 / Durable task store`. The branch
  readout at the strip's right edge follows the context.
- **Overview** is a permanent leftmost pseudo-tab in task context — the
  task's parent page, present even (especially) when the task has zero
  sessions. It cannot be closed. `⌘0` focuses it.
- Session tabs are filtered to the current context (shipped behavior via
  `task_tab_indices`). `⌘1–9` stays context-relative.
- Add button copy tells the truth: `+ Session` (workspace context),
  `Start with ▾` (task with zero sessions), `+ Continue ▾` (task with
  sessions — keeps the shipped handoff/briefing flow).

## Task overview (canvas when no session is focused)

State in plain words, then metadata rows: STATE, ISSUE (linked), BASE,
BRANCH, WORKTREE, CREATED BY (provenance, e.g. "Coordinator session ·
today 14:02"), STOP WHEN, sessions and conversation history with
Open/Resume, latest handoff. Primary verb `Start with ▾` / `+ Continue ▾`;
secondary `Archive`; `Delete worktree…` danger-styled and buried in task
settings. Running tasks show the architecture doc's progress fields (phase,
latest update, elapsed, changed files, verification) with an unambiguous
**Go to session** action.

## Honest state ladder

Rendered as the row's second line and the overview's STATE. Mapped onto
`TaskLifecycle` plus `TaskAttentionReason` (currently modeled, unrendered —
Slice 5's rendering target):

| Copy | Backed by |
|---|---|
| `Preparing…` | Preparing |
| `Ready · not started` | Ready + zero sessions (a valid resting state) |
| `Session open · no brief` | session up, GitTerm **knows** the objective was not delivered |
| `Session open · delivery unknown` | session up on a non-adapted harness; GitTerm cannot observe delivery |
| `Briefed` → `Working · 12m` | Queued / Running |
| `Needs you — <ask>` | WaitingForInput / RequiresInput (name the ask) |
| `Failed · <what>` | Failed / ExecutionFailed |
| `Done · review` | Completed / CompletedUnread |

Never render `Ready · 0 sessions` — it states a deficiency count instead of
a valid posture.

## Task creation defaults

- Base branch preference: `develop`, then `main`, always allowing an
  explicit alternative — **including when the task is created from inside
  another task's context**. The current task's branch is used as base only
  when explicitly requested; the new task is always a navigation sibling
  under the repository (product rule 7), with any base relationship
  recorded in the overview's BASE row, never as nesting.
- **CREATED BY provenance** is captured at creation time and stored on
  `TaskRecord` (creating session id + harness label + timestamp), because
  provenance cannot be retrofitted. The field is optional with graceful
  fallback copy: "Created manually" for direct creation, and harness +
  timestamp alone when caller-session identity is not yet threaded through
  the task-control tools. The overview row renders whatever is known — the
  richer identity plumbing must not block the rail shipping.

## Verbs

Four verbs, four affordances, four labels — no control performs a verb its
shape doesn't announce:

- **Close view** — tab ×, free.
- **Stop agent** — ■ in the session or overview, confirmed (existing
  close-prompt modal).
- **Resume / Continue with…** — overview and the strip's add button.
- **Archive task** — overview and rail row menu only, never near tabs.
- (*Delete worktree…* — fifth, guarded, task settings only.)

## Keyboard

Two persistent axes plus a switcher — deliberately no third number row
(tasks are unordered and renumber constantly; digits defeat twenty of them):

| Keys | Action | Status |
|---|---|---|
| `⌃1–9` | Switch workspace | shipped |
| `⌘1–9` | Switch session within context | shipped |
| `⌘T` | Task switcher — fuzzy, MRU-first, attention pinned; `⌘T ↵` toggles the last two contexts | new |
| `⌘↑` / `esc` on empty canvas | Up to workspace context (General) | new |
| `⌘0` | Overview of current task | new |
| `` ⌃` `` | Jump to next thing that needs you (extend to tasks) | shipped, extend |
| `⌘⇧A` | Attention panel | shipped |
| `⌥⇧1–9` | Launch agent preset (task-aware) | shipped |

## Vocabulary

- **Task**, **Session**, **Agent** (user-facing word for harness; protocol
  names stay out of primary UI), **Overview**.
- "Coordinator" is provenance copy only ("Created by Coordinator session"),
  never a mode, badge, or object.
- Copy never says "tab". "Sprite" stays internal.

## Engineering delta

Mostly deletion and promotion; these survive verbatim: the context stamp,
`TaskSelected` / `enter_task_context`, tier-2 tab filtering, the Tasks
sidebar row rendering, the attention panel, the `+ Continue` handoff flow.

1. Remove `view_context_bar()`; single-row strip; update the 66px webview
   bounds constant.
2. Promote the Tasks sidebar to the rail: pin/collapse state, General row,
   filter pills, sort, new-task ring, per-repo scoping (cross-workspace
   triage moves to the ⚡ attention panel, per the architecture doc's
   division of labor).
3. Spine ◈ status micro-dots for the collapsed state.
4. `⌘T` switcher palette; `⌘↑`, `⌘0`.
5. Slice 5 wiring renders into the ladder above (lifecycle events +
   `TaskAttentionReason` → rail rows, spine dots, ⚡ panel).
6. **Close the worktree leak** (independent of this design):
   `ensure_local_workspace_for_chat` matches by `cwd.starts_with(ws.dir)`,
   so a cwd under `~/.config/gitterm-v5/worktrees` matches nothing and can
   mint a peer workspace chip. Guard: any cwd inside the managed worktree
   root resolves to the owning task's repo workspace and task context
   instead of creating a workspace.

## Fallback position

If usage shows the rail pinned-open rarely, the retreat is
Deck-with-spine-dots (anchor stamp + `⌘T` + micro-dots, no pinned rail).
Nothing above paints us out of that corner — the switcher, stamp, ladder,
verbs, and vocabulary all carry over unchanged.

## Clarifications (decided 2026-08-19)

1. **Strip shape:** one ~33px horizontal row; the two-line stamp is a
   compact inline block within that row, not a second row.
2. **Sidebar region:** for the first implementation the rail and the
   Git / Files / Agent / Chats / Plans panels occupy the same sidebar
   region and are mutually exclusive. The spine micro-dots keep task
   awareness alive while another panel is open — that is the designed
   mitigation, not an accident. Two side regions may be revisited after
   real use.
3. **⌘T scope:** General + tasks of the active repository workspace only.
   `⌃1–9` remains the workspace axis; cross-workspace triage belongs to the
   ⚡ attention panel. (`⌘T` is free today — `⌥⇧T` covers new terminal.)
4. **Task-from-task base branch:** defaults stay `develop`, then `main`;
   the current task's branch only on explicit request (see Task creation
   defaults above).
5. **Non-adapted harness copy:** `Session open · delivery unknown`;
   `Session open · no brief` is reserved for known non-delivery.
6. **CREATED BY scope:** capture-at-creation is in this implementation
   (it cannot be retrofitted); full caller-session identity through the
   task-control tools may land incrementally behind the optional field
   (see Task creation defaults above).
