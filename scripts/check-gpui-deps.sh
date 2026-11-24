#!/bin/bash
# Check both complete lockfiles, including dependencies for non-host targets.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ ! -r Cargo.lock ] || [ ! -r gpui/Cargo.lock ]; then
    echo "Both Cargo.lock and gpui/Cargo.lock are required" >&2
    exit 1
fi
if grep -Eq '^name = "iced([_-][^"]*)?"$' gpui/Cargo.lock; then
    echo "Iced dependency leaked into gitterm-gpui" >&2
    exit 1
fi
if grep -Eq '^name = "(gpui([_-][^"]*)?|lb-wry)"$' Cargo.lock; then
    echo "GPUI dependency leaked into gitterm-v6" >&2
    exit 1
fi
gpui_tree=$(scripts/cargo.sh tree --manifest-path gpui/Cargo.toml --locked --target aarch64-apple-darwin -e normal,build --prefix none | awk '{print $1, $2}' | sort -u)
test "$(echo "$gpui_tree" | grep '^gpui-pre v')" = 'gpui-pre v0.3.14'
test "$(echo "$gpui_tree" | grep '^gpui-kit v')" = 'gpui-kit v0.11.0'
echo "Dependency boundary passed; one gpui-pre 0.3.14 and one gpui-kit 0.11.0"
