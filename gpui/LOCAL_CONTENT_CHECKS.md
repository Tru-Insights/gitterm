# Local retained content — TRU-274 Stage 4 PR 9

Files opens `.md`, `.html` and `.htm` in a workspace's center dock. Build with
`--features gitterm-gpui-shell/excalidraw` to also open `.excalidraw`. Opening the
same path again focuses its existing tab; the dock close button destroys that
surface. Open it again from Files to create a fresh surface. Tabs are ephemeral
and are not restored as documents after restart. Other file types and Git-status
Review opens retain their existing behavior.

Reads use a captured workspace root and core's canonical containment check.
Missing, unreadable, invalid, oversized and escaping files show an inline error.
The leaf `Document::plan` adapter additionally uses core's plans path policy with
an explicit plans/docs root. Local Markdown under plans/docs can be opened
through Files.

The dedicated Plans Viewer now uses `assets/plans-viewer.html` in that same
retained WebKit host, including Iced's TOC, section toggles, Collapse/Expand,
Markdown drop, theme and 40px Back to Terminal header. The Work sidebar opens
`.plans/` and `docs/` rows in this viewer; other document rows and Files keep
the ordinary file viewer. Core's `list_documents` is shared with Iced's routes.
GPUI supplies inert JSON for the captured source/selection rather than starting
another HTTP listener. No document is copied or written. The shared asset's
CDN imports remain unchanged. Workspace changes resolve the saved workspace
home, independent of a session checkout. Workspace/tab changes dismiss Plans as
Iced does; explicit dismissal restores prior focus.

Focused acceptance tests: `scripts/cargo.sh test --manifest-path gpui/Cargo.toml
-p gitterm-gpui-shell -j 4 plans_viewer`. Page control tests execute the actual
shared JavaScript functions in macOS JavaScriptCore without a window or network.

The content coordinators show at most one native surface: Plans covers ordinary
file content while open. Component deferred
popup registration covers More, dock menus and nested popovers; dialogs/sheets,
the Add folder picker and dock motion also hide native content. The component
root paints the single modal dim layer over the snapshot. The shell's dialogs
have no entrance/exit animation so their active lifetime matches that dim layer.
Snapshots, preparation and navigation completions retain generation checks.

## Coordinator's live check (not run by the worker)

Use two synthetic workspace folders and a fresh absolute config directory under
`/private/tmp`. Do not reuse a running instance's config, browser profile or app
bundle. From the repository root, build using:

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-s4pr9 RUST_TEST_THREADS=4 nice -n 10 scripts/cargo.sh build --manifest-path gpui/Cargo.toml --workspace --locked -j 4
```

Launch the built preview with `GITTERM_GPUI_CONFIG_DIR` set to the fresh directory
and `--workspace /absolute/fixture-a --workspace /absolute/fixture-b`. The worker
has not launched this binary. Omit `nice` if the environment denies setpriority.

1. In Files, open Markdown containing a Mermaid diagram, HTML and HTM. Check
   rendering, scroll and text selection. Reopen each path; check that it focuses
   its existing center tab and retains scroll. Switch workspace and return;
   verify that only the selected workspace's content is visible.
2. Close a content tab with the dock close button. Reopen from Files; verify a
   new surface and no old scroll/DOM. Repeat close/reopen while a document loads.
   Open an unsupported `.rs` file and check its previous behavior is unchanged.
3. Open `.plans/plan.md` and `docs/guide.md` from each fixture with distinct
   headings. Verify the heading belongs to that workspace. Try a symlink to an
   outside Markdown file: it must show an error without exposing its contents.
   Try an internal symlink, invalid Excalidraw and a file over the inline size
   limit. Delete a file between listing and open; verify an inline error.
4. Over visible content, open More, its Show pane submenu, each dock ellipsis
   menu, Keyboard shortcuts and New task. In New task open Choose base and
   Stopping boundary. Verify menus remain usable, popups add no modal dim, and
   dialogs dim HTML exactly once. Cancel nested controls and then the dialog;
   content must return only after the last cover closes, with usable focus.
5. Open Add folder; cancel and then repeat with a fixture folder. Check the
   picker covers content throughout and cancellation restores the selected pane.
6. Resize the window and dock dividers; drag/group/split content tabs. Check
   native bounds include neither dock headers nor other panes. Move between
   displays/scales (including Retina). Zoom each split content pane in turn,
   then zoom a non-content pane; verify its content is selected or all native
   surfaces are hidden respectively. Repeat menu/dialog checks during motion.
7. Switch tabs/workspaces or close the tab while a snapshot/navigation is pending.
   Verify no late callback restores old content. Snapshot failure, wrong scale,
   timeout and nested stale completion have deterministic headless tests; there
   is no production failure-injection control in the shell.
8. Quit the isolated preview with content open; verify its child native surface
   detaches. Repeat with the Excalidraw feature enabled and a valid drawing.

Native WebKit navigation, clipping, capture pixels and OS focus still need this
live evidence. Headless tests use a fake host and do not establish those claims.
