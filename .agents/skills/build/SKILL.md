---
name: build
description: "Build gitterm-v6 with its configured command and fix build failures."
---
<!-- agent-kit:managed v0.2.0 sha256:dca40dfa669165cdedb649704a6fc3b6d4dca1db973966ba87e6b43df74e260f -->
# Build

1. Run the build:

```sh
cargo build
```

2. On failure, keep the exact output (file, line, error). Read the failing code
   and its callers before changing anything.
3. Fix the cause with the smallest change. If the failure is the local toolchain
   or setup, report that instead of changing source.
4. Run the build again and report its exit code and what you fixed.

## Repo notes

### gitterm-v2 Build Pipeline

Full reference is `BUILD.md` at the repo root — Linux deps, cross-compilation,
GitHub Actions, etc. The configured `cargo build` is just the dev-loop default;
shipping has more flags and a bundle step.

#### Common Commands

| Need                  | Command                                           |
|-----------------------|---------------------------------------------------|
| Dev loop              | `cargo run`                                       |
| Quick debug build     | `cargo build`                                     |
| Release binary        | `cargo build --release`                           |
| Ship macOS .app       | `./scripts/bundle.sh`                             |
| Install bundle        | `cp -r 'target/GitTerm V6.app' /Applications/`    |
| Launch installed app  | `open '/Applications/GitTerm V6.app'`             |

#### Feature Flags

- `stt` — voice (whisper-rs + cpal). Enabled by default.
- `excalidraw` — diagram viewer surface. Optional. Off by default.

`./scripts/bundle.sh` builds with both (`stt excalidraw`).

#### Before triggering GitHub Actions CI

All three workflows (`build.yml`, `ci.yml`, `windows-build.yml`) clone
`Tru-Insights/iced_term` master fresh on every run. If you've made local
commits to `../iced_term_fork` that gitterm depends on, push them first or
CI fails with `no method named X found` errors:

```bash
cd ../iced_term_fork
git log @{u}..HEAD --oneline   # must be empty before CI
git push origin master          # if it's not
```

#### Output Locations

- `cargo build` → `target/debug/gitterm-v6`
- `cargo build --release` → `target/release/gitterm-v6`
- `./scripts/bundle.sh` → `target/GitTerm V6.app` (custom bundle, multi-instance launcher)
- `cargo bundle --release` → `target/release/bundle/osx/GitTerm V6.app`
  (cargo-bundle crate output; use `bundle.sh` instead for the multi-instance launcher)
