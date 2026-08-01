#!/usr/bin/env bash
set -euo pipefail

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
dockerignore="$repository_root/.dockerignore"

[[ -f "$dockerignore" ]] || {
  echo "missing root .dockerignore" >&2
  exit 1
}

for required_pattern in '.env*' 'node_modules' '.next' 'dist' '.worktrees' '.git'; do
  if ! grep -Fqx "$required_pattern" "$dockerignore"; then
    echo "missing .dockerignore pattern: $required_pattern" >&2
    exit 1
  fi
done

echo "Docker build context exclusions are present."
