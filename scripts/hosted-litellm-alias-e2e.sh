#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LITELLM_IMAGE="ghcr.io/berriai/litellm-database:v1.93.0@sha256:72360d8bd5602faa49be5098a8ac3dd069d9fb74503d6bd014242d96dc753e43"
POSTGRES_IMAGE="postgres:16-alpine@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777"
RUN_ID="${STEWARD_RUN_ID:-litellm-alias-$(date -u +%Y%m%d%H%M%S)-$$}"
if [[ ! "${RUN_ID}" =~ ^[a-z0-9-]+$ ]]; then
  echo "STEWARD_RUN_ID must contain only lowercase ASCII letters, digits, and hyphens" >&2
  exit 2
fi

RUN_DIR="${ROOT}/.steward-run/${RUN_ID}"
NETWORK="steward-${RUN_ID}"
POSTGRES="${NETWORK}-postgres"
PROXY="${NETWORK}-proxy"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker rm -f "${PROXY}" "${POSTGRES}" >/dev/null 2>&1 || status=1
  docker network rm "${NETWORK}" >/dev/null 2>&1 || status=1
  find "${RUN_DIR}" -depth -delete 2>/dev/null || status=1
  if docker inspect "${PROXY}" "${POSTGRES}" >/dev/null 2>&1; then
    echo "hosted LiteLLM test containers remain after cleanup" >&2
    status=1
  fi
  if docker network inspect "${NETWORK}" >/dev/null 2>&1; then
    echo "hosted LiteLLM test network remains after cleanup" >&2
    status=1
  fi
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in cargo curl docker openssl sed; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done
docker info >/dev/null

mkdir -p "${RUN_DIR}"
chmod 700 "${RUN_DIR}"
management_key="$(openssl rand -hex 32)"
cat >"${RUN_DIR}/config.yaml" <<'YAML'
model_list:
  - model_name: claude-sonnet
    litellm_params:
      model: anthropic/claude-3-5-sonnet-20241022
      mock_response: allowed Claude fixture response
    model_info:
      input_cost_per_token: 1.0
      output_cost_per_token: 1.0
  - model_name: codex-mini
    litellm_params:
      model: openai/gpt-4o-mini
      mock_response: allowed Codex fixture response
    model_info:
      input_cost_per_token: 1.0
      output_cost_per_token: 1.0
general_settings:
  disable_spend_logs: false
YAML

docker network create \
  --label "steward.test/run-id=${RUN_ID}" \
  "${NETWORK}" >/dev/null
docker run --rm -d \
  --name "${POSTGRES}" \
  --network "${NETWORK}" \
  --label "steward.test/run-id=${RUN_ID}" \
  -e POSTGRES_DB=litellm \
  -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e POSTGRES_USER=litellm \
  "${POSTGRES_IMAGE}" >/dev/null

for attempt in {1..60}; do
  if docker exec "${POSTGRES}" pg_isready -U litellm -d litellm >/dev/null 2>&1; then
    break
  fi
  if [[ "${attempt}" == 60 ]]; then
    echo "hosted LiteLLM test PostgreSQL did not become ready" >&2
    exit 1
  fi
  sleep 1
done

docker run --rm -d \
  --name "${PROXY}" \
  --network "${NETWORK}" \
  --label "steward.test/run-id=${RUN_ID}" \
  -p 127.0.0.1::4000 \
  -e "DATABASE_URL=postgresql://litellm@${POSTGRES}:5432/litellm" \
  -e "LITELLM_MASTER_KEY=${management_key}" \
  --mount "type=bind,source=${RUN_DIR}/config.yaml,target=/config.yaml,readonly" \
  "${LITELLM_IMAGE}" \
  --config /config.yaml --port 4000 >/dev/null

port="$(docker port "${PROXY}" 4000/tcp | sed -nE 's/.*:([0-9]+)$/\1/p')"
if [[ -z "${port}" ]]; then
  echo "hosted LiteLLM test did not publish its local port" >&2
  exit 1
fi
base_url="http://127.0.0.1:${port}"
for attempt in {1..120}; do
  if curl -fsS "${base_url}/health/liveliness" >/dev/null 2>&1; then
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "${PROXY}" 2>/dev/null || true)" != "true" ]]; then
    docker logs "${PROXY}" >&2
    echo "hosted LiteLLM test proxy exited before becoming ready" >&2
    exit 1
  fi
  if [[ "${attempt}" == 120 ]]; then
    docker logs "${PROXY}" >&2
    echo "hosted LiteLLM test proxy did not become ready" >&2
    exit 1
  fi
  sleep 1
done

STEWARD_TEST_LITELLM_URL="${base_url}" \
STEWARD_TEST_LITELLM_MANAGEMENT_KEY="${management_key}" \
cargo test \
  --manifest-path "${ROOT}/e2e/Cargo.toml" \
  --test hosted_litellm_aliases \
  -- \
  --nocapture \
  --test-threads=1

echo "real hosted LiteLLM launcher alias evidence passed"
