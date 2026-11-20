#!/bin/sh
# Coordinator only, after isolated sign-in and managed-writer isolation review.
# This is a file-tool probe, not proof that managed hooks are sandboxed.
set -eu
exec python3 -I -B "$(dirname "$0")/preview_claude_confinement_probe.py" "$@"
