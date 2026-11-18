#!/bin/sh
# Homebrew cargo can precede rustup on PATH. Always use the rustup toolchain.
set -eu
RUSTUP="${CARGO_HOME:-$HOME/.cargo}/bin/rustup"
TOOLCHAIN_BIN=$(dirname "$("$RUSTUP" which --toolchain stable rustc)")
# Cargo itself also finds rustc/rustdoc and subcommands via PATH.
export PATH="$TOOLCHAIN_BIN:$PATH"
exec "$RUSTUP" run stable cargo "$@"
