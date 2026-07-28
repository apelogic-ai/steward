#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${STEWARD_CONFORMANCE_TARGET:-}"
PINNED_COMMIT="c2af10d9c3dee898e368e6cf3d0f5a1ef6ad0dde"
PINNED_ARCHIVE_SHA256="a000fc4b028921c218d265887c9ecabf35af417d0b86810b99786d544a73fa28"
BUN_IMAGE="oven/bun:1.2.21@sha256:5a2011bf09364b9af658ac1e66f60d08092f4291aeefbff448d58b027734fdd0"
PATCH="${ROOT}/third_party/mcp-gw-patches/c2af10d9/0001-expose-verified-claims-to-tool-policy.patch"

case "${TARGET}" in
  pinned | latest)
    ;;
  *)
    echo "STEWARD_CONFORMANCE_TARGET must be pinned or latest" >&2
    exit 2
    ;;
esac

for command in awk curl docker git tar; do
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
RUN_DIR="$(mktemp -d "${ROOT}/.steward-run/g2-conformance.XXXXXX")"
cleanup() {
  status="$1"
  trap - EXIT INT TERM
  find "${RUN_DIR}" -depth -delete 2>/dev/null || true
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ "${TARGET}" == "pinned" ]]; then
  commit="${PINNED_COMMIT}"
else
  read -r commit _ < <(
    git ls-remote --exit-code https://github.com/apelogic-ai/mcp-gw.git refs/heads/main
  )
  if [[ ! "${commit}" =~ ^[0-9a-f]{40}$ ]]; then
    echo "latest mcp-gw ref did not resolve to a commit" >&2
    exit 1
  fi
fi

archive="${RUN_DIR}/mcp-gw.tar.gz"
source_dir="${RUN_DIR}/source"
curl -fsSL "https://github.com/apelogic-ai/mcp-gw/archive/${commit}.tar.gz" -o "${archive}"
if [[ "${TARGET}" == "pinned" ]]; then
  actual_archive_sha="$("${checksum[@]}" "${archive}" | cut -d ' ' -f 1)"
  if [[ "${actual_archive_sha}" != "${PINNED_ARCHIVE_SHA256}" ]]; then
    echo "pinned mcp-gw source archive checksum mismatch" >&2
    exit 1
  fi
fi
mkdir -p "${source_dir}"
tar -xzf "${archive}" -C "${source_dir}" --strip-components=1

if [[ "${TARGET}" == "pinned" ]]; then
  git -C "${source_dir}" init --quiet
  git -C "${source_dir}" apply --check "${PATCH}"
  git -C "${source_dir}" apply "${PATCH}"
  git -C "${source_dir}" apply --check --reverse "${PATCH}"
fi

mkdir -p "${source_dir}/conformance"
cp \
  "${ROOT}/conformance/fixtures/g2_credential_isolation.test.ts" \
  "${source_dir}/conformance/g2_credential_isolation.test.ts"
mkdir -p "${source_dir}/node_modules"

if bun_output="$(
  docker run --rm \
    --mount "type=bind,source=${source_dir},target=/src,readonly" \
    --mount "type=tmpfs,target=/src/node_modules" \
    --workdir /src \
    --entrypoint sh \
    "${BUN_IMAGE}" \
    -ceu 'bun install --frozen-lockfile; bun test conformance/g2_credential_isolation.test.ts' \
    2>&1
)"; then
  printf '%s\n' "${bun_output}"
else
  printf '%s\n' "${bun_output}" >&2
  exit 1
fi

if ! printf '%s\n' "${bun_output}" | awk '
  $0 == " 1 pass" { passes += 1 }
  $0 == " 0 fail" { failures += 1 }
  $0 ~ /^ [0-9]+ skip$/ { skips += $1 }
  $0 ~ /^Ran 1 test across 1 file\./ { runs += 1 }
  END {
    if (passes != 1 || failures != 1 || skips != 0 || runs != 1) {
      exit 1
    }
  }
'; then
  echo "G-2 upstream evidence must execute exactly one passing Bun test with none skipped" >&2
  exit 1
fi

echo "G-2 upstream result: 1 passed; 0 failed; 0 skipped"
echo "G-2 ${TARGET} upstream evidence passed against mcp-gw ${commit}"
