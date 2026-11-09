---
name: lint
description: "Run the gitterm-v6 lint and format checks and fix what they report."
---
<!-- agent-kit:managed v0.2.0 sha256:31cf5e20ea4cd38d0ad5944044e42b2b24221f912e774e3ad1660524228295ed -->
# Lint

1. Run:

```sh
cargo fmt -- --check
cargo clippy -- -D warnings
```

2. Fix each finding in the code. Do not silence a rule unless the code
   explains why the rule does not apply there.
3. Re-run and report the exit codes.

## Repo notes

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
