#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
run_id="task-$(date -u +%Y%m%d%H%M%S)-$$"
image="steward/task:${run_id}"
workflow_sandbox_image="steward/workflow-sandbox:${run_id}"
mcp_gw_image="steward/mcp-gw:c2af10d9-claims"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker image rm "${image}" >/dev/null 2>&1 || true
  docker image rm "${workflow_sandbox_image}" >/dev/null 2>&1 || true
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

if [[ "${STEWARD_USE_CHART_SUPERVISOR:-0}" != "1" \
  && -z "${STEWARD_OPENSHELL_SUPERVISOR_IMAGE:-}" ]]
then
  if ! "${root}/scripts/build-patched-openshell-supervisor.sh" --image-is-current; then
    "${root}/scripts/build-patched-openshell-supervisor.sh"
  fi
fi

docker build \
  --file "${root}/e2e/Dockerfile.workflow-sandbox" \
  --label "steward.test/run-id=${run_id}" \
  --tag "${workflow_sandbox_image}" \
  "${root}"

STEWARD_E2E_SLICE=task \
STEWARD_RUN_ID="${run_id}" \
STEWARD_S2_CONTROLLER_IMAGE="${image}" \
STEWARD_S5_MCP_GW_IMAGE="${mcp_gw_image}" \
STEWARD_TASK_IMAGE="${image}" \
STEWARD_OPENSHELL_SANDBOX_IMAGE="${workflow_sandbox_image}" \
  bash "${root}/scripts/s0-0-openshell-spike.sh" \
  bash "${root}/scripts/task-submission-inside.sh"
