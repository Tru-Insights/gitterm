#!/bin/bash
# Creates an isolated macOS .app bundle for GitTerm V4

set -e

APP_NAME="GitTerm V4"
BUNDLE_ID="com.cree8.gitterm.v4"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/target/release"
APP_DIR="$PROJECT_DIR/target/$APP_NAME.app"

echo "Building release binary..."
cargo build --release --features "stt excalidraw" --manifest-path "$PROJECT_DIR/Cargo.toml"

echo "Creating app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

# Copy binary with a different name so the launcher script can call it directly
cp "$BUILD_DIR/gitterm-v4" "$APP_DIR/Contents/MacOS/gitterm-v4-bin"

# Create launcher script as the app executable — bypasses Launch Services
# deduplication so each double-click spawns a fresh process
cat > "$APP_DIR/Contents/MacOS/$APP_NAME" << 'LAUNCHER'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
# Spawn binary detached so launcher exits immediately.
# macOS then sees no running process for this bundle, allowing
# subsequent double-clicks to spawn additional instances.
nohup "$DIR/gitterm-v4-bin" "$@" >/dev/null 2>&1 &
disown
exit 0
LAUNCHER
chmod +x "$APP_DIR/Contents/MacOS/$APP_NAME"

# Copy icon
cp "$PROJECT_DIR/assets/icon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"

# Create Info.plist
cat > "$APP_DIR/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>4.0.0</string>
    <key>CFBundleShortVersionString</key>
    <string>4.0.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>$APP_NAME</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.13</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>LSMultipleInstancesProhibited</key>
    <false/>
    <key>NSMicrophoneUsageDescription</key>
    <string>GitTerm V4 uses the microphone for speech-to-text input to the terminal.</string>
</dict>
</plist>
EOF

echo "App bundle created at: $APP_DIR"
echo "You can now run: open \"$APP_DIR\""
