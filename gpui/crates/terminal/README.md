# GitTerm terminal prototype (TRU-278, parent TRU-274)

Route A: the committed iced_term fork backend, decoupled from Iced, plus a
snapshot-driven adaptation of gpui_xterm 0.3.0's GPUI view/render path.
See [ROUTE.md](ROUTE.md) for the decision, evidence, limits, and stage-3 estimate.
This is a macOS prototype in GitTerm's separate `gpui/` workspace.

## Run and check

From the repository root, using rustup through the existing wrapper:

```sh
export CARGO_TARGET_DIR=/private/tmp/gt-target-tru287
scripts/cargo.sh run --manifest-path gpui/Cargo.toml --locked -j 4 -- --terminal
scripts/cargo.sh build --manifest-path gpui/Cargo.toml --locked -j 4
scripts/cargo.sh clippy --manifest-path gpui/Cargo.toml --workspace --locked --all-targets -j 4 -- -D warnings
scripts/cargo.sh fmt --manifest-path gpui/Cargo.toml --all -- --check
scripts/cargo.sh test --manifest-path gpui/Cargo.toml --workspace --locked -j 4
```

Use `nice -n 10` before `scripts/cargo.sh` where permitted. In this sandbox `nice`
reported `setpriority: Operation not permitted`, but still ran Cargo; its exit
codes below are the completed commands' results. The terminal flag runs `$SHELL`
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
| `src/backend.rs` | Fork `src/backend.rs` from the commit above, including its snapshot tests, mouse encoders, selection, geometry, regex/history/search helpers and Alacritty PTY construction (TRU-282 moves scheduling into the bounded `session` worker). Replace Iced `Size` with `LayoutSize`, Iced modifiers with GPUI modifiers (Control correctly supplies mouse modifier 16), remove Tokio event channel, retain viewport snapshots, add deferred interaction/resize/config application, explicit busy results, inclusive extraction, ordered asynchronous input and app-owned URL result. |
| `src/settings.rs` | Fork `src/settings.rs`: **only** `BackendSettings` and its `Default` implementation. Use `$SHELL` with zsh fallback; add the small geometry value type. No font/theme settings copied. |
| `src/backend.rs::paste` | Adapt fork `src/view.rs`'s `BindingAction::Paste` arm: CRLF/LF normalization and mode-2004 envelope. Clipboard acquisition lives in GPUI; additionally strip ESC to prevent pasted content from ending the envelope. |
| `src/events.rs` | New replacement for the fork proxy: send terminal replies directly to the bounded per-session input queue; coalesce UI-only effects. No copied Tokio bridge or Iced actions module. |
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

The crate's `session` worker owns the Alacritty PTY and parser. Reading/parsing
and partial writes each have a 64 KiB budget per turn. The read buffer is 64 KiB;
a busy foreground lock retains these bytes for retry, while cancellation stays
responsive. Interrupted polling retries; registration/read/write/resize-interest
errors become persistent session errors rather than panics. Resize requests
coalesce in one slot. Synchronized-output timeouts remain supported. The grid retains at
most the configured history (100,000 by default) plus the viewport; the stored
render snapshot contains only viewport cells. A failed foreground `try_lock`
keeps the last snapshot and requests a retry. Selection endpoints, scroll,
clear, history configuration, resize and scroll-to-bottom requests are retained
until a successful sync. Paint never holds the live terminal lock.

Input admission is nonblocking and all-or-nothing. Each session admits at most
1 MiB of unsent user input and reserves another 64 KiB for protocol replies;
message counts are also capped at 4,096 (4,352 including replies). One in-flight
message can retain its consumed prefix until completion, so allocation remains
bounded by two maximum messages plus queue overhead. Oversized or full-queue
paste returns `false`, sends no partial envelope, and displays a retry message.
The byte limit is checked before copying/normalizing foreground paste or IME
input. Accepted input retains ordering, ESC stripping, newline normalization and
bracketed-paste safety. A saturated protocol reserve terminates with a visible
error instead of silently dropping replies or allocating indefinitely.

`PtyWrite` bypasses the coalesced UI mailbox, including when GPUI is not pumped.
Latest title/clipboard-store, bell and exit state coalesce; they do not queue
PTY output. OSC52 remains Alacritty's **copy-only** default; clipboard reads
are not enabled. Title/clipboard callbacks run when the view renders.
The prototype shell has no clipboard callback, so OSC52 stores are exposed to
an integrating app, not written to the OS automatically.

`backend()` / `backend_mut()` expose history/search/scroll operations on the
view. `get_all_text`, `search_all`, and `selectable_content` return `None` when
the term is busy, distinct from an empty result. The original synchronous methods
remain compatibility helpers for tests and non-UI callers. UI/service consumers should use the TRU-287 APIs below.
Do not treat this prototype as complete GitTerm integration.

`Backend::cancel` is idempotent and joins the reader. View drop calls the same
path. On Unix, cleanup kills an unreaped owned child before Alacritty's PTY Drop
waits, so a HUP-ignoring shell cannot leak a thread or zombie. Normal child exit
retains its numeric exit code; signal exits report that the code is unavailable
(the PTY API exposes only `Option<i32>`). Exit and I/O-error state persist after
callbacks run, paint a banner, and reject subsequent input. The dead child's PTY
tail is drained with a 1 MiB ceiling so descendants retaining the slave cannot
keep a completed reader alive indefinitely.

`TerminalView::with_url_opener` installs app policy. Holding Cmd over an OSC 8
label or regex URL match shows a pointer; Cmd-click takes precedence over mouse
reporting and selection and calls that callback. Releasing Cmd or leaving the view clears the
hover. A busy parser clears stale link state; click never opens an old hover
location. OSC 8 destinations take precedence over label text; highlights include
wrapped and wide cells within the visible viewport. The terminal crate itself
never launches a URL or process opener.

## TRU-287: find and service jobs

The terminal's focused key handler opens literal find with Cmd+F. Type through
the existing IME input handler, use Backspace to edit, Cmd+G / Shift+Cmd+G (or
Enter / Shift+Enter) to navigate, and Esc to close and restore terminal focus.
The selected match is highlighted independently of the copy selection and
scrolled into view. The bar reports `Busy` during search/contention, then the
match count. The shell may need to route its global Cmd+F action to
`TerminalView::open_find` and focus the terminal if it intercepts that shortcut;
no shell integration was changed here.

- `Backend::search_job(String)` returns `ServiceJob<SearchResults>`; `poll()`
  returns `Pending` or `Ready(Result<_, ServiceError>)`. Search is literal via
  the existing Alacritty regex engine. `set_find_results` validates the revision
  before highlighting; `next_find(previous)` navigates without waiting on a lock.
- `Backend::export_job()` has the same protocol, producing plain text. Apps can
  poll it directly, or install `TerminalView::with_export_callback` and call
  `request_export()`. Accepted view requests deliver success or failure on the
  GPUI foreground notification task, even without a paint. Admission errors
  return directly. A missing callback returns `NoExportCallback`. The crate
  does no file IO, dialog, or save-location policy.
- `Backend::tail_snapshot(n)` returns the last N physical grid rows, including
  blank viewport rows. It includes combining/wide text and joins wrapped rows
  using Alacritty's extraction semantics. Zero returns an empty string; N above
  retention returns all retained text. It needs no render/snapshot sync, and
  its work/allocation scales with N (callers should request small log tails).

Each terminal admits one background search/export job at a time. A worker
copies at most 32 rows under each nonblocking parser lock, releases it, then
performs regex/extraction against a detached grid. Full-history allocation and
regex scans never run on the foreground. Tail uses the same sliced capture over only the
requested rows, with text conversion outside the lock. Contention or mutation
during capture returns `Busy`, never an empty success or a mixed-version grid.
A full-history job needs one temporary grid proportional to retained history;
no history cache, output queue, or extra parser is introduced. Dropping a job
receiver discards its result; an already running finite worker finishes without
holding up view/session cancellation.

Any parsed output, resize, clear or history-limit change invalidates find
positions. The bar conservatively clears and retries after output quiets (at
most ten attempts per second); a continuously changing grid can remain Busy.
Alacritty adjusts live selections during scroll/eviction; pending selection
coordinates from an outdated render are discarded. This favors clearing over
highlighting replacement text at an old coordinate.

## TRU-291: application launch contract

`TerminalLaunch` is the application entry point; `TerminalView::launch(launch,
config, cx)` creates a renderable terminal even when launch fails. `launch.start()`
provides the same contract to headless/backend consumers. `BackendSettings`
remains the low-level compatibility API. Launch options include an optional cwd
(`None` inherits the app cwd), an environment overlay (`Some(value)` sets,
`None` removes), program, exact argument vector, initial columns/rows, and an
optional trusted startup command. The launch geometry reaches the PTY before
spawn; normal view layout subsequently resizes it.

Defaults follow Iced's Unix launch policy: `$SHELL`, then the user's `/etc/passwd`
shell, then `/bin/zsh`, with `-l`; TERM/COLORTERM, LANG and the HOME-based executable
PATH are set before applying the overlay. Parent variables otherwise remain
inherited. No settings file or root Iced shell-integration directory is used.
An `env -u -C` child launcher implements removals and chdir without mutating the app environment;
arguments and paths are passed as separate arguments; environment values stay
in the child environment, never argv. cwd and executable are validated; the
child launcher also refuses a cwd removed during spawn. Program paths containing
`=` are rejected because the platform env utility interprets them as assignments. Failures show the existing persistent terminal banner.

Startup uses shell integration, as Iced does, with a readiness marker instead of
executing a command before the prompt. zsh uses a private temporary ZDOTDIR that
sources the original login files and adds a one-shot ZLE line-init hook. bash uses
PROMPT_COMMAND to append an OSC 133 B marker to its first prompt. On readiness,
the trusted command plus Enter is admitted once through the existing bounded
input queue. This is deliberately command execution, separate from clipboard
paste; paste normalization/enveloping is unchanged. Temporary integration files
are owned by the backend and removed on drop. Startup commands currently require
zsh or bash, with normal interactive startup processing enabled; other programs
can launch with arbitrary arguments without a startup command. Shell rc files
that disable/replace integration can prevent readiness; no timing-based guess at
a prompt is made. Bash rc files can replace PROMPT_COMMAND, just as in Iced.

OSC 7 `file://host/absolute/path` reports (BEL or ST terminated, including split
PTY reads and percent-encoded paths) are coalesced independently of title reports.
Observation is bounded to 4096 bytes per OSC; malformed, control-containing or
oversized paths are ignored. The host is display metadata and is never resolved.
`with_cwd_callback` and `with_title_callback` deliver the latest reports during
foreground rendering. A hidden pane retains the latest report until shown.
The shell displays these reports in its dock pane tab. Terminal and chat find
shortcuts remain scoped to their focused view; the shell does not install a
competing global find handler.

## TRU-274: real-tool PTY gate

`tool_tests.rs` and `tool_view_tests.rs` exercise the production `TerminalLaunch`
backend with isolated homes and temporary files/repos. They poll rendered grids
and modes for vim edit/write/quit, bracketed paste, SGR mouse, alternate screen
restoration, less and git log/diff paging, top/htop resize/quit, sustained output
resize, Unicode and SGR colors, OSC 8 links, title/cwd callbacks, and shell exit.
Close tests assert one callback, joined reader ownership and a reaped child PID.
Missing executables print a runtime `SKIP` reason; a sandbox denying top process
inspection is also reported explicitly. New probes use deadlines and state
polling, with no fixed readiness delays or OS input automation.

OSC 8 activation previously ignored cell hyperlink metadata and only recognized
URL-shaped text. It now resolves the explicit target before the regex fallback,
including wide/wrapped labels, and keeps opening policy in the app callback.
The headless gate does not certify native IME, font rasterization, accessibility,
Retina behavior or interactive feel. Verification and per-behavior test names
are recorded in [evidence/tru274-checks.txt](evidence/tru274-checks.txt).
