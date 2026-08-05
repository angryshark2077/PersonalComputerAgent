#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "S1A bundle verification failed: $1" >&2
  exit 1
}

[[ $# -eq 3 && "$1" == "--team-id" && "$2" =~ ^[A-Z0-9]{10}$ ]] \
  || fail "usage: verify-s1a-bundle.sh --team-id TEAMID <app-or-dmg>"
team_id=$2
input=$3
[[ -e "$input" ]] || fail "input does not exist"
[[ ! -L "$input" ]] || fail "input must not be a symbolic link"

temporary_directory=""
mount_point=""
attached_device=""
mounted=0
cleanup() {
  if [[ "$mounted" -eq 1 ]]; then
    detach_target=${attached_device:-$mount_point}
    if ! hdiutil detach "$detach_target" >/dev/null 2>&1; then
      echo "S1A bundle verification cleanup warning: could not detach $detach_target" >&2
    fi
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
    attach_plist=$(hdiutil attach -readonly -nobrowse -plist -mountpoint "$mount_point" "$input") \
      || fail "could not attach DMG read-only"
    mounted=1
    command -v python3 >/dev/null 2>&1 || fail "python3 is required to parse hdiutil output"
    attached_device=$(python3 -c '
import os, plistlib, sys
requested = os.path.realpath(sys.argv[1])
document = plistlib.load(sys.stdin.buffer)
matches = [
    item.get("dev-entry")
    for item in document.get("system-entities", [])
    if isinstance(item.get("mount-point"), str)
    and os.path.realpath(item["mount-point"]) == requested
    and isinstance(item.get("dev-entry"), str)
]
if len(matches) != 1 or not matches[0].startswith("/dev/disk"):
    raise SystemExit(1)
print(matches[0])
' "$mount_point" <<<"$attach_plist") || fail "could not identify the exact mounted DMG device"
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
bridge_app="$app/Contents/Helpers/PCAPlatformBridge.app"
bridge="$bridge_app/Contents/MacOS/PCAPlatformBridge"
wechat_repair="$app/Contents/Resources/bin/pca-wechat-repair"
ffmpeg="$app/Contents/Resources/bin/ffmpeg"
launch_agent="$app/Contents/Library/LaunchAgents/com.pca.agentd.plist"

[[ -f "$info" ]] || fail "missing Contents/Info.plist"
[[ -f "$bridge_app/Contents/Info.plist" ]] || fail "missing Bridge helper Info.plist"
[[ -f "$launch_agent" ]] || fail "missing com.pca.agentd.plist"

for binary in "$main" "$agent" "$bridge" "$wechat_repair" "$ffmpeg"; do
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
[[ "$(plutil -extract CFBundleIdentifier raw -o - "$bridge_app/Contents/Info.plist")" == "com.pca.PersonalComputerAgent.PlatformBridge" ]] || fail "wrong Bridge helper bundle identifier"
[[ -n "$(plutil -extract NSLocationWhenInUseUsageDescription raw -o - "$bridge_app/Contents/Info.plist")" ]] || fail "missing Bridge helper location usage description"
[[ -n "$(plutil -extract NSScreenCaptureUsageDescription raw -o - "$bridge_app/Contents/Info.plist")" ]] || fail "missing Bridge helper screen capture usage description"
[[ -n "$(plutil -extract NSPhotoLibraryUsageDescription raw -o - "$bridge_app/Contents/Info.plist")" ]] || fail "missing Bridge helper Photos usage description"
[[ -n "$(plutil -extract NSLocationUsageDescription raw -o - "$info")" ]] || fail "missing macOS location usage description"
[[ -n "$(plutil -extract NSPhotoLibraryUsageDescription raw -o - "$info")" ]] || fail "missing macOS Photos usage description"
[[ "$(plutil -extract CFBundleExecutable raw -o - "$info")" == "PersonalComputerAgent" ]] || fail "wrong bundle executable"
[[ "$(plutil -extract LSUIElement raw -o - "$info")" == "true" ]] || fail "LSUIElement must be true"
version=$(plutil -extract CFBundleShortVersionString raw -o - "$info")
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "bundle version must be three numeric components"

for permission_target in "$app" "$bridge_app"; do
  permission_entitlements=$(codesign -d --entitlements :- "$permission_target" 2>/dev/null) \
    || fail "could not inspect signature privacy entitlements for $(basename "$permission_target")"
  permissions_allowed=$(python3 -c '
import plistlib, sys
values = plistlib.load(sys.stdin.buffer)
for key in (
    "com.apple.security.personal-information.location",
    "com.apple.security.personal-information.photos-library",
):
    if values.get(key) is not True:
        raise SystemExit(1)
print("true")
' <<<"$permission_entitlements" 2>/dev/null) \
    || fail "missing location or Photos entitlement for $(basename "$permission_target")"
  [[ "$permissions_allowed" == "true" ]] \
    || fail "privacy entitlements must be enabled for $(basename "$permission_target")"
done

[[ "$(plutil -extract Label raw -o - "$launch_agent")" == "com.pca.agentd" ]] || fail "wrong LaunchAgent label"
[[ "$(plutil -extract BundleProgram raw -o - "$launch_agent")" == "Contents/Resources/bin/pca-agentd" ]] || fail "wrong BundleProgram"
arguments=$(plutil -extract ProgramArguments json -o - "$launch_agent" | tr -d '[:space:]')
[[ "$arguments" == '["pca-agentd","run"]' ]] || fail "wrong LaunchAgent ProgramArguments"
[[ "$(plutil -extract RunAtLoad raw -o - "$launch_agent")" == "true" ]] || fail "RunAtLoad must be true"
[[ "$(plutil -extract KeepAlive raw -o - "$launch_agent")" == "true" ]] || fail "KeepAlive must be true"

links=$(find "$app" -type l -print -quit) || fail "could not enumerate bundle for symbolic links"
[[ -z "$links" ]] || fail "symbolic links are not allowed in the S1A bundle"
runtime_directories=$(find "$app" -type d \( -name Data -o -name Run \) -print -quit) \
  || fail "could not enumerate bundle for writable runtime directories"
[[ -z "$runtime_directories" ]] || fail "writable Data or Run directories must not exist inside the bundle"

metadata=()
codesign --verify --strict --verbose=2 "$bridge_app" >/dev/null 2>&1 \
  || fail "signature verification failed for $(basename "$bridge_app")"
bridge_app_signature=$(codesign -d --verbose=4 "$bridge_app" 2>&1) \
  || fail "could not inspect TeamIdentifier for $(basename "$bridge_app")"
grep -Fxq "TeamIdentifier=$team_id" <<<"$bridge_app_signature" \
  || fail "TeamIdentifier mismatch for $(basename "$bridge_app")"
for signed_target in "$app" "$main" "$agent" "$bridge" "$wechat_repair" "$ffmpeg"; do
  codesign --verify --strict --verbose=2 "$signed_target" >/dev/null 2>&1 \
    || fail "signature verification failed for $(basename "$signed_target")"
  signature_details=$(codesign -d --verbose=4 "$signed_target" 2>&1) \
    || fail "could not inspect TeamIdentifier for $(basename "$signed_target")"
  grep -Fxq "TeamIdentifier=$team_id" <<<"$signature_details" \
    || fail "TeamIdentifier mismatch for $(basename "$signed_target")"
  cdhash=$(sed -n 's/^CDHash=//p' <<<"$signature_details")
  [[ "$cdhash" =~ ^[0-9A-Fa-f]{40}$ ]] \
    || fail "invalid CDHash for $(basename "$signed_target")"
  metadata+=("$cdhash")
done

if [[ "$mounted" -eq 1 ]]; then
  hdiutil detach "$attached_device" >/dev/null 2>&1 || fail "could not detach attached DMG device"
  mounted=0
fi

echo "S1A BUNDLE VERIFIED: $version arm64"
echo "S1A_BUNDLE_METADATA version=$version team_id=$team_id app_cdhash=${metadata[0]} main_cdhash=${metadata[1]} agent_cdhash=${metadata[2]} bridge_cdhash=${metadata[3]} wechat_repair_cdhash=${metadata[4]} ffmpeg_cdhash=${metadata[5]}"
