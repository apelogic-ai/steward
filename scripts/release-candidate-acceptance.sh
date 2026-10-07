#!/usr/bin/env bash
# Build no artifacts: publish the release-validation images and source chart to
# a disposable local registry, then run the same clean-cluster acceptance used
# after immutable release publication.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release_version="${STEWARD_RELEASE_VERSION:-$(awk '$1 == "version:" { print $2; exit }' "${root}/charts/steward/Chart.yaml")}"
run_id="candidate-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}-$$"
registry_name="steward-release-${run_id}"
registry_image='registry:2.8.3@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373'
temp_root="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
run_dir="$(mktemp -d "${temp_root%/}/steward-${run_id}.XXXXXX")"
candidate_tags=()

cleanup() {
  status="$?"
  trap - EXIT INT TERM
  set +e
  docker rm --force "${registry_name}" >/dev/null 2>&1 || true
  if [[ "${#candidate_tags[@]}" -gt 0 ]]; then
    docker image rm "${candidate_tags[@]}" >/dev/null 2>&1 || true
  fi
  find "${run_dir}" -depth -delete 2>/dev/null || status=1
  if docker inspect "${registry_name}" >/dev/null 2>&1 || [[ -e "${run_dir}" ]]; then
    echo "release-candidate cleanup left ${registry_name} or ${run_dir}" >&2
    status=1
  fi
  exit "${status}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for tool in awk docker helm; do
  command -v "${tool}" >/dev/null || { echo "missing ${tool}" >&2; exit 2; }
done
test "${release_version}" = "$(awk '$1 == "appVersion:" { print $2; exit }' "${root}/charts/steward/Chart.yaml")"
for image in \
  steward-apiserver:release-validation \
  steward-controller:release-validation \
  steward-mint:release-validation \
  steward-bridge:release-validation \
  steward-web:release-validation
do
  docker image inspect "${image}" >/dev/null
done

docker run --detach --publish 127.0.0.1::5000 \
  --name "${registry_name}" \
  --label "steward.test/run-id=${run_id}" \
  "${registry_image}" >/dev/null
registry_address="$(docker port "${registry_name}" 5000/tcp | head -n 1)"
if [[ ! "${registry_address}" =~ ^127\.0\.0\.1:[0-9]+$ ]]; then
  echo "disposable registry has unexpected address ${registry_address}" >&2
  exit 1
fi
pull_repository="${registry_address}/steward"
runtime_repository="${registry_name}:5000/steward"

for component in apiserver controller mint bridge web; do
  case "${component}" in
    apiserver|controller|mint) source_image="steward-${component}:release-validation" ;;
    bridge) source_image=steward-bridge:release-validation ;;
    web) source_image=steward-web:release-validation ;;
  esac
  candidate_tag="${pull_repository}:${release_version}-${component}"
  candidate_tags+=("${candidate_tag}")
  docker tag "${source_image}" "${candidate_tag}"
  docker push "${candidate_tag}" >/dev/null
  digest="$(docker buildx imagetools inspect "${candidate_tag}" --format '{{.Manifest.Digest}}')"
  case "${component}" in
    apiserver) api_digest="${digest}" ;;
    controller) controller_digest="${digest}" ;;
    mint) mint_digest="${digest}" ;;
    bridge) bridge_digest="${digest}" ;;
    web) web_digest="${digest}" ;;
  esac
done

helm package "${root}/charts/steward" --destination "${run_dir}" \
  --version "${release_version}" --app-version "${release_version}" >/dev/null
chart_archive="${run_dir}/steward-${release_version}.tgz"
chart_push="$(helm push "${chart_archive}" "oci://${registry_address}/charts" --plain-http 2>&1)"
chart_digest="$(printf '%s\n' "${chart_push}" | awk '$1 == "Digest:" { print $2 }')"
if [[ ! "${chart_digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "local chart publication returned invalid digest ${chart_digest}" >&2
  exit 1
fi

export STEWARD_RELEASE_VERSION="${release_version}"
export STEWARD_RELEASE_IMAGE_REPOSITORY="${runtime_repository}"
export STEWARD_RELEASE_IMAGE_PULL_REPOSITORY="${pull_repository}"
export STEWARD_RELEASE_CHART_REPOSITORY="oci://${registry_address}/charts/steward"
export STEWARD_RELEASE_REGISTRY_MIRROR="${registry_name}:5000"
export STEWARD_RELEASE_PLAIN_HTTP=true
export STEWARD_RELEASE_CHART_DIGEST="${chart_digest}"
export STEWARD_RELEASE_APISERVER_DIGEST="${api_digest}"
export STEWARD_RELEASE_CONTROLLER_DIGEST="${controller_digest}"
export STEWARD_RELEASE_MINT_DIGEST="${mint_digest}"
export STEWARD_RELEASE_BRIDGE_DIGEST="${bridge_digest}"
export STEWARD_RELEASE_WEB_DIGEST="${web_digest}"
bash "${root}/scripts/customer-core-install-e2e.sh"
