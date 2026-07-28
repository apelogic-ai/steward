#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MINT_IMAGE="${STEWARD_MINT_IMAGE:-steward/mint:s1}"

if ! command -v docker >/dev/null 2>&1; then
  echo "required command is missing: docker" >&2
  exit 2
fi

docker build \
  --progress plain \
  --file "${ROOT}/config/s1/steward-mint.Dockerfile" \
  --tag "${MINT_IMAGE}" \
  "${ROOT}"

echo "${MINT_IMAGE}"
