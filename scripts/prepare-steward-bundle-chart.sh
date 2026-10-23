#!/usr/bin/env bash
set -euo pipefail

if (($# != 1)); then
  echo "usage: $0 OUTPUT_CHART_DIRECTORY" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output="$1"
work="$(mktemp -d)"
trap 'find "${work}" -depth -delete 2>/dev/null || true' EXIT INT TERM

if [[ -e "${output}" ]]; then
  echo "output chart directory already exists: ${output}" >&2
  exit 2
fi
for tool in cp grep helm mkdir python3; do
  command -v "${tool}" >/dev/null || {
    echo "missing required bundle preparation tool: ${tool}" >&2
    exit 2
  }
done

release_version="${STEWARD_BUNDLE_RELEASE_VERSION:-}"
release_image_repository="${STEWARD_BUNDLE_RELEASE_IMAGE_REPOSITORY:-}"
release_digests=(
  "${STEWARD_BUNDLE_RELEASE_APISERVER_DIGEST:-}"
  "${STEWARD_BUNDLE_RELEASE_CONTROLLER_DIGEST:-}"
  "${STEWARD_BUNDLE_RELEASE_MINT_DIGEST:-}"
  "${STEWARD_BUNDLE_RELEASE_WEB_DIGEST:-}"
  "${STEWARD_BUNDLE_RELEASE_TRIGGER_RECEIVER_DIGEST:-}"
)
if [[ -n "${release_version}" || -n "${release_image_repository}" ]] \
  || printf '%s\n' "${release_digests[@]}" | grep -q .; then
  if [[ -z "${release_version}" ]] \
    || printf '%s\n' "${release_digests[@]}" | grep -qx ''; then
    echo 'complete Steward bundle release image evidence is required' >&2
    exit 2
  fi
fi

bash "${root}/scripts/prepare-governed-dependency-charts.sh" "${work}/charts"
cp -R "${root}/charts/steward" "${work}/charts/steward"
python3 "${root}/scripts/prepare-steward-bundle-schema.py" \
  "${work}/charts/steward/values.schema.json"
cp -R "${root}/charts/steward-bundle" "${work}/charts/steward-bundle"
cp "${root}/LICENSE" "${work}/charts/steward-bundle/LICENSE"
cp "${root}/THIRD_PARTY_NOTICES.md" \
  "${work}/charts/steward-bundle/THIRD_PARTY_NOTICES.md"

if [[ -n "${release_version}" ]] \
  || printf '%s\n' "${release_digests[@]}" | grep -q .; then
  python3 "${root}/scripts/set-steward-bundle-release-images.py" \
    "${work}/charts/steward-bundle/values.yaml" "${release_version}" \
    "${release_digests[@]}"
fi

export HELM_CACHE_HOME="${work}/helm/cache"
export HELM_CONFIG_HOME="${work}/helm/config"
export HELM_DATA_HOME="${work}/helm/data"
helm dependency update "${work}/charts/steward-bundle" >/dev/null

mkdir -p "$(dirname "${output}")"
cp -R "${work}/charts/steward-bundle" "${output}"
echo "prepared steward-bundle chart in ${output}"
