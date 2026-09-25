#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  echo "usage: $0 --image <repository@sha256:digest> --provider-profile-bundle <archive>" >&2
  exit 2
}

image=""
profile_bundle=""
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --image)
      [[ "$#" -ge 2 ]] || usage
      image="$2"
      shift 2
      ;;
    --provider-profile-bundle)
      [[ "$#" -ge 2 ]] || usage
      profile_bundle="$2"
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ -n "${image}" && -n "${profile_bundle}" ]] || usage
if [[ "${STEWARD_ALLOW_LOCAL_CODEX_IMAGE:-0}" != "1" && ! "${image}" =~ @sha256:[0-9a-f]{64}$ ]]; then
  echo "Codex reference runtime conformance requires an immutable digest reference" >&2
  exit 1
fi
if [[ ! -f "${profile_bundle}" ]]; then
  echo "provider-profile bundle does not exist: ${profile_bundle}" >&2
  exit 1
fi
for command in docker jq tar; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

temporary_directory="$(mktemp -d)"
cleanup() {
  status="$?"
  trap - EXIT INT TERM
  rm -rf "${temporary_directory}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

if [[ "${STEWARD_ALLOW_LOCAL_CODEX_IMAGE:-0}" == "1" ]]; then
  docker image inspect "${image}" >/dev/null
else
  docker pull --platform linux/amd64 "${image}" >/dev/null
fi
image_contract="$(docker image inspect --format '{{.Os}}/{{.Architecture}} {{.Config.User}}' "${image}")"
if [[ "${image_contract}" != "linux/amd64 sandbox" ]]; then
  echo "Codex reference runtime has unexpected platform or user: ${image_contract}" >&2
  exit 1
fi

expected_binary="/usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex/codex"
docker run --rm --platform linux/amd64 --entrypoint /bin/sh "${image}" -c \
  'test "$(id -u)" != 0 && test "$(/usr/bin/codex --version)" = "codex-cli 0.140.0"' \
  >/dev/null
docker run --rm --platform linux/amd64 --entrypoint "${expected_binary}" "${image}" --version \
  | grep -Fx 'codex-cli 0.140.0' >/dev/null

tar -xzf "${profile_bundle}" -C "${temporary_directory}"
for profile in \
  provider-profile-bundle/v1.2.0/profiles/steward-litellm.json \
  provider-profile-bundle/v1.2.0/profiles/steward-mcp-gw.json
do
  jq --exit-status --arg binary "${expected_binary}" \
    '.runtime.requiredBinaries | index($binary) != null' \
    "${temporary_directory}/${profile}" >/dev/null
done

run_id="codex-reference-$(date -u +%Y%m%d%H%M%S)-$$"
run_owned_image="steward/codex-reference-runtime:${run_id}"
docker tag "${image}" "${run_owned_image}"
openshell_cleanup() {
  status="$?"
  trap - EXIT INT TERM
  docker image rm "${run_owned_image}" >/dev/null 2>&1 || true
  rm -rf "${temporary_directory}"
  exit "${status}"
}
trap openshell_cleanup EXIT INT TERM

STEWARD_RUN_ID="${run_id}" \
STEWARD_OPEN_SHELL_RELEASE=v0.0.98 \
STEWARD_OPENSHELL_SUPERVISOR_TOPOLOGY=sidecar \
STEWARD_OPENSHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY=true \
STEWARD_OPENSHELL_SANDBOX_IMAGE="${run_owned_image}" \
bash "${root}/scripts/openshell-testbed.sh" \
  bash "${root}/scripts/codex-reference-runtime-openshell-inside.sh" "${profile_bundle}"

echo "Codex reference runtime linux/amd64 and provider-profile compatibility verified"
