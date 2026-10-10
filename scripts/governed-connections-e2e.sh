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
setup_started="${SECONDS}"

MCP_GW_LOCAL_IMAGE="steward/mcp-gw-github-wrapper:${RUN_ID}"
AGENTGATEWAY_LOCAL_IMAGE="steward/mcp-gw-agentgateway:${RUN_ID}"
MINT_IMAGE="steward/mint:${RUN_ID}"
BRIDGE_IMAGE="steward/connections-bridge:${RUN_ID}"
WEBHOOK_IMAGE="steward/connections-webhook:${RUN_ID}"
SANDBOX_IMAGE="steward/workflow-sandbox:${RUN_ID}"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker image rm "${MCP_GW_LOCAL_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${AGENTGATEWAY_LOCAL_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${MINT_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${BRIDGE_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${WEBHOOK_IMAGE}" >/dev/null 2>&1 || true
  docker image rm "${SANDBOX_IMAGE}" >/dev/null 2>&1 || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

MCP_GW_RELEASE_IMAGE="ghcr.io/apelogic-ai/mcp-gw-github-wrapper@sha256:9f1d76b7418caca120ab1651eb5335269127b1b3bccf4abaad79132d7a64cfe4"
AGENTGATEWAY_RELEASE_IMAGE="ghcr.io/apelogic-ai/mcp-gw-agentgateway@sha256:051e1c979b98561cfb833c8f44a55caac715d231c14bb0060371101fc9465c4a"

docker pull "${MCP_GW_RELEASE_IMAGE}"
docker tag "${MCP_GW_RELEASE_IMAGE}" "${MCP_GW_LOCAL_IMAGE}"
docker pull "${AGENTGATEWAY_RELEASE_IMAGE}"
docker tag "${AGENTGATEWAY_RELEASE_IMAGE}" "${AGENTGATEWAY_LOCAL_IMAGE}"
if [[ -n "${STEWARD_CONNECTIONS_PREBUILT_DIR:-}" ]]; then
  if [[ "${STEWARD_CONNECTIONS_PREBUILT_DIR}" != /* ]]; then
    echo "STEWARD_CONNECTIONS_PREBUILT_DIR must be an absolute path" >&2
    exit 2
  fi
  for artifact in \
    steward-mint-bin \
    steward-connections-bridge \
    governed-connections-webhook \
    governed_connections
  do
    if [[ ! -x "${STEWARD_CONNECTIONS_PREBUILT_DIR}/${artifact}" ]]; then
      echo "prebuilt governed Connections executable is missing: ${artifact}" >&2
      exit 2
    fi
  done
  for artifact in LICENSE THIRD_PARTY_NOTICES.md; do
    if [[ ! -f "${STEWARD_CONNECTIONS_PREBUILT_DIR}/${artifact}" ]]; then
      echo "prebuilt governed Connections artifact is missing: ${artifact}" >&2
      exit 2
    fi
  done
  for image_target in \
    "mint:${MINT_IMAGE}" \
    "bridge:${BRIDGE_IMAGE}" \
    "webhook:${WEBHOOK_IMAGE}"
  do
    target="${image_target%%:*}"
    image="${image_target#*:}"
    docker build \
      --label "steward.test/run-id=${RUN_ID}" \
      --target "${target}" \
      --file "${ROOT}/e2e/Dockerfile.governed-connections-prebuilt" \
      --tag "${image}" \
      "${STEWARD_CONNECTIONS_PREBUILT_DIR}"
  done
  export STEWARD_CONNECTIONS_TEST_BINARY="${STEWARD_CONNECTIONS_PREBUILT_DIR}/governed_connections"
else
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
    --file "${ROOT}/e2e/Dockerfile.governed-connections-webhook" \
    --tag "${WEBHOOK_IMAGE}" \
    "${ROOT}"
fi
docker build \
  --label "steward.test/run-id=${RUN_ID}" \
  --file "${ROOT}/e2e/Dockerfile.workflow-sandbox" \
  --tag "${SANDBOX_IMAGE}" \
  "${ROOT}"

printf 'governed-connections timing: setup-and-build-seconds=%s\n' \
  "$((SECONDS - setup_started))"

STEWARD_RUN_ID="${RUN_ID}" \
STEWARD_OPEN_SHELL_RELEASE=v0.0.98 \
STEWARD_CONNECTIONS_TEST_MCP_GW_IMAGE="${MCP_GW_LOCAL_IMAGE}" \
STEWARD_CONNECTIONS_TEST_AGENTGATEWAY_IMAGE="${AGENTGATEWAY_LOCAL_IMAGE}" \
STEWARD_CONNECTIONS_TEST_MINT_IMAGE="${MINT_IMAGE}" \
STEWARD_CONNECTIONS_TEST_BRIDGE_IMAGE="${BRIDGE_IMAGE}" \
STEWARD_CONNECTIONS_TEST_WEBHOOK_IMAGE="${WEBHOOK_IMAGE}" \
STEWARD_OPENSHELL_SANDBOX_IMAGE="${SANDBOX_IMAGE}" \
bash "${ROOT}/scripts/openshell-testbed.sh" \
  bash "${ROOT}/scripts/governed-connections-inside.sh"
