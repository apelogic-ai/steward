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
  steward-trigger-receiver:release-validation \
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

for component in apiserver controller mint trigger-receiver bridge web; do
  case "${component}" in
    apiserver|controller|mint|trigger-receiver) source_image="steward-${component}:release-validation" ;;
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
    trigger-receiver) trigger_receiver_digest="${digest}" ;;
    bridge) bridge_digest="${digest}" ;;
    web) web_digest="${digest}" ;;
  esac
done

chart_digest="$({
  cd "${root}"
  bash scripts/publish-chart.sh steward "${release_version}" \
    "oci://${registry_address}/charts" "${run_dir}" --plain-http
})"

governed_charts="${run_dir}/charts"
bash "${root}/scripts/prepare-governed-dependency-charts.sh" "${governed_charts}"
for dependency_chart in spire agent-sandbox openshell litellm arc; do
  (
    cd "${run_dir}"
    bash "${root}/scripts/publish-chart.sh" "${dependency_chart}" \
      "${release_version}" "oci://${registry_address}/charts" "${run_dir}" \
      --plain-http >/dev/null
  )
done

bundle_chart="${run_dir}/charts/steward-bundle"
STEWARD_BUNDLE_RELEASE_VERSION="${release_version}" \
  STEWARD_BUNDLE_RELEASE_IMAGE_REPOSITORY="${runtime_repository}" \
  STEWARD_BUNDLE_RELEASE_APISERVER_DIGEST="${api_digest}" \
  STEWARD_BUNDLE_RELEASE_CONTROLLER_DIGEST="${controller_digest}" \
  STEWARD_BUNDLE_RELEASE_MINT_DIGEST="${mint_digest}" \
  STEWARD_BUNDLE_RELEASE_WEB_DIGEST="${web_digest}" \
  STEWARD_BUNDLE_RELEASE_TRIGGER_RECEIVER_DIGEST="${trigger_receiver_digest}" \
  bash "${root}/scripts/prepare-steward-bundle-chart.sh" "${bundle_chart}"
bundle_archive="${run_dir}/steward-bundle-${release_version}.tgz"
(
  cd "${run_dir}"
  bash "${root}/scripts/publish-chart.sh" steward-bundle "${release_version}" \
    "oci://${registry_address}/charts" "${run_dir}" --plain-http >/dev/null
)
bundle_values="${run_dir}/steward-bundle-values.yaml"
helm show values "${bundle_archive}" >"${bundle_values}"
if [[ "$(awk '
  $1 == "repository:" && $0 ~ /^    / { print $2; exit }
' "${bundle_values}")" != "${runtime_repository}" ]]; then
  echo 'candidate bundle values omitted the release image repository' >&2
  exit 1
fi
for component_digest in \
  "apiserver apiserver ${api_digest}" \
  "controller controller ${controller_digest}" \
  "mint mint ${mint_digest}" \
  "web web ${web_digest}" \
  "triggerReceiver trigger-receiver ${trigger_receiver_digest}"; do
  read -r component suffix digest <<<"${component_digest}"
  values="$(awk -v component="${component}" '
    $1 == component ":" && $0 ~ /^    / { active = 1; next }
    active && $1 == "tag:" { tag = $2 }
    active && $1 == "digest:" { print tag, $2; exit }
  ' "${bundle_values}")"
  if [[ "${values}" != "${release_version}-${suffix} ${digest}" ]]; then
    echo "candidate bundle values omitted the ${suffix} release image" >&2
    exit 1
  fi
done
bundle_render="${run_dir}/steward-bundle-rendered.yaml"
helm template candidate-bundle "${bundle_archive}" --namespace steward-test \
  >"${bundle_render}"
if ! grep -Fq "${runtime_repository}:${release_version}-" "${bundle_render}" \
  || grep -Fq 'ghcr.io/apelogic-ai/steward:' "${bundle_render}"; then
  echo 'candidate bundle render omitted active Steward release images' >&2
  exit 1
fi
bash "${root}/scripts/steward-bundle-kind-smoke.sh" "${bundle_archive}"

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
export STEWARD_RELEASE_TRIGGER_RECEIVER_DIGEST="${trigger_receiver_digest}"
export STEWARD_RELEASE_BRIDGE_DIGEST="${bridge_digest}"
export STEWARD_RELEASE_WEB_DIGEST="${web_digest}"
bash "${root}/scripts/customer-core-install-e2e.sh"
