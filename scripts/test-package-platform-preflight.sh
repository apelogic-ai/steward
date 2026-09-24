#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT INT TERM
for name in first second; do
  "$root/scripts/package-platform-preflight.sh" 0.2.2 "$temporary/$name"
done
first="$temporary/first/steward-platform-preflight-0.2.2.tar.gz"
second="$temporary/second/steward-platform-preflight-0.2.2.tar.gz"
cmp "$first" "$second"
cmp "$temporary/first/platform-preflight-bundle.digest" "$temporary/second/platform-preflight-bundle.digest"
tar -xzf "$first" -C "$temporary"
bundle="$temporary/platform-preflight/v1"
"$bundle/steward-platform-preflight" validate --input "$bundle/examples/compact.json" >/dev/null
printf 'deterministic platform preflight bundle verified\n'
