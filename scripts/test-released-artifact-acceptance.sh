#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="${root}/.github/workflows/release.yml"
ci_workflow="${root}/.github/workflows/ci.yml"
release_validator="${root}/scripts/validate-release-artifacts.sh"
harness="${root}/scripts/customer-core-install-e2e.sh"
candidate_harness="${root}/scripts/release-candidate-acceptance.sh"

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
  'STEWARD_RELEASE_TRIGGER_RECEIVER_DIGEST'
do
  grep -Fq "${required}" "${harness}" || {
    echo "released-artifact acceptance omitted candidate-registry input ${required}" >&2
    exit 1
  }
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
