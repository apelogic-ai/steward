#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MCP_GW_COMMIT="c2af10d9c3dee898e368e6cf3d0f5a1ef6ad0dde"
MCP_GW_ARCHIVE_SHA256="a000fc4b028921c218d265887c9ecabf35af417d0b86810b99786d544a73fa28"
MCP_GW_IMAGE="${STEWARD_MCP_GW_IMAGE:-steward/mcp-gw:c2af10d9-claims}"
PATCH="${ROOT}/third_party/mcp-gw-patches/c2af10d9/0001-expose-verified-claims-to-tool-policy.patch"

for command in curl docker git tar; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

if command -v sha256sum >/dev/null 2>&1; then
  checksum=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  checksum=(shasum -a 256)
else
  echo "required command is missing: sha256sum or shasum" >&2
  exit 2
fi

mkdir -p "${ROOT}/.steward-run"
BUILD_DIR="$(mktemp -d "${ROOT}/.steward-run/mcp-gw-build.XXXXXX")"
cleanup() {
  status="$1"
  trap - EXIT INT TERM
  find "${BUILD_DIR}" -depth -delete 2>/dev/null || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

archive="${BUILD_DIR}/source.tar.gz"
source_dir="${BUILD_DIR}/source"
curl -fsSL \
  "https://github.com/apelogic-ai/mcp-gw/archive/${MCP_GW_COMMIT}.tar.gz" \
  -o "${archive}"
actual_archive_sha="$("${checksum[@]}" "${archive}" | cut -d ' ' -f 1)"
if [[ "${actual_archive_sha}" != "${MCP_GW_ARCHIVE_SHA256}" ]]; then
  echo "mcp-gw source archive checksum mismatch" >&2
  exit 1
fi
mkdir -p "${source_dir}"
tar -xzf "${archive}" -C "${source_dir}" --strip-components=1
git -C "${source_dir}" init --quiet
git -C "${source_dir}" apply --check "${PATCH}"
git -C "${source_dir}" apply "${PATCH}"
git -C "${source_dir}" apply --check --reverse "${PATCH}"
patch_sha="$("${checksum[@]}" "${PATCH}" | cut -d ' ' -f 1)"

docker build \
  --progress plain \
  --file "${ROOT}/config/s1/mcp-gw.Dockerfile" \
  --tag "${MCP_GW_IMAGE}" \
  --build-arg "MCP_GW_COMMIT=${MCP_GW_COMMIT}" \
  --build-arg "MCP_GW_PATCH_SHA256=${patch_sha}" \
  "${source_dir}"

echo "${MCP_GW_IMAGE}"
