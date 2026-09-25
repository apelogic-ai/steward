#!/usr/bin/env bash
set -euo pipefail

digest_pattern='^sha256:[0-9a-f]{64}$'
temporary_directory="$(mktemp -d)"
trap 'rm -rf "${temporary_directory}"' EXIT INT TERM

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

require_digest() {
  local value="$1"
  local label="$2"
  [[ "${value}" =~ ${digest_pattern} ]] || fail "${label} must be a lowercase sha256 digest"
  [[ "${value}" != "sha256:$(printf '0%.0s' {1..64})" ]] || fail "${label} must not be the all-zero placeholder"
}

require_tagged_reference() {
  local value="$1"
  local label="$2"
  [[ -n "${value}" && "${value}" != *'://'* && "${value}" != *'@'* ]] ||
    fail "${label} must be a tag reference without a scheme or digest"
  local final_component="${value##*/}"
  [[ "${final_component}" == *:* && "${final_component}" != :* && "${final_component}" != *: ]] ||
    fail "${label} must include an explicit tag"
}

repository_for_reference() {
  local value="$1"
  printf '%s\n' "${value%:*}"
}

tag_for_reference() {
  local value="$1"
  printf '%s\n' "${value##*:}"
}

descriptor_digest() {
  local reference="$1"
  local descriptor_file="$2"
  shift 2
  oras manifest fetch --descriptor "$@" "${reference}" >"${descriptor_file}"
  local digest
  digest="$(jq -er '.digest | select(type == "string")' "${descriptor_file}")" ||
    fail "registry returned no descriptor digest for ${reference}"
  require_digest "${digest}" "descriptor digest for ${reference}"
  printf '%s\n' "${digest}"
}

read_handoff_artifacts() {
  local handoff="$1"
  jq -er '
    if .schemaVersion != "steward.release-handoff/v1" then
      error("release handoff must use steward.release-handoff/v1")
    elif (.version | type) != "string" or .version == "" then
      error("release handoff version is required")
    elif (.commit | type) != "string" or (.commit | test("^[0-9a-f]{40}$") | not) then
      error("release handoff commit must be a full lowercase Git SHA-1")
    elif (.chart.reference | type) != "string" or (.chart.digest | type) != "string" then
      error("release handoff chart coordinate is required")
    elif (.images | type) != "object" or (.images | length) == 0 then
      error("release handoff contains no Steward images")
    else
      ([{
        key: "chart",
        kind: "chart",
        reference: (.chart.reference | sub("^oci://"; "")),
        digest: .chart.digest
      }] +
      [(.images | to_entries[] | {
        key: ("images." + .key), kind: "image",
        reference: .value.reference, digest: .value.digest
      })] +
      [((.referenceRuntimes // {}) | to_entries[] | {
        key: ("referenceRuntimes." + .key), kind: "referenceRuntime",
        reference: .value.reference, digest: .value.digest
      })])
      | sort_by(.key)[]
      | [.key, .kind, .reference, .digest]
      | @tsv
    end
  ' "${handoff}"
}

plan() {
  local handoff=''
  local mappings=''
  local output=''
  local platform=''
  local target_plain_http='false'
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --handoff) handoff="${2:-}"; shift 2 ;;
      --mappings) mappings="${2:-}"; shift 2 ;;
      --output) output="${2:-}"; shift 2 ;;
      --platform) platform="${2:-}"; shift 2 ;;
      --target-plain-http) target_plain_http='true'; shift ;;
      *) fail "unknown plan argument: $1" ;;
    esac
  done
  [[ -f "${handoff}" ]] || fail "--handoff must name a readable release handoff"
  [[ -f "${mappings}" ]] || fail "--mappings must name a readable mapping document"
  [[ -n "${output}" ]] || fail "--output is required"
  if [[ -n "${platform}" ]]; then
    [[ "${platform}" =~ ^[a-z0-9._-]+/[a-z0-9._-]+(/[a-z0-9._-]+)?$ ]] ||
      fail "--platform must use os/architecture or os/architecture/variant"
  fi
  jq -e '.schemaVersion == "steward.registry-mappings/v1" and (.artifacts | type == "object")' \
    "${mappings}" >/dev/null || fail "mapping document must use steward.registry-mappings/v1"

  local records="${temporary_directory}/plan-records.jsonl"
  local targets="${temporary_directory}/target-repositories.txt"
  : >"${records}"
  : >"${targets}"
  local expected_keys="${temporary_directory}/expected-keys.txt"
  local mapped_keys="${temporary_directory}/mapped-keys.txt"
  read_handoff_artifacts "${handoff}" | cut -f 1 >"${expected_keys}"
  jq -r '.artifacts | keys[]' "${mappings}" | sort >"${mapped_keys}"
  sort -o "${expected_keys}" "${expected_keys}"
  cmp -s "${expected_keys}" "${mapped_keys}" ||
    fail "mapping keys must exactly match the release chart, images, and reference runtimes"

  while IFS=$'\t' read -r key kind source_reference release_digest; do
    require_tagged_reference "${source_reference}" "${key} source reference"
    require_digest "${release_digest}" "${key} release digest"
    local target_reference
    target_reference="$(jq -er --arg key "${key}" '.artifacts[$key] | select(type == "string" and length > 0)' "${mappings}")" ||
      fail "mapping for ${key} is required"
    require_tagged_reference "${target_reference}" "${key} target reference"
    local target_repository
    target_repository="$(repository_for_reference "${target_reference}")"
    printf '%s\n' "${target_repository}" >>"${targets}"

    local source_exact="${source_reference}@${release_digest}"
    local descriptor_file
    descriptor_file="${temporary_directory}/source-$(printf '%s' "${key}" | tr -c 'a-zA-Z0-9' '_').json"
    local selected_digest
    if [[ -n "${platform}" && "${kind}" != chart ]]; then
      selected_digest="$(descriptor_digest "${source_exact}" "${descriptor_file}" --platform "${platform}")"
    else
      selected_digest="$(descriptor_digest "${source_exact}" "${descriptor_file}")"
    fi
    jq -cn \
      --arg key "${key}" \
      --arg kind "${kind}" \
      --arg source_reference "${source_reference}" \
      --arg release_digest "${release_digest}" \
      --arg selected_digest "${selected_digest}" \
      --arg target_reference "${target_reference}" \
      --arg target_repository "${target_repository}" \
      '{
        key: $key,
        kind: $kind,
        source: {
          reference: $source_reference,
          releaseDigest: $release_digest,
          digest: $selected_digest
        },
        target: {reference: $target_reference, repository: $target_repository}
      }' >>"${records}"
  done < <(read_handoff_artifacts "${handoff}")

  sort -u "${targets}" | while IFS= read -r repository; do
    if [[ "${target_plain_http}" == true ]]; then
      oras repo tags --plain-http "${repository}" >/dev/null ||
        fail "target repository preflight failed for ${repository}"
    else
      oras repo tags "${repository}" >/dev/null ||
        fail "target repository preflight failed for ${repository}"
    fi
  done

  jq -nS \
    --arg version "$(jq -er '.version' "${handoff}")" \
    --arg commit "$(jq -er '.commit' "${handoff}")" \
    --arg platform "${platform}" \
    --argjson target_plain_http "${target_plain_http}" \
    --slurpfile records "${records}" \
    '{
      schemaVersion: "steward.registry-plan/v1",
      release: {version: $version, commit: $commit},
      platform: (if $platform == "" then null else $platform end),
      targetPlainHttp: $target_plain_http,
      artifacts: ($records | map({key: .key, value: (del(.key))}) | from_entries)
    }' >"${output}"
}

mirror() {
  local plan_file=''
  local output=''
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --plan) plan_file="${2:-}"; shift 2 ;;
      --output) output="${2:-}"; shift 2 ;;
      *) fail "unknown mirror argument: $1" ;;
    esac
  done
  [[ -f "${plan_file}" ]] || fail "--plan must name a readable no-write plan"
  [[ -n "${output}" ]] || fail "--output is required"
  jq -e '.schemaVersion == "steward.registry-plan/v1" and (.artifacts | type == "object")' \
    "${plan_file}" >/dev/null || fail "plan must use steward.registry-plan/v1"
  local target_plain_http
  target_plain_http="$(jq -r '.targetPlainHttp == true' "${plan_file}")"

  local records="${temporary_directory}/lock-records.jsonl"
  : >"${records}"
  while IFS=$'\t' read -r key kind source_reference source_digest release_digest target_reference target_repository; do
    require_digest "${source_digest}" "${key} selected source digest"
    require_digest "${release_digest}" "${key} release digest"
    local status='copied'
    local target_descriptor
    target_descriptor="${temporary_directory}/target-$(printf '%s' "${key}" | tr -c 'a-zA-Z0-9' '_').json"
    local current_digest=''
    if [[ "${target_plain_http}" == true ]]; then
      if oras manifest fetch --descriptor --plain-http "${target_reference}" >"${target_descriptor}" 2>/dev/null; then
        current_digest="$(jq -er '.digest | select(type == "string")' "${target_descriptor}")"
        require_digest "${current_digest}" "target descriptor digest for ${target_reference}"
      fi
    else
      if oras manifest fetch --descriptor "${target_reference}" >"${target_descriptor}" 2>/dev/null; then
        current_digest="$(jq -er '.digest | select(type == "string")' "${target_descriptor}")"
        require_digest "${current_digest}" "target descriptor digest for ${target_reference}"
      fi
    fi
    if [[ -n "${current_digest}" ]]; then
      [[ "${current_digest}" == "${source_digest}" ]] ||
        fail "target ${key} already exists with a different digest"
      status='already-present'
    else
      if [[ "${target_plain_http}" == true ]]; then
        oras cp --recursive --no-tty --to-plain-http \
          "${source_reference}@${source_digest}" "${target_reference}"
      else
        oras cp --recursive --no-tty "${source_reference}@${source_digest}" "${target_reference}"
      fi
    fi
    local target_digest
    if [[ "${target_plain_http}" == true ]]; then
      target_digest="$(descriptor_digest "${target_reference}" "${target_descriptor}" --plain-http)"
    else
      target_digest="$(descriptor_digest "${target_reference}" "${target_descriptor}")"
    fi
    [[ "${target_digest}" == "${source_digest}" ]] ||
      fail "target ${key} did not preserve the selected source manifest"
    jq -cn --arg artifact "${key}" --arg status "${status}" \
      '{artifact: $artifact, status: $status}'
    jq -cn \
      --arg key "${key}" \
      --arg kind "${kind}" \
      --arg source_reference "${source_reference}" \
      --arg source_digest "${source_digest}" \
      --arg release_digest "${release_digest}" \
      --arg target_reference "${target_reference}" \
      --arg target_repository "${target_repository}" \
      '{
        key: $key,
        kind: $kind,
        source: {reference: $source_reference, digest: $source_digest, releaseDigest: $release_digest},
        target: {reference: $target_reference, repository: $target_repository, digest: $source_digest}
      }' >>"${records}"
  done < <(jq -r '.artifacts | to_entries | sort_by(.key)[] | [
    .key, .value.kind, .value.source.reference, .value.source.digest,
    .value.source.releaseDigest, .value.target.reference, .value.target.repository
  ] | @tsv' "${plan_file}")

  local artifacts="${temporary_directory}/artifacts.json"
  jq -s 'map({key: .key, value: (del(.key))}) | from_entries' "${records}" >"${artifacts}"
  local image_repositories
  image_repositories="$(jq -r '[to_entries[] | select(.key | startswith("images.")) | .value.target.repository] | unique[]' "${artifacts}")"
  [[ "$(printf '%s\n' "${image_repositories}" | sed '/^$/d' | wc -l | tr -d ' ')" == 1 ]] ||
    fail "Steward component images must map to one target repository"
  local image_repository="${image_repositories}"

  jq -nS \
    --slurpfile plan "${plan_file}" \
    --slurpfile artifacts "${artifacts}" \
    --arg image_repository "${image_repository}" \
    '($plan[0]) as $p | ($artifacts[0]) as $a |
    {
      schemaVersion: "steward.deployment-lock/v1",
      release: $p.release,
      mode: (if $p.platform == null then "index" else "single-platform" end),
      requestedPlatform: $p.platform,
      artifacts: $a,
      chartValues: {
        images: (
          {repository: $image_repository} +
          ([$a | to_entries[] | select(.key | startswith("images.")) |
            select(.key != "images.bridge") | {
              key: (.key | sub("^images\\."; "")),
              value: {tag: (.value.target.reference | split(":")[-1]), digest: .value.target.digest}
            }] | from_entries)
        )
      },
      executionBindingImages: (
        [$a | to_entries[] | select(.key | startswith("referenceRuntimes.")) | {
          key: (.key | sub("^referenceRuntimes\\."; "")),
          value: (.value.target.repository + "@" + .value.target.digest)
        }] | from_entries
      )
    }
    | .artifacts.chart.flux = {
        ociRepository: {
          url: ("oci://" + $a.chart.target.repository),
          ref: {digest: $a.chart.target.digest}
        }
      }
    | if $a["images.bridge"] then
        .chartValues.connectionsBridge.image = ($a["images.bridge"].target.repository + "@" + $a["images.bridge"].target.digest)
      else . end
    ' >"${output}"
}

usage() {
  cat >&2 <<'USAGE'
usage:
  steward-registry-lock.sh plan --handoff FILE --mappings FILE --output FILE [--platform OS/ARCH[/VARIANT]] [--target-plain-http]
  steward-registry-lock.sh mirror --plan FILE --output FILE
USAGE
  exit 2
}

require_command jq
require_command oras
command="${1:-}"
[[ -n "${command}" ]] || usage
shift
case "${command}" in
  plan) plan "$@" ;;
  mirror) mirror "$@" ;;
  *) usage ;;
esac
