#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-${ROOT}/target/governed-connections-artifacts}"

for command in cargo install sed strip; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

mkdir -p "${OUTPUT_DIR}"

cargo build \
  --locked \
  --manifest-path "${ROOT}/Cargo.toml" \
  --package steward-mint-bin \
  --bin steward-mint-bin
cargo build \
  --locked \
  --manifest-path "${ROOT}/Cargo.toml" \
  --bin steward-connections-bridge
cargo build \
  --locked \
  --manifest-path "${ROOT}/e2e/Cargo.toml" \
  --bin governed-connections-webhook

test_binary="$({
  cargo test \
    --locked \
    --manifest-path "${ROOT}/e2e/Cargo.toml" \
    --test governed_connections \
    --no-run \
    --message-format=json
} | sed -nE '/"name":"governed_connections"/s/.*"executable":"([^"]+)".*/\1/p')"
if [[ -z "${test_binary}" || "${test_binary}" == *$'\n'* || ! -x "${test_binary}" ]]; then
  echo "cargo did not report exactly one executable governed Connections test binary" >&2
  exit 1
fi

install -m 0755 "${ROOT}/target/debug/steward-mint-bin" \
  "${OUTPUT_DIR}/steward-mint-bin"
install -m 0755 "${ROOT}/target/debug/steward-connections-bridge" \
  "${OUTPUT_DIR}/steward-connections-bridge"
install -m 0755 "${ROOT}/target/debug/governed-connections-webhook" \
  "${OUTPUT_DIR}/governed-connections-webhook"
install -m 0755 "${test_binary}" "${OUTPUT_DIR}/governed_connections"
strip \
  "${OUTPUT_DIR}/steward-mint-bin" \
  "${OUTPUT_DIR}/steward-connections-bridge" \
  "${OUTPUT_DIR}/governed-connections-webhook" \
  "${OUTPUT_DIR}/governed_connections"
install -m 0644 "${ROOT}/LICENSE" "${OUTPUT_DIR}/LICENSE"
install -m 0644 "${ROOT}/THIRD_PARTY_NOTICES.md" \
  "${OUTPUT_DIR}/THIRD_PARTY_NOTICES.md"

printf 'packaged governed Connections executables in %s\n' "${OUTPUT_DIR}"
