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

pnpm install --frozen-lockfile
./scripts/verify-structural.sh
bash scripts/tests/test_verify_railway_deployment.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p pca-agentd --features process-test-hooks \
  --test cloud_control_process \
  --test process_lifecycle \
  --test system_collector_process \
  --test collector_commit_kill
cargo build -p pca-agentd --features process-test-hooks --bin pca-s1b-acceptance-agent
PCA_S1B_ACCEPTANCE_AGENT="$repository_root/target/debug/pca-s1b-acceptance-agent" \
  pnpm --filter @pca/cloud-api exec node --import tsx --test \
    "$repository_root/scripts/tests/s1b_pairing_acceptance.ts"
swift build --package-path platform/macos
swift run --package-path platform/macos BridgeContractVerifier
xcodebuild test \
  -project platform/macos/PersonalComputerAgent.xcodeproj \
  -scheme PersonalComputerAgent \
  -only-testing:PersonalComputerAgentTests/PairingCoordinatorTests \
  -derivedDataPath /tmp/pca-verify-full-pairing
pnpm typecheck
pnpm test
python3 scripts/verify_migrations.py .
python3 scripts/verify_cloud_migrations.py .
python3 scripts/verify_boundaries.py .

echo "FULL VERIFICATION PASSED"
