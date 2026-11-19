# Native fixture review (TRU-274 stage 4a)

`gitterm-gpui --review gpui/crates/review/fixtures/showcase` opens a standalone
native review window. This is a display-only fixture demo: no git calls, runtime
status service, settings access, writes, PTYs or chat processes. Build with the
GPUI workspace's pinned toolchain via `scripts/cargo.sh`.

The file list consumes `gitterm_core::status::GitStatusSnapshot` directly,
retaining staged, unstaged and untracked order and status letters. Patches are
supplied separately by `(Group, path)`, so staged and unstaged changes to the
same path do not overwrite each other. Source errors remain visible.

The selected patch uses gpui-component's `DiffState` and unified `Diff`:
virtual scrolling, syntect syntax highlighting, hunk metadata, old/new line
numbers, colored additions/deletions, selectable text and horizontal scrolling.
Binary files show a text placeholder; renames show `old → new`; additions and
deletions retain their `/dev/null` side. Above 2,000 patch lines or 256 KiB
(the Iced review's initial per-file limits), text stays collapsed until **Show
anyway**. The patch is parsed only after expansion, with parse errors displayed.
Malformed or multi-file patches cannot silently show the wrong file.

Click a file or use **Option+↓ / Option+↑** for next/previous (clamped at the
ends); these shortcuts also work with the diff body focused. **Option+V** or
**Mark viewed** toggles the selected entry's viewed mark. Marks are in memory
for this fixture session. `ReviewModel::replace` (also exposed on `ReviewView`) preserves marks, expansion and
selection across reorder, clearing viewed/expanded state for any entry whose
exact patch changes. Removing an entry drops its state. There is no persistence
or patch hashing, and no runtime adapter is introduced at this stage.

## Synthetic fixture format

A directory contains `review.json` and one UTF-8 unified patch per entry:

```json
{"files":[{"group":"staged","path":"src/hello.rs","status":"M","patch":"modified.patch"}]}
```

Groups are `staged`, `unstaged`, `untracked`. Paths/statuses must be nonempty;
unknown manifest fields, duplicate `(group, path)` entries, missing patch files
and patch-file paths outside the fixture directory fail with context. Patch
paths are relative and cannot contain parent components or escaping symlinks.
The manifest is local demo input, not a proposed runtime wire contract.

The bundled showcase is entirely synthetic. It includes modified Rust with
context, a pure rename, separate staged/unstaged versions, binary, deletion,
untracked addition, and a 20,000-line Rust addition (`large.patch`). Recreate the
large patch with `python3 gpui/crates/review/fixtures/generate_large.py`.

## Headless verification

From the repository root (no app launch or OS input):

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-review4a scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml --all -- --check
CARGO_TARGET_DIR=/private/tmp/gt-target-review4a scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --workspace --locked --all-targets -j 4 -- -D warnings
CARGO_TARGET_DIR=/private/tmp/gt-target-review4a scripts/cargo.sh test --manifest-path gpui/Cargo.toml --workspace --locked -j 4
CARGO_TARGET_DIR=/private/tmp/gt-target-review4a scripts/check-gpui-deps.sh
```

Use `nice -n 10` before the script when permitted. The root Iced lockfile is
unchanged; only the GPUI lockfile includes this crate and the dependency-free
shared core.

Model tests cover grouping/status, same-path versions, viewed invalidation,
reorder/removal, navigation boundaries, source errors and edge cases. GPUI tests
use the headless platform for keyboard routing, parser status/counts, large
expansion and visible parse errors. The 20k-line budget includes fixture loading,
parsing and initial virtual layout (<5 seconds); background syntax completion
is awaited and reported separately because it does not block the UI. A marker
slot probe counts actual constructed code rows (<100 initially, <200 including
a jump to line 20,000 while syntax is pending, bounded to one second), using
the same retained DiffState as the review view.
Interactive visual, accessibility and trackpad feel checks remain manual; no
GUI screenshots or live process manipulation are part of this verification.

Verified on arm64 macOS on 18 November 2026: workspace formatting, all-target
Clippy (`-D warnings`), workspace tests (97 passed, including 11 review tests)
and the unchanged dependency boundary script exited 0. The 20k probe measured
180 ms parsing, 222 ms through first layout (38 rows), and an 85 ms tail jump.
Full background syntax coloring completed at 23.2 seconds in this unoptimized
debug run; initial display and scrolling remain available while it prepares.
No release performance or interactive visual check is claimed.
