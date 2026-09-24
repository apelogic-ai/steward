#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT INT TERM

cargo build --quiet --manifest-path "${root}/Cargo.toml" \
  --package xtask --bin steward-provider-profile
installer="${root}/target/debug/steward-provider-profile"
installer_os="$(uname -s | tr '[:upper:]' '[:lower:]')"
installer_architecture="$(uname -m)"

for directory in first second; do
  "${root}/scripts/package-provider-profile-bundle.sh" \
    --version 1.0.0 \
    --installer "${installer}" \
    --installer-os "${installer_os}" \
    --installer-architecture "${installer_architecture}" \
    --output "${temporary_directory}/${directory}" >/dev/null
done

first_archive="${temporary_directory}/first/steward-runtime-providers-1.0.0.tar.gz"
second_archive="${temporary_directory}/second/steward-runtime-providers-1.0.0.tar.gz"
first_digest="${temporary_directory}/first/provider-profile-bundle.digest"
second_digest="${temporary_directory}/second/provider-profile-bundle.digest"

if ! cmp -s "$first_archive" "$second_archive"; then
  echo "deterministic provider-profile bundle archive bytes must be reproducible" >&2
  exit 1
fi
if ! cmp -s "$first_digest" "$second_digest"; then
  echo "deterministic provider-profile bundle digest must be reproducible" >&2
  exit 1
fi

actual_digest="sha256:$(sha256sum "$first_archive" | awk '{print $1}')"
if [[ "$(<"$first_digest")" != "$actual_digest" ]]; then
  echo "digest must bind the archive bytes" >&2
  exit 1
fi

expected_entries=(
  provider-profile-bundle/v1.2.0/README.md
  provider-profile-bundle/v1.2.0/bundle.json
  provider-profile-bundle/v1.2.0/profiles/steward-litellm.json
  provider-profile-bundle/v1.2.0/profiles/steward-mcp-gw.json
  provider-profile-bundle/v1.2.0/examples/inputs.json
  provider-profile-bundle/v1.2.0/release.json
  provider-profile-bundle/v1.2.0/bin/steward-provider-profile
)
actual_entries="$(tar -tzf "$first_archive")"
expected_entries_text="$(printf '%s\n' "${expected_entries[@]}")"
if [[ "$actual_entries" != "$expected_entries_text" ]]; then
  echo "unexpected archive entry count" >&2
  exit 1
fi

tar -xzf "$first_archive" -C "${temporary_directory}"
bundle="${temporary_directory}/provider-profile-bundle/v1.2.0"
tool="${bundle}/bin/steward-provider-profile"
inputs="${bundle}/examples/inputs.json"
output="${temporary_directory}/rendered"
test -x "${tool}"
validation="$(${tool} validate --bundle "${bundle}" --inputs "${inputs}")"
if ! jq -e --arg os "${installer_os}" --arg architecture "${installer_architecture}" '
  .schemaVersion == "steward.provider-profile-result/v1" and
  .operation == "validate" and
  .status == "valid" and
  .installer.os == $os and
  .installer.architecture == $architecture and
  (.closureDigest | test("^sha256:[0-9a-f]{64}$")) and
  (.profiles | length == 2)
' <<<"${validation}" >/dev/null; then
  echo "standalone provider-profile validation did not emit the required machine-readable result" >&2
  exit 1
fi
"${tool}" install --bundle "${bundle}" --inputs "${inputs}" --output "${output}" >/dev/null
"${tool}" reconcile --bundle "${bundle}" --inputs "${inputs}" --output "${output}" >/dev/null
test -s "${output}/install-state.json"
test -s "${output}/profiles/steward-litellm.json"
test -s "${output}/profiles/steward-mcp-gw.json"

echo "deterministic provider-profile bundle archive verified"
