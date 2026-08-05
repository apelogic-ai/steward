#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 3 ]]; then
  echo "usage: $0 <index-ref> <os> <architecture>" >&2
  exit 2
fi

index_ref="$1"
platform_os="$2"
platform_architecture="$3"

index_json="$(oras manifest fetch "${index_ref}")"
matches="$(
  jq --compact-output \
    --arg os "${platform_os}" \
    --arg architecture "${platform_architecture}" \
    '[
      .manifests[]?
      | select(.platform.os == $os and .platform.architecture == $architecture)
      | select((.annotations["vnd.docker.reference.type"] // "") != "attestation-manifest")
    ]' <<< "${index_json}"
)"
match_count="$(jq 'length' <<< "${matches}")"

if [[ "${match_count}" -ne 1 ]]; then
  echo "expected exactly one runnable ${platform_os}/${platform_architecture} manifest in ${index_ref}, found ${match_count}" >&2
  exit 1
fi

platform_digest="$(jq --raw-output '.[0].digest' <<< "${matches}")"
if [[ ! "${platform_digest}" =~ ^sha256:[a-f0-9]{64}$ ]]; then
  echo "resolved ${platform_os}/${platform_architecture} manifest has an invalid digest in ${index_ref}" >&2
  exit 1
fi

printf '%s\n' "${platform_digest}"
