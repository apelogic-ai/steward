#!/usr/bin/env bash
set -euo pipefail

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
expected="${STEWARD_RELEASE_VERSION:-}"

chart_version="$(awk '$1 == "version:" { print $2; exit }' "${root}/charts/steward/Chart.yaml")"
app_version="$(awk '$1 == "appVersion:" { print $2; exit }' "${root}/charts/steward/Chart.yaml")"
readme_version="$(sed -nE 's/^Current installation contract: chart `([^`]+)`.*/\1/p' "${root}/README.md")"
guide_version="$(sed -nE 's/^Release contract: chart `([^`]+)`.*/\1/p' "${root}/docs/installation/installation-guide.md")"

for observed in "${chart_version}" "${app_version}" "${readme_version}" "${guide_version}"; do
  if [[ -z "${observed}" || "${observed}" != "${chart_version}" ]]; then
    echo "release version metadata is inconsistent: chart=${chart_version}, app=${app_version}, README=${readme_version}, guide=${guide_version}" >&2
    exit 1
  fi
done

if [[ -n "${expected}" && "${expected}" != "${chart_version}" ]]; then
  echo "release tag version ${expected} does not match source version ${chart_version}" >&2
  exit 1
fi

printf '%s\n' "${chart_version}"
