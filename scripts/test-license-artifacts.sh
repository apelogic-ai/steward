#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_dir="$(mktemp -d)"
trap 'find "${package_dir}" -depth -delete' EXIT

test -s "${root}/LICENSE"
grep -Fxq 'Copyright (c) 2026 ApeLogic Inc.' "${root}/LICENSE"
cmp -s "${root}/LICENSE" "${root}/charts/steward/LICENSE"
grep -Fxq 'license = "MIT"' "${root}/Cargo.toml"
jq -e '.license == "MIT"' "${root}/package.json" >/dev/null
jq -e '.license == "MIT"' "${root}/web/package.json" >/dev/null
grep -Fxq '  artifacthub.io/license: MIT' "${root}/charts/steward/Chart.yaml"
grep -Fq '[MIT License](LICENSE)' "${root}/README.md"
test -s "${root}/THIRD_PARTY_NOTICES.md"
test -s "${root}/third_party/mcp-gw-patches/c2af10d9/LICENSE"

for dockerfile in package web connections-bridge; do
  grep -Fq 'COPY LICENSE /usr/share/licenses/steward/LICENSE' \
    "${root}/build/${dockerfile}.Dockerfile"
  grep -Fq 'COPY THIRD_PARTY_NOTICES.md /usr/share/licenses/steward/THIRD_PARTY_NOTICES.md' \
    "${root}/build/${dockerfile}.Dockerfile"
done

helm package "${root}/charts/steward" --destination "${package_dir}" >/dev/null
archive="$(find "${package_dir}" -maxdepth 1 -name 'steward-*.tgz' -print -quit)"
test -n "${archive}"
tar -tzf "${archive}" | grep -Fxq steward/LICENSE

echo 'Steward license artifacts and metadata passed'
