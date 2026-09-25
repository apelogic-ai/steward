#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT INT TERM
cargo build --quiet --manifest-path "$root/Cargo.toml" \
  --package xtask --bin steward-provider-profile
installer="$root/target/debug/steward-provider-profile"
installer_os="$(uname -s | tr '[:upper:]' '[:lower:]')"
installer_architecture="$(uname -m)"
"$root/scripts/package-provider-profile-bundle.sh" \
  --version 0.2.3 \
  --installer "$installer" \
  --installer-os "$installer_os" \
  --installer-architecture "$installer_architecture" \
  --output "$temporary/provider-release" >/dev/null
tar -xzf "$temporary/provider-release/steward-runtime-providers-0.2.3.tar.gz" -C "$temporary"
provider_bundle="$temporary/provider-profile-bundle/v1.2.0"
for name in first second; do
  "$root/scripts/package-platform-preflight.sh" 0.2.3 "$temporary/$name"
done
first="$temporary/first/steward-platform-preflight-0.2.3.tar.gz"
second="$temporary/second/steward-platform-preflight-0.2.3.tar.gz"
cmp "$first" "$second"
cmp "$temporary/first/platform-preflight-bundle.digest" "$temporary/second/platform-preflight-bundle.digest"
tar -xzf "$first" -C "$temporary"
bundle="$temporary/platform-preflight/v1"
"$bundle/steward-platform-preflight" validate \
  --input "$bundle/examples/compact.json" \
  --provider-profile-bundle "$provider_bundle" >/dev/null
"$bundle/steward-platform-preflight" validate \
  --input "$bundle/examples/governed-complete.json" \
  --provider-profile-bundle "$provider_bundle" >/dev/null
generated="$temporary/generated"
"$bundle/steward-platform-preflight" generate \
  --input "$bundle/examples/governed-complete.json" \
  --provider-profile-bundle "$provider_bundle" \
  --output "$generated" >/dev/null
profile_result="$("$provider_bundle/bin/steward-provider-profile" validate \
  --bundle "$provider_bundle" \
  --inputs "$generated/provider-profile-inputs.json")"
jq --exit-status --argjson result "$profile_result" '
  .config.apiserver.executionBindings.bindings[0].providerProfiles as $binding |
  ($result.profiles | map({key: .id, value: .digest}) | from_entries) as $digests |
  $binding.tools.digest == $digests[$binding.tools.id] and
  $binding.inference.digest == $digests[$binding.inference.id]
' "$generated/steward-values.json" >/dev/null
printf 'deterministic platform preflight bundle verified\n'
