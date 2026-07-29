#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="s5-$(date -u +%Y%m%d%H%M%S)-$$"
CONTROLLER_IMAGE="steward/controller:${RUN_ID}"
MCP_GW_IMAGE="steward/mcp-gw:c2af10d9-claims"
CLUSTER_NAME="steward-${RUN_ID}"
RUN_DIR="${ROOT}/.steward-run/${RUN_ID}"
KUBECONFIG_PATH="${RUN_DIR}/kubeconfig"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  if [[ "${STEWARD_DEV_KEEP:-0}" != "1" ]]; then
    KUBECONFIG="${KUBECONFIG_PATH}" kind delete cluster \
      --name "${CLUSTER_NAME}" >/dev/null 2>&1 || true
    find "${RUN_DIR}" -depth -delete 2>/dev/null || true
  fi
  docker image rm "${CONTROLLER_IMAGE}" >/dev/null 2>&1 || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in bash docker kind; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

"${ROOT}/scripts/build-patched-mcp-gw.sh"
"${ROOT}/scripts/build-steward-mint-image.sh"
docker build \
  --file "${ROOT}/e2e/Dockerfile.s2" \
  --label "steward.test/run-id=${RUN_ID}" \
  --tag "${CONTROLLER_IMAGE}" \
  "${ROOT}"

STEWARD_E2E_SLICE=s5 \
STEWARD_RUN_ID="${RUN_ID}" \
STEWARD_S2_CONTROLLER_IMAGE="${CONTROLLER_IMAGE}" \
STEWARD_S5_MCP_GW_IMAGE="${MCP_GW_IMAGE}" \
  bash "${ROOT}/scripts/s0-0-openshell-spike.sh" \
  bash "${ROOT}/scripts/s2-inference-inside.sh"
