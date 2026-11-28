#!/bin/bash
# Verify a packaged app without executing it. macOS command tools only.
set -euo pipefail
die() { echo "verify-gpui-bundle: $*" >&2; exit 1; }
[ "$#" -eq 1 ] || die "Usage: $0 <GitTerm GPUI.app>"
APP=$1
PLIST="$APP/Contents/Info.plist"
[ -f "$PLIST" ] || die "Missing Info.plist: $APP"
read_key() {
    local value
    value=$(/usr/libexec/PlistBuddy -c "Print :$1" "$PLIST") || die "Missing $1: $APP"
    [ -n "$value" ] || die "Empty $1: $APP"
    printf '%s\n' "$value"
}
for key in CFBundleName CFBundleIdentifier CFBundlePackageType CFBundleVersion CFBundleShortVersionString CFBundleExecutable CFBundleIconFile LSMinimumSystemVersion NSMicrophoneUsageDescription; do
    read_key "$key" > /dev/null
done
[ "$(read_key CFBundleIdentifier)" = com.cree8.gitterm.gpui ] || die "Wrong bundle identifier: $APP"
[ "$(read_key CFBundlePackageType)" = APPL ] || die "Not an application: $APP"
VERSION=$(read_key CFBundleVersion)
SHORT_VERSION=$(read_key CFBundleShortVersionString)
[ "$VERSION" != 0.0.0 ] && [ "$SHORT_VERSION" != 0.0.0 ] || die "Placeholder version: $APP"
EXECUTABLE=$(read_key CFBundleExecutable)
[ "$EXECUTABLE" = gitterm-gpui ] || die "Unexpected executable: $EXECUTABLE"
BINARY="$APP/Contents/MacOS/$EXECUTABLE"
[ -f "$BINARY" ] && [ -x "$BINARY" ] || die "Missing executable: $BINARY"
lipo "$BINARY" -verify_arch arm64 || die "Executable must contain arm64: $BINARY"
ICON=$(read_key CFBundleIconFile)
case "$ICON" in */*|.|..) die "Invalid icon name: $ICON" ;; esac
case "$ICON" in *.icns) ;; *) ICON="$ICON.icns" ;; esac
[ -s "$APP/Contents/Resources/$ICON" ] || die "Missing icon: $ICON"
codesign --verify --deep --strict "$APP" || die "Invalid code signature: $APP"
# Unstripped release builds expose whisper's C entry points. Stripped builds
# cannot prove the feature from their symbol table; report that limit explicitly.
SYMBOLS=$(mktemp -t gitterm-gpui-symbols.XXXXXX)
trap 'rm -f "$SYMBOLS"' EXIT
if nm -arch arm64 -g "$BINARY" > "$SYMBOLS" 2>/dev/null && [ -s "$SYMBOLS" ]; then
    grep -E '[[:space:]]_whisper_(full|init_from_file)' "$SYMBOLS" > /dev/null || die "Missing stt/whisper symbols: $BINARY"
    echo "STT: whisper symbols present"
else
    echo "STT: symbol table unavailable; feature not checked" >&2
fi
echo "Verified $APP: $SHORT_VERSION ($VERSION)"
