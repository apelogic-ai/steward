#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="${STEWARD_RUN_ID:-governed-connections-$(date -u +%Y%m%d%H%M%S)-$$}"
if [[ ! "${RUN_ID}" =~ ^[a-z0-9-]+$ ]]; then
  echo "STEWARD_RUN_ID must contain only lowercase ASCII letters, digits, and hyphens" >&2
  exit 2
fi
for command in bash docker; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done
docker info >/dev/null

MCP_GW_LOCAL_IMAGE="steward/mcp-gw-github-wrapper:${RUN_ID}"
MINT_IMAGE="steward/mint:${RUN_ID}"
BRIDGE_IMAGE="steward/connections-bridge:${RUN_ID}"
SANDBOX_IMAGE="steward/workflow-sandbox:${RUN_ID}"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker image rm "${MCP_GW_LOCAL_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${MINT_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${BRIDGE_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${SANDBOX_IMAGE}" >/dev/null 2>&1 || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

case "$(docker info --format '{{.Architecture}}')" in
  aarch64 | arm64)
    MCP_GW_RELEASE_IMAGE="ghcr.io/apelogic-ai/mcp-gw-github-wrapper@sha256:f2ad2353a0445b8a89d7da2028a7a42f52bc538f9e63d66c07d242834553d96f"
    ;;
  x86_64 | amd64)
    MCP_GW_RELEASE_IMAGE="ghcr.io/apelogic-ai/mcp-gw-github-wrapper@sha256:6e7da4111d3e46aeac82eaad7a022e18c8cfb007f6024194049f0fe24b54e341"
    ;;
  *)
    echo "unsupported Docker architecture" >&2
    exit 2
    ;;
esac

docker pull "${MCP_GW_RELEASE_IMAGE}"
docker tag "${MCP_GW_RELEASE_IMAGE}" "${MCP_GW_LOCAL_IMAGE}"
docker build \
  --label "steward.test/run-id=${RUN_ID}" \
  --file "${ROOT}/config/s1/steward-mint.Dockerfile" \
  --tag "${MINT_IMAGE}" \
  "${ROOT}"
docker build \
  --label "steward.test/run-id=${RUN_ID}" \
  --file "${ROOT}/build/connections-bridge.Dockerfile" \
  --tag "${BRIDGE_IMAGE}" \
  "${ROOT}"
docker build \
  --label "steward.test/run-id=${RUN_ID}" \
  --file "${ROOT}/e2e/Dockerfile.workflow-sandbox" \
  --tag "${SANDBOX_IMAGE}" \
  "${ROOT}"

STEWARD_RUN_ID="${RUN_ID}" \
STEWARD_OPEN_SHELL_RELEASE=v0.0.98 \
STEWARD_CONNECTIONS_TEST_MCP_GW_IMAGE="${MCP_GW_LOCAL_IMAGE}" \
STEWARD_CONNECTIONS_TEST_MINT_IMAGE="${MINT_IMAGE}" \
STEWARD_CONNECTIONS_TEST_BRIDGE_IMAGE="${BRIDGE_IMAGE}" \
STEWARD_OPENSHELL_SANDBOX_IMAGE="${SANDBOX_IMAGE}" \
bash "${ROOT}/scripts/s0-0-openshell-spike.sh" \
  bash "${ROOT}/scripts/governed-connections-inside.sh"
