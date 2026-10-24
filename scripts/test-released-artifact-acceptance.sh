#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="${root}/.github/workflows/release.yml"
ci_workflow="${root}/.github/workflows/ci.yml"
release_validator="${root}/scripts/validate-release-artifacts.sh"
harness="${root}/scripts/customer-core-install-e2e.sh"
candidate_harness="${root}/scripts/release-candidate-acceptance.sh"
bundle_harness="${root}/scripts/steward-bundle-kind-smoke.sh"
scratch="$(mktemp -d)"
trap 'find "${scratch}" -depth -delete 2>/dev/null || true' EXIT

for required in \
  'released-artifact-acceptance:' \
  'needs: [validate, publish-images]' \
  'needs: [publish-images, publish-chart]' \
  'needs.released-artifact-acceptance.result' \
  'bash scripts/customer-core-install-e2e.sh' \
  'chart: [steward, steward-bundle, spire, agent-sandbox, openshell, litellm, arc]' \
  'scripts/prepare-governed-dependency-charts.sh' \
  'scripts/prepare-steward-bundle-chart.sh' \
  'Download released image digests for bundle' \
  "name: release-image-\${{ matrix.component }}" \
  'pattern: release-image-*' \
  'STEWARD_BUNDLE_RELEASE_APISERVER_DIGEST' \
  'Install released bundle in a clean cluster' \
  'STEWARD_RELEASE_BUNDLE_CHART_REPOSITORY' \
  'scripts/pull-chart-by-digest.sh' \
  "scripts/steward-bundle-kind-smoke.sh \"\${bundle_archive}\"" \
  "dist/helm-chart-\$dependency_chart.digest" \
  'dist/helm-chart-steward-bundle.digest' \
  'bundleChart: {' \
  'dependencyCharts: {' \
  '"agent-sandbox": {reference:' \
  'cp "dist/helm-chart-steward.spdx.json" dist/helm-chart.spdx.json' \
  'cp "dist/helm-chart-steward.vulnerabilities.json" dist/helm-chart.vulnerabilities.json'
do
  grep -Fq "${required}" "${workflow}" || {
    echo "release workflow omitted ${required}" >&2
    exit 1
  }
done

for required in \
  "docker pull \"\${image_pull_repository}@\${digest}\"" \
  "helm_pull=(pull \"\${chart_repository}@\${chart_digest}\"" \
  'kind create cluster' \
  '--set-string spire.className=spire-spire' \
  'images.triggerReceiver.tag' \
  "\${release_version}-trigger-receiver@\${trigger_receiver_digest}" \
  "upgrade --install steward \"\${chart_archive}\"" \
  '_sqlx_migrations' \
  'complete-rendered.yaml'
do
  grep -Fq -- "${required}" "${harness}" || {
    echo "released-artifact acceptance omitted ${required}" >&2
    exit 1
  }
done

if grep -Eq 'docker build[[:space:]]|docker push|registry:2|charts/steward.*upgrade --install' "${harness}"; then
  echo 'released-artifact acceptance must not build or install local artifacts' >&2
  exit 1
fi

if grep -Fq '/private/tmp' "${harness}"; then
  echo 'released-artifact acceptance must use the portable runner temporary directory' >&2
  exit 1
fi

for required in \
  'STEWARD_RELEASE_IMAGE_PULL_REPOSITORY' \
  'STEWARD_RELEASE_REGISTRY_MIRROR' \
  'STEWARD_RELEASE_PLAIN_HTTP' \
  'STEWARD_RELEASE_TRIGGER_RECEIVER_DIGEST' \
  "docker network disconnect kind \"\${registry_mirror%%:*}\""
do
  grep -Fq "${required}" "${harness}" || {
    echo "released-artifact acceptance omitted candidate-registry input ${required}" >&2
    exit 1
  }
done

disconnect_line="$(grep -nF "docker network disconnect kind \"\${registry_mirror%%:*}\"" \
  "${harness}" | cut -d: -f1)"
delete_line="$(grep -nF "kind delete cluster --name \"\${cluster}\"" \
  "${harness}" | cut -d: -f1)"
if [[ -z "${disconnect_line}" || -z "${delete_line}" ]] \
  || ((disconnect_line >= delete_line)); then
  echo 'released-artifact acceptance must disconnect the candidate registry before Kind teardown' >&2
  exit 1
fi

core_cleanup_function="$(awk '
  /^cleanup\(\) \{/ { capture = 1 }
  capture { print }
  capture && /^}$/ { exit }
' "${harness}")"
bundle_cleanup_function="$(awk '
  /^cleanup\(\) \{/ { capture = 1 }
  capture { print }
  capture && /^}$/ { exit }
' "${bundle_harness}")"
mkdir "${scratch}/bin"
cat >"${scratch}/bin/kind" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "$*" in
  'delete cluster --name steward-test')
    if [[ "${KUBECONFIG:-}" != "${FAKE_KIND_EXPECTED_KUBECONFIG:-}" ]]; then
      exit 97
    fi
    printf '%s\n' "${FAKE_KIND_DELETE_MESSAGE:-fake Kind delete diagnostic}"
    exit "${FAKE_KIND_DELETE_STATUS:-0}"
    ;;
  'get clusters')
    if [[ "${FAKE_KIND_LIST_STATUS:-0}" != 0 ]]; then
      exit "${FAKE_KIND_LIST_STATUS}"
    fi
    printf '%s\n' "${FAKE_KIND_CLUSTERS:-}"
    ;;
  *) exit 98 ;;
esac
EOF
chmod 0755 "${scratch}/bin/kind"

run_cleanup_case() (
  local cleanup_function="$1"
  local created="${2:-1}" initial_status="${3:-0}"
  eval "${cleanup_function}"
  local -x PATH="${scratch}/bin:${PATH}"
  cluster=steward-test
  run_dir="$(mktemp -d "${scratch}/cleanup.XXXXXX")"
  kubeconfig="${run_dir}/kubeconfig"
  cluster_created="${created}"
  port_forward_pid=''
  registry_mirror=''
  if [[ "${created}" == 0 ]]; then
    stage=preflight
  else
    stage=complete
  fi
  context=unused
  FAKE_KIND_EXPECTED_KUBECONFIG="${kubeconfig}"
  export cluster run_dir cluster_created port_forward_pid registry_mirror stage kubeconfig context
  export FAKE_KIND_EXPECTED_KUBECONFIG
  set +e
  (exit "${initial_status}")
  cleanup
)

assert_cleanup_contract() {
  local name="$1" cleanup_function="$2"
  local stderr="${scratch}/${name// /-}-delete.stderr"
  if ! FAKE_KIND_DELETE_STATUS=1 FAKE_KIND_DELETE_MESSAGE='visible delete diagnostic' \
    run_cleanup_case "${cleanup_function}" >/dev/null 2>"${stderr}"; then
    echo "${name} rejected verified cleanup after Kind removed the cluster" >&2
    exit 1
  fi
  grep -Fq 'visible delete diagnostic' "${stderr}" || {
    echo "${name} hid the Kind delete diagnostic" >&2
    exit 1
  }
  if FAKE_KIND_DELETE_STATUS=1 FAKE_KIND_LIST_STATUS=1 \
    run_cleanup_case "${cleanup_function}"; then
    echo "${name} passed without verifying that the owned cluster was absent" >&2
    exit 1
  fi
  if FAKE_KIND_CLUSTERS=steward-test run_cleanup_case "${cleanup_function}"; then
    echo "${name} passed while the owned cluster remained" >&2
    exit 1
  fi
  local preflight_status=0
  run_cleanup_case "${cleanup_function}" 0 2 || preflight_status="$?"
  if [[ "${preflight_status}" != 2 ]]; then
    echo "${name} changed the early preflight exit status to ${preflight_status}" >&2
    exit 1
  fi
}

assert_cleanup_contract 'released-artifact acceptance' "${core_cleanup_function}"
assert_cleanup_contract 'bundle acceptance' "${bundle_cleanup_function}"

for cleanup_harness in "${harness}" "${bundle_harness}"; do
  created_line="$(grep -nF 'cluster_created=1' "${cleanup_harness}" | cut -d: -f1)"
  create_line="$(grep -nF 'kind create cluster' "${cleanup_harness}" | cut -d: -f1)"
  if [[ -z "${created_line}" || -z "${create_line}" ]] \
    || ((created_line >= create_line)); then
    echo "${cleanup_harness} must record cluster ownership before Kind creation" >&2
    exit 1
  fi
done

missing_receiver_status=0
missing_receiver_output="$({
  STEWARD_RELEASE_VERSION=0.3.24 \
    STEWARD_RELEASE_IMAGE_REPOSITORY=registry.example.test/steward \
    STEWARD_RELEASE_CHART_REPOSITORY=oci://registry.example.test/charts/steward \
    STEWARD_RELEASE_CHART_DIGEST=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
    STEWARD_RELEASE_APISERVER_DIGEST=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
    STEWARD_RELEASE_CONTROLLER_DIGEST=sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc \
    STEWARD_RELEASE_MINT_DIGEST=sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd \
    bash "${harness}" 2>&1
})" || missing_receiver_status="$?"
if [[ "${missing_receiver_status}" == 0 ]] \
  || ! grep -Fq 'STEWARD_RELEASE_TRIGGER_RECEIVER_DIGEST is required' <<<"${missing_receiver_output}"
then
  echo 'released-artifact acceptance must refuse a missing trigger receiver digest' >&2
  exit 1
fi

for required in \
  'registry:2.8.3@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373' \
  'steward-trigger-receiver:release-validation' \
  'docker push' \
  'scripts/publish-chart.sh' \
  'scripts/pull-chart-by-digest.sh' \
  'scripts/prepare-governed-dependency-charts.sh' \
  'scripts/prepare-steward-bundle-chart.sh' \
  "STEWARD_BUNDLE_RELEASE_IMAGE_REPOSITORY=\"\${runtime_repository}\"" \
  'for dependency_chart in spire agent-sandbox openshell litellm arc' \
  "helm show values \"\${bundle_archive}\"" \
  'candidate bundle values omitted the' \
  'candidate bundle render omitted active Steward release images' \
  'helm template candidate-bundle' \
  "bash \"\${root}/scripts/steward-bundle-kind-smoke.sh\" \"\${bundle_archive}\"" \
  '--plain-http' \
  "bash \"\${root}/scripts/customer-core-install-e2e.sh\""
do
  grep -Fq -- "${required}" "${candidate_harness}" || {
    echo "release-candidate acceptance omitted ${required}" >&2
    exit 1
  }
done

for required in \
  'bash scripts/customer-core-install-e2e.sh --lint-source-chart charts/steward' \
  'bash scripts/release-candidate-acceptance.sh' \
  'Enforce critical trigger receiver image policy' \
  'Enforce critical web image policy' \
  'Enforce critical chart policy'
do
  grep -Fq "${required}" "${ci_workflow}" || {
    echo "pull-request release dry-run omitted ${required}" >&2
    exit 1
  }
done

grep -Fq "bash \"\${root}/scripts/test-steward-bundle-chart.sh\"" \
  "${release_validator}" || {
  echo 'release validation omitted the steward-bundle chart contract' >&2
  exit 1
}

echo 'released-artifact acceptance contract passed'
