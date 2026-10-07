#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="${root}/.github/workflows/release.yml"
ci_workflow="${root}/.github/workflows/ci.yml"
harness="${root}/scripts/customer-core-install-e2e.sh"
candidate_harness="${root}/scripts/release-candidate-acceptance.sh"

for required in \
  'released-artifact-acceptance:' \
  'needs: [publish-images, publish-chart]' \
  'needs.released-artifact-acceptance.result' \
  'bash scripts/customer-core-install-e2e.sh'
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
  'STEWARD_RELEASE_PLAIN_HTTP'
do
  grep -Fq "${required}" "${harness}" || {
    echo "released-artifact acceptance omitted candidate-registry input ${required}" >&2
    exit 1
  }
done

for required in \
  'registry:2.8.3@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373' \
  'docker push' \
  'helm push' \
  '--plain-http 2>&1' \
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
  'Enforce critical web image policy' \
  'Enforce critical chart policy'
do
  grep -Fq "${required}" "${ci_workflow}" || {
    echo "pull-request release dry-run omitted ${required}" >&2
    exit 1
  }
done

echo 'released-artifact acceptance contract passed'
