#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository_root"

if [[ "${PCA_DISABLE_TOOLCHAIN_FALLBACK:-0}" != "1" ]] && [[ -d "/opt/homebrew/opt/rustup/bin" ]]; then
  PATH="/opt/homebrew/opt/rustup/bin:$PATH"
  export PATH
fi

for required_tool in cargo rustc swift pnpm python3; do
  if ! command -v "$required_tool" >/dev/null 2>&1; then
    echo "missing required tool: $required_tool" >&2
    exit 1
  fi
done

./scripts/verify-structural.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
swift build --package-path platform/macos
swift run --package-path platform/macos BridgeContractVerifier
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test
python3 scripts/verify_migrations.py .
python3 scripts/verify_boundaries.py .

echo "FULL VERIFICATION PASSED"
