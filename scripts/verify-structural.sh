#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository_root"

required_files=(
  "00_START_HERE.md"
  "AGENTS.md"
  "Cargo.toml"
  "package.json"
  "packages/contracts/registry.json"
  "tasks/S0_ENGINEERING_BASELINE.md"
)
for required_file in "${required_files[@]}"; do
  if [[ ! -f "$required_file" ]]; then
    echo "missing required file: $required_file" >&2
    exit 1
  fi
done

python3 scripts/verify_contracts.py
python3 scripts/verify_migrations.py .
python3 scripts/verify_boundaries.py .
python3 -m unittest scripts.tests.test_engineering_gates
find scripts -name '*.sh' -type f -exec bash -n {} +

echo "STRUCTURAL VERIFICATION PASSED"
