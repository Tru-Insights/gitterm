# GitTerm GPUI preview (TRU-275 / TRU-280 / TRU-283, parent TRU-274)

This macOS-only app opens a 1440×900 shell with explicit local workspaces and
live Git status. Local retained Markdown/HTML tabs and their coordinator live-check
list are documented in [LOCAL_CONTENT_CHECKS.md](LOCAL_CONTENT_CHECKS.md). Start with `gitterm-gpui --workspace <dir>` (repeatable), or
choose **Add folder…** in the sidebar. With no saved/seeded folders it shows an
empty state with Add folder. Names come from folder names; branches and the Git
pane's staged, unstaged and untracked lists, loading and errors come from the
shared core. Only the selected workspace is polled.

`--sample` explicitly opens the original demo catalog, synthetic chat and Git
status; it cannot be combined with `--workspace` and never loads/saves the local
folder list. Demo layouts live in the config directory's `sample/` subdirectory.
`--terminal` and `--chat <fixture.jsonl>` retain their standalone routes.
Terminal panes still launch `$SHELL` in `$HOME`: workspace cwd awaits TRU-291's
`TerminalLaunch`. The Files tree, task/browser actions and sample-mode chat are
still synthetic. Local mode creates a shell session, with no restored chat or
saved sessions and no remote connections.

GPUI uses only `GITTERM_GPUI_CONFIG_DIR` (absolute; default
`~/.config/gitterm-gpui`). It never imports or writes V6 workspaces/settings.
The shell acquires the preview root's lifetime lock before opening its writer,
seeding tasks, binding servers or starting children. App-initiated Quit (including
⌘Q and the last window closing) runs GPUI's main-thread entity shutdown first,
then the named writer, chat, host/reaper, status/review and terminal completion
barrier, releases the marker and lock, and finally requests platform termination.
Startup errors also drain and release ownership, show a native GPUI error prompt
with the message and preview root, and exit non-zero after dismissal.
Browser shutdown closes tool admission, drains accepted requests, stops engine
sessions even if Chrome has already exited, then gracefully closes owned Chrome.
Each wait is bounded and reports its phase on a timeout; no process signalling
is added by this ownership barrier.

An interrupted or timed-out shutdown leaves `.gpui-shutdown-pending` in that
preview root. Replacement startup checks each recorded launch's PID and birth
time using native process info. It refuses and names any matching live launch;
when none remain it starts normally and logs one recovery line. Unrecorded
surviving descendants cannot be ruled out by this policy. Browser profile
ownership refuses startup only when Chrome's `SingletonLock` names a live process
whose executable is Chrome. Stale or broken singleton artifacts are left intact
for Chrome to handle; GitTerm never deletes them or kills their named processes.
The V6 state root is never locked or recovered by this shell.

Known gap: macOS Dock Quit/logout can bypass the shell's quit route. GPUI's
`on_app_quit` futures get only 200 ms, insufficient for the bounded completion
barrier. Entity shutdown still runs, but the process may exit with its marker
present; the next launch uses the identity/profile recovery policy above.

Explicit folders are canonicalized, deduplicated and saved by atomic tempfile
rename to **`local-workspaces.json`**:

```json
{"version": 1, "folders": ["/absolute/local/repo", "/absolute/another/folder"]}
```

CLI folders append to that list; Add folder saves before changing the catalog.
Missing saved folders remain visible as non-repositories, matching the core
collector's existing failure semantics. Invalid JSON, versions, duplicate or
relative paths fail startup with a path-specific error; the file is preserved.
Picker/save errors appear in stderr and an on-screen banner. No active selection,
status snapshots, runtime IDs or sessions are stored in this file.

The repository root is the Iced workspace; `gpui/` is separately locked and
excluded from root workspace commands. Both frontends depend on
`crates/gitterm-core`; GPUI enables its `runtime` feature. The GPUI lock delta
adds **eight packages**: `gitterm-core`, `git2` 0.19, `libgit2-sys`, `libssh2-sys`,
`libz-sys`, `openssl-probe` 0.1, `openssl-sys` and `vcpkg`. The OpenSSL probe
references are disambiguated from the existing 0.2 version. Existing package
versions are unchanged; Tokio was already present and gains the shell's direct
executor dependency. Root `Cargo.lock`, the Iced app and dependency-check script
are unchanged. The separate Wry graphs remain mandatory (see below).

## Stage 1 local adapter (TRU-274)

`gitterm_gpui_shell::workspace_status::WorkspaceStatus::dispatch(Intent, Instant)`
resolves selection indices and workspace/root identities into `Command::Selection`
and addressed `Command::Status` values. It owns one core `SelectionState` and
one core `GitStatusState` per local workspace/root. `status_view` projects core
state into the same read-only facts used by the Git pane. Bar clicks and Ctrl+n
share `SelectIndex`; Git entry, maintenance, wake, context refresh and completions
all go through the core reducers. Selection alone does not force collection.

One application-level `WorkspaceOwner` retains that model, one multi-thread Tokio
runtime, one `StatusRuntime` and its sole result bridge. Completions update the
owner on the GPUI context and notify the shell/panes, including hidden panes.
Dock close/move/reopen does not own, cancel or recreate jobs. A single five-second
maintenance clock drives only the active root, respects loading, adaptive core
backoff and the hidden-Git 15-second minimum, and detects >300-second wake gaps.
`SchedulePoll` never creates another timer. Application quit closes/drains the
shared runner; local blocking Git retains the core's accounted teardown behavior.

Adapter tests replay the local cases pinned by Iced's
`workspace_selection_traces`, `status_poll_visibility_and_reset_traces`,
`input_wake_and_enter_git_reset_traces` and completion/backoff fixtures in
`src/baseline_tests.rs`. They assert exact core commands, addressed roots and all
core fields after replay. The headless shell test
`live_local_status_crosses_application_bridge_and_survives_closed_git_pane`
creates a temporary repository and separate config, adds/selects it, runs the
real runner/bridge, renders and checks the Git pane's staged/untracked facts,
then collects a later filesystem change through maintenance. It uses GPUI's test
platform, no native application window or OS input.

Stage 1 PR 6 verification on arm64 macOS (18 November 2026): GPUI formatting,
all-target Clippy and the dependency boundary check exited 0. The unfiltered
workspace tests exited 101 solely for the known terminal test
`find_shortcuts_edit_navigate_close_restore_focus_and_export_callback` (PTY gate
deadline); it is unchanged. Excluding that one test, **102 tests passed**, exit 0.
Root `test --workspace` (**1,406 passed**, 14 ignored), Clippy with `-D warnings`
and formatting exited 0. Checks used `scripts/cargo.sh`,
`CARGO_TARGET_DIR=/private/tmp/gt-target-s1pr6`, `CARGO_HOME=/private/tmp/gpui-mockup-cargo`
and `-j 4`; `nice` was denied. The real-status evidence is the headless pipeline
above. No GUI, native picker interaction, screen control, VoiceOver or visual
comparison was performed.

For an isolated manual launch (never use the V6 config directory):

```sh
CARGO_HOME=/private/tmp/gpui-mockup-cargo CARGO_TARGET_DIR=/private/tmp/gt-target-s1pr6 scripts/cargo.sh build --manifest-path gpui/Cargo.toml --workspace --locked -j 4
GITTERM_GPUI_CONFIG_DIR=/private/tmp/gitterm-gpui-local-preview /private/tmp/gt-target-s1pr6/debug/gitterm-gpui --workspace /absolute/temp/repo
```

## Toolchain and build

Use the installed **rustup stable toolchain, Rust 1.99 or newer**. This package's
`rust-version` enforces the minimum (the root Iced package declares none). Do not use Homebrew Cargo:
even `rustup run stable cargo` can find Homebrew's `rustc` on PATH.
`scripts/cargo.sh` obtains the installed compiler path from rustup and prepends
that toolchain's binaries before invoking Cargo. It never installs or updates
toolchains. `rust-toolchain.toml` also selects stable for rustup proxies.

From the repository root:

```sh
CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 nice -n 19 scripts/bundle-gpui.sh
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml -- --check
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 nice -n 10 scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --locked -j 4 -- -D warnings
scripts/check-gpui-deps.sh
```

Omit `nice -n 10` when the environment does not permit priority changes.

The separate, ad-hoc-signed bundle is
`$CARGO_TARGET_DIR/GitTerm GPUI.app` (default: `gpui/target/GitTerm GPUI.app`),
with identifier `com.cree8.gitterm.gpui`. It does not replace GitTerm V6.
The pinned stack is gpui-component/gpui-kit 0.11.0 and gpui-pre 0.3.14.
Cargo tree may print repeated `(*)` references; compare distinct package versions,
as the dependency check does.

### Versioned package, install and rollback

Build, verify, and install from the repository root (macOS):

```sh
CARGO_BUILD_JOBS=4 nice -n 19 scripts/bundle-gpui.sh
scripts/verify-gpui-bundle.sh 'gpui/target/GitTerm GPUI.app'
scripts/install-gpui.sh 'gpui/target/GitTerm GPUI.app'
```

If `CARGO_TARGET_DIR` is set, pass its `GitTerm GPUI.app` to the last two
commands. The bundler keeps `--features stt`, uses four Cargo jobs at nice 19,
and verifies the finished ad-hoc signature, arm64 executable, icon, microphone
description and (when available) whisper symbols. A stripped/unavailable symbol
table is reported as unchecked STT; signature and other checks still apply.

`CFBundleShortVersionString` comes from the app package in `gpui/Cargo.toml`
(`0.1.0`). `CFBundleVersion` is `<short git SHA>[-dirty]+YYYYMMDDTHHMMSSZ`,
with the timestamp in UTC; `-dirty` includes tracked changes and untracked
files. The shell's native standard About panel uses these same Info.plist keys,
so the displayed version identifies the installed bundle without a separate
compiled version string. The packaging test covers clean and dirty stamps.

The installer refuses if any `gitterm-gpui` executable or GitTerm GPUI bundle
executable is running; quit it yourself before installation or rollback. It
copies with `ditto` into a staging directory and verifies before moving the
current bundle. It installs to `~/Applications/GitTerm GPUI.app`, prints old
and new build versions, and keeps the last three archived bundles named
`GitTerm GPUI.prev-<version>.app`. Duplicate versions gain a UTC/PID suffix.
Only those GPUI backups are pruned. Final signature failure restores the old
app. Concurrent script installs are locked; after an interrupted/crashed script,
inspect `.gitterm-gpui-install.lock` and any staging/backup before removing the
stale lock. Do not launch GPUI while installation is in progress.

```sh
scripts/install-gpui.sh --rollback
```

Rollback restores the most recently archived backup (archive time, not build
time), verifies it, and archives the displaced app so it can be restored too.
Rollback also accepts the original signed `0.0.0` bundle archived during the
first versioned installation; it checks the GPUI identity and signature without
applying the new-package verifier's placeholder-version rejection.
An absolute `GITTERM_GPUI_INSTALL_DIR` overrides the destination for either
operation; for example, use a directory under `mktemp -d` for trials.
`nice -n 19 bash tests/gpui-bundle-scripts.sh` exercises installation, a second
installation, retention, rollback, running refusal, failure recovery, package
validation and version stamping entirely in temporary directories with fake
apps/platform checks, plus a tiny ad-hoc-signed arm64 fixture for the real
macOS verification tools. No app is launched and no GPUI release is compiled.

## Stage 2 shell (TRU-280 / TRU-283)

`gitterm-gpui-shell::data::ShellData` is the read-only port for workspace,
session, status and file facts. `LocalData` reads the application owner on the
GPUI context; `SampleData` implements demo mode. GPUI does not depend on the
root Iced package.
The stage 1 design is in the planning repository's
`docs/gpui-stage1-core-design.md` sections 1–2: dock entities do not own service
jobs or recreate sessions.

Every workspace retains its own dock and entity map. Session IDs and persisted
pane IDs are independent of dock positions. The panel registry resolves layout
IDs to these existing entities. Drag pane tabs to move, split or group them;
drag dividers to resize. More → Show pane reopens a closed pane using the same
entity. Session tabs also reopen/reveal their pane. Terminal entities/processes
and output history, chat entities and scroll state, and expanded file
directories remain in the pane when hidden or moved, across
workspace switches and in-process layout reloads. These are in-process
identities; only layout persists on disk. Each session owns its content view;
the dock and layout registry only refer to it. Dock close hides a pane;
More → `Close <session title> permanently` removes the selected session for
this launch and releases its content view. Dropping a terminal uses the existing
backend cancel-on-drop API (including reader join and child reaping). A restart
recreates preview sessions; full saved-session restore is deferred.

Clicks focus the terminal or chat under the pointer. Only the focused terminal
receives typing. Chat Find (Cmd+F/G), plain/Markdown copy (Cmd+C / Cmd+Shift+C),
and code Copy stay scoped to their chat pane, including its find input and rich
text children. The standalone `--terminal` and `--chat <fixture>` routes remain
available.

Layouts save atomically on dock changes under `GITTERM_GPUI_CONFIG_DIR`
(absolute), defaulting to `~/.config/gitterm-gpui`. Local workspace filenames
use a stable hash of each canonical folder path, so adding or removing folders
does not move another workspace's layout. Sample layouts stay in the
`sample/` subdirectory. Older top-level `layout-N.json` files are ignored and
left untouched. Real V3–V6 config directories, their descendants and symlink
aliases are rejected. Panes that no longer exist are dropped while the
remaining layout stays saveable. Duplicate IDs, malformed geometry,
unsupported versions and corrupt JSON are reported with the full file path in
stderr and an on-screen banner. That workspace starts with the default layout
and refuses saves for the launch; repair or move the file before restarting.
Other workspace layouts can still save.

Mocha/Latte use `origin/v6`'s `src/theme.rs` palette and `src/config.rs` defaults
(13px UI, 14px terminal). Toggle the palette from More. Ctrl+1–9 selects a
workspace; Cmd+1–9 selects its session; Option+/ opens shortcut help. The More
menu and New task dialog follow the approved private mockup. All task/browser
actions are samples and create no processes, tasks, branches or worktrees.

From the repository root, build and launch manually with an isolated directory:

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 scripts/cargo.sh build --manifest-path gpui/Cargo.toml --release --workspace --locked -j 4
GITTERM_GPUI_CONFIG_DIR=/private/tmp/gitterm-gpui-tru280 /private/tmp/gt-target-tru280/release/gitterm-gpui
```

Headless verification (does not open an app or send OS input):

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru283 scripts/cargo.sh test --manifest-path gpui/Cargo.toml --workspace --locked -j 4
CARGO_TARGET_DIR=/private/tmp/gt-target-tru283 scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --workspace --locked --all-targets -j 4 -- -D warnings
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml --all -- --check
scripts/check-gpui-deps.sh
```

The test feature is enabled directly on `gpui-pre` and `gpui-component`, following
the mockup. Enabling `gpui-kit/test-support` instead pulls conflicting optional
`gpui-fast-web` resolution constraints into this pinned graph. The added GPUI
lockfile entries are the shell and headless test dependencies; the root lockfile
stays untouched. TRU-283 integrates the content views without changing the
terminal crate. The chat crate exports its scoped key-binding initializer and
scroll state, and focuses itself when its background is clicked.

For TRU-280 on 18 November 2026, the GPUI workspace release build, Clippy with
`-D warnings`, formatting, tests (7 passed) and dependency check above all
exited 0 on arm64 macOS. Builds used `-j 4`; `nice` was denied. Root `Cargo.lock`
remained unchanged.

TRU-283 verification on arm64 macOS, rebased onto `origin/v6` at `282d0f3`
(including TRU-282): workspace debug build, formatting, all-target Clippy with
`-D warnings`, workspace tests (**86 passed**: 12 chat, 9 shell, 65 terminal),
and the dependency boundary check all exited **0**. Commands used
`CARGO_TARGET_DIR=/private/tmp/gt-target-tru283`, `-j 4`, and the existing
`CARGO_HOME=/private/tmp/gpui-mockup-cargo`; `nice` was denied and omitted.
The root lockfile and terminal crate have no task diff. New headless coverage
checks terminal/chat entity survival and history/scroll retention through dock
moves, grouping, collapse/reopen, workspace switching and layout reload;
permanent session close releases the terminal; two terminals and embedded chat
route click focus and typing independently. A two-chat test exercises Find,
plain/Markdown copy, and actual code Copy button clicks without cross-pane focus.
Shell tests start clean zsh fixtures without host rc/session-restore scripts.

Visual, VoiceOver, keyboard/IME and interactive docking feel-tests remain manual
acceptance; headless tests cover entity lifetime, layout persistence and
per-pane input.
No GUI was launched or driven for stage 2 verification.

## Wry finding: resolution, linking, and runtime are separate gates

**The untouched published manifests cannot share a Cargo workspace lockfile.**
Cargo resolves dependencies for all target platforms, even for a macOS build.
wry 0.48.1 requests Android-only `kuchikiki ^0.8` (stable 0.8.2), while
lb-wry 0.53.3 pins Android-only `kuchikiki =0.8.8-speedreader`.
Cargo refuses this same-compatible-version selection. Separate executable
members and macOS target dependency sections do not avoid that conflict.

A temporary resolver experiment succeeded after changing only lb-wry's
Android manifest dependency to `kuchikiki ^0.8`; its macOS source was unchanged.
That experiment was used only to test linking and registration. It is not
part of this repository: neither app uses a patched or vendored lb-wry.
The chosen package layout uses separate workspaces and lockfiles.

**After that manifest-only correction, both libraries link into one macOS
executable, but cannot initialize together.** The headless native probe in
`probes/wry-registration.rs` calls the real public `WryWebView::class()` from
each crate through its corresponding objc2 trait. This triggers the same lazy
Objective-C class registration used by WebView construction, without creating
a window, browser, NSApplication, or synthetic input.

Executed on arm64 macOS with Rust 1.99.1:

| Probe mode (fresh process) | Exit | Result |
| --- | --- | --- |
| wry 0.48 only | 0 | Registers `WryWebView` |
| lb-wry 0.53 only | 0 | Registers `WryWebView` |
| old then new | 101 | Second registration panics: class already exists |
| new then old | 101 | Second registration panics: class already exists |

The probe release link exited 0. wry uses objc2 0.5's `declare_class!` and
lb-wry uses objc2 0.6's `define_class!`; both explicitly name `WryWebView`.
Their ivars differ (`HashMap` versus `Mutex<HashMap>`), so reusing the first
class is unsafe. Other delegate class names and dynamically named URL scheme
handlers also overlap. Renaming one class alone is insufficient.

**Package rule: one Wry implementation per executable/process.** Keep Iced
and GPUI in separate binaries, app bundles, and Cargo workspaces.
A runtime backend switch or separate dylibs does not solve the process-wide
Objective-C collision. Full WebView construction was unnecessary once class
registration failed, and no GUI was launched.

## Stage 0 verification (historical)

The preserved native probe release link exited 0; its four outcomes above
were executed on arm64 macOS, not inferred from source. The probe is retained
as source evidence only, outside Cargo targets: reproducing its combined
link requires the experimental manifest correction described above.

On arm64 macOS with rustup stable Rust 1.99.1, these checks exited 0:

- Iced build, test, Clippy (`-D warnings`), and formatting; build, test and
  Clippy also passed with `--features excalidraw`.
- Iced release build and bundle creation/signature verification via
  `scripts/bundle.sh` with `stt excalidraw`.
- GPUI release build, bundle creation/signature verification, formatting,
  and Clippy (`-D warnings`).
- Dependency boundary check and its regression cases; root `Cargo.lock`
  and the Windows workflow have no diff against `origin/v6`.

Iced tests reported 1,291 passes with default features and 1,303 with
Excalidraw, with 14 existing ignored tests in each run. Builds used `-j 4`
and separate temporary target directories. The environment denied `nice`,
so it was omitted. Neither app was launched; no CI run or visual check is
claimed. See the TRU-275 task handoff for commands and complete exit codes.

## Terminal route prototype (TRU-278)

`gitterm-gpui --terminal` opens a real `$SHELL` PTY in the new
[`gitterm-terminal`](crates/terminal/README.md) workspace member. It reuses the
committed iced_term fork backend without Iced, and adapts gpui_xterm's render
path to bounded snapshots. See [ROUTE.md](crates/terminal/ROUTE.md) for the
Route A recommendation, automated gates and remaining stage-3 work.

Build/test/lint from this workspace now include that member by default; use
`fmt --manifest-path gpui/Cargo.toml --all -- --check` to include its sources.
All terminal fixtures use owned PTYs and GPUI's headless test platform.

## Native chat fixture viewer (TRU-281)

`gitterm-gpui --chat <fixture.jsonl>` opens the display-only native chat viewer
in the `gitterm-gpui-chat` workspace member. See
[`crates/chat/README.md`](crates/chat/README.md) for synthetic fixtures, the
harness event boundary, behavior parity, headless interaction tests and the
5,000-row performance budget. The root Iced dependency graph is unchanged.
