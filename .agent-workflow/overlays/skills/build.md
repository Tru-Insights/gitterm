### gitterm-v2 Build Pipeline

Full reference is `BUILD.md` at the repo root — Linux deps, cross-compilation,
GitHub Actions, etc. The configured `cargo build` is just the dev-loop default;
shipping has more flags and a bundle step.

#### Common Commands

| Need                  | Command                                           |
|-----------------------|---------------------------------------------------|
| Dev loop              | `cargo run`                                       |
| Quick debug build     | `cargo build`                                     |
| Release binary        | `cargo build --release --features stt`            |
| Ship macOS .app       | `./scripts/bundle.sh`                             |
| Install bundle        | `cp -r target/GitTerm.app /Applications/`         |
| Launch installed app  | `open /Applications/GitTerm.app`                  |

#### Feature Flags

- `stt` — voice (whisper-rs + cpal). Required for shipping. Off by default.
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

- `cargo build` → `target/debug/gitterm`
- `cargo build --release ...` → `target/release/gitterm`
- `./scripts/bundle.sh` → `target/GitTerm.app` (custom bundle, multi-instance launcher)
- `cargo bundle --release --features stt` → `target/release/bundle/osx/GitTerm.app`
  (cargo-bundle crate output; use `bundle.sh` instead for the multi-instance launcher)
