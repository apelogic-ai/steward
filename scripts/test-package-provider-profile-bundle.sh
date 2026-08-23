#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT INT TERM

for directory in first second; do
  "${root}/scripts/package-provider-profile-bundle.sh" \
    --version 1.0.0 \
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
  provider-profile-bundle/v1/README.md
  provider-profile-bundle/v1/bundle.json
  provider-profile-bundle/v1/profiles/steward-litellm.json
  provider-profile-bundle/v1/profiles/steward-mcp-gw.json
)
actual_entries="$(tar -tzf "$first_archive")"
expected_entries_text="$(printf '%s\n' "${expected_entries[@]}")"
if [[ "$actual_entries" != "$expected_entries_text" ]]; then
  echo "unexpected archive entry count" >&2
  exit 1
fi

echo "deterministic provider-profile bundle archive verified"
