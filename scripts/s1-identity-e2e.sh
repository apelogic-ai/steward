#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

for command in bash docker jq; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

"${ROOT}/scripts/build-patched-mcp-gw.sh"
"${ROOT}/scripts/build-steward-mint-image.sh"

bash "${ROOT}/scripts/s0-0-openshell-spike.sh" \
  bash "${ROOT}/scripts/s1-identity-inside.sh"
