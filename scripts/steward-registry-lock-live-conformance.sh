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
  docker rm -f "${container_name}" >/dev/null 2>&1 || true
  rm -rf "${temporary_directory}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

docker run --detach --name "${container_name}" --publish 127.0.0.1::5000 "${registry_image}" >/dev/null
registry_port="$(docker port "${container_name}" 5000/tcp | sed 's/.*://')"
if [[ ! "${registry_port}" =~ ^[0-9]+$ ]]; then
  echo "ephemeral registry did not publish a port" >&2
  exit 1
fi

target_repository="localhost:${registry_port}/steward-conformance/artifacts"
oras cp --recursive --no-tty --to-plain-http \
  "${source_reference}@${source_digest}" "${target_repository}:preflight-seed"

jq -n \
  --arg reference "${source_reference}" \
  --arg digest "${source_digest}" \
  '{
    schemaVersion: "steward.release-handoff/v1",
    version: "0.2.6",
    commit: "0123456789abcdef0123456789abcdef01234567",
    chart: {reference: ("oci://" + $reference), digest: $digest},
    images: {apiserver: {reference: $reference, digest: $digest}}
  }' >"${temporary_directory}/handoff.json"

jq -n \
  --arg chart "${target_repository}:steward-chart" \
  --arg image "${target_repository}:steward-apiserver" \
  '{
    schemaVersion: "steward.registry-mappings/v1",
    artifacts: {chart: $chart, "images.apiserver": $image}
  }' >"${temporary_directory}/mappings.json"

"${root}/scripts/steward-registry-lock.sh" plan \
  --handoff "${temporary_directory}/handoff.json" \
  --mappings "${temporary_directory}/mappings.json" \
  --target-plain-http \
  --output "${temporary_directory}/plan.json"
"${root}/scripts/steward-registry-lock.sh" mirror \
  --plan "${temporary_directory}/plan.json" \
  --output "${temporary_directory}/deployment-lock.json" \
  >"${temporary_directory}/mirror-result.jsonl"

jq --exit-status '
  .schemaVersion == "steward.deployment-lock/v1" and
  .artifacts.chart.source.digest == .artifacts.chart.target.digest and
  .artifacts["images.apiserver"].source.digest == .artifacts["images.apiserver"].target.digest and
  .artifacts.chart.flux.ociRepository.ref.digest == .artifacts.chart.target.digest
' "${temporary_directory}/deployment-lock.json" >/dev/null
jq -s --exit-status 'length == 2 and all(.status == "copied")' \
  "${temporary_directory}/mirror-result.jsonl" >/dev/null

for reference in \
  "${target_repository}:steward-chart" \
  "${target_repository}:steward-apiserver"; do
  observed="$(oras manifest fetch --descriptor --plain-http "${reference}" | jq -er '.digest')"
  [[ "${observed}" == "${source_digest}" ]] || {
    echo "target descriptor did not preserve the source digest" >&2
    exit 1
  }
done

echo "live daemonless image-and-chart registry mirror conformance passed"
