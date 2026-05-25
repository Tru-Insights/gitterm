### gitterm-v2 Test Notes

#### Feature Coverage

`cargo test` covers the default feature set (~96 tests). `cargo test --features
excalidraw` adds the diagram-viewer tests (~108 total). CI runs both — run both
locally for parity:

```bash
cargo test
cargo test --features excalidraw
```

#### Test Canaries

- **`tests::collect_file_tree_hides_dotfiles`** (`src/main.rs:14184`) is the
  canary for `services::collect_file_tree` correctly honoring `show_hidden`.
  Pre-TRU-69 this test was failing because the parameter was accepted but
  ignored. If it ever fails again, suspect the same root cause first.
- **`plans_viewer::tests::*`** cover path-traversal safety on
  `/plans/raw/{name}`. Don't regress these without scrutiny — they protect
  against directory escapes and non-`.md` requests.

#### Setup Requirements

No external services required. Tests use `tempfile` for filesystem fixtures
and don't touch the network or the warp log server. The test binary doesn't
launch the Iced GUI.

#### Test Layout

Most tests live inline in `src/main.rs` under `#[cfg(test)] mod tests`. The
plans-viewer tests live in `src/plans_viewer.rs::tests`. There's no separate
`tests/` integration directory.
