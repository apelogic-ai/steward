#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "${fixture}"' EXIT
mkdir -p "${fixture}/charts/steward" "${fixture}/docs/installation"
current_version=0.1.23
previous_version=0.1.22

write_fixture() {
  local chart="$1"
  local app="$2"
  local readme="$3"
  local guide="$4"
  printf 'apiVersion: v2\nname: steward\nversion: %s\nappVersion: %s\n' "${chart}" "${app}" > "${fixture}/charts/steward/Chart.yaml"
  printf 'Current installation contract: chart \x60%s\x60.\n' "${readme}" > "${fixture}/README.md"
  printf 'Release contract: chart \x60%s\x60.\n' "${guide}" > "${fixture}/docs/installation/installation-guide.md"
}

write_fixture "${current_version}" "${current_version}" "${current_version}" "${current_version}"
test "$(bash "${root}/scripts/validate-release-version.sh" "${fixture}")" = "${current_version}"
STEWARD_RELEASE_VERSION="${current_version}" bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null

for mismatch in app readme guide tag; do
  write_fixture "${current_version}" "${current_version}" "${current_version}" "${current_version}"
  case "${mismatch}" in
    app) sed -i.bak "s/appVersion: ${current_version}/appVersion: ${previous_version}/" "${fixture}/charts/steward/Chart.yaml" ;;
    readme) sed -i.bak "s/chart \`${current_version}\`/chart \`${previous_version}\`/" "${fixture}/README.md" ;;
    guide) sed -i.bak "s/chart \`${current_version}\`/chart \`${previous_version}\`/" "${fixture}/docs/installation/installation-guide.md" ;;
    tag)
      if STEWARD_RELEASE_VERSION="${previous_version}" bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null 2>&1; then
        echo 'mismatched release tag unexpectedly passed' >&2
        exit 1
      fi
      continue
      ;;
  esac
  if bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null 2>&1; then
    echo "mismatched ${mismatch} version unexpectedly passed" >&2
    exit 1
  fi
done

echo 'release version metadata validation passed'
