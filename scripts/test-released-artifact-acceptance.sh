#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="${root}/.github/workflows/release.yml"
harness="${root}/scripts/customer-core-install-e2e.sh"

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
  'docker pull "${image_repository}@${digest}"' \
  'helm pull "${chart_repository}@${chart_digest}"' \
  'kind create cluster' \
  'upgrade --install steward "${chart_archive}"' \
  '_sqlx_migrations' \
  'complete-rendered.yaml'
do
  grep -Fq "${required}" "${harness}" || {
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

echo 'released-artifact acceptance contract passed'
