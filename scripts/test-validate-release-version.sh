#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "${fixture}"' EXIT
mkdir -p "${fixture}/charts/steward" "${fixture}/docs/installation"

write_fixture() {
  local chart="$1"
  local app="$2"
  local readme="$3"
  local guide="$4"
  printf 'apiVersion: v2\nname: steward\nversion: %s\nappVersion: %s\n' "${chart}" "${app}" > "${fixture}/charts/steward/Chart.yaml"
  printf 'Current installation contract: chart `%s`.\n' "${readme}" > "${fixture}/README.md"
  printf 'Release contract: chart `%s`.\n' "${guide}" > "${fixture}/docs/installation/installation-guide.md"
}

write_fixture 0.1.22 0.1.22 0.1.22 0.1.22
test "$(bash "${root}/scripts/validate-release-version.sh" "${fixture}")" = 0.1.22
STEWARD_RELEASE_VERSION=0.1.22 bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null

for mismatch in app readme guide tag; do
  write_fixture 0.1.22 0.1.22 0.1.22 0.1.22
  case "${mismatch}" in
    app) sed -i.bak 's/appVersion: 0.1.22/appVersion: 0.1.21/' "${fixture}/charts/steward/Chart.yaml" ;;
    readme) sed -i.bak 's/chart `0.1.22`/chart `0.1.21`/' "${fixture}/README.md" ;;
    guide) sed -i.bak 's/chart `0.1.22`/chart `0.1.21`/' "${fixture}/docs/installation/installation-guide.md" ;;
    tag)
      if STEWARD_RELEASE_VERSION=0.1.21 bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null 2>&1; then
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
