#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: verify-railway-deployment.sh <dashboard-origin> <api-origin>" >&2
  exit 2
}

fail() {
  echo "Railway deployment verification failed: $1" >&2
  exit 1
}

is_https_origin() {
  [[ "$1" =~ ^https://[^/?#[:space:]]+/?$ ]]
}

[[ "$#" -eq 2 ]] || usage
dashboard_origin=${1%/}
api_origin=${2%/}
is_https_origin "$dashboard_origin" || fail "dashboard origin must be a public HTTPS origin"
is_https_origin "$api_origin" || fail "API origin must be a public HTTPS origin"

curl_command=${PCA_RAILWAY_CURL:-curl}
command -v "$curl_command" >/dev/null 2>&1 || fail "curl command is unavailable: $curl_command"

check_health() {
  local service=$1 origin=$2 response
  if ! response=$("$curl_command" --fail --silent --show-error --location --max-time 15 "$origin/healthz"); then
    fail "$service health request failed"
  fi

  if [[ "$response" =~ [Dd][Aa][Tt][Aa][Bb][Aa][Ss][Ee]_[Uu][Rr][Ll]|[Tt][Oo][Kk][Ee][Nn]|[Kk][Ee][Yy][Cc][Hh][Aa][Ii][Nn] ]]; then
    fail "$service health response contains sensitive wording"
  fi

  if ! printf '%s' "$response" | node -e '
let body = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => { body += chunk; });
process.stdin.on("end", () => {
  try {
    const parsed = JSON.parse(body);
    process.exit(parsed && parsed.status === "ok" ? 0 : 1);
  } catch {
    process.exit(1);
  }
});
'; then
    fail "$service health response is not healthy JSON"
  fi
}

check_health "Dashboard" "$dashboard_origin"
check_health "Cloud API" "$api_origin"
echo "Railway public health verification passed."
