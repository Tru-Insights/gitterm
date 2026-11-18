#!/bin/bash
# Regression coverage: lockfile boundaries must also catch non-host packages.
set -euo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE=$(mktemp -d)
trap 'rm -rf "$FIXTURE"' EXIT
mkdir -p "$FIXTURE/scripts" "$FIXTURE/gpui"
cp "$PROJECT_DIR/scripts/check-gpui-deps.sh" "$FIXTURE/scripts/"
cat > "$FIXTURE/scripts/cargo.sh" <<'CARGO'
#!/bin/sh
cat gpui-tree.txt
CARGO
chmod +x "$FIXTURE/scripts/cargo.sh"
cd "$FIXTURE"
printf 'gpui-pre v0.3.12\ngpui-kit v0.9.1\n' > gpui-tree.txt
printf 'name = "gitterm-v6"\n' > Cargo.lock
printf 'name = "gitterm-gpui"\n' > gpui/Cargo.lock
scripts/check-gpui-deps.sh

printf 'name = "iced_core"\n' >> gpui/Cargo.lock
if scripts/check-gpui-deps.sh > result.log 2>&1; then
    echo "Boundary accepted Iced in the GPUI lockfile" >&2
    exit 1
fi
grep -q 'Iced dependency leaked' result.log
printf 'name = "gitterm-gpui"\n' > gpui/Cargo.lock
printf 'name = "gpui-pre"\n' >> Cargo.lock
if scripts/check-gpui-deps.sh > result.log 2>&1; then
    echo "Boundary accepted GPUI in the Iced lockfile" >&2
    exit 1
fi
grep -q 'GPUI dependency leaked' result.log
printf 'name = "gitterm-v6"\n' > Cargo.lock
printf 'gpui-pre v0.3.11\n' >> gpui-tree.txt
if scripts/check-gpui-deps.sh > result.log 2>&1; then
    echo "Boundary accepted multiple gpui-pre versions" >&2
    exit 1
fi
rm Cargo.lock
if scripts/check-gpui-deps.sh > result.log 2>&1; then
    echo "Boundary accepted a missing root lockfile" >&2
    exit 1
fi
grep -q 'Both Cargo.lock and gpui/Cargo.lock are required' result.log
echo "Dependency boundary regression checks passed"
