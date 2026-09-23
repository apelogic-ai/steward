#!/usr/bin/env bash
set -euo pipefail

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
expected="${STEWARD_RELEASE_VERSION:-}"
tick=$'\x60'

chart_version="$(awk '$1 == "version:" { print $2; exit }' "${root}/charts/steward/Chart.yaml")"
app_version="$(awk '$1 == "appVersion:" { print $2; exit }' "${root}/charts/steward/Chart.yaml")"
readme_version="$(sed -nE "s/^Current installation contract: chart ${tick}([^${tick}]+)${tick}.*/\\1/p" "${root}/README.md")"
guide_version="$(sed -nE "s/^Release contract: chart ${tick}([^${tick}]+)${tick}.*/\\1/p" "${root}/docs/installation/installation-guide.md")"

for observed in "${chart_version}" "${app_version}" "${readme_version}" "${guide_version}"; do
  if [[ -z "${observed}" || "${observed}" != "${chart_version}" ]]; then
    echo "release version metadata is inconsistent: chart=${chart_version}, app=${app_version}, README=${readme_version}, guide=${guide_version}" >&2
    exit 1
  fi
done

if [[ "${chart_version}" == "0.2.1" ]]; then
  chart_readme_version="$(sed -nE "s/^Current release contract: chart ${tick}([^${tick}]+)${tick}.*/\\1/p" "${root}/charts/steward/README.md")"
  changelog_version="$(sed -nE 's/^## \[([0-9]+\.[0-9]+\.[0-9]+)\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$/\1/p' "${root}/CHANGELOG.md" | head -n 1)"
  upgrade_version="$(sed -nE 's/^# Upgrade to Steward v([0-9]+\.[0-9]+\.[0-9]+)$/\1/p' "${root}/docs/installation/upgrade-v0.2.0.md")"
  for observed in "${chart_readme_version}" "${changelog_version}" "${upgrade_version}"; do
    if [[ -z "${observed}" || "${observed}" != "${chart_version}" ]]; then
      echo "v0.2 release metadata is inconsistent: chart=${chart_version}, chart-readme=${chart_readme_version}, changelog=${changelog_version}, upgrade=${upgrade_version}" >&2
      exit 1
    fi
  done
fi

if [[ -n "${expected}" && "${expected}" != "${chart_version}" ]]; then
  echo "release tag version ${expected} does not match source version ${chart_version}" >&2
  exit 1
fi

printf '%s\n' "${chart_version}"
