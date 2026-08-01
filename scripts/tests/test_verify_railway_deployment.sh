#!/usr/bin/env bash
set -euo pipefail

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
verifier="$repository_root/scripts/verify-railway-deployment.sh"
test_script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")

if [[ "$(basename "$0")" == "fixture-curl" ]]; then
  printf '%s\n' "${PCA_RAILWAY_FIXTURE_RESPONSE:?fixture response is required}"
  exit 0
fi

fixture_directory=$(mktemp -d)
trap 'rm -rf "$fixture_directory"' EXIT
ln -s "$test_script" "$fixture_directory/fixture-curl"

expect_success() {
  PCA_RAILWAY_CURL=fixture-curl \
    PCA_RAILWAY_FIXTURE_RESPONSE='{"status":"ok"}' \
    PATH="$fixture_directory:$PATH" \
    "$verifier" https://dashboard.example https://api.example
}

expect_failure() {
  if "$@" >/dev/null 2>&1; then
    echo "expected verifier to fail: $*" >&2
    exit 1
  fi
}

expect_success
expect_failure env PCA_RAILWAY_CURL=fixture-curl PCA_RAILWAY_FIXTURE_RESPONSE='{"status":"ok","detail":"DATABASE_URL"}' PATH="$fixture_directory:$PATH" "$verifier" https://dashboard.example https://api.example
expect_failure env PCA_RAILWAY_CURL=fixture-curl PCA_RAILWAY_FIXTURE_RESPONSE='{"status":"ok"}' PATH="$fixture_directory:$PATH" "$verifier" https://dashboard.example
expect_failure env PCA_RAILWAY_CURL=fixture-curl PCA_RAILWAY_FIXTURE_RESPONSE='{"status":"ok"}' PATH="$fixture_directory:$PATH" "$verifier" http://dashboard.example https://api.example

echo "Railway deployment verifier tests passed."
