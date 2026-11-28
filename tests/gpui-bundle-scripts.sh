#!/bin/bash
# No GPUI app is built/launched, and no real Applications dir is used.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")/../scripts" && pwd)"
TEST_ROOT=$(mktemp -d -t gitterm-gpui-package-test.XXXXXX)
TEST_ROOT=$(cd "$TEST_ROOT" && pwd -P)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
export GITTERM_GPUI_INSTALL_DIR="$TEST_ROOT/Applications with spaces"
export GPUI_TEST_ROOT=$TEST_ROOT
mkdir -p "$TEST_ROOT/bin" "$TEST_ROOT/fixtures"
# Mock external checks via PATH, never via a production skip/bypass flag.
cat > "$TEST_ROOT/bin/mock" <<'MOCK'
#!/bin/bash
set -eu
case "$(basename "$0")" in
    ps)
        [ ! -f "$GPUI_TEST_ROOT/ps-fail" ] || exit 1
        cat "$GPUI_TEST_ROOT/processes"
        ;;
    ditto)
        [ ! -f "$GPUI_TEST_ROOT/copy-fail" ] || exit 1
        /usr/bin/ditto "$@"
        if [ -f "$GPUI_TEST_ROOT/running-on-copy" ]; then echo '/tmp/gitterm-gpui' > "$GPUI_TEST_ROOT/processes"; fi
        ;;
    codesign)
        for app; do :; done
        if [ "$1" = --force ]; then touch "$app/signed"; exit; fi
        [ -f "$app/signed" ] || exit 1
        if [ -f "$GPUI_TEST_ROOT/final-sign-fail" ] && [ "$app" = "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.app" ]; then exit 1; fi
        ;;
    lipo)
        [ -f "$(dirname "$1")/arm64" ]
        ;;
    nm)
        for binary; do :; done
        if [ -f "$(dirname "$binary")/stripped" ]; then exit 1; fi
        if [ -f "$(dirname "$binary")/stt" ]; then
            echo '0000000100000000 T _whisper_init_from_file_with_params'
        else
            echo '0000000100000000 T _main'
        fi
        ;;
esac
MOCK
chmod +x "$TEST_ROOT/bin/mock"
for tool in ps ditto codesign lipo nm; do ln -s mock "$TEST_ROOT/bin/$tool"; done
export PATH="$TEST_ROOT/bin:$PATH"
: > "$TEST_ROOT/processes"
fail() { echo "FAIL: $*" >&2; cat "$TEST_ROOT/failure.log" >&2; exit 1; }
expect_failure() {
    if "$@" > "$TEST_ROOT/failure.log" 2>&1; then fail "Unexpected success: $*"; fi
}
make_app() {
    local app=$1 version=$2
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleName</key><string>GitTerm GPUI</string>
<key>CFBundleIdentifier</key><string>com.cree8.gitterm.gpui</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>$version</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleExecutable</key><string>gitterm-gpui</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSMicrophoneUsageDescription</key><string>Dictation</string>
</dict></plist>
PLIST
    echo 'fixture, never executed' > "$app/Contents/MacOS/gitterm-gpui"
    chmod +x "$app/Contents/MacOS/gitterm-gpui"
    echo icon > "$app/Contents/Resources/AppIcon.icns"
    touch "$app/signed" "$app/Contents/MacOS/arm64" "$app/Contents/MacOS/stt"
}
installed_version() {
    /usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.app/Contents/Info.plist"
}
assert_version() { [ "$(installed_version)" = "$1" ] || fail "Expected installed $1"; }
backup_count() {
    local apps=("$GITTERM_GPUI_INSTALL_DIR"/GitTerm\ GPUI.prev-*.app)
    echo "${#apps[@]}"
}
shopt -s nullglob
for version in one two three four five six; do make_app "$TEST_ROOT/fixtures/$version.app" "$version"; done
INSTALL="$SCRIPT_DIR/install-gpui.sh"
VERIFY="$SCRIPT_DIR/verify-gpui-bundle.sh"
expect_failure "$INSTALL" --rollback
"$INSTALL" "$TEST_ROOT/fixtures/one.app"
assert_version one
mkdir "$GITTERM_GPUI_INSTALL_DIR/.gitterm-gpui-install.lock"
expect_failure "$INSTALL" "$TEST_ROOT/fixtures/two.app"
assert_version one
rmdir "$GITTERM_GPUI_INSTALL_DIR/.gitterm-gpui-install.lock"
"$INSTALL" "$TEST_ROOT/fixtures/two.app" > "$TEST_ROOT/install.log"
grep -F 'one -> two' "$TEST_ROOT/install.log" > /dev/null
assert_version two
[ -d "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.prev-one.app" ] || fail 'Missing first backup'
"$INSTALL" --rollback
assert_version one
[ -d "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.prev-two.app" ] || fail 'Rollback discarded displaced build'
for version in three four five six; do "$INSTALL" "$TEST_ROOT/fixtures/$version.app" > /dev/null; done
[ "$(backup_count)" -eq 3 ] || fail 'Retention must keep exactly three'
[ ! -e "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.prev-one.app" ] || fail 'Oldest backup retained'
[ ! -e "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.prev-two.app" ] || fail 'Old backup retained'
"$INSTALL" --rollback > /dev/null
assert_version five
# Reinstall a version already in the backup list: preserve both copies.
"$INSTALL" "$TEST_ROOT/fixtures/five.app" > /dev/null
"$INSTALL" "$TEST_ROOT/fixtures/five.app" > /dev/null
[ "$(backup_count)" -eq 3 ] || fail 'Duplicate version retention failed'

for process in '/tmp/elsewhere/gitterm-gpui' '/tmp/Other/GitTerm GPUI.app/Contents/MacOS/gitterm-gpui' 'gitterm-gpui'; do
    echo "$process" > "$TEST_ROOT/processes"
    for argument in "$TEST_ROOT/fixtures/six.app" --rollback; do
        expect_failure "$INSTALL" "$argument"
        grep -F 'is running' "$TEST_ROOT/failure.log" > /dev/null
        assert_version five
    done
done
: > "$TEST_ROOT/processes"
for failure in ps-fail copy-fail final-sign-fail running-on-copy; do
    touch "$TEST_ROOT/$failure"
    expect_failure "$INSTALL" "$TEST_ROOT/fixtures/six.app"
    assert_version five
    rm "$TEST_ROOT/$failure"
    : > "$TEST_ROOT/processes"
done
# A failed rollback must preserve both the installed app and rollback source.
touch "$TEST_ROOT/final-sign-fail"
before=$(backup_count)
expect_failure "$INSTALL" --rollback
assert_version five
[ "$(backup_count)" -eq "$before" ] || fail 'Failed rollback lost backup'
rm "$TEST_ROOT/final-sign-fail"
echo '/tmp/gitterm-v6' > "$TEST_ROOT/processes"
"$INSTALL" "$TEST_ROOT/fixtures/six.app" > /dev/null
assert_version six

GOOD="$TEST_ROOT/fixtures/six.app"
for key in CFBundleName CFBundleIdentifier CFBundlePackageType CFBundleVersion CFBundleShortVersionString CFBundleExecutable CFBundleIconFile LSMinimumSystemVersion NSMicrophoneUsageDescription; do
    /usr/bin/ditto "$GOOD" "$TEST_ROOT/bad.app"
    /usr/libexec/PlistBuddy -c "Delete :$key" "$TEST_ROOT/bad.app/Contents/Info.plist"
    expect_failure "$VERIFY" "$TEST_ROOT/bad.app"
    rm -rf "$TEST_ROOT/bad.app"
done
for key in CFBundleVersion CFBundleShortVersionString; do
    /usr/bin/ditto "$GOOD" "$TEST_ROOT/bad.app"
    /usr/libexec/PlistBuddy -c "Set :$key 0.0.0" "$TEST_ROOT/bad.app/Contents/Info.plist"
    expect_failure "$VERIFY" "$TEST_ROOT/bad.app"
    rm -rf "$TEST_ROOT/bad.app"
done
for missing in signed Contents/MacOS/gitterm-gpui Contents/MacOS/arm64 Contents/MacOS/stt Contents/Resources/AppIcon.icns; do
    /usr/bin/ditto "$GOOD" "$TEST_ROOT/bad.app"
    rm "$TEST_ROOT/bad.app/$missing"
    expect_failure "$VERIFY" "$TEST_ROOT/bad.app"
    expect_failure "$INSTALL" "$TEST_ROOT/bad.app"
    assert_version six
    rm -rf "$TEST_ROOT/bad.app"
done
touch "$GOOD/Contents/MacOS/stripped"
"$VERIFY" "$GOOD" > /dev/null 2> "$TEST_ROOT/stt.log"
grep -F 'feature not checked' "$TEST_ROOT/stt.log" > /dev/null

# Exercise the real packaging/stamping script in a disposable Git repository,
# with only Cargo and platform checks mocked (no release compilation).
REPO="$TEST_ROOT/repo"
mkdir -p "$REPO/scripts" "$REPO/gpui" "$REPO/assets"
cp "$SCRIPT_DIR/bundle-gpui.sh" "$SCRIPT_DIR/verify-gpui-bundle.sh" "$REPO/scripts/"
printf '[package]\nversion = "0.1.0"\n' > "$REPO/gpui/Cargo.toml"
echo icon > "$REPO/assets/icon.icns"
cat > "$REPO/scripts/cargo.sh" <<'CARGO'
#!/bin/bash
set -eu
[[ " $* " = *' --release '* && " $* " = *' --locked '* && " $* " = *' --features stt '* ]]
[ "$CARGO_BUILD_JOBS" = 4 ]
mkdir -p "$CARGO_TARGET_DIR/release"
echo 'fixture, never executed' > "$CARGO_TARGET_DIR/release/gitterm-gpui"
chmod +x "$CARGO_TARGET_DIR/release/gitterm-gpui"
mkdir -p "$CARGO_TARGET_DIR/GitTerm GPUI.app/Contents/MacOS"
touch "$CARGO_TARGET_DIR/GitTerm GPUI.app/Contents/MacOS/arm64" "$CARGO_TARGET_DIR/GitTerm GPUI.app/Contents/MacOS/stt"
CARGO
chmod +x "$REPO/scripts/cargo.sh"
git -C "$REPO" init -q
git -C "$REPO" add .
git -C "$REPO" -c user.name='Package Test' -c user.email='noreply@example.com' -c core.hooksPath=/dev/null commit -qm 'Fixture'
export CARGO_TARGET_DIR="$TEST_ROOT/target"
"$REPO/scripts/bundle-gpui.sh" > /dev/null
PLIST="$CARGO_TARGET_DIR/GitTerm GPUI.app/Contents/Info.plist"
STAMP=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$PLIST")
SHA=$(git -C "$REPO" rev-parse --short HEAD)
[[ "$STAMP" =~ ^$SHA\+[0-9]{8}T[0-9]{6}Z$ ]] || fail "Bad clean build stamp: $STAMP"
echo dirty >> "$REPO/assets/icon.icns"
"$REPO/scripts/bundle-gpui.sh" > /dev/null
STAMP=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$PLIST")
[[ "$STAMP" =~ ^$SHA-dirty\+[0-9]{8}T[0-9]{6}Z$ ]] || fail "Bad dirty build stamp: $STAMP"

# Smoke-test the actual macOS lipo/nm/codesign commands with a tiny arm64 Mach-O
# fixture, ad-hoc signed without a signing identity/keychain. Never execute it.
REAL="$TEST_ROOT/real.app"
make_app "$REAL" real
rm "$REAL/signed" "$REAL/Contents/MacOS/arm64" "$REAL/Contents/MacOS/stt"
cat > "$TEST_ROOT/fixture.c" <<'C'
void whisper_init_from_file(void) {}
int main(void) { return 0; }
C
/usr/bin/clang -arch arm64 "$TEST_ROOT/fixture.c" -o "$REAL/Contents/MacOS/gitterm-gpui"
/usr/bin/codesign --force --deep -s - "$REAL"
PATH=/usr/bin:/bin:/usr/sbin:/sbin "$VERIFY" "$REAL"
# A failed first install leaves no incomplete installed app.
export GITTERM_GPUI_INSTALL_DIR="$TEST_ROOT/empty-install"
touch "$TEST_ROOT/final-sign-fail"
expect_failure "$INSTALL" "$TEST_ROOT/fixtures/one.app"
[ ! -e "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.app" ] || fail 'Failed first install retained app'
rm "$TEST_ROOT/final-sign-fail"
# The first versioned deployment must be able to restore the old 0.0.0 app.
export GITTERM_GPUI_INSTALL_DIR="$TEST_ROOT/legacy-install"
make_app "$GITTERM_GPUI_INSTALL_DIR/GitTerm GPUI.app" 0.0.0
"$INSTALL" "$TEST_ROOT/fixtures/one.app" > /dev/null
"$INSTALL" --rollback > /dev/null
assert_version 0.0.0
echo 'PASS: GPUI packaging, verification, install, retention, rollback and refusal/failure paths'
