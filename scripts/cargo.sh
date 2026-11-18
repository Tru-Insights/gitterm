#!/bin/sh
# Homebrew cargo can precede rustup on PATH. Always use the rustup toolchain.
set -eu
RUSTUP="${CARGO_HOME:-$HOME/.cargo}/bin/rustup"
[ -x "$RUSTUP" ] || RUSTUP=$(command -v rustup || true)
if [ -z "$RUSTUP" ]; then
    # CI runners install current stable Rust and have no Homebrew Rust to dodge.
    if [ -n "${CI:-}" ]; then exec cargo "$@"; fi
    echo "scripts/cargo.sh: rustup not found; the GPUI app needs rustup stable (Rust 1.99+)." >&2
    exit 1
fi
TOOLCHAIN_BIN=$(dirname "$("$RUSTUP" which --toolchain stable rustc)")
# Cargo itself also finds rustc/rustdoc and subcommands via PATH.
export PATH="$TOOLCHAIN_BIN:$PATH"
exec "$RUSTUP" run stable cargo "$@"
