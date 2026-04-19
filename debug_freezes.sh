#!/bin/bash

echo "GitTerm Freeze Debugging Script"
echo "==============================="
echo
echo "This script will:"
echo "1. Build GitTerm with freeze debugging enabled"
echo "2. Run it with debug output"
echo "3. Help you identify what's causing freezes"
echo

# Enable freeze debugging via environment variable
export GITTERM_DEBUG_FREEZES=1

echo "Building GitTerm with debug logging..."
if cargo build --release --features stt; then
    echo "Build successful!"
    echo
    echo "Running GitTerm with freeze debugging enabled..."
    echo "Look for these patterns in the output:"
    echo "  - [FREEZE-DEBUG] Lines showing slow operations"
    echo "  - CRITICAL: spawn_blocking failed - indicates git ops on main thread"
    echo "  - Large file warnings"
    echo "  - Operations taking >50ms"
    echo
    echo "Starting GitTerm... (Ctrl+C to stop)"
    echo "----------------------------------------"
    
    # Run with debug output and timestamp each line
    ./target/release/gitterm 2>&1 | while IFS= read -r line; do
        printf "[%s] %s\n" "$(date '+%H:%M:%S.%3N')" "$line"
    done
else
    echo "Build failed! Please fix compilation errors first."
    exit 1
fi