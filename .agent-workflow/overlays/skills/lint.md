### gitterm-v2 Lint Notes

#### CI Command Set

CI runs `cargo clippy -- -D warnings` twice — once with default features and
once with `--features excalidraw`. Run both locally to mirror:

```bash
cargo clippy -- -D warnings
cargo clippy --features excalidraw -- -D warnings
```

**Do NOT pass `--all-targets`.** CI doesn't, and it scans examples and tests
which have their own intentional patterns (and would surface false errors).

#### Intentional Lint Allows

`src/tab/mod.rs` carries `#[allow(clippy::large_enum_variant)]` on `TabKind`.
The Terminal variant is ~5 KB and the Agent variant is ~200 B; boxing the
larger one would add indirection to the dominant terminal path for negligible
benefit. Tabs are pinned in a Vec and don't move. **Don't "fix" this lint.**

#### Pre-existing Warnings (Not Our Code)

`iced_term` (the fork at `../iced_term_fork`) has an unused-variable warning
on `viewport` in `view.rs:423`. Ignore — it's the fork, not gitterm.
