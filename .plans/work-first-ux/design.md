# Work-first navigation: the workspace is the piece of work

**Status:** design proposal, 2026-10-30, fifth revision. Tracey's framing:
"the workspace *is* the piece of work, and a repo is just a repo"; GitTerm's
mistake is assuming a workspace is one folder. The fifth revision adds the
branches and worktrees a workspace owns, and a Changes view across them
(screen h), her biggest pain. Mockups: `mockups.html` in this folder
(screens a–h). No ticket yet.

**Builds on:** `task-navigation-design-decision.md` (rail, strip, stamp, ⌘T),
`chats-panel.md` (index, resume-as-tab, scopes), `agent-handoff-ux.md`
(delegations with parent identity; summaries, never transcript copies),
TRU-155 (chat digest), TRU-158 (latest line).

## Problem

She already uses workspaces as pieces of work: `acme` is the new cloud
migration, `ops-platform` is a week-long workbench of ~20 issues,
`data-pipeline` has been open for weeks only to mark where she
stopped. GitTerm binds each to one folder, so the new cloud work's 190 edits in
infra are invisible from infra, `blobfs` and `PRJ-102` find
nothing (`chats::matches_query` sees title, folder, branch), the workbench
is buried under its 170 worker and review sessions, and closing a workspace
(`Event::WorkspaceClose`) throws its tabs away, so the only bookmark is a
bar slot.

## The model

No new nouns. Two existing words, each meaning one thing.

| Object | What it is | Where it lives |
|---|---|---|
| **Workspace** | A piece of work: name, colour, its chats and tabs (workers and reviews folded under coordinators), a **list of repos**, a home folder for new chats, a state (open, parked, done). | `workspaces.json` entry, as today plus `repos`, `home`, `parked`, `done_at`. |
| **Repo** | A checkout folder: console, run command, dev server, bottom terminals, env. Shared by every workspace that lists it: one console and dev server per repo. | New `repos` section of `workspaces.json`; runtime `RepoState` in `App`. |
| Chat | A conversation; belongs to exactly one workspace. Root chats are coordinators; children fold under them. | Transcript + per-chat facts file. |
| Task | Unchanged: one repo, branch, managed worktree, lifecycle. | tasks.json. |

Rules:

- **A workspace starts with the folder it was opened from** as its home and
  first repo, so single-repo work looks exactly like today. Repos join as
  the work touches them: when a chat in the workspace writes to, or starts
  a worker in, a folder outside its repos, that folder is added (auto, with
  an undo toast; reads never add). Repos can also be added or removed by
  hand. Order is join order; the home stays first.
- **A repo is identified by its checkout dir**, grouped by git common dir
  for "workspaces touching this repo" and search. This matters: her
  `gitterm-v5` workspace *is* a linked worktree, and `api-server-ops-494`
  was one; a worktree checkout is shown as `api-server · ops-494`. A
  non-git folder (`acme`, 112 sub-repos; `watch alert app`) is a valid
  entry with console and terminals and no git panel.
- **Task worktrees are not repos.** A task's worktree is that repo's
  execution dir and stays the task's business (overview, worker tabs),
  per the task-navigation decision. The aggregated bottom panel does not
  list it.
- **A repo exists while at least one workspace lists it** (open, parked or
  done). Its settings (run command, env) persist in the `repos` section;
  its runtime (console process, PTYs) exists only while an open workspace
  lists it. "Open a repo alone" is a one-repo workspace, i.e. today.
- **Env lives on the repo**, with optional workspace overrides. Today's env
  is exactly two org-level sets (Tru-Insights on gitterm-v5; Acme on four
  workspaces), so repo-level is the truth. A tab's env is the env of the
  repo it runs in (terminal cwd, chat cwd), then workspace overrides. A
  workspace spanning orgs is therefore fine per tab; the only real conflict
  is a coordinator in a non-git home (acme) running `gh` against both
  orgs, which is the per-command-token situation she has today.
- **Remote.** A repo carries its machine (`WorkspaceLocation`). A workspace
  may list repos on two machines; its bar group is its home's machine.
  Bottom terminals are local PTYs today (`Event::BottomTerminalAdd`), so a
  remote repo contributes no bottom tabs until agentd terminals exist; it
  still contributes its chats and console state as today.
- **Bottom panel in a workspace** shows the consoles and bottom terminals
  of all its local repos together, labelled and coloured by repo. A new
  terminal opens in the current repo with a picker for the others.
  Ownership is the repo's; the workspace aggregates. She never `cd`s in
  GitTerm terminals, so a tab's repo is fixed at creation.
- **× parks.** `WorkspaceClose` keeps the tabs (`dir`, `repo_dir`,
  `chat_session_id`, `session_uid` are already persisted) and takes a note
  prefilled from the latest line, with an optional wake. Running workers
  and dev servers keep running; idle tabs are released and restored on
  reopen by the same path that restores workspaces at startup. The old
  closed-workspaces list becomes the Parked list, with notes. **Done** is a
  separate, deliberate action; done workspaces leave the parked list but
  stay findable (Chats Everywhere; "workspaces touching this repo", folded).
- **Branches.** A workspace owns the branches and worktrees its work
  touches, derived: (1) checkouts a chat edited or launched a worker in,
  from the facts' `(repo, checkout, branch)` touches (TRU-146 resolution),
  which catches `live-pipeline-standby` with no key; (2) task
  worktrees created by its chats (`created_by`); (3) worktrees in its repos
  whose branch contains one of its issue keys; (4) manual Add/Remove,
  persisted. A word match on the workspace name is only a "Maybe"
  suggestion. The other 12 of api-server's 15 worktrees never appear in
  Changes; they live under the Worktrees fold with hygiene flags.
- **Chats.** A chat belongs to the workspace it was started in (facts
  record `workspace_id`); older transcripts attach by cwd under the
  workspace's repos as today. "Move to workspace…" rewrites that one field.
  "New workspace from this chat" (the old spin-out) creates a workspace
  with chosen repos and a seeded chat; the workbench keeps its chat.
- **Bar and keys unchanged.** One kind of chip, machine groups, `⌃1–9`
  left-to-right as shipped. `⌘T` adds parked workspaces (they are off the
  bar, so the keyboard needs a route) above the active workspace's tasks.

### Files

`workspaces.json` (additive, serde defaults; `active:false` already exists):

```text
workspaces[]: + repos [ "<dir>", … ]  home "<dir>"  parked? { note, parked_at, wake_at? }
              + done_at?  + env_overrides {}        (env moves to repos[])
              + branches_added [ {repo, checkout} ]  branches_ignored [ … ]
repos[]:      { dir, location, run_command?, env {}, bottom_terminals [] }
```

Per-chat facts, `<config>/chat-meta/<backend>-<session_id>.json`, written
by the hosting tab from frames it already renders, never by parsing a
transcript: `workspace_id, issue_keys[], repos_touched[{dir, edits}],
checkouts[{repo, dir, branch, edits, last}], children[{session_id, kind,
delegation_id?, task_id?}]`. `checkouts` is the branch ownership source.

## Screens

**a · Bar.** Same bar. Chips are workspaces, which are now pieces of work:
`acme` renamed "Cloud migration", `data-pipeline` parked instead
of occupying a slot, `⏸ n parked` where the closed list was. Hover lists a
workspace's repos. No dots, no folding, no second zone.

**b · Inside a workspace.** Stamp `WORKSPACE · Cloud migration · home
acme`. Rail sections: Chats (coordinators with fan-out folded), Repos
(each with console or dev-server state, `+ add repo`), Issues. Bottom
panel aggregated across repos with a `+ ▾` picker. Selecting a repo row
makes it current (git panel, strip terminals, default for new terminals).

**c · Chat history.** This-workspace scope lists coordinators only, with
the TRU-158 latest line, issue keys, repos touched and fan-out counts;
children expand under the parent. Search covers issue keys, repos touched
and children's titles and says what matched. "Also touched these repos"
lists coordinators from other workspaces.

**d · Park.** × opens the note (prefilled), wake, and what keeps running.
Chip leaves the bar; `⏸ n parked` opens the Parked list (notes, repos,
age, wake, running count) with a Done filter. Preview: note, repos, last
messages, Resume (restores tabs), Mark done.

**e · New workspace.** Goal first: issue keys and repo names detected as
chips; home defaults to the nearest folder containing every named repo,
else last used; Start creates the workspace with those repos and opens the
chat with the goal as first message. "Open folder…" still exists and
gives a one-repo workspace, exactly today.

**f · Growing.** (1) Auto-add toast: "bun-api added to this workspace's
repos · Remove · Rename workspace…". (2) New chat in the workspace is the
strip's `+`; "seed from last chat" is an option on it (record fields +
last chat's handoff + TRU-155 digest, a summary). (3) New workspace from
this chat: name, keys from recent turns, summary from the digest filtered
to those keys (or drafted by the coordinator on request), repos for those
keys, children to move. Both workspaces show the link under Related.

**g · Opening a repo.** Picking a folder that some workspace already lists
shows "Workspaces touching ops-platform" (open, parked with note and
Resume, done folded) before "New workspace here".

**h · Changes across the work.** The Git sidebar gains a scope, `This
checkout | Workspace`, keeping its two modes. Workspace + Changes: per
repo, only the owned branches, each with checkout, uncommitted count,
ahead/behind base, pushed or not, PR/CI when the existing discovery has
them (never fetched here), owner (coordinator, subagent, worker, "edited
by this chat") and last activity; the repo header shows the numbers' age;
a "Maybe" fold for name matches with Add/Ignore. Click opens the shipped
diff panel with two sources: Uncommitted (today's) and **vs base** (branch
against its merge-base with the task's base, else the upstream's
merge-base, else the repo default). Workspace + Worktrees: every worktree
per repo, the work's first, the rest folded with hygiene flags (stale 30d
and clean; merged, from one `git branch --merged` per repo when the fold
opens; folder gone; /tmp; task-owned, Open task only; dirty main checkout
on an old branch). Cleanup is batched and confirmed by a sheet naming the
exact commands (`worktree prune`; `worktree remove` only for clean, merged,
non-task trees; never `--force`). The rail's Workspace perspective shows a
one-line Branches summary linking here. The `This checkout` scope is
today's panel unchanged.

**Polling for h.** Only owned checkouts are polled, not every worktree.
Per refresh: per repo one `git worktree list --porcelain` (already run),
per owned checkout one `git status --porcelain=v2 --branch` (dirty counts
and upstream ahead/behind in one call) and one `git rev-list --left-right
--count base...HEAD`; ~36 short processes for the new cloud workspace, run
sequentially per repo in one background `Task` with a timeout, applied by
event, never in `update()`. Schedule: the current checkout keeps the
per-tab adaptive tier (`next_git_poll_interval_ms`, 5–30 s); other owned
checkouts use the slow tier (15–30 s) and only while the Git sidebar is
visible in Workspace scope; otherwise a stat of `index`, `HEAD` and
`logs/HEAD` under each checkout's gitdir gates whether git runs. Status
output over a cap (the 249-dirty main) is counted, not stored. Rows show
the age of their numbers.

## The refactor, weighed

`struct Workspace` (main.rs ~5138) carries `dir`, `location`, `console`,
`bottom_terminals`, `active_bottom_tab`, `env`. Uses in main.rs: `.console`
54, `.bottom_terminals` 24, `.env` 20, `ws.dir` 62, `.active_bottom_tab`
15, `.location` 65. Extracting `RepoState { dir, location, console,
bottom_terminals, env }` into `App.repos` with `Workspace { home: RepoId,
repos: Vec<RepoId>, current: RepoId, … }` touches ~240 sites, most of them
one-line (`ws.console` → `self.repo(ws.home).console`). The real cost is
borrow structure: code that holds `&mut Workspace` and then reaches the
console must be split, and the active bottom tab becomes `(RepoId,
BottomPanelTab)`. `ws.dir` stays as the home dir, so most of its 62 uses
are untouched. Estimate: one focused PR for the extraction with behaviour
identical (every workspace has exactly one repo), then the aggregation on
top. Everything in steps 1–3 below is useful without it.

## Mapping onto existing code

| Reused as is | New |
|---|---|
| `Workspace` name/colour/tabs, bar, machine groups, `⌃1–9`, `WorkspaceConfig` tabs persistence, startup tab restore, `closed_workspace_configs` and its reopen path (~10841) | `repos[]`/`home`/`parked`/`done_at` on `WorkspaceConfig`; `WorkspaceClose` keeps tabs and takes a note; Parked list with notes and Done filter; `AttentionReason::WorkspaceWoke` |
| `ChatIndexEntry` (`title`, `latest`, `repo_root`, `in_workspace`), index, preview, resume-as-tab, reopen-on-resume | `ChatIndexEntry.facts: Option<ChatFacts>`; `in_workspace` over the repo list; `matches_query` over keys, repos, children |
| `DelegationParent.chat_session_id`, `DelegationChild`, `TaskRecord.repository`/`issue`, `SubagentEvent`, roster strip | `work.rs`: facts types, root/child resolution, auto-add rule, seed composer (pure, unit-tested); chat tab writes facts |
| `ConsoleState`, `BottomTerminal` (has `cwd`), console view, `WorkspaceLocation` | `RepoState` registry; aggregated bottom panel; `+ ▾` picker; env on repo with overrides |
| `AgentSubmitPrompt`, `create_tab_for_workspace`, TRU-143 chips, `chat_summary::ChatDigest`, `⌘T` switcher | New-workspace sheet; New-workspace-from-chat sheet; "Move to workspace…"; parked rows in `⌘T` |
| Git sidebar `GitViewMode::{Changes, Worktrees}`, `GitWorktreeEntry` (dirty counts, prunable, `worktree_task`), `view_worktree_item`, `view_diff_panel`, `GitSelection::Worktree`, `worktree_follow` (TRU-146), `next_git_poll_interval_ms`, PR discovery | Git scope `This checkout | Workspace`; `GitWorktreeEntry` gains ahead/behind base, upstream state, last activity, owner; diff source `BranchVsBase`; per-workspace status poller; hygiene flags and confirmed cleanup sheet; facts `checkouts[]` |

## Decisions (rewritten for this model)

Settled by Tracey: workspace = piece of work; combined bottom panel (try
it); she never `cd`s in GitTerm terminals; × parks.

1. **Repo identity.** Checkout dir, grouped by git common dir (recommend;
   gitterm-v5 is itself a worktree) vs git common dir only.
2. **Auto-add repos.** Auto on write or worker launch, undo toast, reads
   never count (recommend) vs one-tap confirm. Adding spawns nothing, so
   the cost of a wrong add is one toast.
3. **Parking and running work.** Keep workers and dev servers running,
   release idle tabs, show `n running` on the parked row (recommend) vs
   stop everything vs keep every tab alive.
4. **Wake.** None by default, presets offered; a wake returns the chip
   with the attention ring, never open tabs.
5. **Done.** Separate action in the parked list and the workspace menu;
   done entries stay in `workspaces.json` and are findable, never deleted
   by GitTerm (recommend). Delete stays a guarded, separate verb.
6. **Env placement.** On the repo with workspace overrides (recommend);
   migration copies each workspace's env to its home repo. Alternative:
   keep per-workspace env and add nothing.
7. **Keyboard.** `⌃1–9` unchanged; `⌘T` gains parked workspaces (recommend).
8. **Chips.** Plain, as today; hover shows repos (recommend). Dots, dim
   and fold all dropped.
9. **Closed touched repo in the bottom picker.** Not applicable now: a
   workspace's repos are always its own entries; the picker lists them
   all, and a repo with no console yet just starts one.
10. **Backfill.** Facts are forward-only plus an explicit, bounded,
    off-thread "Index older chats" action (recommend); the new cloud and
    ops-platform sessions are the ones she wants findable now.
11. **Files.** Facts per chat, repos as a section of `workspaces.json`
    (recommend; same last-writer-wins caveat across instances as today).
12. **Naming.** A workspace keeps its folder name until renamed (inline
    on the chip or rail header); the auto-add toast offers Rename.
13. **Branch ownership.** Facts touches + task worktrees + issue-key match
    + manual, with name-word matches as "Maybe" only (recommend) vs
    key match only (misses `live-pipeline-standby`) vs every
    worktree in the repo (shows 12 unrelated ones).
14. **Where Changes lives.** Git sidebar scope toggle reusing both modes
    (recommend) vs a rail perspective vs a canvas overview page. A wider
    overview page can come later off the same data.
15. **Base for "vs base".** Task base, else upstream merge-base, else repo
    default (recommend); shown on the row so it is never a guess.
16. **Polling tiers.** Current checkout adaptive, others slow and only
    while visible, stat-gated otherwise (recommend) vs one timer for all.
17. **Hygiene actions.** Prune gone and remove clean-merged-non-task, each
    behind a sheet naming the commands (recommend) vs flags only in v1.
    Task worktrees always go through the task flow.
18. **PR/CI on rows.** Only from the existing discovery cache (recommend)
    vs a `gh` call per branch.

## Incremental path (each step useful on its own)

1. **Facts + search + fold.** Chat tabs write facts (repos, keys,
   children, and `checkouts` with branch); index merges them;
   `matches_query` covers keys, repos, children; Chats groups by root.
   Fixes `blobfs` / `PRJ-102` and the buried workbench. No model change.
2. **Changes across the work (read-only).** Git scope toggle; owned
   branches from facts + tasks + key match; status poller; `vs base`
   diff; Worktrees fold with flags only. The repo set is derived from the
   facts of the workspace's chats, so this needs neither the persisted
   repo list nor the registry. Moved ahead because it is her biggest
   pain and it is pure reading: no git writes, no `Workspace` change.
3. **× parks.** `WorkspaceClose` keeps tabs, takes a note; Parked list
   with notes and Done; `⌘T` parked rows; wake.
4. **Workspace repos (list only).** `repos[]` + `home` persisted; auto-add
   from facts; This-workspace scope spans the list; "workspaces touching
   this repo" on open-folder; New workspace from this chat; Move to
   workspace; manual branch Add/Ignore. Bottom panel still home-only.
5. **Repo registry.** Extract `RepoState`; env on repo; behaviour
   identical with one repo per workspace. One PR, no UI change.
6. **Aggregated bottom panel** with picker and repo-coloured tabs; Repos
   section in the rail with console state; current-repo selection.
7. **New-workspace sheet** (goal first, detection, home default) and the
   confirmed hygiene actions (the only git writes in this design).

## Migration and edge cases (from `workspaces.json`, read-only)

Each workspace becomes `{repos: [dir], home: dir}`; `env` moves to the
repo entry; `active:false` entries become parked with an empty note
(she adds notes as she resumes). 26 workspaces today, 9 open. Found:

- **Folder-of-repos homes:** `acme` (112 sub-repos), `GitRepo` (175),
  `model-game` (1). Fine as non-git repo entries; their sub-repos join the
  workspace only as the work touches them. `acme` is renamed "new cloud
  migration" by her, not by migration.
- **Worktree homes:** `gitterm-v5` (open, daily driver) and
  `api-server-ops-494` (parked). Decision 1 keeps them as entries.
- **Dirs gone:** `core-api-msa-revisions`, `video-editor-mb`.
  Parked list marks "folder gone"; resume offers "recreate" as the chats
  dead-cwd rescue does, or Done.
- **Remote:** `Mac Mini:remote` (home `/`) and `Mac Mini:web-portal`.
  Entries with a remote location; no bottom tabs.
- **No duplicate names or dirs** today, so no env conflict on a shared
  repo. If two workspaces later list one repo with different env, the
  second's difference becomes a workspace override.
- **Bottom terminals** all sit inside their workspace dir today, so every
  existing one maps to its home repo.
- **Tabs** already carry `dir`, `repo_dir`, `chat_session_id`,
  `session_uid`; parking needs no new tab fields.

## Prior plans: kept and revised

- **Task-navigation decision.** Kept: rail, strip, stamp, verbs, `⌘T`,
  tasks as siblings under a repo, worktrees never as bar peers or repos.
  Revised: the workspace is a piece of work spanning repos, not "one
  logical repository"; "coordinator is provenance only" becomes "root
  chats organise history inside a workspace". `⌘T` adds parked workspaces.
- **Chats panel.** Kept: machine → workspace → chats, scope rings, resume
  follows the chat home, read-only, never parse a full transcript.
  Revised: "This workspace" spans the workspace's repo list; the index
  gains the facts file beside head and tail reads.
- **Agent handoff UX.** Nothing revised; summaries-not-transcripts is the
  rule the seeded new chat and New-workspace-from-chat follow.
- **TRU-146 worktree follow.** Kept and extended: the same resolution of
  "which worktree is the agent in" now also feeds the facts' `checkouts`,
  and the Git panel's scope widens from one repo to the workspace's repos.
  Tasks still own their worktrees (`task_worktree.rs`); hygiene never
  removes one.
- **Earlier revisions of this document.** The Work noun, pinning, the
  two-zone bar, Work records, repo chips with dots, and the pin nudge are
  dropped; what they bought is now carried by the workspace itself.
