#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POSTGRES_IMAGE="postgres:16-alpine@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777"
RUN_ID="postgres-tls-$(date -u +%Y%m%d%H%M%S)-$$"
CONTAINER="steward-${RUN_ID}"
TLS_STAGER="${CONTAINER}-tls-stage"
TLS_VOLUME="${CONTAINER}-tls"
RUN_DIR="${ROOT}/.steward-run/${RUN_ID}"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker rm -f "${TLS_STAGER}" >/dev/null 2>&1 || true
  docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
  docker volume rm -f "${TLS_VOLUME}" >/dev/null 2>&1 || true
  find "${RUN_DIR}" -depth -delete 2>/dev/null || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in cargo docker openssl sed; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done
docker info >/dev/null

mkdir -p "${RUN_DIR}"
openssl req \
  -x509 \
  -newkey rsa:2048 \
  -nodes \
  -days 1 \
  -subj "/CN=localhost" \
  -keyout "${RUN_DIR}/server.key" \
  -out "${RUN_DIR}/server.crt" >/dev/null 2>&1

docker volume create \
  --label "steward.test/run-id=${RUN_ID}" \
  "${TLS_VOLUME}" >/dev/null
docker run --rm \
  --name "${TLS_STAGER}" \
  --label "steward.test/run-id=${RUN_ID}" \
  --entrypoint sh \
  -v "${RUN_DIR}:/tls-source:ro" \
  -v "${TLS_VOLUME}:/tls-output" \
  "${POSTGRES_IMAGE}" \
  -c 'cp /tls-source/server.key /tls-output/server.key
cp /tls-source/server.crt /tls-output/server.crt
chown postgres:postgres /tls-output/server.key /tls-output/server.crt
chmod 600 /tls-output/server.key
chmod 644 /tls-output/server.crt'

docker run -d \
  --name "${CONTAINER}" \
  --label "steward.test/run-id=${RUN_ID}" \
  -e POSTGRES_DB=steward \
  -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e POSTGRES_USER=steward \
  -p 127.0.0.1::5432 \
  -v "${TLS_VOLUME}:/tls-input:ro" \
  -v "${ROOT}/scripts/postgres-tls-init.sh:/docker-entrypoint-initdb.d/001-require-tls.sh:ro" \
  "${POSTGRES_IMAGE}" >/dev/null

for attempt in {1..60}; do
  if docker exec "${CONTAINER}" pg_isready -h 127.0.0.1 -U steward -d steward >/dev/null 2>&1; then
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "${CONTAINER}" 2>/dev/null || true)" != "true" ]]; then
    docker logs "${CONTAINER}" >&2
    echo "TLS-required PostgreSQL exited before becoming ready" >&2
    exit 1
  fi
  if [[ "${attempt}" == 60 ]]; then
    docker logs "${CONTAINER}" >&2
    echo "TLS-required PostgreSQL did not become ready" >&2
    exit 1
  fi
  sleep 1
done

port="$(docker port "${CONTAINER}" 5432/tcp | sed -nE 's/.*:([0-9]+)$/\1/p')"
if [[ -z "${port}" ]]; then
  echo "TLS-required PostgreSQL did not publish a local port" >&2
  exit 1
fi

export STEWARD_TEST_PLAINTEXT_DATABASE_URL="postgres://steward@127.0.0.1:${port}/steward?sslmode=disable"
export STEWARD_TEST_TLS_DATABASE_URL="postgres://steward@127.0.0.1:${port}/steward?sslmode=require"
cargo test \
  --manifest-path "${ROOT}/e2e/Cargo.toml" \
  --test postgres_tls \
  -- \
  --nocapture
