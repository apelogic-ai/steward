#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --version <MAJOR.MINOR.PATCH> --output <directory>" >&2
}

version=""
output_directory=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      version="${2:-}"
      shift 2
      ;;
    --output)
      output_directory="${2:-}"
      shift 2
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ -z "$output_directory" ]]; then
  usage
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bundle_directory="${root}/config/provider-profile-bundle/v1"
archive_name="steward-runtime-providers-${version}.tar.gz"
archive_path="${output_directory}/${archive_name}"
digest_path="${output_directory}/provider-profile-bundle.digest"

for required in \
  "${bundle_directory}/bundle.json" \
  "${bundle_directory}/README.md" \
  "${bundle_directory}/profiles/steward-litellm.json" \
  "${bundle_directory}/profiles/steward-mcp-gw.json"
do
  test -s "$required"
done

mkdir -p "$output_directory"
if [[ -e "$archive_path" || -e "$digest_path" ]]; then
  echo "provider-profile bundle output must be absent: ${output_directory}" >&2
  exit 1
fi

bundle_paths=(
  provider-profile-bundle/v1/README.md
  provider-profile-bundle/v1/bundle.json
  provider-profile-bundle/v1/profiles/steward-litellm.json
  provider-profile-bundle/v1/profiles/steward-mcp-gw.json
)

# GNU tar metadata controls plus gzip -n make the release bundle bytes
# reproducible. macOS's BSD tar lacks the GNU normalisation switches, so the
# local fallback uses the same explicitly sorted files; the CI release runner
# always executes the GNU branch.
if tar --help 2>&1 | grep -Fq -- '--sort'; then
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=ustar \
    -C "${root}/config" -cf - "${bundle_paths[@]}" \
    | gzip -n > "$archive_path"
else
  tar -C "${root}/config" -cf - "${bundle_paths[@]}" \
    | gzip -n > "$archive_path"
fi

expected_entries="$(cat <<'ENTRIES'
provider-profile-bundle/v1/README.md
provider-profile-bundle/v1/bundle.json
provider-profile-bundle/v1/profiles/steward-litellm.json
provider-profile-bundle/v1/profiles/steward-mcp-gw.json
ENTRIES
)"
actual_entries="$(tar -tzf "$archive_path")"
if [[ "$actual_entries" != "$expected_entries" ]]; then
  echo "provider-profile bundle archive entries are not installer-compatible" >&2
  diff -u <(printf '%s\n' "$expected_entries") <(printf '%s\n' "$actual_entries") >&2 || true
  exit 1
fi
for archived_file in \
  README.md \
  bundle.json \
  profiles/steward-litellm.json \
  profiles/steward-mcp-gw.json
do
  if ! cmp -s "${bundle_directory}/${archived_file}" \
    <(tar -xOzf "$archive_path" "provider-profile-bundle/v1/${archived_file}")
  then
    echo "provider-profile bundle archive content does not match ${archived_file}" >&2
    exit 1
  fi
done

printf 'sha256:%s\n' "$(sha256sum "$archive_path" | awk '{print $1}')" > "$digest_path"
printf 'provider-profile-bundle=%s\n' "$archive_name"
printf 'provider-profile-bundle-digest=%s\n' "$(<"$digest_path")"
