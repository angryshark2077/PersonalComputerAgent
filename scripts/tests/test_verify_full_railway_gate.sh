#!/usr/bin/env bash
set -euo pipefail

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture_directory=$(mktemp -d)
trap 'rm -rf "$fixture_directory"' EXIT

mkdir -p "$fixture_directory/scripts/tests" "$fixture_directory/bin"
cp "$repository_root/scripts/verify-full.sh" "$fixture_directory/scripts/verify-full.sh"

cat >"$fixture_directory/scripts/verify-structural.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
EOF
chmod +x "$fixture_directory/scripts/verify-structural.sh"

cat >"$fixture_directory/scripts/tests/test_verify_railway_deployment.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "fixture Railway deployment verifier executed"
EOF
chmod +x "$fixture_directory/scripts/tests/test_verify_railway_deployment.sh"

for tool in cargo rustc swift pnpm python3 xcodebuild; do
  cat >"$fixture_directory/bin/$tool" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
EOF
  chmod +x "$fixture_directory/bin/$tool"
done

output=$(cd "$fixture_directory" && \
  PATH="$fixture_directory/bin:$PATH" \
  PCA_DISABLE_TOOLCHAIN_FALLBACK=1 \
  ./scripts/verify-full.sh)

deployment_line=$(printf '%s\n' "$output" | grep -n 'fixture Railway deployment verifier executed' | cut -d: -f1)
success_line=$(printf '%s\n' "$output" | grep -n '^FULL VERIFICATION PASSED$' | cut -d: -f1)

[[ -n "$deployment_line" ]] || {
  echo "expected Railway deployment verifier to execute" >&2
  exit 1
}
[[ -n "$success_line" ]] || {
  echo "expected full verification success" >&2
  exit 1
}
[[ "$deployment_line" -lt "$success_line" ]] || {
  echo "expected Railway deployment verifier before final success" >&2
  exit 1
}

echo "Full verification Railway gate test passed."
