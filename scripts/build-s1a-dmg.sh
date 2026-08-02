#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo 'usage: build-s1a-dmg.sh --team-id TEAMID --identity "Apple Development: Name (IDENTIFIER)" --version VERSION --output PATH.dmg' >&2
  exit 2
}

team_id=""
identity=""
version=""
output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --team-id) [[ $# -ge 2 ]] || usage; team_id=$2; shift 2 ;;
    --identity) [[ $# -ge 2 ]] || usage; identity=$2; shift 2 ;;
    --version) [[ $# -ge 2 ]] || usage; version=$2; shift 2 ;;
    --output) [[ $# -ge 2 ]] || usage; output=$2; shift 2 ;;
    *) usage ;;
  esac
done

[[ "$team_id" =~ ^[A-Z0-9]{10}$ ]] || usage
[[ "$identity" != *$'\n'* && "$identity" == "Apple Development: "?* ]] || usage
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
[[ "$output" == *.dmg ]] || usage

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ "$output" = /* ]]; then
  final_output=$output
else
  final_output="$repository_root/$output"
fi
[[ ! -e "$final_output" && ! -L "$final_output" ]] || {
  echo "output already exists; refusing to overwrite it" >&2
  exit 1
}

for preflight_tool in security openssl; do
  command -v "$preflight_tool" >/dev/null 2>&1 || {
    echo "missing required build tool: $preflight_tool" >&2
    exit 1
  }
done
identities=$(security find-identity -v -p codesigning 2>/dev/null || true)
if ! grep -Fq -- "\"$identity\"" <<<"$identities"; then
  echo "requested Apple Development signing identity is not available in the current Keychain" >&2
  exit 1
fi
certificate=$(security find-certificate -c "$identity" -p 2>/dev/null || true)
certificate_subject=$(openssl x509 -noout -subject -nameopt RFC2253 <<<"$certificate" 2>/dev/null || true)
if ! grep -Eq "(^|,)OU=$team_id(,|$)" <<<"${certificate_subject#subject=}"; then
  echo "requested signing identity does not belong to --team-id" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1 && [[ -d "/opt/homebrew/opt/rustup/bin" ]]; then
  PATH="/opt/homebrew/opt/rustup/bin:$PATH"
  export PATH
fi
for tool in cargo swift xcodebuild codesign lipo hdiutil plutil python3; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required build tool: $tool" >&2; exit 1; }
done

project_dir="$repository_root/platform/macos"
temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/pca-s1a-build.XXXXXX")
build_inputs="$temporary_directory/build-inputs"
cleanup() {
  rm -rf "$temporary_directory"
}
trap cleanup EXIT INT TERM

mkdir -m 0700 -p "$build_inputs"

PCA_APP_VERSION="$version" cargo build \
  --manifest-path "$repository_root/Cargo.toml" \
  --release \
  --target aarch64-apple-darwin \
  -p pca-agentd \
  -p pca-wechat-repair
install -m 0755 \
  "$repository_root/target/aarch64-apple-darwin/release/pca-agentd" \
  "$build_inputs/pca-agentd"
install -m 0755 \
  "$repository_root/target/aarch64-apple-darwin/release/pca-wechat-repair" \
  "$build_inputs/pca-wechat-repair"

swift build \
  --package-path "$project_dir" \
  --configuration release \
  --arch arm64 \
  --product PCAPlatformBridge
swift_bin_path=$(swift build \
  --package-path "$project_dir" \
  --configuration release \
  --arch arm64 \
  --show-bin-path)
install -m 0755 "$swift_bin_path/PCAPlatformBridge" "$build_inputs/PCAPlatformBridge"
install -m 0644 \
  "$project_dir/PersonalComputerAgent/Resources/com.pca.agentd.plist" \
  "$build_inputs/com.pca.agentd.plist"

archive="$temporary_directory/PersonalComputerAgent.xcarchive"
PCA_PREBUILT_DIR="$build_inputs" xcodebuild archive \
  -project "$project_dir/PersonalComputerAgent.xcodeproj" \
  -scheme PersonalComputerAgent \
  -configuration Release \
  -destination 'generic/platform=macOS' \
  -archivePath "$archive" \
  ARCHS=arm64 \
  ONLY_ACTIVE_ARCH=NO \
  DEVELOPMENT_TEAM="$team_id" \
  CODE_SIGN_IDENTITY="$identity" \
  CODE_SIGN_STYLE=Manual \
  PCA_PREBUILT_DIR="$build_inputs" \
  MARKETING_VERSION="$version" \
  CURRENT_PROJECT_VERSION=1

archived_app="$archive/Products/Applications/PersonalComputerAgent.app"
[[ -d "$archived_app" ]] || { echo "Xcode archive did not produce the expected app" >&2; exit 1; }
dmg_root="$temporary_directory/dmg-root"
mkdir -m 0700 "$dmg_root"
app="$dmg_root/Install Personal Computer Agent.app"
cp -R "$archived_app" "$app"

agent="$app/Contents/Resources/bin/pca-agentd"
bridge="$app/Contents/Resources/bin/PCAPlatformBridge"
wechat_repair="$app/Contents/Resources/bin/pca-wechat-repair"
main="$app/Contents/MacOS/PersonalComputerAgent"
for binary in "$agent" "$bridge" "$wechat_repair" "$main"; do
  [[ "$(lipo -archs "$binary")" == "arm64" ]] || { echo "non-arm64 executable produced" >&2; exit 1; }
done

codesign --force --options runtime --timestamp=none --sign "$identity" "$agent"
codesign --force --options runtime --timestamp=none --sign "$identity" "$bridge"
codesign --force --options runtime --timestamp=none --sign "$identity" "$wechat_repair"
codesign \
  --force \
  --options runtime \
  --timestamp=none \
  --entitlements "$project_dir/PersonalComputerAgent/PersonalComputerAgent.entitlements" \
  --sign "$identity" \
  "$app"

signature_details=$(codesign -d --verbose=4 "$app" 2>&1)
grep -Fq "TeamIdentifier=$team_id" <<<"$signature_details" || {
  echo "signed app TeamIdentifier does not match --team-id" >&2
  exit 1
}
"$repository_root/scripts/verify-s1a-bundle.sh" --team-id "$team_id" "$app"

temporary_dmg="$temporary_directory/PersonalComputerAgent-S1A-arm64.dmg"
hdiutil create \
  -fs HFS+ \
  -format UDZO \
  -volname "Personal Computer Agent" \
  -srcfolder "$dmg_root" \
  "$temporary_dmg" >/dev/null
"$repository_root/scripts/verify-s1a-bundle.sh" --team-id "$team_id" "$temporary_dmg"

mkdir -p "$(dirname "$final_output")"
[[ ! -e "$final_output" && ! -L "$final_output" ]] || {
  echo "output appeared during build; refusing to overwrite it" >&2
  exit 1
}
mv "$temporary_dmg" "$final_output"
echo "S1A DMG CREATED: $final_output"
