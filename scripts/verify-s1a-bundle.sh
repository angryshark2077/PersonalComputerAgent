#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "S1A bundle verification failed: $1" >&2
  exit 1
}

[[ $# -eq 1 ]] || fail "usage: verify-s1a-bundle.sh <app-or-dmg>"
input=$1
[[ -e "$input" ]] || fail "input does not exist"
[[ ! -L "$input" ]] || fail "input must not be a symbolic link"

temporary_directory=""
mount_point=""
mounted=0
cleanup() {
  if [[ "$mounted" -eq 1 ]]; then
    hdiutil detach "$mount_point" >/dev/null 2>&1 || true
  fi
  if [[ -n "$temporary_directory" && -d "$temporary_directory" ]]; then
    rm -rf "$temporary_directory"
  fi
}
trap cleanup EXIT INT TERM

case "$input" in
  *.app)
    [[ -d "$input" ]] || fail "app input is not a directory"
    app=$input
    ;;
  *.dmg)
    [[ -f "$input" ]] || fail "DMG input is not a regular file"
    temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/pca-s1a-verify.XXXXXX")
    mount_point="$temporary_directory/mount"
    mkdir -m 0700 "$mount_point"
    hdiutil attach -readonly -nobrowse -mountpoint "$mount_point" "$input" >/dev/null
    mounted=1
    shopt -s nullglob dotglob
    payload=("$mount_point"/*)
    [[ ${#payload[@]} -eq 1 ]] || fail "DMG must contain a single app and no unexpected payload"
    [[ -d "${payload[0]}" && "${payload[0]}" == *.app ]] || fail "DMG payload must be one app"
    app=${payload[0]}
    ;;
  *) fail "input must be an .app or .dmg" ;;
esac

info="$app/Contents/Info.plist"
main="$app/Contents/MacOS/PersonalComputerAgent"
agent="$app/Contents/Resources/bin/pca-agentd"
bridge="$app/Contents/Resources/bin/PCAPlatformBridge"
launch_agent="$app/Contents/Library/LaunchAgents/com.pca.agentd.plist"

[[ -f "$info" ]] || fail "missing Contents/Info.plist"
[[ -f "$launch_agent" ]] || fail "missing com.pca.agentd.plist"

for binary in "$main" "$agent" "$bridge"; do
  [[ -f "$binary" ]] || fail "missing $(basename "$binary")"
  [[ -x "$binary" ]] || fail "$(basename "$binary") is not executable"
  mode=$(stat -f '%Lp' "$binary")
  (( (8#$mode & 8#022) == 0 )) || fail "$(basename "$binary") is group/world writable"
  arches=$(lipo -archs "$binary") || fail "cannot inspect architecture for $(basename "$binary")"
  [[ "$arches" == "arm64" ]] || fail "$(basename "$binary") must contain exactly arm64"
done

plist_mode=$(stat -f '%Lp' "$launch_agent")
(( (8#$plist_mode & 8#111) == 0 )) || fail "LaunchAgent plist must not be executable"
(( (8#$plist_mode & 8#022) == 0 )) || fail "LaunchAgent plist is group/world writable"

[[ "$(plutil -extract CFBundleIdentifier raw -o - "$info")" == "com.pca.PersonalComputerAgent" ]] || fail "wrong bundle identifier"
[[ "$(plutil -extract CFBundleExecutable raw -o - "$info")" == "PersonalComputerAgent" ]] || fail "wrong bundle executable"
[[ "$(plutil -extract LSUIElement raw -o - "$info")" == "true" ]] || fail "LSUIElement must be true"
version=$(plutil -extract CFBundleShortVersionString raw -o - "$info")
[[ -n "$version" ]] || fail "missing bundle version"

[[ "$(plutil -extract Label raw -o - "$launch_agent")" == "com.pca.agentd" ]] || fail "wrong LaunchAgent label"
[[ "$(plutil -extract BundleProgram raw -o - "$launch_agent")" == "Contents/Resources/bin/pca-agentd" ]] || fail "wrong BundleProgram"
arguments=$(plutil -extract ProgramArguments json -o - "$launch_agent" | tr -d '[:space:]')
[[ "$arguments" == '["pca-agentd","run"]' ]] || fail "wrong LaunchAgent ProgramArguments"
[[ "$(plutil -extract RunAtLoad raw -o - "$launch_agent")" == "true" ]] || fail "RunAtLoad must be true"
[[ "$(plutil -extract KeepAlive raw -o - "$launch_agent")" == "true" ]] || fail "KeepAlive must be true"

if find "$app" -type l -print -quit | grep -q .; then
  fail "symbolic links are not allowed in the S1A bundle"
fi
if find "$app" -type d \( -name Data -o -name Run \) -print -quit | grep -q .; then
  fail "writable Data or Run directories must not exist inside the bundle"
fi

for nested in "$agent" "$bridge"; do
  codesign --verify --strict --verbose=2 "$nested" >/dev/null 2>&1 || fail "nested signature verification failed for $(basename "$nested")"
done
codesign --verify --deep --strict --verbose=2 "$app" >/dev/null 2>&1 || fail "app signature verification failed"

echo "S1A BUNDLE VERIFIED: $version arm64"
