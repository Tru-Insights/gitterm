# GitTerm terminal prototype (TRU-278, parent TRU-274)

Route A: the committed iced_term fork backend, decoupled from Iced, plus a
snapshot-driven adaptation of gpui_xterm 0.3.0's GPUI view/render path.
See [ROUTE.md](ROUTE.md) for the decision, evidence, limits, and stage-3 estimate.
This is a macOS prototype in GitTerm's separate `gpui/` workspace.

## Run and check

From the repository root, using rustup through the existing wrapper:

```sh
export CARGO_TARGET_DIR=/private/tmp/gt-target-tru278
scripts/cargo.sh run --manifest-path gpui/Cargo.toml --locked -j 4 -- --terminal
scripts/cargo.sh build --manifest-path gpui/Cargo.toml --locked -j 4
scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --locked -j 4 -- -D warnings
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml --all -- --check
scripts/cargo.sh test --manifest-path gpui/Cargo.toml --locked -j 4 -- --nocapture --test-threads=1
```

Use `nice -n 10` before `scripts/cargo.sh` where permitted. It was denied in
this sandbox, so verification omitted it. The terminal flag runs `$SHELL`
(`/bin/zsh` when unset) in a real Alacritty PTY. Without the flag, the existing
stage-0 dock opens. Neither mode reads or writes GitTerm settings; this binary
has no Iced runtime or running-GitTerm interaction. No visible app was launched
for this task; GPUI's test platform paints the view without an OS window.

Verification used the existing writable `CARGO_HOME=/private/tmp/gpui-mockup-cargo`
to avoid writes to the sandbox's read-only normal registry. Its `bin/rustup`
symlink points to the installed rustup, so `scripts/cargo.sh` still selects the
installed stable compiler. No toolchain installation or source registry patch
was made. That temporary Cargo home is optional in an unrestricted checkout.

## Source identity and licences

Checked before copying on 2026-11-18:

- GitTerm's root [README](../../../README.md#license) declares **MIT**;
  root bundle metadata names Copyright 2025 Cree8. There is no standalone root
  licence file. This crate does not change the project's licence declaration.
- [Upstream iced_term licence](https://raw.githubusercontent.com/Harzu/iced_term/master/LICENSE)
  is **MIT**, Copyright (c) 2024 Ilia Shvyrialkin. The Tru-Insights fork at
  **261234b14952d9607ae9298db2e33f11733e9248** retains the same licence and
  MIT package declaration. The full notice is [licenses/iced_term-MIT](licenses/iced_term-MIT).
- [gpui_xterm 0.3.0](https://github.com/Modolet/gpui_xterm) declares **MIT**,
  Copyright (c) 2026 Modolet. Its published full notice is
  [licenses/gpui_xterm-MIT](licenses/gpui_xterm-MIT).
- The fork labels its visible-regex helper as copied from Alacritty's
  `alacritty/src/display/hint.rs`. Preserve that attribution and
  [Alacritty's Apache-2.0 notice](licenses/Alacritty-Apache-2.0). Alacritty
  terminal 0.25.1 and GPUI remain dependencies with their own licences.

These permissive terms permit this adaptation with their notices retained;
no incompatible licence was found. [SOURCES.json](SOURCES.json) records SHA256
of **original input bytes**, before adaptation/formatting, not hashes of the
modified files. The registry input was the published `gpui_xterm-0.3.0` source;
its package checksum remains pinned in `gpui/Cargo.lock`.

| Destination | Input and adaptation |
| --- | --- |
| `src/backend.rs` | Fork `src/backend.rs` from the commit above, including its snapshot tests, mouse encoders, selection, geometry, regex/history/search helpers and Alacritty event-loop construction. Replace Iced `Size` with `LayoutSize`, Iced modifiers with GPUI modifiers (Control correctly supplies mouse modifier 16), remove Tokio event channel, retain viewport snapshots, add deferred interaction/resize/config application, explicit busy results, inclusive extraction, ordered asynchronous input and app-owned URL result. |
| `src/settings.rs` | Fork `src/settings.rs`: **only** `BackendSettings` and its `Default` implementation. Use `$SHELL` with zsh fallback; add the small geometry value type. No font/theme settings copied. |
| `src/backend.rs::paste` | Adapt fork `src/view.rs`'s `BindingAction::Paste` arm: CRLF/LF normalization and mode-2004 envelope. Clipboard acquisition lives in GPUI; additionally strip ESC to prevent pasted content from ending the envelope. |
| `src/events.rs` | New replacement for the fork proxy: send terminal replies directly to Alacritty's event-loop input channel; coalesce UI-only effects. No copied Tokio bridge or Iced actions module. |
| `src/view.rs` | Published xterm `src/view.rs`, retaining EntityInputHandler/UTF16 preedit, focus/key actions, menus, callbacks, drag/semantic/line selection, canvas/font measurement. Replace reader, queue, foreground parser and writer with Backend; use a 16 ms notification task; connect native Cmd-C/V and mode-aware mouse; defer copy during parser contention. |
| `src/render.rs` | Published xterm `src/render.rs`, including renderer tests except its old TerminalState ingest test. Paint immutable viewport cells/colors/cursor/selection instead of locking live Term; preserve geometric glyphs/font fallback/preedit and append combining marks. |
| `src/colors.rs`, `src/glyphs.rs`, `src/input.rs` | Published files and their tests, with attribution headers/formatting. No new emulator or key-encoding scheme. |
| `src/mouse.rs` | Only published `pixel_to_cell` helper; other xterm mouse/selection encoders are omitted because the fork supplies them. |
| `src/test_support.rs`, `*_tests.rs` | Port TRU-276's real-PTY query, zsh, reader-observer/RSS, history and headless entity probes into this implementation. Observer delegates polling/writes/child events to the real Alacritty PTY, and counts every byte actually returned to its parser. Clipboard is a thread-local fixture in tests; no OS clipboard or synthetic OS input. |

Fork reads used `git show 261234b:<path>` from
`/Users/t.trewin/GitRepo/iced_term_fork`. Its already dirty `src/backend.rs` and
untracked `iced_term_fork` entry were excluded, and the fork was never changed.
In particular its uncommitted URL delimiter work is **not** in this crate.

## Ownership and limits

Alacritty's own event loop owns reading, parsing and partial/asynchronous PTY
writes on its worker. Its read buffer is bounded at 1 MiB. The grid retains at
most the configured history (100,000 by default) plus the viewport; the stored
render snapshot contains only viewport cells. A failed foreground `try_lock`
keeps the last snapshot and requests a retry. Selection endpoints, scroll,
clear, history configuration, resize and scroll-to-bottom requests are retained
until a successful sync. Paint never holds the live terminal lock.

`PtyWrite` bypasses the coalesced UI mailbox, including when GPUI is not pumped.
Latest title/clipboard-store, bell and exit state coalesce; they do not queue
PTY output. OSC52 remains Alacritty's **copy-only** default; clipboard reads
are not enabled. Title/clipboard callbacks run when the view renders.
The prototype shell has no clipboard callback, so OSC52 stores are exposed to
an integrating app, not written to the OS automatically.

`backend()` / `backend_mut()` expose history/search/scroll operations on the
view. `get_all_text`, `search_all`, and `selectable_content` return `None` when
the term is busy, distinct from an empty result. Search/export still perform
full-history work on the caller; stage 3 must move service exports/search jobs
off foreground and add an efficient tail API. Search positions can expire as
history is evicted. Do not treat this prototype as complete GitTerm integration.
