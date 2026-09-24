#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --version <MAJOR.MINOR.PATCH> --installer <binary> --installer-os <os> --installer-architecture <architecture> --output <directory>" >&2
}

version=""
installer=""
installer_os=""
installer_architecture=""
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
    --installer)
      installer="${2:-}"
      shift 2
      ;;
    --installer-os)
      installer_os="${2:-}"
      shift 2
      ;;
    --installer-architecture)
      installer_architecture="${2:-}"
      shift 2
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ -z "$output_directory" ]] \
  || [[ ! -x "$installer" ]] || [[ ! "$installer_os" =~ ^[a-z0-9_-]+$ ]] \
  || [[ ! "$installer_architecture" =~ ^[a-z0-9_-]+$ ]]; then
  usage
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bundle_directory="${root}/config/provider-profile-bundle/v1.2.0"
archive_name="steward-runtime-providers-${version}.tar.gz"
archive_path="${output_directory}/${archive_name}"
digest_path="${output_directory}/provider-profile-bundle.digest"
staging_directory="$(mktemp -d)"
trap 'rm -rf "$staging_directory"' EXIT INT TERM

for required in \
  "${bundle_directory}/bundle.json" \
  "${bundle_directory}/README.md" \
  "${bundle_directory}/profiles/steward-litellm.json" \
  "${bundle_directory}/profiles/steward-mcp-gw.json" \
  "${bundle_directory}/examples/inputs.json"
do
  test -s "$required"
done

mkdir -p "$output_directory"
if [[ -e "$archive_path" || -e "$digest_path" ]]; then
  echo "provider-profile bundle output must be absent: ${output_directory}" >&2
  exit 1
fi

bundle_paths=(
  provider-profile-bundle/v1.2.0/README.md
  provider-profile-bundle/v1.2.0/bundle.json
  provider-profile-bundle/v1.2.0/profiles/steward-litellm.json
  provider-profile-bundle/v1.2.0/profiles/steward-mcp-gw.json
  provider-profile-bundle/v1.2.0/examples/inputs.json
  provider-profile-bundle/v1.2.0/release.json
  provider-profile-bundle/v1.2.0/bin/steward-provider-profile
)

staged_bundle="${staging_directory}/provider-profile-bundle/v1.2.0"
mkdir -p "${staged_bundle}/profiles" "${staged_bundle}/examples" "${staged_bundle}/bin"
cp "${bundle_directory}/README.md" "${staged_bundle}/README.md"
cp "${bundle_directory}/bundle.json" "${staged_bundle}/bundle.json"
cp "${bundle_directory}/profiles/steward-litellm.json" "${staged_bundle}/profiles/steward-litellm.json"
cp "${bundle_directory}/profiles/steward-mcp-gw.json" "${staged_bundle}/profiles/steward-mcp-gw.json"
cp "${bundle_directory}/examples/inputs.json" "${staged_bundle}/examples/inputs.json"
install -m 0755 "$installer" "${staged_bundle}/bin/steward-provider-profile"
jq -n \
  --arg source_release "v${version}" \
  --arg installer_os "$installer_os" \
  --arg installer_architecture "$installer_architecture" \
  '{
    schema: "steward.provider-profile-release/v1",
    sourceRelease: $source_release,
    installer: {os: $installer_os, architecture: $installer_architecture}
  }' > "${staged_bundle}/release.json"

# Normalise the staged tree for both GNU tar and the BSD tar shipped by macOS.
# The explicit archive paths exclude directories, so fixed file mtimes plus
# gzip -n are sufficient for reproducible bytes on the local fallback.
find "${staging_directory}" -type f -exec touch -t 197001010000 {} +

# GNU tar metadata controls plus gzip -n make the release bundle bytes
# reproducible. macOS's BSD tar lacks the GNU normalisation switches, so the
# local fallback uses the same explicitly sorted files; the CI release runner
# always executes the GNU branch.
if tar --help 2>&1 | grep -Fq -- '--sort'; then
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=ustar \
    -C "${staging_directory}" -cf - "${bundle_paths[@]}" \
    | gzip -n > "$archive_path"
else
  COPYFILE_DISABLE=1 tar -C "${staging_directory}" -cf - "${bundle_paths[@]}" \
    | gzip -n > "$archive_path"
fi

expected_entries="$(cat <<'ENTRIES'
provider-profile-bundle/v1.2.0/README.md
provider-profile-bundle/v1.2.0/bundle.json
provider-profile-bundle/v1.2.0/profiles/steward-litellm.json
provider-profile-bundle/v1.2.0/profiles/steward-mcp-gw.json
provider-profile-bundle/v1.2.0/examples/inputs.json
provider-profile-bundle/v1.2.0/release.json
provider-profile-bundle/v1.2.0/bin/steward-provider-profile
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
  profiles/steward-mcp-gw.json \
  examples/inputs.json
do
  if ! cmp -s "${bundle_directory}/${archived_file}" \
    <(tar -xOzf "$archive_path" "provider-profile-bundle/v1.2.0/${archived_file}")
  then
    echo "provider-profile bundle archive content does not match ${archived_file}" >&2
    exit 1
  fi
done
if ! cmp -s "$installer" \
  <(tar -xOzf "$archive_path" "provider-profile-bundle/v1.2.0/bin/steward-provider-profile")
then
  echo "provider-profile bundle archive installer does not match the released binary" >&2
  exit 1
fi

printf 'sha256:%s\n' "$(sha256sum "$archive_path" | awk '{print $1}')" > "$digest_path"
printf 'provider-profile-bundle=%s\n' "$archive_name"
printf 'provider-profile-bundle-digest=%s\n' "$(<"$digest_path")"
