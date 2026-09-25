#!/usr/bin/env bash
set -euo pipefail

# The release workflow sets this for the real source tree. Fixture assertions
# below set it explicitly only when testing tag/source agreement.
unset STEWARD_RELEASE_VERSION

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
  printf 'Current installation contract: chart \x60%s\x60.\n' "${readme}" > "${fixture}/README.md"
  printf 'Release contract: chart \x60%s\x60.\n' "${guide}" > "${fixture}/docs/installation/installation-guide.md"
  printf 'Current release contract: chart \x60%s\x60.\n' "${guide}" > "${fixture}/charts/steward/README.md"
  printf '## [%s] - 2026-09-22\n' "${guide}" > "${fixture}/CHANGELOG.md"
  printf '# Upgrade to Steward v%s\n' "${guide}" > "${fixture}/docs/installation/upgrade-v0.2.0.md"
}

write_fixture 0.1.23 0.1.23 0.1.23 0.1.23
test "$(bash "${root}/scripts/validate-release-version.sh" "${fixture}")" = 0.1.23
STEWARD_RELEASE_VERSION=0.1.23 bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null

for mismatch in app readme guide tag; do
  write_fixture 0.1.23 0.1.23 0.1.23 0.1.23
  case "${mismatch}" in
    app) sed -i.bak 's/appVersion: 0.1.23/appVersion: 0.1.22/' "${fixture}/charts/steward/Chart.yaml" ;;
    readme) sed -i.bak "s/chart \`0.1.23\`/chart \`0.1.22\`/" "${fixture}/README.md" ;;
    guide) sed -i.bak "s/chart \`0.1.23\`/chart \`0.1.22\`/" "${fixture}/docs/installation/installation-guide.md" ;;
    tag)
      if STEWARD_RELEASE_VERSION=0.1.22 bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null 2>&1; then
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

write_fixture 0.2.5 0.2.5 0.2.5 0.2.5
test "$(bash "${root}/scripts/validate-release-version.sh" "${fixture}")" = 0.2.5
for mismatch in chart-readme changelog upgrade; do
  write_fixture 0.2.5 0.2.5 0.2.5 0.2.5
  case "${mismatch}" in
    chart-readme) sed -i.bak "s/chart \`0.2.5\`/chart \`0.1.23\`/" "${fixture}/charts/steward/README.md" ;;
    changelog) sed -i.bak 's/\[0.2.5\]/[0.1.23]/' "${fixture}/CHANGELOG.md" ;;
    upgrade) sed -i.bak 's/v0.2.5/v0.1.23/' "${fixture}/docs/installation/upgrade-v0.2.0.md" ;;
  esac
  if bash "${root}/scripts/validate-release-version.sh" "${fixture}" >/dev/null 2>&1; then
    echo "mismatched ${mismatch} version unexpectedly passed" >&2
    exit 1
  fi
done

echo 'release version metadata validation passed'
