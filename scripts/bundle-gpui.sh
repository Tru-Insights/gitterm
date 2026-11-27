#!/bin/bash
# Stage 0 has its own executable and bundle; never install over GitTerm V6.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"
if [ "$(uname -s)" != Darwin ]; then
    echo "GitTerm GPUI packaging requires macOS" >&2
    exit 1
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_DIR/gpui/target}"
APP_DIR="$TARGET_DIR/GitTerm GPUI.app"
"$SCRIPT_DIR/cargo.sh" build --manifest-path "$PROJECT_DIR/gpui/Cargo.toml" --release --locked --features stt
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$TARGET_DIR/release/gitterm-gpui" "$APP_DIR/Contents/MacOS/gitterm-gpui"
cp "$PROJECT_DIR/assets/icon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"
cat > "$APP_DIR/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>GitTerm GPUI</string>
    <key>CFBundleIdentifier</key><string>com.cree8.gitterm.gpui</string>
    <key>CFBundleVersion</key><string>0.0.0</string>
    <key>CFBundleShortVersionString</key><string>0.0.0</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>gitterm-gpui</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>GitTerm V6 uses the microphone for speech-to-text input to the terminal.</string>
</dict>
</plist>
PLIST
codesign --force --deep -s - "$APP_DIR"
codesign --verify --deep "$APP_DIR"
echo "App bundle created at: $APP_DIR"
