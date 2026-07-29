#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${STEWARD_CONFORMANCE_TARGET:-}"
PINNED_IMAGE="ghcr.io/berriai/litellm-database:v1.93.0@sha256:72360d8bd5602faa49be5098a8ac3dd069d9fb74503d6bd014242d96dc753e43"
LATEST_IMAGE="ghcr.io/berriai/litellm-database:main-latest"
POSTGRES_IMAGE="postgres:16-alpine@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777"

case "${TARGET}" in
  pinned)
    litellm_image="${PINNED_IMAGE}"
    ;;
  latest)
    litellm_image="${LATEST_IMAGE}"
    ;;
  *)
    echo "STEWARD_CONFORMANCE_TARGET must be pinned or latest" >&2
    exit 2
    ;;
esac

for command in curl docker jq openssl; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

mkdir -p "${ROOT}/.steward-run"
RUN_DIR="$(mktemp -d "${ROOT}/.steward-run/g5-conformance.XXXXXX")"
suffix="${RUN_DIR##*.}"
network="steward-g5-${suffix}"
postgres="steward-g5-postgres-${suffix}"
proxy="steward-g5-proxy-${suffix}"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker rm -f "${proxy}" "${postgres}" >/dev/null 2>&1 || true
  docker network rm "${network}" >/dev/null 2>&1 || true
  find "${RUN_DIR}" -depth -delete 2>/dev/null || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

master_key="$(openssl rand -hex 32)"
cat >"${RUN_DIR}/config.yaml" <<'YAML'
model_list:
  - model_name: openai/model-a
    litellm_params:
      model: openai/model-a
      mock_response: allowed fixture response
    model_info:
      input_cost_per_token: 1.0
      output_cost_per_token: 1.0
  - model_name: openai/model-b
    litellm_params:
      model: openai/model-b
      mock_response: forbidden fixture response
    model_info:
      input_cost_per_token: 1.0
      output_cost_per_token: 1.0
YAML

docker network create \
  --label "steward.test/run-id=g5-${suffix}" \
  "${network}" >/dev/null
docker run --rm -d \
  --name "${postgres}" \
  --network "${network}" \
  --label "steward.test/run-id=g5-${suffix}" \
  -e POSTGRES_DB=litellm \
  -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e POSTGRES_USER=litellm \
  "${POSTGRES_IMAGE}" >/dev/null

for attempt in {1..60}; do
  if docker exec "${postgres}" pg_isready -U litellm -d litellm >/dev/null 2>&1; then
    break
  fi
  if [[ "${attempt}" == 60 ]]; then
    echo "G-5 Postgres did not become ready" >&2
    exit 1
  fi
  sleep 1
done

docker run --rm -d \
  --name "${proxy}" \
  --network "${network}" \
  --label "steward.test/run-id=g5-${suffix}" \
  -p 127.0.0.1::4000 \
  -e "DATABASE_URL=postgresql://litellm@${postgres}:5432/litellm" \
  -e "LITELLM_MASTER_KEY=${master_key}" \
  --mount "type=bind,source=${RUN_DIR}/config.yaml,target=/config.yaml,readonly" \
  "${litellm_image}" \
  --config /config.yaml --port 4000 >/dev/null

port="$(docker port "${proxy}" 4000/tcp | sed -nE 's/.*:([0-9]+)$/\1/p')"
if [[ -z "${port}" ]]; then
  echo "G-5 LiteLLM container did not publish its test port" >&2
  exit 1
fi
base_url="http://127.0.0.1:${port}"
for attempt in {1..120}; do
  if curl -fsS "${base_url}/health/liveliness" >/dev/null 2>&1; then
    break
  fi
  if ! docker inspect "${proxy}" >/dev/null 2>&1; then
    echo "G-5 LiteLLM container exited during startup" >&2
    exit 1
  fi
  if [[ "${attempt}" == 120 ]]; then
    echo "G-5 LiteLLM did not become ready" >&2
    exit 1
  fi
  sleep 1
done

runtime_key="$(
  curl -fsS \
    -H "Authorization: Bearer ${master_key}" \
    -H "Content-Type: application/json" \
    -d '{"key_alias":"runtime-a","models":["openai/model-a"]}' \
    "${base_url}/key/generate" |
    jq -er '.key | select(type == "string" and length > 0)'
)"
response="${RUN_DIR}/denied.json"
status="$(
  curl -sS \
    -o "${response}" \
    -w '%{http_code}' \
    -H "Authorization: Bearer ${runtime_key}" \
    -H "Content-Type: application/json" \
    -d '{"model":"openai/model-b","messages":[{"role":"user","content":"hello"}]}' \
    "${base_url}/v1/chat/completions"
)"
if [[ "${status}" != "403" ]]; then
  echo "G-5 violation returned HTTP ${status}, expected 403" >&2
  exit 1
fi
if ! jq -e '.. | strings | select(test("not.*allowed|allowed.*model"; "i"))' \
  "${response}" >/dev/null
then
  echo "G-5 denial did not identify the model allowlist" >&2
  exit 1
fi

echo "G-5 upstream result: 1 passed; 0 failed; 0 skipped"
echo "G-5 ${TARGET} upstream evidence passed against LiteLLM"
