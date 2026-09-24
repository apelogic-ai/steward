#!/usr/bin/env bash
set -euo pipefail

version="${1:?version is required}"
output_directory="${2:?output directory is required}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must be MAJOR.MINOR.PATCH" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archive="$output_directory/steward-platform-preflight-$version.tar.gz"
digest="$output_directory/platform-preflight-bundle.digest"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT INT TERM
bundle="$staging/platform-preflight/v1"
mkdir -p "$bundle/examples" "$output_directory"
install -m 0755 "$root/scripts/steward-platform-preflight.py" "$bundle/steward-platform-preflight"
install -m 0755 "$root/scripts/steward-gateway-backend-tls-check.sh" "$bundle/steward-gateway-backend-tls-check"
cp "$root/config/platform-preflight/v1/input.schema.json" "$bundle/input.schema.json"
cp "$root/config/platform-preflight/v1/namespace-map.schema.json" "$bundle/namespace-map.schema.json"
cp "$root/config/platform-preflight/v1/examples/compact.json" "$bundle/examples/compact.json"
cp "$root/config/platform-preflight/v1/examples/separated.json" "$bundle/examples/separated.json"
cp "$root/docs/installation/platform-preflight.md" "$bundle/README.md"
printf '{"schemaVersion":"steward.platform-preflight-release/v1","sourceRelease":"v%s"}\n' "$version" > "$bundle/release.json"
find "$staging" -type f -exec touch -t 197001010000 {} +
paths=(
  platform-preflight/v1/README.md
  platform-preflight/v1/input.schema.json
  platform-preflight/v1/namespace-map.schema.json
  platform-preflight/v1/examples/compact.json
  platform-preflight/v1/examples/separated.json
  platform-preflight/v1/release.json
  platform-preflight/v1/steward-platform-preflight
  platform-preflight/v1/steward-gateway-backend-tls-check
)
if tar --help 2>&1 | grep -Fq -- '--sort'; then
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=ustar \
    -C "$staging" -cf - "${paths[@]}" | gzip -n > "$archive"
else
  COPYFILE_DISABLE=1 tar -C "$staging" -cf - "${paths[@]}" | gzip -n > "$archive"
fi
printf 'sha256:%s\n' "$(sha256sum "$archive" | awk '{print $1}')" > "$digest"
