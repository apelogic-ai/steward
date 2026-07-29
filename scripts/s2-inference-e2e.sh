#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="s2-$(date -u +%Y%m%d%H%M%S)-$$"
CONTROLLER_IMAGE="steward/controller:${RUN_ID}"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker image rm "${CONTROLLER_IMAGE}" >/dev/null 2>&1 || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in bash docker; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

"${ROOT}/scripts/build-steward-mint-image.sh"
docker build \
  --file "${ROOT}/e2e/Dockerfile.s2" \
  --label "steward.test/run-id=${RUN_ID}" \
  --tag "${CONTROLLER_IMAGE}" \
  "${ROOT}"

STEWARD_RUN_ID="${RUN_ID}" \
STEWARD_S2_CONTROLLER_IMAGE="${CONTROLLER_IMAGE}" \
  bash "${ROOT}/scripts/s0-0-openshell-spike.sh" \
  bash "${ROOT}/scripts/s2-inference-inside.sh"
