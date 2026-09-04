#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MINT_IMAGE="steward/mint:s1"
MCP_GW_IMAGE="steward/mcp-gw:c2af10d9-claims"

for variable in STEWARD_RUN_ID STEWARD_S2_CONTROLLER_IMAGE STEWARD_TASK_IMAGE STEWARD_S5_MCP_GW_IMAGE; do
  if [[ -z "${!variable:-}" ]]; then
    echo "${variable} is required for the task lifecycle image callback" >&2
    exit 2
  fi
done

TASK_IMAGE="steward/task:${STEWARD_RUN_ID}"
if [[ "${STEWARD_S2_CONTROLLER_IMAGE}" != "${TASK_IMAGE}" || "${STEWARD_TASK_IMAGE}" != "${TASK_IMAGE}" ]]; then
  echo "task lifecycle images must be the run-scoped ${TASK_IMAGE}" >&2
  exit 2
fi
if [[ "${STEWARD_S5_MCP_GW_IMAGE}" != "${MCP_GW_IMAGE}" ]]; then
  echo "task lifecycle mcp-gw image must be ${MCP_GW_IMAGE}" >&2
  exit 2
fi

docker build \
  --file "${ROOT}/e2e/Dockerfile.task" \
  --label "steward.test/run-id=${STEWARD_RUN_ID}" \
  --tag "${TASK_IMAGE}" \
  "${ROOT}"
"${ROOT}/scripts/build-steward-mint-image.sh"
"${ROOT}/scripts/build-patched-mcp-gw.sh"
docker run --rm \
  --entrypoint bun \
  --volume "${ROOT}/config/s5:/workspace/config/s5:ro" \
  --workdir /workspace \
  "${MCP_GW_IMAGE}" \
  test config/s5/capture-proxy.test.ts

for image in "${MINT_IMAGE}" "${MCP_GW_IMAGE}" "${TASK_IMAGE}"; do
  docker image inspect "${image}" >/dev/null
done

exec bash "${ROOT}/scripts/s2-inference-inside.sh"
