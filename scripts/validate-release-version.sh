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

for dependency_chart in spire agent-sandbox openshell litellm arc steward-bundle; do
  dependency_chart_file="${root}/charts/${dependency_chart}/Chart.yaml"
  dependency_version="$(awk '$1 == "version:" { print $2; exit }' "${dependency_chart_file}")"
  if [[ -z "${dependency_version}" || "${dependency_version}" != "${chart_version}" ]]; then
    echo "release chart versions are inconsistent: steward=${chart_version}, ${dependency_chart}=${dependency_version}" >&2
    exit 1
  fi
done

bundle_chart_file="${root}/charts/steward-bundle/Chart.yaml"
bundle_app_version="$(awk '$1 == "appVersion:" { print $2; exit }' "${bundle_chart_file}")"
if [[ -z "${bundle_app_version}" || "${bundle_app_version}" != "${chart_version}" ]]; then
  echo "release bundle application version is inconsistent: steward=${chart_version}, steward-bundle=${bundle_app_version}" >&2
  exit 1
fi
for dependency_chart in steward spire agent-sandbox openshell litellm arc; do
  bundle_dependency_version="$({
    awk -v chart="${dependency_chart}" '
      $1 == "-" && $2 == "name:" { active = ($3 == chart) }
      active && $1 == "version:" { print $2; exit }
    ' "${bundle_chart_file}"
  })"
  if [[ -z "${bundle_dependency_version}" || "${bundle_dependency_version}" != "${chart_version}" ]]; then
    echo "release bundle dependency version is inconsistent: steward=${chart_version}, ${dependency_chart}=${bundle_dependency_version}" >&2
    exit 1
  fi
done
bundle_values_file="${root}/charts/steward-bundle/values.yaml"
for component_suffix in \
  'apiserver apiserver' \
  'controller controller' \
  'mint mint' \
  'web web' \
  'triggerReceiver trigger-receiver'; do
  read -r component suffix <<<"${component_suffix}"
  bundle_image_tag="$(awk -v component="${component}" '
    $1 == component ":" && $0 ~ /^    / { active = 1; next }
    active && $1 == "tag:" { print $2; exit }
  ' "${bundle_values_file}")"
  if [[ "${bundle_image_tag}" != "${chart_version}-${suffix}" ]]; then
    echo "release bundle Steward image tag is inconsistent: ${component}=${bundle_image_tag}, expected=${chart_version}-${suffix}" >&2
    exit 1
  fi
done

if [[ "${chart_version}" == "0.3.25" ]]; then
  chart_readme_version="$(sed -nE "s/^Current release contract: chart ${tick}([^${tick}]+)${tick}.*/\\1/p" "${root}/charts/steward/README.md")"
  changelog_version="$(sed -nE 's/^## \[([0-9]+\.[0-9]+\.[0-9]+)\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$/\1/p' "${root}/CHANGELOG.md" | head -n 1)"
  upgrade_version="$(sed -nE 's/^# Upgrade to Steward v([0-9]+\.[0-9]+\.[0-9]+)$/\1/p' "${root}/docs/installation/upgrade-v0.3.0.md")"
  for observed in "${chart_readme_version}" "${changelog_version}"; do
    if [[ -z "${observed}" || "${observed}" != "${chart_version}" ]]; then
      echo "v0.3 release metadata is inconsistent: chart=${chart_version}, chart-readme=${chart_readme_version}, changelog=${changelog_version}, upgrade=${upgrade_version}" >&2
      exit 1
    fi
  done
  if [[ "${upgrade_version}" != "0.3.0" ]]; then
    echo "v0.3 release metadata is inconsistent: chart=${chart_version}, chart-readme=${chart_readme_version}, changelog=${changelog_version}, upgrade=${upgrade_version}" >&2
    exit 1
  fi
fi

if [[ -n "${expected}" && "${expected}" != "${chart_version}" ]]; then
  echo "release tag version ${expected} does not match source version ${chart_version}" >&2
  exit 1
fi

printf '%s\n' "${chart_version}"
