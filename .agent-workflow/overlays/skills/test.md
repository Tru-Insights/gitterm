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
