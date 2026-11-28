#!/bin/bash
# Stage before replacing; never terminate a running app. Only GPUI is managed.
set -euo pipefail
shopt -s nullglob
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
die() { echo "install-gpui: $*" >&2; exit 1; }
[ "$#" -eq 1 ] || die "Usage: $0 <built .app> | --rollback"
INSTALL_DIR=${GITTERM_GPUI_INSTALL_DIR:-"$HOME/Applications"}
[[ "$INSTALL_DIR" = /* ]] || die "Install directory must be absolute: $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
INSTALL_DIR=$(cd "$INSTALL_DIR" && pwd -P)
CURRENT="$INSTALL_DIR/GitTerm GPUI.app"
LOCK="$INSTALL_DIR/.gitterm-gpui-install.lock"
mkdir "$LOCK" 2>/dev/null || die "Another install is active (or inspect stale lock): $LOCK"
STAGE=
ARCHIVE=
REPLACED=false
COMMITTED=false
cleanup() {
    local result=$?
    trap - EXIT
    if [ "$COMMITTED" = false ] && [ -n "$ARCHIVE" ]; then
        if [ "$REPLACED" = true ]; then rm -rf -- "$CURRENT"; fi
        if ! mv -- "$ARCHIVE" "$CURRENT"; then
            echo "Recovery failed; previous app remains at $ARCHIVE" >&2
            result=1
        fi
    elif [ "$COMMITTED" = false ] && [ "$REPLACED" = true ]; then
        rm -rf -- "$CURRENT"
    fi
    if [ -n "$STAGE" ]; then rm -rf -- "$STAGE"; fi
    rmdir "$LOCK"
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
refuse_running() {
    local processes
    processes=$(ps -axww -o comm=) || die "Cannot inspect running processes; refusing replacement"
    # comm includes the executable path (and no argv), avoiding matches on this
    # script's arguments. Also catch a GPUI executable outside the install dir.
    if printf '%s\n' "$processes" | grep -E '(^|[[:space:]]|/)gitterm-gpui$|GitTerm GPUI[^/]*\.app/Contents/MacOS/' > /dev/null; then
        die "GitTerm GPUI is running; quit it before installing or rolling back"
    fi
}
version() {
    /usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$1/Contents/Info.plist"
}
backups() {
    BACKUPS=("$INSTALL_DIR"/GitTerm\ GPUI.prev-*.app)
    local app name sorted
    [ "${#BACKUPS[@]}" -gt 0 ] || return 0
    for app in "${BACKUPS[@]}"; do
        [ -d "$app" ] && [ ! -L "$app" ] || die "Refusing invalid backup: $app"
        name=${app##*/}
        [[ "$name" =~ ^GitTerm\ GPUI.prev-[a-zA-Z0-9._+-]+\.app$ ]] || die "Invalid backup name: $app"
    done
    # macOS ls compares subsecond mtimes; Bash 3.2's -nt/-ot only use seconds.
    sorted=$(ls -1dt -- "${BACKUPS[@]}") || die "Cannot order GPUI backups"
    BACKUPS=()
    while IFS= read -r app; do BACKUPS[${#BACKUPS[@]}]=$app; done <<< "$sorted"
}
refuse_running
backups
ROLLBACK=
if [ "$1" = --rollback ]; then
    [ "${#BACKUPS[@]}" -gt 0 ] || die "No previous GPUI bundle in $INSTALL_DIR"
    SOURCE=${BACKUPS[0]}
    ROLLBACK=$SOURCE
else
    [ -d "$1" ] || die "Bundle not found: $1"
    SOURCE=$(cd "$1" && pwd -P)
fi
verify_candidate() {
    if [ -n "$ROLLBACK" ]; then
        # The first versioned install may archive the previous 0.0.0 bundle.
        # Permit restoring that signed legacy app, while checking its identity.
        local plist="$1/Contents/Info.plist"
        [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist")" = com.cree8.gitterm.gpui ] || die "Wrong rollback bundle identity: $1"
        [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist")" = gitterm-gpui ] || die "Wrong rollback executable: $1"
        [ -x "$1/Contents/MacOS/gitterm-gpui" ] || die "Missing rollback executable: $1"
        codesign --verify --deep --strict "$1"
    else
        "$SCRIPT_DIR/verify-gpui-bundle.sh" "$1"
    fi
}
verify_candidate "$SOURCE"
NEW_VERSION=$(version "$SOURCE")
OLD_VERSION=none
HAD_CURRENT=false
if [ -e "$CURRENT" ] || [ -L "$CURRENT" ]; then
    [ -d "$CURRENT" ] && [ ! -L "$CURRENT" ] || die "Refusing invalid installed app: $CURRENT"
    OLD_VERSION=$(version "$CURRENT")
    [[ "$OLD_VERSION" =~ ^[a-zA-Z0-9][a-zA-Z0-9._+-]*$ ]] || die "Unsafe installed version: $OLD_VERSION"
    HAD_CURRENT=true
fi
STAGE=$(mktemp -d "$INSTALL_DIR/.gitterm-gpui-stage.XXXXXX")
ditto "$SOURCE" "$STAGE/GitTerm GPUI.app"
verify_candidate "$STAGE/GitTerm GPUI.app"
refuse_running
if [ "$HAD_CURRENT" = true ]; then
    DEST="$INSTALL_DIR/GitTerm GPUI.prev-$OLD_VERSION.app"
    # Reinstalling the same version must never overwrite an existing backup.
    if [ -e "$DEST" ] || [ -L "$DEST" ]; then
        DEST="$INSTALL_DIR/GitTerm GPUI.prev-$OLD_VERSION-$(date -u +%Y%m%dT%H%M%SZ)-$$.app"
    fi
    [ ! -e "$DEST" ] && [ ! -L "$DEST" ] || die "Backup already exists: $DEST"
    mv -- "$CURRENT" "$DEST"
    ARCHIVE=$DEST
    # Order by time archived, not by a bundle's original build/copy mtime.
    touch "$ARCHIVE"
fi
mv -- "$STAGE/GitTerm GPUI.app" "$CURRENT"
REPLACED=true
codesign --verify --deep --strict "$CURRENT"
COMMITTED=true
if [ -n "$ROLLBACK" ]; then rm -rf -- "$ROLLBACK"; fi
backups
while [ "${#BACKUPS[@]}" -gt 3 ]; do
    OLDEST=${BACKUPS[${#BACKUPS[@]}-1]}
    rm -rf -- "$OLDEST"
    backups
done
echo "GitTerm GPUI: $OLD_VERSION -> $NEW_VERSION"
echo "Installed at: $CURRENT"
