---
name: test
description: "Run the gitterm-v6 test suite and fix or report failures."
---
<!-- agent-kit:managed v0.2.0 sha256:de326f7b266c20daf47da881ccea55eeaeef57bb9881108a049e06a2b2116c8e -->
# Test

1. Run the tests:

```sh
cargo test
```

2. For each failure, decide from the code whether the test or the code is wrong.
   Do not weaken or delete a test to make it pass.
3. Fix, re-run, and report the exit code with a pass/fail summary.

## Repo notes

### gitterm-v2 Test Notes

#### Feature Coverage

`cargo test` covers the default feature set. `cargo test --features excalidraw`
also covers the optional diagram-viewer feature. CI runs both — run both
locally for parity:

```bash
cargo test
cargo test --features excalidraw
```

#### Test Coverage

- **`plans_viewer::tests::*`** cover path-traversal safety on
  `/plans/raw/{name}`. Don't regress these without scrutiny — they protect
  against directory escapes and non-`.md` requests.

#### Setup Requirements

No external services required. Tests use `tempfile` for filesystem fixtures
and don't touch the network or the warp log server. The test binary doesn't
launch the Iced GUI.

#### Test Layout

Tests primarily live inline in source modules under `#[cfg(test)]`. The
`tests/` directory holds fixtures and page-check scripts, alongside an
integration test.
