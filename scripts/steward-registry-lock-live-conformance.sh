#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
registry_image="docker.io/library/registry@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373"
source_reference="docker.io/library/registry:2"
source_digest="sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373"
run_id="${GITHUB_RUN_ID:-local}-$(printf '%s' "${GITHUB_RUN_ATTEMPT:-1}-$$" | tr -cd 'a-zA-Z0-9_.-')"
container_name="steward-registry-conformance-${run_id}"
temporary_directory="$(mktemp -d)"

cleanup() {
  status="$?"
  trap - EXIT INT TERM
  docker rm -f "$container_name" >/dev/null 2>&1 || true
  rm -rf "$temporary_directory"
  exit "$status"
}
trap cleanup EXIT INT TERM

docker run --detach --name "$container_name" --publish 127.0.0.1::5000 "$registry_image" >/dev/null
registry_port="$(docker port "$container_name" 5000/tcp | sed 's/.*://')"
if [[ ! "$registry_port" =~ ^[0-9]+$ ]]; then
  echo "ephemeral registry did not publish a port" >&2
  exit 1
fi

jq -n \
  --arg reference "$source_reference" \
  --arg digest "$source_digest" \
  '{
    schemaVersion: "steward.release-handoff/v1",
    version: "0.2.2",
    commit: "0123456789abcdef0123456789abcdef01234567",
    images: {apiserver: {reference: $reference, digest: $digest}}
  }' > "$temporary_directory/handoff.json"

python3 "$root/scripts/steward-registry-lock.py" mirror \
  --handoff "$temporary_directory/handoff.json" \
  --target-prefix "localhost:${registry_port}/steward-conformance" \
  --output "$temporary_directory/deployment-lock.json"

target_reference="$(jq -r '.artifacts["images.apiserver"].target.reference' "$temporary_directory/deployment-lock.json")"
target_digest="$(jq -r '.artifacts["images.apiserver"].target.digest' "$temporary_directory/deployment-lock.json")"
jq --exit-status '
  .mode == "index" and
  .artifacts["images.apiserver"].copyMode == "index" and
  ([.artifacts["images.apiserver"].platforms[] | select(.os == "linux" and .architecture == "amd64")] | length == 1) and
  ([.artifacts["images.apiserver"].platforms[] | select(.os == "linux" and .architecture == "arm64")] | length == 1)
' "$temporary_directory/deployment-lock.json" >/dev/null
docker buildx imagetools inspect "${target_reference%:*}@${target_digest}" >/dev/null
docker pull --platform linux/amd64 "${target_reference%:*}@${target_digest}" >/dev/null
docker image rm "${target_reference%:*}@${target_digest}" >/dev/null 2>&1 || true

echo "live multi-platform registry mirror conformance passed"
