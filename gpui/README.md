# GitTerm GPUI stage 0 (TRU-275, parent TRU-274)

This macOS-only build gate opens one 1440×900 window titled
“GitTerm · GPUI stage 0”, containing an empty gpui-component `DockArea`.
It does not read GitTerm settings, start terminals, or contain migration code.
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

Use the installed **rustup stable toolchain, Rust 1.99 or newer**. The packages'
`rust-version` declarations enforce the minimum. Do not use Homebrew Cargo:
even `rustup run stable cargo` can find Homebrew's `rustc` on PATH.
`scripts/cargo.sh` obtains the installed compiler path from rustup and prepends
that toolchain's binaries before invoking Cargo. It never installs or updates
toolchains. `rust-toolchain.toml` also selects stable for rustup proxies.

From the repository root:

```sh
CARGO_TARGET_DIR=/private/tmp/gt-target-tru275-gpui nice -n 10 scripts/cargo.sh build --manifest-path gpui/Cargo.toml --release --locked -j 4
CARGO_TARGET_DIR=/private/tmp/gt-target-tru275-gpui nice -n 10 scripts/bundle-gpui.sh
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml -- --check
CARGO_TARGET_DIR=/private/tmp/gt-target-tru275-gpui nice -n 10 scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --locked -j 4 -- -D warnings
scripts/check-gpui-deps.sh
```

Omit `nice -n 10` when the environment does not permit priority changes.

The separate, ad-hoc-signed bundle is
`$CARGO_TARGET_DIR/GitTerm GPUI.app` (default: `gpui/target/GitTerm GPUI.app`),
with identifier `com.cree8.gitterm.gpui`. It does not replace GitTerm V6.
The pinned stack is gpui-component/gpui-webview/gpui-kit 0.9.1,
gpui_xterm 0.3.0, and gpui-pre 0.3.12. Cargo tree may print repeated `(*)`
references; compare distinct package versions, as the dependency check does.

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

## Verification

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
