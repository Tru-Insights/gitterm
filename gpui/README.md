# GitTerm GPUI preview (TRU-275 / TRU-280, parent TRU-274)

This macOS-only app opens a 1440×900 window titled
“GitTerm · GPUI shell · Sample data”. The default dispatch opens the stage 2
shell in `crates/shell`: workspace/repository sidebar, session tabs, and a
gpui-component `DockArea`. Terminal and chat panes are placeholders; Git status
and the expandable file tree use sample data. By default it never reads GitTerm settings
or starts terminals or services.
`--terminal` opens the TRU-278 terminal route prototype instead (see below).
The repository root is the Iced workspace. `gpui/` is a separate workspace
with package `gitterm-gpui`, its own `Cargo.toml` and `Cargo.lock`, and is
explicitly excluded from the root workspace. Root Cargo commands (including
`--workspace` and `fmt --all`) never descend into GPUI. GPUI dependencies are
target-gated to macOS; the Windows workflow remains unchanged.

The root lockfile is identical to `origin/v6`: Iced's dependency graph is
unchanged. Separate lockfiles avoid the incompatible published Wry manifests
described below without vendoring or patching either dependency. A future
shared core crate will be consumed by both apps through path dependencies.
Its dependencies must stay minimal because Cargo resolves them twice, once
per workspace. No shared crate is introduced in stage 0.

## Toolchain and build

Use the installed **rustup stable toolchain, Rust 1.99 or newer**. This package's
`rust-version` enforces the minimum (the root Iced package declares none). Do not use Homebrew Cargo:
even `rustup run stable cargo` can find Homebrew's `rustc` on PATH.
`scripts/cargo.sh` obtains the installed compiler path from rustup and prepends
that toolchain's binaries before invoking Cargo. It never installs or updates
toolchains. `rust-toolchain.toml` also selects stable for rustup proxies.

From the repository root:

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 nice -n 10 scripts/cargo.sh build --manifest-path gpui/Cargo.toml --release --locked -j 4
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 nice -n 10 scripts/bundle-gpui.sh
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml -- --check
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 nice -n 10 scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --locked -j 4 -- -D warnings
scripts/check-gpui-deps.sh
```

Omit `nice -n 10` when the environment does not permit priority changes.

The separate, ad-hoc-signed bundle is
`$CARGO_TARGET_DIR/GitTerm GPUI.app` (default: `gpui/target/GitTerm GPUI.app`),
with identifier `com.cree8.gitterm.gpui`. It does not replace GitTerm V6.
The pinned stack is gpui-component/gpui-webview/gpui-kit 0.9.1,
gpui_xterm 0.3.0, and gpui-pre 0.3.12. Cargo tree may print repeated `(*)`
references; compare distinct package versions, as the dependency check does.

## Stage 2 shell (TRU-280)

`gitterm-gpui-shell::data::ShellData` is the frontend-neutral port for workspace,
session, status and file facts. `SampleData` implements it today. The later core
adapter belongs behind this port; GPUI does not depend on the root Iced package.
The stage 1 design is in the planning repository's
`docs/gpui-stage1-core-design.md` sections 1–2: dock entities do not own service
jobs or recreate sessions.

Every workspace retains its own dock and entity map. Session IDs and persisted
pane IDs are independent of dock positions. The panel registry resolves layout
IDs to these existing entities. Drag pane tabs to move, split or group them;
drag dividers to resize. More → Show pane reopens a closed pane using the same
entity. Session tabs also reopen/reveal their pane. Draft text, sample output
count and expanded file directories remain in the pane when hidden or moved.
These are in-process identities; only layout, not draft text, persists on disk.

Layouts save atomically on dock changes to `layout-<workspace-id>.json` under
`GITTERM_GPUI_CONFIG_DIR` (absolute), defaulting to
`~/.config/gitterm-gpui`. Real V3–V6 config directories, their descendants and
symlink aliases are rejected. Unknown IDs, duplicate IDs, malformed geometry,
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

Headless verification (does not open an app or send synthetic input):

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 scripts/cargo.sh test --manifest-path gpui/Cargo.toml --workspace --locked -j 4
CARGO_TARGET_DIR=/private/tmp/gt-target-tru280 scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --workspace --locked -j 4 -- -D warnings
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml --all -- --check
scripts/check-gpui-deps.sh
```

The test feature is enabled directly on `gpui-pre` and `gpui-component`, following
the mockup. Enabling `gpui-kit/test-support` instead pulls conflicting optional
`gpui-fast-web` resolution constraints into this pinned graph. The added GPUI
lockfile entries are the shell and headless test dependencies; the root lockfile
stays untouched. No terminal/chat crate changes are needed for this slice.

On 18 November 2026, the GPUI workspace release build, Clippy with
`-D warnings`, formatting, tests (7 passed) and dependency check above all
exited 0 on arm64 macOS. Builds used `-j 4`; `nice` was denied. Root `Cargo.lock`
remained unchanged.

Visual, VoiceOver, keyboard/IME and interactive docking feel-tests remain manual
acceptance; headless tests prove entity lifetime and layout persistence only.
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
