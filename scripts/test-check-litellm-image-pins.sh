#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'find "${fixture}" -depth -delete 2>/dev/null || true' EXIT INT TERM

mkdir -p "${fixture}/scripts" "${fixture}/charts/litellm"
cp "${root}/scripts/check-litellm-image-pins.sh" "${fixture}/scripts/"
cp "${root}/scripts/g4-upstream-conformance" "${fixture}/scripts/"
git -C "${fixture}" init --quiet

write_values() {
  local repository="$1" tag="$2"
  printf '%s\n' \
    'litellm:' \
    '  image:' \
    "    repository: ${repository}" \
    "    tag: ${tag}" \
    >"${fixture}/charts/litellm/values.yaml"
}

assert_passes() {
  local description="$1" repository="$2" tag="$3"
  write_values "${repository}" "${tag}"
  git -C "${fixture}" add scripts/check-litellm-image-pins.sh \
    scripts/g4-upstream-conformance charts/litellm/values.yaml
  if ! bash "${fixture}/scripts/check-litellm-image-pins.sh" >/dev/null 2>&1; then
    echo "LiteLLM image pin checker rejected ${description}" >&2
    exit 1
  fi
}

assert_refused() {
  local description="$1" repository="$2" tag="$3"
  write_values "${repository}" "${tag}"
  git -C "${fixture}" add charts/litellm/values.yaml
  if bash "${fixture}/scripts/check-litellm-image-pins.sh" >/dev/null 2>&1; then
    echo "LiteLLM image pin checker accepted ${description}" >&2
    exit 1
  fi
}

vetted_reference="$(
  sed -nE 's/^LATEST_IMAGE="([^"]+)"$/\1/p' \
    "${root}/scripts/g4-upstream-conformance"
)"
repository="${vetted_reference%%:*}"
vetted_tag="${vetted_reference#*:}"
digest="${vetted_reference##*@}"

assert_passes 'the vetted split pin' "${repository}" "${vetted_tag}"
assert_refused 'a missing digest' "${repository}" 'v1.106.2'
assert_refused 'a malformed digest' "${repository}" 'v1.106.2@sha256:bad'
assert_refused 'a different digest' "${repository}" \
  'v1.106.2@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
assert_refused 'a mutable tag' "${repository}" "latest@${digest}"
assert_refused 'a repository override without a digest' \
  'registry.example.test/mirror/litellm-database' 'v1.106.2'

echo 'LiteLLM split image pin regressions passed'
